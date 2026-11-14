//! Manufacturer-documented JSON Integration for IO-Link through the actual CLI.
//! SICK SIG350 example: GET .../devices/master1port1/processdata/getdata/value.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write as _},
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
    let until = Instant::now() + Duration::from_secs(5);
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "IO-Link client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn request(stream: &mut impl io::Read, format: &str, base: &str) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16_384);
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let header = String::from_utf8(bytes).unwrap();
    assert!(
        header.starts_with(&format!(
            "GET {base}/devices/master1port1/processdata/getdata/value?format={format} HTTP/1.1\r\n"
        )),
        "{header}"
    );
    assert!(header.contains("Connection: close\r\n"));
    assert!(
        !header.contains("Content-Length:"),
        "a process read has no configuration body"
    );
    header
}
fn response(stream: &mut impl io::Write, body: &str, chunked: bool) {
    let bytes = if chunked {
        let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for part in body.as_bytes().chunks(13) {
            bytes.extend_from_slice(format!("{:x};extension=1\r\n", part.len()).as_bytes());
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
    for part in bytes.chunks(7) {
        stream.write_all(part).unwrap();
    }
    stream.flush().unwrap();
}
fn config(listener: &TcpListener) -> Value {
    json!({"version":1,"sources":[{"id":"link","protocol":"iolink_json",
        "address":listener.local_addr().unwrap().to_string(),"device":"master1port1",
        "every_ms":3_600_000,"timeout_ms":1000}]})
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
fn samples(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample")
        .collect()
}

#[test]
fn byte_array_process_values_preserve_quality_raw_input_mappings_and_exports() {
    for chunked in [false, true] {
        let home = Home::new("iolink-json-byte-array");
        let listener = listener();
        let mut config = config(&listener);
        config["sources"][0]["signals"] = json!([
            {"name":"temperature","offset":0,"data_type":"u16","right_shift":2,"scale":0.1},
            {"name":"counter","offset":2,"data_type":"u64","byte_order":"big"}
        ]);
        config["sources"][0]["json_signals"] = json!([{ "name":"iq", "pointer":"/iqValue" }]);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, "byteArray", "/iolink/v1");
            response(
                &mut stream,
                r#"{"iolink":{"valid":true,"value":[3,200,255,255,255,255,255,255,255,255]},"iqValue":false,"vendor_time":"2026-11-10T14:00:00.123456789Z"}"#,
                chunked,
            );
        });
        record(&home, &config).success();
        server.join().unwrap();
        let rows = samples(&home);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["value"]["response"]["iolink"]["value"][0], 3);
        assert_eq!(
            rows[0]["value"]["response"]["vendor_time"],
            "2026-11-10T14:00:00.123456789Z"
        );
        assert!((rows[1]["value"].as_f64().unwrap() - 24.2).abs() < 1e-12);
        assert_eq!(rows[2]["value"].as_u64(), Some(u64::MAX));
        assert_eq!(rows[3]["value"], false);
        assert!(rows.iter().all(|row| row["elapsed_ns"].is_u64() && row["elapsed_ns"] == rows[0]["elapsed_ns"]));
        home.run(&["--json", "data", "export", "run.jsonl", "-o", "run.csv"])
            .success();
        home.run(&[
            "--json",
            "data",
            "export",
            "run.jsonl",
            "-o",
            "export.jsonl",
            "--format",
            "jsonl",
        ])
        .success();
        assert!(
            fs::read_to_string(home.path("run.csv"))
                .unwrap()
                .contains("18446744073709551615")
        );
        assert!(
            fs::read_to_string(home.path("export.jsonl"))
                .unwrap()
                .contains("2026-11-10T14:00:00.123456789Z")
        );
    }
}

