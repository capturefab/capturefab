//! Explicit long-time layouts from Siemens tables, not device packet captures.
use super::*;

fn serve_long(stream: &mut TcpStream, address: u32, bytes: &[u8], pdu: u16) {
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
fn long_samples(home: &Home) -> Vec<Value> {
    rows(home)
        .into_iter()
        .filter(|row| row["source"] == "plc")
        .collect()
}
fn raw_long(value: &Value, kind: &str, bytes: [u8; 8], status: &str) {
    assert_eq!(value["kind"], "s7_temporal");
    assert_eq!(value["data_type"], kind);
    assert_eq!(value["decode_status"], status);
    assert_eq!(
        value["raw"],
        json!({"encoding":"base64","bytes":8,"data":STANDARD.encode(bytes)})
    );
    assert_eq!(value["unit"], "ns");
}
fn long_exports(home: &Home, samples: &[Value]) {
    for format in ["jsonl", "csv"] {
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--source",
            "plc",
            "--format",
            format,
            "-o",
            &format!("long.{format}"),
        ]);
    }
    let exported = fs::read_to_string(home.path("long.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), samples.len());
    for (row, sample) in exported.iter().zip(samples) {
        assert_eq!(row["signal"], sample["signal"]);
        assert_eq!(row["value"], sample["value"]);
    }
    let csv = fs::read_to_string(home.path("long.csv")).unwrap();
    for sample in samples {
        let value = sample["value"].to_string();
        let cell = if value.contains([',', '"', '\r', '\n']) {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value
        };
        assert!(csv.contains(&cell), "{csv}");
    }
}

#[test]
fn long_values_keep_signed_unsigned_raw_limits_aliases_and_legacy_scalars() {
    let home = Home::new("s7-long-values");
    let listener = listener();
    let duration = [i64::MIN, -1, 0, 1, i64::MAX];
    let clock = [0_u64, 1, 86_399_999_999_999, 86_400_000_000_000, u64::MAX];
    let civil = [
        0_u64,
        1,
        951_782_400_123_456_789,
        9_223_372_036_854_775_807,
        1_u64 << 63,
    ];
    let queries = vec![
        (
            "duration",
            "ltime",
            0_u32,
            5,
            duration
                .into_iter()
                .flat_map(i64::to_be_bytes)
                .collect::<Vec<_>>(),
        ),
        (
            "clock",
            "ltime_of_day",
            128,
            5,
            clock.into_iter().flat_map(u64::to_be_bytes).collect(),
        ),
        (
            "civil",
            "date_and_ltime",
            256,
            5,
            civil.into_iter().flat_map(u64::to_be_bytes).collect(),
        ),
        (
            "duration_scalar",
            "ltime",
            512,
            1,
            (-1_i64).to_be_bytes().to_vec(),
        ),
        ("clock_scalar", "ltod", 520, 1, 1_u64.to_be_bytes().to_vec()),
        ("civil_scalar", "ldt", 528, 1, 1_u64.to_be_bytes().to_vec()),
        ("numeric", "u64", 536, 1, u64::MAX.to_be_bytes().to_vec()),
        ("legacy", "s5time", 544, 1, vec![0x21, 0x23]),
        (
            "short_time",
            "time",
            552,
            1,
            (-500_i32).to_be_bytes().to_vec(),
        ),
    ];
    let signals = queries
        .iter()
        .map(|(name, kind, address, count, _)| {
            let mut signal = json!({"name":name,"db":1,"address":address,"data_type":kind});
            if *count != 1 {
                signal["elements"] = serde_json::from_str("5e0").unwrap();
            }
            signal
        })
        .collect::<Vec<_>>();
    let config = config(&listener, &json!(signals));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0x0100, 0x0102);
        for (_, _, address, _, bytes) in queries {
            serve_long(&mut stream, address, &bytes, 240);
        }
        expect_closed(&mut stream);
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    let samples = long_samples(&home);
    assert_eq!(samples.len(), 9);
    let named = |name: &str| &samples.iter().find(|row| row["signal"] == name).unwrap()["value"];
    for (index, ns) in duration.into_iter().enumerate() {
        let value = &named("duration")[index];
        raw_long(value, "ltime", ns.to_be_bytes(), "decoded");
        assert_eq!(value["nanoseconds"].as_i64().unwrap(), ns);
        assert_eq!(value["semantic"], "duration");
        assert!(value.get("calendar").is_none());
    }
    for (index, ns) in clock.into_iter().enumerate() {
        let value = &named("clock")[index];
        let status = if ns < 86_400_000_000_000 {
            "decoded"
        } else {
            "invalid_value"
        };
        raw_long(value, "ltod", ns.to_be_bytes(), status);
        assert_eq!(value["nanoseconds"].as_u64().unwrap(), ns);
        assert_eq!(value["clock_basis"], "unspecified");
    }
    assert_eq!(named("clock")[0]["time_of_day"], "00:00:00.000000000");
    assert_eq!(named("clock")[1]["time_of_day"], "00:00:00.000000001");
    assert_eq!(named("clock")[2]["time_of_day"], "23:59:59.999999999");
    for index in [3, 4] {
        assert!(named("clock")[index].get("time_of_day").is_none());
        assert_eq!(
            named("clock")[index]["invalid_fields"],
            json!(["nanoseconds"])
        );
    }
    for (index, ns) in civil.into_iter().enumerate() {
        let value = &named("civil")[index];
        let status = if ns < (1_u64 << 63) {
            "decoded"
        } else {
            "invalid_value"
        };
        raw_long(value, "ldt", ns.to_be_bytes(), status);
        assert_eq!(value["nanoseconds_since_1970"].as_u64().unwrap(), ns);
        assert_eq!(value["clock_basis"], "unspecified");
    }
    for (index, text) in [
        "1970-01-01T00:00:00.000000000",
        "1970-01-01T00:00:00.000000001",
        "2000-02-29T00:00:00.123456789",
        "2262-04-11T23:47:16.854775807",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(named("civil")[index]["calendar"], text);
    }
    assert!(named("civil")[4].get("calendar").is_none());
    assert_eq!(
        named("civil")[4]["invalid_fields"],
        json!(["nanoseconds_since_1970"])
    );
    assert!(named("duration_scalar").is_object());
    assert_eq!(
        named("duration_scalar")["nanoseconds"].as_i64().unwrap(),
        -1
    );
    assert_eq!(named("clock_scalar")["time_of_day"], "00:00:00.000000001");
    assert_eq!(
        named("civil_scalar")["calendar"],
        "1970-01-01T00:00:00.000000001"
    );
    assert_eq!(named("numeric").as_u64().unwrap(), u64::MAX);
    assert_eq!(named("legacy").as_u64().unwrap(), 123_000);
    assert_eq!(named("short_time")["milliseconds"], -500);
    assert_eq!(named("short_time")["unit"], "ms");
    for sample in &samples {
        assert!(sample.get("source_time").is_none());
    }
    long_exports(&home, &samples);
}

