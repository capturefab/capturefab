//! Independent complete84-byte ST literals, over direct and routed CLI reads.
use super::{
    accept, cleanup, config, listener, read_request, readings, record, register, response, send,
    support::{cli::Home, time},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    thread,
};

fn cases() -> Vec<Value> {
    let data: Value =
        serde_json::from_str(include_str!("../fixtures/pccc/st-literals.json")).unwrap();
    data["vectors"].as_array().unwrap().clone()
}
fn unhex(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0);
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn raw(case: &Value) -> Vec<u8> {
    let text: String = case["native_hex_chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part.as_str().unwrap())
        .collect();
    let bytes = unhex(&text);
    assert_eq!(bytes.len(), 84);
    bytes
}
fn expected(case: &Value, encoding: Option<&str>) -> Value {
    let mut value = json!({"kind":"pccc_string","length_word_raw":case["length_word_raw"],"capacity":82,
        "length_status":case["length_status"],"raw":{"encoding":"base64","scope":"native_element","data":STANDARD.encode(raw(case))}});
    if case["length_status"] == "valid" {
        let active = if case["active_hex"] == "" {
            Vec::new()
        } else {
            unhex(case["active_hex"].as_str().unwrap())
        };
        value["active"] =
            json!({"encoding":"base64","scope":"active_data","data":STANDARD.encode(&active)});
        if let Some(encoding) = encoding {
            value["encoding"] = json!(encoding);
            let text = case[if encoding == "ascii" {
                "ascii_text"
            } else {
                "utf8_text"
            }]
            .as_str()
            .unwrap_or_else(|| std::str::from_utf8(&active).unwrap());
            value["text"] = json!(text);
        }
    }
    value
}
fn retired(stream: &mut TcpStream) {
    // Existing best-effort UnregisterSession is allowed; an A2/reconnect is not.
    let mut header = [0; 24];
    let mut seen = 0;
    while seen < header.len() {
        match stream.read(&mut header[seen..]) {
            Ok(0) => return,
            Ok(count) => {
                seen += count;
                assert_eq!(header[0], 0x66, "extra application query after failure");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                return;
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert_eq!(&header[..4], &[0x66, 0, 0, 0]);
}

#[test]
fn whole_strings_direct_and_routed_keep_raw_active_text_and_old_values() {
    for (slot, cap) in [(None, 220), (Some(2), 224)] {
        let home = Home::new("pccc-st-native");
        let server = listener();
        let mut cfg = config(
            &server,
            json!([
                {"name":"odd","address":"st3:10","encoding":"ascii"},
                {"name":"array","address":"ST8:20","elements":3},
                {"name":"bytes","address":"ST9:0"},
                {"name":"utf8","address":"ST9:1","encoding":"utf8"},
                {"name":"full","address":"ST9:2","data_type":"string","encoding":"ascii"},
                {"name":"odd81","address":"ST9:3","encoding":"ascii"},
                {"name":"invalid83","address":"ST9:4","encoding":"ascii"},
                {"name":"last","address":"ST255:255","encoding":"utf8"},
                {"name":"numeric","address":"N7:0"},
                {"name":"timer","address":"T4:0"}
            ]),
        );
        cfg["sources"][0]["max_read_bytes"] = cap.into();
        if let Some(slot) = slot {
            cfg["sources"][0]["slot"] = slot.into();
        }
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let c = cases();
            let array_first = [raw(&c[1]), raw(&c[2])].concat();
            for (fields, data) in [
                ((3, 0x8d, 10, 84), raw(&c[0])),
                ((8, 0x8d, 20, 168), array_first),
                ((8, 0x8d, 22, 84), raw(&c[5])),
                ((9, 0x8d, 0, 84), raw(&c[3])),
                ((9, 0x8d, 1, 84), raw(&c[4])),
                ((9, 0x8d, 2, 84), raw(&c[7])),
                ((9, 0x8d, 3, 84), raw(&c[6])),
                ((9, 0x8d, 4, 84), raw(&c[8])),
                ((255, 0x8d, 255, 84), raw(&c[9])),
                ((7, 0x89, 0, 2), vec![0xff, 0xff]),
                ((4, 0x86, 0, 6), vec![5, 0xc0, 100, 0, 17, 0]),
            ] {
                let packet = read_request(&mut stream, slot, fields);
                send(&mut stream, &response(&packet, slot, &data));
            }
            cleanup(&mut stream);
        });
        record(&home, &cfg, "300ms").success();
        worker.join().unwrap();
        let c = cases();
        let want = [
            expected(&c[0], Some("ascii")),
            json!([
                expected(&c[1], None),
                expected(&c[2], None),
                expected(&c[5], None)
            ]),
            expected(&c[3], None),
            expected(&c[4], Some("utf8")),
            expected(&c[7], Some("ascii")),
            expected(&c[6], Some("ascii")),
            expected(&c[8], Some("ascii")),
            expected(&c[9], Some("utf8")),
            json!(-1),
            super::pccc_tc::timer(),
        ];
        let rows = readings(&home);
        assert_eq!(rows.len(), want.len());
        for (row, value) in rows.iter().zip(want) {
            assert_eq!(row["value"], value);
        }
        assert!(rows.iter().all(|row| row["elapsed_ns"].is_u64()
            && row["elapsed_ns"] == rows[0]["elapsed_ns"]
            && row.get("source_time").is_none()));
        super::pccc_tc::exports(&home, &rows);
    }
}

#[test]
fn strict_string_text_failure_drops_whole_later_poll_and_never_queries_tail() {
    for encoding in ["ascii", "utf8"] {
        let home = Home::new("pccc-st-text-prefix");
        let server = listener();
        let mut cfg = config(
            &server,
            json!([
                {"name":"first","address":"ST3:0"},
                {"name":"array","address":"ST8:10","elements":3,"encoding":encoding}
            ]),
        );
        cfg["sources"][0]["max_read_bytes"] = 84.into();
        cfg["sources"][0]["every_ms"] = 200.into();
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let c = cases();
            for (fields, case) in [
                ((3, 0x8d, 0, 84), 0),
                ((8, 0x8d, 10, 84), 1),
                ((8, 0x8d, 11, 84), 5),
                ((8, 0x8d, 12, 84), 2),
                ((3, 0x8d, 0, 84), 8),
                ((8, 0x8d, 10, 84), 0),
                ((8, 0x8d, 11, 84), 3),
            ] {
                let packet = read_request(&mut stream, None, fields);
                send(&mut stream, &response(&packet, None, &raw(&c[case])));
            }
            retired(&mut stream);
            server.set_nonblocking(true).unwrap();
            assert_eq!(
                server.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        assert_eq!(
            record(&home, &cfg, "550ms").failure(1)["code"],
            "incomplete"
        );
        worker.join().unwrap();
        let c = cases();
        let rows = readings(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["value"], expected(&c[0], None));
        assert_eq!(
            rows[1]["value"],
            json!([
                expected(&c[1], Some(encoding)),
                expected(&c[5], Some(encoding)),
                expected(&c[2], Some(encoding))
            ])
        );
        let end: Value = serde_json::from_str(
            fs::read_to_string(home.path("run.jsonl"))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(end["status"], "failed");
    }
}

#[test]
fn malformed_whole_string_tail_drops_only_the_incomplete_poll() {
    for fault in ["short", "long", "transaction"] {
        let home = Home::new("pccc-st-wire-prefix");
        let server = listener();
        let mut cfg = config(
            &server,
            json!([{"name":"array","address":"ST9:10","elements":3}]),
        );
        cfg["sources"][0]["every_ms"] = 200.into();
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let c = cases();
            for poll in 0..2 {
                let packet = read_request(&mut stream, None, (9, 0x8d, 10, 168));
                send(
                    &mut stream,
                    &response(&packet, None, &[raw(&c[0]), raw(&c[1])].concat()),
                );
                let packet = read_request(&mut stream, None, (9, 0x8d, 12, 84));
                let mut data = raw(&c[2]);
                if poll == 1 {
                    if fault == "short" {
                        data.pop();
                    }
                    if fault == "long" {
                        data.push(0);
                    }
                }
                let mut wire = response(&packet, None, &data);
                if poll == 1 && fault == "transaction" {
                    wire[53] ^= 0x80;
                }
                send(&mut stream, &wire);
            }
            retired(&mut stream);
        });
        assert_eq!(
            record(&home, &cfg, "550ms").failure(1)["code"],
            "incomplete"
        );
        worker.join().unwrap();
        let rows = readings(&home);
        let c = cases();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["value"],
            json!([
                expected(&c[0], None),
                expected(&c[1], None),
                expected(&c[2], None)
            ])
        );
    }
}

#[test]
fn string_siblings_share_request_and_acquisition_deadlines_without_tail_query() {
    for acquisition in [false, true] {
        let home = Home::new("pccc-st-cutoff");
        let server = listener();
        let mut cfg = config(
            &server,
            json!([
                {"name":"first","address":"ST3:0"}, {"name":"second","address":"ST8:0"},
                {"name":"never","address":"ST9:0"}
            ]),
        );
        cfg["sources"][0]["timeout_ms"] = if acquisition {
            time::ms(1000).into()
        } else {
            time::ms(250).into()
        };
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let c = cases();
            let packet = read_request(&mut stream, None, (3, 0x8d, 0, 84));
            thread::sleep(time::millis(160));
            send(&mut stream, &response(&packet, None, &raw(&c[0])));
            let packet = read_request(&mut stream, None, (8, 0x8d, 0, 84));
            thread::sleep(time::millis(160));
            match stream.write_all(&response(&packet, None, &raw(&c[1]))) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(error) => panic!("{error}"),
            }
            retired(&mut stream);
        });
        assert_eq!(
            record(&home, &cfg, &time::arg(if acquisition { 250 } else { 700 })).failure(4)["code"],
            "timeout"
        );
        worker.join().unwrap();
        assert_eq!(readings(&home), [] as [Value; 0]);
    }
}

