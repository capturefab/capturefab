//! Synthetic worker transcripts qualify coordinator lifecycle, not camera firmware.
use super::{assert_worker_reaped, configured, monotonic_ns, rows};
use crate::support::{
    cli::{Home, Run},
    time,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead as _, Write as _},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn trace(operation: &str) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("key-trace")
        .unwrap();
    writeln!(file, "{operation}").unwrap();
}

fn operation_ns() -> u64 {
    #[cfg(target_os = "macos")]
    let clock = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_os = "macos"))]
    let clock = libc::CLOCK_MONOTONIC;
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes this complete timespec on success.
    let success = unsafe { libc::clock_gettime(clock, time.as_mut_ptr()) };
    assert_eq!(success, 0);
    // SAFETY: the successful call above initialized the complete timespec.
    let time = unsafe { time.assume_init() };
    u64::try_from(time.tv_sec).unwrap() * 1_000_000_000 + u64::try_from(time.tv_nsec).unwrap()
}

pub(super) fn worker(mode: &str) -> ! {
    let mut output = std::io::stdout();
    output.write_all(b"\n").unwrap();
    let mut sequence = 0;
    let mut emit = |mut value: Value| {
        value["seq"] = json!(sequence);
        sequence += 1;
        serde_json::to_writer(&mut output, &value).unwrap();
        output.write_all(b"\n").unwrap();
        output.flush().unwrap();
    };
    emit(json!({"event":"hello","protocol":10,"build":"industrial-key-fixture"}));
    let identity = json!({"id":"sim:0","transport":"simulator","vendor":"Fixture","model":"Key source",
        "serial":"key-source","profile":{"kind":"area_scan","source":"identity","evidence":[]}});
    let mut prepared = false;
    let mut keyed = false;
    let mut setup_observed = None;
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match request["op"].as_str().unwrap() {
            "shutdown" => std::process::exit(0),
            "cancel" => {}
            "stop" => {
                assert!(request["job"].as_u64().is_some_and(|job| job > 0));
                trace("stop_job");
            }
            "ping" => emit(
                json!({"event":"reply","id":request["id"],"ok":true,"result":{"heartbeat_age_ms":0}}),
            ),
            "status" => emit(json!({"event":"reply","id":request["id"],"ok":true,
                "result":{"connected":identity,"streaming":false,"frames":0,"dropped":0,"fps":0.0}})),
            "run" => {
                let id = request["id"].as_u64().unwrap();
                emit(json!({"event":"accepted","id":id}));
                emit(json!({"event":"started","id":id,"budget_ms":60000}));
                let outer = &request["command"];
                let wrapped = outer["op"] == "session_until";
                let command = if wrapped { &outer["command"] } else { outer };
                let operation = command["op"].as_str().unwrap();
                trace(operation);
                match operation {
                    "connect" | "check_connection" => emit(
                        json!({"event":"reply","id":id,"ok":true,"result":{"connected":identity}}),
                    ),
                    "disconnect" => emit(json!({"event":"reply","id":id,"ok":true,"result":{}})),
                    "set_values" => {
                        assert!(wrapped && !prepared && !keyed);
                        assert!(outer["deadline_ns"].as_u64().is_some_and(|value| value > 0));
                        setup_observed = Some(operation_ns());
                        assert_eq!(command["values"].as_array().unwrap().len(), 2);
                        assert!(
                            !command["values"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|value| value["name"] == "ActionDeviceKey")
                        );
                        prepared = true;
                        if mode == "industrial_key_deadline" {
                            // Startup consumes the same source budget. Spend
                            // part of what actually remains, so this case can
                            // reach key admission without renewing that budget.
                            let budget = outer["deadline_ns"]
                                .as_u64()
                                .unwrap()
                                .checked_sub(setup_observed.unwrap())
                                .unwrap();
                            assert!(budget <= time::ms(2000) * 1_000_000);
                            let progress = (budget / 4).min(200_000_000);
                            assert!(progress > 0);
                            std::thread::sleep(Duration::from_nanos(progress));
                        }
                        emit(json!({"event":"reply","id":id,"ok":true,"result":{"changed":[]}}));
                    }
                    "write_action_device_key_until" => {
                        assert!(!wrapped && prepared && !keyed);
                        assert_eq!(command["key"], 17);
                        if mode == "industrial_key_deadline" {
                            // Setup receipt is after source-read start, so this
                            // upper bound cannot shorten the legitimate cutoff.
                            // A fresh 2s budget after positive setup progress
                            // exceeds it; the field check detects that renewal.
                            assert!(
                                command["deadline_ns"].as_u64().unwrap()
                                    <= setup_observed
                                        .unwrap()
                                        .checked_add(time::ms(2000) * 1_000_000)
                                        .unwrap()
                            );
                        }
                        if mode == "industrial_key_denied" {
                            std::thread::sleep(Duration::from_millis(100));
                            emit(
                                json!({"event":"reply","id":id,"ok":false,"error":{"code":"permission_denied",
                                "message":"fixture key write denied","hint":"first fixture hint",
                                "details":{"native_code":-1005,"write_attempted":true,"write_may_have_applied":true}}}),
                            );
                            continue;
                        }
                        if mode == "industrial_key_deadline" {
                            // Exceed the original key cutoff using only what
                            // actually remains at receipt. A command queued at
                            // expiry still waits past it without a fixture panic.
                            let remaining = command["deadline_ns"]
                                .as_u64()
                                .unwrap()
                                .saturating_sub(operation_ns());
                            std::thread::sleep(Duration::from_nanos(
                                remaining.checked_add(25_000_000).unwrap(),
                            ));
                        }
                        keyed = true;
                        emit(
                            json!({"event":"reply","id":id,"ok":true,"result":{"camera":"sim:0","feature":"ActionDeviceKey",
                            "write_completed":true,"completion":"producer_reported","readback":"unavailable_write_only",
                            "rollback":"not_available","scope":"current_control_epoch"}}),
                        );
                        if mode == "industrial_key_crash" {
                            std::process::exit(9);
                        }
                    }
                    "schedule" => {
                        assert!(wrapped && prepared && keyed);
                        let job = command["job"].as_u64().unwrap();
                        let path = command["output"]
                            .as_str()
                            .unwrap()
                            .replace("{seq}", &format!("{job:04}"));
                        fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
                        fs::write(&path, [7]).unwrap();
                        let wall_ms = u64::try_from(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis(),
                        )
                        .unwrap();
                        emit(
                            json!({"event":"saved","job":job,"file":{"kind":"capture","wall_ms":wall_ms,
                            "received_monotonic_ns":monotonic_ns(),"path":path,"bytes":1,"frame_id":job,
                            "width":1,"height":1,"pixel_format":0x0108_0001}}),
                        );
                        emit(
                            json!({"event":"job_end","job":job,"status":"done","summary":{"captured":1}}),
                        );
                        if mode == "industrial_key_saved_error" {
                            emit(
                                json!({"event":"reply","id":id,"ok":false,"error":{"code":"failed","message":"fixture late schedule failure"}}),
                            );
                        } else {
                            emit(
                                json!({"event":"reply","id":id,"ok":true,"result":{"job":{"id":job,"captured":1}}}),
                            );
                        }
                    }
                    other => panic!("unexpected industrial key fixture command: {other}"),
                }
            }
            other => panic!("unexpected industrial key fixture request: {other}"),
        }
    }
    std::process::exit(0);
}

