//! Independent YRC1000 HSES read fixtures; no physical controller or safety claim.
#[path = "support/mod.rs"]
mod support;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs, io,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use support::cli::{Home, Run};
use support::time;

const PROFILE: &str = "yrc1000_hw1483358_rev12";
const PACKET_PINS: &str = "d2e1bcd2dfa6864958bf9fbcdbc911ad67338cc927e44a281e75899fdf8f2c32";

fn packet_literals() -> Value {
    let text = include_str!("fixtures/yaskawa_hses/packets.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(text.as_bytes())),
        PACKET_PINS
    );
    serde_json::from_str(text).unwrap()
}
fn unhex(text: &str) -> Vec<u8> {
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    assert_eq!(tail, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    result
}
fn literal(name: &str) -> Vec<u8> {
    let all = packet_literals();
    let selected = &all["packets"][name];
    let bytes = unhex(selected["hex"].as_str().unwrap());
    assert_eq!(
        bytes.len(),
        usize::try_from(selected["bytes"].as_u64().unwrap()).unwrap()
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        selected["sha256"].as_str().unwrap()
    );
    bytes
}

#[derive(Clone)]
struct Spec {
    signal: String,
    resource: &'static str,
    index: Option<u16>,
    group: Option<u8>,
    command: u16,
    instance: u16,
    attribute: u8,
    service: u8,
    response: Vec<u8>,
    data: Value,
}
impl Spec {
    fn config(&self) -> Value {
        let mut value = json!({"signal":self.signal,"resource":self.resource});
        if let Some(index) = self.index {
            value["index"] = json!(index);
        }
        if let Some(group) = self.group {
            value["group"] = json!(group);
        }
        value
    }
    fn request(&self, id: u8) -> Vec<u8> {
        let mut request = literal("status_request");
        request[11] = id;
        request[24..26].copy_from_slice(&self.command.to_le_bytes());
        request[26..28].copy_from_slice(&self.instance.to_le_bytes());
        request[28] = self.attribute;
        request[29] = self.service;
        request
    }
    fn reply(&self, id: u8) -> Vec<u8> {
        let mut response = self.response.clone();
        response[11] = id;
        response
    }
}
fn status_data(unknown: bool) -> Value {
    if unknown {
        json!({"words_raw":[2_147_483_869_u32,2_147_483_726_u32],"flags":{"step":true,"one_cycle":false,"auto":true,"running":true,"teach":false,"play":true,"command_remote":true,"pendant_hold":true,"command_hold":true,"alarm":false,"error":false,"servo_on":true},"word1_bit4_raw":true,"word2_bit2_raw":true,"unqualified_bits":[2_147_483_664_u32,2_147_483_652_u32],"conflicting_semantics":"unqualified_manual_version"})
    } else {
        json!({"words_raw":[34,0],"flags":{"step":false,"one_cycle":true,"auto":false,"running":false,"teach":true,"play":false,"command_remote":false,"pendant_hold":false,"command_hold":false,"alarm":false,"error":false,"servo_on":false},"word1_bit4_raw":false,"word2_bit2_raw":false,"unqualified_bits":[0,0],"conflicting_semantics":"unqualified_manual_version"})
    }
}
fn status(signal: &str, unknown: bool) -> Spec {
    Spec {
        signal: signal.into(),
        resource: "status",
        index: None,
        group: None,
        command: 0x72,
        instance: 1,
        attribute: 0,
        service: 1,
        response: literal(if unknown {
            "status_unknown_response"
        } else {
            "status_response"
        }),
        data: status_data(unknown),
    }
}
fn current_position(signal: &str, group: u8, unknown: bool) -> Spec {
    let mut response = literal("base_position_response");
    let coordinate_type = if unknown { 99 } else { 16 };
    response[32..36].copy_from_slice(&u32::try_from(coordinate_type).unwrap().to_le_bytes());
    let mut data = json!({"coordinate_type_raw":coordinate_type,"figure_raw":4_660,"tool_number_raw":3,"user_coordinate_raw":0,"extended_figure_raw":165,"coordinates_raw":[-100_000,250_000,0,-900_000,450_000,1_800_000,7,-8],"units_status":if unknown {"unqualified_returned_type"}else{"documented_base_units"}});
    if !unknown {
        data["position_um"] = json!([-100_000, 250_000, 0]);
        data["orientation_1e4_degree"] = json!([-900_000, 450_000, 1_800_000]);
        data["remaining_slots_raw"] = json!([7, -8]);
    }
    Spec {
        signal: signal.into(),
        resource: "base_position",
        index: None,
        group: Some(group),
        command: 0x75,
        instance: 100 + u16::from(group),
        attribute: 0,
        service: 1,
        response,
        data,
    }
}
fn variable(signal: &str, resource: &'static str, index: u16, reply: &str, value: Value) -> Spec {
    let command = match resource {
        "byte" => 0x7a,
        "integer" => 0x7b,
        "double_integer" => 0x7c,
        "real_raw" => 0x7d,
        "string32" => 0x8c,
        _ => panic!("not a closed read variable"),
    };
    let mut data = json!({"index":index,"allocation_origin":"configured_unverified"});
    match resource {
        "real_raw" => {
            data["word_raw"] = value;
            data["interpretation"] = json!("unqualified_float_wire_encoding");
        }
        "string32" => {
            data["bytes_raw"] = value;
            data["encoding"] = json!("unqualified");
        }
        _ => data["value"] = value,
    }
    Spec {
        signal: signal.into(),
        resource,
        index: Some(index),
        group: None,
        command,
        instance: index,
        attribute: 1,
        service: 0x0e,
        response: literal(reply),
        data,
    }
}
fn specs() -> Vec<Spec> {
    let string: [u8; 32] = *b"AB\0CDEFGHIJKLMNOqrstuvwxyz123456";
    let mut specs = vec![
        status("status", false),
        status("unknown", true),
        current_position("base", 1, false),
        variable("byte", "byte", 7, "byte7_response", json!(165)),
        variable("integer", "integer", 8, "integer8_response", json!(-12_345)),
        variable(
            "double",
            "double_integer",
            9,
            "double9_response",
            json!(-123_456_789),
        ),
        variable(
            "real",
            "real_raw",
            10,
            "real10_raw_response",
            json!(1_069_547_520_u32),
        ),
        variable("string", "string32", 11, "string32_response", json!(string)),
        current_position("unknown_type", 8, true),
    ];
    let mut real = variable(
        "raw_high",
        "real_raw",
        u16::MAX,
        "real10_raw_response",
        json!(4_290_772_993_u32),
    );
    real.response[32..36].copy_from_slice(&[1, 0, 0xc0, 0xff]);
    specs.push(real);
    let mut byte = variable("byte_high", "byte", u16::MAX, "byte7_response", json!(255));
    byte.response[32] = 255;
    specs.push(byte);
    let mut integer = variable(
        "integer_min",
        "integer",
        99,
        "integer8_response",
        json!(-32_768),
    );
    integer.response[32..34].copy_from_slice(&[0, 0x80]);
    specs.push(integer);
    let mut double = variable(
        "double_min",
        "double_integer",
        99,
        "double9_response",
        json!(-2_147_483_648_i64),
    );
    double.response[32..36].copy_from_slice(&[0, 0, 0, 0x80]);
    specs.push(double);
    specs
}
fn reads(specs: &[Spec]) -> Value {
    Value::Array(specs.iter().map(Spec::config).collect())
}

struct Seen {
    peer: SocketAddr,
    id: u8,
    request: Vec<u8>,
}
struct Emission {
    bytes: Vec<u8>,
    wrong_peer: bool,
    delay: Duration,
}
fn emit(bytes: Vec<u8>) -> Emission {
    Emission {
        bytes,
        wrong_peer: false,
        delay: Duration::ZERO,
    }
}
struct Peer {
    socket: UdpSocket,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<Seen>>>,
}
impl Peer {
    fn new(
        count: usize,
        mut handler: impl FnMut(usize, &[u8], SocketAddr) -> Vec<Emission> + Send + 'static,
    ) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = socket.try_clone().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let wrong = UdpSocket::bind("127.0.0.1:0").unwrap();
        let handle = thread::spawn(move || {
            let until = Instant::now() + time::secs(8);
            let mut buffer = vec![0; 65_535];
            let mut seen = Vec::new();
            while !stopped.load(Ordering::SeqCst) {
                assert!(
                    Instant::now() < until,
                    "HSES fixture exceeded its bounded lifetime"
                );
                match server.recv_from(&mut buffer) {
                    Ok((length, peer)) => {
                        assert!(
                            seen.len() < count,
                            "unexpected write, retry or further HSES read"
                        );
                        assert_eq!(length, 32, "all admitted commands have no request payload");
                        let request = &buffer[..length];
                        let id = request[11];
                        let messages = handler(seen.len(), request, peer);
                        seen.push(Seen {
                            peer,
                            id,
                            request: request.to_vec(),
                        });
                        for message in messages {
                            thread::sleep(message.delay);
                            let output = if message.wrong_peer { &wrong } else { &server };
                            match output.send_to(&message.bytes, peer) {
                                Ok(sent) => assert_eq!(sent, message.bytes.len()),
                                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
                                Err(error) => panic!("{error}"),
                            }
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::ConnectionRefused
                                | io::ErrorKind::ConnectionReset
                        ) => {}
                    Err(error) => panic!("{error}"),
                }
            }
            assert_eq!(seen.len(), count, "missing expected read datagram");
            seen
        });
        Self {
            socket,
            stop,
            handle: Some(handle),
        }
    }
    fn source(&self, reads: Value) -> Value {
        let mut source = json!({"id":"robot","protocol":"yaskawa_hses","profile":PROFILE,"address":self.socket.local_addr().unwrap().to_string(),"reply_horizon_ms":2000,"every_ms":3_600_000,"timeout_ms":500});
        source["reads"] = reads;
        source
    }
    fn finish(mut self) -> Vec<Seen> {
        self.stop.store(true, Ordering::SeqCst);
        let seen = self.handle.take().unwrap().join().unwrap();
        self.socket.set_nonblocking(true).unwrap();
        let mut extra = [0; 33];
        // A delayed send to a retired client may leave asynchronous ICMP
        // (ConnectionReset on Windows). Consume only those errors; every
        // actual datagram is still forbidden.
        for _ in 0..32 {
            match self.socket.recv_from(&mut extra) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return seen,
                Err(error) => panic!("unexpected final UDP error: {error}"),
                Ok(_) => panic!("queued unexpected HSES request"),
            }
        }
        panic!("asynchronous UDP errors exceeded the bounded final check")
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn record(home: &Home, sources: &Value, duration: &str) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        duration,
        "-o",
        "run.jsonl",
    ])
}
fn rows(home: &Home, name: &str) -> Vec<Value> {
    fs::read_to_string(home.path(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn recorded(home: &Home, status: &str) -> Vec<Value> {
    let rows = rows(home, "run.jsonl");
    assert_eq!(rows[0]["kind"], "header");
    assert_eq!(rows.last().unwrap()["status"], status);
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let samples: Vec<_> = rows
        .into_iter()
        .filter(|row| row["kind"] == "sample")
        .collect();
    let mut prior = std::collections::BTreeMap::new();
    for (index, sample) in samples.iter().enumerate() {
        assert_eq!(sample["seq"], index + 1);
        assert!(sample.get("source_time").is_none());
        let elapsed = sample["elapsed_ns"].as_u64().unwrap();
        let old = prior
            .insert(sample["source"].as_str().unwrap(), elapsed)
            .unwrap_or(0);
        assert!(elapsed >= old);
        let time = chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            time,
            origin + chrono::Duration::nanoseconds(i64::try_from(elapsed).unwrap())
        );
        assert!(sample["read_duration_ns"].as_u64().unwrap() <= elapsed);
        sample["delivery_latency_ns"].as_u64().unwrap();
    }
    samples
}
fn check_native(sample: &Value, spec: &Spec, seen: &Seen, horizon: u64) {
    assert_eq!(sample["signal"], spec.signal);
    let value = &sample["value"];
    assert_eq!(value["kind"], "yaskawa_hses");
    assert_eq!(value["profile"], PROFILE);
    assert_eq!(value["resource"], spec.resource);
    assert_eq!(
        value["request"],
        json!({"request_id":seen.id,"command":spec.command,"instance":spec.instance,"attribute":spec.attribute,"service":spec.service})
    );
    assert_eq!(
        value["correlation"],
        json!({"scope":"source_epoch_only","basis":"operator_declared_reply_horizon","reply_horizon_ms":horizon,"source_port":seen.peer.port()})
    );
    assert_eq!(
        value["response"],
        json!({"service_raw":spec.service|0x80,"status_raw":0,"added_status_words":0,"added_status_raw":0,"added_status_bytes_hex":"00000000"})
    );
    assert_eq!(value["raw_response_hex"], hex(&spec.reply(seen.id)));
    assert_eq!(value["data"], spec.data);
    assert_eq!(
        value["host_window_basis"],
        "request_admission_through_datagram_received_including_quarantine"
    );
    let start = value["host_window_ns"]["start"].as_u64().unwrap();
    let end = value["host_window_ns"]["end"].as_u64().unwrap();
    assert!(start > 0 && end >= start);
    assert_eq!(seen.request, spec.request(seen.id));
}
fn exports(home: &Home, samples: &[Value], incomplete: bool) {
    for (format, path) in [("csv", "values.csv"), ("jsonl", "values.jsonl")] {
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            path,
        ];
        if incomplete {
            args.push("--allow-incomplete");
        }
        home.ok(&args);
    }
    let projected: Vec<_> = samples
        .iter()
        .map(|sample| {
            let mut sample = sample.clone();
            sample.as_object_mut().unwrap().remove("kind");
            sample
        })
        .collect();
    assert_eq!(rows(home, "values.jsonl"), projected);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), samples.len() + 1);
    for sample in samples {
        let names = format!(
            ",\"{}\",\"{}\",",
            sample["source"].as_str().unwrap(),
            sample["signal"].as_str().unwrap()
        );
        let field = format!("\"{}\"", sample["value"].to_string().replace('"', "\"\""));
        assert!(
            csv.lines()
                .any(|line| line.contains(&names) && line.ends_with(&field)),
            "missing opaque native CSV value"
        );
    }
}

