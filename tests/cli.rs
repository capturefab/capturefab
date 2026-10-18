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
/// Spawned binaries keep their cache, such as dataset staging, out of the
/// user's.
fn test_cache() -> PathBuf {
    std::env::temp_dir().join("capturefab-test-cache")
}

fn cmd(dir: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .args(args)
        .output()
        .unwrap()
}

fn serve_at(dir: &PathBuf, cwd: &PathBuf, name: &str) -> Child {
    serve_with(dir, cwd, name, &[])
}

fn serve_with(dir: &PathBuf, cwd: &PathBuf, name: &str, env: &[(&str, &str)]) -> Child {
    serve_ready(serve_command(dir, cwd, name, env).spawn().unwrap())
}

fn serve_command(dir: &PathBuf, cwd: &PathBuf, name: &str, env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .envs(env.iter().copied())
        .current_dir(cwd)
        .args(["--json", "serve", "--name", name])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

/// Wait for a spawned serve to say it is ready.
fn serve_ready(mut child: Child) -> Child {
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
            .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
            .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
            .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
            .env("CAPTUREFAB_CACHE_DIR", test_cache())
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

fn ring_files(dir: &PathBuf) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "shm"))
        .collect()
}

#[test]
fn viewerless_streams_report_no_losses_and_keep_frames_off_disk() {
    let root = temp_root("ring-viewerless");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &root, "rings"),
        dir: root.clone(),
    };
    json(&dir, &["--json", "connect", "sim:0"]);
    json(&dir, &["--json", "start"]);
    std::thread::sleep(std::time::Duration::from_secs(5));
    let status = json(&dir, &["--json", "status"]);
    let camera = &status["cameras"][0];
    let frames = camera["frames"].as_u64().unwrap();
    let dropped = camera["dropped"].as_u64().unwrap();
    assert!(frames > 30, "{camera}");
    // Frames nobody previewed were neither lost nor skipped.
    assert_eq!(
        (camera["lost"].as_u64(), camera["preview_skipped"].as_u64()),
        (Some(0), Some(0))
    );
    assert!(dropped * 10 < frames, "{dropped} dropped of {frames}");
    assert_eq!(status["lost"], 0);
    let backing = json(&dir, &["--json", "doctor"])["ring_backing"].clone();
    if cfg!(target_os = "linux") {
        assert_eq!(backing, "memfd");
        let pid = camera["worker_pid"].as_u64().unwrap();
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
        assert!(maps.contains("/memfd:capturefab-ring"), "{maps}");
    } else if cfg!(target_os = "macos") {
        assert_eq!(backing, "posix-shm");
    }
    if cfg!(unix) {
        assert!(ring_files(&dir).is_empty(), "frame rings stay in memory");
    }
    json(&dir, &["--json", "stop"]);
}

#[test]
fn a_direct_timelapse_waits_for_its_job_and_reports_it() {
    let root = temp_root("direct-timelapse");
    let dir = root.join("sessions");
    let frames = root.join("frames");
    std::fs::create_dir_all(&root).unwrap();
    let result = json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            frames.to_str().unwrap(),
            "-n",
            "3",
            "--interval",
            "400ms",
        ],
    );
    assert_eq!(result["job"]["status"], "complete", "{result}");
    assert_eq!(result["job"]["captured"], 3, "{result}");
    assert_eq!(std::fs::read_dir(&frames).unwrap().count(), 3);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[cfg(unix)]
