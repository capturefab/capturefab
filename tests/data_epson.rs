//! Independent read-only Epson Remote Ethernet CLI fixtures; no robot qualification.
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use support::cli::{Home, Run};
use support::time;
const PASSWORD_ENV: &str = "CAPTUREFAB_EPSON_FIXTURE_PASSWORD";
const PASSWORD: &str = "fixture-pass-73";
fn unhex(raw: &str) -> Vec<u8> {
    let (pairs, tail) = raw.as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn literal(name: &str) -> Vec<u8> {
    for (raw, sha) in [
        (
            include_str!("fixtures/epson/monitor-literals.json"),
            "0921fcd25f6936e389bc8643f94d0664095b60128a7f993f868840893e8e26b9",
        ),
        (
            include_str!("fixtures/epson/monitor-extra-literals.json"),
            "a080bfd845a31b9e80747d0a8265ed0a7178802c4c0e912a340d21a24406f015",
        ),
    ] {
        assert_eq!(format!("{:x}", Sha256::digest(raw.as_bytes())), sha);
        let value: Value = serde_json::from_str(raw).unwrap();
        if let Some(v) = value["literals"].get(name) {
            let raw = unhex(v["hex"].as_str().unwrap());
            assert_eq!(
                raw.len(),
                usize::try_from(v["bytes"].as_u64().unwrap()).unwrap()
            );
            assert_eq!(format!("{:x}", Sha256::digest(&raw)), v["sha256"]);
            return raw;
        }
    }
    panic!("unknown independent literal");
}
fn term(name: &str) -> &'static [u8] {
    match name {
        "crlf" => b"\r\n",
        "cr" => b"\r",
        "lf" => b"\n",
        _ => panic!("terminator"),
    }
}
fn frame(body: &str, ending: &[u8]) -> Vec<u8> {
    let mut raw = body.as_bytes().to_vec();
    raw.extend_from_slice(ending);
    raw
}
fn literal_body(name: &str) -> String {
    let raw = literal(name);
    String::from_utf8(raw.strip_suffix(b"\r\n").unwrap().to_vec()).unwrap()
}
fn line(stream: &mut TcpStream, ending: &[u8]) -> Vec<u8> {
    let mut raw = Vec::new();
    while !raw.ends_with(ending) {
        assert!(raw.len() < 4096, "request bound");
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        raw.push(byte[0]);
    }
    let command = std::str::from_utf8(raw.strip_suffix(ending).unwrap()).unwrap();
    let method = command
        .strip_prefix('$')
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    assert!(
        matches!(
            method,
            "Login"
                | "GetStatus"
                | "GetAlm"
                | "GetCurRobot"
                | "GetIO"
                | "GetIOByte"
                | "GetIOWord"
                | "GetMemIO"
                | "GetMemIOByte"
                | "GetMemIOWord"
                | "GetVariable"
        ),
        "control/unknown method"
    );
    raw
}
struct Reply {
    raw: Vec<u8>,
    delay: Duration,
    chunk: Option<usize>,
    tolerate_close: bool,
}
impl Reply {
    fn new(body: &str, ending: &[u8]) -> Self {
        Self {
            raw: frame(body, ending),
            delay: Duration::ZERO,
            chunk: None,
            tolerate_close: false,
        }
    }
}
struct Server {
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<usize>>,
}
fn accepted(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + time::secs(5);
    loop {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false).unwrap();
                s.set_nodelay(true).unwrap();
                s.set_read_timeout(Some(time::secs(3))).unwrap();
                s.set_write_timeout(Some(time::secs(3))).unwrap();
                return Some(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "Epson connection deadline");
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("{e}"),
        }
    }
}
fn transmit(stream: &mut TcpStream, reply: &Reply) {
    thread::sleep(reply.delay);
    for bytes in reply
        .raw
        .chunks(reply.chunk.unwrap_or_else(|| reply.raw.len().max(1)))
    {
        if reply.chunk.is_some() {
            thread::sleep(time::millis(100));
        }
        match stream.write_all(bytes) {
            Ok(()) => {}
            Err(e)
                if reply.tolerate_close
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    ) =>
            {
                return;
            }
            Err(e) => panic!("fixture reply write failed: {e}"),
        }
    }
}
impl Server {
    fn start(
        count: usize,
        ending: &'static [u8],
        mut handler: impl FnMut(usize, &[u8]) -> Reply + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = listener.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handle = thread::spawn(move || {
            let Some(mut stream) = accepted(&socket, &stopped) else {
                return 0;
            };
            for i in 0..count {
                let raw = line(&mut stream, ending);
                let reply = handler(i, &raw);
                transmit(&mut stream, &reply);
            }
            match stream.read(&mut [0]) {
                Ok(0) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                other => panic!("unexpected Logout/retry/control/extra request: {other:?}"),
            }
            socket.set_nonblocking(true).unwrap();
            assert_eq!(
                socket.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            count
        });
        Self {
            listener,
            stop,
            handle: Some(handle),
        }
    }
    fn address(&self) -> SocketAddr {
        self.listener.local_addr().unwrap()
    }
    fn finish(mut self, count: usize) {
        self.stop.store(true, Ordering::SeqCst);
        assert_eq!(self.handle.take().unwrap().join().unwrap(), count);
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
fn source(address: SocketAddr, profile: &str, terminator: &str, reads: Vec<Value>) -> Value {
    let mut value = json!({"id":"robot","protocol":"epson_remote","profile":profile,"address":address.to_string(),"password_env":PASSWORD_ENV,"terminator":terminator,"every_ms":3_600_000,"timeout_ms":time::ms(1000)});
    value["reads"] = Value::Array(reads);
    value
}
fn record(home: &Home, source: Value, duration: &str) -> Run {
    let mut config = json!({"version":1});
    config["sources"] = Value::Array(vec![source]);
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    home.run_env(
        &[(PASSWORD_ENV, PASSWORD)],
        &[
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            duration,
            "-o",
            "run.jsonl",
        ],
    )
}
fn rows(home: &Home, path: &str) -> Vec<Value> {
    fs::read_to_string(home.path(path))
        .unwrap()
        .lines()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect()
}
fn samples(home: &Home, status: &str) -> Vec<Value> {
    let all = rows(home, "run.jsonl");
    assert_eq!(all.last().unwrap()["status"], status);
    let origin =
        chrono::DateTime::parse_from_rfc3339(all[0]["started_at"].as_str().unwrap()).unwrap();
    let result: Vec<_> = all.into_iter().filter(|r| r["kind"] == "sample").collect();
    let mut prev = 0;
    for (i, r) in result.iter().enumerate() {
        assert_eq!(r["seq"], i + 1);
        assert_eq!(r["source"], "robot");
        assert!(r.get("source_time").is_none());
        let ns = r["elapsed_ns"].as_u64().unwrap();
        assert!(ns >= prev);
        prev = ns;
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(r["time"].as_str().unwrap()).unwrap(),
            origin + chrono::Duration::nanoseconds(i64::try_from(ns).unwrap())
        );
        r["read_duration_ns"].as_u64().unwrap();
        r["delivery_latency_ns"].as_u64().unwrap();
    }
    result
}
fn exports(home: &Home, incomplete: bool) {
    for (format, path) in [("jsonl", "values.jsonl"), ("csv", "values.csv")] {
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            path,
        ];
        if incomplete {
            args.push("--allow-incomplete");
        }
        home.ok(&args);
    }
    let original: Vec<_> = rows(home, "run.jsonl")
        .into_iter()
        .filter(|r| r["kind"] == "sample")
        .collect();
    let projected: Vec<_> = original
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r.as_object_mut().unwrap().remove("kind");
            r
        })
        .collect();
    assert_eq!(rows(home, "values.jsonl"), projected);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), original.len() + 1);
    for r in original {
        assert!(csv.contains(&format!(
            "\"{}\"",
            r["value"].to_string().replace('"', "\"\"")
        )));
    }
    for path in ["sources.json", "run.jsonl", "values.jsonl", "values.csv"] {
        assert!(
            !fs::read_to_string(home.path(path))
                .unwrap()
                .contains(PASSWORD),
            "private credential in {path}"
        );
    }
}
fn observation(
    value: &Value,
    profile: &str,
    selection: &Value,
    reply_body: &str,
    ending: &[u8],
    native: &Value,
) -> (u64, u64) {
    assert_eq!(value["kind"], "epson_remote");
    assert_eq!(value["profile"], profile);
    assert_eq!(value["resource"], selection["resource"]);
    assert_eq!(&value["selection"], selection);
    assert_eq!(value["raw_reply_text"], reply_body);
    assert_eq!(
        value["raw_reply_base64"],
        STANDARD.encode(frame(reply_body, ending))
    );
    assert_eq!(value["reply_bytes"], reply_body.len() + ending.len());
    assert_eq!(&value["native"], native);
    assert_eq!(
        value["host_window_basis"],
        "request_start_through_reply_received"
    );
    let start = value["host_window_ns"]["start"].as_u64().unwrap();
    let end = value["host_window_ns"]["end"].as_u64().unwrap();
    assert!(start > 0 && end >= start);
    (start, end)
}
fn status(eleven: bool) -> Value {
    if eleven {
        json!({"kind":"status","flags_raw":"00100000001","flag_width":11,"flag_mapping":"documented_11_flags","named_flags":{"test":false,"teach":false,"auto":true,"warning":false,"serror":false,"safeguard":false,"estop":false,"error":false,"paused":false,"running":false,"ready":true},"code_text":"0000"})
    } else {
        json!({"kind":"status","flags_raw":"0010000001","flag_width":10,"flag_mapping":"unqualified_documented_width","code_text":"0042"})
    }
}
struct Case {
    selection: Value,
    request: String,
    reply: String,
    native: Value,
}
fn cases(profile: &str) -> Vec<Case> {
    let mut c = vec![Case {
        selection: json!({"signal":"status","resource":"status"}),
        request: literal_body("status_request"),
        reply: literal_body("status_11_response"),
        native: status(true),
    }];
    for (resource, method, address, bits, text, value) in [
        ("io", "GetIO", 0, 1, "1", 1),
        ("io_byte", "GetIOByte", 1, 8, "AF", 175),
        ("io_word", "GetIOWord", 1, 16, "010F", 271),
        ("memory_io", "GetMemIO", 65_535, 1, "0", 0),
        ("memory_io_byte", "GetMemIOByte", 2, 8, "00", 0),
        ("memory_io_word", "GetMemIOWord", 3, 16, "FFFF", 65_535),
    ] {
        c.push(Case {
            selection: json!({"signal":resource,"resource":resource,"address":address}),
            request: format!("${method},{address}"),
            reply: format!("#{method},{text}"),
            native: json!({"kind":"io","text":text,"value":value,"bits":bits}),
        });
    }
    c.push(Case{selection:json!({"signal":"scalar","resource":"variable","name":"gscale","declared_type":"Double"}),request:literal_body("variable_scalar_request"),reply:literal_body("variable_scalar_reply"),native:json!({"kind":"variable","values_text":[" 1.2500"],"count":1,"datatype_provenance":"configured_no_numeric_conversion"})});
    c.push(Case{selection:json!({"signal":"array","resource":"variable","name":"gby2","declared_type":"Byte","indices":[3,0],"count":3}),request:literal_body("variable_array_request"),reply:literal_body("variable_array_reply"),native:json!({"kind":"variable","values_text":["001","-02","+3"],"count":3,"datatype_provenance":"configured_no_numeric_conversion"})});
    if profile != "rc_plus_8_0_rc800" {
        c.push(Case {
            selection: json!({"signal":"alarms","resource":"alarms"}),
            request: "$GetAlm".into(),
            reply: literal_body("alarms_response"),
            native: json!({"kind":"alarms","count":2,"alarm_codes_text":["1","9"]}),
        });
    }
    if profile != "rc_plus_7_5" {
        c.push(Case {
            selection: json!({"signal":"robot_number","resource":"current_robot"}),
            request: literal_body("current_robot_request"),
            reply: literal_body("current_robot_reply"),
            native: json!({"kind":"current_robot","number_text":"0002","number":2}),
        });
    }
    c
}

