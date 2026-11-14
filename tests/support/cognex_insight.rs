//! Cognex Native Mode `GV` through the CLI, using the vendor examples and
//! the independent Robot Raconteur client's login exchange.

use super::{Device, Home, accept, assert_unpublished, listener, receive, support::cli::Run};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write as _,
    net::{SocketAddr, TcpStream},
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

const USER: &str = "cognex-fixture-user";
const PASSWORD: &str = "cognex-fixture-password";
const USER_ENV: &str = "CAPTUREFAB_TEST_COGNEX_USER";
const PASSWORD_ENV: &str = "CAPTUREFAB_TEST_COGNEX_PASSWORD";
const LEGACY: &[u8] = b"Welcome to In-Sight(tm)  IS7802 Session 0\r\nUser: ";
const MODERN: &[u8] = b"Welcome to In-Sight(tm)  IS3800 Session 1\r\nUser:";

fn expect(stream: &mut TcpStream, stop: &AtomicBool, expected: &[u8]) {
    let mut actual = vec![0; expected.len()];
    assert!(receive(stream, &mut actual, stop));
    assert_eq!(actual, expected);
}

fn fragments(stream: &mut TcpStream, bytes: &[u8]) {
    for part in bytes.chunks(4) {
        stream.write_all(part).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
}

fn credentials(stream: &mut TcpStream, stop: &AtomicBool, banner: &[u8], password: &str) {
    fragments(stream, banner);
    expect(stream, stop, format!("{USER}\r\n").as_bytes());
    if banner == LEGACY {
        // The server can echo the username before the unterminated prompt.
        fragments(stream, format!("{USER}\r\nPassword: ").as_bytes());
    } else {
        fragments(stream, b"Password:");
    }
    expect(stream, stop, format!("{password}\r\n").as_bytes());
}

fn login(stream: &mut TcpStream, stop: &AtomicBool, banner: &[u8], password: &str) {
    credentials(stream, stop, banner, password);
    fragments(stream, b"\r\nUser Logged In\r\n");
}

fn source(address: SocketAddr, signals: Value) -> Value {
    let mut config = json!({"id":"vision", "protocol":"cognex_insight", "address":address.to_string(),
        "username_env":USER_ENV, "password_env":PASSWORD_ENV,
        "timeout_ms":time::ms(1500), "every_ms":3_600_000});
    config["signals"] = signals;
    config
}

fn one_signal() -> Value {
    json!([{"name":"width", "cell":"Output", "format":"number"}])
}

fn record(home: &Home, source: &Value, password: &str, duration: &str) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
    home.run_env(
        &[(USER_ENV, USER), (PASSWORD_ENV, password)],
        &[
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            &super::window(duration),
            "-o",
            "run.jsonl",
        ],
    )
}

fn rows(home: &Home, name: &str) -> Vec<Value> {
    fs::read_to_string(home.path(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_no_secret(home: &Home, run: &Run) {
    let recording = fs::read_to_string(home.path("run.jsonl")).unwrap();
    for text in [&run.stdout, &run.stderr, &recording] {
        for secret in [USER, PASSWORD, "private-device-diagnostic"] {
            assert!(
                !text.contains(secret),
                "unexpected disclosure in command output"
            );
        }
    }
}

#[test]
fn cognex_insight_replays_legacy_gv_and_negotiates_telnet_without_enabling_options() {
    let home = Home::new("cognex-legacy");
    let (listener, address) = listener();
    let camera = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        // RFC 854 options split across reads. Neither echo nor binary is
        // needed for the standard Native Mode line protocol.
        stream.write_all(b"\xff").unwrap();
        thread::sleep(Duration::from_millis(3));
        stream.write_all(b"\xfb\x01\xff\xfd\x00").unwrap();
        expect(&mut stream, &stop, b"\xff\xfe\x01\xff\xfc\x00");
        fragments(&mut stream, b"\xff\xfa\x18User:\xff\xff\xff\xf0");
        login(&mut stream, &stop, LEGACY, PASSWORD);
        expect(&mut stream, &stop, b"GVA001\r\n");
        // Explorer Get Value example, with one interleaved Telnet NOP.
        fragments(&mut stream, b"1\r\n4.\xff\xf1000\r\n");
        expect(&mut stream, &stop, b"GVJob.Robot.FormatString\r\n");
        fragments(&mut stream, b"1\r\n  PASS,0042  \r\n");
        assert!(!receive(&mut stream, &mut [0], &stop));
        2
    });
    let cfg = source(
        address,
        json!([
            {"name":"width", "cell":"A001", "format":"number"},
            {"name":"formatted", "cell":"Job.Robot.FormatString"}
        ]),
    );
    let run = record(&home, &cfg, PASSWORD, "800ms");
    assert_eq!(run.success()["samples"], 2);
    assert_eq!(camera.finish(), 2);
    let recording = rows(&home, "run.jsonl");
    assert_eq!(recording[0]["sources"][0]["protocol"], "cognex_insight");
    assert_eq!(
        recording[1]["value"],
        serde_json::from_str::<Value>(
            r#"{
        "cell":"A001","command_status":1,"state":"available","text":"4.000","number":4.000
    }"#
        )
        .unwrap()
    );
    assert_eq!(
        recording[2]["value"],
        json!({"cell":"Job.Robot.FormatString", "command_status":1,
        "state":"available", "text":"  PASS,0042  "})
    );
    assert_no_secret(&home, &run);
    home.ok(&["data", "export", "run.jsonl", "-o", "out.csv"]);
    let csv = fs::read_to_string(home.path("out.csv")).unwrap();
    assert!(csv.contains("4.000") && csv.contains("  PASS,0042  "));
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "out.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported = rows(&home, "out.jsonl");
    assert_eq!(exported.len(), 2);
    for (output, sample) in exported.iter().zip(recording.into_iter().skip(1).take(2)) {
        let mut expected = sample;
        expected.as_object_mut().unwrap().remove("kind");
        assert_eq!(*output, expected);
    }
}

