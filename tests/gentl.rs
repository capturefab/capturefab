//! Actual CLI/worker/helper capture against an independent minimal C producer.
#![cfg(all(feature = "gentl", unix))]
#[path = "support/industrial_action_key_gentl.rs"]
mod industrial_action_key_gentl;
mod support;
#[path = "support/video_startup.rs"]
mod video_startup;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use support::{
    cli::{Home, Run},
    time,
};

fn producer(directory: &Path, mode: u8) -> PathBuf {
    let path = directory.join(format!("mock-{mode}.cti"));
    let mut compiler = Command::new("cc");
    compiler.arg(if cfg!(target_os = "macos") {
        "-dynamiclib"
    } else {
        "-shared"
    });
    let output = compiler
        .arg("-fPIC")
        .arg("-O2")
        .arg(format!("-DMODE={mode}"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/gentl.c"))
        .arg("-o")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    path.canonicalize().unwrap()
}
fn selector(path: &Path) -> String {
    format!(
        "gentl:{}",
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "producer":path,"interface":"if/0;?test","device":"device:0/test"
            }))
            .unwrap()
        )
    )
}
fn component_selector(path: &Path, part: Option<u32>) -> String {
    let encoded = selector(path);
    let mut value: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded.strip_prefix("gentl:").unwrap())
            .unwrap(),
    )
    .unwrap();
    if let Some(part) = part {
        value["part"] = json!(part);
    }
    format!(
        "gentl:{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
    )
}
fn map_selector(path: &Path, part: u32, expected_format: u32) -> String {
    let id = component_selector(path, Some(part));
    let mut selector: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(id.strip_prefix("gentl:").unwrap())
            .unwrap(),
    )
    .unwrap();
    selector["part_format"] = json!(expected_format);
    format!(
        "gentl:{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&selector).unwrap())
    )
}

fn multipart_bytes(wide: bool, two_images: bool) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0x80, 0x3f, 0, 0, 0, 0x40, 0, 0, 0x40, 0x40];
    bytes.extend(1..=if wide { 12 } else { 6 });
    bytes.extend_from_slice(if two_images {
        &[9, 8, 7, 6, 5, 4]
    } else {
        &[9, 8]
    });
    bytes
}
fn run(home: &Home, paths: &[PathBuf], args: &[&str]) -> Run {
    let output = home
        .command()
        .env_remove("GENICAM_GENTL64_PATH")
        .env(
            "CAPTUREFAB_GENTL_PRODUCERS",
            std::env::join_paths(paths).unwrap(),
        )
        .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
        .env("CAPTUREFAB_GENTL_MOCK_LOCK", home.root.join("sdk.lock"))
        .args(args)
        .output()
        .unwrap();
    Run::of_output(output, args)
}
#[test]
fn write_only_action_key_uses_one_held_native_producer_without_readback_or_reload() {
    // Existing modes end at198;199 is dedicated to the actual helper/port path.
    let home = Home::new("gentl-action-key");
    let cti = producer(&home.root, 199);
    let camera = selector(&cti);
    run(
        &home,
        std::slice::from_ref(&cti),
        &[
            "--json",
            "session",
            "start",
            "--name",
            "action-key",
            "-c",
            &camera,
        ],
    )
    .success();
    let warm = |args: &[&str]| {
        Run::of_output(
            home.warm_command()
                .env_remove("GENICAM_GENTL64_PATH")
                .env("CAPTUREFAB_GENTL_PRODUCERS", &cti)
                .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
                .env("CAPTUREFAB_GENTL_MOCK_LOCK", home.root.join("sdk.lock"))
                .args(args)
                .output()
                .unwrap(),
            args,
        )
    };
    // The ordinary transaction still refuses a WO checkpoint before mutation.
    warm(&[
        "--json",
        "--session",
        "action-key",
        "set",
        "-c",
        &camera,
        "ActionDeviceKey=287454020",
    ])
    .failure(1);
    assert!(
        !fs::read_to_string(home.root.join("sdk.log"))
            .unwrap()
            .contains("action_key_write")
    );
    let expected = json!({"camera":camera,"feature":"ActionDeviceKey","write_completed":true,
        "completion":"producer_reported","readback":"unavailable_write_only",
        "rollback":"not_available","scope":"current_control_epoch"});
    let assert_result = |mut result: Value| {
        let decode = |selector: &str| -> Value {
            serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(selector.strip_prefix("gentl:").unwrap())
                    .unwrap(),
            )
            .unwrap()
        };
        // The worker publishes the canonical encoding; JSON object field order
        // in the caller's equivalent selector does not change device identity.
        assert_eq!(decode(result["camera"].as_str().unwrap()), decode(&camera));
        result["camera"] = json!(camera);
        assert_eq!(result, expected);
    };
    for value in ["ActionDeviceKey=287454020", "ActionDeviceKey=2864434397"] {
        let result = warm(&[
            "--json",
            "--session",
            "action-key",
            "set",
            "--write-only",
            "--write-timeout",
            "1s",
            "-c",
            &camera,
            value,
        ])
        .success();
        assert_result(result);
    }
    let denied = warm(&[
        "--json",
        "--session",
        "action-key",
        "set",
        "--write-only",
        "--write-timeout",
        "1s",
        "-c",
        &camera,
        "ActionDeviceKey=16909060",
    ])
    .failure(3);
    assert_eq!(denied["code"], "permission_denied");
    assert_eq!(denied["details"]["gentl_error"], -1005);
    assert_eq!(denied["details"]["write_attempted"], true);
    assert_eq!(denied["details"]["write_may_have_applied"], true);
    let short = warm(&[
        "--json",
        "--session",
        "action-key",
        "set",
        "--write-only",
        "--write-timeout",
        "1s",
        "-c",
        &camera,
        "ActionDeviceKey=270544960",
    ])
    .failure(1);
    assert_eq!(short["code"], "failed");
    assert_eq!(short["details"]["write_may_have_applied"], true);
    let after = warm(&[
        "--json",
        "--session",
        "action-key",
        "set",
        "--write-only",
        "--write-timeout",
        "1s",
        "-c",
        &camera,
        "ActionDeviceKey=1432778632",
    ])
    .success();
    assert_result(after);
    // Producer entry is delayed2s, comfortably beyond the original500ms budget.
    let late = warm(&[
        "--json",
        "--session",
        "action-key",
        "set",
        "--write-only",
        "--write-timeout",
        "500ms",
        "-c",
        &camera,
        "ActionDeviceKey=3735928559",
    ])
    .failure(4);
    assert_eq!(late["code"], "timeout");
    assert!(
        late["details"]
            .get("write_attempted")
            .is_none_or(|attempted| attempted == true)
    );
    assert_eq!(late["details"]["write_may_have_applied"], true);
    // A coordinator cutoff may precede the worker's failure response. The
    // producer trace proves entry; wait only for this owned helper's retirement.
    let late_log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    let pid = late_log
        .lines()
        .find(|line| line.ends_with(" action_key_late"))
        .unwrap()
        .split_once(' ')
        .unwrap()
        .0
        .parse::<i32>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: signal zero probes only the recorded test-owned helper PID.
        if unsafe { libc::kill(pid, 0) } != 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed-out key helper {pid} was not reaped"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let gone = warm(&[
        "--json",
        "--session",
        "action-key",
        "set",
        "--write-only",
        "--write-timeout",
        "3s",
        "-c",
        &camera,
        "ActionDeviceKey=7",
    ])
    .failure(3);
    assert_eq!(gone["code"], "disconnected");
    assert_eq!(gone["details"]["write_attempted"], false);
    assert_eq!(gone["details"]["write_may_have_applied"], false);
    home.run(&["--json", "session", "stop", "action-key"])
        .success();
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    for event in [
        " init",
        " dev_open",
        " action_key_bytes_one",
        " action_key_bytes_two",
        " action_key_denied",
        " action_key_short",
        " action_key_bytes_after",
        " action_key_late",
    ] {
        assert_eq!(
            log.lines().filter(|line| line.ends_with(event)).count(),
            1,
            "{log}"
        );
    }
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" action_key_write"))
            .count(),
        6,
        "{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" action_key_error_read"))
            .count(),
        3,
        "{log}"
    );
    assert!(!log.contains("forbidden_action_key_read"), "{log}");
    assert!(!log.contains("wrong_action_key_bytes"), "{log}");
    assert!(!log.contains("producer_lock_busy"), "{log}");
}

#[test]
fn cti_discovery_and_capture_stay_isolated_and_follow_buffer_ownership() {
    let home = Home::new("gentl-image");
    let producer = producer(&home.root, 0);
    let cameras = run(
        &home,
        std::slice::from_ref(&producer),
        &["--json", "list", "--kind", "gentl", "--timeout", "2s"],
    )
    .success();
    let cameras = cameras["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 1);
    let camera = &cameras[0];
    assert_eq!(camera["vendor"], "Capturefab");
    assert_eq!(camera["serial"], "MOCK-001");
    let id = camera["id"].as_str().unwrap();
    let decode = |id: &str| -> Value {
        serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(id.strip_prefix("gentl:").unwrap())
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(decode(id), decode(&selector(&producer)));
    fs::remove_file(home.root.join("sdk.log")).unwrap();
    let output = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            id,
            "-o",
            "frame.raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    )
    .success();
    assert_eq!(output["count"], 1);
    // Requeue poisons the SDK buffer. The helper must finish sending the
    // actual bytes before it allows the producer to reuse that memory.
    assert_eq!(
        fs::read(home.work.join("frame.raw")).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
    let metadata: Value =
        serde_json::from_slice(&fs::read(home.work.join("frame.raw.json")).unwrap()).unwrap();
    assert_eq!(metadata["width"], 3);
    assert_eq!(metadata["height"], 2);
    assert_eq!(metadata["device_ts_ns"], 1_234_000);
    assert_eq!(metadata["device_timestamp"]["ticks"], 98_765);
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    let operations = log
        .lines()
        .map(|line| line.split_once(' ').unwrap().1)
        .collect::<Vec<_>>();
    assert_eq!(
        operations.iter().filter(|name| **name == "alloc").count(),
        2
    );
    assert_eq!(
        operations.iter().filter(|name| **name == "revoke").count(),
        2
    );
    for name in [
        "acq_start",
        "acq_stop",
        "ds_stop",
        "event_unregister",
        "ds_close",
        "dev_close",
        "if_close",
        "tl_close",
        "close_lib",
    ] {
        assert!(operations.contains(&name), "missing {name}: {log}");
    }
}
#[test]
fn stream_payload_size_uses_the_declared_source_after_locking() {
    for (mode, size_query, skipped_query) in [
        (105, " stream_payload_size\n", " remote_payload_size\n"),
        (106, " remote_payload_size\n", " stream_payload_size\n"),
        (109, " remote_payload_size\n", " stream_payload_size\n"),
    ] {
        let home = Home::new("gentl-stream-size");
        let producer = producer(&home.root, mode);
        capture_point_cloud(&home, &producer, &selector(&producer), "image.raw").success();
        assert_eq!(
            fs::read(home.work.join("image.raw")).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        assert!(!log.contains(skipped_query), "mode {mode}: {log}");
        assert_eq!(log.matches(" stream_defines_payload_size\n").count(), 1);
        let mut remaining = log.as_str();
        for operation in [
            " tl_lock\n",
            " stream_defines_payload_size\n",
            size_query,
            " alloc\n",
            " ds_start\n",
            " acq_start\n",
            " acq_stop\n",
            " ds_stop\n",
            " tl_unlock\n",
        ] {
            remaining = remaining
                .split_once(operation)
                .unwrap_or_else(|| {
                    panic!("mode {mode}, missing or out of order {operation}: {log}")
                })
                .1;
        }
        assert!(!log.contains(" stale_camera_state\n"), "{log}");
        assert!(!log.contains(" stale_buffer\n"), "{log}");
    }
}

#[test]
fn stream_payload_size_rejects_invalid_authoritative_sizes_before_allocation() {
    for (mode, message) in [
        (107, "GenTL stream payload size is zero or exceeds 128 MiB"),
        (108, "GenTL information type or size differs from its ABI"),
    ] {
        let home = Home::new("gentl-invalid-stream-size");
        let producer = producer(&home.root, mode);
        let error =
            capture_point_cloud(&home, &producer, &selector(&producer), "image.raw").failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        let (_, after_size) = log.split_once(" stream_payload_size\n").unwrap();
        assert!(after_size.contains(" tl_unlock\n"), "{log}");
        for unexpected in [
            " remote_payload_size\n",
            " alloc\n",
            " unexpected_alloc\n",
            " acq_start\n",
            " stale_camera_state\n",
        ] {
            assert!(!log.contains(unexpected), "mode {mode}: {log}");
        }
        assert!(fs::read_dir(&home.work).unwrap().next().is_none());
    }
}

#[test]
fn serial_lookup_retires_discovery_before_opening_the_selected_producer() {
    let home = Home::new("gentl-serial-lifetime");
    let producer = producer(&home.root, 36);
    run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            "MOCK-001",
            "-o",
            "serial.raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    )
    .success();
    assert_eq!(
        fs::read(home.work.join("serial.raw")).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert!(
        !log.lines()
            .any(|line| line.ends_with(" producer_lock_busy")),
        "acquisition must wait for discovery to release the producer: {log}"
    );
    let mut owners = BTreeSet::new();
    let mut owner = None;
    for (pid, event) in log.lines().filter_map(|line| line.split_once(' ')) {
        match event {
            "producer_locked" => {
                assert!(owner.replace(pid).is_none(), "overlapping owners: {log}");
                owners.insert(pid);
            }
            "producer_unlocked" => assert_eq!(owner.take(), Some(pid), "{log}"),
            _ => {}
        }
    }
    assert!(
        owners.len() >= 2,
        "discovery and capture need separate owners: {log}"
    );
}

#[test]
fn padded_big_endian_samples_are_normalized_without_reading_requeued_memory() {
    let home = Home::new("gentl-padded");
    let producer = producer(&home.root, 1);
    run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "padded.raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    )
    .success();
    assert_eq!(
        fs::read(home.work.join("padded.raw")).unwrap(),
        [2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11]
    );
    let metadata: Value =
        serde_json::from_slice(&fs::read(home.work.join("padded.raw.json")).unwrap()).unwrap();
    assert_eq!(metadata["pfnc_code"], 0x0110_0007_u32);
    assert_eq!(metadata["bytes"], 12);
}
#[test]
fn malformed_and_unsupported_buffers_never_publish_a_capture() {
    for mode in [4, 5, 6, 7, 10, 14, 15, 16, 17, 18, 19, 20, 23, 24] {
        let home = Home::new("gentl-malformed");
        let producer = producer(&home.root, mode);
        let output = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                "bad.raw",
                "--fresh",
                "0",
                "--timeout",
                "1s",
            ],
        );
        assert_ne!(output.code, Some(0), "mode{mode}: {}", output.stdout);
        let error = output.json();
        assert!(error["error"].is_object(), "{error}");
        if [6, 14, 15, 19, 20].contains(&mode) {
            assert_eq!(output.code, Some(5), "{error}");
            assert_eq!(error["error"]["code"], "unsupported", "{error}");
        }
        assert!(!home.work.join("bad.raw").exists());
        assert!(!home.work.join("bad.raw.json").exists());
    }
}

#[test]
fn ordinary_trigger_waits_preserve_the_helper_until_a_frame_arrives() {
    let home = Home::new("gentl-trigger-waits");
    let producer = producer(&home.root, 26);
    let began = Instant::now();
    run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "triggered.raw",
            "--fresh",
            "0",
            "--timeout",
            &time::arg(3000),
        ],
    )
    .success();
    assert!(
        began.elapsed() >= Duration::from_millis(350),
        "fixture must span several caller deadlines"
    );
    assert!(began.elapsed() < time::secs(5));
    assert_eq!(
        fs::read(home.work.join("triggered.raw")).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" dev_open"))
            .count(),
        1,
        "native helper must not restart: {log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" wait_timeout"))
            .count(),
        50,
        "{log}"
    );
    assert!(log.lines().any(|line| line.ends_with(" frame")), "{log}");
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" dev_open")
                || line.ends_with(" wait_timeout")
                || line.ends_with(" frame"))
            .map(|line| line.split_once(' ').unwrap().0)
            .collect::<BTreeSet<_>>()
            .len(),
        1,
        "all waits and the delivered frame must use the original helper: {log}"
    );
}

#[test]
fn multipart_inspection_reports_the_original_unsupported_error() {
    let home = Home::new("gentl-multipart-inspect");
    let producer = producer(&home.root, 6);
    let output = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "inspect",
            "-c",
            &selector(&producer),
            "--preview",
            "none",
            "--fresh",
            "0",
            "--timeout",
            "1s",
        ],
    );
    let error = output.json();
    assert_eq!(output.code, Some(5), "{error}");
    assert_eq!(error["error"]["code"], "unsupported", "{error}");
}

#[test]
fn a_stuck_native_event_call_is_retired_within_the_caller_deadline() {
    let home = Home::new("gentl-stuck-event");
    let producer = producer(&home.root, 27);
    let began = Instant::now();
    let result = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "stuck.raw",
            "--fresh",
            "0",
            "--timeout",
            "1s",
        ],
    );
    assert_ne!(result.code, Some(0));
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "{}",
        result.stdout
    );
    assert!(!home.work.join("stuck.raw").exists());
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    let pid = log
        .lines()
        .find(|line| line.ends_with(" stuck_event"))
        .unwrap_or_else(|| panic!("stuck call was not reached: {log}"))
        .split_once(' ')
        .unwrap()
        .0
        .parse::<i32>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: signal0 queries existence without sending a signal.
        if unsafe { libc::kill(pid, 0) } == -1 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stuck SDK helper {pid} was not killed/reaped: {log}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn a_hung_or_crashing_provider_does_not_hide_a_healthy_provider() {
    for mode in [2, 3, 9] {
        let home = Home::new("gentl-isolation");
        let bad = producer(&home.root, mode);
        let good = producer(&home.root, 0);
        let began = Instant::now();
        let output = run(
            &home,
            &[bad, good],
            &["--json", "list", "--kind", "gentl", "--timeout", "2s"],
        )
        .success();
        assert!(began.elapsed() < Duration::from_secs(5), "{output}");
        assert_eq!(output["cameras"].as_array().unwrap().len(), 1, "{output}");
        assert!(
            output["warnings"].is_array(),
            "provider failure must be reported: {output}"
        );
        // Both the crashing/hung helper and the successful helper must leave
        // no live or zombie native process after the CLI has completed.
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        let pids = log
            .lines()
            .map(|line| line.split_once(' ').unwrap().0.parse::<i32>().unwrap())
            .collect::<BTreeSet<_>>();
        assert!(
            pids.len() >= 2,
            "both producers must have been loaded: {log}"
        );
        for pid in pids {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                // SAFETY: signal0 only queries process existence.
                if unsafe { libc::kill(pid, 0) } == -1 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ESRCH)
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "helper {pid} was not reaped: {log}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn legacy_producer_clock_ticks_are_retained_without_inventing_nanoseconds() {
    let home = Home::new("gentl-ticks");
    let producer = producer(&home.root, 12);
    let captured = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "ticks.raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    )
    .success();
    let metadata: Value =
        serde_json::from_slice(&fs::read(home.work.join("ticks.raw.json")).unwrap()).unwrap();
    let inspected = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "inspect",
            "-c",
            &selector(&producer),
            "--preview",
            "none",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    )
    .success();
    let stdout = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "-",
            "-f",
            "raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    );
    assert_eq!(stdout.code, Some(0), "{}", stdout.stderr);
    assert_eq!(stdout.raw, [1, 2, 3, 4, 5, 6]);
    let streamed: Value = serde_json::from_str(stdout.stderr.lines().last().unwrap()).unwrap();
    // These results cross different worker/saver and CLI projection paths.
    // A value above i64::MAX must remain exact even without a clock frequency.
    for frame in [&captured, &metadata, &inspected["frame"], &streamed] {
        assert_eq!(frame["device_timestamp"]["ticks"].as_u64(), Some(u64::MAX));
        assert!(frame.get("device_ts_ns").is_none(), "{frame}");
        assert!(
            frame["device_timestamp"].get("frequency_hz").is_none(),
            "{frame}"
        );
    }
}

