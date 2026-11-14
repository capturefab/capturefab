//! Legacy `CSPv4` direct SLC reads through the CLI, with Redpoint's registration
//! and a bounded pseudo-peer based on `PCCCComm` commit 223cfa29.

use super::{accept, listener, readings, record, send, support::cli::Home};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::Duration,
};

const REGISTER: [u8; 28] = [
    1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
const SESSION: [u8; 4] = [1, 2, 3, 4];

fn registration(stream: &mut TcpStream) -> [u8; 28] {
    let mut request = [0; 28];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(request, REGISTER);
    request[0] = 2;
    request[4..8].copy_from_slice(&SESSION);
    request
}

fn command(stream: &mut TcpStream, transaction: u16, control: u8, fields: &[u8]) {
    let mut header = [0; 28];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[..2], &[1, 7]);
    assert_eq!(&header[4..8], &SESSION);
    assert_eq!(&header[8..], &[0; 20]);
    assert_eq!(
        usize::from(u16::from_be_bytes([header[2], header[3]])),
        9 + fields.len()
    );
    let mut body = vec![0; 9 + fields.len()];
    stream.read_exact(&mut body).unwrap();
    assert_eq!(&body[..6], &[0, control, 0, 0, 0x0f, 0]);
    assert_eq!(&body[6..8], &transaction.to_le_bytes());
    assert_eq!(body[8], 0xa2, "only read commands allowed");
    assert_eq!(&body[9..], fields);
}

fn response(transaction: u16, control: u8, data: &[u8]) -> Vec<u8> {
    let mut reply = vec![2, 7];
    reply.extend(u16::try_from(8 + data.len()).unwrap().to_be_bytes());
    reply.extend(SESSION);
    reply.extend([0; 20]);
    reply.extend([0, control, 0, 0, 0x4f, 0]);
    reply.extend(transaction.to_le_bytes());
    reply.extend_from_slice(data);
    reply
}

fn closed(stream: &mut TcpStream) {
    assert_eq!(
        stream.read(&mut [0]).unwrap(),
        0,
        "unexpected extra command"
    );
}

fn config(listener: &TcpListener, signals: &Value) -> Value {
    json!({"version":1,"sources":[{
        "id":"plc","protocol":"pccc_csp","address":listener.local_addr().unwrap(),
        "every_ms":3_600_000,"timeout_ms":1000,"signals":signals
    }]})
}

