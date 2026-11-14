//! Recording integrity, shared clocks, failure recovery and streaming exports.
mod support;
use serde_json::{Value, json};
#[cfg(unix)]
use std::time::Instant;
use std::{fs, time::Duration};
use support::{cli::Home, time};

fn config(home: &Home) {
    fs::write(
        home.path("sources.json"),
        serde_json::to_vec(&json!({
            "version": 1, "sources": [
                {"id":"press", "protocol":"simulator", "signal":"cycle", "every_ms":time::ms(5)},
                {"id":"oven", "protocol":"simulator", "signal":"temperature", "every_ms":time::ms(7)}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn record(home: &Home) -> Value {
    config(home);
    home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &time::arg(80),
        "-o",
        "run.jsonl",
    ])
}

#[test]
fn every_advertised_data_protocol_can_be_exported() {
    let home = Home::new("data-protocol-catalog");
    let catalog = home.ok(&["data", "protocols"]);
    let protocols = catalog["protocols"].as_array().unwrap();
    assert_ne!(protocols, &Vec::<Value>::new());
    let sources = protocols
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let protocol = row["protocol"].as_str().unwrap();
            json!({"id":format!("source-{index}"),"protocol":protocol})
        })
        .collect::<Vec<_>>();
    let header = |sources: &[Value]| {
        json!({
            "kind":"header","version":1,"clock":"host_monotonic_receive",
            "run_id":"00000000000000000000000000000000",
            "started_at":"2026-01-01T00:00:00Z","sources":sources
        })
    };
    let end = json!({"kind":"end","samples":0,"status":"complete","elapsed_ns":0});
    // The catalogue can exceed the public per-recording limit of 64 sources.
    let mut covered = 0;
    for (batch, selected) in sources.chunks(64).enumerate() {
        let input = format!("catalog-{batch}.jsonl");
        let output = format!("catalog-{batch}.csv");
        let header = header(selected);
        fs::write(home.path(&input), format!("{header}\n{end}\n")).unwrap();
        let result = home.ok(&["data", "export", &input, "-o", &output]);
        assert_eq!(result["complete"], true);
        assert_eq!(result["samples"], 0);
        covered += selected.len();
    }
    assert_eq!(covered, protocols.len());
    let too_many: Vec<_> = (0..65)
        .map(|index| json!({"id":format!("over-limit-{index}"),"protocol":sources[0]["protocol"]}))
        .collect();
    let header = header(&too_many);
    fs::write(home.path("over-limit.jsonl"), format!("{header}\n{end}\n")).unwrap();
    let error = home.fails(
        1,
        &["data", "export", "over-limit.jsonl", "-o", "over-limit.csv"],
    );
    assert_eq!(error["code"], "incomplete");
    assert!(!home.path("over-limit.csv").exists());
}

#[test]
fn two_sources_share_one_clock_and_export_without_losing_values() {
    let home = Home::new("data-clocks");
    let result = record(&home);
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    assert_eq!(rows[0]["kind"], "header");
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let mut counters = std::collections::BTreeMap::new();
    for (index, row) in rows[1..rows.len() - 1].iter().enumerate() {
        assert_eq!(row["seq"], index + 1);
        assert_eq!(row["kind"], "sample");
        let counter = counters.entry(row["source"].as_str().unwrap()).or_insert(0);
        assert_eq!(row["value"], *counter);
        *counter += 1;
        let time = chrono::DateTime::parse_from_rfc3339(row["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            u64::try_from((time - origin).num_nanoseconds().unwrap()).unwrap(),
            row["elapsed_ns"].as_u64().unwrap()
        );
        assert!(row["read_duration_ns"].as_u64().unwrap() <= row["elapsed_ns"].as_u64().unwrap());
    }
    assert_eq!(counters.len(), 2);
    let end = rows.last().unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["samples"], result["samples"]);
    let exported = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "samples.jsonl",
        "--format",
        "jsonl",
    ]);
    assert_eq!(exported["samples"], result["samples"]);
    let samples: Vec<Value> = fs::read_to_string(home.path("samples.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (sample, original) in samples.iter().zip(&rows[1..rows.len() - 1]) {
        let mut original = original.clone();
        original.as_object_mut().unwrap().remove("kind");
        assert_eq!(*sample, original);
    }
    let filtered = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "press.csv",
        "--source",
        "press",
    ]);
    assert_eq!(filtered["samples"], counters["press"]);
    let csv = fs::read_to_string(home.path("press.csv")).unwrap();
    assert!(csv.starts_with(
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n"
    ));
    assert_eq!(
        csv.lines().count(),
        usize::try_from(counters["press"]).unwrap() + 1
    );
}

