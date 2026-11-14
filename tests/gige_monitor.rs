//! Passive `GigE` monitors receive an independent externally running multicast stream.
#![cfg(unix)]
#[path = "support/gige_monitor.rs"]
mod fixture;
mod support;

use fixture::{PIXELS, Producer, Scenario, Sessions, run};
use serde_json::{Value, json};
use std::{fs, sync::Mutex};
use support::cli::Home;

// The independent bootstrap peer deliberately owns the standard GVCP port.
static SERIAL: Mutex<()> = Mutex::new(());

fn saved(home: &Home, camera: &str, session: Option<&str>, file: &str) -> Value {
    let mut args = vec!["--json"];
    if let Some(session) = session {
        args.extend(["--session", session]);
    }
    args.extend(["capture", "-c", camera, "-o", file, "--timeout", "1s"]);
    let value = run(home, &args).success();
    assert_eq!(value["count"], 1, "{value}");
    assert_eq!(value["files"], json!([home.real(file)]));
    assert!(value.get("warmup_ms").is_none(), "{value}");
    assert!(value.get("settled").is_none(), "{value}");
    value
}
fn raw(home: &Home, file: &str) {
    assert_eq!(fs::read(home.path(file)).unwrap(), PIXELS);
    let metadata: Value =
        serde_json::from_slice(&fs::read(home.path(&format!("{file}.json"))).unwrap()).unwrap();
    assert_eq!(metadata["width"], 4);
    assert_eq!(metadata["height"], 2);
    assert_eq!(metadata["row_bytes"], 4);
    assert_eq!(metadata["bytes"], 8);
    assert_eq!(metadata["pixel_format"], "Mono8");
    assert_eq!(metadata["pfnc_code"], 0x0108_0001);
    assert_eq!(metadata["device_ts_ns"], 1234);
    assert!(metadata["frame_id"].as_u64().unwrap() >= 42);
}
fn png(home: &Home, file: &str) {
    let bytes = fs::read(home.path(file)).unwrap();
    let mut reader = png::Decoder::new(bytes.as_slice()).read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut pixels).unwrap();
    assert_eq!((info.width, info.height), (4, 2));
    assert_eq!(
        (info.color_type, info.bit_depth),
        (png::ColorType::Grayscale, png::BitDepth::Eight)
    );
    assert_eq!(&pixels[..info.buffer_size()], &PIXELS);
}

#[test]
fn frozen_wire_packets_have_independent_complete_native_pixels() {
    let packets = fixture::literals();
    assert_eq!(packets.len(), 3);
    assert_eq!(
        packets.iter().map(Vec::len).collect::<Vec<_>>(),
        [44, 16, 16]
    );
    assert_eq!(&packets[1][8..], &PIXELS);
    assert_eq!(&packets[0][20..32], &[1, 8, 0, 1, 0, 0, 0, 4, 0, 0, 0, 2]);
    assert_eq!(&packets[2][12..], &2_u32.to_be_bytes());
}

#[test]
fn two_monitors_share_one_home_capture_exact_pixels_and_leave_controller_running() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let producer = Producer::new(Scenario::Frames);
    assert_ne!(producer.destination_port, producer.source_port);
    let home = Home::new("gige-monitor-coexist");
    let camera = Producer::selector();
    let mut sessions = Sessions::new(&home);
    sessions.start("monitor-a", &camera);
    sessions.start("monitor-b", &camera);
    let info = run(
        &home,
        &["--json", "--session", "monitor-a", "info", "-c", &camera],
    )
    .success();
    assert_eq!(info["camera"], camera);
    assert_eq!(info["camera_access"], "read_only_monitor");
    assert_eq!(info["acquisition_control"], "external");
    assert_eq!(info["vendor"], "Independent");
    assert_eq!(info["model"], "ExternalController");
    assert_eq!(info["width"], 4);
    assert_eq!(info["height"], 2);
    let values = run(
        &home,
        &[
            "--json",
            "--session",
            "monitor-a",
            "get",
            "-c",
            &camera,
            "Width",
            "ControllerPrivilege",
        ],
    )
    .success();
    assert_eq!(values["values"]["Width"], 4);
    assert_eq!(values["values"]["ControllerPrivilege"], 2);
    let features = run(
        &home,
        &[
            "--json",
            "--session",
            "monitor-a",
            "features",
            "--all",
            "-c",
            &camera,
        ],
    )
    .success();
    let rows = features["features"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|row| row["name"] == "Width" && row["access"] == "rw")
    );
    assert!(
        rows.iter()
            .any(|row| row["name"] == "AcquisitionStart" && row["access"] == "wo")
    );
    for row in rows {
        assert_eq!(row["writable"], false, "{row}");
    }
    saved(&home, &camera, Some("monitor-a"), "first.raw");
    raw(&home, "first.raw");
    saved(&home, &camera, Some("monitor-b"), "second.png");
    png(&home, "second.png");
    let before = producer.snapshot();
    assert!(
        before
            .reads
            .iter()
            .filter(|(address, length)| *address == 0 && *length == 0xf8)
            .count()
            >= 4
    );
    sessions.stop("monitor-a");
    producer.wait_after(before.ticks);
    saved(&home, &camera, Some("monitor-b"), "after-first-exit.raw");
    raw(&home, "after-first-exit.raw");
    sessions.stop("monitor-b");
    let before = producer.snapshot();
    producer.wait_after(before.ticks);
    assert!(producer.snapshot().emitted > before.emitted);
    producer.assert_passive();
    let listed = run(&home, &["--json", "session", "list"]).success();
    assert_eq!(listed["sessions"], json!([]));
}

