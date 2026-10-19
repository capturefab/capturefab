//! Exercise the public binary, real subprocess workers, mmap IPC and concurrent capture.
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
struct Fixture {
    child: Child,
    dir: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn cmd(dir: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .args(args)
        .output()
        .unwrap()
}

fn serve_at(dir: &PathBuf, cwd: &PathBuf, name: &str) -> Child {
    serve_env(dir, cwd, name, &[])
}

fn serve_env(dir: &PathBuf, cwd: &PathBuf, name: &str, env: &[(&str, &str)]) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .envs(env.iter().copied())
        .current_dir(cwd)
        .args(["--json", "serve", "--name", name])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["result"]["ready"],
        true,
        "{ready}"
    );
    child
}

#[test]
fn automatic_session_targeting_workflow_paths_and_partial_failure() {
    let root = std::env::temp_dir().join(format!(
        "capturefab-workflow-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let dir = root.join("sessions");
    let caller = root.join("caller");
    let server = root.join("server");
    let plans = root.join("plans");
    for path in [&caller, &server, &plans] {
        std::fs::create_dir_all(path).unwrap();
    }
    let _fixture = Fixture {
        child: serve_at(&dir, &server, "visual"),
        dir: root.clone(),
    };
    let out = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .current_dir(&caller)
        .args([
            "--json",
            "--camera",
            "sim:visible",
            "capture",
            "-o",
            "first.ppm",
            "--set",
            "Width=64",
            "--set",
            "Height=64",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(envelope["session"], "visual");
    assert!(
        std::fs::read(caller.join("first.ppm"))
            .unwrap()
            .starts_with(b"P6\n64 64\n")
    );
    assert!(!server.join("first.ppm").exists());
    let initial = json(&dir, &["--json", "status"]);
    let worker_pid = initial["cameras"][0]["worker_pid"].clone();
    json(&dir, &["--json", "connect", "sim:other"]);
    json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:visible",
            "set",
            "ExposureTime=2000",
        ],
    );
    let status = json(&dir, &["--json", "status"]);
    assert_eq!(status["active_camera"], "sim:other");
    assert!(
        status["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .any(|camera| camera["worker_pid"] == worker_pid)
    );
    assert!(
        status["activity"]
            .as_array()
            .unwrap()
            .iter()
            .any(|activity| activity["operation"] == "capture" && activity["state"] == "succeeded")
    );

    let workflow = plans.join("workflow.json");
    std::fs::write(&workflow, serde_json::to_vec(&serde_json::json!({"version":1,"steps":[
        {"command":{"op":"connect","camera":"sim:workflow","timeout_ms":2000}},
        {"camera":"sim:workflow","command":{"op":"set","feature":"Width","value":"64"}},
        {"camera":"sim:workflow","command":{"op":"set","feature":"Height","value":"64"}},
        {"camera":"sim:workflow","command":{"op":"capture","output":"frames/shot.png","count":1,"timeout_ms":2000,"format":"png"}}
    ]})).unwrap()).unwrap();
    let plan = json(
        &dir,
        &[
            "--json",
            "workflow",
            "apply",
            workflow.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(plan["step_count"], 4);
    assert_eq!(
        json(&dir, &["--json", "status"])["cameras"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let applied = json(
        &dir,
        &["--json", "workflow", "apply", workflow.to_str().unwrap()],
    );
    assert_eq!(applied["completed_steps"], 4);
    assert!(plans.join("frames/shot.png").exists());
    assert!(!server.join("frames/shot.png").exists());

    std::fs::write(&workflow, r#"{"version":1,"steps":[{"camera":"sim:workflow","command":{"op":"set","feature":"Width","value":"128"}},{"command":{"op":"status","typo":true}}]}"#).unwrap();
    let invalid = cmd(
        &dir,
        &["--json", "workflow", "apply", workflow.to_str().unwrap()],
    );
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(
        json(
            &dir,
            &["--json", "--camera", "sim:workflow", "get", "Width"]
        )["features"][0]["value"],
        64
    );
    std::fs::write(&workflow, r#"{"version":1,"steps":[{"camera":"sim:workflow","command":{"op":"set","feature":"Width","value":"96"}},{"camera":"sim:workflow","command":{"op":"set","feature":"NoSuchFeature","value":"1"}},{"camera":"sim:workflow","command":{"op":"stop"}}]}"#).unwrap();
    let failed = cmd(
        &dir,
        &["--json", "workflow", "apply", workflow.to_str().unwrap()],
    );
    assert!(!failed.status.success());
    let failure: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failure["error"]["details"]["completed_steps"], 1);
    assert_eq!(failure["error"]["details"]["failed_step"], 2);
    assert_eq!(
        json(
            &dir,
            &["--json", "--camera", "sim:workflow", "get", "Width"]
        )["features"][0]["value"],
        96
    );
}

#[test]
fn ambiguous_sessions_require_target_and_direct_bypasses_them() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-ambiguous-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = std::env::current_dir().unwrap();
    let _first = Fixture {
        child: serve_at(&dir, &cwd, "first"),
        dir: dir.clone(),
    };
    let _second = Fixture {
        child: serve_at(&dir, &cwd, "second"),
        dir: dir.clone(),
    };
    let out = cmd(&dir, &["--json", "--camera", "sim:0", "set", "Width=64"]);
    assert_eq!(out.status.code(), Some(2));
    let failure: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("multiple sessions")
    );
    // One-shot signal reads need no workbench, so they never ask to choose.
    let read = json(&dir, &["--json", "signals", "read", "sim-signal://line1"]);
    assert!(read.to_string().contains("line1"), "{read}");
    for name in ["first", "second"] {
        assert!(
            json(&dir, &["--json", "--session", name, "status"])["cameras"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        json(
            &dir,
            &["--json", "--direct", "--camera", "sim:0", "get", "Width"]
        )["features"][0]["value"],
        640
    );
    let env_direct = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_SESSION", "first")
        .env("CAPTUREFAB_CAMERA", "sim:environment")
        .args(["--json", "--direct", "get", "Width"])
        .output()
        .unwrap();
    assert!(
        env_direct.status.success(),
        "{}",
        String::from_utf8_lossy(&env_direct.stdout)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&env_direct.stdout).unwrap()["result"]["features"][0]["value"],
        640
    );
    assert!(
        json(&dir, &["--json", "--session", "first", "status"])["cameras"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let env_stdout = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_SESSION", "first")
        .env_remove("CAPTUREFAB_CAMERA")
        .args([
            "--camera",
            "sim:stdout",
            "capture",
            "-o",
            "-",
            "--format",
            "raw",
            "--set",
            "Width=64",
            "--set",
            "Height=64",
        ])
        .output()
        .unwrap();
    assert!(
        env_stdout.status.success(),
        "{}",
        String::from_utf8_lossy(&env_stdout.stderr)
    );
    assert_eq!(env_stdout.stdout.len(), 4096);
    assert!(
        json(&dir, &["--json", "--session", "first", "status"])["cameras"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let schema = json(&dir, &["schema", "--json"]);
    assert!(
        schema["cli"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "workflow")
    );
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn session_recording_duration_stops_original_camera_after_selection_changes() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-duration-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = std::env::current_dir().unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &cwd, "recording"),
        dir: dir.clone(),
    };
    json(&dir, &["--json", "connect", "sim:recording"]);
    json(&dir, &["--json", "set", "Width=64", "Height=64"]);
    json(&dir, &["--json", "connect", "sim:other"]);
    let output = dir.join("clip.mkv");
    let mut recording = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .args([
            "--json",
            "--camera",
            "sim:recording",
            "record",
            "-o",
            output.to_str().unwrap(),
            "--encoder",
            "libx264",
            "--duration",
            "2s",
            "--fps",
            "10",
            "--max-file-size",
            "8MiB",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        if recording.try_wait().unwrap().is_some() {
            let finished = recording.wait_with_output().unwrap();
            panic!(
                "recording exited before it was observed: {}\n{}",
                String::from_utf8_lossy(&finished.stdout),
                String::from_utf8_lossy(&finished.stderr)
            );
        }
        let state = json(&dir, &["--json", "--camera", "sim:recording", "status"]);
        if state["forwarding"].is_string() {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "recording failed to start"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    json(&dir, &["--json", "select", "sim:other"]);
    let finished = recording.wait_with_output().unwrap();
    assert!(
        finished.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&finished.stdout),
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(start.elapsed() >= std::time::Duration::from_secs(2));
    assert!(std::fs::metadata(output).unwrap().len() > 0);
    let state = json(&dir, &["--json", "status"]);
    assert_eq!(state["active_camera"], "sim:other");
    assert!(
        state["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .find(|camera| camera["info"]["id"] == "sim:recording")
            .unwrap()["forwarding"]
            .is_null()
    );
}
fn json(dir: &PathBuf, args: &[&str]) -> Value {
    let out = cmd(dir, args);
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    v["result"].clone()
}

/// A short timed recording that fails before any status shows it running
/// fails the command with the camera's error, even when that error repeats
/// the camera's last one. FFmpeg is missing here on purpose; the encoder
/// fails with the first frame, after the forward reply.
#[test]
fn a_short_timed_recording_that_cannot_start_fails() {
    let root = temp_root("record-cannot-start");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let missing = root.join("no-ffmpeg");
    let env = [("CAPTUREFAB_FFMPEG", missing.to_str().unwrap())];
    let output = root.join("clip.mkv");
    let record = |camera: Option<&str>| {
        let mut args = vec!["--json"];
        args.extend(
            camera
                .map(|camera| ["--camera", camera])
                .into_iter()
                .flatten(),
        );
        args.extend([
            "record",
            "-o",
            output.to_str().unwrap(),
            "--duration",
            "1s",
            "--fps",
            "10",
            "--max-file-size",
            "8MiB",
        ]);
        let out = cmd_env(&dir, &env, &args);
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        assert!(
            !out.status.success() && reply["ok"] == false,
            "{reply}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let message = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(message.starts_with("Forwarding stopped: "), "{reply}");
        assert!(!output.exists());
    };
    // A temporary worker.
    record(Some("sim:0"));
    // A session, twice: the second failure logs the same error again.
    let _fixture = Fixture {
        child: serve_env(&dir, &root, "cannot-start", &env),
        dir: root.clone(),
    };
    json(&dir, &["--json", "connect", "sim:0"]);
    record(None);
    record(None);
}

#[test]
fn network_scan_preserves_active_non_gige_streams_and_retains_report() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-network-ci-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .args([
            "--json",
            "--camera",
            "sim:0",
            "serve",
            "--name",
            "network-ci",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["result"]["ready"],
        true,
        "{ready}"
    );
    let _fixture = Fixture {
        child,
        dir: dir.clone(),
    };
    json(&dir, &["--json", "--session", "network-ci", "start"]);
    let before = json(&dir, &["--json", "--session", "network-ci", "status"]);
    let result = json(
        &dir,
        &[
            "--json",
            "--session",
            "network-ci",
            "--timeout-ms",
            "50",
            "network-scan",
        ],
    );
    let report = &result["network_scan"];
    assert!(report["interfaces"].is_array());
    assert!(report["cameras"].is_array());
    assert!(report["warnings"].is_array());
    assert!(report["scanned_at_ms"].as_u64().unwrap() > 0);
    assert!(report["cameras"].as_array().unwrap().iter().all(
        |camera| camera["camera"]["transport"] == "gig_e"
            && camera["active"] == false
            && camera["tuning"].is_null()
    ));
    let after = json(&dir, &["--json", "--session", "network-ci", "status"]);
    assert_eq!(after["network_scan"], *report);
    assert_eq!(after["connected"]["id"], before["connected"]["id"]);
    assert_eq!(
        after["cameras"][0]["worker_pid"],
        before["cameras"][0]["worker_pid"]
    );
    assert_eq!(after["streaming"], true);
    assert!(after["frames"].as_u64().unwrap() >= before["frames"].as_u64().unwrap());
}

#[test]
fn invalid_inputs_fail_before_creating_camera_resources() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-validation-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for args in [
        vec!["--camera", "sim:0", "capture", "-o", "rtsp://host/stream"],
        vec!["--camera", "sim:0", "capture", "-o", "shot-{unknown}.png"],
        vec![
            "--camera",
            "sim:0",
            "capture",
            "-o",
            "shot.png",
            "--delay",
            "1.1ms",
            "--set",
            "Width=128",
        ],
        vec![
            "--camera", "sim:0", "forward", "-o", "shot.mp4", "--fps", "NaN", "--auto",
        ],
        vec![
            "--camera",
            "sim:0",
            "memory",
            "read",
            "18446744073709551615",
            "2",
        ],
        vec!["--camera", "sim:0", "set", "Width=128", "=bad"],
        vec!["--camera", "gige:gige:192.0.2.1", "get", "Width"],
        vec!["--camera", "sim:0", "serve", "--name", "../bad"],
        vec!["--camera", "sim:0", "xml", "-o", ""],
        vec!["--camera", "sim:0", "gui", "--screenshot", ""],
        vec!["destination", "remove", "../bad"],
        vec!["destination", "test", "../bad"],
        vec![
            "destination",
            "add-s3",
            "cloud",
            "--bucket",
            "bad bucket",
            "--env",
            "--no-check",
        ],
        vec!["destination", "add-s3", "cloud", "--bucket", "bucket"],
    ] {
        let result = cmd(&dir, &[&["--json"][..], args.as_slice()].concat());
        assert_eq!(
            result.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        let response: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(response["error"]["code"], "usage");
        assert!(
            !dir.exists(),
            "invalid input created camera/session resources: {args:?}"
        );
    }
    for (name, value) in [
        ("CAPTUREFAB_FRAME_BYTES", "4097"),
        ("CAPTUREFAB_FRAME_BYTES", "not-a-number"),
        ("CAPTUREFAB_MAX_CAMERAS", "0"),
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .env(name, value)
            .args(["--json", "--camera", "sim:0", "get", "Width"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains(name));
        assert!(
            !dir.exists(),
            "invalid {name} created camera/session resources"
        );
    }
}

#[test]
fn rpc_rejects_ignored_fields_before_accessing_a_session() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!(
        "capturefab-json-validation-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for request in [
        r#"{"op":"status","unknown":1}"#,
        r#"{"op":"status","op":"start"}"#,
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .args(["--json", "--session", "missing", "rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert_eq!(
            result.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        let response: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(response["error"]["code"], "usage");
        assert!(!dir.exists());
    }
}

#[test]
fn live_source_unavailability_keeps_its_operation_category() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-source-validation-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let source = format!("file:{}", dir.join("missing.mp4").display());
    let result = cmd(&dir, &["--json", "--camera", &source, "get", "Width"]);
    assert_eq!(
        result.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["error"]["code"],
        "unavailable"
    );
    assert!(!dir.exists());
    let result = cmd(
        &dir,
        &[
            "--json",
            "--camera",
            "onvif:https://192.0.2.1/onvif/device_service",
            "get",
            "Width",
        ],
    );
    assert_eq!(
        result.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert!(!dir.exists());
    #[cfg(not(feature = "scanner"))]
    {
        let result = cmd(
            &dir,
            &["--json", "--camera", "scanner:sane:test", "get", "Width"],
        );
        assert_eq!(
            result.status.code(),
            Some(5),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(!dir.exists());
    }
    #[cfg(feature = "s3")]
    for command in ["retry", "forget"] {
        let result = cmd(&dir, &["--json", "uploads", command, "bad-id"]);
        assert_eq!(
            result.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(!dir.exists());
    }
}

#[test]
fn undo_redo_camera_session_contract() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-history-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .args(["--json", "serve", "--name", "history-ci"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["ready"],
        true
    );
    let _fixture = Fixture {
        child,
        dir: dir.clone(),
    };
    let session = |args: &[&str]| {
        json(
            &dir,
            &[&["--json", "--session", "history-ci"], args].concat(),
        )
    };
    let camera = |id: &str, args: &[&str]| session(&[&["--camera", id], args].concat());
    let rpc = |id: &str, request: Value| {
        let mut process = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .args(["--json", "--session", "history-ci", "--camera", id, "rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        serde_json::to_writer(process.stdin.take().unwrap(), &request).unwrap();
        let out = process.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    };
    session(&["connect", "sim:0"]);
    session(&["connect", "sim:1"]);
    let original = camera("sim:0", &["get", "Width"])["features"][0]["value"].clone();
    assert!(camera("sim:0", &["history"])["history"]["undo"].is_null());
    let write = camera("sim:0", &["set", "Width=128"]);
    assert_eq!(write["features"][0]["camera_change"]["camera"], "sim:0");
    assert!(
        write["features"][0]["camera_change"]["last_changed_at_us"]
            .as_u64()
            .unwrap()
            > 0
    );
    camera("sim:1", &["set", "Width=256"]);
    let status = session(&["status"]);
    assert_eq!(status["active_camera"], "sim:1");
    for id in ["sim:0", "sim:1"] {
        assert_eq!(camera(id, &["history"])["history"]["undo"], "Set Width");
        let reported = status["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .find(|camera| camera["info"]["id"] == id)
            .unwrap();
        assert_eq!(reported["history"]["undo"], "Set Width");
        if id == "sim:0" {
            assert_eq!(
                reported["history"]["entries"][0]["last_changed_at_us"],
                write["features"][0]["camera_change"]["last_changed_at_us"]
            );
        }
    }
    let plain = cmd(
        &dir,
        &["--session", "history-ci", "--camera", "sim:0", "history"],
    );
    assert!(plain.status.success());
    assert!(String::from_utf8_lossy(&plain.stdout).contains("Undo: Set Width"));
    assert_eq!(camera("sim:0", &["undo"])["undone"], "Set Width");
    assert_eq!(
        camera("sim:0", &["get", "Width"])["features"][0]["value"],
        original
    );
    assert_eq!(
        camera("sim:1", &["get", "Width"])["features"][0]["value"],
        256
    );
    assert_eq!(session(&["status"])["active_camera"], "sim:1");
    let invalid = cmd(
        &dir,
        &[
            "--json",
            "--session",
            "history-ci",
            "--camera",
            "sim:0",
            "set",
            "Width=0",
        ],
    );
    assert!(!invalid.status.success());
    assert_eq!(
        camera("sim:0", &["history"])["history"]["redo"],
        "Set Width"
    );
    assert_eq!(
        rpc("sim:0", serde_json::json!({"op":"redo"}))["redone"],
        "Set Width"
    );
    // Raw writes bypass history; replay must detect and retain the conflicting entry.
    camera("sim:0", &["memory", "write", "0x100", "c0000000"]);
    let conflict = cmd(
        &dir,
        &[
            "--json",
            "--session",
            "history-ci",
            "--camera",
            "sim:0",
            "undo",
        ],
    );
    assert!(!conflict.status.success());
    assert!(
        serde_json::from_slice::<Value>(&conflict.stdout).unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("changed outside this history")
    );
    assert_eq!(
        camera("sim:0", &["history"])["history"]["undo"],
        "Set Width"
    );
    assert_eq!(
        camera("sim:0", &["get", "Width"])["features"][0]["value"],
        192
    );
    camera("sim:0", &["memory", "write", "0x100", "80000000"]);
    rpc("sim:0", serde_json::json!({"op":"undo"}));
    camera("sim:0", &["set", "Width=160"]);
    assert!(camera("sim:0", &["history"])["history"]["redo"].is_null());
    let no_redo = cmd(
        &dir,
        &[
            "--json",
            "--session",
            "history-ci",
            "--camera",
            "sim:0",
            "redo",
        ],
    );
    assert!(!no_redo.status.success());
    camera("sim:0", &["start"]);
    assert_eq!(camera("sim:0", &["undo"])["undone"], "Start stream");
    assert_eq!(camera("sim:0", &["status"])["streaming"], false);
    camera("sim:0", &["redo"]);
    assert_eq!(camera("sim:0", &["status"])["streaming"], true);
    camera("sim:0", &["disconnect"]);
    session(&["connect", "sim:0"]);
    assert!(camera("sim:0", &["history"])["history"]["undo"].is_null());
    assert_eq!(
        camera("sim:1", &["history"])["history"]["undo"],
        "Set Width"
    );
    #[cfg(all(
        feature = "scanner",
        any(target_os = "linux", target_os = "windows", target_os = "macos")
    ))]
    {
        // Scanner settings are local registers; opening and editing an invented
        // device ID never starts a driver helper or moves a real scanner.
        let scanner = match std::env::consts::OS {
            "linux" => "scanner:sane:HISTORY-TEST",
            "windows" => "scanner:wia:HISTORY-TEST",
            "macos" => "scanner:ica:HISTORY-TEST",
            _ => unreachable!(),
        };
        session(&["connect", scanner]);
        camera(scanner, &["set", "ScanResolution=300"]);
        assert_eq!(camera(scanner, &["undo"])["undone"], "Set ScanResolution");
        assert_eq!(
            camera(scanner, &["get", "ScanResolution"])["features"][0]["value"],
            150
        );
        camera(scanner, &["redo"]);
        assert_eq!(
            camera(scanner, &["get", "ScanResolution"])["features"][0]["value"],
            300
        );
        for (setting, field, original) in [
            ("ScanMode=Gray", "ScanMode", "Color"),
            ("ScanSource=Feeder", "ScanSource", "Default"),
        ] {
            camera(scanner, &["set", setting]);
            camera(scanner, &["undo"]);
            assert_eq!(
                camera(scanner, &["get", field])["features"][0]["value"],
                original
            );
        }
        assert_eq!(camera(scanner, &["status"])["frames"], 0);
    }
    for action in ["undo", "redo", "history"] {
        let direct = cmd(&dir, &["--json", "--direct", "--camera", "sim:0", action]);
        assert_eq!(direct.status.code(), Some(2));
        assert!(
            serde_json::from_slice::<Value>(&direct.stdout).unwrap()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--session")
        );
    }
}

#[test]
fn parallel_camera_workers_and_public_cli() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-cli-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .args(["--json", "serve", "--name", "ci"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["ready"],
        true
    );
    let fixture = Fixture {
        child,
        dir: dir.clone(),
    };
    let a = json(&dir, &["--json", "--session", "ci", "connect", "sim:0"]);
    let b = json(&dir, &["--json", "--session", "ci", "connect", "sim:1"]);
    assert_ne!(a["worker_pid"], b["worker_pid"]);
    assert_ne!(a["worker_pid"], fixture.child.id());
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "set",
            "Width=128",
        ],
    );
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:1",
            "set",
            "Width=256",
        ],
    );
    let a = json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "get",
            "Width",
        ],
    );
    let b = json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:1",
            "get",
            "Width",
        ],
    );
    assert_eq!(a["features"][0]["value"], 128);
    assert_eq!(b["features"][0]["value"], 256);
    let outa = dir.join("a.png");
    let outb = dir.join("b.png");
    let da = dir.clone();
    let db = dir.clone();
    let pa = outa.to_string_lossy().to_string();
    let pb = outb.to_string_lossy().to_string();
    let ta = std::thread::spawn(move || {
        json(
            &da,
            &[
                "--json",
                "--session",
                "ci",
                "--camera",
                "sim:0",
                "capture",
                "-o",
                &pa,
            ],
        )
    });
    let tb = std::thread::spawn(move || {
        json(
            &db,
            &[
                "--json",
                "--session",
                "ci",
                "--camera",
                "sim:1",
                "capture",
                "-o",
                &pb,
            ],
        )
    });
    ta.join().unwrap();
    tb.join().unwrap();
    assert!(
        std::fs::read(&outa)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    assert!(
        std::fs::read(&outb)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    let existing = cmd(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            outa.to_str().unwrap(),
        ],
    );
    assert!(!existing.status.success());
    let status = json(&dir, &["--json", "--session", "ci", "status"]);
    assert_eq!(status["cameras"].as_array().unwrap().len(), 2);
    // Targeted status must not report another camera's identity.
    let targeted = json(
        &dir,
        &["--json", "--session", "ci", "--camera", "sim:0", "status"],
    );
    assert_eq!(targeted["connected"]["id"], "sim:0");
    let sequence = dir.join("scheduled");
    let result = json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            sequence.to_str().unwrap(),
            "-n",
            "3",
            "--delay",
            "150ms",
            "--interval",
            "100ms",
        ],
    );
    let id = result["job"]["id"].as_u64().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let jobs = json(
            &dir,
            &["--json", "--session", "ci", "--camera", "sim:0", "jobs"],
        );
        let job = jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == id)
            .unwrap();
        if job["status"] == "complete" {
            assert_eq!(job["captured"], 3);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "schedule did not complete: {job}"
        );
        assert_ne!(job["status"], "failed", "schedule failed: {job}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(std::fs::read_dir(sequence).unwrap().count(), 3);
    let cancelled = dir.join("cancelled.png");
    let result = json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            cancelled.to_str().unwrap(),
            "--delay",
            "1h",
        ],
    );
    let id = result["job"]["id"].as_u64().unwrap().to_string();
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "cancel",
            &id,
        ],
    );
    assert!(!cancelled.exists());
    // Existing owned files from both cameras count against a smaller new budget.
    let full = cmd(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            dir.join("full.png").to_str().unwrap(),
            "--max-space",
            "1B",
        ],
    );
    assert_eq!(
        full.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&full.stdout).unwrap()["error"]["code"],
        "storage_full"
    );
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:0",
            "disconnect",
        ],
    );
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "--camera",
            "sim:1",
            "disconnect",
        ],
    );
    let raw = cmd(
        &dir,
        &[
            "--camera",
            "sim:raw",
            "capture",
            "-o",
            "-",
            "-f",
            "raw",
            "--set",
            "Width=64",
            "--set",
            "Height=64",
        ],
    );
    assert!(
        raw.status.success(),
        "{}",
        String::from_utf8_lossy(&raw.stderr)
    );
    assert_eq!(raw.stdout.len(), 4096);
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|e| e
            .unwrap()
            .path()
            .extension()
            .is_none_or(|s| s != "shm")),
        "shared ring resources must be released"
    );
}
#[test]
fn auto_mode_session_contract() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-auto-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .args([
            "--json", "--camera", "sim:0", "serve", "--name", "auto-ci", "--auto",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["ready"],
        true
    );
    let _fixture = Fixture {
        child,
        dir: dir.clone(),
    };
    let session = |args: &[&str]| json(&dir, &[&["--json", "--session", "auto-ci"], args].concat());
    let status = session(&["status"]);
    assert_eq!(status["auto"]["balance"], 0.5);
    assert!(status["cameras"][0]["auto"].is_object());
    assert_eq!(
        session(&["auto", "--balance", "0.25"])["auto"]["balance"],
        0.25
    );
    assert_eq!(session(&["auto"])["auto"]["balance"], 0.25);
    assert_eq!(session(&["status"])["cameras"][0]["auto"]["balance"], 0.25);
    let before_width = session(&["get", "Width"])["features"][0]["value"].clone();
    for args in [
        vec![
            "capture",
            "-o",
            "rtsp://host/capture",
            "--set",
            "Width=128",
            "--auto",
            "--balance",
            "quality",
        ],
        vec![
            "capture",
            "-o",
            "bad.png",
            "--delay",
            "wrong",
            "--set",
            "Width=128",
            "--auto",
            "--balance",
            "quality",
        ],
        vec![
            "forward",
            "-o",
            "bad.mp4",
            "--fps",
            "NaN",
            "--auto",
            "--balance",
            "quality",
        ],
        vec!["set", "ExposureTime=NaN"],
    ] {
        let invalid = cmd(
            &dir,
            &[&["--json", "--session", "auto-ci"][..], args.as_slice()].concat(),
        );
        assert!(!invalid.status.success(), "{args:?}");
        assert_eq!(
            session(&["status"])["auto"]["balance"],
            0.25,
            "invalid input changed auto mode: {args:?}"
        );
        assert_eq!(
            session(&["get", "Width"])["features"][0]["value"],
            before_width,
            "invalid input changed a feature: {args:?}"
        );
    }
    assert!(session(&["manual"])["auto"].is_null());
    assert!(session(&["status"])["auto"].is_null());
    let saved = dir.join("session.png");
    session(&[
        "capture",
        "--auto",
        "--balance",
        "quality",
        "-o",
        saved.to_str().unwrap(),
    ]);
    assert!(saved.exists());
    assert_eq!(session(&["status"])["auto"]["balance"], 0.0);
    let usage = cmd(
        &dir,
        &["--json", "--session", "auto-ci", "auto", "--balance", "2"],
    );
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&usage.stdout).unwrap()["error"]["code"],
        "usage"
    );
    let direct = cmd(&dir, &["--json", "--direct", "--camera", "sim:0", "auto"]);
    assert_eq!(direct.status.code(), Some(2));
    assert!(
        serde_json::from_slice::<Value>(&direct.stdout).unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--session")
    );
    let tuned = dir.join("tuned.png");
    json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "--auto",
            "-o",
            tuned.to_str().unwrap(),
        ],
    );
    assert!(
        std::fs::read(&tuned)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
}

