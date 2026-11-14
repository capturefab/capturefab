//! Independent W342 timer/counter literals, frozen before the implementation.
use super::*;
use sha2::{Digest as _, Sha256};

const LITERALS: &str = include_str!("../fixtures/fins/timer-literals.json");

fn literals() -> Value {
    serde_json::from_str(LITERALS).unwrap()
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2));
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn cases() -> Vec<Value> {
    literals()["cases"].as_array().unwrap().clone()
}

fn signals(cases: &[Value]) -> Value {
    Value::Array(cases.iter().map(|case| case["signal"].clone()).collect())
}

fn only_plc(mut config: Value) -> Value {
    config["sources"].as_array_mut().unwrap().truncate(1);
    config
}

fn assert_values_and_exports(home: &Home, cases: &[Value]) {
    let all = rows(home);
    let samples = &all[1..all.len() - 1];
    assert_eq!(samples.len(), cases.len());
    assert_eq!(all[0]["clock"], "host_monotonic_receive");
    let mut previous = 0;
    for (index, (sample, case)) in samples.iter().zip(cases).enumerate() {
        assert_eq!(sample["seq"], index + 1);
        assert_eq!(sample["signal"], case["signal"]["name"]);
        assert_eq!(sample["value"], case["expected"]);
        assert!(sample.get("source_time").is_none());
        let elapsed = sample["elapsed_ns"].as_u64().unwrap();
        assert!(elapsed >= previous);
        assert!(elapsed <= time::ms(500) * 1_000_000);
        assert!(sample["read_duration_ns"].as_u64().unwrap() <= elapsed);
        assert_eq!(sample["elapsed_ns"], samples[0]["elapsed_ns"]);
        assert!(sample["delivery_latency_ns"].is_u64());
        chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
        previous = elapsed;
    }
    assert_eq!(all.last().unwrap()["status"], "complete");
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Vec<Value> = fs::read_to_string(home.path("values.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let expected: Vec<Value> = samples
        .iter()
        .map(|sample| {
            let mut value = sample.clone();
            value.as_object_mut().unwrap().remove("kind");
            value
        })
        .collect();
    assert_eq!(exported, expected);
    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    let mut expected = String::from(
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n",
    );
    for sample in samples {
        let mut fields = Vec::new();
        for field in [
            "seq",
            "source",
            "signal",
            "time",
            "elapsed_ns",
            "read_duration_ns",
            "delivery_latency_ns",
            "source_time",
            "value",
        ] {
            let value = &sample[field];
            if field == "value" {
                fields.push(format!("\"{}\"", value.to_string().replace('"', "\"\"")));
            } else if let Some(text) = value.as_str() {
                fields.push(format!("\"{}\"", text.replace('"', "\"\"")));
            } else if value.is_null() {
                fields.push(String::from("\"\""));
            } else {
                fields.push(value.to_string());
            }
        }
        expected.push_str(&fields.join(","));
        expected.push('\n');
    }
    assert_eq!(csv, expected);
}

#[test]
fn primary_literals_are_immutable_and_transport_envelopes_are_independent() {
    assert_eq!(
        format!("{:x}", Sha256::digest(LITERALS.as_bytes())),
        "c34efec9bdc6580acc9294c850dc0ec7057e097d403fefd56ec67997a4c0e0cc"
    );
    for case in cases() {
        let request = unhex(case["udp_request_hex"].as_str().unwrap());
        let reply = unhex(case["udp_reply_hex"].as_str().unwrap());
        assert_eq!(&request[10..], unhex(case["command_hex"].as_str().unwrap()));
        assert_eq!(
            &reply[14..],
            unhex(case["response_data_hex"].as_str().unwrap())
        );
        assert_eq!(
            packet(2, 0, &request),
            unhex(case["tcp_request_hex"].as_str().unwrap())
        );
        assert_eq!(
            packet(2, 0, &reply),
            unhex(case["tcp_reply_hex"].as_str().unwrap())
        );
        for field in ["fa_request", "fa_reply"] {
            let frame = case[field].as_str().unwrap().as_bytes();
            assert_eq!(&frame[frame.len() - 2..], b"*\r");
            let body_end = frame.len() - 4;
            let xor = frame[..body_end].iter().fold(0u8, |sum, byte| sum ^ byte);
            assert_eq!(
                unhex(std::str::from_utf8(&frame[body_end..body_end + 2]).unwrap()),
                [xor]
            );
        }
    }
}

#[test]
fn tcp_and_udp_read_native_timer_counter_geometry_and_exact_exports() {
    let cases = cases();
    let home = Home::new("fins-timer-tcp");
    let listener = listener();
    let config = only_plc(config(&listener, &signals(&cases)));
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0, CLIENT);
            for case in &cases {
                let expected = unhex(case["tcp_request_hex"].as_str().unwrap());
                assert_eq!(receive(&mut stream), expected[8..]);
                send(&mut stream, &unhex(case["tcp_reply_hex"].as_str().unwrap())).unwrap();
            }
            let mut byte = [0; 1];
            assert_eq!(
                stream.read(&mut byte).unwrap(),
                0,
                "no further requests or writes"
            );
        });
        assert_eq!(record(&home, &config).success()["samples"], cases.len());
    });
    assert_values_and_exports(&home, &cases);

    let home = Home::new("fins-timer-udp");
    let socket = udp_socket();
    let config = udp_config(&socket, LOCAL, &signals(&cases));
    thread::scope(|scope| {
        scope.spawn(|| {
            for (index, case) in cases.iter().enumerate() {
                // UDP starts at SID0; TCP's first request starts at SID1.
                let mut request = unhex(case["udp_request_hex"].as_str().unwrap());
                request[9] = u8::try_from(index).unwrap();
                let peer = udp_request(&socket, &request);
                let mut reply = unhex(case["udp_reply_hex"].as_str().unwrap());
                reply[9] = request[9];
                assert_eq!(socket.send_to(&reply, peer).unwrap(), reply.len());
            }
        });
        assert_eq!(record(&home, &config).success()["samples"], cases.len());
    });
    socket.set_nonblocking(true).unwrap();
    assert_eq!(
        socket.recv_from(&mut [0; 32]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_values_and_exports(&home, &cases);
}

#[test]
fn timer_limits_transforms_and_last_logical_elements_remain_exact() {
    let home = Home::new("fins-timer-limits");
    let listener = listener();
    let signals = json!([
        {"name":"max_pv","area":"timer","address":0,"elements":999},
        {"name":"max_flags","area":"counter","address":0,"data_type":"bool","elements":1998},
        {"name":"manual_scale","area":"timer","address":0,"scale":0.5,"offset":1},
        {"name":"manual_bytes","area":"counter","address":4095,"byte_order":"little","word_order":"big"},
        {"name":"manual_signed","area":"counter","address":4095,"data_type":"i16"}
    ]);
    let config = only_plc(config(&listener, &signals));
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0, CLIENT);
            for (sid, area, address, count, data) in [
                (1, 0x89, 0, 999, [0x12, 0x34].repeat(999)),
                (2, 0x09, 0x8000, 1998, [0, 1].repeat(999)),
                (3, 0x89, 0, 1, vec![0x12, 0x34]),
                (4, 0x89, 0x8fff, 1, vec![0x34, 0x12]),
                (5, 0x89, 0x8fff, 1, vec![0x80, 1]),
            ] {
                let request = read_request(&mut stream, LOCAL, sid, area, address, 0, count);
                reply(&mut stream, &request, &data);
            }
        });
        assert_eq!(record(&home, &config).success()["samples"], 5);
    });
    let all = rows(&home);
    assert_eq!(all[1]["value"], json!(vec![0x1234u16; 999]));
    assert_eq!(all[2]["value"], json!([false, true].repeat(999)));
    assert_eq!(all[3]["value"], 2331.0);
    assert_eq!(all[4]["value"], 0x1234);
    assert_eq!(all[5]["value"], -32767);
}

