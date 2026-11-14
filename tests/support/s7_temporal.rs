//! Spec-derived Siemens temporal storage through the real CLI, without an SDK.
use super::*;

const DT: [u8; 8] = [0x08, 0x10, 0x25, 0x08, 0x12, 0x34, 0x56, 0x77];
const DTL: [u8; 12] = [0x07, 0xd8, 12, 16, 3, 20, 30, 20, 0x0e, 0xe6, 0xb2, 0x80];

fn serve_temporal(stream: &mut TcpStream, address: u32, bytes: &[u8], pdu: u16) {
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
fn temporal_values(home: &Home) -> Vec<Value> {
    rows(home)
        .into_iter()
        .filter(|row| row["source"] == "plc")
        .collect()
}
fn exact_raw(value: &Value, kind: &str, bytes: &[u8], status: &str) {
    assert_eq!(value["kind"], "s7_temporal");
    assert_eq!(value["data_type"], kind);
    assert_eq!(value["decode_status"], status);
    assert_eq!(
        value["raw"],
        json!({"encoding":"base64","bytes":bytes.len(),"data":STANDARD.encode(bytes)})
    );
}
fn temporal_exports(home: &Home, samples: &[Value]) {
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "--source",
        "plc",
        "--format",
        "jsonl",
        "-o",
        "time.jsonl",
    ]);
    let exported = fs::read_to_string(home.path("time.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), samples.len());
    for (row, sample) in exported.iter().zip(samples) {
        assert_eq!(row["signal"], sample["signal"]);
        assert_eq!(row["value"], sample["value"]);
    }
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "--source",
        "plc",
        "--format",
        "csv",
        "-o",
        "time.csv",
    ]);
    let csv = fs::read_to_string(home.path("time.csv")).unwrap();
    for sample in samples {
        let cell = format!("\"{}\"", sample["value"].to_string().replace('"', "\"\""));
        assert!(csv.contains(&cell), "{csv}");
    }
}

#[test]
fn temporal_values_keep_exact_units_raw_bytes_and_invalid_application_states() {
    let home = Home::new("s7-temporal-values");
    let listener = listener();
    let mut later = DTL;
    later[..2].copy_from_slice(&2263_u16.to_be_bytes());
    later[2] = 1;
    later[3] = 1;
    later[8..].copy_from_slice(&1_u32.to_be_bytes());
    let mut bad_date = DTL;
    bad_date[..2].copy_from_slice(&2100_u16.to_be_bytes());
    bad_date[2] = 2;
    bad_date[3] = 29;
    let mut mismatch = DTL;
    mismatch[4] = 1;
    let mut bad_bcd = DT;
    bad_bcd[0] = 0xa0;
    let cases = vec![
        ("dates", "date", 2, vec![0, 0, 0xff, 0xff]),
        (
            "duration",
            "time",
            2,
            [i32::MIN.to_be_bytes(), i32::MAX.to_be_bytes()].concat(),
        ),
        (
            "tod",
            "time_of_day",
            3,
            [
                0_u32.to_be_bytes(),
                86_399_999_u32.to_be_bytes(),
                86_400_000_u32.to_be_bytes(),
            ]
            .concat(),
        ),
        ("bcd", "dt", 2, [DT.as_slice(), bad_bcd.as_slice()].concat()),
        (
            "binary",
            "dtl",
            3,
            [DTL.as_slice(), later.as_slice(), bad_date.as_slice()].concat(),
        ),
        ("weekday", "dtl", 1, mismatch.to_vec()),
    ];
    let mut signals = cases
        .iter()
        .enumerate()
        .map(|(index, (name, kind, elements, _))| {
            json!({
                "name":name,"db":1,"address":256+index*64,"data_type":kind,"elements":elements
            })
        })
        .collect::<Vec<_>>();
    signals.extend([
        json!({"name":"scalar_date","db":1,"address":1024,"data_type":"date"}),
        json!({"name":"merker_duration","area":"merker","address":15,"data_type":"time"}),
        json!({"name":"input_tod","area":"input","address":15,"data_type":"tod"}),
        json!({"name":"output_date","area":"output","address":17,"data_type":"date"}),
    ]);
    let config = config(&listener, &json!(signals));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0x0100, 0x0102);
        for (index, (_, _, _, bytes)) in cases.into_iter().enumerate() {
            serve_temporal(
                &mut stream,
                256 + u32::try_from(index * 64).unwrap(),
                &bytes,
                240,
            );
        }
        for (area, db, address, bytes) in [
            (0x84, 1, 1024, vec![0, 0]),
            (0x83, 0, 15, (-500_i32).to_be_bytes().to_vec()),
            (0x81, 0, 15, 37_230_400_u32.to_be_bytes().to_vec()),
            (0x82, 0, 17, vec![0, 0]),
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
    let samples = temporal_values(&home);
    assert_eq!(samples.len(), 10);
    let named = |name: &str| &samples.iter().find(|row| row["signal"] == name).unwrap()["value"];
    let dates = named("dates");
    assert_eq!(dates[0]["date"], "1990-01-01");
    assert_eq!(dates[1]["date"], "2169-06-06");
    assert_eq!(dates[1]["days_since_1990"], 65535);
    exact_raw(&dates[0], "date", &[0, 0], "decoded");
    exact_raw(&dates[1], "date", &[0xff, 0xff], "decoded");
    for (index, ms) in [i32::MIN, i32::MAX].into_iter().enumerate() {
        let value = &named("duration")[index];
        exact_raw(value, "time", &ms.to_be_bytes(), "decoded");
        assert_eq!(value["milliseconds"], ms);
        assert_eq!(value["unit"], "ms");
    }
    assert_eq!(named("tod")[1]["time_of_day"], "23:59:59.999");
    exact_raw(
        &named("tod")[2],
        "tod",
        &86_400_000_u32.to_be_bytes(),
        "invalid_value",
    );
    assert!(named("tod")[2].get("time_of_day").is_none());
    exact_raw(&named("bcd")[0], "date_and_time", &DT, "decoded");
    assert_eq!(named("bcd")[0]["calendar"], "2008-10-25T08:12:34.567");
    exact_raw(&named("bcd")[1], "date_and_time", &bad_bcd, "invalid_value");
    assert!(named("bcd")[1].get("calendar").is_none());
    exact_raw(&named("binary")[0], "dtl", &DTL, "decoded");
    assert_eq!(named("binary")[0]["fields"]["nanosecond"], 250_000_000);
    assert_eq!(named("binary")[1]["fields"]["year"], 2263);
    assert_eq!(named("binary")[1]["fields"]["nanosecond"], 1);
    assert_eq!(
        named("binary")[1]["within_recommended_conversion_range"],
        false
    );
    assert_eq!(named("binary")[1]["decode_status"], "decoded");
    exact_raw(&named("binary")[2], "dtl", &bad_date, "invalid_value");
    assert_eq!(named("weekday")["weekday_matches_date"], false);
    assert_eq!(named("weekday")["decode_status"], "decoded");
    assert!(named("scalar_date").is_object());
    assert_eq!(named("merker_duration")["milliseconds"], -500);
    assert_eq!(named("input_tod")["time_of_day"], "10:20:30.400");
    assert_eq!(named("output_date")["date"], "1990-01-01");
    for sample in &samples {
        assert!(sample.get("source_time").is_none());
    }
    for name in ["bcd", "binary"] {
        for value in named(name).as_array().unwrap() {
            assert_eq!(value["clock_basis"], "unspecified");
            if let Some(text) = value["calendar"].as_str() {
                assert!(!text.ends_with('Z') && !text.contains('+'));
            }
        }
    }
    temporal_exports(&home, &samples);
}

