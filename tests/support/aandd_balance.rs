//! Manufacturer literals and independently declared standard-format controls.
//! This exercises native TCP/PTY capture, not a physical A&D balance.
use super::{
    Device, Home, accept, assert_unpublished, listener, receive, serial_pair, support::cli::Run,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write as _},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

fn cases() -> Vec<Value> {
    serde_json::from_str::<Value>(include_str!("../fixtures/aandd/standard-replies.json")).unwrap()
        ["cases"]
        .as_array()
        .unwrap()
        .clone()
}
fn query(port: &mut impl Read, stop: &AtomicBool, cr: bool) {
    if cr {
        let mut bytes = [0; 2];
        assert!(receive(port, &mut bytes, stop));
        assert_eq!(&bytes, b"Q\r");
    } else {
        let mut bytes = [0; 3];
        assert!(receive(port, &mut bytes, stop));
        assert_eq!(&bytes, b"Q\r\n");
    }
}
fn no_more(port: &mut impl Read, stop: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match port.read(&mut [0; 1]) {
            Ok(0) => return,
            Ok(_) => panic!("unexpected command after A&D transaction"),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                return;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                assert!(Instant::now() < deadline, "A&D connection did not close");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("A&D fixture read: {error}"),
        }
    }
}
fn configured(mut source: Value) -> Value {
    source["id"] = json!("balance");
    source["protocol"] = json!("aandd_balance");
    if source.get("every_ms").is_none() {
        source["every_ms"] = json!(3_600_000);
    }
    source
}
fn record_sources(home: &Home, sources: &[Value], duration: &str) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        duration,
        "-o",
        "run.jsonl",
    ])
}
fn record(home: &Home, source: Value, duration: &str) -> Run {
    record_sources(home, &[configured(source)], duration)
}
fn rows(home: &Home, name: &str) -> Vec<Value> {
    fs::read_to_string(home.path(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn balance_values(home: &Home, name: &str) -> Vec<Value> {
    rows(home, name)
        .into_iter()
        .filter(|row| row["source"] == "balance")
        .map(|row| row["value"].clone())
        .collect()
}
fn assert_samples(home: &Home, expected: &[Value], complete: bool, total: usize) {
    assert_eq!(balance_values(home, "run.jsonl"), expected);
    let recorded = rows(home, "run.jsonl");
    let end = recorded.last().unwrap();
    assert_eq!(end["status"], if complete { "complete" } else { "failed" });
    assert_eq!(end["samples"], total);
    for sample in recorded.iter().filter(|row| row["kind"] == "sample") {
        assert!(sample.get("source_time").is_none());
        assert!(sample["elapsed_ns"].is_u64());
        assert!(sample["read_duration_ns"].is_u64());
        assert!(sample["delivery_latency_ns"].is_u64());
    }
    if !complete {
        assert_eq!(
            home.fails(1, &["data", "export", "run.jsonl", "-o", "refused.csv"])["code"],
            "incomplete"
        );
        assert!(!home.path("refused.csv").exists());
    }
    for format in ["jsonl", "csv"] {
        let mut args = vec![
            "--json",
            "data",
            "export",
            "run.jsonl",
            "-o",
            format,
            "--format",
            format,
        ];
        if !complete {
            args.push("--allow-incomplete");
        }
        assert_eq!(home.run(&args).success()["samples"], total);
    }
    assert_eq!(balance_values(home, "jsonl"), expected);
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json"
    );
    let lines = lines.collect::<Vec<_>>();
    assert_eq!(lines.len(), total);
    let values = lines
        .iter()
        .filter(|line| line.split(',').nth(1) == Some("\"balance\""))
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    let quoted = expected
        .iter()
        .map(|value| format!("\"{}\"", value.to_string().replace('"', "\"\"")))
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        quoted.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[test]
fn aandd_all_literal_statuses_keep_exact_values_and_exports_over_tcp_and_serial() {
    for (index, case) in cases().into_iter().enumerate() {
        for serial in [false, true] {
            let home = Home::new(&format!("aandd-{index}-{serial}"));
            let cr = case["terminator"] == "cr";
            let frame = case["frame"].as_str().unwrap().as_bytes().to_vec();
            let (device, source) = if serial {
                let (mut master, slave, path) = serial_pair();
                let device = Device::start(move |stop| {
                    let _slave = slave;
                    query(&mut master, &stop, cr);
                    for byte in frame {
                        master.write_all(&[byte]).unwrap();
                    }
                    no_more(&mut master, &stop);
                    1_u64
                });
                (
                    device,
                    json!({"serial":{"device":path,"baud":9.6e3,"parity":"none","data_bits":8},
                    "terminator":if cr {"cr"} else {"crlf"}}),
                )
            } else {
                let (listen, address) = listener();
                let device = Device::start(move |stop| {
                    let mut port = accept(&listen, &stop).unwrap();
                    query(&mut port, &stop, cr);
                    for byte in frame {
                        port.write_all(&[byte]).unwrap();
                    }
                    no_more(&mut port, &stop);
                    1_u64
                });
                (
                    device,
                    json!({"address":address,"terminator":if cr {"cr"} else {"crlf"}}),
                )
            };
            assert_eq!(record(&home, source, "300ms").success()["samples"], 1);
            assert_eq!(device.finish(), 1);
            let expected = case["expected"].clone();
            if let Some(number) = expected.get("value") {
                let saved = balance_values(&home, "run.jsonl");
                assert_eq!(
                    saved.first().unwrap()["value"].to_string(),
                    number.to_string()
                );
            }
            let samples = rows(&home, "run.jsonl");
            let sample = samples.iter().find(|row| row["kind"] == "sample").unwrap();
            assert_eq!(sample["signal"], "measurement");
            assert_samples(&home, &[expected], true, 1);
        }
    }
}

#[test]
fn aandd_mixed_source_and_multiple_records_keep_same_owner_and_custom_signal() {
    let home = Home::new("aandd-mixed");
    let (listen, address) = listener();
    let device = Device::start(move |stop| {
        let mut port = accept(&listen, &stop).unwrap();
        query(&mut port, &stop, false);
        port.write_all(b"ST,+03142.06  g\r\n").unwrap();
        query(&mut port, &stop, false);
        port.write_all(b"EC,E02\r\n").unwrap();
        query(&mut port, &stop, false);
        port.write_all(b"OL,-9999999E+19\r\n").unwrap();
        no_more(&mut port, &stop);
        3_u64
    });
    let source = configured(json!({"address":address,"every_ms":200,"signal":"gross_observation"}));
    let sources = [
        source,
        json!({"id":"counter","protocol":"simulator","signal":"cycle","every_ms":3_600_000}),
    ];
    assert_eq!(
        record_sources(&home, &sources, "550ms").success()["samples"],
        4
    );
    assert_eq!(device.finish(), 3);
    let literals = cases();
    let expected = [0, 8, 3].map(|index| literals[index]["expected"].clone());
    assert_samples(&home, &expected, true, 4);
    let samples = rows(&home, "run.jsonl");
    assert!(
        samples
            .iter()
            .filter(|row| row["source"] == "balance")
            .all(|row| row["signal"] == "gross_observation")
    );
    let counter = samples
        .iter()
        .find(|row| row["source"] == "counter")
        .unwrap();
    assert_eq!(counter["value"], 0);
}

#[test]
fn aandd_invalid_reply_never_publishes_partial_value_or_retries_and_keeps_prior_poll() {
    let invalid: &[(&[u8], i32, &str)] = &[
        (b"ST,+03142.06 g\r\n", 1, "failed"),
        (b"ST,+03142.06  g\n", 1, "failed"),
        (b"ST,+03142.06  g\rX", 1, "failed"),
        (b"ST,+03142.06  g\r\nST,+03142.06  g\r\n", 1, "failed"),
        (b"EC,E02\r\nX", 1, "failed"),
        (b"EC,E2\r\n", 1, "failed"),
        (b"EC,E0X\r\n", 1, "failed"),
        (b"ST, 03142.06  g\r\n", 1, "failed"),
        (b"ST,+0314E.06  g\r\n", 1, "failed"),
        (b"ST,+03142.06\tg\r\n", 1, "failed"),
        (b"OL,+1234567E+19\r\n", 1, "failed"),
        (b"\x06\r\n", 1, "failed"),
        (b"XX,+03142.06  g\r\n", 5, "unsupported"),
        (b"ST,+03142.06", 3, "disconnected"),
    ];
    for (index, &(bad, exit, code)) in invalid.iter().enumerate() {
        for prefix in [false, true] {
            let home = Home::new(&format!("aandd-bad-{index}-{prefix}"));
            let (listen, address) = listener();
            let device = Device::start(move |stop| {
                let mut port = accept(&listen, &stop).unwrap();
                if prefix {
                    query(&mut port, &stop, false);
                    port.write_all(b"ST,+03142.06  g\r\n").unwrap();
                }
                query(&mut port, &stop, false);
                let _ = port.write_all(bad);
                if let Err(error) = port.shutdown(std::net::Shutdown::Write) {
                    assert!(
                        matches!(
                            error.kind(),
                            io::ErrorKind::NotConnected
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::BrokenPipe
                        ),
                        "A&D fixture shutdown: {error}"
                    );
                }
                no_more(&mut port, &stop);
                1 + u64::from(prefix)
            });
            assert_eq!(
                record(&home, json!({"address":address,"every_ms":100}), "1s").failure(exit)["code"],
                code
            );
            assert_eq!(device.finish(), 1 + u64::from(prefix));
            let expected = if prefix {
                vec![cases().first().unwrap()["expected"].clone()]
            } else {
                Vec::new()
            };
            if !prefix {
                assert_unpublished(&home);
            }
            assert_samples(&home, &expected, false, expected.len());
        }
    }
}

fn delayed_reply(port: &mut (impl Read + io::Write), stop: &AtomicBool, prefix: bool) {
    if prefix {
        query(port, stop, false);
        port.write_all(b"ST,+03142.06  g\r\n").unwrap();
    }
    query(port, stop, false);
    thread::sleep(Duration::from_millis(140));
    let _ = port.write_all(b"ST,+03142.06  g\r");
    thread::sleep(Duration::from_millis(140));
    let _ = port.write_all(b"\n");
    no_more(port, stop);
}

#[test]
fn aandd_split_reply_shares_original_deadline_over_tcp_and_actual_pty() {
    for serial in [false, true] {
        for prefix in [false, true] {
            for cutoff in [false, true] {
                let home = Home::new(&format!("aandd-deadline-{serial}-{prefix}-{cutoff}"));
                let (device, mut source) = if serial {
                    let (mut master, slave, path) = serial_pair();
                    let device = Device::start(move |stop| {
                        let _slave = slave;
                        delayed_reply(&mut master, &stop, prefix);
                        1 + u64::from(prefix)
                    });
                    (
                        device,
                        json!({"serial":{"device":path,"baud":9600,"parity":"none","data_bits":8}}),
                    )
                } else {
                    let (listen, address) = listener();
                    let device = Device::start(move |stop| {
                        let mut port = accept(&listen, &stop).unwrap();
                        delayed_reply(&mut port, &stop, prefix);
                        1 + u64::from(prefix)
                    });
                    (device, json!({"address":address}))
                };
                source["every_ms"] = json!(100);
                source["timeout_ms"] = json!(if cutoff { 2000 } else { 250 });
                let duration = if cutoff {
                    if prefix { "320ms" } else { "220ms" }
                } else {
                    "2s"
                };
                assert_eq!(
                    record(&home, source, duration).failure(4)["code"],
                    "timeout"
                );
                assert_eq!(device.finish(), 1 + u64::from(prefix));
                let expected = if prefix {
                    vec![cases().first().unwrap()["expected"].clone()]
                } else {
                    Vec::new()
                };
                assert_samples(&home, &expected, false, expected.len());
            }
        }
    }
}

#[test]
fn aandd_configuration_fails_before_opening_any_transport() {
    let (listen, address) = listener();
    let invalid = [
        json!({}),
        json!({"address":"127.0.0.1:0"}),
        json!({"address":"0.0.0.0:9000"}),
        json!({"address":"localhost:9000"}),
        json!({"address":address,"serial":{"device":"missing"}}),
        json!({"serial":{"device":""}}),
        json!({"serial":{"device":"missing","baud":115_200}}),
        json!({"serial":{"device":"missing","baud":300}}),
        json!({"serial":{"device":"missing","data_bits":8}}),
        json!({"serial":{"device":"missing","parity":"none"}}),
        json!({"serial":{"device":"missing","stop_bits":2}}),
        json!({"address":address,"terminator":"lf"}),
        json!({"address":address,"command":"T"}),
        json!({"address":address,"signal":""}),
        json!({"address":address,"serial":{"device":"missing","handshake":"rts"}}),
    ];
    for (index, source) in invalid.into_iter().enumerate() {
        let home = Home::new(&format!("aandd-config-{index}"));
        assert_eq!(record(&home, source, "1s").failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        let error = listen.accept().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }
}
