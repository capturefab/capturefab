//! Manual-derived ILD2300 words over an actual PTY; no physical sensor claim.
use super::{Device, Home, assert_unpublished, receive, serial_pair, support::cli::Run};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write as _,
    os::fd::AsRawFd as _,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

fn cases() -> Vec<Value> {
    serde_json::from_str::<Value>(include_str!("../fixtures/microepsilon/ild2300-dist1.json"))
        .unwrap()["cases"]
        .as_array()
        .unwrap()
        .clone()
}
fn wire(case: &Value) -> Vec<u8> {
    let text = case["wire_hex"].as_str().unwrap().as_bytes();
    let (pairs, remainder) = text.as_chunks::<2>();
    assert_eq!(remainder, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn expected(case: &Value, alignment: &[u8]) -> Value {
    let raw = wire(case);
    let code = case["raw_code"].as_u64().unwrap();
    let mut value = json!({
        "kind":"microepsilon_rs422", "profile":"ild2300_dist1",
        "configuration_status":"configured_unverified", "code_bits":18,
        "raw":{"encoding":"base64","bytes":3,"data":STANDARD.encode(raw)},
        "raw_code":code, "decode_status":"raw_code",
        "checksum_status":"not_in_word_format", "freshness_status":"not_proven"
    });
    if (262_073..=262_082).contains(&code) {
        value["decode_status"] = json!("device_error");
        value["error"] = case["name"].clone();
    }
    if !alignment.is_empty() {
        value["initial_alignment"] = json!({
            "bytes":alignment.len(), "encoding":"base64", "data":STANDARD.encode(alignment)
        });
    }
    value
}
fn configured(mut source: Value) -> Value {
    source["id"] = json!("laser");
    source["protocol"] = json!("microepsilon_rs422");
    source["every_ms"] = json!(3_600_000);
    if source.get("timeout_ms").is_none() {
        source["timeout_ms"] = json!(500);
    }
    source
}
fn record(home: &Home, source: Value, duration: &str, mixed: bool) -> Run {
    let mut sources = vec![configured(source)];
    if mixed {
        sources.push(json!({
            "id":"counter", "protocol":"simulator", "signal":"cycle", "every_ms":3_600_000
        }));
    }
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
fn rows(home: &Home, path: &str) -> Vec<Value> {
    fs::read_to_string(home.path(path))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn values(home: &Home, path: &str) -> Vec<Value> {
    rows(home, path)
        .into_iter()
        .filter(|row| row["source"] == "laser")
        .map(|row| row["value"].clone())
        .collect()
}
fn assert_recording(home: &Home, expected: &[Value], complete: bool, total: usize) {
    assert_eq!(values(home, "run.jsonl"), expected);
    let recorded = rows(home, "run.jsonl");
    let end = recorded.last().unwrap();
    assert_eq!(end["status"], if complete { "complete" } else { "failed" });
    assert_eq!(end["samples"], total);
    for sample in recorded.iter().filter(|row| row["kind"] == "sample") {
        assert!(sample.get("source_time").is_none());
        for field in ["elapsed_ns", "read_duration_ns", "delivery_latency_ns"] {
            assert!(sample[field].is_u64());
        }
        if sample["source"] == "laser" {
            assert!(sample["value"]["raw_code"].is_u64());
            for absent in ["value", "unit", "valid", "source_time"] {
                assert!(sample["value"].get(absent).is_none());
            }
        }
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
    assert_eq!(values(home, "jsonl"), expected);
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json"
    );
    let lines = lines.collect::<Vec<_>>();
    assert_eq!(lines.len(), total);
    let actual = lines
        .iter()
        .filter(|line| line.split(',').nth(1) == Some("\"laser\""))
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    let quoted = expected
        .iter()
        .map(|value| format!("\"{}\"", value.to_string().replace('"', "\"\"")))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        quoted.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

fn await_port(slave: &File, stop: &AtomicBool, baud: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stop.load(Ordering::Acquire) {
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: slave is live and settings is valid writable storage.
        let status = unsafe { libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) };
        assert_eq!(status, 0);
        // SAFETY: successful tcgetattr initialized settings.
        let settings = unsafe { settings.assume_init() };
        // SAFETY: settings is initialized and readable for this call.
        let speed = unsafe { libc::cfgetispeed(&raw const settings) };
        let expected = if baud == 9600 {
            libc::B9600
        } else {
            libc::B115200
        };
        if speed == expected && settings.c_lflag & (libc::ICANON | libc::ECHO) == 0 {
            assert_eq!(settings.c_cflag & libc::CSIZE, libc::CS8);
            assert_eq!(settings.c_cflag & (libc::PARENB | libc::CSTOPB), 0);
            assert_eq!(
                settings.c_iflag & (libc::IXON | libc::IXOFF | libc::IXANY),
                0
            );
            return true;
        }
        assert!(
            Instant::now() < deadline,
            "Micro-Epsilon PTY was not configured"
        );
        thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
fn ild2300_all_manual_derived_words_are_passive_exact_and_unscaled_with_mixed_exports() {
    let home = Home::new("ild2300-all-words");
    let input = cases();
    let expected = input
        .iter()
        .map(|case| expected(case, &[]))
        .collect::<Vec<_>>();
    let (mut master, slave, path) = serial_pair();
    let device = Device::start(move |stop| {
        assert!(await_port(&slave, &stop, 9600));
        for case in &input {
            for byte in wire(case) {
                master.write_all(&[byte]).unwrap();
            }
        }
        assert!(
            !receive(&mut master, &mut [0], &stop),
            "passive ILD2300 source transmitted a byte"
        );
        u64::try_from(input.len()).unwrap()
    });
    assert_eq!(
        record(
            &home,
            json!({"device":path,"profile":"ild2300_dist1","baud":9.6e3}),
            "450ms",
            true
        )
        .success()["samples"],
        24
    );
    assert_eq!(device.finish(), 23);
    assert_recording(&home, &expected, true, 24);
    assert!(
        rows(&home, "run.jsonl")
            .iter()
            .filter(|row| row["source"] == "laser")
            .all(|row| row["signal"] == "distance1")
    );
}

#[test]
fn ild2300_only_the_actual_initial_h_or_mh_suffix_is_retained_once() {
    for (index, prefix) in [&[][..], &[0xbf], &[0x7e, 0xbf]].into_iter().enumerate() {
        let home = Home::new(&format!("ild2300-alignment-{index}"));
        let prefix = prefix.to_vec();
        let expected = [expected(&cases()[6], &prefix), expected(&cases()[0], &[])];
        let (mut master, slave, path) = serial_pair();
        let device = Device::start(move |stop| {
            assert!(await_port(&slave, &stop, 115_200));
            master.write_all(&prefix).unwrap();
            master.write_all(&[0x38]).unwrap();
            thread::sleep(Duration::from_millis(2));
            master.write_all(&[0x7f]).unwrap();
            thread::sleep(Duration::from_millis(2));
            master.write_all(&[0x87, 0, 0x40, 0x80]).unwrap();
            assert!(!receive(&mut master, &mut [0], &stop));
            2_u64
        });
        assert_eq!(
            record(
                &home,
                json!({"device":path,"profile":"ild2300_dist1","signal":"code"}),
                "250ms",
                false
            )
            .success()["samples"],
            2
        );
        assert_eq!(device.finish(), 2);
        assert_recording(&home, &expected, true, 2);
        assert!(
            rows(&home, "run.jsonl")
                .iter()
                .filter(|row| row["source"] == "laser")
                .all(|row| row["signal"] == "code")
        );
    }
}

#[test]
fn ild2300_bad_order_marker_and_startup_sequences_do_not_resync_or_publish_partial_values() {
    let invalid: &[&[u8]] = &[
        &[0x40, 0x40],
        &[0x80, 0x80],
        &[0x80, 0x40],
        &[0x40, 0],
        &[0xc0],
        &[0, 0x40, 0xc0],
        &[0, 0, 0x80],
        &[0, 0x40, 0x40],
        &[0, 0x80],
    ];
    for (index, &bad) in invalid.iter().enumerate() {
        for prefix in [false, true] {
            let home = Home::new(&format!("ild2300-invalid-{index}-{prefix}"));
            let bad = bad.to_vec();
            let (mut master, slave, path) = serial_pair();
            let device = Device::start(move |stop| {
                assert!(await_port(&slave, &stop, 9600));
                if prefix {
                    master.write_all(&[0x38, 0x7f, 0x87]).unwrap();
                }
                master.write_all(&bad).unwrap();
                assert!(!receive(&mut master, &mut [0], &stop));
                1 + u64::from(prefix)
            });
            assert_eq!(
                record(
                    &home,
                    json!({"device":path,"profile":"ild2300_dist1","baud":9600}),
                    "1s",
                    false
                )
                .failure(1)["code"],
                "failed"
            );
            assert_eq!(device.finish(), 1 + u64::from(prefix));
            let expected = if prefix {
                vec![expected(&cases()[6], &[])]
            } else {
                Vec::new()
            };
            if !prefix {
                assert_unpublished(&home);
            }
            assert_recording(&home, &expected, false, expected.len());
        }
    }
}

#[test]
fn ild2300_split_words_share_one_deadline_and_partial_cutoffs_remain_strict_after_success() {
    for prefix in [false, true] {
        for cutoff in [false, true] {
            let home = Home::new(&format!("ild2300-deadline-{prefix}-{cutoff}"));
            let (mut master, slave, path) = serial_pair();
            let device = Device::start(move |stop| {
                assert!(await_port(&slave, &stop, 9600));
                if prefix {
                    master.write_all(&[0x38, 0x7f, 0x87]).unwrap();
                }
                master.write_all(&[0]).unwrap();
                thread::sleep(Duration::from_millis(140));
                let _ = master.write_all(&[0x40]);
                thread::sleep(Duration::from_millis(140));
                let _ = master.write_all(&[0x80]);
                assert!(!receive(&mut master, &mut [0], &stop));
                1 + u64::from(prefix)
            });
            let duration = if cutoff { "220ms" } else { "1s" };
            let timeout = if cutoff { 2000 } else { 250 };
            assert_eq!(record(&home, json!({"device":path,"profile":"ild2300_dist1","baud":9600,"timeout_ms":timeout}), duration, false).failure(4)["code"], "timeout");
            assert_eq!(device.finish(), 1 + u64::from(prefix));
            let expected = if prefix {
                vec![expected(&cases()[6], &[])]
            } else {
                Vec::new()
            };
            assert_recording(&home, &expected, false, expected.len());
        }
    }
    for partial in [&[0x40][..], &[0x80], &[0x40, 0x80]] {
        let home = Home::new("ild2300-incomplete-alignment");
        let partial = partial.to_vec();
        let (mut master, slave, path) = serial_pair();
        let device = Device::start(move |stop| {
            assert!(await_port(&slave, &stop, 9600));
            master.write_all(&partial).unwrap();
            assert!(!receive(&mut master, &mut [0], &stop));
            1_u64
        });
        assert_eq!(
            record(
                &home,
                json!({"device":path,"profile":"ild2300_dist1","baud":9600,"timeout_ms":80}),
                "1s",
                false
            )
            .failure(4)["code"],
            "timeout"
        );
        assert_eq!(device.finish(), 1);
        assert_unpublished(&home);
        assert_recording(&home, &[], false, 0);
    }
}

#[test]
fn ild2300_eof_and_invalid_config_do_not_restart_or_create_measurements() {
    let home = Home::new("ild2300-eof");
    let (mut master, slave, path) = serial_pair();
    let device = Device::start(move |stop| {
        assert!(await_port(&slave, &stop, 9600));
        master.write_all(&[0]).unwrap();
        thread::sleep(Duration::from_millis(20));
        drop(master);
        drop(slave);
        1_u64
    });
    assert_eq!(
        record(
            &home,
            json!({"device":path,"profile":"ild2300_dist1","baud":9600}),
            "1s",
            false
        )
        .failure(3)["code"],
        "disconnected"
    );
    assert_eq!(device.finish(), 1);
    assert_recording(&home, &[], false, 0);
    for (index, source) in [
        json!({}),
        json!({"device":"missing"}),
        json!({"profile":"ild2300_dist1"}),
        json!({"device":"missing","profile":"auto"}),
        json!({"device":"","profile":"ild2300_dist1"}),
        json!({"device":"bad\u{0000}path","profile":"ild2300_dist1"}),
        json!({"device":"missing","profile":"ild2300_dist1","baud":500_000}),
        json!({"device":"missing","profile":"ild2300_dist1","baud":57_600}),
        json!({"device":"missing","profile":"ild2300_dist1","baud":0}),
        json!({"device":"missing","profile":"ild2300_dist1","baud":-1}),
        json!({"device":"missing","profile":"ild2300_dist1","baud":4_294_967_296_u64}),
        json!({"device":"missing","profile":"ild2300_dist1","signal":""}),
        json!({"device":"missing","profile":"ild2300_dist1","parity":"none"}),
        json!({"device":"missing","profile":"ild2300_dist1","command":"OUTPUT RS422"}),
        json!({"device":"missing","profile":"ild2300_dist1","address":"127.0.0.1:1234"}),
    ]
    .into_iter()
    .enumerate()
    {
        let home = Home::new(&format!("ild2300-config-{index}"));
        assert_eq!(
            record(&home, source, "1s", false).failure(2)["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
