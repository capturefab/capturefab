use super::*;
use crate::hubs::fake_http::{self, FakeHttp, Reply};
use crate::hubs::registry::parse_url;
use std::time::Duration;

fn endpoint(fake: &FakeHttp) -> Endpoint {
    Endpoint::new(
        "Test hub t",
        parse_url(&fake.url()).unwrap(),
        &tls::Trust::Platform,
    )
    .unwrap()
}
fn quick() -> Limits {
    Limits::request(Duration::from_secs(5), 1 << 20)
}
fn get(target: &str) -> Request<'static> {
    Request::get("the test resource", target.into(), "*/*", quick())
}
fn fetch(fake: &FakeHttp, target: &str) -> Result<(u16, Vec<u8>), HttpError> {
    let response = send(&endpoint(fake), &get(target), &Auth::None, None)?;
    let status = response.status;
    Ok((status, response.body.read_all(1 << 20)?))
}

#[test]
fn bodies_are_framed_by_length_chunks_or_close() {
    let fake = FakeHttp::start(|request| {
        match request.path() {
        "/length" => Reply::text(200, "hello"),
        "/chunked" => Reply::new(200).chunked(b"chunked body".to_vec(), 1),
        "/extensions" => Reply::raw(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;name=value\r\nabc\r\n0\r\nTrailer: x\r\n\r\n",
        ),
        "/eof" => Reply::raw(b"HTTP/1.0 200 OK\r\n\r\nuntil close"),
        "/interim" => Reply::raw(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"),
        "/empty" => Reply::raw(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"),
        "/bare-lf" => Reply::raw(b"HTTP/1.1 200\nContent-Length: 3\n\nabc"),
        _ => Reply::new(404),
    }
    });
    assert_eq!(fetch(&fake, "/length").unwrap(), (200, b"hello".to_vec()));
    assert_eq!(
        fetch(&fake, "/chunked").unwrap(),
        (200, b"chunked body".to_vec())
    );
    assert_eq!(fetch(&fake, "/extensions").unwrap(), (200, b"abc".to_vec()));
    assert_eq!(
        fetch(&fake, "/eof").unwrap(),
        (200, b"until close".to_vec())
    );
    assert_eq!(fetch(&fake, "/interim").unwrap(), (200, b"ok".to_vec()));
    assert_eq!(fetch(&fake, "/empty").unwrap(), (204, Vec::new()));
    assert_eq!(fetch(&fake, "/bare-lf").unwrap(), (200, b"abc".to_vec()));
    assert_eq!(fetch(&fake, "/missing").unwrap().0, 404);
}

#[test]
fn malformed_or_encoded_responses_are_refused() {
    let many = {
        let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
        for index in 0..101 {
            raw.extend(format!("X-{index}: v\r\n").as_bytes());
        }
        raw.extend(b"Content-Length: 0\r\n\r\n");
        raw
    };
    let fake = FakeHttp::start(move |request| match request.path() {
        "/folded" => {
            Reply::raw(b"HTTP/1.1 200 OK\r\nX-A: one\r\n two\r\nContent-Length: 0\r\n\r\n")
        }
        "/lengths" => {
            Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabcd")
        }
        "/gzip" => {
            Reply::raw(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n")
        }
        "/compressed" => {
            Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\n\r\n")
        }
        "/status" => Reply::raw(b"HTTP/2 200 OK\r\n\r\n"),
        "/truncated" => Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc"),
        "/huge" => Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n"),
        "/many" => Reply::raw(&many),
        "/closed" => Reply::raw(b""),
        _ => Reply::new(404),
    });
    for (path, needle) in [
        ("/folded", "folded header"),
        ("/lengths", "conflicting lengths"),
        ("/gzip", "unsupported transfer coding"),
        ("/compressed", "compressed response"),
        ("/status", "bad status line"),
        ("/truncated", "truncated body"),
        ("/huge", "sent more than"),
        ("/many", "too many headers"),
        ("/closed", "closed the connection before answering"),
    ] {
        let error = fetch(&fake, path).unwrap_err();
        assert!(error.message().contains(needle), "{path}: {error}");
        assert!(
            !error.message().contains(path),
            "{path}: messages never name paths"
        );
    }
}

#[test]
fn repeated_headers_stay_separate() {
    let fake = FakeHttp::start(|_| {
        Reply::new(401)
            .header("WWW-Authenticate", "Digest realm=\"a\", nonce=\"1\"")
            .header("WWW-Authenticate", "Basic realm=\"b\"")
            .header("Set-Cookie", "a=1")
            .header("Set-Cookie", "b=2")
    });
    let response = send(&endpoint(&fake), &get("/"), &Auth::None, None).unwrap();
    assert_eq!(response.headers("www-authenticate").count(), 2);
    assert_eq!(
        response.headers("set-cookie").collect::<Vec<_>>(),
        ["a=1", "b=2"]
    );
}

#[test]
fn stalled_servers_time_out_by_phase() {
    let fake = FakeHttp::start(|request| match request.path() {
        "/slow-head" => Reply::new(200).stall(Duration::from_secs(3)),
        _ => Reply::new(200).stream(Box::new(|out| {
            out.write_all(b"part")?;
            std::thread::sleep(Duration::from_secs(3));
            Ok(())
        })),
    });
    let short = |headers: u64, idle: u64, total: Option<u64>| Limits {
        headers: Duration::from_millis(headers),
        idle: Duration::from_millis(idle),
        total: total.map(Duration::from_millis),
        max_body: None,
    };
    // The response head never arrives.
    let start = Instant::now();
    let request = Request::get(
        "the head",
        "/slow-head".into(),
        "*/*",
        short(300, 300, None),
    );
    let error = send(&endpoint(&fake), &request, &Auth::None, None)
        .err()
        .expect("head timeout");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(error.kind(), ErrorKind::Timeout);
    assert!(
        error
            .message()
            .contains("did not answer within 300 ms (timed out)")
    );
    // The body stops arriving.
    let request = Request::get(
        "the stream",
        "/stream".into(),
        "*/*",
        short(2000, 300, None),
    );
    let response = send(&endpoint(&fake), &request, &Auth::None, None).unwrap();
    let error = response.body.read_all(1 << 20).unwrap_err();
    assert!(
        error
            .message()
            .contains("sent nothing for the stream in 300 ms"),
        "{error}"
    );
    // The body takes too long overall.
    let request = Request::get(
        "the stream",
        "/stream".into(),
        "*/*",
        short(2000, 2000, Some(500)),
    );
    let response = send(&endpoint(&fake), &request, &Auth::None, None).unwrap();
    let error = response.body.read_all(1 << 20).unwrap_err();
    assert!(error.message().contains("within 500 ms"), "{error}");
    assert_eq!(crate::cli::error_code(&error.into_error()), ("timeout", 4));
}

#[test]
fn drip_fed_lines_cannot_extend_absolute_deadlines() {
    use std::net::TcpListener;

    for phase in ["head", "chunk", "trailer"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = parse_url(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let ep = Endpoint::new("Test hub t", url, &tls::Trust::Platform).unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut socket = BufReader::new(socket);
            loop {
                if read_line(&mut socket, MAX_HEADER_LINE).unwrap().is_empty() {
                    break;
                }
            }
            let socket = socket.get_mut();
            let line = match phase {
                "head" => [b"HTTP/1.1 200 ".as_slice(), &[b'x'; 100], b"\r\n\r\n"].concat(),
                "chunk" => {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                        .unwrap();
                    [b"1;".as_slice(), &[b'x'; 100], b"\r\na\r\n0\r\n\r\n"].concat()
                }
                _ => {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n")
                        .unwrap();
                    [b"Trailer: ".as_slice(), &[b'x'; 100], b"\r\n\r\n"].concat()
                }
            };
            for byte in line {
                if socket.write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let request = Request::get(
            "the test resource",
            "/".into(),
            "*/*",
            Limits::request(Duration::from_millis(250), 1 << 20),
        );
        let start = Instant::now();
        let error = send(&ep, &request, &Auth::None, None)
            .and_then(|response| response.body.read_all(1 << 20))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Timeout, "{phase}: {error}");
        assert!(start.elapsed() < Duration::from_secs(1), "{phase}");
        server.join().unwrap();
    }
}

#[test]
fn a_blocked_body_read_is_cancelled_promptly() {
    // The head arrives; the body never does.
    let fake = FakeHttp::start(|_| {
        Reply::new(200).stream(Box::new(|_| {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }))
    });
    let request = Request::get(
        "the stream",
        "/".into(),
        "*/*",
        Limits {
            headers: Duration::from_secs(5),
            idle: Duration::from_secs(5),
            total: None,
            max_body: None,
        },
    );
    let cancel = Arc::new(Cancel::default());
    let response = send(&endpoint(&fake), &request, &Auth::None, Some(&cancel)).unwrap();
    let canceller = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        canceller.cancel();
    });
    let start = Instant::now();
    assert!(response.body.read_all(1 << 20).is_err());
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "cancelling took {elapsed:?}"
    );
    // A cancelled handle refuses later requests without connecting.
    let before = fake.connections();
    let error = send(&endpoint(&fake), &get("/"), &Auth::None, Some(&cancel))
        .err()
        .expect("cancelled");
    assert_eq!(error.problem, Problem::Cancelled);
    assert_eq!(fake.connections(), before);
}

#[test]
fn unreachable_servers_and_redirects_have_fixed_messages() {
    let url = parse_url(&format!("http://127.0.0.1:{}", fake_http::refused_port())).unwrap();
    let ep = Endpoint::new("Test hub t", url, &tls::Trust::Platform).unwrap();
    let error = send(&ep, &get("/api/"), &Auth::None, None)
        .err()
        .expect("refused");
    assert!(error.message().contains("connection refused"), "{error}");
    assert_eq!(
        crate::cli::error_code(&error.into_error()),
        ("unavailable", 3)
    );
    let fake = FakeHttp::start(|request| match request.path() {
        "/away" => {
            Reply::new(307).header("Location", "https://other.example:8443/api/?token=secret")
        }
        _ => Reply::new(301).header("Location", "/relative?token=secret"),
    });
    let error = fetch(&fake, "/away").unwrap_err();
    assert_eq!(
        error.message(),
        "Test hub t answered with a redirect to other.example:8443 (HTTP 307); add the hub with that address"
    );
    let error = fetch(&fake, "/here").unwrap_err();
    assert!(!error.message().contains("secret") && !error.message().contains("relative"));
    assert!(error.permanent());
    assert_eq!(fake.requests().len(), 2, "redirects are never followed");
}

#[test]
fn requests_are_minimal_and_never_carry_injected_lines() {
    let fake = FakeHttp::start(|_| Reply::text(200, "ok"));
    let secret = Secret::new(crate::hubs::Kind::Go2rtc, "CANARY password".into()).unwrap();
    let response = send(
        &endpoint(&fake),
        &get("/api/streams?src=a%20b"),
        &Auth::Basic {
            user: "admin",
            password: &secret,
        },
        None,
    )
    .unwrap();
    response.body.read_all(64).unwrap();
    let seen = &fake.requests()[0];
    assert_eq!(seen.target, "/api/streams?src=a%20b");
    let names = seen
        .headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "host",
            "user-agent",
            "accept",
            "connection",
            "authorization"
        ]
    );
    assert_eq!(seen.header("connection"), Some("close"));
    assert!(seen.header("authorization").unwrap().starts_with("Basic "));
    for target in ["/a b", "/a\r\nHost: evil", "relative", "/\u{7f}"] {
        let error = send(&endpoint(&fake), &get(target), &Auth::None, None)
            .err()
            .expect("refused");
        assert_eq!(error.problem, Problem::InvalidRequest, "{target:?}");
    }
    let error = send(
        &endpoint(&fake),
        &get("/"),
        &Auth::Bearer("token\r\nX-Evil: 1"),
        None,
    )
    .err()
    .expect("refused");
    assert_eq!(error.problem, Problem::InvalidRequest);
    assert_eq!(
        fake.requests().len(),
        1,
        "nothing was sent for invalid requests"
    );
}