#[test]
fn config_rejects_mistakes_before_creating_output() {
    let home = Home::new("data-config");
    let cases = [
        json!({"version":2,"sources":[]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"simulator","signal":"s","every_ms":0}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"simulator","signal":"s","typo":1}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"simulator","signal":"s"},{"id":"x","protocol":"simulator","signal":"t"}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"simulator","signal":""}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"camera","camera":"sim:0","output":"frame-{seq}.jpg","part_format":"Confidence8"}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"elements":0}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"elements":126}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"data_type":"u64","elements":32}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"data_type":"u64","elements":65535}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"table":"coil","data_type":"bool","elements":2001}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":65532,"data_type":"u32","elements":3}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":65535,"table":"discrete","data_type":"bool","elements":2}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"ethernet_ip_assembly","address":"127.0.0.1:44818","instance":100,"data_length":9,"signals":[{"name":"count","offset":2,"data_type":"u64"}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"ethernet_ip_assembly","address":"127.0.0.1:44818","instance":100,"data_length":1,"raw_signal":"status","signals":[{"name":"status","offset":0,"data_type":"bool"}]}]}),
        json!({"version":1,"sources":[{"id":"x","protocol":"ethernet_ip_assembly","address":"127.0.0.1:44818","instance":100,"data_length":1,"signals":[{"name":"status","offset":0,"data_type":"bool","bit":8}]}]}),
    ];
    for case in cases {
        fs::write(home.path("sources.json"), case.to_string()).unwrap();
        home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "1ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn numeric_config_errors_are_rejected_exactly_before_creating_output() {
    let home = Home::new("data-exact-config");
    for input in [
        r#"{"version":1.00000000000000000001,"sources":[{"id":"x","protocol":"simulator","signal":"s"}]}"#,
        r#"{"version":1.0,"sources":[{"id":"x","protocol":"simulator","signal":"s","every_ms":1.00000000000000000001}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"simulator","signal":"s","every_ms":3.600001e6}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"simulator","signal":"s","timeout_ms":18446744073709551616.0}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","unit":2.56e2,"signals":[{"name":"x","address":0}]}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":6.5536e4}]}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"elements":2.00000000000000000001}]}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"elements":6.5536e4}]}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"x","address":0,"elements":"2"}]}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"ethernet_ip","address":"127.0.0.1:44818","slot":256.0,"signals":[{"name":"x","tag":"X"}]}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"ethernet_ip_assembly","address":"127.0.0.1:44818","instance":100.00000000000000000001,"data_length":168,"raw_signal":"bytes"}]}"#,
        r#"{"version":1e0,"sources":[{"id":"x","protocol":"scpi","connection":{"transport":"serial","device":"unopened","baud":1e1000},"signals":[{"name":"x","query":"READ?"}]}]}"#,
    ] {
        fs::write(home.path("sources.json"), input).unwrap();
        home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "1ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert!(!home.path("run.jsonl").exists(), "{input}");
    }
}

#[test]
fn duplicate_adapter_options_are_rejected_before_recording() {
    let home = Home::new("data-duplicate-config");
    fs::write(home.path("sources.json"),r#"{"version":1,"sources":[{"id":"counter","protocol":"simulator","signal":"one","signal":"two"}]}"#).unwrap();
    let error = home.fails(
        2,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "1s",
            "-o",
            "run.jsonl",
        ],
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("duplicate field `signal`"),
        "{error}"
    );
    assert!(!home.path("run.jsonl").exists());
}

#[test]
fn export_detects_truncation_and_corruption_and_never_overwrites() {
    let home = Home::new("data-integrity");
    record(&home);
    let original = fs::read(home.path("run.jsonl")).unwrap();
    home.fails(
        1,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "1ms",
            "-o",
            "run.jsonl",
        ],
    );
    assert_eq!(fs::read(home.path("run.jsonl")).unwrap(), original);
    let mut rows = rows(&home);
    rows.pop();
    let mut partial = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    partial.push_str("\n{\"kind\":\"sam");
    fs::write(home.path("partial.jsonl"), &partial).unwrap();
    home.fails(1, &["data", "export", "partial.jsonl", "-o", "partial.csv"]);
    assert!(!home.path("partial.csv").exists());
    let recovered = home.ok(&[
        "data",
        "export",
        "partial.jsonl",
        "-o",
        "partial.csv",
        "--allow-incomplete",
    ]);
    assert_eq!(recovered["complete"], false);
    let saved = fs::read(home.path("partial.csv")).unwrap();
    home.fails(
        1,
        &[
            "data",
            "export",
            "partial.jsonl",
            "-o",
            "partial.csv",
            "--allow-incomplete",
        ],
    );
    assert_eq!(fs::read(home.path("partial.csv")).unwrap(), saved);
    rows[1]["seq"] = json!(100);
    fs::write(
        home.path("bad.jsonl"),
        format!(
            "{}\n",
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )
    .unwrap();
    home.fails(
        1,
        &[
            "data",
            "export",
            "bad.jsonl",
            "-o",
            "bad.csv",
            "--allow-incomplete",
        ],
    );
    assert!(!home.path("bad.csv").exists());
}

