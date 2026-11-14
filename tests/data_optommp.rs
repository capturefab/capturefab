//! `OptoMMP` through the real CLI against a private fake controller; no physical device claim.
mod support;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

const UNIT_TYPE: u64 = 0xFFFF_F030_0020;
const PUC: u64 = 0xFFFF_F030_0004;
const MM1: u32 = 0xF000_0022;
/// Published manufacturer control word (form 1465 printed p98): 15.5.
const PUBLISHED: u32 = 0x4178_0000;

fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Darwin accepted sockets inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream.set_nodelay(true).unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(2));
            }
            other => panic!("private OptoMMP accept: {other:?}"),
        }
    }
}

/// Request roles observed by the fake: (label, address).
type Log = Arc<Mutex<Vec<(u8, u64)>>>;

enum Reply {
    Word(u32),
    Nak(u32),
}

/// Answers each `ReadQuadlet` from `map`; `fail` overrides the n-th request (0-based).
fn serve(
    listener: TcpListener,
    map: BTreeMap<u64, u32>,
    fail: Option<(usize, Reply)>,
    log: Log,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stream = accept(&listener);
        let mut index = 0;
        loop {
            let mut request = [0; 12];
            match stream.read(&mut request[..1]) {
                Ok(0) => break,
                Ok(_) => stream.read_exact(&mut request[1..]).unwrap(),
                Err(error) => panic!("fake controller read: {error}"),
            }
            assert_eq!(&request[..2], &[0, 0], "destination ID");
            assert_eq!(request[2] & 3, 0, "retry bits");
            assert_eq!(request[3], 0x40, "only ReadQuadlet");
            assert_eq!(&request[4..6], &[0, 0], "source ID");
            let label = request[2] >> 2;
            let mut address = [0; 8];
            address[2..].copy_from_slice(&request[6..]);
            let address = u64::from_be_bytes(address);
            log.lock().unwrap().push((label, address));
            let mut response = vec![0, 0, label << 2, 0x60, 0x12, 0x34, 0, 0, 0, 0, 0, 0];
            match fail.as_ref().filter(|(at, _)| *at == index) {
                Some((_, Reply::Nak(code))) => {
                    response[6] = u8::try_from(*code).unwrap() << 4;
                    stream.write_all(&response).unwrap();
                    break;
                }
                Some((_, Reply::Word(word))) => response.extend_from_slice(&word.to_be_bytes()),
                None => response.extend_from_slice(
                    &map.get(&address)
                        .copied()
                        .unwrap_or_else(|| panic!("unexpected address {address:012x}"))
                        .to_be_bytes(),
                ),
            }
            stream.write_all(&response).unwrap();
            index += 1;
        }
        // Drain to end of stream: the client never sends anything after a failure.
        let mut rest = Vec::new();
        drop(stream.read_to_end(&mut rest));
        assert_eq!(rest, Vec::<u8>::new(), "no request after a failed exchange");
        // No reconnect: a second connection never arrives.
        listener.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "unexpected reconnect"
        );
    })
}

