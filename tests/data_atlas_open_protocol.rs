//! Manufacturer-table literals through the real CLI; no physical controller claim.
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

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/atlas_open_protocol/pf6000.json")).unwrap()
}
fn frame(group: &str, name: &str) -> Vec<u8> {
    let corpus = corpus();
    let hex = corpus[group]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap()["wire_hex"]
        .as_str()
        .unwrap();
    let (pairs, tail) = hex.as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream.set_nodelay(true).unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(2));
            }
            other => panic!("private PF6000 accept: {other:?}"),
        }
    }
}
fn expect(stream: &mut TcpStream, name: &str) {
    let expected = frame("outbound_frames", name);
    let mut actual = vec![0; expected.len()];
    stream.read_exact(&mut actual).unwrap();
    assert_eq!(actual, expected, "{name}");
}
fn closed(stream: &mut TcpStream) {
    let mut byte = [0];
    assert_eq!(
        stream.read(&mut byte).unwrap(),
        0,
        "no extra controller commands"
    );
}
fn setup(stream: &mut TcpStream) {
    expect(stream, "startup_request");
    stream
        .write_all(&frame("valid_startups", "startup_005"))
        .unwrap();
    expect(stream, "subscribe_result");
}
fn configure(home: &Home, address: &str) {
    fs::write(
        home.path("sources.json"),
        serde_json::to_vec(&json!({
            "version":1,"sources":[{"id":"tightener","protocol":"atlas_open_protocol",
                "profile":"pf6000_v2_8","address":address,"timeout_ms":1000}]
        }))
        .unwrap(),
    )
    .unwrap();
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn args() -> [&'static str; 8] {
    [
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "500ms",
        "-o",
        "run.jsonl",
    ]
}

#[test]
fn fragmented_startup_and_coalesced_duplicate_events_are_saved_before_each_ack() {
    let home = Home::new("atlas-checkpoint-duplicates");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    configure(&home, &listener.local_addr().unwrap().to_string());
    let saved = home.path("run.jsonl");
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        expect(&mut stream, "startup_request");
        for byte in frame("valid_startups", "startup_005") {
            stream.write_all(&[byte]).unwrap();
        }
        expect(&mut stream, "subscribe_result");
        let result = frame("valid_results", "result_003");
        stream
            .write_all(
                &[
                    frame("control_frames", "accepted_subscribe_zero_alias"),
                    result.clone(),
                    result,
                ]
                .concat(),
            )
            .unwrap();
        for minimum in [1, 2] {
            expect(&mut stream, "result_ack");
            let data = fs::read_to_string(&saved).unwrap();
            assert!(data.ends_with('\n'));
            let records = data
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert!(records.iter().filter(|row| row["kind"] == "sample").count() >= minimum);
        }
        expect(&mut stream, "unsubscribe");
        stream
            .write_all(&frame("control_frames", "accepted_unsubscribe_blank_alias"))
            .unwrap();
        closed(&mut stream);
    });
    let result = home.ok(&args());
    peer.join().unwrap();
    assert_eq!(result["samples"], 2);
    let records = rows(&home);
    let samples = records
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    let [first, second] = samples.as_slice() else {
        panic!("both duplicate wire events must remain")
    };
    assert_eq!(
        first["value"],
        corpus()["valid_results"][0]["expected_value"]
    );
    assert_eq!(second["value"], first["value"]);
    assert_eq!(first["seq"], 1);
    assert_eq!(second["seq"], 2);
    assert!(first.get("source_time").is_none());
    assert!(first["elapsed_ns"].as_u64().unwrap() <= second["elapsed_ns"].as_u64().unwrap());
    assert_eq!(records.last().unwrap()["status"], "complete");
    let exported = home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    assert_eq!(exported["samples"], 2);
    assert!(
        fs::read_to_string(home.path("values.csv"))
            .unwrap()
            .contains("23459")
    );
}

#[test]
fn idle_subscription_becomes_ready_and_unsubscribes_without_event_ack() {
    let home = Home::new("atlas-idle");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    configure(&home, &listener.local_addr().unwrap().to_string());
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        setup(&mut stream);
        stream
            .write_all(&frame("control_frames", "accepted_subscribe_one"))
            .unwrap();
        expect(&mut stream, "unsubscribe");
        stream
            .write_all(&frame("control_frames", "accepted_unsubscribe_one"))
            .unwrap();
        closed(&mut stream);
    });
    let result = home.ok(&args());
    peer.join().unwrap();
    assert_eq!(result["samples"], 0);
    assert_eq!(rows(&home).last().unwrap()["status"], "complete");
}