#[test]
fn temporal_arrays_reassemble_fixed_layouts_under_each_negotiated_pdu() {
    for pdu in [240_u16, 241, 960] {
        let home = Home::new("s7-temporal-fragments");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"numeric","db":1,"address":0,"data_type":"u64"},
                {"name":"bcd","db":1,"address":100,"data_type":"date_and_time","elements":100},
                {"name":"binary","db":1,"address":2000,"data_type":"dtl","elements":100}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, pdu);
            let reference = read_request(&mut stream, 0x84, 1, 0, 8);
            reply(&mut stream, &reference, &u64::MAX.to_be_bytes(), false);
            let bcd = (0_u8..100)
                .flat_map(|ms| {
                    let mut bytes = DT;
                    bytes[6] = ms / 10;
                    bytes[7] = ((ms % 10) << 4) | 7;
                    bytes
                })
                .collect::<Vec<_>>();
            let binary = (0_u32..100)
                .flat_map(|ns| {
                    let mut bytes = DTL;
                    bytes[8..].copy_from_slice(&ns.to_be_bytes());
                    bytes
                })
                .collect::<Vec<_>>();
            serve_temporal(&mut stream, 100, &bcd, pdu);
            serve_temporal(&mut stream, 2000, &binary, pdu);
            expect_closed(&mut stream);
        });
        assert_eq!(record(&home, &config).success()["status"], "complete");
        server.join().unwrap();
        let values = temporal_values(&home);
        assert_eq!(values.len(), 3);
        assert_eq!(values[0]["value"], u64::MAX);
        for (kind, field) in [(1, "millisecond"), (2, "nanosecond")] {
            let array = values[kind]["value"].as_array().unwrap();
            assert_eq!(array.len(), 100);
            for (index, value) in array.iter().enumerate() {
                assert_eq!(value["fields"][field], index);
                assert_eq!(value["decode_status"], "decoded");
            }
        }
        temporal_exports(&home, &values);
    }
}

