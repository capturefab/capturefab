//! Mitsubishi JY997D45801H sections 7.5 and Appendix C through the CLI.
mod support;

use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;
use support::time;

fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + time::secs(4);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream.set_read_timeout(Some(time::secs(2))).unwrap();
                stream.set_write_timeout(Some(time::secs(2))).unwrap();
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "MC 1E did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn request(stream: &mut TcpStream, expected: &[u8]) {
    let mut bytes = vec![0; expected.len()];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(
        bytes, expected,
        "only the configured MC 1E read is permitted"
    );
}

fn reply(stream: &mut TcpStream, bytes: &[u8]) {
    for chunk in bytes.chunks(3) {
        stream.write_all(chunk).unwrap();
    }
}

fn config(listener: &TcpListener, ascii: bool, signals: &Value) -> Value {
    json!({"version":1,"sources":[{"id":"fx","protocol":"mc_1e",
        "address":listener.local_addr().unwrap().to_string(), "encoding":if ascii {"ascii"} else {"binary"},
        "monitoring_timer":1e1, "every_ms":3_600_000, "timeout_ms":time::ms(1000), "signals":signals}]})
}

fn record(home: &Home, config: &Value) -> support::cli::Run {
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &time::arg(500),
        "-o",
        "run.jsonl",
    ])
}

fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn send_words(stream: &mut TcpStream, ascii: bool, words: &[u16]) {
    if ascii {
        let mut body = String::from("8100");
        for word in words {
            use std::fmt::Write as _;
            write!(body, "{word:04X}").unwrap();
        }
        reply(stream, body.as_bytes());
    } else {
        let mut body = vec![0x81, 0];
        body.extend(words.iter().flat_map(|word| word.to_le_bytes()));
        reply(stream, &body);
    }
}

#[test]
fn binary_and_ascii_follow_mitsubishi_examples_and_preserve_exact_values() {
    for ascii in [false, true] {
        let home = Home::new("mc-1e-values");
        let listener = listener();
        let config = config(
            &listener,
            ascii,
            &json!([
                {"name":"words","device":"D","address":0,"elements":5},
                {"name":"flags","device":"M","address":100,"data_type":"bool","elements":12},
                {"name":"outputs","device":"Y","address":0},
                {"name":"wide","device":"R","address":0,"data_type":"u64","elements":2},
                {"name":"counters","device":"CN","address":220,"data_type":"u32","elements":32},
                {"name":"max_bits","device":"M","address":100,"data_type":"bool","elements":256},
                {"name":"odd_bits","device":"X","address":15,"data_type":"bool","elements":3}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            // Appendix C's D0..D4 read, encoded in both documented modes.
            request(
                &mut stream,
                if ascii {
                    b"01FF000A4420000000000500"
                } else {
                    &[1, 255, 10, 0, 0, 0, 0, 0, 0x20, 0x44, 5, 0]
                },
            );
            send_words(&mut stream, ascii, &[1, 2, 3, 4, 5]);
            // Section 7.5.2 M100 through M111, high-nibble first.
            request(
                &mut stream,
                if ascii {
                    b"00FF000A4D20000000640C00"
                } else {
                    &[0, 255, 10, 0, 100, 0, 0, 0, 0x20, 0x4d, 12, 0]
                },
            );
            reply(
                &mut stream,
                if ascii {
                    b"8000101010101010"
                } else {
                    &[0x80, 0, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10]
                },
            );
            // Section 7.5.5 Y000 through Y017 as one word.
            request(
                &mut stream,
                if ascii {
                    b"01FF000A5920000000000100"
                } else {
                    &[1, 255, 10, 0, 0, 0, 0, 0, 0x20, 0x59, 1, 0]
                },
            );
            send_words(&mut stream, ascii, &[0x0029]);
            request(
                &mut stream,
                if ascii {
                    b"01FF000A5220000000000800"
                } else {
                    &[1, 255, 10, 0, 0, 0, 0, 0, 0x20, 0x52, 8, 0]
                },
            );
            send_words(
                &mut stream,
                ascii,
                &[
                    0xffff, 0xffff, 0xffff, 0xffff, 0xcdef, 0x89ab, 0x4567, 0x0123,
                ],
            );
            // Section 7.5.5 C220..C251: 32 counters occupy 64 words.
            request(
                &mut stream,
                if ascii {
                    b"01FF000A434E000000DC4000"
                } else {
                    &[1, 255, 10, 0, 220, 0, 0, 0, 0x4e, 0x43, 64, 0]
                },
            );
            send_words(&mut stream, ascii, &[0x5678, 0x1234].repeat(32));
            request(
                &mut stream,
                if ascii {
                    b"00FF000A4D20000000640000"
                } else {
                    &[0, 255, 10, 0, 100, 0, 0, 0, 0x20, 0x4d, 0, 0]
                },
            );
            let mut all_bits = if ascii {
                b"8000".to_vec()
            } else {
                vec![0x80, 0]
            };
            all_bits.extend(if ascii {
                b"10".repeat(128)
            } else {
                vec![0x10; 128]
            });
            reply(&mut stream, &all_bits);
            // FX X017 (octal) is address 15. Three bits add one zero nibble.
            request(
                &mut stream,
                if ascii {
                    b"00FF000A58200000000F0300"
                } else {
                    &[0, 255, 10, 0, 15, 0, 0, 0, 0x20, 0x58, 3, 0]
                },
            );
            reply(
                &mut stream,
                if ascii {
                    b"80001010"
                } else {
                    &[0x80, 0, 0x10, 0x10]
                },
            );
        });
        assert_eq!(record(&home, &config).success()["samples"], 7);
        server.join().unwrap();
        let rows = rows(&home);
        assert_eq!(rows[0]["sources"][0]["protocol"], "mc_1e");
        assert_eq!(rows[1]["value"], json!([1, 2, 3, 4, 5]));
        assert_eq!(rows[2]["value"], json!([true, false].repeat(6)));
        assert_eq!(rows[3]["value"], 0x29);
        assert_eq!(
            rows[4]["value"],
            json!([u64::MAX, 0x0123_4567_89ab_cdef_u64])
        );
        assert_eq!(rows[5]["value"], json!(vec![0x1234_5678u32; 32]));
        assert_eq!(rows[6]["value"], json!([true, false].repeat(128)));
        assert_eq!(rows[7]["value"], json!([true, false, true]));
        for format in ["csv", "jsonl"] {
            home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "-o",
                format,
                "--format",
                format,
            ]);
            let exported = fs::read_to_string(home.path(format)).unwrap();
            assert!(exported.contains("[18446744073709551615,81985529216486895]"));
        }
    }
}