fn declarations(home: &Home, two: bool, timeout_ms: u64) {
    let mut sources = vec![
        json!({"id":"a","protocol":"camera","camera":"sim:0","output":"frames/a-{seq}.raw",
        "action_device_key":17,"settings":["Width=1","Height=1"],"every_ms":time::ms(10000),"timeout_ms":timeout_ms}),
    ];
    if two {
        sources.push(
            json!({"id":"b","protocol":"camera","camera":"sim:0","output":"frames/b-{seq}.raw",
            "settings":["Height=1"],"every_ms":time::ms(10000),"timeout_ms":timeout_ms}),
        );
    }
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
}

fn operations(home: &Home) -> Vec<String> {
    fs::read_to_string(home.path("key-trace"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn shared_still_sources_assign_one_key_before_both_jobs_without_exporting_it() {
    let (home, mut command) = configured("industrial_key_ok", 1000);
    declarations(&home, true, time::ms(2000));
    let window = time::arg(3000);
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &window,
        "-o",
        "run.jsonl",
    ];
    let result = Run::of_output(command.args(args).output().unwrap(), &args).success();
    assert_eq!(result["samples"], 2);
    let trace = operations(&home);
    assert_eq!(trace.iter().filter(|op| *op == "connect").count(), 1);
    assert_eq!(trace.iter().filter(|op| *op == "set_values").count(), 1);
    assert_eq!(
        trace
            .iter()
            .filter(|op| *op == "write_action_device_key_until")
            .count(),
        1
    );
    assert_eq!(trace.iter().filter(|op| *op == "schedule").count(), 2);
    let samples = rows(&home)
        .into_iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 2);
    for sample in samples {
        assert_eq!(
            fs::read(sample["value"]["path"].as_str().unwrap()).unwrap(),
            [7]
        );
        assert!(sample["elapsed_ns"].as_u64().is_some_and(|ns| ns > 0));
    }
    for (output, format) in [("export.jsonl", "jsonl"), ("export.csv", "csv")] {
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            output,
        ]);
        let exported = fs::read_to_string(home.path(output)).unwrap();
        assert!(!exported.contains("action_device_key") && !exported.contains("ActionDeviceKey"));
    }
    let recording = fs::read_to_string(home.path("run.jsonl")).unwrap();
    assert!(!recording.contains("action_device_key") && !recording.contains("ActionDeviceKey"));
    assert_worker_reaped(&home);
}

