//! MQTT authentication and verified TLS through a complete CLI recording.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

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
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "MQTT client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn packet(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut prefix = [0; 2];
    stream.read_exact(&mut prefix).unwrap();
    assert!(prefix[1] < 128, "small fixture packet expected");
    let mut body = vec![0; usize::from(prefix[1])];
    stream.read_exact(&mut body).unwrap();
    (prefix[0], body)
}

fn broker(stream: &mut (impl Read + Write), authenticated: bool) {
    let (kind, body) = packet(stream);
    assert_eq!(kind, 0x10);
    assert_eq!(
        &body[..10],
        &[
            0,
            4,
            b'M',
            b'Q',
            b'T',
            b'T',
            4,
            if authenticated { 0xc2 } else { 2 },
            0,
            30
        ]
    );
    let mut expected = Vec::new();
    for text in if authenticated {
        vec!["fixture", "reader", "sensitive-mqtt-password"]
    } else {
        vec!["fixture"]
    } {
        expected.extend_from_slice(&u16::try_from(text.len()).unwrap().to_be_bytes());
        expected.extend_from_slice(text.as_bytes());
    }
    assert_eq!(&body[10..], expected);
    stream.write_all(&[0x20, 2, 0, 0]).unwrap();
    stream.flush().unwrap();
    let (kind, body) = packet(stream);
    assert_eq!(kind, 0x82);
    assert_eq!(body, [0, 1, 0, 4, b'l', b'i', b'n', b'e', 1]);
    stream
        .write_all(&[
            0x90, 3, 0, 1, 1, 0x32, 10, 0, 4, b'l', b'i', b'n', b'e', 0, 7, b'4', b'2',
        ])
        .unwrap();
    stream.flush().unwrap();
    assert_eq!(packet(stream), (0x40, vec![0, 7]));
    let mut end = [0];
    let _ = stream.read(&mut end); // Client closes at the common cutoff.
}

fn source(listener: &TcpListener) -> Value {
    json!({"id":"sensor", "protocol":"mqtt", "broker":listener.local_addr().unwrap().to_string(),
        "topics":["line"], "client_id":"fixture", "timeout_ms":1000})
}

fn record(home: &Home, source: &Value) -> support::cli::Run {
    fs::write(
        home.work.join("config.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
    home.run_env(
        &[
            ("MQTT_TEST_USER", "reader"),
            ("MQTT_TEST_PASSWORD", "sensitive-mqtt-password"),
        ],
        &[
            "--json",
            "data",
            "record",
            "--config",
            "config.json",
            "--for",
            "300ms",
            "-o",
            "run.jsonl",
        ],
    )
}

fn verify_samples(home: &Home) {
    let text = fs::read_to_string(home.work.join("run.jsonl")).unwrap();
    assert!(!text.contains("sensitive-mqtt-password"));
    assert!(!text.contains("MQTT_TEST_PASSWORD"));
    let rows: Vec<Value> = text
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
    let samples: Vec<_> = rows.iter().filter(|row| row["kind"] == "sample").collect();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0]["value"], 42);
    assert_eq!(samples[0]["signal"], "line");
    assert_eq!(rows.last().unwrap()["status"], "complete");
    let exported = home.ok(&["data", "export", "run.jsonl", "-o", "samples.csv"]);
    assert_eq!(exported["samples"], 1);
}

fn qos2_handshake(stream: &mut TcpStream) {
    assert_eq!(packet(stream).0, 0x10);
    stream.write_all(&[0x20, 2, 0, 0]).unwrap();
    let (header, subscribe) = packet(stream);
    assert_eq!(header, 0x82);
    assert_eq!(subscribe, [0, 1, 0, 4, b'l', b'i', b'n', b'e', 2]);
    stream.write_all(&[0x90, 3, 0, 1, 2]).unwrap();
}

fn qos2_publish(stream: &mut TcpStream, id: u16, duplicate: bool, literal: &str) {
    let mut bytes = vec![
        if duplicate { 0x3c } else { 0x34 },
        0,
        0,
        4,
        b'l',
        b'i',
        b'n',
        b'e',
    ];
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(literal.as_bytes());
    bytes[1] = u8::try_from(bytes.len() - 2).unwrap();
    stream.write_all(&bytes).unwrap();
}

fn qos2_release(stream: &mut TcpStream, id: u16) {
    let [high, low] = id.to_be_bytes();
    stream.write_all(&[0x62, 2, high, low]).unwrap();
    assert_eq!(packet(stream), (0x70, vec![high, low]));
}

