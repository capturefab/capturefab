//! Native industrial TCP, serial and CIP devices through one real CLI recording.
#![cfg(unix)]
#[path = "support/aandd_balance.rs"]
mod aandd_balance;
#[path = "support/bacnet_ip.rs"]
mod bacnet_ip;
#[path = "support/cognex_insight.rs"]
mod cognex_insight;
#[path = "support/df1.rs"]
mod df1;
#[path = "support/fatek_fbs.rs"]
mod fatek_fbs;
#[path = "support/fx_computer_link.rs"]
mod fx_computer_link;
#[path = "support/hart.rs"]
mod hart;
#[path = "support/iec62056_21.rs"]
mod iec62056_21;
#[path = "support/keyence_iv.rs"]
mod keyence_iv;
#[path = "support/keyence_kv.rs"]
mod keyence_kv;
#[path = "support/ls_cnet.rs"]
mod ls_cnet;
#[path = "support/ls_xgt.rs"]
mod ls_xgt;
#[path = "support/mewtocol.rs"]
mod mewtocol;
#[path = "support/microepsilon_rs422.rs"]
mod microepsilon_rs422;
#[path = "support/modbus_ascii.rs"]
mod modbus_ascii;
#[path = "support/modbus_rtu_cutoff.rs"]
mod modbus_rtu_cutoff;
#[path = "support/mt_sics.rs"]
mod mt_sics;
#[path = "support/nmea_gnss.rs"]
mod nmea_gnss;
#[path = "support/omron_compoway_f.rs"]
mod omron_compoway_f;
#[path = "support/pfeiffer_tpg.rs"]
mod pfeiffer_tpg;
#[path = "support/sartorius_sbi.rs"]
mod sartorius_sbi;
#[path = "support/sdi12.rs"]
mod sdi12;
#[path = "support/sick_cola.rs"]
mod sick_cola;
#[path = "support/sick_cola_ascii.rs"]
mod sick_cola_ascii;
#[path = "support/sick_cola_binary.rs"]
mod sick_cola_binary;
#[path = "support/sml.rs"]
mod sml;
mod support;
#[path = "support/ur_rtde.rs"]
mod ur_rtde;
#[path = "support/vaisala_wxt.rs"]
mod vaisala_wxt;
#[path = "support/yokogawa_fam3.rs"]
mod yokogawa_fam3;

