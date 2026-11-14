//! `GoPxL` Ethernet ASCII through the real CLI against a private fake sensor; no device claim.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

/// The manufacturer's documented request and reply (`GoPxL` 1.3 polling commands).
const REQUEST: &[u8] = b"measurement,0,1\r\n";
const REPLY: &[u8] = b"OK,M0,V-5000,D0,M1,V5000,D1\r\n";

fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Darwin accepted sockets inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(2));
            }
            other => panic!("private GoPxL accept: {other:?}"),
        }
    }
}

/// Answers each exact request with `replies[n]` (the last one repeats).
fn serve(
    listener: TcpListener,
    replies: Vec<Vec<u8>>,
    count: Arc<Mutex<usize>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stream = accept(&listener);
        loop {
            let mut request = vec![0; REQUEST.len()];
            match stream.read(&mut request[..1]) {
                Ok(0) => break,
                Ok(_) => stream.read_exact(&mut request[1..]).unwrap(),
                Err(error) => panic!("fake sensor read: {error}"),
            }
            assert_eq!(request, REQUEST, "only the measurement query is sent");
            let index = {
                let mut count = count.lock().unwrap();
                *count += 1;
                *count - 1
            };
            let reply = replies.get(index).or_else(|| replies.last()).unwrap();
            stream.write_all(reply).unwrap();
        }
        listener.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "unexpected reconnect"
        );
    })
}

fn configure(home: &Home, address: &str, trailing: &str) {
    fs::write(
        home.path("sources.json"),
        serde_json::to_vec(&json!({
            "version":1,"sources":[{"id":"gauge","protocol":"gopxl_ascii",
                "profile":"gopxl_1_3_measurement","address":address,
                "delimiter":",","terminator":"crlf","invalid_token":"INVALID",
                "trailing_delimiter":trailing,"every_ms":100,"timeout_ms":1000,
                "measurements":[{"name":"gap","id":0},{"name":"flush","id":1}]}]
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
fn samples(rows: &[Value]) -> Vec<&Value> {
    rows.iter().filter(|row| row["kind"] == "sample").collect()
}
fn args() -> [&'static str; 8] {
    [
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "450ms",
        "-o",
        "run.jsonl",
    ]
}

#[test]
fn polls_keep_native_values_and_decisions_and_round_trip_to_csv() {
    let home = Home::new("gopxl-polls");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    configure(&home, &listener.local_addr().unwrap().to_string(), "either");
    let count = Arc::new(Mutex::new(0));
    let peer = serve(listener, vec![REPLY.to_vec()], count.clone());
    let result = home.ok(&args());
    peer.join().unwrap();
    let records = rows(&home);
    let samples = samples(&records);
    let polls = *count.lock().unwrap();
    assert!(polls >= 2);
    assert_eq!(samples.len(), polls * 2);
    assert_eq!(result["samples"], polls * 2);
    let [gap, flush] = &samples[..2] else {
        panic!("two measurements per poll")
    };
    assert_eq!(gap["signal"], "gap");
    assert_eq!(gap["value"]["value"].to_string(), "-5.000");
    assert_eq!(gap["value"]["scaled_i32"], -5000);
    assert_eq!(gap["value"]["pass"], false);
    assert_eq!(flush["value"]["value"].to_string(), "5.000");
    assert_eq!(flush["value"]["pass"], true);
    assert_eq!(flush["value"]["decision_invalid"], false);
    assert_eq!(gap["elapsed_ns"], flush["elapsed_ns"]);
    assert!(samples.iter().all(|row| row.get("source_time").is_none()));
    assert_eq!(records.last().unwrap()["status"], "complete");
    let exported = home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    assert_eq!(exported["samples"], polls * 2);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert!(csv.contains("-5.000") && csv.contains("-5000"), "{csv}");
}

#[test]
fn device_error_and_malformed_replies_withhold_the_poll_and_keep_earlier_ones() {
    for (second, code, reason) in [
        (
            b"ERROR,There is no measurement data to output. Please confirm that the sensor is running\r\n".to_vec(),
            "failed",
            "device_error",
        ),
        (b"OK,M1,V5000,D1,M0,V-5000,D0\r\n".to_vec(), "incomplete", "id_mismatch"),
        // A trailing comma where the operator declared none.
        (b"OK,M0,V-5000,D0,M1,V5000,D1,\r\n".to_vec(), "incomplete", "malformed_reply"),
        // Unsolicited output after the terminator means asynchronous output is on.
        ([REPLY, b"OK"].concat(), "incomplete", "malformed_reply"),
    ] {
        let home = Home::new("gopxl-failure");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        configure(&home, &listener.local_addr().unwrap().to_string(), "absent");
        let count = Arc::new(Mutex::new(0));
        let peer = serve(listener, vec![REPLY.to_vec(), second], count.clone());
        let error = home.fails(1, &args());
        peer.join().unwrap();
        assert_eq!(error["code"], code, "{reason}");
        assert_eq!(error["details"]["reason"], reason);
        assert_eq!(error["details"]["samples"], 2);
        assert_eq!(*count.lock().unwrap(), 2, "no request after a failed poll");
        if reason == "device_error" {
            assert_eq!(error["details"]["gopxl_reply"]["classification"], "no_data");
        }
        let records = rows(&home);
        assert_eq!(samples(&records).len(), 2);
        assert_eq!(records.last().unwrap()["status"], "failed");
    }
}

#[test]
fn invalid_selectors_fail_before_creating_output_or_connecting() {
    let home = Home::new("gopxl-preflight");
    for (key, value) in [
        ("terminator", json!("none")),
        ("delimiter", json!("M")),
        ("invalid_token", json!("123")),
        ("trailing_delimiter", Value::Null),
        ("command", json!("start")),
        ("address", json!("sensor.local:8190")),
    ] {
        configure(&home, "127.0.0.1:8190", "either");
        let mut config: Value =
            serde_json::from_slice(&fs::read(home.path("sources.json")).unwrap()).unwrap();
        config["sources"][0][key] = value;
        fs::write(
            home.path("sources.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let error = home.fails(2, &args());
        assert_eq!(error["code"], "usage", "{key}");
        assert!(!home.path("run.jsonl").exists());
    }
}
