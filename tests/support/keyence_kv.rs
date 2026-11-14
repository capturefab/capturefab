//! Keyence KV Host Link fixtures through the CLI. The first two exchanges
//! are published in the manufacturer's communication manual, printed p. 8-21.

use super::{Device, Home, accept, assert_unpublished, listener};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    thread,
};

fn config(address: std::net::SocketAddr, signals: &Value) -> Value {
    json!({"id":"kv","protocol":"keyence_kv","address":address.to_string(),
        "every_ms":3_600_000,"timeout_ms":time::ms(800),"signals":signals})
}

fn record(home: &Home, cfg: &Value) -> super::support::cli::Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[cfg]}).to_string(),
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

fn expect(stream: &mut impl Read, bytes: &[u8]) {
    let mut actual = vec![0; bytes.len()];
    stream.read_exact(&mut actual).unwrap();
    assert_eq!(actual, bytes);
}

fn send(stream: &mut impl Write, bytes: &[u8]) {
    for part in bytes.chunks(3) {
        stream.write_all(part).unwrap();
    }
}

fn values(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample")
        .map(|row| row["value"].clone())
        .collect()
}

#[test]
fn keyence_kv_replays_published_bit_and_signed_decimal_reads() {
    let home = Home::new("keyence-kv-vendor");
    let (listener, address) = listener();
    let cfg = config(
        address,
        &json!([
            {"name":"bits","device":"R","address":16,"data_type":"bool","elements":4},
            {"name":"signed","device":"DM","address":200,"data_type":"i16","elements":3}
        ]),
    );
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        expect(&mut stream, b"RDS R100 4\r");
        send(&mut stream, b"1 0 1 0\r\n");
        expect(&mut stream, b"RDS DM200.S 3\r");
        send(&mut stream, b"+15025 -25400 +00000\r\n");
        2
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    assert_eq!(server.finish(), 2);
    assert_eq!(
        values(&home),
        [json!([true, false, true, false]), json!([15025, -25400, 0])]
    );
}

#[test]
fn keyence_kv_reads_native_formats_and_exact_raw_word_types_through_exports() {
    let home = Home::new("keyence-kv-types");
    let (listener, address) = listener();
    let mut cfg = config(
        address,
        &json!([
            {"name":"wide","device":"DM","address":0,"data_type":"u64","elements":2},
            {"name":"word_doubles","device":"W","address":16,"data_type":"u32","elements":2},
            {"name":"timers","device":"TC","address":100,"data_type":"i32","elements":3},
            {"name":"float","device":"TM","address":10,"data_type":"f32"},
            {"name":"timer_word","device":"TC","address":101,"data_type":"u16"},
            {"name":"counter_word","device":"CC","address":102,"data_type":"i16"}
        ]),
    );
    cfg["max_points"] = serde_json::from_str("8e0").unwrap();
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        for (request, reply) in [
            (
                &b"RDS DM0.H 8\r"[..],
                &b"FFFF FFFF FFFF FFFF 0001 0000 0000 0020\r\n"[..],
            ),
            (b"RDS W10.D 2\r", b"4294967295 0000000000\r\n"),
            (
                b"RDS TC100.L 3\r",
                b"-2147483648 +2147483647 0000000000\r\n",
            ),
            (b"RDS TM10.H 2\r", b"0000 3FA0\r\n"),
            (b"RD TC101.U\r", b"65535\r\n"),
            (b"RD CC102.S\r", b"-32768\r\n"),
        ] {
            expect(&mut stream, request);
            send(&mut stream, reply);
        }
        6
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 6);
    assert_eq!(server.finish(), 6);
    assert_eq!(
        values(&home),
        [
            json!([u64::MAX, (1_u64 << 53) + 1]),
            json!([u32::MAX, 0]),
            json!([i32::MIN, i32::MAX, 0]),
            json!(1.25),
            json!(u16::MAX),
            json!(i16::MIN)
        ]
    );
    for (format, output) in [("csv", "out.csv"), ("jsonl", "out.jsonl")] {
        assert_eq!(
            home.run(&[
                "--json",
                "data",
                "export",
                "run.jsonl",
                "--format",
                format,
                "-o",
                output
            ])
            .success()["samples"],
            6
        );
        assert!(
            fs::read_to_string(home.path(output))
                .unwrap()
                .contains("[18446744073709551615,9007199254740993]")
        );
    }
}

