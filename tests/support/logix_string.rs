//! Read-only member-path fixtures derived from Rockwell's published tag types.
//! They are protocol peers, not captured physical-device transcripts.
use super::{
    Home, Packet, SESSION, accept, config, listener, path, query, read_request, record, register,
    reply, rows, send, unregister,
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

fn member(root: &[u8], name: &[u8], index: Option<u8>) -> Vec<u8> {
    [root, &path(name, index)].concat()
}
fn fast_send(stream: &mut TcpStream, request: &Packet, body: &[u8]) {
    let mut header = request.header;
    header[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
    header[4..8].copy_from_slice(&SESSION.to_le_bytes());
    stream.write_all(&header).unwrap();
    stream.write_all(body).unwrap();
}
fn length(stream: &mut TcpStream, slot: Option<u8>, root: &[u8], value: i32) {
    let request = query(
        stream,
        slot,
        &read_request(&member(root, b"LEN", None), 1, None),
    );
    send(
        stream,
        &request,
        &reply(0xcc, 0, 0xc4, &value.to_le_bytes()),
    )
    .unwrap();
}
fn data(stream: &mut TcpStream, slot: Option<u8>, root: &[u8], payload: &[u8], chunk: usize) {
    let path = member(root, b"DATA", Some(0));
    for (index, bytes) in payload.chunks(chunk).enumerate() {
        let offset = index * chunk;
        let request = query(
            stream,
            slot,
            &read_request(
                &path,
                u16::try_from(payload.len()).unwrap(),
                (index != 0).then_some(u32::try_from(offset).unwrap()),
            ),
        );
        let partial = offset + bytes.len() < payload.len();
        let body = reply(
            if index == 0 { 0xcc } else { 0xd2 },
            if partial { 6 } else { 0 },
            0xc2,
            bytes,
        );
        if payload.len() > 1024 {
            // Keep the large budget/boundary fixture independent of tens of
            // thousands of tiny writes; small cases retain split wire reads.
            fast_send(stream, &request, &body);
        } else {
            send(stream, &request, &body).unwrap();
        }
    }
}
fn string(
    stream: &mut TcpStream,
    slot: Option<u8>,
    root: &[u8],
    active: &[u8],
    capacity: usize,
    chunk: usize,
) {
    let n = i32::try_from(active.len()).unwrap();
    length(stream, slot, root, n);
    let mut payload = vec![0xa5; capacity];
    assert!(active.len() <= payload.len());
    payload[..active.len()].copy_from_slice(active);
    data(stream, slot, root, &payload, chunk);
    length(stream, slot, root, n);
}
fn expected(active: &[u8], capacity: u16, encoding: Option<&str>) -> Value {
    let mut value = json!({"kind":"logix_string","length":active.len(),"configured_capacity":capacity,
        "raw":{"encoding":"base64","scope":"active_data","data":base64::engine::general_purpose::STANDARD.encode(active)}});
    if let Some(encoding) = encoding {
        value["encoding"] = json!(encoding);
        value["text"] = json!(std::str::from_utf8(active).unwrap());
    }
    value
}
fn check_exports(home: &Home, values: &[Value]) {
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
    let exported = fs::read_to_string(home.path("strings.jsonl")).unwrap();
    let exported = exported
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), values.len());
    for (row, value) in exported.iter().zip(values) {
        assert_eq!(&row["value"], value);
    }
    let csv = fs::read_to_string(home.path("strings.csv")).unwrap();
    assert!(csv.contains("logix_string"));
    assert!(csv.contains("active_data"));
    // Parse the CSV's quoted fields independently to compare complete nested
    // objects, including escaped NULs and arbitrary 8-bit raw bytes.
    let mut records = Vec::new();
    for line in csv.lines().skip(1) {
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = !quoted,
                ',' if !quoted => fields.push(std::mem::take(&mut field)),
                _ => field.push(ch),
            }
        }
        assert!(!quoted);
        fields.push(field);
        records.push(fields);
    }
    assert_eq!(records.len(), values.len());
    for (fields, value) in records.iter().zip(values) {
        assert!(
            fields
                .iter()
                .any(|field| serde_json::from_str::<Value>(field).ok().as_ref() == Some(value))
        );
    }
}

