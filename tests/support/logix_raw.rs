//! Opaque structure transfers through the recorder CLI, with exact raw exports.
//! The small literal is Rockwell 1756-PM020I pp.59–60; other peers are spec-derived.
use super::{
    Home, Packet, SESSION, accept, config, listener, path, query, read_request, record, register,
    reply, rows, rr, send, unregister,
};
use crate::support::time;
use base64::Engine as _;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    thread,
    time::Instant,
};

const VENDOR_REQUEST: &[u8] = &[
    0x4c, 8, 0x91, 14, 0x4d, 0x61, 0x63, 0x68, 0x69, 0x6e, 0x65, 0x53, 0x75, 0x6d, 0x6d, 0x61,
    0x72, 0x79, 1, 0,
];
// Exact published reply, including the three alignment pads.
const VENDOR_REPLY: &[u8] = &[
    0xcc, 0, 0, 0, 0xa0, 2, 0xcd, 0x9e, 1, 0, 0, 0, 0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0,
    8, 0, 9, 0, 10, 0, 11, 0, 0, 0, 0x80, 0x3f,
];
fn expected(handle: u16, data: &[u8], elements: u16, max_bytes: usize) -> Value {
    let [lo, hi] = handle.to_le_bytes();
    json!({"kind":"logix_struct_raw","layout":"opaque","elements_requested":elements,
        "tag_type_hex":format!("a002{lo:02x}{hi:02x}"),"structure_handle":handle,
        "bytes":data.len(),"configured_max_bytes":max_bytes,
        "raw":{"encoding":"base64","scope":"structure_data","data":base64::engine::general_purpose::STANDARD.encode(data)}})
}
fn raw(service: u8, status: u8, descriptor: &[u8], data: &[u8]) -> Vec<u8> {
    rr(&[&[service, 0, status, 0], descriptor, data].concat())
}
fn fast_send(stream: &mut TcpStream, packet: &Packet, body: &[u8], corrupt: &str) {
    let mut header = packet.header;
    header[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
    let session = if corrupt == "session" {
        SESSION ^ 1
    } else {
        SESSION
    };
    header[4..8].copy_from_slice(&session.to_le_bytes());
    if corrupt == "context" {
        header[12] ^= 1;
    }
    stream.write_all(&header).unwrap();
    stream.write_all(body).unwrap();
}
fn no_more(stream: &mut TcpStream) {
    unregister(stream);
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).unwrap(), 0);
}
fn no_samples(home: &Home) {
    let rows = rows(home);
    assert!(!rows.iter().any(|r| r["kind"] == "sample"));
    assert_eq!(rows.last().unwrap()["status"], "failed");
}
fn numeric(stream: &mut TcpStream, slot: Option<u8>) {
    let packet = query(stream, slot, &read_request(&path(b"Ready", None), 1, None));
    send(stream, &packet, &reply(0xcc, 0, 0xc1, &[1])).unwrap();
}
fn transfers(stream: &mut TcpStream, name: &[u8], data: &[u8], chunks: usize) {
    for (index, bytes) in data.chunks(chunks).enumerate() {
        let offset = index * chunks;
        let packet = query(
            stream,
            None,
            &read_request(
                &path(name, None),
                1,
                (index != 0).then_some(u32::try_from(offset).unwrap()),
            ),
        );
        let body = raw(
            if index == 0 { 0xcc } else { 0xd2 },
            if offset + bytes.len() < data.len() {
                6
            } else {
                0
            },
            &[0xa0, 2, 0x34, 0x12],
            bytes,
        );
        fast_send(stream, &packet, &body, "");
    }
}

