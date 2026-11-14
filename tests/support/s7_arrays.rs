//! Spec-derived standard absolute-layout array peers, not physical PLC captures.
use super::*;

fn serve_array(stream: &mut TcpStream, address: u32, bytes: &[u8], pdu: u16) {
    let maximum = usize::from(pdu) - 18;
    for (index, chunk) in bytes.chunks(maximum).enumerate() {
        let reference = read_request(
            stream,
            0x84,
            1,
            address + u32::try_from(index * maximum).unwrap(),
            u16::try_from(chunk.len()).unwrap(),
        );
        reply_fast(stream, &reference, chunk);
    }
}

fn assert_arrays(home: &Home, expected: &[(String, Value)]) {
    let rows = rows(home);
    let samples = rows
        .iter()
        .filter(|row| row["source"] == "plc")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), expected.len());
    for (row, (name, value)) in samples.iter().zip(expected) {
        assert_eq!(row["signal"], *name);
        assert_eq!(row["value"], *value);
        assert!(row.get("source_time").is_none());
    }
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "arrays.jsonl",
        "--format",
        "jsonl",
        "--source",
        "plc",
    ]);
    let exported = fs::read_to_string(home.path("arrays.jsonl")).unwrap();
    let values = exported
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values.len(), expected.len());
    for (row, (name, value)) in values.iter().zip(expected) {
        assert_eq!(row["signal"], *name);
        assert_eq!(row["value"], *value);
    }
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "arrays.csv",
        "--source",
        "plc",
    ]);
    let csv = fs::read_to_string(home.path("arrays.csv")).unwrap();
    for (_, value) in expected {
        let encoded = value.to_string();
        let cell = if encoded.contains([',', '"', '\r', '\n']) {
            format!("\"{}\"", encoded.replace('"', "\"\""))
        } else {
            encoded
        };
        assert!(csv.contains(&cell), "{csv}");
    }
}

#[test]
fn arrays_preserve_all_numeric_codecs_exact_exports_and_scalar_one() {
    let cases = vec![
        ("u8", vec![0, 127, 255], json!([0, 127, 255])),
        ("i8", vec![128, 0, 127], json!([-128, 0, 127])),
        (
            "u16",
            [0_u16, 1, u16::MAX]
                .into_iter()
                .flat_map(u16::to_be_bytes)
                .collect(),
            json!([0, 1, 65535]),
        ),
        (
            "i16",
            [i16::MIN, -7, i16::MAX]
                .into_iter()
                .flat_map(i16::to_be_bytes)
                .collect(),
            json!([-32768, -7, 32767]),
        ),
        (
            "u32",
            [0_u32, 1, u32::MAX]
                .into_iter()
                .flat_map(u32::to_be_bytes)
                .collect(),
            json!([0, 1, u32::MAX]),
        ),
        (
            "i32",
            [i32::MIN, -7, i32::MAX]
                .into_iter()
                .flat_map(i32::to_be_bytes)
                .collect(),
            json!([i32::MIN, -7, i32::MAX]),
        ),
        (
            "u64",
            [0_u64, 9_007_199_254_740_993, u64::MAX]
                .into_iter()
                .flat_map(u64::to_be_bytes)
                .collect(),
            json!([0, 9_007_199_254_740_993_u64, u64::MAX]),
        ),
        (
            "i64",
            [i64::MIN, -9_007_199_254_740_993, i64::MAX]
                .into_iter()
                .flat_map(i64::to_be_bytes)
                .collect(),
            json!([i64::MIN, -9_007_199_254_740_993_i64, i64::MAX]),
        ),
        (
            "f32",
            [1.5_f32, -2.25, 0.0]
                .into_iter()
                .flat_map(f32::to_be_bytes)
                .collect(),
            json!([1.5, -2.25, 0.0]),
        ),
        (
            "f64",
            [1.5_f64, -2.25, 0.0]
                .into_iter()
                .flat_map(f64::to_be_bytes)
                .collect(),
            json!([1.5, -2.25, 0.0]),
        ),
        (
            "s5time",
            [0x0000_u16, 0x1123, 0x3999]
                .into_iter()
                .flat_map(u16::to_be_bytes)
                .collect(),
            json!([0, 12300, 9_990_000]),
        ),
    ];
    let home = Home::new("s7-arrays-codecs");
    let listener = listener();
    let mut signals = cases.iter().enumerate().map(|(index, (kind, _, _))| {
        json!({"name":kind,"db":1,"address":index * 32,"data_type":kind,"elements":3.0})
    }).collect::<Vec<_>>();
    signals.extend([
        json!({"name":"scaled","db":1,"address":400,"data_type":"i16","elements":3,"scale":0.5,"offset":-1}),
        json!({"name":"implicit_one","db":1,"address":410,"data_type":"u64"}),
        json!({"name":"explicit_one","db":1,"address":420,"data_type":"u64","elements":1e0}),
    ]);
    let config = config(&listener, &json!(signals));
    let mut expected = cases
        .iter()
        .map(|(name, _, value)| ((*name).to_owned(), value.clone()))
        .collect::<Vec<_>>();
    expected.extend([
        ("scaled".into(), json!([-2.0, -1.0, 1.0])),
        ("implicit_one".into(), json!(u64::MAX)),
        ("explicit_one".into(), json!(u64::MAX)),
    ]);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0x0100, 0x0102);
        for (index, (_, bytes, _)) in cases.into_iter().enumerate() {
            let reference = read_request(
                &mut stream,
                0x84,
                1,
                u32::try_from(index * 32).unwrap(),
                u16::try_from(bytes.len()).unwrap(),
            );
            reply(&mut stream, &reference, &bytes, true);
        }
        for (address, bytes) in [
            (
                400,
                [-2_i16, 0, 4]
                    .into_iter()
                    .flat_map(i16::to_be_bytes)
                    .collect::<Vec<_>>(),
            ),
            (410, u64::MAX.to_be_bytes().to_vec()),
            (420, u64::MAX.to_be_bytes().to_vec()),
        ] {
            let reference = read_request(
                &mut stream,
                0x84,
                1,
                address,
                u16::try_from(bytes.len()).unwrap(),
            );
            reply(&mut stream, &reference, &bytes, false);
        }
        expect_closed(&mut stream);
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    assert_arrays(&home, &expected);
}

