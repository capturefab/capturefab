//! Manufacturer SBI literals and independently declared fixed-format controls.
//! This exercises native TCP/PTY capture, not a physical Sartorius SBI balance.
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
    serde_json::from_str::<Value>(include_str!("../fixtures/sartorius/sbi-replies.json")).unwrap()
        ["cases"]
        .as_array()
        .unwrap()
        .clone()
}
fn query(port: &mut impl Read, stop: &AtomicBool) {
    let mut bytes = [0; 4];
    assert!(receive(port, &mut bytes, stop));
    assert_eq!(&bytes, b"\x1bP\r\n");
}

fn no_more(port: &mut impl Read, stop: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match port.read(&mut [0; 1]) {
            Ok(0) => return,
            Ok(_) => panic!("unexpected command after Sartorius SBI transaction"),
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
                assert!(
                    Instant::now() < deadline,
                    "Sartorius SBI connection did not close"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("Sartorius SBI fixture read: {error}"),
        }
    }
}
fn configured(mut source: Value) -> Value {
    source["id"] = json!("balance");
    source["protocol"] = json!("sartorius_sbi");
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
fn sbi_primary_and_derived_replies_keep_exact_fields_over_tcp_and_actual_pty() {
    for (index, case) in cases().into_iter().enumerate() {
        let transports: &[bool] = if index < 2 {
            &[false, true]
        } else if index % 2 == 0 {
            &[false]
        } else {
            &[true]
        };
        for &serial in transports {
            let home = Home::new(&format!("sbi-literal-{index}-{serial}"));
            let frame = case["frame"].as_str().unwrap().as_bytes().to_vec();
            let (device, mut source) = if serial {
                let (mut master, slave, path) = serial_pair();
                let device = Device::start(move |stop| {
                    let _slave = slave;
                    query(&mut master, &stop);
                    for byte in frame {
                        master.write_all(&[byte]).unwrap();
                    }
                    no_more(&mut master, &stop);
                    1_u64
                });
                (
                    device,
                    json!({"serial":{"device":path,"baud":9.6e3,"parity":"none","data_bits":8}}),
                )
            } else {
                let (listen, address) = listener();
                let device = Device::start(move |stop| {
                    let mut port = accept(&listen, &stop).unwrap();
                    query(&mut port, &stop);
                    for byte in frame {
                        port.write_all(&[byte]).unwrap();
                    }
                    no_more(&mut port, &stop);
                    1_u64
                });
                (device, json!({"address":address}))
            };
            source["format"] = case["format"].clone();
            assert_eq!(record(&home, source, "300ms").success()["samples"], 1);
            assert_eq!(device.finish(), 1);
            let expected = case["expected"].clone();
            if let Some(number) = expected.get("value") {
                let values = balance_values(&home, "run.jsonl");
                assert_eq!(
                    values.first().unwrap()["value"].to_string(),
                    number.to_string()
                );
            }
            assert_samples(&home, &[expected], true, 1);
        }
    }
}

#[test]
fn sbi_nonnumeric_states_keep_one_channel_with_mixed_source_and_exact_exports() {
    let home = Home::new("sbi-mixed");
    let (listen, address) = listener();
    let device = Device::start(move |stop| {
        let mut port = accept(&listen, &stop).unwrap();
        for frame in [
            &b"N     +   123.56 g  \r\n"[..],
            b"Stat     ERR 123    \r\n",
            b"Stat        High    \r\n",
        ] {
            query(&mut port, &stop);
            port.write_all(frame).unwrap();
        }
        no_more(&mut port, &stop);
        3_u64
    });
    let source = configured(
        json!({"address":address,"format":"sbi22","every_ms":200,"signal":"observation"}),
    );
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
    let expected = [1, 10, 9].map(|index| literals[index]["expected"].clone());
    assert_samples(&home, &expected, true, 4);
    let samples = rows(&home, "run.jsonl");
    assert!(
        samples
            .iter()
            .filter(|row| row["source"] == "balance")
            .all(|row| row["signal"] == "observation")
    );
    let counter = samples
        .iter()
        .find(|row| row["source"] == "counter")
        .unwrap();
    assert_eq!(counter["value"], 0);
}

#[test]
fn sbi_malformed_replies_fail_atomically_without_retry_and_retain_completed_prefix() {
    let invalid: &[(&[u8], i32, &str)] = &[
        (b"+   123.56 g \r\n", 1, "failed"),
        (b"+   123.56 g  \n", 1, "failed"),
        (b"+   123.56 g  \rX", 1, "failed"),
        (b"+   123.56 g  \r\nX", 1, "failed"),
        (b"+   123.56 g  \r\n+   123.56 g  \r\n", 1, "failed"),
        (b"+   123.56\tg  \r\n", 1, "failed"),
        (b"+   12,356 g  \r\n", 1, "failed"),
        (b"+   123.5[6]g \r\n", 1, "failed"),
        (b"   Err 1X3    \r\n", 1, "failed"),
        (b"      High   \r\n", 1, "failed"),
        (b"N     +   123.56 g  \r\n", 1, "failed"),
        (b"+   123.", 3, "disconnected"),
    ];
    for (index, &(bad, exit, code)) in invalid.iter().enumerate() {
        for prefix in [false, true] {
            let home = Home::new(&format!("sbi-invalid-{index}-{prefix}"));
            let (listen, address) = listener();
            let device = Device::start(move |stop| {
                let mut port = accept(&listen, &stop).unwrap();
                if prefix {
                    query(&mut port, &stop);
                    port.write_all(b"+   123.56 g  \r\n").unwrap();
                }
                query(&mut port, &stop);
                let _ = port.write_all(bad);
                if let Err(error) = port.shutdown(std::net::Shutdown::Write) {
                    assert!(
                        matches!(
                            error.kind(),
                            io::ErrorKind::NotConnected
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::BrokenPipe
                        ),
                        "SBI fixture shutdown: {error}"
                    );
                }
                no_more(&mut port, &stop);
                1 + u64::from(prefix)
            });
            assert_eq!(
                record(
                    &home,
                    json!({"address":address,"format":"sbi16","every_ms":100}),
                    "1s"
                )
                .failure(exit)["code"],
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
        query(port, stop);
        port.write_all(b"+   123.56 g  \r\n").unwrap();
    }
    query(port, stop);
    thread::sleep(Duration::from_millis(140));
    let _ = port.write_all(b"+   123.56 g  \r");
    thread::sleep(Duration::from_millis(140));
    let _ = port.write_all(b"\n");
    no_more(port, stop);
}

#[test]
fn sbi_segmented_tcp_and_pty_share_original_request_and_acquisition_deadlines() {
    for serial in [false, true] {
        for prefix in [false, true] {
            for cutoff in [false, true] {
                let home = Home::new(&format!("sbi-deadline-{serial}-{prefix}-{cutoff}"));
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
                source["format"] = json!("sbi16");
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
fn sbi_usage_checks_finish_before_any_connection_or_recording() {
    let (listen, address) = listener();
    let invalid = [
        json!({}),
        json!({"address":address}),
        json!({"format":"sbi16"}),
        json!({"address":address,"format":"auto"}),
        json!({"address":"127.0.0.1:0","format":"sbi16"}),
        json!({"address":"0.0.0.0:9000","format":"sbi22"}),
        json!({"address":"localhost:9000","format":"sbi16"}),
        json!({"address":address,"format":"sbi16","serial":{"device":"missing"}}),
        json!({"format":"sbi16","serial":{"device":""}}),
        json!({"format":"sbi16","serial":{"device":"missing","baud":300}}),
        json!({"format":"sbi16","serial":{"device":"missing","data_bits":6}}),
        json!({"format":"sbi16","serial":{"device":"missing","stop_bits":2}}),
        json!({"address":address,"format":"sbi16","command":"T"}),
        json!({"address":address,"format":"sbi16","signal":""}),
        json!({"format":"sbi16","serial":{"device":"missing","handshake":"rts"}}),
    ];
    for (index, source) in invalid.into_iter().enumerate() {
        let home = Home::new(&format!("sbi-config-{index}"));
        assert_eq!(record(&home, source, "1s").failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert_eq!(
            listen.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
