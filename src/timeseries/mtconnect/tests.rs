use super::*;
use serde_json::{Value, json};
use std::fmt::Write as _;

fn namespace_attributes(count: usize) -> String {
    let mut text = String::new();
    for number in 0..count {
        write!(text, "x:a{number}=\"v\" ").unwrap();
    }
    text
}

fn literal(name: &str) -> String {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/mtconnect/literals.json"
    ))
    .unwrap();
    let entry = &fixtures["literals"][name];
    let text = entry["xml"].as_str().unwrap();
    assert_eq!(
        text.len(),
        usize::try_from(entry["bytes"].as_u64().unwrap()).unwrap()
    );
    assert_eq!(
        crate::types::hex(Sha256::digest(text.as_bytes())),
        entry["sha256"].as_str().unwrap()
    );
    text.to_owned()
}
fn document(text: &str) -> roxmltree::Document<'_> {
    xml::document(text, &mut Work::default()).unwrap()
}
fn rejected<T>(result: Result<T>) -> anyhow::Error {
    let Err(error) = result else {
        panic!("expected malformed or unsupported input to fail")
    };
    error
}
fn versioned(text: &str, version: &str) -> String {
    let mut text = text.replace(":1.2\"", &format!(":{version}\""));
    if !matches!(version, "1.2" | "1.3" | "1.4" | "1.5" | "1.6") {
        text = text.replace(
            " sender=",
            " deviceModelChangeTime=\"2026-11-11T11:00:00Z\" sender=",
        );
    }
    text
}
fn config() -> Config {
    serde_json::from_value(json!({"address":"127.0.0.1:8082","base_path":"/UMC"})).unwrap()
}
fn no_null(value: &Value) {
    match value {
        Value::Null => panic!("native envelope must not contain null"),
        Value::Array(values) => {
            for value in values {
                no_null(value);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                no_null(value);
            }
        }
        _ => {}
    }
}

#[test]
fn qualified_namespaces_and_native_conditions_preserve_legacy_producer_metadata() {
    for version in [
        "1.2", "1.3", "1.4", "1.5", "1.6", "1.7", "1.8", "2.0", "2.1", "2.2", "2.3", "2.4", "2.5",
        "2.6", "2.7",
    ] {
        let probe = versioned(&literal("probe12"), version)
            .replace("synthetic-1", "000")
            .replace("version=\"1.2.0\"", "version=\"1.2.0.1.2\"");
        let probe_doc = document(&probe);
        let model = xml::probe(&probe_doc).unwrap();
        assert_eq!(model.version, version);
        let current = versioned(&literal("current12"), version).replace("synthetic-1", "000");
        let current_doc = document(&current);
        let stream = xml::streams(&current_doc, &model, None).unwrap();
        let rows = serde_json::to_value(&stream.observations).unwrap();
        no_null(&rows);
        assert_eq!(rows.as_array().unwrap().len(), 4);
        assert_eq!(rows[0]["text"], "  O1234 & SETUP  ");
        assert_eq!(rows[1]["text"], "1.2500");
        assert_eq!(rows[2]["attributes"]["nativeCode"], "E01");
        assert_eq!(rows[3]["attributes"]["nativeCode"], "E01");
        let items = serde_json::to_value(&model.items).unwrap();
        let position = items
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["attributes"]["id"] == "p1")
            .unwrap();
        assert_eq!(position["attributes"]["units"], "MILLIMETER");
        assert_eq!(position["attributes"]["nativeUnits"], "INCH");
        assert_eq!(position["attributes"]["nativeScale"], "25.4");
    }
    let unqualified = literal("probe12").replace(":1.2\"", ":2.8\"");
    assert_eq!(
        crate::types::classify(&rejected(xml::document(&unqualified, &mut Work::default()))).kind,
        ErrorKind::Unsupported
    );
}