fn a_new_session_sweeps_ring_files_a_killed_one_left() {
    let root = temp_root("ring-sweep");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let files = [("CAPTUREFAB_RING_FILE_TEST", "1")];
    let mut killed = serve_with(&dir, &root, "killed", &files);
    json(&dir, &["--json", "--session", "killed", "connect", "sim:0"]);
    let stale = ring_files(&dir);
    assert_eq!(stale.len(), 1);
    killed.kill().unwrap();
    killed.wait().unwrap();
    assert!(stale[0].exists(), "a killed session cannot remove its ring");
    let _fixture = Fixture {
        child: serve_with(&dir, &root, "next", &files),
        dir: root.clone(),
    };
    json(&dir, &["--json", "--session", "next", "connect", "sim:1"]);
    let live = ring_files(&dir);
    assert!(!stale[0].exists(), "{live:?}");
    assert_eq!(live.len(), 1);
    json(&dir, &["--json", "--session", "next", "disconnect"]);
    assert!(ring_files(&dir).is_empty());
}

/// Send `signal` to `child` and wait up to 30 s for it to exit.
#[cfg(unix)]
fn signal_and_wait(child: &mut Child, signal: i32) -> std::process::ExitStatus {
    // SAFETY: kill only sends a signal to the child this test spawned.
    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(std::time::Instant::now() < deadline, "still running");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Whether process `pid` still exists, within a few seconds for its
/// parent to reap it.
#[cfg(unix)]
fn process_lives(pid: i32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// `systemctl stop` sends SIGTERM, and a closed terminal SIGHUP: serve then
/// shuts down as it does on Ctrl-C. Its camera workers stop, the upload
/// queue's journal is folded into the queue, the session's descriptor goes
/// and it exits successfully.
#[test]
#[cfg(unix)]
fn serve_shuts_down_cleanly_on_sigterm_and_sighup() {
    for (signal, name) in [(libc::SIGTERM, "term"), (libc::SIGHUP, "hup")] {
        let root = temp_root(&format!("signal-{name}"));
        let (dir, staging) = (root.join("sessions"), root.join("staging"));
        std::fs::create_dir_all(&root).unwrap();
        let env = [
            ("CAPTUREFAB_STAGING_DIR", staging.to_str().unwrap()),
            ("CAPTUREFAB_FTP_PASSWORD", "secret"),
        ];
        let mut serve = serve_with(&dir, &root, name, &env);
        json(&dir, &["--json", "--session", name, "connect", "sim:0"]);
        json(&dir, &["--json", "--session", name, "start"]);
        let status = json(&dir, &["--json", "--session", name, "status"]);
        let worker = status["cameras"][0]["worker_pid"].as_i64().unwrap() as i32;
        assert!(process_lives(worker));
        // In a process group of its own, which a terminal's Ctrl-C or
        // hangup does not reach.
        // SAFETY: getpgid only reads the process group of a child of serve.
        assert_eq!(unsafe { libc::getpgid(worker) }, worker);
        if cfg!(feature = "ftp") {
            // A capture whose upload stays queued: nothing listens on port 1.
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
            json_env(
                &dir,
                &env,
                &[
                    "--json",
                    "--session",
                    name,
                    "--camera",
                    "sim:0",
                    "capture",
                    "-o",
                    "queued.png",
                    "--destination",
                    "down",
                ],
            );
            assert!(dir.join("uploads.journal").is_file());
        }
        let descriptor = dir.join(format!("{name}.json"));
        assert!(descriptor.is_file());
        let exit = signal_and_wait(&mut serve, signal);
        assert!(exit.success(), "{name}: {exit:?}");
        assert!(!descriptor.exists(), "{name}: the descriptor is removed");
        assert!(!process_lives(worker), "{name}: the camera worker exits");
        if cfg!(feature = "ftp") {
            assert!(!dir.join("uploads.journal").exists(), "{name}: folded");
            let queue: Value =
                serde_json::from_slice(&std::fs::read(dir.join("uploads.json")).unwrap()).unwrap();
            assert_eq!(queue["items"].as_array().unwrap().len(), 1, "{queue}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// `nohup` starts serve with SIGHUP ignored, so it must outlive the
/// terminal it was started from; SIGTERM still stops it.
#[test]
#[cfg(unix)]
fn serve_under_nohup_ignores_sighup() {
    use std::os::unix::process::CommandExt;
    let root = temp_root("nohup");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let mut command = serve_command(&dir, &root, "nohup", &[]);
    // SAFETY: signal is async-signal-safe, as pre_exec requires.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut serve = serve_ready(command.spawn().unwrap());
    // SAFETY: kill only sends a signal to the child this test spawned.
    assert_eq!(unsafe { libc::kill(serve.id() as i32, libc::SIGHUP) }, 0);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(serve.try_wait().unwrap().is_none(), "serve outlives SIGHUP");
    assert!(dir.join("nohup.json").is_file());
    let exit = signal_and_wait(&mut serve, libc::SIGTERM);
    assert!(exit.success(), "{exit:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A terminal sends Ctrl-C, and the shell the SIGHUP of a closed terminal,
/// to serve's whole process group. Its camera workers are not in it, so serve
/// still finishes a recording to a bucket: kept in the storage ledger and
/// queued for upload.
#[test]
#[cfg(unix)]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn serve_finishes_a_recording_when_its_terminal_signals_the_process_group() {
    use std::os::unix::process::CommandExt;
    if !cfg!(feature = "ftp") {
        return;
    }
    for (signal, name) in [(libc::SIGINT, "int"), (libc::SIGHUP, "hup")] {
        let root = temp_root(&format!("group-{name}"));
        let (dir, staging) = (root.join("sessions"), root.join("staging"));
        std::fs::create_dir_all(&root).unwrap();
        let env = [
            ("CAPTUREFAB_STAGING_DIR", staging.to_str().unwrap()),
            ("CAPTUREFAB_FTP_PASSWORD", "secret"),
        ];
        let mut command = serve_command(&dir, &root, name, &env);
        command.process_group(0);
        let mut serve = serve_ready(command.spawn().unwrap());
        json(&dir, &["--json", "--session", name, "connect", "sim:rec"]);
        json(
            &dir,
            &["--json", "--session", name, "set", "Width=64", "Height=64"],
        );
        // Nothing listens on port 1, so the upload stays queued.
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
        json_env(
            &dir,
            &env,
            &[
                "--json",
                "--session",
                name,
                "record",
                "-o",
                "clip.mp4",
                "--destination",
                "down",
                "--encoder",
                "libx264",
                "--fps",
                "10",
                "--max-file-size",
                "8MiB",
            ],
        );
        let start = std::time::Instant::now();
        loop {
            let status = json(&dir, &["--json", "--session", name, "status"]);
            if status["forwarding"].is_string() && start.elapsed().as_secs() >= 1 {
                break;
            }
            assert!(start.elapsed().as_secs() < 10, "recording failed to start");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // SAFETY: serve leads the group this test made for it.
        assert_eq!(unsafe { libc::killpg(serve.id() as i32, signal) }, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let exit = loop {
            if let Some(status) = serve.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{name}: still running"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(exit.success(), "{name}: {exit:?}");
        let storage = json(&dir, &["--json", "storage"]);
        assert_eq!(storage["files"], 1, "{name}: {storage}");
        assert_eq!(storage["externally_changed_files"], 0, "{name}: {storage}");
        assert_eq!(storage["active_reservations"], 0, "{name}: {storage}");
        let queue: Value =
            serde_json::from_slice(&std::fs::read(dir.join("uploads.json")).unwrap()).unwrap();
        let items = queue["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{name}: {queue}");
        assert!(
            items[0]["path"].as_str().unwrap().ends_with("clip.mp4"),
            "{name}: {queue}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// A recording that runs when serve gets SIGTERM is finished: complete on
/// disk and in the storage ledger, with nothing left reserved.
#[test]
#[cfg(unix)]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn serve_finishes_a_recording_on_sigterm() {
    let root = temp_root("signal-recording");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let mut serve = serve_at(&dir, &root, "rec");
    json(&dir, &["--json", "--session", "rec", "connect", "sim:rec"]);
    json(
        &dir,
        &["--json", "--session", "rec", "set", "Width=64", "Height=64"],
    );
    // An MP4 is unreadable until FFmpeg writes its index at the end.
    let clip = root.join("clip.mp4");
    json(
        &dir,
        &[
            "--json",
            "--session",
            "rec",
            "record",
            "-o",
            clip.to_str().unwrap(),
            "--encoder",
            "libx264",
            "--fps",
            "10",
            "--max-file-size",
            "8MiB",
        ],
    );
    let start = std::time::Instant::now();
    loop {
        let status = json(&dir, &["--json", "--session", "rec", "status"]);
        if status["forwarding"].is_string() && start.elapsed().as_secs() >= 1 {
            break;
        }
        assert!(start.elapsed().as_secs() < 10, "recording failed to start");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let exit = signal_and_wait(&mut serve, libc::SIGTERM);
    assert!(exit.success(), "{exit:?}");
    let storage = json(&dir, &["--json", "storage"]);
    assert_eq!(storage["files"], 1, "{storage}");
    assert_eq!(storage["active_reservations"], 0, "{storage}");
    assert_eq!(storage["reserved_bytes"], 0, "{storage}");
    let ffmpeg = std::env::var("CAPTUREFAB_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let decoded = Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&clip)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success() && decoded.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
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
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache());
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
    assert_eq!(status["destinations"]["archive"]["failed"], 1);
    let retried = json(&dir, &["--json", "uploads", "retry", "--now"]);
    assert_eq!(retried["retried"], 1);
    let status = json(&dir, &["--json", "uploads"]);
    assert_eq!(status["destinations"]["archive"]["pending"], 1);
    assert_eq!(status["items"][0]["bucket"], item["bucket"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_reports_destination_features() {
    let dir = temp_root("doctor-destinations");
    let doctor = json(&dir, &["--json", "doctor"]);
    assert!(doctor["host"]["cpu_cores"].as_u64().unwrap() >= 1);
    assert!(doctor["efficiency"]["efficient"].is_boolean());
    assert!(matches!(
        doctor["efficiency"]["setting"].as_str(),
        Some("auto" | "smooth" | "efficient")
    ));
    // CAPTUREFAB_SESSION_DIR is both the runtime and the state directory;
    // session_directory stays for older scripts.
    let path = Value::from(dir.to_string_lossy().into_owned());
    for key in ["session_directory", "runtime_directory", "state_directory"] {
        assert_eq!(doctor[key], path, "{key}");
    }
    for key in ["session", "runtime", "state"] {
        assert_eq!(doctor["paths"][key]["path"], path, "{key}");
    }
    let cache = Value::from(test_cache().to_string_lossy().into_owned());
    assert_eq!(doctor["cache_directory"], cache);
    assert_eq!(doctor["paths"]["cache"]["path"], cache);
    let checks = doctor["checks"].as_array().unwrap();
    assert!(checks.iter().all(|check| {
        check["id"].is_string()
            && matches!(check["level"].as_str(), Some("ok" | "warn"))
            && check["message"].is_string()
    }));
    assert!(checks.iter().any(|check| check["id"] == "efficiency"));
    assert!(
        doctor["graphics"]["available"].is_boolean(),
        "{}",
        doctor["graphics"]
    );
    // Timed after the first launch, which can include the OS scanning a
    // freshly linked binary, and without the GPU probe, whose own deadline
    // bounds a stalled driver.
    let missing = dir.join("no-gpu-probe");
    let started = std::time::Instant::now();
    let out = cmd_env(
        &dir,
        &[("CAPTUREFAB_EXECUTABLE", missing.to_str().unwrap())],
        &["--json", "doctor"],
    );
    let elapsed = started.elapsed();
    assert!(out.status.success());
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "doctor took {elapsed:?}"
    );
    let graphics = &serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"]["graphics"];
    assert_eq!(graphics["available"], false, "{graphics}");
    if cfg!(feature = "gui") {
        assert!(
            graphics["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("GPU probe")),
            "{graphics}"
        );
    }
    for (feature, compiled) in [
        ("s3", cfg!(feature = "s3")),
        ("ftp", cfg!(feature = "ftp")),
        ("sftp", cfg!(feature = "sftp")),
        ("hubs", cfg!(feature = "hubs")),
        ("keychain", cfg!(feature = "keychain")),
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
    if cfg!(feature = "s3") {
        assert!(schema["uploads"]["cli"].as_str().unwrap().contains("--now"));
        assert!(
            schema["uploads"]["status"]
                .as_str()
                .unwrap()
                .contains("destinations")
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn storage_status_reports_free_space_and_the_schema_its_floor() {
    let dir = temp_root("storage-status");
    let status = json(&dir, &["--json", "storage"]);
    assert!(status["free_bytes"].is_u64(), "{status}");
    assert_eq!(status["files"], 0);
    let schema = json(&dir, &["--json", "schema"]);
    assert_eq!(schema["storage"]["free_space_floor"], 512 * 1024 * 1024);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_prints_a_checklist_and_follows_the_efficiency_setting() {
    let dir = temp_root("doctor-checklist");
    let out = cmd(&dir, &["doctor"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.lines().any(|line| line.starts_with("ok    ")),
        "{text}"
    );
    assert!(text.contains("#host-checks-and-efficiency-mode"), "{text}");
    let efficient = cmd_env(
        &dir,
        &[("CAPTUREFAB_EFFICIENCY", "efficient")],
        &["--json", "doctor"],
    );
    let efficient: Value = serde_json::from_slice(&efficient.stdout).unwrap();
    assert_eq!(efficient["result"]["efficiency"]["efficient"], true);
    let flag = cmd_env(
        &dir,
        &[("CAPTUREFAB_EFFICIENCY", "efficient")],
        &["--efficiency", "smooth", "--json", "doctor"],
    );
    let flag: Value = serde_json::from_slice(&flag.stdout).unwrap();
    assert_eq!(
        flag["result"]["efficiency"],
        serde_json::json!({"setting":"smooth","efficient":false,"reasons":["set by --efficiency smooth"]})
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_measures_the_storage_budget_on_the_capture_folder_disk() {
    let root = temp_root("doctor-budget");
    let dir = root.join("sessions");
    let shots = root.join("shots");
    std::fs::create_dir_all(&shots).unwrap();
    let shot = shots.join("a.png");
    // The default capture folder is ~/Capturefab; it need not exist yet.
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = [("HOME", home.to_str().unwrap())];
    json_env(
        &dir,
        &env,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            shot.to_str().unwrap(),
        ],
    );
    let doctor = json_env(&dir, &env, &["--json", "doctor"]);
    let budget = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "storage_budget")
        .unwrap();
    let message = budget["message"].as_str().unwrap();
    // doctor reads the storage ledger's usage summary, so the capture's
    // bytes count as used, and measures free space where `storage --json`
    // does.
    assert!(
        message.contains(&format!("free on {}", home.join("Capturefab").display())),
        "{message}"
    );
    assert!(
        !message.contains(", 0.0 KiB used") && !message.contains("(0.0 KiB of"),
        "{message}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The binary after the split, with XDG_RUNTIME_DIR at `run` and the state
/// directory in `state`; `root/login` stands in for /run/user/<uid>, so the
/// user's own is never looked at. Returns the JSON result and stderr.
#[cfg(target_os = "linux")]
fn upgraded(
    root: &std::path::Path,
    run: &std::path::Path,
    state: &std::path::Path,
    env: &[(&str, &str)],
    args: &[&str],
) -> (Value, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env_remove("CAPTUREFAB_SESSION_DIR")
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .env("CAPTUREFAB_LOGIN_RUNTIME_DIR_TEST", root.join("login"))
        .env("XDG_RUNTIME_DIR", run)
        .env("XDG_STATE_HOME", state)
        .envs(env.iter().copied())
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result = serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone();
    (result, String::from_utf8(out.stderr).unwrap())
}

/// Linux keeps settings, destinations, the upload queue and the storage
/// ledger out of the runtime directory, a RAM disk that logout clears. What
/// an older version kept there moves to the state directory once.
#[cfg(target_os = "linux")]
#[test]
fn linux_state_moves_out_of_the_runtime_directory_once() {
    use std::os::unix::fs::DirBuilderExt;
    let root = temp_root("state-dir");
    let (home, run, state) = (root.join("home"), root.join("run"), root.join("state"));
    let (archive, staging) = (root.join("archive"), root.join("staging"));
    let old = run.join("capturefab");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&old)
        .unwrap();
    let shared = [
        ("HOME", home.to_str().unwrap()),
        ("CAPTUREFAB_STAGING_DIR", staging.to_str().unwrap()),
        ("CAPTUREFAB_FTP_PASSWORD", "secret"),
    ];
    // An older version kept everything in its session directory.
    let before = |args: &[&str]| json_env(&old, &shared, args);
    before(&[
        "--json",
        "destination",
        "add-folder",
        "archive",
        archive.to_str().unwrap(),
    ]);
    before(&["--json", "storage", "configure", "--max-space", "2GiB"]);
    let shot = root.join("shots/a.png");
    before(&[
        "--json",
        "--camera",
        "sim:0",
        "capture",
        "-o",
        shot.to_str().unwrap(),
    ]);
    if cfg!(feature = "ftp") {
        before(&[
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
        ]);
        before(&[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            "b.png",
            "--destination",
            "down",
        ]);
    }
    let after = |args: &[&str]| upgraded(&root, &run, &state, &shared, args);
    let (listed, moved) = after(&["--json", "destination", "list"]);
    assert_eq!(listed["destinations"][0]["name"], "archive", "{listed}");
    assert!(moved.contains("moved destinations.json from"), "{moved}");
    let new = state.join("capturefab");
    assert!(new.join("destinations.json").is_file());
    assert!(!old.join("destinations.json").exists());
    let (storage, moved) = after(&["--json", "storage"]);
    assert!(moved.contains("storage.json"), "{moved}");
    assert_eq!(storage["policy"]["max_bytes"], 2_u64 << 30, "{storage}");
    assert_eq!(storage["files"], if cfg!(feature = "ftp") { 2 } else { 1 });
    assert_eq!(
        storage["ledger"],
        Value::from(new.join("storage.json").to_str().unwrap())
    );
    assert!(!old.join("storage.json").exists() && !old.join("storage-policy.json").exists());
    if cfg!(feature = "ftp") {
        let (uploads, moved) = after(&["--json", "uploads"]);
        // The queue may be only a journal so far.
        assert!(moved.contains("moved uploads"), "{moved}");
        assert_eq!(uploads["pending"], 1, "{uploads}");
        assert!(!old.join("uploads.json").exists() && !old.join("uploads.journal").exists());
    }
    // Once moved, nothing moves again, and doctor names both directories.
    let (doctor, quiet) = after(&["--json", "doctor"]);
    assert!(!quiet.contains("moved"), "{quiet}");
    let runtime = Value::from(old.to_str().unwrap());
    assert_eq!(doctor["runtime_directory"], runtime);
    assert_eq!(doctor["session_directory"], runtime);
    assert_eq!(
        doctor["state_directory"],
        Value::from(new.to_str().unwrap())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A service without XDG_RUNTIME_DIR kept its state on disk while logins
/// kept theirs in the runtime directory. After an upgrade, whichever process
/// runs first moves the one copy there is, and a copy that cannot move is
/// named rather than dropped.
#[cfg(target_os = "linux")]
#[test]
fn linux_state_moves_from_whichever_folder_an_older_version_used() {
    use std::os::unix::fs::DirBuilderExt;
    let root = temp_root("state-sources");
    let (home, run, state) = (root.join("home"), root.join("run"), root.join("state"));
    let service = home.join(".cache/capturefab/sessions");
    let shell = run.join("capturefab");
    for dir in [&service, &shell] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .unwrap();
    }
    let shared = [("HOME", home.to_str().unwrap())];
    let configure = |dir: &PathBuf, args: &[&str]| {
        let mut command = vec!["--json", "storage", "configure"];
        command.extend(args);
        json_env(dir, &shared, &command)
    };
    configure(
        &service,
        &["--max-space", "3GiB", "--on-full", "delete-oldest"],
    );
    let after = |args: &[&str]| upgraded(&root, &run, &state, &shared, args);
    // An SSH shell's runtime directory holds nothing; the service's budget
    // still moves rather than falling back to the default.
    let (storage, moved) = after(&["--json", "storage"]);
    assert!(
        moved.contains(&format!("from {}", service.display())),
        "{moved}"
    );
    assert_eq!(storage["policy"]["max_bytes"], 3_u64 << 30, "{storage}");
    assert_eq!(storage["policy"]["on_full"], "delete-oldest");
    assert!(!service.join("storage-policy.json").exists());
    // A copy an older shell made meanwhile never replaces it, and is named
    // on stderr and by doctor.
    configure(&shell, &["--max-space", "1GiB"]);
    let (storage, left) = after(&["--json", "storage"]);
    assert_eq!(storage["policy"]["max_bytes"], 3_u64 << 30, "{storage}");
    assert!(
        left.contains(&format!(
            "not using the storage ledger and budget an older version left in {}",
            shell.display()
        )),
        "{left}"
    );
    let (doctor, _) = after(&["--json", "doctor"]);
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "old_state")
        .cloned()
        .unwrap_or_else(|| panic!("{doctor}"));
    assert_eq!(check["level"], "warn");
    let message = check["message"].as_str().unwrap();
    assert!(message.contains(shell.to_str().unwrap()), "{message}");
    assert!(shell.join("storage-policy.json").is_file(), "never deleted");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn serve_greets_people_and_keeps_its_json_ready_line() {
    let dir = temp_root("serve-banner");
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .args(["serve", "--name", "banner"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = lines.next().unwrap().unwrap();
    let stop = lines.next().unwrap().unwrap();
    let _fixture = Fixture { child, dir };
    assert!(
        ready.starts_with("Session banner is ready (pid "),
        "{ready}"
    );
    assert!(
        ready.ends_with("Try: capturefab --session banner discover"),
        "{ready}"
    );
    assert_eq!(stop, "Ctrl-C stops the session.");
}

#[cfg(not(feature = "gui"))]
#[test]
fn headless_build_without_a_command_explains_serve() {
    let dir = temp_root("headless-window");
    let out = cmd(&dir, &[]);
    assert_eq!(out.status.code(), Some(5));
    let message = String::from_utf8_lossy(&out.stderr);
    assert!(
        message.contains("capturefab serve --name bench"),
        "{message}"
    );
    // It fails before creating a session, which would create the directory.
    assert!(!dir.exists());
    // Asked for JSON, or for a session, it is still unsupported.
    let out = cmd(&dir, &["--json"]);
    assert_eq!(out.status.code(), Some(5));
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply["error"]["code"], "unsupported", "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("capturefab serve --name bench"),
        "{reply}"
    );
    let out = cmd(&dir, &["--session", "bench"]);
    assert_eq!(out.status.code(), Some(5));
    assert!(!dir.exists());
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
    // The ledger is a snapshot plus a journal of later changes.
    let ledger = ["storage.json", "storage.journal"]
        .map(|name| std::fs::read_to_string(dir.join(name)).unwrap_or_default())
        .concat();
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
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
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