#[test]
fn iodd_values_units_exact_json_numbers_and_escaped_pointer_keys_survive() {
    let home = Home::new("iolink-json-iodd");
    let listener = listener();
    let mut config = config(&listener);
    config["sources"][0]["format"] = json!("iodd");
    config["sources"][0]["base_path"] = json!("/gateway/iolink/v1/");
    config["sources"][0]["json_signals"] = json!([
        {"name":"distance","pointer":"/iolink/value/Distance"},
        {"name":"counter","pointer":"/iolink/value/Counter/value"},
        {"name":"exact","pointer":"/iolink/value/a~1b~0c/value"}
    ]);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        request(&mut stream, "iodd", "/gateway/iolink/v1");
        response(
            &mut stream,
            r#"{"iolink":{"valid":true,"value":{"Distance":{"value":55,"unit":"cm"},"Counter":{"value":18446744073709551615},"a/b~c":{"value":1.234567890123456789}}},"iqValue":true}"#,
            true,
        );
    });
    record(&home, &config).success();
    server.join().unwrap();
    let rows = samples(&home);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[1]["value"], json!({"value":55,"unit":"cm"}));
    assert_eq!(rows[2]["value"].as_u64(), Some(u64::MAX));
    assert_eq!(rows[3]["value"].to_string(), "1.234567890123456789");
}

#[test]
fn digital_input_mode_and_basic_or_bearer_headers_are_read_only() {
    for bearer in [false, true] {
        let home = Home::new("iolink-json-http-auth");
        let listener = listener();
        let mut config = config(&listener);
        config["sources"][0]["host"] = json!("gateway.test:8080");
        config["sources"][0]["json_signals"] = json!([{ "name":"cq", "pointer":"/cqValue" }]);
        if bearer {
            config["sources"][0]["bearer_token_env"] = json!("CAPTUREFAB_TEST_IOLINK_TOKEN");
        } else {
            config["sources"][0]["username_env"] = json!("CAPTUREFAB_TEST_IOLINK_USER");
            config["sources"][0]["password_env"] = json!("CAPTUREFAB_TEST_IOLINK_PASSWORD");
        }
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let header = request(&mut stream, "byteArray", "/iolink/v1");
            assert!(header.contains("Host: gateway.test:8080\r\n"));
            assert!(header.contains(if bearer {
                "Authorization: Bearer fixture.token\r\n"
            } else {
                "Authorization: Basic b3BlcmF0b3I6c2Vuc29yLXNlY3JldA==\r\n"
            }));
            response(&mut stream, r#"{"cqValue":true,"iqValue":false}"#, false);
        });
        fs::write(home.path("sources.json"), config.to_string()).unwrap();
        let output = home
            .command()
            .env("CAPTUREFAB_TEST_IOLINK_TOKEN", "fixture.token")
            .env("CAPTUREFAB_TEST_IOLINK_USER", "operator")
            .env("CAPTUREFAB_TEST_IOLINK_PASSWORD", "sensor-secret")
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
        Run::of_output(output, &["data", "record"]).success();
        server.join().unwrap();
        assert_eq!(samples(&home)[1]["value"], true);
        assert!(
            !fs::read_to_string(home.path("run.jsonl"))
                .unwrap()
                .contains("sensor-secret")
        );
    }
}

#[test]
fn invalid_quality_values_and_selected_fields_never_publish_partial_batches() {
    for body in [
        r#"{"iolink":{"valid":false,"value":[12,22]}}"#,
        r#"{"iolink":{"value":[12,22]}}"#,
        r#"{"iolink":{"valid":true,"value":[12,256]}}"#,
        r#"{"iolink":{"valid":true,"value":[12,1.5]}}"#,
        r#"{"iolink":{"valid":true,"value":[12,-1]}}"#,
        r#"{"iolink":{"valid":true,"value":[12,22]},"iqValue":1}"#,
        r#"{"iolink":{"valid":true,"value":[12,22]}}"#,
        r#"{"iqValue":false}"#,
    ] {
        let home = Home::new("iolink-json-invalid-pd");
        let listener = listener();
        let mut config = config(&listener);
        config["sources"][0]["signals"] = json!([{ "name":"word","offset":0 }]);
        config["sources"][0]["json_signals"] = json!([{ "name":"iq","pointer":"/iqValue" }]);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, "byteArray", "/iolink/v1");
            response(&mut stream, body, false);
        });
        assert_eq!(record(&home, &config).failure(1)["code"], "incomplete");
        server.join().unwrap();
        assert_eq!(samples(&home), Vec::<Value>::new());
    }
}

