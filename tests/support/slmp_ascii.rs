//! Mitsubishi `SH-080956ENG-N` §§4.1–4.2 and 5.2 through the real CLI.
//! Printed pp. 48 and 50 give the M100 and TN100 request and reply data.

use super::{Home, accept, config, listener, record, rows, send, time};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
};

const M100: &[u8] = b"500000FF03FF000018000404010001M*0001000008";
const M100_REPLY: &[u8] = b"D00000FF03FF00000C000000010011";
const TN100: &[u8] = b"500000FF03FF000018000404010000TN0001000003";
const TN100_REPLY: &[u8] = b"D00000FF03FF0000100000123400021DEF";
const D0: &[u8] = b"500000FF03FF000018000404010000D*0000000001";
const D0_REPLY: &[u8] = b"D00000FF03FF00000800000007";
const D1: &[u8] = b"500000FF03FF000018000404010000D*0000010001";

fn source(listener: &TcpListener, signals: &Value) -> Value {
    let mut cfg = config(listener, signals);
    cfg["sources"][0]["encoding"] = json!("ascii");
    cfg
}

fn expect(stream: &mut TcpStream, bytes: &[u8]) {
    let mut request = vec![0; bytes.len()];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(request, bytes);
}

fn published(home: &Home) -> Vec<Value> {
    rows(home)
        .into_iter()
        .filter(|row| row["source"] == "plc")
        .collect()
}

#[test]
fn slmp_ascii_replays_vendor_bit_and_timer_data_in_local_3e_frames() {
    let home = Home::new("slmp-ascii-vendor");
    let listener = listener();
    let cfg = source(
        &listener,
        &json!([
            {"name":"bits","device":"M","address":100,"data_type":"bool","elements":8},
            {"name":"timers","device":"TN","address":100,"elements":3}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        expect(&mut stream, M100);
        send(&mut stream, M100_REPLY);
        expect(&mut stream, TN100);
        send(&mut stream, TN100_REPLY);
    });
    record(&home, &cfg).success();
    server.join().unwrap();
    let samples = published(&home);
    assert_eq!(samples.len(), 2);
    assert_eq!(
        samples[0]["value"],
        json!([false, false, false, true, false, false, true, true])
    );
    assert_eq!(samples[1]["value"], json!([0x1234, 2, 0x1def]));
}

#[test]
fn slmp_ascii_iq_r_preserves_decimal_and_hexadecimal_addresses_and_exact_exports() {
    let home = Home::new("slmp-ascii-iq-r");
    let listener = listener();
    let mut cfg = source(
        &listener,
        &json!([
            {"name":"counter","device":"D","address":12_345_678,"data_type":"u64","elements":2},
            {"name":"inputs","device":"X","address":0x1234_abcd,"data_type":"bool","elements":3}
        ]),
    );
    cfg["sources"][0]["device_format"] = json!("iq_r");
    cfg["sources"][0]["network"] = json!(0x12);
    cfg["sources"][0]["station"] = json!(0x34);
    cfg["sources"][0]["module_io"] = json!(0x03e1);
    cfg["sources"][0]["multidrop"] = json!(1);
    cfg["sources"][0]["monitoring_timer"] = json!(0x1234);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        expect(
            &mut stream,
            b"5000123403E101001C123404010002D***123456780008",
        );
        send(
            &mut stream,
            b"D000123403E10100240000FFFFFFFFFFFFFFFF0001000000000020",
        );
        expect(
            &mut stream,
            b"5000123403E101001C123404010003X***1234ABCD0003",
        );
        send(&mut stream, b"D000123403E10100070000101");
    });
    record(&home, &cfg).success();
    server.join().unwrap();
    let samples = published(&home);
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0]["value"], json!([u64::MAX, (1_u64 << 53) + 1]));
    assert_eq!(samples[1]["value"], json!([true, false, true]));
    for (format, output) in [("csv", "out.csv"), ("jsonl", "out.jsonl")] {
        assert_eq!(
            home.run(&[
                "--json",
                "data",
                "export",
                "run.jsonl",
                "--source",
                "plc",
                "--format",
                format,
                "-o",
                output
            ])
            .success()["samples"],
            2
        );
        assert!(
            fs::read_to_string(home.path(output))
                .unwrap()
                .contains("[18446744073709551615,9007199254740993]")
        );
    }
}

