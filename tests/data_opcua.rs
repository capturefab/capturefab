//! OPC-UA sessions and reads through real TCP, without a Python executable.
#[path = "support/opcua.rs"]
mod fixture;
mod support;

use opcua::types::{
    ByteString, DataValue, DateTime, StatusCode, UAString, Variant, VariantScalarTypeId,
};
use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};
use support::time;

const SOURCE_TIME: &str = "2026-11-10T12:34:56.123456700Z";

fn value(value: impl Into<Variant>) -> DataValue {
    DataValue::new_at(value, DateTime::parse_from_rfc3339(SOURCE_TIME).unwrap())
}

fn source(server: &fixture::Server, names: &[&str]) -> Value {
    json!({
        "id": "plc",
        "protocol": "opc_ua",
        "endpoint": server.endpoint,
        "sampling_ms": 20,
        "request_timeout_ms": 3000,
        "nodes": names.iter().map(|name| json!({
            "name": name, "node_id": server.node(name)
        })).collect::<Vec<_>>()
    })
}

fn configure(home: &Home, source: &Value) {
    fs::write(
        home.path("sources.json"),
        json!({"version": 1, "sources": [source]}).to_string(),
    )
    .unwrap();
}

fn record(home: &Home, password: Option<&str>) -> Run {
    record_for(home, password, &time::arg(600))
}

fn record_for(home: &Home, password: Option<&str>, duration: &str) -> Run {
    let empty_path = home.path("empty-path");
    fs::create_dir_all(&empty_path).unwrap();
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        duration,
        "-o",
        "run.jsonl",
    ];
    let mut command = home.command();
    // Nothing can be found by executable name: the fixture and client must
    // establish, authenticate and read the session entirely within Rust.
    command.env("PATH", empty_path);
    if let Some(password) = password {
        command
            .env("CAPTUREFAB_OPCUA_TEST_USER", fixture::USER)
            .env("CAPTUREFAB_OPCUA_TEST_PASSWORD", password);
    }
    Run::of_output(command.args(args).output().unwrap(), &args)
}

fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_failed_without_samples(home: &Home, run: &Run) -> Value {
    assert_ne!(run.code, Some(0), "{}\n{}", run.stdout, run.stderr);
    let error = run.json()["error"].clone();
    assert!(error["code"].is_string(), "{error}");
    assert!(
        rows(home).iter().all(|row| row["kind"] != "sample"),
        "a failed poll must publish none of its nodes"
    );
    error
}

#[test]
fn reads_typed_batches_and_source_timestamps_without_python_on_path() {
    let home = Home::new("opcua-native-read");
    let server = fixture::Server::start(
        &home.root,
        vec![
            ("pressure", value(12.5_f64)),
            ("ready", value(true)),
            ("signed", value(i64::MIN)),
            ("unsigned", value(u64::MAX)),
            ("text", value(UAString::from("line 1\nline 2 ☃"))),
            ("array", value(vec![-1_i32, 0, i32::MAX])),
            ("bytes", value(ByteString::from(vec![0, 255, 10]))),
            ("null", value(Variant::Empty)),
            ("empty_array", value(Vec::<i32>::new())),
            (
                "matrix",
                value((
                    VariantScalarTypeId::Double,
                    vec![1.0_f64, 2.0, 3.0, 4.0],
                    vec![2, 2],
                )),
            ),
        ],
    );
    let expected = [
        ("pressure", json!(12.5)),
        ("ready", json!(true)),
        ("signed", json!(i64::MIN)),
        ("unsigned", json!(u64::MAX)),
        ("text", json!("line 1\nline 2 ☃")),
        ("array", json!([-1, 0, i32::MAX])),
        ("bytes", json!({"encoding":"base64", "data":"AP8K"})),
        ("null", Value::Null),
        ("empty_array", json!([])),
        (
            "matrix",
            json!({"type":"Matrix", "element_type":"Double", "dimensions":[2,2],
                   "values":[1.0,2.0,3.0,4.0]}),
        ),
    ];
    let names = expected.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    let mut config = source(&server, &names);
    // A poll still running at the cutoff fails the recording by design. Poll
    // at 0, 250 and 500 (scaled) of the 600 window, so the last read has a
    // fifth of the window to finish however slow the runner is.
    config["sampling_ms"] = json!(time::ms(250));
    configure(&home, &config);
    let result = record(&home, None).success();
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    let samples = rows
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert!(samples.len() >= expected.len(), "{result}");
    assert_eq!(samples.len() % expected.len(), 0);
    for batch in samples.chunks_exact(expected.len()) {
        let elapsed = &batch[0]["elapsed_ns"];
        for (sample, (signal, value)) in batch.iter().zip(&expected) {
            assert_eq!(sample["source"], "plc");
            assert_eq!(sample["signal"], *signal);
            assert_eq!(&sample["value"], value);
            assert_eq!(&sample["elapsed_ns"], elapsed);
            assert_eq!(
                chrono::DateTime::parse_from_rfc3339(sample["source_time"].as_str().unwrap())
                    .unwrap(),
                chrono::DateTime::parse_from_rfc3339(SOURCE_TIME).unwrap()
            );
        }
    }
    assert_eq!(
        samples.len(),
        usize::try_from(result["samples"].as_u64().unwrap()).unwrap()
    );
    assert!(server.reads.load(Ordering::Relaxed) >= u64::try_from(samples.len()).unwrap());
}