#[test]
fn configuration_errors_do_not_create_output_or_connect() {
    for update in [
        json!({"address":"127.0.0.1:0"}),
        json!({"device":"../port1"}),
        json!({"device":""}),
        json!({"base_path":"//other-host"}),
        json!({"base_path":"/iolink/../reset"}),
        json!({"base_path":"/iolink?configure=1"}),
        json!({"host":"host\r\nX-Foo: bad"}),
        json!({"json_signals":[{"name":"x","pointer":"/~2"}]}),
        json!({"json_signals":[{"name":"x","pointer":"value"}]}),
        json!({"json_signals":[{"name":"process_data","pointer":"/iqValue"}]}),
        json!({"format":"iodd","signals":[{"name":"x","offset":0}]}),
        json!({"username_env":"USER"}),
        json!({"username_env":"USER","password_env":"PASS","bearer_token_env":"TOKEN"}),
    ] {
        let home = Home::new("iolink-json-invalid-config");
        let listener = listener();
        let mut config = config(&listener);
        config["sources"][0]
            .as_object_mut()
            .unwrap()
            .extend(update.as_object().unwrap().clone());
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn bad_http_framing_server_status_and_trickled_headers_fail_with_bounded_reads() {
    for (reply, code, reason) in [
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            1,
            "incomplete",
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\n\r\n",
            1,
            "incomplete",
        ),
        (
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
            3,
            "auth_rejected",
        ),
        (
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            3,
            "unavailable",
        ),
        (
            "HTTP/1.1 302 Found\r\nLocation: http://other/\r\n\r\n",
            5,
            "unsupported",
        ),
    ] {
        let home = Home::new("iolink-json-http-failure");
        let listener = listener();
        let config = config(&listener);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, "byteArray", "/iolink/v1");
            stream.write_all(reply.as_bytes()).unwrap();
        });
        assert_eq!(record(&home, &config).failure(code)["code"], reason);
        server.join().unwrap();
        assert_eq!(samples(&home), Vec::<Value>::new());
    }
    let home = Home::new("iolink-json-http-deadline");
    let listener = listener();
    let mut config = config(&listener);
    config["sources"][0]["timeout_ms"] = json!(60);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        request(&mut stream, "byteArray", "/iolink/v1");
        for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
    });
    let began = Instant::now();
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    assert!(began.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
}

#[test]
fn arbitrary_http_chunks_and_separate_header_trailer_budgets_preserve_values() {
    let home = Home::new("iolink-json-many-chunks");
    let listener = listener();
    let mut config = config(&listener);
    config["sources"][0]["format"] = json!("iodd");
    config["sources"][0]["json_signals"] =
        json!([{ "name":"blob","pointer":"/iolink/value/blob" }]);
    let expected = "segment".repeat(800);
    let body =
        json!({"iolink":{"valid":true,"value":{"blob":expected,"unit":"fixture"}}}).to_string();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        request(&mut stream, "iodd", "/iolink/v1");
        let mut reply = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-Pad: {}\r\n\r\n",
            "h".repeat(12_000)
        )
        .into_bytes();
        // More than 256 chunks and more than 16 KiB of aggregate chunk lines.
        for (index, byte) in body.bytes().enumerate() {
            reply.extend_from_slice(if index % 2 == 0 {
                b"1 \t; part = \"\x80\xff\"; bare; token=a-b\r\n"
            } else {
                b"1;note=\"escaped\\\"quote\\\\slash\";!#$%&'*+-.^_`|~=token\r\n"
            });
            reply.push(byte);
            reply.extend_from_slice(b"\r\n");
        }
        reply.extend_from_slice(b"0;done=\"\xff\"\r\nX_Token!#$%&'*+-.^`|~: \x80\xff\r\n");
        reply.extend_from_slice(format!("X-Pad: {}\r\n", "t".repeat(12_000)).as_bytes());
        // The aggregate byte limit also bounds any number of short trailers.
        for _ in 0..80 {
            reply.extend_from_slice(b"X-Vendor: complete\r\n");
        }
        reply.extend_from_slice(b"\r\n");
        stream.write_all(&reply).unwrap();
    });
    record(&home, &config).success();
    server.join().unwrap();
    let rows = samples(&home);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0]["value"]["response"]["iolink"]["value"]["blob"],
        expected
    );
    assert_eq!(rows[1]["value"], expected);
    assert_eq!(rows[0]["elapsed_ns"], rows[1]["elapsed_ns"]);
}