#[test]
fn sample_keeps_activation_multiplicity_document_order_and_filtered_header_progress() {
    let probe =
        versioned(&literal("probe12"), "2.5").replace("instanceId=\"7\"", "instanceId=\"8\"");
    let probe_doc = document(&probe);
    let model = xml::probe(&probe_doc).unwrap();
    let sample = literal("sample25");
    let sample_doc = document(&sample);
    let result = xml::streams(&sample_doc, &model, Some((105, 4))).unwrap();
    let rows = serde_json::to_value(&result.observations).unwrap();
    assert_eq!(result.header.next, Some(109));
    assert_eq!(rows[0]["attributes"]["sequence"], "108");
    assert_eq!(rows[0]["text"], "UNAVAILABLE");
    assert_eq!(rows[1]["attributes"]["conditionId"], "activation-a");
    assert_eq!(rows[2]["attributes"]["conditionId"], "activation-b");
    assert_eq!(rows[3]["qname"]["name"], "Normal");
    assert_eq!(rows[3]["text"], "");
    let duplicate = sample.replace("sequence=\"106\"", "sequence=\"105\"");
    let duplicate_doc = document(&duplicate);
    rejected(xml::streams(&duplicate_doc, &model, Some((105, 4))));
    let probe = literal("probe12");
    let probe_doc = document(&probe);
    let model = xml::probe(&probe_doc).unwrap();
    let empty = literal("filtered_empty12");
    let empty_doc = document(&empty);
    let page = xml::streams(&empty_doc, &model, Some((105, 1))).unwrap();
    assert_eq!(page.observations.len(), 0);
    assert_eq!(page.header.next, Some(114));
}

#[test]
fn model_binding_rejects_unknown_ids_categories_components_epochs_and_namespaces() {
    let probe = literal("probe12");
    let probe_doc = document(&probe);
    let model = xml::probe(&probe_doc).unwrap();
    for bad in [
        literal("current12").replace("dataItemId=\"p1\"", "dataItemId=\"missing\""),
        literal("current12").replace("component=\"Controller\"", "component=\"Axes\""),
        literal("current12").replace("componentId=\"c1\"", "componentId=\"c2\""),
        literal("current12").replace("uuid=\"synthetic-1\"", "uuid=\"different\""),
        literal("current12").replace("instanceId=\"7\"", "instanceId=\"8\""),
        literal("current12").replace(":1.2\"", ":2.5\""),
        literal("current12")
            .replace("<Events>", "<Samples>")
            .replace("</Events>", "</Samples>"),
    ] {
        let bad_doc = document(&bad);
        rejected(xml::streams(&bad_doc, &model, None));
    }
    let duplicate = probe.replace("id=\"e1\"", "id=\"p1\"");
    let duplicate_doc = document(&duplicate);
    rejected(xml::probe(&duplicate_doc));
}

#[test]
fn exact_epoch_sequences_and_empty_current_windows_do_not_depend_on_float_precision() {
    let base = 9_007_199_254_740_993_u64;
    let probe =
        literal("probe12").replace("instanceId=\"7\"", &format!("instanceId=\"{}\"", u64::MAX));
    let probe_doc = document(&probe);
    let model = xml::probe(&probe_doc).unwrap();
    let mut current =
        literal("current12").replace("instanceId=\"7\"", &format!("instanceId=\"{}\"", u64::MAX));
    for offset in 0..=5 {
        current = current.replace(
            &format!("=\"{}\"", 100 + offset),
            &format!("=\"{}\"", base + offset),
        );
    }
    let current_doc = document(&current);
    let stream = xml::streams(&current_doc, &model, None).unwrap();
    assert_eq!(stream.header.first, Some(base));
    assert_eq!(stream.header.next, Some(base + 5));
    assert_eq!(stream.header.instance, u64::MAX);
    let probe = literal("probe12");
    let probe_doc = document(&probe);
    let model = xml::probe(&probe_doc).unwrap();
    let empty = literal("filtered_empty12")
        .replace("firstSequence=\"100\"", "firstSequence=\"1\"")
        .replace("lastSequence=\"113\"", "lastSequence=\"0\"")
        .replace("nextSequence=\"114\"", "nextSequence=\"1\"");
    let empty_doc = document(&empty);
    let stream = xml::streams(&empty_doc, &model, None).unwrap();
    assert_eq!(stream.observations.len(), 0);
    assert_eq!(stream.header.next, Some(1));
}

#[test]
fn canonical_model_ignores_header_volatility_but_keeps_meaningful_leaf_whitespace() {
    let original = literal("probe12");
    let reordered = original
        .replace(
            "id=\"p1\" category=\"SAMPLE\"",
            "category=\"SAMPLE\" id=\"p1\"",
        )
        .replace("2026-11-11T12:00:00Z", "2026-11-11T13:00:00Z");
    let a = document(&original);
    let b = document(&reordered);
    let a = xml::probe(&a).unwrap();
    let b = xml::probe(&b).unwrap();
    assert_eq!(
        serde_json::to_vec(&a.fingerprint).unwrap(),
        serde_json::to_vec(&b.fingerprint).unwrap()
    );
    let one = original.replace(
        "nativeScale=\"25.4\"/>",
        "nativeScale=\"25.4\"><x:Initial xmlns:x=\"urn:extension\"> </x:Initial></DataItem>",
    );
    let two = one.replace("> </x:Initial>", ">  </x:Initial>");
    let a = document(&one);
    let b = document(&two);
    let a = xml::probe(&a).unwrap();
    let b = xml::probe(&b).unwrap();
    assert_ne!(
        serde_json::to_vec(&a.fingerprint).unwrap(),
        serde_json::to_vec(&b.fingerprint).unwrap()
    );
}