use serde_json::{Value, json};
use std::{
    ffi::CStr,
    fs::{self, File},
    io::{Read, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::{AsRawFd, FromRawFd as _},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use support::{cli::Home, time};

const WIDE_COUNTS: [u64; 3] = [u64::MAX, 0x0123_4567_89ab_cdef, (1 << 53) + 1];
const WIDE_SIGNED: [i64; 2] = [i64::MIN, -0x0123_4567_89ab_cdef];

struct Device {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<u64>>,
}
impl Device {
    fn start(serve: impl FnOnce(Arc<AtomicBool>) -> u64 + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        Self {
            stop,
            worker: Some(thread::spawn(move || serve(done))),
        }
    }
    fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap()
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

/// A `--for` window written as `"1s"` or `"600ms"`, stretched by the test time scale.
fn window(duration: &str) -> String {
    let millis = duration.strip_suffix("ms").map_or_else(
        || duration.strip_suffix('s').unwrap().parse::<u64>().unwrap() * 1000,
        |millis| millis.parse::<u64>().unwrap(),
    );
    time::arg(millis)
}

fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    (listener, address)
}
fn accept(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    let deadline = Instant::now() + time::secs(10);
    loop {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // BSD accepts can inherit the listener's O_NONBLOCK status.
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                stream.set_write_timeout(Some(time::secs(1))).unwrap();
                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "recorder did not connect to device"
                );
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
/// Wait up to 10 ms for `events` on a fixture descriptor. Unlike a short
/// sleep, a readiness wakeup is not delayed by timer coalescing on a loaded host.
fn wait_ready(file: &impl AsRawFd, events: libc::c_short) {
    let mut descriptor = libc::pollfd {
        fd: file.as_raw_fd(),
        events,
        revents: 0,
    };
    // SAFETY: one valid pollfd for a descriptor the caller keeps open.
    let status = unsafe { libc::poll(&raw mut descriptor, 1, 10) };
    assert!(
        status >= 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted,
        "fixture poll: {}",
        std::io::Error::last_os_error()
    );
}
/// Keep partial requests over short readiness timeouts, and bound every fixture
/// even when a product failure prevents its normal EOF/stop handshake.
fn receive(input: &mut (impl Read + AsRawFd), mut bytes: &mut [u8], stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + time::secs(10);
    while !bytes.is_empty() {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match input.read(bytes) {
            Ok(0) => return false,
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                assert!(Instant::now() < deadline, "device request did not finish");
                wait_ready(input, libc::POLLIN);
            }
            Err(error) => panic!("device receive: {error}"),
        }
    }
    true
}

fn modbus_tcp_reply(stream: &mut TcpStream, request: [u8; 12], pdu: &[u8]) {
    let mut response = Vec::from(&request[..4]);
    response.extend_from_slice(&u16::try_from(pdu.len() + 1).unwrap().to_be_bytes());
    response.push(request[6]);
    response.extend_from_slice(pdu);
    stream.write_all(&response).unwrap();
}

fn modbus_tcp(listener: &TcpListener, stop: &AtomicBool) -> u64 {
    let Some(mut stream) = accept(listener, stop) else {
        return 0;
    };
    let mut requests = 0;
    loop {
        let mut request = [0; 12];
        if !receive(&mut stream, &mut request, stop) {
            break;
        }
        assert_eq!(&request[2..7], &[0, 0, 0, 6, 17]);
        let pdu = match &request[7..] {
            [3, 0, 0, 0, 2] => vec![3, 4, 0xff, 0xff, 0xff, 0xff],
            [4, 0, 9, 0, 2] => {
                let mut pdu = vec![4, 4];
                pdu.extend_from_slice(&12.5_f32.to_be_bytes());
                pdu
            }
            [1, 0, 7, 0, 1] => vec![1, 1, 1],
            [3, 0, 20, 0, 4] => {
                let mut pdu = vec![3, 8];
                pdu.extend_from_slice(&(u64::MAX - 1).to_be_bytes());
                pdu
            }
            [4, 0, 24, 0, 4] => {
                let mut pdu = vec![4, 8];
                // Both orders little: least-significant register first, with
                // least-significant byte first within each register.
                pdu.extend_from_slice(&12.5_f64.to_le_bytes());
                pdu
            }
            [3, 0, 100, 0, 12] => {
                let mut pdu = vec![3, 24];
                for value in WIDE_COUNTS {
                    pdu.extend_from_slice(&value.to_le_bytes());
                }
                pdu
            }
            [1, 0, 70, 0, 13] => vec![1, 2, 0x8d, 0x15],
            [4, 0xff, 0x83, 0, 125] => {
                // The final register of a maximum read is address 65535.
                let mut pdu = vec![4, 250];
                for register in 0_u16..125 {
                    pdu.extend_from_slice(&register.to_be_bytes());
                }
                pdu
            }
            other => panic!("unexpected Modbus request; only FC1/3/4 reads are allowed: {other:?}"),
        };
        modbus_tcp_reply(&mut stream, request, &pdu);
        requests += 1;
    }
    requests
}

fn crc(bytes: &[u8]) -> u16 {
    let mut value = 0xffff;
    for &byte in bytes {
        value ^= u16::from(byte);
        for _ in 0..8 {
            value = (value >> 1) ^ if value & 1 == 0 { 0 } else { 0xa001 };
        }
    }
    value
}
fn serial_pair() -> (File, File, String) {
    let (mut master, mut slave) = (-1, -1);
    let mut name: [libc::c_char; 256] = [0; 256];
    // SAFETY: output pointers and name storage are valid; null options select defaults.
    assert_eq!(
        // SAFETY: output allocations and optional null settings are valid for openpty.
        unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                name.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned new descriptors, transferred to owned File values.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: this is the other descriptor returned by successful openpty.
    let slave = unsafe { File::from_raw_fd(slave) };
    for file in [&master, &slave] {
        // SAFETY: the owned descriptor stays live through both fcntl calls.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        // SAFETY: set close-on-exec so the recorder does not inherit fixture descriptors.
        assert_eq!(
            // SAFETY: this owned descriptor is live; F_SETFD receives a valid flag mask.
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) },
            0
        );
    }
    // SAFETY: the master is live; preserve its flags while enabling bounded nonblocking reads.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    // SAFETY: same live descriptor and valid flag mask.
    assert_eq!(
        // SAFETY: the master descriptor is live and O_NONBLOCK is a valid status flag.
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    // SAFETY: successful openpty wrote a NUL-terminated name into the supplied array.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    (master, slave, name)
}
fn modbus_rtu(mut master: File, _slave: File, stop: &AtomicBool) -> u64 {
    let mut requests = 0;
    loop {
        let mut request = [0; 8];
        if !receive(&mut master, &mut request, stop) {
            break;
        }
        assert_eq!(
            crc(&request[..6]),
            u16::from_le_bytes([request[6], request[7]])
        );
        let mut response = match &request[..6] {
            [3, 3, 0, 3, 0, 1] => {
                let mut response = vec![3, 3, 2];
                response.extend_from_slice(&(-1234_i16).to_be_bytes());
                response
            }
            [247, 2, 0, 9, 0, 1] => vec![247, 2, 1, 1],
            [3, 3, 0, 19, 0, 4] => {
                let mut response = vec![3, 3, 8];
                for register in (i64::MIN + 1).to_be_bytes().as_chunks::<2>().0 {
                    response.extend_from_slice(&[register[1], register[0]]);
                }
                response
            }
            [19, 3, 0, 100, 0, 8] => {
                let mut response = vec![19, 3, 16];
                for value in WIDE_SIGNED {
                    for register in value.to_be_bytes().as_chunks::<2>().0 {
                        response.extend_from_slice(&[register[1], register[0]]);
                    }
                }
                response
            }
            [247, 2, 0xf8, 0x30, 0x07, 0xd0] => {
                // 2,000 bits end at 65535. With CRC, the reply is 255 bytes.
                let mut response = vec![247, 2, 250];
                response.resize(253, 0x81);
                response
            }
            other => panic!("unexpected serial request; only FC2/3 reads are allowed: {other:?}"),
        };
        response.extend_from_slice(&crc(&response).to_le_bytes());
        master.write_all(&response).unwrap();
        requests += 1;
    }
    requests
}

fn encapsulation(stream: &mut TcpStream, stop: &AtomicBool) -> Option<([u8; 24], Vec<u8>)> {
    let mut header = [0; 24];
    if !receive(stream, &mut header, stop) {
        return None;
    }
    let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
    assert!(size <= 1024);
    let mut body = vec![0; size];
    if !receive(stream, &mut body, stop) {
        return None;
    }
    Some((header, body))
}
fn cip_reply(stream: &mut TcpStream, mut header: [u8; 24], body: &[u8]) {
    header[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
    header[4..8].copy_from_slice(&0x4433_2211_u32.to_le_bytes());
    header[8..12].copy_from_slice(&[0; 4]);
    stream.write_all(&header).unwrap();
    stream.write_all(body).unwrap();
}
fn ethernet_ip(listener: &TcpListener, stop: &AtomicBool) -> u64 {
    let Some(mut stream) = accept(listener, stop) else {
        return 0;
    };
    let Some((header, body)) = encapsulation(&mut stream, stop) else {
        return 0;
    };
    assert_eq!(&header[..12], &[0x65, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(body, [1, 0, 0, 0]);
    cip_reply(&mut stream, header, &body);
    let mut requests = 0;
    while let Some((header, body)) = encapsulation(&mut stream, stop) {
        assert_eq!(&header[4..8], &0x4433_2211_u32.to_le_bytes());
        match u16::from_le_bytes([header[0], header[1]]) {
            0x66 => {
                assert_eq!(body, [] as [u8; 0]);
                break;
            }
            0x6f => {
                assert!(body.len() >= 20);
                assert_eq!(&body[..14], &[0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0]);
                assert_eq!(
                    usize::from(u16::from_le_bytes([body[14], body[15]])),
                    body.len() - 16
                );
                let request = &body[16..];
                assert!(request.len() >= 4);
                assert_eq!(request[0], 0x4c, "only CIP Read Tag is allowed");
                let path_len = usize::from(request[1]) * 2;
                assert_eq!(&request[2 + path_len..], &[1, 0]);
                let path = &request[2..2 + path_len];
                assert!(path.len() >= 2);
                assert_eq!(path[0], 0x91);
                let name_len = usize::from(path[1]);
                let name = std::str::from_utf8(&path[2..2 + name_len]).unwrap();
                let mut cip = vec![0xcc, 0, 0, 0];
                match name {
                    "TotalCycles" => {
                        cip.extend_from_slice(&[0xc9, 0]);
                        cip.extend_from_slice(&u64::MAX.to_le_bytes());
                    }
                    "Ready" => cip.extend_from_slice(&[0xc1, 0, 0xff]),
                    other => panic!("unexpected tag: {other}"),
                }
                let mut response = vec![0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
                response.extend_from_slice(&u16::try_from(cip.len()).unwrap().to_le_bytes());
                response.extend_from_slice(&cip);
                cip_reply(&mut stream, header, &response);
                requests += 1;
            }
            other => panic!("unexpected EtherNet/IP service: {other}"),
        }
    }
    requests
}

#[test]
fn native_modbus_tcp_rtu_and_ethernet_ip_share_a_recording_and_exports() {
    let home = Home::new("data-native-devices");
    let (tcp_listener, tcp_address) = listener();
    let (cip_listener, cip_address) = listener();
    let (master, slave, device) = serial_pair();
    let tcp = Device::start(move |stop| modbus_tcp(&tcp_listener, &stop));
    let cip = Device::start(move |stop| ethernet_ip(&cip_listener, &stop));
    let rtu = Device::start(move |stop| modbus_rtu(master, slave, &stop));
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[
        {"id":"modbus_tcp","protocol":"modbus_tcp","address":tcp_address.to_string(),"unit":17,"every_ms":time::ms(100),"timeout_ms":time::ms(1000),
         "signals":[{"name":"count","address":0,"data_type":"u32"},{"name":"temperature","table":"input","address":9,"data_type":"f32","scale":0.1},{"name":"running","table":"coil","address":7,"data_type":"bool","elements":1.0},
                    {"name":"wide_count","address":20,"data_type":"u64"},{"name":"energy","table":"input","address":24,"data_type":"f64","word_order":"little","byte_order":"little","scale":0.1},
                    {"name":"wide_counts","address":100,"data_type":"u64","word_order":"little","byte_order":"little","elements":serde_json::from_str::<Value>("3e0").unwrap()},
                    {"name":"flags","table":"coil","address":70,"data_type":"bool","elements":13},
                    {"name":"register_window","table":"input","address":65_411,"elements":125,"scale":0.5,"offset":2}]},
        {"id":"modbus_rtu","protocol":"modbus_rtu","device":device,"unit":3,"parity":"none","stop_bits":2,"every_ms":time::ms(100),"timeout_ms":time::ms(1000),
         "signals":[{"name":"signed","address":3,"data_type":"i16"},{"name":"present","unit":247,"table":"discrete","address":9,"data_type":"bool"},
                    {"name":"wide_signed","address":19,"data_type":"i64","byte_order":"little"},
                    {"name":"wide_signed_array","unit":19,"address":100,"data_type":"i64","byte_order":"little","elements":2},
                    {"name":"inputs","unit":247,"table":"discrete","address":63_536,"data_type":"bool","elements":serde_json::from_str::<Value>("2e3").unwrap()}]},
        {"id":"ethernet_ip","protocol":"ethernet_ip","address":cip_address.to_string(),"every_ms":time::ms(100),"timeout_ms":time::ms(1000),
         "signals":[{"name":"cycles","tag":"TotalCycles"},{"name":"ready","tag":"Ready"}]}
    ]}).to_string()).unwrap();
    let result = home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &time::arg(500),
        "-o",
        "run.jsonl",
    ]);
    assert!(tcp.finish() >= 8);
    assert!(rtu.finish() >= 5);
    assert!(cip.finish() >= 2);
    assert_eq!(result["status"], "complete");
    let rows: Vec<Value> = fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let mut signals = std::collections::BTreeSet::new();
    let samples = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    let expected_registers = json!(
        (0_u16..125)
            .map(|value| f64::from(value) * 0.5 + 2.0)
            .collect::<Vec<_>>()
    );
    let expected_inputs = json!(
        (0..2000)
            .map(|index| matches!(index % 8, 0 | 7))
            .collect::<Vec<_>>()
    );
    for sample in &samples {
        let source = sample["source"].as_str().unwrap();
        let signal = sample["signal"].as_str().unwrap();
        signals.insert((source, signal));
        let value = &sample["value"];
        match (source, signal) {
            ("modbus_tcp", "count") => assert_eq!(value.as_f64(), Some(f64::from(u32::MAX))),
            ("modbus_tcp", "temperature" | "energy") => assert_eq!(value.as_f64(), Some(1.25)),
            ("modbus_tcp", "wide_count") => assert_eq!(value.as_u64(), Some(u64::MAX - 1)),
            ("modbus_rtu", "signed") => assert_eq!(value.as_f64(), Some(-1234.0)),
            ("modbus_rtu", "wide_signed") => assert_eq!(value.as_i64(), Some(i64::MIN + 1)),
            ("modbus_tcp", "running") | ("modbus_rtu", "present") | ("ethernet_ip", "ready") => {
                assert_eq!(value.as_bool(), Some(true));
            }
            ("modbus_tcp", "wide_counts") => assert_eq!(*value, json!(WIDE_COUNTS)),
            ("modbus_tcp", "flags") => assert_eq!(
                *value,
                json!([
                    true, false, true, true, false, false, false, true, true, false, true, false,
                    true
                ])
            ),
            ("modbus_tcp", "register_window") => assert_eq!(*value, expected_registers),
            ("modbus_rtu", "wide_signed_array") => assert_eq!(*value, json!(WIDE_SIGNED)),
            ("modbus_rtu", "inputs") => assert_eq!(*value, expected_inputs),
            ("ethernet_ip", "cycles") => assert_eq!(value.as_u64(), Some(u64::MAX)),
            other => panic!("unexpected source/signal: {other:?}"),
        }
        let time = chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            u64::try_from((time - origin).num_nanoseconds().unwrap()).unwrap(),
            sample["elapsed_ns"].as_u64().unwrap()
        );
        assert!(
            sample["read_duration_ns"].as_u64().unwrap() <= sample["elapsed_ns"].as_u64().unwrap()
        );
        assert_eq!(sample["delivery_latency_ns"], 0);
    }
    assert_eq!(signals.len(), 15);
    assert_eq!(result["samples"], samples.len());
    assert_eq!(rows.last().unwrap()["samples"], samples.len());
    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), samples.len() + 1);
    assert!(csv.contains("18446744073709551615"));
    assert!(csv.contains("18446744073709551614"));
    assert!(csv.contains("-9223372036854775807"));
    assert!(csv.contains(&json!(WIDE_COUNTS).to_string()));
    assert!(csv.contains(&json!(WIDE_SIGNED).to_string()));
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Vec<Value> = fs::read_to_string(home.path("values.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(exported.len(), samples.len());
    for (actual, original) in exported.iter().zip(samples) {
        let mut expected = original.clone();
        expected.as_object_mut().unwrap().remove("kind");
        assert_eq!(*actual, expected);
    }
}

fn record_modbus(home: &Home, source: &Value) -> support::cli::Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &time::arg(2000),
        "-o",
        "run.jsonl",
    ])
}
fn assert_unpublished(home: &Home) {
    let rows = fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(!rows.iter().any(|row| row["kind"] == "sample"));
    assert_eq!(rows.last().unwrap()["status"], "failed");
}
fn expect_modbus_request(stream: &mut TcpStream, stop: &AtomicBool, pdu: [u8; 5]) -> [u8; 12] {
    let mut request = [0; 12];
    assert!(receive(stream, &mut request, stop));
    assert_eq!(&request[2..7], &[0, 0, 0, 6, 17]);
    assert_eq!(&request[7..], &pdu);
    request
}