#[test]
fn malformed_events_stop_without_ack_and_preserve_the_preceding_saved_event() {
    for name in [
        "result_no_ack_one",
        "result_bad_supported_id",
        "native_id_overflow",
        "result_non_ascii_opaque_span",
    ] {
        let home = Home::new("atlas-malformed");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        configure(&home, &listener.local_addr().unwrap().to_string());
        let peer = thread::spawn(move || {
            let mut stream = accept(&listener);
            setup(&mut stream);
            stream
                .write_all(
                    &[
                        frame("control_frames", "accepted_subscribe_one"),
                        frame("valid_results", "result_003"),
                    ]
                    .concat(),
                )
                .unwrap();
            expect(&mut stream, "result_ack");
            stream.write_all(&frame("malformed_frames", name)).unwrap();
            closed(&mut stream);
        });
        let error = home.fails(1, &args());
        peer.join().unwrap();
        assert_eq!(error["code"], "incomplete");
        assert_eq!(error["details"]["samples"], 1);
        let records = rows(&home);
        assert_eq!(records.last().unwrap()["status"], "failed");
        assert_eq!(
            records.iter().filter(|row| row["kind"] == "sample").count(),
            1
        );
        let exported = home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--allow-incomplete",
            "-o",
            "partial.csv",
        ]);
        assert_eq!(exported["samples"], 1);
    }
}

#[test]
fn native_subscription_errors_and_wrong_correlations_keep_their_actual_cause() {
    for (reply, kind) in [
        ("error_duplicate_subscription_one", "failed"),
        ("accepted_unsubscribe_one", "incomplete"),
    ] {
        let home = Home::new("atlas-control-error");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        configure(&home, &listener.local_addr().unwrap().to_string());
        let peer = thread::spawn(move || {
            let mut stream = accept(&listener);
            setup(&mut stream);
            stream.write_all(&frame("control_frames", reply)).unwrap();
            closed(&mut stream);
        });
        let error = home.fails(1, &args());
        peer.join().unwrap();
        assert_eq!(error["code"], kind);
        if kind == "failed" {
            assert_eq!(error["details"]["failed_mid"], 60);
            assert_eq!(error["details"]["error_code_raw"], "09");
        }
        assert_eq!(error["details"]["samples"], 0);
    }
}

#[test]
fn partial_frame_timeout_and_unsubscribe_race_never_send_a_result_ack() {
    for late in [false, true] {
        let home = Home::new("atlas-shutdown");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        configure(&home, &listener.local_addr().unwrap().to_string());
        let peer = thread::spawn(move || {
            let mut stream = accept(&listener);
            setup(&mut stream);
            stream
                .write_all(&frame("control_frames", "accepted_subscribe_one"))
                .unwrap();
            if late {
                expect(&mut stream, "unsubscribe");
                stream
                    .write_all(&frame("valid_results", "result_003"))
                    .unwrap();
            } else {
                stream.write_all(b"041").unwrap();
            }
            closed(&mut stream);
        });
        let error = home.fails(if late { 1 } else { 4 }, &args());
        peer.join().unwrap();
        assert_eq!(error["code"], if late { "incomplete" } else { "timeout" });
        assert_eq!(error["details"]["samples"], 0);
        if late {
            assert_eq!(
                error["details"]["atlas_open_protocol_frame"]["raw_hex"],
                corpus()["valid_results"][0]["wire_hex"]
            );
        }
        assert_eq!(rows(&home).last().unwrap()["status"], "failed");
    }
}

#[test]
fn invalid_selectors_fail_before_creating_output_or_connecting() {
    let home = Home::new("atlas-config-preflight");
    for value in [
        json!({"profile":"pf6000_v2_8","address":"255.255.255.255:4545"}),
        json!({"profile":"future","address":"127.0.0.1:4545"}),
        json!({"profile":"pf6000_v2_8","address":"127.0.0.1:4545","command":"0060"}),
    ] {
        let mut source = value;
        source["id"] = json!("tightener");
        source["protocol"] = json!("atlas_open_protocol");
        fs::write(
            home.path("sources.json"),
            serde_json::to_vec(&json!({"version":1,"sources":[source]})).unwrap(),
        )
        .unwrap();
        let error = home.fails(2, &args());
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}
