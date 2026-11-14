//! Read-only ifm `IoT` Core through the actual CLI and an independent HTTP fixture.
mod support;
use serde_json::{Value, json};
#[cfg(feature = "mqtt-tls")]
use std::io::Read as _;
use std::{
    fs,
    io::Write as _,
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};

fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "ifm_iot client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn request(stream: &mut impl std::io::Read, numeric: bool) -> Value {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 16_384);
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    let header = String::from_utf8(header).unwrap();
    assert!(header.starts_with("POST / HTTP/1.1\r\n"));
    assert!(header.contains("Connection: close\r\n"));
    let length = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(length < 8192);
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["code"],
        if numeric { json!(10) } else { json!("request") }
    );
    assert_eq!(value["cid"], 1);
    assert_eq!(
        value["adr"],
        "/iolinkmaster/port[2]/iolinkdevice/pdin/getdata"
    );
    assert!(
        value.get("data").is_none(),
        "a process read cannot modify master or sensor configuration"
    );
    value
}
fn response(stream: &mut impl std::io::Write, value: &Value, chunked: bool) {
    let body = value.to_string();
    let bytes = if chunked {
        let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for part in body.as_bytes().chunks(11) {
            bytes.extend_from_slice(format!("{:x};fixture=1\r\n", part.len()).as_bytes());
            bytes.extend_from_slice(part);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"0\r\nFixture: complete\r\n\r\n");
        bytes
    } else {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    // Arbitrary TCP/TLS fragments must not become extra records.
    for part in bytes.chunks(7) {
        stream.write_all(part).unwrap();
    }
    stream.flush().unwrap();
}
fn config(listener: &TcpListener) -> Value {
    json!({"version":1,"sources":[{
        "id":"link","protocol":"ifm_iot","address":listener.local_addr().unwrap().to_string(),
        "port":2,"every_ms":3_600_000,"timeout_ms":1000,
        "signals":[
            {"name":"temperature","offset":0,"data_type":"u16","right_shift":2,"scale":0.1},
            {"name":"counter","offset":2,"data_type":"u64","byte_order":"little"},
            {"name":"state","offset":0,"data_type":"bool","bit":0}
        ]
    }]})
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
        "250ms",
        "-o",
        "run.jsonl",
    ])
}
fn readings(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample")
        .collect()
}
fn verify(home: &Home) {
    let samples = readings(home);
    assert_eq!(samples.len(), 4);
    assert_eq!(samples[0]["value"]["data_hex"], "03c9ffffffffffffffff");
    assert!((samples[1]["value"].as_f64().unwrap() - 24.2).abs() < 1e-12);
    assert_eq!(samples[2]["value"].as_u64(), Some(u64::MAX));
    assert_eq!(samples[3]["value"], true);
    for sample in &samples {
        assert_eq!(sample["elapsed_ns"], samples[0]["elapsed_ns"]);
        assert!(sample.get("source_time").is_none());
    }
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "out.jsonl",
        "--format",
        "jsonl",
    ]);
    assert!(
        fs::read_to_string(home.path("out.jsonl"))
            .unwrap()
            .contains("18446744073709551615")
    );
}

#[test]
fn bounded_post_preserves_raw_exact_and_explicit_packed_sensor_values() {
    for numeric in [false, true] {
        let home = Home::new("ifm-process-input");
        let listener = listener();
        let mut configured = config(&listener);
        if numeric {
            configured["sources"][0]["code_format"] = json!("numeric");
        }
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            assert!(request(&mut stream, numeric).get("auth").is_none());
            response(
                &mut stream,
                &json!({"cid":1,"code":200,"data":{"value":"03C9FFFFFFFFFFFFFFFF"}}),
                numeric,
            );
        });
        record(&home, &configured).success();
        server.join().unwrap();
        verify(&home);
    }
}

#[test]
fn stale_correlation_invalid_payload_and_device_diagnostics_fail_explicitly() {
    for (reply, exit, code) in [
        (
            json!({"cid":2,"code":200,"data":{"value":"03C9"}}),
            1,
            "incomplete",
        ),
        (
            json!({"cid":1,"code":200,"data":{"value":"XYZ"}}),
            1,
            "incomplete",
        ),
        (
            json!({"cid":1,"code":200,"data":{"value":"03C9"}}),
            1,
            "incomplete",
        ),
        (
            json!({"cid":1,"code":530,"data":["invalid process input"]}),
            1,
            "incomplete",
        ),
        (
            json!({"cid":1,"code":503,"data":{"message":"no device"}}),
            3,
            "unavailable",
        ),
        (json!({"cid":1,"code":401,"data":{}}), 3, "auth_rejected"),
    ] {
        let home = Home::new("ifm-invalid-input");
        let listener = listener();
        let configured = config(&listener);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, false);
            response(&mut stream, &reply, false);
        });
        let error = record(&home, &configured).failure(exit);
        assert_eq!(error["code"], code);
        server.join().unwrap();
        assert_eq!(readings(&home), Vec::<Value>::new());
    }
}