#[test]
fn long_arrays_reassemble_exact_element_boundaries_at_standard_and_odd_pdus() {
    for pdu in [240_u16, 241, 960] {
        let home = Home::new("s7-long-fragments");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"duration","db":1,"address":0,"data_type":"ltime","elements":150},
                {"name":"clock","db":1,"address":1000,"data_type":"ltod","elements":150},
                {"name":"civil","db":1,"address":2000,"data_type":"ldt","elements":150}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, pdu);
            let durations = (-75_i64..75).flat_map(i64::to_be_bytes).collect::<Vec<_>>();
            let clocks = (0_u64..150).flat_map(u64::to_be_bytes).collect::<Vec<_>>();
            let civil = (951_782_400_123_456_700_u64..951_782_400_123_456_850)
                .flat_map(u64::to_be_bytes)
                .collect::<Vec<_>>();
            for (address, bytes) in [(0, durations), (1000, clocks), (2000, civil)] {
                serve_long(&mut stream, address, &bytes, pdu);
            }
            expect_closed(&mut stream);
        });
        assert_eq!(record(&home, &config).success()["status"], "complete");
        server.join().unwrap();
        let samples = long_samples(&home);
        let [duration, clock, civil] = samples.as_slice() else {
            panic!("expected three arrays")
        };
        for (row, kind) in [(duration, "ltime"), (clock, "ltod"), (civil, "ldt")] {
            let array = row["value"].as_array().unwrap();
            assert_eq!(array.len(), 150);
            for (index, value) in array.iter().enumerate() {
                let unsigned = u64::try_from(index).unwrap();
                let (field, ns) = if kind == "ldt" {
                    ("nanoseconds_since_1970", 951_782_400_123_456_700 + unsigned)
                } else {
                    ("nanoseconds", unsigned)
                };
                if kind == "ltime" {
                    let signed = i64::try_from(index).unwrap() - 75;
                    assert_eq!(value[field].as_i64().unwrap(), signed);
                    raw_long(value, kind, signed.to_be_bytes(), "decoded");
                } else {
                    assert_eq!(value[field].as_u64().unwrap(), ns);
                    raw_long(value, kind, ns.to_be_bytes(), "decoded");
                }
            }
        }
        assert_eq!(
            civil["value"][0]["calendar"],
            "2000-02-29T00:00:00.123456700"
        );
        assert_eq!(
            civil["value"][149]["calendar"],
            "2000-02-29T00:00:00.123456849"
        );
        long_exports(&home, &samples);
    }
}