#[test]
fn malformed_chunk_metadata_trailers_and_decoded_overflow_are_atomic() {
    let body = r#"{"iolink":{"valid":true,"value":[12,34]}}"#;
    let prefix = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
    let complete = format!("{:x}\r\n{body}\r\n0\r\n", body.len());
    let oversized_trailers = format!(
        "X-First: {}\r\nX-Second: {}\r\n\r\n",
        "a".repeat(9_000),
        "b".repeat(9_000)
    );
    let mut cases = vec![
        (
            format!("1;padding={}\r\n", "x".repeat(16_384)).into_bytes(),
            "oversized HTTP response framing",
        ),
        (
            [complete.as_bytes(), oversized_trailers.as_bytes()].concat(),
            "oversized HTTP response framing",
        ),
        (b"100001\r\n".to_vec(), "body exceeds size limit"),
        (
            b"ffffffffffffffffffffffffffffffff\r\n".to_vec(),
            "chunk size overflows",
        ),
        (b"\r\n".to_vec(), "invalid HTTP chunk size"),
        (b"1\r\nxXX".to_vec(), "invalid HTTP chunk delimiter"),
        (
            format!("{:x}\r\n{body}\r\n", body.len()).into_bytes(),
            "truncated or oversized HTTP response framing",
        ),
    ];
    for chunk in [
        &b"1;note=\rX\r\n"[..],
        &b"1;note=\0\r\n"[..],
        &b"1;note=\x7f\r\n"[..],
        &b"1;note=\"\r\"\r\n"[..],
        &b"1;note=\"\\\r\"\r\n"[..],
        &b"1;note=\"unterminated\r\n"[..],
        &b"1;=value\r\n"[..],
        &b"1;name=\r\n"[..],
        &b"1;name=\"valid\"junk\r\n"[..],
        &b"1;;name=x\r\n"[..],
        &b"1;\r\n"[..],
    ] {
        cases.push((chunk.to_vec(), "invalid HTTP chunk extension"));
    }
    for trailer in [
        &b"Missing-Colon\r\n\r\n"[..],
        &b"Bad Name: ignored\r\n\r\n"[..],
        &b" folded: ignored\r\n\r\n"[..],
        &b"X-Device: \0\r\n\r\n"[..],
        &b"X-Device: \x7f\r\n\r\n"[..],
        &b"0\r\n\r\n"[..],
    ] {
        cases.push(([complete.as_bytes(), trailer].concat(), "HTTP field"));
    }
    for trailer in [
        "Content-Length: 1\r\n\r\n",
        "Transfer-Encoding: chunked\r\n\r\n",
        "Content-Encoding: gzip\r\n\r\n",
    ] {
        cases.push((
            [complete.as_bytes(), trailer.as_bytes()].concat(),
            "trailer cannot redefine",
        ));
    }
    // The first 256 chunks exactly fill the body budget; one more positive
    // chunk must fail before reading its payload or publishing any JSON.
    let mut overflow = Vec::new();
    for _ in 0..256 {
        overflow.extend_from_slice(b"1000\r\n");
        overflow.extend_from_slice(&[b'x'; 4096]);
        overflow.extend_from_slice(b"\r\n");
    }
    overflow.extend_from_slice(b"1\r\n");
    cases.push((overflow, "body exceeds size limit"));
    for (payload, expected) in cases {
        let home = Home::new("iolink-json-chunk-bounds");
        let listener = listener();
        let config = config(&listener);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, "byteArray", "/iolink/v1");
            let reply = [prefix.as_slice(), payload.as_slice()].concat();
            // Oversized framing can close the client before all bytes are sent.
            let _ = stream.write_all(&reply);
        });
        let error = record(&home, &config).failure(1);
        server.join().unwrap();
        assert_eq!(error["code"], "incomplete");
        assert!(
            error["message"].as_str().unwrap().contains(expected),
            "expected {expected}: {error}"
        );
        assert_eq!(samples(&home), [] as [Value; 0]);
    }
}

