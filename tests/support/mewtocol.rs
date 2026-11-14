//! Panasonic MEWTOCOL-COM reads over the real CLI and native TCP/PTY transports.
//! Known-answer RCS request: Panasonic ACG-M0024-2-1 p. 61.

use super::{Device, Home, accept, assert_unpublished, listener, receive, serial_pair};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    thread,
    time::Duration,
};

const X0: &[u8] = b"%01#RCSX00001D\r";
const X0_REPLY: &[u8] = b"%01$RC021\r";
const DT0: &[u8] = b"%01#RDD000000000055\r";
const DT0_REPLY: &[u8] = b"%01$RD010017\r";
const DT1: &[u8] = b"%01#RDD000010000155\r";

fn source(signals: &Value) -> Value {
    json!({"id":"fp","protocol":"mewtocol","every_ms":3_600_000,
        "timeout_ms":800,"signals":signals})
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

fn values(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample")
        .map(|row| row["value"].clone())
        .collect()
}

fn expect(input: &mut impl Read, expected: &[u8]) {
    let mut actual = vec![0; expected.len()];
    input.read_exact(&mut actual).unwrap();
    assert_eq!(actual, expected);
}

fn send(output: &mut impl Write, frame: &[u8]) {
    for chunk in frame.chunks(5) {
        output.write_all(chunk).unwrap();
    }
}

#[test]
fn mewtocol_tcp_replays_vendor_contact_and_little_endian_register_data() {
    let home = Home::new("mewtocol-tcp");
    let (listener, address) = listener();
    let mut cfg = source(&json!([
        {"name":"contact","area":"X","address":0,"data_type":"bool"},
        {"name":"words","area":"DT","address":0,"elements":2}
    ]));
    cfg["address"] = json!(address.to_string());
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        expect(&mut stream, X0);
        send(&mut stream, X0_REPLY);
        expect(&mut stream, b"%01#RDD000000000154\r");
        send(&mut stream, b"%01$RD05000A0062\r");
        2
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    assert_eq!(server.finish(), 2);
    assert_eq!(values(&home), [json!(false), json!([5, 10])]);
}

#[test]
fn mewtocol_serial_validates_station_and_reads_mixed_radix_contacts() {
    let home = Home::new("mewtocol-serial");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        for (request, reply) in [
            (
                &b"%07#RCP8R000FR0010R0011R0012R0013R0014R0015R001608\r"[..],
                &b"%07$RC0101010117\r"[..],
            ),
            (b"%07#RCSR001717\r", b"%07$RC126\r"),
            (b"%09#RDL001230012650\r", b"%09$RD01000000000020001D\r"),
            (b"%07#RCST987617\r", b"%07$RC027\r"),
        ] {
            let mut actual = vec![0; request.len()];
            assert!(receive(&mut master, &mut actual, &stop));
            assert_eq!(actual, request);
            send(&mut master, reply);
        }
        assert!(!receive(&mut master, &mut [0], &stop));
        4
    });
    let mut cfg = source(&json!([
        {"name":"contacts","area":"R","address":15,"data_type":"bool","elements":9},
        {"name":"counter","unit":9,"area":"LD","address":123,"data_type":"u64"},
        {"name":"timer","area":"T","address":9876,"data_type":"bool"}
    ]));
    cfg["unit"] = json!(7);
    cfg["serial"] = json!({"device":device,"parity":"none"});
    cfg["serial"]["baud"] = serde_json::from_str("9.6e3").unwrap();
    assert_eq!(record(&home, &cfg).success()["samples"], 3);
    assert_eq!(server.finish(), 4);
    assert_eq!(
        values(&home),
        [
            json!([false, true, false, true, false, true, false, true, true]),
            json!((1_u64 << 53) + 1),
            json!(false)
        ]
    );
}