#[test]
fn optional_producer_clock_frequency_is_preserved_without_creating_nanoseconds() {
    for mode in [21, 22, 25] {
        let home = Home::new("gentl-clock-frequency");
        let producer = producer(&home.root, mode);
        run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                "clock.raw",
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
        .success();
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("clock.raw.json")).unwrap()).unwrap();
        assert_eq!(metadata["device_timestamp"]["ticks"], 98_765);
        if mode == 21 {
            assert_eq!(metadata["device_timestamp"]["frequency_hz"], 1_000_000);
            assert!(
                metadata.get("device_ts_ns").is_none(),
                "raw clock frequency must not manufacture nanoseconds: {metadata}"
            );
        } else {
            assert!(
                metadata["device_timestamp"].get("frequency_hz").is_none(),
                "zero/missing frequency must remain unknown: {metadata}"
            );
            assert_eq!(metadata["device_ts_ns"], 1_234_000);
        }
    }
}

#[test]
fn enumeration_waits_share_the_deadline_across_empty_interfaces_and_open() {
    for mode in [28, 30] {
        let home = Home::new("gentl-enumeration-budget");
        let producer = producer(&home.root, mode);
        let began = Instant::now();
        let output = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "list",
                "--kind",
                "gentl",
                "--timeout",
                &time::arg(2000),
            ],
        )
        .success();
        assert_eq!(output["cameras"].as_array().unwrap().len(), 1, "{output}");
        assert!(
            output.get("warnings").is_none(),
            "healthy enumeration must finish before its outer deadline: {output}"
        );
        assert!(began.elapsed() < time::secs(3));
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        let waits = log
            .lines()
            .filter_map(|line| {
                line.split_once("_wait_ms=")
                    .map(|(_, value)| value.parse::<u64>().unwrap())
            })
            .collect::<Vec<_>>();
        assert_eq!(waits.len(), if mode == 28 { 4 } else { 17 }, "{log}");
        assert!(
            waits.iter().all(|wait| *wait < time::ms(1000)),
            "native SDK waits must share the operation budget: {log}"
        );
        if mode == 28 {
            run(
                &home,
                std::slice::from_ref(&producer),
                &[
                    "--json",
                    "capture",
                    "-c",
                    &selector(&producer),
                    "-o",
                    "enumerated.raw",
                    "--fresh",
                    "0",
                    "--timeout",
                    &time::arg(2000),
                ],
            )
            .success();
            assert_eq!(
                fs::read(home.work.join("enumerated.raw")).unwrap(),
                [1, 2, 3, 4, 5, 6]
            );
        }
    }
}

#[test]
fn a_failed_interface_keeps_the_same_producers_healthy_camera() {
    let home = Home::new("gentl-interface-failure");
    let producer = producer(&home.root, 29);
    let output = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "list",
            "--kind",
            "gentl",
            "--timeout",
            &time::arg(2000),
        ],
    )
    .success();
    assert_eq!(output["cameras"].as_array().unwrap().len(), 1, "{output}");
    assert!(
        output["warnings"]
            .as_array()
            .is_some_and(|warnings| !warnings.is_empty()),
        "{output}"
    );
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" if_close"))
            .count(),
        3,
        "every opened interface must close: {log}"
    );
}

#[test]
fn four_hung_producers_cannot_starve_the_healthy_fifth() {
    let home = Home::new("gentl-provider-fairness");
    let bad = producer(&home.root, 2);
    let mut paths = Vec::new();
    for index in 0..4 {
        let path = home.root.join(format!("a-hung-{index}.cti"));
        fs::copy(&bad, &path).unwrap();
        paths.push(path.canonicalize().unwrap());
    }
    paths.push(producer(&home.root, 0));
    let began = Instant::now();
    let output = run(
        &home,
        &paths,
        &["--json", "list", "--kind", "gentl", "--timeout", "3s"],
    )
    .success();
    assert!(began.elapsed() < Duration::from_secs(4), "{output}");
    assert_eq!(output["cameras"].as_array().unwrap().len(), 1, "{output}");
    assert!(output["warnings"].is_array(), "{output}");
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(
        log.lines().filter(|line| line.ends_with(" init")).count(),
        5,
        "every configured producer must get an attempt: {log}"
    );
}

#[test]
fn completed_discovery_survives_producer_teardown_hang_or_crash() {
    for mode in [31, 32, 35] {
        let home = Home::new("gentl-teardown");
        let producer = producer(&home.root, mode);
        let began = Instant::now();
        let output = run(
            &home,
            std::slice::from_ref(&producer),
            &["--json", "list", "--kind", "gentl", "--timeout", "2s"],
        )
        .success();
        assert!(began.elapsed() < Duration::from_secs(3), "{output}");
        // Bootstrap and mode 35's delayed initialization precede the discovery
        // reply. Prove teardown was reached before testing reply retention.
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        assert!(
            log.lines().any(|line| line.ends_with(" close_lib")),
            "{log}"
        );
        assert_eq!(
            output["cameras"].as_array().unwrap().len(),
            1,
            "teardown cannot discard completed discovery: {output}"
        );
        assert!(
            output["warnings"]
                .as_array()
                .is_some_and(|warnings| !warnings.is_empty()),
            "cleanup failure must be visible: {output}"
        );
    }
}

#[test]
fn double_nul_xml_url_lists_and_fallback_open_the_camera() {
    for mode in [33, 34] {
        let home = Home::new("gentl-url-list");
        let producer = producer(&home.root, mode);
        run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                "urls.raw",
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
        .success();
        assert_eq!(
            fs::read(home.work.join("urls.raw")).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
    }
}

fn original_chunk_payload() -> Vec<u8> {
    vec![
        0x91, 0x92, 0x93, 0x94, 1, 2, 3, 4, 5, 6, 0xa1, 0xa2, 7, 8, 9, 10, 11, 12, 0xa3, 0xa4,
        0xa5, 0xa6, 255, 255, 255, 255, 255, 255, 255, 255, 0xc1, 0xc2,
    ]
}

#[test]
fn indexed_chunks_preserve_the_full_payload_wide_ids_and_xml_measurements() {
    for mode in [37, 46, 48, 50] {
        let home = Home::new("gentl-indexed-chunks");
        let producer = producer(&home.root, mode);
        run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                "image.raw",
                "--fresh",
                "2",
                "--timeout",
                "2s",
            ],
        )
        .success();
        assert_eq!(
            fs::read(home.work.join("image.raw")).unwrap(),
            [2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11]
        );
        assert_eq!(
            fs::read(home.work.join("image.raw.chunks.raw")).unwrap(),
            original_chunk_payload(),
            "SDK requeue must not change retained chunk bytes"
        );
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("image.raw.chunks.raw.json")).unwrap())
                .unwrap();
        let chunks = &metadata["chunks"];
        assert_eq!(chunks["layout_id"], 0xaabb_ccdd_0011_2233_u64);
        assert_eq!(chunks["payload_bytes"], 32);
        assert_eq!(chunks["image_bytes"], 18);
        assert_eq!(chunks["image_layout"]["offset"], 4);
        assert_eq!(chunks["image_layout"]["row_stride"], 8);
        assert_eq!(chunks["image_layout"]["padding_y"], 2);
        assert_eq!(chunks["image_layout"]["byte_order"], "big");
        assert!(
            chunks.get("byte_order").is_none(),
            "producer descriptors have no inferred footer endian: {chunks}"
        );
        assert!(
            chunks.get("chunk_offset").is_none(),
            "indexed chunks need not trail the image: {chunks}"
        );
        assert_eq!(chunks["entries"][0]["id"], u64::MAX);
        assert_eq!(chunks["entries"][1]["id"], 0x1_0000_0001_u64);
        assert_eq!(
            chunks["values"]["values"]["ChunkCounterSample"],
            u64::MAX,
            "{chunks}"
        );
        assert_eq!(chunks["values"]["units"]["ChunkCounterSample"], "ticks");
    }
}

#[test]
fn unavailable_chunk_descriptors_keep_opaque_payload_with_explicit_errors() {
    for mode in [38, 39] {
        let home = Home::new("gentl-opaque-chunks");
        let producer = producer(&home.root, mode);
        run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                "opaque.raw",
                "--fresh",
                "2",
                "--timeout",
                "2s",
            ],
        )
        .success();
        assert_eq!(
            fs::read(home.work.join("opaque.raw.chunks.raw")).unwrap(),
            original_chunk_payload()
        );
        let metadata: Value = serde_json::from_slice(
            &fs::read(home.work.join("opaque.raw.chunks.raw.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["chunks"]["entries"], json!([]));
        assert!(
            metadata["chunks"]["descriptor_error"]
                .as_str()
                .is_some_and(|error| !error.is_empty()),
            "{metadata}"
        );
        assert!(
            metadata["chunks"]["values"]["errors"]["ChunkCounterSample"].is_string(),
            "{metadata}"
        );
    }
}

#[test]
fn malformed_chunk_indexes_and_unknown_trailing_data_never_publish_a_capture() {
    for mode in [40, 41, 42, 43, 44, 45, 47, 51, 52, 53] {
        let home = Home::new("gentl-malformed-chunks");
        let producer = producer(&home.root, mode);
        let result = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                "invalid.raw",
                "--fresh",
                "0",
                "--timeout",
                "1s",
            ],
        );
        assert_ne!(result.code, Some(0), "mode{mode}: {}", result.stdout);
        assert!(result.json()["error"].is_object());
        for file in [
            "invalid.raw",
            "invalid.raw.json",
            "invalid.raw.chunks.raw",
            "invalid.raw.chunks.raw.json",
        ] {
            assert!(!home.work.join(file).exists(), "mode{mode}: {file}");
        }
    }
}