#[test]
fn literal_structure_and_opaque_arrays_preserve_descriptor_padding_and_exports() {
    for slot in [None, Some(3)] {
        let home = Home::new("logix-raw-exact");
        let listener = listener();
        let mut cfg = config(
            &listener,
            json!([
                {"name":"ready","tag":"Ready"},
                {"name":"summary","tag":"MachineSummary","data_type":"logix_struct_raw","max_bytes":32},
                {"name":"array","tag":"Records[3]","data_type":"logix_struct_raw","max_bytes":7,"elements":2},
                {"name":"minimum","tag":"Minimum","data_type":"logix_struct_raw","max_bytes":1}
            ]),
            slot,
        );
        cfg["sources"][0]["signals"][1]["max_bytes"] = serde_json::from_str("3.2e1").unwrap();
        cfg["sources"][0]["signals"][2]["elements"] = serde_json::from_str("2.0").unwrap();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            numeric(&mut stream, slot);
            let request = query(&mut stream, slot, VENDOR_REQUEST);
            send(&mut stream, &request, &rr(VENDOR_REPLY)).unwrap();
            // Seven arbitrary bytes for two requested elements stay one blob.
            // This spec-derived peer does not prove a real two-instance layout.
            for (index, offset, payload, status) in [
                (0, 0, &[0xff, 0][..], 6),
                (1, 2, &[0xcc, 0xd2][..], 6),
                (2, 4, &[1, 0x80, 0x7f][..], 0),
            ] {
                let request = query(
                    &mut stream,
                    slot,
                    &read_request(
                        &path(b"Records", Some(3)),
                        2,
                        (index != 0).then_some(offset),
                    ),
                );
                send(
                    &mut stream,
                    &request,
                    &raw(
                        if index == 0 { 0xcc } else { 0xd2 },
                        status,
                        &[0xa0, 2, 0x34, 0x12],
                        payload,
                    ),
                )
                .unwrap();
            }
            // A zero handle is opaque, not an invalid identity.
            let request = query(
                &mut stream,
                slot,
                &read_request(&path(b"Minimum", None), 1, None),
            );
            send(
                &mut stream,
                &request,
                &raw(0xcc, 0, &[0xa0, 2, 0, 0], &[0xff]),
            )
            .unwrap();
            no_more(&mut stream);
        });
        assert_eq!(record(&home, &cfg).success()["samples"], 4);
        server.join().unwrap();
        let values = rows(&home)
            .into_iter()
            .filter(|r| r["kind"] == "sample")
            .map(|r| r["value"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![
                json!(true),
                expected(0x9ecd, &VENDOR_REPLY[8..], 1, 32),
                expected(0x1234, &[0xff, 0, 0xcc, 0xd2, 1, 0x80, 0x7f], 2, 7),
                expected(0, &[0xff], 1, 1)
            ]
        );
        assert!(values[2].is_object());
        assert!(values[2].get("stride").is_none());
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "-o",
            "raw.jsonl",
            "--format",
            "jsonl",
        ]);
        home.ok(&["data", "export", "run.jsonl", "-o", "raw.csv"]);
        let exported = fs::read_to_string(home.path("raw.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["value"].clone())
            .collect::<Vec<_>>();
        assert_eq!(exported, values);
        let csv = fs::read_to_string(home.path("raw.csv")).unwrap();
        for value in values {
            assert!(csv.contains(&serde_json::to_string(&value).unwrap().replace('"', "\"\"")));
        }
    }
}

