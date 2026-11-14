//! Vendor SDK bridges can identify themselves without impersonating a built-in protocol.
#![cfg(unix)]
mod support;

use serde_json::{Value, json};
use std::fs;
use support::cli::Home;

fn config(home: &Home, program: &str) {
    fs::write(home.path("sources.json"), json!({"version":1,"sources":[
        {"id":"vendor", "protocol":"helper", "executable":"/bin/sh", "args":["-c",program], "timeout_ms":100},
        {"id":"cycles", "protocol":"simulator", "signal":"cycle", "every_ms":10}
    ]}).to_string()).unwrap();
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn assert_reaped(home: &Home, path: &str) {
    let pid = fs::read_to_string(home.path(path))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !support::cli::process_lives(pid),
        "helper process {pid} survived capture"
    );
}

#[test]
fn generic_helper_preserves_values_timestamps_and_cleans_its_process_group() {
    let home = Home::new("generic-vendor-helper");
    config(
        &home,
        "echo $$ > helper.pid; sleep 30 & echo $! > child.pid; printf '%s\\n' '{\"signal\":\"counter\",\"value\":18446744073709551617,\"source_time\":\"2026-11-10T14:00:00.123456789+01:00\"}' '{\"signal\":\"sample\",\"value\":{\"value\":12.5,\"unit\":\"C\"}}'; wait",
    );
    let result = home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "800ms",
        "-o",
        "run.jsonl",
    ]);
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    assert_eq!(rows[0]["sources"][0]["protocol"], "helper");
    let readings: Vec<_> = rows
        .iter()
        .filter(|row| row["source"] == "vendor")
        .collect();
    assert_eq!(readings.len(), 2);
    assert_eq!(readings[0]["value"].to_string(), "18446744073709551617");
    assert_eq!(readings[0]["source_time"], "2026-11-10T13:00:00.123456789Z");
    assert_eq!(readings[1]["value"], json!({"value":12.5,"unit":"C"}));
    for format in ["csv", "jsonl"] {
        let path = format!("vendor.{format}");
        let exported = home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--source",
            "vendor",
            "--format",
            format,
            "-o",
            &path,
        ]);
        assert_eq!(exported["samples"], 2);
        assert!(
            fs::read_to_string(home.path(&path))
                .unwrap()
                .contains("18446744073709551617")
        );
    }
    assert_reaped(&home, "helper.pid");
    assert_reaped(&home, "child.pid");
}

#[test]
fn later_invalid_helper_output_preserves_complete_samples_for_recovery() {
    let home = Home::new("generic-helper-recovery");
    config(
        &home,
        "echo $$ > helper.pid; printf '%s\\n' '{\"signal\":\"good\",\"value\":42}' 'private-vendor-secret'; sleep 30",
    );
    let run = home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "800ms",
        "-o",
        "run.jsonl",
    ]);
    let error = run.failure(1);
    assert_eq!(error["details"]["status"], "failed");
    assert!(!run.stdout.contains("private-vendor-secret"));
    assert!(!run.stderr.contains("private-vendor-secret"));
    let rows = rows(&home);
    assert_eq!(
        rows.iter().filter(|row| row["source"] == "vendor").count(),
        1
    );
    assert_eq!(rows.last().unwrap()["status"], "failed");
    let exported = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "--allow-incomplete",
        "--source",
        "vendor",
        "-o",
        "recovered.csv",
    ]);
    assert_eq!(exported["samples"], 1);
    assert_eq!(exported["complete"], false);
    assert_reaped(&home, "helper.pid");
}