#[test]
fn duplicate_chunk_ids_remain_raw_and_report_ambiguous_xml_values() {
    let home = Home::new("gentl-duplicate-chunks");
    let producer = producer(&home.root, 49);
    run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "duplicate.raw",
            "--fresh",
            "2",
            "--timeout",
            "2s",
        ],
    )
    .success();
    assert_eq!(
        fs::read(home.work.join("duplicate.raw.chunks.raw")).unwrap(),
        original_chunk_payload()
    );
    let metadata: Value =
        serde_json::from_slice(&fs::read(home.work.join("duplicate.raw.chunks.raw.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["chunks"]["entries"].as_array().unwrap().len(), 3);
    assert!(
        metadata["chunks"]["values"]["errors"]["ChunkCounterSample"].is_string(),
        "{metadata}"
    );
    assert!(
        metadata["chunks"]["values"]["values"]
            .get("ChunkCounterSample")
            .is_none(),
        "{metadata}"
    );
}

#[test]
fn producer_advertised_http_xml_uses_the_bounded_shared_fetcher() {
    use std::io::{Read as _, Write as _};
    let home = Home::new("gentl-http-xml");
    let producer = producer(&home.root, 13);
    let cache = home.root.join("vendor.xml");
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let file = cache.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "producer HTTP XML was not requested"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("HTTP XML accept failed: {error}"),
            }
        };
        // Use a blocking stream so the configured I/O timeouts apply.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 16 * 1024);
            socket.read_exact(&mut byte).unwrap();
            request.extend_from_slice(&byte);
        }
        assert!(request.starts_with(b"GET /vendor/mock.xml?SchemaVersion=1.1 HTTP/1.1\r\n"));
        let xml = fs::read(file).unwrap();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", xml.len()).unwrap();
        socket.write_all(&xml).unwrap();
    });
    let args = [
        "--json",
        "capture",
        "-c",
        &selector(&producer),
        "-o",
        "http.raw",
        "--fresh",
        "0",
        "--timeout",
        "2s",
    ];
    let output = home
        .command()
        .env_remove("GENICAM_GENTL64_PATH")
        .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
        .env("CAPTUREFAB_GENTL_MOCK_XML_FILE", &cache)
        .env(
            "CAPTUREFAB_GENTL_MOCK_XML_URL",
            format!("http://{address}/vendor/mock.xml?SchemaVersion=1.1"),
        )
        .args(args)
        .output()
        .unwrap();
    server.join().unwrap();
    Run::of_output(output, &args).success();
    assert_eq!(
        fs::read(home.work.join("http.raw")).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn producer_xml_file_urls_support_sdk_install_paths_with_spaces_and_unicode() {
    let home = Home::new("gentl-file-xml");
    let producer = producer(&home.root, 13);
    let file = home.root.join("SDK cache μ.xml");
    let encoded = file
        .to_string_lossy()
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    let args = [
        "--json",
        "capture",
        "-c",
        &selector(&producer),
        "-o",
        "file.raw",
        "--fresh",
        "0",
        "--timeout",
        "2s",
    ];
    let output = home
        .command()
        .env_remove("GENICAM_GENTL64_PATH")
        .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
        .env("CAPTUREFAB_GENTL_MOCK_XML_FILE", &file)
        .env("CAPTUREFAB_GENTL_MOCK_XML_URL", format!("file://{encoded}"))
        .args(args)
        .output()
        .unwrap();
    Run::of_output(output, &args).success();
    assert_eq!(
        fs::read(home.work.join("file.raw")).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
}

#[cfg(feature = "copy-audit")]
#[test]
fn pipe_pixels_share_the_pool_and_only_required_normalization_copies() {
    for mode in [0, 1, 37, 54] {
        let home = Home::new("gentl-copy-audit");
        let producer = producer(&home.root, mode);
        let reports = home.root.join("reports");
        fs::create_dir(&reports).unwrap();
        let args = [
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "frame.raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ];
        Run::of_output(
            home.command()
                .env_remove("GENICAM_GENTL64_PATH")
                .env("CAPTUREFAB_COPY_REPORT", &reports)
                .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
                .args(args)
                .output()
                .unwrap(),
            &args,
        )
        .success();
        let mut pipe_calls = 0;
        for entry in fs::read_dir(&reports).unwrap() {
            let report: Value =
                serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
            let calls = report["sites"]["pipe_read"]["calls"].as_u64().unwrap();
            if calls == 0 {
                continue;
            }
            pipe_calls += calls;
            assert_eq!(
                report["sites"]["pipe_read"]["bytes"],
                calls
                    * match mode {
                        0 => 6,
                        37 => 32,
                        54 => 20,
                        _ => 18,
                    }
            );
            assert_eq!(
                report["sites"]["gentl_normalize"]["bytes"],
                calls * if matches!(mode, 0 | 54) { 0 } else { 24 }
            );
            assert_eq!(
                report["sites"]["chunk_image"]["bytes"],
                calls * if mode == 37 { 18 } else { 0 }
            );
            assert_eq!(
                report["sites"]["multipart_image"]["bytes"],
                calls * if mode == 54 { 6 } else { 0 }
            );
            assert_eq!(
                report["pool"]["allocations"].as_u64().unwrap()
                    + report["pool"]["reuses"].as_u64().unwrap(),
                calls * if matches!(mode, 37 | 54) { 2 } else { 1 }
            );
            for site in report["sites"].as_object().unwrap().values() {
                if site["class"] == "avoidable" {
                    assert_eq!(site["bytes"], 0);
                }
            }
        }
        assert!(pipe_calls > 0, "worker must report its pixel boundary");
    }
}

#[test]
fn multipart_stills_archive_every_component_and_select_an_unambiguous_image() {
    for (mode, selected) in [
        (54, None),
        (55, None),
        (56, Some(1)),
        (56, Some(2)),
        (73, None),
    ] {
        let home = Home::new("gentl-multipart");
        let producer = producer(&home.root, mode);
        let output = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &component_selector(&producer, selected),
                "-o",
                "image.raw",
                "--fresh",
                "0",
                "--timeout",
                "2s",
                "--manifest",
                "capture.jsonl",
            ],
        )
        .success();
        let selected = selected.unwrap_or(1);
        let pixels = if selected == 2 {
            vec![9, 8, 7, 6, 5, 4]
        } else if mode == 55 {
            vec![2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11]
        } else {
            vec![1, 2, 3, 4, 5, 6]
        };
        assert_eq!(fs::read(home.work.join("image.raw")).unwrap(), pixels);
        let archive = home.work.join("image.raw.multipart.raw");
        assert_eq!(
            fs::read(&archive).unwrap(),
            multipart_bytes(mode == 55, mode == 56)
        );
        let meta: Value = serde_json::from_slice(
            &fs::read(home.work.join("image.raw.multipart.raw.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["storage"], "concatenated_parts");
        assert_eq!(meta["multipart"]["selected"], selected);
        assert_eq!(
            meta["multipart"]["original_filled_bytes"],
            if mode == 55 {
                26
            } else if mode == 56 {
                24
            } else {
                20
            }
        );
        let parts = meta["multipart"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        for (index, offset) in [0, 24, 48].into_iter().enumerate() {
            assert_eq!(parts[index]["original_offset"], offset);
        }
        assert_eq!(parts[1]["stored_offset"], 12);
        assert_eq!(parts[2]["stored_offset"], if mode == 55 { 24 } else { 18 });
        assert_eq!(parts[0]["format"], 0x1234_5678_9abc_def0_u64);
        assert_eq!(parts[0]["source_id"], u64::MAX - 1);
        assert_eq!(parts[1]["source_id"], u64::MAX);
        assert_eq!(parts[1]["region_id"], 0x1_0000_0001_u64);
        assert_eq!(parts[2]["purpose_id"], 0x2_0000_0003_u64);
        if mode == 73 {
            assert!(meta["multipart"].get("byte_order").is_none());
        } else {
            assert_eq!(
                meta["multipart"]["byte_order"],
                if mode == 55 { "big" } else { "little" }
            );
        }
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.work.join("image.raw.json")).unwrap()).unwrap();
        let manifest: Value = serde_json::from_str(
            fs::read_to_string(home.work.join("capture.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let reference = &sidecar["multipart"];
        assert_eq!(
            reference["path"],
            archive.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(reference["selected"], selected);
        assert_eq!(manifest["multipart"], *reference);
        assert_eq!(
            output["multipart"][output["files"][0].as_str().unwrap()],
            *reference
        );
        assert_eq!(meta["device_timestamp"]["ticks"], 98_765);
        assert_eq!(meta["device_ts_ns"], 1_234_000);
        assert!(reference["sha256"].is_string());
    }
}

const COORD3D_C16: u32 = 0x0110_00b8;
const CONFIDENCE16: u32 = 0x0110_00c7;

#[test]
fn multipart_scalar_maps_export_full_precision_raw_and_png_from_saved_selectors() {
    let samples: [[u16; 6]; 3] = [
        [0, 1, 0x7fff, 0x8000, 0xfffe, 0xffff],
        [10, 20, 30, 40, 50, 60],
        [0xffff, 0x4321, 0x8001, 0x00ff, 0x0100, 0],
    ];
    for mode in [76, 77] {
        let home = Home::new("gentl-multipart-scalar");
        let producer = producer(&home.root, mode);
        let capture = |camera: &str, output: &str| {
            run(
                &home,
                std::slice::from_ref(&producer),
                &[
                    "--json",
                    "--no-session",
                    "capture",
                    "-c",
                    camera,
                    "-o",
                    output,
                    "--fresh",
                    "0",
                    "--timeout",
                    "2s",
                ],
            )
        };
        let intensity = capture(&selector(&producer), "intensity.raw").success();
        assert_eq!(intensity["pixel_format"], "Mono16");
        assert_eq!(
            fs::read(home.work.join("intensity.raw")).unwrap(),
            samples[1]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        let original = samples
            .into_iter()
            .flatten()
            .flat_map(|sample| {
                if mode == 77 {
                    sample.to_be_bytes()
                } else {
                    sample.to_le_bytes()
                }
            })
            .collect::<Vec<_>>();
        let companion: Value = serde_json::from_slice(
            &fs::read(home.work.join("intensity.raw.multipart.raw.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(companion["multipart"]["selected"], 1);
        let choices = companion["image_selectors"].as_array().unwrap();
        assert_eq!(choices.len(), 2);
        for (choice, (part, code, name)) in choices.iter().zip([
            (0, COORD3D_C16, "Coord3D_C16"),
            (2, CONFIDENCE16, "Confidence16"),
        ]) {
            assert_eq!(choice["part"], part);
            let id = choice["camera"].as_str().unwrap();
            let words = samples[part];
            let raw = format!("{name}.raw");
            let png = format!("{name}.png");
            for output in [&raw, &png] {
                let saved = capture(id, output).success();
                assert_eq!(saved["pixel_format"], name);
                assert_eq!(saved["bit_depth"], 16);
                assert_eq!(
                    fs::read(home.work.join(format!("{output}.multipart.raw"))).unwrap(),
                    original
                );
                let metadata: Value = serde_json::from_slice(
                    &fs::read(home.work.join(format!("{output}.multipart.raw.json"))).unwrap(),
                )
                .unwrap();
                assert_eq!(metadata["multipart"]["selected"], part);
                assert_eq!(metadata["multipart"]["parts"][part]["format"], code);
                assert!(metadata.get("units").is_none());
            }
            assert_eq!(
                fs::read(home.work.join(&raw)).unwrap(),
                words
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            let sidecar: Value =
                serde_json::from_slice(&fs::read(home.work.join(format!("{raw}.json"))).unwrap())
                    .unwrap();
            assert_eq!(sidecar["pfnc_code"], code);
            assert!(sidecar.get("unit").is_none());

            let bytes = fs::read(home.work.join(&png)).unwrap();
            let mut decoder = png::Decoder::new(bytes.as_slice()).read_info().unwrap();
            let mut decoded = vec![0; decoder.output_buffer_size()];
            let info = decoder.next_frame(&mut decoded).unwrap();
            assert_eq!(
                (info.color_type, info.bit_depth),
                (png::ColorType::Grayscale, png::BitDepth::Sixteen)
            );
            let expected_png = words
                .into_iter()
                .flat_map(u16::to_be_bytes)
                .collect::<Vec<_>>();
            assert_eq!(&decoded[..info.buffer_size()], expected_png.as_slice());
            let provenance: Value = serde_json::from_str(
                &decoder
                    .info()
                    .utf8_text
                    .iter()
                    .find(|entry| entry.keyword == "capturefab")
                    .unwrap()
                    .get_text()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(provenance["pixel_format"], name);
            assert_eq!(provenance["pfnc_code"], code);
            assert!(provenance.get("unit").is_none());
        }
    }
}

#[test]
fn multipart_packed_range_selectors_export_exact_raw_png_and_archive() {
    let home = Home::new("gentl-packed-range");
    let producer = producer(&home.root, 110);
    let capture = |camera: &str, output: &str| {
        run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "--no-session",
                "capture",
                "-c",
                camera,
                "-o",
                output,
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
    };
    let packed10 = [0x00, 0x04, 0x50, 0xd5, 0xff, 0x01, 0x8e, 0xa4, 0xaa, 0xff];
    let packed12 = [0xbc, 0x3a, 0x12, 0x01, 0xf0, 0xff];
    let intensity = [11, 12, 13, 14];
    let archive = [
        packed10.as_slice(),
        intensity.as_slice(),
        packed12.as_slice(),
    ]
    .concat();
    let saved = capture(&selector(&producer), "intensity.raw").success();
    assert_eq!(saved["pixel_format"], "Mono8");
    assert_eq!(
        fs::read(home.work.join("intensity.raw")).unwrap(),
        intensity
    );
    let companion: Value = serde_json::from_slice(
        &fs::read(home.work.join("intensity.raw.multipart.raw.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(companion["multipart"]["selected"], 1);
    let choices = companion["image_selectors"].as_array().unwrap();
    assert_eq!(choices.len(), 2);
    for (choice, (part, code, name, bits, packed, samples)) in choices.iter().zip([
        (
            0,
            0x010a_00d7u32,
            "Coord3D_C10p",
            10u8,
            packed10.as_slice(),
            [0u16, 1, 0x155, 0x3ff, 0x201, 0x123, 0x2aa, 0x3fe].as_slice(),
        ),
        (
            2,
            0x010c_00dau32,
            "Coord3D_C12p",
            12,
            packed12.as_slice(),
            [0xabcu16, 0x123, 1, 0xfff].as_slice(),
        ),
    ]) {
        assert_eq!(choice["part"], part);
        let id = choice["camera"].as_str().unwrap();
        let raw = format!("{name}.raw");
        let png = format!("{name}.png");
        for output in [&raw, &png] {
            let result = capture(id, output).success();
            assert_eq!(result["pixel_format"], name);
            assert_eq!(
                fs::read(home.work.join(format!("{output}.multipart.raw"))).unwrap(),
                archive
            );
            let metadata: Value = serde_json::from_slice(
                &fs::read(home.work.join(format!("{output}.multipart.raw.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(metadata["multipart"]["selected"], part);
            assert_eq!(metadata["multipart"]["byte_order"], "little");
        }
        assert_eq!(fs::read(home.work.join(&raw)).unwrap(), packed);
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.work.join(format!("{raw}.json"))).unwrap())
                .unwrap();
        assert_eq!(sidecar["pfnc_code"], code);
        let bytes = fs::read(home.work.join(png)).unwrap();
        let mut decoder = png::Decoder::new(bytes.as_slice()).read_info().unwrap();
        let mut decoded = vec![0; decoder.output_buffer_size()];
        let info = decoder.next_frame(&mut decoded).unwrap();
        assert_eq!(
            (info.color_type, info.bit_depth),
            (png::ColorType::Grayscale, png::BitDepth::Sixteen)
        );
        assert_eq!(decoder.info().sbit.as_deref(), Some([bits].as_slice()));
        let expected: Vec<u8> = samples
            .iter()
            .flat_map(|sample| (sample << (16 - bits)).to_be_bytes())
            .collect();
        assert_eq!(&decoded[..info.buffer_size()], expected);
    }
}

#[test]
fn multipart_packed_range_requires_little_order_aligned_rows_and_pfnc32() {
    for (mode, expected, valid_alternate) in [
        (111, "confirmed little-endian packing", false),
        (112, "byte-aligned rows", true),
        (113, "not a supported image or scalar map", true),
        (114, "confirmed little-endian packing", false),
    ] {
        let home = Home::new("gentl-packed-range-unsupported");
        let producer = producer(&home.root, mode);
        let id = map_selector(&producer, 0, 0x010a_00d7);
        let error = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "--no-session",
                "capture",
                "-c",
                &id,
                "-o",
                "invalid.raw",
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
        .failure(5);
        assert!(
            error["message"].as_str().unwrap().contains(expected),
            "{error}"
        );
        let choices = error["details"]["image_selectors"].as_array().unwrap();
        assert!(choices.iter().all(|choice| choice["part"] != 0));
        assert_eq!(
            choices.iter().any(|choice| choice["part"] == 2),
            valid_alternate
        );
        assert_eq!(error["details"]["parts"].as_array().unwrap().len(), 3);
        assert!(!home.work.join("invalid.raw").exists());
        assert!(!home.work.join("invalid.raw.multipart.raw").exists());
    }
}

#[test]
fn multipart_scalar_maps_require_format_bound_selection_and_keep_measurements_out_of_exposure() {
    for mode in [78, 79] {
        let home = Home::new("gentl-multipart-map-selection");
        let producer = producer(&home.root, mode);
        let requested = if mode == 78 {
            selector(&producer)
        } else {
            map_selector(&producer, 0, COORD3D_C16)
        };
        let capture = |camera: &str, output: &str| {
            run(
                &home,
                std::slice::from_ref(&producer),
                &[
                    "--json",
                    "--no-session",
                    "capture",
                    "-c",
                    camera,
                    "-o",
                    output,
                    "--fresh",
                    "0",
                    "--timeout",
                    "2s",
                ],
            )
        };
        let error = capture(&requested, "invalid.raw").failure(5);
        let details = &error["details"];
        assert_eq!(details["parts"].as_array().unwrap().len(), 3);
        assert!(!home.work.join("invalid.raw.multipart.raw").exists());
        let choices = details["image_selectors"].as_array().unwrap();
        if mode == 79 {
            assert!(choices.is_empty(), "{error}");
            continue;
        }
        assert_eq!(choices.len(), 2);
        let depth = choices[0]["camera"].as_str().unwrap();
        assert_eq!(choices[0]["part"], 0);
        let mismatch =
            capture(&map_selector(&producer, 0, CONFIDENCE16), "mismatch.raw").failure(5);
        assert_eq!(
            mismatch["details"]["image_selectors"].as_array().unwrap(),
            choices
        );
        let jpeg = capture(depth, "lossy.jpg").failure(5);
        assert!(jpeg["hint"].as_str().unwrap().contains(".raw or .png"));
        for output in [
            "mismatch.raw",
            "lossy.jpg",
            "lossy.jpg.multipart.raw",
            "lossy.jpg.multipart.raw.json",
        ] {
            assert!(!home.work.join(output).exists());
        }

        // The fixture's camera XML supplies no profile evidence for depth.
        let measured = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "--no-session",
                "inspect",
                "-c",
                depth,
                "--preview",
                "none",
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
        .success();
        assert_eq!(measured["frame"]["pixel_format"], "Coord3D_C16");
        for key in ["exposure", "focus", "clipping", "auto_meter"] {
            assert!(measured.get(key).is_none(), "{measured}");
        }
        let log = home.root.join("sdk.log");
        fs::remove_file(&log).unwrap();
        let automatic = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "--no-session",
                "capture",
                "-c",
                depth,
                "--auto",
                "-o",
                "automatic.raw",
                "--timeout",
                "2s",
            ],
        )
        .failure(5);
        assert!(
            automatic["message"]
                .as_str()
                .unwrap()
                .contains("requires an intensity image"),
            "{automatic}"
        );
        assert!(!fs::read_to_string(log).unwrap().contains("acq_start"));
        assert!(!home.work.join("automatic.raw").exists());
    }
}

const COORD3D_ABC32F: u32 = 0x0260_00c0;
const POINT_CLOUD_WORDS: [u32; 9] = [
    0x3f80_0000,
    0x8000_0000,
    0x7fc0_1234,
    0x7f80_0001,
    0xff80_0000,
    0xc020_0000,
    0,
    0,
    0,
];

fn capture_point_cloud(home: &Home, producer: &Path, camera: &str, output: &str) -> Run {
    run(
        home,
        &[producer.to_owned()],
        &[
            "--json",
            "--no-session",
            "capture",
            "-c",
            camera,
            "-o",
            output,
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    )
}

#[test]
fn multipart_point_cloud_raw_preserves_exact_words_and_every_original_component() {
    for mode in [80, 81] {
        let home = Home::new("gentl-point-cloud");
        let producer = producer(&home.root, mode);
        let intensity =
            capture_point_cloud(&home, &producer, &selector(&producer), "intensity.raw").success();
        assert_eq!(intensity["pixel_format"], "Mono16");
        let companion: Value = serde_json::from_slice(
            &fs::read(home.work.join("intensity.raw.multipart.raw.json")).unwrap(),
        )
        .unwrap();
        let choices = companion["image_selectors"].as_array().unwrap();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0]["part"], 0);
        let camera = choices[0]["camera"].as_str().unwrap();

        let saved = capture_point_cloud(&home, &producer, camera, "cloud.raw").success();
        assert_eq!(saved["pixel_format"], "Coord3D_ABC32f");
        assert_eq!(saved["bit_depth"], 32);
        assert_eq!(saved["width"], 3);
        assert_eq!(saved["height"], 1);
        assert_eq!(
            fs::read(home.work.join("cloud.raw")).unwrap(),
            POINT_CLOUD_WORDS
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>()
        );
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.work.join("cloud.raw.json")).unwrap()).unwrap();
        assert_eq!(sidecar["pfnc_code"], COORD3D_ABC32F);
        assert_eq!(sidecar["row_bytes"], 36);
        assert_eq!(
            sidecar["sample_layout"],
            json!({
                "sample_type":"float32", "organization":"interleaved", "components":["A","B","C"], "byte_order":"little"
            })
        );
        assert!(sidecar.get("unit").is_none());
        let order = if mode == 81 { "big" } else { "little" };
        let mut expected = POINT_CLOUD_WORDS
            .into_iter()
            .flat_map(|word| {
                if mode == 81 {
                    word.to_be_bytes()
                } else {
                    word.to_le_bytes()
                }
            })
            .collect::<Vec<_>>();
        expected.extend(
            [10u16, 20, 30, 0xffff, 0x1234, 0]
                .into_iter()
                .flat_map(|sample| {
                    if mode == 81 {
                        sample.to_be_bytes()
                    } else {
                        sample.to_le_bytes()
                    }
                }),
        );
        assert_eq!(
            fs::read(home.work.join("cloud.raw.multipart.raw")).unwrap(),
            expected
        );
        let layout: Value = serde_json::from_slice(
            &fs::read(home.work.join("cloud.raw.multipart.raw.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(layout["multipart"]["selected"], 0);
        assert_eq!(layout["multipart"]["byte_order"], order);
        assert_eq!(layout["multipart"]["parts"][0]["format"], COORD3D_ABC32F);
        assert_eq!(layout["multipart"]["parts"][1]["original_offset"], 40);
        assert_eq!(layout["multipart"]["parts"][1]["stored_offset"], 36);

        if mode == 80 {
            for output in ["cloud.png", "cloud.jpg"] {
                let error = capture_point_cloud(&home, &producer, camera, output).failure(5);
                assert!(error["hint"].as_str().unwrap().contains(".raw"));
                assert!(!home.work.join(output).exists());
                assert!(!home.work.join(format!("{output}.multipart.raw")).exists());
            }
        }
    }
}

const COORD3D_ABC16: u32 = 0x0230_00b9;
const UNSIGNED_CLOUD_WORDS: [u16; 9] = [0, 1, 0xffff, 0x8000, 0xfffe, 0x1234, 7, 8, 9];

#[test]
fn multipart_abc16_raw_preserves_unsigned_words_and_original_components() {
    for mode in [116, 117] {
        let home = Home::new("gentl-unsigned-cloud");
        let producer = producer(&home.root, mode);
        capture_point_cloud(&home, &producer, &selector(&producer), "intensity.raw").success();
        let companion: Value = serde_json::from_slice(
            &fs::read(home.work.join("intensity.raw.multipart.raw.json")).unwrap(),
        )
        .unwrap();
        let choices = companion["image_selectors"].as_array().unwrap();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0]["part"], 0);
        let camera = choices[0]["camera"].as_str().unwrap();
        let result = capture_point_cloud(&home, &producer, camera, "cloud.raw").success();
        assert_eq!(result["pixel_format"], "Coord3D_ABC16");
        assert_eq!(result["bit_depth"], 16);
        assert_eq!(
            (result["width"].as_u64(), result["height"].as_u64()),
            (Some(3), Some(1))
        );
        let expected: Vec<_> = UNSIGNED_CLOUD_WORDS
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(fs::read(home.work.join("cloud.raw")).unwrap(), expected);
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("cloud.raw.json")).unwrap()).unwrap();
        assert_eq!(metadata["pfnc_code"], COORD3D_ABC16);
        assert_eq!(metadata["row_bytes"], 18);
        assert_eq!(
            metadata["sample_layout"],
            json!({
                "sample_type":"uint16", "organization":"interleaved", "components":["A","B","C"], "byte_order":"little"
            })
        );
        assert!(metadata.get("unit").is_none());
        let original = UNSIGNED_CLOUD_WORDS
            .into_iter()
            .chain([10, 20, 30, 0xffff, 0x1234, 0])
            .flat_map(|word| {
                if mode == 117 {
                    word.to_be_bytes()
                } else {
                    word.to_le_bytes()
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(original.len(), 30);
        assert_eq!(
            fs::read(home.work.join("cloud.raw.multipart.raw")).unwrap(),
            original
        );
        assert_eq!(
            fs::read(home.work.join("intensity.raw.multipart.raw")).unwrap(),
            original
        );
        let archive: Value = serde_json::from_slice(
            &fs::read(home.work.join("cloud.raw.multipart.raw.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            archive["multipart"]["byte_order"],
            if mode == 117 { "big" } else { "little" }
        );
        assert_eq!(archive["multipart"]["parts"][0]["format"], COORD3D_ABC16);
        assert_eq!(archive["multipart"]["parts"][1]["original_offset"], 40);
        assert_eq!(archive["multipart"]["parts"][1]["stored_offset"], 18);
        assert_eq!(archive["multipart"]["parts"][2]["stored_offset"], 24);
        if mode == 116 {
            for output in ["cloud.png", "cloud.jpg"] {
                let error = capture_point_cloud(&home, &producer, camera, output).failure(5);
                assert!(error["hint"].as_str().unwrap().contains(".raw"));
                assert!(!home.work.join(output).exists());
                assert!(!home.work.join(format!("{output}.multipart.raw")).exists());
            }
            let measured = run(
                &home,
                std::slice::from_ref(&producer),
                &[
                    "--json",
                    "--no-session",
                    "inspect",
                    "-c",
                    camera,
                    "--preview",
                    "none",
                    "--fresh",
                    "0",
                    "--timeout",
                    "2s",
                ],
            )
            .success();
            assert_eq!(measured["frame"]["pixel_format"], "Coord3D_ABC16");
            for key in ["exposure", "focus", "clipping", "auto_meter"] {
                assert!(measured.get(key).is_none(), "{measured}");
            }
            let log = home.root.join("sdk.log");
            fs::remove_file(&log).unwrap();
            let automatic = run(
                &home,
                std::slice::from_ref(&producer),
                &[
                    "--json",
                    "--no-session",
                    "capture",
                    "-c",
                    camera,
                    "--auto",
                    "-o",
                    "automatic.raw",
                    "--timeout",
                    "2s",
                ],
            )
            .failure(5);
            assert!(
                automatic["message"]
                    .as_str()
                    .unwrap()
                    .contains("requires an intensity image")
            );
            assert!(!fs::read_to_string(log).unwrap().contains("acq_start"));
            assert!(!home.work.join("automatic.raw").exists());
        }
    }
}

#[test]
fn multipart_abc16_refuses_unknown_endian_wrong_namespace_and_unbound_selector() {
    for mode in [116, 118, 119] {
        let home = Home::new("gentl-unsigned-cloud-selection");
        let producer = producer(&home.root, mode);
        let camera = if mode == 116 {
            component_selector(&producer, Some(0))
        } else {
            map_selector(&producer, 0, COORD3D_ABC16)
        };
        let error = capture_point_cloud(&home, &producer, &camera, "invalid.raw").failure(5);
        assert_eq!(error["details"]["parts"].as_array().unwrap().len(), 3);
        let choices = error["details"]["image_selectors"].as_array().unwrap();
        if mode == 116 {
            assert!(choices.iter().any(|choice| choice["part"] == 0), "{error}");
        } else {
            assert!(choices.iter().all(|choice| choice["part"] != 0), "{error}");
        }
        assert!(!home.work.join("invalid.raw").exists());
        assert!(!home.work.join("invalid.raw.multipart.raw").exists());
    }
}

#[test]
fn abc16_ordinary_and_gendc_raw_keep_exact_unsigned_coordinate_words() {
    let expected = [0u16, 1, 0xffff, 0x8000, 0xfffe, 0x1234]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    for mode in [115, 120] {
        let home = Home::new("gentl-ordinary-unsigned-cloud");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let camera = if mode == 120 {
            capture_point_cloud(&home, &producer, &id, "intensity.raw").success();
            let companion: Value = serde_json::from_slice(
                &fs::read(home.work.join("intensity.raw.gendc.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                companion["image_selectors"][0]["gendc"]["pfnc_code"],
                COORD3D_ABC16
            );
            companion["image_selectors"][0]["camera"]
                .as_str()
                .unwrap()
                .to_owned()
        } else {
            id
        };
        let result = capture_point_cloud(&home, &producer, &camera, "cloud.raw").success();
        assert_eq!(result["pixel_format"], "Coord3D_ABC16");
        assert_eq!(
            (result["width"].as_u64(), result["height"].as_u64()),
            (Some(2), Some(1))
        );
        assert_eq!(result["bit_depth"], 16);
        assert_eq!(fs::read(home.work.join("cloud.raw")).unwrap(), expected);
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("cloud.raw.json")).unwrap()).unwrap();
        assert_eq!(metadata["row_bytes"], 12);
        assert_eq!(
            metadata["sample_layout"],
            json!({
                "sample_type":"uint16", "organization":"interleaved", "components":["A","B","C"], "byte_order":"little"
            })
        );
        assert!(metadata.get("unit").is_none());
        if mode == 120 {
            let archive = fs::read(home.work.join("cloud.raw.gendc")).unwrap();
            assert_eq!(
                archive,
                fs::read(home.work.join("intensity.raw.gendc")).unwrap()
            );
            assert_eq!(&archive[438..450], expected);
        }
    }
}

#[test]
fn multipart_point_cloud_requires_a_format_bound_selector_and_reported_endian() {
    for mode in [82, 83] {
        let home = Home::new("gentl-point-cloud-selection");
        let producer = producer(&home.root, mode);
        let id = if mode == 82 {
            selector(&producer)
        } else {
            map_selector(&producer, 0, COORD3D_ABC32F)
        };
        let error = capture_point_cloud(&home, &producer, &id, "invalid.raw").failure(5);
        let choices = error["details"]["image_selectors"].as_array().unwrap();
        assert!(!home.work.join("invalid.raw").exists());
        assert!(!home.work.join("invalid.raw.multipart.raw").exists());
        if mode == 83 {
            assert!(choices.is_empty(), "{error}");
            continue;
        }
        assert_eq!(error["details"]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(choices.len(), 2);
        let camera = choices[0]["camera"].as_str().unwrap();
        let stale = map_selector(&producer, 0, COORD3D_C16);
        let error = capture_point_cloud(&home, &producer, &stale, "stale.raw").failure(5);
        assert_eq!(
            error["details"]["image_selectors"].as_array().unwrap(),
            choices
        );
        assert!(!home.work.join("stale.raw").exists());
        capture_point_cloud(&home, &producer, camera, "range.raw").success();
        let mut expected = POINT_CLOUD_WORDS
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(fs::read(home.work.join("range.raw")).unwrap(), expected);
        expected.extend(
            [0xffffu16, 0x1234, 0]
                .into_iter()
                .flat_map(u16::to_le_bytes),
        );
        assert_eq!(
            fs::read(home.work.join("range.raw.multipart.raw")).unwrap(),
            expected
        );
    }
}

#[test]
fn multipart_selection_errors_supply_copyable_ids_and_keep_the_full_part_table() {
    for (mode, part) in [(56, None), (57, None), (54, Some(0)), (54, Some(9))] {
        let home = Home::new("gentl-multipart-selection");
        let producer = producer(&home.root, mode);
        let error = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &component_selector(&producer, part),
                "-o",
                "image.raw",
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
        .failure(5);
        let details = &error["details"];
        assert_eq!(details["parts"].as_array().unwrap().len(), 3, "{error}");
        let choices = details["image_selectors"].as_array().unwrap();
        if mode == 56 {
            assert_eq!(choices.len(), 2);
            let id = choices[1]["camera"].as_str().unwrap();
            assert_eq!(choices[1]["part"], 2);
            let output = run(
                &home,
                std::slice::from_ref(&producer),
                &[
                    "--json",
                    "capture",
                    "-c",
                    id,
                    "-o",
                    "selected.raw",
                    "--fresh",
                    "0",
                    "--timeout",
                    "2s",
                ],
            )
            .success();
            assert_eq!(output["count"], 1);
            assert_eq!(
                fs::read(home.work.join("selected.raw")).unwrap(),
                [9, 8, 7, 6, 5, 4]
            );
        }
        assert!(!home.work.join("image.raw").exists());
        assert!(!home.work.join("image.raw.multipart.raw").exists());
    }
}

#[test]
fn multipart_malformed_ranges_and_unsupported_layouts_never_publish_a_partial_image() {
    for (mode, exit) in [
        (58, 5),
        (59, 1),
        (60, 1),
        (61, 1),
        (62, 1),
        (63, 1),
        (64, 1),
        (65, 5),
        (66, 5),
        (67, 5),
        (68, 5),
        (69, 5),
        (70, 5),
        (72, 5),
        (74, 1),
    ] {
        let home = Home::new("gentl-multipart-invalid");
        let producer = producer(&home.root, mode);
        let error = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                "image.raw",
                "--fresh",
                "0",
                "--timeout",
                &time::arg(2000),
            ],
        )
        .failure(exit);
        assert_eq!(
            error["code"],
            if exit == 5 { "unsupported" } else { "failed" },
            "mode{mode}: {error}"
        );
        assert!(
            fs::read_dir(&home.work).unwrap().next().is_none(),
            "mode{mode}"
        );
        if mode == 62 {
            // Invalid overlap is rejected before offering any choices for two images.
            assert!(error["details"].get("image_selectors").is_none());
        }
    }
}

#[test]
fn multipart_companion_collisions_number_static_names_without_overwriting_artifacts() {
    for output in ["image.raw", "{serial}.raw"] {
        let home = Home::new("gentl-multipart-number");
        let producer = producer(&home.root, 54);
        let stem = if output == "image.raw" {
            "image"
        } else {
            "MOCK-001"
        };
        let base = format!("{stem}.raw.multipart.raw");
        let numbered = format!("{stem}-0001.raw.multipart.raw.json");
        fs::write(home.work.join(&base), b"original payload").unwrap();
        fs::write(home.work.join(&numbered), b"original metadata").unwrap();
        let result = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                output,
                "--on-exists",
                "number",
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
        .success();
        assert!(
            result["files"][0]
                .as_str()
                .unwrap()
                .ends_with(&format!("{stem}-0002.raw")),
            "{result}"
        );
        assert_eq!(fs::read(home.work.join(base)).unwrap(), b"original payload");
        assert_eq!(
            fs::read(home.work.join(numbered)).unwrap(),
            b"original metadata"
        );
        assert!(!home.work.join(format!("{stem}.raw")).exists());
        assert!(!home.work.join(format!("{stem}-0001.raw")).exists());
    }
}

#[test]
fn failed_multipart_capture_reports_previously_saved_archive_references() {
    let home = Home::new("gentl-multipart-failed-summary");
    let producer = producer(&home.root, 54);
    fs::write(
        home.work.join("part-000002.raw.multipart.raw.json"),
        b"existing",
    )
    .unwrap();
    let error = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "part-{frame}.raw",
            "-n",
            "2",
            "--every",
            "100ms",
            "--fresh",
            "0",
            "--timeout",
            "3s",
        ],
    )
    .failure(1);
    assert_eq!(error["code"], "exists", "{error}");
    let details = &error["details"];
    let files = details["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{error}");
    let image = files[0].as_str().unwrap();
    assert!(image.ends_with("part-000001.raw"));
    assert!(
        details["multipart"][image]["path"]
            .as_str()
            .unwrap()
            .ends_with("part-000001.raw.multipart.raw")
    );
    assert_eq!(
        fs::read(home.work.join("part-000002.raw.multipart.raw.json")).unwrap(),
        b"existing"
    );
    assert!(!home.work.join("part-000002.raw").exists());
    assert!(!home.work.join("part-000002.raw.multipart.raw").exists());
}

#[test]
fn multipart_stdout_refuses_to_discard_unselected_components() {
    let home = Home::new("gentl-multipart-stdout");
    let producer = producer(&home.root, 54);
    let result = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "-",
            "-f",
            "raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ],
    );
    assert_eq!(result.code, Some(5), "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("unsupported"));
}

#[test]
fn warm_multipart_selection_retry_releases_only_the_old_exclusive_sdk_owner() {
    for (mode, requested) in [(56, None), (75, Some(1))] {
        let home = Home::new("gentl-multipart-warm-selection");
        let producer = producer(&home.root, mode);
        let warm = |args: &[&str]| {
            Run::of_output(
                home.warm_command()
                    .env_remove("GENICAM_GENTL64_PATH")
                    .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
                    .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
                    .env("CAPTUREFAB_GENTL_MOCK_LOCK", home.root.join("sdk.lock"))
                    .args(args)
                    .output()
                    .unwrap(),
                args,
            )
        };
        // Starting the coordinator with this environment also exercises real worker inheritance.
        warm(&["--json", "info", "-c", "sim:0"]).success();
        let error = warm(&[
            "--json",
            "capture",
            "-c",
            &component_selector(&producer, requested),
            "-o",
            "ambiguous.raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ])
        .failure(5);
        let choices = error["details"]["image_selectors"].as_array().unwrap();
        assert_eq!(choices.len(), if mode == 56 { 2 } else { 1 });
        let id = choices.last().unwrap()["camera"].as_str().unwrap();
        warm(&[
            "--json",
            "capture",
            "-c",
            id,
            "-o",
            "selected.raw",
            "--fresh",
            "0",
            "--timeout",
            "2s",
        ])
        .success();
        assert_eq!(
            fs::read(home.work.join("selected.raw")).unwrap(),
            [9, 8, 7, 6, 5, 4]
        );
        let status = home.run(&["--json", "session", "status", "auto"]).success();
        assert!(
            status["cameras"]
                .as_array()
                .unwrap()
                .iter()
                .any(|camera| camera["id"] == "sim:0"),
            "{status}"
        );
        // Both helper lifecycles must release remote state and host buffers.
        home.run(&["--json", "session", "stop", "auto"]).success();
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        assert!(!log.contains("producer_lock_busy"), "{log}");
        assert!(!log.contains("stale_camera_state"), "{log}");
        assert!(!log.contains("stale_buffer"), "{log}");
        let calls = log
            .lines()
            .map(|line| line.split_once(' ').unwrap())
            .collect::<Vec<_>>();
        let owners = calls
            .iter()
            .enumerate()
            .filter_map(|(index, (pid, call))| {
                (*call == "producer_locked"
                    && calls
                        .iter()
                        .any(|(owner, event)| owner == pid && *event == "dev_open"))
                .then_some((index, *pid))
            })
            .collect::<Vec<_>>();
        let [(first, first_pid), (second, second_pid)] = owners.as_slice() else {
            panic!("expected exactly two exclusive acquisition owners: {log}");
        };
        assert_ne!(first_pid, second_pid);
        let mut required = vec!["dev_open", "tl_lock", "ds_start", "acq_start"];
        if mode == 75 {
            required.push("transfer_start");
        }
        required.extend(["frame", "acq_stop"]);
        if mode == 75 {
            required.push("transfer_stop");
        }
        required.extend([
            "ds_stop",
            "revoke",
            "revoke",
            "tl_unlock",
            "ds_close",
            "dev_close",
            "producer_unlocked",
        ]);
        for (pid, calls) in [
            (first_pid, &calls[*first..*second]),
            (second_pid, &calls[*second..]),
        ] {
            let mut calls = calls
                .iter()
                .filter_map(|(owner, call)| (owner == pid).then_some(*call));
            for expected in &required {
                assert!(
                    calls.any(|call| call == *expected),
                    "mode {mode}, PID {pid} missing {expected} in order: {log}"
                );
            }
        }
    }
}

#[test]
fn a_stuck_multipart_descriptor_call_is_retired_without_publishing_a_partial_capture() {
    let home = Home::new("gentl-multipart-stuck");
    let producer = producer(&home.root, 71);
    let began = Instant::now();
    let result = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "capture",
            "-c",
            &selector(&producer),
            "-o",
            "stuck.raw",
            "--fresh",
            "0",
            "--timeout",
            "1s",
        ],
    );
    assert_ne!(result.code, Some(0));
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "{}",
        result.stdout
    );
    assert!(fs::read_dir(&home.work).unwrap().next().is_none());
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    let pid = log
        .lines()
        .find(|line| line.ends_with(" part_info"))
        .unwrap()
        .split_once(' ')
        .unwrap()
        .0
        .parse::<i32>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: signal zero only checks whether the known child process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stuck descriptor helper {pid} was not reaped"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn multipart_video_returns_unsupported_instead_of_dropping_components() {
    let home = Home::new("gentl-multipart-video");
    let producer = producer(&home.root, 54);
    let error = run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "record",
            "-c",
            &selector(&producer),
            "-o",
            "clip.mkv",
            "--for",
            "1s",
            "--fps",
            "10",
        ],
    )
    .failure(5);
    assert_eq!(error["code"], "unsupported", "{error}");
    assert!(fs::read_dir(&home.work).unwrap().next().is_none());
}

#[test]
fn gendc_stills_preserve_the_descriptor_all_parts_and_source_clocks() {
    use sha2::{Digest as _, Sha256};

    for (mode, version, hash) in [
        (
            84,
            1,
            "065fb2adff586286b32934b731e586d03aa9b1c5a90822302e3b9b5000413cdf",
        ),
        (
            85,
            2,
            "63d2f9c1cfb5b5c340655782613f6c25765780c36fbab178ace74a4bc397f922",
        ),
    ] {
        let home = Home::new("gentl-gendc");
        let producer = producer(&home.root, mode);
        let captured =
            capture_point_cloud(&home, &producer, &selector(&producer), "image.raw").success();
        assert_eq!(captured["pixel_format"], "Mono8");
        assert_eq!(
            fs::read(home.work.join("image.raw")).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        let archive = fs::read(home.work.join("image.raw.gendc")).unwrap();
        assert_eq!(archive.len(), 474);
        assert_eq!(format!("{:x}", Sha256::digest(&archive)), hash);
        let meta: Value =
            serde_json::from_slice(&fs::read(home.work.join("image.raw.gendc.json")).unwrap())
                .unwrap();
        let layout = &meta["multipart"];
        let container = &layout["gendc"];
        assert_eq!(meta["storage"], "linear_gendc_container");
        assert_eq!(meta["device_ts_ns"], 1_234_000);
        assert_eq!(meta["device_timestamp"]["ticks"], 98_765);
        assert_eq!(layout["data_type_namespace"], "gendc");
        assert_eq!(layout["byte_order"], "little");
        assert_eq!(layout["parts"][1]["data_type"], 0x4100);
        assert_eq!(layout["parts"][1]["original_offset"], 438);
        assert_eq!(layout["parts"][1]["stored_offset"], 438);
        assert_eq!(layout["parts"][2]["data_type"], 0x4000);
        assert_eq!(layout["parts"][2]["gendc"]["component"], 2);
        assert_eq!(container["version"], json!([1, version, 0]));
        assert_eq!(container["descriptor_bytes"], 432);
        assert_eq!(container["id"].as_u64(), Some(0xf123_4567_89ab_cdef));
        assert_eq!(container["timestamp_ptp"], version == 2);
        assert_eq!(
            container["components"][0]["timestamp_ns"],
            9_007_199_254_740_993_u64
        );
        assert_eq!(
            container["components"][1]["timestamp_ns"],
            9_007_199_254_740_995_u64
        );
        assert_eq!(container["components"][2]["type_id"], 0x8001);
        let chosen = &meta["image_selectors"][0];
        assert_eq!(chosen["gendc"]["component"], 1);
        assert_eq!(chosen["gendc"]["part"], 0);
        assert_eq!(chosen["gendc"]["pfnc_code"], COORD3D_ABC32F);
        assert_eq!(chosen["gendc"]["type_id"], 4);
        let camera = chosen["camera"].as_str().unwrap();
        let cloud = capture_point_cloud(&home, &producer, camera, "cloud.raw").success();
        assert_eq!(cloud["pixel_format"], "Coord3D_ABC32f");
        assert_eq!(cloud["width"], 2);
        assert_eq!(cloud["height"], 1);
        assert_eq!(
            fs::read(home.work.join("cloud.raw")).unwrap(),
            archive[438..462]
        );
        assert_eq!(
            fs::read(home.work.join("cloud.raw.gendc")).unwrap(),
            archive
        );
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.work.join("cloud.raw.json")).unwrap()).unwrap();
        assert_eq!(sidecar["sample_layout"]["organization"], "interleaved");
        assert_eq!(sidecar["sample_layout"]["byte_order"], "little");
        assert_eq!(sidecar["multipart"]["storage"], "linear_gendc_container");
        assert_eq!(
            cloud["multipart"][cloud["files"][0].as_str().unwrap()],
            sidecar["multipart"]
        );
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        assert!(log.contains(" gendc_payload\n"));
        assert!(!log.contains(" part_info\n"));
        assert!(!log.contains(" chunk_descriptors\n"));
    }
}

#[test]
fn gendc_selectors_offer_valid_components_and_reject_other_payloads_and_bindings() {
    let home = Home::new("gentl-gendc-selection");
    let gendc = producer(&home.root, 86);
    let base = selector(&gendc);
    let error = capture_point_cloud(&home, &gendc, &base, "invalid.raw").failure(5);
    assert_eq!(error["details"]["gendc"]["components"][0]["invalid"], true);
    let choices = error["details"]["image_selectors"].as_array().unwrap();
    assert_eq!(choices.len(), 1);
    let selected = choices[0]["camera"].as_str().unwrap();
    capture_point_cloud(&home, &gendc, selected, "range.raw").success();
    let legacy = map_selector(&gendc, 1, COORD3D_ABC32F);
    let error = capture_point_cloud(&home, &gendc, &legacy, "legacy.raw").failure(5);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("multipart payload")
    );
    let mut stale: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(selected.strip_prefix("gentl:").unwrap())
            .unwrap(),
    )
    .unwrap();
    stale["gendc"]["source_id"] = json!(11);
    let encode = |value: &Value| {
        format!(
            "gentl:{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
        )
    };
    let error = capture_point_cloud(&home, &gendc, &encode(&stale), "stale.raw").failure(5);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("matching explicit component")
    );
    assert_eq!(error["details"]["image_selectors"], json!(choices));
    let ordinary = producer(&home.root, 0);
    stale["producer"] = json!(ordinary);
    let error = capture_point_cloud(&home, &ordinary, &encode(&stale), "ordinary.raw").failure(5);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("matching multipart or GenDC")
    );
    for invalid in ["invalid", "legacy", "stale", "ordinary"] {
        assert!(!home.work.join(format!("{invalid}.raw")).exists());
        assert!(!home.work.join(format!("{invalid}.raw.gendc")).exists());
    }
    let error = capture_point_cloud(&home, &gendc, selected, "cloud.png").failure(5);
    assert!(error["hint"].as_str().unwrap().contains(".raw"));
    assert!(!home.work.join("cloud.png.gendc").exists());
}

#[test]
fn gendc_rejects_preliminary_unwritten_and_malformed_containers_before_publication() {
    for (mode, exit, message) in [
        (87, 5, "outside its descriptor and parts"),
        (88, 5, "final descriptor"),
        (89, 1, "part ranges are inconsistent"),
        (90, 1, "range overflow"),
        (91, 1, "part ranges are inconsistent"),
        (92, 1, "not completely written"),
        (93, 5, "version must be 1.1 or 1.2"),
        (94, 5, "exactly one supported intensity image"),
        (95, 5, "requires a contiguous buffer"),
    ] {
        let home = Home::new("gentl-gendc-invalid");
        let producer = producer(&home.root, mode);
        let error = capture_point_cloud(&home, &producer, &selector(&producer), "invalid.raw")
            .failure(exit);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "mode {mode}: {error}"
        );
        assert!(
            fs::read_dir(&home.work).unwrap().next().is_none(),
            "mode {mode}"
        );
    }
}

#[test]
fn gendc_coordinate_maps_and_gap_free_buffers_without_intended_size() {
    for mode in [96, 97] {
        let home = Home::new("gentl-gendc-map");
        let producer = producer(&home.root, mode);
        capture_point_cloud(&home, &producer, &selector(&producer), "image.raw").success();
        let meta: Value =
            serde_json::from_slice(&fs::read(home.work.join("image.raw.gendc.json")).unwrap())
                .unwrap();
        if mode == 97 {
            assert!(
                meta["multipart"]["gendc"]
                    .get("intended_buffer_bytes")
                    .is_none()
            );
            continue;
        }
        let choice = &meta["image_selectors"][0];
        assert_eq!(choice["gendc"]["pfnc_code"], COORD3D_C16);
        assert_eq!(choice["gendc"]["header_type"], 0x4200);
        let saved = capture_point_cloud(
            &home,
            &producer,
            choice["camera"].as_str().unwrap(),
            "depth.raw",
        )
        .success();
        assert_eq!(saved["pixel_format"], "Coord3D_C16");
        assert_eq!(
            fs::read(home.work.join("depth.raw")).unwrap(),
            [0x34, 0x12, 0xff, 0xff]
        );
    }
}

#[test]
fn gendc_preserves_automatic_unpadded_selection_and_offers_padded_alternate() {
    let home = Home::new("gentl-gendc-two-intensities");
    let producer = producer(&home.root, 98);
    capture_point_cloud(&home, &producer, &selector(&producer), "default.raw").success();
    assert_eq!(
        fs::read(home.work.join("default.raw")).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
    let default_meta: Value =
        serde_json::from_slice(&fs::read(home.work.join("default.raw.gendc.json")).unwrap())
            .unwrap();
    assert_eq!(default_meta["multipart"]["selected"], 0);
    let choices = default_meta["image_selectors"].as_array().unwrap();
    assert_eq!(choices.len(), 1);
    let padded = &choices[0];
    assert_eq!(padded["gendc"]["component"], 1);
    capture_point_cloud(
        &home,
        &producer,
        padded["camera"].as_str().unwrap(),
        "image.raw",
    )
    .success();
    assert_eq!(fs::read(home.work.join("image.raw")).unwrap(), [0x34, 0x12]);
    let archive = fs::read(home.work.join("image.raw.gendc")).unwrap();
    assert_eq!(
        archive,
        fs::read(home.work.join("default.raw.gendc")).unwrap()
    );
    assert_eq!(&archive[430..434], &[0x34, 0x12, 0xff, 0xff]);
    let meta: Value =
        serde_json::from_slice(&fs::read(home.work.join("image.raw.gendc.json")).unwrap()).unwrap();
    assert_eq!(meta["multipart"]["selected"], 1);
    assert_eq!(meta["multipart"]["gendc"]["components"][1]["type_id"], 1);
    assert_eq!(meta["multipart"]["parts"][1]["gendc"]["x_padding_bytes"], 2);
}

#[test]
fn gendc_padded_rows_and_array_tails_preserve_exact_selected_samples_and_archives() {
    for (mode, format, height, padding, raw, stored) in [
        (
            102,
            "Coord3D_C16",
            2,
            (1, 2),
            [0x34, 0x12, 0xdc, 0xfe, 0xff, 0xff, 0x01, 0x80].as_slice(),
            [
                0x34, 0x12, 0xdc, 0xfe, 0xd1, 0xff, 0xff, 0x01, 0x80, 0xd2, 0xe1, 0xe2,
            ]
            .as_slice(),
        ),
        (
            104,
            "Confidence16",
            1,
            (3, 0),
            [0x34, 0x12, 0xff, 0xff].as_slice(),
            [0x34, 0x12, 0xff, 0xff, 0xf1, 0xf2, 0xf3].as_slice(),
        ),
    ] {
        let home = Home::new("gentl-gendc-padded-measurement");
        let producer = producer(&home.root, mode);
        capture_point_cloud(&home, &producer, &selector(&producer), "image.raw").success();
        assert_eq!(
            fs::read(home.work.join("image.raw")).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        let archive = fs::read(home.work.join("image.raw.gendc")).unwrap();
        let meta: Value =
            serde_json::from_slice(&fs::read(home.work.join("image.raw.gendc.json")).unwrap())
                .unwrap();
        let parts = meta["multipart"]["parts"].as_array().unwrap();
        let image_start = usize::try_from(parts[0]["original_offset"].as_u64().unwrap()).unwrap();
        let map_start = usize::try_from(parts[1]["original_offset"].as_u64().unwrap()).unwrap();
        assert_eq!(
            &archive[image_start..map_start],
            &[1, 2, 3, 0xa1, 0xa2, 4, 5, 6, 0xb1, 0xb2, 0xc1]
        );
        assert_eq!(&archive[map_start..map_start + stored.len()], stored);
        assert_eq!(parts[0]["gendc"]["x_padding_bytes"], 2);
        assert_eq!(parts[0]["gendc"]["y_padding_bytes"], 1);
        assert_eq!(parts[1]["gendc"]["x_padding_bytes"], padding.0);
        assert_eq!(parts[1]["gendc"]["y_padding_bytes"], padding.1);

        let choice = &meta["image_selectors"][0];
        assert_eq!(choice["gendc"]["component"], 1);
        let saved = capture_point_cloud(
            &home,
            &producer,
            choice["camera"].as_str().unwrap(),
            "map.raw",
        )
        .success();
        assert_eq!(saved["pixel_format"], format);
        assert_eq!(saved["width"], 2);
        assert_eq!(saved["height"], height);
        assert_eq!(fs::read(home.work.join("map.raw")).unwrap(), raw);
        assert_eq!(fs::read(home.work.join("map.raw.gendc")).unwrap(), archive);
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.work.join("map.raw.json")).unwrap()).unwrap();
        assert_eq!(sidecar["row_bytes"], 4);
    }
}

#[test]
fn gendc_selected_padding_must_match_the_written_part_length() {
    let home = Home::new("gentl-gendc-invalid-padding");
    let producer = producer(&home.root, 103);
    let camera = format!(
        "gentl:{}",
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "producer":producer, "interface":"if/0;?test", "device":"device:0/test",
                "gendc":{
                    "component":1, "part":0, "type_id":4, "header_type":0x4200,
                    "pfnc_code":COORD3D_C16, "group_id":7, "source_id":8, "region_id":9
                }
            }))
            .unwrap()
        )
    );
    let error = capture_point_cloud(&home, &producer, &camera, "invalid.raw").failure(1);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("exact padded PFNC geometry"),
        "{error}"
    );
    assert!(fs::read_dir(&home.work).unwrap().next().is_none());
}

