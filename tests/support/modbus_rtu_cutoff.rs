//! Real RTU cutoff phases: only a quiet gap before the first request is idle.
use super::{Device, Home, crc, receive, serial_pair, support::cli::Run};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{self, Read as _, Write as _},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

fn config(device: &str, signals: &Value) -> Value {
    json!({"version":1,"sources":[{"id":"meter","protocol":"modbus_rtu","device":device,
        "unit":3,"baud":300,"parity":"none","stop_bits":2,"every_ms":1,"timeout_ms":1000,"signals":signals}]})
}
fn record(home: &Home, config: &Value) -> Run {
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
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
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn expect_request(port: &mut File, stop: &AtomicBool, address: u16) {
    let mut request = [0; 8];
    assert!(receive(port, &mut request, stop));
    let [high, low] = address.to_be_bytes();
    let [
        unit,
        function,
        a,
        b,
        count_high,
        count_low,
        crc_low,
        crc_high,
    ] = request;
    let body = [unit, function, a, b, count_high, count_low];
    assert_eq!(body, [3, 3, high, low, 0, 1]);
    assert_eq!(crc(&body), u16::from_le_bytes([crc_low, crc_high]));
}
fn reply(port: &mut File, value: u16) {
    let [high, low] = value.to_be_bytes();
    let mut response = vec![3, 3, 2, high, low];
    response.extend_from_slice(&crc(&response).to_le_bytes());
    port.write_all(&response).unwrap();
}
fn no_more(port: &mut File, stop: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !stop.load(Ordering::Acquire) {
        match port.read(&mut [0]) {
            Ok(0) => return,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "RTU owner did not close after cutoff"
                );
                thread::sleep(Duration::from_millis(1));
            }
            other => panic!("unexpected later RTU request: {other:?}"),
        }
    }
}
fn hold_until_final_quiet_gap(run: &Path, stop: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let started = loop {
        assert!(
            !stop.load(Ordering::Acquire),
            "recorder stopped before cutoff fixture was armed"
        );
        if let Ok(text) = fs::read_to_string(run)
            && let Some(first) = text.lines().next()
            && let Ok(header) = serde_json::from_str::<Value>(first)
            && let Some(started) = header["started_at"].as_str()
        {
            break chrono::DateTime::parse_from_rfc3339(started).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "recording header was not written"
        );
        thread::sleep(Duration::from_millis(1));
    };
    let target = started + chrono::Duration::milliseconds(900);
    loop {
        assert!(
            !stop.load(Ordering::Acquire),
            "recorder stopped before final reply"
        );
        let left = target.signed_duration_since(chrono::Utc::now());
        if left <= chrono::Duration::zero() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "wall clock moved during cutoff fixture"
        );
        thread::sleep(left.to_std().unwrap().min(Duration::from_millis(10)));
    }
}
fn assert_prefix(home: &Home, expected: &[(&str, u16)], status: &str) {
    let rows = rows(home);
    let samples = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), expected.len());
    for (sample, (signal, value)) in samples.into_iter().zip(expected) {
        assert_eq!(sample["signal"], *signal);
        assert_eq!(sample["value"].as_f64(), Some(f64::from(*value)));
    }
    let end = rows.last().unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["status"], status);
    assert_eq!(
        end["samples"].as_u64(),
        Some(u64::try_from(expected.len()).unwrap())
    );
}

#[test]
fn rtu_cutoff_before_a_new_poll_request_is_idle_after_one_completed_poll() {
    let home = Home::new("rtu-cutoff-first-gap");
    let (mut master, slave, device) = serial_pair();
    let run = home.path("run.jsonl");
    let worker = Device::start(move |stop| {
        expect_request(&mut master, &stop, 0);
        drop(slave);
        hold_until_final_quiet_gap(&run, &stop);
        reply(&mut master, 7);
        // At300baud with11bits/character the128.3ms quiet gap cannot fit
        // in the remaining~100ms. No second request may appear on the wire.
        no_more(&mut master, &stop);
        1
    });
    let result = record(
        &home,
        &config(&device, &json!([{"name":"value","address":0}])),
    )
    .success();
    assert_eq!(worker.finish(), 1);
    assert_eq!(result["status"], "complete");
    assert_prefix(&home, &[("value", 7)], "complete");
}

#[test]
fn rtu_cutoff_between_siblings_fails_without_publishing_the_partial_second_poll() {
    let home = Home::new("rtu-cutoff-sibling-gap");
    let (mut master, slave, device) = serial_pair();
    let run = home.path("run.jsonl");
    let worker = Device::start(move |stop| {
        expect_request(&mut master, &stop, 0);
        drop(slave);
        reply(&mut master, 7);
        expect_request(&mut master, &stop, 1);
        reply(&mut master, 8);
        expect_request(&mut master, &stop, 0);
        hold_until_final_quiet_gap(&run, &stop);
        reply(&mut master, 9);
        // This gap follows a completed sibling in the same poll, so the
        // missing sibling is a failed transaction even without another write.
        no_more(&mut master, &stop);
        3
    });
    let error = record(
        &home,
        &config(
            &device,
            &json!([
                {"name":"first","address":0},{"name":"second","address":1}
            ]),
        ),
    )
    .failure(4);
    assert_eq!(worker.finish(), 3);
    assert_eq!(error["code"], "timeout");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("inter-frame gap")
    );
    assert_prefix(&home, &[("first", 7), ("second", 8)], "failed");
}

#[test]
fn rtu_cutoff_during_a_sent_request_remains_a_failure_after_a_completed_poll() {
    let home = Home::new("rtu-cutoff-inflight");
    let (mut master, slave, device) = serial_pair();
    let worker = Device::start(move |stop| {
        expect_request(&mut master, &stop, 0);
        drop(slave);
        reply(&mut master, 7);
        expect_request(&mut master, &stop, 0);
        // Correct unit/function/count, then only half the value and no CRC.
        master.write_all(&[3, 3, 2, 0]).unwrap();
        no_more(&mut master, &stop);
        2
    });
    let error = record(
        &home,
        &config(&device, &json!([{"name":"value","address":0}])),
    )
    .failure(4);
    assert_eq!(worker.finish(), 2);
    assert_eq!(error["code"], "timeout");
    assert_prefix(&home, &[("value", 7)], "failed");
}