#[test]
fn positive_http_chunks_cannot_restart_the_total_read_deadline() {
    let home = Home::new("iolink-json-chunk-deadline");
    let listener = listener();
    let mut config = config(&listener);
    config["sources"][0]["timeout_ms"] = json!(60);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        request(&mut stream, "byteArray", "/iolink/v1");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        let began = Instant::now();
        for _ in 0..30 {
            if stream.write_all(b"1;progress=yes\r\nx\r\n").is_err() {
                return began.elapsed();
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("client kept accepting chunks beyond its total read deadline");
    });
    let began = Instant::now();
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    assert!(began.elapsed() < Duration::from_secs(2));
    assert!(server.join().unwrap() < Duration::from_millis(500));
    assert_eq!(samples(&home), [] as [Value; 0]);
}

#[test]
fn maximum_byte_arrays_preserve_terminal_mapping_and_short_inputs_are_atomic() {
    for length in [32, 33, 1] {
        let home = Home::new("iolink-json-byte-bounds");
        let listener = listener();
        let mut config = config(&listener);
        config["sources"][0]["signals"] = if length == 1 {
            json!([{"name":"first","offset":0,"data_type":"u8"},{"name":"short","offset":0,"data_type":"u16"}])
        } else {
            json!([{"name":"last","offset":24,"data_type":"u64"},{"name":"bit","offset":31,"data_type":"bool","bit":7}])
        };
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, "byteArray", "/iolink/v1");
            let mut payload = vec![7; length];
            if length >= 32 {
                payload[24..32].fill(255);
            }
            response(
                &mut stream,
                &json!({"iolink":{"valid":true,"value":payload}}).to_string(),
                false,
            );
        });
        let run = record(&home, &config);
        server.join().unwrap();
        if length == 32 {
            run.success();
            let rows = samples(&home);
            assert_eq!(rows.len(), 3);
            assert_eq!(
                rows[0]["value"]["response"]["iolink"]["value"]
                    .as_array()
                    .unwrap()
                    .len(),
                32
            );
            assert_eq!(rows[1]["value"].as_u64(), Some(u64::MAX));
            assert_eq!(rows[2]["value"], true);
        } else {
            assert_eq!(run.failure(1)["code"], "incomplete");
            assert_eq!(samples(&home), [] as [Value; 0]);
        }
    }
}

#[test]
fn raw_document_and_selected_value_share_one_atomic_serialized_limit() {
    for select in [false, true] {
        let home = Home::new("iolink-json-batch-bound");
        let listener = listener();
        let mut config = config(&listener);
        config["sources"][0]["format"] = json!("iodd");
        if select {
            config["sources"][0]["json_signals"] =
                json!([{ "name":"blob","pointer":"/iolink/value/blob" }]);
        }
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, "iodd", "/iolink/v1");
            let body = json!({"iolink":{"valid":true,"value":{"blob":"x".repeat(600_000),"unit":"fixture"}}}).to_string();
            assert!(body.len() < 1 << 20);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body.as_bytes()).unwrap();
        });
        let run = record(&home, &config);
        server.join().unwrap();
        if select {
            let error = run.failure(1);
            assert_eq!(error["code"], "incomplete");
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("serialized process-data batch"),
                "{error}"
            );
            assert_eq!(samples(&home), [] as [Value; 0]);
        } else {
            run.success();
            let rows = samples(&home);
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0]["value"]["response"]["iolink"]["value"]["blob"]
                    .as_str()
                    .unwrap()
                    .len(),
                600_000
            );
        }
    }
}