/// RIO MM1: analog channel 0 and digital channel 3.
fn rio_map() -> BTreeMap<u64, u32> {
    BTreeMap::from([
        (UNIT_TYPE, 0x103),
        (PUC, 0),
        (0xFFFF_F010_0000, MM1),
        (0xFFFF_F010_0004, 0x6000_0015),
        (0xFFFF_F010_0240, MM1),
        (0xFFFF_F010_0244, 0x5400_007A),
        (0xFFFF_F026_0000, PUBLISHED),
        (0xFFFF_F026_002C, 0x0000_ABCD),
        (0xFFFF_F01E_00C0, 0x8000_0001),
        (0xFFFF_F01E_00D4, 0),
    ])
}
fn rio_channels() -> Value {
    json!([
        {"name":"flow","module":0,"channel":0,"resource":"analog_eu",
            "expected_module_id":MM1,"expected_channel_type":0x6000_0015_u32},
        {"name":"door","module":0,"channel":3,"resource":"digital_state",
            "expected_module_id":MM1,"expected_channel_type":0x5400_007A_u32}
    ])
}
fn configure(home: &Home, address: &str, model: &str, channels: &Value) {
    fs::write(
        home.path("sources.json"),
        serde_json::to_vec(&json!({
            "version":1,"sources":[{"id":"edge","protocol":"optommp",
                "profile":"groov_expanded_inputs","model":model,"address":address,
                "every_ms":100,"timeout_ms":1000,"channels":channels}]
        }))
        .unwrap(),
    )
    .unwrap();
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn samples(rows: &[Value]) -> Vec<&Value> {
    rows.iter().filter(|row| row["kind"] == "sample").collect()
}
fn args() -> [&'static str; 8] {
    [
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "450ms",
        "-o",
        "run.jsonl",
    ]
}

#[test]
fn complete_polls_preserve_native_words_quality_and_round_trip_to_csv() {
    let home = Home::new("optommp-polls");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    configure(
        &home,
        &listener.local_addr().unwrap().to_string(),
        "rio_mm1",
        &rio_channels(),
    );
    let log = Log::default();
    let peer = serve(listener, rio_map(), None, log.clone());
    let result = home.ok(&args());
    peer.join().unwrap();
    let records = rows(&home);
    let samples = samples(&records);
    let count = samples.len();
    assert!(
        count >= 4 && count.is_multiple_of(2),
        "whole polls only: {count}"
    );
    assert_eq!(result["samples"], count);
    let [flow, door] = &samples[..2] else {
        panic!("two channels per poll")
    };
    assert_eq!(flow["signal"], "flow");
    assert_eq!(flow["value"]["value"], 15.5);
    assert_eq!(flow["value"]["raw_hex"], "41780000");
    assert_eq!(flow["value"]["classification"], "finite");
    assert_eq!(flow["value"]["quality_code"], 0xABCD);
    assert_eq!(flow["value"]["value_source_id"], 0x1234);
    assert_eq!(flow["value"]["requested_value_address"], "fffff0260000");
    assert_eq!(flow["value"]["requested_quality_address"], "fffff026002c");
    assert_eq!(
        flow["value"]["units_provenance"],
        "externally_configured_unqueried"
    );
    assert_eq!(door["value"]["value"], 0x8000_0001_u32);
    assert_eq!(door["value"]["encoding"], "unsigned32_be");
    assert_eq!(door["value"]["requested_value_address"], "fffff01e00c0");
    // One generic time per poll; per-response clocks stay inside each value.
    assert_eq!(flow["elapsed_ns"], door["elapsed_ns"]);
    assert_eq!(
        flow["value"]["poll_observed_ns"],
        door["value"]["poll_observed_ns"]
    );
    assert!(
        flow["value"]["host_value_observed_ns"].as_u64()
            < door["value"]["host_value_observed_ns"].as_u64()
    );
    assert_eq!(samples.last().unwrap()["value"]["poll_ordinal"], count / 2);
    assert!(samples.iter().all(|row| row.get("source_time").is_none()));
    assert_eq!(records.last().unwrap()["status"], "complete");
    // Ten requests per poll in the documented order; labels step mod 64 from 1.
    let log = log.lock().unwrap();
    let order = [
        UNIT_TYPE,
        PUC,
        0xFFFF_F010_0000,
        0xFFFF_F010_0004,
        0xFFFF_F010_0240,
        0xFFFF_F010_0244,
        0xFFFF_F026_0000,
        0xFFFF_F026_002C,
        0xFFFF_F01E_00C0,
        0xFFFF_F01E_00D4,
    ];
    assert_eq!(log.len(), order.len() * (count / 2));
    for (index, (label, address)) in log.iter().enumerate() {
        assert_eq!(usize::from(*label), (index + 1) % 64);
        assert_eq!(*address, order[index % order.len()]);
    }
    drop(log);
    let exported = home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    assert_eq!(exported["samples"], count);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert!(
        csv.contains("41780000") && csv.contains("80000001"),
        "{csv}"
    );
}

#[test]
fn native_error_mid_poll_withholds_the_poll_and_keeps_earlier_ones() {
    let home = Home::new("optommp-nak");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    configure(
        &home,
        &listener.local_addr().unwrap().to_string(),
        "rio_mm1",
        &rio_channels(),
    );
    // Second poll: NAK code 14 on the digital value read (request 19, 0-based 18).
    let peer = serve(
        listener,
        rio_map(),
        Some((18, Reply::Nak(14))),
        Log::default(),
    );
    let error = home.fails(1, &args());
    peer.join().unwrap();
    assert_eq!(error["details"]["samples"], 2);
    let native = &error["details"]["optommp_response"];
    assert_eq!(native["response_code"], 14);
    assert_eq!(native["transaction_label"], 19);
    assert_eq!(native["source_id"], 0x1234);
    assert_eq!(native["raw_header_hex"], "00004c601234e00000000000");
    assert_eq!(native["requested_address"], "fffff01e00c0");
    let records = rows(&home);
    assert_eq!(samples(&records).len(), 2);
    assert_eq!(records.last().unwrap()["status"], "failed");
    let exported = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "--allow-incomplete",
        "-o",
        "partial.csv",
    ]);
    assert_eq!(exported["samples"], 2);
}

#[test]
fn identity_failures_stop_before_value_reads_without_retry() {
    for (index, word, reason, requests) in [
        (0, 0x108, "unit_type_mismatch", 1),
        (1, 1, "puc_not_cleared", 2),
        // Channel 0 reconfigured as an analog output.
        (3, 0xA000_0020, "channel_type_mismatch", 4),
        (4, 0, "module_id_mismatch", 5),
    ] {
        let home = Home::new("optommp-identity");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        configure(
            &home,
            &listener.local_addr().unwrap().to_string(),
            "rio_mm1",
            &rio_channels(),
        );
        let log = Log::default();
        let peer = serve(
            listener,
            rio_map(),
            Some((index, Reply::Word(word))),
            log.clone(),
        );
        let error = home.fails(1, &args());
        peer.join().unwrap();
        assert_eq!(error["details"]["reason"], reason);
        assert_eq!(error["details"]["samples"], 0);
        assert_eq!(log.lock().unwrap().len(), requests, "{reason}");
        assert_eq!(rows(&home).last().unwrap()["status"], "failed");
    }
}

#[test]
fn invalid_selectors_fail_before_creating_output_or_connecting() {
    let home = Home::new("optommp-preflight");
    let channel = rio_channels()[0].clone();
    let mut output = channel.clone();
    output["expected_channel_type"] = json!(0x9000_007B_u32);
    let mut relay = channel.clone();
    relay["channel"] = json!(8);
    let mut energy = channel.clone();
    energy["expected_module_id"] = json!(0x6000_002D_u32);
    let mut raw = channel.clone();
    raw["address"] = json!("fffff01c0000");
    for (model, channels, address) in [
        ("rio_mm1", json!([output]), "127.0.0.1:2001"),
        ("rio_mm1", json!([relay]), "127.0.0.1:2001"),
        ("rio_mm1", json!([energy]), "127.0.0.1:2001"),
        ("rio_mm1", json!([raw]), "127.0.0.1:2001"),
        ("rio_mm2", json!([channel.clone()]), "127.0.0.1:2001"),
        ("rio_mm1", json!([channel]), "255.255.255.255:2001"),
    ] {
        configure(&home, address, model, &channels);
        let error = home.fails(2, &args());
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}
