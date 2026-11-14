//! Independent ABB RWS XHTML/authentication fixtures; no real robot is claimed.
mod support;

use md5::{Digest as _, Md5};
use serde_json::{Value, json};
use sha2::Sha256;
#[cfg(feature = "mqtt-tls")]
use std::io::Read as _;
use std::{
    collections::BTreeMap,
    fs, io,
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use support::{
    cli::{Home, Run},
    time,
};

const USER_ENV: &str = "CAPTUREFAB_ABB_FIXTURE_USER";
const PASS_ENV: &str = "CAPTUREFAB_ABB_FIXTURE_PASS";
const USER: &str = "fixture-reader";
const PASSWORD: &str = "rws-fixture-secret-41";
const COOKIE_A: &str = "fixture-session-A";
const COOKIE_B: &str = "fixture-session-B";
const REALM: &str = "ABB-fixture";
const NONCE: &str = "finite-manual-nonce";

// Six immutable spec-derived literals copied from the pre-implementation plan.
// Hashes cover exactly UTF-8 body bytes, with no trailing newline.
const CONTROLLER: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="state"><ul><li class="pnl-ctrlstate" title="ctrlstate"><span class="ctrlstate">motoroff</span></li></ul></div></body></html>"#;
const EXECUTION2: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="state"><span class="ctrlexecstate">running</span><span class="rapidexeccycle">once</span><span class="hdtrun">false</span></div></body></html>"#;
const EXECUTION1: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="state"><ul><li class="rap-execution" title="execution"><span class="ctrlexecstate">stopped</span><span class="cycle">forever</span></li></ul></div></body></html>"#;
const PROPERTIES: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="state"><ul><li class="rap-sympropvar" title="RAPID/T_ROB1/Main/Count"><span class="symburl">RAPID/T_ROB1/Main/Count</span><span class="dattyp">num</span><span class="ndim">0</span><span class="dim"></span><span class="symtyp">var</span><span class="typurl">RAPID/num</span></li></ul></div></body></html>"#;
const NUMBER: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="state"><ul><li class="rap-data" title="RAPID/T_ROB1/Main/Count"><span class="value">1.25E-3</span></li></ul></div></body></html>"#;
const STRING: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="state"><ul><li class="rap-data" title="RAPID/T_ROB1/Main/Label"><span class="value">&quot;A&amp;B&quot;</span></li></ul></div></body></html>"#;

const LITERAL_PINS: [(&str, &str); 6] = [
    (
        CONTROLLER,
        "5c3da6c24f6cb728b997de797dd19076e27ddadd3b4c0ebaed050fd94cfdde9e",
    ),
    (
        EXECUTION2,
        "64ddc1af7b319cf4eac777b6f0becdbaa04029678a5074aa4c90a85c85b0e9f4",
    ),
    (
        EXECUTION1,
        "4ac327211acd98ffb261aeee756d0b8dd54ec50be769d620e9be5c3f82e91698",
    ),
    (
        PROPERTIES,
        "c4e2acf973afd3bbe43265a8ed06581ff010fd54371e1a2f8686a47ee6432f68",
    ),
    (
        NUMBER,
        "f6f144442bf2049460660431c3c21e29827b29ad397b33c7fc198ae4df786cdf",
    ),
    (
        STRING,
        "14718eaf6d47473725b94320eba0ae40e691225db9daf2093b99258fea8ce79c",
    ),
];

trait Duplex: io::Read + io::Write + Send {}
impl<T: io::Read + io::Write + Send> Duplex for T {}

struct Request {
    path: String,
    fields: BTreeMap<String, String>,
    received: Instant,
}
fn request(stream: &mut dyn Duplex) -> io::Result<Request> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() == 16 * 1024 {
            return Err(io::Error::other("request headers exceed fixture bound"));
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
    }
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let mut lines = text.split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| io::Error::other("missing request"))?;
    let words: Vec<_> = first.split(' ').collect();
    if words.len() != 3 || words[0] != "GET" || words[2] != "HTTP/1.1" {
        return Err(io::Error::other("only an exact HTTP GET is allowed"));
    }
    let mut fields = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("malformed request field"))?;
        if fields
            .insert(key.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return Err(io::Error::other("duplicate request header"));
        }
    }
    if fields.contains_key("transfer-encoding")
        || fields
            .get("content-length")
            .is_some_and(|value| value != "0")
    {
        return Err(io::Error::other("unexpected GET body"));
    }
    assert_eq!(
        fields.get("accept-encoding").map(String::as_str),
        Some("identity")
    );
    assert_eq!(fields.get("connection").map(String::as_str), Some("close"));
    Ok(Request {
        path: words[1].to_owned(),
        fields,
        received: Instant::now(),
    })
}