#[test]
fn ipv6_hosts_are_bracketed() {
    let Ok(listener) = std::net::TcpListener::bind("[::1]:0") else {
        return;
    };
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        let body = if head.contains(&format!("Host: [::1]:{port}\r\n")) {
            "yes"
        } else {
            "no!"
        };
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{body}");
    });
    let url = parse_url(&format!("http://[::1]:{port}")).unwrap();
    let ep = Endpoint::new("Test hub t", url, &tls::Trust::Platform).unwrap();
    let response = send(&ep, &get("/"), &Auth::None, None).unwrap();
    assert_eq!(response.body.read_all(8).unwrap(), b"yes");
}

fn tls_endpoint(fake: &FakeHttp, trust: tls::Trust) -> Endpoint {
    Endpoint::new("Test hub t", parse_url(&fake.url()).unwrap(), &trust).unwrap()
}
fn pin(text: &str) -> [u8; 32] {
    crate::hubs::registry::parse_fingerprint(text).unwrap()
}

#[test]
fn self_signed_certificates_are_refused_with_their_fingerprint_unless_pinned() {
    let fake = FakeHttp::start_tls(fake_http::localhost(), |_| Reply::text(200, "secure"));
    let error = send(
        &tls_endpoint(&fake, tls::Trust::Platform),
        &get("/"),
        &Auth::None,
        None,
    )
    .err()
    .expect("untrusted");
    let Problem::Certificate(rejected) = error.problem else {
        panic!("{error}");
    };
    assert_eq!(rejected.offered, pin(fake_http::LOCALHOST_SHA256));
    assert_eq!(rejected.reason, tls::Reason::Untrusted);
    assert!(
        error.message().contains(fake_http::LOCALHOST_SHA256),
        "{error}"
    );
    assert!(error.message().contains("--accept-certificate"));
    assert!(
        fake.requests().is_empty(),
        "nothing is sent over an untrusted connection"
    );
    let pinned = tls_endpoint(&fake, tls::Trust::Pinned(pin(fake_http::LOCALHOST_SHA256)));
    let response = send(&pinned, &get("/"), &Auth::None, None).unwrap();
    assert_eq!(response.body.read_all(64).unwrap(), b"secure");
    // A different certificate is refused, with both fingerprints.
    let other = FakeHttp::start_tls(fake_http::other(), |_| Reply::text(200, "imposter"));
    let ep = tls_endpoint(&other, tls::Trust::Pinned(pin(fake_http::LOCALHOST_SHA256)));
    let error = send(&ep, &get("/"), &Auth::Bearer("CANARY-token"), None)
        .err()
        .expect("changed");
    assert!(
        error
            .message()
            .contains("presented a different certificate"),
        "{error}"
    );
    assert!(error.message().contains(fake_http::LOCALHOST_SHA256));
    assert!(error.message().contains(fake_http::OTHER_SHA256));
    assert!(error.permanent());
    assert!(
        other.requests().is_empty(),
        "the token never reached the imposter"
    );
}

