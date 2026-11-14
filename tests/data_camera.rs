//! Camera acquisition cutoffs and delayed file delivery through the real CLI/coordinator.
#![cfg(unix)]
#[path = "support/industrial_action_key.rs"]
mod industrial_action_key;
mod support;

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead as _, Write as _},
    os::unix::fs::PermissionsExt as _,
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use support::cli::{Home, Run};

/// The test executable also acts as a controllable camera worker. The shell
/// wrapper selects just this test; no interpreter or second compiled fixture is
/// needed. Its real worker protocol makes the product coordinator own all job
/// waiting, saved-file observation, cutoff handling, and process cleanup.
#[test]
fn fixture_worker() {
    let Ok(mode) = std::env::var("CAPTUREFAB_DATA_CAMERA_FIXTURE_MODE") else {
        return;
    };
    fs::write("fixture-worker.pid", std::process::id().to_string()).unwrap();
    if mode.starts_with("industrial_key_") {
        industrial_action_key::worker(&mode);
    }
    let mut output = std::io::stdout();
    // Terminate any test-harness prefix before the worker's first JSON line.
    output.write_all(b"\n").unwrap();
    let mut sequence = 0;
    let mut emit = |mut value: Value| {
        value["seq"] = json!(sequence);
        sequence += 1;
        serde_json::to_writer(&mut output, &value).unwrap();
        output.write_all(b"\n").unwrap();
        output.flush().unwrap();
    };
    emit(json!({"event":"hello","protocol":10,"build":"delayed-camera-fixture"}));
    let mut pending = None;
    let mut retired = false;
    let generation = if mode == "retirement" {
        let generation = fs::read_to_string("fixture-generation")
            .ok()
            .map_or(1, |value| value.parse::<u32>().unwrap() + 1);
        fs::write("fixture-generation", generation.to_string()).unwrap();
        generation
    } else {
        0
    };
    let identity = json!({"id":"sim:0","transport":"simulator","vendor":"Fixture","model":"Delayed Camera","serial":"delay",
        "profile":{"kind":"area_scan","source":"identity","evidence":[]}});
    let retirement = json!({"code":"disconnected","message":"first ownership lease expired",
        "hint":"connect a fresh camera binding with capturefab session start --name ownership -c sim:0",
        "details":{"controller_ownership":"retired","reason":"lease_expired"}});
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match request["op"].as_str().unwrap() {
            "shutdown" => std::process::exit(0),
            "ping" => emit(
                json!({"event":"reply","id":request["id"],"ok":true,"result":{"heartbeat_age_ms":0}}),
            ),
            "status" => emit(
                json!({"event":"reply","id":request["id"],"ok":true,"result":{"connected":identity,"streaming":false,"frames":0,"dropped":0,"fps":0.0}}),
            ),
            "run" => {
                let id = request["id"].as_u64().unwrap();
                let command = &request["command"];
                emit(json!({"event":"accepted","id":id}));
                emit(json!({"event":"started","id":id,"budget_ms":60000}));
                match command["op"].as_str().unwrap() {
                    "check_connection" if retired => {
                        emit(json!({"event":"reply","id":id,"ok":false,"error":retirement}));
                    }
                    "connect" | "check_connection" => emit(
                        json!({"event":"reply","id":id,"ok":true,"result":{"connected":identity}}),
                    ),
                    "disconnect" => {
                        emit(
                            json!({"event":"reply","id":id,"ok":true,"result":{"connected":null}}),
                        );
                    }
                    "info" if mode == "retirement" && generation == 1 => {
                        retired = true;
                        emit(json!({"event":"retired","error":retirement}));
                        emit(json!({"event":"reply","id":id,"ok":false,"error":retirement}));
                    }
                    "info" if mode == "retirement" => emit(
                        json!({"event":"reply","id":id,"ok":true,"result":{"camera":"sim:0","generation":generation}}),
                    ),
                    "schedule" => {
                        let job = command["job"].as_u64().unwrap();
                        if mode == "multipart" {
                            assert_eq!(command["part_format"], "Coord3D_ABC32f");
                            assert_eq!(command["format"], "raw");
                        }
                        let mut observed = monotonic_ns();
                        let wall_ms = u64::try_from(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis(),
                        )
                        .unwrap();
                        let reply = json!({"event":"reply","id":id,"ok":true,"result":{"job":{"id":job,"captured":0}}});
                        if mode == "multipart_pair" {
                            if let Some(previous) = pending.take() {
                                // Both jobs are waiting on the same frame. The
                                // second publishes before its schedule reply.
                                save_pair_capture(command, observed, wall_ms, &mut emit);
                                emit(reply);
                                save_pair_capture(&previous, observed, wall_ms, &mut emit);
                            } else {
                                pending = Some(command.clone());
                                emit(reply);
                            }
                            continue;
                        }
                        emit(reply);
                        match mode.as_str() {
                            "delayed" => std::thread::sleep(Duration::from_millis(1250)),
                            "hung_after_first" if job > 1 => {
                                std::thread::sleep(Duration::from_secs(30));
                            }
                            "late" => {
                                std::thread::sleep(Duration::from_millis(1250));
                                observed = monotonic_ns();
                            }
                            _ => {}
                        }
                        let path = command["output"]
                            .as_str()
                            .unwrap()
                            .replace("{seq}", &format!("{job:04}"));
                        fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
                        let (pixel_format, pixels) = if mode == "multipart" {
                            (0x0260_00c0, vec![0; 12])
                        } else {
                            (capturefab::testing::types::MONO8, vec![7])
                        };
                        let frame = capturefab::testing::types::Frame {
                            pixel_encoding: None,
                            width: 1,
                            height: 1,
                            pixel_format,
                            data: pixels.into(),
                            ..Default::default()
                        };
                        let format = command["format"].as_str().unwrap();
                        let bytes = capturefab::testing::frame::encode(&frame, format).unwrap();
                        let mut file = fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)
                            .unwrap();
                        file.write_all(&bytes).unwrap();
                        file.sync_all().unwrap();
                        if mode == "recording_first" {
                            fs::write("fixture-video.mkv", b"fixture").unwrap();
                            emit(json!({"event":"saved","file":{
                                "kind":"recording","wall_ms":wall_ms,
                                "path":"fixture-video.mkv","bytes":7
                            }}));
                        }
                        let mut saved = json!({"event":"saved","job":job,"file":{
                            "kind":"capture","wall_ms":wall_ms,"received_monotonic_ns":observed,
                            "path":path,"bytes":bytes.len(),"frame_id":job,"device_ts_ns":job*1_000_000,
                            "width":1,"height":1,"pixel_format":pixel_format
                        }});
                        if mode == "multipart" {
                            let archive = format!("{path}.multipart.raw");
                            let metadata = format!("{archive}.json");
                            fs::write(&archive, &bytes).unwrap();
                            fs::write(&metadata, b"{}").unwrap();
                            saved["file"]["multipart"] = json!({"path":archive,"metadata_path":metadata,"bytes":bytes.len()});
                        }
                        emit(saved);
                        emit(
                            json!({"event":"job_end","job":job,"status":"done","summary":{"captured":1}}),
                        );
                    }
                    operation => panic!("unexpected fixture command: {operation}"),
                }
            }
            operation => panic!("unexpected fixture request: {operation}"),
        }
    }
    std::process::exit(0);
}