#[test]
fn malformed_or_incomplete_raw_transfers_rollback_siblings_and_stop_queries() {
    for mode in [
        "short_descriptor",
        "wrong_prefix",
        "wrong_length",
        "atomic",
        "changed_handle",
        "changed_prefix",
        "missing_descriptor_cont",
        "empty_first",
        "empty_partial",
        "empty_final",
        "over_limit",
        "partial_at_cap",
        "later_over_limit",
        "wrong_service",
        "context",
        "session",
        "reserved",
        "extended",
        "truncated_status",
        "cip_status",
        "route_status",
        "route_partial",
    ] {
        let home = Home::new("logix-raw-invalid");
        let listener = listener();
        let slot = mode.starts_with("route_").then_some(3);
        let cfg = config(
            &listener,
            json!([
            {"name":"ready","tag":"Ready"},
            {"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":4},
            {"name":"later","tag":"Later"}]),
            slot,
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            numeric(&mut stream, slot);
            let request = query(
                &mut stream,
                slot,
                &read_request(&path(b"Record", None), 1, None),
            );
            let continuation = matches!(
                mode,
                "changed_handle"
                    | "changed_prefix"
                    | "missing_descriptor_cont"
                    | "empty_final"
                    | "later_over_limit"
            );
            let descriptor = match mode {
                "short_descriptor" => &[0xa0, 2, 0x34][..],
                "wrong_prefix" => &[0xa1, 2, 0x34, 0x12][..],
                "wrong_length" => &[0xa0, 3, 0x34, 0x12][..],
                "atomic" => &[0xc4, 0, 0, 0][..],
                _ => &[0xa0, 2, 0x34, 0x12][..],
            };
            let payload = match mode {
                "short_descriptor" | "empty_first" | "empty_partial" => &[][..],
                "over_limit" => &[1, 2, 3, 4, 5][..],
                "partial_at_cap" => &[1, 2, 3, 4][..],
                _ => &[1, 2][..],
            };
            let partial = continuation || matches!(mode, "empty_partial" | "partial_at_cap");
            let body = match mode {
                // Header rejection occurs before body reads. Send no body so
                // dropping a failed socket does not race an unread-tail RST.
                "context" | "session" => Vec::new(),
                "reserved" => rr(&[0xcc, 1, 0, 0, 0xa0, 2, 0x34, 0x12, 1]),
                "extended" => rr(&[0xcc, 0, 0, 1, 0, 0, 0xa0, 2, 0x34, 0x12, 1]),
                "truncated_status" => rr(&[0xcc, 0, 1, 2, 0x12, 3]),
                "cip_status" => rr(&[0xcc, 0, 1, 1, 0x12, 3]),
                "route_status" => rr(&[0xd2, 0, 1, 1, 0x12, 3, 0, 0]),
                "route_partial" => raw(0xd2, 6, descriptor, payload),
                _ => raw(
                    if mode == "wrong_service" { 0xd2 } else { 0xcc },
                    u8::from(partial) * 6,
                    descriptor,
                    payload,
                ),
            };
            fast_send(&mut stream, &request, &body, mode);
            if continuation {
                let request = query(
                    &mut stream,
                    slot,
                    &read_request(&path(b"Record", None), 1, Some(2)),
                );
                let body = match mode {
                    "changed_handle" => raw(0xd2, 0, &[0xa0, 2, 0x78, 0x56], &[3]),
                    "changed_prefix" => raw(0xd2, 0, &[0xa1, 2, 0x34, 0x12], &[3]),
                    "missing_descriptor_cont" => raw(0xd2, 0, &[0xa0, 2], &[]),
                    "empty_final" => raw(0xd2, 0, &[0xa0, 2, 0x34, 0x12], &[]),
                    "later_over_limit" => raw(0xd2, 0, &[0xa0, 2, 0x34, 0x12], &[3, 4, 5]),
                    _ => unreachable!(),
                };
                fast_send(&mut stream, &request, &body, "");
            }
            no_more(&mut stream);
        });
        let error = record(&home, &cfg).failure(1);
        let diagnostic = match mode {
            "short_descriptor" | "missing_descriptor_cont" => "complete structure descriptor",
            "wrong_prefix" | "wrong_length" | "atomic" | "changed_prefix" => "exact A0 02",
            "changed_handle" => "descriptor changed",
            "empty_first" | "empty_partial" | "empty_final" => "no value bytes",
            "over_limit" | "partial_at_cap" | "later_over_limit" => "max_bytes",
            "wrong_service" | "route_partial" => "reply service",
            "context" => "encapsulation",
            "session" => "session",
            "reserved" => "CIP reply header",
            "extended" => "extended status",
            "truncated_status" => "truncated CIP extended status",
            "cip_status" | "route_status" => "CIP read rejected",
            _ => unreachable!(),
        };
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        if matches!(mode, "cip_status" | "route_status") {
            assert_eq!(error["details"]["cip_status"], 1);
            assert_eq!(error["details"]["extended_status"], json!([0x312]));
        }
        server.join().unwrap();
        no_samples(&home);
    }
}

#[test]
fn raw_fragments_share_the_original_sibling_and_acquisition_deadline() {
    for (name, timeout, duration) in [("request", 250, 1000), ("acquisition", 3000, 250)] {
        let home = Home::new(name);
        let listener = listener();
        let mut cfg = config(
            &listener,
            json!([
            {"name":"ready","tag":"Ready"},
            {"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":32},
            {"name":"later","tag":"Later"}]),
            None,
        );
        cfg["sources"][0]["timeout_ms"] = json!(time::ms(timeout));
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            thread::sleep(time::millis(100));
            numeric(&mut stream, None);
            let request = query(
                &mut stream,
                None,
                &read_request(&path(b"Record", None), 1, None),
            );
            thread::sleep(time::millis(80));
            send(
                &mut stream,
                &request,
                &raw(0xcc, 6, &[0xa0, 2, 0x34, 0x12], &[1, 2]),
            )
            .unwrap();
            query(
                &mut stream,
                None,
                &read_request(&path(b"Record", None), 1, Some(2)),
            );
            thread::sleep(time::millis(100));
            no_more(&mut stream);
        });
        fs::write(home.path("sources.json"), cfg.to_string()).unwrap();
        let began = Instant::now();
        assert_eq!(
            home.run(&[
                "--json",
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                &time::arg(duration),
                "-o",
                "run.jsonl"
            ])
            .failure(4)["code"],
            "timeout"
        );
        assert!(began.elapsed() < time::secs(2));
        server.join().unwrap();
        no_samples(&home);
    }
}