#[test]
fn packed_bool_arrays_advance_from_start_bit_and_ignore_adjacent_bits() {
    let home = Home::new("s7-arrays-bool");
    let listener = listener();
    let config = config(
        &listener,
        &json!([
            {"name":"bits","db":1,"address":31,"data_type":"bool","bit":6,"elements":13},
            {"name":"edge","area":"merker","address":2_097_151,"data_type":"bool","bit":6,"elements":2},
            {"name":"scalar","area":"input","address":7,"data_type":"bool","bit":7,"elements":1}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0x0100, 0x0102);
        for (area, db, address, bytes) in [
            (0x84, 1, 31, vec![0xb5, 0x5a, 0xe7]),
            (0x83, 0, 2_097_151, vec![0x7f]),
            (0x81, 0, 7, vec![0x80]),
        ] {
            let reference = read_request(
                &mut stream,
                area,
                db,
                address,
                u16::try_from(bytes.len()).unwrap(),
            );
            reply(&mut stream, &reference, &bytes, true);
        }
        expect_closed(&mut stream);
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    assert_arrays(
        &home,
        &[
            (
                "bits".into(),
                json!([
                    false, true, false, true, false, true, true, false, true, false, true, true,
                    true
                ]),
            ),
            ("edge".into(), json!([true, false])),
            ("scalar".into(), json!(true)),
        ],
    );
}

#[test]
fn segmented_arrays_reassemble_byte_ranges_and_whole_indexed_words() {
    for pdu in [240_u16, 241, 960] {
        let home = Home::new("s7-arrays-pdu");
        let listener = listener();
        let count = 130_u16;
        let expected = (0..count)
            .map(|index| u64::MAX - u64::from(index))
            .collect::<Vec<_>>();
        let numeric = expected
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect::<Vec<_>>();
        let words = (0..500_u16).collect::<Vec<_>>();
        let config = config(
            &listener,
            &json!([
                {"name":"values","db":1,"address":1000,"data_type":"u64","elements":count},
                {"name":"timers","area":"timer","address":7,"elements":500},
                {"name":"counters","area":"counter","address":0x00ff_fffe,"elements":2}
            ]),
        );
        let sent_words = words.clone();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, pdu);
            serve_array(&mut stream, 1000, &numeric, pdu);
            let maximum = (usize::from(pdu) - 18) / 2;
            for (index, chunk) in sent_words.chunks(maximum).enumerate() {
                let mut parameters = vec![4, 1, 0x12, 10, 0x10, 0x1d];
                parameters.extend_from_slice(&u16::try_from(chunk.len()).unwrap().to_be_bytes());
                parameters.extend_from_slice(&[0, 0, 0x1d]);
                let address = 7 + u32::try_from(index * maximum).unwrap();
                parameters.extend_from_slice(&address.to_be_bytes()[1..]);
                let reference = expect_read_var(&mut stream, &parameters);
                let payload = chunk
                    .iter()
                    .flat_map(|value| value.to_be_bytes())
                    .collect::<Vec<_>>();
                let mut data = vec![0xff, 9];
                data.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
                data.extend_from_slice(&payload);
                send_pdu(&mut stream, &response(&reference, &[4, 1], &data), true);
            }
            let reference = expect_read_var(
                &mut stream,
                &[
                    4, 1, 0x12, 10, 0x10, 0x1c, 0, 2, 0, 0, 0x1c, 0xff, 0xff, 0xfe,
                ],
            );
            reply(&mut stream, &reference, &[0, 1, 0xff, 0xff], false);
            expect_closed(&mut stream);
        });
        assert_eq!(record(&home, &config).success()["status"], "complete");
        server.join().unwrap();
        assert_arrays(
            &home,
            &[
                ("values".into(), json!(expected)),
                ("timers".into(), json!(words)),
                ("counters".into(), json!([1, 65535])),
            ],
        );
    }
}

