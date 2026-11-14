//! Original JSON numeric precision survives acquisition, recording and both exports.
#![cfg(unix)]
mod support;

use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

const VALUES: &str = "[18446744073709551617,-18446744073709551617,0.123456789012345678901234567890123456789,1.234567890123456789e+100]";

fn assert_recording_and_exports(home: &Home) {
    let result = home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "250ms",
        "-o",
        "run.jsonl",
    ]);
    assert_eq!(result["samples"], 1);
    assert_eq!(result["status"], "complete");
    let rows: Vec<Value> = fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[1]["value"].to_string(), VALUES);
    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert!(csv.contains(VALUES), "{csv}");
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Value =
        serde_json::from_str(&fs::read_to_string(home.path("values.jsonl")).unwrap()).unwrap();
    assert_eq!(exported["value"].to_string(), VALUES);
}

#[test]
fn helper_json_numbers_survive_recording_and_exports_without_rounding() {
    let home = Home::new("data-helper-precision");
    let program = format!("printf '%s\\n' '{{\"signal\":\"count\",\"value\":{VALUES}}}'; sleep 30");
    fs::write(
        home.path("sources.json"),
        format!(r#"{{"version":1.0,"sources":[{{"id":"counts","protocol":"opc_ua","executable":"/bin/sh","args":["-c",{}],"every_ms":1e2,"timeout_ms":1e3}}]}}"#, json!(program)),
    )
    .unwrap();
    assert_recording_and_exports(&home);
}

fn packet(stream: &mut TcpStream) -> u8 {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut header = [0];
    stream.read_exact(&mut header).unwrap();
    let mut remaining = 0usize;
    let mut multiplier = 1;
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        remaining += usize::from(byte[0] & 0x7f) * multiplier;
        if byte[0] < 128 {
            break;
        }
        multiplier *= 128;
        assert!(multiplier <= 128 * 128 * 128);
    }
    assert!(remaining <= 2048);
    let mut body = vec![0; remaining];
    stream.read_exact(&mut body).unwrap();
    header[0]
}

#[test]
fn mqtt_json_numbers_survive_recording_and_exports_without_rounding() {
    let home = Home::new("data-mqtt-precision");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let broker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "recorder did not connect");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        assert_eq!(packet(&mut stream), 0x10);
        stream.write_all(&[0x20, 2, 0, 0]).unwrap();
        assert_eq!(packet(&mut stream), 0x82);
        stream.write_all(&[0x90, 3, 0, 1, 1]).unwrap();
        let mut body = vec![0, 10];
        body.extend_from_slice(b"line/count");
        body.extend_from_slice(VALUES.as_bytes());
        let mut header = vec![0x30];
        let mut remaining = body.len();
        loop {
            let byte = u8::try_from(remaining % 128).unwrap();
            remaining /= 128;
            header.push(byte | if remaining == 0 { 0 } else { 0x80 });
            if remaining == 0 {
                break;
            }
        }
        stream.write_all(&header).unwrap();
        stream.write_all(&body).unwrap();
        thread::sleep(Duration::from_millis(500));
    });
    fs::write(
        home.path("sources.json"),
        format!(r#"{{"version":1e0,"sources":[{{"id":"counts","protocol":"mqtt","broker":"{address}","topics":["line/count"],"qos":1.0,"every_ms":1.0e2,"timeout_ms":1e3}}]}}"#),
    )
    .unwrap();
    assert_recording_and_exports(&home);
    broker.join().unwrap();
}