fn save_pair_capture(command: &Value, observed: u64, wall_ms: u64, emit: &mut impl FnMut(Value)) {
    assert_eq!(command["format"], "raw");
    let archive_bytes = [
        1, 0, 0x80, 0x3f, 0, 0, 0, 0x80, 0x34, 0x12, 0xc0, 0x7f, 0x34, 0x12,
    ];
    let (part, pixel_format, pixels) = match command["part_format"].as_str().unwrap() {
        "Coord3D_ABC32f" => (0, 0x0260_00c0, &archive_bytes[..12]),
        "Confidence16" => (1, 0x0110_00c7, &archive_bytes[12..]),
        format => panic!("unexpected multipart format {format}"),
    };
    let job = command["job"].as_u64().unwrap();
    let path = command["output"]
        .as_str()
        .unwrap()
        .replace("{seq}", &format!("{job:04}"));
    fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    fs::write(&path, pixels).unwrap();
    let archive = format!("{path}.multipart.raw");
    let metadata = format!("{archive}.json");
    fs::write(&archive, archive_bytes).unwrap();
    fs::write(
        &metadata,
        json!({"multipart":{"selected":part}}).to_string(),
    )
    .unwrap();
    emit(json!({"event":"saved","job":job,"file":{
        "kind":"capture","wall_ms":wall_ms,"received_monotonic_ns":observed,
        "path":path,"bytes":pixels.len(),"frame_id":71,"device_ts_ns":123_456,
        "device_timestamp":{"ticks":9_007_199_254_740_993u64},
        "width":1,"height":1,"pixel_format":pixel_format,
        "multipart":{"path":archive,"metadata_path":metadata,"bytes":archive_bytes.len(),"selected":part}
    }}));
    emit(json!({"event":"job_end","job":job,"status":"done","summary":{"captured":1}}));
}