#[test]
fn valid_http_tokens_and_opaque_extension_values_are_accepted_but_controls_reject() {
    for mode in 0..3 {
        let home = Home::new("iolink-json-header-tokens");
        let listener = listener();
        let config = config(&listener);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream, "byteArray", "/iolink/v1");
            let body = r#"{"iolink":{"valid":true,"value":[12,34]}}"#;
            let mut reply = b"HTTP/1.1 200 OK\r\nX_Trace: fixture\r\nX!#$%&'*+-.^`|~: ignored\r\nX-Device-Note: ".to_vec();
            reply.extend_from_slice(if mode == 1 {
                &[0_u8, 0xff]
            } else {
                &[0x80, 0xff]
            });
            let whitespace = if mode == 2 { "\u{a0}" } else { " " };
            reply.extend_from_slice(
                format!(
                    "\r\nContent-Length:{whitespace}{}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            stream.write_all(&reply).unwrap();
        });
        let run = record(&home, &config);
        server.join().unwrap();
        if mode != 0 {
            assert_eq!(run.failure(1)["code"], "incomplete");
            assert_eq!(samples(&home), [] as [Value; 0]);
        } else {
            run.success();
            assert_eq!(samples(&home).len(), 1);
        }
    }
}

#[cfg(feature = "mqtt-tls")]
#[test]
fn private_ca_tls_verifies_identity_before_read_only_basic_and_bearer_requests() {
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
    use std::sync::Arc;
    for (wrong_name, bearer) in [(false, false), (false, true), (true, false), (true, true)] {
        let home = Home::new("iolink-json-tls-auth");
        let listener = listener();
        let mut config = config(&listener);
        let ca_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = params.self_signed(&ca_key).unwrap();
        fs::write(home.path("ca.pem"), ca.pem()).unwrap();
        let issuer = Issuer::new(params, ca_key);
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
        config["sources"][0]["tls"] = json!({"server_name":if wrong_name {"wrong.test"} else {"iolink.test"},"ca_certificate":"ca.pem"});
        config["sources"][0]["host"] = json!("gateway.test:443");
        if bearer {
            config["sources"][0]["bearer_token_env"] = json!("CAPTUREFAB_TEST_IOLINK_TLS_TOKEN");
        } else {
            config["sources"][0]["username_env"] = json!("CAPTUREFAB_TEST_IOLINK_TLS_USER");
            config["sources"][0]["password_env"] = json!("CAPTUREFAB_TEST_IOLINK_TLS_PASSWORD");
        }
        let server = thread::spawn(move || {
            let mut stream = StreamOwned::new(
                ServerConnection::new(Arc::new(server_config)).unwrap(),
                accept(&listener),
            );
            if wrong_name {
                io::Read::read(&mut stream, &mut [0; 1]).unwrap_err();
            } else {
                let header = request(&mut stream, "byteArray", "/iolink/v1");
                assert!(header.contains("Host: gateway.test:443\r\n"));
                assert!(header.contains(if bearer {
                    "Authorization: Bearer fixture.tls.token\r\n"
                } else {
                    "Authorization: Basic b3BlcmF0b3I6c2Vuc29yLXNlY3JldA==\r\n"
                }));
                response(
                    &mut stream,
                    r#"{"iolink":{"valid":true,"value":[12,34]}}"#,
                    false,
                );
            }
        });
        fs::write(home.path("sources.json"), config.to_string()).unwrap();
        let output = home
            .command()
            .env("CAPTUREFAB_TEST_IOLINK_TLS_USER", "operator")
            .env("CAPTUREFAB_TEST_IOLINK_TLS_PASSWORD", "sensor-secret")
            .env("CAPTUREFAB_TEST_IOLINK_TLS_TOKEN", "fixture.tls.token")
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
            assert_eq!(samples(&home), [] as [Value; 0]);
        } else {
            run.success();
            assert_eq!(samples(&home).len(), 1);
        }
        server.join().unwrap();
        let recorded = fs::read_to_string(home.path("run.jsonl")).unwrap();
        for secret in [
            "sensor-secret",
            "fixture.tls.token",
            "b3BlcmF0b3I6c2Vuc29yLXNlY3JldA==",
        ] {
            assert!(!recorded.contains(secret));
            assert!(!run.stdout.contains(secret));
            assert!(!run.stderr.contains(secret));
        }
    }
}