#[test]
fn raw_transfer_stops_before_an_unaffordable_continuation_or_sibling() {
    let home = Home::new("logix-raw-budget");
    let listener = listener();
    let mut cfg = config(
        &listener,
        json!([
        {"name":"ready","tag":"Ready"},
        {"name":"first","tag":"First","data_type":"logix_struct_raw","max_bytes":1_048_576},
        {"name":"second","tag":"Second","data_type":"logix_struct_raw","max_bytes":1_048_576},
        {"name":"later","tag":"Later"}]),
        None,
    );
    cfg["sources"][0]["timeout_ms"] = json!(5000);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        numeric(&mut stream, None);
        let bytes = vec![0xff; 400_000];
        // Compare the real public serialized shape at adjacent wire boundaries,
        // independently of the adapter's incremental base64 budget formula.
        for (length, fits) in [(385_024, true), (389_120, false)] {
            let readings = json!([
                {"signal":"ready","value":true},
                {"signal":"first","value":expected(0x1234,&bytes,1,1_048_576)},
                {"signal":"second","value":expected(0x1234,&bytes[..length],1,1_048_576)}]);
            assert_eq!(
                serde_json::to_vec(&readings).unwrap().len() <= 1 << 20,
                fits
            );
        }
        transfers(&mut stream, b"First", &bytes, 4096);
        for (index, payload) in bytes.chunks(4096).take(95).enumerate() {
            let offset = index * 4096;
            let request = query(
                &mut stream,
                None,
                &read_request(
                    &path(b"Second", None),
                    1,
                    (index != 0).then_some(u32::try_from(offset).unwrap()),
                ),
            );
            fast_send(
                &mut stream,
                &request,
                &raw(
                    if index == 0 { 0xcc } else { 0xd2 },
                    6,
                    &[0xa0, 2, 0x34, 0x12],
                    payload,
                ),
                "",
            );
        }
        // No 96th second fragment and no Later query after the budget error.
        no_more(&mut stream);
    });
    fs::write(home.path("sources.json"), cfg.to_string()).unwrap();
    let error = home
        .run(&[
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "5s",
            "-o",
            "run.jsonl",
        ])
        .failure(1);
    assert!(error["message"].as_str().unwrap().contains("1 MiB"));
    server.join().unwrap();
    no_samples(&home);
}

#[test]
fn raw_options_and_counts_are_validated_before_connecting() {
    for signal in [
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw"}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":0}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":1_048_577}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":1.5}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":"32"}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":4_294_967_296_u64}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":32,"capacity":82}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":32,"encoding":"ascii"}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":32,"elements":0}),
        json!({"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":32,"elements":257}),
        json!({"name":"raw","tag":"Bad..Name","data_type":"logix_struct_raw","max_bytes":32}),
        json!({"name":"raw","tag":"Record","max_bytes":32}),
        json!({"name":"raw","tag":"Record","data_type":"logix_string","max_bytes":32,"capacity":82}),
        json!({"name":"raw","tag":"Record","data_type":"micro800_string","max_bytes":32,"capacity":252}),
    ] {
        let home = Home::new("logix-raw-config");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let cfg = config(&listener, json!([signal]), None);
        record(&home, &cfg).failure(2);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    for profile in ["micro800", "omron_nj_nx"] {
        let home = Home::new("logix-raw-profile");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let mut cfg = config(
            &listener,
            json!([{"name":"raw","tag":"Record","data_type":"logix_struct_raw","max_bytes":32}]),
            None,
        );
        cfg["sources"][0]["profile"] = json!(profile);
        record(&home, &cfg).failure(2);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn ordinary_atomic_tags_do_not_fall_back_to_opaque_structure_capture() {
    let home = Home::new("logix-no-raw-fallback");
    let listener = listener();
    let cfg = config(
        &listener,
        json!([{"name":"summary","tag":"MachineSummary"}]),
        None,
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        let request = query(&mut stream, None, VENDOR_REQUEST);
        send(&mut stream, &request, &rr(VENDOR_REPLY)).unwrap();
        no_more(&mut stream);
    });
    assert_eq!(record(&home, &cfg).failure(5)["code"], "unsupported");
    server.join().unwrap();
    no_samples(&home);
}