#[test]
fn qos2_duplicates_are_acknowledged_once_per_publication_and_ids_can_be_reused() {
    let home = Home::new("mqtt-qos2");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = source(&listener);
    config["qos"] = json!(2);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        qos2_handshake(&mut stream);
        qos2_publish(&mut stream, u16::MAX, false, "41");
        assert_eq!(packet(&mut stream), (0x50, vec![0xff, 0xff]));
        // Duplicate PUBLISH must be recognized by ID, even if a broker omits
        // its DUP flag. An unrelated PUBREL must not release that stored ID.
        qos2_publish(&mut stream, u16::MAX, false, "41");
        assert_eq!(packet(&mut stream), (0x50, vec![0xff, 0xff]));
        qos2_release(&mut stream, 7);
        qos2_publish(&mut stream, u16::MAX, true, "41");
        assert_eq!(packet(&mut stream), (0x50, vec![0xff, 0xff]));
        // A second identifier can be in flight at the same time.
        qos2_publish(&mut stream, 1, false, "42");
        assert_eq!(packet(&mut stream), (0x50, vec![0, 1]));
        qos2_release(&mut stream, u16::MAX);
        qos2_release(&mut stream, u16::MAX);
        // After PUBCOMP, even DUP=1 with the same ID is a new publication.
        qos2_publish(&mut stream, u16::MAX, true, "18446744073709551617");
        assert_eq!(packet(&mut stream), (0x50, vec![0xff, 0xff]));
        qos2_release(&mut stream, 1);
        qos2_release(&mut stream, u16::MAX);
        let _ = stream.read(&mut [0]);
    });
    record(&home, &config).success();
    server.join().unwrap();
    let text = fs::read_to_string(home.work.join("run.jsonl")).unwrap();
    let rows: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let samples: Vec<_> = rows.iter().filter(|row| row["kind"] == "sample").collect();
    assert_eq!(samples.len(), 3, "{text}");
    assert_eq!(samples[0]["value"], 41);
    assert_eq!(samples[1]["value"], 42);
    assert_eq!(samples[2]["value"].to_string(), "18446744073709551617");
    assert_eq!(
        home.ok(&["data", "export", "run.jsonl", "-o", "samples.csv"])["samples"],
        3
    );
    assert!(
        fs::read_to_string(home.work.join("samples.csv"))
            .unwrap()
            .contains("18446744073709551617")
    );
}

#[test]
fn qos2_payload_is_recorded_at_publish_without_waiting_for_pubrel() {
    let home = Home::new("mqtt-qos2-cutoff");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = source(&listener);
    config["qos"] = json!(2);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        qos2_handshake(&mut stream);
        qos2_publish(&mut stream, 7, false, "42");
        assert_eq!(packet(&mut stream), (0x50, vec![0, 7]));
        // Method B delivers at PUBLISH. No PUBREL arrives before cutoff.
        let _ = stream.read(&mut [0]);
    });
    record(&home, &config).success();
    server.join().unwrap();
    verify_samples(&home);
}

#[test]
fn qos2_broker_disconnect_is_explicit_and_keeps_the_received_publication() {
    let home = Home::new("mqtt-qos2-disconnected");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = source(&listener);
    config["qos"] = json!(2);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        qos2_handshake(&mut stream);
        qos2_publish(&mut stream, 7, false, "42");
        assert_eq!(packet(&mut stream), (0x50, vec![0, 7]));
    });
    let error = record(&home, &config).failure(3);
    assert_eq!(error["code"], "disconnected");
    assert_eq!(error["details"]["samples"], 1);
    server.join().unwrap();
    let text = fs::read_to_string(home.work.join("run.jsonl")).unwrap();
    let rows: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.iter().filter(|row| row["kind"] == "sample").count(), 1);
    assert_eq!(rows.last().unwrap()["status"], "failed");
}

#[test]
fn environment_credentials_authenticate_on_plain_tcp() {
    let home = Home::new("mqtt-auth");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = source(&listener);
    config["username_env"] = json!("MQTT_TEST_USER");
    config["password_env"] = json!("MQTT_TEST_PASSWORD");
    let server = thread::spawn(move || broker(&mut accept(&listener), true));
    let result = record(&home, &config);
    result.success();
    assert!(!result.stdout.contains("sensitive-mqtt-password"));
    assert!(!result.stderr.contains("sensitive-mqtt-password"));
    server.join().unwrap();
    verify_samples(&home);
}