#[test]
fn a_pin_still_requires_the_matching_private_key() {
    use rustls::{
        server::{ClientHello, ResolvesServerCert},
        sign::CertifiedKey,
    };
    #[derive(Debug)]
    struct Mismatched(Arc<CertifiedKey>);
    impl ResolvesServerCert for Mismatched {
        fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = provider
        .key_provider
        .load_private_key(rustls::pki_types::PrivateKeyDer::Pkcs8(
            fake_http::other().key.into(),
        ))
        .unwrap();
    let certified = CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(
            fake_http::localhost().cert,
        )],
        key,
    );
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Mismatched(Arc::new(certified)))),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicBool::new(false));
    let flag = served.clone();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let connection = rustls::ServerConnection::new(config).unwrap();
        let mut tls = rustls::StreamOwned::new(connection, stream);
        let mut buf = [0u8; 64];
        if matches!(tls.read(&mut buf), Ok(n) if n > 0) {
            flag.store(true, Ordering::SeqCst);
        }
    });
    let url = parse_url(&format!("https://localhost:{port}")).unwrap();
    let ep = Endpoint::new(
        "Test hub t",
        url,
        &tls::Trust::Pinned(pin(fake_http::LOCALHOST_SHA256)),
    )
    .unwrap();
    let error = send(&ep, &get("/"), &Auth::None, None)
        .err()
        .expect("signature check");
    assert!(matches!(error.problem, Problem::Tls(_)), "{error}");
    std::thread::sleep(Duration::from_millis(50));
    assert!(!served.load(Ordering::SeqCst));
}

