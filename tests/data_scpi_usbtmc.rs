//! Real SCPI/worker acquisition against independent manual USB operations.
mod support;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::fs;
use support::cli::Home;

const WIRE: &str = include_str!("fixtures/usbtmc/wire.json");
const SELECTOR: &str = include_str!("fixtures/usbtmc/selector-control.json");
const IPC: &str = include_str!("fixtures/usbtmc/ipc.json");

fn unhex(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn hash(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::new();
    for byte in Sha256::digest(bytes) {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    text
}
fn source(signals: Value) -> Value {
    let selector: Value = serde_json::from_str(SELECTOR).unwrap();
    let mut connection = selector["selector"].clone();
    connection["transport"] = json!("usbtmc");
    let mut source = json!({"id":"instrument","protocol":"scpi","connection":connection,
        "every_ms":3_600_000,"timeout_ms":1000});
    source["signals"] = signals;
    source
}
fn line_signal() -> Value {
    json!([{"name":"number","query":"READ?","format":"number"}])
}
#[cfg(feature = "usb")]
fn write_config(home: &Home, source: Value) {
    let mut config = json!({"version":1});
    config["sources"] = Value::Array(vec![source]);
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
}
fn preflight_failure(home: &Home, exit: i32) -> Value {
    let trace = home.path("usb-trace.jsonl").to_string_lossy().into_owned();
    let run = home.run_env(
        &[
            (
                "CAPTUREFAB_EXECUTABLE",
                env!("CARGO_BIN_EXE_capturefab-helper-fixture"),
            ),
            ("CAPTUREFAB_USBTMC_CASE", "forbidden_preflight"),
            ("CAPTUREFAB_USBTMC_TRACE", &trace),
        ],
        &[
            "--json",
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
    let error = run.failure(exit);
    assert!(!home.path("usb-trace.jsonl").exists());
    error
}

#[test]
fn exact_selector_failures_precede_worker_source_and_recording_effects() {
    let held: Value = serde_json::from_str(SELECTOR).unwrap();
    let home = Home::new("usbtmc-preflight");
    for serial in held["invalid_serials"].as_array().unwrap() {
        let mut config = source(line_signal());
        config["connection"]["serial_number"] = serial.clone();
        fs::write(home.path("sources.json"), json!({"version":1,"sources":[
            {"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"},config
        ]}).to_string()).unwrap();
        assert_eq!(preflight_failure(&home, 2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
    for selector in held["invalid_selector_json"].as_array().unwrap() {
        let fields = selector
            .as_str()
            .unwrap()
            .strip_prefix('{')
            .unwrap()
            .strip_suffix('}')
            .unwrap();
        let config = format!(
            "{{\"version\":1,\"sources\":[{{\"id\":\"instrument\",\"protocol\":\"scpi\",\"connection\":{{\"transport\":\"usbtmc\",{fields}}},\"signals\":[{{\"name\":\"number\",\"query\":\"READ?\",\"format\":\"number\"}}]}}]}}"
        );
        fs::write(home.path("sources.json"), config).unwrap();
        assert_eq!(preflight_failure(&home, 2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn frozen_wire_frame_controls_and_feature_preflight_remain_exact() {
    assert_eq!(
        hash(WIRE.as_bytes()),
        "cde5c5b5078f29c2669077833a81f7bd7497781905fb770cadbf97c160c4eba3"
    );
    assert_eq!(
        hash(SELECTOR.as_bytes()),
        "0a1ec921926d3e91e29e6318afc54c83cc736784bdfa2e65667c87e562eb1540"
    );
    assert_eq!(
        hash(IPC.as_bytes()),
        "5d5db67140ac12ed6a67fe4a7123486beb34ebd4472fe1e68911c6ef2e79db6b"
    );
    let wire: Value = serde_json::from_str(WIRE).unwrap();
    assert_eq!(
        unhex(
            wire["application_controls"]["definite_block_reply_hex"]
                .as_str()
                .unwrap()
        )
        .len(),
        18
    );
    let ipc: Value = serde_json::from_str(IPC).unwrap();
    for case in ipc["frames"].as_array().unwrap() {
        let body = case["body_utf8"].as_str().unwrap().as_bytes();
        let mut framed = unhex(case["length_prefix_hex"].as_str().unwrap());
        assert_eq!(
            usize::try_from(u32::from_le_bytes(framed.as_slice().try_into().unwrap())).unwrap(),
            body.len()
        );
        framed.extend_from_slice(body);
        assert_eq!(hash(&framed), case["frame_sha256"]);
    }
    #[cfg(not(feature = "usb"))]
    {
        let home = Home::new("usbtmc-feature-off");
        let config = source(line_signal());
        fs::write(home.path("sources.json"),json!({"version":1,"sources":[
            {"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"},config
        ]}).to_string()).unwrap();
        assert_eq!(preflight_failure(&home, 5)["code"], "unsupported");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[cfg(feature = "usb")]
mod native {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::{
        io::Write as _,
        process::{Child, Command, Output, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use support::cli::Run;

    struct OwnedChild(Option<Child>);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let Some(mut child) = self.0.take() else {
                return;
            };
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            #[cfg(unix)]
            {
                let group = i32::try_from(child.id()).unwrap();
                // SAFETY: this is the unreaped child-owned group created below.
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        eprintln!("fixture reap failed: {error}");
                        break;
                    }
                    Ok(None) => {}
                }
                if Instant::now() >= until {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            // One final owned reaper may outlive an unconfirmed OS settlement.
            let _reaper = thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
    fn controlled(
        home: &Home,
        command: &mut Command,
        input: Option<&Value>,
        limit: Duration,
        label: &str,
    ) -> (Run, u32) {
        let output_path = home.path(&format!("{label}.stdout"));
        let error_path = home.path(&format!("{label}.stderr"));
        command
            .stdout(fs::File::create(&output_path).unwrap())
            .stderr(fs::File::create(&error_path).unwrap());
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        let mut owned = OwnedChild(Some(child));
        if let Some(input) = input {
            let mut stdin = owned.0.as_mut().unwrap().stdin.take().unwrap();
            stdin
                .write_all(&serde_json::to_vec(input).unwrap())
                .unwrap();
        }
        let until = Instant::now() + limit;
        let status = loop {
            if let Some(status) = owned.0.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < until,
                "{label} exceeded owned test deadline"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let _ = owned.0.take();
        assert!(fs::metadata(&output_path).unwrap().len() <= 2 * 1024 * 1024);
        assert!(fs::metadata(&error_path).unwrap().len() <= 2 * 1024 * 1024);
        (
            Run::of_output(
                Output {
                    status,
                    stdout: fs::read(output_path).unwrap(),
                    stderr: fs::read(error_path).unwrap(),
                },
                &[label],
            ),
            pid,
        )
    }
    fn fixture_env(command: &mut Command, home: &Home, case: &str) {
        command
            .env(
                "CAPTUREFAB_EXECUTABLE",
                env!("CARGO_BIN_EXE_capturefab-helper-fixture"),
            )
            .env("CAPTUREFAB_USBTMC_CASE", case)
            .env("CAPTUREFAB_USBTMC_TRACE", home.path("usb-trace.jsonl"))
            .env("CAPTUREFAB_SESSION_DIR", &home.sessions)
            .env("CAPTUREFAB_CACHE_DIR", home.path("cache"));
    }
    fn record(home: &Home, config: Value, case: &str) -> (Run, u32) {
        write_config(home, config);
        let mut command = home.command();
        fixture_env(&mut command, home, case);
        command.args([
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "400ms",
            "-o",
            "run.jsonl",
        ]);
        controlled(home, &mut command, None, Duration::from_secs(8), "record")
    }
    fn trace(home: &Home) -> Vec<Value> {
        let bytes = fs::read(home.path("usb-trace.jsonl")).unwrap();
        assert!(bytes.len() <= 1_048_576);
        std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn rows(home: &Home) -> Vec<Value> {
        fs::read_to_string(home.path("run.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["kind"] == "sample")
            .collect()
    }
    fn operations<'a>(trace: &'a [Value], operation: &str) -> Vec<&'a Value> {
        trace
            .iter()
            .filter(|row| row["operation"] == operation)
            .collect()
    }
    fn fields<'a>(value: &'a Value, key: &str) -> Vec<&'a Value> {
        let mut found = Vec::new();
        match value {
            Value::Object(values) => {
                for (name, value) in values {
                    if name == key {
                        found.push(value);
                    }
                    found.extend(fields(value, key));
                }
            }
            Value::Array(values) => {
                for value in values {
                    found.extend(fields(value, key));
                }
            }
            _ => {}
        }
        found
    }
    fn safe_operations(log: &[Value], parent_pid: u32) {
        assert_ne!(log, &[] as &[Value]);
        let pid = log[0]["pid"].as_u64().unwrap();
        assert_ne!(pid, u64::from(parent_pid));
        assert_ne!(pid, u64::from(std::process::id()));
        assert!(log.iter().all(|row| row["pid"].as_u64() == Some(pid)));
        for row in operations(log, "control_in") {
            assert!(matches!(
                (
                    row["details"]["request_type"].as_u64().unwrap(),
                    row["details"]["request"].as_u64().unwrap()
                ),
                (161, 7) | (162, 1..=4)
            ));
        }
        for forbidden in [
            "detach",
            "set_configuration",
            "set_interface",
            "reset",
            "initiate_clear",
            "trigger",
            "status_poll",
            "remote",
        ] {
            assert_eq!(operations(log, forbidden), [] as [&Value; 0]);
        }
    }
    fn quote(text: &str) -> String {
        format!("\"{}\"", text.replace('"', "\"\""))
    }
    fn exports(home: &Home, data: &[Value], incomplete: bool) {
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "-o",
            "export.jsonl",
            "--format",
            "jsonl",
        ];
        if incomplete {
            args.push("--allow-incomplete");
        }
        home.ok(&args);
        let expected: Vec<_> = data
            .iter()
            .map(|row| {
                let mut row = row.clone();
                row.as_object_mut().unwrap().remove("kind");
                row
            })
            .collect();
        let actual: Vec<Value> = fs::read_to_string(home.path("export.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(actual, expected);
        let mut args = vec!["data", "export", "run.jsonl", "-o", "export.csv"];
        if incomplete {
            args.push("--allow-incomplete");
        }
        home.ok(&args);
        let mut expected = "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n".to_owned();
        for row in data {
            let mut fields = Vec::new();
            for key in [
                "seq",
                "source",
                "signal",
                "time",
                "elapsed_ns",
                "read_duration_ns",
                "delivery_latency_ns",
                "source_time",
            ] {
                fields.push(match row.get(key) {
                    Some(Value::String(value)) => quote(value),
                    Some(Value::Number(value)) => value.to_string(),
                    None if key == "source_time" => quote(""),
                    value => panic!("unexpected CSV field {key}: {value:?}"),
                });
            }
            fields.push(quote(&row["value"].to_string()));
            expected.push_str(&fields.join(","));
            expected.push('\n');
        }
        assert_eq!(
            fs::read_to_string(home.path("export.csv")).unwrap(),
            expected
        );
    }
    fn binary(home: &Home, value: &Value) {
        let held: Value = serde_json::from_str(WIRE).unwrap();
        let expected = unhex(
            held["application_controls"]["definite_block_reply_hex"]
                .as_str()
                .unwrap(),
        );
        let path = value["raw"]["path"].as_str().unwrap();
        assert_eq!(fs::read(path).unwrap(), expected);
        assert_eq!(value["raw"]["bytes"], 18);
        assert_eq!(value["raw"]["sha256"], hash(&expected));
        assert_eq!(value["block"]["payload_offset"], 9);
        assert_eq!(value["block"]["payload_bytes"], 7);
        assert_eq!(value["block"]["header"], "#17");
        let metadata: Value =
            serde_json::from_slice(&fs::read(value["metadata_path"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(metadata["response"], *value);
        assert!(std::path::Path::new(path).starts_with(home.real("")));
    }

    #[test]
    fn manual_usb_payloads_clocks_padding_and_whole_exports_survive_real_scpi() {
        for case in ["line", "continuation", "pad63", "exact_mps", "cap_future"] {
            let home = Home::new(&format!("usbtmc-{case}"));
            let mut signals = if case == "exact_mps" {
                json!([{"name":"wave","query":"WAVE?","format":"binary_block","response_prefix":":WAVE ","response_terminator":"crlf","output":"wave.scpi"}])
            } else {
                json!([
                    {"name":"number","query":"READ?","format":"number"},
                    {"name":"vector","query":"VEC?","format":"numbers"},
                    {"name":"wave","query":"WAVE?","format":"binary_block","response_prefix":":WAVE ","response_terminator":"crlf","output":"wave.scpi"}
                ])
            };
            if case == "cap_future" {
                signals[0]["name"] = json!("label");
                signals[0]["format"] = json!("text");
            }
            let (run, pid) = record(&home, source(signals), case);
            run.success();
            let data = rows(&home);
            assert_eq!(data.len(), if case == "exact_mps" { 1 } else { 3 });
            let log = trace(&home);
            safe_operations(&log, pid);
            assert_eq!(operations(&log, "query").len(), data.len());
            assert_eq!(operations(&log, "release").len(), 1);
            assert_eq!(operations(&log, "control_in").len(), 1);
            for (index, row) in data.iter().enumerate() {
                assert!(row.get("source_time").is_none());
                let value = &row["value"];
                let clock = operations(&log, "native_in_return")
                    .into_iter()
                    .rfind(|event| {
                        event["details"]["query_ordinal"].as_u64()
                            == Some(u64::try_from(index + 1).unwrap())
                            && event["details"]["eom"] == true
                    })
                    .unwrap()["details"]["observed_ns"]
                    .clone();
                assert_eq!(value["received_monotonic_ns"], clock);
                match row["signal"].as_str().unwrap() {
                    "number" => {
                        assert_eq!(value["value"], json!(1.25));
                        assert_eq!(
                            STANDARD
                                .decode(value["raw"]["data"].as_str().unwrap())
                                .unwrap(),
                            b"1.25\n"
                        );
                    }
                    "label" => {
                        assert_eq!(value["value"], "1.25");
                        assert_eq!(
                            STANDARD
                                .decode(value["raw"]["data"].as_str().unwrap())
                                .unwrap(),
                            b"1.25\n"
                        );
                    }
                    "vector" => assert_eq!(value["value"], json!([1.25, -0.0, 2.5])),
                    "wave" => binary(&home, value),
                    _ => panic!("unexpected USB signal"),
                }
            }
            exports(&home, &data, false);
        }
    }

    #[test]
    fn malformed_wire_and_native_prefix_errors_retire_without_later_queries() {
        for (case, exit, code) in [
            ("bad_tag", 1, "incomplete"),
            ("bad_inverse", 1, "incomplete"),
            ("reserved", 1, "incomplete"),
            ("bad_attribute", 1, "incomplete"),
            ("false_termchar", 1, "incomplete"),
            ("zero", 1, "incomplete"),
            ("over_request", 1, "incomplete"),
            ("premature", 1, "incomplete"),
            ("overlong", 1, "incomplete"),
            ("no_short", 1, "incomplete"),
            ("missing_eom", 1, "incomplete"),
            ("native_prefix", 3, "unavailable"),
            ("premature_release_error", 1, "incomplete"),
        ] {
            let home = Home::new(&format!("usbtmc-{case}"));
            let (run, pid) = record(
                &home,
                source(json!([
                    {"name":"first","query":"READ?","format":"number"},
                    {"name":"must_not_query","query":"SECOND?","format":"number"}
                ])),
                case,
            );
            let error = run.failure(exit);
            assert_eq!(error["code"], code);
            assert_eq!(rows(&home), [] as [Value; 0]);
            let log = trace(&home);
            safe_operations(&log, pid);
            assert_eq!(operations(&log, "query").len(), 1);
            assert_eq!(operations(&log, "release").len(), 1);
            let evidence = fields(&error, "usbtmc_evidence");
            if matches!(
                case,
                "native_prefix" | "premature" | "premature_release_error"
            ) {
                assert_ne!(evidence, [] as [&Value; 0]);
                for value in evidence {
                    assert_eq!(
                        STANDARD
                            .decode(value["payload_prefix_base64"].as_str().unwrap())
                            .unwrap(),
                        b"1.25"
                    );
                    assert_eq!(value["payload_prefix_total"], 4);
                    assert_eq!(value["payload_prefix_truncated"], false);
                    assert_eq!(
                        value["observed_ns"],
                        operations(&log, "native_in_return")[0]["details"]["observed_ns"]
                    );
                }
            } else if matches!(
                case,
                "bad_tag"
                    | "bad_inverse"
                    | "reserved"
                    | "bad_attribute"
                    | "false_termchar"
                    | "zero"
                    | "over_request"
            ) {
                assert_eq!(evidence, [] as [&Value; 0]);
            }
            if case == "premature_release_error" {
                assert!(run.stdout.contains("fixture release failed"));
            }
        }
    }

    #[test]
    fn binary_artifact_sibling_failure_and_prior_poll_prefix_are_atomic() {
        let home = Home::new("usbtmc-sibling-rollback");
        let (run, _) = record(
            &home,
            source(json!([
                {"name":"wave","query":"WAVE?","format":"binary_block","response_prefix":":WAVE ","response_terminator":"crlf","output":"atomic.scpi"},
                {"name":"later","query":"SECOND?","format":"number"}
            ])),
            "second_bad",
        );
        assert_eq!(run.failure(1)["code"], "incomplete");
        assert_eq!(rows(&home), [] as [Value; 0]);
        assert!(!home.path("atomic.scpi").exists() && !home.path("atomic.scpi.json").exists());
        assert!(fs::read_dir(&home.work).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".partial")
        }));
        let home = Home::new("usbtmc-prior-prefix");
        let mut config = source(
            json!([{"name":"wave","query":"WAVE?","format":"binary_block","response_prefix":":WAVE ","response_terminator":"crlf","output":"wave-{seq}.scpi"}]),
        );
        config["every_ms"] = json!(1);
        let (run, pid) = record(&home, config, "second_bad");
        assert_eq!(run.failure(1)["code"], "incomplete");
        let data = rows(&home);
        assert_eq!(data.len(), 1);
        binary(&home, &data[0]["value"]);
        assert!(home.path("wave-000001.scpi").exists());
        assert!(!home.path("wave-000002.scpi").exists());
        let log = trace(&home);
        safe_operations(&log, pid);
        assert_eq!(operations(&log, "query").len(), 2);
        exports(&home, &data, true);
    }

    #[test]
    fn capabilities_identity_and_partial_open_fail_before_application_transfers() {
        for (case, exit, code) in [
            ("cap_failed", 1, "failed"),
            ("cap_short", 1, "incomplete"),
            ("cap_extension", 5, "unsupported"),
            ("busy", 3, "unavailable"),
            ("open_partial", 3, "unavailable"),
        ] {
            let home = Home::new(&format!("usbtmc-{case}"));
            let (run, pid) = record(&home, source(line_signal()), case);
            let error = run.failure(exit);
            assert_eq!(error["code"], code);
            if case == "cap_failed" {
                assert!(
                    fields(&error, "usbtmc_status")
                        .iter()
                        .any(|value| **value == 128)
                );
            }
            let log = trace(&home);
            safe_operations(&log, pid);
            assert_eq!(operations(&log, "query"), [] as [&Value; 0]);
            assert_eq!(operations(&log, "submit_in"), [] as [&Value; 0]);
            assert_eq!(operations(&log, "release").len(), 1);
            assert_eq!(rows(&home), [] as [Value; 0]);
        }
        for case in ["descriptor_mismatch", "alt", "no_interrupt"] {
            let home = Home::new(&format!("usbtmc-{case}"));
            let (run, pid) = record(&home, source(line_signal()), case);
            assert!(matches!(run.code, Some(1 | 3 | 5)), "{}", run.stdout);
            let error = run.json();
            assert!(error["error"]["code"].is_string());
            let log = trace(&home);
            safe_operations(&log, pid);
            assert_eq!(operations(&log, "query"), [] as [&Value; 0]);
            assert_eq!(rows(&home), [] as [Value; 0]);
        }
    }

    #[test]
    fn original_deadlines_and_short_finish_bound_real_client_owned_workers() {
        let home = Home::new("usbtmc-late-data");
        let mut config = source(line_signal());
        config["timeout_ms"] = json!(500);
        let (run, pid) = record(&home, config, "late");
        assert_eq!(run.failure(4)["code"], "timeout");
        let log = trace(&home);
        safe_operations(&log, pid);
        assert_eq!(operations(&log, "query").len(), 1);
        assert_eq!(rows(&home), [] as [Value; 0]);
        for case in [
            "probe_late",
            "open_hang",
            "claim_hang",
            "wait_hang",
            "release_hang",
            "in_pending",
            "out_pending",
            "abort_failed",
            "abort_pending",
            "cancel_hang",
        ] {
            let home = Home::new(&format!("usbtmc-probe-{case}"));
            let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab-helper-fixture"));
            command.arg("__usbtmc-probe").current_dir(&home.work);
            fixture_env(&mut command, &home, case);
            let selector: Value = serde_json::from_str(SELECTOR).unwrap();
            let input = json!({"selector":selector["selector"],"acquisition_ms":500,"finish_ms":if case=="probe_late" {1000} else {100}});
            let began = Instant::now();
            let (run, pid) = controlled(
                &home,
                &mut command,
                Some(&input),
                Duration::from_secs(2),
                "probe",
            );
            let value = run.success();
            assert!(began.elapsed() < Duration::from_secs(2));
            assert!(value["executor_joined"].is_boolean());
            if case == "release_hang" {
                assert_eq!(value["acquisition"]["ok"], true);
            } else {
                assert_eq!(value["acquisition"]["ok"], false);
                assert_eq!(value["acquisition"]["code"], "timeout");
            }
            let log = trace(&home);
            safe_operations(&log, pid);
            assert!(operations(&log, "query").len() <= 1);
            if case == "probe_late" {
                assert_eq!(value["executor_joined"], true);
                assert_eq!(operations(&log, "native_in_return").len(), 1);
                assert_eq!(operations(&log, "query").len(), 1);
                assert_eq!(value["payload_base64"], "");
            }
            if case == "open_hang" {
                assert_eq!(operations(&log, "open").len(), 1);
            }
            if case == "claim_hang" {
                assert_eq!(operations(&log, "claim").len(), 1);
            }
            if case == "wait_hang" {
                assert!(
                    operations(&log, "wait")
                        .iter()
                        .any(|row| row["details"]["direction"] == "In")
                );
            }
            if case == "release_hang" {
                assert_eq!(operations(&log, "release").len(), 1);
            }
            if case == "cancel_hang" {
                assert!(
                    operations(&log, "cancel")
                        .iter()
                        .any(|row| row["details"]["direction"] == "In")
                );
            }
            if case == "abort_failed" {
                assert!(
                    !operations(&log, "control_in")
                        .iter()
                        .any(|row| row["details"]["request"] == 4)
                );
            }
            if case == "out_pending" {
                let cancelled = log
                    .iter()
                    .position(|row| {
                        row["operation"] == "cancel" && row["details"]["direction"] == "Out"
                    })
                    .unwrap();
                let aborted = log
                    .iter()
                    .position(|row| {
                        row["operation"] == "control_in" && row["details"]["request"] == 1
                    })
                    .unwrap();
                let checked = log
                    .iter()
                    .position(|row| {
                        row["operation"] == "control_in" && row["details"]["request"] == 2
                    })
                    .unwrap();
                let halt = log
                    .iter()
                    .position(|row| row["operation"] == "clear_halt")
                    .unwrap();
                assert!(cancelled < aborted && aborted < checked && checked < halt);
                assert_eq!(operations(&log, "submit_in"), [] as [&Value; 0]);
                assert_eq!(value["payload_base64"], "");
            }
            if case == "abort_pending" {
                let checks = operations(&log, "control_in")
                    .into_iter()
                    .filter(|row| row["details"]["request"] == 4)
                    .count();
                assert!(
                    checks >= 2,
                    "pending status loop was not exercised: {log:?}"
                );
                assert_eq!(value["finish"]["ok"], false);
                assert_eq!(value["payload_base64"], "");
                assert_eq!(operations(&log, "query").len(), 1);
            }
        }
    }

    #[test]
    fn malformed_private_replies_never_become_scpi_data_or_new_usb_requests() {
        for case in [
            "ipc_wrong_id",
            "ipc_bad_version",
            "ipc_unknown",
            "ipc_null",
            "ipc_bad_length",
            "ipc_bad_clock",
        ] {
            let home = Home::new(&format!("usbtmc-{case}"));
            let (run, pid) = record(&home, source(line_signal()), case);
            assert_eq!(run.failure(1)["code"], "incomplete");
            assert_eq!(rows(&home), [] as [Value; 0]);
            let log = trace(&home);
            safe_operations(&log, pid);
            assert_eq!(operations(&log, "query"), [] as [&Value; 0]);
            assert_eq!(operations(&log, "control_in"), [] as [&Value; 0]);
            assert!(operations(&log, "manual_query").len() <= 1);
        }
    }
}