#[test]
fn missing_credentials_fail_without_echoing_or_connecting() {
    let home = Home::new("mqtt-missing-auth");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = source(&listener);
    config["username_env"] = json!("CAPTUREFAB_MQTT_TEST_UNSET_CREDENTIAL");
    let result = record(&home, &config);
    assert_eq!(result.failure(3)["code"], "auth_required");
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(feature = "mqtt-tls")]
mod tls {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::{
        RootCertStore, ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer,
    };
    use std::sync::Arc;

    fn certificates(home: &Home, mutual: bool) -> (Value, Arc<ServerConfig>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::default();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        fs::write(home.work.join("ca.pem"), ca_cert.pem()).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["broker.test".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_cert = params.signed_by(&server_key, &issuer).unwrap();
        let builder =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap();
        let builder = if mutual {
            let mut roots = RootCertStore::empty();
            roots.add(ca_cert.der().clone()).unwrap();
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::new(rustls::crypto::ring::default_provider()),
            )
            .build()
            .unwrap();
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let server = builder
            .with_single_cert(
                vec![server_cert.der().clone()],
                PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
            )
            .unwrap();
        let mut config = json!({"server_name":"broker.test", "ca_certificate":"ca.pem"});
        if mutual {
            let client_key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(vec!["reader.test".into()]).unwrap();
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            let client_cert = params.signed_by(&client_key, &issuer).unwrap();
            fs::write(home.work.join("client.pem"), client_cert.pem()).unwrap();
            fs::write(home.work.join("client-key.pem"), client_key.serialize_pem()).unwrap();
            config["client_certificate"] = json!("client.pem");
            config["client_key"] = json!("client-key.pem");
        }
        (config, Arc::new(server))
    }

    #[test]
    fn private_ca_tls_and_client_certificates_deliver_samples() {
        for mutual in [false, true] {
            let home = Home::new("mqtt-tls");
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut config = source(&listener);
            let (tls, server_config) = certificates(&home, mutual);
            config["tls"] = tls;
            let server = thread::spawn(move || {
                let mut stream = StreamOwned::new(
                    ServerConnection::new(server_config).unwrap(),
                    accept(&listener),
                );
                broker(&mut stream, false);
                assert_eq!(stream.conn.peer_certificates().is_some(), mutual);
            });
            record(&home, &config).success();
            server.join().unwrap();
            verify_samples(&home);
        }
    }

    #[test]
    fn rejects_wrong_server_identity_and_untrusted_ca_before_mqtt_connect() {
        for wrong_name in [false, true] {
            let home = Home::new("mqtt-untrusted");
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut config = source(&listener);
            let (mut tls, server_config) = certificates(&home, false);
            if wrong_name {
                tls["server_name"] = json!("wrong.test");
            } else {
                tls.as_object_mut().unwrap().remove("ca_certificate");
            }
            config["tls"] = tls;
            let server = thread::spawn(move || {
                let mut stream = StreamOwned::new(
                    ServerConnection::new(server_config).unwrap(),
                    accept(&listener),
                );
                assert!(
                    stream.read(&mut [0; 1]).is_err(),
                    "unverified server received MQTT data"
                );
            });
            let result = record(&home, &config);
            assert_eq!(result.failure(3)["code"], "auth_rejected");
            server.join().unwrap();
            let rows = fs::read_to_string(home.work.join("run.jsonl")).unwrap();
            assert!(!rows.contains("\"kind\":\"sample\""));
        }
    }

    #[test]
    fn stalled_tls_handshake_respects_the_total_read_deadline() {
        let home = Home::new("mqtt-tls-timeout");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = source(&listener);
        config["tls"] = json!({"server_name":"broker.test"});
        config["timeout_ms"] = json!(80);
        let (stop, finish) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let _stream = accept(&listener);
            finish.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let start = Instant::now();
        let result = record(&home, &config);
        stop.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(result.failure(4)["code"], "timeout");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(not(feature = "mqtt-tls"))]
#[test]
fn tls_configuration_fails_explicitly_in_a_build_without_tls() {
    let home = Home::new("mqtt-no-tls");
    let config = json!({"id":"sensor", "protocol":"mqtt", "broker":"127.0.0.1:1883",
        "topics":["line"], "tls":{"server_name":"broker.test"}});
    assert_eq!(record(&home, &config).failure(5)["code"], "unsupported");
    assert!(!home.work.join("run.jsonl").exists());
}