#[test]
fn missing_node_or_bad_quality_rejects_the_whole_poll() {
    for bad_node in ["missing", "bad_quality"] {
        let home = Home::new("opcua-native-status");
        let server = fixture::Server::start(
            &home.root,
            vec![
                ("pressure", value(12.5_f64)),
                (
                    "bad_quality",
                    DataValue::new_at_status(
                        42_i32,
                        DateTime::parse_from_rfc3339(SOURCE_TIME).unwrap(),
                        StatusCode::BadSensorFailure,
                    ),
                ),
            ],
        );
        configure(&home, &source(&server, &["pressure", bad_node]));
        let error = assert_failed_without_samples(&home, &record(&home, None));
        assert_eq!(error["code"], "unavailable", "{error}");
        assert!(server.reads.load(Ordering::Relaxed) > 0);
    }
}

#[test]
fn signed_encrypted_username_session_requires_the_pinned_certificate() {
    let home = Home::new("opcua-native-security");
    let server = fixture::Server::start(&home.root, vec![("pressure", value(12.5_f64))]);
    let (certificate, private_key) = fixture::client_keypair(&home.root.join("client-pki"));
    let mut config = source(&server, &["pressure"]);
    // One authenticated Read per policy verifies the session and value. Its
    // next poll is due after the recording cutoff, keeping this security test
    // independent of final-poll scheduling while preserving every RPC deadline.
    config["sampling_ms"] = json!(time::ms(3000));
    config["username_env"] = json!("CAPTUREFAB_OPCUA_TEST_USER");
    config["password_env"] = json!("CAPTUREFAB_OPCUA_TEST_PASSWORD");
    config["security"] = json!({
        "policy": "basic256_sha256",
        "certificate": certificate,
        "private_key": private_key,
        "server_certificate": server.certificate,
    });
    for policy in [
        "basic256_sha256",
        "aes128_sha256_rsa_oaep",
        "aes256_sha256_rsa_pss",
    ] {
        if home.path("run.jsonl").exists() {
            fs::remove_file(home.path("run.jsonl")).unwrap();
        }
        config["security"]["policy"] = json!(policy);
        configure(&home, &config);
        let result = record(&home, Some(fixture::PASSWORD)).success();
        assert_eq!(result["samples"], 1);
        assert!(rows(&home).iter().any(|row| row["value"] == 12.5));
    }

    fs::remove_file(home.path("run.jsonl")).unwrap();
    let rejected = record(&home, Some("wrong-fixture-password-private"));
    let error = assert_failed_without_samples(&home, &rejected);
    assert_eq!(error["code"], "auth_rejected", "{error}");
    for secret in [
        fixture::PASSWORD,
        "wrong-fixture-password-private",
        fixture::USER,
    ] {
        assert!(!rejected.stdout.contains(secret));
        assert!(!rejected.stderr.contains(secret));
    }

    if home.path("run.jsonl").exists() {
        fs::remove_file(home.path("run.jsonl")).unwrap();
    }
    // A second valid certificate is still the wrong certificate for this server.
    config["security"]["server_certificate"] = json!(certificate);
    configure(&home, &config);
    let rejected = record(&home, Some(fixture::PASSWORD));
    let error = assert_failed_without_samples(&home, &rejected);
    assert_eq!(error["code"], "unavailable", "{error}");
}

#[test]
fn oversized_value_fails_without_publishing_a_partial_batch() {
    let home = Home::new("opcua-native-limit");
    let server = fixture::Server::start(
        &home.root,
        vec![
            ("pressure", value(12.5_f64)),
            ("large", value(UAString::from("x".repeat(1_048_577)))),
        ],
    );
    configure(&home, &source(&server, &["pressure", "large"]));
    let error = assert_failed_without_samples(&home, &record(&home, None));
    assert!(
        matches!(error["code"].as_str(), Some("incomplete" | "unavailable")),
        "{error}"
    );
}