#[test]
fn csp_direct_reads_known_words_segmented_floats_and_bit_and_preserves_exports() {
    let home = Home::new("pccc-csp");
    let listener = listener();
    let mut config = config(
        &listener,
        &json!([
            {"name":"words","address":"N7:0","elements":3},
            {"name":"floats","address":"F8:0","elements":2},
            {"name":"bit","address":"B3:0/15"},
            {"name":"last","address":"N255:255","data_type":"u16"}
        ]),
    );
    let cap: Value = serde_json::from_str("6e0").unwrap();
    config["sources"][0]["max_read_bytes"] = cap;
    let worker = thread::spawn(move || {
        let mut stream = accept(&listener);
        let register = registration(&mut stream);
        send(&mut stream, &register);
        command(&mut stream, 1, 0, &[6, 7, 0x89, 0, 0]);
        // PCCCComm's SLC fallback N7 words, with dynamic session/TNS.
        send(
            &mut stream,
            &response(1, 0, &[0x7b, 0, 0xc8, 1, 0xeb, 0xfc]),
        );

        command(&mut stream, 2, 0, &[4, 8, 0x8a, 0, 0]);
        let mut reply = response(2, 0, &1.25_f32.to_le_bytes());
        reply[20] = 0x42; // This data context is opaque.
        send(&mut stream, &reply);

        command(&mut stream, 3, 0, &[4, 8, 0x8a, 1, 0]);
        send(&mut stream, &response(3, 0, &(-2.5_f32).to_le_bytes()));

        command(&mut stream, 4, 0, &[2, 3, 0x85, 0, 0]);
        send(&mut stream, &response(4, 5, &[0, 0x80]));

        command(
            &mut stream,
            5,
            0,
            &[2, 0xff, 0xff, 0, 0x89, 0xff, 0xff, 0, 0],
        );
        send(&mut stream, &response(5, 0, &[0xff, 0xff]));
        closed(&mut stream);
    });
    let run = record(&home, &config, "500ms");
    assert!(run.code == Some(0), "{}", run.stderr);
    worker.join().unwrap();

    let rows = readings(&home);
    assert_eq!(
        rows.iter().map(|row| &row["value"]).collect::<Vec<_>>(),
        [
            &json!([123, 456, -789]),
            &json!([1.25, -2.5]),
            &json!(true),
            &json!(65535)
        ]
    );
    let csv = home.run(&["--json", "data", "export", "run.jsonl", "-o", "out.csv"]);
    assert!(csv.code == Some(0), "{}", csv.stderr);
    assert!(
        fs::read_to_string(home.path("out.csv"))
            .unwrap()
            .contains("[123,456,-789]")
    );
    let jsonl = home.run(&[
        "--json",
        "data",
        "export",
        "run.jsonl",
        "--format",
        "jsonl",
        "-o",
        "out.jsonl",
    ]);
    assert!(jsonl.code == Some(0), "{}", jsonl.stderr);
    let expected = rows
        .into_iter()
        .map(|mut row| {
            row.as_object_mut().unwrap().remove("kind");
            row
        })
        .collect::<Vec<_>>();
    let actual = fs::read_to_string(home.path("out.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn csp_refuses_invalid_registration_before_sending_pccc() {
    for (index, value, message) in [
        (0, 3, "mode or submode"),
        (1, 7, "mode or submode"),
        (3, 1, "registration length, session or context"),
        (15, 4, "registration length, session or context"),
    ] {
        let home = Home::new("pccc-csp-register");
        let listener = listener();
        let config = config(&listener, &json!([{"name":"word","address":"N7:0"}]));
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            let mut reply = registration(&mut stream);
            reply[index] = value;
            send(&mut stream, &reply);
            closed(&mut stream);
        });
        let run = record(&home, &config, "1s");
        let error = run.failure(1);
        worker.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        assert_eq!(readings(&home), Vec::<Value>::new());
    }

    let home = Home::new("pccc-csp-zero-session");
    let listener = listener();
    let config = config(&listener, &json!([{"name":"word","address":"N7:0"}]));
    let worker = thread::spawn(move || {
        let mut stream = accept(&listener);
        let mut reply = registration(&mut stream);
        reply[4..8].fill(0);
        send(&mut stream, &reply);
        closed(&mut stream);
    });
    let run = record(&home, &config, "1s");
    let error = run.failure(1);
    worker.join().unwrap();
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("registration length, session or context")
    );
    assert_eq!(readings(&home), Vec::<Value>::new());
}