#[test]
fn malformed_tcp_arrays_discard_successful_siblings() {
    let mut nonfinite = vec![4, 8];
    nonfinite.extend_from_slice(&1.25_f32.to_be_bytes());
    nonfinite.extend_from_slice(&f32::NAN.to_be_bytes());
    for (signal, pdu, response, reason) in [
        (
            json!({"name":"array","address":16,"table":"coil","data_type":"bool","elements":13}),
            [1, 0, 16, 0, 13],
            vec![1, 2, 0x8d, 0x35],
            "padding bits",
        ),
        (
            json!({"name":"array","address":16,"data_type":"u64","elements":2}),
            [3, 0, 16, 0, 8],
            vec![3, 8, 0, 0, 0, 0, 0, 0, 0, 7],
            "response byte count",
        ),
        (
            json!({"name":"array","address":16,"table":"input","data_type":"f32","elements":2}),
            [4, 0, 16, 0, 4],
            nonfinite,
            "non-finite",
        ),
    ] {
        let home = Home::new("modbus-tcp-array-error");
        let (listener, address) = listener();
        let server = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            let request = expect_modbus_request(&mut stream, &stop, [3, 0, 0, 0, 1]);
            modbus_tcp_reply(&mut stream, request, &[3, 2, 0, 7]);
            let request = expect_modbus_request(&mut stream, &stop, pdu);
            modbus_tcp_reply(&mut stream, request, &response);
            2
        });
        let run = record_modbus(
            &home,
            &json!({"id":"modbus","protocol":"modbus_tcp","address":address.to_string(),"unit":17,
            "every_ms":3_600_000,"timeout_ms":time::ms(1000),"signals":[{"name":"sibling","address":0},signal]}),
        );
        assert_eq!(server.finish(), 2);
        let error = run.failure(1);
        assert_eq!(error["code"], "failed");
        assert!(
            error["message"].as_str().unwrap().contains(reason),
            "{error}"
        );
        assert_unpublished(&home);
    }
}