fn remote_error(spec: &Spec, id: u8, status: u8, words: u8, added: u32) -> Vec<u8> {
    let mut reply = literal("attribute_error_response");
    reply[11] = id;
    reply[24] = spec.service | 0x80;
    reply[25] = status;
    reply[26] = words;
    reply[28..32].copy_from_slice(&added.to_le_bytes());
    reply
}
fn check_error(error: &Value, status: u8, words: u8, added: u32) {
    assert_eq!(error["code"], "failed");
    assert_eq!(error["details"]["hses_status"], status);
    assert_eq!(error["details"]["hses_added_status_words"], words);
    assert_eq!(error["details"]["hses_added_status_raw"], added);
    assert_eq!(
        error["details"]["hses_added_status_bytes_hex"],
        hex(&added.to_le_bytes())
    );
}

#[test]
fn immutable_primary_and_spec_packets_have_exact_extent_and_hash() {
    let all = packet_literals();
    let packets = all["packets"].as_object().unwrap();
    assert_eq!(packets.len(), 13);
    for name in packets.keys() {
        let bytes = literal(name);
        assert_eq!(&bytes[..4], b"YERC");
        assert_eq!(&bytes[4..6], &[32, 0]);
        assert_eq!(
            bytes.len(),
            32 + usize::from(u16::from_le_bytes([bytes[6], bytes[7]]))
        );
    }
    assert_eq!(
        status("status", false).request(0),
        literal("status_request")
    );
    assert_eq!(status("status", false).reply(0), literal("status_response"));
}

