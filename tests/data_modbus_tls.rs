//! Verified Modbus TCP over TLS through the recording CLI.
mod support;

use serde_json::{Value, json};
use std::{fs, net::TcpListener};
use support::cli::{Home, Run};

fn record(home: &Home, source: &Value) -> Run {
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
        "2s",
        "-o",
        "run.jsonl",
    ])
}

fn source(listener: &TcpListener) -> Value {
    json!({"id":"meter", "protocol":"modbus_tcp", "address":listener.local_addr().unwrap().to_string(),
        "every_ms":3_600_000, "timeout_ms":1000, "signals":[{"name":"holding","address":0}],
        "tls":{"server_name":"meter.test"}})
}

#[cfg(feature = "mqtt-tls")]
mod tls {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, ExtendedKeyUsagePurpose, IsCa,
        Issuer, KeyPair, KeyUsagePurpose,
    };
    use rustls::{
        RootCertStore, ServerConfig, ServerConnection, StreamOwned,
        pki_types::{CertificateDer, PrivatePkcs8KeyDer},
        server::WebPkiClientVerifier,
    };
    use std::{
        io::{self, Read as _},
        net::TcpStream,
        sync::{Arc, mpsc},
        thread,
        time::{Duration, Instant},
    };

    fn certificates(
        home: &Home,
        mutual: bool,
    ) -> (Value, Arc<ServerConfig>, Option<CertificateDer<'static>>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::default();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        fs::write(home.path("ca.pem"), ca_cert.pem()).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["meter.test".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_cert = params.signed_by(&server_key, &issuer).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap();
        let builder = if mutual {
            let mut roots = RootCertStore::empty();
            roots.add(ca_cert.der().clone()).unwrap();
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
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
        let mut tls = json!({"server_name":"meter.test", "ca_certificate":"ca.pem"});
        let client_cert = mutual.then(|| {
            let client_key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(vec!["reader.test".into()]).unwrap();
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            // Modbus Security v36 §8.4 uses a DER UTF8String for a provisioned role.
            params
                .custom_extensions
                .push(CustomExtension::from_oid_content(
                    &[1, 3, 6, 1, 4, 1, 50_316, 802, 1],
                    b"\x0c\x08Operator".to_vec(),
                ));
            let certificate = params.signed_by(&client_key, &issuer).unwrap();
            fs::write(home.path("client.pem"), certificate.pem()).unwrap();
            fs::write(home.path("client-key.pem"), client_key.serialize_pem()).unwrap();
            tls["client_certificate"] = json!("client.pem");
            tls["client_key"] = json!("client-key.pem");
            certificate.der().clone()
        });
        (tls, Arc::new(server), client_cert)
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
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "Modbus TLS client did not connect"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn request(stream: &mut impl io::Read, transaction: u8, unit: u8, pdu: [u8; 5]) {
        let mut bytes = [0; 12];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes[..7], [0, transaction, 0, 0, 0, 6, unit]);
        assert_eq!(bytes[7..], pdu);
    }

    fn response(
        stream: &mut impl io::Write,
        transaction: u8,
        unit: u8,
        pdu: &[u8],
    ) -> io::Result<()> {
        let length = u8::try_from(pdu.len() + 1).map_err(io::Error::other)?;
        stream.write_all(&[0, transaction, 0, 0, 0])?;
        stream.flush()?;
        stream.write_all(&[length, unit])?;
        stream.flush()?;
        stream.write_all(pdu)?;
        stream.flush()
    }

    fn rows(home: &Home) -> Vec<Value> {
        fs::read_to_string(home.path("run.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_unpublished(home: &Home) {
        let rows = rows(home);
        assert!(rows.iter().all(|row| row["kind"] != "sample"));
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }

    #[test]
    fn private_ca_and_mutual_tls_keep_standard_mbap_requests() {
        for mutual in [false, true] {
            let home = Home::new("modbus-tls");
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut source = source(&listener);
            let (tls, server, client_cert) = certificates(&home, mutual);
            source["tls"] = tls;
            let unit = if mutual { 255 } else { 1 };
            if mutual {
                source["unit"] = json!(unit);
            }
            source["signals"] = json!([
                {"name":"coil", "address":16, "table":"coil", "data_type":"bool"},
                {"name":"discrete", "address":17, "table":"discrete", "data_type":"bool"},
                {"name":"holding", "address":18},
                {"name":"input", "address":19, "table":"input"}
            ]);
            let server = thread::spawn(move || {
                let mut stream =
                    StreamOwned::new(ServerConnection::new(server).unwrap(), accept(&listener));
                for (transaction, pdu, reply) in [
                    (1, [1, 0, 16, 0, 1], &[1, 1, 1][..]),
                    (2, [2, 0, 17, 0, 1], &[2, 1, 0]),
                    (3, [3, 0, 18, 0, 1], &[3, 2, 0x12, 0x34]),
                    (4, [4, 0, 19, 0, 1], &[4, 2, 0, 42]),
                ] {
                    request(&mut stream, transaction, unit, pdu);
                    response(&mut stream, transaction, unit, reply).unwrap();
                }
                assert_eq!(
                    stream
                        .conn
                        .peer_certificates()
                        .and_then(|chain| chain.first()),
                    client_cert.as_ref()
                );
            });
            let run = record(&home, &source);
            server.join().unwrap();
            run.success();
            let rows = rows(&home);
            let values: Vec<_> = rows
                .iter()
                .filter(|row| row["kind"] == "sample")
                .map(|row| (&row["signal"], &row["value"]))
                .collect();
            assert_eq!(
                values,
                [
                    (&json!("coil"), &json!(true)),
                    (&json!("discrete"), &json!(false)),
                    (&json!("holding"), &json!(4660.0)),
                    (&json!("input"), &json!(42.0)),
                ]
            );
        }
    }

    #[test]
    fn invalid_tls_identity_or_missing_client_certificate_blocks_modbus() {
        for invalid in ["server name", "server CA", "client certificate"] {
            let home = Home::new("modbus-tls-rejected");
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut source = source(&listener);
            let (mut tls, server, _) = certificates(&home, true);
            match invalid {
                "server name" => tls["server_name"] = json!("other.test"),
                "server CA" => {
                    tls.as_object_mut().unwrap().remove("ca_certificate");
                }
                _ => {
                    tls.as_object_mut().unwrap().remove("client_certificate");
                    tls.as_object_mut().unwrap().remove("client_key");
                }
            }
            source["tls"] = tls;
            let server = thread::spawn(move || {
                let mut stream =
                    StreamOwned::new(ServerConnection::new(server).unwrap(), accept(&listener));
                assert!(
                    stream.read(&mut [0]).is_err(),
                    "received Modbus data for {invalid}"
                );
            });
            let run = record(&home, &source);
            server.join().unwrap();
            assert_eq!(run.failure(3)["code"], "auth_rejected");
            assert_unpublished(&home);
        }
    }

    #[test]
    fn tls_poll_shares_the_deadline_and_discards_a_successful_sibling() {
        let home = Home::new("modbus-tls-poll-timeout");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut source = source(&listener);
        let (tls, server, _) = certificates(&home, false);
        source["tls"] = tls;
        source["timeout_ms"] = json!(700);
        source["signals"] =
            json!([{"name":"holding","address":0},{"name":"input","address":1,"table":"input"}]);
        let server = thread::spawn(move || {
            let mut stream =
                StreamOwned::new(ServerConnection::new(server).unwrap(), accept(&listener));
            request(&mut stream, 1, 1, [3, 0, 0, 0, 1]);
            thread::sleep(Duration::from_millis(400));
            response(&mut stream, 1, 1, &[3, 2, 0, 7]).unwrap();
            request(&mut stream, 2, 1, [4, 0, 1, 0, 1]);
            thread::sleep(Duration::from_millis(400));
            let _ = response(&mut stream, 2, 1, &[4, 2, 0, 9]);
        });
        let run = record(&home, &source);
        server.join().unwrap();
        assert_eq!(run.failure(4)["code"], "timeout");
        assert_unpublished(&home);
    }

    #[test]
    fn stalled_tls_handshake_uses_the_poll_deadline() {
        let home = Home::new("modbus-tls-handshake-timeout");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut source = source(&listener);
        source["timeout_ms"] = json!(100);
        let (stop, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let mut prefix = [0];
            stream.read_exact(&mut prefix).unwrap();
            assert_eq!(prefix, [0x16]); // TLS handshake record, before any MBAP ADU.
            finish.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let run = record(&home, &source);
        stop.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(run.failure(4)["code"], "timeout");
        assert_unpublished(&home);
    }
}

#[cfg(not(feature = "mqtt-tls"))]
#[test]
fn tls_configuration_requires_the_tls_build_feature() {
    let home = Home::new("modbus-no-tls");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    assert_eq!(
        record(&home, &source(&listener)).failure(5)["code"],
        "unsupported"
    );
    assert!(!home.path("run.jsonl").exists());
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