#[test]
fn invalid_rtu_reply_discards_successful_siblings_from_another_unit() {
    for (wrong_unit, reason) in [(false, "CRC mismatch"), (true, "unexpected unit")] {
        let home = Home::new("modbus-rtu-multidrop-error");
        let (mut master, slave, device) = serial_pair();
        let server = Device::start(move |stop| {
            // Keep the PTY alive until the recorder has consumed the reply.
            let _slave = slave;
            let reply_unit = if wrong_unit { 3 } else { 17 };
            let mut array = vec![reply_unit, 3, 16];
            array.resize(19, 0);
            for (unit, pdu, mut response) in [
                (3, [3, 0, 0, 0, 1], vec![3, 3, 2, 0, 7]),
                (17, [3, 0, 16, 0, 8], array),
            ] {
                let mut request = [0; 8];
                assert!(receive(&mut master, &mut request, &stop));
                assert_eq!(request[0], unit);
                assert_eq!(&request[1..6], &pdu);
                assert_eq!(
                    crc(&request[..6]),
                    u16::from_le_bytes([request[6], request[7]])
                );
                let checksum = crc(&response) ^ u16::from(unit == 17 && !wrong_unit);
                response.extend_from_slice(&checksum.to_le_bytes());
                master.write_all(&response).unwrap();
            }
            assert!(!receive(&mut master, &mut [0; 8], &stop));
            2
        });
        let run = record_modbus(
            &home,
            &json!({"id":"modbus","protocol":"modbus_rtu","device":device,"unit":3,
            "parity":"none","stop_bits":2,"every_ms":time::ms(100),"timeout_ms":time::ms(1000),
            "signals":[{"name":"sibling","address":0},{"name":"array","unit":17,"address":16,"data_type":"i64","elements":2}]}),
        );
        assert_eq!(server.finish(), 2);
        let error = run.failure(1);
        assert_eq!(error["code"], "failed");
        assert!(
            error["message"].as_str().unwrap().contains(reason),
            "{error}"
        );
        assert_unpublished(&home);
    }
}

