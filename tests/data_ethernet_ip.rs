//! Actual CLI fixtures for Rockwell 1756-PM020 Read Tag/Read Tag Fragmented.
//! <https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf>
//! Independent wire assertions; no reference implementation is linked or copied.
#[path = "support/logix_bits.rs"]
mod logix_bits;
#[path = "support/logix_raw.rs"]
mod logix_raw;
#[path = "support/logix_string.rs"]
mod logix_string;
#[path = "support/micro800.rs"]
mod micro800;
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};

const SESSION: u32 = 0x4433_2211;
fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "CIP client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
struct Packet {
    header: [u8; 24],
    body: Vec<u8>,
}
fn receive(stream: &mut TcpStream) -> Packet {
    let mut header = [0; 24];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[8..12], [0; 4]);
    assert_eq!(header[20..24], [0; 4]);
    let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
    assert!(length <= 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    Packet { header, body }
}
fn send(stream: &mut TcpStream, request: &Packet, body: &[u8]) -> io::Result<()> {
    let mut header = request.header;
    let length = u16::try_from(body.len())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    header[2..4].copy_from_slice(&length.to_le_bytes());
    header[4..8].copy_from_slice(&SESSION.to_le_bytes());
    let bytes = [header.as_slice(), body].concat();
    for chunk in bytes.chunks(3) {
        stream.write_all(chunk)?;
    }
    Ok(())
}
fn register(stream: &mut TcpStream) {
    let request = receive(stream);
    assert_eq!(
        request.header[..12],
        [0x65, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(request.body, [1, 0, 0, 0]);
    assert_eq!(request.header[12..20], [0; 8]);
    send(stream, &request, &[1, 0, 0, 0]).unwrap();
}
fn unregister(stream: &mut TcpStream) {
    let request = receive(stream);
    assert_eq!(request.header[..8], [0x66, 0, 0, 0, 0x11, 0x22, 0x33, 0x44]);
    assert_eq!(request.body, [] as [u8; 0]);
}
fn path(name: &[u8], index: Option<u8>) -> Vec<u8> {
    let mut bytes = vec![0x91, u8::try_from(name.len()).unwrap()];
    bytes.extend_from_slice(name);
    if !name.len().is_multiple_of(2) {
        bytes.push(0);
    }
    if let Some(index) = index {
        bytes.extend_from_slice(&[0x28, index]);
    }
    bytes
}
fn query(stream: &mut TcpStream, slot: Option<u8>, request: &[u8]) -> Packet {
    let packet = receive(stream);
    assert_eq!(packet.header[..2], [0x6f, 0]);
    assert_eq!(packet.header[4..8], SESSION.to_le_bytes());
    assert!(packet.body.len() >= 16);
    assert_eq!(
        packet.body[..14],
        [0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0]
    );
    let length = usize::from(u16::from_le_bytes([packet.body[14], packet.body[15]]));
    assert_eq!(length, packet.body.len() - 16);
    let mut expected = Vec::new();
    if let Some(slot) = slot {
        expected.extend_from_slice(&[0x52, 2, 0x20, 6, 0x24, 1, 0x0a, 0x0e]);
        expected.extend_from_slice(&u16::try_from(request.len()).unwrap().to_le_bytes());
        expected.extend_from_slice(request);
        if !request.len().is_multiple_of(2) {
            expected.push(0);
        }
        expected.extend_from_slice(&[1, 0, 1, slot]);
    } else {
        expected.extend_from_slice(request);
    }
    assert_eq!(
        &packet.body[16..],
        expected,
        "only the exact configured read and byte offset are permitted"
    );
    packet
}
fn read_request(path: &[u8], elements: u16, offset: Option<u32>) -> Vec<u8> {
    let mut request = vec![
        if offset.is_some() { 0x52 } else { 0x4c },
        u8::try_from(path.len() / 2).unwrap(),
    ];
    request.extend_from_slice(path);
    request.extend_from_slice(&elements.to_le_bytes());
    if let Some(offset) = offset {
        request.extend_from_slice(&offset.to_le_bytes());
    }
    request
}
fn rr(cip: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
    bytes.extend_from_slice(&u16::try_from(cip.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(cip);
    bytes
}
fn reply(service: u8, status: u8, kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut cip = vec![service, 0, status, 0];
    cip.extend_from_slice(&kind.to_le_bytes());
    cip.extend_from_slice(payload);
    rr(&cip)
}

fn config(listener: &TcpListener, signals: Value, slot: Option<u8>) -> Value {
    let mut config = json!({"version":1,"sources":[{"id":"plc","protocol":"ethernet_ip",
        "address":listener.local_addr().unwrap().to_string(),"every_ms":3_600_000,"timeout_ms":2000}]});
    config["sources"][0]["signals"] = signals;
    if let Some(slot) = slot {
        config["sources"][0]["slot"] = json!(slot);
    }
    config
}
fn record(home: &Home, config: &Value) -> Run {
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "1s",
        "-o",
        "run.jsonl",
    ])
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect()
}
fn array(stream: &mut TcpStream, mode: &str, path: &[u8], kind: u16, payload: &[u8]) {
    let slot = (mode != "direct").then_some(3);
    for (index, chunk) in payload.chunks(488).enumerate() {
        let offset = u32::try_from(index * 488).unwrap();
        let request = query(
            stream,
            slot,
            &read_request(path, 256, (index != 0).then_some(offset)),
        );
        let service = if index == 0 { 0xcc } else { 0xd2 };
        let partial = usize::try_from(offset).unwrap() + chunk.len() < payload.len();
        send(
            stream,
            &request,
            &reply(service, if partial { 6 } else { 0 }, kind, chunk),
        )
        .unwrap();
    }
}

#[test]
fn direct_and_routed_arrays_continue_by_exact_value_byte_offsets_and_survive_exports() {
    for mode in ["direct", "route_pass"] {
        let home = Home::new("cip-array");
        let listener = listener();
        let slot = (mode != "direct").then_some(3);
        let mut config = config(
            &listener,
            json!([
                {"name":"ready","tag":"Ready"},
                {"name":"counts","tag":"Counts[3]","elements":256},
                {"name":"pressure","tag":"Pressure[2]","elements":256}
            ]),
            slot,
        );
        config["version"] = serde_json::from_str("1e0").unwrap();
        config["sources"][0]["every_ms"] = serde_json::from_str("3.6e6").unwrap();
        config["sources"][0]["timeout_ms"] = serde_json::from_str("2e3").unwrap();
        for index in [1, 2] {
            config["sources"][0]["signals"][index]["elements"] =
                serde_json::from_str("2.56e2").unwrap();
        }
        if slot.is_some() {
            config["sources"][0]["slot"] = serde_json::from_str("3.0").unwrap();
        }
        let expected_counts = (0_u64..256)
            .map(|index| u64::MAX - index)
            .collect::<Vec<_>>();
        let payload = expected_counts
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        let pressure = (0_u16..256)
            .map(|index| f32::from(index) / 4.0)
            .collect::<Vec<_>>();
        let pressure_payload = pressure
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let request = query(
                &mut stream,
                slot,
                &read_request(&path(b"Ready", None), 1, None),
            );
            send(&mut stream, &request, &reply(0xcc, 0, 0xc1, &[0xff])).unwrap();
            array(&mut stream, mode, &path(b"Counts", Some(3)), 0xc9, &payload);
            array(
                &mut stream,
                mode,
                &path(b"Pressure", Some(2)),
                0xca,
                &pressure_payload,
            );
            unregister(&mut stream);
        });
        record(&home, &config).success();
        server.join().unwrap();
        let rows = rows(&home);
        let samples = rows
            .iter()
            .filter(|row| row["kind"] == "sample")
            .collect::<Vec<_>>();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0]["value"], true);
        assert_eq!(samples[1]["value"], json!(expected_counts));
        assert_eq!(samples[2]["value"], json!(pressure));
        home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "-o",
            "values.jsonl",
            "--format",
            "jsonl",
        ]);
        assert!(
            fs::read_to_string(home.path("values.csv"))
                .unwrap()
                .contains("18446744073709551615")
        );
        let exported = fs::read_to_string(home.path("values.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(exported.len(), samples.len());
        for (exported, sample) in exported.iter().zip(samples) {
            assert_eq!(exported["value"], sample["value"]);
        }
    }
}

