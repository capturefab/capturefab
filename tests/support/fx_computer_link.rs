//! Read-only serial FX Computer Link via the actual CLI. The three published
//! Mitsubishi `JY997D16901R` §§7.1–7.3 exchanges include the host ACK.

use super::{Device, Home, assert_unpublished, receive, serial_pair};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{ErrorKind, Read as _, Write as _},
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

const BR: &[u8] = b"\x0505FFBRAX00400547";
const BR_REPLY: &[u8] = b"\x0205FF01101\x03E7";
const WR: &[u8] = b"\x0505FFWR0TN1230264";
const QR: &[u8] = b"\x0505FFQR0R030000029B";
const WORD_REPLY: &[u8] = b"\x0205FF7BC91234\x03B3";
const ACK: &[u8] = b"\x0605FF";

fn source(device: &str, signals: &Value) -> Value {
    json!({"id":"fx","protocol":"fx_computer_link","device":device,
        "data_bits":8,"parity":"none","unit":5,"every_ms":3_600_000,
        "timeout_ms":time::ms(800),"signals":signals})
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
        &time::arg(1000),
        "-o",
        "run.jsonl",
    ])
}

fn samples(home: &Home, file: &str) -> Vec<Value> {
    fs::read_to_string(home.path(file))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample" || row.get("kind").is_none())
        .collect()
}

fn expect(master: &mut File, stop: &AtomicBool, expected: &[u8]) {
    let mut bytes = vec![0; expected.len()];
    assert!(receive(master, &mut bytes, stop));
    assert_eq!(bytes, expected);
}

fn expect_silence(master: &mut File, stop: &AtomicBool) {
    assert!(!receive(master, &mut [0], stop));
    // The CLI has finished before the stop flag. Check once more in case the
    // flag became visible before the worker observed an already queued byte.
    assert_eq!(
        master.read(&mut [0]).unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
}

fn send(master: &mut File, frame: &[u8]) {
    for part in frame.chunks(3) {
        master.write_all(part).unwrap();
        thread::sleep(Duration::from_micros(100));
    }
}

#[test]
fn fx_link_replays_mitsubishi_br_wr_and_qr_frames_with_ack() {
    for (signal, request, reply, value, wait) in [
        (
            json!({"name":"bits","device":"X","address":32,"data_type":"bool","elements":5}),
            BR,
            BR_REPLY,
            json!([false, true, true, false, true]),
            100_u64,
        ),
        (
            json!({"name":"timers","device":"TN","address":123,"elements":2}),
            WR,
            WORD_REPLY,
            json!([31689, 4660]),
            0,
        ),
        (
            json!({"name":"registers","device":"R","address":30000,"elements":2}),
            QR,
            WORD_REPLY,
            json!([31689, 4660]),
            0,
        ),
    ] {
        let home = Home::new("fx-link-vendor");
        let (mut master, slave, device) = serial_pair();
        let worker = Device::start(move |stop| {
            let _slave = slave;
            expect(&mut master, &stop, request);
            thread::sleep(Duration::from_millis(wait));
            let reply_started = Instant::now();
            send(&mut master, reply);
            expect(&mut master, &stop, ACK);
            assert!(reply_started.elapsed() >= time::millis(30));
            expect_silence(&mut master, &stop);
            1
        });
        let mut config = source(&device, &json!([signal]));
        config["message_wait_ms"] = json!(wait);
        config["ack_delay_ms"] = serde_json::from_str(&format!("{}e1", time::ms(3))).unwrap();
        let run = record(&home, &config);
        assert_eq!(run.success()["samples"], 1);
        assert_eq!(worker.finish(), 1);
        let rows = samples(&home, "run.jsonl");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["value"], value);
    }
}

#[test]
fn fx_link_format_four_unchecked_segments_multiple_stations_and_preserves_exports() {
    let home = Home::new("fx-link-format4");
    let (mut master, slave, device) = serial_pair();
    let worker = Device::start(move |stop| {
        let _slave = slave;
        expect(&mut master, &stop, b"\x0505FFWR0D010004\r\n");
        send(&mut master, b"\x0205FFFFFFFFFFFFFFFFFF\x03\r\n");
        expect(&mut master, &stop, b"\x0605FF\r\n");
        expect(&mut master, &stop, b"\x0505FFWR0D010404\r\n");
        send(&mut master, b"\x0205FF0001000000000020\x03\r\n");
        expect(&mut master, &stop, b"\x0605FF\r\n");

        expect(&mut master, &stop, b"\x050AFFBR0M000000\r\n");
        let mut bits = b"\x020AFF".to_vec();
        bits.extend(b"01".repeat(128));
        bits.extend(b"\x03\r\n");
        master.write_all(&bits).unwrap();
        expect(&mut master, &stop, b"\x060AFF\r\n");

        expect(&mut master, &stop, b"\x050FFFWR0D020002\r\n");
        send(&mut master, b"\x020FFF00003FA0\x03\r\n");
        expect(&mut master, &stop, b"\x060FFF\r\n");
        expect_silence(&mut master, &stop);
        4
    });
    let mut config = source(
        &device,
        &json!([
            {"name":"wide","device":"D","address":100,"data_type":"u64","elements":2},
            {"name":"bits","unit":10,"device":"M","address":0,"data_type":"bool","elements":256},
            {"name":"value","unit":15,"device":"D","address":200,"data_type":"f32"}
        ]),
    );
    config["format"] = json!("format_4");
    config["sum_check"] = json!(false);
    config["max_words"] = serde_json::from_str("4e0").unwrap();
    let run = record(&home, &config);
    assert_eq!(run.success()["samples"], 3);
    assert_eq!(worker.finish(), 4);
    let rows = samples(&home, "run.jsonl");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["value"], json!([u64::MAX, (1_u64 << 53) + 1]));
    assert_eq!(
        rows[1]["value"],
        json!((0..256).map(|bit| bit % 2 == 1).collect::<Vec<_>>())
    );
    assert_eq!(rows[2]["value"], json!(1.25));
    let csv = home.run(&["--json", "data", "export", "run.jsonl", "-o", "out.csv"]);
    assert_eq!(csv.success()["samples"], 3);
    assert!(
        fs::read_to_string(home.path("out.csv"))
            .unwrap()
            .contains("[18446744073709551615,9007199254740993]")
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
    assert_eq!(jsonl.success()["samples"], 3);
    let expected = rows
        .into_iter()
        .map(|mut row| {
            assert_eq!(
                row.as_object_mut().unwrap().remove("kind"),
                Some(json!("sample"))
            );
            row
        })
        .collect::<Vec<_>>();
    assert_eq!(samples(&home, "out.jsonl"), expected);
}

