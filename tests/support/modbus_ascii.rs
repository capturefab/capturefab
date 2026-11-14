//! The D100 read follows the Delta DVP programming manual. PTYs exercise
//! protocol framing with 8N1, independently of the configured hardware UART.

use super::{Device, Home, assert_unpublished, receive, record_modbus, serial_pair};
use crate::support::time;
use serde_json::{Value, json};
use std::{fs, io::Write as _, sync::atomic::AtomicBool, thread, time::Duration};

const D100: &[u8; 17] = b":01031064000286\r\n";
const D100_REPLY: &[u8] = b":0103041234ABCD3A\r\n";
const MAX_INPUT: &[u8; 17] = b":F704FF83007D06\r\n";
const WIDE_INPUT: &[u8; 17] = b":F70410000008ED\r\n";

fn source(device: &str, signals: &Value) -> Value {
    json!({"id":"bus","protocol":"modbus_ascii","device":device,
        "data_bits":8,"parity":"none","every_ms":3_600_000,"timeout_ms":time::ms(1000),"signals":signals})
}

fn expect(master: &mut fs::File, stop: &AtomicBool, expected: &[u8; 17]) {
    let mut request = [0; 17];
    assert!(receive(master, &mut request, stop));
    assert_eq!(&request, expected);
}

#[test]
fn modbus_ascii_reads_delta_registers_and_multiple_stations_on_one_terminal() {
    let home = Home::new("modbus-ascii-read");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        expect(&mut master, &stop, D100);
        master.write_all(b"\r\n:01").unwrap();
        for part in D100_REPLY.chunks(4) {
            master.write_all(part).unwrap();
            thread::sleep(Duration::from_millis(3));
        }
        expect(&mut master, &stop, WIDE_INPUT);
        master
            .write_all(b":F70410FFFFFFFFFFFFFFFF0020000000000001DC\r\n")
            .unwrap();
        expect(&mut master, &stop, b":01010700000DEA\r\n");
        master.write_all(b":0101028D155A\r\n").unwrap();
        expect(&mut master, &stop, b":F70200090001FD\r\n");
        master.write_all(b":F702010105\r\n").unwrap();
        expect(&mut master, &stop, MAX_INPUT);
        // 125 registers (250 data bytes) end at address 65535. The
        // all-zero data leave LRC = -(0xF7 + 0x04 + 0xFA) mod 256.
        let max_reply = format!(":F704FA{}0B\r\n", "00".repeat(250));
        assert_eq!(max_reply.len(), 511);
        master.write_all(max_reply.as_bytes()).unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        5
    });
    let result = record_modbus(
        &home,
        &source(
            &device,
            &json!([
                {"name":"delta_d100","address":0x1064,"data_type":"u32"},
                {"name":"wide","unit":247.0,"address":0x1000,"table":"input","data_type":"u64","elements":2},
                {"name":"coils","address":0x700,"table":"coil","data_type":"bool","elements":13},
                {"name":"input","unit":247,"address":9,"table":"discrete","data_type":"bool"},
                {"name":"maximum","unit":247,"address":0xff83,"table":"input","elements":125}
            ]),
        ),
    )
    .success();
    assert_eq!(server.finish(), 5);
    assert_eq!(result["status"], "complete");
    assert_eq!(result["samples"], 5);

    let rows = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let rows = rows
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["sources"][0]["protocol"], "modbus_ascii");
    let samples = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    let expected_bits = (0..13)
        .map(|index| 0x158d & (1 << index) != 0)
        .collect::<Vec<_>>();
    for (sample, (name, value)) in samples.iter().zip([
        ("delta_d100", json!(f64::from(0x1234_abcd))),
        ("wide", json!([u64::MAX, (1_u64 << 53) + 1])),
        ("coils", json!(expected_bits)),
        ("input", json!(true)),
        ("maximum", json!(vec![0.0; 125])),
    ]) {
        assert_eq!(sample["signal"], name);
        assert_eq!(sample["value"], value);
    }
    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert!(csv.contains("[18446744073709551615,9007199254740993]"));
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported = fs::read_to_string(home.path("values.jsonl")).unwrap();
    let exported = exported
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), samples.len());
    for (actual, original) in exported.iter().zip(samples) {
        let mut expected = original.clone();
        expected.as_object_mut().unwrap().remove("kind");
        assert_eq!(*actual, expected);
    }
}

#[test]
fn modbus_ascii_rejects_corrupt_or_wrong_station_replies_atomically() {
    let oversized = format!(":{}\r\n", "00".repeat(256));
    for (reply, cause) in [
        (&b":F703020007FE\r\n"[..], "LRC mismatch"),
        (&b":F70302000GFD\r\n"[..], "invalid hex digit"),
        (&b":F70302007FD\r\n"[..], "invalid ASCII response length"),
        (&b":F703020007FD\n"[..], "must end with CRLF"),
        (&b":F6830285\r\n"[..], "unexpected unit"),
        (&b":F7830284\r\n"[..], "Modbus exception 2"),
        (oversized.as_bytes(), "exceeds 513 characters"),
    ] {
        let home = Home::new("modbus-ascii-reject");
        let (mut master, slave, device) = serial_pair();
        let reply = reply.to_vec();
        let server = Device::start(move |stop| {
            let _slave = slave;
            expect(&mut master, &stop, D100);
            master.write_all(D100_REPLY).unwrap();
            expect(&mut master, &stop, b":F7030000000105\r\n");
            master.write_all(&reply).unwrap();
            assert!(!receive(&mut master, &mut [0], &stop));
            2
        });
        let error = record_modbus(
            &home,
            &source(
                &device,
                &json!([{"name":"valid","address":0x1064,"data_type":"u32"},
                    {"name":"bad","unit":247,"address":0}]),
            ),
        )
        .failure(1);
        assert_eq!(server.finish(), 2);
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        assert_unpublished(&home);
    }
}

#[test]
fn modbus_ascii_later_fragments_share_the_first_requests_deadline() {
    let home = Home::new("modbus-ascii-deadline");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        expect(&mut master, &stop, D100);
        thread::sleep(time::millis(300));
        master.write_all(D100_REPLY).unwrap();
        expect(&mut master, &stop, b":F7030000000105\r\n");
        master.write_all(b":F70302").unwrap();
        thread::sleep(time::millis(300));
        master.write_all(b"0007FD\r\n").unwrap();
        2
    });
    let mut cfg = source(
        &device,
        &json!([{"name":"first","address":0x1064,"data_type":"u32"},
            {"name":"second","unit":247,"address":0}]),
    );
    cfg["timeout_ms"] = json!(time::ms(450));
    let error = record_modbus(&home, &cfg).failure(4);
    assert_eq!(server.finish(), 2);
    assert_eq!(error["code"], "timeout");
    assert_unpublished(&home);
}

#[test]
fn modbus_ascii_rejects_invalid_bus_settings_before_opening() {
    for (key, value) in [
        ("unit", json!(0)),
        ("unit", json!(248)),
        ("data_bits", json!(6)),
        ("stop_bits", json!(3)),
        ("baud", json!(9601)),
        ("signals", json!([{"name":"value","address":0,"unit":0}])),
        (
            "signals",
            json!([{"name":"value","address":0,"unit":247.5}]),
        ),
    ] {
        let home = Home::new("modbus-ascii-config");
        let mut cfg = source("/missing-device", &json!([{"name":"value","address":0}]));
        cfg[key] = value;
        record_modbus(&home, &cfg).failure(2);
        assert!(!home.path("run.jsonl").exists());
    }
}