#[test]
fn camera_types_propagate_through_workers_and_preserve_native_capture() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-camera-types-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .args(["--json", "serve", "--name", "types-ci"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["ready"],
        true,
        "{line}"
    );
    let fixture = Fixture {
        child,
        dir: dir.clone(),
    };
    let mut worker_pids = Vec::new();
    for (selector, kind, source, control, height, bytes_per_pixel) in [
        (
            "sim:0",
            "area_scan",
            "device_metadata",
            "ExposureTime",
            64,
            1,
        ),
        (
            "sim:line",
            "line_scan",
            "device_metadata",
            "AcquisitionLineRate",
            16,
            1,
        ),
        (
            "sim:depth",
            "depth",
            "features",
            "Scan3dCoordinateScale",
            64,
            2,
        ),
        (
            "sim:microscope",
            "microscope",
            "features",
            "FocusPosition",
            64,
            1,
        ),
        (
            "sim:thermal",
            "thermal",
            "features",
            "TemperatureLinearResolution",
            64,
            2,
        ),
    ] {
        let connected = json(
            &dir,
            &["--json", "--session", "types-ci", "connect", selector],
        );
        assert_eq!(
            connected["connected"]["profile"]["kind"], kind,
            "{selector}"
        );
        assert_eq!(
            connected["connected"]["profile"]["source"], source,
            "{selector}"
        );
        assert!(
            !connected["connected"]["profile"]["evidence"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let pid = connected["worker_pid"].as_u64().unwrap();
        assert_ne!(pid, u64::from(fixture.child.id()));
        assert!(!worker_pids.contains(&pid));
        worker_pids.push(pid);

        let info = json(
            &dir,
            &[
                "--json",
                "--session",
                "types-ci",
                "--camera",
                selector,
                "info",
            ],
        );
        assert_eq!(info["camera"]["profile"]["kind"], kind);
        assert!(
            info["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature["name"] == control)
        );
        if kind == "line_scan" {
            assert!(
                !info["features"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|feature| feature["name"] == "AcquisitionFrameRate")
            );
        }
        let dimensions = format!("Height={height}");
        let output = dir.join(format!("{kind}.raw"));
        json(
            &dir,
            &[
                "--json",
                "--session",
                "types-ci",
                "--camera",
                selector,
                "capture",
                "-o",
                output.to_str().unwrap(),
                "-f",
                "raw",
                "--set",
                "Width=64",
                "--set",
                &dimensions,
            ],
        );
        let raw = std::fs::read(&output).unwrap();
        assert_eq!(raw.len(), 64 * height * bytes_per_pixel, "{selector}");
        if bytes_per_pixel == 2 {
            let samples: Vec<_> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            assert!(
                samples.iter().any(|sample| *sample > 255),
                "{selector}: scalar samples were reduced to a preview"
            );
            assert!(samples.windows(2).any(|pair| pair[0] != pair[1]));
        }

        if ["line_scan", "depth", "thermal"].contains(&kind) {
            let automatic = cmd(
                &dir,
                &[
                    "--json",
                    "--session",
                    "types-ci",
                    "--camera",
                    selector,
                    "auto",
                ],
            );
            assert_eq!(
                automatic.status.code(),
                Some(5),
                "{selector}: {}",
                String::from_utf8_lossy(&automatic.stdout)
            );
            let error: Value = serde_json::from_slice(&automatic.stdout).unwrap();
            assert_eq!(error["error"]["code"], "unsupported");
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("automatic image tuning")
            );
        }
        let status = json(
            &dir,
            &[
                "--json",
                "--session",
                "types-ci",
                "--camera",
                selector,
                "status",
            ],
        );
        assert_eq!(
            status["connected"]["profile"]["kind"], kind,
            "profile lost after capture"
        );
    }
    let all = json(&dir, &["--json", "--session", "types-ci", "status"]);
    assert_eq!(all["cameras"].as_array().unwrap().len(), 5);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn discovery_reports_native_backend_failure_without_hiding_other_cameras() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-discovery-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_FFMPEG", dir.join("missing-ffmpeg"))
        .args(["--json", "serve", "--name", "discovery"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["result"]["ready"],
        true
    );
    let _fixture = Fixture {
        child,
        dir: dir.clone(),
    };
    let output = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_FFMPEG", dir.join("missing-ffmpeg"))
        .args([
            "--json",
            "--session",
            "discovery",
            "--simulate",
            "--timeout-ms",
            "100",
            "discover",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["ok"], true);
    assert!(
        response["result"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                let text = warning.as_str().unwrap();
                text.contains("Native camera discovery") && text.contains("cannot launch FFmpeg")
            }),
        "{response}"
    );
    assert!(
        response["result"]["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|device| device["id"] == "sim:0"),
        "{response}"
    );
    json(
        &dir,
        &["--json", "--session", "discovery", "connect", "sim:0"],
    );
    // A connected worker refreshes its own logs; coordinator diagnostics must
    // survive these snapshots as well as the initial discovery response.
    json(&dir, &["--json", "--session", "discovery", "get", "Width"]);
    let status = json(&dir, &["--json", "--session", "discovery", "status"]);
    assert!(
        status["logs"].as_array().unwrap().iter().any(|entry| {
            entry["level"] == "warn"
                && entry["message"]
                    .as_str()
                    .unwrap()
                    .contains("Native camera discovery")
        }),
        "{status}"
    );
}
#[test]
fn stale_session_descriptor_is_replaced_but_live_one_is_kept() {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-stale-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let serve = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .args(["--json", "serve", "--name", "stale"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (child, line)
    };
    let (mut first, ready) = serve();
    assert!(ready.contains("\"ready\":true"), "{ready}");
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(dir.join("stale.json").exists());
    let (second, ready) = serve();
    assert!(ready.contains("\"ready\":true"), "{ready}");
    let _fixture = Fixture {
        child: second,
        dir: dir.clone(),
    };
    let (mut third, refused) = serve();
    assert!(refused.contains("already exists"), "{refused}");
    third.wait().unwrap();
}

// ---------------------------------------------------------------------------
// Network destinations (FTP, SFTP, S3 presets) and the upload queue.

#[path = "support/fake_ftp.rs"]
mod fake_ftp;

fn temp_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "capturefab-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Run the binary with extra environment variables; never touches the
/// keychain because every destination here uses environment secrets.
fn cmd_env(dir: &PathBuf, env: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env_remove("CAPTUREFAB_FTP_PASSWORD")
        .env_remove("CAPTUREFAB_SFTP_PASSWORD")
        .env("CAPTUREFAB_SESSION_DIR", dir);
    for (name, value) in env {
        command.env(name, value);
    }
    command.args(args).output().unwrap()
}

fn json_env(dir: &PathBuf, env: &[(&str, &str)], args: &[&str]) -> Value {
    let out = cmd_env(dir, env, args);
    assert!(
        out.status.success(),
        "{args:?}\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
}

#[test]
fn destination_server_inputs_are_usage_errors_before_touching_settings() {
    let dir = temp_root("destination-validation");
    for args in [
        &[
            "destination",
            "add-ftp",
            "x",
            "--host",
            "ftp://h",
            "--user",
            "u",
        ][..],
        &[
            "destination",
            "add-ftp",
            "x",
            "--host",
            "h",
            "--user",
            "u",
            "--port",
            "0",
        ],
        &[
            "destination",
            "add-ftp",
            "x",
            "--host",
            "h",
            "--user",
            "u",
            "--dir",
            "../a",
        ],
        &[
            "destination",
            "add-ftp",
            "x",
            "--host",
            "h",
            "--user",
            "a b",
        ],
        &["destination", "add-sftp", "x", "--host", "h", "--user", "u"],
        &[
            "destination",
            "add-sftp",
            "x",
            "--host",
            "h",
            "--user",
            "u",
            "--host-key",
            "bogus",
        ],
        &[
            "destination",
            "add-sftp",
            "x",
            "--host",
            "h",
            "--user",
            "u",
            "--accept-host-key",
            "--no-check",
        ],
        &[
            "destination",
            "add-s3",
            "x",
            "--bucket",
            "b",
            "--env",
            "--preset",
            "rustfs",
        ],
        &[
            "destination",
            "add-s3",
            "x",
            "--bucket",
            "bucket",
            "--env",
            "--preset",
            "aws",
        ],
    ] {
        let result = cmd(&dir, &[&["--json"][..], args].concat());
        assert_eq!(
            result.status.code(),
            Some(2),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let response: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(response["error"]["code"], "usage", "{args:?}");
        assert!(
            !dir.exists(),
            "invalid input touched the session dir: {args:?}"
        );
    }
}

#[test]
fn ftp_and_sftp_destinations_save_list_and_remove_without_secrets() {
    let dir = temp_root("server-destinations");
    let saved = json_env(
        &dir,
        &[],
        &[
            "--json",
            "destination",
            "add-ftp",
            "line1",
            "--host",
            "10.0.0.5",
            "--user",
            "cam",
            "--dir",
            "incoming",
            "--plain",
            "--password-env",
            "--no-check",
        ],
    );
    let ftp = &saved["destination"];
    assert_eq!(ftp["kind"], "ftp");
    assert_eq!(ftp["port"], 21);
    assert_eq!(ftp["tls"], "plain");
    assert_eq!(ftp["password"], "environment");
    assert_eq!(ftp["location"], "ftp://10.0.0.5:21/incoming");
    assert_eq!(ftp["insecure"], true);
    assert!(saved.get("check").is_none());
    let key = format!("SHA256:{}", "x".repeat(43));
    let saved = json_env(
        &dir,
        &[],
        &[
            "--json",
            "destination",
            "add-sftp",
            "nas",
            "--host",
            "nas.local",
            "--port",
            "2222",
            "--user",
            "cam",
            "--dir",
            "/srv/drop",
            "--host-key",
            &key,
            "--no-check",
        ],
    );
    assert_eq!(saved["destination"]["kind"], "sftp");
    assert_eq!(saved["destination"]["password"], "none");
    assert_eq!(
        saved["destination"]["location"],
        "sftp://nas.local:2222//srv/drop"
    );
    assert_eq!(saved["destination"]["insecure"], false);
    assert_eq!(saved["host_key"]["trust"], "pinned");
    assert_eq!(saved["host_key"]["fingerprint"], key.as_str());
    let listed = json(&dir, &["--json", "destination", "list"]);
    let rows = listed["destinations"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(!row["location"].as_str().unwrap().contains("cam"), "{row}");
        assert!(row.get("secret_saved").is_none(), "{row}");
    }
    let text = std::fs::read_to_string(dir.join("destinations.json")).unwrap();
    assert!(!text.contains("secret"));
    // A missing password variable is reported by the uploading process.
    let test = cmd_env(&dir, &[], &["--json", "destination", "test", "line1"]);
    assert_eq!(test.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&test.stdout).contains("CAPTUREFAB_FTP_PASSWORD is not set"));
    let probe = cmd(
        &dir,
        &["--json", "destination", "test", "nas", "--probe-writes"],
    );
    assert_eq!(probe.status.code(), Some(2));
    for name in ["line1", "nas"] {
        let removed = json(
            &dir,
            &["--json", "destination", "remove", name, "--forget-secret"],
        );
        assert_eq!(removed["removed"], name);
    }
    assert!(
        json(&dir, &["--json", "destination", "list"])["destinations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn direct_capture_to_unreachable_ftp_destination_stays_queued() {
    let root = temp_root("ftp-unreachable");
    let (dir, staging) = (root.join("sessions"), root.join("staging"));
    let catalog = root.join("catalog.duckdb");
    let env = [
        ("CAPTUREFAB_STAGING_DIR", staging.to_str().unwrap()),
        ("CAPTUREFAB_FTP_PASSWORD", "secret"),
    ];
    json_env(
        &dir,
        &env,
        &[
            "--json",
            "destination",
            "add-ftp",
            "down",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--user",
            "cam",
            "--plain",
            "--password-env",
            "--no-check",
        ],
    );
    let result = json_env(
        &dir,
        &env,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            "shot.png",
            "--destination",
            "down",
            "--dataset",
            catalog.to_str().unwrap(),
        ],
    );
    let uploads = &result["uploads"];
    assert_eq!(uploads["pending"], 1, "{result}");
    assert!(
        uploads["last_error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("connection refused"),
        "{uploads}"
    );
    assert!(staging.join("down/shot.png").is_file());
    let status = json_env(&dir, &env, &["--json", "uploads"]);
    assert_eq!(status["items"][0]["remote"]["kind"], "ftp");
    assert!(status["items"][0].get("bucket").is_none());
    assert_eq!(status["completed_seq"], 0);
    // The queued upload's id, key and URL reach the dataset catalog.
    let queued = &result["queued"][0];
    assert_eq!(queued["key"], "shot.png", "{result}");
    assert_eq!(queued["url"], "ftp://127.0.0.1:1/shot.png");
    let rows = dataset_rows(
        &dir,
        &catalog,
        "SELECT destination, object_key, upload_id, remote_url, uploaded_ms FROM captures",
    );
    assert_eq!(
        rows,
        serde_json::json!([["down", "shot.png", queued["upload_id"], queued["url"], null]])
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn direct_capture_uploads_to_loopback_ftp_and_removes_staged_copy() {
    use sha2::{Digest, Sha256};
    let fake = fake_ftp::FakeFtp::start(|_| {});
    let root = temp_root("ftp-upload");
    let (dir, staging) = (root.join("sessions"), root.join("staging"));
    let env = [
        ("CAPTUREFAB_STAGING_DIR", staging.to_str().unwrap()),
        ("CAPTUREFAB_FTP_PASSWORD", "secret"),
    ];
    let port = fake.port.to_string();
    // A refused sign-in is not saved: every upload would fail.
    let add = |name: &str| {
        vec![
            "--json".to_string(),
            "destination".into(),
            "add-ftp".into(),
            name.into(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            port.clone(),
            "--user".into(),
            "cam".into(),
            "--dir".into(),
            "dir".into(),
            "--plain".into(),
            "--password-env".into(),
        ]
    };
    let refused_args = add("refused");
    let refused = cmd_env(
        &dir,
        &[
            ("CAPTUREFAB_STAGING_DIR", staging.to_str().unwrap()),
            ("CAPTUREFAB_FTP_PASSWORD", "wrong"),
        ],
        &refused_args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stdout).contains("the server check failed"));
    let listed = json_env(&dir, &env, &["--json", "destination", "list"]);
    assert!(!listed.to_string().contains("refused"), "{listed}");
    let saved = json_env(
        &dir,
        &env,
        &[
            "--json",
            "destination",
            "add-ftp",
            "line1",
            "--host",
            "127.0.0.1",
            "--port",
            &port,
            "--user",
            "cam",
            "--dir",
            "dir",
            "--plain",
            "--password-env",
        ],
    );
    assert_eq!(saved["check"]["ok"], true, "{saved}");
    let result = json_env(
        &dir,
        &env,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            "shot.png",
            "--destination",
            "line1",
        ],
    );
    assert_eq!(result["uploads"]["pending"], 0, "{result}");
    assert_eq!(result["uploads"]["failed"], 0, "{result}");
    let received = fake
        .file(&format!("{}/dir/shot.png", fake_ftp::HOME))
        .expect("uploaded");
    let sha = format!("{:x}", Sha256::digest(&received));
    let status = json_env(&dir, &env, &["--json", "uploads"]);
    assert_eq!(status["completed_seq"], 1);
    let completed = &status["recent_completed"][0];
    assert_eq!(completed["sha256"], sha.as_str());
    assert_eq!(completed["kind"], "ftp");
    assert_eq!(completed["key"], "shot.png");
    assert_eq!(
        completed["url"],
        format!("ftp://127.0.0.1:{port}/dir/shot.png").as_str()
    );
    assert!(
        !staging.join("line1/shot.png").exists(),
        "staged copy removed"
    );
    assert!(fake.commands().iter().any(|c| c == "TYPE I"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn legacy_s3_upload_queue_lists_unchanged() {
    let dir = temp_root("legacy-queue");
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let item = serde_json::json!({
        "id": "0123456789abcdef",
        "destination": "archive",
        "bucket": {"endpoint": "https://storage.example", "region": "auto", "bucket": "camera-archive", "prefix": "", "path_style": true, "credentials": {"source": "environment"}, "keep_local": true},
        "path": dir.join("shot.png"),
        "key": "line-1/shot.png",
        "bytes": 10,
        "sha256": "a".repeat(64),
        "queued": 0,
        "attempts": 3,
        "next_attempt": 0,
        "state": "failed",
        "error": "previous failure"
    });
    std::fs::write(
        dir.join("uploads.json"),
        serde_json::to_vec(&serde_json::json!({"version": 1, "items": [item], "uploaded": 4, "uploaded_bytes": 40, "failures": 1})).unwrap(),
    )
    .unwrap();
    let status = json(&dir, &["--json", "uploads"]);
    assert_eq!(status["failed"], 1);
    assert_eq!(status["uploaded"], 4);
    let listed = &status["items"][0];
    assert_eq!(listed["bucket"], item["bucket"]);
    assert!(listed.get("remote").is_none());
    let rewritten: Value =
        serde_json::from_slice(&std::fs::read(dir.join("uploads.json")).unwrap()).unwrap();
    assert_eq!(rewritten["items"][0], item, "queue keeps the legacy shape");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_reports_destination_features() {
    let dir = temp_root("doctor-destinations");
    let doctor = json(&dir, &["--json", "doctor"]);
    for (feature, compiled) in [
        ("s3", cfg!(feature = "s3")),
        ("ftp", cfg!(feature = "ftp")),
        ("sftp", cfg!(feature = "sftp")),
    ] {
        assert_eq!(
            doctor["features"][feature].as_bool(),
            Some(compiled),
            "{feature}"
        );
    }
    let schema = json(&dir, &["--json", "schema"]);
    let text = schema.to_string();
    for command in ["add-ftp", "add-sftp", "probe_writes", "accept_host_key"] {
        assert!(text.contains(command), "{command}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn dataset_rows(dir: &PathBuf, file: &std::path::Path, sql: &str) -> Value {
    json(
        dir,
        &[
            "--json",
            "dataset",
            "query",
            "--file",
            file.to_str().unwrap(),
            sql,
        ],
    )["rows"]
        .clone()
}

#[test]
fn dataset_records_direct_captures() {
    let root = temp_root("dataset-direct");
    let dir = root.join("sessions");
    let db = root.join("d.duckdb");
    for name in ["a.png", "b.png"] {
        let output = root.join(name);
        json(
            &dir,
            &[
                "--json",
                "--camera",
                "sim:0",
                "capture",
                "-o",
                output.to_str().unwrap(),
                "--dataset",
                db.to_str().unwrap(),
            ],
        );
    }
    let rows = dataset_rows(
        &dir,
        &db,
        "SELECT count(*), min(width) > 0, min(origin), max(camera_id) FROM captures",
    );
    assert_eq!(rows, serde_json::json!([[2, true, "capture", "sim:0"]]));
    // The catalog's SHA-256 is the one the storage ledger recorded.
    let sha = dataset_rows(&dir, &db, "SELECT sha256 FROM captures ORDER BY id LIMIT 1")[0][0]
        .as_str()
        .unwrap()
        .to_owned();
    let ledger = std::fs::read_to_string(dir.join("storage.json")).unwrap();
    assert!(ledger.contains(&sha), "{sha}");
    let status = json(
        &dir,
        &[
            "--json",
            "dataset",
            "status",
            "--file",
            db.to_str().unwrap(),
        ],
    );
    assert_eq!(status["captures"], 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dataset_records_schedule_and_session_notes_and_locks_the_file() {
    let root = temp_root("dataset-session");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &root, "ds"),
        dir: root.clone(),
    };
    let db = root.join("d.duckdb");
    let db_text = db.to_str().unwrap();
    let opened = json(
        &dir,
        &["--json", "--session", "ds", "dataset", "open", db_text],
    );
    assert_eq!(opened["captures"], 0);
    json(&dir, &["--json", "--session", "ds", "connect", "sim:0"]);
    let frames = root.join("frames");
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ds",
            "capture",
            "-n",
            "3",
            "--interval",
            "200ms",
            "-o",
            frames.to_str().unwrap(),
            "--context",
            "line=L1",
        ],
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = json(&dir, &["--json", "--session", "ds", "dataset", "status"]);
        if status["captures"] == 3 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let note = json(
        &dir,
        &[
            "--json",
            "--session",
            "ds",
            "dataset",
            "note",
            "shift B",
            "--context",
            "line=L1",
        ],
    );
    assert!(note["id"].as_u64().is_some());
    let status = json(&dir, &["--json", "--session", "ds", "dataset", "status"]);
    assert!(status["events"].as_u64().unwrap() >= 2, "{status}");
    let rows = json(
        &dir,
        &[
            "--json",
            "--session",
            "ds",
            "dataset",
            "query",
            "SELECT origin, count(*), count(job_id), min(context) FROM captures GROUP BY 1",
        ],
    )["rows"]
        .clone();
    assert_eq!(
        rows,
        serde_json::json!([["schedule", 3, 3, r#"{"line":"L1"}"#]])
    );
    // Bad SQL is a usage error through a session too, not only with --file.
    for sql in ["SELEC 1", "DELETE FROM captures", "SELECT * FROM nope"] {
        let out = cmd(
            &dir,
            &["--json", "--session", "ds", "dataset", "query", sql],
        );
        assert_eq!(out.status.code(), Some(2), "{sql}");
    }
    // The session holds the file: a direct read is refused and points to it.
    let out = cmd(
        &dir,
        &["--json", "dataset", "query", "--file", db_text, "SELECT 1"],
    );
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("--session"));
    // A second --dataset for a different file is refused, not switched.
    let other = root.join("other.duckdb");
    let out = cmd(
        &dir,
        &[
            "--json",
            "--session",
            "ds",
            "capture",
            "-o",
            root.join("x.png").to_str().unwrap(),
            "--dataset",
            other.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success() && !other.exists());
    let closed = json(&dir, &["--json", "--session", "ds", "dataset", "close"]);
    assert_eq!(closed["closed"]["captures"], 3);
    assert_eq!(
        dataset_rows(&dir, &db, "SELECT count(*) FROM captures")[0][0],
        3
    );
}

#[test]
fn dataset_export_bundle_never_overwrites() {
    let root = temp_root("dataset-export");
    let dir = root.join("sessions");
    let db = root.join("d.duckdb");
    json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            root.join("a.png").to_str().unwrap(),
            "--dataset",
            db.to_str().unwrap(),
        ],
    );
    let out_dir = root.join("bundle");
    let export = [
        "--json",
        "dataset",
        "export",
        "--file",
        db.to_str().unwrap(),
        "-o",
        out_dir.to_str().unwrap(),
        "--format",
        "csv",
    ];
    let result = json(&dir, &export);
    assert_eq!(result["tables"]["captures"], 1);
    for name in [
        "manifest.json",
        "README.md",
        "captures.csv",
        "captures_with_signals.csv",
    ] {
        assert!(out_dir.join(name).is_file(), "{name}");
    }
    let before: Vec<_> = ["manifest.json", "README.md", "captures.csv"]
        .iter()
        .map(|name| std::fs::read(out_dir.join(name)).unwrap())
        .collect();
    let out = cmd(&dir, &export);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("already exists"));
    let after: Vec<_> = ["manifest.json", "README.md", "captures.csv"]
        .iter()
        .map(|name| std::fs::read(out_dir.join(name)).unwrap())
        .collect();
    assert_eq!(before, after);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dataset_rejects_bad_inputs_exit_2() {
    let root = temp_root("dataset-usage");
    let dir = root.join("sessions");
    let db = root.join("d.duckdb");
    json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            root.join("a.png").to_str().unwrap(),
            "--dataset",
            db.to_str().unwrap(),
        ],
    );
    let file = db.to_str().unwrap();
    let bad_ext = root.join("d.db");
    let export = root.join("e");
    for args in [
        vec![
            "dataset",
            "query",
            "--file",
            bad_ext.to_str().unwrap(),
            "SELECT 1",
        ],
        vec![
            "dataset", "query", "--file", file, "--limit", "0", "SELECT 1",
        ],
        vec![
            "dataset",
            "export",
            "--file",
            file,
            "-o",
            export.to_str().unwrap(),
            "--tables",
            "nope",
        ],
        vec![
            "dataset",
            "query",
            "--file",
            file,
            "DELETE FROM captures; SELECT 1",
        ],
        vec!["dataset", "open", "--new", "--existing", file],
        vec!["dataset", "status"],
        vec!["dataset", "open", file],
        vec!["dataset", "note", "x", "--context", "Bad=1"],
    ] {
        let mut full = vec!["--json"];
        full.extend(args.iter().copied());
        let out = cmd(&dir, &full);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    assert!(!export.exists());
    assert_eq!(
        dataset_rows(&dir, &db, "SELECT count(*) FROM captures")[0][0],
        1
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Needs FFmpeg: a recording stopped by --duration or Ctrl-C is cataloged.
#[cfg(unix)]
#[test]
#[ignore]
fn dataset_recordings_are_cataloged_on_duration_and_ctrl_c() {
    let root = temp_root("dataset-record");
    let dir = root.join("sessions");
    let db = root.join("d.duckdb");
    std::fs::create_dir_all(&root).unwrap();
    json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "record",
            "-o",
            root.join("a.mp4").to_str().unwrap(),
            "--duration",
            "2s",
            "--dataset",
            db.to_str().unwrap(),
        ],
    );
    let child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .args([
            "--json",
            "--camera",
            "sim:0",
            "record",
            "-o",
            root.join("b.mp4").to_str().unwrap(),
            "--dataset",
            db.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success() || output.status.code() == Some(130));
    let rows = dataset_rows(
        &dir,
        &db,
        "SELECT file_name, bytes > 0, end_ms > start_ms FROM recordings ORDER BY file_name",
    );
    assert_eq!(
        rows,
        serde_json::json!([["a.mp4", true, true], ["b.mp4", true, true]])
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Needs FFmpeg: a templated recording picks a free name in the worker,
/// reports it as `path`, and the catalog records the same file and context.
#[test]
#[ignore]
fn record_template_expands() {
    let root = temp_root("record-template");
    let dir = root.join("sessions");
    let db = root.join("d.duckdb");
    let template = root.join("r/{camera}-{seq}.mp4");
    let mut paths = Vec::new();
    for _ in 0..2 {
        let result = json(
            &dir,
            &[
                "--json",
                "--camera",
                "sim:0",
                "record",
                "-o",
                template.to_str().unwrap(),
                "--duration",
                "1s",
                "--dataset",
                db.to_str().unwrap(),
                "--context",
                "line=L1",
            ],
        );
        assert!(result["since_ms"].as_u64().is_some(), "{result}");
        let path = PathBuf::from(result["path"].as_str().unwrap());
        assert!(path.is_file(), "{result}");
        paths.push(path.file_name().unwrap().to_str().unwrap().to_owned());
    }
    assert!(paths[0].ends_with("-0001.mp4") && paths[1].ends_with("-0002.mp4"));
    assert!(!paths[0].contains('{'), "{paths:?}");
    let rows = dataset_rows(
        &dir,
        &db,
        "SELECT file_name, context::JSON->>'line' FROM recordings ORDER BY file_name",
    );
    assert_eq!(
        rows,
        serde_json::json!([[paths[0], "L1"], [paths[1], "L1"]])
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn naming_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "capturefab-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn capture_outputs_number_templates_and_carry_context() {
    let root = naming_root("naming-cli");
    let dir = root.join("sessions");
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let capture = |args: &[&str]| {
        let mut all = vec!["--json", "--camera", "sim:0", "capture"];
        all.extend_from_slice(args);
        cmd(&dir, &all)
    };
    let files = |output: &std::process::Output| -> Vec<String> {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        v["result"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_owned())
            .collect()
    };
    // A fixed name is refused once taken, unless numbering is asked for.
    let file = out.join("capture.png");
    let file = file.to_str().unwrap();
    files(&capture(&["-o", file]));
    let again = capture(&["-o", file]);
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stdout).contains("output already exists"));
    assert_eq!(
        files(&capture(&["-o", file, "--on-exists", "number"])),
        [out.join("capture-0001.png").to_str().unwrap()]
    );
    // Templates expand per frame; {seq} continues.
    let template = out.join("d/{date}/{camera}-{seq}.png");
    for n in 1..=2 {
        let output = capture(&["-o", template.to_str().unwrap(), "--context", "line=L1"]);
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        let path = std::path::Path::new(v["result"]["files"][0].as_str().unwrap());
        assert!(path.exists());
        assert!(
            path.to_str().unwrap().ends_with(&format!("-{n:04}.png")),
            "{path:?}"
        );
        let folder = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(folder.len(), 10, "{folder}");
        assert_eq!(v["result"]["frames"][0]["file"], v["result"]["files"][0]);
        assert!(v["result"]["frames"][0]["wall_ms"].as_u64().unwrap() > 0);
        assert_eq!(v["result"]["context"]["line"], "L1");
    }
    // A directory continues its numbering.
    let frames = out.join("frames");
    for _ in 0..2 {
        files(&capture(&["-o", frames.to_str().unwrap(), "-n", "2"]));
    }
    assert_eq!(std::fs::read_dir(&frames).unwrap().count(), 4);
    // Bad context is a usage error before any session state exists.
    let fresh = root.join("fresh-sessions");
    for bad in [
        &["--context", "Bad=1"][..],
        &["--context", "a=1", "--context", "a=2"],
    ] {
        let mut all = vec!["--json", "--camera", "sim:0", "capture", "-o", file];
        all.extend_from_slice(bad);
        assert_eq!(cmd(&fresh, &all).status.code(), Some(2), "{bad:?}");
    }
    assert!(!fresh.exists());
    let _ = std::fs::remove_dir_all(&root);
}

fn signals_json(dir: &PathBuf, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    json_env(dir, &[], &all)
}

#[test]
fn signals_read_and_browse_simulator() {
    let root = temp_root("signals-sim");
    let dir = root.join("sessions");
    let read = signals_json(&dir, &["signals", "read", "sim-signal://line1"]);
    let samples = read["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 6, "{read}");
    assert!(samples.iter().any(|s| s["tag"] == "line1/temp_c"));
    assert!(read["id"].as_str().unwrap().starts_with("signal:"));
    let browse = signals_json(
        &dir,
        &[
            "signals",
            "browse",
            "sim-signal://line1",
            "--duration",
            "1s",
        ],
    );
    assert_eq!(browse["items"].as_array().unwrap().len(), 6, "{browse}");
    let limited = signals_json(
        &dir,
        &[
            "signals",
            "browse",
            "sim-signal://line1?tags=9",
            "--limit",
            "4",
        ],
    );
    assert_eq!(limited["items"].as_array().unwrap().len(), 4);
    assert_eq!(limited["truncated"], true);
    let doctor = signals_json(&dir, &["doctor"]);
    for feature in ["modbus", "mqtt", "opcua", "mtconnect", "s3"] {
        assert!(doctor["features"][feature].is_boolean(), "{feature}");
    }
    assert_eq!(doctor["signals"]["sim-signal"], true);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn signals_usage_errors_and_unsupported_browse() {
    let root = temp_root("signals-usage");
    let dir = root.join("sessions");
    for args in [
        &["signals", "read", "ftp://x"][..],
        &["signals", "read", "modbus-tcp://h/1/holding/70000"],
        &["signals", "browse", "sim-signal://a", "--duration", "61s"],
        &["log", "sim-signal://a", "--duration", "1s"],
        &["log", "sim-signal://a", "--csv", "out.txt"],
    ] {
        let out = cmd(&dir, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    let out = cmd(&dir, &["signals", "start", "sim-signal://a"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a running session"));
    let out = cmd(
        &dir,
        &["signals", "browse", "modbus-tcp://127.0.0.1/1/holding/0"],
    );
    assert_eq!(out.status.code(), Some(5));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn signal_credentials_are_redacted() {
    let root = temp_root("signals-redact");
    let dir = root.join("sessions");
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let uri = format!("mqtt://user:secret@127.0.0.1:{port}/plant/#");
    let out = cmd(&dir, &["--json", "signals", "read", &uri]);
    assert!(matches!(out.status.code(), Some(3 | 4)), "{out:?}");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!all.contains("secret"), "{all}");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(feature = "modbus")]
#[test]
fn signals_read_modbus_from_loopback_fake() {
    let root = temp_root("signals-modbus");
    let dir = root.join("sessions");
    let fake = capturefab::signals::modbus::fake::serve(
        std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
    );
    // 123.456 as f32 in cdab word order, then 1.5.
    fake.set_registers(3, 0, &[0xE979, 0x42F6, 0x0000, 0x3FC0]);
    let port = fake.address().port();
    let read = signals_json(
        &dir,
        &[
            "signals",
            "read",
            &format!(
                "modbus-tcp://127.0.0.1:{port}/1/holding/0?count=2&type=f32&order=cdab&name=temp"
            ),
        ],
    );
    let samples = read["samples"].as_array().unwrap();
    assert_eq!(samples[0]["tag"], "temp[0]");
    assert!((samples[0]["value"].as_f64().unwrap() - 123.456).abs() < 1e-3);
    assert_eq!(samples[1]["value"], 1.5);
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let out = cmd(
        &dir,
        &[
            "signals",
            "read",
            &format!("modbus-tcp://127.0.0.1:{closed}/1/holding/0"),
        ],
    );
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn signals_session_lifecycle() {
    let root = temp_root("signals-session");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture {
        child: serve_at(&dir, &root, "sig"),
        dir: root.clone(),
    };
    let session = ["--json", "--session", "sig"];
    let run = |args: &[&str]| {
        let mut all = session.to_vec();
        all.extend_from_slice(args);
        cmd(&dir, &all)
    };
    let out = run(&[
        "signals",
        "start",
        "sim-signal://line1",
        "--interval",
        "100ms",
    ]);
    assert!(out.status.success(), "{out:?}");
    let started: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = started["result"]["id"].as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid = loop {
        let status: Value = serde_json::from_slice(&run(&["signals"]).stdout).unwrap();
        let source = &status["result"]["sources"][0];
        if source["samples"].as_u64().unwrap_or(0) >= 5 && source["state"] == "running" {
            assert_eq!(status["result"]["persisting"], false);
            break source["helper_pid"].as_u64().unwrap();
        }
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let snapshot: Value = serde_json::from_slice(&run(&["status"]).stdout).unwrap();
    assert!(
        snapshot["result"]["signals"][0]["tags"]
            .as_array()
            .unwrap()
            .len()
            <= 32
    );
    assert_eq!(
        run(&["signals", "start", "sim-signal://line1"])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(
        run(&["signals", "stop", "signal:0000000000000000"])
            .status
            .code(),
        Some(3)
    );
    let stopped: Value = serde_json::from_slice(&run(&["signals", "stop", &id]).stdout).unwrap();
    assert_eq!(stopped["result"]["stopped"][0]["state"], "stopped");
    let status: Value = serde_json::from_slice(&run(&["signals"]).stdout).unwrap();
    assert!(status["result"]["sources"].as_array().unwrap().is_empty());
    // A source still running when the session dies exits with it.
    assert!(
        run(&["signals", "start", "sim-signal://line2"])
            .status
            .success()
    );
    let status: Value = serde_json::from_slice(&run(&["signals"]).stdout).unwrap();
    let pid2 = status["result"]["sources"][0]["helper_pid"]
        .as_u64()
        .unwrap();
    assert_ne!(pid, pid2);
    drop(fixture);
    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while Command::new("kill")
            .args(["-0", &pid2.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "helper {pid2} outlived its session"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[test]
fn log_simulator_to_csv_and_dataset() {
    let root = temp_root("signals-log");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let csv = root.join("out.csv");
    let dataset = root.join("d.duckdb");
    let csv_arg = csv.to_string_lossy().into_owned();
    let dataset_arg = dataset.to_string_lossy().into_owned();
    let log = signals_json(
        &dir,
        &[
            "log",
            "sim-signal://line1",
            "--interval",
            "100ms",
            "--duration",
            "1s",
            "--csv",
            &csv_arg,
            "--dataset",
            &dataset_arg,
        ],
    );
    let written = log["written"].as_u64().unwrap();
    assert!(written >= 30, "{log}");
    assert_eq!(log["sources"][0]["state"], "stopped");
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(text.starts_with("ts_ms,source_ts_ms,source_id,tag,value,quality\n"));
    assert_eq!(text.lines().count() as u64, written + 1);
    let rows = signals_json(
        &dir,
        &[
            "dataset",
            "query",
            "--file",
            &dataset_arg,
            "SELECT count(*) FROM samples",
        ],
    );
    assert_eq!(rows["rows"][0][0].as_u64(), Some(written));
    let out = cmd(
        &dir,
        &[
            "log",
            "sim-signal://line1",
            "--duration",
            "1s",
            "--csv",
            &csv_arg,
        ],
    );
    // An existing output is an operation error, as for capture and export.
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&csv).unwrap(), text);
    // A run that fails before logging anything leaves no header-only CSV,
    // so the same command works once the source is reachable.
    let failed = root.join("refused.csv");
    let out = cmd(
        &dir,
        &[
            "log",
            "modbus-tcp://127.0.0.1:1/1/holding/0",
            "--duration",
            "1s",
            "--csv",
            failed.to_str().unwrap(),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!failed.exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn dataset_with_signals_asof() {
    let root = temp_root("dataset-signals");
    let dir = root.join("sessions");
    let db = root.join("d.duckdb");
    let db_text = db.to_str().unwrap();
    json(
        &dir,
        &[
            "--json",
            "log",
            "sim-signal://line1",
            "--duration",
            "2s",
            "--dataset",
            db_text,
        ],
    );
    json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            root.join("a.png").to_str().unwrap(),
            "--dataset",
            db_text,
        ],
    );
    let rows = dataset_rows(
        &dir,
        &db,
        "SELECT count(*), count(value_double), min(sample_age_ms) >= 0 FROM captures_with_signals",
    );
    assert!(rows[0][1].as_u64().unwrap() > 0, "{rows}");
    assert_eq!(rows[0][2], true);
    let out_dir = root.join("bundle");
    let result = json(
        &dir,
        &[
            "--json",
            "dataset",
            "export",
            "--file",
            db_text,
            "-o",
            out_dir.to_str().unwrap(),
            "--format",
            "csv",
        ],
    );
    assert_eq!(result["tables"]["captures_wide"], 1);
    let joined = std::fs::read_to_string(out_dir.join("captures_with_signals.csv")).unwrap();
    assert!(joined.contains("line1/"), "{joined}");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out_dir.join("manifest.json")).unwrap()).unwrap();
    assert!(
        manifest["signals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["samples"].as_u64() > Some(0)),
        "{manifest}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Agents inspect frames through a session: a stopped camera acquires one
/// frame and stays stopped, a live one waits for frames published after the
/// request, regions crop the full-resolution frame, and nothing is saved.
#[test]
fn rpc_frame_inspects_stopped_and_live_cameras_without_saving() {
    use base64::Engine;
    use std::io::Write;
    let root = temp_root("frame-rpc");
    let dir = root.join("sessions");
    let cwd = root.join("server");
    std::fs::create_dir_all(&cwd).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &cwd, "frame-ci"),
        dir: root.clone(),
    };
    let rpc = |request: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env_remove("CAPTUREFAB_SESSION")
            .env_remove("CAPTUREFAB_CAMERA")
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .args([
                "--json",
                "--session",
                "frame-ci",
                "--camera",
                "sim:0",
                "rpc",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let response: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "{request}: {error}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code(), response)
    };
    let ok = |request: &str| {
        let (code, response) = rpc(request);
        assert_eq!(code, Some(0), "{request}: {response}");
        response["result"].clone()
    };
    let session = |args: &[&str]| {
        json(
            &dir,
            &[
                &["--json", "--session", "frame-ci", "--camera", "sim:0"],
                args,
            ]
            .concat(),
        )
    };
    json(
        &dir,
        &["--json", "--session", "frame-ci", "connect", "sim:0"],
    );

    let grabbed = ok(r#"{"op":"frame","max_dimension":256}"#);
    assert_eq!(grabbed["camera"], "sim:0");
    assert_eq!(grabbed["frame"]["source"], "grab");
    assert_eq!(grabbed["frame"]["pixel_format"], "Mono8");
    assert_eq!(grabbed["frame"]["width"], 640);
    assert_eq!(grabbed["frame"]["height"], 480);
    let mean = grabbed["exposure"]["mean"].as_f64().unwrap();
    assert!((0.3..0.6).contains(&mean), "{grabbed}");
    let image = &grabbed["image"];
    assert_eq!(image["mime"], "image/jpeg");
    assert_eq!(image["width"], 256);
    assert_eq!(image["height"], 192);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image["base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes.len() as u64, image["bytes"].as_u64().unwrap());
    assert!(capturefab::jpeg::well_formed(&bytes, 256, 192));
    let status = session(&["status"]);
    assert_eq!(status["streaming"], false);
    assert!(
        status["activity"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["operation"] != "frame"),
        "inspections are observations"
    );

    session(&["start"]);
    let live = ok(r#"{"op":"frame","fresh":2,"max_dimension":0}"#);
    assert_eq!(live["frame"]["source"], "live");
    assert!(
        live["frame"]["waited_frames"].as_u64().unwrap() >= 2,
        "{live}"
    );
    assert!(live["frame"]["seq"].as_u64() > grabbed["frame"]["seq"].as_u64());
    assert!(live["frame"]["age_ms"].as_u64().unwrap() < 5000);
    assert!(live["image"].is_null());

    let region =
        ok(r#"{"op":"frame","region":[0.25,0.25,0.5,0.5],"max_dimension":2048,"format":"png"}"#);
    assert_eq!(
        region["region"],
        serde_json::json!({"x":160,"y":120,"width":320,"height":240,"normalized":[0.25,0.25,0.5,0.5]})
    );
    assert_eq!(region["image"]["mime"], "image/png");
    assert_eq!(region["image"]["width"], 320);
    assert_eq!(region["image"]["height"], 240);
    assert!(region["focus"]["tenengrad"].as_f64().unwrap() > 0.0);

    // One undoable change; the next fresh frame shows it, and undo restores it.
    let changed = ok(
        r#"{"op":"set_many","values":[{"feature":"ExposureTime","value":"40000"},{"feature":"Gain","value":"0"}]}"#,
    );
    assert_eq!(changed["values"][0]["name"], "ExposureTime");
    let token = changed["camera_change"]["token"].as_str().unwrap();
    let bright = ok(r#"{"op":"frame","max_dimension":0}"#);
    assert!(
        bright["exposure"]["highlights"].as_f64().unwrap() > 0.2,
        "{bright}"
    );
    assert!(
        bright["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().starts_with("highlights clipped")),
        "{bright}"
    );
    ok(&format!(
        r#"{{"op":"replay_camera_change","token":"{token}","undo":true}}"#
    ));
    let restored = ok(r#"{"op":"frame","max_dimension":0}"#);
    assert!(
        (restored["exposure"]["mean"].as_f64().unwrap() - mean).abs() < 0.1,
        "{restored}"
    );
    assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0, "nothing saved");

    let (code, invalid) = rpc(r#"{"op":"frame","fresh":11}"#);
    assert_eq!(code, Some(2), "{invalid}");
    assert_eq!(invalid["error"]["code"], "usage");
    assert!(
        invalid["error"]["message"]
            .as_str()
            .unwrap()
            .contains("fresh frames must be 0..10")
    );
    let (code, grab) = rpc(r#"{"op":"grab","timeout_ms":1000}"#);
    assert_eq!(code, Some(1), "{grab}");
    // Thirty frames a second cannot bring ten new ones within 200 ms.
    let (code, slow) = rpc(r#"{"op":"frame","fresh":10,"timeout_ms":200}"#);
    assert_eq!(code, Some(4), "{slow}");
    assert_eq!(slow["error"]["code"], "timeout");
    assert!(
        slow["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("no new frame from sim:0 within 200 ms (timed out)"),
        "{slow}"
    );
    assert_eq!(session(&["status"])["streaming"], true);
}

fn tools_output(
    dir: &PathBuf,
    cwd: &PathBuf,
    args: &[&str],
    input: Option<&str>,
) -> std::process::Output {
    use std::io::Write;
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_STAGING_DIR", dir.join("uploads"))
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn tools_response(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn tools_ok(dir: &PathBuf, cwd: &PathBuf, args: &[&str]) -> Value {
    let output = tools_output(dir, cwd, args, None);
    assert!(
        output.status.success(),
        "{args:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response = tools_response(&output);
    assert_eq!(response["ok"], true);
    response["result"].clone()
}

fn tools_fixture(name: &str) -> (Fixture, PathBuf, PathBuf, PathBuf) {
    let root = temp_root(name);
    let dir = root.join("sessions");
    let cwd = root.join("caller");
    let server = root.join("server");
    let captures = root.join("captures");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&server).unwrap();
    let child = serve_env(
        &dir,
        &server,
        "tools-ci",
        &[(
            "CAPTUREFAB_STAGING_DIR",
            dir.join("uploads").to_str().unwrap(),
        )],
    );
    let fixture = Fixture { child, dir: root };
    tools_ok(
        &dir,
        &cwd,
        &[
            "--json",
            "destination",
            "add-folder",
            "captures",
            captures.to_str().unwrap(),
        ],
    );
    (fixture, dir, cwd, captures)
}

fn tools_call(dir: &PathBuf, cwd: &PathBuf, name: &str, args: Value, flags: &[&str]) -> Value {
    let args = args.to_string();
    let mut argv = vec![
        "--json",
        "--session",
        "tools-ci",
        "--actor",
        "tools-integration",
        "--actor-kind",
        "agent",
        "tools",
        "call",
        name,
        "--args",
        &args,
    ];
    argv.extend_from_slice(flags);
    tools_ok(dir, cwd, &argv)
}

fn tools_photo_args(camera: &str, name: &str) -> Value {
    serde_json::json!({"camera":camera,"count":1,"format":"png","file_name":name,"destination":"captures","tags":null})
}

#[test]
fn tools_list_and_overview_work_without_a_session() {
    let root = temp_root("tools-offline");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("sessions");
    let listed = tools_ok(&dir, &root, &["--json", "tools", "list"]);
    let catalog = listed.as_array().unwrap();
    assert_eq!(catalog.len(), 24);
    for name in [
        "get_overview",
        "inspect_frame",
        "set_camera_features",
        "capture_photos",
        "create_s3_destination",
    ] {
        let tool = catalog.iter().find(|tool| tool["name"] == name).unwrap();
        assert!(tool["risk"].is_string());
        assert!(tool["needs_session"].is_boolean());
        assert!(tool["needs_vision"].is_boolean());
        assert_eq!(tool["input_schema"]["additionalProperties"], false);
    }
    let overview = tools_ok(&dir, &root, &["--json", "tools", "call", "get_overview"]);
    assert_eq!(overview["session"], false);
    assert!(overview["cameras"].as_array().unwrap().is_empty());
    assert!(overview["storage"].is_object());
    let from_stdin = tools_output(
        &dir,
        &root,
        &[
            "--json",
            "tools",
            "call",
            "get_overview",
            "--args-file",
            "-",
        ],
        Some("{}\n"),
    );
    assert!(
        from_stdin.status.success(),
        "{}",
        String::from_utf8_lossy(&from_stdin.stdout)
    );
    assert_eq!(tools_response(&from_stdin)["result"]["session"], false);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn tools_schema_and_doctor_describe_shared_registry() {
    let root = temp_root("tools-schema");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("sessions");
    let schema = tools_ok(&dir, &root, &["--json", "schema"]);
    let tools = &schema["tools"];
    let catalog = tools["catalog"].as_array().unwrap();
    assert_eq!(catalog.len(), 24);
    assert_eq!(
        tools["catalog"],
        tools_ok(&dir, &root, &["--json", "tools", "list"])
    );
    for (name, risk, needs_session) in [
        ("get_overview", "observe", false),
        ("list_cameras", "observe", false),
        ("list_destinations", "observe", false),
        ("get_storage", "observe", false),
        ("list_jobs", "observe", true),
        ("get_camera_features", "observe", true),
        ("inspect_frame", "observe", true),
        ("wait_for", "observe", true),
        ("connect_camera", "reversible", true),
        ("start_live_view", "reversible", true),
        ("stop_live_view", "reversible", true),
        ("set_camera_features", "reversible", true),
        ("set_camera_roi", "reversible", true),
        ("set_auto_exposure", "reversible", true),
        ("run_camera_command", "reversible", true),
        ("undo_camera_change", "reversible", true),
        ("capture_photos", "bounded", true),
        ("start_recording", "bounded", true),
        ("stop_recording", "bounded", true),
        ("schedule_captures", "bounded", true),
        ("cancel_job", "bounded", true),
        ("stream_to_url", "disruptive", true),
        ("create_folder_destination", "human_only", false),
        ("create_s3_destination", "human_only", false),
    ] {
        let tool = catalog.iter().find(|tool| tool["name"] == name).unwrap();
        assert_eq!(tool["risk"], risk, "{name}");
        assert_eq!(tool["needs_session"], needs_session, "{name}");
        assert_eq!(
            tool["needs_vision"], false,
            "metrics remain available without vision: {name}"
        );
        let input = &tool["input_schema"];
        let properties = input["properties"].as_object().unwrap();
        let required = input["required"].as_array().unwrap();
        assert_eq!(input["additionalProperties"], false, "{name}");
        assert_eq!(required.len(), properties.len(), "{name}");
        for key in properties.keys() {
            assert!(
                required.contains(&serde_json::json!(key)),
                "{name}.{key} must be required, with null for defaults"
            );
        }
        assert!(!properties.contains_key("secret"), "{name}");
        assert!(!properties.contains_key("password"), "{name}");
    }
    let photos = catalog
        .iter()
        .find(|tool| tool["name"] == "capture_photos")
        .unwrap();
    assert_eq!(
        photos["input_schema"]["properties"]["count"]["type"],
        serde_json::json!(["integer", "null"])
    );
    assert_eq!(
        photos["input_schema"]["properties"]["destination"]["type"],
        serde_json::json!(["string", "null"])
    );
    let features = catalog
        .iter()
        .find(|tool| tool["name"] == "set_camera_features")
        .unwrap();
    let settings = &features["input_schema"]["properties"]["values"]["items"];
    assert_eq!(settings["additionalProperties"], false);
    assert_eq!(
        settings["required"],
        serde_json::json!(["feature", "value", "expect"])
    );
    assert_eq!(
        settings["properties"]["expect"]["type"],
        serde_json::json!(["string", "null"])
    );
    assert_eq!(tools["plan"]["version"], 1);
    assert_eq!(tools["plan"]["max_calls"], 64);
    assert_eq!(tools["plan"]["max_bytes"], 256 * 1024);
    assert_eq!(
        tools["issues"]["exit_codes"],
        serde_json::json!({"usage":2,"unavailable":3,"timeout":4,"unsupported":5,"storage_full":6,"operation_failed":1})
    );
    assert_eq!(
        tools["confirmation"]["risks"],
        serde_json::json!(["disruptive", "human_only"])
    );
    assert_eq!(tools["confirmation"]["flag"], "--confirmed-by-user");
    assert_eq!(tools["images"]["directory_mode"], "0700");
    assert_eq!(tools["images"]["file_mode"], "0600");
    assert_eq!(tools["images"]["max_files"], 64);
    assert_eq!(tools["images"]["max_bytes"], 64 * 1024 * 1024);
    assert_eq!(
        tools["images"]["metadata"],
        serde_json::json!(["path", "mime", "width", "height", "bytes", "sha256"])
    );
    let cli_tools = schema["cli"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|command| command["name"] == "tools")
        .unwrap();
    let commands = cli_tools["commands"].as_array().unwrap();
    assert_eq!(
        commands
            .iter()
            .map(|command| command["name"].as_str().unwrap())
            .filter(|name| *name != "help")
            .collect::<Vec<_>>(),
        ["list", "call", "apply"]
    );
    let call = commands
        .iter()
        .find(|command| command["name"] == "call")
        .unwrap();
    let arguments = call["arguments"].as_array().unwrap();
    for name in [
        "args",
        "args-file",
        "dry-run",
        "secret-stdin",
        "confirmed-by-user",
        "image-out",
        "inline-image",
    ] {
        assert!(
            arguments.iter().any(|argument| argument["long"] == name),
            "tools call --{name}"
        );
    }
    let apply = commands
        .iter()
        .find(|command| command["name"] == "apply")
        .unwrap();
    let arguments = apply["arguments"].as_array().unwrap();
    assert!(
        arguments
            .iter()
            .any(|argument| argument["name"] == "file" && argument["required"] == true)
    );
    for name in ["dry-run", "secret-stdin", "confirmed-by-user"] {
        assert!(
            arguments.iter().any(|argument| argument["long"] == name),
            "tools apply --{name}"
        );
    }
    let doctor = tools_ok(&dir, &root, &["--json", "doctor"]);
    assert_eq!(doctor["features"]["ai"], cfg!(feature = "ai"));
    assert_eq!(doctor["features"]["uploads"], cfg!(feature = "uploads"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn tools_capture_photos_numbers_existing_files_and_dry_run_saves_nothing() {
    let (_fixture, dir, cwd, captures) = tools_fixture("tools-capture");
    tools_call(
        &dir,
        &cwd,
        "connect_camera",
        serde_json::json!({"camera":"sim:0","size":null,"fps":null}),
        &[],
    );
    let planned = tools_call(
        &dir,
        &cwd,
        "capture_photos",
        tools_photo_args("sim:0", "planned.png"),
        &["--dry-run"],
    );
    assert_eq!(planned["checked"].as_array().unwrap().len(), 1);
    assert!(
        planned["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["command"]["op"] == "capture")
    );
    assert!(!captures.join("planned.png").exists());
    let first = tools_call(
        &dir,
        &cwd,
        "capture_photos",
        tools_photo_args("sim:0", "shot.png"),
        &[],
    );
    assert_eq!(first["results"].as_array().unwrap().len(), 1);
    assert_eq!(first["results"][0]["files"].as_array().unwrap().len(), 1);
    let original = std::fs::read(captures.join("shot.png")).unwrap();
    tools_call(
        &dir,
        &cwd,
        "capture_photos",
        tools_photo_args("sim:0", "shot.png"),
        &[],
    );
    assert_eq!(std::fs::read(captures.join("shot.png")).unwrap(), original);
    let files = std::fs::read_dir(&captures)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(
        files
            .iter()
            .all(|path| path.extension().and_then(|ext| ext.to_str()) == Some("png"))
    );
}

#[test]
fn tools_usage_errors_have_field_details_and_never_create_session_state() {
    let root = temp_root("tools-usage");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("sessions");
    let mut invalid = tools_photo_args("sim:0", "photo.png");
    invalid["count"] = serde_json::json!("ten");
    let invalid = invalid.to_string();
    let output = tools_output(
        &dir,
        &root,
        &[
            "--json",
            "tools",
            "call",
            "capture_photos",
            "--args",
            &invalid,
        ],
        None,
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let error = tools_response(&output);
    assert_eq!(error["error"]["details"]["issues"][0]["field"], "count");
    assert!(
        !dir.exists(),
        "offline argument validation created session state"
    );
    for args in [
        vec!["--json", "tools", "call", "not_a_tool"],
        vec!["--json", "tools", "call", "get_overview", "--args", "{"],
        vec![
            "--json",
            "tools",
            "call",
            "get_overview",
            "--args",
            "{}",
            "--args-file",
            "missing.json",
        ],
        vec!["--json", "--camera", "sim:0", "tools", "list"],
    ] {
        let output = tools_output(&dir, &root, &args, None);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!dir.exists(), "{args:?} created session state");
    }
    for (tool, args) in [
        ("start_live_view", r#"{"camera":"sim:0","camera":"sim:0"}"#),
        (
            "set_camera_features",
            r#"{"camera":"sim:0","values":[{"feature":"Gain","value":"0","value":"1","expect":null}],"restart_stream":false}"#,
        ),
    ] {
        let output = tools_output(
            &dir,
            &root,
            &["--json", "tools", "call", tool, "--args", args],
            None,
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{tool}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            tools_response(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("duplicate")
        );
        assert!(!dir.exists(), "duplicate args created session state");
    }
    let oversized = serde_json::json!({"text":"x".repeat(256*1024)}).to_string();
    let oversized_path = root.join("oversized-args.json");
    std::fs::write(&oversized_path, oversized).unwrap();
    assert_eq!(
        tools_output(
            &dir,
            &root,
            &[
                "--json",
                "tools",
                "call",
                "get_overview",
                "--args-file",
                oversized_path.to_str().unwrap()
            ],
            None
        )
        .status
        .code(),
        Some(2)
    );
    assert!(!dir.exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn tools_missing_destination_and_unconfirmed_stream_have_distinct_errors() {
    let (_fixture, dir, cwd, _) = tools_fixture("tools-refusals");
    tools_call(
        &dir,
        &cwd,
        "connect_camera",
        serde_json::json!({"camera":"sim:0","size":null,"fps":null}),
        &[],
    );
    let mut missing = tools_photo_args("sim:0", "missing.png");
    missing["destination"] = serde_json::json!("not-saved");
    let missing = missing.to_string();
    let output = tools_output(
        &dir,
        &cwd,
        &[
            "--json",
            "--session",
            "tools-ci",
            "tools",
            "call",
            "capture_photos",
            "--args",
            &missing,
        ],
        None,
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        tools_response(&output)["error"]["details"]["issues"][0]["field"],
        "destination"
    );
    let stream=serde_json::json!({"camera":"sim:0","url":"rtmp://uncontacted.example/live/","duration":null,"codec":null,"fps":null,"bitrate":null}).to_string();
    let output = tools_output(
        &dir,
        &cwd,
        &[
            "--json",
            "--session",
            "tools-ci",
            "tools",
            "call",
            "stream_to_url",
            "--args",
            &stream,
        ],
        None,
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        tools_response(&output)["error"]["details"]["risk"],
        "disruptive"
    );
    let status = tools_ok(
        &dir,
        &cwd,
        &[
            "--json",
            "--session",
            "tools-ci",
            "--camera",
            "sim:0",
            "status",
        ],
    );
    assert!(status["forwarding"].is_null());
}

#[test]
fn tools_inspect_frame_delivers_private_bounded_images_and_measurements() {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let (_fixture, dir, cwd, captures) = tools_fixture("tools-inspect");
    tools_call(
        &dir,
        &cwd,
        "connect_camera",
        serde_json::json!({"camera":"sim:0","size":null,"fps":null}),
        &[],
    );
    let inspected = tools_call(
        &dir,
        &cwd,
        "inspect_frame",
        serde_json::json!({"camera":"sim:0","fresh":1,"max_dimension":256,"region":null,"include_image":true}),
        &[],
    );
    assert!(inspected["exposure"]["mean"].as_f64().unwrap() > 0.0);
    let image = &inspected["image"];
    let path = PathBuf::from(image["path"].as_str().unwrap());
    assert!(path.starts_with(dir.join("inspect")), "{path:?}");
    let width = image["width"].as_u64().unwrap();
    let height = image["height"].as_u64().unwrap();
    assert!(width > 0 && height > 0 && width.max(height) <= 256);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len() as u64, image["bytes"].as_u64().unwrap());
    assert_eq!(image["sha256"], format!("{:x}", Sha256::digest(&bytes)));
    assert!(matches!(
        image["mime"].as_str(),
        Some("image/jpeg" | "image/png")
    ));
    assert!(image.get("base64").is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dir.join("inspect"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert_eq!(
        std::fs::read_dir(captures).unwrap().count(),
        0,
        "inspection must not save captures"
    );
    let extension = if image["mime"] == "image/jpeg" {
        "jpg"
    } else {
        "png"
    };
    let exported_path = cwd.join(format!("diagnostic.{extension}"));
    let inspect = serde_json::json!({"camera":"sim:0","fresh":1,"max_dimension":256,"region":null,"include_image":true});
    let exported = tools_call(
        &dir,
        &cwd,
        "inspect_frame",
        inspect.clone(),
        &[
            "--image-out",
            exported_path.to_str().unwrap(),
            "--inline-image",
        ],
    );
    assert_eq!(exported["image"]["path"], exported_path.to_str().unwrap());
    let exported_bytes = std::fs::read(&exported_path).unwrap();
    let inline = base64::engine::general_purpose::STANDARD
        .decode(exported["image"]["base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(inline, exported_bytes);
    let inspect = inspect.to_string();
    let refused = tools_output(
        &dir,
        &cwd,
        &[
            "--json",
            "--session",
            "tools-ci",
            "tools",
            "call",
            "inspect_frame",
            "--args",
            &inspect,
            "--image-out",
            exported_path.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        !refused.status.success(),
        "an explicit image export must not overwrite"
    );
    assert_eq!(std::fs::read(exported_path).unwrap(), exported_bytes);
}

#[test]
fn tools_inspect_frame_without_a_session_explains_how_to_start_one() {
    let root = temp_root("tools-no-session");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("sessions");
    let args=serde_json::json!({"camera":"sim:0","fresh":1,"max_dimension":256,"region":null,"include_image":true}).to_string();
    let output = tools_output(
        &dir,
        &root,
        &["--json", "tools", "call", "inspect_frame", "--args", &args],
        None,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(tools_response(&output)["error"]["message"].as_str().unwrap().contains("inspect_frame needs a running session; start one with `capturefab serve --name NAME`"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn tools_setting_exposure_changes_the_frame_and_undo_restores_it() {
    let (_fixture, dir, cwd, _) = tools_fixture("tools-closed-loop");
    tools_call(
        &dir,
        &cwd,
        "connect_camera",
        serde_json::json!({"camera":"sim:0","size":null,"fps":null}),
        &[],
    );
    tools_call(
        &dir,
        &cwd,
        "start_live_view",
        serde_json::json!({"camera":"sim:0"}),
        &[],
    );
    let inspect = serde_json::json!({"camera":"sim:0","fresh":2,"max_dimension":0,"region":null,"include_image":false});
    let baseline = tools_call(&dir, &cwd, "inspect_frame", inspect.clone(), &[]);
    let mean = baseline["exposure"]["mean"].as_f64().unwrap();
    let features = tools_call(
        &dir,
        &cwd,
        "get_camera_features",
        serde_json::json!({"camera":"sim:0","search":null,"names":["ExposureTime"]}),
        &[],
    );
    let exposure = &features["features"][0]["value"];
    let changed = tools_call(
        &dir,
        &cwd,
        "set_camera_features",
        serde_json::json!({"camera":"sim:0","values":[{"feature":"ExposureTime","value":"40000","expect":exposure.to_string()}],"restart_stream":false}),
        &[],
    );
    let token = changed["results"][0]["camera_change"]["token"]
        .as_str()
        .unwrap();
    let brighter = tools_call(&dir, &cwd, "inspect_frame", inspect.clone(), &[]);
    assert!(
        brighter["exposure"]["mean"].as_f64().unwrap() > mean + 0.15,
        "before {baseline}; after {brighter}"
    );
    tools_call(
        &dir,
        &cwd,
        "undo_camera_change",
        serde_json::json!({"camera":"sim:0","token":token}),
        &[],
    );
    let restored = tools_call(&dir, &cwd, "inspect_frame", inspect, &[]);
    assert!(
        (restored["exposure"]["mean"].as_f64().unwrap() - mean).abs() < 0.1,
        "before {baseline}; restored {restored}"
    );
}

#[test]
fn tools_apply_executes_a_two_step_plan_in_order() {
    let (_fixture, dir, cwd, captures) = tools_fixture("tools-apply");
    let plan = serde_json::json!({"version":1,"calls":[{"tool":"connect_camera","args":{"camera":"sim:apply","size":null,"fps":null}},{"tool":"capture_photos","args":tools_photo_args("sim:apply","applied.png")}]});
    let path = cwd.join("plan.json");
    std::fs::write(&path, plan.to_string()).unwrap();
    let dry = tools_ok(
        &dir,
        &cwd,
        &[
            "--json",
            "--session",
            "tools-ci",
            "tools",
            "apply",
            path.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(dry["checked"].as_array().unwrap().len(), 2);
    assert!(!captures.join("applied.png").exists());
    let applied = tools_ok(
        &dir,
        &cwd,
        &[
            "--json",
            "--session",
            "tools-ci",
            "--actor",
            "tools-integration",
            "--actor-kind",
            "agent",
            "tools",
            "apply",
            path.to_str().unwrap(),
        ],
    );
    assert_eq!(applied["completed"].as_array().unwrap().len(), 2);
    assert!(captures.join("applied.png").is_file());
    let status = tools_ok(&dir, &cwd, &["--json", "--session", "tools-ci", "status"]);
    assert!(
        status["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .any(|camera| camera["info"]["id"] == "sim:apply")
    );
}

#[test]
fn tools_secret_stdin_requires_exactly_one_consumer_before_touching_session_state() {
    let root = temp_root("tools-secret-count");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("sessions");
    let output = tools_output(
        &dir,
        &root,
        &["--json", "tools", "call", "get_overview", "--secret-stdin"],
        Some("unused-secret\n"),
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(!dir.exists());
    let destination = |name: &str| serde_json::json!({"tool":"create_s3_destination","args":{"name":name,"bucket":"capturefab-test-bucket","region":"us-east-1","endpoint":null,"prefix":null,"path_style":false,"credentials":"keychain","profile":null,"access_key_id":"AKIA1234567890ABCDEF","keep_local":false}});
    let plan = serde_json::json!({"version":1,"calls":[destination("one"),destination("two")]});
    let path = root.join("two-secrets.json");
    std::fs::write(&path, plan.to_string()).unwrap();
    let output = tools_output(
        &dir,
        &root,
        &[
            "--json",
            "tools",
            "apply",
            path.to_str().unwrap(),
            "--secret-stdin",
            "--dry-run",
        ],
        Some("never-stored-secret\n"),
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!dir.exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("never-stored-secret"));
    let output = tools_output(
        &dir,
        &root,
        &[
            "--json",
            "tools",
            "call",
            "get_overview",
            "--args-file",
            "-",
            "--secret-stdin",
        ],
        Some("{}\n"),
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(!dir.exists());
    #[cfg(feature = "s3")]
    {
        let plan = serde_json::json!({"version":1,"calls":[destination("one")]});
        let path = root.join("one-secret.json");
        std::fs::write(&path, plan.to_string()).unwrap();
        let output = tools_output(
            &dir,
            &root,
            &[
                "--json",
                "tools",
                "apply",
                path.to_str().unwrap(),
                "--secret-stdin",
                "--dry-run",
                "--confirmed-by-user",
            ],
            Some("single-dry-run-secret\n"),
        );
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            tools_response(&output)["result"]["checked"][0]["risk"],
            "human_only"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("single-dry-run-secret"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("single-dry-run-secret"));
        assert!(
            !dir.join("destinations.json").exists(),
            "dry-run must not create a destination or store its secret"
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(feature = "ai")]
mod assistant_cli_tests {
    use super::*;
    use capturefab::assistant::ProviderId;
    use serde_json::json;
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    struct Directory(PathBuf);
    impl Directory {
        fn new(name: &str) -> Self {
            let root = temp_root(name);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The only HTTP peer these tests use. Replies are finite and scripted;
    /// unexpected requests fail locally instead of falling through to a service.
    struct ProviderServer {
        base_url: String,
        requests: Arc<Mutex<Vec<Value>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl ProviderServer {
        fn new(replies: Vec<Value>) -> Self {
            Self::statuses(replies.into_iter().map(|body| (200, body)).collect())
        }
        fn statuses(replies: Vec<(u16, Value)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                let mut replies = VecDeque::from(replies);
                while !stopped.load(Ordering::Relaxed) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("Scripted provider accept failed: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut reader = BufReader::new(&mut stream);
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    assert_eq!(first.trim_end(), "POST /v1/chat/completions HTTP/1.1");
                    let mut length = None;
                    let mut header_bytes = first.len();
                    loop {
                        let mut line = String::new();
                        assert!(reader.read_line(&mut line).unwrap() > 0);
                        header_bytes += line.len();
                        assert!(header_bytes <= 16 * 1024);
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                    let length = length.expect("Bounded JSON request has Content-Length");
                    assert!(length <= 1024 * 1024);
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    drop(reader);
                    seen.lock()
                        .unwrap()
                        .push(serde_json::from_slice(&body).unwrap());
                    let (status, body) = replies.pop_front().unwrap_or_else(|| {
                        (400, json!({"error":{"message":"Unexpected request to the offline scripted provider."}}))
                    });
                    let body = body.to_string();
                    write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                    stream.flush().unwrap();
                }
            });
            Self {
                base_url,
                requests,
                stop,
                worker: Some(worker),
            }
        }
    }
    impl Drop for ProviderServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !thread::panicking() {
                    result.expect("The scripted provider worker failed.");
                }
            }
        }
    }

    fn command(dir: &PathBuf, cwd: &PathBuf, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
        command
            .env_remove("CAPTUREFAB_SESSION")
            .env_remove("CAPTUREFAB_CAMERA")
            .env_remove("CAPTUREFAB_ACTOR")
            .env_remove("CAPTUREFAB_ACTOR_KIND");
        for id in ProviderId::ALL {
            for name in id.info().key_envs {
                command.env_remove(name);
            }
        }
        for name in [
            "AWS_PROFILE",
            "AWS_DEFAULT_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "GOOGLE_CLOUD_QUOTA_PROJECT",
            "OLLAMA_HOST",
            "AZURE_OPENAI_ENDPOINT",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_GATEWAY_ID",
        ] {
            command.env_remove(name);
        }
        // An explicit dummy Custom bearer skips the native keychain fallback.
        command
            .env("CAPTUREFAB_AI_API_KEY", "offline-test")
            .env("CAPTUREFAB_SESSION_DIR", dir)
            .env("CAPTUREFAB_STAGING_DIR", dir.join("uploads"))
            .envs(env.iter().copied())
            .current_dir(cwd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn output(
        dir: &PathBuf,
        cwd: &PathBuf,
        args: &[&str],
        input: Option<&str>,
        env: &[(&str, &str)],
    ) -> std::process::Output {
        let mut command = command(dir, cwd, args, env);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }
    fn ok(dir: &PathBuf, cwd: &PathBuf, args: &[&str]) -> Value {
        let out = output(dir, cwd, args, None, &[]);
        assert!(
            out.status.success(),
            "{args:?}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let value = tools_response(&out);
        assert_eq!(value["version"], 1);
        assert_eq!(value["ok"], true);
        value["result"].clone()
    }
    fn settings(dir: &PathBuf, server: &ProviderServer) {
        std::fs::create_dir_all(dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(dir.join("assistant.json")).unwrap();
        let value = json!({"version":1,"provider":"custom","providers":{"custom":{"model":"offline-camera-model","base_url":server.base_url,"vision":"off","saved_key":false}}});
        write!(file, "{value}").unwrap();
    }
    fn fixture(name: &str) -> (Fixture, PathBuf, PathBuf) {
        let (fixture, dir, cwd, _captures) = tools_fixture(name);
        tools_ok(
            &dir,
            &cwd,
            &["--json", "--session", "tools-ci", "connect", "sim:0"],
        );
        (fixture, dir, cwd)
    }
    fn call(id: &str, name: &str, args: Value) -> Value {
        json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":50,"completion_tokens":15}})
    }
    fn end() -> Value {
        json!({"choices":[{"message":{"role":"assistant","content":"The plan is ready."},"finish_reason":"stop"}],"usage":{"prompt_tokens":50,"completion_tokens":15}})
    }
    fn photo(name: &str) -> Value {
        json!({"camera":"sim:0","count":1,"format":"png","file_name":name,"destination":null,"tags":null})
    }
    fn proposal(mut args: Value) -> Value {
        args["replace_step"] = Value::Null;
        args
    }
    fn finish() -> Value {
        call(
            "finished",
            "finish_task",
            json!({"outcome":"succeeded","summary":"Saved the requested photograph."}),
        )
    }
    fn assert_png(path: &std::path::Path) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }
    fn assert_end_log(dir: &PathBuf, id: &str, outcome: &str) -> Vec<Value> {
        let log = ok(dir, dir, &["--json", "assistant", "log", id]);
        let records = log.as_array().unwrap();
        assert_eq!(records.first().unwrap()["kind"], "start");
        assert_eq!(records.last().unwrap()["kind"], "end");
        assert_eq!(records.last().unwrap()["outcome"], outcome);
        assert!(!log.to_string().contains("offline-test"));
        records.clone()
    }

    #[test]
    fn assistant_providers_report_environment_presence_without_keychain_or_session() {
        let root = Directory::new("assistant-providers");
        let dir = root.0.join("sessions");
        let absent = ok(&dir, &root.0, &["--json", "assistant", "providers"]);
        let providers = absent.as_array().unwrap();
        assert_eq!(providers.len(), 22);
        for id in ProviderId::ALL {
            let provider = providers.iter().find(|item| item["id"] == id.id()).unwrap();
            assert!(provider["name"].is_string());
            assert!(provider["credential"]["ready"].is_boolean());
            assert!(provider["vision"].get("mode").is_some());
            assert!(provider["vision"].get("resolved").is_some());
            assert!(provider["vision"]["source"].is_string());
            assert!(provider["vision"]["placement"].is_string());
        }
        assert_eq!(
            providers
                .iter()
                .find(|item| item["id"] == "openai")
                .unwrap()["credential"]["ready"],
            false
        );
        let present = output(
            &dir,
            &root.0,
            &["--json", "assistant", "providers"],
            None,
            &[("OPENAI_API_KEY", "offline-openai-presence")],
        );
        assert!(present.status.success());
        let present = tools_response(&present);
        let provider = present["result"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "openai")
            .unwrap();
        assert_eq!(provider["credential"]["ready"], true);
        assert!(!present.to_string().contains("offline-openai-presence"));
        assert!(!present.to_string().contains("offline-test"));
        assert!(
            !dir.exists(),
            "Listing providers must not create session state."
        );
    }

    #[test]
    fn assistant_configuration_and_prompt_errors_fail_before_session_or_keychain_access() {
        let root = Directory::new("assistant-invalid");
        let dir = root.0.join("sessions");
        for (args, field) in [
            (
                vec!["--json", "assistant", "use", "unknown-provider"],
                "provider",
            ),
            (
                vec![
                    "--json",
                    "assistant",
                    "use",
                    "custom",
                    "--model",
                    "fixture",
                    "--base-url",
                    "http://public.example.test/v1",
                ],
                "base_url",
            ),
            (
                vec![
                    "--json",
                    "assistant",
                    "use",
                    "azure",
                    "--endpoint",
                    "https://fixture.openai.azure.com/evil",
                ],
                "endpoint",
            ),
            (
                vec![
                    "--json",
                    "assistant",
                    "key",
                    "set",
                    "unknown-provider",
                    "--key-stdin",
                ],
                "provider",
            ),
        ] {
            let out = output(&dir, &root.0, &args, None, &[]);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{args:?}\n{}",
                String::from_utf8_lossy(&out.stdout)
            );
            let error = tools_response(&out);
            assert_eq!(error["ok"], false);
            assert_eq!(error["error"]["code"], "usage");
            assert!(
                error["error"]["details"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["field"] == field),
                "{error}"
            );
            assert!(!dir.exists());
        }
        for args in [
            vec!["--json", "assistant", "key", "set", "custom"],
            vec![
                "--json",
                "assistant",
                "ask",
                "rtsp://user:private-password@camera/live",
            ],
            vec![
                "--json",
                "assistant",
                "ask",
                "two sources",
                "--prompt-file",
                "-",
            ],
            vec![
                "--json",
                "assistant",
                "run",
                "inspect",
                "--camera",
                "sim:0",
                "--max-time",
                "30s",
            ],
            vec![
                "--json",
                "assistant",
                "run",
                "inspect",
                "--camera",
                "sim:0",
                "--approvals",
                "ask",
            ],
        ] {
            let out = output(&dir, &root.0, &args, None, &[]);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{args:?}\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(!dir.exists(), "{args:?}");
            assert!(!String::from_utf8_lossy(&out.stdout).contains("private-password"));
        }
    }

    #[test]
    fn assistant_use_persists_only_non_secret_settings_in_a_private_file() {
        let root = Directory::new("assistant-use");
        let dir = root.0.join("sessions");
        let server = ProviderServer::new(Vec::new());
        ok(
            &dir,
            &root.0,
            &[
                "--json",
                "assistant",
                "use",
                "custom",
                "--model",
                "offline-camera-model",
                "--base-url",
                &server.base_url,
                "--vision",
                "off",
                "--effort",
                "low",
            ],
        );
        let path = dir.join("assistant.json");
        let saved = std::fs::read_to_string(&path).unwrap();
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["version"], 1);
        assert_eq!(saved["provider"], "custom");
        assert_eq!(
            saved["providers"]["custom"]["model"],
            "offline-camera-model"
        );
        assert_eq!(saved["providers"]["custom"]["base_url"], server.base_url);
        assert_eq!(saved["providers"]["custom"]["vision"], "off");
        assert_eq!(saved["providers"]["custom"]["effort"], "low");
        assert!(!saved.to_string().contains("offline-test"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn assistant_limit_flags_have_field_errors_before_any_provider_or_session_request() {
        let root = Directory::new("assistant-limit-args");
        let dir = root.0.join("sessions");
        let server = ProviderServer::new(Vec::new());
        settings(&dir, &server);
        for (flag, value, field) in [
            ("--max-time", "30s", "max_time"),
            ("--max-rounds", "1001", "max_rounds"),
            ("--max-images", "2001", "max_images"),
            ("--max-cost", "501", "max_cost"),
        ] {
            let out = output(
                &dir,
                &root.0,
                &[
                    "--json",
                    "assistant",
                    "run",
                    "Observe the workbench",
                    "--camera",
                    "sim:0",
                    flag,
                    value,
                ],
                None,
                &[],
            );
            assert_eq!(
                out.status.code(),
                Some(2),
                "{flag}\n{}",
                String::from_utf8_lossy(&out.stdout)
            );
            let failure = tools_response(&out);
            assert!(
                failure["error"]["details"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["field"] == field),
                "{failure}"
            );
            assert_eq!(
                std::fs::read_dir(&dir).unwrap().count(),
                1,
                "Only the prewritten assistant settings may exist."
            );
        }
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn assistant_ask_returns_a_ready_reviewable_plan_and_private_export() {
        let (_fixture, dir, cwd) = fixture("assistant-ask");
        let server = ProviderServer::new(vec![
            call("overview", "get_overview", json!({})),
            call(
                "photo",
                "capture_photos",
                proposal(photo("assistant-plan.png")),
            ),
            end(),
        ]);
        settings(&dir, &server);
        let plan_out = cwd.join("assistant-plan.json");
        let out = output(
            &dir,
            &cwd,
            &[
                "--json",
                "assistant",
                "ask",
                "--prompt-file",
                "-",
                "--plan-out",
                plan_out.to_str().unwrap(),
            ],
            Some("Capture one photograph\n"),
            &[],
        );
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let result = tools_response(&out)["result"].clone();
        assert!(result["text"].as_str().unwrap().contains("plan is ready"));
        assert_eq!(result["plan"]["version"], 1);
        let calls = result["plan"]["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["tool"], "capture_photos");
        assert_eq!(calls[0]["args"]["camera"], "sim:0");
        let steps = result["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["risk"], "bounded");
        assert_eq!(steps[0]["leaves_computer"], false);
        assert!(
            !steps[0]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["severity"] == "error")
        );
        assert!(!cwd.join("assistant-plan.png").exists());
        let exported: Value = serde_json::from_slice(&std::fs::read(&plan_out).unwrap()).unwrap();
        assert_eq!(exported, result["plan"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&plan_out).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["model"], "offline-camera-model");
        assert!(
            !serde_json::to_string(&*requests)
                .unwrap()
                .contains("offline-test")
        );
        assert!(requests[1]["messages"].to_string().contains("sim:0"));
    }

    #[test]
    fn assistant_ask_apply_saves_the_approved_local_capture() {
        let (_fixture, dir, cwd) = fixture("assistant-apply");
        let server = ProviderServer::new(vec![
            call("overview", "get_overview", json!({})),
            call(
                "photo",
                "capture_photos",
                proposal(photo("assistant-applied.png")),
            ),
            end(),
        ]);
        settings(&dir, &server);
        let result = ok(
            &dir,
            &cwd,
            &[
                "--json",
                "--actor",
                "assistant-test-client",
                "--actor-kind",
                "agent",
                "assistant",
                "ask",
                "Capture a photograph",
                "--apply",
            ],
        );
        assert_eq!(result["plan"]["calls"][0]["tool"], "capture_photos");
        assert!(!result["applied"].is_null());
        assert_png(&cwd.join("assistant-applied.png"));
        let status = tools_ok(&dir, &cwd, &["--json", "--session", "tools-ci", "status"]);
        assert!(status["activity"].as_array().unwrap().iter().any(
            |activity| activity["operation"] == "capture"
                && activity["actor"]["name"] == "assistant-test-client"
                && activity["actor"]["kind"] == "agent"
        ));
        assert_eq!(server.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn assistant_ask_apply_refuses_a_tool_outside_the_default_allowlist() {
        let (_fixture, dir, cwd) = fixture("assistant-apply-refused");
        let server = ProviderServer::new(vec![
            call(
                "change",
                "set_camera_features",
                proposal(
                    json!({"camera":"sim:0","values":[{"feature":"Gain","value":"3","expect":null}],"restart_stream":false}),
                ),
            ),
            end(),
        ]);
        settings(&dir, &server);
        let out = output(
            &dir,
            &cwd,
            &["--json", "assistant", "ask", "Adjust gain", "--apply"],
            None,
            &[],
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let current = tools_ok(
            &dir,
            &cwd,
            &[
                "--json",
                "--session",
                "tools-ci",
                "--camera",
                "sim:0",
                "get",
                "Gain",
            ],
        );
        assert_eq!(current["features"][0]["value"].as_f64(), Some(0.0));
    }

    #[test]
    fn assistant_run_autonomously_captures_finishes_and_exposes_its_action_log() {
        let (_fixture, dir, cwd) = fixture("assistant-run");
        let server = ProviderServer::new(vec![
            call("overview", "get_overview", json!({})),
            call("photo", "capture_photos", photo("assistant-task.png")),
            finish(),
        ]);
        settings(&dir, &server);
        let events = cwd.join("task-events.jsonl");
        let result = ok(
            &dir,
            &cwd,
            &[
                "--json",
                "assistant",
                "run",
                "Capture one photograph",
                "--camera",
                "sim:0",
                "--mode",
                "autonomous",
                "--approvals",
                "deny",
                "--events",
                events.to_str().unwrap(),
            ],
        );
        assert_eq!(result["outcome"], "finished");
        assert_eq!(result["totals"]["captures"], 1);
        assert_eq!(result["totals"]["rounds"], 3);
        assert!(result["leftovers"].is_array());
        assert!(std::path::Path::new(result["log"].as_str().unwrap()).is_file());
        assert_png(&cwd.join("assistant-task.png"));
        let id = result["run_id"].as_str().unwrap();
        let records = assert_end_log(&dir, id, "finished");
        assert_eq!(records[0]["actor"]["kind"], "agent");
        assert!(
            records[0]["actor"]["id"]
                .as_str()
                .unwrap()
                .starts_with("assistant:")
        );
        assert!(records.iter().any(|record| record["kind"] == "tool"
            && record["name"] == "capture_photos"
            && record["outcome"] == "ok"));
        let runs = ok(&dir, &cwd, &["--json", "assistant", "runs"]);
        assert!(
            runs.as_array()
                .unwrap()
                .iter()
                .any(|run| run["run_id"] == id && run["complete"] == true)
        );
        let streamed = std::fs::read_to_string(&events).unwrap();
        let streamed: Vec<Value> = streamed
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(streamed, records);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&events).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(server.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn assistant_run_denies_a_disruptive_overwrite_of_a_human_setting() {
        let (_fixture, dir, cwd) = fixture("assistant-denied");
        tools_ok(
            &dir,
            &cwd,
            &[
                "--json",
                "--session",
                "tools-ci",
                "--camera",
                "sim:0",
                "set",
                "ExposureTime=1000",
            ],
        );
        let server = ProviderServer::new(vec![
            call(
                "change",
                "set_camera_features",
                json!({"camera":"sim:0","values":[{"feature":"ExposureTime","value":"2000","expect":"1000"}],"restart_stream":false}),
            ),
            finish(),
        ]);
        settings(&dir, &server);
        let result = ok(
            &dir,
            &cwd,
            &[
                "--json",
                "assistant",
                "run",
                "Adjust exposure if allowed",
                "--camera",
                "sim:0",
                "--mode",
                "autonomous",
                "--approvals",
                "deny",
            ],
        );
        assert_eq!(result["outcome"], "finished");
        let current = tools_ok(
            &dir,
            &cwd,
            &[
                "--json",
                "--session",
                "tools-ci",
                "--camera",
                "sim:0",
                "get",
                "ExposureTime",
            ],
        );
        assert_eq!(current["features"][0]["value"].as_f64(), Some(1000.0));
        let records = assert_end_log(&dir, result["run_id"].as_str().unwrap(), "finished");
        assert!(records.iter().any(|record| record["kind"] == "approval"
            && record["risk"] == "disruptive"
            && record["decision"] == "denied"));
        assert!(!records.iter().any(|record| record["kind"] == "tool"
            && record["name"] == "set_camera_features"
            && record["outcome"] == "ok"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]["messages"]
                .to_string()
                .contains("Do not retry this action")
        );
    }

    #[test]
    fn assistant_run_round_limit_stops_a_loop_with_a_reviewable_failure() {
        let (_fixture, dir, cwd) = fixture("assistant-round-limit");
        let server = ProviderServer::new(
            (1..=5)
                .map(|round| call(&format!("overview-{round}"), "get_overview", json!({})))
                .collect(),
        );
        settings(&dir, &server);
        let out = output(
            &dir,
            &cwd,
            &[
                "--json",
                "assistant",
                "run",
                "Keep observing",
                "--camera",
                "sim:0",
                "--mode",
                "autonomous",
                "--approvals",
                "deny",
                "--max-rounds",
                "2",
            ],
            None,
            &[],
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let failure = tools_response(&out);
        assert_eq!(failure["ok"], false);
        assert_eq!(failure["error"]["code"], "operation_failed");
        let result = &failure["error"]["details"];
        assert_eq!(result["outcome"], "limit");
        assert_eq!(result["totals"]["rounds"], 2);
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_end_log(&dir, result["run_id"].as_str().unwrap(), "limit");
    }

    #[test]
    fn assistant_provider_failures_put_scrubbed_details_in_the_json_error() {
        let root = Directory::new("assistant-provider-error");
        let dir = root.0.join("sessions");
        let server = ProviderServer::statuses(vec![(
            400,
            json!({"error":{"message":"Model fixture rejected this request. Bearer planted-private-key rtsp://user:planted-password@camera/live"}}),
        )]);
        settings(&dir, &server);
        let out = output(
            &dir,
            &root.0,
            &["--json", "assistant", "ask", "Read the session overview"],
            None,
            &[],
        );
        assert_eq!(out.status.code(), Some(1));
        let failure = tools_response(&out);
        let message = failure["error"]["details"]["provider_message"]
            .as_str()
            .unwrap();
        assert!(message.contains("Model fixture rejected"));
        assert!(!failure.to_string().contains("planted-private-key"));
        assert!(!failure.to_string().contains("planted-password"));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn assistant_provider_failure_after_capture_keeps_effects_and_the_complete_event_log() {
        let (_fixture, dir, cwd) = fixture("assistant-partial-provider-error");
        let server = ProviderServer::statuses(vec![
            (
                200,
                call(
                    "photo",
                    "capture_photos",
                    photo("assistant-before-error.png"),
                ),
            ),
            (
                400,
                json!({"error":{"message":"Model fixture rejected verification. Bearer planted-private-key rtsp://user:planted-password@camera/live"}}),
            ),
        ]);
        settings(&dir, &server);
        let events = cwd.join("failed-task-events.jsonl");
        let out = output(
            &dir,
            &cwd,
            &[
                "--json",
                "assistant",
                "run",
                "Capture one photograph and verify it",
                "--camera",
                "sim:0",
                "--mode",
                "autonomous",
                "--approvals",
                "deny",
                "--events",
                events.to_str().unwrap(),
            ],
            None,
            &[],
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let failure = tools_response(&out);
        assert_eq!(failure["ok"], false);
        let details = &failure["error"]["details"];
        assert_eq!(details["outcome"], "failed");
        assert_eq!(details["totals"]["captures"], 1);
        assert_eq!(details["totals"]["rounds"], 2);
        assert!(details["leftovers"].is_array());
        let id = details["run_id"].as_str().unwrap();
        assert!(std::path::Path::new(details["log"].as_str().unwrap()).is_file());
        assert!(
            details["provider_message"]
                .as_str()
                .unwrap()
                .contains("Model fixture rejected verification")
        );
        assert!(!failure.to_string().contains("planted-private-key"));
        assert!(!failure.to_string().contains("planted-password"));
        assert_png(&cwd.join("assistant-before-error.png"));
        let records = assert_end_log(&dir, id, "failed");
        assert!(records.iter().any(|record| record["kind"] == "tool"
            && record["name"] == "capture_photos"
            && record["outcome"] == "ok"));
        let streamed = std::fs::read_to_string(events).unwrap();
        let streamed: Vec<Value> = streamed
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            streamed, records,
            "A failed provider request must still flush the end record."
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn assistant_ask_provider_failure_keeps_the_partial_plan_without_capturing() {
        let (_fixture, dir, cwd) = fixture("assistant-partial-plan-error");
        let mut proposed = call(
            "photo",
            "capture_photos",
            proposal(photo("assistant-unrun-partial.png")),
        );
        proposed["choices"][0]["message"]["content"] = json!("I will prepare the photograph.");
        let server = ProviderServer::statuses(vec![
            (200, proposed),
            (
                400,
                json!({"error":{"message":"Model fixture rejected the plan follow-up. Bearer planted-private-key rtsp://user:planted-password@camera/live"}}),
            ),
        ]);
        settings(&dir, &server);
        let out = output(
            &dir,
            &cwd,
            &[
                "--json",
                "assistant",
                "ask",
                "Prepare a photograph without running it",
            ],
            None,
            &[],
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let failure = tools_response(&out);
        assert_eq!(failure["ok"], false);
        let details = &failure["error"]["details"];
        assert!(
            details["provider_message"]
                .as_str()
                .unwrap()
                .contains("Model fixture rejected the plan follow-up")
        );
        assert_eq!(details["text"], "I will prepare the photograph.");
        assert_eq!(details["plan"]["version"], 1);
        let calls = details["plan"]["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["tool"], "capture_photos");
        assert_eq!(calls[0]["args"]["camera"], "sim:0");
        let steps = details["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["tool"], "capture_photos");
        assert_eq!(steps[0]["risk"], "bounded");
        assert!(!failure.to_string().contains("planted-private-key"));
        assert!(!failure.to_string().contains("planted-password"));
        assert!(!cwd.join("assistant-unrun-partial.png").exists());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn assistant_sigterm_stops_the_task_and_finishes_its_action_log() {
        struct Running(Option<Child>);
        impl Drop for Running {
            fn drop(&mut self) {
                if let Some(child) = &mut self.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let (_fixture, dir, cwd) = fixture("assistant-sigterm");
        let server = ProviderServer::new(
            (1..=5)
                .map(|round| call(&format!("overview-{round}"), "get_overview", json!({})))
                .collect(),
        );
        settings(&dir, &server);
        let mut running = Running(Some(
            command(
                &dir,
                &cwd,
                &[
                    "--json",
                    "assistant",
                    "run",
                    "Keep observing until stopped",
                    "--camera",
                    "sim:0",
                    "--mode",
                    "autonomous",
                    "--approvals",
                    "deny",
                ],
                &[],
            )
            .spawn()
            .unwrap(),
        ));
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while server.requests.lock().unwrap().is_empty() {
            assert!(
                running.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "The assistant exited before sending its first request."
            );
            assert!(
                std::time::Instant::now() < ready_deadline,
                "The assistant did not reach the scripted provider."
            );
            thread::sleep(Duration::from_millis(10));
        }
        let pid = running.0.as_ref().unwrap().id().to_string();
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        let stop_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while running.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < stop_deadline,
                "SIGTERM did not stop the assistant within 10 seconds."
            );
            thread::sleep(Duration::from_millis(10));
        }
        let out = running.0.take().unwrap().wait_with_output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let failure = tools_response(&out);
        assert_eq!(failure["ok"], false);
        assert_eq!(failure["error"]["code"], "operation_failed");
        let details = &failure["error"]["details"];
        assert_eq!(details["outcome"], "stopped");
        assert_eq!(details["totals"]["captures"], 0);
        assert!(details["totals"]["rounds"].as_u64().unwrap() >= 1);
        assert!(std::path::Path::new(details["log"].as_str().unwrap()).is_file());
        let records = assert_end_log(&dir, details["run_id"].as_str().unwrap(), "stopped");
        assert!(records.iter().any(|record| record["kind"] == "stop"));
    }
}
