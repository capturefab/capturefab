//! SCPI acquisition through the real CLI, with independent TCP/serial fixtures.
#[path = "support/scpi_hislip.rs"]
mod scpi_hislip;
#[path = "support/scpi_vxi11.rs"]
mod scpi_vxi11;
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::{self, Read as _, Write as _},
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
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "SCPI client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn query(stream: &mut impl io::Read, expected: &[u8]) {
    let until = Instant::now() + Duration::from_secs(5);
    let mut received = Vec::new();
    while !received.ends_with(b"\n") {
        assert!(received.len() < 8192);
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => panic!("SCPI client closed while awaiting query"),
            Ok(_) => received.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                assert!(Instant::now() < until, "SCPI query did not arrive");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("SCPI query: {error}"),
        }
    }
    assert_eq!(received, expected);
}
fn config(listener: &TcpListener, signals: Value) -> Value {
    let mut config = json!({"version":1,"sources":[{"id":"instrument","protocol":"scpi",
        "connection":{"transport":"tcp","address":listener.local_addr().unwrap().to_string()},
        "every_ms":3_600_000,"timeout_ms":1000}]});
    config["sources"][0]["signals"] = signals;
    config
}
fn record(home: &Home, config: &Value) -> Run {
    record_json(home, &config.to_string())
}
fn record_json(home: &Home, config: &str) -> Run {
    fs::write(home.path("sources.json"), config).unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "800ms",
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
fn decoded(raw: &Value) -> Vec<u8> {
    assert_eq!(raw["encoding"], "base64");
    STANDARD.decode(raw["data"].as_str().unwrap()).unwrap()
}
fn hash(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut text = String::new();
    for byte in digest {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    text
}

#[test]
fn numeric_vectors_raw_legacy_text_sentinels_and_instrument_status_survive_exports() {
    let home = Home::new("scpi-numeric");
    let listener = listener();
    let mut config = config(
        &listener,
        json!([
            {"name":"identity","query":"*IDN?","format":"text"},
            {"name":"voltage","query":"MEAS:VOLT:DC? 10,0.001","unit":"V"},
            {"name":"buffer","query":"TRAC:DATA? 1,5,\"defbuffer1\",READ","format":"numbers"},
            {"name":"overload","query":"FETC?","sentinels":[{"token":"9.9E37","status":"overload"}]},
            {"name":"special","query":"READ?"},
            {"name":"label","query":"SYST:LABEL? \"a;\"\"b\"","format":"text"},
            {"name":"error","query":"SYST:ERR?","format":"error_queue"},
            {"name":"no_error","query":"SYST:ERR?","format":"error_queue"}
        ]),
    );
    // Exercise integral exponent notation through the actual nested adapter.
    config["sources"][0]["signals"][0]["max_bytes"] = json!(1_000_000);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let replies: &[(&[u8], &[u8])] = &[
            (b"*IDN?\n", b"ACME,MODEL,\xff,SERIAL\r\n"),
            (b"MEAS:VOLT:DC? 10,0.001\n", b"+1.234567890123456789E+03\n"),
            (
                b"TRAC:DATA? 1,5,\"defbuffer1\",READ\n",
                b"+18446744073709551615,-9223372036854775808,+.5,00012.,1.234567890123456789\n",
            ),
            (b"FETC?\n", b"9.9E37\n"),
            (b"READ?\n", b"NaN\n"),
            (b"SYST:LABEL? \"a;\"\"b\"\n", b"quoted;reply\n"),
            (
                b"SYST:ERR?\n",
                b"-222,\"Out of range, \"\"voltage\"\"\"\r\n",
            ),
            (b"SYST:ERR?\n", b"+0,\"No error\"\n"),
        ];
        for (index, (expected, reply)) in replies.iter().enumerate() {
            query(&mut stream, expected);
            if index == replies.len() - 1 {
                thread::sleep(Duration::from_millis(50));
            }
            for fragment in reply.chunks(3) {
                stream.write_all(fragment).unwrap();
            }
        }
    });
    record_json(
        &home,
        &config
            .to_string()
            .replace("\"max_bytes\":1000000", "\"max_bytes\":1e6"),
    )
    .success();
    server.join().unwrap();
    assert!(
        fs::read_to_string(home.path("sources.json"))
            .unwrap()
            .contains("1e6")
    );
    let rows = samples(&home);
    assert_eq!(rows.len(), 8);
    assert_eq!(
        decoded(&rows[0]["value"]["raw"]),
        b"ACME,MODEL,\xff,SERIAL\r\n"
    );
    assert_eq!(
        decoded(&rows[0]["value"]["value"]),
        b"ACME,MODEL,\xff,SERIAL"
    );
    assert_eq!(
        rows[1]["value"]["value"].to_string(),
        "1.234567890123456789e+03"
    );
    assert_eq!(
        decoded(&rows[1]["value"]["raw"]),
        b"+1.234567890123456789E+03\n"
    );
    assert_eq!(rows[1]["value"]["unit"], "V");
    let vector = &rows[2]["value"]["value"];
    assert_eq!(vector[0].as_u64(), Some(u64::MAX));
    assert_eq!(vector[1].as_i64(), Some(i64::MIN));
    assert_eq!(vector[2], json!(0.5));
    assert_eq!(vector[3], json!(12.0));
    assert_eq!(vector[4].to_string(), "1.234567890123456789");
    assert_eq!(rows[3]["value"]["value"]["status"], "overload");
    assert_eq!(rows[4]["value"]["value"]["status"], "not_a_number");
    assert_eq!(rows[5]["value"]["value"], "quoted;reply");
    assert_eq!(rows[6]["value"]["value"]["code"], -222);
    assert_eq!(
        rows[6]["value"]["value"]["message"],
        "Out of range, \"voltage\""
    );
    assert_eq!(rows[6]["value"]["status"], "instrument_error");
    assert_eq!(rows[7]["value"]["status"], "no_error");
    let first = rows[0]["value"]["received_monotonic_ns"].as_u64().unwrap();
    let last = rows[7]["value"]["received_monotonic_ns"].as_u64().unwrap();
    assert!(last.saturating_sub(first) >= 40_000_000);
    assert!(
        rows.iter()
            .all(|row| row["elapsed_ns"] == rows[0]["elapsed_ns"]
                && row.get("source_time").is_none())
    );
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "export.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Vec<Value> = fs::read_to_string(home.path("export.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        exported.iter().map(|row| &row["value"]).collect::<Vec<_>>(),
        rows.iter().map(|row| &row["value"]).collect::<Vec<_>>()
    );
    home.ok(&["data", "export", "run.jsonl", "-o", "export.csv"]);
    assert!(
        fs::read_to_string(home.path("export.csv"))
            .unwrap()
            .contains("18446744073709551615")
    );
}

