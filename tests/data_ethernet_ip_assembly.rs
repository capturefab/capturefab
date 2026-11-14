//! Loopback wire fixtures for the Keyence DL-EP1 IL 168-byte monitor block
//! (manual 96132E, pp. 3-8–3-12 and 5-23) and the TR Electronic C-58
//! position/flags assembly (TR-ECE-BA-DGB-0135 v08, §8.3.3.5).
mod support;

use base64::Engine as _;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};

const REGISTER: [u8; 28] = [
    0x65, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
];
const CPF: [u8; 14] = [0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
const UNREGISTER: [u8; 24] = [
    0x66, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no Assembly TCP client arrived");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn fixture(listener: &TcpListener, cip_request: &[u8], replies: &[&[u8]]) {
    let mut stream = accept(listener);
    let mut request = [0; 28];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(request, REGISTER);
    let mut registered = REGISTER;
    registered[4..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    stream.write_all(&registered).unwrap();

    let request_length = u16::try_from(cip_request.len()).unwrap();
    let mut request = vec![0; 40 + cip_request.len()];
    for (index, cip_reply) in replies.iter().enumerate() {
        stream.read_exact(&mut request).unwrap();
        let mut header = [0; 24];
        header[0] = 0x6f;
        header[2..4].copy_from_slice(&(16 + request_length).to_le_bytes());
        header[4..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        header[12..20].copy_from_slice(&(u64::try_from(index).unwrap() + 1).to_le_bytes());
        assert_eq!(request[..24], header);
        assert_eq!(request[24..38], CPF);
        assert_eq!(request[38..40], request_length.to_le_bytes());
        assert_eq!(&request[40..], cip_request);

        let cip_length = u16::try_from(cip_reply.len()).unwrap();
        header[2..4].copy_from_slice(&(16 + cip_length).to_le_bytes());
        let mut response = header.to_vec();
        response.extend_from_slice(&CPF);
        response.extend_from_slice(&cip_length.to_le_bytes());
        response.extend_from_slice(cip_reply);
        for chunk in response.chunks(7) {
            stream.write_all(chunk).unwrap();
        }
    }

    // A closed session may omit the best-effort unregister. Any other command
    // would be an unintended write, connection open, retry or extra poll.
    let mut end = [0; 24];
    if stream.read(&mut end[..1]).unwrap() != 0 {
        stream.read_exact(&mut end[1..]).unwrap();
        assert_eq!(end, UNREGISTER);
        assert_eq!(stream.read(&mut end[..1]).unwrap(), 0);
    }
}

fn record(home: &Home, sources: &Value) -> Run {
    let config = json!({"version":1,"sources":sources});
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

fn rows(home: &Home, file: &str) -> Vec<Value> {
    fs::read_to_string(home.path(file))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn raw(bytes: &[u8]) -> Value {
    json!({"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(bytes)})
}

#[test]
fn keyence_monitor_and_encoder_position_keep_typed_values_and_raw_bytes() {
    let home = Home::new("eip-assembly");
    let keyence = TcpListener::bind("127.0.0.1:0").unwrap();
    let encoder = TcpListener::bind("127.0.0.1:0").unwrap();
    let sources = json!([
        {"id":"keyence","protocol":"ethernet_ip_assembly","address":keyence.local_addr().unwrap().to_string(),
         "instance":100,"data_length":168,"every_ms":60_000,"raw_signal":"raw","signals":[
             {"name":"pv1","offset":48,"data_type":"i32"},
             {"name":"pv2","offset":52,"data_type":"i32"},
             {"name":"high1","offset":18,"data_type":"bool","bit":0},
             {"name":"over1","offset":6,"data_type":"bool","bit":0},
             {"name":"under1","offset":8,"data_type":"bool","bit":0},
             {"name":"invalid1","offset":10,"data_type":"bool","bit":0}]},
        {"id":"encoder","protocol":"ethernet_ip_assembly","address":encoder.local_addr().unwrap().to_string(),
         "instance":102,"data_length":9,"every_ms":60_000,"raw_signal":"raw","signals":[
             {"name":"position","offset":0,"data_type":"u64"},
             {"name":"alarm","offset":8,"data_type":"bool","bit":0},
             {"name":"warning","offset":8,"data_type":"bool","bit":1}]}
    ]);
    let mut monitor = [0; 168];
    // ID1 has an invalid P.V. flag but the last raw measurement is retained.
    monitor[10] = 1;
    monitor[18] = 1;
    monitor[48..56].copy_from_slice(&[0xc7, 0xcf, 0xff, 0xff, 0x85, 0x1a, 0, 0]);
    let position = [1, 0, 0, 0, 0, 0, 0x20, 0, 1];
    let keyence_peer = thread::spawn(move || {
        let reply = [&[0x8e, 0, 0, 0][..], &monitor].concat();
        fixture(
            &keyence,
            &[0x0e, 3, 0x20, 4, 0x24, 0x64, 0x30, 3],
            &[&reply],
        );
    });
    let encoder_peer = thread::spawn(move || {
        let reply = [&[0x8e, 0, 0, 0][..], &position].concat();
        fixture(
            &encoder,
            &[0x0e, 3, 0x20, 4, 0x24, 0x66, 0x30, 3],
            &[&reply],
        );
    });
    let result = record(&home, &sources).success();
    keyence_peer.join().unwrap();
    encoder_peer.join().unwrap();

    let expected = BTreeMap::from([
        (("keyence", "pv1"), json!(-12_345)),
        (("keyence", "pv2"), json!(6789)),
        (("keyence", "high1"), json!(true)),
        (("keyence", "over1"), json!(false)),
        (("keyence", "under1"), json!(false)),
        (("keyence", "invalid1"), json!(true)),
        (("keyence", "raw"), raw(&monitor)),
        (("encoder", "position"), json!(9_007_199_254_740_993_u64)),
        (("encoder", "alarm"), json!(true)),
        (("encoder", "warning"), json!(false)),
        (("encoder", "raw"), raw(&position)),
    ]);
    assert_eq!(result["samples"], expected.len());
    let recorded = rows(&home, "run.jsonl");
    assert_eq!(recorded.len(), expected.len() + 2);
    assert_eq!(recorded[0]["kind"], "header");
    assert_eq!(recorded.last().unwrap()["status"], "complete");
    let samples = &recorded[1..recorded.len() - 1];
    let observed = samples
        .iter()
        .map(|row| {
            assert_eq!(row["kind"], "sample");
            assert!(row.get("source_time").is_none());
            let key = (
                row["source"].as_str().unwrap(),
                row["signal"].as_str().unwrap(),
            );
            (key, row["value"].clone())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(observed, expected);

    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), expected.len() + 1);
    for ((source, signal), value) in expected {
        let field = format!("\"{}\"", value.to_string().replace('"', "\"\""));
        assert!(
            csv.lines()
                .any(|row| row.contains(&format!(",\"{source}\",\"{signal}\","))
                    && row.ends_with(&field)),
            "missing CSV value for {source}/{signal}"
        );
    }
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported = rows(&home, "values.jsonl");
    assert_eq!(exported.len(), samples.len());
    for (exported, sample) in exported.iter().zip(samples) {
        let mut expected = sample.clone();
        expected.as_object_mut().unwrap().remove("kind");
        assert_eq!(*exported, expected);
    }
}

#[test]
fn extended_assembly_instance_reuses_one_session_and_advances_context() {
    let home = Home::new("eip-assembly-wide-path");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let instance: Value = serde_json::from_str("3e2").unwrap();
    let sources = json!([
        {"id":"sensor","protocol":"ethernet_ip_assembly","address":listener.local_addr().unwrap().to_string(),
         "instance":instance,"data_length":3,"every_ms":600,"signals":[
             {"name":"scaled","offset":1,"data_type":"i16","byte_order":"big","scale":0.5,"offset_value":-4}]}
    ]);
    let peer = thread::spawn(move || {
        fixture(
            &listener,
            &[0x0e, 4, 0x20, 4, 0x25, 0, 0x2c, 0x01, 0x30, 3],
            &[
                &[0x8e, 0, 0, 0, 0x44, 0x10, 0],
                &[0x8e, 0, 0, 0, 0x44, 0xff, 0xfe],
            ],
        );
    });
    assert_eq!(record(&home, &sources).success()["samples"], 2);
    peer.join().unwrap();
    let recorded = rows(&home, "run.jsonl");
    assert_eq!(recorded[1]["value"], 2044.0);
    assert_eq!(recorded[2]["value"], -5.0);
}

#[test]
fn malformed_or_partial_assembly_reply_does_not_publish_samples() {
    for (service, status, length, exit) in [
        (0x8e, 0, 167, 1),
        (0x8e, 0, 169, 1),
        (0x8e, 6, 168, 5),
        (0xcc, 0, 168, 1),
    ] {
        let home = Home::new("eip-assembly-failed");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sources = json!([
            {"id":"keyence","protocol":"ethernet_ip_assembly","address":listener.local_addr().unwrap().to_string(),
             "instance":100,"data_length":168,"raw_signal":"raw","signals":[
                 {"name":"pv1","offset":48,"data_type":"i32"}]}
        ]);
        let peer = thread::spawn(move || {
            let mut response = vec![service, 0, status, 0];
            response.resize(4 + length, 0);
            fixture(
                &listener,
                &[0x0e, 3, 0x20, 4, 0x24, 0x64, 0x30, 3],
                &[&response],
            );
        });
        record(&home, &sources).failure(exit);
        peer.join().unwrap();
        let rows = rows(&home, "run.jsonl");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["status"], "failed");
        assert_eq!(rows[1]["samples"], 0);
    }
}