#[test]
fn frigate_style_certificates_can_be_pinned() {
    let fake = FakeHttp::start_tls(fake_http::frigate_default(), |_| {
        Reply::text(200, "frigate")
    });
    let error = send(
        &tls_endpoint(&fake, tls::Trust::Platform),
        &get("/"),
        &Auth::None,
        None,
    )
    .err()
    .expect("untrusted");
    assert!(matches!(error.problem, Problem::Certificate(_)), "{error}");
    let pinned = tls_endpoint(
        &fake,
        tls::Trust::Pinned(pin(fake_http::FRIGATE_DEFAULT_SHA256)),
    );
    let response = send(&pinned, &get("/"), &Auth::None, None).unwrap();
    assert_eq!(response.body.read_all(64).unwrap(), b"frigate");
}

#[test]
fn fingerprints_are_fetched_without_sending_a_request() {
    let fake = FakeHttp::start_tls(fake_http::localhost(), |_| Reply::text(200, "x"));
    let offered = fetch_fingerprint(
        "Test hub t",
        &parse_url(&fake.url()).unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(offered, pin(fake_http::LOCALHOST_SHA256));
    std::thread::sleep(Duration::from_millis(50));
    assert!(fake.requests().is_empty());
}

#[test]
fn a_drip_fed_tls_record_cannot_extend_the_fingerprint_deadline() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = parse_url(&format!("https://{}", listener.local_addr().unwrap())).unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        // Receive the ClientHello, then slowly send one incomplete TLS
        // handshake record. rustls must wait for the complete record before
        // it can reject its contents, so the network deadline must bound it.
        assert!(socket.read(&mut [0; 4096]).unwrap() > 0);
        socket.write_all(&[22, 3, 3, 0, 100]).unwrap();
        for _ in 0..100 {
            if socket.write_all(&[0]).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let start = Instant::now();
    let error = fetch_fingerprint("Test hub t", &url, Duration::from_millis(250)).unwrap_err();
    assert_eq!(crate::cli::error_code(&error), ("timeout", 4));
    assert!(start.elapsed() < Duration::from_secs(1));
    server.join().unwrap();
}

#[test]
fn certificates_are_checked_against_the_configured_name() {
    let fake = FakeHttp::start_tls(fake_http::ha_example_test(), |_| Reply::text(200, "named"));
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(fake_http::test_ca()))
        .unwrap();
    let trust = tls::Trust::Roots(Arc::new(roots));
    let url = parse_url(&format!("https://127.0.0.1:{}", fake.addr.port())).unwrap();
    let named = |name: &str| Endpoint {
        label: "Test hub t".into(),
        url: url.clone(),
        tls: Some((
            tls::client_config(&trust).unwrap(),
            ServerName::try_from(name.to_owned()).unwrap(),
        )),
    };
    let response = send(&named("ha.example.test"), &get("/"), &Auth::None, None).unwrap();
    assert_eq!(response.body.read_all(64).unwrap(), b"named");
    let error = send(&named("other.example.test"), &get("/"), &Auth::None, None)
        .err()
        .expect("mismatch");
    let Problem::Certificate(rejected) = error.problem else {
        panic!("{error}");
    };
    assert_eq!(rejected.reason, tls::Reason::NameMismatch);
    assert!(error.message().contains("--tls-server-name"), "{error}");
}