#[test]
fn cognex_insight_reads_published_values_with_blank_password_and_preserves_empty_marker_and_bytes()
{
    let home = Home::new("cognex-published");
    let (listener, address) = listener();
    let camera = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        login(&mut stream, &stop, MODERN, "");
        for output in [
            &b"1\r\n15.000\r\n"[..],
            &b"1\r\n9007199254740993.001\r\n"[..],
        ] {
            expect(&mut stream, &stop, b"GVOutput\r\n");
            fragments(&mut stream, output);
            expect(&mut stream, &stop, b"GVEmptyCell\r\n");
            fragments(&mut stream, b"1\r\n\r\n");
            expect(&mut stream, &stop, b"GVErrorCell\r\n");
            fragments(&mut stream, b"1\r\n#ERR\r\n");
            expect(&mut stream, &stop, b"GVByteCell\r\n");
            fragments(&mut stream, b"1\r\n\x81\xff\xff\r\n");
        }
        assert!(!receive(&mut stream, &mut [0], &stop));
        8
    });
    let mut cfg = source(
        address,
        json!([
            {"name":"published", "cell":"Output", "format":"number"},
            {"name":"blank", "cell":"EmptyCell", "format":"number"},
            {"name":"marker", "cell":"ErrorCell", "format":"number"},
            {"name":"raw", "cell":"ByteCell"}
        ]),
    );
    cfg["every_ms"] = json!(time::ms(400));
    let result = record(&home, &cfg, "", "720ms").success();
    assert_eq!(camera.finish(), 8);
    assert_eq!(result["samples"], 8);
    let recording = rows(&home, "run.jsonl");
    for (at, exact) in [(1, "15.000"), (5, "9007199254740993.001")] {
        assert_eq!(recording[at]["value"]["number"].to_string(), exact);
        assert_eq!(recording[at]["value"]["text"], exact);
        assert_eq!(
            recording[at + 1]["value"],
            json!({"cell":"EmptyCell","command_status":1,"state":"empty","text":""})
        );
        assert_eq!(
            recording[at + 2]["value"],
            json!({"cell":"ErrorCell","command_status":1,"state":"device_marker","text":"#ERR"})
        );
        assert_eq!(
            recording[at + 3]["value"],
            json!({"cell":"ByteCell","command_status":1,"state":"available","raw":{"encoding":"base64","data":"gf8="}})
        );
    }
}

#[test]
fn cognex_insight_authentication_failure_does_not_expose_echo_or_diagnostics() {
    let home = Home::new("cognex-auth-rejected");
    let (listener, address) = listener();
    let camera = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        credentials(&mut stream, &stop, MODERN, PASSWORD);
        let reject = format!("private-device-diagnostic: {USER}/{PASSWORD}\r\n");
        stream.write_all(reject.as_bytes()).unwrap();
        assert!(!receive(&mut stream, &mut [0], &stop));
        0
    });
    let run = record(&home, &source(address, one_signal()), PASSWORD, "600ms");
    assert_eq!(run.failure(3)["code"], "auth_rejected");
    assert_eq!(camera.finish(), 0);
    assert_unpublished(&home);
    assert_no_secret(&home, &run);
}

#[test]
fn cognex_insight_rejects_unsupported_and_wrong_peer_banners_before_sending_credentials() {
    for (banner, exit, message) in [
        (
            &b"Welcome to In-Sight(tm)  IS2000-120 Session 0\r\nUser: "[..],
            5,
            "requires Serial Native",
        ),
        (
            &b"Wrong peer private-device-diagnostic\r\nUser: "[..],
            1,
            "expected Cognex In-Sight welcome",
        ),
    ] {
        let home = Home::new("cognex-peer");
        let (listener, address) = listener();
        let camera = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            stream.write_all(banner).unwrap();
            assert!(!receive(&mut stream, &mut [0], &stop));
            0
        });
        let run = record(&home, &source(address, one_signal()), PASSWORD, "600ms");
        assert!(
            run.failure(exit)["message"]
                .as_str()
                .unwrap()
                .contains(message)
        );
        assert_eq!(camera.finish(), 0);
        assert_unpublished(&home);
        assert_no_secret(&home, &run);
    }
}