#[test]
fn forbidden_mutation_and_malformed_selectors_are_rejected_before_even_bootstrap_reads() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let producer = Producer::new(Scenario::Frames);
    let home = Home::new("gige-monitor-static");
    let camera = Producer::selector();
    for words in [
        vec!["capture", "--set", "Width=4", "-o", "denied.raw"],
        vec!["capture", "--auto", "-o", "denied.raw"],
        vec!["capture", "--warmup", "1ms", "-o", "denied.raw"],
        vec!["capture", "--fresh", "1", "-o", "denied.raw"],
        vec!["record", "--auto", "-o", "denied.mkv"],
        vec!["record", "--warmup", "1ms", "-o", "denied.mkv"],
        vec!["inspect", "--set", "Width=4"],
        vec!["set", "Width=4", "--dry-run"],
        vec!["exec", "AcquisitionStart"],
        vec!["auto", "--off"],
        vec!["get", "TriggerMode[TriggerSelector=FrameStart]"],
        vec![
            "debug",
            "read",
            "--feature",
            "Buffer[TriggerSelector=FrameStart]",
        ],
        vec!["debug", "write", "0x11000", "00000004"],
        vec!["preset", "save", "blocked"],
        vec!["preset", "apply", "blocked", "--dry-run"],
    ] {
        let before = producer.snapshot().commands;
        let args = [vec!["--json"], words, vec!["-c", camera.as_str()]].concat();
        let error = run(&home, &args).failure(5);
        assert_eq!(error["code"], "unsupported", "{args:?}: {error}");
        assert_eq!(
            producer.snapshot().commands,
            before,
            "preflight sent I/O: {args:?}"
        );
        assert!(!home.path("denied.raw").exists());
        assert!(!home.path("denied.raw.json").exists());
        assert!(!home.path("denied.mkv").exists());
    }
    for selector in [
        "gige-monitor:",
        "gige-monitor://127.0.0.1",
        "gige-monitor://127.0.0.1:3956?interface=127.0.0.1&mac=020000000001",
        "gige-monitor://127.0.0.1/path?interface=127.0.0.1&mac=020000000001",
        "gige-monitor://127.0.0.1?interface=127.0.0.1&mac=020000000001&mac=020000000002",
        "gige-monitor://127.0.0.1?interface=127.0.0.1&mac=020000000001&unknown=1",
    ] {
        let before = producer.snapshot().commands;
        let error = run(&home, &["--json", "info", "-c", selector]).failure(2);
        assert_eq!(error["code"], "usage");
        assert_eq!(producer.snapshot().commands, before, "{selector}");
    }
    assert_eq!(producer.snapshot().commands, [] as [u16; 0]);
    producer.assert_passive();
}

#[test]
fn bad_identity_or_stream_configuration_never_falls_back_to_control_access() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (scenario, wrong_mac, expected_exit, expected_code) in [
        (Scenario::Frames, true, 1, "incomplete"),
        (Scenario::Unicast, false, 5, "unsupported"),
        (Scenario::ZeroSource, false, 5, "unsupported"),
        (Scenario::NoCapability, false, 5, "unsupported"),
    ] {
        let producer = Producer::new(scenario);
        let home = Home::new("gige-monitor-bad-bootstrap");
        let camera = if wrong_mac {
            "gige-monitor://127.0.0.1?interface=127.0.0.1&mac=020000000002".into()
        } else {
            Producer::selector()
        };
        let error = run(
            &home,
            &[
                "--json",
                "capture",
                "-c",
                &camera,
                "-o",
                "never.raw",
                "--timeout",
                "500ms",
            ],
        )
        .failure(expected_exit);
        assert_eq!(error["code"], expected_code, "{scenario:?}: {error}");
        assert!(!home.path("never.raw").exists());
        assert!(!home.path("never.raw.json").exists());
        let before = producer.snapshot();
        assert!(before.commands.contains(&0x0084));
        producer.wait_after(before.ticks);
        producer.assert_passive();
    }
}

#[test]
fn selected_malformed_packets_and_silent_stream_keep_external_controller_unchanged() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (scenario, expected_exit, expected_code) in [
        (Scenario::Malformed, 1, "incomplete"),
        (Scenario::NoFrames, 4, "frame_timeout"),
    ] {
        let producer = Producer::new(scenario);
        let home = Home::new("gige-monitor-receive-errors");
        let camera = Producer::selector();
        let error = run(
            &home,
            &[
                "--json",
                "capture",
                "-c",
                &camera,
                "-o",
                "never.raw",
                "--timeout",
                "500ms",
            ],
        )
        .failure(expected_exit);
        assert_eq!(error["code"], expected_code, "{scenario:?}: {error}");
        assert!(!home.path("never.raw").exists());
        assert!(!home.path("never.raw.json").exists());
        let before = producer.snapshot();
        assert!(before.reads.iter().any(|(address, _)| *address == 0xd1c));
        producer.wait_after(before.ticks);
        producer.assert_passive();
    }
}
