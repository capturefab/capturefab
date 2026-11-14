//! Keyence IV2/IV3 field network and IV4 user manual replies through the CLI.

use super::{Device, Home, accept, assert_unpublished, listener, receive};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write as _,
    net::{SocketAddr, TcpStream},
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

const IV2: &[u8] = b"VI,IV2-G500CA,R1.10.00\r";
const IV3: &[u8] = b"VI,IV3-G500CA,R1.10.00\r";
const IV4: &[u8] = b"VI,IV4-G500CA,R1.10.00\r";
// The manuals show these two tools with one score and one diameter. The
// example's typesetting spaces do not appear in the command/reply bytes.
const EXAMPLE: &[u8] = b"RT,01234,NG,01,OK,0000080,02,NG,0021500\r";

fn request(stream: &mut TcpStream, stop: &AtomicBool, command: [u8; 3]) {
    let mut actual = [0; 3];
    assert!(receive(stream, &mut actual, stop));
    assert_eq!(actual, command);
}

fn respond(stream: &mut TcpStream, bytes: &[u8]) {
    for part in bytes.chunks(7) {
        stream.write_all(part).unwrap();
    }
}

fn source(address: SocketAddr) -> Value {
    json!({"id":"sensor", "protocol":"keyence_iv", "address":address.to_string(),
        "timeout_ms":1000, "every_ms":3_600_000})
}

fn record(home: &Home, source: &Value, duration: &str) -> super::support::cli::Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        duration,
        "-o",
        "run.jsonl",
    ])
}

fn rows(home: &Home, file: &str) -> Vec<Value> {
    fs::read_to_string(home.path(file))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn keyence_iv_reads_the_vendor_vi_and_rt_examples_for_each_family() {
    for (identity, model) in [
        (IV2, "IV2-G500CA"),
        (IV3, "IV3-G500CA"),
        (IV4, "IV4-G500CA"),
    ] {
        let home = Home::new("keyence-iv-standard");
        let (listener, address) = listener();
        let fixture = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            request(&mut stream, &stop, *b"VI\r");
            respond(&mut stream, identity);
            request(&mut stream, &stop, *b"RT\r");
            respond(&mut stream, EXAMPLE);
            assert!(!receive(&mut stream, &mut [0], &stop));
            1
        });
        let summary = record(&home, &source(address), "600ms").success();
        assert_eq!(fixture.finish(), 1);
        assert_eq!(summary["samples"], 1);
        let recorded = rows(&home, "run.jsonl");
        assert_eq!(recorded[0]["sources"][0]["protocol"], "keyence_iv");
        assert_eq!(recorded[1]["signal"], "inspection");
        assert_eq!(
            recorded[1]["value"],
            json!({
                "state":"available", "model":model, "firmware":"R1.10.00", "format":"standard",
                "raw_reply":str::from_utf8(EXAMPLE).unwrap(),
                "result":{"number":1234,"number_raw":"01234","status":"NG","tools":[
                    {"number":1,"status":"OK","measurement_code":80,"measurement_raw":"0000080"},
                    {"number":2,"status":"NG","measurement_code":21500,"measurement_raw":"0021500"}
                ]}
            })
        );
        assert_eq!(recorded.last().unwrap()["status"], "complete");
        home.ok(&["data", "export", "run.jsonl", "-o", "results.csv"]);
        let csv = fs::read_to_string(home.path("results.csv")).unwrap();
        assert!(csv.contains("0000080") && csv.contains("0021500") && csv.contains("01234"));
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "-o",
            "results.jsonl",
            "--format",
            "jsonl",
        ]);
        let mut expected = recorded[1].clone();
        expected.as_object_mut().unwrap().remove("kind");
        assert_eq!(rows(&home, "results.jsonl"), [expected]);
    }
}