#[test]
fn malformed_array_elements_and_middle_fragments_discard_prior_siblings() {
    for mode in [
        "return_code",
        "transport",
        "length",
        "reference",
        "nonfinite",
        "bcd",
    ] {
        let home = Home::new("s7-arrays-malformed");
        let listener = listener();
        let kind = if mode == "nonfinite" {
            "f32"
        } else if mode == "bcd" {
            "s5time"
        } else {
            "u16"
        };
        let config = config(
            &listener,
            &json!([
                {"name":"valid","db":1,"address":0},
                {"name":"bad","db":1,"address":100,"elements":120,"data_type":kind},
                {"name":"later","db":1,"address":800}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0x0100, 0x0102);
            let reference = read_request(&mut stream, 0x84, 1, 0, 2);
            reply(&mut stream, &reference, &[0, 7], false);
            let width: u16 = if mode == "nonfinite" { 480 } else { 240 };
            let reference = read_request(&mut stream, 0x84, 1, 100, 222);
            let mut first = [0; 222];
            if mode == "nonfinite" {
                first[220..].copy_from_slice(&f32::NAN.to_be_bytes()[..2]);
            }
            reply_fast(&mut stream, &reference, &first);
            let rest = (width - 222).min(222);
            let mut reference = read_request(&mut stream, 0x84, 1, 322, rest);
            let mut payload = vec![0; usize::from(rest)];
            if mode == "nonfinite" {
                // One NaN element crosses the bytewise PDU boundary at
                // relative offsets220..224; no element boundary is assumed.
                assert!(payload.len() >= 2);
                payload[..2].copy_from_slice(&f32::NAN.to_be_bytes()[2..]);
            } else if mode == "bcd" {
                assert!(payload.len() >= 2);
                payload[..2].copy_from_slice(&[0x1a, 0x23]);
            }
            let mut data = vec![0xff, 4];
            data.extend_from_slice(&(rest * 8).to_be_bytes());
            data.extend_from_slice(&payload);
            assert!(data.len() >= 4);
            assert_eq!(reference.len(), 2);
            match mode {
                "return_code" => data[0] = 0x05,
                "transport" => data[1] = 3,
                "length" => data[3] ^= 8,
                "reference" => reference[1] ^= 1,
                _ => (),
            }
            send_pdu(&mut stream, &response(&reference, &[4, 1], &data), false);
            if mode == "nonfinite" {
                let reference = read_request(&mut stream, 0x84, 1, 544, 36);
                reply_fast(&mut stream, &reference, &[0; 36]);
            }
            expect_closed(&mut stream);
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed");
        let message = error["message"].as_str().unwrap();
        let cause = match mode {
            "return_code" => "rejected a Read Var item",
            "transport" | "length" => "transport or data length",
            "reference" => "PDU reference",
            "nonfinite" => "non-finite floating-point",
            "bcd" => "invalid S5TIME BCD",
            _ => unreachable!(),
        };
        assert!(message.contains(cause), "{error}");
        server.join().unwrap();
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["source"] == "plc"));
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn array_continuations_share_the_original_poll_deadline() {
    for acquisition_cutoff in [false, true] {
        let home = Home::new("s7-arrays-deadline");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([
                {"name":"valid","db":1,"address":0},
                {"name":"array","db":1,"address":100,"data_type":"u64","elements":100},
                {"name":"later","db":1,"address":1000}
            ]),
        );
        config["sources"][0]["timeout_ms"] =
            json!(time::ms(if acquisition_cutoff { 1000 } else { 250 }));
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0x0100, 0x0102);
            let reference = read_request(&mut stream, 0x84, 1, 0, 2);
            reply(&mut stream, &reference, &[0, 7], false);
            let reference = read_request(&mut stream, 0x84, 1, 100, 222);
            thread::sleep(time::millis(160));
            reply_fast(&mut stream, &reference, &[0; 222]);
            drop(read_request(&mut stream, 0x84, 1, 322, 222));
            thread::sleep(time::millis(160));
            expect_closed(&mut stream);
        });
        let start = Instant::now();
        let duration = &time::arg(if acquisition_cutoff { 250 } else { 500 });
        assert_eq!(
            record_for(&home, &config, duration).failure(4)["code"],
            "timeout"
        );
        assert!(start.elapsed() < time::secs(2));
        server.join().unwrap();
        assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
    }
}

