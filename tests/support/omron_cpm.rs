//! W353-E1-06 complete C-mode frames. All reply bytes are spec-derived,
//! not a stock decoder/device trace; the immutable JSON predates this decoder.

use super::{Home, Terminal, config, record, rows};
use serde_json::{Value, json};
use std::{fs, io::Write as _, os::fd::AsRawFd as _, thread, time::Duration};

fn vectors() -> Value {
    serde_json::from_str(include_str!("../fixtures/omron/cpm-literals.json")).unwrap()
}
fn cpm_config(terminal: &Terminal, signals: &Value) -> Value {
    let mut config = config(terminal, "c_mode_cpm", signals);
    config["sources"][0]["model"] = json!("cpm2a");
    config["sources"][0]["unit"] = json!(0);
    config["sources"][0]["timeout_ms"] = json!(550);
    config
}
fn exchange(terminal: &Terminal, case: &Value) {
    terminal.expect(case["request_ascii"].as_str().unwrap().as_bytes());
    terminal.reply(case["response_ascii"].as_str().unwrap().as_bytes());
}
fn no_more(terminal: &Terminal) {
    let mut poll = libc::pollfd {
        fd: terminal.master.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        // SAFETY: poll contains one live PTY master and a nonblocking timeout.
        unsafe { libc::poll(&raw mut poll, 1, 0) },
        0,
        "unexpected later command or reconnect bytes"
    );
}
fn frame(body: &[u8]) -> Vec<u8> {
    let sum = body.iter().fold(0, |sum, &byte| sum ^ byte);
    [body, format!("{sum:02X}*\r").as_bytes()].concat()
}
fn projected(home: &Home, expected: &[Value], complete: bool, total: usize) {
    let recorded = rows(home);
    let end = recorded.last().unwrap();
    assert_eq!(end["status"], if complete { "complete" } else { "failed" });
    assert_eq!(end["samples"], total);
    let samples = recorded
        .iter()
        .filter(|row| row["source"] == "plc")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), expected.len());
    let config: Value =
        serde_json::from_str(&fs::read_to_string(home.path("sources.json")).unwrap()).unwrap();
    let declared = config["sources"][0]["signals"].as_array().unwrap();
    for (index, (sample, expected)) in samples.iter().zip(expected).enumerate() {
        assert_eq!(sample["signal"], declared[index % declared.len()]["name"]);
        assert_eq!(&sample["value"], expected);
        assert!(sample.get("source_time").is_none());
        for field in ["elapsed_ns", "read_duration_ns", "delivery_latency_ns"] {
            assert!(sample[field].is_u64());
        }
    }
    for format in ["jsonl", "csv"] {
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "-o",
            format,
            "--format",
            format,
        ];
        if !complete {
            args.push("--allow-incomplete");
        }
        assert_eq!(home.ok(&args)["samples"], total);
    }
    let exported = fs::read_to_string(home.path("jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["source"] == "plc")
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), samples.len());
    for (row, original) in exported.iter().zip(&samples) {
        let mut original = Value::clone(*original);
        assert_eq!(
            original.as_object_mut().unwrap().remove("kind"),
            Some(json!("sample"))
        );
        assert_eq!(*row, original);
    }
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json"
    );
    let actual = lines
        .filter(|line| line.split(',').nth(1) == Some("\"plc\""))
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    let want = expected
        .iter()
        .map(|value| format!("\"{}\"", value.to_string().replace('"', "\"\"")))
        .collect::<Vec<_>>();
    assert_eq!(actual, want.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn legacy_native_areas_and_complete_frame_chunks_preserve_types_and_exports() {
    let home = Home::new("omron-cpm-complete");
    let terminal = Terminal::open();
    let input = vectors();
    let cases = input["cases"].as_array().unwrap();
    let mut config = cpm_config(
        &terminal,
        &json!([
            {"name":"ir","area":"ir_sr","address":0,"elements":2},
            {"name":"hr","area":"hr","address":1,"elements":3,"data_type":"i16"},
            {"name":"lr","area":"lr","address":14,"elements":2,"unit":31},
            {"name":"ar","area":"ar","address":22,"elements":2,"unit":1},
            {"name":"pv","area":"tc_pv","address":7,"elements":3},
            {"name":"flags","area":"tc_status","address":0,"elements":5,"data_type":"bool"},
            {"name":"dm","area":"dm","address":0,"elements":10},
            {"name":"exact","area":"dm","address":0,"data_type":"i64"},
            {"name":"word_chunks","area":"ir_sr","address":200,"elements":31},
            {"name":"flag_chunks","area":"tc_status","address":0,"elements":256,"data_type":"bool"}
        ]),
    );
    config["sources"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"sim","protocol":"simulator","signal":"cycle","every_ms":3_600_000}));
    thread::scope(|scope| {
        scope.spawn(|| {
            for index in [0, 1, 2, 3, 4, 5, 7, 10] {
                exchange(&terminal, &cases[index]);
            }
            let schedule = &input["multi_command_cases"][0]["requests_ascii"];
            terminal.expect(schedule[0].as_str().unwrap().as_bytes());
            terminal.reply(cases[8]["response_ascii"].as_str().unwrap().as_bytes());
            terminal.expect(schedule[1].as_str().unwrap().as_bytes());
            terminal.reply(b"@00RR00000040*\r");
            let schedule = &input["multi_command_cases"][1]["requests_ascii"];
            for index in 0..3 {
                terminal.expect(schedule[index].as_str().unwrap().as_bytes());
                if index < 2 {
                    terminal.reply(cases[9]["response_ascii"].as_str().unwrap().as_bytes());
                } else {
                    terminal.reply(b"@00RG00000000000000000055*\r");
                }
            }
        });
        assert_eq!(record(&home, &config).success()["samples"], 11);
    });
    no_more(&terminal);
    let expected = [
        cases[0]["native_words_u16"].clone(),
        json!([-1, -32_768, 1]),
        cases[2]["native_words_u16"].clone(),
        cases[3]["native_words_u16"].clone(),
        cases[4]["native_words_u16"].clone(),
        cases[5]["native_completion_flags"].clone(),
        cases[7]["native_words_u16"].clone(),
        json!(i64::MIN),
        json!(vec![0; 31]),
        json!(vec![false; 256]),
    ];
    projected(&home, &expected, true, 11);
    let records = rows(&home);
    for (index, row) in records
        .iter()
        .filter(|row| row["kind"] == "sample")
        .enumerate()
    {
        assert_eq!(row["seq"], index + 1);
    }
}

