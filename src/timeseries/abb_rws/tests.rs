use super::*;
use serde_json::json;
use std::io::Read as _;

fn config() -> Config {
    serde_json::from_value(json!({
        "api":"rws1","address":"127.0.0.1:8080",
        "username_env":"ABB_USER","password_env":"ABB_PASSWORD",
        "reads":[{"signal":"state","resource":"controller_state"}]
    }))
    .unwrap()
}
fn symbol(inner: &str) -> String {
    format!(
        "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><div class=\"state\"><ul><li class=\"rap-data\" title=\"RAPID/T_ROB1/Main/Count\"><span class=\"value\">{inner}</span></li></ul></div></body></html>"
    )
}
fn parsed(xml: &str) -> Result<Value> {
    parser::parse(
        Api::Rws1,
        Resource::RapidSymbol,
        false,
        Some("RAPID/T_ROB1/Main/Count"),
        xml,
    )
}

#[test]
fn whole_rapid_value_preserves_text_cdata_entities_and_excludes_comments() {
    let xml = symbol("  [&quot;A&amp;B&quot;,<![CDATA[1.25E-3]]><!--not data-->,42]  ");
    assert_eq!(
        parsed(&xml).unwrap()["value_text"],
        "  [\"A&B\",1.25E-3,42]  "
    );
    assert_eq!(parsed(&symbol("")).unwrap()["value_text"], "");
    parsed(&symbol("<span>nested</span>")).unwrap_err();
}

#[test]
fn symbol_structure_namespace_title_and_text_bounds_are_enforced() {
    let xml = symbol("value");
    for wrong in [
        xml.replace("http://www.w3.org/1999/xhtml", "urn:wrong"),
        xml.replace("RAPID/T_ROB1/Main/Count", "RAPID/T_ROB1/Main/Other"),
        xml.replace(
            "<span class=\"value\">value</span>",
            "<span class=\"value\">a</span><span class=\"value\">b</span>",
        ),
        xml.replace("<span class=\"value\">value</span>", ""),
        format!("<!DOCTYPE html [<!ENTITY bad 'expanded'>]>{xml}"),
        symbol(&"x".repeat(64 * 1024 + 1)),
    ] {
        parsed(&wrong).unwrap_err();
    }
    let unrelated = xml.replace(
        "</ul>",
        "<li class=\"future-resource\"><a href=\"https://elsewhere.invalid/\">future</a></li></ul>",
    );
    assert_eq!(parsed(&unrelated).unwrap()["value_text"], "value");
}

#[test]
fn xhtml_depth_and_element_limits_fail_without_entity_expansion() {
    let deep = symbol(&format!("{}x{}", "<i>".repeat(70), "</i>".repeat(70)));
    parsed(&deep).unwrap_err();
    let many = symbol("x").replace("</body>", &format!("{}</body>", "<aside/>".repeat(4096)));
    parsed(&many).unwrap_err();
}

#[test]
fn content_type_uses_explicit_api_and_strict_parameter_syntax() {
    for (api, mime) in [
        (Api::Rws1, "application/xhtml+xml"),
        (Api::Rws1, "application/xhtml+xml;v=1.0;charset=UTF-8"),
        (
            Api::Rws2,
            "application/xhtml+xml;v=\"2.0\";charset=\"UTF-8\"",
        ),
        (Api::Rws2, "application/xhtml+xml"),
    ] {
        parser::media_type(api, &[mime]).unwrap();
    }
    for mime in [
        "text/html",
        "application/xhtml+xml;v=1.0",
        "application/xhtml+xml;v=2.0;v=2.0",
        "application/xhtml+xml;v=\"2.0",
        "application/xhtml+xml;v=2.0\"",
        "application/xhtml+xml;charset=latin1",
        "application/xhtml+xml;=ignored",
        "application/xhtml+xml;bad name=token",
        "application/xhtml+xml;charset=UTF-8;charset=UTF-8",
    ] {
        parser::media_type(Api::Rws2, &[mime]).unwrap_err();
    }
    parser::media_type(Api::Rws1, &[]).unwrap_err();
    parser::media_type(
        Api::Rws1,
        &["application/xhtml+xml", "application/xhtml+xml"],
    )
    .unwrap_err();
}