#[test]
fn array_json_budget_stops_without_querying_a_later_signal() {
    for final_elements in [50_000_u16, 65_535] {
        let home = Home::new("s7-arrays-budget");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([
                {"name":"first","db":1,"address":0,"data_type":"bool","elements":65535},
                {"name":"second","db":1,"address":0,"data_type":"bool","elements":65535},
                {"name":"third","db":1,"address":0,"data_type":"bool","elements":final_elements},
                {"name":"later","db":1,"address":10000}
            ]),
        );
        config["sources"][0]["timeout_ms"] = json!(3000);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, 960);
            for _ in 0..2 {
                serve_array(&mut stream, 0, &[0; 8192], 960);
            }
            if final_elements == 50_000 {
                serve_array(&mut stream, 0, &[0; 6250], 960);
            }
            // 50000 reaches the exact per-element charge during decode;
            // 65535 fails the lower-bound budget before its first query.
            expect_closed(&mut stream);
        });
        let error = record_for(&home, &config, "4s").failure(1);
        assert!(
            error["message"].as_str().unwrap().contains("1 MiB"),
            "{error}"
        );
        server.join().unwrap();
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["source"] == "plc"));
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn invalid_array_configurations_fail_before_connecting_or_output() {
    for patch in [
        json!({"elements":0}),
        json!({"elements":1.5}),
        json!({"elements":"2"}),
        json!({"elements":65536}),
        json!({"elements":-1}),
        json!({"data_type":"u64","elements":16385}),
        json!({"data_type":"bool","bit":7,"elements":2,"address":2_097_151}),
        json!({"data_type":"u16","elements":2,"address":2_097_150}),
        json!({"area":"timer","db":0,"elements":2,"address":0x00ff_ffff}),
        json!({"area":"counter","db":0,"elements":3,"address":0x00ff_fffe}),
        json!({"data_type":"string","capacity":2,"elements":2}),
        json!({"data_type":"wstring","capacity":2,"elements":2}),
        json!({"data_type":"bool","elements":2,"scale":2}),
        json!({"data_type":"u64","elements":2,"offset":1}),
    ] {
        let home = Home::new("s7-arrays-config");
        let listener = listener();
        let mut signal = json!({"name":"values","db":1,"address":0});
        for (key, value) in patch.as_object().unwrap() {
            signal[key].clone_from(value);
        }
        let config = config(&listener, &json!([signal]));
        assert_eq!(
            record(&home, &config).failure(2)["code"],
            "usage",
            "{patch}"
        );
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