#[test]
fn malformed_and_rejected_replies_discard_completed_siblings() {
    let cases: &[(bool, &[u8], &str, &str)] = &[
        (false, &[0x81, 0, 0x10, 0x10], "failed", "subheader differs"),
        (
            false,
            &[0x80, 0, 0x20, 0x10],
            "failed",
            "nibble must be 0 or 1",
        ),
        (false, &[0x80, 0, 0x10, 0x11], "failed", "padding nibble"),
        (false, &[0x80, 0, 0x10], "disconnected", "MC 1E I/O"),
        (
            false,
            &[0x80, 0x5b, 0x12, 0],
            "failed",
            "target rejected read",
        ),
        (false, &[0x80, 0x5b, 0x12, 1], "failed", "reserved byte"),
        (true, b"8G001010", "failed", "non-hexadecimal digit"),
        (true, b"8000+110", "failed", "non-hexadecimal digit"),
        (true, b"80001011", "failed", "padding nibble"),
        (true, b"805b1200", "failed", "target rejected read"),
    ];
    for &(ascii, bytes, code, message) in cases {
        let home = Home::new("mc-1e-invalid");
        let listener = listener();
        let config = config(
            &listener,
            ascii,
            &json!([
                {"name":"first","device":"D","address":0},
                {"name":"second","device":"M","address":100,"data_type":"bool","elements":3}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(
                &mut stream,
                if ascii {
                    b"01FF000A4420000000000100"
                } else {
                    &[1, 255, 10, 0, 0, 0, 0, 0, 0x20, 0x44, 1, 0]
                },
            );
            send_words(&mut stream, ascii, &[42]);
            request(
                &mut stream,
                if ascii {
                    b"00FF000A4D20000000640300"
                } else {
                    &[0, 255, 10, 0, 100, 0, 0, 0, 0x20, 0x4d, 3, 0]
                },
            );
            // An invalid header can close the connection before the last byte.
            let _ = stream.write_all(bytes);
        });
        let error = record(&home, &config).failure(if code == "failed" { 1 } else { 3 });
        assert_eq!(error["code"], code, "{bytes:?}");
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        if message == "target rejected read" {
            assert_eq!(error["details"]["mc_complete_code"], 0x5b);
            assert_eq!(error["details"]["mc_abnormal_code"], 0x12);
        } else {
            assert!(error["details"].get("mc_complete_code").is_none());
        }
        server.join().unwrap();
        let rows = rows(&home);
        assert_eq!(rows.len(), 2, "{bytes:?}");
        assert_eq!(rows[1]["status"], "failed");
    }
}

#[test]
fn split_ascii_replies_and_later_reads_share_one_poll_deadline() {
    let home = Home::new("mc-1e-deadline");
    let listener = listener();
    let mut config = config(
        &listener,
        true,
        &json!([
            {"name":"first","device":"D","address":0}, {"name":"second","device":"D","address":1}
        ]),
    );
    config["sources"][0]["timeout_ms"] = json!(time::ms(240));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        request(&mut stream, b"01FF000A4420000000000100");
        thread::sleep(time::millis(100));
        reply(&mut stream, b"81000001");
        request(&mut stream, b"01FF000A4420000000010100");
        thread::sleep(time::millis(60));
        reply(&mut stream, b"8100");
        thread::sleep(time::millis(130));
        let _ = stream.write_all(b"0002");
    });
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    server.join().unwrap();
    assert_eq!(rows(&home).len(), 2);
}

#[test]
fn invalid_points_addresses_and_data_types_fail_before_connecting() {
    for signal in [
        json!({"name":"bad","device":"D","address":0,"elements":65}),
        json!({"name":"bad","device":"M","address":0,"data_type":"bool","elements":257}),
        json!({"name":"bad","device":"Y","address":1}),
        json!({"name":"bad","device":"M","address":0,"elements":33}),
        json!({"name":"bad","device":"D","address":7999,"elements":2}),
        json!({"name":"bad","device":"M","address":7680,"data_type":"bool"}),
        json!({"name":"bad","device":"D","address":0,"data_type":"bool"}),
        json!({"name":"bad","device":"CN","address":200}),
        json!({"name":"bad","device":"CN","address":199,"elements":2}),
        json!({"name":"bad","device":"CN","address":255,"data_type":"u32","elements":2}),
        json!({"name":"bad","device":"X","address":255,"data_type":"bool","elements":2}),
        json!({"name":"bad","device":"D","address":0,"elements":1.5}),
        json!({"name":"bad","device":"D","address":0,"write":true}),
    ] {
        let home = Home::new("mc-1e-config");
        let listener = listener();
        let config = config(&listener, false, &json!([signal]));
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