#[test]
fn serial_unit_overrides_validate_before_opening_the_port() {
    let home = Home::new("modbus-rtu-units");
    for unit in [0, 248] {
        let error = record_modbus(
            &home,
            &json!({"id":"modbus","protocol":"modbus_rtu","device":home.path("no-port"),
            "signals":[{"name":"meter","unit":unit,"address":0}]}),
        )
        .failure(2);
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("unit must be 1..247"),
            "{error}"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn independent_serial_sources_cannot_reopen_the_same_terminal() {
    let home = Home::new("modbus-rtu-one-port");
    let (_master, _slave, device) = serial_pair();
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[
            {"id":"first","protocol":"modbus_rtu","device":device,"unit":1,
             "parity":"none","stop_bits":2,"signals":[{"name":"value","address":0}]},
            {"id":"second","protocol":"modbus_rtu","device":device,"unit":2,
             "parity":"none","stop_bits":2,"signals":[{"name":"value","address":0}]}
        ]})
        .to_string(),
    )
    .unwrap();
    let error = home.fails(
        3,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            &time::arg(2000),
            "-o",
            "run.jsonl",
        ],
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("serial configuration"),
        "{error}"
    );
    assert!(!home.path("run.jsonl").exists());
}

#[test]
fn modbus_poll_budget_rejects_before_querying_another_array() {
    let home = Home::new("modbus-array-budget");
    let (listener, address) = listener();
    let bytes = serde_json::to_vec(&json!({"signal":"bits_00","value":vec![false; 2000]}))
        .unwrap()
        .len()
        + 1;
    assert!(bytes * 87 + 1 < 1 << 20);
    assert!(bytes * 88 + 1 > 1 << 20);
    let signals = (0..89)
        .map(|index| {
            json!({"name":format!("bits_{index:02}"),"address":0,
        "table":"coil","data_type":"bool","elements":2000})
        })
        .collect::<Vec<_>>();
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        let mut response = vec![1, 250];
        response.resize(252, 0);
        for _ in 0..88 {
            let request = expect_modbus_request(&mut stream, &stop, [1, 0, 0, 0x07, 0xd0]);
            modbus_tcp_reply(&mut stream, request, &response);
        }
        stream.set_read_timeout(Some(time::secs(3))).unwrap();
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("the 89th query must not be sent: {other:?}"),
        }
        88
    });
    let run = record_modbus(
        &home,
        &json!({"id":"modbus","protocol":"modbus_tcp","address":address.to_string(),"unit":17,
        "every_ms":3_600_000,"timeout_ms":time::ms(1800),"signals":signals}),
    );
    assert_eq!(server.finish(), 88);
    let error = run.failure(1);
    assert_eq!(error["code"], "failed");
    assert!(
        error["message"].as_str().unwrap().contains("1 MiB"),
        "{error}"
    );
    assert_unpublished(&home);
}