#[test]
fn long_middle_wire_failure_discards_earlier_siblings_and_stops_queries() {
    for mode in ["reference", "status", "extent"] {
        let home = Home::new("s7-long-wire");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"earlier","db":1,"address":0,"data_type":"ltime"},
                {"name":"broken","db":1,"address":100,"data_type":"ldt","elements":100},
                {"name":"later","db":1,"address":2000,"data_type":"ltod"}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, 241);
            serve_long(&mut stream, 0, &i64::MIN.to_be_bytes(), 241);
            let reference = read_request(&mut stream, 0x84, 1, 100, 223);
            reply_fast(&mut stream, &reference, &[0; 223]);
            let mut reference = read_request(&mut stream, 0x84, 1, 323, 223);
            let mut data = vec![0xff, 4, 0x06, 0xf8]; //223*8 bits
            data.extend_from_slice(&[0; 223]);
            assert_eq!(reference.len(), 2);
            assert!(data.len() >= 4);
            match mode {
                "reference" => reference[1] ^= 1,
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
        let diagnostic = match mode {
            "reference" => "PDU reference",
            "status" => "rejected a Read Var item",
            "extent" => "sample width differs",
            _ => unreachable!(),
        };
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{error}"
        );
        assert_eq!(long_samples(&home), [] as [Value; 0]);
        assert_eq!(rows(&home).last().unwrap()["status"], "failed");
    }
}

#[test]
fn long_object_budget_rejects_before_a_later_query_or_large_array_io() {
    for minimum_only in [false, true] {
        let home = Home::new("s7-long-budget");
        let listener = listener();
        let first = if minimum_only {
            json!({"name":"large","db":1,"address":0,"data_type":"ltime","elements":16384})
        } else {
            json!({"name":"large","db":1,"address":0,"data_type":"ltod","elements":7000})
        };
        let mut config = config(
            &listener,
            &json!([first,{"name":"later","db":1,"address":100_000}]),
        );
        config["sources"][0]["timeout_ms"] = json!(5000);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, 960);
            if !minimum_only {
                serve_long(&mut stream, 0, &u64::MAX.to_be_bytes().repeat(7000), 960);
            }
            expect_closed(&mut stream);
        });
        let error = record_for(&home, &config, "6s").failure(1);
        server.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains("1 MiB"),
            "{error}"
        );
        assert_eq!(long_samples(&home), [] as [Value; 0]);
    }
}

#[test]
fn long_continuations_preserve_request_and_acquisition_deadlines() {
    for acquisition in [false, true] {
        let home = Home::new("s7-long-cutoff");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([
                {"name":"civil","db":1,"address":0,"data_type":"ldt","elements":100},
                {"name":"later","db":1,"address":2000,"data_type":"ltime"}
            ]),
        );
        config["sources"][0]["timeout_ms"] = json!(time::ms(if acquisition { 1000 } else { 250 }));
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, 241);
            let reference = read_request(&mut stream, 0x84, 1, 0, 223);
            thread::sleep(time::millis(160));
            reply_fast(&mut stream, &reference, &[0; 223]);
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
        assert_eq!(long_samples(&home), [] as [Value; 0]);
    }
}

#[test]
fn long_layouts_refuse_scaling_text_options_indexed_areas_and_bad_extents() {
    let mut patches = Vec::new();
    for kind in ["ltime", "ltod", "ldt"] {
        for patch in [
            json!({"scale":2}),
            json!({"offset":1}),
            json!({"bit":1}),
            json!({"area":"timer","db":0}),
            json!({"area":"counter","db":0}),
            json!({"capacity":8}),
            json!({"encoding":"ascii"}),
            json!({"elements":0}),
            json!({"elements":16385}),
            json!({"address":2_097_145}),
        ] {
            let mut signal = json!({"name":"value","db":1,"address":0,"data_type":kind});
            for (key, value) in patch.as_object().unwrap() {
                signal[key].clone_from(value);
            }
            patches.push(signal);
        }
    }
    for signal in patches {
        let home = Home::new("s7-long-config");
        let listener = listener();
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