#[test]
fn stalled_endpoint_respects_request_timeout_and_stops_before_recording_deadline() {
    let home = Home::new("opcua-native-connect-timeout");
    // The kernel accepts TCP, but this listener never sends an OPC-UA reply.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("opc.tcp://{}/stalled", listener.local_addr().unwrap());
    configure(
        &home,
        &json!({
            "id":"plc", "protocol":"opc_ua", "endpoint":endpoint,
            "request_timeout_ms":50,
            "nodes":[{"name":"pressure", "node_id":"ns=2;s=pressure"}]
        }),
    );
    let started = Instant::now();
    let run = record_for(&home, None, "5s");
    assert!(started.elapsed() < Duration::from_secs(2));
    let error = assert_failed_without_samples(&home, &run);
    assert_eq!(error["code"], "timeout", "{error}");
}

#[test]
fn sampling_wait_at_acquisition_end_finishes_without_an_extra_poll() {
    let home = Home::new("opcua-native-sampling-end");
    let server = fixture::Server::start(&home.root, vec![("pressure", value(12.5_f64))]);
    let mut config = source(&server, &["pressure"]);
    config["sampling_ms"] = json!(time::ms(3000));
    configure(&home, &config);
    let started = Instant::now();
    let result = record(&home, None).success();
    assert!(started.elapsed() < time::secs(2));
    assert_eq!(result["status"], "complete");
    assert_eq!(result["samples"], 1);
    assert_eq!(server.reads.load(Ordering::Relaxed), 1);
}