#[test]
fn old_srm1_profile_reads_its_documented_dm_end_without_alias_or_fins() {
    for model in ["srm1", "srm1_v2"] {
        let home = Home::new("omron-srm1");
        let terminal = Terminal::open();
        let mut config = cpm_config(
            &terminal,
            &json!([{"name":"last","area":"dm","address":2047}]),
        );
        config["sources"][0]["model"] = json!(model);
        thread::scope(|scope| {
            scope.spawn(|| {
                terminal.expect(b"@00RD2047000156*\r");
                terminal.reply(b"@00RD00FFFF56*\r");
            });
            assert_eq!(record(&home, &config).success()["samples"], 1);
        });
        no_more(&terminal);
        projected(&home, &[json!(65_535)], true, 1);
    }
}

#[test]
fn malformed_late_sibling_and_remote_status_keep_only_earlier_complete_poll() {
    let input = vectors();
    let first = &input["cases"][0];
    let second = &input["cases"][5];
    let mut bad_fcs = second["response_ascii"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec();
    bad_fcs[7] ^= 1;
    for (reply, code, remote) in [
        (bad_fcs, "incomplete", None),
        (frame(b"@01RG0010101"), "incomplete", None),
        (frame(b"@00RH0010101"), "incomplete", None),
        (frame(b"@00RG001010"), "incomplete", None),
        (frame(b"@00RG0010201"), "incomplete", None),
        (vec![b'A'; 132], "incomplete", None),
        (
            input["cases"][6]["response_ascii"]
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
            "failed",
            Some(20),
        ),
    ] {
        let home = Home::new("omron-cpm-prefix");
        let terminal = Terminal::open();
        let mut config = cpm_config(
            &terminal,
            &json!([
                {"name":"words","area":"ir_sr","address":0,"elements":2},
                {"name":"flags","area":"tc_status","address":0,"elements":5,"data_type":"bool"}
            ]),
        );
        config["sources"][0]["every_ms"] = json!(200);
        let error = thread::scope(|scope| {
            scope.spawn(|| {
                exchange(&terminal, first);
                exchange(&terminal, second);
                exchange(&terminal, first);
                terminal.expect(second["request_ascii"].as_str().unwrap().as_bytes());
                (&terminal.master).write_all(&reply).unwrap();
            });
            record(&home, &config).failure(1)
        });
        assert_eq!(error["code"], code);
        if let Some(status) = remote {
            assert_eq!(error["details"]["host_link_end_code"], status);
        }
        no_more(&terminal);
        projected(
            &home,
            &[
                first["native_words_u16"].clone(),
                second["native_completion_flags"].clone(),
            ],
            false,
            2,
        );
        assert_eq!(
            home.fails(1, &["data", "export", "run.jsonl", "-o", "refused"])["code"],
            "incomplete"
        );
        assert!(!home.path("refused").exists());
    }
}

#[test]
fn malformed_first_poll_never_publishes_a_valid_sibling() {
    let home = Home::new("omron-cpm-atomic");
    let terminal = Terminal::open();
    let input = vectors();
    let config = cpm_config(
        &terminal,
        &json!([
            {"name":"words","area":"ir_sr","address":0,"elements":2},
            {"name":"flags","area":"tc_status","address":0,"elements":5,"data_type":"bool"}
        ]),
    );
    thread::scope(|scope| {
        scope.spawn(|| {
            exchange(&terminal, &input["cases"][0]);
            terminal.expect(
                input["cases"][5]["request_ascii"]
                    .as_str()
                    .unwrap()
                    .as_bytes(),
            );
            terminal.reply(&frame(b"@00RG0010201"));
        });
        assert_eq!(record(&home, &config).failure(1)["code"], "incomplete");
    });
    no_more(&terminal);
    projected(&home, &[], false, 0);
}

#[test]
fn original_request_and_acquisition_cutoffs_do_not_admit_a_third_read() {
    for acquisition in [false, true] {
        let home = Home::new("omron-cpm-deadline");
        let terminal = Terminal::open();
        let input = vectors();
        let mut config = cpm_config(
            &terminal,
            &json!([
                {"name":"words","area":"ir_sr","address":0,"elements":2},
                {"name":"flags","area":"tc_status","address":0,"elements":5,"data_type":"bool"},
                {"name":"never","area":"hr","address":0}
            ]),
        );
        config["sources"][0]["timeout_ms"] = json!(if acquisition { 5000 } else { 1000 });
        thread::scope(|scope| {
            scope.spawn(|| {
                terminal.expect(
                    input["cases"][0]["request_ascii"]
                        .as_str()
                        .unwrap()
                        .as_bytes(),
                );
                thread::sleep(Duration::from_millis(600));
                terminal.reply(
                    input["cases"][0]["response_ascii"]
                        .as_str()
                        .unwrap()
                        .as_bytes(),
                );
                terminal.expect(
                    input["cases"][5]["request_ascii"]
                        .as_str()
                        .unwrap()
                        .as_bytes(),
                );
                for chunk in input["cases"][5]["response_ascii"]
                    .as_str()
                    .unwrap()
                    .as_bytes()
                    .chunks(6)
                {
                    thread::sleep(Duration::from_millis(200));
                    (&terminal.master).write_all(chunk).unwrap();
                }
            });
            let run = if acquisition {
                run_for(&home, &config, "1s")
            } else {
                run_for(&home, &config, "2s")
            };
            assert_eq!(run.failure(4)["code"], "timeout");
        });
        no_more(&terminal);
        projected(&home, &[], false, 0);
    }
}

fn run_for(home: &Home, config: &Value, duration: &str) -> super::Run {
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    home.run(&[
        "--json",
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

#[test]
fn cpm_model_shape_and_closed_serial_controls_fail_before_open_or_output() {
    let home = Home::new("omron-cpm-admission");
    let base = json!({"id":"plc","protocol":"omron_host_link","mode":"c_mode_cpm","model":"cpm1","device":home.path("must-not-open"),"signals":[{"name":"value","area":"dm","address":0}]});
    let mut bodies = Vec::new();
    for patch in [
        json!({"model":"cs1"}),
        json!({"baud":115_200}),
        json!({"baud":691_200}),
        json!({"unit":32}),
        json!({"data_bits":6}),
        json!({"stop_bits":3}),
        json!({"response_wait_ms":0}),
    ] {
        let mut body = base.clone();
        for (key, value) in patch.as_object().unwrap() {
            body[key].clone_from(value);
        }
        bodies.push(body);
    }
    for signal in [
        json!({"name":"x","area":"dm","address":1023,"elements":2}),
        json!({"name":"x","area":"dm","address":6143,"elements":2}),
        json!({"name":"x","area":"ir_sr","address":19,"elements":2}),
        json!({"name":"x","area":"ir_sr","address":199}),
        json!({"name":"x","area":"ar","address":22}),
        json!({"name":"x","area":"tc_pv","address":127,"elements":2}),
        json!({"name":"x","area":"lr","address":15,"data_type":"u32"}),
        json!({"name":"x","area":"tc_status","address":0}),
        json!({"name":"x","area":"tc_status","address":0,"data_type":"bool","word_order":"big"}),
        json!({"name":"x","area":"dm","address":0,"data_type":"bool"}),
        json!({"name":"x","area":"dm","address":0,"elements":0}),
        json!({"name":"x","area":"dm","address":0,"unit":32}),
        json!({"name":"x","area":"dm","address":0,"bit":0}),
        json!({"name":"x","area":"wr","address":0}),
        json!({"name":"x","area":"dm","address":0,"bank":0}),
        json!({"name":"x","area":"dm","address":0,"data_type":"u64","scale":2}),
    ] {
        let mut body = base.clone();
        body["signals"] = json!([signal]);
        bodies.push(body);
    }
    let mut minimum = base.clone();
    minimum["model"] = json!("cpm2a");
    minimum["signals"] = json!((0..256).map(|index| json!({"name":format!("value{index}"),"area":"dm","address":0,"elements":2048})).collect::<Vec<_>>());
    bodies.push(minimum);
    let mut missing = base;
    missing.as_object_mut().unwrap().remove("model");
    bodies.push(missing);
    for body in bodies {
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[body]}).to_string(),
        )
        .unwrap();
        assert_eq!(
            home.fails(
                2,
                &[
                    "--json",
                    "data",
                    "record",
                    "--config",
                    "sources.json",
                    "--for",
                    "10ms",
                    "-o",
                    "run.jsonl"
                ]
            )["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