#[test]
fn closed_reads_preserve_integer_words_string_storage_position_units_and_exports() {
    let expected = specs();
    let served = expected.clone();
    let peer = Peer::new(expected.len(), move |index, request, _| {
        let id = u8::try_from(index).unwrap();
        let spec = &served[index];
        assert_eq!(request, spec.request(id));
        let correct = spec.reply(id);
        if index == 0 {
            // The wrong-source packet has the matching ID but poisoned words.
            // The valid old-ID packet is from the actual configured endpoint.
            let mut poison = correct.clone();
            poison[32..40].fill(0xff);
            let mut stale = poison.clone();
            stale[11] = u8::MAX;
            vec![
                Emission {
                    bytes: poison,
                    wrong_peer: true,
                    delay: Duration::ZERO,
                },
                emit(stale),
                Emission {
                    bytes: correct,
                    wrong_peer: false,
                    delay: Duration::from_millis(10),
                },
            ]
        } else {
            vec![emit(correct)]
        }
    });
    let home = Home::new("yaskawa-hses-complete");
    let source = peer.source(reads(&expected));
    let result=record(&home,&json!([source,{"id":"cycle","protocol":"simulator","signal":"cycle","every_ms":3_600_000}]),"400ms").success();
    let seen = peer.finish();
    assert_eq!(result["samples"], expected.len() + 1);
    let samples = recorded(&home, "complete");
    let native: Vec<_> = samples
        .iter()
        .filter(|sample| sample["source"] == "robot")
        .collect();
    assert_eq!(native.len(), expected.len());
    for ((sample, spec), packet) in native.into_iter().zip(&expected).zip(&seen) {
        check_native(sample, spec, packet, 2000);
    }
    let simulated: Vec<_> = samples
        .iter()
        .filter(|sample| sample["source"] == "cycle")
        .collect();
    assert_eq!(simulated.len(), 1);
    assert_eq!(simulated[0]["value"], 0);
    exports(&home, &samples, false);
}