#[test]
fn large_definite_blocks_stream_to_raw_artifacts_and_recoverable_metadata() {
    let home = Home::new("scpi-waveform");
    let listener = listener();
    let config = config(
        &listener,
        json!([
            {"name":"preamble","query":"WFMOUTPRE?","format":"text"},
            {"name":"waveform","query":"CURVE?","format":"binary_block","response_prefix":":CURVE ","response_terminator":"crlf","output":"scope-{seq}.scpi","data_type":"u16","byte_order":"little","max_bytes":4_000_000}
        ]),
    );
    let payload: Vec<u8> = (0_u8..=255).cycle().take(2 * 1024 * 1024).collect();
    let expected = payload.clone();
    let header = format!("#{}{}", payload.len().to_string().len(), payload.len());
    let expected_header = header.clone();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        query(&mut stream, b"WFMOUTPRE?\n");
        stream
            .write_all(b"BYT_NR 2;BN_FMT RI;BYT_OR LSB;XINCR 1.0E-9;YMULT 2.5E-3\n")
            .unwrap();
        query(&mut stream, b"CURVE?\n");
        thread::sleep(Duration::from_millis(25));
        stream.write_all(b":CURVE ").unwrap();
        stream.write_all(header.as_bytes()).unwrap();
        for part in payload.chunks(31_337) {
            stream.write_all(part).unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
    });
    record(&home, &config).success();
    server.join().unwrap();
    let rows = samples(&home);
    assert_eq!(rows.len(), 2);
    let value = &rows[1]["value"];
    let mut wire = b":CURVE ".to_vec();
    wire.extend_from_slice(expected_header.as_bytes());
    wire.extend_from_slice(&expected);
    wire.extend_from_slice(b"\r\n");
    assert!(wire.len() > 1 << 20);
    let raw = &value["raw"];
    assert_eq!(raw["encoding"], "file");
    assert_eq!(fs::read(raw["path"].as_str().unwrap()).unwrap(), wire);
    assert_eq!(
        raw["bytes"].as_u64(),
        Some(u64::try_from(wire.len()).unwrap())
    );
    assert_eq!(raw["sha256"], hash(&wire));
    assert_eq!(value["block"]["header"], expected_header);
    assert_eq!(value["block"]["payload_bytes"], expected.len());
    assert_eq!(value["block"]["payload_offset"], 7 + expected_header.len());
    assert_eq!(decoded(&value["block"]["prefix"]), b":CURVE ");
    assert_eq!(value["block"]["elements"], expected.len() / 2);
    assert_eq!(value["block"]["byte_order"], "little");
    let metadata: Value =
        serde_json::from_slice(&fs::read(value["metadata_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(metadata["response"], *value);
    assert_eq!(metadata["format_version"], 1);
    assert!(
        value["received_monotonic_ns"].as_u64().unwrap()
            > rows[0]["value"]["received_monotonic_ns"].as_u64().unwrap()
    );
    assert!(fs::read_to_string(home.path("run.jsonl")).unwrap().len() < 16 * 1024);
}

#[test]
fn malformed_blocks_failed_siblings_and_publication_collisions_preserve_atomic_polls() {
    for mode in 0..11 {
        let home = Home::new("scpi-atomic");
        let listener = listener();
        let mut signals = json!([{"name":"block","query":"CURVE?","format":"binary_block","output":"artifact.scpi"}]);
        if mode == 2 {
            signals[0]["max_bytes"] = json!(8);
        }
        if mode == 3 {
            signals[0]["data_type"] = json!("u16");
        }
        if matches!(mode, 6 | 10) {
            signals.as_array_mut().unwrap().push(json!({"name":"sibling","query":"FETC?","format":if mode == 10 { "text" } else { "number" },"max_bytes":1_000_000}));
        }
        if mode == 7 {
            signals.as_array_mut().unwrap().push(json!({"name":"collision","query":"CURVE?","format":"binary_block","output":"artifact.scpi"}));
        }
        if mode == 8 {
            fs::write(home.path("artifact.scpi.json"), b"existing metadata").unwrap();
        }
        if mode == 9 {
            fs::write(home.path("artifact.scpi"), b"existing raw data").unwrap();
        }
        let config = config(&listener, signals);
        let raw_path = home.path("artifact.scpi");
        let metadata_path = home.path("artifact.scpi.json");
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            query(&mut stream, b"CURVE?\n");
            let reply: &[u8] = match mode {
                0 => b"#210abc",
                1 => b"#0unbounded\n",
                2 => b"#210",
                3 => b"#13",
                4 => b"#2xx",
                5 => b"#2100123456789X",
                _ => b"#2100123456789\n",
            };
            let _ = stream.write_all(reply);
            if matches!(mode, 6 | 7 | 10) {
                query(
                    &mut stream,
                    if mode == 7 { b"CURVE?\n" } else { b"FETC?\n" },
                );
                assert!(
                    !raw_path.exists() && !metadata_path.exists(),
                    "sibling queries must finish before publication"
                );
                if mode == 10 {
                    let _ = stream.write_all(format!("{}\n", "x".repeat(600_000)).as_bytes());
                } else {
                    let _ = stream.write_all(if mode == 7 {
                        b"#2100123456789\n"
                    } else {
                        b"invalid-number\n"
                    });
                }
            }
        });
        let run = record(&home, &config);
        server.join().unwrap();
        let error = run.failure(if mode == 1 { 5 } else { 1 });
        assert_eq!(
            error["code"],
            if mode == 1 {
                "unsupported"
            } else if matches!(mode, 7..=9) {
                "exists"
            } else {
                "incomplete"
            }
        );
        assert_eq!(samples(&home), [] as [Value; 0]);
        if mode == 9 {
            assert_eq!(
                fs::read(home.path("artifact.scpi")).unwrap(),
                b"existing raw data"
            );
        } else {
            assert!(!home.path("artifact.scpi").exists());
        }
        if mode == 8 {
            assert_eq!(
                fs::read(home.path("artifact.scpi.json")).unwrap(),
                b"existing metadata"
            );
        } else {
            assert!(!home.path("artifact.scpi.json").exists());
        }
        assert!(fs::read_dir(&home.work).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".partial")
        }));
    }
}