fn monotonic_ns() -> u64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes this valid timespec on success.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) };
    assert_eq!(result, 0);
    // SAFETY: the successful clock_gettime initialized the value.
    let time = unsafe { time.assume_init() };
    u64::try_from(time.tv_sec).unwrap() * 1_000_000_000 + u64::try_from(time.tv_nsec).unwrap()
}

fn configured(mode: &str, every_ms: u64) -> (Home, Command) {
    let home = Home::new("data-camera-delivery");
    let worker = home.path("worker.sh");
    fs::write(&worker,b"#!/bin/sh\nexec \"$CAPTUREFAB_DATA_CAMERA_FIXTURE_EXE\" --exact fixture_worker --nocapture\n").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[
        {"id":"sensor","protocol":"simulator","signal":"cycle","every_ms":10},
        {"id":"camera","protocol":"camera","camera":"sim:0","output":"frames/{seq}.png","every_ms":every_ms,"timeout_ms":2000}
    ]}).to_string()).unwrap();
    let mut command = home.command();
    command
        .env("CAPTUREFAB_EXECUTABLE", &worker)
        .env(
            "CAPTUREFAB_DATA_CAMERA_FIXTURE_EXE",
            std::env::current_exe().unwrap(),
        )
        .env("CAPTUREFAB_DATA_CAMERA_FIXTURE_MODE", mode);
    (home, command)
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn saved_frames(home: &Home) -> usize {
    fs::read_dir(home.path("frames")).unwrap().count()
}
fn assert_worker_reaped(home: &Home) {
    let pid = fs::read_to_string(home.path("fixture-worker.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert!(
        !support::cli::process_lives(pid),
        "fixture worker must be reaped"
    );
}

#[test]
fn multipart_selection_and_archive_references_reach_the_industrial_timeline() {
    let (home, mut command) = configured("multipart", 5000);
    let mut config: Value =
        serde_json::from_slice(&fs::read(home.path("sources.json")).unwrap()).unwrap();
    config["sources"][1]["part_format"] = json!("Coord3D_ABC32f");
    config["sources"][1]["output"] = json!("frames/{seq}.raw");
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "1s",
        "-o",
        "run.jsonl",
    ];
    let result = Run::of_output(command.args(args).output().unwrap(), &args).success();
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    assert!(rows.iter().any(|row| row["source"] == "sensor"));
    let image = rows.iter().find(|row| row["source"] == "camera").unwrap();
    let value = &image["value"];
    assert_eq!(value["pixel_format"], "Coord3D_ABC32f");
    assert_eq!(value["bytes"], 12);
    let archive = &value["multipart"];
    assert_eq!(archive["bytes"], 12);
    assert_eq!(
        fs::read(archive["path"].as_str().unwrap()).unwrap(),
        [0; 12]
    );
    assert!(std::path::Path::new(archive["metadata_path"].as_str().unwrap()).is_file());
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "frames.jsonl",
        "--format",
        "jsonl",
        "--source",
        "camera",
    ]);
    let exported: Value = serde_json::from_str(
        fs::read_to_string(home.path("frames.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(exported["value"]["multipart"], *archive);
    assert_worker_reaped(&home);
}

#[test]
fn same_camera_routes_overlapping_multipart_jobs_to_their_sources() {
    let (home, mut command) = configured("multipart_pair", 5000);
    fs::write(home.path("sources.json"), json!({"version":1,"sources":[
        {"id":"range","protocol":"camera","camera":"sim:0","output":"frames/range-{seq}.raw",
         "part_format":"Coord3D_ABC32f","every_ms":5000,"timeout_ms":2000},
        {"id":"confidence","protocol":"camera","camera":"sim:0","output":"frames/confidence-{seq}.raw",
         "part_format":"Confidence16","every_ms":5000,"timeout_ms":2000}
    ]}).to_string()).unwrap();
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "1s",
        "-o",
        "run.jsonl",
    ];
    let result = Run::of_output(command.args(args).output().unwrap(), &args).success();
    assert_eq!(result["status"], "complete");
    assert_eq!(result["samples"], 2);
    let samples = rows(&home);
    let range = samples.iter().find(|row| row["source"] == "range").unwrap();
    let confidence = samples
        .iter()
        .find(|row| row["source"] == "confidence")
        .unwrap();
    assert_eq!(range["elapsed_ns"], confidence["elapsed_ns"]);
    assert_eq!(range["time"], confidence["time"]);
    assert!(range["elapsed_ns"].as_u64().is_some_and(|ns| ns > 0));
    let expected_archive = [
        1, 0, 0x80, 0x3f, 0, 0, 0, 0x80, 0x34, 0x12, 0xc0, 0x7f, 0x34, 0x12,
    ];
    for (sample, source, part, format, pixels) in [
        (range, "range", 0, "Coord3D_ABC32f", &expected_archive[..12]),
        (
            confidence,
            "confidence",
            1,
            "Confidence16",
            &expected_archive[12..],
        ),
    ] {
        let value = &sample["value"];
        let path = std::path::Path::new(value["path"].as_str().unwrap());
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(source)
        );
        assert_eq!(fs::read(path).unwrap(), pixels);
        assert_eq!(value["pixel_format"], format);
        assert_eq!(value["frame_id"], 71);
        assert_eq!(value["device_ts_ns"], 123_456);
        assert_eq!(value["device_timestamp"]["ticks"], 9_007_199_254_740_993u64);
        let archive = &value["multipart"];
        assert_eq!(archive["selected"], part);
        assert_eq!(
            fs::read(archive["path"].as_str().unwrap()).unwrap(),
            expected_archive
        );
        let metadata: Value =
            serde_json::from_slice(&fs::read(archive["metadata_path"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(metadata["multipart"]["selected"], part);
    }
    assert_worker_reaped(&home);
}

#[test]
fn a_frame_received_before_cutoff_is_reported_after_delayed_file_delivery() {
    let (home, mut command) = configured("delayed", 100);
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "1s",
        "-o",
        "run.jsonl",
    ];
    let result = Run::of_output(command.args(args).output().unwrap(), &args).success();
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    let images = rows
        .iter()
        .filter(|row| row["source"] == "camera")
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 1);
    let image = images[0];
    assert!(
        image["elapsed_ns"].as_u64().unwrap() < 1_000_000_000,
        "frame received before acquisition cutoff: {image}"
    );
    assert!(
        image["delivery_latency_ns"].as_u64().unwrap() >= 1_200_000_000,
        "file delivered after cutoff: {image}"
    );
    assert_eq!(saved_frames(&home), images.len());
    assert!(
        fs::metadata(image["value"]["path"].as_str().unwrap())
            .unwrap()
            .len()
            > 0
    );
    assert_eq!(rows.last().unwrap()["status"], "complete");
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "frames.jsonl",
        "--format",
        "jsonl",
        "--source",
        "camera",
    ]);
    assert_worker_reaped(&home);
}