#[test]
fn configuration_preflight_rejects_injection_and_duplicate_signals() {
    config().validate().unwrap();
    for address in [
        "0.0.0.0:80",
        "224.0.0.1:80",
        "255.255.255.255:80",
        "127.0.0.1:0",
        "[::1]:80",
    ] {
        let mut cfg = config();
        cfg.address = address.parse().unwrap();
        cfg.validate().unwrap_err();
    }
    for host in [
        "evil\r\nHeader: value",
        "user@host",
        "host/path",
        "host?query",
        "host:0",
    ] {
        let mut cfg = config();
        cfg.host = Some(host.into());
        cfg.validate().unwrap_err();
    }
    let mut cfg = config();
    cfg.reads.push(cfg.reads[0].clone());
    cfg.validate().unwrap_err();
    let mut cfg = config();
    cfg.api = Api::Rws2;
    cfg.validate().unwrap_err();
    for part in [".", "..", "a/b", "a\\b", "a?b", "a#b", "a[1]", "a\nb"] {
        let mut cfg = config();
        cfg.reads[0] = Selection {
            signal: "symbol".into(),
            resource: Resource::RapidSymbol,
            task: Some("T_ROB1".into()),
            module: Some("Main".into()),
            symbol: Some(part.into()),
        };
        cfg.validate().unwrap_err();
    }
}

#[test]
fn generation_paths_are_closed_and_segments_encode_original_bytes_once() {
    let read = Selection {
        signal: "symbol".into(),
        resource: Resource::RapidSymbol,
        task: Some("T_ROB1".into()),
        module: Some("Main".into()),
        symbol: Some("Å%20".into()),
    };
    assert_eq!(
        read.path(Api::Rws1, false),
        "/rw/rapid/symbol/data/RAPID/T_ROB1/Main/%C3%85%2520"
    );
    assert_eq!(
        read.path(Api::Rws2, true),
        "/rw/rapid/symbol/RAPID/T_ROB1/Main/%C3%85%2520/properties"
    );
    let read = &config().reads[0];
    assert_eq!(read.path(Api::Rws1, false), "/rw/panel/ctrlstate");
    assert_eq!(read.path(Api::Rws2, false), "/rw/panel/ctrl-state");
}

#[test]
fn digest_nonce_count_survives_algorithm_rechallenge() {
    let mut session = auth::Session::with_credentials(
        Zeroizing::new("reader".into()),
        Zeroizing::new("secret".into()),
    )
    .unwrap();
    for (algorithm, nonce, count) in [
        ("MD5", "nonce", 1_u32),
        ("md5", "nonce", 2),
        ("mD5", "nonce", 3),
        ("SHA-256", "nonce", 4),
        ("sha-256", "nonce", 5),
        ("sha-256", "new-nonce", 1),
    ] {
        session
            .challenge(&[(
                "www-authenticate".into(),
                format!(
                    "Digest realm=\"realm\", nonce=\"{nonce}\", qop=\"auth\", algorithm={algorithm}"
                ),
            )])
            .unwrap();
        let header = session
            .authorization(Api::Rws1, "/rw/panel/opmode")
            .unwrap()
            .unwrap();
        assert!(header.contains(&format!(", nc={count:08x},")));
        assert!(header.contains(&format!("algorithm={algorithm},")));
    }
}