#[test]
fn more_than_256_progressing_fragments_complete_without_an_arbitrary_count_cap() {
    let home = Home::new("cip-many-fragments");
    let listener = listener();
    let config = config(
        &listener,
        json!([{"name":"counts","tag":"Counts[3]","elements":64}]),
        None,
    );
    let expected = (0_u64..64)
        .map(|index| u64::MAX - index)
        .collect::<Vec<_>>();
    let payload = expected
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        // 256 one-byte partial replies, then a final 256-byte payload. This
        // also splits wide integers across replies and excludes repeated type.
        for index in 0..=256 {
            let offset = u32::try_from(index).unwrap();
            let request = query(
                &mut stream,
                None,
                &read_request(
                    &path(b"Counts", Some(3)),
                    64,
                    (index != 0).then_some(offset),
                ),
            );
            assert!(index < payload.len());
            let value = if index == 256 {
                &payload[index..]
            } else {
                &payload[index..=index]
            };
            send(
                &mut stream,
                &request,
                &reply(
                    if index == 0 { 0xcc } else { 0xd2 },
                    if index == 256 { 0 } else { 6 },
                    0xc9,
                    value,
                ),
            )
            .unwrap();
        }
        unregister(&mut stream);
    });
    record(&home, &config).success();
    server.join().unwrap();
    let samples = rows(&home)
        .into_iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0]["value"], json!(expected));
}