#[test]
fn temporal_wire_errors_discard_earlier_valid_calendar_siblings() {
    for mode in ["reference", "transport", "status", "extent"] {
        let home = Home::new("s7-temporal-wire");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"valid","db":1,"address":0,"data_type":"dt"},
                {"name":"bad","db":1,"address":64,"data_type":"dtl"},
                {"name":"later","db":1,"address":100,"data_type":"time"}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0x0100, 0x0102);
            let reference = read_request(&mut stream, 0x84, 1, 0, 8);
            reply(&mut stream, &reference, &DT, true);
            let mut reference = read_request(&mut stream, 0x84, 1, 64, 12);
            let mut data = vec![0xff, 4, 0, 96];
            data.extend_from_slice(&DTL);
            assert_eq!(reference.len(), 2);
            assert!(data.len() >= 4);
            match mode {
                "reference" => reference[1] ^= 1,
                "transport" => data[1] = 3,
                "status" => data[0] = 5,
                "extent" => {
                    data.pop();
                }
                _ => unreachable!(),
            }
            send_pdu(&mut stream, &response(&reference, &[4, 1], &data), false);
            expect_closed(&mut stream);
        });
        let error = record(&home, &config).failure(1);
        server.join().unwrap();
        assert_eq!(error["code"], "failed");
        let diagnostic = match mode {
            "reference" => "PDU reference",
            "transport" => "transport or data length",
            "status" => "rejected a Read Var item",
            "extent" => "sample width differs",
            _ => unreachable!(),
        };
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{error}"
        );
        if mode == "status" {
            assert_eq!(error["details"]["return_code"], 5);
        }
        assert_eq!(temporal_values(&home), [] as [Value; 0]);
        assert_eq!(rows(&home).last().unwrap()["status"], "failed");
    }
}

#[test]
fn temporal_object_budget_stops_before_the_next_value_or_array_query() {
    for minimum_only in [false, true] {
        let home = Home::new("s7-temporal-budget");
        let listener = listener();
        let signal = if minimum_only {
            json!({"name":"large","db":1,"address":0,"data_type":"date","elements":65535})
        } else {
            json!({"name":"large","db":1,"address":0,"data_type":"dtl","elements":4000})
        };
        let mut config = config(
            &listener,
            &json!([signal,{"name":"later","db":1,"address":100_000}]),
        );
        config["sources"][0]["timeout_ms"] = json!(5000);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, 960);
            if !minimum_only {
                serve_temporal(&mut stream, 0, &DTL.repeat(4000), 960);
            }
            expect_closed(&mut stream);
        });
        let error = record_for(&home, &config, "6s").failure(1);
        server.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains("1 MiB"),
            "{error}"
        );
        assert_eq!(temporal_values(&home), [] as [Value; 0]);
    }
}

#[test]
fn temporal_continuations_share_both_request_and_acquisition_cutoffs() {
    for acquisition in [false, true] {
        let home = Home::new("s7-temporal-cutoff");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([
                {"name":"calendar","db":1,"address":0,"data_type":"dtl","elements":100},
                {"name":"later","db":1,"address":2000}
            ]),
        );
        config["sources"][0]["timeout_ms"] = json!(time::ms(if acquisition { 1000 } else { 250 }));
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, 241);
            let reference = read_request(&mut stream, 0x84, 1, 0, 223);
            thread::sleep(time::millis(160));
            reply_fast(&mut stream, &reference, &DTL.repeat(100)[..223]);
            drop(read_request(&mut stream, 0x84, 1, 223, 223));
            thread::sleep(time::millis(160));
            expect_closed(&mut stream);
        });
        let duration = &time::arg(if acquisition { 250 } else { 500 });
        assert_eq!(
            record_for(&home, &config, duration).failure(4)["code"],
            "timeout"
        );
        server.join().unwrap();
        assert_eq!(temporal_values(&home), [] as [Value; 0]);
    }
}

#[test]
fn temporal_scaling_text_options_and_indexed_areas_remain_explicit_usage_errors() {
    let mut cases = Vec::new();
    for kind in ["date", "time", "tod", "date_and_time", "dtl"] {
        cases.push(json!({"data_type":kind,"scale":2}));
        cases.push(json!({"data_type":kind,"offset":1}));
        cases.push(json!({"data_type":kind,"bit":1}));
        cases.push(json!({"data_type":kind,"area":"timer","db":0}));
    }
    cases.extend([
        json!({"data_type":"dtl","elements":10923}),
        json!({"data_type":"dtl","capacity":12}),
        json!({"data_type":"date","encoding":"ascii"}),
        json!({"data_type":"dt","address":2_097_145}),
    ]);
    for patch in cases {
        let home = Home::new("s7-temporal-config");
        let listener = listener();
        let mut signal = json!({"name":"value","db":1,"address":0});
        for (key, value) in patch.as_object().unwrap() {
            signal[key].clone_from(value);
        }
        let config = config(&listener, &json!([signal]));
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