#[test]
fn correlated_malformed_and_remote_status_errors_retain_only_the_prior_poll() {
    let spec = variable("byte", "byte", 7, "byte7_response", json!(165));
    for mode in 0..8 {
        let served = spec.clone();
        let peer = Peer::new(2, move |index, request, _| {
            let id = u8::try_from(index).unwrap();
            assert_eq!(request, served.request(id));
            let mut response = served.reply(id);
            if index == 1 {
                match mode {
                    0 => {
                        response.push(0);
                        response[6..8].copy_from_slice(&2_u16.to_le_bytes());
                    }
                    1 => response[6..8].copy_from_slice(&2_u16.to_le_bytes()),
                    2 => response[12..16].copy_from_slice(&0_u32.to_le_bytes()),
                    3 => response[24] = 0x81,
                    4 => {
                        response.resize(512, 0);
                        response[6..8].copy_from_slice(&480_u16.to_le_bytes());
                    }
                    5 => response = remote_error(&served, id, 0x1f, 1, 0xa002),
                    6 => response = remote_error(&served, id, 0, 1, 1),
                    _ => response = remote_error(&served, id, 0x28, 2, 0xaabb_ccdd),
                }
            }
            vec![emit(response)]
        });
        let home = Home::new("yaskawa-hses-prefix");
        let mut source = peer.source(reads(std::slice::from_ref(&spec)));
        source["every_ms"] = json!(200);
        let error = record(&home, &json!([source]), "1s").failure(1);
        let seen = peer.finish();
        match mode {
            5 => check_error(&error, 0x1f, 1, 0xa002),
            6 => check_error(&error, 0, 1, 1),
            7 => check_error(&error, 0x28, 2, 0xaabb_ccdd),
            _ => assert_eq!(error["code"], "incomplete"),
        }
        let samples = recorded(&home, "failed");
        assert_eq!(samples.len(), 1);
        check_native(&samples[0], &spec, &seen[0], 2000);
        exports(&home, &samples, true);
    }
}