#[test]
fn malformed_continuations_never_publish_prior_tags_or_partial_values() {
    for mode in [
        "no_progress",
        "partial_complete",
        "short_final",
        "extra_final",
        "type_changed",
        "missing_type",
        "wrong_service",
        "truncated_status",
        "wrong_session",
        "wrong_context",
        "bad_reserved",
        "nested_twice",
        "target_status",
        "route_status",
        "unknown_type",
        "unknown_cc",
        "unknown_d2",
        "nested_once",
    ] {
        let home = Home::new("cip-invalid-fragment");
        let listener = listener();
        let config = config(
            &listener,
            json!([
                {"name":"ready","tag":"Ready"}, {"name":"counts","tag":"Counts[3]","elements":64}
            ]),
            Some(3),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let request = query(
                &mut stream,
                Some(3),
                &read_request(&path(b"Ready", None), 1, None),
            );
            send(&mut stream, &request, &reply(0xcc, 0, 0xc1, &[1])).unwrap();
            let request = query(
                &mut stream,
                Some(3),
                &read_request(&path(b"Counts", Some(3)), 64, None),
            );
            send(
                &mut stream,
                &request,
                &reply(0xcc, 6, 0xc9, &u64::MAX.to_le_bytes()),
            )
            .unwrap();
            let mut request = query(
                &mut stream,
                Some(3),
                &read_request(&path(b"Counts", Some(3)), 64, Some(8)),
            );
            let mut cip = vec![0xd2, 0, 0, 0, 0xc9, 0];
            cip.extend_from_slice(&[0; 504]);
            assert!(cip.len() >= 6);
            match mode {
                "no_progress" => {
                    cip.truncate(6);
                    assert!(cip.len() >= 3);
                    cip[2] = 6;
                }
                "partial_complete" => cip[2] = 6,
                "short_final" => cip.truncate(14),
                "extra_final" => cip.push(0),
                "type_changed" => cip[4] = 0xc5,
                "missing_type" => cip.truncate(4),
                "wrong_service" => cip[0] = 0xcc,
                "truncated_status" => cip = vec![0xd2, 0, 5, 2, 0],
                "wrong_session" => {}
                "wrong_context" => request.header[12] ^= 1,
                "bad_reserved" => cip[1] = 1,
                "nested_twice" => cip = [vec![0xd2, 0, 0, 0, 0xd2, 0, 0, 0], cip].concat(),
                "target_status" => cip = vec![0xd2, 0, 0xff, 1, 0x05, 0x21],
                "route_status" => cip = vec![0xd2, 0, 1, 1, 0x12, 0x03, 0, 0],
                "unknown_type" => cip[4] = 0xd3,
                "unknown_cc" => cip[4] = 0xcc,
                "unknown_d2" => cip[4] = 0xd2,
                "nested_once" => cip = [vec![0xd2, 0, 0, 0], cip].concat(),
                _ => unreachable!(),
            }
            let body = rr(&cip);
            // send() restores the valid session for ordinary replies, so inject
            // a wrong session only after constructing the full response here.
            if mode == "wrong_session" {
                let mut header = request.header;
                header[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
                header[4..8].copy_from_slice(&(SESSION ^ 1).to_le_bytes());
                let _ = stream.write_all(&[header.as_slice(), body.as_slice()].concat());
            } else {
                let _ = send(&mut stream, &request, &body);
            }
        });
        let exit = if matches!(
            mode,
            "nested_once" | "nested_twice" | "unknown_type" | "unknown_cc" | "unknown_d2"
        ) {
            5
        } else {
            1
        };
        let error = record(&home, &config).failure(exit);
        if mode == "target_status" {
            assert_eq!(error["details"]["cip_status"], 0xff);
            assert_eq!(error["details"]["extended_status"], json!([0x2105]));
        }
        if mode == "route_status" {
            assert_eq!(error["details"]["cip_status"], 1);
            assert_eq!(error["details"]["extended_status"], json!([0x0312]));
        }
        server.join().unwrap();
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["kind"] == "sample"), "{mode}");
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn initial_router_errors_are_preserved_without_guessing_aliases_or_nested_payload_headers() {
    for mode in [
        "cm_error",
        "cm_partial",
        "success_alias",
        "success_wrapper",
        "unsupported_cc",
        "unsupported_d2",
    ] {
        let home = Home::new("cip-initial-service");
        let listener = listener();
        let config = config(
            &listener,
            json!([{"name":"counts","tag":"Counts[3]","elements":2}]),
            Some(3),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let request = query(
                &mut stream,
                Some(3),
                &read_request(&path(b"Counts", Some(3)), 2, None),
            );
            let cip = match mode {
                "cm_error" => vec![0xd2, 0, 1, 1, 0x12, 0x03, 0, 0],
                "cm_partial" => vec![0xd2, 0, 6, 0, 0xc3, 0, 0x34, 0x12],
                "success_alias" => vec![0xd2, 0, 0, 0, 0xc3, 0, 0x34, 0x12, 0x78, 0x56],
                // These bytes can also mean unsupported atomic type00CC.
                // Guessing an inner CC header would fabricate two accepted INTs.
                "success_wrapper" => vec![
                    0xd2, 0, 0, 0, 0xcc, 0, 0, 0, 0xc3, 0, 0x34, 0x12, 0x78, 0x56,
                ],
                "unsupported_cc" => vec![
                    0xcc, 0, 0, 0, 0xcc, 0, 0, 0, 0xc3, 0, 0x34, 0x12, 0x78, 0x56,
                ],
                "unsupported_d2" => vec![
                    0xcc, 0, 0, 0, 0xd2, 0, 0, 0, 0xc3, 0, 0x34, 0x12, 0x78, 0x56,
                ],
                _ => unreachable!(),
            };
            let _ = send(&mut stream, &request, &rr(&cip));
        });
        let error = record(&home, &config).failure(if mode.starts_with("unsupported") {
            5
        } else {
            1
        });
        if mode == "cm_error" {
            assert_eq!(error["details"]["cip_status"], 1);
            assert_eq!(error["details"]["extended_status"], json!([0x0312]));
        } else if !mode.starts_with("unsupported") {
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("unexpected CIP reply service"),
                "{mode}: {error}"
            );
        }
        server.join().unwrap();
        assert!(
            !rows(&home).iter().any(|row| row["kind"] == "sample"),
            "{mode}"
        );
    }
}