#[test]
fn csv_quotes_json_strings_objects_commas_quotes_and_newlines() {
    let home = Home::new("data-csv");
    record(&home);
    let mut rows = rows(&home);
    rows[1]["value"] = json!({"text":"a,\"b\"\nc"});
    fs::write(
        home.path("run.jsonl"),
        format!(
            "{}\n",
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )
    .unwrap();
    home.ok(&["data", "export", "run.jsonl", "-o", "run.csv"]);
    let csv = fs::read_to_string(home.path("run.csv")).unwrap();
    let escaped = format!("\"{}\"", rows[1]["value"].to_string().replace('"', "\"\""));
    assert!(csv.contains(&escaped), "{csv}");
}

#[cfg(unix)]
#[test]
fn helper_failure_stops_active_peers_and_keeps_partial_samples() {
    let home = Home::new("data-failure");
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[
        {"id":"busy","protocol":"simulator","signal":"s","every_ms":1},
        {"id":"plc","protocol":"opc_ua","executable":"/bin/sh","args":["-c","printf '%s\\n' '{\"signal\":\"s\",\"value\":42}'; sleep 0.08"],"every_ms":1,"timeout_ms":100}
    ]}).to_string()).unwrap();
    let start = Instant::now();
    let error = home.fails(
        3,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "5s",
            "-o",
            "run.jsonl",
        ],
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(error["details"]["status"], "failed");
    let rows = rows(&home);
    assert!(
        rows.iter()
            .any(|row| row["source"] == "plc" && row["value"] == 42)
    );
    assert_eq!(rows.last().unwrap()["status"], "failed");
    home.fails(1, &["data", "export", "run.jsonl", "-o", "strict.csv"]);
    let recovered = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "recovered.csv",
        "--allow-incomplete",
    ]);
    assert_eq!(recovered["samples"], error["details"]["samples"]);
}

#[cfg(unix)]
#[test]
fn interrupt_finishes_recording_and_reports_saved_samples() {
    let home = Home::new("data-interrupt");
    config(&home);
    let mut child = home
        .command()
        .args([
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "30s",
            "-o",
            "run.jsonl",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ready_by = Instant::now() + time::secs(5);
    loop {
        assert!(child.try_wait().unwrap().is_none());
        if fs::metadata(home.path("run.jsonl")).is_ok_and(|meta| meta.len() > 1000) {
            break;
        }
        assert!(Instant::now() < ready_by);
        std::thread::sleep(Duration::from_millis(10));
    }
    support::cli::signal_and_wait(&mut child, libc::SIGINT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["details"]["status"], "interrupted");
    assert_eq!(rows(&home).last().unwrap()["status"], "interrupted");
}

#[test]
fn export_rejects_inconsistent_clocks_even_during_recovery() {
    let home = Home::new("data-clock-corruption");
    record(&home);
    let original = rows(&home);
    for case in 0..6 {
        let mut rows = original.clone();
        match case {
            0 => rows[1]["time"] = json!("2000-01-01T00:00:00Z"),
            1 => rows[1]["read_duration_ns"] = json!(u64::MAX),
            2 => rows.last_mut().unwrap()["elapsed_ns"] = json!(0),
            3 => rows[0]["sources"][0]["protocol"] = json!("unknown"),
            4 => rows[0]["run_id"] = json!(""),
            _ => {
                let first = rows[1].clone();
                let later = rows
                    .iter_mut()
                    .skip(2)
                    .find(|row| row["source"] == first["source"])
                    .unwrap();
                let elapsed = first["elapsed_ns"].as_u64().unwrap().saturating_sub(1);
                let origin = chrono::DateTime::parse_from_rfc3339(
                    original[0]["started_at"].as_str().unwrap(),
                )
                .unwrap();
                later["elapsed_ns"] = json!(elapsed);
                later["read_duration_ns"] = json!(0);
                later["time"] = json!(
                    (origin + chrono::Duration::nanoseconds(i64::try_from(elapsed).unwrap()))
                        .to_rfc3339()
                );
            }
        }
        let bytes = format!(
            "{}\n",
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
        fs::write(home.path("bad-clock.jsonl"), bytes).unwrap();
        home.fails(
            1,
            &[
                "data",
                "export",
                "bad-clock.jsonl",
                "-o",
                "bad-clock.csv",
                "--allow-incomplete",
            ],
        );
        assert!(!home.path("bad-clock.csv").exists());
    }
}

#[test]
fn an_unresponsive_first_poll_never_produces_a_complete_recording() {
    use std::net::TcpListener;
    let home = Home::new("data-unready");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    });
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[{
            "id":"plc","protocol":"modbus_tcp","address":address.to_string(),
            "signals":[{"name":"pressure","address":0}],"timeout_ms":1000
        }]})
        .to_string(),
    )
    .unwrap();
    let error = home.fails(
        4,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "50ms",
            "-o",
            "run.jsonl",
        ],
    );
    server.join().unwrap();
    assert_eq!(error["code"], "timeout");
    assert_eq!(error["details"]["status"], "failed");
    assert_eq!(error["details"]["samples"], 0);
    assert_eq!(rows(&home).last().unwrap()["status"], "failed");
}

