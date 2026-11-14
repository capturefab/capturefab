//! Exact DA `SHORT_STRING` replies through the real recorder CLI.
//! Wire values are spec-derived; no physical or stock-server capture is claimed.
use super::{
    Home, Packet, SESSION, accept, close, connected_reply, listener, open, open_reply,
    open_request, path, read_tag, record, register, rows, rr, send, source, unit_body, unregister,
};
use base64::Engine as _;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

fn expected(data: &[u8], capacity: u16, encoding: Option<&str>) -> Value {
    let mut value = json!({"kind":"micro800_string","length":data.len(),"configured_capacity":capacity,
        "raw":{"encoding":"base64","scope":"active_data","data":base64::engine::general_purpose::STANDARD.encode(data)}});
    if let Some(encoding) = encoding {
        value["encoding"] = json!(encoding);
        value["text"] = json!(std::str::from_utf8(data).unwrap());
    }
    value
}
fn fast_send(stream: &mut TcpStream, request: &Packet, body: &[u8]) {
    let mut header = request.header;
    header[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
    header[4..8].copy_from_slice(&SESSION.to_le_bytes());
    stream.write_all(&header).unwrap();
    stream.write_all(body).unwrap();
}
fn string(stream: &mut TcpStream, originator: u32, sequence: u16, tag: &[u8], data: &[u8]) {
    let request = read_tag(stream, sequence, tag, 1, 0);
    let mut payload = vec![u8::try_from(data.len()).unwrap()];
    payload.extend_from_slice(data);
    fast_send(
        stream,
        &request,
        &unit_body(originator, sequence, &connected_reply(0, 0xda, &payload)),
    );
}
fn no_more(stream: &mut TcpStream) {
    unregister(stream);
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).unwrap(), 0);
}
fn failed(home: &Home) {
    let rows = rows(home);
    assert!(!rows.iter().any(|r| r["kind"] == "sample"));
    assert_eq!(rows.last().unwrap()["status"], "failed");
}

