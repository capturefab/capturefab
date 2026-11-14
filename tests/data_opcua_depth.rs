//! Real receive-path checks for bounded SDK decoding; no crash-sized stimuli.
#[path = "support/opcua.rs"]
#[allow(
    dead_code,
    reason = "shared server fixture exposes controls used by other integration targets"
)]
mod fixture;
mod support;

use opcua::types::{DataValue, DiagnosticInfo, StatusCode, Variant};
use serde_json::{Value, json};
use std::{
    fs,
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};

struct Recording(Option<Child>);
impl Recording {
    fn finish(mut self) -> Run {
        let mut child = self.0.take().unwrap();
        let until = Instant::now() + Duration::from_secs(12);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bounded decoder regression child did not exit");
            }
            thread::sleep(Duration::from_millis(10));
        }
        Run::of_output(child.wait_with_output().unwrap(), &["data", "record"])
    }
}
impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
fn seed(row: &Value, mode: &str) -> bool {
    if row["kind"] != "sample" {
        return false;
    }
    if mode == "read" {
        return row["value"] == 17;
    }
    row["value"]["notifications"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|block| block["items"].as_array().into_iter().flatten())
        .any(|item| item["data_value"]["value"]["value"] == 17)
}
fn nested(diagnostic: bool) -> DataValue {
    if diagnostic {
        let mut value = DiagnosticInfo::default();
        for _ in 0..12 {
            value = DiagnosticInfo {
                inner_diagnostic_info: Some(Box::new(value)),
                ..DiagnosticInfo::default()
            };
        }
        DataValue::from(Variant::DiagnosticInfo(Box::new(value)))
    } else {
        let mut value = DataValue::default();
        for _ in 0..12 {
            value = DataValue {
                value: Some(Variant::DataValue(Box::new(value))),
                ..DataValue::default()
            };
        }
        value
    }
}
#[test]
fn read_and_subscription_retire_on_recursive_wire_values_and_keep_the_saved_prefix() {
    for mode in ["read", "subscription"] {
        for diagnostic in [false, true] {
            let home = Home::new(&format!("opcua-wire-depth-{mode}-{diagnostic}"));
            let server = fixture::Server::start_namespace_reply(
                &home.root,
                Vec::new(),
                DataValue::from(17_i32),
                Duration::ZERO,
            );
            let mut source = json!({"id":"native","protocol":"opc_ua","endpoint":server.endpoint,
                "mode":mode,"sampling_ms":20,"request_timeout_ms":1200,"timeout_ms":2000,
                "nodes":[{"name":"value","node_id":"i=2255"}]});
            if mode == "subscription" {
                source["subscription"] = json!({"publishing_ms":100});
            }
            fs::write(
                home.path("sources.json"),
                json!({"version":1,"sources":[source]}).to_string(),
            )
            .unwrap();
            let child = home
                .command()
                .args([
                    "--json",
                    "data",
                    "record",
                    "--config",
                    "sources.json",
                    "--for",
                    "5s",
                    "-o",
                    "run.jsonl",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let child = Recording(Some(child));
            let until = Instant::now() + Duration::from_secs(3);
            while !rows(&home).iter().any(|row| seed(row, mode)) {
                assert!(Instant::now() < until, "no initial native value for {mode}");
                thread::sleep(Duration::from_millis(10));
            }
            // Thirteen small built-in levels exceed the SDK's ten active guards.
            // This deliberately stays far below any stack-overflow stimulus.
            server.set_namespace_reply(nested(diagnostic));
            let run = child.finish();
            let error = run.failure(3);
            if let Some(status) = error["details"]["opcua_status"].as_u64() {
                assert!(
                    [
                        StatusCode::BadDecodingError,
                        StatusCode::BadCommunicationError,
                        StatusCode::BadConnectionClosed
                    ]
                    .into_iter()
                    .any(|value| u64::from(value.bits()) == status),
                    "expected native receive-path failure: {error}"
                );
            } else {
                // With reconnects disabled the SDK may discard the transport
                // failure code and return Good as the event-loop completion.
                // This is not an OPC-UA Good error response.
                assert_eq!(error["details"]["sdk_status_raw"], 0);
                assert_eq!(error["details"]["sdk_error_status_present"], false);
            }
            let recorded = rows(&home);
            assert!(recorded.iter().any(|row| seed(row, mode)));
            assert_eq!(recorded.last().unwrap()["status"], "failed");
            assert!(
                recorded
                    .iter()
                    .filter(|row| row["kind"] == "sample")
                    .all(|row| {
                        seed(row, mode)
                            || (mode == "subscription"
                                && row["value"]["message_type"] == "keepalive")
                    })
            );
            let exported = home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "--allow-incomplete",
                "--format",
                "jsonl",
                "-o",
                "prefix.jsonl",
            ]);
            assert_eq!(exported["complete"], false);
            assert_eq!(exported["samples"], error["details"]["samples"]);
        }
    }
}