#[test]
fn structured_content_retains_namespace_attributes_removed_entries_and_logical_text() {
    let probe = versioned(&literal("probe12"), "2.5").replace(
        "id=\"e1\" category=\"EVENT\" type=\"PROGRAM\"",
        "id=\"e1\" category=\"EVENT\" type=\"VARIABLE\" representation=\"DATA_SET\"",
    );
    let probe_doc = document(&probe);
    let model = xml::probe(&probe_doc).unwrap();
    let current=versioned(&literal("current12"),"2.5").replace("<Program dataItemId=\"e1\"","<VariableDataSet count=\"2\" resetTriggered=\"MANUAL\" dataItemId=\"e1\"").replace("  O1234 &amp; SETUP  </Program>","<Entry key=\"a\">α<![CDATA[&β <!DOCTYPE literal>]]><!--excluded-->γ</Entry><Entry key=\"b\" removed=\"true\"/></VariableDataSet>");
    let current_doc = document(&current);
    let stream = xml::streams(&current_doc, &model, None).unwrap();
    let rows = serde_json::to_value(&stream.observations).unwrap();
    no_null(&rows);
    assert_eq!(rows[0]["attributes"]["resetTriggered"], "MANUAL");
    assert_eq!(rows[0]["content"][1]["attributes"]["removed"], "true");
    let text: String = rows[0]["content"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(text, "α&β <!DOCTYPE literal>γ");
}

#[test]
fn xml_media_and_work_bounds_reject_ambiguous_or_amplifying_inputs() {
    for mime in [
        "application/xml; charset=UTF-8",
        "text/xml; charset=\"utf-8\"",
    ] {
        xml::media_type(&[("content-type".into(), mime.into())]).unwrap();
    }
    for mime in [
        "text/html",
        "application/xml;=bad",
        "application/xml; charset=\"utf-8",
        "text/xml; charset=latin1",
        "application/xml; charset=UTF-8; charset=UTF-8",
    ] {
        xml::media_type(&[("content-type".into(), mime.into())]).unwrap_err();
    }
    let probe = literal("probe12");
    rejected(xml::document(
        &format!("<!DOCTYPE x [<!ENTITY y 'bad'>]>{probe}"),
        &mut Work::default(),
    ));
    let many = format!(
        "<MTConnectDevices xmlns=\"urn:mtconnect.org:MTConnectDevices:1.2\">{}</MTConnectDevices>",
        "<x/>".repeat(14_000)
    );
    let mut work = Work::default();
    xml::document(&many, &mut work).unwrap();
    xml::document(&many, &mut work).unwrap();
    rejected(xml::document(&many, &mut work));
    let uri = format!("urn:{}", "x".repeat(24 * 1024));
    let attrs = namespace_attributes(50);
    let input = format!(
        "<MTConnectDevices xmlns=\"urn:mtconnect.org:MTConnectDevices:1.2\" xmlns:x=\"{uri}\" {attrs}/>"
    );
    let doc = document(&input);
    serde_json::to_writer(Size(0), &xml::Attributes(doc.root_element())).unwrap_err();
}

#[test]
fn configuration_requires_unambiguous_auth_safe_paths_and_exact_resume_integers() {
    config().validate().unwrap();
    for path in [
        "//host",
        "/UMC/../device",
        "/UMC%2fother",
        "/UMC?x=1",
        "/UMC#part",
        "http://host",
    ] {
        let mut c = config();
        c.base_path = path.into();
        c.validate().unwrap_err();
    }
    for host in [
        "[]",
        ":80",
        "host:12:34",
        "user@host",
        "host/path",
        "bad\r\nheader",
    ] {
        let mut c = config();
        c.host = Some(host.into());
        c.validate().unwrap_err();
    }
    let mut c = config();
    c.mode = Mode::Current;
    c.start = Start::Named(Box::new(StartKind::BufferBegin));
    c.validate().unwrap_err();
    for count in [0, 1025] {
        let mut c = config();
        c.count = count;
        c.validate().unwrap_err();
    }
    let mut c = config();
    c.auth = AuthKind::Basic;
    c.username_env = Some("USER_NAME".into());
    c.password_env = Some("USER_PASSWORD".into());
    c.validate().unwrap_err();
    let c:Config=serde_json::from_str(r#"{"address":"127.0.0.1:8082","count":1e3,"start":{"instance_id":9007199254740993,"sequence":9007199254740994}}"#).unwrap();
    c.validate().unwrap();
    assert_eq!(c.count, 1000);
    let Start::Resume(resume) = c.start else {
        panic!("resume expected")
    };
    assert_eq!(resume.instance_id, 9_007_199_254_740_993);
    assert_eq!(resume.sequence, 9_007_199_254_740_994);
}

#[test]
fn poll_clamps_count_commits_complete_state_and_keeps_http_failure_sticky() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let incoming = listener.try_clone().unwrap();
    let mut c = config();
    c.address = listener.local_addr().unwrap();
    let probe = literal("probe12").replace("bufferSize=\"1024\"", "bufferSize=\"1\"");
    let current = literal("current12")
        .replace("bufferSize=\"1024\"", "bufferSize=\"1\"")
        .replace("firstSequence=\"100\"", "firstSequence=\"104\"");
    let server = std::thread::spawn(move || {
        for (index, path) in ["/UMC/probe", "/UMC/current", "/UMC/probe"]
            .into_iter()
            .enumerate()
        {
            let until = Instant::now() + Duration::from_secs(2);
            let (mut socket, _) = loop {
                match incoming.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < until, "client did not connect");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                io::Read::read_exact(&mut socket, &mut byte).unwrap();
                head.push(byte[0]);
                assert!(head.len() < 16 * 1024);
            }
            let header = std::str::from_utf8(&head).unwrap();
            assert!(header.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
            assert!(header.contains("Accept-Encoding: identity\r\n"));
            if index == 2 {
                io::Write::write_all(&mut socket,b"HTTP/1.1 500 Failure\r\nContent-Type: application/octet-stream\r\nContent-Length: 2\r\nConnection: close\r\n\r\n\xff\xfe").unwrap();
            } else {
                let body = if index == 0 { &probe } else { &current };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                io::Write::write_all(&mut socket, response.as_bytes()).unwrap();
            }
        }
    });
    let mut reader = Reader {
        tls: net::prepare(None).unwrap(),
        auth: auth::Session::new(&c).unwrap(),
        config: c,
        state: None,
        failure: None,
        observed: None,
        ready: false,
    };
    let batch = reader.read(Duration::from_secs(2)).unwrap();
    assert_eq!(batch.len(), 1);
    no_null(&batch[0].value);
    assert_eq!(batch[0].value["effective_count"], 1);
    assert_eq!(reader.state.as_ref().unwrap().next, 105);
    assert!(reader.ready());
    let failed = reader.read(Duration::from_secs(2)).unwrap_err();
    assert_eq!(
        crate::types::classify(&failed).kind,
        ErrorKind::OperationFailed
    );
    assert_eq!(reader.state.as_ref().unwrap().next, 105);
    let again = reader.read(Duration::from_millis(100)).unwrap_err();
    assert_eq!(
        crate::types::classify(&again).kind,
        ErrorKind::OperationFailed
    );
    server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn expanded_model_hash_work_stops_before_requesting_current_state() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let incoming = listener.try_clone().unwrap();
    let namespace = format!("urn:{}", "x".repeat(192 * 1024));
    let attributes = namespace_attributes(60);
    let probe = literal("probe12").replace(
        "<Device id=",
        &format!("<Device xmlns:x=\"{namespace}\" {attributes}id="),
    );
    assert!(probe.len() < MAX_BODY);
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(2);
        let (mut socket, _) = loop {
            match incoming.accept() {
                Ok(pair) => break pair,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < until, "client did not connect");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") {
            io::Read::read_exact(&mut socket, &mut byte).unwrap();
            head.push(byte[0]);
            assert!(head.len() < 16 * 1024);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{probe}",
            probe.len()
        );
        io::Write::write_all(&mut socket, response.as_bytes()).unwrap();
    });
    let mut c = config();
    c.address = listener.local_addr().unwrap();
    let mut reader = Reader {
        tls: net::prepare(None).unwrap(),
        auth: auth::Session::new(&c).unwrap(),
        config: c,
        state: None,
        failure: None,
        observed: None,
        ready: false,
    };
    let error = reader.read(Duration::from_secs(2)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert!(reader.state.is_none());
    assert!(!reader.ready());
    server.join().unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}
