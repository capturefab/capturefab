//! Spec-derived `DataMan` ID Reader fixtures; no SDK or physical reader is claimed.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use support::{
    cli::{Home, Run},
    time,
};

const REGISTER: [u8; 28] = [
    0x65, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
];
const SESSION: [u8; 4] = [0x11, 0x22, 0x33, 0x44];
const REQUEST: [u8; 8] = [0x0e, 3, 0x20, 0x79, 0x24, 1, 0x30, 0x10];
const CPF: [u8; 14] = [0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];

struct Peer {
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

enum Answer {
    Complete(Vec<u8>),
    Slow(Vec<u8>),
    Oversized,
}

fn accept(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + time::secs(5);
    loop {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream.set_read_timeout(Some(time::secs(3))).unwrap();
                stream.set_write_timeout(Some(time::secs(3))).unwrap();
                return Some(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "DataMan client did not arrive");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn query(stream: &mut TcpStream, context: u64) -> [u8; 24] {
    let mut packet = [0; 48];
    stream.read_exact(&mut packet).unwrap();
    let mut header = [0; 24];
    header[0] = 0x6f;
    header[2] = 24;
    header[4..8].copy_from_slice(&SESSION);
    header[12..20].copy_from_slice(&context.to_le_bytes());
    assert_eq!(packet[..24], header);
    assert_eq!(packet[24..38], CPF);
    assert_eq!(packet[38..40], [8, 0]);
    assert_eq!(
        packet[40..],
        REQUEST,
        "only the complete result GET is admitted"
    );
    header
}

fn packet(mut header: [u8; 24], cip: &[u8]) -> Vec<u8> {
    header[2..4].copy_from_slice(&u16::try_from(16 + cip.len()).unwrap().to_le_bytes());
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(&CPF);
    bytes.extend_from_slice(&u16::try_from(cip.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(cip);
    bytes
}

fn finish_wire(stream: &mut TcpStream) {
    let mut bytes = Vec::new();
    let mut block = [0; 25];
    loop {
        match stream.read(&mut block) {
            Ok(0) => break,
            Ok(count) => {
                bytes.extend_from_slice(&block[..count]);
                assert!(bytes.len() <= 24, "unexpected retry, write or extra poll");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ) =>
            {
                break;
            }
            Err(error) => panic!("DataMan connection did not retire: {error}"),
        }
    }
    if !bytes.is_empty() {
        let mut unregister = [0; 24];
        unregister[0] = 0x66;
        unregister[4..8].copy_from_slice(&SESSION);
        assert_eq!(
            bytes,
            unregister[..bytes.len()],
            "only best-effort unregister is allowed"
        );
    }
}

impl Peer {
    fn new(answers: Vec<Answer>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let owned = listener.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handle = thread::spawn(move || {
            let Some(mut stream) = accept(&owned, &stopped) else {
                return;
            };
            let mut register = [0; 28];
            stream.read_exact(&mut register).unwrap();
            assert_eq!(register, REGISTER);
            register[4..8].copy_from_slice(&SESSION);
            stream.write_all(&register).unwrap();
            for (index, answer) in answers.into_iter().enumerate() {
                let mut header = query(&mut stream, u64::try_from(index).unwrap() + 1);
                match answer {
                    Answer::Complete(cip) => {
                        for part in packet(header, &cip).chunks(7) {
                            stream.write_all(part).unwrap();
                        }
                    }
                    Answer::Slow(cip) => {
                        // A complete valid reply would arrive only after400ms.
                        // Resetting a250ms deadline on each receive would accept it.
                        let bytes = packet(header, &cip);
                        let groups = bytes.len().div_ceil(4);
                        for part in bytes.chunks(groups) {
                            thread::sleep(time::millis(100));
                            if stream.write_all(part).is_err() {
                                break;
                            }
                        }
                    }
                    Answer::Oversized => {
                        header[2..4].copy_from_slice(&8193_u16.to_le_bytes());
                        stream.write_all(&header).unwrap();
                    }
                }
            }
            finish_wire(&mut stream);
        });
        Self {
            listener,
            stop,
            handle: Some(handle),
        }
    }
    fn source(&self) -> Value {
        json!({"id":"reader","protocol":"cognex_dataman","profile":"cip_result",
            "address":self.listener.local_addr().unwrap().to_string(),"signal":"result",
            "every_ms":3_600_000,"timeout_ms":2000})
    }
    fn finish(mut self) {
        self.handle.take().unwrap().join().unwrap();
        self.listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(self.listener.accept(),Err(error) if error.kind()==io::ErrorKind::WouldBlock),
            "source reconnected"
        );
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn record(home: &Home, sources: &Value, duration: &str) -> Run {
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
fn rows(home: &Home, name: &str) -> Vec<Value> {
    fs::read_to_string(home.path(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn literals() -> Value {
    serde_json::from_str(include_str!("fixtures/dataman/results.json")).unwrap()
}
fn unhex(value: &str) -> Vec<u8> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    assert_eq!(remainder, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn vector(name: &str) -> (Vec<u8>, Value) {
    let values = literals();
    let value = &values["cases"][name];
    (
        unhex(value["cip_hex"].as_str().unwrap()),
        value["expected"].clone(),
    )
}
fn check_rows(home: &Home, expected: &[Value], status: &str) -> Vec<Value> {
    let rows = rows(home, "run.jsonl");
    assert_eq!(rows[0]["kind"], "header");
    assert_eq!(rows.last().unwrap()["status"], status);
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let mut previous = std::collections::BTreeMap::new();
    let samples: Vec<_> = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .cloned()
        .collect();
    for (index, sample) in samples.iter().enumerate() {
        assert_eq!(sample["seq"], index + 1);
        let received =
            chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
        let elapsed = sample["elapsed_ns"].as_u64().unwrap();
        let source = sample["source"].as_str().unwrap();
        let prior = previous.insert(source, elapsed).unwrap_or(0);
        assert!(elapsed >= prior);
        assert_eq!(
            received,
            origin + chrono::Duration::nanoseconds(i64::try_from(elapsed).unwrap())
        );
        assert!(sample["read_duration_ns"].as_u64().unwrap() <= elapsed);
        sample["delivery_latency_ns"].as_u64().unwrap();
        assert!(sample.get("source_time").is_none());
    }
    let native: Vec<_> = samples
        .iter()
        .filter(|row| row["source"] == "reader")
        .map(|row| row["value"].clone())
        .collect();
    assert_eq!(native, expected);
    samples
}
fn exports(home: &Home, samples: &[Value], complete: bool) {
    let mut jsonl_args = vec![
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
    ];
    let mut csv_args = vec!["data", "export", "run.jsonl", "-o", "values.csv"];
    if !complete {
        let mut strict_args = vec!["--json"];
        strict_args.extend_from_slice(&jsonl_args);
        assert_eq!(home.run(&strict_args).failure(1)["code"], "incomplete");
        assert!(!home.path("values.jsonl").exists());
        jsonl_args.push("--allow-incomplete");
        csv_args.push("--allow-incomplete");
    }
    home.ok(&jsonl_args);
    let projected = rows(home, "values.jsonl");
    let expected: Vec<_> = samples
        .iter()
        .map(|sample| {
            let mut sample = sample.clone();
            sample.as_object_mut().unwrap().remove("kind");
            sample
        })
        .collect();
    assert_eq!(projected, expected);
    home.ok(&csv_args);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), samples.len() + 1);
    for sample in samples {
        let field = format!("\"{}\"", sample["value"].to_string().replace('"', "\"\""));
        let names = format!(
            ",\"{}\",\"{}\",",
            sample["source"].as_str().unwrap(),
            sample["signal"].as_str().unwrap()
        );
        assert!(
            csv.lines()
                .any(|line| line.contains(&names) && line.ends_with(&field)),
            "missing CSV object"
        );
    }
}

#[test]
fn complete_binary_results_keep_tail_repeat_wrap_and_typed_exports() {
    let home = Home::new("dataman-complete");
    let (binary, expected_binary) = vector("binary_tail");
    let (empty, expected_empty) = vector("empty");
    let (wrapped, expected_wrapped) = vector("wrapped");
    let peer = Peer::new(vec![
        Answer::Complete(binary.clone()),
        Answer::Complete(binary),
        Answer::Complete(empty),
        Answer::Complete(wrapped),
    ]);
    let mut source = peer.source();
    source["every_ms"] = json!(time::ms(300));
    let sources =
        json!([source,{"id":"sim","protocol":"simulator","signal":"cycle","every_ms":3_600_000}]);
    let result = record(&home, &sources, &time::arg(1050)).success();
    peer.finish();
    assert_eq!(result["samples"], 5);
    let samples = check_rows(
        &home,
        &[
            expected_binary.clone(),
            expected_binary,
            expected_empty,
            expected_wrapped,
        ],
        "complete",
    );
    exports(&home, &samples, true);
}

#[test]
fn utf8_presentation_preserves_bytes_nul_and_unknown_native_codes() {
    let home = Home::new("dataman-utf8");
    let (cip, expected) = vector("utf8");
    let peer = Peer::new(vec![Answer::Complete(cip)]);
    let mut source = peer.source();
    source["encoding"] = json!("utf8");
    assert_eq!(
        record(&home, &json!([source]), "300ms").success()["samples"],
        1
    );
    peer.finish();
    let samples = check_rows(&home, &[expected], "complete");
    exports(&home, &samples, true);
}

#[test]
fn late_malformed_partial_status_and_strict_text_failures_retain_only_prior_poll() {
    let (good, expected) = vector("empty");
    let malformed = [
        (unhex("8e0000000000000000000400414243"), "incomplete", None),
        (unhex("8e000600"), "incomplete", Some(("cip_status", 6))),
        (unhex("8e000100"), "failed", Some(("cip_status", 1))),
        (unhex("8e0000000000000000000200aabb"), "incomplete", None),
        (unhex("8e0000000000000000000200ff80"), "incomplete", None),
    ];
    for (index, (bad, code, detail)) in malformed.into_iter().enumerate() {
        let home = Home::new("dataman-prefix");
        let peer = Peer::new(vec![Answer::Complete(good.clone()), Answer::Complete(bad)]);
        let mut source = peer.source();
        source["every_ms"] = json!(time::ms(200));
        if index == 3 {
            source["max_result_bytes"] = json!(1);
        }
        if index == 4 {
            source["encoding"] = json!("utf8");
        }
        let error = record(&home, &json!([source]), &time::arg(1000)).failure(1);
        peer.finish();
        assert_eq!(error["code"], code);
        if let Some((field, value)) = detail {
            assert_eq!(error["details"][field], value);
        }
        let expected = if index == 4 {
            let mut value = expected.clone();
            value["encoding"] = json!("utf8");
            value["text"] = json!("");
            value
        } else {
            expected.clone()
        };
        let samples = check_rows(&home, &[expected], "failed");
        assert_eq!(samples.len(), 1);
        exports(&home, &samples, false);
    }
}

#[test]
fn declared_native_and_encapsulation_bounds_fail_before_first_publication() {
    for oversized in [false, true] {
        let home = Home::new("dataman-zero-prefix");
        let peer = Peer::new(vec![if oversized {
            Answer::Oversized
        } else {
            Answer::Complete(unhex("8e00000000000000000002004142"))
        }]);
        let mut source = peer.source();
        source["max_result_bytes"] = json!(1);
        record(&home, &json!([source]), "1s").failure(1);
        peer.finish();
        assert_eq!(check_rows(&home, &[], "failed").len(), 0);
    }
}

#[test]
fn progressing_valid_reply_cannot_renew_request_or_acquisition_deadline() {
    for acquisition in [false, true] {
        let home = Home::new("dataman-deadline");
        let (cip, _) = vector("empty");
        let peer = Peer::new(vec![Answer::Slow(cip)]);
        let mut source = peer.source();
        source["timeout_ms"] = json!(time::ms(if acquisition { 2000 } else { 250 }));
        let error = record(
            &home,
            &json!([source]),
            &time::arg(if acquisition { 250 } else { 1000 }),
        )
        .failure(4);
        peer.finish();
        assert_eq!(error["code"], "timeout");
        assert_eq!(check_rows(&home, &[], "failed").len(), 0);
    }
}

#[test]
fn invalid_config_is_rejected_before_any_transport_or_recording() {
    let base = json!({"id":"reader","protocol":"cognex_dataman","profile":"cip_result","address":"127.0.0.1:44818","signal":"result"});
    for patch in [
        json!({"address":"127.0.0.1:0"}),
        json!({"profile":"complete_latest_unbuffered"}),
        json!({"profile":""}),
        json!({"encoding":"ascii"}),
        json!({"max_result_bytes":0}),
        json!({"max_result_bytes":8165}),
        json!({"signal":""}),
        json!({"slot":1}),
        json!({"command":"GET RESULT"}),
    ] {
        let home = Home::new("dataman-preopen");
        let mut source = base.clone();
        for (key, value) in patch.as_object().unwrap() {
            source[key].clone_from(value);
        }
        assert_eq!(
            record(&home, &json!([source]), "1s").failure(2)["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