#[test]
fn malformed_http_framing_and_total_deadlines_stop_without_partial_samples() {
    for slow in [false, true] {
        let home = Home::new("ifm-http-bounds");
        let listener = listener();
        let mut configured = config(&listener);
        configured["sources"][0]["timeout_ms"] = json!(80);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, false);
            if slow {
                for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n" {
                    if stream.write_all(&[*byte]).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(15));
                }
            } else {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            }
        });
        let start = Instant::now();
        let error = record(&home, &configured).failure(if slow { 4 } else { 1 });
        assert_eq!(error["code"], if slow { "timeout" } else { "incomplete" });
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(readings(&home), Vec::<Value>::new());
    }
}

#[test]
fn invalid_mapping_and_credentials_without_tls_fail_before_connect() {
    for updates in [
        json!({"port":0}),
        json!({"username_env":"IFM_USER","password_env":"IFM_PASS"}),
        json!({"signals":[{"name":"bad","offset":31,"data_type":"u64"}]}),
        json!({"signals":[{"name":"bad","offset":0,"data_type":"i16","right_shift":16}]}),
    ] {
        let home = Home::new("ifm-config");
        let listener = listener();
        let mut configured = config(&listener);
        configured["sources"][0]
            .as_object_mut()
            .unwrap()
            .extend(updates.as_object().unwrap().clone());
        assert_eq!(record(&home, &configured).failure(2)["code"], "usage");
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[cfg(feature = "mqtt-tls")]
#[test]
fn private_ca_tls_protects_documented_json_auth_and_rejects_wrong_identity() {
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
    use std::sync::Arc;
    for wrong_name in [false, true] {
        let home = Home::new("ifm-tls-auth");
        let listener = listener();
        let mut configured = config(&listener);
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::default();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let certificate = ca.self_signed(&ca_key).unwrap();
        fs::write(home.path("ca.pem"), certificate.pem()).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["iolink.test".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.signed_by(&key, &issuer).unwrap();
        let server_config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![certificate.der().clone()],
                    PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
                )
                .unwrap();
        configured["sources"][0]["tls"] = json!({"server_name":if wrong_name {"wrong.test"} else {"iolink.test"},"ca_certificate":"ca.pem"});
        configured["sources"][0]["username_env"] = json!("CAPTUREFAB_TEST_IFM_USER");
        configured["sources"][0]["password_env"] = json!("CAPTUREFAB_TEST_IFM_PASS");
        let server = thread::spawn(move || {
            let mut stream = StreamOwned::new(
                ServerConnection::new(Arc::new(server_config)).unwrap(),
                accept(&listener),
            );
            if wrong_name {
                assert!(
                    stream.read(&mut [0; 1]).is_err(),
                    "unverified server received process-data request"
                );
            } else {
                let value = request(&mut stream, false);
                assert_eq!(value["auth"]["user"], "YWRtaW5pc3RyYXRvcg==");
                assert_eq!(value["auth"]["passwd"], "c2Vuc29yLXNlY3JldA==");
                response(
                    &mut stream,
                    &json!({"cid":1,"code":200,"data":{"value":"03C9FFFFFFFFFFFFFFFF"}}),
                    true,
                );
            }
        });
        fs::write(home.path("sources.json"), configured.to_string()).unwrap();
        let output = home
            .command()
            .env("CAPTUREFAB_TEST_IFM_USER", "administrator")
            .env("CAPTUREFAB_TEST_IFM_PASS", "sensor-secret")
            .args([
                "--json",
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "250ms",
                "-o",
                "run.jsonl",
            ])
            .output()
            .unwrap();
        let run = Run::of_output(output, &["data", "record"]);
        if wrong_name {
            assert_eq!(run.failure(3)["code"], "auth_rejected");
        } else {
            run.success();
            verify(&home);
        }
        server.join().unwrap();
        let rows = fs::read_to_string(home.path("run.jsonl")).unwrap();
        assert!(!rows.contains("sensor-secret"));
        assert!(!rows.contains("c2Vuc29yLXNlY3JldA=="));
    }
}
