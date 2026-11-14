//! The independent literal weight replies are from the IND360 User's Guide
//! §C.2.3 and the `NewClassic` MT-SICS Reference Manual p. 103.
use super::{Device, Home, accept, assert_unpublished, listener, receive, serial_pair};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write as _},
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

fn expect_query(port: &mut impl Read, stop: &AtomicBool) {
    let mut request = [0; 4];
    assert!(receive(port, &mut request, stop));
    assert_eq!(&request, b"SI\r\n");
}

fn record(home: &Home, mut source: Value) -> super::support::cli::Run {
    source["id"] = json!("scale");
    source["protocol"] = json!("mt_sics");
    source["every_ms"] = json!(3_600_000);
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
        "650ms",
        "-o",
        "run.jsonl",
    ])
}

fn assert_reading(home: &Home, signal: &str, value: &Value) {
    let output = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let rows = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["sources"][0]["protocol"], "mt_sics");
    assert_eq!(rows[1]["source"], "scale");
    assert_eq!(rows[1]["signal"], signal);
    assert_eq!(&rows[1]["value"], value);
    assert_eq!(rows[2]["status"], "complete");
    for format in ["csv", "jsonl"] {
        assert_eq!(
            home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "-o",
                format,
                "--format",
                format
            ])["samples"],
            1
        );
    }
    let jsonl = fs::read_to_string(home.path("jsonl")).unwrap();
    assert_eq!(
        &serde_json::from_str::<Value>(&jsonl).unwrap()["value"],
        value
    );
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    assert!(
        csv.contains(&value.to_string().replace('"', "\"\"")),
        "{csv}"
    );
}

#[test]
fn mt_sics_ind360_tcp_replays_documented_stable_reply_and_exports() {
    let home = Home::new("mt-sics-ind360");
    let (listener, address) = listener();
    let scale = Device::start(move |stop| {
        let mut socket = accept(&listener, &stop).unwrap();
        expect_query(&mut socket, &stop);
        socket.write_all(b"S S      0.").unwrap();
        thread::sleep(Duration::from_millis(3));
        socket.write_all(b"256 kg\r\n").unwrap();
        assert!(!receive(&mut socket, &mut [0], &stop));
        1
    });
    let run = record(&home, json!({"address":address}));
    assert_eq!(run.success()["samples"], 1);
    assert_eq!(scale.finish(), 1);
    assert_reading(
        &home,
        "net_weight",
        &json!({"status":"stable","raw":"0.256","value":0.256,"unit":"kg"}),
    );
}

#[test]
fn mt_sics_newclassic_serial_replays_documented_dynamic_reply_and_exports() {
    let home = Home::new("mt-sics-newclassic");
    let (mut master, slave, device) = serial_pair();
    let scale = Device::start(move |stop| {
        let _slave = slave;
        expect_query(&mut master, &stop);
        master.write_all(b"S D     129.07 g\r\n").unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let run = record(
        &home,
        json!({"serial":{"device":device,"baud":9.6e3},"signal":"balance"}),
    );
    assert_eq!(run.success()["samples"], 1);
    assert_eq!(scale.finish(), 1);
    assert_reading(
        &home,
        "balance",
        &json!({"status":"dynamic","raw":"129.07","value":129.07,"unit":"g"}),
    );
}

#[test]
fn mt_sics_split_crlf_cannot_renew_the_poll_deadline() {
    let home = Home::new("mt-sics-deadline");
    let (listener, address) = listener();
    let scale = Device::start(move |stop| {
        let mut socket = accept(&listener, &stop).unwrap();
        expect_query(&mut socket, &stop);
        thread::sleep(Duration::from_millis(160));
        let _ = socket.write_all(b"S I\r");
        thread::sleep(Duration::from_millis(160));
        let _ = socket.write_all(b"\n");
        assert!(!receive(&mut socket, &mut [0], &stop));
        1
    });
    let run = record(&home, json!({"address":address,"timeout_ms":250}));
    assert_eq!(run.failure(4)["code"], "timeout");
    assert_eq!(scale.finish(), 1);
    assert_unpublished(&home);
}