#[test]
fn oversized_reply_stops_queries_before_allocating_later_values_or_artifacts() {
    let home = Home::new("scpi-incremental-budget");
    let listener = listener();
    let config = config(
        &listener,
        json!([
            {"name":"large","query":"BIG?","format":"text","max_bytes":1_000_000},
            {"name":"must_not_query","query":"CURVE?","format":"binary_block","output":"unexpected.scpi"}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        query(&mut stream, b"BIG?\n");
        stream
            .write_all(format!("{}\n", "x".repeat(600_000)).as_bytes())
            .unwrap();
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).unwrap(),
            0,
            "client issued another query after exhausting its serialized poll budget"
        );
    });
    let error = record(&home, &config).failure(1);
    server.join().unwrap();
    assert_eq!(error["code"], "incomplete");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("serialized poll")
    );
    assert_eq!(samples(&home), [] as [Value; 0]);
    assert!(!home.path("unexpected.scpi").exists() && !home.path("unexpected.scpi.json").exists());
}

#[test]
fn legacy_error_messages_preserve_status_quoted_bytes_and_original_reply() {
    let home = Home::new("scpi-error-encoding");
    let listener = listener();
    let config = config(
        &listener,
        json!([{"name":"error","query":"SYST:ERR?","format":"error_queue"}]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        query(&mut stream, b"SYST:ERR?\n");
        stream
            .write_all(b"-222,\"Legacy \"\"voltage\"\" \xff\"\r\n")
            .unwrap();
    });
    record(&home, &config).success();
    server.join().unwrap();
    let rows = samples(&home);
    assert_eq!(rows.len(), 1);
    let value = &rows[0]["value"];
    assert_eq!(value["status"], "instrument_error");
    assert_eq!(value["value"]["code"], -222);
    assert_eq!(
        decoded(&value["value"]["message"]),
        b"Legacy \"voltage\" \xff"
    );
    assert_eq!(
        decoded(&value["raw"]),
        b"-222,\"Legacy \"\"voltage\"\" \xff\"\r\n"
    );
}

