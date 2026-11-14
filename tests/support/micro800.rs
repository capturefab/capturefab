//! Direct Micro800 Class 3 messages through the real recorder CLI.
//! The connection shape follows the pinned libplctag Micro800 profile.

use super::{
    Home, Packet, SESSION, accept, config, listener, path, read_request, receive, record, register,
    rows, rr, send, unregister,
};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    net::{TcpListener, TcpStream},
    thread,
};

const TARGET: u32 = 0x7856_3412;
const OPEN: &[u8] = &[0x54, 2, 0x20, 6, 0x24, 1, 0x0a, 0x0e];
const CLOSE: &[u8] = &[0x4e, 2, 0x20, 6, 0x24, 1, 0x0a, 0x0e];
const ROUTER: &[u8] = &[0x20, 2, 0x24, 1];

fn source(listener: &TcpListener, signals: Value) -> Value {
    let mut cfg = config(listener, signals, None);
    cfg["sources"][0]["profile"] = json!("micro800");
    cfg["sources"][0]["originator_vendor_id"] = serde_json::from_str("5e2").unwrap();
    cfg
}

fn open_request(stream: &mut TcpStream) -> (Packet, [u8; 8], u32) {
    let packet = receive(stream);
    assert_eq!(packet.header[..2], [0x6f, 0]);
    assert_eq!(packet.header[4..8], SESSION.to_le_bytes());
    assert_ne!(packet.header[12..20], [0; 8]);
    assert_eq!(
        packet.body[..16],
        [0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0, 46, 0]
    );
    let cip = &packet.body[16..];
    assert_eq!(cip.len(), 46);
    assert_eq!(&cip[..8], OPEN);
    assert_eq!(cip[8..12], [0; 4]);
    let originator = u32::from_le_bytes(cip[12..16].try_into().unwrap());
    assert_ne!(originator, 0);
    let identity: [u8; 8] = cip[16..24].try_into().unwrap();
    assert_ne!(identity[..2], [0; 2]);
    assert_eq!(identity[2..4], 500_u16.to_le_bytes());
    assert_ne!(identity[4..8], [0; 4]);
    assert_eq!(
        cip[24..42],
        [
            3, 0, 0, 0, 0x40, 0x42, 0x0f, 0, 0xf8, 0x43, 0x40, 0x42, 0x0f, 0, 0xf8, 0x43, 0xa3, 2
        ]
    );
    assert_eq!(&cip[42..], ROUTER);
    (packet, identity, originator)
}

fn open_reply(identity: [u8; 8], originator: u32, api_us: u32) -> Vec<u8> {
    let mut cip = vec![0xd4, 0, 0, 0];
    cip.extend(TARGET.to_le_bytes());
    cip.extend(originator.to_le_bytes());
    cip.extend(identity);
    cip.extend(api_us.to_le_bytes());
    cip.extend(api_us.to_le_bytes());
    cip.extend([0, 0]);
    cip
}

fn open(stream: &mut TcpStream, api_us: u32) -> ([u8; 8], u32) {
    register(stream);
    let (packet, identity, originator) = open_request(stream);
    send(
        stream,
        &packet,
        &rr(&open_reply(identity, originator, api_us)),
    )
    .unwrap();
    (identity, originator)
}

fn unit_body(connection_id: u32, sequence: u16, cip: &[u8]) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0, 0, 0, 2, 0, 0xa1, 0, 4, 0];
    body.extend(connection_id.to_le_bytes());
    body.extend([0xb1, 0]);
    body.extend(u16::try_from(cip.len() + 2).unwrap().to_le_bytes());
    body.extend(sequence.to_le_bytes());
    body.extend_from_slice(cip);
    body
}

fn read_tag(stream: &mut TcpStream, sequence: u16, tag: &[u8], count: u16, offset: u32) -> Packet {
    let request = receive(stream);
    assert_eq!(request.header[..2], [0x70, 0]);
    assert_eq!(request.header[4..8], SESSION.to_le_bytes());
    assert_eq!(request.header[12..20], [0; 8]);
    assert_eq!(
        request.body,
        unit_body(TARGET, sequence, &read_request(tag, count, Some(offset)))
    );
    request
}

fn connected_reply(status: u8, kind: u16, data: &[u8]) -> Vec<u8> {
    let mut cip = vec![0xd2, 0, status, 0];
    cip.extend(kind.to_le_bytes());
    cip.extend_from_slice(data);
    cip
}