#[test]
fn slmp_ascii_rejects_wrong_shape_and_payload_without_publishing_earlier_reads() {
    for (mode, second, diagnostic) in [
        ("route", &b"D00001FF03FF000008"[..], "subheader or route"),
        ("size_digit", b"D00000FF03FF00000G", "invalid hexadecimal"),
        (
            "size",
            b"D00000FF03FF00FFFF",
            "oversized 3E ASCII response length",
        ),
        (
            "status_digit",
            b"D00000FF03FF000008000G0001",
            "invalid hexadecimal",
        ),
        (
            "word_digit",
            b"D00000FF03FF000008000000G1",
            "invalid hexadecimal",
        ),
        (
            "short",
            b"D00000FF03FF0000070000123",
            "value length differs",
        ),
        ("bits", b"D00000FF03FF0000070000120", "contain only 0 or 1"),
        (
            "bad_error",
            b"D00000FF03FF000016C05102FD03FF0004030000",
            "error command differs",
        ),
        (
            "end_code",
            b"D00000FF03FF000016C05102FD03FF0004010000",
            "target rejected read",
        ),
        (
            "status_only",
            b"D00000FF03FF000004C051",
            "target rejected read",
        ),
    ] {
        let home = Home::new("slmp-ascii-malformed");
        let listener = listener();
        let signal = if mode == "bits" {
            json!({"name":"bad","device":"M","address":100,"data_type":"bool","elements":3})
        } else {
            json!({"name":"bad","device":"D","address":1})
        };
        let cfg = source(
            &listener,
            &json!([{"name":"valid","device":"D","address":0},signal]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            expect(&mut stream, D0);
            send(&mut stream, D0_REPLY);
            let expected = if mode == "bits" {
                &b"500000FF03FF000018000404010001M*0001000003"[..]
            } else {
                D1
            };
            expect(&mut stream, expected);
            // Bad headers can be rejected before the body is read.
            stream.write_all(second).unwrap();
        });
        let error = record(&home, &cfg).failure(1);
        server.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        if matches!(mode, "end_code" | "status_only") {
            assert_eq!(error["details"]["slmp_end_code"], 0xc051);
        }
        assert!(published(&home).is_empty(), "{mode}");
        assert_eq!(rows(&home).last().unwrap()["status"], "failed");
    }
}

#[test]
fn slmp_ascii_enforces_point_and_address_extents() {
    for (format, signal, diagnostic) in [
        (
            "standard",
            json!({"device":"M","address":0,"data_type":"bool","elements":3585}),
            "up to 3584 bits",
        ),
        (
            "standard",
            json!({"device":"D","address":0,"elements":961}),
            "1–960 words",
        ),
        (
            "standard",
            json!({"device":"D","address":999_999,"elements":2}),
            "address format",
        ),
        (
            "standard",
            json!({"device":"B","address":0xff_ffff,"data_type":"bool","elements":2}),
            "address format",
        ),
        (
            "iq_r",
            json!({"device":"D","address":99_999_999,"elements":2}),
            "address format",
        ),
        (
            "iq_r",
            json!({"device":"W","address":u32::MAX,"elements":2}),
            "address format",
        ),
    ] {
        let home = Home::new("slmp-ascii-config");
        let listener = listener();
        let mut signal = signal;
        signal["name"] = json!("range");
        let mut cfg = source(&listener, &json!([signal]));
        cfg["sources"][0]["device_format"] = json!(format);
        let error = record(&home, &cfg).failure(2);
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{error}"
        );
        assert!(!home.path("run.jsonl").exists());
    }

    let home = Home::new("slmp-ascii-limits");
    let listener = listener();
    let cfg = source(
        &listener,
        &json!([
            {"name":"bits","device":"M","address":996_416,"data_type":"bool","elements":3584},
            {"name":"words","device":"W","address":0xff_fc40,"elements":960}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        expect(&mut stream, b"500000FF03FF000018000404010001M*9964160E00");
        let bits = [b"D00000FF03FF000E040000".as_slice(), &b"01".repeat(1792)].concat();
        stream.write_all(&bits).unwrap();
        expect(&mut stream, b"500000FF03FF000018000404010000W*FFFC4003C0");
        let words = [b"D00000FF03FF000F040000".as_slice(), &b"ABCD".repeat(960)].concat();
        stream.write_all(&words).unwrap();
    });
    record(&home, &cfg).success();
    server.join().unwrap();
    let samples = published(&home);
    assert_eq!(
        samples[0]["value"],
        json!((0..3584).map(|bit| bit % 2 == 1).collect::<Vec<_>>())
    );
    assert_eq!(samples[1]["value"], json!(vec![0xabcd; 960]));
}

#[test]
fn slmp_ascii_incomplete_sibling_does_not_extend_the_poll_deadline() {
    let home = Home::new("slmp-ascii-deadline");
    let listener = listener();
    let mut cfg = source(
        &listener,
        &json!([
            {"name":"first","device":"D","address":0},
            {"name":"second","device":"D","address":1}
        ]),
    );
    cfg["sources"][0]["timeout_ms"] = json!(time::ms(300));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        expect(&mut stream, D0);
        thread::sleep(time::millis(180));
        send(&mut stream, D0_REPLY);
        expect(&mut stream, D1);
        send(&mut stream, b"D00000FF03FF0000080000");
        thread::sleep(time::millis(180));
        drop(stream.write_all(b"0001"));
    });
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    server.join().unwrap();
    assert_eq!(published(&home), Vec::<Value>::new());
    assert_eq!(rows(&home).last().unwrap()["status"], "failed");
}