#[test]
fn logix_strings_preserve_full_empty_binary_and_indexed_values_through_exports() {
    for slot in [None, Some(3)] {
        let home = Home::new("cip-string-members");
        let listener = listener();
        let mut cfg = config(
            &listener,
            json!([
                {"name":"empty","tag":"Empty","data_type":"logix_string","capacity":82},
                {"name":"full","tag":"Full","data_type":"logix_string","capacity":5},
                {"name":"ascii","tag":"Ascii","data_type":"logix_string","capacity":82,"encoding":"ascii"},
                {"name":"unicode","tag":"Unicode","data_type":"logix_string","capacity":5,"encoding":"utf8"},
                {"name":"array","tag":"Program:Main.Labels[255]","data_type":"logix_string","capacity":7,"elements":2},
                {"name":"maximum","tag":"Maximum","data_type":"logix_string","capacity":65535}
            ]),
            slot,
        );
        cfg["sources"][0]["signals"][0]["capacity"] = serde_json::from_str("8.2e1").unwrap();
        cfg["sources"][0]["signals"][4]["elements"] = serde_json::from_str("2.0").unwrap();
        let maximum = vec![0xff; 65535];
        let expected_maximum = maximum.clone();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            string(&mut stream, slot, &path(b"Empty", None), b"", 82, 488);
            string(
                &mut stream,
                slot,
                &path(b"Full", None),
                &[0, 1, 0x7f, 0x80, 0xff],
                5,
                488,
            );
            string(&mut stream, slot, &path(b"Ascii", None), b"A\0B", 82, 488);
            string(
                &mut stream,
                slot,
                &path(b"Unicode", None),
                "測\0".as_bytes(),
                5,
                1,
            );
            let program = path(b"Program:Main", None);
            let labels = [program, path(b"Labels", Some(255))].concat();
            string(&mut stream, slot, &labels, b"one", 7, 488);
            let labels = [
                path(b"Program:Main", None),
                path(b"Labels", None),
                vec![0x29, 0, 0, 1],
            ]
            .concat();
            string(&mut stream, slot, &labels, b"two", 7, 488);
            string(
                &mut stream,
                slot,
                &path(b"Maximum", None),
                &maximum,
                65535,
                4096,
            );
            unregister(&mut stream);
        });
        record(&home, &cfg).success();
        server.join().unwrap();
        let values = rows(&home)
            .into_iter()
            .filter(|row| row["kind"] == "sample")
            .map(|row| row["value"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![
                expected(b"", 82, None),
                expected(&[0, 1, 0x7f, 0x80, 0xff], 5, None),
                expected(b"A\0B", 82, Some("ascii")),
                expected("測\0".as_bytes(), 5, Some("utf8")),
                json!([expected(b"one", 7, None), expected(b"two", 7, None)]),
                expected(&expected_maximum, 65535, None),
            ]
        );
        assert_eq!(values[2]["raw"]["data"], "QQBC");
        assert!(values[0].get("encoding").is_none());
        check_exports(&home, &values);
    }
}

fn no_more(stream: &mut TcpStream) {
    // Error cleanup may only unregister; any attempted later read is a failure.
    unregister(stream);
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).unwrap(), 0);
}
fn failed(home: &Home) {
    let rows = rows(home);
    assert!(!rows.iter().any(|row| row["kind"] == "sample"));
    assert_eq!(rows.last().unwrap()["status"], "failed");
}