#[test]
fn fx_link_malformed_second_reply_never_publishes_first_signal_or_acknowledges_bad_data() {
    for (second, message, status) in [
        (
            &b"\x0206FF7BC91234\x03B4\r\n"[..],
            "station or PLC number",
            None,
        ),
        (b"\x0205FF7BC91234\x03B4\r\n", "sum check", None),
        (b"\x0205FF7BCZ1234\x03D4\r\n", "invalid hexadecimal", None),
        (b"\x0205FF7BC91234\x03B3\n\r", "length or terminator", None),
        (
            b"\x0205FF7BC91234\x03B3\r\nX",
            "bytes after read reply",
            None,
        ),
        (b"\x1505FF07\r\n", "PLC rejected read", Some(7)),
        (b"\x1505FF0Z\r\n", "invalid hexadecimal", None),
        (b"\x1505FF07\r\nX", "bytes after read reply", None),
    ] {
        let home = Home::new("fx-link-invalid");
        let (mut master, slave, device) = serial_pair();
        let worker = Device::start(move |stop| {
            let _slave = slave;
            expect(&mut master, &stop, b"\x0505FFWR0TN1230264\r\n");
            send(&mut master, b"\x0205FF7BC91234\x03B3\r\n");
            expect(&mut master, &stop, b"\x0605FF\r\n");
            expect(&mut master, &stop, b"\x0505FFQR0R030000029B\r\n");
            master.write_all(second).unwrap();
            expect_silence(&mut master, &stop);
            2
        });
        let mut config = source(
            &device,
            &json!([
                {"name":"first","device":"TN","address":123,"elements":2},
                {"name":"second","device":"R","address":30000,"elements":2}
            ]),
        );
        config["format"] = json!("format_4");
        let run = record(&home, &config);
        let error = run.failure(1);
        assert_eq!(worker.finish(), 2);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        if let Some(status) = status {
            assert_eq!(error["details"]["fx_error_code"], status);
        } else {
            assert!(error["details"].get("fx_error_code").is_none());
        }
        assert_unpublished(&home);
    }
}

#[test]
fn fx_link_partial_poll_does_not_get_a_new_deadline_after_ack() {
    let home = Home::new("fx-link-timeout");
    let (mut master, slave, device) = serial_pair();
    let worker = Device::start(move |stop| {
        let _slave = slave;
        expect(&mut master, &stop, WR);
        thread::sleep(time::millis(420));
        send(&mut master, WORD_REPLY);
        expect(&mut master, &stop, ACK);
        expect(&mut master, &stop, QR);
        thread::sleep(time::millis(420));
        let _ = master.write_all(WORD_REPLY);
        expect_silence(&mut master, &stop);
        2
    });
    let mut config = source(
        &device,
        &json!([
            {"name":"first","device":"TN","address":123,"elements":2},
            {"name":"second","device":"R","address":30000,"elements":2}
        ]),
    );
    config["timeout_ms"] = json!(time::ms(700));
    let run = record(&home, &config);
    assert_eq!(run.failure(4)["code"], "timeout");
    assert_eq!(worker.finish(), 2);
    assert_unpublished(&home);
}

#[test]
fn fx_link_ack_delay_must_fit_the_poll_deadline() {
    let home = Home::new("fx-link-ack-deadline");
    let (mut master, slave, device) = serial_pair();
    let worker = Device::start(move |stop| {
        let _slave = slave;
        expect(&mut master, &stop, WR);
        send(&mut master, WORD_REPLY);
        expect_silence(&mut master, &stop);
        1
    });
    let mut config = source(
        &device,
        &json!([{"name":"timer","device":"TN","address":123,"elements":2}]),
    );
    config["ack_delay_ms"] = json!(time::ms(900));
    let run = record(&home, &config);
    assert_eq!(run.failure(4)["code"], "timeout");
    assert_eq!(worker.finish(), 1);
    assert_unpublished(&home);

    for invalid in [0, 30_001] {
        let home = Home::new("fx-link-invalid-ack-delay");
        config["ack_delay_ms"] = json!(invalid);
        let error = record(&home, &config).failure(2);
        assert!(error["message"].as_str().unwrap().contains("ack_delay_ms"));
    }
}