#[test]
fn gendc_requeues_incomplete_buffers_before_capturing_a_complete_frame() {
    let home = Home::new("gentl-gendc-incomplete");
    let producer = producer(&home.root, 99);
    capture_point_cloud(&home, &producer, &selector(&producer), "image.raw").success();
    assert_eq!(
        fs::read(home.work.join("image.raw")).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" gendc_payload"))
            .count(),
        3
    );
    assert!(log.lines().filter(|line| line.ends_with(" queue")).count() >= 5);
}

#[test]
fn gendc_manifest_cannot_claim_a_companion_path() {
    let home = Home::new("gentl-gendc-manifest");
    let producer = producer(&home.root, 84);
    let camera = selector(&producer);
    for manifest in ["image.raw.gendc", "image.raw.gendc.json"] {
        let error = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "--no-session",
                "capture",
                "-c",
                &camera,
                "-o",
                "image.raw",
                "--manifest",
                manifest,
                "--fresh",
                "0",
                "--timeout",
                "2s",
            ],
        )
        .failure(2);
        assert_eq!(error["code"], "usage", "{error}");
        assert!(fs::read_dir(&home.work).unwrap().next().is_none());
    }
}

#[test]
fn coord3d_ac16_ordinary_and_gendc_stills_keep_exact_profile_words() {
    const AC16: u32 = 0x0220_00bb;
    let expected = [0x34, 0x12, 0xdc, 0xfe, 0xff, 0xff, 0x01, 0x80];
    for mode in [100, 101] {
        let home = Home::new("gentl-coordinate-profile");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let camera = if mode == 101 {
            capture_point_cloud(&home, &producer, &id, "intensity.raw").success();
            let companion: Value = serde_json::from_slice(
                &fs::read(home.work.join("intensity.raw.gendc.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(companion["image_selectors"][0]["gendc"]["pfnc_code"], AC16);
            companion["image_selectors"][0]["camera"]
                .as_str()
                .unwrap()
                .to_owned()
        } else {
            id
        };
        let result = capture_point_cloud(&home, &producer, &camera, "profile.raw").success();
        assert_eq!(result["pixel_format"], "Coord3D_AC16");
        assert_eq!(result["width"], 2);
        assert_eq!(result["height"], 1);
        assert_eq!(result["bit_depth"], 16);
        assert_eq!(fs::read(home.work.join("profile.raw")).unwrap(), expected);
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("profile.raw.json")).unwrap()).unwrap();
        assert_eq!(metadata["pfnc_code"], AC16);
        assert_eq!(metadata["row_bytes"], 8);
        assert_eq!(
            metadata["sample_layout"],
            json!({
                "sample_type": "uint16", "organization": "interleaved",
                "components": ["A", "C"], "byte_order": "little"
            })
        );
        assert!(metadata.get("unit").is_none());
        if mode == 101 {
            let archive = fs::read(home.work.join("profile.raw.gendc")).unwrap();
            assert_eq!(
                archive,
                fs::read(home.work.join("intensity.raw.gendc")).unwrap()
            );
            assert_eq!(archive[438..446], expected);
        }
        let error = capture_point_cloud(&home, &producer, &camera, "profile.png").failure(5);
        assert!(error["message"].as_str().unwrap().contains("Coord3D_AC16"));
        assert!(error["hint"].as_str().unwrap().contains(".raw"));
        assert!(!home.work.join("profile.png").exists());
        assert!(!home.work.join("profile.png.gendc").exists());
    }
}

#[test]
fn named_register_cli_describes_then_roundtrips_exact_banked_bytes() {
    let home = Home::new("gentl-register");
    let producer = producer(&home.root, 121);
    let id = selector(&producer);
    let command = |args: &[&str]| {
        let output = home
            .warm_command()
            .env_remove("GENICAM_GENTL64_PATH")
            .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
            .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
            .args(args)
            .output()
            .unwrap();
        Run::of_output(output, args)
    };
    let descriptor = command(&["--json", "get", "-c", &id, "LUTValueAll", "HugeBuffer"]).success();
    assert_eq!(
        descriptor["values"]["LUTValueAll"],
        json!({"kind":"register","length_bytes":8192})
    );
    assert_eq!(
        descriptor["values"]["HugeBuffer"],
        json!({"kind":"register","length_bytes":1_048_576})
    );
    assert!(
        !fs::read_to_string(home.root.join("sdk.log"))
            .unwrap()
            .contains("register_body_")
    );
    let target = "LUTValueAll[LUTSelector=LUT1]";
    let read = command(&["--json", "debug", "read", "-c", &id, "--feature", target]).success();
    assert_eq!(read["feature"], "LUTValueAll");
    assert_eq!(read["address"], 0xa000);
    assert_eq!(read["bytes"], 8192);
    assert_eq!(
        read["bank"],
        json!([{"selector":"LUTSelector","value":"LUT1"}])
    );
    let expected = (8192..16384)
        .map(|index| ["00", "80", "ff"][index % 3])
        .collect::<String>();
    assert_eq!(read["hex"], expected);
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(log.matches("register_body_read").count(), 1);
    assert_eq!(
        log.matches("register_length").count(),
        2,
        "one descriptor and one named read"
    );
    assert!(log.rfind("register_bank_zero").unwrap() > log.rfind("register_body_read").unwrap());
    let mut bytes = "ff8000".repeat(2730);
    bytes.push_str("ff80");
    let write = command(&[
        "--json",
        "debug",
        "write",
        "-c",
        &id,
        "--feature",
        target,
        "--hex",
        &bytes,
    ])
    .success();
    assert_eq!(write["written"], 8192);
    assert_eq!(
        command(&["--json", "debug", "read", "-c", &id, "--feature", target]).success()["hex"],
        bytes
    );
    let numeric = command(&["--json", "debug", "read", "-c", &id, "0xa000", "3"]).success();
    assert_eq!(numeric["hex"], "ff8000");
    command(&["--json", "set", "-c", &id, "LUTValueAll=00"]).failure(5);
    let before = fs::read_to_string(home.root.join("sdk.log"))
        .unwrap()
        .matches("register_body_write")
        .count();
    command(&[
        "--json",
        "debug",
        "write",
        "-c",
        &id,
        "--feature",
        target,
        "--hex",
        "00",
    ])
    .failure(1);
    command(&[
        "--json",
        "debug",
        "read",
        "-c",
        &id,
        "--feature",
        "LUTValueAll[Width=3]",
    ])
    .failure(2);
    command(&[
        "--json",
        "debug",
        "read",
        "-c",
        &id,
        "--feature",
        "HugeBuffer",
    ])
    .failure(5);
    let after = fs::read_to_string(home.root.join("sdk.log"))
        .unwrap()
        .matches("register_body_write")
        .count();
    assert_eq!(before, after);
    home.run(&["--json", "session", "stop", "auto"]).success();
}

#[test]
fn named_register_cli_deadline_retires_delayed_read_and_write_helpers() {
    for mode in [122, 123] {
        let home = Home::new("gentl-register-cutoff");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let mut args = vec![
            "--json",
            "debug",
            if mode == 122 { "read" } else { "write" },
            "-c",
            &id,
            "--feature",
            "LUTValueAll[LUTSelector=LUT1]",
            "--timeout",
            "100ms",
        ];
        let data = "80".repeat(8192);
        if mode == 123 {
            args.extend(["--hex", &data]);
        }
        let error = run(&home, std::slice::from_ref(&producer), &args).failure(4);
        assert!(error["message"].is_string());
        let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
        let body = if mode == 122 {
            "register_body_read"
        } else {
            "register_body_write"
        };
        let position = log
            .find(body)
            .expect("fixture reached native delayed body call");
        assert!(
            !log.get(position..).unwrap().contains("register_bank_zero"),
            "no selector restore starts after cutoff: {log}"
        );
        assert_eq!(log.matches(body).count(), 1);
        // A fresh explicit operation gets a fresh helper and leaves no stale
        // pending read/write response in the next connection.
        assert_eq!(
            run(
                &home,
                std::slice::from_ref(&producer),
                &["--json", "debug", "read", "-c", &id, "0x200", "4"]
            )
            .success()["hex"],
            "00000000"
        );
    }
}

#[test]
fn full_cap_register_write_uses_rpc_stdin_without_an_argv_hex_payload() {
    let home = Home::new("gentl-register-stdin");
    let producer = producer(&home.root, 124);
    let id = selector(&producer);
    run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "session",
            "start",
            "--name",
            "register-stdin",
            "-c",
            &id,
        ],
    )
    .success();
    let expected = (0..65536)
        .map(|index| ["80", "ff", "00"][index % 3])
        .collect::<String>();
    let request = json!({"op":"write_register","name":{"name":"LUTValueAll"},"hex":expected,"timeout_ms":5000});
    let encoded = serde_json::to_vec(&request).unwrap();
    assert!(encoded.len() < 1024 * 1024);
    assert_eq!(request["hex"].as_str().unwrap().len(), 131_072);
    let path = home.work.join("write-register.json");
    fs::write(&path, &encoded).unwrap();
    let args = [
        "--json",
        "--session",
        "register-stdin",
        "debug",
        "rpc",
        "-c",
        &id,
    ];
    let output = home
        .warm_command()
        .env_remove("GENICAM_GENTL64_PATH")
        .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
        .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
        .args(args)
        .stdin(fs::File::open(&path).unwrap())
        .output()
        .unwrap();
    let written = Run::of_output(output, &args).success();
    assert_eq!(written["feature"], "LUTValueAll");
    assert_eq!(written["address"], 0x8000);
    assert_eq!(written["written"], 65536);
    assert_eq!(written["bank"], json!([]));
    let args = [
        "--json",
        "--session",
        "register-stdin",
        "debug",
        "read",
        "-c",
        &id,
        "--feature",
        "LUTValueAll",
    ];
    let output = home
        .warm_command()
        .env_remove("GENICAM_GENTL64_PATH")
        .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
        .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
        .args(args)
        .output()
        .unwrap();
    let read = Run::of_output(output, &args).success();
    assert_eq!(read["bytes"], 65536);
    assert_eq!(read["hex"], expected);
    let mut oversized = request;
    oversized["hex"] = json!("00".repeat(65537));
    home.run_stdin(
        &serde_json::to_string(&oversized).unwrap(),
        &[
            "--json",
            "--session",
            "register-stdin",
            "debug",
            "rpc",
            "-c",
            &id,
        ],
    )
    .failure(2);
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(
        log.matches("dev_open").count(),
        1,
        "stdin write and readback must share the explicitly held SDK owner"
    );
    assert_eq!(log.matches("register_body_write").count(), 1);
    assert_eq!(log.matches("register_body_read").count(), 1);
    assert!(
        !log.contains("acq_start"),
        "raw RPC does not execute acquisition or file commands"
    );
    home.run(&["--json", "session", "stop", "register-stdin"])
        .success();
}