#[test]
fn keyence_iv_detailed_master_readout_preserves_ocr_delimiters_padding_and_wrap() {
    let home = Home::new("keyence-iv-detailed");
    let (listener, address) = listener();
    let fixture = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        request(&mut stream, &stop, *b"VI\r");
        respond(&mut stream, IV3);
        for (number, status) in [("32767", "07"), ("00000", "99")] {
            request(&mut stream, &stop, *b"RT\r");
            // Synthetic detailed frame with the exact record widths from
            // H06MX pp. 6-24–26. The OCR comma is inside its 16-byte field.
            let reply = format!(
                "RT,{number},{status},1010,00021,127,01,OK,0000080,A,B1234567890123  ,64,--,0000000{}\r",
                " ".repeat(19)
            );
            respond(&mut stream, reply.as_bytes());
        }
        assert!(!receive(&mut stream, &mut [0], &stop));
        2
    });
    let mut cfg = source(address);
    cfg["format"] = json!("detailed_master");
    cfg["signal"] = json!("station_a");
    cfg["every_ms"] = json!(350);
    let result = record(&home, &cfg, "650ms").success();
    assert_eq!(fixture.finish(), 2);
    assert_eq!(result["samples"], 2);
    let records = rows(&home, "run.jsonl");
    let samples = records
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 2);
    for (row, (number, raw, status)) in samples
        .iter()
        .zip([(32767, "32767", "07"), (0, "00000", "99")])
    {
        assert_eq!(row["signal"], "station_a");
        assert_eq!(row["value"]["format"], "detailed_master");
        assert_eq!(
            row["value"]["result"],
            json!({
                "number":number,"number_raw":raw,"status":status,
                "details":{"logic":[true,false,true,false],"processing_time_ms":21,"program":127},
                "tools":[
                    {"number":1,"status":"OK","measurement_code":80,"measurement_raw":"0000080","details_raw":",A,B1234567890123  "},
                    {"number":64,"status":"--","measurement_raw":"0000000","details_raw":" ".repeat(19)}
                ]
            })
        );
    }
}

#[test]
fn keyence_iv_no_current_result_is_explicit_and_can_recover_next_poll() {
    let home = Home::new("keyence-iv-no-result");
    let (listener, address) = listener();
    let fixture = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        request(&mut stream, &stop, *b"VI\r");
        respond(&mut stream, IV3);
        request(&mut stream, &stop, *b"RT\r");
        respond(&mut stream, b"ER,RT,03\r");
        request(&mut stream, &stop, *b"RT\r");
        respond(&mut stream, b"RT,00001,07,00,--,0000000,08,07,0000080\r");
        assert!(!receive(&mut stream, &mut [0], &stop));
        2
    });
    let mut cfg = source(address);
    cfg["every_ms"] = json!(350);
    let result = record(&home, &cfg, "650ms").success();
    assert_eq!(fixture.finish(), 2);
    assert_eq!(result["samples"], 2);
    let recorded = rows(&home, "run.jsonl");
    assert_eq!(
        recorded[1]["value"],
        json!({"state":"unavailable","command_error":3,
        "raw_reply":"ER,RT,03\r","model":"IV3-G500CA","firmware":"R1.10.00","format":"standard"})
    );
    assert_eq!(
        recorded[2]["value"]["result"],
        json!({"number":1,"number_raw":"00001","status":"07","tools":[
            {"number":0,"status":"--","measurement_raw":"0000000"},
            {"number":8,"status":"07","measurement_code":80,"measurement_raw":"0000080"}
        ]})
    );
}

#[test]
fn keyence_iv_requires_identified_family_before_sending_result_command() {
    for (identity, format, cause) in [
        (
            &b"Welcome to In-Sight(tm) 3800 Session 1\r"[..],
            "standard",
            "expected VI",
        ),
        (
            &b"VI,IV-500CA,R2.00.00\r"[..],
            "standard",
            "did not identify",
        ),
        (&b"VI,IV3-,R1.30.00\r"[..], "standard", "invalid VI model"),
        (IV2, "detailed_master", "requires IV3"),
        (
            &b"VI,IV4-HG500CA,R1.10.00\r"[..],
            "standard",
            "unsupported IV4 sensor-head model",
        ),
    ] {
        let home = Home::new("keyence-iv-identity");
        let (listener, address) = listener();
        let fixture = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            request(&mut stream, &stop, *b"VI\r");
            respond(&mut stream, identity);
            assert!(!receive(&mut stream, &mut [0], &stop));
            1
        });
        let mut cfg = source(address);
        cfg["format"] = json!(format);
        let error = record(&home, &cfg, "600ms").failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        assert_eq!(fixture.finish(), 1);
        assert_unpublished(&home);
    }
}