#[test]
fn csp_header_and_transaction_failures_are_atomic() {
    for (kind, cause) in [
        ("mode", "mode or submode"),
        ("submode", "mode or submode"),
        ("session", "session differs"),
        ("foreign_status", "session differs"),
        ("status", "CSP request rejected"),
        ("length", "invalid PCCC reply length"),
        ("destination", "direct local LSAP"),
        ("source", "direct local LSAP"),
        ("routed", "direct local LSAP"),
        ("command", "invalid PCCC response command"),
        ("tns", "transaction differs"),
        ("pccc_status", "target rejected typed file read"),
        ("width", "data length differs"),
    ] {
        let home = Home::new("pccc-csp-atomic");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([
                {"name":"first","address":"N7:0"},
                {"name":"second","address":"N7:1"}
            ]),
        );
        config["sources"][0]["lsap_control"] = json!(5);
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            let register = registration(&mut stream);
            send(&mut stream, &register);
            command(&mut stream, 1, 5, &[2, 7, 0x89, 0, 0]);
            send(&mut stream, &response(1, 5, &[1, 0]));
            command(&mut stream, 2, 5, &[2, 7, 0x89, 1, 0]);
            let mut reply = response(2, 5, &[2, 0]);
            match kind {
                "mode" => reply[0] = 4,
                "submode" => reply[1] = 1,
                "session" => reply[7] ^= 1,
                "foreign_status" => {
                    reply[7] ^= 1;
                    reply[11] = 1;
                }
                "status" => reply[8..12].copy_from_slice(&0x0102_0304_u32.to_be_bytes()),
                "length" => reply[2..4].copy_from_slice(&233_u16.to_be_bytes()),
                "destination" => reply[28] = 1,
                "source" => reply[30] = 1,
                "routed" => reply[31] = 1,
                "command" => reply[32] = 0x41,
                "tns" => reply[34] ^= 1,
                "pccc_status" => {
                    reply[33] = 0xf0;
                    reply[36] = 7;
                }
                "width" => {
                    reply[3] += 1;
                    reply.push(0);
                }
                _ => unreachable!(),
            }
            if matches!(
                kind,
                "mode" | "submode" | "session" | "foreign_status" | "status" | "length"
            ) {
                reply.truncate(28); // Header rejection may close immediately.
            }
            send(&mut stream, &reply);
            closed(&mut stream);
        });
        let run = record(&home, &config, "1s");
        let error = run.failure(1);
        worker.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{kind}: {error}"
        );
        if kind == "status" {
            assert_eq!(error["details"]["csp_status"], 0x0102_0304);
        }
        if kind == "pccc_status" {
            assert_eq!(error["details"]["pccc_status"], 0xf0);
            assert_eq!(error["details"]["extended_status"], 7);
        }
        assert_eq!(readings(&home), Vec::<Value>::new());
    }
}

#[test]
fn csp_uses_one_deadline_across_segments_and_closes_on_timeout() {
    let home = Home::new("pccc-csp-deadline");
    let listener = listener();
    let mut config = config(
        &listener,
        &json!([{"name":"array","address":"N7:0","elements":3}]),
    );
    config["sources"][0]["max_read_bytes"] = json!(4);
    config["sources"][0]["timeout_ms"] = json!(700);
    let worker = thread::spawn(move || {
        let mut stream = accept(&listener);
        let register = registration(&mut stream);
        send(&mut stream, &register);
        command(&mut stream, 1, 0, &[4, 7, 0x89, 0, 0]);
        thread::sleep(Duration::from_millis(420));
        send(&mut stream, &response(1, 0, &[1, 0, 2, 0]));
        command(&mut stream, 2, 0, &[2, 7, 0x89, 2, 0]);
        thread::sleep(Duration::from_millis(420));
        let _ = stream.write_all(&response(2, 0, &[3, 0]));
        let closed = stream.read(&mut [0]);
        assert!(
            matches!(closed, Ok(0))
                || closed.is_err_and(|error| error.kind() == io::ErrorKind::ConnectionReset)
        );
    });
    let run = record(&home, &config, "2s");
    let error = run.failure(4);
    worker.join().unwrap();
    assert_eq!(error["code"], "timeout");
    assert_eq!(readings(&home), Vec::<Value>::new());
}

