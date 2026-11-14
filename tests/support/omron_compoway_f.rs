//! Omron H175 §2-3 and §3-1, Z200 §3-5 and Z280 §6-6 Variable Area Read fixtures.
//! The PTY tests bytes and deadlines using portable 8N1 settings.

use super::{Device, Home, assert_unpublished, receive, serial_pair};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write as _,
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

const G3ZA_DWORD: &[u8] = b"\x02010000101C00001000001\x03A";
const G3ZA_WORD: &[u8] = b"\x02010000101800001000001\x03:";
const G3PW_DWORD: &[u8] = b"\x02010000101CE0004000001\x031";
const G3PW_WORD: &[u8] = b"\x020100001018E0004000001\x03J";
const E5CC_PV: &[u8] = b"\x02010000101C00000000001\x03@";
const E5CC_REPLY: &[u8] = b"\x0201000001010000FFFFFB5E\x03v";
// The G3ZA and G3PW manuals leave the BCC to be calculated. These
// replies' final raw BCC is 0x02, even though 0x02 is also the start byte.
const DWORD_REPLY: &[u8] = b"\x020100000101000000000000\x03\x02";
const WORD_REPLY: &[u8] = b"\x02010000010100000000\x03\x02";
const I16_ARRAY: &[u8] = b"\x022700001018E0004000002\x03M";
const I16_REPLY: &[u8] = b"\x0227000001010000FFFE7FFF\x03t";
const LAST_DWORD: &[u8] = b"\x02990000101C0FFFF000001\x03A";
// Raw BCC 0x03 follows the separate 0x03 ETX.
const LAST_REPLY: &[u8] = b"\x0299000001010000FFFFFFFF\x03\x03";
const I32_ARRAY: &[u8] = b"\x02270000101C0FFF8000008\x033";
const I32_REPLY: &[u8] =
    b"\x0227000001010000FFFFFFFF800000007FFFFFFF0000000000000001000000020000000300000004\x03{";

fn source(device: &str, signals: &Value) -> Value {
    json!({"id":"power","protocol":"omron_compoway_f","device":device,
        "data_bits":8,"parity":"none","stop_bits":1,
        "every_ms":3_600_000,"timeout_ms":800,"signals":signals})
}

fn record(home: &Home, source: &Value) -> super::support::cli::Run {
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
        "1s",
        "-o",
        "run.jsonl",
    ])
}

fn expect(master: &mut File, stop: &AtomicBool, request: &[u8]) {
    let mut actual = [0; 24];
    assert_eq!(request.len(), actual.len());
    assert!(receive(master, &mut actual, stop));
    assert_eq!(actual.as_slice(), request);
}

#[test]
fn compoway_f_reads_vendor_fixtures_and_multidrop_arrays() {
    let home = Home::new("compoway-f-read");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        for (request, reply) in [
            (G3ZA_DWORD, DWORD_REPLY),
            (G3ZA_WORD, WORD_REPLY),
            (G3PW_DWORD, DWORD_REPLY),
            (G3PW_WORD, WORD_REPLY),
            (E5CC_PV, E5CC_REPLY),
            (I16_ARRAY, I16_REPLY),
            (LAST_DWORD, LAST_REPLY),
            (I32_ARRAY, I32_REPLY),
        ] {
            expect(&mut master, &stop, request);
            let (bcc, body) = reply.split_last().unwrap();
            for chunk in body.chunks(12) {
                master.write_all(chunk).unwrap();
                thread::sleep(Duration::from_millis(2));
            }
            master.write_all(&[*bcc]).unwrap();
        }
        assert!(!receive(&mut master, &mut [0], &stop));
        8
    });
    let config = source(
        &device,
        &json!([
            {"name":"g3za_control_32","variable_type":0xc0,"address":1,"data_type":"u32"},
            {"name":"g3za_control_16","variable_type":0x80,"address":1,"data_type":"u16"},
            {"name":"g3pw_current_32","variable_type":0xce,"address":4,"data_type":"u32"},
            {"name":"g3pw_current_16","variable_type":0x8e,"address":4,"data_type":"u16"},
            {"name":"e5cc_pv","variable_type":0xc0,"address":0,"data_type":"i32"},
            {"name":"signed_16","unit":27.0,"variable_type":0x8e,"address":4,"elements":2,"data_type":"i16"},
            {"name":"unsigned_32","unit":99,"variable_type":0xc0,"address":65535,"data_type":"u32"},
            {"name":"signed_32","unit":27,"variable_type":0xc0,"address":65528,"elements":8,"data_type":"i32"}
        ]),
    );
    let run = record(&home, &config);
    assert_eq!(server.finish(), 8);
    assert_eq!(run.success()["samples"], 8);
    let text = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let rows = text
        .lines()
        .map(|row| serde_json::from_str::<Value>(row).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["sources"][0]["protocol"], "omron_compoway_f");
    let samples = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 8);
    for (sample, (name, value)) in samples.iter().zip([
        ("g3za_control_32", json!(0)),
        ("g3za_control_16", json!(0)),
        ("g3pw_current_32", json!(0)),
        ("g3pw_current_16", json!(0)),
        ("e5cc_pv", json!(-1186)),
        ("signed_16", json!([-2, i16::MAX])),
        ("unsigned_32", json!(u32::MAX)),
        ("signed_32", json!([-1, i32::MIN, i32::MAX, 0, 1, 2, 3, 4])),
    ]) {
        assert_eq!(sample["signal"], name);
        assert_eq!(sample["value"], value);
    }
    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert!(csv.contains("4294967295"));
    assert!(csv.contains("[-1,-2147483648,2147483647,0,1,2,3,4]"));
}