#[test]
fn banked_opaque_register_is_never_traversed_saved_or_restored_by_presets() {
    let home = Home::new("gentl-register-preset");
    let producer = producer(&home.root, 125);
    let id = selector(&producer);
    run(
        &home,
        std::slice::from_ref(&producer),
        &["--json", "preset", "save", "opaque", "-c", &id],
    )
    .success();
    let mut document = home.run(&["--json", "preset", "show", "opaque"]).success();
    let values = document["values"].as_array().unwrap();
    assert!(values.iter().any(|row| row["feature"] == "LineSelector"));
    assert!(
        values
            .iter()
            .all(|row| row["feature"] != "LUTValueAll" && row["feature"] != "HugeBuffer")
    );
    assert!(
        document["skipped"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|row| row["feature"] != "LUTValueAll")
    );
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert!(
        !log.contains("register_bank_one") && !log.contains("register_bank_zero"),
        "preset save must not traverse a Register's LineSelector banks: {log}"
    );
    assert!(!log.contains("register_body_read") && !log.contains("register_body_write"));
    // A manually supplied scalar must not turn an opaque register into a
    // preset value or trigger selector traversal during preview/application.
    document["values"] = json!([{"feature":"LUTValueAll","value":0,"at":[{"selector":"LineSelector","value":"LUT1"}]}]);
    fs::write(
        home.work.join("forged-register.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let args = [
        "--json",
        "preset",
        "apply",
        "--file",
        "forged-register.json",
        "-c",
        &id,
    ];
    let preview = run(
        &home,
        std::slice::from_ref(&producer),
        &[args.as_slice(), &["--dry-run"]].concat(),
    )
    .success();
    assert_eq!(preview["not_applied"][0]["status"], "refused");
    assert_eq!(
        preview["not_applied"][0]["reason"],
        "not allowed in a preset"
    );
    let applied = run(&home, std::slice::from_ref(&producer), &args).success();
    assert_eq!(applied["applied"], 0);
    assert_eq!(applied["not_applied"][0]["status"], "refused");
    run(
        &home,
        std::slice::from_ref(&producer),
        &[args.as_slice(), &["--strict"]].concat(),
    )
    .failure(5);
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert!(
        !log.contains("register_bank_one") && !log.contains("register_bank_zero"),
        "presets must reject raw values before switching banks: {log}"
    );
    assert!(!log.contains("register_body_read") && !log.contains("register_body_write"));
}

fn ids_literal_samples(twelve: bool) -> (Vec<u8>, Vec<u16>) {
    if twelve {
        (
            vec![
                0x12, 0xab, 0xc3, 0x45, 0xde, 0xf6, 0, 0xff, 0xf0, 0x78, 0x13, 0x59,
            ],
            vec![0x123, 0xabc, 0x456, 0xdef, 0, 0xfff, 0x789, 0x135],
        )
    } else {
        (
            vec![0, 85, 170, 255, 249, 255, 170, 85, 0, 6],
            vec![1, 342, 683, 1023, 1022, 681, 340, 0],
        )
    }
}

#[test]
fn ids_grouped_capture_keeps_source_encoding_and_lossless_png_with_normal_jpeg_and_inspect() {
    for (mode, twelve) in [(126, false), (127, true)] {
        let home = Home::new("gentl-ids-grouped");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let (bytes, samples) = ids_literal_samples(twelve);
        let name = if twelve {
            "Mono12g24IDS"
        } else {
            "Mono10g40IDS"
        };
        let bits = if twelve { 12 } else { 10 };
        let code = if twelve {
            0x4000_001f_u32
        } else {
            0x4000_000f_u32
        };
        let encoding = json!({"namespace":"ids","name":name,"code":code,"gentl_namespace":4});
        let raw = home.root.join("ids.raw");
        let manifest = home.root.join("manifest.jsonl");
        let captured = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                raw.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(fs::read(&raw).unwrap(), bytes);
        assert_eq!(captured["pixel_format"], name);
        assert_eq!(captured["pixel_encoding"], encoding);
        assert_eq!(captured["bit_depth"], bits);
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.root.join("ids.raw.json")).unwrap()).unwrap();
        assert_eq!(sidecar["pixel_encoding"], encoding);
        assert!(!sidecar.as_object().unwrap().contains_key("pfnc_code"));
        assert_eq!(sidecar["row_bytes"], bytes.len() / 2);
        assert_eq!(
            sidecar["sample_layout"]["organization"],
            "grouped_monochrome"
        );
        assert_eq!(sidecar["sample_layout"]["significant_bits"], bits);
        let manifest: Value =
            serde_json::from_str(fs::read_to_string(manifest).unwrap().trim()).unwrap();
        assert_eq!(manifest["pixel_format"], name);
        assert_eq!(manifest["pixel_encoding"], encoding);
        let png_path = home.root.join("ids.png");
        let capture = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                png_path.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(capture["bit_depth"], 16);
        let png = fs::read(png_path).unwrap();
        let mut reader = png::Decoder::new(png.as_slice()).read_info().unwrap();
        assert_eq!(reader.info().color_type, png::ColorType::Grayscale);
        assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
        let mut decoded = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut decoded).unwrap();
        let expected: Vec<_> = samples
            .iter()
            .flat_map(|sample| (sample << (16 - bits)).to_be_bytes())
            .collect();
        assert_eq!(&decoded[..info.buffer_size()], expected);
        #[cfg(feature = "jpeg")]
        {
            let jpeg_path = home.root.join("ids.jpg");
            let jpeg = run(
                &home,
                std::slice::from_ref(&producer),
                &[
                    "--json",
                    "capture",
                    "-c",
                    &id,
                    "-o",
                    jpeg_path.to_str().unwrap(),
                ],
            )
            .success();
            assert_eq!(jpeg["pixel_encoding"], encoding);
            assert_eq!(jpeg["bit_depth"], 8);
            assert_eq!(jpeg["source_bits"], bits);
            let jpeg = fs::read(jpeg_path).unwrap();
            assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        }
        let inspected = run(
            &home,
            std::slice::from_ref(&producer),
            &["--json", "inspect", "-c", &id, "--preview", "none"],
        )
        .success();
        assert_eq!(inspected["frame"]["pixel_format"], name);
        assert_eq!(inspected["frame"]["pixel_encoding"], encoding);
        assert_eq!(inspected["frame"]["bits"], bits);
        assert!(inspected["exposure"].is_object());
        let auto = run(
            &home,
            &[producer],
            &["--json", "auto", "-c", &id, "--timeout", "500ms"],
        )
        .success();
        assert!(auto["mean"].as_f64().is_some(), "{auto}");
    }
}