#[test]
fn immutable_primary_and_spec_ascii_literals_are_unchanged() {
    for name in [
        "status_request",
        "status_11_response",
        "word_request",
        "word_response",
        "alarms_response",
        "variable_array_request",
        "variable_error14",
        "login_request",
        "login_reply",
        "variable_scalar_request",
        "variable_scalar_reply",
        "variable_array_reply",
        "current_robot_request",
        "current_robot_reply",
        "status_10_reply",
    ] {
        literal(name);
    }
}
#[test]
fn every_qualified_monitor_method_keeps_native_text_windows_and_one_login_across_polls() {
    for (profile, terminator) in [
        ("rc_plus_7_5", "crlf"),
        ("rc_plus_8_0", "cr"),
        ("rc_plus_8_0_rc800", "lf"),
    ] {
        let home = Home::new("epson-monitor");
        let ending = term(terminator);
        let cases = cases(profile);
        let n = cases.len();
        let wire: Vec<_> = cases
            .iter()
            .map(|c| (c.request.clone(), c.reply.clone()))
            .collect();
        let server = Server::start(1 + 2 * n, ending, move |i, raw| {
            if i == 0 {
                assert_eq!(raw, frame(&format!("$Login,{PASSWORD}"), ending));
                Reply::new("#Login,0", ending)
            } else {
                let case = (i - 1) % n;
                let poll = (i - 1) / n;
                assert_eq!(raw, frame(&wire[case].0, ending));
                Reply::new(
                    if case == 0 && poll == 1 {
                        "#GetStatus,0010000001,0042"
                    } else {
                        &wire[case].1
                    },
                    ending,
                )
            }
        });
        let mut config = source(
            server.address(),
            profile,
            terminator,
            cases.iter().map(|c| c.selection.clone()).collect(),
        );
        config["every_ms"] = json!(time::ms(300));
        let report = record(&home, config, &time::arg(550)).success();
        assert_eq!(report["samples"], 2 * n);
        assert_eq!(report["status"], "complete");
        server.finish(1 + 2 * n);
        let rows = samples(&home, "complete");
        assert_eq!(rows.len(), 2 * n);
        let mut prior = 0;
        for (i, row) in rows.iter().enumerate() {
            let c = &cases[i % n];
            assert_eq!(row["signal"], c.selection["signal"]);
            let body = if i >= n && i % n == 0 {
                literal_body("status_10_reply")
            } else {
                c.reply.clone()
            };
            let native = if i >= n && i % n == 0 {
                status(false)
            } else {
                c.native.clone()
            };
            let (start, end) =
                observation(&row["value"], profile, &c.selection, &body, ending, &native);
            assert!(start >= prior);
            prior = end;
            assert_eq!(
                row["value"]["method"],
                c.request.trim_start_matches('$').split(',').next().unwrap()
            );
        }
        exports(&home, false);
    }
}
#[test]
fn hundred_values_and_full_unsigned_text_are_not_coerced_or_reconstructed() {
    let home = Home::new("epson-native-limits");
    let values: Vec<_> = (0..100).map(|i| format!("{i:04}")).collect();
    let payload = values.join(",");
    let server_payload = payload.clone();
    let server = Server::start(4, b"\r\n", move |i, raw| {
        let (req, body) = match i {
            0 => (format!("$Login,{PASSWORD}"), "#Login,0".to_owned()),
            1 => (
                "$GetVariable,gcount,UInt64".to_owned(),
                "#GetVariable,18446744073709551615".to_owned(),
            ),
            2 => (
                "$GetVariable,gvalues(0),Integer,100".to_owned(),
                format!("#GetVariable,{server_payload}"),
            ),
            _ => (
                "$GetCurRobot".to_owned(),
                "#GetCurRobot,4294967295".to_owned(),
            ),
        };
        assert_eq!(raw, frame(&req, b"\r\n"));
        Reply::new(&body, b"\r\n")
    });
    let selections = vec![
        json!({"signal":"unsigned","resource":"variable","name":"gcount","declared_type":"UInt64"}),
        json!({"signal":"array","resource":"variable","name":"gvalues","declared_type":"Integer","indices":[0],"count":100}),
        json!({"signal":"robot","resource":"current_robot"}),
    ];
    let config = source(server.address(), "rc_plus_8_0", "crlf", selections.clone());
    record(&home, config, &time::arg(500)).success();
    server.finish(4);
    let rows = samples(&home, "complete");
    assert_eq!(rows.len(), 3);
    observation(
        &rows[0]["value"],
        "rc_plus_8_0",
        &selections[0],
        "#GetVariable,18446744073709551615",
        b"\r\n",
        &json!({"kind":"variable","values_text":["18446744073709551615"],"count":1,"datatype_provenance":"configured_no_numeric_conversion"}),
    );
    observation(
        &rows[1]["value"],
        "rc_plus_8_0",
        &selections[1],
        &format!("#GetVariable,{payload}"),
        b"\r\n",
        &json!({"kind":"variable","values_text":values,"count":100,"datatype_provenance":"configured_no_numeric_conversion"}),
    );
    observation(
        &rows[2]["value"],
        "rc_plus_8_0",
        &selections[2],
        "#GetCurRobot,4294967295",
        b"\r\n",
        &json!({"kind":"current_robot","number_text":"4294967295","number":u32::MAX}),
    );
    exports(&home, false);
}
#[test]
fn malformed_late_reply_or_native_error_preserves_prior_complete_poll_and_scrubs_reflections() {
    for mode in [
        "wrong_method",
        "hex_width",
        "hex_digit",
        "extra",
        "reflection",
        "remote",
        "timeout",
        "overflow",
    ] {
        let home = Home::new("epson-prefix");
        let server = Server::start(5, b"\r\n", move |i, raw| {
            let request = match i {
                0 => format!("$Login,{PASSWORD}"),
                1 | 3 => "$GetStatus".to_owned(),
                _ => "$GetIOWord,1".to_owned(),
            };
            assert_eq!(raw, frame(&request, b"\r\n"));
            let body = if i == 0 {
                "#Login,0".to_owned()
            } else if matches!(i, 1 | 3) {
                literal_body("status_11_response")
            } else if i == 2 {
                literal_body("word_response")
            } else {
                match mode {
                    "wrong_method" => format!(
                        "{}\r\n{}",
                        literal_body("status_11_response"),
                        literal_body("word_response")
                    ),
                    "hex_width" => "#GetIOWord,10F".into(),
                    "hex_digit" => "#GetIOWord,01xF".into(),
                    "extra" => "#GetIOWord,010F,0".into(),
                    "reflection" => format!("#GetIOWord,{PASSWORD}"),
                    "remote" => "!GetIOWord,20".into(),
                    "timeout" => "!GetIOWord,19".into(),
                    "overflow" => format!("#GetIOWord,{}", "0".repeat(16 * 1024)),
                    _ => unreachable!(),
                }
            };
            let mut reply = Reply::new(&body, b"\r\n");
            reply.tolerate_close = i == 4;
            reply
        });
        let mut config = source(
            server.address(),
            "rc_plus_7_5",
            "crlf",
            vec![
                json!({"signal":"status","resource":"status"}),
                json!({"signal":"word","resource":"io_word","address":1}),
            ],
        );
        config["every_ms"] = json!(time::ms(300));
        let run = record(&home, config, &time::arg(1000));
        let error = run.failure(if mode == "timeout" { 4 } else { 1 });
        server.finish(5);
        assert_eq!(
            error["code"],
            match mode {
                "remote" => "failed",
                "timeout" => "timeout",
                _ => "incomplete",
            }
        );
        if matches!(mode, "remote" | "timeout") {
            assert_eq!(error["details"]["method"], "GetIOWord");
            assert_eq!(
                error["details"]["native_error_code"],
                if mode == "remote" { 20 } else { 19 }
            );
        }
        assert!(!run.stdout.contains(PASSWORD));
        assert!(!run.stderr.contains(PASSWORD));
        let rows = samples(&home, "failed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["value"]["native"], status(true));
        assert_eq!(rows[1]["value"]["native"]["text"], "010F");
        exports(&home, true);
    }
}
#[test]
fn login_native_failures_are_typed_private_and_never_retried_or_logged_out() {
    for (code, exit, kind) in [
        (11, 3, "auth_required"),
        (13, 3, "auth_rejected"),
        (98, 3, "auth_rejected"),
        (19, 4, "timeout"),
        (99, 1, "failed"),
    ] {
        let home = Home::new("epson-login-refused");
        let server = Server::start(1, b"\r\n", move |_, raw| {
            assert_eq!(raw, literal("login_request"));
            Reply::new(&format!("!Login,{code}"), b"\r\n")
        });
        let config = source(
            server.address(),
            "rc_plus_7_5",
            "crlf",
            vec![json!({"signal":"status","resource":"status"})],
        );
        let run = record(&home, config, &time::arg(500));
        let error = run.failure(exit);
        server.finish(1);
        assert_eq!(error["code"], kind);
        assert_eq!(error["details"]["method"], "Login");
        assert_eq!(error["details"]["native_error_code"], code);
        assert!(!run.stdout.contains(PASSWORD));
        assert!(!run.stderr.contains(PASSWORD));
        assert_eq!(samples(&home, "failed").len(), 0);
    }
}
#[test]
fn login_and_progressing_read_replies_share_one_original_deadline_without_later_get() {
    for setup in [false, true] {
        let home = Home::new("epson-deadline");
        let server = Server::start(if setup { 2 } else { 3 }, b"\r\n", move |i, raw| {
            let request = match i {
                0 => format!("$Login,{PASSWORD}"),
                1 => "$GetStatus".to_owned(),
                _ => "$GetIOWord,1".to_owned(),
            };
            assert_eq!(raw, frame(&request, b"\r\n"));
            let mut reply = Reply::new(
                if i == 0 {
                    "#Login,0"
                } else if i == 1 {
                    "#GetStatus,00100000001,0000"
                } else {
                    "#GetIOWord,010F"
                },
                b"\r\n",
            );
            if setup && i == 0 || !setup && i == 1 {
                reply.delay = time::millis(100);
            }
            if setup && i == 1 || !setup && i == 2 {
                reply.chunk = Some(6);
                reply.tolerate_close = true;
            }
            reply
        });
        let mut config = source(
            server.address(),
            "rc_plus_7_5",
            "crlf",
            vec![
                json!({"signal":"status","resource":"status"}),
                json!({"signal":"word","resource":"io_word","address":1}),
                json!({"signal":"never","resource":"memory_io","address":0}),
            ],
        );
        config["timeout_ms"] = json!(time::ms(250));
        assert_eq!(
            record(&home, config, &time::arg(1000)).failure(4)["code"],
            "timeout"
        );
        server.finish(if setup { 2 } else { 3 });
        assert_eq!(samples(&home, "failed").len(), 0);
    }
}
#[test]
fn variable_count_and_status_width_errors_are_atomic_native_framing_failures() {
    for mode in [
        "array_short",
        "array_extra",
        "array_empty",
        "nine_flags",
        "twelve_flags",
        "flag_digit",
        "status_code_width",
        "alarm_count",
    ] {
        let home = Home::new("epson-native-malformed");
        let selection = if mode.starts_with("array") {
            json!({"signal":"value","resource":"variable","name":"gby2","declared_type":"Byte","indices":[3,0],"count":3})
        } else if mode == "alarm_count" {
            json!({"signal":"value","resource":"alarms"})
        } else {
            json!({"signal":"value","resource":"status"})
        };
        let request = if mode.starts_with("array") {
            literal_body("variable_array_request")
        } else if mode == "alarm_count" {
            "$GetAlm".into()
        } else {
            "$GetStatus".into()
        };
        let server = Server::start(2, b"\r\n", move |i, raw| {
            if i == 0 {
                assert_eq!(raw, literal("login_request"));
                Reply::new("#Login,0", b"\r\n")
            } else {
                assert_eq!(raw, frame(&request, b"\r\n"));
                Reply::new(
                    match mode {
                        "array_short" => "#GetVariable,1,2",
                        "array_extra" => "#GetVariable,1,2,3,4",
                        "array_empty" => "#GetVariable,1,,3",
                        "nine_flags" => "#GetStatus,001000001,0000",
                        "twelve_flags" => "#GetStatus,001000000001,0000",
                        "flag_digit" => "#GetStatus,001x0000001,0000",
                        "status_code_width" => "#GetStatus,00100000001,000",
                        "alarm_count" => "#GetAlm,3,1,9",
                        _ => unreachable!(),
                    },
                    b"\r\n",
                )
            }
        });
        assert_eq!(
            record(
                &home,
                source(server.address(), "rc_plus_7_5", "crlf", vec![selection]),
                "500ms"
            )
            .failure(1)["code"],
            "incomplete"
        );
        server.finish(2);
        assert_eq!(samples(&home, "failed").len(), 0);
    }
}
#[test]
fn unsupported_profiles_types_and_injected_selectors_fail_before_output_or_connection() {
    let base = json!({"signal":"value","resource":"status"});
    for (profile, read, expected_exit) in [
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"current_robot"}),
            5,
        ),
        (
            "rc_plus_8_0_rc800",
            json!({"signal":"bad","resource":"alarms"}),
            5,
        ),
        (
            "rc_plus_8_0",
            json!({"signal":"bad","resource":"variable","name":"gtext","declared_type":"String"}),
            5,
        ),
        ("rc_plus_7_5", json!({"signal":"bad","resource":"start"}), 2),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"io","address":65536}),
            2,
        ),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"status","address":1}),
            2,
        ),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"variable","name":"x(0),Integer","declared_type":"Integer"}),
            2,
        ),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"variable","name":"garray","declared_type":"Byte","indices":[0,0,0,0],"count":1}),
            2,
        ),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"variable","name":"garray","declared_type":"Byte","indices":[0]}),
            2,
        ),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"variable","name":"garray","declared_type":"Byte","count":2}),
            2,
        ),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"variable","name":"garray","declared_type":"Byte","indices":[0],"count":101}),
            2,
        ),
        (
            "rc_plus_7_5",
            json!({"signal":"bad","resource":"variable","name":"garray","declared_type":"Byte","indices":[0],"count":0}),
            2,
        ),
    ] {
        let home = Home::new("epson-preopen");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = source(listener.local_addr().unwrap(), profile, "crlf", vec![read]);
        assert_eq!(
            record(&home, config, &time::arg(1000)).failure(expected_exit)["code"],
            if expected_exit == 5 {
                "unsupported"
            } else {
                "usage"
            }
        );
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    for patch in [
        json!({"profile":"rc_plus_unknown"}),
        json!({"terminator":"auto"}),
        json!({"address":"localhost:5000"}),
        json!({"address":"127.0.0.1:0"}),
        json!({"reads":[]}),
        json!({"password_env":"BAD=NAME"}),
        json!({"tls":{}}),
    ] {
        let home = Home::new("epson-config");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = source(
            listener.local_addr().unwrap(),
            "rc_plus_7_5",
            "crlf",
            vec![base.clone()],
        );
        for (k, v) in patch.as_object().unwrap() {
            config[k].clone_from(v);
        }
        assert_eq!(
            record(&home, config, &time::arg(1000)).failure(2)["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