// AutomationDirect MB-GATEWAY Rev. H pp. 5-3 and 5-7 describe the downstream
// unit 5 read and the local unit 255 cache read, respectively. Transaction
// IDs and response register values here are fixture data.
// <https://cdn.automationdirect.com/static/manuals/mbgatewaym/ch5.pdf>
const GATEWAY_UNIT_ONE: [u8; 12] = [0, 1, 0, 0, 0, 6, 1, 3, 0, 0, 0, 1];
const GATEWAY_UNIT_FIVE: [u8; 12] = [0, 2, 0, 0, 0, 6, 5, 3, 0, 10, 0, 1];
const GATEWAY_CACHE: [u8; 12] = [0, 3, 0, 0, 0, 6, 255, 3, 0, 0, 0, 2];

fn expect_gateway_request(
    stream: &mut TcpStream,
    stop: &AtomicBool,
    expected: [u8; 12],
) -> [u8; 12] {
    let mut request = [0; 12];
    assert!(receive(stream, &mut request, stop));
    assert_eq!(request, expected);
    request
}

#[test]
fn modbus_tcp_gateway_reads_multiple_units_and_local_cache_on_one_stream() {
    let home = Home::new("modbus-tcp-gateway");
    let (listener, address) = listener();
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        for (expected, pdu) in [
            (GATEWAY_UNIT_ONE, &[3, 2, 0, 7][..]),
            (GATEWAY_UNIT_FIVE, &[3, 2, 0x12, 0x34][..]),
            (GATEWAY_CACHE, &[3, 4, 0, 7, 0xab, 0xcd][..]),
        ] {
            let request = expect_gateway_request(&mut stream, &stop, expected);
            modbus_tcp_reply(&mut stream, request, pdu);
        }
        assert!(!receive(&mut stream, &mut [0], &stop));
        3
    });
    let run = record_modbus(
        &home,
        &json!({"id":"gateway","protocol":"modbus_tcp","address":address.to_string(),
        "every_ms":3_600_000,"timeout_ms":time::ms(1000),"signals":[
            {"name":"station_one","address":0},
            {"name":"station_five","unit":5.0,"address":10},
            {"name":"gateway_cache","unit":255,"address":0,"elements":2}
        ]}),
    );
    assert_eq!(server.finish(), 3);
    assert_eq!(run.success()["samples"], 3);
    let samples = fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|line| {
            let row = serde_json::from_str::<Value>(line).unwrap();
            (row["kind"] == "sample").then_some(row)
        })
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 3);
    for (sample, (signal, value)) in samples.iter().zip([
        ("station_one", json!(7.0)),
        ("station_five", json!(4660.0)),
        ("gateway_cache", json!([7.0, 43_981.0])),
    ]) {
        assert_eq!(sample["signal"], signal);
        assert_eq!(sample["value"], value);
    }
}

#[test]
fn modbus_tcp_gateway_rejects_source_unit_echo_for_an_override_atomically() {
    let home = Home::new("modbus-tcp-gateway-unit-error");
    let (listener, address) = listener();
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        let request = expect_gateway_request(&mut stream, &stop, GATEWAY_UNIT_ONE);
        modbus_tcp_reply(&mut stream, request, &[3, 2, 0, 7]);
        expect_gateway_request(&mut stream, &stop, GATEWAY_UNIT_FIVE);
        // Correct transaction and response payload with the first unit's ID.
        stream
            .write_all(&[0, 2, 0, 0, 0, 5, 1, 3, 2, 0x12, 0x34])
            .unwrap();
        2
    });
    let run = record_modbus(
        &home,
        &json!({"id":"gateway","protocol":"modbus_tcp","address":address.to_string(),
        "unit":1,"every_ms":3_600_000,"timeout_ms":time::ms(1000),"signals":[
            {"name":"station_one","address":0},
            {"name":"station_five","unit":5,"address":10}
        ]}),
    );
    assert_eq!(server.finish(), 2);
    let error = run.failure(1);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("unit in TCP response"),
        "{error}"
    );
    assert_unpublished(&home);
}