#[test]
fn mixed_recording_checks_part_selection_on_the_connected_camera() {
    let home = Home::new("data-camera-part-selection");
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[
            {"id":"press","protocol":"simulator","signal":"cycle","every_ms":10},
            {"id":"inspection","protocol":"camera","camera":"sim:0","output":"frame.raw",
             "part_format":"Confidence8","timeout_ms":5000}
        ]})
        .to_string(),
    )
    .unwrap();
    let error = home.fails(
        5,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "5s",
            "-o",
            "run.jsonl",
        ],
    );
    assert_eq!(error["code"], "unsupported");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("native GigE Vision"),
        "{error}"
    );
    assert!(!home.path("frame.raw").exists());
    let rows = rows(&home);
    assert_eq!(rows[0]["sources"][0]["id"], "press");
    assert_eq!(rows[0]["sources"][1]["id"], "inspection");
    assert!(rows.iter().all(|row| row["source"] != "inspection"));
    assert_eq!(rows.last().unwrap()["status"], "failed");
}

#[test]
fn camera_files_and_sensor_values_share_the_frame_receive_timeline() {
    let home = Home::new("data-camera");
    let extension = if cfg!(feature = "jpeg") { "jpg" } else { "png" };
    let output = format!("frames/{{seq}}.{extension}");
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[
            {"id":"press","protocol":"simulator","signal":"cycle","every_ms":time::ms(10)},
            {"id":"inspection","protocol":"camera","camera":"sim:0","output":output,
             "every_ms":time::ms(100),"timeout_ms":time::ms(1000),"settings":["Width=64","Height=64"]}
        ]})
        .to_string(),
    )
    .unwrap();
    let result = home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &time::arg(650),
        "-o",
        "run.jsonl",
    ]);
    let rows = rows(&home);
    let origin =
        chrono::DateTime::parse_from_rfc3339(rows[0]["started_at"].as_str().unwrap()).unwrap();
    let images = rows
        .iter()
        .filter(|row| row["source"] == "inspection")
        .collect::<Vec<_>>();
    assert!(images.len() >= 2, "{result}");
    assert!(rows.iter().any(|row| row["source"] == "press"));
    for image in images {
        let value = &image["value"];
        let bytes = fs::read(value["path"].as_str().unwrap()).unwrap();
        assert_eq!(value["width"], 64);
        assert_eq!(value["height"], 64);
        assert_eq!(value["bytes"], bytes.len());
        if cfg!(feature = "jpeg") {
            assert_eq!(&bytes[..2], &[0xff, 0xd8]);
        } else {
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        }
        assert!(image["delivery_latency_ns"].as_u64().unwrap() > 0);
        let received =
            chrono::DateTime::parse_from_rfc3339(value["received_at"].as_str().unwrap()).unwrap();
        let time = chrono::DateTime::parse_from_rfc3339(image["time"].as_str().unwrap()).unwrap();
        assert!((received - time).num_milliseconds().abs() <= 3, "{image}");
        assert_eq!(
            u64::try_from((time - origin).num_nanoseconds().unwrap()).unwrap(),
            image["elapsed_ns"].as_u64().unwrap()
        );
        assert!(value["device_ts_ns"].is_u64());
    }
    home.ok(&["data", "export", "run.jsonl", "-o", "joint.csv"]);
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "images.jsonl",
        "--format",
        "jsonl",
        "--source",
        "inspection",
    ]);
}