fn close(stream: &mut TcpStream, identity: [u8; 8], reply: bool) {
    let request = receive(stream);
    assert_eq!(request.header[..2], [0x6f, 0]);
    assert_eq!(request.header[4..8], SESSION.to_le_bytes());
    assert_ne!(request.header[12..20], [0; 8]);
    let mut cip = CLOSE.to_vec();
    cip.extend(identity);
    cip.extend([2, 0]);
    cip.extend_from_slice(ROUTER);
    assert_eq!(request.body, rr(&cip));
    if reply {
        let mut response = vec![0xce, 0, 0, 0];
        response.extend(identity);
        response.extend([0, 0]);
        send(stream, &request, &rr(&response)).unwrap();
    }
    unregister(stream);
}

#[test]
fn micro800_class3_reads_fragments_with_exact_ids_and_sequence_and_closes() {
    let home = Home::new("micro800-fragments");
    let listener = listener();
    let cfg = source(
        &listener,
        json!([
            {"name":"wide", "tag":"Count[3]", "elements":3},
            {"name":"enabled", "tag":"Enabled"}
        ]),
    );
    let worker = thread::spawn(move || {
        let mut stream = accept(&listener);
        let (identity, originator) = open(&mut stream, 1_000_000);
        let raw = [u64::MAX, (1_u64 << 53) + 1, 17]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect::<Vec<_>>();
        let tag = path(b"Count", Some(3));
        for (sequence, offset, payload, status) in [
            (1, 0, &raw[..5], 6),
            (2, 5, &raw[5..17], 6),
            (3, 17, &raw[17..], 0),
        ] {
            let mut request = read_tag(&mut stream, sequence, &tag, 3, offset);
            // A connected frame's Sender Context does not take part in correlation.
            request.header[12..20].fill(0xa7);
            send(
                &mut stream,
                &request,
                &unit_body(
                    originator,
                    sequence,
                    &connected_reply(status, 0xc9, payload),
                ),
            )
            .unwrap();
        }
        let request = read_tag(&mut stream, 4, &path(b"Enabled", None), 1, 0);
        send(
            &mut stream,
            &request,
            &unit_body(originator, 4, &connected_reply(0, 0xc1, &[1])),
        )
        .unwrap();
        close(&mut stream, identity, true);
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    worker.join().unwrap();
    let rows = rows(&home);
    assert_eq!(rows[1]["value"], json!([u64::MAX, (1_u64 << 53) + 1, 17]));
    assert_eq!(rows[2]["value"], true);
    assert_eq!(
        home.run(&[
            "--json",
            "data",
            "export",
            "run.jsonl",
            "--format",
            "jsonl",
            "-o",
            "out.jsonl"
        ])
        .success()["samples"],
        2
    );
    let export = std::fs::read_to_string(home.path("out.jsonl")).unwrap();
    assert!(export.contains("[18446744073709551615,9007199254740993,17]"));
}

#[test]
fn micro800_rejects_connection_and_sequence_mismatches_without_partial_publication() {
    for mode in [
        "connection",
        "sequence",
        "session",
        "length",
        "item",
        "capacity",
        "service",
    ] {
        let home = Home::new("micro800-invalid");
        let listener = listener();
        let cfg = source(
            &listener,
            json!([{"name":"first","tag":"A"},{"name":"second","tag":"B"}]),
        );
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            let (_, originator) = open(&mut stream, 1_000_000);
            let request = read_tag(&mut stream, 1, &path(b"A", None), 1, 0);
            let cip = connected_reply(0, 0xc3, &[42, 0]);
            send(&mut stream, &request, &unit_body(originator, 1, &cip)).unwrap();
            let mut request = read_tag(&mut stream, 2, &path(b"B", None), 1, 0);
            let mut response = unit_body(originator, 2, &cip);
            match mode {
                "connection" => response[12] ^= 1,
                "sequence" => response[20] ^= 1,
                "length" => response[18] ^= 1,
                "item" => response[16] = 0xb2,
                "capacity" => {
                    response.resize(525, 0);
                    response[18..20].copy_from_slice(&505_u16.to_le_bytes());
                }
                "service" => response[22] = 0xcc,
                "session" => {
                    // Send a hand-built encapsulation error for a foreign session.
                    request.header[2..4]
                        .copy_from_slice(&u16::try_from(response.len()).unwrap().to_le_bytes());
                    request.header[4..8].copy_from_slice(&(SESSION ^ 1).to_le_bytes());
                    request.header[8] = 1;
                    use std::io::Write as _;
                    stream.write_all(&request.header).unwrap();
                    stream.write_all(&response).unwrap();
                    unregister(&mut stream);
                    return;
                }
                _ => unreachable!(),
            }
            send(&mut stream, &request, &response).unwrap();
            unregister(&mut stream);
        });
        let error = record(&home, &cfg).failure(1);
        let expected = match mode {
            "connection" | "sequence" => "connection ID or sequence",
            "session" => "session handle",
            "length" | "capacity" => "connected data length",
            "item" => "Common Packet Format",
            "service" => "reply service",
            _ => unreachable!(),
        };
        assert!(
            error["message"].as_str().unwrap().contains(expected),
            "{error}"
        );
        worker.join().unwrap();
        let rows = rows(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["samples"], 0);
        assert_eq!(rows[1]["status"], "failed");
    }
}