#[test]
fn keyence_iv_rejects_bad_result_envelopes_and_tool_records_atomically() {
    let oversized = format!("RT,00001,OK,{}\r", "0".repeat(3072));
    for (identity, format, reply, cause) in [
        (IV3, "standard", &b"ER,RT,02\r"[..], "command error 02"),
        (IV3, "standard", &b"ER,T2,03\r"[..], "expected RT"),
        (
            IV3,
            "standard",
            &b"RT,32768,OK\r"[..],
            "exceeds the device range",
        ),
        (
            IV3,
            "standard",
            &b"RT,00001,OK,01,OK,0000080,01,NG,0000000\r"[..],
            "duplicate tool",
        ),
        (
            IV3,
            "standard",
            &b"RT,00001,07,09,07,0000080\r"[..],
            "tools 00–08",
        ),
        (
            IV2,
            "standard",
            &b"RT,00001,OK,17,OK,0000080\r"[..],
            "exceeds the device range",
        ),
        (
            IV3,
            "standard",
            &b"RT,00001,OK,01,--,0000001\r"[..],
            "unavailable tool",
        ),
        (
            IV3,
            "standard",
            &b"RT,00001,OK,01,OK,000080\r"[..],
            "malformed fixed-width",
        ),
        (
            IV3,
            "detailed",
            &b"RT,00001,OK,1002,00001,001\r"[..],
            "four binary digits",
        ),
        (
            IV3,
            "detailed",
            &b"RT,00001,OK,1001,00001,001,01,OK,0000080\r"[..],
            "malformed fixed-width",
        ),
        (
            IV3,
            "detailed",
            &b"RT,00001,OK,1001,00001,001,01,OK,0000080xxxxxxxxxxxxxxxxxxx\r"[..],
            "malformed detailed tool extension",
        ),
        (
            IV3,
            "standard",
            oversized.as_bytes(),
            "exceeds the byte limit",
        ),
        (
            IV4,
            "standard",
            &b"RT,00001,07,09,07,0000080\r"[..],
            "tools 00–08",
        ),
        (
            IV4,
            "standard",
            &b"RT,00001,--,01,0000001,0000002,01,0000003,0000004\r"[..],
            "duplicate tool number in IV4 through-count",
        ),
        (
            IV4,
            "standard",
            &b"RT,00001,--,00,0000001,0000002\r"[..],
            "numbered 01–08",
        ),
        (
            IV4,
            "standard",
            &b"RT,00001,--,01,0000001,0000002,02,--,0000000\r"[..],
            "invalid fixed-width decimal",
        ),
    ] {
        let home = Home::new("keyence-iv-result-reject");
        let (listener, address) = listener();
        let reply = reply.to_vec();
        let fixture = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            request(&mut stream, &stop, *b"VI\r");
            respond(&mut stream, identity);
            request(&mut stream, &stop, *b"RT\r");
            stream.write_all(&reply).unwrap();
            if reply.len() <= 3072 {
                assert!(!receive(&mut stream, &mut [0], &stop));
            }
            1
        });
        let mut cfg = source(address);
        cfg["format"] = json!(format);
        let error = record(&home, &cfg, "600ms").failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        assert_eq!(fixture.finish(), 1);
        assert_unpublished(&home);
    }
}

