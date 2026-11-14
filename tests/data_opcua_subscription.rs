//! Native OPC-UA subscriptions through the cached Rust server, without a helper.
#[path = "support/opcua.rs"]
#[allow(
    dead_code,
    reason = "the shared server also supports other OPC-UA integration suites"
)]
mod fixture;
mod support;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use opcua::types::{BinaryDecodable as _, ContextOwned, DataValue, StatusCode, Variant};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    time::{Duration, Instant},
};
use support::cli::Home;

const LITERALS: &str = include_str!("fixtures/opcua/subscription-literals.json");
const LITERAL_SHA: &str = "91e7cd69323f907ed8500018b37af241e002f58a0194e993f13915e3362e6411";

fn literals() -> Value {
    serde_json::from_str(LITERALS).unwrap()
}
fn hex(text: &str) -> Vec<u8> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    assert_eq!(rest, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn literal<'a>(all: &'a Value, name: &str) -> &'a Value {
    all["datavalues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == name)
        .unwrap()
}
fn native(name: &str) -> DataValue {
    let all = literals();
    let bytes = hex(literal(&all, name)["binary_hex"].as_str().unwrap());
    let context = ContextOwned::default();
    let mut input = bytes.as_slice();
    let result = DataValue::decode(&mut input, &context.context()).unwrap();
    assert_eq!(input, &[] as &[u8]);
    result
}
fn source(server: &fixture::Server, names: &[&str]) -> Value {
    json!({"id":"native","protocol":"opc_ua","endpoint":server.endpoint,
        "mode":"subscription","sampling_ms":20,"request_timeout_ms":3000,"timeout_ms":3000,
        "every_ms":1,"subscription":{"signal":"changes","publishing_ms":100,
            "queue_size":128,"max_notifications_per_publish":1024},
        "nodes":names.iter().map(|name|json!({"name":name,"node_id":server.node(name)})).collect::<Vec<_>>()})
}
fn configure(home: &Home, source: &Value) {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn samples(all: &[Value]) -> Vec<&Value> {
    all.iter().filter(|row| row["kind"] == "sample").collect()
}
fn recorded(home: &Home, duration: &str) -> Value {
    home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        duration,
        "-o",
        "run.jsonl",
    ])
}
fn clocks(all: &[Value]) {
    let start =
        chrono::DateTime::parse_from_rfc3339(all[0]["started_at"].as_str().unwrap()).unwrap();
    for (index, row) in samples(all).iter().enumerate() {
        assert_eq!(row["seq"], index + 1);
        assert_eq!(row["source"], "native");
        assert_eq!(row["signal"], "changes");
        assert!(row.get("source_time").is_none());
        assert!(row["read_duration_ns"].as_u64().unwrap() <= row["elapsed_ns"].as_u64().unwrap());
        let actual = chrono::DateTime::parse_from_rfc3339(row["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            u64::try_from((actual - start).num_nanoseconds().unwrap()).unwrap(),
            row["elapsed_ns"].as_u64().unwrap()
        );
        assert!(row["delivery_latency_ns"].is_u64());
    }
}
fn csv(text: &str) -> Vec<Vec<String>> {
    let mut result = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        if ch == '"' {
            if quoted && chars.peek() == Some(&'"') {
                field.push('"');
                chars.next();
            } else {
                quoted = !quoted;
            }
        } else if ch == ',' && !quoted {
            row.push(std::mem::take(&mut field));
        } else if ch == '\n' && !quoted {
            row.push(std::mem::take(&mut field));
            result.push(std::mem::take(&mut row));
        } else if ch != '\r' || quoted {
            field.push(ch);
        }
    }
    assert!(!quoted);
    assert_eq!(field, "");
    assert_eq!(row, Vec::<String>::new());
    result
}
fn exports(home: &Home, all: &[Value]) {
    let expected = samples(all);
    for format in ["jsonl", "csv"] {
        let output = format!("samples.{format}");
        let result = home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            &output,
        ]);
        assert_eq!(result["samples"], expected.len());
        let text = fs::read_to_string(home.path(&output)).unwrap();
        if format == "jsonl" {
            let actual: Vec<Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(actual.len(), expected.len());
            for (actual, original) in actual.iter().zip(&expected) {
                let mut original = (*original).clone();
                original.as_object_mut().unwrap().remove("kind");
                assert_eq!(actual, &original);
            }
        } else {
            let table = csv(&text);
            assert_eq!(table.len(), expected.len() + 1);
            assert_eq!(
                table[0],
                [
                    "seq",
                    "source",
                    "signal",
                    "time",
                    "elapsed_ns",
                    "read_duration_ns",
                    "delivery_latency_ns",
                    "source_time",
                    "value_json"
                ]
            );
            for (row, original) in table.iter().skip(1).zip(&expected) {
                assert_eq!(row.len(), 9);
                assert_eq!(row[0], original["seq"].to_string());
                assert_eq!(row[1], "native");
                assert_eq!(row[2], "changes");
                assert_eq!(row[3], original["time"].as_str().unwrap());
                assert_eq!(row[4], original["elapsed_ns"].to_string());
                assert_eq!(row[5], original["read_duration_ns"].to_string());
                assert_eq!(row[6], original["delivery_latency_ns"].to_string());
                assert_eq!(row[7], "");
                assert_eq!(
                    serde_json::from_str::<Value>(&row[8]).unwrap(),
                    original["value"]
                );
            }
        }
    }
}
fn assert_expected(actual: &Value, expected: &Value) {
    if let Some(object) = expected.as_object() {
        for (key, value) in object {
            if key == "value_lexeme" {
                assert_eq!(
                    actual["value"].as_f64().unwrap().to_bits(),
                    (-0.0_f64).to_bits()
                );
            } else {
                assert_expected(&actual[key], value);
            }
        }
    } else {
        assert_eq!(actual, expected);
    }
}
fn items(all: &[Value]) -> BTreeMap<String, Value> {
    let mut seen = BTreeMap::new();
    let mut last_sequence = None;
    for row in samples(all) {
        let value = &row["value"];
        assert_eq!(value["kind"], "opcua_publish");
        assert_eq!(value["delivery"], "publish");
        assert_eq!(value["lossy"], false);
        let sequence = value["sequence_number"].as_u64().unwrap();
        assert!((1..=u64::from(u32::MAX)).contains(&sequence));
        assert!(value["publish_time_ticks"].is_i64());
        assert!(value["available_sequence_numbers"].is_array());
        assert!(value["acknowledgements"].is_array());
        for ack in value["acknowledgements"].as_array().unwrap() {
            assert_eq!(ack["status_raw"], 0);
        }
        if value["message_type"] == "keepalive" {
            assert_eq!(value["notifications"], json!([]));
            if let Some(last) = last_sequence {
                assert_eq!(sequence, last + 1);
            }
        } else {
            assert_eq!(value["message_type"], "notification");
            if let Some(last) = last_sequence {
                assert_eq!(sequence, last + 1);
            } else {
                assert_eq!(sequence, 1);
            }
            last_sequence = Some(sequence);
            for block in value["notifications"].as_array().unwrap() {
                assert_eq!(block["kind"], "data_change");
                for item in block["items"].as_array().unwrap() {
                    let data = &item["data_value"];
                    assert_eq!(data["kind"], "opcua_data_value");
                    assert_eq!(data["representation"], "async_opcua_0_19_decoded");
                    assert!(data["value_present"].is_boolean());
                    assert!(data["status_present"].is_boolean());
                    seen.insert(item["name"].as_str().unwrap().to_owned(), data.clone());
                }
            }
        }
    }
    seen
}
#[test]
fn independent_native_literals_have_exact_frozen_bytes_and_extents() {
    assert_eq!(format!("{:x}", Sha256::digest(LITERALS)), LITERAL_SHA);
    let all = literals();
    assert_eq!(all["datavalues"].as_array().unwrap().len(), 27);
    for case in all["datavalues"].as_array().unwrap() {
        let raw = hex(case["binary_hex"].as_str().unwrap());
        assert_eq!(
            raw.len(),
            usize::try_from(case["bytes"].as_u64().unwrap()).unwrap()
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&raw)),
            case["sha256"].as_str().unwrap()
        );
    }
    assert_eq!(all["notification_messages"].as_array().unwrap().len(), 2);
    for case in all["notification_messages"].as_array().unwrap() {
        let raw = hex(case["notification_message_hex"].as_str().unwrap());
        assert_eq!(u64::try_from(raw.len()).unwrap(), case["bytes"]);
        assert_eq!(
            format!("{:x}", Sha256::digest(&raw)),
            case["sha256"].as_str().unwrap()
        );
    }
}
#[test]
fn cached_server_preserves_typed_native_values_and_envelope_exports() {
    let home = Home::new("opcua-subscription-native");
    const NAMES: [&str; 17] = [
        "u64_above_exact_double",
        "u64_max",
        "f32_nan_payload",
        "f32_negative_zero",
        "f64_nan_payload",
        "f64_negative_zero",
        "f64_positive_infinity",
        "f32_negative_infinity",
        "string_null",
        "string_empty",
        "string_nul_unicode",
        "bytestring_null",
        "bytestring_empty",
        "bytestring_binary",
        "int32_array_endpoints",
        "uint16_matrix_2x2",
        "empty_typed_array",
    ];
    let values = NAMES
        .iter()
        .map(|name| {
            (
                *name,
                if *name == "empty_typed_array" {
                    DataValue::from(Variant::from(Vec::<i32>::new()))
                } else {
                    native(name)
                },
            )
        })
        .collect();
    let server = fixture::Server::start(&home.root, values);
    configure(&home, &source(&server, &NAMES));
    assert_eq!(recorded(&home, "1800ms")["status"], "complete");
    let all = rows(&home);
    clocks(&all);
    let seen = items(&all);
    assert_eq!(seen.len(), NAMES.len());
    let setups: Vec<_> = samples(&all)
        .iter()
        .filter_map(|row| row["value"].get("setup"))
        .collect();
    assert_eq!(setups.len(), 1);
    assert_eq!(setups[0]["requested"]["publishing_ms"], 100);
    assert_eq!(setups[0]["requested"]["sampling_ms"], 20);
    assert_eq!(setups[0]["requested"]["queue_size"], 128);
    assert_eq!(
        setups[0]["requested"]["max_notifications_per_publish"],
        1024
    );
    let revisions = setups[0]["revised"]["items"].as_array().unwrap();
    assert_eq!(revisions.len(), NAMES.len());
    for revised in revisions {
        assert!(revised["queue_size"].as_u64().unwrap() >= 2);
        assert!(revised["monitored_item_id"].as_u64().unwrap() > 0);
        assert!(revised["sampling_ms"].as_f64().unwrap().is_finite());
    }
    let cases = literals();
    for name in NAMES {
        let actual = &seen[name];
        assert_eq!(actual["value_present"], true);
        let expected = if name == "empty_typed_array" {
            &literal(&cases, "int32_array_empty")["expected"]
        } else {
            &literal(&cases, name)["expected"]
        };
        assert_expected(actual, expected);
        if expected["value"]["classification"]
            .as_str()
            .is_some_and(|class| class != "finite")
        {
            assert!(
                actual["value"].get("value").is_none(),
                "nonfinite values must not turn into JSON null"
            );
        }
        if expected["value"]["is_null"] == true {
            assert!(actual["value"].get("value").is_none());
            assert!(actual["value"].get("base64").is_none());
        }
        if let Some(binary) = actual.get("value_binary_base64") {
            assert_eq!(actual["value_binary_encoding"], "ua_binary_decoded_variant");
            let binary = STANDARD.decode(binary.as_str().unwrap()).unwrap();
            assert_ne!(binary, Vec::<u8>::new());
            if name != "empty_typed_array" {
                let raw = hex(literal(&cases, name)["binary_hex"].as_str().unwrap());
                assert_eq!(binary, &raw[1..]);
            }
        }
        if name == "empty_typed_array" {
            assert_eq!(
                actual["value"]["null_empty_distinction"],
                "not_retained_by_sdk"
            );
        }
    }
    exports(&home, &all);
}
#[test]
fn non_good_data_quality_is_captured_as_data_and_has_no_arbitrary_source_time() {
    let home = Home::new("opcua-subscription-quality");
    let server = fixture::Server::start(
        &home.root,
        vec![
            ("bad", native("bad_without_value")),
            (
                "uncertain",
                DataValue {
                    value: Some(Variant::Int32(-1)),
                    status: Some(StatusCode::from(0x4000_0000_u32)),
                    ..Default::default()
                },
            ),
        ],
    );
    configure(&home, &source(&server, &["bad", "uncertain"]));
    recorded(&home, "1800ms");
    let all = rows(&home);
    clocks(&all);
    let seen = items(&all);
    assert_eq!(seen["bad"]["status_raw"], 0x8005_0000_u32);
    assert_eq!(seen["uncertain"]["status_raw"], 0x4000_0000_u32);
    exports(&home, &all);
}
#[test]
fn namespace_resolution_and_verified_security_are_shared_with_subscription_mode() {
    let home = Home::new("opcua-subscription-security");
    let server =
        fixture::Server::start(&home.root, vec![("wide", native("u64_above_exact_double"))]);
    let (certificate, private_key) = fixture::client_keypair(&home.root.join("client-pki"));
    let mut config = source(&server, &["wide"]);
    config["nodes"][0]["namespace_uri"] = json!(fixture::NAMESPACE);
    config["nodes"][0]["node_id"] = json!("s=wide");
    config["security"] = json!({"policy":"basic256_sha256","certificate":certificate,
        "private_key":private_key,"server_certificate":server.certificate});
    configure(&home, &config);
    recorded(&home, "2400ms");
    let all = rows(&home);
    clocks(&all);
    let seen = items(&all);
    assert_eq!(seen["wide"]["value"]["value"], 9_007_199_254_740_993_u64);
    assert!(
        server
            .namespace_reads
            .load(std::sync::atomic::Ordering::Relaxed)
            <= 1
    );
}
#[test]
fn original_setup_deadline_covers_namespace_read_and_no_node_values_escape() {
    let home = Home::new("opcua-subscription-setup-deadline");
    let server = fixture::Server::start_namespace_reply(
        &home.root,
        vec![("wide", native("u64_max"))],
        DataValue::from(Variant::from(vec![
            opcua::types::UAString::from("http://opcfoundation.org/UA/"),
            opcua::types::UAString::from(fixture::NAMESPACE),
        ])),
        Duration::from_millis(450),
    );
    let mut config = source(&server, &["wide"]);
    config["timeout_ms"] = json!(250);
    config["nodes"][0]["node_id"] = json!("s=wide");
    config["nodes"][0]["namespace_uri"] = json!(fixture::NAMESPACE);
    configure(&home, &config);
    let began = Instant::now();
    let error = home.fails(
        4,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "2s",
            "-o",
            "run.jsonl",
        ],
    );
    assert_eq!(error["code"], "timeout");
    assert!(began.elapsed() < Duration::from_secs(6));
    assert_eq!(samples(&rows(&home)).len(), 0);
    assert_eq!(server.reads.load(std::sync::atomic::Ordering::Relaxed), 0);
}
#[test]
fn invalid_subscription_configuration_fails_before_output_or_connection() {
    let home = Home::new("opcua-subscription-config");
    let base = json!({"id":"native","protocol":"opc_ua","endpoint":"opc.tcp://127.0.0.1:9/fixture",
        "mode":"subscription","nodes":[{"name":"v","node_id":"ns=1;s=v"}],"subscription":{}});
    let mut cases = Vec::new();
    for patch in [
        json!({"publishing_ms":0}),
        json!({"queue_size":1}),
        json!({"queue_size":4097}),
        json!({"max_notifications_per_publish":0}),
        json!({"max_notifications_per_publish":1025}),
        json!({"signal":""}),
        json!({"queue_size":2.5}),
    ] {
        let mut config = base.clone();
        config["subscription"] = patch;
        cases.push(config);
    }
    let mut read = base;
    read["mode"] = json!("read");
    cases.push(read);
    for config in cases {
        configure(&home, &config);
        let error = home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "20ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[path = "support/opcua_gap.rs"]
mod gap;

struct RecordingChild(Option<std::process::Child>);
impl RecordingChild {
    fn finish(mut self) -> support::cli::Run {
        let end = Instant::now() + Duration::from_secs(12);
        let mut child = self.0.take().unwrap();
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= end {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bounded subscription child did not exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        support::cli::Run::of_output(child.wait_with_output().unwrap(), &["data", "record"])
    }
}
impl Drop for RecordingChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn namespace_value(marker: &str) -> DataValue {
    DataValue::from(Variant::from(vec![
        opcua::types::UAString::from("http://opcfoundation.org/UA/"),
        opcua::types::UAString::from("urn:capturefab:opcua-fixture"),
        opcua::types::UAString::from(fixture::NAMESPACE),
        opcua::types::UAString::from(marker),
    ]))
}
fn namespace_marker(envelope: &Value) -> &str {
    let notifications = envelope["notifications"].as_array().unwrap();
    assert_eq!(notifications.len(), 1);
    let items = notifications[0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "namespace");
    let array = &items[0]["data_value"]["value"];
    assert_eq!(array["type"], "Array");
    assert_eq!(array["element_type"], "String");
    let values = array["values"].as_array().unwrap();
    assert_eq!(values.len(), 4);
    assert_eq!(values[0]["value"], "http://opcfoundation.org/UA/");
    assert_eq!(values[1]["value"], "urn:capturefab:opcua-fixture");
    assert_eq!(values[2]["value"], fixture::NAMESPACE);
    values[3]["value"].as_str().unwrap()
}
#[test]
fn real_republish_gap_releases_original_publish_at_actual_recovery_frontier_and_exports() {
    let home = Home::new("opcua-subscription-recovery");
    let server = fixture::Server::start_namespace_reply(
        &home.root,
        vec![("unused", native("u64_max"))],
        namespace_value("urn:initial"),
        Duration::ZERO,
    );
    let upstream = server
        .endpoint
        .strip_prefix("opc.tcp://")
        .unwrap()
        .strip_suffix("/fixture")
        .unwrap()
        .parse()
        .unwrap();
    let proxy = gap::Proxy::start(upstream);
    let mut config = source(&server, &["unused"]);
    config["endpoint"] = json!(proxy.endpoint);
    config["nodes"] = json!([{"name":"namespace","node_id":"i=2255"}]);
    configure(&home, &config);
    let child = home
        .command()
        .args([
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "3s",
            "-o",
            "run.jsonl",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let child = RecordingChild(Some(child));
    assert!(matches!(
        proxy.events.recv_timeout(Duration::from_secs(5)).unwrap(),
        gap::Event::Suppressed
    ));
    server.set_namespace_reply(namespace_value("urn:middle"));
    assert!(matches!(
        proxy.events.recv_timeout(Duration::from_secs(5)).unwrap(),
        gap::Event::Repaired
    ));
    server.set_namespace_reply(namespace_value("urn:after"));
    let run = child.finish();
    assert_eq!(run.success()["status"], "complete");
    let trace = proxy.finish();
    assert_eq!(trace.accepted, 2);
    assert_eq!(trace.endpoint_rewrites, 2);
    assert_eq!(trace.suppressed, 1);
    assert_eq!(trace.republished, 1);
    assert!(trace.repair_forwarded);
    assert!(trace.ordinary_after_repair > 0);
    assert!(trace.null_available > 0);
    assert_ne!(trace.original_notification, Vec::<u8>::new());
    assert_eq!(trace.original_notification, trace.repaired_notification);
    for sequence in [1, 2] {
        let acks: Vec<_> = trace
            .acknowledgements
            .iter()
            .filter(|(_, seq, _)| *seq == sequence)
            .collect();
        assert_ne!(acks, Vec::<&(u32, u32, bool)>::new());
        for (subscription, _, repaired) in acks {
            assert_eq!(*subscription, trace.subscription_id);
            assert!(*repaired);
        }
    }
    let all = rows(&home);
    clocks(&all);
    let recorded = samples(&all);
    let repair = recorded
        .iter()
        .position(|row| {
            row["value"]["delivery"] == "republish" && row["value"]["sequence_number"] == 1
        })
        .unwrap();
    let release = recorded
        .iter()
        .position(|row| row["value"]["kind"] == "opcua_recovery_release")
        .unwrap();
    let later = recorded
        .iter()
        .position(|row| {
            row["value"]["delivery"] == "publish" && row["value"]["sequence_number"] == 3
        })
        .unwrap();
    assert!(repair < release && release < later);
    assert_eq!(namespace_marker(&recorded[repair]["value"]), "urn:initial");
    let released = &recorded[release]["value"];
    assert_eq!(released["clock_basis"], "recovery_completed_receive");
    assert!(
        released["original_received_monotonic_ns"].as_u64().unwrap()
            < released["recovery_completed_received_monotonic_ns"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(released["pending"]["kind"], "opcua_publish");
    assert_eq!(released["pending"]["delivery"], "publish");
    assert_eq!(released["pending"]["sequence_number"], 2);
    assert_eq!(namespace_marker(&released["pending"]), "urn:middle");
    assert_eq!(namespace_marker(&recorded[later]["value"]), "urn:after");
    assert_eq!(
        recorded[repair]["elapsed_ns"],
        recorded[release]["elapsed_ns"]
    );
    let mut previous = 0;
    for row in recorded {
        let elapsed = row["elapsed_ns"].as_u64().unwrap();
        assert!(elapsed >= previous);
        previous = elapsed;
    }
    exports(&home, &all);
}
#[test]
fn export_accepts_recovery_frontier_but_rejects_restamped_raw_backwards_sequence() {
    let home = Home::new("opcua-recovery-export-boundary");
    let header = json!({"kind":"header","version":1,"clock":"host_monotonic_receive",
        "run_id":"00000000000000000000000000000000","started_at":"2026-01-01T00:00:00Z",
        "sources":[{"id":"native","protocol":"opc_ua"}]});
    let pending = json!({"kind":"opcua_publish","message_type":"notification","delivery":"publish","sequence_number":2,"notifications":[]});
    let release = json!({"kind":"opcua_recovery_release","clock_basis":"recovery_completed_receive",
        "original_received_monotonic_ns":100,"recovery_completed_received_monotonic_ns":110,"pending":pending});
    for backwards in [false, true] {
        let mut all = vec![header.clone()];
        for (index, elapsed) in [110, if backwards { 100 } else { 110 }, 120]
            .into_iter()
            .enumerate()
        {
            let value = if index == 1 {
                if backwards {
                    pending.clone()
                } else {
                    release.clone()
                }
            } else {
                json!({"kind":"opcua_publish","delivery":if index==0 {"republish"} else {"publish"},"sequence_number":if index==0 {1} else {3}})
            };
            all.push(
                json!({"kind":"sample","seq":index+1,"source":"native","signal":"changes",
                "time":format!("2026-01-01T00:00:00.{elapsed:09}Z"),"elapsed_ns":elapsed,
                "read_duration_ns":0,"delivery_latency_ns":0,"value":value}),
            );
        }
        all.push(json!({"kind":"end","samples":3,"status":"complete","elapsed_ns":120}));
        let mut text = all
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        fs::write(home.path("run.jsonl"), text).unwrap();
        if backwards {
            let error = home.fails(1, &["data", "export", "run.jsonl", "-o", "backwards.csv"]);
            assert_eq!(error["code"], "incomplete");
            assert!(!home.path("backwards.csv").exists());
        } else {
            exports(&home, &all);
        }
    }
}