#[test]
fn late_sibling_has_no_partial_poll_and_shared_deadline_cannot_be_renewed() {
    let pair = vec![
        variable("byte", "byte", 7, "byte7_response", json!(165)),
        variable("integer", "integer", 8, "integer8_response", json!(-12_345)),
        variable(
            "never",
            "double_integer",
            9,
            "double9_response",
            json!(-123_456_789),
        ),
    ];
    for mode in 0..3 {
        let served = pair.clone();
        let peer = Peer::new(2, move |index, request, _| {
            let id = u8::try_from(index).unwrap();
            let spec = &served[index];
            assert_eq!(request, spec.request(id));
            let mut response = spec.reply(id);
            if mode == 0 {
                if index == 1 {
                    response.push(0);
                    response[6..8].copy_from_slice(&3_u16.to_le_bytes());
                }
                vec![emit(response)]
            } else {
                let mut stale = response.clone();
                stale[11] = id.wrapping_sub(1);
                vec![
                    Emission {
                        bytes: stale,
                        wrong_peer: false,
                        delay: time::millis(50),
                    },
                    Emission {
                        bytes: response,
                        wrong_peer: false,
                        delay: time::millis(100),
                    },
                ]
            }
        });
        let home = Home::new("yaskawa-hses-atomic-deadline");
        let mut source = peer.source(reads(&pair));
        source["reply_horizon_ms"] = json!(time::ms(2000));
        source["timeout_ms"] = json!(time::ms(if mode == 2 { 2000 } else { 250 }));
        let error = record(
            &home,
            &json!([source]),
            &time::arg(if mode == 2 { 250 } else { 1000 }),
        )
        .failure(if mode == 0 { 1 } else { 4 });
        peer.finish();
        assert_eq!(
            error["code"],
            if mode == 0 { "incomplete" } else { "timeout" }
        );
        assert_eq!(recorded(&home, "failed").len(), 0);
    }
}

