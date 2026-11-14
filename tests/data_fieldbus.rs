//! Linux packet-socket acquisition through the CLI in a disposable network namespace.
#![cfg(target_os = "linux")]
mod support;

use serde_json::{Value, json};
use std::{
    fs, io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
    process::{Child, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};

const GOOD_PROVIDER: [u8; 6] = [2, 0, 0, 0, 0, 1];
const BAD_PROVIDER: [u8; 6] = [2, 0, 0, 0, 0, 2];

fn isolated() {
    assert_eq!(
        std::env::var("CAPTUREFAB_FIELDBUS_TEST_ISOLATED").as_deref(),
        Ok("1"),
        "run only in a disposable Linux container/network namespace with CAP_NET_RAW"
    );
    assert!(
        std::path::Path::new("/.dockerenv").exists(),
        "the fixture deliberately transmits only on a disposable container's loopback"
    );
}

fn socket() -> OwnedFd {
    let protocol = u16::try_from(libc::ETH_P_ALL).unwrap().to_be();
    // SAFETY: socket creates a new independent descriptor; no pointers are involved.
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            i32::from(protocol),
        )
    };
    assert!(
        fd >= 0,
        "CAP_NET_RAW is required: {}",
        io::Error::last_os_error()
    );
    // SAFETY: socket returned a new owned descriptor.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: the interface name is NUL-terminated and valid for this call.
    let index = unsafe { libc::if_nametoindex(c"lo".as_ptr()) };
    assert!(index > 0);
    // SAFETY: sockaddr_ll is a C structure valid when initialized with zeroes.
    let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    address.sll_family = u16::try_from(libc::AF_PACKET).unwrap();
    address.sll_protocol = protocol;
    address.sll_ifindex = i32::try_from(index).unwrap();
    // SAFETY: the address pointer and size describe one initialized sockaddr_ll.
    let bound = unsafe {
        libc::bind(
            fd,
            (&raw const address).cast(),
            libc::socklen_t::try_from(size_of_val(&address)).unwrap(),
        )
    };
    assert_eq!(bound, 0, "{}", io::Error::last_os_error());
    socket
}

fn send(socket: &OwnedFd, frame: &[u8]) {
    // SAFETY: the slice is readable for its length and the socket remains owned/open.
    let count = unsafe { libc::send(socket.as_raw_fd(), frame.as_ptr().cast(), frame.len(), 0) };
    assert_eq!(
        usize::try_from(count).unwrap(),
        frame.len(),
        "{}",
        io::Error::last_os_error()
    );
}

fn ethernet(provider: [u8; 6], kind: u16, payload: &[u8], tags: &[u16]) -> Vec<u8> {
    let mut frame = vec![0xff; 6];
    frame.extend_from_slice(&provider);
    for tag in tags {
        frame.extend_from_slice(&tag.to_be_bytes());
        frame.extend_from_slice(&[0, 7]);
    }
    frame.extend_from_slice(&kind.to_be_bytes());
    frame.extend_from_slice(payload);
    frame.resize(frame.len().max(60), 0);
    frame
}

fn ethercat(provider: [u8; 6]) -> Vec<u8> {
    // LRD at logical address 0x10000, four input bytes and completed WKC=1.
    let mut datagram = vec![0x0a, 1];
    datagram.extend_from_slice(&0x10000_u32.to_le_bytes());
    datagram.extend_from_slice(&4_u16.to_le_bytes());
    datagram.extend_from_slice(&[0, 0, 0x38, 0xff, 0, 1, 1, 0]);
    let mut payload = (u16::try_from(datagram.len()).unwrap() | 0x1000)
        .to_le_bytes()
        .to_vec();
    payload.extend_from_slice(&datagram);
    ethernet(provider, 0x88a4, &payload, &[0x8100])
}

fn profinet(provider: [u8; 6], valid: bool) -> Vec<u8> {
    let mut payload = vec![0; 46];
    payload[..2].copy_from_slice(&0x8000_u16.to_be_bytes());
    payload[2..5].copy_from_slice(&[0x04, 0xd2, 0x80]);
    payload[42..].copy_from_slice(&[0x12, 0x34, if valid { 0x35 } else { 0x31 }, 0]);
    ethernet(provider, 0x8892, &payload, &[0x88a8, 0x8100])
}

fn configure(home: &Home, provider: [u8; 6]) {
    let source_mac = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        provider[0], provider[1], provider[2], provider[3], provider[4], provider[5]
    );
    fs::write(home.path("sources.json"), json!({"version":1,"sources":[
        {"id":"ethercat","protocol":"ethercat","interface":"lo","logical_address":65536,
         "source_mac":source_mac,"timeout_ms":100,
         "signals":[{"name":"temperature","offset":0,"data_type":"i16","scale":0.1},
                    {"name":"ready","offset":3,"data_type":"bool"}]},
        {"id":"profinet","protocol":"profinet","interface":"lo","frame_id":32768,
         "source_mac":source_mac,"data_length":40,"direction":"outgoing","timeout_ms":100,
         "signals":[{"name":"pressure","offset":0,"data_type":"u16","byte_order":"big","scale":0.01,"status_offset":2}]}
    ]}).to_string()).unwrap();
}

struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            drop(child.kill());
            drop(child.wait());
        }
    }
}

fn capture(home: &Home, fail_quality: bool, provider: [u8; 6]) -> Run {
    let mut command = home.command();
    command
        .args([
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "900ms",
            "-o",
            "run.jsonl",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard(Some(command.spawn().unwrap()));
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let sender = thread::spawn(move || {
        let socket = socket();
        let input = ethercat(provider);
        let good = profinet(provider, true);
        let bad = profinet(provider, false);
        let noise = ethernet(provider, 0x0800, &[0; 1480], &[]);
        let mut other_provider = profinet(provider, false);
        other_provider[6] = 3;
        let mut other_frame = profinet(provider, false);
        other_frame[23] = 1;
        let started = Instant::now();
        while !stopped.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(3) {
            // The capture sockets must discard unrelated IP traffic before it
            // consumes their queue; this also exercises the installed BPF filter.
            for _ in 0..256 {
                send(&socket, &noise);
            }
            send(&socket, &other_provider);
            send(&socket, &other_frame);
            send(&socket, &input);
            send(
                &socket,
                if fail_quality && started.elapsed() > Duration::from_millis(250) {
                    &bad
                } else {
                    &good
                },
            );
            thread::sleep(Duration::from_millis(15));
        }
    });
    let wait_by = Instant::now() + Duration::from_secs(5);
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < wait_by,
            "capture did not stop within its deadline"
        );
        thread::sleep(Duration::from_millis(10));
    }
    done.store(true, Ordering::Release);
    sender.join().unwrap();
    Run::of_output(
        child.0.take().unwrap().wait_with_output().unwrap(),
        &["data", "record"],
    )
}

fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
#[ignore = "requires isolated disposable Docker Linux network namespace and CAP_NET_RAW"]
fn packet_sockets_record_vlan_fieldbus_frames_and_export_typed_samples() {
    isolated();
    let home = Home::new("fieldbus-live-kernel");
    // The two tests share loopback when Rust runs them in parallel. Distinct
    // providers prevent the negative-quality fixture contaminating this run.
    configure(&home, GOOD_PROVIDER);
    let result = capture(&home, false, GOOD_PROVIDER).success();
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let samples = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert!(samples.iter().any(|row| row["source"] == "ethercat"
        && row["signal"] == "temperature"
        && row["value"] == -20.0));
    assert!(samples.iter().any(|row| row["source"] == "ethercat"
        && row["signal"] == "ready"
        && row["value"] == true));
    assert!(
        samples
            .iter()
            .any(|row| row["source"] == "profinet" && row["value"] == 12.34)
    );
    for sample in &samples {
        let time = chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            u64::try_from((time - origin).num_nanoseconds().unwrap()).unwrap(),
            sample["elapsed_ns"].as_u64().unwrap()
        );
    }
    assert_eq!(
        samples.len(),
        usize::try_from(result["samples"].as_u64().unwrap()).unwrap()
    );
    let csv = home.ok(&["data", "export", "run.jsonl", "-o", "run.csv"]);
    assert_eq!(csv["samples"], result["samples"]);
    let jsonl = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "profinet.jsonl",
        "--format",
        "jsonl",
        "--source",
        "profinet",
    ]);
    let exported = fs::read_to_string(home.path("profinet.jsonl")).unwrap();
    let exported: Vec<Value> = exported
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        exported.len(),
        usize::try_from(jsonl["samples"].as_u64().unwrap()).unwrap()
    );
    assert!(
        exported
            .iter()
            .all(|row| row["source"] == "profinet" && row["value"] == 12.34)
    );
}

#[test]
#[ignore = "requires isolated disposable Docker Linux network namespace and CAP_NET_RAW"]
fn invalid_provider_quality_stops_recording_and_preserves_complete_rows() {
    isolated();
    let home = Home::new("fieldbus-quality-kernel");
    configure(&home, BAD_PROVIDER);
    let error = capture(&home, true, BAD_PROVIDER).failure(1);
    assert_eq!(error["code"], "incomplete");
    assert_eq!(error["details"]["status"], "failed");
    let rows = rows(&home);
    assert!(
        rows.iter()
            .any(|row| row["kind"] == "sample" && row["source"] == "profinet")
    );
    assert_eq!(rows.last().unwrap()["status"], "failed");
    let recovered = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "recovered.jsonl",
        "--format",
        "jsonl",
        "--allow-incomplete",
    ]);
    assert_eq!(recovered["samples"], error["details"]["samples"]);
}