#[test]
fn keyence_kv_segments_exactly_at_word_limits_and_mixed_radix_carry() {
    let home = Home::new("keyence-kv-segments");
    let (listener, address) = listener();
    let mut cfg = config(
        address,
        &json!([
            {"name":"file","device":"ZF","address":524_280,"data_type":"u64","elements":2},
            {"name":"latches","device":"LR","address":15,"data_type":"bool","elements":6}
        ]),
    );
    cfg["max_points"] = json!(4);
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        for (request, reply) in [
            (&b"RDS ZF524280.H 4\r"[..], &b"0001 0000 0000 0000\r\n"[..]),
            (b"RDS ZF524284.H 4\r", b"0002 0000 0000 0000\r\n"),
            (b"RDS LR15 4\r", b"0 1 0 1\r\n"),
            (b"RDS LR103 2\r", b"1 0\r\n"),
        ] {
            expect(&mut stream, request);
            send(&mut stream, reply);
        }
        4
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    assert_eq!(server.finish(), 4);
    assert_eq!(
        values(&home),
        [
            json!([1, 2]),
            json!([false, true, false, true, true, false])
        ]
    );
}

#[test]
fn keyence_kv_rejects_malformed_or_rejected_siblings_atomically() {
    for (mode, reply, diagnostic) in [
        ("short", &b"042\r\n"[..], "decimal response width"),
        ("overflow", b"65536\r\n", "exceeds 16 bits"),
        ("digit", b"000G2\r\n", "decimal response width"),
        ("separator", b"000 2\r\n", "decimal response width"),
        ("terminator", b"00042\n", "missing CRLF"),
        ("oversized", b"XXXXXXXX", "exceeds"),
        ("bit_error", b"E0\r\n", "PLC rejected read"),
        ("bit_value", b"2\r\n", "bit response"),
        ("hex", b"FFFF 000G\r\n", "invalid hexadecimal"),
        ("nonfinite", b"0000 7F80\r\n", "non-finite"),
        ("signed", b"2147483647\r\n", "leading sign"),
    ] {
        let home = Home::new("keyence-kv-malformed");
        let (listener, address) = listener();
        let (signal, request) = match mode {
            "bit_error" | "bit_value" => (
                json!({"name":"bad","device":"R","address":0,"data_type":"bool"}),
                &b"RD R0\r"[..],
            ),
            "hex" | "nonfinite" => (
                json!({"name":"bad","device":"DM","address":1,"data_type":"f32"}),
                &b"RDS DM1.H 2\r"[..],
            ),
            "signed" => (
                json!({"name":"bad","device":"DM","address":1,"data_type":"i32"}),
                &b"RD DM1.L\r"[..],
            ),
            _ => (
                json!({"name":"bad","device":"DM","address":1}),
                &b"RD DM1.U\r"[..],
            ),
        };
        let cfg = config(
            address,
            &json!([{"name":"good","device":"DM","address":0},signal]),
        );
        let server = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            expect(&mut stream, b"RD DM0.U\r");
            send(&mut stream, b"00042\r\n");
            expect(&mut stream, request);
            stream.write_all(reply).unwrap();
            2
        });
        let error = record(&home, &cfg).failure(1);
        assert_eq!(server.finish(), 2);
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        if mode == "bit_error" {
            assert_eq!(error["details"]["keyence_error_code"], "E0");
        }
        assert_unpublished(&home);
    }
}

#[test]
fn keyence_kv_all_queries_share_original_poll_deadline() {
    let home = Home::new("keyence-kv-deadline");
    let (listener, address) = listener();
    let mut cfg = config(
        address,
        &json!([
            {"name":"first","device":"DM","address":0},
            {"name":"second","device":"DM","address":1}
        ]),
    );
    cfg["timeout_ms"] = json!(time::ms(350));
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        expect(&mut stream, b"RD DM0.U\r");
        thread::sleep(time::millis(200));
        send(&mut stream, b"00042\r\n");
        expect(&mut stream, b"RD DM1.U\r");
        stream.write_all(b"000").unwrap();
        thread::sleep(time::millis(200));
        drop(stream.write_all(b"43\r\n"));
        2
    });
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    assert_eq!(server.finish(), 2);
    assert_unpublished(&home);
}

#[test]
fn keyence_kv_rejects_unrepresentable_device_extents_and_types() {
    for signal in [
        json!({"name":"bad","device":"DM","address":65534,"data_type":"u32"}),
        json!({"name":"bad","device":"MR","address":63999,"data_type":"bool","elements":2}),
        json!({"name":"bad","device":"TM","address":511,"elements":2}),
        json!({"name":"bad","device":"TC","address":3999,"data_type":"u32","elements":2}),
        json!({"name":"bad","device":"TC","address":0,"data_type":"f32"}),
        json!({"name":"bad","device":"DM","address":0,"data_type":"bool"}),
    ] {
        let home = Home::new("keyence-kv-config");
        let (_, address) = listener();
        assert_eq!(
            record(&home, &config(address, &json!([signal]))).failure(2)["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