#[test]
fn whole_string_invalid_options_fail_before_connect_or_recording() {
    for change in [
        json!({"signals":[{"name":"s","address":"ST0:0"}]}),
        json!({"signals":[{"name":"s","address":"ST1:0"}]}),
        json!({"signals":[{"name":"s","address":"ST2:0"}]}),
        json!({"signals":[{"name":"s","address":"ST256:0"}]}),
        json!({"signals":[{"name":"s","address":"ST255:255","elements":2}]}),
        json!({"signals":[{"name":"s","address":"ST9:0.LEN"}]}),
        json!({"signals":[{"name":"s","address":"ST9:0","data_type":"i16"}]}),
        json!({"signals":[{"name":"s","address":"ST9:0","encoding":"latin1"}]}),
        json!({"signals":[{"name":"s","address":"N7:0","encoding":"ascii"}]}),
        json!({"signals":[{"name":"s","address":"ST9:0","scale":2}]}),
        json!({"profile":"plc5"}),
        json!({"max_read_bytes":83}),
    ] {
        let home = Home::new("pccc-st-config");
        let server = listener();
        let mut cfg = config(&server, json!([{"name":"s","address":"ST9:0"}]));
        for (key, value) in change.as_object().unwrap() {
            cfg["sources"][0][key].clone_from(value);
        }
        record(&home, &cfg, "100ms").failure(2);
        assert!(!home.path("run.jsonl").exists());
        server.set_nonblocking(true).unwrap();
        assert_eq!(
            server.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