fn two_signals() -> Value {
    json!([
        {"name":"valid","variable_type":0xc0,"address":1,"data_type":"u32"},
        {"name":"sibling","unit":27,"variable_type":0x8e,"address":4,"elements":2,"data_type":"i16"}
    ])
}

#[test]
fn compoway_f_rejects_bad_frames_and_device_errors_atomically() {
    let mut bad_bcc = I16_REPLY.to_vec();
    *bad_bcc.last_mut().unwrap() ^= 1;
    let oversized = [2; 82];
    for (reply, cause, codes) in [
        (bad_bcc.as_slice(), "BCC mismatch", None),
        (DWORD_REPLY, "unexpected unit or sub-address", None),
        (
            &b"\x0227010001010000FFFE7FFF\x03u"[..],
            "unexpected unit or sub-address",
            None,
        ),
        (
            &b"\x0227000001020000FFFE7FFF\x03w"[..],
            "unexpected FINS-mini response service",
            None,
        ),
        (
            &b"\x0227000001010000FFFF7FFG\x03v"[..],
            "non-hex digit",
            None,
        ),
        (
            &b"\x0227000001010000FFFF\x03\x06"[..],
            "requested variable count",
            None,
        ),
        (
            &b"\x02270013\x03\x04"[..],
            "controller rejected",
            Some((0x13, None)),
        ),
        (
            &b"\x0227000F01011101\x03q"[..],
            "controller rejected",
            Some((0x0f, Some(0x1101))),
        ),
        (
            &b"\x0227000001011101\x03\x07"[..],
            "controller rejected",
            Some((0, Some(0x1101))),
        ),
        (&oversized[..], "exceeds the frame bound", None),
    ] {
        let home = Home::new("compoway-f-reject");
        let (mut master, slave, device) = serial_pair();
        let reply = reply.to_vec();
        let server = Device::start(move |stop| {
            let _slave = slave;
            expect(&mut master, &stop, G3ZA_DWORD);
            master.write_all(DWORD_REPLY).unwrap();
            expect(&mut master, &stop, I16_ARRAY);
            master.write_all(&reply).unwrap();
            assert!(!receive(&mut master, &mut [0], &stop));
            2
        });
        let run = record(&home, &source(&device, &two_signals()));
        assert_eq!(server.finish(), 2);
        let error = run.failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        if let Some((end_code, response_code)) = codes {
            assert_eq!(error["details"]["end_code"], end_code);
            assert_eq!(
                error["details"].get("response_code"),
                response_code.map(Value::from).as_ref()
            );
        }
        assert_unpublished(&home);
    }
}

#[test]
fn compoway_f_uses_one_deadline_for_both_stations_and_the_final_bcc() {
    let home = Home::new("compoway-f-timeout");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        expect(&mut master, &stop, G3ZA_DWORD);
        thread::sleep(Duration::from_millis(400));
        master.write_all(DWORD_REPLY).unwrap();
        expect(&mut master, &stop, I16_ARRAY);
        let (bcc, body) = I16_REPLY.split_last().unwrap();
        master.write_all(body).unwrap();
        thread::sleep(Duration::from_millis(400));
        let _ = master.write_all(&[*bcc]);
        2
    });
    let mut config = source(&device, &two_signals());
    config["timeout_ms"] = json!(700);
    let run = record(&home, &config);
    assert_eq!(server.finish(), 2);
    assert_eq!(run.failure(4)["code"], "timeout");
    assert_unpublished(&home);
}

#[test]
fn compoway_f_rejects_bad_maps_before_opening_the_port() {
    let home = Home::new("compoway-f-invalid");
    for patch in [
        json!({"unit":100}),
        json!({"variable_type":0x20}),
        json!({"data_type":"u16"}),
        json!({"elements":9}),
        json!({"address":65535,"elements":2}),
    ] {
        let mut signals = two_signals();
        for (key, value) in patch.as_object().unwrap() {
            signals[0][key] = value.clone();
        }
        let config = source("unused", &signals);
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}
