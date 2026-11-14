//! Direct Windows helpers; these cases require a real Windows runner.
#![cfg(windows)]
mod support;

use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError},
    System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
};

fn executable(home: &Home) -> std::path::PathBuf {
    let dir = home.path("direct helper ☃ folder");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("owned helper.exe");
    fs::copy(env!("CARGO_BIN_EXE_capturefab-helper-fixture"), &path).unwrap();
    path
}
fn configure(home: &Home, executable: &Path, args: Vec<Value>) {
    let mut source = json!({"id":"vendor","protocol":"helper","executable":executable,
        "timeout_ms":1000});
    source["args"] = Value::Array(args);
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
}
fn arguments(home: &Home, mode: &str) -> Vec<Value> {
    vec![json!(mode), json!(home.work)]
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn samples(rows: &[Value]) -> Vec<&Value> {
    rows.iter().filter(|row| row["kind"] == "sample").collect()
}
fn process_running(pid: u32) -> bool {
    assert!(pid > 0);
    // SAFETY: Query only the exact PID emitted by the owned test child.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // SAFETY: read the error from the immediately preceding OpenProcess.
        let error = unsafe { GetLastError() };
        // Match the Microsoft .NET local nonzero-PID check; access denial and
        // all other query failures must fail, never masquerade as cleanup.
        assert_eq!(
            error, ERROR_INVALID_PARAMETER,
            "owned PID {pid} query failed: {error}"
        );
        return false;
    }
    let mut code = 0;
    // SAFETY: handle is live and code is a writable u32.
    let ok = unsafe { GetExitCodeProcess(handle, &raw mut code) };
    // SAFETY: close the unique query handle opened above.
    assert_ne!(unsafe { CloseHandle(handle) }, 0);
    assert_ne!(ok, 0);
    code == 259
}
fn assert_reaped(home: &Home, name: &str) {
    let pid: u32 = fs::read_to_string(home.path(name))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    while process_running(pid) && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_running(pid),
        "owned helper PID {pid} survived its source"
    );
}
fn exports(home: &Home, expected: &[&Value], incomplete: bool) {
    for format in ["jsonl", "csv"] {
        let output = format!("samples.{format}");
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            &output,
        ];
        if incomplete {
            args.push("--allow-incomplete");
        }
        let result = home.ok(&args);
        assert_eq!(result["samples"], expected.len());
        if format == "jsonl" {
            let actual: Vec<Value> = fs::read_to_string(home.path(&output))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            for (actual, expected) in actual.iter().zip(expected) {
                let mut expected = (*expected).clone();
                expected.as_object_mut().unwrap().remove("kind");
                assert_eq!(actual, &expected);
            }
            assert_eq!(actual.len(), expected.len());
        } else {
            let text = fs::read_to_string(home.path(&output)).unwrap();
            assert!(text.starts_with("seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n"));
            for sample in expected {
                assert!(text.contains(sample["signal"].as_str().unwrap()));
            }
        }
    }
}
#[test]
fn direct_unicode_executable_and_crt_arguments_preserve_readings() {
    let home = Home::new("windows-helper-arguments");
    let executable = executable(&home);
    let expected = [
        "",
        "two words",
        "quote\"middle",
        "trailing\\",
        "\\\"quoted\\",
        "☃😀",
    ];
    let mut args = arguments(&home, "args");
    args.extend(expected.iter().map(|arg| json!(arg)));
    configure(&home, &executable, args);
    let result = home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "600ms",
        "-o",
        "run.jsonl",
    ]);
    assert_eq!(result["status"], "complete");
    let all = rows(&home);
    let recorded = samples(&all);
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0]["value"], json!(expected));
    assert_eq!(recorded[1]["value"], u64::MAX);
    assert_eq!(recorded[1]["source_time"], "2026-11-10T13:00:00.123456789Z");
    for (index, row) in recorded.iter().enumerate() {
        assert_eq!(row["seq"], index + 1);
        assert_eq!(row["source"], "vendor");
        assert!(row["read_duration_ns"].as_u64().unwrap() <= row["elapsed_ns"].as_u64().unwrap());
    }
    assert!(
        !fs::read_to_string(home.path("run.jsonl"))
            .unwrap()
            .contains("helper-private-stderr")
    );
    exports(&home, &recorded, false);
    assert_eq!(fs::read(home.path("stdin-eof")).unwrap(), b"eof");
    assert_reaped(&home, "helper.pid");
}
#[test]
fn finishing_source_reaps_exact_owned_helper_and_descendant() {
    let home = Home::new("windows-helper-tree");
    let executable = executable(&home);
    configure(&home, &executable, arguments(&home, "tree"));
    home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "600ms",
        "-o",
        "run.jsonl",
    ]);
    assert_eq!(samples(&rows(&home)).len(), 1);
    assert_eq!(fs::read(home.path("stdin-eof")).unwrap(), b"eof");
    assert_reaped(&home, "helper.pid");
    assert_reaped(&home, "descendant.pid");
}
#[test]
fn malformed_line_and_partial_eof_preserve_only_complete_prefix() {
    for mode in ["invalid", "partial"] {
        let home = Home::new("windows-helper-prefix");
        let executable = executable(&home);
        configure(&home, &executable, arguments(&home, mode));
        let run = home.run(&[
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "900ms",
            "-o",
            "run.jsonl",
        ]);
        let (exit, code) = if mode == "invalid" {
            (1, "failed")
        } else {
            (3, "disconnected")
        };
        let error = run.failure(exit);
        assert_eq!(error["code"], code);
        assert!(!run.stdout.contains("private-invalid-helper-line"));
        assert!(!run.stderr.contains("private-invalid-helper-line"));
        let all = rows(&home);
        let recorded = samples(&all);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0]["signal"], "prefix");
        assert_eq!(all.last().unwrap()["status"], "failed");
        exports(&home, &recorded, true);
        assert_reaped(&home, "helper.pid");
    }
}
#[test]
fn invalid_executable_and_argument_shapes_fail_before_spawn_or_output() {
    let home = Home::new("windows-helper-admission");
    let executable = executable(&home);
    let mut cases = vec![
        json!({"executable":"helper.exe"}),
        json!({"executable":"relative\\helper.exe"}),
        json!({"executable":"C:\\explicit\\helper.cmd"}),
        json!({"executable":"C:\\explicit\\helper.ps1"}),
        json!({"executable":executable,"args":["NUL\u{0}argument"]}),
        json!({"executable":executable,"args":["x".repeat(32768)]}),
    ];
    cases.push(json!({"executable":executable,"args":vec!["x";1025]}));
    for case in cases {
        let mut source = json!({"id":"vendor","protocol":"helper"});
        for (key, value) in case.as_object().unwrap() {
            source[key].clone_from(value);
        }
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[source]}).to_string(),
        )
        .unwrap();
        let error = home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "10ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("helper.pid").exists());
    }
}