#[test]
fn micro800_short_strings_keep_exact_bytes_arrays_and_fragment_offsets_through_exports() {
    let home = Home::new("micro800-short-string");
    let listener = listener();
    let mut cfg = source(
        &listener,
        json!([
            {"name":"ready","tag":"Ready"},
            {"name":"empty","tag":"Empty","data_type":"micro800_string","capacity":252},
            {"name":"binary","tag":"Binary","data_type":"micro800_string","capacity":252},
            {"name":"unicode","tag":"Unicode","data_type":"micro800_string","capacity":252,"encoding":"utf8"},
            {"name":"labels","tag":"Labels[255]","data_type":"micro800_string","capacity":252,"elements":2},
            {"name":"maximum","tag":"Maximum","data_type":"micro800_string","capacity":252},
            {"name":"ascii","tag":"Initial","data_type":"micro800_string","capacity":1,"encoding":"ascii"}
        ]),
    );
    cfg["sources"][0]["signals"][1]["capacity"] = serde_json::from_str("2.52e2").unwrap();
    cfg["sources"][0]["signals"][4]["elements"] = serde_json::from_str("2.0").unwrap();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let (identity, originator) = open(&mut stream, 1_000_000);
        let r = read_tag(&mut stream, 1, &path(b"Ready", None), 1, 0);
        send(
            &mut stream,
            &r,
            &unit_body(originator, 1, &connected_reply(0, 0xc1, &[1])),
        )
        .unwrap();
        string(&mut stream, originator, 2, &path(b"Empty", None), &[]);
        // Offsets include the one-byte length field, excluding repeated DA00.
        for (sequence, offset, payload, status) in [
            (3, 0, &[4][..], 6),
            (4, 1, &[b'A', 0][..], 6),
            (5, 3, &[0xff, 0x80][..], 0),
        ] {
            let r = read_tag(&mut stream, sequence, &path(b"Binary", None), 1, offset);
            send(
                &mut stream,
                &r,
                &unit_body(
                    originator,
                    sequence,
                    &connected_reply(status, 0xda, payload),
                ),
            )
            .unwrap();
        }
        string(
            &mut stream,
            originator,
            6,
            &path(b"Unicode", None),
            "測\0".as_bytes(),
        );
        string(
            &mut stream,
            originator,
            7,
            &path(b"Labels", Some(255)),
            b"one",
        );
        let tag = [path(b"Labels", None), vec![0x29, 0, 0, 1]].concat();
        string(&mut stream, originator, 8, &tag, b"two");
        string(
            &mut stream,
            originator,
            9,
            &path(b"Maximum", None),
            &[0xff; 252],
        );
        string(&mut stream, originator, 10, &path(b"Initial", None), b"A");
        close(&mut stream, identity, true);
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 7);
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
            expected(&[], 252, None),
            expected(&[b'A', 0, 0xff, 0x80], 252, None),
            expected("測\0".as_bytes(), 252, Some("utf8")),
            json!([expected(b"one", 252, None), expected(b"two", 252, None)]),
            expected(&[0xff; 252], 252, None),
            expected(b"A", 1, Some("ascii"))
        ]
    );
    assert!(values[1].get("encoding").is_none());
    assert_eq!(values[2]["raw"]["data"], "QQD/gA==");
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "strings.jsonl",
        "--format",
        "jsonl",
    ]);
    home.ok(&["data", "export", "run.jsonl", "-o", "strings.csv"]);
    let exported = fs::read_to_string(home.path("strings.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap()["value"].clone())
        .collect::<Vec<_>>();
    assert_eq!(exported, values);
    let csv = fs::read_to_string(home.path("strings.csv")).unwrap();
    // Complete nested value JSON remains exactly available after CSV unquoting.
    for value in values {
        let value = serde_json::to_string(&value).unwrap().replace('"', "\"\"");
        assert!(csv.contains(&value));
    }
}

#[test]
fn malformed_short_strings_never_publish_prior_values_or_query_later_signals() {
    for mode in [
        "wrong_type",
        "type_pad",
        "missing_count",
        "over252",
        "configured_cap",
        "short",
        "trailing",
        "padded256",
        "changed_type",
        "empty_partial",
        "partial_complete",
        "ascii",
        "utf8",
    ] {
        let home = Home::new("micro800-string-invalid");
        let listener = listener();
        let mut signal =
            json!({"name":"text","tag":"Text","data_type":"micro800_string","capacity":252});
        if mode == "configured_cap" {
            signal["capacity"] = json!(2);
        }
        if matches!(mode, "ascii" | "utf8") {
            signal["encoding"] = json!(mode);
        }
        let cfg = source(
            &listener,
            json!([{"name":"first","tag":"First"},signal,{"name":"later","tag":"Later"}]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let (_, originator) = open(&mut stream, 1_000_000);
            let r = read_tag(&mut stream, 1, &path(b"First", None), 1, 0);
            send(
                &mut stream,
                &r,
                &unit_body(originator, 1, &connected_reply(0, 0xc3, &[42, 0])),
            )
            .unwrap();
            let r = read_tag(&mut stream, 2, &path(b"Text", None), 1, 0);
            let payload = match mode {
                "missing_count" => vec![],
                "over252" => [vec![253], vec![b'A'; 253]].concat(),
                "short" => vec![3, b'A', b'B'],
                "trailing" => vec![2, b'A', b'B', b'C'],
                "padded256" => [vec![3, b'A', b'B', b'C'], vec![0; 252]].concat(),
                "changed_type" => vec![3, b'A'],
                "empty_partial" => vec![3],
                "partial_complete" => vec![0],
                "ascii" | "utf8" => vec![3, 0xff, 0x80, 0],
                _ => vec![3, b'A', b'B', b'C'],
            };
            let status = if matches!(mode, "changed_type" | "empty_partial" | "partial_complete") {
                6
            } else {
                0
            };
            let kind = match mode {
                "wrong_type" => 0xd0,
                "type_pad" => 0xffda,
                _ => 0xda,
            };
            fast_send(
                &mut stream,
                &r,
                &unit_body(originator, 2, &connected_reply(status, kind, &payload)),
            );
            if matches!(mode, "changed_type" | "empty_partial") {
                let offset = if mode == "changed_type" { 2 } else { 1 };
                let r = read_tag(&mut stream, 3, &path(b"Text", None), 1, offset);
                let cip = if mode == "changed_type" {
                    connected_reply(0, 0xc2, b"BC")
                } else {
                    connected_reply(6, 0xda, &[])
                };
                send(&mut stream, &r, &unit_body(originator, 3, &cip)).unwrap();
            }
            no_more(&mut stream);
        });
        let error = record(&home, &cfg).failure(1);
        let diagnostic = match mode {
            "wrong_type" | "type_pad" | "changed_type" => "wrong type",
            "missing_count" => "missing its byte count",
            "over252" | "configured_cap" => "byte count exceeds",
            "short" => "size differs",
            "trailing" | "padded256" => "exceeds requested",
            "empty_partial" => "no progress",
            "partial_complete" => "partial status",
            "ascii" => "not ASCII",
            "utf8" => "valid UTF-8",
            _ => unreachable!(),
        };
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        server.join().unwrap();
        failed(&home);
    }
}

#[test]
fn short_string_partial_read_cannot_refresh_the_handshake_or_acquisition_deadline() {
    for (name, timeout, duration) in [("request", 250, "1s"), ("acquisition", 3000, "250ms")] {
        let home = Home::new(name);
        let listener = listener();
        let mut cfg = source(
            &listener,
            json!([{"name":"text","tag":"Text","data_type":"micro800_string","capacity":252}]),
        );
        cfg["sources"][0]["timeout_ms"] = json!(timeout);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let (r, identity, originator) = open_request(&mut stream);
            thread::sleep(Duration::from_millis(100));
            send(
                &mut stream,
                &r,
                &rr(&open_reply(identity, originator, 1_000_000)),
            )
            .unwrap();
            let r = read_tag(&mut stream, 1, &path(b"Text", None), 1, 0);
            thread::sleep(Duration::from_millis(80));
            send(
                &mut stream,
                &r,
                &unit_body(originator, 1, &connected_reply(6, 0xda, &[3])),
            )
            .unwrap();
            read_tag(&mut stream, 2, &path(b"Text", None), 1, 1);
            thread::sleep(Duration::from_millis(100));
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
                duration,
                "-o",
                "run.jsonl"
            ])
            .failure(4)["code"],
            "timeout"
        );
        assert!(began.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        failed(&home);
    }
}