#[test]
fn cookies_retain_actual_names_paths_security_expiry_and_origin() {
    let mut cookies = auth::Cookies::default();
    let now = Instant::now();
    cookies
        .receive(
            [
                "-http-session-=session; Path=/rw",
                "ABBCX=context; Path=/rw/panel; Secure",
            ]
            .into_iter(),
            "robot.test",
            "/rw/panel/opmode",
            true,
            now,
        )
        .unwrap();
    assert_eq!(
        &*cookies.header("/rw/panel/opmode", true),
        "-http-session-=session; ABBCX=context"
    );
    assert_eq!(
        &*cookies.header("/rw/panel/opmode", false),
        "-http-session-=session"
    );
    assert_eq!(&*cookies.header("/rwx/panel", true), "");
    cookies
        .receive(
            std::iter::once("-http-session-=gone; Path=/rw; Max-Age=0"),
            "robot.test",
            "/rw/panel/opmode",
            true,
            now,
        )
        .unwrap();
    assert_eq!(&*cookies.header("/rw/system", true), "");
    cookies
        .receive(
            std::iter::once("other=x; Domain=elsewhere.invalid; Path=/"),
            "robot.test",
            "/rw/system",
            true,
            now,
        )
        .unwrap_err();
    cookies
        .receive(
            std::iter::once("joined=a, another=b; Path=/"),
            "robot.test",
            "/rw/system",
            true,
            now,
        )
        .unwrap_err();
}

#[test]
fn paced_poll_failure_retires_session_and_keeps_original_typed_cause() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut cfg = config();
    cfg.address = listener.local_addr().unwrap();
    let mut reader = Reader {
        config: cfg,
        tls: net::prepare(None).unwrap(),
        session: auth::Session::with_credentials(
            Zeroizing::new("reader".into()),
            Zeroizing::new("secret".into()),
        )
        .unwrap(),
        last_send: Some(Instant::now()),
        failure: None,
        observed: None,
        ready: false,
    };
    let first = reader.read(Duration::from_millis(10)).unwrap_err();
    assert_eq!(crate::types::classify(&first).kind, ErrorKind::Timeout);
    let second = reader.read(Duration::from_secs(2)).unwrap_err();
    assert_eq!(crate::types::classify(&second).kind, ErrorKind::Timeout);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(!reader.ready());
}

#[test]
fn escaped_output_budget_fails_without_publishing_partial_reads() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = config();
    cfg.address = listener.local_addr().unwrap();
    for index in 0..4 {
        cfg.reads.push(Selection {
            signal: format!("value{index}"),
            resource: Resource::RapidSymbol,
            task: Some("T_ROB1".into()),
            module: Some("Main".into()),
            symbol: Some("Count".into()),
        });
    }
    cfg.reads.remove(0);
    let mut reader = Reader {
        config: cfg,
        tls: net::prepare(None).unwrap(),
        session: auth::Session::with_credentials(
            Zeroizing::new("reader".into()),
            Zeroizing::new("secret".into()),
        )
        .unwrap(),
        last_send: None,
        failure: None,
        observed: None,
        ready: false,
    };
    // Raw bytes fit; JSON escaping plus retained parsed values exceeds the batch.
    let properties = "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><div class=\"state\"><ul><li class=\"rap-sympropvar\" title=\"RAPID/T_ROB1/Main/Count\"><span class=\"symburl\">RAPID/T_ROB1/Main/Count</span><span class=\"dattyp\">string</span><span class=\"ndim\">0</span><span class=\"dim\"></span><span class=\"symtyp\">var</span><span class=\"typurl\">RAPID/string</span></li></ul></div></body></html>";
    let value = symbol(&"\t".repeat(64 * 1024));
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        let mut requests = 0_usize;
        while Instant::now() < until && requests < 8 {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
                assert!(head.len() < 16384);
            }
            let body = if requests.is_multiple_of(2) {
                properties
            } else {
                &value
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/xhtml+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if stream.write_all(reply.as_bytes()).is_err() {
                break;
            }
            requests += 1;
        }
        requests
    });
    let error = reader.read(Duration::from_secs(4)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert!(reader.observed_ns().is_none());
    assert!(!reader.ready());
    assert_eq!(server.join().unwrap(), 8);
    reader.read(Duration::from_secs(1)).unwrap_err();
}
