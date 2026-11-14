//! Independent XML/HTTP `MTConnect` fixtures; no CNC or physical-agent qualification.
mod support;
use md5::{Digest as _, Md5};
use serde_json::{Value, json};
use sha2::Sha256;
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

const LITERALS_SHA: &str = "e61d0c603203de9330614877df105f6f8db1bc01238c5062849c0c4ba2fd70fe";
const USER_ENV: &str = "CAPTUREFAB_MTCONNECT_FIXTURE_USER";
const PASS_ENV: &str = "CAPTUREFAB_MTCONNECT_FIXTURE_PASS";
const TOKEN_ENV: &str = "CAPTUREFAB_MTCONNECT_FIXTURE_TOKEN";
const USER: &str = "mt-fixture-reader";
const PASSWORD: &str = "mt-fixture-password-73";
const TOKEN: &str = "mt-fixture-bearer-91";
fn literal(name: &str) -> String {
    let text = include_str!("fixtures/mtconnect/literals.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(text.as_bytes())),
        LITERALS_SHA
    );
    let all: Value = serde_json::from_str(text).unwrap();
    let value = &all["literals"][name];
    let xml = value["xml"].as_str().unwrap();
    assert_eq!(
        xml.len(),
        usize::try_from(value["bytes"].as_u64().unwrap()).unwrap()
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(xml.as_bytes())),
        value["sha256"].as_str().unwrap()
    );
    xml.to_owned()
}
trait Duplex: io::Read + io::Write + Send {}
impl<T: io::Read + io::Write + Send> Duplex for T {}
struct Request {
    path: String,
    fields: BTreeMap<String, String>,
}
fn request(stream: &mut dyn Duplex) -> io::Result<Request> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() == 16 * 1024 {
            return Err(io::Error::other("request header bound"));
        }
        let mut one = [0];
        stream.read_exact(&mut one)?;
        bytes.push(one[0]);
    }
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let mut lines = text.split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| io::Error::other("missing request"))?;
    let words: Vec<_> = first.split(' ').collect();
    if words.len() != 3 || words[0] != "GET" || words[2] != "HTTP/1.1" {
        return Err(io::Error::other("only GET is admitted"));
    }
    let mut fields = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("bad field"))?;
        if fields
            .insert(name.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return Err(io::Error::other("duplicate field"));
        }
    }
    assert_eq!(fields["accept"], "application/xml, text/xml");
    assert_eq!(fields["accept-encoding"], "identity");
    assert_eq!(fields["connection"], "close");
    assert!(!fields.contains_key("cookie"));
    assert!(!fields.contains_key("transfer-encoding"));
    assert!(!fields.contains_key("content-length") || fields["content-length"] == "0");
    Ok(Request {
        path: words[1].to_owned(),
        fields,
    })
}
struct Reply {
    status: u16,
    body: String,
    challenge: bool,
    delay: Duration,
    slow: bool,
}
impl Reply {
    fn xml(body: &str) -> Self {
        Self {
            status: 200,
            body: body.to_owned(),
            challenge: false,
            delay: Duration::ZERO,
            slow: false,
        }
    }
    fn challenge() -> Self {
        Self {
            status: 401,
            body: String::new(),
            challenge: true,
            delay: Duration::ZERO,
            slow: false,
        }
    }
}
fn reply(stream: &mut dyn Duplex, response: &Reply) -> io::Result<()> {
    thread::sleep(response.delay);
    let mut header = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: application/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    if response.challenge {
        header.push_str("WWW-Authenticate: Digest realm=\"MT-fixture\", nonce=\"finite-mt-nonce\", algorithm=MD5, qop=\"auth\"\r\n");
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    if response.slow {
        let middle = response.body.len() / 2;
        thread::sleep(time::millis(100));
        stream.write_all(&response.body.as_bytes()[..middle])?;
        stream.flush()?;
        thread::sleep(time::millis(100));
        stream.write_all(&response.body.as_bytes()[middle..])?;
    } else {
        stream.write_all(response.body.as_bytes())?;
    }
    stream.flush()
}
struct Server {
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<usize>>,
}
fn accept(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let end = Instant::now() + time::secs(5);
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
                assert!(Instant::now() < end, "fake MTConnect agent lifetime");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
impl Server {
    fn plain(count: usize, handler: impl FnMut(usize, Request) -> Reply + Send + 'static) -> Self {
        Self::wrapped(count, handler, |stream| Box::new(stream))
    }
    fn wrapped(
        count: usize,
        mut handler: impl FnMut(usize, Request) -> Reply + Send + 'static,
        wrap: impl Fn(TcpStream) -> Box<dyn Duplex> + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let owned = listener.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handle = thread::spawn(move || {
            let mut index = 0;
            while let Some(stream) = accept(&owned, &stopped) {
                assert!(
                    index < count,
                    "unexpected request/fallback/schema fetch after declared exchanges"
                );
                let mut stream = wrap(stream);
                let request = request(stream.as_mut()).unwrap();
                let response = handler(index, request);
                index += 1;
                let sent = reply(stream.as_mut(), &response);
                if !response.slow {
                    sent.unwrap();
                }
            }
            assert_eq!(index, count, "missing MTConnect requests");
            index
        });
        Self {
            listener,
            stop,
            handle: Some(handle),
        }
    }
    fn source(&self) -> Value {
        json!({"id":"mill","protocol":"mtconnect","address":self.listener.local_addr().unwrap().to_string(),"base_path":"/UMC","signal":"observations","count":5,"every_ms":3_600_000,"timeout_ms":time::ms(2000)})
    }
    fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
        self.listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(self.listener.accept(),Err(error)if error.kind()==io::ErrorKind::WouldBlock),
            "queued extra MTConnect connection"
        );
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
fn record(home: &Home, sources: &Value, for_time: &str) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
    home.run_env(
        &[(USER_ENV, USER), (PASS_ENV, PASSWORD), (TOKEN_ENV, TOKEN)],
        &[
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            for_time,
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
fn samples(home: &Home, status: &str) -> Vec<Value> {
    let rows = rows(home, "run.jsonl");
    assert_eq!(rows[0]["kind"], "header");
    assert_eq!(rows.last().unwrap()["status"], status);
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let mut previous = BTreeMap::new();
    let values: Vec<_> = rows
        .into_iter()
        .filter(|row| row["kind"] == "sample")
        .collect();
    for (index, value) in values.iter().enumerate() {
        assert_eq!(value["seq"], index + 1);
        assert_eq!(value["source"], "mill");
        assert_eq!(value["signal"], "observations");
        assert!(value.get("source_time").is_none());
        let elapsed = value["elapsed_ns"].as_u64().unwrap();
        let before = previous
            .insert(value["source"].as_str().unwrap(), elapsed)
            .unwrap_or(0);
        assert!(elapsed >= before);
        assert!(value["read_duration_ns"].as_u64().unwrap() <= elapsed);
        let time = chrono::DateTime::parse_from_rfc3339(value["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            time,
            origin + chrono::Duration::nanoseconds(i64::try_from(elapsed).unwrap())
        );
        value["delivery_latency_ns"].as_u64().unwrap();
    }
    values
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
    let projected: Vec<_> = samples
        .iter()
        .map(|sample| {
            let mut sample = sample.clone();
            sample.as_object_mut().unwrap().remove("kind");
            sample
        })
        .collect();
    assert_eq!(rows(home, "values.jsonl"), projected);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), samples.len() + 1);
    for sample in samples {
        let field = format!("\"{}\"", sample["value"].to_string().replace('"', "\"\""));
        assert!(
            csv.lines().any(|line| line.ends_with(&field)),
            "missing opaque MTConnect CSV value"
        );
    }
    for name in ["sources.json", "run.jsonl", "values.jsonl", "values.csv"] {
        let text = fs::read_to_string(home.path(name)).unwrap();
        for secret in [USER, PASSWORD, TOKEN, "finite-mt-nonce"] {
            assert!(!text.contains(secret), "secret persisted in {name}");
        }
    }
}
fn set_header(xml: &str, first: u64, last: u64, next: u64) -> String {
    xml.replace(
        "firstSequence=\"100\"",
        &format!("firstSequence=\"{first}\""),
    )
    .replace("lastSequence=\"104\"", &format!("lastSequence=\"{last}\""))
    .replace("nextSequence=\"105\"", &format!("nextSequence=\"{next}\""))
}
fn probe(version: &str, dataset: bool) -> String {
    let mut xml = literal("probe12").replace(
        "MTConnectDevices:1.2",
        &format!("MTConnectDevices:{version}"),
    );
    if version == "2.5" {
        xml = xml
            .replace("instanceId=\"7\"", "instanceId=\"8\"")
            .replace("version=\"1.2.0\"", "version=\"2.5.0\"")
            .replace(
                "bufferSize=\"1024\"",
                "bufferSize=\"1024\" deviceModelChangeTime=\"2026-11-11T11:00:00Z\"",
            );
    }
    if dataset {
        xml=xml.replace("</DataItems>","<DataItem id=\"ds1\" category=\"EVENT\" type=\"VARIABLE\" representation=\"DATA_SET\"/></DataItems>");
    }
    xml
}
fn current(version: &str, first: u64, last: u64, next: u64) -> String {
    let mut xml = set_header(&literal("current12"), first, last, next).replace(
        "MTConnectStreams:1.2",
        &format!("MTConnectStreams:{version}"),
    );
    if version == "2.5" {
        xml = xml
            .replace("instanceId=\"7\"", "instanceId=\"8\"")
            .replace("version=\"1.2.0\"", "version=\"2.5.0\"")
            .replace(
                "bufferSize=\"1024\"",
                "bufferSize=\"1024\" deviceModelChangeTime=\"2026-11-11T11:00:00Z\"",
            )
            .replace("sequence=\"100\"", "conditionId=\"old-a\" sequence=\"100\"")
            .replace("sequence=\"101\"", "conditionId=\"old-b\" sequence=\"101\"");
    }
    xml
}
fn historical(version: &str) -> String {
    let mut xml = literal("sample25");
    if version == "1.2" {
        xml = xml
            .replace("MTConnectStreams:2.5", "MTConnectStreams:1.2")
            .replace("instanceId=\"8\"", "instanceId=\"7\"")
            .replace("version=\"2.5.0\"", "version=\"1.2.0\"")
            .replace(" deviceModelChangeTime=\"2026-11-11T11:00:00Z\"", "");
        for id in ["activation-a", "activation-b"] {
            xml = xml.replace(&format!(" conditionId=\"{id}\""), "");
        }
    }
    xml
}
fn delta() -> String {
    r#"<MTConnectStreams xmlns="urn:mtconnect.org:MTConnectStreams:2.5"><Header creationTime="not-a-source-clock" sender="fixture" instanceId="8" version="2.5.0" bufferSize="1024" firstSequence="100" lastSequence="113" nextSequence="114" deviceModelChangeTime="2026-11-11T11:00:00Z"/><Streams><DeviceStream name="MILL" uuid="synthetic-1"><ComponentStream component="Controller" componentId="c1"><Events><VariableDataSet dataItemId="ds1" sequence="110" timestamp="native-not-utc" resetTriggered="MANUAL" count="3">  <Entry key="alpha">1.2500</Entry><!--not logical text--><Entry key="gone" removed="true"/><Entry key="literal"><![CDATA[A&B]]></Entry> </VariableDataSet></Events></ComponentStream></DeviceStream></Streams></MTConnectStreams>"#.into()
}
fn document(document: &Value, path: &str, xml: &str) -> (u64, u64) {
    assert_eq!(document["request_path"], path);
    assert_eq!(document["raw_response_xml"], xml);
    assert_eq!(
        document["response_sha256"],
        format!("{:x}", Sha256::digest(xml.as_bytes()))
    );
    assert_eq!(
        document["host_window_basis"],
        "endpoint_start_through_body_received_including_auth"
    );
    let start = document["host_window_ns"]["start"].as_u64().unwrap();
    let end = document["host_window_ns"]["end"].as_u64().unwrap();
    assert!(start > 0 && end >= start);
    (start, end)
}
fn attributes(values: &[(&str, &str)]) -> Value {
    json!(values.iter().copied().collect::<BTreeMap<_, _>>())
}
fn observation(
    version: &str,
    category: &str,
    name: &str,
    attrs: Value,
    text: &str,
    content: Value,
) -> Value {
    let mut value = json!({"device_uuid":"synthetic-1","device_name":"MILL","component_id":"c1","category":category,"qname":{"namespace":format!("urn:mtconnect.org:MTConnectStreams:{version}"),"name":name},"text":text});
    value["attributes"] = attrs;
    value["content"] = content;
    value
}
fn baseline_observations(version: &str) -> Value {
    let mut observations = vec![
        observation(
            version,
            "EVENT",
            "Program",
            attributes(&[
                ("dataItemId", "e1"),
                ("sequence", "103"),
                ("timestamp", "2026-11-11T11:59:58Z"),
            ]),
            "  O1234 & SETUP  ",
            json!(["  O1234 & SETUP  "]),
        ),
        observation(
            version,
            "SAMPLE",
            "Position",
            attributes(&[
                ("dataItemId", "p1"),
                ("sequence", "102"),
                ("timestamp", "2026-11-11T11:59:59Z"),
            ]),
            "1.2500",
            json!(["1.2500"]),
        ),
    ];
    for (sequence, text, time, id) in [
        ("100", "first", "2026-11-11T11:59:56Z", "old-a"),
        ("101", "second", "2026-11-11T11:59:57Z", "old-b"),
    ] {
        let mut attrs = attributes(&[
            ("dataItemId", "cnd"),
            ("type", "SYSTEM"),
            ("nativeCode", "E01"),
            ("sequence", sequence),
            ("timestamp", time),
        ]);
        if version == "2.5" {
            attrs["conditionId"] = json!(id);
        }
        observations.push(observation(
            version,
            "CONDITION",
            "Fault",
            attrs,
            text,
            json!([text]),
        ));
    }
    json!(observations)
}
fn historical_observations(version: &str) -> Value {
    let mut observations = vec![observation(
        version,
        "SAMPLE",
        "Position",
        attributes(&[
            ("dataItemId", "p1"),
            ("sequence", "108"),
            ("timestamp", "2026-11-11T12:00:01Z"),
        ]),
        "UNAVAILABLE",
        json!(["UNAVAILABLE"]),
    )];
    for (name, sequence, id, text, time) in [
        ("Fault", "105", "activation-a", "A", "2026-11-11T12:00:00Z"),
        ("Fault", "106", "activation-b", "B", "2026-11-11T12:00:00Z"),
        ("Normal", "107", "activation-a", "", "2026-11-11T12:00:01Z"),
    ] {
        let mut attrs = attributes(&[
            ("dataItemId", "cnd"),
            ("type", "SYSTEM"),
            ("nativeCode", "E01"),
            ("sequence", sequence),
            ("timestamp", time),
        ]);
        if version == "2.5" {
            attrs["conditionId"] = json!(id);
        }
        observations.push(observation(
            version,
            "CONDITION",
            name,
            attrs,
            text,
            if text.is_empty() {
                json!([])
            } else {
                json!([text])
            },
        ));
    }
    json!(observations)
}
fn dataset_observations() -> Value {
    let node = |attrs: Value, content: Value| json!({"qname":{"namespace":"urn:mtconnect.org:MTConnectStreams:2.5","name":"Entry"},"attributes":attrs,"content":content});
    json!([observation(
        "2.5",
        "EVENT",
        "VariableDataSet",
        attributes(&[
            ("dataItemId", "ds1"),
            ("sequence", "110"),
            ("timestamp", "native-not-utc"),
            ("resetTriggered", "MANUAL"),
            ("count", "3")
        ]),
        "   ",
        json!([
            "  ",
            node(json!({"key":"alpha"}), json!(["1.2500"])),
            node(json!({"key":"gone","removed":"true"}), json!([])),
            node(json!({"key":"literal"}), json!(["A&B"])),
            " "
        ])
    )])
}
fn catalogue(value: &Value, version: &str) {
    let mut expected = BTreeMap::new();
    for (id, category, attrs) in [
        (
            "p1",
            "SAMPLE",
            json!({"id":"p1","category":"SAMPLE","type":"POSITION","units":"MILLIMETER","nativeUnits":"INCH","nativeScale":"25.4"}),
        ),
        (
            "e1",
            "EVENT",
            json!({"id":"e1","category":"EVENT","type":"PROGRAM"}),
        ),
        (
            "cnd",
            "CONDITION",
            json!({"id":"cnd","category":"CONDITION","type":"SYSTEM"}),
        ),
    ] {
        expected.insert(id.to_owned(),json!({"device_uuid":"synthetic-1","device_name":"MILL","component_id":"c1","category":category,"attributes":attrs}));
    }
    if version == "2.5" {
        expected.insert("ds1".into(),json!({"device_uuid":"synthetic-1","device_name":"MILL","component_id":"c1","category":"EVENT","attributes":{"id":"ds1","category":"EVENT","type":"VARIABLE","representation":"DATA_SET"}}));
    }
    let actual: BTreeMap<_, _> = value["catalogue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["attributes"]["id"].as_str().unwrap().to_owned(),
                entry.clone(),
            )
        })
        .collect();
    assert_eq!(actual, expected);
    let fingerprint = value["catalogue_fingerprint"].as_str().unwrap();
    assert!(fingerprint.len() == 64 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn frozen_predecoder_xml_bytes_and_hashes_are_unchanged() {
    for name in ["probe12", "current12", "sample25", "filtered_empty12"] {
        literal(name);
    }
}

#[test]
fn grouped_conditions_dataset_deltas_empty_pages_and_idle_heads_preserve_cursor() {
    for version in ["1.2", "2.5"] {
        let mut script = Vec::new();
        let mut probes = Vec::new();
        let mut heads = Vec::new();
        let mut pages = Vec::new();
        for poll in 0..4 {
            let model = probe(version, version == "2.5").replace(
                "creationTime=\"2026-11-11T12:00:00Z\"",
                &format!("creationTime=\"2026-11-11T12:00:0{poll}Z\""),
            );
            let head = current(
                version,
                100,
                if poll == 0 {
                    104
                } else if poll == 1 {
                    108
                } else {
                    113
                },
                if poll == 0 {
                    105
                } else if poll == 1 {
                    109
                } else {
                    114
                },
            );
            script.push(("/UMC/probe".to_owned(), model.clone()));
            script.push(("/UMC/current".to_owned(), head.clone()));
            probes.push(model);
            heads.push(head);
            if poll == 1 {
                let xml = historical(version);
                script.push(("/UMC/sample?from=105&count=5".into(), xml.clone()));
                pages.push(xml);
            }
            if poll == 2 {
                let xml = if version == "1.2" {
                    literal("filtered_empty12")
                } else {
                    delta()
                };
                script.push(("/UMC/sample?from=109&count=5".into(), xml.clone()));
                pages.push(xml);
            }
        }
        assert_eq!(script.len(), 10);
        let server = Server::plain(10, move |index, request| {
            assert_eq!(request.path, script[index].0);
            assert!(!request.fields.contains_key("authorization"));
            Reply::xml(&script[index].1)
        });
        let home = Home::new("mtconnect-cursor");
        let mut source = server.source();
        source["every_ms"] = json!(time::ms(300));
        assert_eq!(
            record(&home, &json!([source]), &time::arg(1050)).success()["samples"],
            4
        );
        server.finish();
        let samples = samples(&home, "complete");
        assert_eq!(samples.len(), 4);
        let mut fingerprint = None;
        for (index, sample) in samples.iter().enumerate() {
            let value = &sample["value"];
            assert_eq!(value["kind"], "mtconnect");
            assert_eq!(value["mode"], "sample");
            assert_eq!(value["namespace_version"], version);
            assert_eq!(value["configured_count"], 5);
            assert_eq!(value["effective_count"], 5);
            catalogue(value, version);
            if let Some(prior) = &fingerprint {
                assert_eq!(value["catalogue_fingerprint"], *prior);
            } else {
                fingerprint = Some(value["catalogue_fingerprint"].clone());
            }
            let (_, probe_end) = document(&value["probe"], "/UMC/probe", &probes[index]);
            assert_eq!(value["probe"]["observations"], json!([]));
            let phase = ["baseline_current", "sample", "sample", "caught_up"][index];
            assert_eq!(value["phase"], phase);
            assert_eq!(value["next"], [105, 109, 114, 114][index]);
            if index == 0 {
                assert!(value.get("from").is_none());
                assert!(value.get("checkpoint").is_none());
                let (start, _) = document(&value["data"], "/UMC/current", &heads[index]);
                assert!(start >= probe_end);
                assert_eq!(
                    value["data"]["observations"],
                    baseline_observations(version)
                );
            } else {
                assert_eq!(value["from"], [0, 105, 109, 114][index]);
                let (start, head_end) =
                    document(&value["checkpoint"], "/UMC/current", &heads[index]);
                assert!(start >= probe_end);
                assert_eq!(value["checkpoint"]["observations"], json!([]));
                if index == 3 {
                    assert!(value.get("data").is_none());
                } else {
                    let path = if index == 1 {
                        "/UMC/sample?from=105&count=5"
                    } else {
                        "/UMC/sample?from=109&count=5"
                    };
                    let (start, _) = document(&value["data"], path, &pages[index - 1]);
                    assert!(start >= head_end);
                    assert_eq!(
                        value["data"]["observations"],
                        if index == 1 {
                            historical_observations(version)
                        } else if version == "1.2" {
                            json!([])
                        } else {
                            dataset_observations()
                        }
                    );
                }
            }
        }
        exports(&home, &samples, false);
    }
}

#[test]
fn namespace_admission_and_haas_style_identity_keep_explicit_effective_count() {
    for version in [
        "1.2", "1.3", "1.4", "1.5", "1.6", "1.7", "1.8", "2.0", "2.1", "2.2", "2.3", "2.4", "2.5",
        "2.6", "2.7",
    ] {
        let transform = |xml: String, family: &str| {
            let xml = xml
                .replace(
                    &format!("MTConnect{family}:1.2"),
                    &format!("MTConnect{family}:{version}"),
                )
                .replace("bufferSize=\"1024\"", "bufferSize=\"3\"")
                .replace("name=\"MILL\"", "name=\"UMC-750SS\"")
                .replace("uuid=\"synthetic-1\"", "uuid=\"000\"")
                .replace("sender=\"fixture\"", "sender=\"NGC\"")
                .replace("version=\"1.2.0\"", "version=\"1.2.0.1.2\"");
            if matches!(
                version,
                "1.7" | "1.8" | "2.0" | "2.1" | "2.2" | "2.3" | "2.4" | "2.5" | "2.6" | "2.7"
            ) {
                xml.replace(
                    "bufferSize=\"3\"",
                    "bufferSize=\"3\" deviceModelChangeTime=\"2026-11-11T11:00:00Z\"",
                )
            } else {
                xml
            }
        };
        let model = transform(literal("probe12"), "Devices");
        let head = transform(set_header(&literal("current12"), 102, 104, 105), "Streams");
        let offered_model = model.clone();
        let offered_head = head.clone();
        let server = Server::plain(2, move |index, request| {
            assert_eq!(
                request.path,
                if index == 0 {
                    "/UMC/probe"
                } else {
                    "/UMC/current"
                }
            );
            Reply::xml(if index == 0 {
                &offered_model
            } else {
                &offered_head
            })
        });
        let home = Home::new("mtconnect-namespace");
        let mut source = server.source();
        source["mode"] = json!("current");
        source["count"] = json!(1024);
        assert_eq!(
            record(&home, &json!([source]), &time::arg(150)).success()["samples"],
            1
        );
        server.finish();
        let rows = samples(&home, "complete");
        let value = &rows[0]["value"];
        assert_eq!(value["namespace_version"], version);
        assert_eq!(value["phase"], "current");
        assert_eq!(value["effective_count"], 3);
        assert_eq!(value["configured_count"], 1024);
        assert_eq!(value["next"], 105);
        assert!(value.get("from").is_none());
        assert!(value.get("checkpoint").is_none());
        document(&value["probe"], "/UMC/probe", &model);
        document(&value["data"], "/UMC/current", &head);
        assert_eq!(value["probe"]["header_raw"]["version"], "1.2.0.1.2");
        assert_eq!(value["probe"]["header_raw"]["sender"], "NGC");
        let observations = value["data"]["observations"].as_array().unwrap();
        assert_eq!(observations.len(), 4);
        assert!(
            observations
                .iter()
                .all(|observation| observation["device_uuid"] == "000"
                    && observation["device_name"] == "UMC-750SS")
        );
        assert_eq!(observations[1]["text"], "1.2500");
    }
}

#[test]
fn lost_buffer_reset_and_changed_models_retire_without_rebase_or_partial_rows() {
    for mode in 0..4 {
        let version = if mode == 3 { "2.5" } else { "1.2" };
        let first_probe = probe(version, version == "2.5");
        let first_current = current(version, 100, 104, 105);
        let changed = match mode {
            1 => first_probe.replace("instanceId=\"7\"", "instanceId=\"9\""),
            2 => first_probe.replace("units=\"MILLIMETER\"", "units=\"CENTIMETER\""),
            3 => first_probe.replace("2026-11-11T11:00:00Z", "2026-11-11T11:01:00Z"),
            _ => first_probe.clone(),
        };
        let mut script = vec![
            ("/UMC/probe", first_probe),
            ("/UMC/current", first_current.clone()),
            ("/UMC/probe", changed),
        ];
        if mode == 0 {
            script.push(("/UMC/current", current(version, 106, 113, 114)));
        }
        let count = script.len();
        let server = Server::plain(count, move |index, request| {
            assert_eq!(request.path, script[index].0);
            Reply::xml(&script[index].1)
        });
        let home = Home::new("mtconnect-failed-prefix");
        let mut source = server.source();
        source["every_ms"] = json!(time::ms(250));
        let error = record(&home, &json!([source]), &time::arg(1000)).failure(1);
        server.finish();
        assert_eq!(error["code"], "incomplete");
        if mode == 0 {
            assert_eq!(error["details"]["requested"], 105);
            assert_eq!(error["details"]["first_sequence"], 106);
            assert_eq!(error["details"]["last_sequence"], 113);
            assert_eq!(error["details"]["instance_id"], 7);
        }
        let samples = samples(&home, "failed");
        assert_eq!(samples.len(), 1);
        document(&samples[0]["value"]["data"], "/UMC/current", &first_current);
        assert_eq!(
            samples[0]["value"]["data"]["observations"],
            baseline_observations(version)
        );
        exports(&home, &samples, true);
    }
}

#[test]
fn mismatched_references_duplicate_sequences_unknown_versions_and_error_xml_are_atomic() {
    let error_xml = format!(
        "<MTConnectError xmlns=\"urn:mtconnect.org:MTConnectError:1.2\"><Header creationTime=\"2026-11-11T12:00:00Z\" sender=\"fixture\" instanceId=\"7\" version=\"1.2.0\"/><Errors><Error errorCode=\"OUT_OF_RANGE\">{PASSWORD}</Error></Errors></MTConnectError>\n"
    );
    for mode in 0..6 {
        let model = if mode == 4 {
            probe("1.2", false).replace("MTConnectDevices:1.2", "MTConnectDevices:2.8")
        } else {
            probe("1.2", false)
        };
        let mut body = literal("current12");
        match mode {
            0 => body = body.replace("dataItemId=\"e1\"", "dataItemId=\"foreign\""),
            1 => body = body.replace("uuid=\"synthetic-1\"", "uuid=\"foreign\""),
            2 => body = body.replace("sequence=\"101\"", "sequence=\"100\""),
            3 | 5 => body = error_xml.clone(),
            _ => {}
        }
        let count = if mode == 4 { 1 } else { 2 };
        let server = Server::plain(count, move |index, request| {
            assert_eq!(
                request.path,
                if index == 0 {
                    "/UMC/probe"
                } else {
                    "/UMC/current"
                }
            );
            let mut response = Reply::xml(if index == 0 { &model } else { &body });
            if index == 1 && mode == 5 {
                response.status = 404;
            }
            response
        });
        let home = Home::new("mtconnect-atomic-error");
        let source = server.source();
        let error = record(&home, &json!([source]), &time::arg(1000)).failure(if mode == 4 {
            5
        } else {
            1
        });
        server.finish();
        assert_eq!(
            error["code"],
            if mode == 4 {
                "unsupported"
            } else if matches!(mode, 3 | 5) {
                "failed"
            } else {
                "incomplete"
            }
        );
        if matches!(mode, 3 | 5) {
            assert_eq!(
                error["details"]["mtconnect_error_codes"],
                json!(["OUT_OF_RANGE"])
            );
            assert!(!error.to_string().contains(PASSWORD));
        }
        assert_eq!(samples(&home, "failed").len(), 0);
    }
}

fn digest(request: &Request, nc: u32) {
    let raw = request.fields["authorization"]
        .strip_prefix("Digest ")
        .unwrap();
    let mut fields = BTreeMap::new();
    for entry in raw.split(',') {
        let (key, value) = entry.trim().split_once('=').unwrap();
        fields.insert(key, value.trim_matches('"'));
    }
    for (key, value) in [
        ("username", USER),
        ("realm", "MT-fixture"),
        ("nonce", "finite-mt-nonce"),
        ("uri", request.path.as_str()),
        ("qop", "auth"),
    ] {
        assert_eq!(fields[key], value);
    }
    let nc = format!("{nc:08x}");
    assert_eq!(fields["nc"], nc);
    let hash = |value: &str| format!("{:x}", Md5::digest(value.as_bytes()));
    let a1 = hash(&format!("{USER}:MT-fixture:{PASSWORD}"));
    let a2 = hash(&format!("GET:{}", request.path));
    assert_eq!(
        fields["response"],
        hash(&format!(
            "{a1}:finite-mt-nonce:{nc}:{}:auth:{a2}",
            fields["cnonce"]
        ))
    );
}

#[test]
fn digest_auth_and_all_endpoint_progress_share_one_original_deadline() {
    let server = Server::plain(4, |index, request| {
        assert_eq!(
            request.path,
            match index {
                0..=1 => "/UMC/probe",
                2 => "/UMC/current",
                _ => "/UMC/sample?from=100&count=5",
            }
        );
        if index == 0 {
            assert!(!request.fields.contains_key("authorization"));
            let mut reply = Reply::challenge();
            reply.delay = time::millis(100);
            reply
        } else {
            digest(&request, u32::try_from(index).unwrap());
            // A complete late page is otherwise valid: a fresh per-endpoint
            // budget would publish it, while the original poll budget expires.
            let body = if index == 1 {
                literal("probe12")
            } else {
                literal("current12")
            };
            let mut response = Reply::xml(&body);
            if index == 3 {
                response.slow = true;
            }
            response
        }
    });
    let home = Home::new("mtconnect-deadline");
    let mut source = server.source();
    source["auth"] = json!("digest");
    source["username_env"] = json!(USER_ENV);
    source["password_env"] = json!(PASS_ENV);
    source["start"] = json!("buffer_begin");
    source["timeout_ms"] = json!(time::ms(250));
    let error = record(&home, &json!([source]), &time::arg(1000)).failure(4);
    server.finish();
    assert_eq!(error["code"], "timeout");
    assert_eq!(samples(&home, "failed").len(), 0);
}

#[test]
fn invalid_static_paths_modes_counts_and_auth_fail_before_output() {
    let base =
        json!({"id":"mill","protocol":"mtconnect","address":"127.0.0.1:1","signal":"observations"});
    for patch in [
        json!({"address":"127.0.0.1:0"}),
        json!({"address":"localhost:8082"}),
        json!({"base_path":"/UMC/../other"}),
        json!({"base_path":"/UMC?secret=x"}),
        json!({"base_path":"/UMC%2fother"}),
        json!({"count":0}),
        json!({"count":1025}),
        json!({"mode":"history"}),
        json!({"mode":"current","start":"buffer_begin"}),
        json!({"start":105}),
        json!({"start":{"instance_id":7,"sequence":0}}),
        json!({"auth":"basic","username_env":USER_ENV,"password_env":PASS_ENV}),
        json!({"auth":"bearer","bearer_token_env":TOKEN_ENV}),
        json!({"auth":"none","username_env":USER_ENV,"password_env":PASS_ENV}),
        json!({"signal":""}),
        json!({"xpath":"//DataItem"}),
    ] {
        let home = Home::new("mtconnect-preopen");
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

#[test]
fn digest_nonce_reuse_rechallenge_and_auth_refusals_do_not_fallback() {
    let server = Server::plain(6, |index, request| {
        assert_eq!(
            request.path,
            if matches!(index, 0 | 1 | 3) {
                "/UMC/probe"
            } else {
                "/UMC/current"
            }
        );
        if index == 0 {
            assert!(!request.fields.contains_key("authorization"));
            Reply::challenge()
        } else {
            digest(&request, u32::try_from(index).unwrap());
            if index == 4 {
                Reply::challenge()
            } else {
                Reply::xml(&literal(if matches!(index, 1 | 3) {
                    "probe12"
                } else {
                    "current12"
                }))
            }
        }
    });
    let home = Home::new("mtconnect-digest");
    let mut source = server.source();
    source["mode"] = json!("current");
    source["every_ms"] = json!(time::ms(300));
    source["auth"] = json!("digest");
    source["username_env"] = json!(USER_ENV);
    source["password_env"] = json!(PASS_ENV);
    assert_eq!(
        record(&home, &json!([source]), &time::arg(500)).success()["samples"],
        2
    );
    server.finish();
    let rows = samples(&home, "complete");
    for row in &rows {
        assert_eq!(row["value"]["phase"], "current");
        assert_eq!(
            row["value"]["data"]["observations"],
            baseline_observations("1.2")
        );
    }
    exports(&home, &rows, false);
    for forbidden in [false, true] {
        let server = Server::plain(if forbidden { 1 } else { 2 }, move |index, request| {
            assert_eq!(request.path, "/UMC/probe");
            if index == 0 {
                assert!(!request.fields.contains_key("authorization"));
            } else {
                digest(&request, 1);
            }
            let mut reply = Reply::challenge();
            if forbidden {
                reply.status = 403;
            }
            reply
        });
        let home = Home::new("mtconnect-auth-refused");
        let mut source = server.source();
        source["auth"] = json!("digest");
        source["username_env"] = json!(USER_ENV);
        source["password_env"] = json!(PASS_ENV);
        let error = record(&home, &json!([source]), &time::arg(1000)).failure(3);
        server.finish();
        assert_eq!(
            error["code"],
            if forbidden {
                "permission_denied"
            } else {
                "auth_rejected"
            }
        );
        assert_eq!(
            error["details"]["http_status"],
            if forbidden { 403 } else { 401 }
        );
        assert_eq!(samples(&home, "failed").len(), 0);
        for secret in [USER, PASSWORD, "finite-mt-nonce"] {
            assert!(!error.to_string().contains(secret));
        }
    }
}

#[test]
fn resumed_u64_cursor_is_exact_and_filtered_empty_page_can_advance() {
    const CURSOR: u64 = 9_007_199_254_740_993;
    let model = literal("probe12").replace("instanceId=\"7\"", &format!("instanceId=\"{CURSOR}\""));
    let head = current("1.2", CURSOR, CURSOR + 1, CURSOR + 2)
        .replace("instanceId=\"7\"", &format!("instanceId=\"{CURSOR}\""));
    let page = format!(
        "<MTConnectStreams xmlns=\"urn:mtconnect.org:MTConnectStreams:1.2\"><Header creationTime=\"not-UTC\" sender=\"fixture\" instanceId=\"{CURSOR}\" version=\"1.2.0\" bufferSize=\"1024\" firstSequence=\"{CURSOR}\" lastSequence=\"{}\" nextSequence=\"{}\"/><Streams/></MTConnectStreams>",
        CURSOR + 1,
        CURSOR + 2
    );
    let offered_page = page.clone();
    let server = Server::plain(3, move |index, request| {
        assert_eq!(
            request.path,
            match index {
                0 => "/UMC/probe".to_owned(),
                1 => "/UMC/current".to_owned(),
                _ => format!("/UMC/sample?from={CURSOR}&count=5"),
            }
        );
        Reply::xml(match index {
            0 => &model,
            1 => &head,
            _ => &offered_page,
        })
    });
    let home = Home::new("mtconnect-u64-resume");
    let mut source = server.source();
    source["start"] = json!({"instance_id":CURSOR,"sequence":CURSOR});
    assert_eq!(
        record(&home, &json!([source]), &time::arg(150)).success()["samples"],
        1
    );
    server.finish();
    let rows = samples(&home, "complete");
    assert_eq!(rows[0]["value"]["phase"], "sample");
    assert_eq!(rows[0]["value"]["from"].as_u64().unwrap(), CURSOR);
    assert_eq!(rows[0]["value"]["next"].as_u64().unwrap(), CURSOR + 2);
    document(
        &rows[0]["value"]["data"],
        &format!("/UMC/sample?from={CURSOR}&count=5"),
        &page,
    );
    assert_eq!(rows[0]["value"]["data"]["observations"], json!([]));
    exports(&home, &rows, false);
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
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        fs::write(home.path("ca.pem"), ca_cert.pem()).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["mt.test".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&server_key, &issuer).unwrap();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS12])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
                )
                .unwrap();
        (
            json!({"server_name":"mt.test","ca_certificate":"ca.pem"}),
            Arc::new(config),
        )
    }

    #[test]
    fn verified_tls_basic_and_bearer_keep_native_xml_and_do_not_send_cookies() {
        for bearer in [false, true] {
            let home = Home::new("mtconnect-private-tls");
            let (tls, config) = certificates(&home);
            let server = Server::wrapped(
                2,
                move |index, request| {
                    assert_eq!(
                        request.path,
                        if index == 0 {
                            "/UMC/probe"
                        } else {
                            "/UMC/current"
                        }
                    );
                    assert_eq!(
                        request.fields["authorization"],
                        if bearer {
                            format!("Bearer {TOKEN}")
                        } else {
                            format!("Basic {}", STANDARD.encode(format!("{USER}:{PASSWORD}")))
                        }
                    );
                    Reply::xml(&literal(if index == 0 { "probe12" } else { "current12" }))
                },
                move |stream| {
                    Box::new(StreamOwned::new(
                        ServerConnection::new(config.clone()).unwrap(),
                        stream,
                    ))
                },
            );
            let mut source = server.source();
            source["tls"] = tls;
            source["mode"] = json!("current");
            source["auth"] = json!(if bearer { "bearer" } else { "basic" });
            if bearer {
                source["bearer_token_env"] = json!(TOKEN_ENV);
            } else {
                source["username_env"] = json!(USER_ENV);
                source["password_env"] = json!(PASS_ENV);
            }
            assert_eq!(
                record(&home, &json!([source]), &time::arg(300)).success()["samples"],
                1
            );
            server.finish();
            let rows = samples(&home, "complete");
            assert_eq!(
                rows[0]["value"]["data"]["observations"],
                baseline_observations("1.2")
            );
            document(
                &rows[0]["value"]["data"],
                "/UMC/current",
                &literal("current12"),
            );
            exports(&home, &rows, false);
        }
    }

    #[test]
    fn tls_wrong_identity_refuses_http_and_credentials() {
        let home = Home::new("mtconnect-wrong-name");
        let (mut tls, config) = certificates(&home);
        tls["server_name"] = json!("wrong.test");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let thread = thread::spawn(move || {
            let stop = AtomicBool::new(false);
            let mut stream = accept(&listener, &stop).unwrap();
            let mut connection = ServerConnection::new(config).unwrap();
            while connection.is_handshaking() {
                if connection.complete_io(&mut stream).is_err() {
                    return;
                }
            }
            panic!("untrusted name completed TLS handshake");
        });
        let source = json!({"id":"mill","protocol":"mtconnect","address":address,"tls":tls,"auth":"basic","username_env":USER_ENV,"password_env":PASS_ENV,"timeout_ms":time::ms(1000)});
        let error = record(&home, &json!([source]), &time::arg(1500)).failure(3);
        thread.join().unwrap();
        assert_eq!(error["code"], "auth_rejected");
        assert_eq!(samples(&home, "failed").len(), 0);
        for secret in [USER, PASSWORD] {
            assert!(!error.to_string().contains(secret));
        }
    }
}

#[cfg(not(feature = "mqtt-tls"))]
#[test]
fn tls_requires_the_build_feature_before_output_or_connection() {
    let home = Home::new("mtconnect-no-tls");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = json!({"id":"mill","protocol":"mtconnect","address":listener.local_addr().unwrap().to_string(),"tls":{"server_name":"mt.test"}});
    assert_eq!(
        record(&home, &json!([source]), &time::arg(1000)).failure(5)["code"],
        "unsupported"
    );
    assert!(!home.path("run.jsonl").exists());
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}