#[test]
fn keyence_iv_version_and_fragmented_result_share_one_deadline() {
    let home = Home::new("keyence-iv-deadline");
    let (listener, address) = listener();
    let fixture = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        request(&mut stream, &stop, *b"VI\r");
        thread::sleep(Duration::from_millis(240));
        respond(&mut stream, IV3);
        request(&mut stream, &stop, *b"RT\r");
        stream.write_all(b"RT,01234,NG,").unwrap();
        thread::sleep(Duration::from_millis(240));
        let _ = stream.write_all(b"01,OK,0000080\r");
        1
    });
    let mut cfg = source(address);
    cfg["timeout_ms"] = json!(400);
    let error = record(&home, &cfg, "1s").failure(4);
    assert_eq!(error["code"], "timeout");
    assert_eq!(fixture.finish(), 1);
    assert_unpublished(&home);
}

#[test]
fn keyence_iv4_standard_sorting_master_and_through_count_records() {
    for (format, reply, result) in [
        (
            "standard",
            &b"RT,00042,07,00,--,0000000,08,07,0000080\r"[..],
            json!({"number":42,"number_raw":"00042","status":"07","tools":[
                {"number":0,"status":"--","measurement_raw":"0000000"},
                {"number":8,"status":"07","measurement_code":80,"measurement_raw":"0000080"}
            ]}),
        ),
        (
            "standard_master",
            &b"RT,32767,99,64,OK,9999999\r"[..],
            json!({"number":32767,"number_raw":"32767","status":"99","tools":[
                {"number":64,"status":"OK","measurement_code":9_999_999,"measurement_raw":"9999999"}
            ]}),
        ),
        (
            "standard",
            &b"RT,00000,--,01,0000002,0000010,08,9999999,9999999\r"[..],
            json!({"number":0,"number_raw":"00000","status":"--","tools":[],"through_counts":[
                {"number":1,"count":2,"count_raw":"0000002","daily_total":10,"daily_total_raw":"0000010"},
                {"number":8,"count":9_999_999,"count_raw":"9999999","daily_total":9_999_999,"daily_total_raw":"9999999"}
            ]}),
        ),
        (
            "standard",
            &b"RT,00001,--,01,--,0000000\r"[..],
            json!({"number":1,"number_raw":"00001","status":"--","tools":[
                {"number":1,"status":"--","measurement_raw":"0000000"}
            ]}),
        ),
    ] {
        let home = Home::new("keyence-iv4-formats");
        let (listener, address) = listener();
        let fixture = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            request(&mut stream, &stop, *b"VI\r");
            respond(&mut stream, IV4);
            request(&mut stream, &stop, *b"RT\r");
            respond(&mut stream, reply);
            assert!(!receive(&mut stream, &mut [0], &stop));
            1
        });
        let mut cfg = source(address);
        cfg["format"] = json!(format);
        let summary = record(&home, &cfg, "600ms").success();
        assert_eq!(summary["samples"], 1);
        assert_eq!(fixture.finish(), 1);
        let recorded = rows(&home, "run.jsonl");
        assert_eq!(
            recorded[1]["value"],
            json!({
                "state":"available","model":"IV4-G500CA","firmware":"R1.10.00","format":format,
                "raw_reply":str::from_utf8(reply).unwrap(),"result":result
            })
        );
    }
}

#[test]
fn keyence_iv4_rejects_detailed_formats_before_polling() {
    for format in ["detailed", "detailed_master"] {
        let home = Home::new("keyence-iv4-detailed");
        let (listener, address) = listener();
        let fixture = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            request(&mut stream, &stop, *b"VI\r");
            respond(&mut stream, IV4);
            assert!(!receive(&mut stream, &mut [0], &stop));
            1
        });
        let mut cfg = source(address);
        cfg["format"] = json!(format);
        let error = record(&home, &cfg, "600ms").failure(5);
        assert_eq!(error["code"], "unsupported");
        assert!(error["message"].as_str().unwrap().contains("IV4 detailed"));
        assert_eq!(fixture.finish(), 1);
        assert_unpublished(&home);
    }
}