#[test]
fn ids_grouped_capture_preserves_padding_and_chunk_archive_namespace() {
    for (mode, twelve) in [(136, false), (137, true)] {
        let home = Home::new("gentl-ids-grouped");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let output = home.root.join("ids.raw");
        let (bytes, _) = ids_literal_samples(twelve);
        let result = run(
            &home,
            &[producer],
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                output.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(fs::read(&output).unwrap(), bytes);
        let reference = &result["files"][0];
        assert_eq!(reference.as_str(), output.to_str());
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.root.join("ids.raw.json")).unwrap()).unwrap();
        let chunks = &sidecar["chunks"];
        let metadata: Value =
            serde_json::from_slice(&fs::read(chunks["metadata_path"].as_str().unwrap()).unwrap())
                .unwrap();
        let index = &metadata["chunks"];
        let archive = fs::read(chunks["path"].as_str().unwrap()).unwrap();
        let row = bytes.len() / 2;
        let mut expected = vec![0x91; 2 + (row + 2) * 2 + 1 + 8];
        expected[2..2 + row].copy_from_slice(&bytes[..row]);
        expected[2 + row + 2..2 + row + 2 + row].copy_from_slice(&bytes[row..]);
        let image_end = 2 + (row + 2) * 2 + 1;
        expected[image_end..].fill(0xff);
        assert_eq!(archive, expected);
        assert_eq!(
            index["image_layout"]["pixel_encoding"],
            sidecar["pixel_encoding"]
        );
        assert!(
            !index["image_layout"]
                .as_object()
                .unwrap()
                .contains_key("pfnc_code")
        );
        assert_eq!(index["image_layout"]["offset"], 2);
        assert_eq!(index["image_layout"]["row_stride"], row + 2);
        assert_eq!(index["image_layout"]["padding_y"], 1);
        assert_eq!(index["values"]["values"]["ChunkCounterSample"], u64::MAX);
    }
}

#[test]
fn ids_grouped_capture_refuses_unqualified_or_malformed_layout_before_publication() {
    for (mode, exit, message) in [
        (128, 5, "namespace"),
        (129, 5, "qualified byte layout"),
        (130, 5, "qualified byte layout"),
        (131, 5, "qualified byte layout"),
        (132, 5, "complete byte-aligned rows"),
        (133, 1, "buffer"),
        (134, 5, "namespace"),
        (135, 5, "big-endian"),
    ] {
        let home = Home::new("gentl-ids-grouped");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let output = home.root.join("never.raw");
        let error = run(
            &home,
            &[producer],
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                output.to_str().unwrap(),
            ],
        )
        .failure(exit);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "mode{mode}: {error}"
        );
        assert!(!output.exists(), "mode{mode}");
        assert!(!home.root.join("never.raw.json").exists());
    }
}

#[test]
fn ids_defined_custom_namespace_still_requires_the_qualified_device_xml_layout() {
    for (mode, twelve) in [(138, false), (139, true)] {
        let home = Home::new("gentl-ids-custom-namespace");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let path = home.root.join("ids.raw");
        let capture = run(
            &home,
            &[producer],
            &["--json", "capture", "-c", &id, "-o", path.to_str().unwrap()],
        )
        .success();
        assert_eq!(fs::read(path).unwrap(), ids_literal_samples(twelve).0);
        let expected = json!({"namespace":"ids","name":if twelve {"Mono12g24IDS"} else {"Mono10g40IDS"},"code":if twelve {0x4000_001f_u32} else {0x4000_000f_u32},"gentl_namespace":1000});
        assert_eq!(capture["pixel_encoding"], expected);
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.root.join("ids.raw.json")).unwrap()).unwrap();
        assert_eq!(sidecar["pixel_encoding"], expected);
        assert!(!sidecar.as_object().unwrap().contains_key("pfnc_code"));
    }
    // Wrong-vendor1000 and undefined1001 are in the negative matrix above.
}

// IDS Figure32 diagram-derived grouped mosaics. Synthetic C producer, no physical qualification.
fn ids_bayer_literal(twelve: bool) -> Vec<u8> {
    if twelve {
        vec![
            0xe7, 0x2a, 0x61, 0xb4, 0x58, 0x3d, 0xcf, 0x19, 0xb2, 0xa0, 0x46, 0xf8, 0x36, 0x95,
            0xec, 0x74, 0xd1, 0x92, 0x5a, 0x8f, 0x07, 0x21, 0xbc, 0x53, 0xa9, 0x45, 0x75, 0xf2,
            0x16, 0xb0, 0x8c, 0x63, 0x9d, 0xd8, 0x28, 0xa6, 0x68, 0xc3, 0xf1, 0x1d, 0xa2, 0x64,
            0x47, 0x9b, 0x2e, 0x35, 0xe0, 0x8a,
        ]
    } else {
        vec![
            0xe8, 0x24, 0xb5, 0x78, 0x39, 0xc5, 0x19, 0xaa, 0x55, 0x8d, 0x2d, 0x8e, 0x70, 0xe3,
            0xc6, 0x50, 0xbb, 0x1c, 0xc1, 0x36, 0xb0, 0x44, 0xf7, 0x16, 0x87, 0x7e, 0xa9, 0x21,
            0xd7, 0x78, 0x6a, 0xd5, 0x3b, 0xa0, 0x39, 0xf1, 0x4b, 0x94, 0x25, 0x93,
        ]
    }
}