#[test]
fn short_string_arrays_stop_at_the_exact_shared_json_budget() {
    let home = Home::new("micro800-string-budget");
    let listener = listener();
    let mut cfg = source(
        &listener,
        json!([
            {"name":"ready","tag":"Ready"},
            {"name":"a0","tag":"Texts[0]","data_type":"micro800_string","capacity":252,"elements":256,"encoding":"ascii"},
            {"name":"a1","tag":"Texts[256]","data_type":"micro800_string","capacity":252,"elements":256,"encoding":"ascii"},
            {"name":"a2","tag":"Texts[512]","data_type":"micro800_string","capacity":252,"elements":256,"encoding":"ascii"},
            {"name":"later","tag":"Later"}
        ]),
    );
    cfg["sources"][0]["timeout_ms"] = json!(3000);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let (_, originator) = open(&mut stream, 1_000_000);
        let r = read_tag(&mut stream, 1, &path(b"Ready", None), 1, 0);
        send(
            &mut stream,
            &r,
            &unit_body(originator, 1, &connected_reply(0, 0xc1, &[1])),
        )
        .unwrap();
        // This public value shape is 2002 serialized bytes. Two 256-element
        // arrays fit; the 12th value of the third crosses 1 MiB. There
        // must be no 525th STRING query or later numeric query.
        assert_eq!(
            serde_json::to_vec(&expected(&[0; 252], 252, Some("ascii")))
                .unwrap()
                .len(),
            2002
        );
        for index in 0_u16..524 {
            let tag = u8::try_from(index).map_or_else(
                |_| {
                    [
                        path(b"Texts", None),
                        vec![0x29, 0],
                        index.to_le_bytes().to_vec(),
                    ]
                    .concat()
                },
                |index| path(b"Texts", Some(index)),
            );
            string(&mut stream, originator, index + 2, &tag, &[0; 252]);
        }
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
            "3s",
            "-o",
            "run.jsonl",
        ])
        .failure(1);
    assert!(error["message"].as_str().unwrap().contains("1 MiB"));
    server.join().unwrap();
    failed(&home);
}

#[test]
fn short_string_options_and_bounds_are_validated_before_connecting() {
    for signal in [
        json!({"name":"s","tag":"Text","data_type":"micro800_string"}),
        json!({"name":"s","tag":"Text","data_type":"micro800_string","capacity":0}),
        json!({"name":"s","tag":"Text","data_type":"micro800_string","capacity":253}),
        json!({"name":"s","tag":"Text","data_type":"micro800_string","capacity":2.5}),
        json!({"name":"s","tag":"Text","data_type":"micro800_string","capacity":"252"}),
        json!({"name":"s","tag":"Text","data_type":"micro800_string","capacity":252,"encoding":"latin1"}),
        json!({"name":"s","tag":"Text","data_type":"micro800_string","capacity":252,"elements":2}),
        json!({"name":"s","tag":"Text[4294967295]","data_type":"micro800_string","capacity":252,"elements":2}),
        json!({"name":"s","tag":"Program:Main.Text","data_type":"micro800_string","capacity":252}),
    ] {
        let home = Home::new("micro800-string-config");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let cfg = source(&listener, json!([signal]));
        record(&home, &cfg).failure(2);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    for profile in ["logix", "omron_nj_nx"] {
        let home = Home::new("micro800-string-profile");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let mut cfg = source(
            &listener,
            json!([{"name":"s","tag":"Text","data_type":"micro800_string","capacity":252}]),
        );
        cfg["sources"][0]["profile"] = json!(profile);
        cfg["sources"][0]
            .as_object_mut()
            .unwrap()
            .remove("originator_vendor_id");
        record(&home, &cfg).failure(2);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