#[test]
fn cognex_insight_a_bad_sibling_status_or_value_keeps_the_whole_poll_unpublished() {
    for (reply, message, native_status) in [
        (&b"-1\r\n"[..], "GV was rejected", Some(-1)),
        (
            &b"private-device-diagnostic\r\n"[..],
            "GV line exceeds",
            None,
        ),
        (
            &b"1\r\nprivate-device-diagnostic\r\n"[..],
            "exact JSON number",
            None,
        ),
        (&b"1\n"[..], "require CRLF", None),
    ] {
        let home = Home::new("cognex-gv-reject");
        let (listener, address) = listener();
        let camera = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            login(&mut stream, &stop, MODERN, PASSWORD);
            expect(&mut stream, &stop, b"GVOutput\r\n");
            stream.write_all(b"1\r\n15.000\r\n").unwrap();
            expect(&mut stream, &stop, b"GVOther\r\n");
            stream.write_all(reply).unwrap();
            2
        });
        let cfg = source(
            address,
            json!([
                {"name":"first", "cell":"Output", "format":"number"},
                {"name":"second", "cell":"Other", "format":"number"}
            ]),
        );
        let run = record(&home, &cfg, PASSWORD, "600ms");
        let error = run.failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        assert_eq!(error["details"]["native_status"].as_i64(), native_status);
        assert_eq!(camera.finish(), 2);
        assert_unpublished(&home);
        assert_no_secret(&home, &run);
    }
}

#[test]
fn cognex_insight_login_and_fragmented_value_share_one_deadline() {
    let home = Home::new("cognex-deadline");
    let (listener, address) = listener();
    let camera = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        stream.write_all(MODERN).unwrap();
        expect(&mut stream, &stop, format!("{USER}\r\n").as_bytes());
        thread::sleep(time::millis(240));
        stream.write_all(b"Password:").unwrap();
        expect(&mut stream, &stop, format!("{PASSWORD}\r\n").as_bytes());
        stream.write_all(b"User Logged In\r\n").unwrap();
        expect(&mut stream, &stop, b"GVOutput\r\n");
        stream.write_all(b"1\r\n15.").unwrap();
        thread::sleep(time::millis(240));
        let _ = stream.write_all(b"000\r\n");
        1
    });
    let mut cfg = source(address, one_signal());
    cfg["timeout_ms"] = json!(time::ms(400));
    let run = record(&home, &cfg, PASSWORD, "1s");
    assert_eq!(run.failure(4)["code"], "timeout");
    assert_eq!(camera.finish(), 1);
    assert_unpublished(&home);
    assert_no_secret(&home, &run);
}

#[test]
fn cognex_insight_bounds_telnet_control_traffic_and_incremental_json() {
    let home = Home::new("cognex-telnet-limit");
    let (telnet_listener, address) = listener();
    let camera = Device::start(move |stop| {
        let mut stream = accept(&telnet_listener, &stop).unwrap();
        stream.write_all(&[255, 241].repeat(513)).unwrap();
        0
    });
    let run = record(&home, &source(address, one_signal()), PASSWORD, "600ms");
    assert!(
        run.failure(1)["message"]
            .as_str()
            .unwrap()
            .contains("Telnet control data exceeds")
    );
    assert_eq!(camera.finish(), 0);
    assert_unpublished(&home);

    let home = Home::new("cognex-json-limit");
    let (listener, address) = listener();
    let camera = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        login(&mut stream, &stop, MODERN, PASSWORD);
        let reply = format!("1\r\n{}\r\n", "x".repeat(16 * 1024));
        // 63 readings fit. The 64th crosses 1 MiB including its metadata.
        for _ in 0..64 {
            expect(&mut stream, &stop, b"GVOutput\r\n");
            stream.write_all(reply.as_bytes()).unwrap();
        }
        assert!(!receive(&mut stream, &mut [0], &stop));
        64
    });
    let mut cfg = source(
        address,
        json!(
            (0..70)
                .map(|index| json!({"name":format!("value{index:02}"), "cell":"Output"}))
                .collect::<Vec<_>>()
        ),
    );
    cfg["timeout_ms"] = json!(time::ms(2500));
    let run = record(&home, &cfg, PASSWORD, "2s");
    assert!(
        run.failure(1)["message"]
            .as_str()
            .unwrap()
            .contains("serialized poll exceeds")
    );
    assert_eq!(camera.finish(), 64);
    assert_unpublished(&home);
}

#[test]
fn cognex_insight_rejects_command_and_credential_injection_before_connecting() {
    let (listener, address) = listener();
    for (cell, password, message) in [
        ("Output\r\nRT", PASSWORD, "GV cell"),
        ("A400", PASSWORD, "GV cell"),
        (
            "Output",
            "private-device-diagnostic\r\nRT",
            "without control characters",
        ),
    ] {
        let home = Home::new("cognex-config");
        let cfg = source(address, json!([{"name":"width", "cell":cell}]));
        let run = record(&home, &cfg, password, "600ms");
        assert!(
            run.failure(2)["message"]
                .as_str()
                .unwrap()
                .contains(message)
        );
        assert!(!run.stdout.contains("private-device-diagnostic"));
        assert!(!run.stderr.contains("private-device-diagnostic"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