#[test]
fn micro800_requires_correlated_open_and_shares_handshake_and_read_deadline() {
    for mode in ["identity", "zero_id", "app_length", "deadline"] {
        let home = Home::new("micro800-open");
        let listener = listener();
        let mut cfg = source(&listener, json!([{"name":"value","tag":"A"}]));
        cfg["sources"][0]["timeout_ms"] = json!(time::ms(700));
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let (request, identity, originator) = open_request(&mut stream);
            let mut cip = open_reply(identity, originator, 1_000_000);
            match mode {
                "identity" => cip[12] ^= 1,
                "zero_id" => cip[4..8].fill(0),
                "app_length" => cip[28] = 1,
                "deadline" => thread::sleep(time::millis(400)),
                _ => unreachable!(),
            }
            send(&mut stream, &request, &rr(&cip)).unwrap();
            if mode == "deadline" {
                let request = read_tag(&mut stream, 1, &path(b"A", None), 1, 0);
                thread::sleep(time::millis(400));
                let response = unit_body(originator, 1, &connected_reply(0, 0xc3, &[42, 0]));
                drop(send(&mut stream, &request, &response));
            }
            unregister(&mut stream);
        });
        let run = record(&home, &cfg);
        if mode == "deadline" {
            assert_eq!(run.failure(4)["code"], "timeout");
        } else {
            assert!(
                run.failure(1)["message"]
                    .as_str()
                    .unwrap()
                    .contains("Forward Open")
            );
        }
        worker.join().unwrap();
        assert_eq!(rows(&home)[1]["samples"], 0);
    }
}

#[test]
fn micro800_renews_inactive_connection_and_bounds_close_without_reply() {
    let home = Home::new("micro800-idle");
    let listener = listener();
    let mut cfg = source(&listener, json!([{"name":"value","tag":"A"}]));
    cfg["sources"][0]["every_ms"] = json!(time::ms(700));
    let worker = thread::spawn(move || {
        let mut first = accept(&listener);
        let (_, originator) = open(&mut first, 1000);
        let request = read_tag(&mut first, 1, &path(b"A", None), 1, 0);
        send(
            &mut first,
            &request,
            &unit_body(originator, 1, &connected_reply(0, 0xc3, &[1, 0])),
        )
        .unwrap();
        unregister(&mut first);
        let mut second = accept(&listener);
        let (identity, originator) = open(&mut second, 1_000_000);
        let request = read_tag(&mut second, 1, &path(b"A", None), 1, 0);
        send(
            &mut second,
            &request,
            &unit_body(originator, 1, &connected_reply(0, 0xc3, &[2, 0])),
        )
        .unwrap();
        close(&mut second, identity, false);
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    worker.join().unwrap();
    let rows = rows(&home);
    assert_eq!(rows[1]["value"], 1);
    assert_eq!(rows[2]["value"], 2);
}

#[test]
fn micro800_rejects_slot_and_program_scope_before_network_io() {
    let listener = listener();
    for (tag, slot, expected) in [
        ("Tag", Some(0), "without a slot"),
        ("Program:Main.Tag", None, "global variable"),
        ("Tag__name", None, "consecutive underscores"),
    ] {
        let home = Home::new("micro800-config");
        let mut cfg = source(&listener, json!([{"name":"value","tag":tag}]));
        if let Some(slot) = slot {
            cfg["sources"][0]["slot"] = json!(slot);
        }
        let error = record(&home, &cfg).failure(2);
        assert!(
            error["message"].as_str().unwrap().contains(expected),
            "{error}"
        );
    }
}

#[path = "micro800_string.rs"]
mod strings;