#[test]
fn mewtocol_extended_reads_segmented_arrays_and_preserves_exports() {
    let home = Home::new("mewtocol-extended");
    let (listener, address) = listener();
    let mut cfg = source(&json!([
        {"name":"wide","area":"DT","address":0,"data_type":"u64","elements":128},
        {"name":"file","area":"FL","address":200,"elements":509}
    ]));
    cfg["address"] = json!(address.to_string());
    cfg["unit"] = json!(17);
    cfg["frame"] = json!("extended");
    cfg["request_bcc"] = json!(false);
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        expect(&mut stream, b"<17#RDD0000000507**\r");
        // Even repetitions of F digits cancel in the independent XOR.
        let wide = format!("<17$RD{}08\r", "FFFFFFFFFFFFFFFF".repeat(127));
        stream.write_all(wide.as_bytes()).unwrap();
        expect(&mut stream, b"<17#RDD0050800511**\r");
        send(&mut stream, b"<17$RD01000000000020000B\r");
        expect(&mut stream, b"<17#RDF0020000708**\r");
        let words = format!("<17$RD{}0C\r", "CDAB".repeat(509));
        stream.write_all(words.as_bytes()).unwrap();
        3
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    assert_eq!(server.finish(), 3);
    let mut wide = vec![u64::MAX; 127];
    wide.push((1_u64 << 53) + 1);
    assert_eq!(values(&home), [json!(wide), json!(vec![0xabcd; 509])]);
    for (format, file) in [("csv", "out.csv"), ("jsonl", "out.jsonl")] {
        assert_eq!(
            home.run(&[
                "--json",
                "data",
                "export",
                "run.jsonl",
                "--format",
                format,
                "-o",
                file
            ])
            .success()["samples"],
            2
        );
        let data = fs::read_to_string(home.path(file)).unwrap();
        assert!(data.contains("18446744073709551615,9007199254740993"));
    }
}

#[test]
fn mewtocol_rejects_malformed_sibling_without_publishing_earlier_reads() {
    for (reply, diagnostic, status) in [
        (&b"%02$RD020017\r"[..], "header or station", None),
        (b"%01$RC020013\r", "command or status", None),
        (b"%01$RD020015\r", "reply BCC", None),
        (b"%01$RD0G0061\r", "invalid hexadecimal", None),
        (b"%01$RD0214\r", "value length", None),
        (&[b'A'; 118], "single-frame limit", None),
        (b"%01!4203\r", "PLC rejected", Some(0x42)),
        (b"%01!ZZ05\r", "invalid hexadecimal", None),
        (b"%01$RD0200**\r", "invalid hexadecimal", None),
    ] {
        let home = Home::new("mewtocol-malformed");
        let (listener, address) = listener();
        let mut cfg = source(&json!([
            {"name":"first","area":"DT","address":0},
            {"name":"second","area":"DT","address":1}
        ]));
        cfg["address"] = json!(address.to_string());
        let server = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            expect(&mut stream, DT0);
            send(&mut stream, DT0_REPLY);
            expect(&mut stream, DT1);
            stream.write_all(reply).unwrap();
            2
        });
        let error = record(&home, &cfg).failure(1);
        assert_eq!(server.finish(), 2);
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{error}"
        );
        if let Some(status) = status {
            assert_eq!(error["details"]["mewtocol_error_code"], status);
        }
        assert_unpublished(&home);
    }
}

#[test]
fn mewtocol_partial_poll_uses_original_deadline() {
    let home = Home::new("mewtocol-deadline");
    let (listener, address) = listener();
    let mut cfg = source(&json!([
        {"name":"first","area":"DT","address":0},
        {"name":"second","area":"DT","address":1}
    ]));
    cfg["address"] = json!(address.to_string());
    cfg["timeout_ms"] = json!(350);
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        expect(&mut stream, DT0);
        thread::sleep(Duration::from_millis(200));
        send(&mut stream, DT0_REPLY);
        expect(&mut stream, DT1);
        stream.write_all(b"%01$RD02").unwrap();
        thread::sleep(Duration::from_millis(200));
        drop(stream.write_all(b"0014\r"));
        2
    });
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    assert_eq!(server.finish(), 2);
    assert_unpublished(&home);
}

#[test]
fn mewtocol_validates_device_and_frame_limits_before_network() {
    for (signal, limit, unit) in [
        (
            json!({"name":"value","area":"DT","address":99999,"elements":2}),
            27,
            1,
        ),
        (
            json!({"name":"value","area":"R","address":15999,"data_type":"bool","elements":2}),
            27,
            1,
        ),
        (json!({"name":"value","area":"R","address":0}), 27, 1),
        (
            json!({"name":"value","area":"DT","address":0,"data_type":"u64"}),
            3,
            1,
        ),
        (json!({"name":"value","area":"DT","address":0}), 28, 1),
        (json!({"name":"value","area":"DT","address":0}), 27, 100),
    ] {
        let home = Home::new("mewtocol-config");
        let (_, address) = listener();
        let mut cfg = source(&json!([signal]));
        cfg["address"] = json!(address.to_string());
        cfg["max_words"] = json!(limit);
        cfg["unit"] = json!(unit);
        assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}