#[test]
fn later_read_fragments_cannot_extend_the_total_poll_deadline() {
    let home = Home::new("cip-fragment-timeout");
    let listener = listener();
    let mut config = config(
        &listener,
        json!([
            {"name":"ready","tag":"Ready"}, {"name":"counts","tag":"Counts[3]","elements":64}
        ]),
        None,
    );
    config["sources"][0]["timeout_ms"] = json!(250);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        let request = query(
            &mut stream,
            None,
            &read_request(&path(b"Ready", None), 1, None),
        );
        send(&mut stream, &request, &reply(0xcc, 0, 0xc1, &[1])).unwrap();
        let request = query(
            &mut stream,
            None,
            &read_request(&path(b"Counts", Some(3)), 64, None),
        );
        thread::sleep(Duration::from_millis(100));
        send(&mut stream, &request, &reply(0xcc, 6, 0xc9, &[0; 8])).unwrap();
        let request = query(
            &mut stream,
            None,
            &read_request(&path(b"Counts", Some(3)), 64, Some(8)),
        );
        thread::sleep(Duration::from_millis(200));
        let _ = send(&mut stream, &request, &reply(0xd2, 0, 0xc9, &[0; 504]));
    });
    let began = Instant::now();
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    assert!(began.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    assert!(!rows(&home).iter().any(|row| row["kind"] == "sample"));
}