#[test]
fn a_camera_timeout_after_a_successful_frame_cannot_be_reported_complete() {
    // Leave startup headroom so this exercises the second delivery timeout,
    // rather than expiring while the test worker executable is being loaded.
    let (home, mut command) = configured("hung_after_first", 100);
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "2s",
        "-o",
        "run.jsonl",
    ];
    let started = Instant::now();
    let error = Run::of_output(command.args(args).output().unwrap(), &args).failure(4);
    assert!(
        started.elapsed() < support::time::secs(9),
        "delivery timeout remains bounded"
    );
    assert_eq!(error["code"], "timeout");
    assert_eq!(error["details"]["status"], "failed");
    let rows = rows(&home);
    assert_eq!(
        rows.iter().filter(|row| row["source"] == "camera").count(),
        1
    );
    assert_eq!(saved_frames(&home), 1);
    assert_eq!(rows.last().unwrap()["status"], "failed");
    assert_worker_reaped(&home);
}

#[test]
fn a_frame_received_after_cutoff_reports_its_saved_file_as_incomplete() {
    let (home, mut command) = configured("late", 100);
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "1s",
        "-o",
        "run.jsonl",
    ];
    let error = Run::of_output(command.args(args).output().unwrap(), &args).failure(1);
    assert_eq!(error["code"], "incomplete");
    assert_eq!(error["details"]["status"], "failed");
    assert!(
        fs::metadata(error["details"]["late_file"].as_str().unwrap())
            .unwrap()
            .len()
            > 0
    );
    let rows = rows(&home);
    assert_eq!(
        rows.iter().filter(|row| row["source"] == "camera").count(),
        0
    );
    assert_eq!(saved_frames(&home), 1);
    assert_eq!(rows.last().unwrap()["status"], "failed");
    assert_worker_reaped(&home);
}