#[test]
fn csp_rejects_unsupported_file_ranges_before_connecting() {
    for (address, elements) in [("L19:0", 1), ("N256:0", 1), ("N7:255", 2), ("B2:0", 1)] {
        let home = Home::new("pccc-csp-config");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let config = config(
            &listener,
            &json!([{"name":"bad","address":address,"elements":elements}]),
        );
        let run = record(&home, &config, "1s");
        assert_eq!(run.failure(2)["code"], "usage");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn csp_whole_timer_counter_elements_chunk_and_export_without_word_stride_guessing() {
    let home = Home::new("csp-tc");
    let listener = listener();
    let mut cfg = config(
        &listener,
        &json!([
            {"name":"timer","address":"T4:0"},
            {"name":"counter","address":"C5:7","data_type":"counter","elements":2},
            {"name":"array","address":"T9:0","elements":38},
            {"name":"old","address":"N7:0"}
        ]),
    );
    cfg["sources"][0]["max_read_bytes"] = 224.into();
    let worker = thread::spawn(move || {
        let mut stream = accept(&listener);
        let register = registration(&mut stream);
        send(&mut stream, &register);
        let cases = [
            (vec![6, 4, 0x86, 0, 0], vec![5, 0xc0, 100, 0, 17, 0]),
            (
                vec![12, 5, 0x87, 7, 0],
                vec![1, 0xa4, 0, 0x80, 0xff, 0x7f, 2, 0x58, 0xff, 0xff, 0, 0],
            ),
            (vec![222, 9, 0x86, 0, 0], vec![0; 222]),
            (vec![6, 9, 0x86, 37, 0], vec![0; 6]),
            (vec![2, 7, 0x89, 0, 0], vec![0xff, 0xff]),
        ];
        for (i, (fields, data)) in cases.iter().enumerate() {
            let tns = u16::try_from(i + 1).unwrap();
            command(&mut stream, tns, 0, fields);
            send(&mut stream, &response(tns, 0, data));
        }
        closed(&mut stream);
    });
    record(&home, &cfg, "300ms").success();
    worker.join().unwrap();
    let rows = readings(&home);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["value"], super::pccc_tc::timer());
    assert_eq!(rows[1]["value"], super::pccc_tc::counter());
    assert_eq!(rows[2]["value"].as_array().unwrap().len(), 38);
    assert!(
        rows[2]["value"]
            .as_array()
            .unwrap()
            .iter()
            .all(|value| value["native_words_raw"] == json!([0, 0, 0]))
    );
    assert_eq!(rows[3]["value"], -1);
    assert!(
        rows.iter()
            .all(|row| row["elapsed_ns"] == rows[0]["elapsed_ns"]
                && row.get("source_time").is_none())
    );
    super::pccc_tc::exports(&home, &rows);
}

#[test]
fn csp_incomplete_late_timer_segment_retains_only_complete_prior_poll() {
    let home = Home::new("csp-tc-prefix");
    let listener = listener();
    let mut cfg = config(
        &listener,
        &json!([{"name":"timers","address":"T4:0","elements":38}]),
    );
    cfg["sources"][0]["max_read_bytes"] = 224.into();
    cfg["sources"][0]["every_ms"] = 200.into();
    let worker = thread::spawn(move || {
        let mut stream = accept(&listener);
        let register = registration(&mut stream);
        send(&mut stream, &register);
        for poll in 0..2 {
            let tns = 1 + poll * 2;
            command(&mut stream, tns, 0, &[222, 4, 0x86, 0, 0]);
            send(&mut stream, &response(tns, 0, &[0; 222]));
            command(&mut stream, tns + 1, 0, &[6, 4, 0x86, 37, 0]);
            send(
                &mut stream,
                &response(tns + 1, 0, &vec![0; if poll == 0 { 6 } else { 5 }]),
            );
        }
        closed(&mut stream);
    });
    assert_eq!(
        record(&home, &cfg, "550ms").failure(1)["code"],
        "incomplete"
    );
    worker.join().unwrap();
    let rows = readings(&home);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["value"].as_array().unwrap().len(), 38);
}