fn many_status() -> Vec<Spec> {
    (0..32)
        .map(|index| status(&format!("status{index}"), false))
        .collect()
}

#[test]
fn id_exhaustion_rotates_the_ephemeral_source_epoch_before_reusing_zero() {
    let expected = many_status();
    let served = expected.clone();
    let peer = Peer::new(257, move |index, request, _| {
        let id = u8::try_from(index % 256).unwrap();
        let spec = &served[index % 32];
        assert_eq!(request, spec.request(id));
        if index == 256 {
            let mut stale = spec.reply(u8::MAX);
            stale[32..40].fill(0xff);
            vec![emit(stale), emit(remote_error(spec, id, 0x1f, 1, 0xa002))]
        } else {
            vec![emit(spec.reply(id))]
        }
    });
    let home = Home::new("yaskawa-hses-epochs");
    let mut source = peer.source(reads(&expected));
    source["every_ms"] = json!(1);
    let error = record(&home, &json!([source]), "2s").failure(1);
    let seen = peer.finish();
    check_error(&error, 0x1f, 1, 0xa002);
    assert_eq!(seen.len(), 257);
    let original = seen[0].peer;
    assert!(original.is_ipv4());
    assert!(seen[..256].iter().all(|packet| packet.peer == original));
    assert_ne!(seen[256].peer.port(), original.port());
    assert_eq!(seen[256].id, 0);
    let samples = recorded(&home, "failed");
    assert_eq!(samples.len(), 256);
    for (index, sample) in samples.iter().enumerate() {
        check_native(sample, &expected[index % 32], &seen[index], 2000);
    }
    exports(&home, &samples, true);
}