// W506 7-3-4 and 7-4-5 encode VarAA.MemB[1,2] as two ANSI symbols and
// two 8-bit member IDs. W506 7-6-1 gives the little-endian element count.
const OMRON_BOOL: &[u8] = b"\x4c\x09\x91\x05VarAA\0\x91\x04MemB\x28\x01\x28\x02\x01\0";
const OMRON_UINTS: &[u8] = b"\x4c\x05\x91\x03Var\0\x28\x01\x28\0\x02\0";
const OMRON_ULINT: &[u8] = b"\x4c\x05\x91\x07Counter\0\x01\0";
const OMRON_WORD: &[u8] = b"\x4c\x03\x91\x04Word\x01\0";
// W506 7-7-3 describes two independent bytes. Forced FALSE is still FALSE.
const OMRON_FORCED_FALSE: &[u8] = b"\xcc\0\0\0\xc1\0\0\x01";

#[test]
fn omron_nj_nx_reads_vendor_bool_path_and_exact_numeric_values() {
    let home = Home::new("omron-cip-read");
    let listener = listener();
    let mut config = config(
        &listener,
        json!([
            {"name":"ready","tag":"VarAA.MemB[1,2]"},
            {"name":"range","tag":"Var[1,0]","elements":2},
            {"name":"count","tag":"Counter"},
            {"name":"mask","tag":"Word"}
        ]),
        None,
    );
    config["sources"][0]["profile"] = json!("omron_nj_nx");
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        for (sequence, (request, response)) in [
            (OMRON_BOOL, OMRON_FORCED_FALSE),
            (OMRON_UINTS, &b"\xcc\0\0\0\xc7\0\x34\x12\xcd\xab"[..]),
            (OMRON_ULINT, &b"\xcc\0\0\0\xc9\0\x01\0\0\0\0\0\x20\0"[..]),
            (OMRON_WORD, &b"\xcc\0\0\0\xd2\0\xff\xff"[..]),
        ]
        .into_iter()
        .enumerate()
        {
            let request = query(&mut stream, None, request);
            let context = u64::try_from(sequence + 1).unwrap().to_le_bytes();
            assert_eq!(request.header[12..20], context);
            send(&mut stream, &request, &rr(response)).unwrap();
        }
        unregister(&mut stream);
    });
    assert_eq!(record(&home, &config).success()["samples"], 4);
    server.join().unwrap();
    let rows = rows(&home);
    assert_eq!(rows[0]["sources"][0]["protocol"], "ethernet_ip");
    let samples = rows.iter().filter(|row| row["kind"] == "sample");
    for (row, (signal, value)) in samples.zip([
        ("ready", json!({"value":false,"forced":true})),
        ("range", json!([0x1234, 0xabcd])),
        ("count", json!((1_u64 << 53) + 1)),
        ("mask", json!(u16::MAX)),
    ]) {
        assert_eq!(row["signal"], signal);
        assert_eq!(row["value"], value);
    }
    assert_eq!(rows.last().unwrap()["status"], "complete");
    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    assert!(
        fs::read_to_string(home.path("values.csv"))
            .unwrap()
            .contains("9007199254740993")
    );
}