#[test]
fn query_writes_and_all_reply_bytes_share_one_total_deadline() {
    let home = Home::new("scpi-deadline");
    let listener = listener();
    let mut config = config(
        &listener,
        json!([
            {"name":"first","query":"CURVE?","format":"binary_block","output":"deadline.scpi"},
            {"name":"second","query":"FETC?"}
        ]),
    );
    config["sources"][0]["timeout_ms"] = json!(200);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        query(&mut stream, b"CURVE?\n");
        thread::sleep(Duration::from_millis(140));
        stream.write_all(b"#2100123456789\n").unwrap();
        query(&mut stream, b"FETC?\n");
        thread::sleep(Duration::from_millis(140));
        let _ = stream.write_all(b"1.25\n");
    });
    let began = Instant::now();
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    assert!(began.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    assert_eq!(samples(&home), [] as [Value; 0]);
    assert!(!home.path("deadline.scpi").exists() && !home.path("deadline.scpi.json").exists());
}

#[test]
fn invalid_queries_and_incoherent_formats_fail_before_connect_or_files() {
    let changes = [
        json!({"query":"*RST"}),
        json!({"query":"*IDN?;*RST"}),
        json!({"query":"FETC?\n*RST"}),
        json!({"query":"DISP:TEXT \"?\""}),
        json!({"query":"FETC? \"unterminated"}),
        json!({"format":"binary_block"}),
        json!({"response_terminator":"none"}),
        json!({"data_type":"f32"}),
        json!({"max_bytes":0}),
        json!({"max_bytes":1_048_577}),
        json!({"format":"binary_block","output":"x.scpi","max_bytes":1_073_741_825}),
        json!({"format":"binary_block","output":"https://example.test/x"}),
        json!({"sentinels":[{"token":"9.9E37","status":"overload"},{"token":"9.9E37","status":"same"}]}),
    ];
    for change in changes {
        let home = Home::new("scpi-invalid");
        let listener = listener();
        let mut signal = json!({"name":"value","query":"FETC?"});
        signal
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        let config = config(&listener, json!([signal]));
        record(&home, &config).failure(2);
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn fractional_and_overflow_integer_configuration_fails_before_connect() {
    for (field, token) in [
        ("max_bytes", "0.1"),
        ("max_bytes", "1.00000000000000000001"),
        ("max_bytes", "1e-9999"),
        ("max_bytes", "18446744073709551616"),
        ("max_bytes", "1.8446744073709551616e19"),
        ("max_bytes", "1e999999999999999999999999"),
        ("max_bytes", "-1"),
        ("max_bytes", "\"1\""),
        ("baud", "9600.01"),
        ("baud", "4.294967296e9"),
        ("data_bits", "8.01"),
        ("data_bits", "2.56e2"),
        ("stop_bits", "1.01"),
        ("stop_bits", "256.0"),
    ] {
        let home = Home::new("scpi-invalid-integer");
        let listener = listener();
        let mut config = config(
            &listener,
            json!([{"name":"value","query":"FETC?","max_bytes":12345}]),
        );
        if field != "max_bytes" {
            config["sources"][0]["connection"] = json!({
                "transport":"serial","device":"unopened-scpi-test-device"
            });
            config["sources"][0]["connection"][field] = json!(12345);
        }
        let error = record_json(
            &home,
            &config.to_string().replace(
                &format!("\"{field}\":12345"),
                &format!("\"{field}\":{token}"),
            ),
        )
        .failure(2);
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("expected an exact unsigned integer"),
            "{field}={token}: {error}"
        );
        assert!(!home.path("run.jsonl").exists(), "{field}={token}");
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "{field}={token}"
        );
    }
}