#[test]
fn finalized_video_metadata_does_not_replace_a_pending_still_observation() {
    let (home, mut command) = configured("recording_first", 3_600_000);
    let args = [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "2s",
        "-o",
        "run.jsonl",
    ];
    let result = Run::of_output(command.args(args).output().unwrap(), &args).success();
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    let images = rows
        .iter()
        .filter(|row| row["source"] == "camera")
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 1);
    assert!(
        images[0]["value"]["path"]
            .as_str()
            .unwrap()
            .ends_with(".png")
    );
    assert_eq!(
        fs::read(home.path("fixture-video.mkv")).unwrap(),
        b"fixture"
    );
    assert_eq!(saved_frames(&home), 1);
    assert_worker_reaped(&home);
}

#[test]
fn explicit_session_start_recovers_a_retired_warm_binding() {
    let (home, mut command) = configured("retirement", 5000);
    // Session start re-execs through the override too. Keep the real serve
    // process and substitute only its isolated __worker child.
    fs::write(home.path("worker.sh"), b"#!/bin/sh\nif [ \"$1\" = \"__worker\" ]; then\n  exec \"$CAPTUREFAB_DATA_CAMERA_FIXTURE_EXE\" --exact fixture_worker --nocapture\nfi\nexec \"$CAPTUREFAB_DATA_CAMERA_BINARY\" \"$@\"\n").unwrap();
    command.env(
        "CAPTUREFAB_DATA_CAMERA_BINARY",
        env!("CARGO_BIN_EXE_capturefab"),
    );
    let args = [
        "--json",
        "session",
        "start",
        "--name",
        "ownership",
        "-c",
        "sim:0",
    ];
    Run::of_output(command.args(args).output().unwrap(), &args).success();
    home.ok(&["session", "status", "ownership"]);
    let before_pid = fs::read_to_string(home.path("fixture-worker.pid"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let error = home.fails(3, &["--session", "ownership", "info", "-c", "sim:0"]);
    assert_eq!(error["code"], "disconnected");
    assert_eq!(error["details"]["controller_ownership"], "retired");
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("session start --name ownership -c sim:0")
    );
    // The normal warm path touches its descriptor; it must keep the same
    // terminal binding, never silently make a fresh worker or query.
    let again = home.fails(3, &["--session", "ownership", "info", "-c", "sim:0"]);
    assert_eq!(again, error);
    assert_eq!(
        fs::read_to_string(home.path("fixture-generation")).unwrap(),
        "1"
    );
    home.ok(&["session", "start", "--name", "ownership", "-c", "sim:0"]);
    home.ok(&["session", "status", "ownership"]);
    let after_pid = fs::read_to_string(home.path("fixture-worker.pid"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert_ne!(after_pid, before_pid);
    assert!(!support::cli::process_lives(before_pid));
    let value = home.ok(&["--session", "ownership", "info", "-c", "sim:0"]);
    assert_eq!(value["generation"], 2);
    assert_eq!(
        fs::read_to_string(home.path("fixture-generation")).unwrap(),
        "2"
    );
    home.ok(&["session", "stop", "ownership"]);
    assert!(!support::cli::process_lives(after_pid));
}