#[test]
fn invalid_timer_selections_fail_before_output_or_any_connection() {
    for area in ["timer", "counter"] {
        for patch in [
            json!({"address":4096}),
            json!({"address":65535}),
            json!({"address":4095,"elements":2}),
            json!({"address":4094,"data_type":"bool","elements":3}),
            json!({"elements":0}),
            json!({"elements":1000}),
            json!({"data_type":"bool","elements":1999}),
            json!({"data_type":"bool","bit":0}),
            json!({"bit":1}),
            json!({"bank":0}),
            json!({"data_type":"u32"}),
            json!({"data_type":"i32"}),
            json!({"data_type":"u64"}),
            json!({"data_type":"i64"}),
            json!({"data_type":"f32"}),
            json!({"data_type":"f64"}),
            json!({"data_type":"bool","scale":2}),
        ] {
            let home = Home::new("fins-timer-config");
            let listener = listener();
            let mut signal = json!({"name":"value","area":area,"address":0});
            for (name, value) in patch.as_object().unwrap() {
                signal[name].clone_from(value);
            }
            let config = only_plc(config(&listener, &json!([signal])));
            assert_eq!(
                record(&home, &config).failure(2)["code"],
                "usage",
                "{patch}"
            );
            assert!(!home.path("run.jsonl").exists());
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }
}

#[test]
fn late_timer_errors_are_atomic_and_previous_polls_still_export() {
    for mode in ["bit", "short", "end_code", "deadline"] {
        let home = Home::new("fins-timer-prefix");
        let listener = listener();
        let mut config = only_plc(config(
            &listener,
            &json!([
                {"name":"pv","area":"timer","address":4095},
                {"name":"done","area":"counter","address":4095,"data_type":"bool"}
            ]),
        ));
        config["sources"][0]["every_ms"] = json!(time::ms(1));
        config["sources"][0]["timeout_ms"] = json!(time::ms(250));
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0, CLIENT);
            for sid in [1, 3] {
                let first = read_request(&mut stream, LOCAL, sid, 0x89, 0x0fff, 0, 1);
                if sid == 3 && mode == "deadline" {
                    thread::sleep(time::millis(150));
                }
                reply(&mut stream, &first, &[0x80, 1]);
                let second = read_request(&mut stream, LOCAL, sid + 1, 0x09, 0x8fff, 0, 1);
                if sid == 1 {
                    reply(&mut stream, &second, &[1]);
                    continue;
                }
                let response = match mode {
                    "bit" => response(&second, &[2]),
                    "short" => response(&second, &[]),
                    "end_code" => {
                        let mut bytes = response(&second, &[]);
                        bytes[28..30].copy_from_slice(&[0x11, 0x03]);
                        bytes
                    }
                    "deadline" => {
                        thread::sleep(time::millis(150));
                        response(&second, &[0])
                    }
                    _ => unreachable!(),
                };
                let _ = send(&mut stream, &response);
                let mut byte = [0; 1];
                match stream.read(&mut byte) {
                    Ok(0) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset
                                | io::ErrorKind::ConnectionAborted
                                | io::ErrorKind::BrokenPipe
                        ) => {}
                    other => panic!("unexpected later query: {other:?}"),
                }
            }
        });
        let error = record(&home, &config).failure(if mode == "deadline" { 4 } else { 1 });
        if mode == "end_code" {
            assert_eq!(error["details"]["fins_end_code"], 0x1103);
        }
        server.join().unwrap();
        let all = rows(&home);
        assert_eq!(all.len(), 4);
        assert_eq!(all[1]["value"], 0x8001);
        assert_eq!(all[2]["value"], true);
        assert_eq!(all[3]["samples"], 2);
        assert_eq!(all[3]["status"], "failed");
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--allow-incomplete",
            "-o",
            "prefix.jsonl",
            "--format",
            "jsonl",
        ]);
        let exported: Vec<Value> = fs::read_to_string(home.path("prefix.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(exported.len(), 2);
        assert_eq!(exported[0]["value"], 0x8001);
        assert_eq!(exported[1]["value"], true);
    }
}