fn st_hex(text: &str) -> Vec<u8> {
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    assert_eq!(tail, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn st_literal(id: &str, encoding: Option<&str>) -> (Vec<u8>, Value) {
    let literals: Value =
        serde_json::from_str(include_str!("../fixtures/pccc/st-literals.json")).unwrap();
    let item = literals["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == id)
        .unwrap();
    let raw = item["native_hex_chunks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chunk| st_hex(chunk.as_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(raw.len(), 84);
    let mut value = json!({"kind":"pccc_string","length_word_raw":item["length_word_raw"],
        "capacity":82,"length_status":item["length_status"],
        "raw":{"encoding":"base64","scope":"native_element","data":STANDARD.encode(&raw)}});
    if let Some(active) = item.get("active_hex") {
        value["active"] = json!({"encoding":"base64","scope":"active_data",
            "data":STANDARD.encode(st_hex(active.as_str().unwrap()))});
        if let Some(encoding) = encoding {
            value["encoding"] = encoding.into();
            value["text"] = item[match encoding {
                "ascii" => "ascii_text",
                "utf8" => "utf8_text",
                _ => panic!("unknown test encoding"),
            }]
            .clone();
        }
    }
    (raw, value)
}

fn st_exports(home: &Home, samples: &[Value], incomplete: bool) {
    for (format, path) in [("jsonl", "st.jsonl"), ("csv", "st.csv")] {
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
        let text = fs::read_to_string(home.path(path)).unwrap();
        if format == "jsonl" {
            let expected = samples
                .iter()
                .map(|sample| {
                    let mut sample = sample.clone();
                    sample.as_object_mut().unwrap().remove("kind");
                    sample
                })
                .collect::<Vec<_>>();
            let actual = text
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        } else {
            let actual = text
                .lines()
                .skip(1)
                .map(|line| {
                    let field = line.splitn(9, ',').nth(8).unwrap();
                    let text = field.strip_prefix('"').map_or_else(
                        || field.to_owned(),
                        |quoted| quoted.strip_suffix('"').unwrap().replace("\"\"", "\""),
                    );
                    serde_json::from_str::<Value>(&text).unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                samples
                    .iter()
                    .map(|sample| sample["value"].clone())
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn csp_whole_strings_preserve_raw_active_and_native_chunk_progression() {
    for cap in [84_u16, 168] {
        let home = Home::new("csp-st");
        let listener = listener();
        let mut cfg = config(
            &listener,
            &json!([
                {"name":"array","address":"st3:0","elements":3,"encoding":"ascii"},
                {"name":"full","address":"ST8:7","data_type":"string"},
                {"name":"empty","address":"ST9:0","encoding":"ascii"},
                {"name":"invalid","address":"ST9:1","encoding":"ascii"},
                {"name":"last","address":"ST255:255","encoding":"utf8"},
                {"name":"high","address":"ST8:8"},
                {"name":"old","address":"N7:0"},
                {"name":"timer","address":"T4:0"}
            ]),
        );
        cfg["sources"][0]["max_read_bytes"] = cap.into();
        let (abc, a) = st_literal("odd_abc", Some("ascii"));
        let (nul, n) = st_literal("embedded_nul", Some("ascii"));
        let (one, o) = st_literal("odd_one", Some("ascii"));
        let (full, f) = st_literal("full_82", None);
        let (empty, e) = st_literal("empty_nonzero", Some("ascii"));
        let (invalid, i) = st_literal("invalid_83", Some("ascii"));
        let (last, l) = st_literal("invalid_65535", Some("utf8"));
        let (high, h) = st_literal("high_bytes", None);
        let expected = vec![
            json!([a, n, o]),
            f,
            e,
            i,
            l,
            h,
            json!(-1),
            super::pccc_tc::timer(),
        ];
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            let register = registration(&mut stream);
            send(&mut stream, &register);
            let mut tns = 1;
            if cap == 84 {
                for (element, raw) in [abc, nul, one].into_iter().enumerate() {
                    command(
                        &mut stream,
                        tns,
                        0,
                        &[84, 3, 0x8d, u8::try_from(element).unwrap(), 0],
                    );
                    send(&mut stream, &response(tns, 0, &raw));
                    tns += 1;
                }
            } else {
                command(&mut stream, tns, 0, &[168, 3, 0x8d, 0, 0]);
                send(&mut stream, &response(tns, 0, &[abc, nul].concat()));
                tns += 1;
                command(&mut stream, tns, 0, &[84, 3, 0x8d, 2, 0]);
                send(&mut stream, &response(tns, 0, &one));
                tns += 1;
            }
            for (fields, data) in [
                (vec![84, 8, 0x8d, 7, 0], full),
                (vec![84, 9, 0x8d, 0, 0], empty),
                (vec![84, 9, 0x8d, 1, 0], invalid),
                (vec![84, 0xff, 0xff, 0, 0x8d, 0xff, 0xff, 0, 0], last),
                (vec![84, 8, 0x8d, 8, 0], high),
                (vec![2, 7, 0x89, 0, 0], vec![0xff, 0xff]),
                (vec![6, 4, 0x86, 0, 0], vec![5, 0xc0, 100, 0, 17, 0]),
            ] {
                command(&mut stream, tns, 0, &fields);
                send(&mut stream, &response(tns, 0, &data));
                tns += 1;
            }
            closed(&mut stream);
        });
        record(&home, &cfg, "350ms").success();
        worker.join().unwrap();
        let samples = readings(&home);
        assert_eq!(
            samples
                .iter()
                .map(|row| row["value"].clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(samples.iter().all(|row| row["elapsed_ns"].is_u64()
            && row["elapsed_ns"] == samples[0]["elapsed_ns"]
            && row.get("source_time").is_none()));
        st_exports(&home, &samples, false);
    }
}

#[test]
fn csp_string_late_failures_keep_the_complete_prior_poll_only() {
    for mode in ["short", "ascii", "utf8"] {
        let home = Home::new("csp-st-prefix");
        let listener = listener();
        let mut cfg = config(
            &listener,
            &json!([
                {"name":"first","address":"ST9:0"},
                {"name":"second","address":"ST9:1"}
            ]),
        );
        cfg["sources"][0]["every_ms"] = 200.into();
        if mode != "short" {
            cfg["sources"][0]["signals"][1]["encoding"] = mode.into();
        }
        let (abc, a) = st_literal("odd_abc", None);
        let (good, g) = st_literal(
            if mode == "utf8" {
                "utf8_six"
            } else {
                "odd_abc"
            },
            if mode == "short" { None } else { Some(mode) },
        );
        let (bad, _) = st_literal("high_bytes", None);
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            let register = registration(&mut stream);
            send(&mut stream, &register);
            for poll in 0..2 {
                let tns = 1 + poll * 2;
                command(&mut stream, tns, 0, &[84, 9, 0x8d, 0, 0]);
                send(&mut stream, &response(tns, 0, &abc));
                command(&mut stream, tns + 1, 0, &[84, 9, 0x8d, 1, 0]);
                let data: &[u8] = if poll == 0 {
                    &good
                } else if mode == "short" {
                    &good[..83]
                } else {
                    &bad
                };
                send(&mut stream, &response(tns + 1, 0, data));
            }
            closed(&mut stream);
        });
        assert_eq!(
            record(&home, &cfg, "550ms").failure(1)["code"],
            "incomplete"
        );
        worker.join().unwrap();
        let samples = readings(&home);
        assert_eq!(
            samples
                .iter()
                .map(|row| row["value"].clone())
                .collect::<Vec<_>>(),
            [a, g]
        );
        st_exports(&home, &samples, true);
    }
}

#[test]
fn csp_strings_keep_the_original_chunk_deadline_and_reject_small_caps_before_connect() {
    let home = Home::new("csp-st-cutoff");
    let listener = listener();
    let mut cfg = config(
        &listener,
        &json!([{"name":"array","address":"ST9:0","elements":3}]),
    );
    cfg["sources"][0]["max_read_bytes"] = 84.into();
    cfg["sources"][0]["timeout_ms"] = 250.into();
    let (raw, _) = st_literal("odd_abc", None);
    let worker = thread::spawn(move || {
        let mut stream = accept(&listener);
        let register = registration(&mut stream);
        send(&mut stream, &register);
        command(&mut stream, 1, 0, &[84, 9, 0x8d, 0, 0]);
        thread::sleep(Duration::from_millis(160));
        send(&mut stream, &response(1, 0, &raw));
        command(&mut stream, 2, 0, &[84, 9, 0x8d, 1, 0]);
        thread::sleep(Duration::from_millis(160));
        let _ = stream.write_all(&response(2, 0, &raw));
        let seen = stream.read(&mut [0]);
        assert!(
            matches!(seen, Ok(0))
                || seen.is_err_and(|error| error.kind() == io::ErrorKind::ConnectionReset)
        );
    });
    assert_eq!(record(&home, &cfg, "700ms").failure(4)["code"], "timeout");
    worker.join().unwrap();
    assert_eq!(readings(&home), Vec::<Value>::new());

    let home = Home::new("csp-st-cap");
    let listener = super::listener();
    listener.set_nonblocking(true).unwrap();
    let mut cfg = config(&listener, &json!([{"name":"s","address":"ST9:0"}]));
    cfg["sources"][0]["max_read_bytes"] = 83.into();
    assert_eq!(record(&home, &cfg, "1ms").failure(2)["code"], "usage");
    assert!(!home.path("run.jsonl").exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}