#[test]
fn ids_bayer_capture_keeps_original_mosaic_descriptor_cfa_and_color_conversions() {
    for (mode, twelve, chunks, namespace) in [
        (140, false, false, 4),
        (141, true, false, 4),
        (142, false, true, 4),
        (143, true, true, 4),
        (144, false, false, 1000),
        (145, true, false, 1000),
    ] {
        let home = Home::new("gentl-ids-bayer");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let name = if twelve {
            "BayerRG12g24IDS"
        } else {
            "BayerRG10g40IDS"
        };
        let bits = if twelve { 12 } else { 10 };
        let code = if twelve {
            0x4000_0011_u32
        } else {
            0x4000_0001_u32
        };
        let encoding =
            json!({"namespace":"ids","name":name,"code":code,"gentl_namespace":namespace});
        let bytes = ids_bayer_literal(twelve);
        let raw = home.root.join("bayer.raw");
        let manifest = home.root.join("manifest.jsonl");
        let captured = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                raw.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(fs::read(&raw).unwrap(), bytes);
        assert_eq!(captured["pixel_encoding"], encoding);
        assert_eq!(captured["pixel_format"], name);
        assert_eq!(captured["bit_depth"], bits);
        let sidecar: Value =
            serde_json::from_slice(&fs::read(home.root.join("bayer.raw.json")).unwrap()).unwrap();
        assert_eq!(sidecar["pixel_encoding"], encoding);
        assert!(sidecar.get("pfnc_code").is_none());
        assert_eq!(
            sidecar["sample_layout"],
            json!({"sample_type":"unsigned","significant_bits":bits,"organization":"grouped_bayer","color_filter":"RGGB","group_pixels":if twelve {2} else {4},"group_bytes":if twelve {3} else {5}})
        );
        assert_eq!(sidecar["row_bytes"], bytes.len() / 4);
        let manifest: Value =
            serde_json::from_str(fs::read_to_string(manifest).unwrap().trim()).unwrap();
        assert_eq!(manifest["pixel_encoding"], encoding);
        if chunks {
            let reference = &sidecar["chunks"];
            let archived = fs::read(reference["path"].as_str().unwrap()).unwrap();
            let row = bytes.len() / 4;
            let mut expected = vec![0x91; 2 + (row + 2) * 4 + 1 + 8];
            for y in 0..4 {
                expected[2 + y * (row + 2)..2 + y * (row + 2) + row]
                    .copy_from_slice(&bytes[y * row..(y + 1) * row]);
            }
            let end = expected.len() - 8;
            expected[end..].fill(0xff);
            assert_eq!(archived, expected);
            let metadata: Value = serde_json::from_slice(
                &fs::read(reference["metadata_path"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(
                metadata["chunks"]["image_layout"]["pixel_encoding"],
                encoding
            );
            assert!(
                metadata["chunks"]["image_layout"]
                    .get("pfnc_code")
                    .is_none()
            );
            assert_eq!(metadata["chunks"]["image_layout"]["offset"], 2);
            assert_eq!(metadata["chunks"]["image_layout"]["row_stride"], row + 2);
            assert_eq!(
                metadata["chunks"]["values"]["values"]["ChunkCounterSample"],
                u64::MAX
            );
        }
        if mode <= 141 {
            let png = home.root.join("bayer.png");
            let saved = run(
                &home,
                std::slice::from_ref(&producer),
                &["--json", "capture", "-c", &id, "-o", png.to_str().unwrap()],
            )
            .success();
            assert_eq!(saved["bit_depth"], 8);
            assert_eq!(saved["source_bits"], bits);
            let png = fs::read(png).unwrap();
            let reader = png::Decoder::new(png.as_slice()).read_info().unwrap();
            assert_eq!(reader.info().color_type, png::ColorType::Rgb);
            assert_eq!(reader.info().bit_depth, png::BitDepth::Eight);
            #[cfg(feature = "jpeg")]
            {
                let jpeg = home.root.join("bayer.jpg");
                let saved = run(
                    &home,
                    std::slice::from_ref(&producer),
                    &["--json", "capture", "-c", &id, "-o", jpeg.to_str().unwrap()],
                )
                .success();
                assert_eq!(saved["pixel_encoding"], encoding);
                assert_eq!(saved["source_bits"], bits);
                assert_eq!(&fs::read(jpeg).unwrap()[..2], &[0xff, 0xd8]);
            }
            let inspected = run(
                &home,
                std::slice::from_ref(&producer),
                &["--json", "-v", "inspect", "-c", &id, "--preview", "none"],
            )
            .success();
            assert_eq!(inspected["frame"]["pixel_encoding"], encoding);
            assert!(inspected["channels"].is_object());
            assert_ne!(inspected["channels"]["red"], inspected["channels"]["blue"]);
            let auto = run(
                &home,
                &[producer],
                &["--json", "auto", "-c", &id, "--timeout", "500ms"],
            )
            .success();
            assert!(auto["mean"].as_f64().is_some(), "{auto}");
        }
    }
}

#[test]
fn ids_bayer_capture_refuses_unqualified_layout_groups_and_contradictory_endian() {
    for (mode, message) in [
        (146, "qualified"),
        (147, "qualified"),
        (148, "complete byte-aligned rows"),
        (149, "big-endian"),
    ] {
        let home = Home::new("gentl-ids-bayer-rejected");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let path = home.root.join("never.raw");
        let error = run(
            &home,
            &[producer],
            &["--json", "capture", "-c", &id, "-o", path.to_str().unwrap()],
        )
        .failure(5);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "mode{mode}: {error}"
        );
        assert!(!path.exists());
        assert!(!home.root.join("never.raw.json").exists());
    }
}

// Manufacturer GR models are advertised; GB/BG positives qualify the documented
// byte/CFA representation only, without an observed device XML or hardware claim.
#[test]
fn ids_other_cfa_capture_preserves_each_declared_filter_and_original_payload() {
    for (mode, name, filter, code, twelve, chunks, namespace) in [
        (
            150,
            "BayerGR10g40IDS",
            "GRBG",
            0x4000_0003_u32,
            false,
            false,
            4,
        ),
        (151, "BayerGR12g24IDS", "GRBG", 0x4000_0013, true, false, 4),
        (152, "BayerGB10g40IDS", "GBRG", 0x4000_0002, false, false, 4),
        (153, "BayerGB12g24IDS", "GBRG", 0x4000_0012, true, false, 4),
        (154, "BayerBG10g40IDS", "BGGR", 0x4000_0004, false, false, 4),
        (155, "BayerBG12g24IDS", "BGGR", 0x4000_0014, true, false, 4),
        (156, "BayerGR10g40IDS", "GRBG", 0x4000_0003, false, true, 4),
        (157, "BayerGR12g24IDS", "GRBG", 0x4000_0013, true, true, 4),
        (158, "BayerGB10g40IDS", "GBRG", 0x4000_0002, false, true, 4),
        (159, "BayerGB12g24IDS", "GBRG", 0x4000_0012, true, true, 4),
        (160, "BayerBG10g40IDS", "BGGR", 0x4000_0004, false, true, 4),
        (161, "BayerBG12g24IDS", "BGGR", 0x4000_0014, true, true, 4),
        (
            166,
            "BayerGR10g40IDS",
            "GRBG",
            0x4000_0003,
            false,
            false,
            1000,
        ),
        (
            167,
            "BayerGR12g24IDS",
            "GRBG",
            0x4000_0013,
            true,
            false,
            1000,
        ),
    ] {
        let home = Home::new("gentl-ids-other-cfa");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let raw = home.root.join("mosaic.raw");
        let manifest = home.root.join("manifest.jsonl");
        let bits = if twelve { 12 } else { 10 };
        let original = ids_bayer_literal(twelve);
        let encoding =
            json!({"namespace":"ids","name":name,"code":code,"gentl_namespace":namespace});
        let captured = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                raw.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(fs::read(&raw).unwrap(), original);
        assert_eq!(captured["pixel_format"], name);
        assert_eq!(captured["pixel_encoding"], encoding);
        assert_eq!(captured["bit_depth"], bits);
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.root.join("mosaic.raw.json")).unwrap()).unwrap();
        assert_eq!(metadata["pixel_encoding"], encoding);
        assert!(metadata.get("pfnc_code").is_none());
        assert_eq!(metadata["row_bytes"], original.len() / 4);
        assert_eq!(
            metadata["sample_layout"],
            json!({"sample_type":"unsigned","significant_bits":bits,"organization":"grouped_bayer","color_filter":filter,"group_pixels":if twelve {2} else {4},"group_bytes":if twelve {3} else {5}})
        );
        let record: Value =
            serde_json::from_str(fs::read_to_string(manifest).unwrap().trim()).unwrap();
        assert_eq!(record["pixel_encoding"], encoding);
        if chunks {
            let reference = &metadata["chunks"];
            let bytes = fs::read(reference["path"].as_str().unwrap()).unwrap();
            let row = original.len() / 4;
            let mut expected = vec![0x91; 2 + (row + 2) * 4 + 1 + 8];
            for y in 0..4 {
                expected[2 + y * (row + 2)..2 + y * (row + 2) + row]
                    .copy_from_slice(&original[y * row..(y + 1) * row]);
            }
            let end = expected.len() - 8;
            expected[end..].fill(0xff);
            assert_eq!(bytes, expected);
            let index: Value = serde_json::from_slice(
                &fs::read(reference["metadata_path"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(index["chunks"]["image_layout"]["pixel_encoding"], encoding);
            assert!(index["chunks"]["image_layout"].get("pfnc_code").is_none());
            assert_eq!(index["chunks"]["image_layout"]["offset"], 2);
            assert_eq!(index["chunks"]["image_layout"]["row_stride"], row + 2);
            assert_eq!(
                index["chunks"]["values"]["values"]["ChunkCounterSample"],
                u64::MAX
            );
        }
        if mode <= 155 {
            let png = home.root.join("mosaic.png");
            let capture = run(
                &home,
                std::slice::from_ref(&producer),
                &["--json", "capture", "-c", &id, "-o", png.to_str().unwrap()],
            )
            .success();
            assert_eq!(capture["pixel_encoding"], encoding);
            assert_eq!(capture["bit_depth"], 8);
            assert_eq!(capture["source_bits"], bits);
            let png = fs::read(png).unwrap();
            let reader = png::Decoder::new(png.as_slice()).read_info().unwrap();
            assert_eq!(reader.info().color_type, png::ColorType::Rgb);
            assert_eq!(reader.info().bit_depth, png::BitDepth::Eight);
        }
        if mode <= 151 {
            #[cfg(feature = "jpeg")]
            {
                let jpeg = home.root.join("mosaic.jpg");
                let capture = run(
                    &home,
                    std::slice::from_ref(&producer),
                    &["--json", "capture", "-c", &id, "-o", jpeg.to_str().unwrap()],
                )
                .success();
                assert_eq!(capture["pixel_encoding"], encoding);
                assert_eq!(capture["source_bits"], bits);
                assert_eq!(&fs::read(jpeg).unwrap()[..2], &[0xff, 0xd8]);
            }
            let inspected = run(
                &home,
                std::slice::from_ref(&producer),
                &["--json", "-v", "inspect", "-c", &id, "--preview", "none"],
            )
            .success();
            assert_eq!(inspected["frame"]["pixel_encoding"], encoding);
            assert!(inspected["channels"].is_object());
            let auto = run(
                &home,
                &[producer],
                &["--json", "auto", "-c", &id, "--timeout", "500ms"],
            )
            .success();
            assert!(auto["mean"].as_f64().is_some(), "{auto}");
        }
    }
}

#[test]
fn ids_other_cfa_capture_retains_strict_identity_symbolic_geometry_and_namespace_gates() {
    for (mode, message) in [
        (162, "qualified"),
        (163, "qualified"),
        (164, "complete byte-aligned rows"),
        (165, "big-endian"),
        (168, "namespace"),
    ] {
        let home = Home::new("gentl-ids-other-cfa-rejected");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let raw = home.root.join("never.raw");
        let error = run(
            &home,
            &[producer],
            &["--json", "capture", "-c", &id, "-o", raw.to_str().unwrap()],
        )
        .failure(5);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "mode{mode}: {error}"
        );
        assert!(!raw.exists());
        assert!(!home.root.join("never.raw.json").exists());
    }
}

// Synthetic CTI vectors implement the six published standard PFNC layouts;
// they are not a physical Basler capture or an observed vendor producer.
fn bicolor_words(bits: u32) -> Vec<u16> {
    let maximum = u16::MAX >> (16 - bits);
    vec![
        0,
        1,
        maximum,
        maximum - 1,
        maximum / 2,
        17,
        33,
        65,
        97,
        129,
        maximum - 7,
        maximum - 9,
    ]
}

fn bicolor_pixels(bits: u32, blue_first: bool) -> Vec<u8> {
    let maximum = u16::MAX >> (16 - bits);
    let mut pixels = [
        [0, 1, maximum],
        [maximum / 4, maximum - 1, maximum],
        [maximum / 2, 17, maximum],
        [33, 65, 97],
        [maximum.midpoint(26), 129, 97],
        [maximum - 7, maximum - 9, 97],
    ];
    if blue_first {
        for pixel in &mut pixels {
            pixel.swap(0, 2);
        }
    }
    pixels
        .into_iter()
        .flatten()
        .map(|sample| u8::try_from(sample >> (bits - 8)).unwrap())
        .collect()
}

#[test]
fn standard_bicolor_captures_preserve_pairs_depth_and_independent_row_phase() {
    for (mode, name, code, bits, blue_first) in [
        (169, "BiColorRGBG8", 0x0210_00a5_u32, 8, false),
        (170, "BiColorBGRG8", 0x0210_00a6, 8, true),
        (171, "BiColorRGBG10", 0x0220_00a7, 10, false),
        (172, "BiColorBGRG10", 0x0220_00a9, 10, true),
        (173, "BiColorRGBG12", 0x0220_00ab, 12, false),
        (174, "BiColorBGRG12", 0x0220_00ad, 12, true),
        // Two byte components do not become a big-endian16-bit word merely
        // because the pixel occupancy is16bits.
        (179, "BiColorRGBG8", 0x0210_00a5, 8, false),
    ] {
        let home = Home::new("gentl-bicolor");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let raw = home.work.join("color.raw");
        let saved = run(
            &home,
            std::slice::from_ref(&producer),
            &["--json", "capture", "-c", &id, "-o", raw.to_str().unwrap()],
        )
        .success();
        let words = bicolor_words(bits);
        let bytes = if bits == 8 {
            words
                .iter()
                .map(|&word| u8::try_from(word).unwrap())
                .collect::<Vec<_>>()
        } else {
            words.iter().flat_map(|word| word.to_le_bytes()).collect()
        };
        assert_eq!(fs::read(&raw).unwrap(), bytes, "mode{mode}");
        assert_eq!(saved["pixel_format"], name);
        assert_eq!(saved["bit_depth"], bits);
        assert!(saved.get("pixel_encoding").is_none());
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("color.raw.json")).unwrap()).unwrap();
        assert_eq!(metadata["pfnc_code"], code);
        assert_eq!(metadata["row_bytes"], if bits == 8 { 6 } else { 12 });
        assert_eq!(metadata["bytes"], bytes.len());
        let png_path = home.work.join("color.png");
        let saved = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                png_path.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(saved["bit_depth"], 8);
        if bits > 8 {
            assert_eq!(saved["source_bits"], bits);
        }
        let png = fs::read(png_path).unwrap();
        let mut png = png::Decoder::new(png.as_slice()).read_info().unwrap();
        assert_eq!(png.info().color_type, png::ColorType::Rgb);
        let mut pixels = vec![0; png.output_buffer_size()];
        let output = png.next_frame(&mut pixels).unwrap();
        pixels.truncate(output.buffer_size());
        assert_eq!(pixels, bicolor_pixels(bits, blue_first), "mode{mode}");
        let inspected = run(
            &home,
            std::slice::from_ref(&producer),
            &["--json", "-v", "inspect", "-c", &id, "--preview", "none"],
        )
        .success();
        assert_eq!(inspected["frame"]["pixel_format"], name);
        assert!(inspected["channels"].is_object());
        #[cfg(feature = "jpeg")]
        if mode == 169 {
            let jpeg_path = home.work.join("color.jpg");
            run(
                &home,
                &[producer],
                &[
                    "--json",
                    "capture",
                    "-c",
                    &id,
                    "-o",
                    jpeg_path.to_str().unwrap(),
                ],
            )
            .success();
            let image =
                turbojpeg::decompress(&fs::read(jpeg_path).unwrap(), turbojpeg::PixelFormat::RGB)
                    .unwrap();
            assert_eq!((image.width, image.height), (3, 2));
        }
    }
}

#[test]
fn standard_bicolor_padding_chunks_and_manifest_keep_original_big_endian_words() {
    let home = Home::new("gentl-bicolor-chunks");
    let producer = producer(&home.root, 178);
    let id = selector(&producer);
    let raw = home.work.join("color.raw");
    let manifest = home.work.join("manifest.jsonl");
    run(
        &home,
        &[producer],
        &[
            "--json",
            "capture",
            "-c",
            &id,
            "-o",
            raw.to_str().unwrap(),
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    )
    .success();
    let words = bicolor_words(12);
    let normalized = words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect::<Vec<_>>();
    assert_eq!(fs::read(raw).unwrap(), normalized);
    let metadata: Value =
        serde_json::from_slice(&fs::read(home.work.join("color.raw.json")).unwrap()).unwrap();
    let reference = &metadata["chunks"];
    let mut original = vec![0x91; 39];
    for y in 0..2 {
        for i in 0..6 {
            let at = 2 + y * 14 + i * 2;
            original[at..at + 2].copy_from_slice(&words[y * 6 + i].to_be_bytes());
        }
    }
    original[31..].fill(0xff);
    assert_eq!(
        fs::read(reference["path"].as_str().unwrap()).unwrap(),
        original
    );
    let index: Value =
        serde_json::from_slice(&fs::read(reference["metadata_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let chunks = &index["chunks"];
    assert_eq!(chunks["image_layout"]["pfnc_code"], 0x0220_00ab);
    assert_eq!(chunks["image_layout"]["offset"], 2);
    assert_eq!(chunks["image_layout"]["row_stride"], 14);
    assert_eq!(chunks["image_layout"]["padding_y"], 1);
    assert_eq!(chunks["image_layout"]["byte_order"], "big");
    assert_eq!(chunks["values"]["values"]["ChunkCounterSample"], u64::MAX);
    let saved: Value = serde_json::from_str(fs::read_to_string(manifest).unwrap().trim()).unwrap();
    assert_eq!(saved["chunks"], *reference);
}

#[test]
fn standard_bicolor_rejects_unknown_wide_endian_truncation_and_custom_namespace() {
    for (mode, exit, message) in [
        (175, 5, "byte order"),
        (176, 1, "image range"),
        (177, 5, "namespace"),
    ] {
        let home = Home::new("gentl-bicolor-invalid");
        let producer = producer(&home.root, mode);
        let raw = home.work.join("never.raw");
        let error = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                raw.to_str().unwrap(),
            ],
        )
        .failure(exit);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "mode{mode}: {error}"
        );
        assert!(!raw.exists());
        assert!(!home.work.join("never.raw.json").exists());
    }
}

fn packed_bicolor_literals() -> Value {
    serde_json::from_str(include_str!("fixtures/bicolor/packed-literals.json")).unwrap()
}

fn packed_literal_bytes(value: &Value) -> Vec<u8> {
    let (pairs, tail) = value.as_str().unwrap().as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn packed_bicolor_capture_keeps_literal_bytes_and_native_depth_on_both_color_orders() {
    let literal = packed_bicolor_literals();
    for (mode, case, code, bits, blue) in [
        (180, 0, 0x0214_00a8_u32, 10, false),
        (181, 0, 0x0214_00aa, 10, true),
        (182, 1, 0x0218_00ac, 12, false),
        (183, 1, 0x0218_00ae, 12, true),
        (190, 2, 0x0218_00ac, 12, false),
        // A byte-packed stream has no multi-byte word unit to reverse or
        // reject when the otherwise standard PFNC producer omits byte order.
        (191, 0, 0x0214_00a8, 10, false),
    ] {
        let case = &literal["cases"][case];
        let descriptor = literal["formats"]
            .as_array()
            .unwrap()
            .iter()
            .find(|format| format["code"] == code)
            .unwrap();
        let name = descriptor["name"].as_str().unwrap();
        let home = Home::new("gentl-packed-bicolor");
        let producer = producer(&home.root, mode);
        let id = selector(&producer);
        let raw = home.work.join("color.raw");
        let saved = run(
            &home,
            std::slice::from_ref(&producer),
            &["--json", "capture", "-c", &id, "-o", raw.to_str().unwrap()],
        )
        .success();
        let original = packed_literal_bytes(&case["tight_hex"]);
        assert_eq!(fs::read(&raw).unwrap(), original, "mode{mode}");
        assert_eq!(saved["pixel_format"], name);
        assert_eq!(saved["bit_depth"], bits);
        assert!(saved.get("pixel_encoding").is_none());
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("color.raw.json")).unwrap()).unwrap();
        assert_eq!(metadata["pfnc_code"], code);
        assert_eq!(metadata["row_bytes"], case["row_bytes"]);
        assert_eq!(metadata["width"], case["width"]);
        assert_eq!(metadata["height"], case["height"]);
        assert_eq!(metadata["bytes"], original.len());
        let mut native: Vec<Vec<[u16; 3]>> =
            serde_json::from_value(case["rgbg_native_rgb"].clone()).unwrap();
        if blue {
            for pixel in native.iter_mut().flatten() {
                pixel.swap(0, 2);
            }
        }
        let expected = native
            .iter()
            .flatten()
            .flatten()
            .map(|word| u8::try_from(word >> (bits - 8)).unwrap())
            .collect::<Vec<_>>();
        let png_path = home.work.join("color.png");
        let saved = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &id,
                "-o",
                png_path.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(saved["bit_depth"], 8);
        assert_eq!(saved["source_bits"], bits);
        let bytes = fs::read(png_path).unwrap();
        let mut png = png::Decoder::new(bytes.as_slice()).read_info().unwrap();
        assert_eq!(png.info().color_type, png::ColorType::Rgb);
        let mut decoded = vec![0; png.output_buffer_size()];
        let output = png.next_frame(&mut decoded).unwrap();
        decoded.truncate(output.buffer_size());
        assert_eq!(decoded, expected, "mode{mode}");
        let inspected = run(
            &home,
            std::slice::from_ref(&producer),
            &["--json", "-v", "inspect", "-c", &id, "--preview", "none"],
        )
        .success();
        assert_eq!(inspected["frame"]["pixel_format"], name);
        assert!(inspected["channels"].is_object());
        #[cfg(feature = "jpeg")]
        if mode == 180 {
            let jpeg_path = home.work.join("color.jpg");
            run(
                &home,
                &[producer],
                &[
                    "--json",
                    "capture",
                    "-c",
                    &id,
                    "-o",
                    jpeg_path.to_str().unwrap(),
                ],
            )
            .success();
            let image =
                turbojpeg::decompress(&fs::read(jpeg_path).unwrap(), turbojpeg::PixelFormat::RGB)
                    .unwrap();
            assert_eq!((image.width, image.height), (4, 2));
        }
    }
}