#[test]
fn malformed_members_and_encoding_never_publish_prior_siblings_or_query_later_signals() {
    for mode in [
        "negative",
        "too_long",
        "len_type",
        "data_type",
        "short_data",
        "changed_len",
        "reread_type",
        "ascii",
        "utf8",
    ] {
        let home = Home::new("cip-string-invalid");
        let listener = listener();
        let mut signal =
            json!({"name":"text","tag":"Text","data_type":"logix_string","capacity":5});
        if matches!(mode, "ascii" | "utf8") {
            signal["encoding"] = json!(mode);
        }
        let cfg = config(
            &listener,
            json!([{"name":"ready","tag":"Ready"},signal,{"name":"later","tag":"Later"}]),
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
            let root = path(b"Text", None);
            let request = query(
                &mut stream,
                Some(3),
                &read_request(&member(&root, b"LEN", None), 1, None),
            );
            let length: i32 = match mode {
                "negative" => -1,
                "too_long" => 6,
                _ => 3,
            };
            send(
                &mut stream,
                &request,
                &reply(
                    0xcc,
                    0,
                    if mode == "len_type" { 0xc8 } else { 0xc4 },
                    &length.to_le_bytes(),
                ),
            )
            .unwrap();
            if !matches!(mode, "negative" | "too_long" | "len_type") {
                let request = query(
                    &mut stream,
                    Some(3),
                    &read_request(&member(&root, b"DATA", Some(0)), 5, None),
                );
                let payload = if matches!(mode, "ascii" | "utf8") {
                    &[0xff, 0x80, 0, 0xa5, 0xa5][..]
                } else {
                    &b"abcZZ"[..]
                };
                send(
                    &mut stream,
                    &request,
                    &reply(
                        0xcc,
                        0,
                        if mode == "data_type" { 0xc6 } else { 0xc2 },
                        if mode == "short_data" {
                            &payload[..4]
                        } else {
                            payload
                        },
                    ),
                )
                .unwrap();
                if !matches!(mode, "data_type" | "short_data") {
                    let request = query(
                        &mut stream,
                        Some(3),
                        &read_request(&member(&root, b"LEN", None), 1, None),
                    );
                    let after: i32 = if mode == "changed_len" { 4 } else { 3 };
                    send(
                        &mut stream,
                        &request,
                        &reply(
                            0xcc,
                            0,
                            if mode == "reread_type" { 0xc8 } else { 0xc4 },
                            &after.to_le_bytes(),
                        ),
                    )
                    .unwrap();
                }
            }
            no_more(&mut stream);
        });
        let error = record(&home, &cfg).failure(1);
        let diagnostic = match mode {
            "negative" | "too_long" => "negative or exceeds",
            "len_type" | "data_type" | "reread_type" => "wrong atomic type",
            "short_data" => "size differs",
            "changed_len" => "LEN changed",
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
fn length_reread_and_data_continuations_share_the_original_poll_deadline() {
    for phase in ["data", "reread"] {
        let home = Home::new("cip-string-deadline");
        let listener = listener();
        let mut cfg = config(
            &listener,
            json!([{"name":"text","tag":"Text","data_type":"logix_string","capacity":5}]),
            None,
        );
        cfg["sources"][0]["timeout_ms"] = json!(250);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let root = path(b"Text", None);
            let request = query(
                &mut stream,
                None,
                &read_request(&member(&root, b"LEN", None), 1, None),
            );
            thread::sleep(Duration::from_millis(60));
            send(
                &mut stream,
                &request,
                &reply(0xcc, 0, 0xc4, &3_i32.to_le_bytes()),
            )
            .unwrap();
            let request = query(
                &mut stream,
                None,
                &read_request(&member(&root, b"DATA", Some(0)), 5, None),
            );
            thread::sleep(Duration::from_millis(60));
            send(&mut stream, &request, &reply(0xcc, 6, 0xc2, b"ab")).unwrap();
            let request = query(
                &mut stream,
                None,
                &read_request(&member(&root, b"DATA", Some(0)), 5, Some(2)),
            );
            thread::sleep(Duration::from_millis(if phase == "data" {
                160
            } else {
                60
            }));
            if phase == "reread" {
                send(&mut stream, &request, &reply(0xd2, 0, 0xc2, b"cZZ")).unwrap();
                query(
                    &mut stream,
                    None,
                    &read_request(&member(&root, b"LEN", None), 1, None),
                );
                thread::sleep(Duration::from_millis(100));
                // Deliberately leave this response unanswered. A fresh
                // request budget would wait longer; the original one expires.
            }
            // Keep the peer alive until the client closes; no new query can
            // be confused with a late data/reread reply on the next poll.
            no_more(&mut stream);
        });
        let began = Instant::now();
        assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
        assert!(began.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        failed(&home);
    }
}

#[test]
fn serialized_string_arrays_stop_before_an_unaffordable_data_query_and_publish_nothing() {
    let home = Home::new("cip-string-budget");
    let listener = listener();
    let cfg = config(
        &listener,
        json!([
            {"name":"ready","tag":"Ready"},
            {"name":"texts","tag":"Texts[0]","data_type":"logix_string","capacity":65535,"elements":4,"encoding":"ascii"},
            {"name":"later","tag":"Later"}
        ]),
        None,
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        let request = query(
            &mut stream,
            None,
            &read_request(&path(b"Ready", None), 1, None),
        );
        send(&mut stream, &request, &reply(0xcc, 0, 0xc1, &[1])).unwrap();
        for index in 0..2 {
            string(
                &mut stream,
                None,
                &path(b"Texts", Some(index)),
                &vec![0; 65535],
                65535,
                4096,
            );
        }
        // Two complete values use about960KiB. After the third LEN, even
        // minimum base64/text output cannot fit, so do not request its DATA.
        length(&mut stream, None, &path(b"Texts", Some(2)), 65535);
        no_more(&mut stream);
    });
    let error = record(&home, &cfg).failure(1);
    assert!(error["message"].as_str().unwrap().contains("1 MiB"));
    server.join().unwrap();
    failed(&home);
}

#[test]
fn invalid_string_configuration_is_rejected_before_connecting() {
    for signal in [
        json!({"name":"s","tag":"Text","data_type":"logix_string"}),
        json!({"name":"s","tag":"Text","data_type":"logix_string","capacity":0}),
        json!({"name":"s","tag":"Text","data_type":"logix_string","capacity":65536}),
        json!({"name":"s","tag":"Text","data_type":"logix_string","capacity":2.5}),
        json!({"name":"s","tag":"Text","data_type":"logix_string","capacity":"82"}),
        json!({"name":"s","tag":"Text","capacity":82}),
        json!({"name":"s","tag":"Text","encoding":"ascii"}),
        json!({"name":"s","tag":"Text","data_type":"logix_string","capacity":82,"encoding":"latin1"}),
        json!({"name":"s","tag":"Text","data_type":"logix_string","capacity":82,"elements":2}),
        json!({"name":"s","tag":"Text[4294967295]","data_type":"logix_string","capacity":82,"elements":2}),
        json!({"name":"s","tag":"Text..X","data_type":"logix_string","capacity":82}),
    ] {
        let home = Home::new("cip-string-config");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let cfg = config(&listener, json!([signal]), None);
        record(&home, &cfg).failure(2);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    for profile in ["omron_nj_nx", "micro800"] {
        let home = Home::new("cip-string-profile");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let mut cfg = config(
            &listener,
            json!([{"name":"s","tag":"Text","data_type":"logix_string","capacity":82}]),
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