#[test]
fn a_fixed_bind_honors_the_operator_quarantine_without_a_257th_request() {
    let reservation = UdpSocket::bind("127.0.0.1:0").unwrap();
    let local = reservation.local_addr().unwrap();
    let expected = many_status();
    let served = expected.clone();
    let peer = Peer::new(256, move |index, request, source| {
        assert_eq!(source, local);
        let id = u8::try_from(index).unwrap();
        let spec = &served[index % 32];
        assert_eq!(request, spec.request(id));
        if index == 255 {
            let mut poison = spec.reply(0);
            poison[32..40].fill(0xff);
            vec![
                emit(spec.reply(id)),
                Emission {
                    bytes: poison,
                    wrong_peer: false,
                    delay: Duration::from_millis(1),
                },
            ]
        } else {
            vec![emit(spec.reply(id))]
        }
    });
    // Reserve the fixed client port until both fake-server sockets exist.
    drop(reservation);
    let home = Home::new("yaskawa-hses-quarantine");
    let mut source = peer.source(reads(&expected));
    source["every_ms"] = json!(1);
    source["reply_horizon_ms"] = json!(30_000);
    source["bind"] = json!(local.to_string());
    let error = record(&home, &json!([source]), "2s").failure(4);
    let seen = peer.finish();
    assert_eq!(error["code"], "timeout");
    assert_eq!(seen.len(), 256);
    let samples = recorded(&home, "failed");
    assert_eq!(samples.len(), 256);
    for (index, sample) in samples.iter().enumerate() {
        check_native(sample, &expected[index % 32], &seen[index], 30_000);
    }
    exports(&home, &samples, true);
}

#[test]
fn static_profile_horizon_read_and_endpoint_errors_fail_before_output() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let base = json!({"id":"robot","protocol":"yaskawa_hses","profile":PROFILE,"address":socket.local_addr().unwrap().to_string(),"reply_horizon_ms":2000,"timeout_ms":500,"reads":[{"signal":"state","resource":"status"}]});
    for patch in [
        json!({"profile":"dx200"}),
        json!({"address":"127.0.0.1:0"}),
        json!({"address":"localhost:10040"}),
        json!({"address":"239.1.2.3:10040"}),
        json!({"reply_horizon_ms":0}),
        json!({"reply_horizon_ms":30_001}),
        json!({"reply_horizon_ms":499}),
        json!({"reads":[]}),
        json!({"reads":[{"signal":"same","resource":"status"},{"signal":"same","resource":"status"}]}),
        json!({"reads":[{"signal":"position","resource":"base_position","group":0}]}),
        json!({"reads":[{"signal":"position","resource":"base_position","group":9}]}),
        json!({"reads":[{"signal":"position","resource":"base_position"}]}),
        json!({"reads":[{"signal":"position","resource":"base_position","group":1,"index":1}]}),
        json!({"reads":[{"signal":"byte","resource":"byte"}]}),
        json!({"reads":[{"signal":"byte","resource":"byte","index":65_536}]}),
        json!({"reads":[{"signal":"status","resource":"status","index":1}]}),
        json!({"reads":[{"signal":"string","resource":"string16_prefix","index":0}]}),
        json!({"command":0x10}),
    ] {
        let home = Home::new("yaskawa-hses-preopen");
        let mut source = base.clone();
        for (key, value) in patch.as_object().unwrap() {
            source[key].clone_from(value);
        }
        assert_eq!(
            record(&home, &json!([source]), "1s").failure(2)["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
        let mut packet = [0; 33];
        assert!(
            matches!(socket.recv_from(&mut packet),Err(error) if error.kind()==io::ErrorKind::WouldBlock)
        );
    }
}