#[test]
fn packed_bicolor_chunk_archives_preserve_padding_and_manifest_references() {
    let literal = packed_bicolor_literals();
    for (mode, case, code, order) in [
        (184, 0, 0x0214_00a8_u32, "little"),
        (185, 1, 0x0218_00ae, "big"),
    ] {
        let case = &literal["cases"][case];
        let home = Home::new("gentl-packed-bicolor-chunks");
        let producer = producer(&home.root, mode);
        let raw = home.work.join("color.raw");
        let manifest = home.work.join("manifest.jsonl");
        run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                raw.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        )
        .success();
        assert_eq!(
            fs::read(raw).unwrap(),
            packed_literal_bytes(&case["tight_hex"])
        );
        let metadata: Value =
            serde_json::from_slice(&fs::read(home.work.join("color.raw.json")).unwrap()).unwrap();
        let reference = &metadata["chunks"];
        let mut original = packed_literal_bytes(&case["padded_layout"]["payload_hex"]);
        original.extend_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            fs::read(reference["path"].as_str().unwrap()).unwrap(),
            original
        );
        let index: Value = serde_json::from_slice(
            &fs::read(reference["metadata_path"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        let chunks = &index["chunks"];
        let layout = &chunks["image_layout"];
        assert_eq!(layout["pfnc_code"], code);
        assert_eq!(layout["offset"], 2);
        assert_eq!(layout["row_bytes"], case["row_bytes"]);
        assert_eq!(layout["row_stride"], case["padded_layout"]["row_stride"]);
        assert_eq!(layout["padding_y"], 1);
        assert_eq!(layout["byte_order"], order);
        assert_eq!(layout["continuous_packing"], false);
        assert_eq!(chunks["values"]["values"]["ChunkCounterSample"], u64::MAX);
        let saved: Value =
            serde_json::from_str(fs::read_to_string(manifest).unwrap().trim()).unwrap();
        assert_eq!(saved["chunks"], *reference);
    }
}

#[test]
fn packed_bicolor_odd10p_truncation_and_custom_namespace_never_publish_artifacts() {
    for (mode, exit, message) in [
        (186, 5, "even source width"),
        (187, 5, "even source width"),
        (188, 1, "image range"),
        (189, 5, "namespace"),
    ] {
        let home = Home::new("gentl-packed-bicolor-invalid");
        let producer = producer(&home.root, mode);
        let raw = home.work.join("never.raw");
        let error = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "capture",
                "-c",
                &selector(&producer),
                "-o",
                raw.to_str().unwrap(),
            ],
        )
        .failure(exit);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "mode{mode}: {error}"
        );
        assert!(!raw.exists());
        assert!(!home.work.join("never.raw.json").exists());
    }
}

fn selected_job_part(
    home: &Home,
    producer: &Path,
    camera: &str,
    format: &str,
    index: Option<&str>,
    output: &str,
) -> Run {
    let mut args = vec![
        "--json",
        "--no-session",
        "capture",
        "-c",
        camera,
        "-o",
        output,
        "--part-format",
        format,
        "--fresh",
        "0",
        "--warmup",
        "0",
        "--timeout",
        "3s",
        "--manifest",
        "selected.jsonl",
    ];
    if let Some(index) = index {
        args.extend(["--part-index", index]);
    }
    run(home, &[producer.to_owned()], &args)
}

fn job_part_payload(mode: u8) -> Vec<u8> {
    if mode == 198 {
        return vec![0x12, 0x34, 200];
    }
    let mut payload = if mode == 193 {
        vec![
            10, 20, 30, 40, 50, 60, 10, 20, 30, 40, 50, 60, 130, 140, 150, 160, 170, 180,
        ]
    } else {
        vec![10, 20, 10, 20, 30, 40]
    };
    payload.extend([0x12, 0x34, 200, 0xab]);
    payload
}

#[test]
fn per_job_multipart_indices_select_exact_standard_parts_and_keep_every_archive_byte() {
    for (mode, format, index, expected, width, height) in [
        (192, "Mono8", "1", vec![10, 20, 30, 40], 2, 2),
        (
            193,
            "RGB8",
            "1",
            vec![10, 20, 30, 40, 50, 60, 130, 140, 150, 160, 170, 180],
            2,
            2,
        ),
        (197, "Mono8", "0", vec![10, 20], 2, 1),
        (198, "Confidence8", "1", vec![200], 1, 1),
    ] {
        let home = Home::new("gentl-job-part");
        let producer = producer(&home.root, mode);
        let camera = selector(&producer);
        let result =
            selected_job_part(&home, &producer, &camera, format, Some(index), "part.raw").success();
        assert_eq!(result["count"], 1);
        assert_eq!(result["part_format"], format);
        assert_eq!(result["part_index"], index.parse::<u32>().unwrap());
        assert_eq!(result["width"], width);
        assert_eq!(result["height"], height);
        assert_eq!(fs::read(home.work.join("part.raw")).unwrap(), expected);
        assert_eq!(
            fs::read(home.work.join("part.raw.multipart.raw")).unwrap(),
            job_part_payload(mode)
        );
        let metadata: Value = serde_json::from_slice(
            &fs::read(home.work.join("part.raw.multipart.raw.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["multipart"]["selected"],
            index.parse::<u32>().unwrap()
        );
        assert_eq!(metadata["multipart"]["projected_format"], format);
        assert!(metadata["multipart"].get("selection_required").is_none());
        let selected = &metadata["multipart"]["parts"][index.parse::<usize>().unwrap()];
        assert_eq!(selected["namespace"], if mode == 197 { 1 } else { 4 });
        assert_eq!(selected["source_id"], 68);
        assert_eq!(selected["region_id"], 7);
        assert_eq!(
            selected["purpose_id"],
            if mode == 198 {
                6
            } else if index == "1" {
                65_281
            } else {
                1
            }
        );
        assert_eq!(
            selected["original_offset"],
            if mode == 198 {
                48
            } else if index == "1" {
                16
            } else {
                4
            }
        );
        let manifest: Value = serde_json::from_str(
            fs::read_to_string(home.work.join("selected.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["multipart"]["selected"],
            index.parse::<u32>().unwrap()
        );
        assert_eq!(manifest["multipart"]["part_format"], format);
        assert_eq!(manifest["device_ts_ns"], 1_234_000);
        assert_eq!(manifest["frame_id"], 1);
    }
}

#[test]
fn multipart_index_conflicts_are_pure_and_invalid_parts_do_not_publish() {
    for mode in [192, 194] {
        let home = Home::new("gentl-job-part-invalid");
        let producer = producer(&home.root, mode);
        let camera = selector(&producer);
        let error = selected_job_part(
            &home,
            &producer,
            &camera,
            "Mono8",
            if mode == 192 { None } else { Some("1") },
            "bad.raw",
        )
        .failure(5);
        assert!(
            error["message"].as_str().unwrap().contains(if mode == 192 {
                "multiple Mono8"
            } else {
                "namespace"
            }),
            "{error}"
        );
        assert!(!home.work.join("bad.raw").exists());
        assert!(!home.work.join("bad.raw.multipart.raw").exists());
    }
    let home = Home::new("gentl-job-part-binding");
    // This path intentionally does not exist: a pure URI conflict must win before loading it.
    let producer = home.root.join("missing.cti");
    let camera = component_selector(&producer, Some(0));
    let error = selected_job_part(
        &home,
        &producer,
        &camera,
        "Mono8",
        Some("1"),
        "conflict.raw",
    )
    .failure(2);
    assert!(
        error["message"].as_str().unwrap().contains("must match"),
        "{error}"
    );
    assert!(!home.root.join("sdk.log").exists());
    assert!(!home.work.join("selected.jsonl").exists());
    let id = selector(&producer);
    for index in ["32", "-1", "0.5"] {
        let error =
            selected_job_part(&home, &producer, &id, "Mono8", Some(index), "range.raw").failure(2);
        assert!(error["message"].is_string(), "{error}");
    }
}

#[test]
fn warm_multipart_jobs_keep_one_owner_across_default_failure_and_other_part_selection() {
    let home = Home::new("gentl-job-part-warm");
    let producer = producer(&home.root, 192);
    let camera = selector(&producer);
    run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json", "session", "start", "--name", "parts", "-c", &camera,
        ],
    )
    .success();
    let warm = |args: &[&str]| {
        Run::of_output(
            home.warm_command()
                .env_remove("GENICAM_GENTL64_PATH")
                .env("CAPTUREFAB_GENTL_PRODUCERS", &producer)
                .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
                .env("CAPTUREFAB_GENTL_MOCK_LOCK", home.root.join("sdk.lock"))
                .args(args)
                .output()
                .unwrap(),
            args,
        )
    };
    for (index, output, expected) in [
        ("0", "left.raw", vec![10, 20]),
        ("1", "combined.raw", vec![10, 20, 30, 40]),
    ] {
        warm(&[
            "--json",
            "--session",
            "parts",
            "capture",
            "-c",
            &camera,
            "-o",
            output,
            "--part-format",
            "Mono8",
            "--part-index",
            index,
            "--fresh",
            "0",
            "--warmup",
            "0",
            "--timeout",
            "3s",
        ])
        .success();
        assert_eq!(fs::read(home.work.join(output)).unwrap(), expected);
        if index == "0" {
            let error = warm(&[
                "--json",
                "--session",
                "parts",
                "capture",
                "-c",
                &camera,
                "-o",
                "default.raw",
                "--fresh",
                "0",
                "--warmup",
                "0",
                "--timeout",
                "3s",
            ])
            .failure(5);
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("part selection"),
                "{error}"
            );
            assert!(!home.work.join("default.raw").exists());
        }
    }
    home.run(&["--json", "session", "stop", "parts"]).success();
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" dev_open"))
            .count(),
        1,
        "{log}"
    );
    assert!(!log.contains("producer_lock_busy"), "{log}");
    for name in ["left.raw", "combined.raw"] {
        assert_eq!(
            fs::read(home.work.join(format!("{name}.multipart.raw"))).unwrap(),
            job_part_payload(192)
        );
    }
}

#[test]
fn a_later_exact_part_mismatch_retains_prior_burst_files_and_archive_references() {
    for (mode, code) in [(195, 5), (196, 1)] {
        let home = Home::new("gentl-job-part-late");
        let producer = producer(&home.root, mode);
        let camera = selector(&producer);
        let error = run(
            &home,
            std::slice::from_ref(&producer),
            &[
                "--json",
                "--no-session",
                "capture",
                "-c",
                &camera,
                "-o",
                "part-{frame}.raw",
                "-n",
                "2",
                "--every",
                "100ms",
                "--part-format",
                "Mono8",
                "--part-index",
                "1",
                "--fresh",
                "0",
                "--warmup",
                "0",
                "--timeout",
                "3s",
                "--manifest",
                "kept.jsonl",
            ],
        )
        .failure(code);
        let files = error["details"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{error}");
        let path = files[0].as_str().unwrap();
        assert_eq!(fs::read(path).unwrap(), [10, 20, 30, 40]);
        let reference = &error["details"]["multipart"][path];
        assert_eq!(reference["selected"], 1);
        assert_eq!(
            fs::read(reference["path"].as_str().unwrap()).unwrap(),
            job_part_payload(mode)
        );
        assert!(!home.work.join("part-000002.raw").exists());
        let rows = fs::read_to_string(home.work.join("kept.jsonl")).unwrap();
        assert_eq!(rows.lines().count(), 1);
    }
}

#[test]
fn two_industrial_still_sources_project_different_parts_through_one_cti_owner() {
    let home = Home::new("gentl-job-part-data");
    let producer = producer(&home.root, 192);
    let camera = selector(&producer);
    fs::write(home.work.join("sources.json"),json!({"version":1,"sources":[
        {"id":"left","protocol":"camera","camera":camera,"output":"left-{seq}.raw","part_format":"Mono8","part_index":0,"warmup_ms":0,"timeout_ms":5000,"every_ms":3_600_000},
        {"id":"combined","protocol":"camera","camera":camera,"output":"combined-{seq}.raw","part_format":"Mono8","part_index":1,"warmup_ms":0,"timeout_ms":5000,"every_ms":3_600_000}
    ]}).to_string()).unwrap();
    run(
        &home,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "2s",
            "-o",
            "run.jsonl",
        ],
    )
    .success();
    let rows = fs::read_to_string(home.work.join("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for (source, expected, index) in [
        ("left", vec![10, 20], 0),
        ("combined", vec![10, 20, 30, 40], 1),
    ] {
        let row = rows.iter().find(|row| row["source"] == source).unwrap();
        let value = &row["value"];
        assert_eq!(value["pixel_format"], "Mono8");
        assert_eq!(value["multipart"]["selected"], index);
        assert_eq!(fs::read(value["path"].as_str().unwrap()).unwrap(), expected);
        assert_eq!(
            fs::read(value["multipart"]["path"].as_str().unwrap()).unwrap(),
            job_part_payload(192)
        );
        assert!(row["elapsed_ns"].is_u64(), "{row}");
        assert!(row.get("source_time").is_none(), "{row}");
        let metadata: Value = serde_json::from_slice(
            &fs::read(value["multipart"]["metadata_path"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(metadata["received_monotonic_ns"].is_u64());
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(value["received_at"].as_str().unwrap())
                .unwrap()
                .timestamp_millis(),
            metadata["wall_ms"].as_i64().unwrap()
        );
    }
    home.run(&[
        "--json",
        "data",
        "export",
        "run.jsonl",
        "-o",
        "selected.jsonl",
        "--format",
        "jsonl",
    ])
    .success();
    let exported = fs::read_to_string(home.work.join("selected.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for row in rows.iter().filter(|row| row["source"].is_string()) {
        let source = &row["source"];
        assert!(exported.iter().any(|copy| copy["source"] == *source
            && copy["value"]["multipart"] == row["value"]["multipart"]));
    }
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    // A complete selector opens directly, without an extra discovery owner.
    assert_eq!(
        log.lines().filter(|line| line.ends_with(" init")).count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" dev_open"))
            .count(),
        1,
        "{log}"
    );
    assert!(!log.contains("producer_lock_busy"), "{log}");
}

#[test]
fn selected_auto_meters_the_exact_part_and_resets_a_warm_controller_before_capture() {
    let home = Home::new("gentl-job-part-auto");
    let cti = producer(&home.root, 192);
    let camera = selector(&cti);
    run(
        &home,
        std::slice::from_ref(&cti),
        &[
            "--json",
            "session",
            "start",
            "--name",
            "auto-parts",
            "-c",
            &camera,
        ],
    )
    .success();
    let warm = |args: &[&str], input: Option<&Value>| {
        let mut command = home.warm_command();
        command
            .env_remove("GENICAM_GENTL64_PATH")
            .env("CAPTUREFAB_GENTL_PRODUCERS", &cti)
            .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
            .env("CAPTUREFAB_GENTL_MOCK_LOCK", home.root.join("sdk.lock"))
            .args(args);
        if let Some(input) = input {
            let path = home.work.join("auto-request.json");
            fs::write(&path, serde_json::to_vec(input).unwrap()).unwrap();
            command.stdin(fs::File::open(path).unwrap());
        }
        Run::of_output(command.output().unwrap(), args)
    };
    let rpc = [
        "--json",
        "--session",
        "auto-parts",
        "debug",
        "rpc",
        "-c",
        &camera,
    ];
    let request = json!({"op":"auto","part_selection":{"format":"Mono8","index":0}});
    let entered = warm(&rpc, Some(&request)).success();
    assert_eq!(entered["auto"]["metering_part"]["index"], 0);
    assert!(entered["auto"]["brightness"].is_null());
    warm(&rpc, Some(&json!({"op":"start"}))).success();
    let deadline = Instant::now() + time::secs(5);
    loop {
        let status = warm(&rpc, Some(&request)).success();
        if let Some(mean) = status["auto"]["brightness"].as_f64() {
            assert!((mean - 15.0 / 255.0).abs() < 1e-12, "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Switching the active controller must publish a fresh, empty meter.
    let combined = json!({"op":"auto","part_selection":{"format":"Mono8","index":1}});
    let reset = warm(&rpc, Some(&combined)).success();
    assert_eq!(reset["auto"]["metering_part"]["index"], 1);
    assert!(reset["auto"]["brightness"].is_null());
    let deadline = Instant::now() + time::secs(5);
    loop {
        let status = warm(&rpc, Some(&combined)).success();
        if let Some(mean) = status["auto"]["brightness"].as_f64() {
            assert!((mean - 25.0 / 255.0).abs() < 1e-12, "{status}");
            assert_eq!(status["auto"]["strategy"], "software");
            break;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(10));
    }
    // --auto must pass the same explicit selection through prepare/settle.
    // Falling back to default auto would reject the ambiguous representative.
    let captured = warm(
        &[
            "--json",
            "--session",
            "auto-parts",
            "capture",
            "-c",
            &camera,
            "-o",
            "combined-auto.raw",
            "--auto",
            "--part-format",
            "Mono8",
            "--part-index",
            "1",
            "--warmup",
            "0",
            "--fresh",
            "0",
            "--timeout",
            &time::arg(3000),
        ],
        None,
    )
    .success();
    assert_eq!(captured["part_format"], "Mono8");
    assert_eq!(captured["part_index"], 1);
    assert_eq!(
        fs::read(home.work.join("combined-auto.raw")).unwrap(),
        [10, 20, 30, 40]
    );
    assert_eq!(
        fs::read(home.work.join("combined-auto.raw.multipart.raw")).unwrap(),
        job_part_payload(192)
    );
    home.run(&["--json", "session", "stop", "auto-parts"])
        .success();
    let log = fs::read_to_string(home.root.join("sdk.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" dev_open"))
            .count(),
        1,
        "{log}"
    );
    assert!(!log.contains("producer_lock_busy"), "{log}");

    let invalid = Home::new("gentl-job-part-auto-map");
    let missing = invalid.root.join("missing.cti");
    let id = selector(&missing);
    let error = run(
        &invalid,
        std::slice::from_ref(&missing),
        &[
            "--json",
            "capture",
            "-c",
            &id,
            "-o",
            "never.raw",
            "--auto",
            "--set",
            "exposure=1234",
            "--part-format",
            "Mono16",
            "--part-index",
            "1",
        ],
    )
    .failure(5);
    assert!(
        error["message"].as_str().unwrap().contains("ordinary"),
        "{error}"
    );
    assert!(!invalid.root.join("sdk.log").exists());
    assert!(!invalid.work.join("never.raw").exists());

    let bad = Home::new("gentl-job-part-auto-bad-namespace");
    let producer = producer(&bad.root, 194);
    let id = selector(&producer);
    let error = run(
        &bad,
        std::slice::from_ref(&producer),
        &[
            "--json",
            "--no-session",
            "capture",
            "-c",
            &id,
            "-o",
            "never.raw",
            "--auto",
            "--part-format",
            "Mono8",
            "--part-index",
            "1",
            "--warmup",
            "0",
        ],
    )
    .failure(5);
    assert!(
        error["message"].as_str().unwrap().contains("namespace"),
        "{error}"
    );
    assert!(!bad.work.join("never.raw").exists());
    assert!(!bad.work.join("never.raw.multipart.raw").exists());
}