struct Response {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
    delay: Duration,
    slow: bool,
}
impl Response {
    fn xml(body: &str, api: &str) -> Self {
        Self {
            status: 200,
            body: body.into(),
            headers: vec![(
                "Content-Type".into(),
                if api == "rws2" {
                    "application/xhtml+xml;v=2.0".into()
                } else {
                    "application/xhtml+xml".into()
                },
            )],
            delay: Duration::ZERO,
            slow: false,
        }
    }
    fn challenge() -> Self {
        Self {
            status: 401,
            body: String::new(),
            headers: vec![(
                "WWW-Authenticate".into(),
                format!(
                    "Digest realm=\"{REALM}\", nonce=\"{NONCE}\", qop=\"auth\", algorithm=MD5, opaque=\"finite-opaque\""
                ),
            )],
            delay: Duration::ZERO,
            slow: false,
        }
    }
    fn cookies(mut self, secure: bool) -> Self {
        let suffix = if secure { "; Secure" } else { "" };
        self.headers.push((
            "Set-Cookie".into(),
            format!("http-session={COOKIE_A}; Path=/rw; HttpOnly{suffix}"),
        ));
        self.headers.push((
            "Set-Cookie".into(),
            format!("ABBCX={COOKIE_B}; Path=/rw{suffix}"),
        ));
        // These two must not be sent on ordinary HTTP /rw reads.
        if !secure {
            self.headers.push((
                "Set-Cookie".into(),
                "other-path=forbidden; Path=/other".into(),
            ));
            self.headers.push((
                "Set-Cookie".into(),
                "secure-only=forbidden; Path=/rw; Secure".into(),
            ));
        }
        self
    }
}
fn respond(stream: &mut dyn Duplex, response: &Response) -> io::Result<()> {
    thread::sleep(response.delay);
    let mut header = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (key, value) in &response.headers {
        header.push_str(key);
        header.push_str(": ");
        header.push_str(value);
        header.push_str("\r\n");
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    if response.slow {
        let middle = response.body.len() / 2;
        thread::sleep(time::millis(75));
        stream.write_all(&response.body.as_bytes()[..middle])?;
        stream.flush()?;
        thread::sleep(time::millis(75));
        stream.write_all(&response.body.as_bytes()[middle..])?;
    } else {
        stream.write_all(response.body.as_bytes())?;
    }
    stream.flush()
}

struct Server {
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<Instant>>>,
}
fn accepted(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + time::secs(5);
    loop {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream.set_read_timeout(Some(time::secs(3))).unwrap();
                stream.set_write_timeout(Some(time::secs(3))).unwrap();
                return Some(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "RWS fake controller did not finish"
                );
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
impl Server {
    fn plain(
        count: usize,
        handler: impl FnMut(usize, Request) -> Response + Send + 'static,
    ) -> Self {
        Self::wrapped(count, handler, |stream| Box::new(stream))
    }
    fn wrapped(
        count: usize,
        mut handler: impl FnMut(usize, Request) -> Response + Send + 'static,
        wrap: impl Fn(TcpStream) -> Box<dyn Duplex> + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let owned = listener.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handle = thread::spawn(move || {
            let mut times = Vec::new();
            while let Some(stream) = accepted(&owned, &stopped) {
                assert!(
                    times.len() < count,
                    "unexpected RWS connection/retry after declared exchanges"
                );
                let mut stream = wrap(stream);
                let request = request(stream.as_mut()).unwrap();
                times.push(request.received);
                let response = handler(times.len() - 1, request);
                let result = respond(stream.as_mut(), &response);
                if !response.slow {
                    result.unwrap();
                }
            }
            assert_eq!(times.len(), count, "missing RWS exchanges");
            times
        });
        Self {
            listener,
            stop,
            handle: Some(handle),
        }
    }
    fn source(&self, api: &str, reads: Value) -> Value {
        let mut source = json!({"id":"robot","protocol":"abb_rws","api":api,"address":self.listener.local_addr().unwrap().to_string(),"host":"controller.example","username_env":USER_ENV,"password_env":PASS_ENV,"every_ms":3_600_000,"timeout_ms":time::ms(3000)});
        source["reads"] = reads;
        source
    }
    fn finish(mut self) -> Vec<Instant> {
        self.stop.store(true, Ordering::SeqCst);
        let times = self.handle.take().unwrap().join().unwrap();
        self.listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(self.listener.accept(),Err(error) if error.kind()==io::ErrorKind::WouldBlock),
            "queued unexpected RWS connection"
        );
        times
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn digest_fields(value: &str) -> BTreeMap<String, String> {
    let value = value
        .strip_prefix("Digest ")
        .unwrap_or_else(|| panic!("expected Digest, never Basic fallback"));
    let mut fields = BTreeMap::new();
    for part in value.split(',') {
        let (key, value) = part.trim().split_once('=').unwrap();
        let value = value.trim();
        let value = if value.starts_with('"') {
            value.strip_prefix('"').unwrap().strip_suffix('"').unwrap()
        } else {
            value
        };
        assert!(fields.insert(key.to_owned(), value.to_owned()).is_none());
    }
    fields
}
fn md5(value: &str) -> String {
    format!("{:x}", Md5::digest(value.as_bytes()))
}
fn verify_digest(request: &Request, nc: u32) {
    let fields = digest_fields(&request.fields["authorization"]);
    for (key, value) in [
        ("username", USER),
        ("realm", REALM),
        ("nonce", NONCE),
        ("qop", "auth"),
        ("algorithm", "MD5"),
        ("opaque", "finite-opaque"),
        ("uri", request.path.as_str()),
    ] {
        assert_eq!(
            fields.get(key).map(String::as_str),
            Some(value),
            "incorrect Digest field {key}"
        );
    }
    let count = format!("{nc:08x}");
    assert_eq!(fields.get("nc"), Some(&count));
    let cnonce = &fields["cnonce"];
    assert!(!cnonce.is_empty() && cnonce.len() <= 256);
    let a1 = md5(&format!("{USER}:{REALM}:{PASSWORD}"));
    let a2 = md5(&format!("GET:{}", request.path));
    let expected = md5(&format!("{a1}:{NONCE}:{count}:{cnonce}:auth:{a2}"));
    assert_eq!(
        fields.get("response"),
        Some(&expected),
        "Digest response did not authenticate exact GET target"
    );
}
fn cookies(request: &Request) {
    let raw = &request.fields["cookie"];
    let fields: BTreeMap<_, _> = raw
        .split(';')
        .map(|part| part.trim().split_once('=').unwrap())
        .collect();
    assert_eq!(
        fields,
        BTreeMap::from([("ABBCX", COOKIE_B), ("http-session", COOKIE_A)])
    );
}
fn check_request(request: &Request, path: &str, api: &str) {
    assert_eq!(request.path, path);
    assert_eq!(
        request.fields.get("host").map(String::as_str),
        Some("controller.example")
    );
    assert_eq!(
        request.fields.get("accept").map(String::as_str),
        Some(if api == "rws1" {
            "application/xhtml+xml"
        } else {
            "application/xhtml+xml;v=2.0"
        })
    );
}

fn record(home: &Home, sources: &Value, duration: &str) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
    home.run_env(
        &[(USER_ENV, USER), (PASS_ENV, PASSWORD)],
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
fn rows(home: &Home, name: &str) -> Vec<Value> {
    fs::read_to_string(home.path(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn recorded(home: &Home, status: &str) -> Vec<Value> {
    let rows = rows(home, "run.jsonl");
    assert_eq!(rows[0]["kind"], "header");
    assert_eq!(rows.last().unwrap()["status"], status);
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let samples: Vec<_> = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .cloned()
        .collect();
    let mut last = BTreeMap::new();
    for (index, sample) in samples.iter().enumerate() {
        assert_eq!(sample["seq"], index + 1);
        let elapsed = sample["elapsed_ns"].as_u64().unwrap();
        let old = last
            .insert(sample["source"].as_str().unwrap(), elapsed)
            .unwrap_or(0);
        assert!(elapsed >= old);
        let received =
            chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            received,
            origin + chrono::Duration::nanoseconds(i64::try_from(elapsed).unwrap())
        );
        assert!(sample["read_duration_ns"].as_u64().unwrap() <= elapsed);
        sample["delivery_latency_ns"].as_u64().unwrap();
        assert!(sample.get("source_time").is_none());
    }
    assert_eq!(rows.last().unwrap()["samples"], samples.len());
    samples
}
fn exports(home: &Home, samples: &[Value], incomplete: bool) {
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
    let expected: Vec<_> = samples
        .iter()
        .map(|sample| {
            let mut sample = sample.clone();
            sample.as_object_mut().unwrap().remove("kind");
            sample
        })
        .collect();
    assert_eq!(rows(home, "values.jsonl"), expected);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), samples.len() + 1);
    for sample in samples {
        let field = format!("\"{}\"", sample["value"].to_string().replace('"', "\"\""));
        let names = format!(
            ",\"{}\",\"{}\",",
            sample["source"].as_str().unwrap(),
            sample["signal"].as_str().unwrap()
        );
        assert!(
            csv.lines()
                .any(|line| line.contains(&names) && line.ends_with(&field)),
            "missing exact CSV value"
        );
    }
    for name in ["sources.json", "run.jsonl", "values.jsonl", "values.csv"] {
        let text = fs::read_to_string(home.path(name)).unwrap();
        for secret in [USER, PASSWORD, COOKIE_A, COOKIE_B, "finite-manual-nonce"] {
            assert!(!text.contains(secret), "secret persisted in {name}");
        }
    }
}
fn observation(value: &Value, path: &str, xml: &str, fields: &Value) -> (u64, u64) {
    assert_eq!(value["request_path"], path);
    assert_eq!(value["fields"], *fields);
    assert_eq!(value["raw_response_xml"], xml);
    assert_eq!(
        value["response_sha256"],
        format!("{:x}", Sha256::digest(xml.as_bytes()))
    );
    let start = value["host_window_ns"]["start"].as_u64().unwrap();
    let end = value["host_window_ns"]["end"].as_u64().unwrap();
    assert!(start > 0 && end >= start);
    (start, end)
}
fn envelope(value: &Value, api: &str, resource: &str) {
    assert_eq!(value["kind"], "abb_rws");
    assert_eq!(value["api"], api);
    assert_eq!(value["resource"], resource);
}
fn state_xml(class: &str, title: &str, inner: &str) -> String {
    format!(
        "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><div class=\"state\"><ul><li class=\"{class}\" title=\"{title}\">{inner}</li></ul></div></body></html>"
    )
}
fn mode_xml() -> String {
    state_xml("pnl-opmode", "opmode", "<span class=\"opmode\">AUTO</span>")
}
fn system_xml(api: &str) -> String {
    state_xml(
        if api == "rws1" {
            "sys-system-li"
        } else {
            "sys-system"
        },
        "system",
        "<span class=\"name\">Cell &amp; Line</span><span class=\"rwversion\">6.15.08</span><span class=\"rwversionname\">RobotWare</span><span class=\"sysid\">cell-123</span><span class=\"starttm\">not-a-host-clock</span>",
    )
}
fn properties(name: &str, datatype: &str, dimensions: &str) -> String {
    PROPERTIES
        .replace("Count", name)
        .replace("RAPID/num", &format!("RAPID/{datatype}"))
        .replace(">num<", &format!(">{datatype}<"))
        .replace(
            "<span class=\"ndim\">0</span>",
            if dimensions.is_empty() {
                "<span class=\"ndim\">0</span>"
            } else {
                "<span class=\"ndim\">1</span>"
            },
        )
        .replace(
            "<span class=\"dim\"></span>",
            &format!("<span class=\"dim\">{dimensions}</span>"),
        )
}
fn symbol_path(api: &str, name: &str, properties: bool) -> String {
    let tail = if properties { "properties" } else { "data" };
    if api == "rws1" {
        format!("/rw/rapid/symbol/{tail}/RAPID/T_ROB1/Main/{name}")
    } else {
        format!("/rw/rapid/symbol/RAPID/T_ROB1/Main/{name}/{tail}")
    }
}

struct Observed {
    name: &'static str,
    path: String,
    xml: String,
    fields: Value,
}
struct ReadSpec {
    signal: &'static str,
    resource: &'static str,
    observations: Vec<Observed>,
}
fn specs(api: &str, unknown: bool) -> Vec<ReadSpec> {
    let controller = if unknown {
        CONTROLLER.replace("motoroff", "vendor_future_state")
    } else {
        CONTROLLER.into()
    };
    let mut specs = vec![
        ReadSpec {
            signal: "controller",
            resource: "controller_state",
            observations: vec![Observed {
                name: "",
                path: if api == "rws1" {
                    "/rw/panel/ctrlstate".into()
                } else {
                    "/rw/panel/ctrl-state".into()
                },
                xml: controller,
                fields: json!({"ctrlstate":if unknown {"vendor_future_state"}else{"motoroff"},"known":!unknown}),
            }],
        },
        ReadSpec {
            signal: "mode",
            resource: "operation_mode",
            observations: vec![Observed {
                name: "",
                path: "/rw/panel/opmode".into(),
                xml: mode_xml(),
                fields: json!({"opmode":"AUTO","known":true}),
            }],
        },
        ReadSpec {
            signal: "system",
            resource: "system_info",
            observations: vec![Observed {
                name: "",
                path: "/rw/system".into(),
                xml: system_xml(api),
                fields: json!({"name":"Cell & Line","rwversion":"6.15.08","rwversionname":"RobotWare","sysid":"cell-123","starttm":"not-a-host-clock"}),
            }],
        },
        ReadSpec {
            signal: "execution",
            resource: "rapid_execution",
            observations: vec![Observed {
                name: "",
                path: "/rw/rapid/execution".into(),
                xml: if api == "rws1" {
                    EXECUTION1.into()
                } else {
                    EXECUTION2.into()
                },
                fields: if api == "rws1" {
                    json!({"ctrlexecstate":"stopped","ctrlexecstate_known":true,"cycle":"forever","cycle_known":true})
                } else {
                    json!({"ctrlexecstate":"running","ctrlexecstate_known":true,"rapidexeccycle":"once","rapidexeccycle_known":true,"hdtrun_text":"false","hdtrun":false,"hdtrun_known":true})
                },
            }],
        },
    ];
    for (name, signal, datatype, dimensions, xml, text) in [
        ("Count", "number", "num", "", NUMBER.to_owned(), "1.25E-3"),
        ("Label", "label", "string", "", STRING.to_owned(), "\"A&B\""),
        (
            "Items",
            "array",
            "num",
            "3",
            state_xml(
                "rap-data",
                "RAPID/T_ROB1/Main/Items",
                "<span class=\"value\">[1.25E-3,-2,3]</span>",
            ),
            "[1.25E-3,-2,3]",
        ),
    ] {
        let symbol_url = format!("RAPID/T_ROB1/Main/{name}");
        specs.push(ReadSpec {signal,resource:"rapid_symbol",observations:vec![
            Observed {name:"properties",path:symbol_path(api,name,true),xml:properties(name,datatype,dimensions),fields:json!({"symburl":symbol_url,"dattyp":datatype,"ndim":if dimensions.is_empty(){"0"}else{"1"},"dim":dimensions,"symtyp":"var","typurl":format!("RAPID/{datatype}")})},
            Observed {name:"value",path:symbol_path(api,name,false),xml,fields:json!({"symbol_url":symbol_url,"value_text":text})},
        ]});
    }
    specs
}
fn reads(specs: &[ReadSpec]) -> Value {
    Value::Array(
        specs
            .iter()
            .map(|spec| {
                let mut value = json!({"signal":spec.signal,"resource":spec.resource});
                if spec.resource == "rapid_symbol" {
                    let name = match spec.signal {
                        "number" => "Count",
                        "label" => "Label",
                        _ => "Items",
                    };
                    value["task"] = json!("T_ROB1");
                    value["module"] = json!("Main");
                    value["symbol"] = json!(name);
                }
                value
            })
            .collect(),
    )
}
fn check_native(samples: &[Value], api: &str, expected: &[ReadSpec]) {
    let native: Vec<_> = samples
        .iter()
        .filter(|sample| sample["source"] == "robot")
        .collect();
    assert_eq!(native.len(), expected.len());
    let mut last = 0;
    for (sample, spec) in native.into_iter().zip(expected) {
        assert_eq!(sample["signal"], spec.signal);
        let value = &sample["value"];
        envelope(value, api, spec.resource);
        if spec.resource == "rapid_symbol" {
            assert_eq!(value.as_object().unwrap().len(), 5);
            assert_eq!(value["datatype_provenance"], "separate_properties_request");
            assert_eq!(value["observations"].as_object().unwrap().len(), 2);
            for expected in &spec.observations {
                let (start, end) = observation(
                    &value["observations"][expected.name],
                    &expected.path,
                    &expected.xml,
                    &expected.fields,
                );
                assert!(start >= last);
                last = end;
            }
        } else {
            assert_eq!(value.as_object().unwrap().len(), 4);
            let expected = &spec.observations[0];
            let (start, end) = observation(
                &value["observation"],
                &expected.path,
                &expected.xml,
                &expected.fields,
            );
            assert!(start >= last);
            last = end;
        }
    }
}
/// The product spaces RWS sends by at least 50 ms. The fake stamps each
/// request when its polling accept loop gets to it, which can wake late on a
/// loaded runner and shorten one gap, so single gaps get that jitter while
/// the average keeps the full pacing.
fn paced(times: &[Instant]) {
    for pair in times.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= Duration::from_millis(30),
            "RWS sends did not retain 50ms pacing"
        );
    }
    if let (Some(first), Some(last), Some(gaps)) =
        (times.first(), times.last(), u32::try_from(times.len()).ok())
        && gaps > 1
    {
        assert!(
            last.duration_since(*first) >= Duration::from_millis(45) * (gaps - 1),
            "RWS sends did not retain 50ms pacing on average"
        );
    }
}

#[test]
fn immutable_xhtml_literals_keep_the_preimplementation_hashes() {
    for (literal, sha) in LITERAL_PINS {
        assert_eq!(format!("{:x}", Sha256::digest(literal.as_bytes())), sha);
    }
}

#[test]
fn rws1_digest_nonce_cookies_full_values_and_exports_survive_two_polls() {
    let first = specs("rws1", false);
    let second = specs("rws1", true);
    let mut script = Vec::new();
    for (poll, specs) in [&first, &second].into_iter().enumerate() {
        for spec in specs {
            for observation in &spec.observations {
                if poll == 0 && matches!(spec.resource, "controller_state" | "rapid_execution") {
                    script.push((observation.path.clone(), None));
                }
                script.push((observation.path.clone(), Some(observation.xml.clone())));
            }
        }
    }
    assert_eq!(script.len(), 22);
    let server = Server::plain(22, move |index, request| {
        let (path, body) = &script[index];
        check_request(&request, path, "rws1");
        if index == 0 {
            assert!(!request.fields.contains_key("authorization"));
            assert!(!request.fields.contains_key("cookie"));
        } else {
            verify_digest(&request, u32::try_from(index).unwrap());
            if index >= 2 {
                cookies(&request);
            }
        }
        body.as_ref().map_or_else(Response::challenge, |body| {
            let response = Response::xml(body, "rws1");
            if index == 1 {
                response.cookies(false)
            } else {
                response
            }
        })
    });
    let home = Home::new("abb-rws1-complete");
    let mut source = server.source("rws1", reads(&first));
    source["every_ms"] = json!(time::ms(1000));
    let result=record(&home,&json!([source,{"id":"cycle","protocol":"simulator","signal":"cycle","every_ms":3_600_000}]),&time::arg(1900)).success();
    let times = server.finish();
    paced(&times);
    assert_eq!(result["samples"], 15);
    let samples = recorded(&home, "complete");
    let mut expected = first;
    expected.extend(second);
    check_native(&samples, "rws1", &expected);
    let simulated: Vec<_> = samples
        .iter()
        .filter(|sample| sample["source"] == "cycle")
        .collect();
    assert_eq!(simulated.len(), 1);
    assert_eq!(simulated[0]["value"], 0);
    exports(&home, &samples, false);
}

#[test]
fn malformed_late_siblings_and_http_statuses_keep_only_the_prior_complete_poll() {
    let malformed = mode_xml().replace(
        "<span class=\"opmode\">AUTO</span>",
        "<span class=\"opmode\">AUTO</span><span class=\"opmode\">MANF</span>",
    );
    let wrong_namespace = mode_xml().replace("http://www.w3.org/1999/xhtml", "urn:not-abb-xhtml");
    let forbidden = format!(
        "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><div class=\"status\"><span class=\"code\">-42</span><span class=\"msg\">{PASSWORD} {COOKIE_A}</span></div></body></html>"
    );
    let unavailable = "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><div class=\"status\"><span class=\"code\">-107</span><span class=\"msg\">No such resource</span></div></body></html>";
    for (status, xml, exit, code) in [
        (200, malformed.as_str(), 1, "incomplete"),
        (200, wrong_namespace.as_str(), 1, "incomplete"),
        (403, forbidden.as_str(), 3, "permission_denied"),
        (404, unavailable, 3, "unavailable"),
    ] {
        let xml = xml.to_owned();
        let server = Server::plain(4, move |index, request| {
            let path = if index.is_multiple_of(2) {
                "/rw/panel/ctrlstate"
            } else {
                "/rw/panel/opmode"
            };
            check_request(&request, path, "rws1");
            assert!(!request.fields.contains_key("authorization"));
            let mode = mode_xml();
            let mut response = Response::xml(
                if index == 3 {
                    &xml
                } else if index.is_multiple_of(2) {
                    CONTROLLER
                } else {
                    &mode
                },
                "rws1",
            );
            if index == 3 {
                response.status = status;
            }
            response
        });
        let home = Home::new("abb-rws-prefix");
        let all = specs("rws1", false);
        let mut source = server.source("rws1", reads(&all[..2]));
        source["every_ms"] = json!(time::ms(300));
        let run = record(&home, &json!([source]), &time::arg(1000));
        let error = run.failure(exit);
        server.finish();
        assert_eq!(error["code"], code);
        if status != 200 {
            assert_eq!(error["details"]["http_status"], status);
        }
        if status == 404 {
            assert_eq!(
                error["details"]["abb_rws_status"],
                json!({"code_text":"-107","message":"No such resource"})
            );
        }
        if status == 403 {
            assert!(error["details"].get("abb_rws_status").is_none());
        }
        for secret in [USER, PASSWORD, COOKIE_A, COOKIE_B] {
            assert!(!run.stdout.contains(secret));
            assert!(!run.stderr.contains(secret));
        }
        let samples = recorded(&home, "failed");
        assert_eq!(samples.len(), 2);
        check_native(&samples, "rws1", &all[..2]);
        exports(&home, &samples, true);
    }
}

#[test]
fn authentication_and_valid_body_progress_share_the_original_poll_deadline() {
    let server = Server::plain(3, |index, request| {
        let path = if index < 2 {
            "/rw/panel/ctrlstate"
        } else {
            "/rw/panel/opmode"
        };
        check_request(&request, path, "rws1");
        if index == 0 {
            assert!(!request.fields.contains_key("authorization"));
            let mut response = Response::challenge();
            response.delay = time::millis(100);
            response
        } else {
            verify_digest(&request, u32::try_from(index).unwrap());
            if index == 1 {
                Response::xml(CONTROLLER, "rws1")
            } else {
                let mut response = Response::xml(&mode_xml(), "rws1");
                response.slow = true;
                response
            }
        }
    });
    let home = Home::new("abb-rws-deadline");
    let all = specs("rws1", false);
    let mut source = server.source("rws1", reads(&all[..2]));
    source["timeout_ms"] = json!(time::ms(250));
    let error = record(&home, &json!([source]), &time::arg(1000)).failure(4);
    server.finish();
    assert_eq!(error["code"], "timeout");
    assert_eq!(recorded(&home, "failed").len(), 0);
}

#[test]
fn pacing_survives_poll_boundaries_and_failure_never_retries() {
    let server = Server::plain(3, |index, request| {
        check_request(&request, "/rw/panel/ctrlstate", "rws1");
        assert!(!request.fields.contains_key("authorization"));
        let mut response = Response::xml(CONTROLLER, "rws1");
        if index == 2 {
            response.status = 503;
        }
        response
    });
    let home = Home::new("abb-rws-pacing");
    let all = specs("rws1", false);
    let mut source = server.source("rws1", reads(&all[..1]));
    source["every_ms"] = json!(1);
    let error = record(&home, &json!([source]), &time::arg(1000)).failure(3);
    let times = server.finish();
    paced(&times);
    assert_eq!(error["code"], "unavailable");
    assert_eq!(error["details"]["http_status"], 503);
    let samples = recorded(&home, "failed");
    let mut expected = specs("rws1", false);
    expected.truncate(1);
    expected.push(ReadSpec {
        signal: "controller",
        resource: "controller_state",
        observations: vec![Observed {
            name: "",
            path: "/rw/panel/ctrlstate".into(),
            xml: CONTROLLER.into(),
            fields: json!({"ctrlstate":"motoroff","known":true}),
        }],
    });
    check_native(&samples, "rws1", &expected);
    exports(&home, &samples, true);
}

#[test]
fn repeated_auth_refusal_is_typed_and_never_downgrades_or_loops() {
    let server = Server::plain(2, |index, request| {
        check_request(&request, "/rw/panel/ctrlstate", "rws1");
        if index == 0 {
            assert!(!request.fields.contains_key("authorization"));
        } else {
            verify_digest(&request, 1);
        }
        Response::challenge()
    });
    let home = Home::new("abb-rws-auth-rejected");
    let all = specs("rws1", false);
    let source = server.source("rws1", reads(&all[..1]));
    let run = record(&home, &json!([source]), &time::arg(1000));
    let error = run.failure(3);
    server.finish();
    assert_eq!(error["code"], "auth_rejected");
    assert_eq!(error["details"]["http_status"], 401);
    assert_eq!(recorded(&home, "failed").len(), 0);
    for secret in [USER, PASSWORD, COOKIE_A, COOKIE_B] {
        assert!(!run.stdout.contains(secret));
        assert!(!run.stderr.contains(secret));
    }
}

#[test]
fn invalid_configuration_is_usage_before_output_or_network() {
    let base = json!({"id":"robot","protocol":"abb_rws","api":"rws1","address":"127.0.0.1:1","username_env":USER_ENV,"password_env":PASS_ENV,"reads":[{"signal":"state","resource":"controller_state"}]});
    for patch in [
        json!({"api":"automatic"}),
        json!({"api":"rws2"}),
        json!({"address":"127.0.0.1:0"}),
        json!({"address":"localhost:80"}),
        json!({"username_env":""}),
        json!({"password_env":""}),
        json!({"host":"bad\r\nhost"}),
        json!({"reads":[]}),
        json!({"reads":[{"signal":"same","resource":"controller_state"},{"signal":"same","resource":"operation_mode"}]}),
        json!({"reads":[{"signal":"state","resource":"controller_state","task":"T_ROB1"}]}),
        json!({"reads":[{"signal":"value","resource":"rapid_symbol","task":"..","module":"Main","symbol":"Count"}]}),
        json!({"reads":[{"signal":"value","resource":"rapid_symbol","task":"T_ROB1","module":"Main","symbol":"A/B"}]}),
        json!({"command":"POST /rw/rapid/execution"}),
    ] {
        let home = Home::new("abb-rws-preopen");
        let mut source = base.clone();
        for (key, value) in patch.as_object().unwrap() {
            source[key].clone_from(value);
        }
        assert_eq!(
            record(&home, &json!([source]), &time::arg(1000)).failure(2)["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}

#[cfg(not(feature = "mqtt-tls"))]
#[test]
fn rws2_without_tls_build_is_unsupported_before_output_or_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let home = Home::new("abb-rws-no-tls");
    let source = json!({"id":"robot","protocol":"abb_rws","api":"rws2","address":listener.local_addr().unwrap().to_string(),"username_env":USER_ENV,"password_env":PASS_ENV,"tls":{"server_name":"rws.test"},"reads":[{"signal":"state","resource":"controller_state"}]});
    assert_eq!(
        record(&home, &json!([source]), &time::arg(1000)).failure(5)["code"],
        "unsupported"
    );
    assert!(!home.path("run.jsonl").exists());
    assert!(matches!(listener.accept(),Err(error) if error.kind()==io::ErrorKind::WouldBlock));
}

#[cfg(feature = "mqtt-tls")]
mod tls {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};

    fn certificates(home: &Home) -> (Value, Arc<ServerConfig>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::default();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca_certificate = ca.self_signed(&ca_key).unwrap();
        fs::write(home.path("plant-ca.pem"), ca_certificate.pem()).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["rws.test".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.signed_by(&key, &issuer).unwrap();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS12])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![certificate.der().clone()],
                    PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
                )
                .unwrap();
        (
            json!({"server_name":"rws.test","ca_certificate":"plant-ca.pem"}),
            Arc::new(config),
        )
    }

    #[test]
    fn rws2_verified_tls_basic_and_secure_cookies_keep_exact_native_text() {
        let home = Home::new("abb-rws2-tls");
        let (tls, config) = certificates(&home);
        let expected = specs("rws2", false);
        let script: Vec<_> = expected
            .iter()
            .flat_map(|read| {
                read.observations
                    .iter()
                    .map(|observation| (observation.path.clone(), observation.xml.clone()))
            })
            .collect();
        assert_eq!(script.len(), 10);
        let server = Server::wrapped(
            10,
            move |index, request| {
                let (path, xml) = &script[index];
                check_request(&request, path, "rws2");
                let basic = format!("Basic {}", STANDARD.encode(format!("{USER}:{PASSWORD}")));
                assert_eq!(request.fields.get("authorization"), Some(&basic));
                if index == 0 {
                    assert!(!request.fields.contains_key("cookie"));
                    Response::xml(xml, "rws2").cookies(true)
                } else {
                    cookies(&request);
                    Response::xml(xml, "rws2")
                }
            },
            move |stream| {
                Box::new(StreamOwned::new(
                    ServerConnection::new(config.clone()).unwrap(),
                    stream,
                ))
            },
        );
        let mut source = server.source("rws2", reads(&expected));
        source["tls"] = tls;
        let result = record(&home, &json!([source]), &time::arg(1300)).success();
        let times = server.finish();
        paced(&times);
        assert_eq!(result["samples"], 7);
        let samples = recorded(&home, "complete");
        check_native(&samples, "rws2", &expected);
        exports(&home, &samples, false);
    }

    #[test]
    fn wrong_certificate_or_server_name_prevents_any_rws_http_request() {
        for wrong_name in [false, true] {
            let home = Home::new("abb-rws2-rejected");
            let (mut tls, config) = certificates(&home);
            if wrong_name {
                tls["server_name"] = json!("other.test");
            } else {
                let key = KeyPair::generate().unwrap();
                let mut params = CertificateParams::default();
                params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
                let certificate = params.self_signed(&key).unwrap();
                fs::write(home.path("other-ca.pem"), certificate.pem()).unwrap();
                tls["ca_certificate"] = json!("other-ca.pem");
            }
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = thread::spawn(move || {
                let stop = AtomicBool::new(false);
                let stream = accepted(&listener, &stop).unwrap();
                let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), stream);
                stream.read_exact(&mut [0]).unwrap_err();
            });
            let source = json!({"id":"robot","protocol":"abb_rws","api":"rws2","address":address,"username_env":USER_ENV,"password_env":PASS_ENV,"tls":tls,"every_ms":3_600_000,"reads":[{"signal":"state","resource":"controller_state"}]});
            let run = record(&home, &json!([source]), &time::arg(1000));
            let error = run.failure(3);
            server.join().unwrap();
            assert_eq!(error["code"], "auth_rejected");
            assert_eq!(recorded(&home, "failed").len(), 0);
            for secret in [USER, PASSWORD, COOKIE_A, COOKIE_B] {
                assert!(!run.stdout.contains(secret));
                assert!(!run.stderr.contains(secret));
            }
        }
    }
}