#[cfg(unix)]
mod serial {
    use super::*;
    use std::{
        ffi::CStr,
        fs::File,
        os::fd::{AsRawFd as _, FromRawFd as _},
    };
    fn pair() -> (File, File, String) {
        let (mut master, mut slave) = (-1, -1);
        let mut name: [libc::c_char; 256] = [0; 256];
        assert_eq!(
            // SAFETY: these live outputs/name accept openpty's independent descriptors.
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: successful openpty transfers ownership of each descriptor once.
        let master = unsafe { File::from_raw_fd(master) };
        // SAFETY: this is the second independently owned descriptor from openpty.
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            // SAFETY: each file owns a live descriptor for these flag operations.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_eq!(
                // SAFETY: file remains live and only the close-on-exec flag changes.
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) },
                0
            );
        }
        // SAFETY: master stays live while its current nonblocking flags are read.
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            // SAFETY: this owned master descriptor supports bounded polling reads.
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        // SAFETY: successful openpty writes a NUL-terminated name in this allocation.
        let name = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        (master, slave, name)
    }
    #[test]
    fn one_serial_owner_runs_multiple_scpi_queries_with_decimal_baud_notation() {
        let home = Home::new("scpi-serial");
        let (mut master, slave, device) = pair();
        let config = json!({"version":1,"sources":[{"id":"meter","protocol":"scpi","connection":{"transport":"serial","device":device,"baud":9600,"data_bits":8,"stop_bits":1},"every_ms":3_600_000,"timeout_ms":1000,"signals":[
            {"name":"voltage","query":"FETC?"},{"name":"buffer","query":"TRAC:DATA?","format":"numbers"}
        ]}]});
        let (done, keep_open) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            query(&mut master, b"FETC?\n");
            master.write_all(b"+.125\r\n").unwrap();
            query(&mut master, b"TRAC:DATA?\n");
            master.write_all(b"+18446744073709551615,-1\n").unwrap();
            let _ = keep_open.recv_timeout(Duration::from_secs(3));
        });
        record_json(
            &home,
            &config
                .to_string()
                .replace("\"baud\":9600", "\"baud\":9.6e3")
                .replace("\"data_bits\":8", "\"data_bits\":8.0")
                .replace("\"stop_bits\":1", "\"stop_bits\":1e0"),
        )
        .success();
        assert!(
            fs::read_to_string(home.path("sources.json"))
                .unwrap()
                .contains("9.6e3")
        );
        let rows = samples(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["value"]["value"], json!(0.125));
        assert_eq!(rows[1]["value"]["value"][0].as_u64(), Some(u64::MAX));
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            // SAFETY: the retained slave and termios output allocation remain live.
            unsafe { libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) },
            0
        );
        // SAFETY: successful tcgetattr initialized the settings allocation.
        let settings = unsafe { settings.assume_init() };
        assert_eq!(
            // SAFETY: this reads only the initialized termios speed field.
            unsafe { libc::cfgetispeed(&raw const settings) },
            libc::B9600
        );
        assert_eq!(
            settings.c_iflag & (libc::IXON | libc::IXOFF | libc::IXANY),
            0
        );
        let _ = done.send(());
        server.join().unwrap();
    }
}