fn uri_source(server: &fixture::Server, names: &[&str]) -> Value {
    let mut configured = source(server, names);
    for (node, name) in configured["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(names)
    {
        node["node_id"] = json!(format!("s={name}"));
        node["namespace_uri"] = json!(fixture::NAMESPACE);
    }
    // Only one application poll, then the existing passive sampling wait.
    configured["sampling_ms"] = json!(5000);
    configured
}
fn namespace_table(server: &fixture::Server) -> DataValue {
    assert!(server.namespace > 1);
    DataValue::value_only(
        (0..=server.namespace)
            .map(|index| {
                UAString::from(match index {
                    0 => "http://opcfoundation.org/UA/".to_owned(),
                    1 => "urn:capturefab:opcua-fixture".to_owned(),
                    index if index == server.namespace => fixture::NAMESPACE.to_owned(),
                    index => format!("urn:capturefab:other:{index}"),
                })
            })
            .collect::<Vec<_>>(),
    )
}

#[test]
fn namespace_uris_resolve_again_for_two_explicit_sessions_after_reordering() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let first = Home::new("opcua-uri-first");
    let server = fixture::Server::start_on(
        &first.root,
        vec![("pressure", value(12.5_f64)), ("unsigned", value(u64::MAX))],
        listener.try_clone().unwrap(),
        false,
    );
    let original_index = server.namespace;
    let mut configured = uri_source(&server, &["pressure", "unsigned"]);
    configured["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"state","node_id":"i=2259"}));
    configure(&first, &configured);
    assert_eq!(record_for(&first, None, "1s").success()["samples"], 3);
    let expected = [
        ("pressure", json!(12.5)),
        ("unsigned", json!(u64::MAX)),
        ("state", json!(0)),
    ];
    let assert_values = |home: &Home| {
        let rows = rows(home);
        let samples = rows
            .iter()
            .filter(|row| row["kind"] == "sample")
            .collect::<Vec<_>>();
        assert_eq!(samples.len(), 3);
        for (signal, expected) in &expected {
            let sample = samples.iter().find(|row| row["signal"] == *signal).unwrap();
            assert_eq!(sample["value"], *expected);
            if *signal != "state" {
                assert_eq!(
                    chrono::DateTime::parse_from_rfc3339(sample["source_time"].as_str().unwrap())
                        .unwrap(),
                    chrono::DateTime::parse_from_rfc3339(SOURCE_TIME).unwrap()
                );
            }
        }
    };
    assert_values(&first);
    drop(server);
    let second = Home::new("opcua-uri-second");
    let server = fixture::Server::start_on(
        &second.root,
        vec![("pressure", value(12.5_f64)), ("unsigned", value(u64::MAX))],
        listener.try_clone().unwrap(),
        true,
    );
    assert_ne!(server.namespace, original_index);
    assert_eq!(configured["endpoint"], server.endpoint);
    // Reuse the exact same URI/identifier configuration and endpoint.
    configure(&second, &configured);
    assert_eq!(record_for(&second, None, "1s").success()["samples"], 3);
    assert_values(&second);
    assert!(server.reads.load(Ordering::Relaxed) > 0);
}

#[test]
fn namespace_table_is_read_once_before_all_uri_values() {
    let home = Home::new("opcua-uri-once");
    let server = fixture::Server::start_namespace_reply(
        &home.root,
        vec![("pressure", value(12.5_f64)), ("unsigned", value(u64::MAX))],
        DataValue::null(),
        Duration::ZERO,
    );
    server.set_namespace_reply(namespace_table(&server));
    configure(&home, &uri_source(&server, &["pressure", "unsigned"]));
    assert_eq!(record_for(&home, None, "1s").success()["samples"], 2);
    assert_eq!(server.namespace_reads.load(Ordering::Relaxed), 1);
    assert_eq!(server.reads.load(Ordering::Relaxed), 2);
}

#[test]
fn invalid_namespace_tables_publish_no_legacy_or_uri_sibling() {
    const STANDARD: &str = "http://opcfoundation.org/UA/";
    let table = |uris: &[&str]| {
        DataValue::value_only(
            uris.iter()
                .map(|uri| UAString::from(*uri))
                .collect::<Vec<_>>(),
        )
    };
    let bad_status = DataValue::from((
        table(&[STANDARD, "urn:server", fixture::NAMESPACE])
            .value
            .unwrap(),
        StatusCode::BadUserAccessDenied,
    ));
    let oversized = format!("urn:{}", "x".repeat(4096));
    let budget = DataValue::value_only(
        std::iter::once(UAString::from(STANDARD))
            .chain(
                (1..270).map(|index| UAString::from(format!("urn:{index}:{}", "x".repeat(4030)))),
            )
            .collect::<Vec<_>>(),
    );
    for (label, reply, code) in [
        (
            "missing",
            table(&[STANDARD, "urn:server", "urn:other"]),
            "unavailable",
        ),
        (
            "duplicate",
            table(&[STANDARD, fixture::NAMESPACE, fixture::NAMESPACE]),
            "incomplete",
        ),
        ("bad-status", bad_status, "unavailable"),
        ("wrong-type", DataValue::value_only(42_u32), "incomplete"),
        (
            "wrong-array",
            DataValue::value_only(vec![1_u32, 2]),
            "incomplete",
        ),
        ("no-value", DataValue::null(), "incomplete"),
        ("empty", table(&[]), "incomplete"),
        ("empty-uri", table(&[STANDARD, ""]), "incomplete"),
        (
            "wrong-zero",
            table(&["urn:wrong", fixture::NAMESPACE]),
            "incomplete",
        ),
        (
            "oversized-uri",
            table(&[STANDARD, &oversized]),
            "incomplete",
        ),
        ("budget", budget, "incomplete"),
    ] {
        let home = Home::new(&format!("opcua-uri-{label}"));
        let server = fixture::Server::start_namespace_reply(
            &home.root,
            vec![("pressure", value(12.5_f64))],
            reply,
            Duration::ZERO,
        );
        let mut configured = uri_source(&server, &["pressure"]);
        configured["nodes"].as_array_mut().unwrap().insert(
            0,
            json!({"name":"legacy","node_id":server.node("pressure")}),
        );
        configure(&home, &configured);
        let run = record_for(&home, None, "2s");
        let error = assert_failed_without_samples(&home, &run);
        assert_eq!(error["code"], code, "{label}: {error}");
        assert_eq!(server.namespace_reads.load(Ordering::Relaxed), 1, "{label}");
        assert_eq!(server.reads.load(Ordering::Relaxed), 0, "{label}");
    }
}

#[test]
fn namespace_resolution_shares_request_and_original_acquisition_deadlines() {
    for (label, request, duration, delay) in [
        ("request", 1000_u64, "4s", 2000_u64),
        ("acquisition", 5000, "2s", 3000),
    ] {
        let home = Home::new(&format!("opcua-uri-{label}-deadline"));
        let server = fixture::Server::start_namespace_reply(
            &home.root,
            vec![("pressure", value(12.5_f64))],
            DataValue::null(),
            Duration::from_millis(delay),
        );
        server.set_namespace_reply(namespace_table(&server));
        let mut configured = uri_source(&server, &["pressure"]);
        configured["request_timeout_ms"] = json!(request);
        configured["timeout_ms"] = json!(5000);
        configure(&home, &configured);
        let error = assert_failed_without_samples(&home, &record_for(&home, None, duration));
        assert_eq!(error["code"], "timeout", "{label}: {error}");
        assert_eq!(server.namespace_reads.load(Ordering::Relaxed), 1);
        assert_eq!(server.reads.load(Ordering::Relaxed), 0);
    }
}