#[test]
fn setup_failure_cutoff_and_worker_loss_never_reopen_or_repeat_the_key() {
    for (mode, timeout, exit) in [
        ("industrial_key_denied", time::ms(2000), 3),
        ("industrial_key_deadline", time::ms(2000), 4),
        ("industrial_key_crash", time::ms(2000), 3),
    ] {
        let (home, mut command) = configured(mode, 1000);
        declarations(&home, true, timeout);
        let window = time::arg(3000);
        let args = [
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            &window,
            "-o",
            "run.jsonl",
        ];
        let began = Instant::now();
        let error = Run::of_output(command.args(args).output().unwrap(), &args).failure(exit);
        if mode == "industrial_key_denied" {
            assert_eq!(error["code"], "permission_denied");
            assert_eq!(error["details"]["native_code"], -1005);
            assert_eq!(error["details"]["write_may_have_applied"], true);
        } else if mode == "industrial_key_deadline" {
            assert_eq!(error["code"], "timeout");
            assert_eq!(error["details"]["initialization_stage"], "write_key");
            assert_eq!(error["details"]["write_may_have_applied"], true);
            assert!(began.elapsed() < time::secs(5));
        }
        let trace = operations(&home);
        assert_eq!(trace.iter().filter(|op| *op == "connect").count(), 1);
        assert_eq!(
            trace
                .iter()
                .filter(|op| *op == "write_action_device_key_until")
                .count(),
            1,
            "mode={mode}; error={error}; trace={trace:?}"
        );
        assert_eq!(trace.iter().filter(|op| *op == "schedule").count(), 0);
        assert_eq!(
            rows(&home)
                .iter()
                .filter(|row| row["kind"] == "sample")
                .count(),
            0
        );
        assert_worker_reaped(&home);
    }
}

#[test]
fn already_saved_capture_drains_after_pinned_schedule_failure() {
    let (home, mut command) = configured("industrial_key_saved_error", 1000);
    declarations(&home, false, time::ms(2000));
    let window = time::arg(3000);
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &window,
        "-o",
        "run.jsonl",
    ];
    let error = Run::of_output(command.args(args).output().unwrap(), &args).failure(1);
    assert_eq!(error["code"], "failed");
    let samples = rows(&home)
        .into_iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 1);
    assert_eq!(
        fs::read(samples[0]["value"]["path"].as_str().unwrap()).unwrap(),
        [7]
    );
    assert_eq!(
        operations(&home)
            .iter()
            .filter(|op| *op == "schedule")
            .count(),
        1
    );
    assert_worker_reaped(&home);
}

#[test]
fn key_conflicts_are_usage_before_an_earlier_serial_source_can_open() {
    let home = Home::new("industrial-key-config");
    for field in [
        json!({"action_device_key":18}),
        json!({"settings":["ActionDeviceKey=17"]}),
        json!({"settings":["ActionDeviceKey[1]=17"]}),
    ] {
        let mut sibling = json!({"id":"b","protocol":"video","camera":"sim:0","output":"clip.mkv"});
        let Value::Object(fields) = field else {
            panic!("fixture fields must be an object");
        };
        sibling.as_object_mut().unwrap().extend(fields);
        fs::write(home.path("sources.json"),json!({"version":1,"sources":[
            {"id":"serial","protocol":"modbus_rtu","device":"does-not-exist","baud":9600,"unit":1,"signals":[{"name":"x","address":0}]},
            {"id":"a","protocol":"camera","camera":"sim:0","output":"frame.raw","action_device_key":17},sibling
        ]}).to_string()).unwrap();
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
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("clip.mkv").exists());
        assert!(!home.path("frame.raw").exists());
    }
}