#[test]
fn omron_nj_nx_rejects_bad_values_atomically_without_fragment_fallback() {
    let oversized = vec![0; 503];
    for (response, exit, cause) in [
        (
            &b"\xcc\0\0\0\xc7\x01\x34\x12\xcd\xab"[..],
            1,
            "zero AddInfo length",
        ),
        (
            &b"\xcc\0\0\0\xc7\0\x34\x12"[..],
            1,
            "differs from requested element count",
        ),
        (
            &b"\xcc\0\0\0\xc1\0\x01\0"[..],
            5,
            "one scalar or indexed element",
        ),
        (
            &b"\xcc\0\x06\0\xc7\0\x34\x12"[..],
            5,
            "partial variable read",
        ),
        (&b"\xcc\0\x11\0"[..], 1, "CIP read rejected"),
        (oversized.as_slice(), 1, "CIP reply exceeds 502 bytes"),
    ] {
        let home = Home::new("omron-cip-reject");
        let listener = listener();
        let mut config = config(
            &listener,
            json!([
                {"name":"ready","tag":"VarAA.MemB[1,2]"},
                {"name":"range","tag":"Var[1,0]","elements":2}
            ]),
            None,
        );
        config["sources"][0]["profile"] = json!("omron_nj_nx");
        let body = rr(response);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let request = query(&mut stream, None, OMRON_BOOL);
            send(&mut stream, &request, &rr(OMRON_FORCED_FALSE)).unwrap();
            let request = query(&mut stream, None, OMRON_UINTS);
            send(&mut stream, &request, &body).unwrap();
            unregister(&mut stream);
        });
        let error = record(&home, &config).failure(exit);
        server.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["kind"] == "sample"));
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn omron_nj_nx_validates_direct_routes_and_indexed_ranges_before_opening() {
    let home = Home::new("omron-cip-invalid");
    let listener = listener();
    for (signal, slot, cause) in [
        (
            json!({"name":"x","tag":"X"}),
            Some(1),
            "direct CPU connection",
        ),
        (
            json!({"name":"x","tag":"X","elements":2}),
            None,
            "explicit first element index",
        ),
        (
            json!({"name":"x","tag":"X[65536]"}),
            None,
            "indices must fit 16 bits",
        ),
        (
            json!({"name":"x","tag":"Program:Main.X"}),
            None,
            "published variable name",
        ),
    ] {
        let mut config = config(&listener, json!([signal]), slot);
        config["sources"][0]["profile"] = json!("omron_nj_nx");
        let error = record(&home, &config).failure(2);
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
