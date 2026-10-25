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

/// The binary with isolated state, and an update URL that goes nowhere: a
/// closed loopback port, never capturefab.com.
fn binary(dir: &PathBuf) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .env("CAPTUREFAB_UPDATE_URL", "http://127.0.0.1:9/releases.json");
    command
}

fn cmd(dir: &PathBuf, args: &[&str]) -> std::process::Output {
    binary(dir).args(args).output().unwrap()
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A test runner may itself inherit ignored SIGHUP. Ordinary fixtures
        // represent a terminal launch; the explicit nohup test overrides this
        // in its later pre_exec hook to exercise inherited ignore separately.
        // SAFETY: signal is async-signal-safe, as pre_exec requires.
        unsafe {
            command.pre_exec(|| {
                if libc::signal(libc::SIGHUP, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
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
    // The CLI refuses `-n 3 -o burst.png`; workflow and RPC capture steps
    // keep filling a folder of that name, so validate and apply agree.
    std::fs::write(&workflow, serde_json::to_vec(&serde_json::json!({"version":1,"steps":[
        {"camera":"sim:workflow","command":{"op":"capture","output":"frames/burst.png","count":3,"timeout_ms":2000,"format":"png"}}
    ]})).unwrap()).unwrap();
    json(
        &dir,
        &["--json", "workflow", "validate", workflow.to_str().unwrap()],
    );
    let burst = json(
        &dir,
        &["--json", "workflow", "apply", workflow.to_str().unwrap()],
    );
    assert_eq!(burst["completed_steps"], 1);
    assert!(plans.join("frames/burst.png/frame-000003.png").exists());

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
        child: serve_with(&dir, &root, "cannot-start", &env),
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
        vec!["destination", "add-folder", "../bad", "/tmp"],
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
    assert!(refused.contains("is already running"), "{refused}");
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
    // Wait for the frames this test checks instead of a fixed recording time.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        let status = json(&dir, &["--json", "status"]);
        if status["cameras"][0]["frames"].as_u64().unwrap() > 30 {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "viewerless stream never produced enough frames: {status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
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
/// shuts down as it does on Ctrl-C. Its camera workers stop, saved captures
/// remain readable, the session's descriptor goes and it exits successfully.
#[test]
#[cfg(unix)]
fn serve_shuts_down_cleanly_on_sigterm_and_sighup() {
    for (signal, name) in [(libc::SIGTERM, "term"), (libc::SIGHUP, "hup")] {
        let root = temp_root(&format!("signal-{name}"));
        let dir = root.join("sessions");
        let captures = root.join("captures");
        std::fs::create_dir_all(&root).unwrap();
        let mut serve = serve_at(&dir, &root, name);
        json(&dir, &["--json", "--session", name, "connect", "sim:0"]);
        json(&dir, &["--json", "--session", name, "start"]);
        let status = json(&dir, &["--json", "--session", name, "status"]);
        let worker = status["cameras"][0]["worker_pid"].as_i64().unwrap() as i32;
        assert!(process_lives(worker));
        // Camera workers have their own process groups, outside terminal signals.
        assert_eq!(unsafe { libc::getpgid(worker) }, worker);
        json(
            &dir,
            &[
                "--json",
                "destination",
                "add-folder",
                "captures",
                captures.to_str().unwrap(),
            ],
        );
        json(
            &dir,
            &[
                "--json",
                "--session",
                name,
                "--camera",
                "sim:0",
                "capture",
                "-o",
                "saved.png",
                "--destination",
                "captures",
            ],
        );
        let path = captures.join("saved.png");
        let saved = std::fs::read(&path).unwrap();
        assert!(saved.starts_with(b"\x89PNG"));
        let descriptor = dir.join(format!("{name}.json"));
        assert!(descriptor.is_file());
        let exit = signal_and_wait(&mut serve, signal);
        assert!(exit.success(), "{name}: {exit:?}");
        assert!(!descriptor.exists(), "{name}: the descriptor is removed");
        assert!(!process_lives(worker), "{name}: the camera worker exits");
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        let storage = json(&dir, &["--json", "storage"]);
        assert_eq!(storage["files"], 1);
        assert_eq!(storage["active_reservations"], 0);
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
/// still finishes a recording in its selected folder and storage ledger.
#[test]
#[cfg(unix)]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn serve_finishes_a_recording_when_its_terminal_signals_the_process_group() {
    use std::os::unix::process::CommandExt;
    for (signal, name) in [(libc::SIGINT, "int"), (libc::SIGHUP, "hup")] {
        let root = temp_root(&format!("group-{name}"));
        let dir = root.join("sessions");
        let recordings = root.join("recordings");
        std::fs::create_dir_all(&root).unwrap();
        let mut command = serve_command(&dir, &root, name, &[]);
        command.process_group(0);
        let mut serve = serve_ready(command.spawn().unwrap());
        json(&dir, &["--json", "--session", name, "connect", "sim:rec"]);
        json(
            &dir,
            &["--json", "--session", name, "set", "Width=64", "Height=64"],
        );
        json(
            &dir,
            &[
                "--json",
                "destination",
                "add-folder",
                "recordings",
                recordings.to_str().unwrap(),
            ],
        );
        json(
            &dir,
            &[
                "--json",
                "--session",
                name,
                "record",
                "-o",
                "clip.mp4",
                "--destination",
                "recordings",
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
        let clip = recordings.join("clip.mp4");
        assert!(std::fs::metadata(&clip).unwrap().len() > 0);
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
// Local destinations, host checks and storage.

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
/// real environment or the user’s saved state.
fn cmd_env(dir: &PathBuf, env: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let mut command = binary(dir);
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
fn doctor_reports_destination_features() {
    let dir = temp_root("doctor-destinations");
    let doctor = json(&dir, &["--json", "doctor"]);
    assert!(doctor["host"]["cpu_cores"].as_u64().unwrap() >= 1);
    assert!(doctor["efficiency"]["efficient"].is_boolean());
    assert!(matches!(
        doctor["efficiency"]["setting"].as_str(),
        Some("auto" | "smooth" | "efficient")
    ));
    // CAPTUREFAB_SESSION_DIR is both the runtime and the state directory.
    let path = Value::from(dir.to_string_lossy().into_owned());
    for key in ["runtime_directory", "state_directory"] {
        assert_eq!(doctor[key], path, "{key}");
    }
    for key in ["runtime", "state"] {
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
    for id in ["efficiency", "camera_network", "ffmpeg"] {
        assert!(checks.iter().any(|check| check["id"] == id), "{id}");
    }
    // A private session directory raises nothing.
    assert!(!checks.iter().any(|check| check["id"] == "private_dirs"));
    // camera_network informs; serve prints every warning at start.
    assert!(
        checks
            .iter()
            .any(|check| check["id"] == "camera_network" && check["level"] == "ok")
    );
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
        elapsed < std::time::Duration::from_secs(5),
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
    assert!(text.contains("add-folder"));
    assert!(schema.get("uploads").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_warns_when_ffmpeg_does_not_run_and_when_sessions_are_shared() {
    let dir = temp_root("doctor-ffmpeg");
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing-ffmpeg");
    let env = [("CAPTUREFAB_FFMPEG", missing.to_str().unwrap())];
    let doctor = json_env(&dir, &env, &["--json", "doctor"]);
    let check = |id: &str| {
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} check: {doctor}"))
    };
    let ffmpeg = check("ffmpeg");
    assert_eq!(ffmpeg["level"], "warn", "{ffmpeg}");
    assert!(
        ffmpeg["message"]
            .as_str()
            .unwrap()
            .starts_with("Webcams, RTSP/SRT, files and recording need FFmpeg"),
        "{ffmpeg}"
    );
    assert!(
        ffmpeg["fix"]
            .as_str()
            .unwrap()
            .contains("CAPTUREFAB_FFMPEG")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let doctor = json(&dir, &["--json", "doctor"]);
        let private = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "private_dirs")
            .cloned()
            .unwrap_or_else(|| panic!("{doctor}"));
        assert_eq!(private["level"], "warn");
        assert!(
            private["fix"]
                .as_str()
                .unwrap()
                .starts_with(&format!("chmod 700 {}", dir.display())),
            "{private}"
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_local_address_without_a_gige_camera_suggests_the_simulator() {
    let dir = temp_root("gige-refused");
    let out = cmd(
        &dir,
        &[
            "--json",
            "--timeout-ms",
            "500",
            "info",
            "--camera",
            "127.0.0.1",
        ],
    );
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(3), "{reply}");
    assert_eq!(reply["error"]["code"], "unavailable", "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("capturefab simulate run --preset gige"),
        "{message}"
    );
    assert!(message.contains("sim:0"), "{message}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_refused_rtsp_stream_explains_itself_in_one_line() {
    let dir = temp_root("rtsp-refused");
    let out = cmd(
        &dir,
        &[
            "--json",
            "info",
            "--camera",
            "rtsp://user:secret@127.0.0.1:1/x",
        ],
    );
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(3), "{reply}");
    assert_eq!(reply["error"]["code"], "unavailable", "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(message.contains("127.0.0.1:1"), "{message}");
    assert!(
        !message.contains('\n') && !message.contains("secret") && !message.contains("@ 0x"),
        "{message}"
    );
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
    let env = [
        ("HOME", home.to_str().unwrap()),
        ("USERPROFILE", home.to_str().unwrap()),
    ];
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

/// Linux keeps persistent state on disk independently of the runtime directory.
#[cfg(target_os = "linux")]
#[test]
fn linux_state_is_written_to_the_configured_state_directory() {
    let root = temp_root("state-dir");
    let (home, run, state) = (root.join("home"), root.join("run"), root.join("state"));
    let archive = root.join("archive");
    let command = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env_remove("CAPTUREFAB_SESSION")
            .env_remove("CAPTUREFAB_CAMERA")
            .env_remove("CAPTUREFAB_SESSION_DIR")
            .env("CAPTUREFAB_CACHE_DIR", test_cache())
            .env("CAPTUREFAB_LOGIN_RUNTIME_DIR_TEST", root.join("login"))
            .env("XDG_RUNTIME_DIR", &run)
            .env("XDG_STATE_HOME", &state)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone()
    };
    command(&[
        "--json",
        "destination",
        "add-folder",
        "archive",
        archive.to_str().unwrap(),
    ]);
    command(&["--json", "storage", "configure", "--max-space", "2GiB"]);
    let directory = state.join("capturefab");
    assert!(directory.join("destinations.json").is_file());
    assert!(directory.join("storage-policy.json").is_file());
    assert!(!run.join("capturefab/destinations.json").exists());
    assert!(!run.join("capturefab/storage-policy.json").exists());
    let listed = command(&["--json", "destination", "list"]);
    assert_eq!(listed["destinations"][0]["name"], "archive");
    let storage = command(&["--json", "storage"]);
    assert_eq!(storage["policy"]["max_bytes"], 2_u64 << 30);
    let doctor = command(&["--json", "doctor"]);
    assert_eq!(
        doctor["runtime_directory"],
        run.join("capturefab").to_str().unwrap()
    );
    assert_eq!(doctor["state_directory"], directory.to_str().unwrap());
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

#[cfg(all(feature = "gui", target_os = "linux"))]
#[test]
fn no_display_without_a_command_lists_the_cli() {
    let dir = temp_root("no-display");
    let out = cmd_env(&dir, &[("DISPLAY", ""), ("WAYLAND_DISPLAY", "")], &[]);
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Usage:") && help.contains("discover"),
        "{help}"
    );
    assert!(help.contains("capturefab serve --name bench"), "{help}");
    // No window means no session: nothing was created.
    assert!(!dir.exists());
    // Asked for JSON, it is still an error, since there is no command to run.
    let out = cmd_env(
        &dir,
        &[("DISPLAY", ""), ("WAYLAND_DISPLAY", "")],
        &["--json"],
    );
    assert_eq!(out.status.code(), Some(1));
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
    for name in ["manifest.json", "README.md", "captures.csv"] {
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
        let frame = &v["result"]["frames"][0];
        assert_eq!(frame["pixel_format_name"], "Mono8");
        assert!(frame["settings"]["ExposureTime"].is_number(), "{frame}");
        // The tags and settings travel inside the file, without a dataset.
        let reader = png::Decoder::new(std::fs::File::open(path).unwrap())
            .read_info()
            .unwrap();
        assert_eq!(reader.info().color_type, png::ColorType::Grayscale);
        let embedded = reader
            .info()
            .utf8_text
            .iter()
            .find(|chunk| chunk.keyword == "capturefab")
            .unwrap()
            .get_text()
            .unwrap();
        let embedded: Value = serde_json::from_str(&embedded).unwrap();
        assert_eq!(embedded["context"]["line"], "L1");
        assert_eq!(embedded["frame_id"], frame["id"]);
        assert_eq!(embedded["settings"], frame["settings"]);
    }
    let jpeg = out.join("tagged.jpeg");
    let output = capture(&[
        "-o",
        jpeg.to_str().unwrap(),
        "--format",
        "jpeg",
        "--context",
        "part=B-7",
    ]);
    files(&output);
    let bytes = std::fs::read(&jpeg).unwrap();
    let at = bytes.windows(2).position(|w| w == [0xFF, 0xFE]).unwrap();
    let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
    let embedded: Value = serde_json::from_slice(&bytes[at + 4..at + 2 + length]).unwrap();
    assert_eq!(embedded["context"]["part"], "B-7");
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
    tools_output_env(dir, cwd, args, input, &[])
}

fn tools_output_env(
    dir: &PathBuf,
    cwd: &PathBuf,
    args: &[&str],
    input: Option<&str>,
    env: &[(&str, &str)],
) -> std::process::Output {
    use std::io::Write;
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .envs(env.iter().copied())
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
    let child = serve_at(&dir, &server, "tools-ci");
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
    assert_eq!(catalog.len(), 29);
    for name in [
        "get_overview",
        "inspect_frame",
        "set_camera_features",
        "capture_photos",
        "create_folder_destination",
    ] {
        let tool = catalog.iter().find(|tool| tool["name"] == name).unwrap();
        assert!(tool["risk"].is_string());
        assert!(tool["needs_session"].is_boolean());
        assert!(tool["needs_vision"].is_boolean());
        assert_eq!(tool["input_schema"]["additionalProperties"], false);
    }
    for removed in [
        "list_data_sources",
        "read_data_source",
        "browse_data_source",
        "start_signal_capture",
        "stop_signal_capture",
        "wait_for_data",
    ] {
        assert!(catalog.iter().all(|tool| tool["name"] != removed));
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
fn tools_support_compact_and_targeted_discovery_without_a_session() {
    let root = temp_root("tools-discovery");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("sessions");
    let full = tools_ok(&dir, &root, &["--json", "tools", "list"]);
    let compact = tools_ok(&dir, &root, &["--json", "tools", "list", "--compact"]);
    let entries = compact.as_array().unwrap();
    assert_eq!(entries.len(), full.as_array().unwrap().len());
    assert!(compact.to_string().len() * 3 < full.to_string().len());
    for (entry, complete) in entries.iter().zip(full.as_array().unwrap()) {
        for key in [
            "name",
            "title",
            "kind",
            "risk",
            "needs_session",
            "needs_vision",
        ] {
            assert_eq!(entry[key], complete[key]);
        }
        assert!(entry.get("input_schema").is_none());
        assert!(entry.get("description").is_none());
    }
    let name = "capture_photos";
    let described = tools_ok(&dir, &root, &["--json", "tools", "describe", name]);
    assert_eq!(
        described,
        *full
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap()
    );
    let unknown = tools_output(
        &dir,
        &root,
        &["--json", "tools", "describe", "missing_tool"],
        None,
    );
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(tools_response(&unknown)["error"]["code"], "usage");
    assert!(
        !dir.exists(),
        "tool discovery must not create a session directory"
    );
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
    let stream = serde_json::json!({"camera":"sim:0","url":"rtmp://uncontacted.example/live/","duration":null,"codec":null,"fps":null,"bitrate":null});
    let plan = cwd.join("stream.json");
    std::fs::write(
        &plan,
        serde_json::json!({"version":1,"calls":[{"tool":"stream_to_url","args":stream}]})
            .to_string(),
    )
    .unwrap();
    let stream = stream.to_string();
    let missing_ffmpeg = cwd.join("missing-ffmpeg");
    let unavailable = [("CAPTUREFAB_FFMPEG", missing_ffmpeg.to_str().unwrap())];
    // Exercise inherited FFmpeg configuration and an explicitly absent encoder,
    // even on hosts/builds that normally provide FFmpeg. Never open the URL.
    for env in [&[][..], &unavailable[..]] {
        for command in [
            vec!["call", "stream_to_url", "--args", &stream],
            vec!["apply", plan.to_str().unwrap()],
        ] {
            let mut args = vec!["--json", "--session", "tools-ci", "tools"];
            args.extend(command);
            let output = tools_output_env(&dir, &cwd, &args, None, env);
            assert_eq!(
                output.status.code(),
                Some(2),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let response = tools_response(&output);
            assert_eq!(response["error"]["code"], "usage");
            assert_eq!(response["error"]["details"]["risk"], "disruptive");
            if !env.is_empty() {
                assert!(
                    response["error"]["details"]["checked"][0]["issues"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|issue| issue["code"] == "unavailable")
                );
                for flag in ["--dry-run", "--confirmed-by-user"] {
                    let mut args = args.clone();
                    args.push(flag);
                    let output = tools_output_env(&dir, &cwd, &args, None, env);
                    assert_eq!(
                        output.status.code(),
                        Some(3),
                        "{}",
                        String::from_utf8_lossy(&output.stdout)
                    );
                    let response = tools_response(&output);
                    assert_eq!(response["error"]["code"], "unavailable");
                    assert!(response["error"]["details"]["risk"].is_null());
                }
            }
        }
    }
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
    // A separate CLI process cannot prove the token belongs to its run.
    // The shared policy therefore requires approval before undoing it.
    let undo_args = serde_json::json!({"camera":"sim:0","token":token});
    let denied = tools_output(
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
            "call",
            "undo_camera_change",
            "--args",
            &undo_args.to_string(),
        ],
        None,
    );
    assert_eq!(denied.status.code(), Some(2));
    assert_eq!(
        tools_response(&denied)["error"]["details"]["risk"],
        "disruptive"
    );
    let unchanged = tools_call(&dir, &cwd, "inspect_frame", inspect.clone(), &[]);
    assert!(
        (unchanged["exposure"]["mean"].as_f64().unwrap()
            - brighter["exposure"]["mean"].as_f64().unwrap())
        .abs()
            < 0.1,
        "an unapproved undo changed the frame: before {brighter}; after {unchanged}"
    );
    tools_call(
        &dir,
        &cwd,
        "undo_camera_change",
        undo_args,
        &["--confirmed-by-user"],
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
fn tools_secret_stdin_requires_a_consumer_before_touching_session_state() {
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
    assert!(!String::from_utf8_lossy(&output.stdout).contains("unused-secret"));
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
            Self::serve(replies)
        }
        fn serve(replies: Vec<(u16, Value)>) -> Self {
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
        assert_eq!(providers.len(), ProviderId::ALL.len());
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
    fn assistant_local_provider_selection_needs_no_endpoint_or_credentials() {
        let root = Directory::new("assistant-local-provider");
        let dir = root.0.join("sessions");
        let server = ProviderServer::new(Vec::new());
        let out = output(
            &dir,
            &root.0,
            &["--json", "assistant", "use", "apple_foundation_models"],
            None,
            &[
                ("OPENAI_API_KEY", "unused-remote-key"),
                ("OLLAMA_HOST", &server.base_url),
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let result = tools_response(&out);
        assert_eq!(result["result"]["provider"], "apple_foundation_models");
        assert_eq!(result["result"]["model"], "on-device");
        let saved = std::fs::read_to_string(dir.join("assistant.json")).unwrap();
        let settings: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(settings["provider"], "apple_foundation_models");
        let local = &settings["providers"]["apple_foundation_models"];
        assert_eq!(local["saved_key"], false);
        assert!(local["base_url"].is_null());
        assert!(local["endpoint"].is_null());
        assert!(!saved.contains("unused-remote-key"));
        assert!(!saved.contains("offline-test"));
        assert!(!saved.contains(&server.base_url));
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn assistant_local_provider_rejects_remote_configuration_and_keys() {
        let root = Directory::new("assistant-local-invalid");
        let dir = root.0.join("sessions");
        for (extra, field) in [
            (vec!["--base-url", "https://remote.example/v1"], "base_url"),
            (vec!["--model", "remote-model"], "model"),
            (vec!["--vision", "on"], "vision"),
        ] {
            let mut args = vec!["--json", "assistant", "use", "apple_foundation_models"];
            args.extend(extra);
            let out = output(&dir, &root.0, &args, None, &[]);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            let error = tools_response(&out);
            assert!(
                error["error"]["details"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["field"] == field),
                "{error}"
            );
            assert!(!dir.exists(), "Rejected settings must not create state.");
        }
        for action in ["set", "remove"] {
            let mut args = vec![
                "--json",
                "assistant",
                "key",
                action,
                "apple_foundation_models",
            ];
            if action == "set" {
                args.push("--key-stdin");
            }
            let out = output(&dir, &root.0, &args, None, &[]);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            let error = tools_response(&out);
            assert_eq!(error["error"]["code"], "usage");
            assert!(
                !dir.exists(),
                "Keyless providers must not create credential state."
            );
        }
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

    #[cfg(feature = "parquet")]
    #[test]
    fn assistant_queries_project_private_metadata_before_sql_without_changing_raw_cli() {
        let root = temp_root("assistant-query-projection");
        let dir = root.join("sessions");
        let cwd = root.join("caller");
        let home = root.join("HOME_QUERY_PRIVATE_CANARY");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let home_text = home.to_str().unwrap();
        let proxy = "http://PROXY_USER_QUERY_CANARY:PROXY_PASSWORD_QUERY_CANARY@proxy.invalid/PROXY_PATH_QUERY_CANARY";
        let mut server_env = vec![
            ("HOME", home_text),
            ("USERPROFILE", home_text),
            ("HTTP_PROXY", proxy),
            ("http_proxy_username", "PROXY_USER_QUERY_CANARY"),
            ("http_proxy_password", "PROXY_PASSWORD_QUERY_CANARY"),
        ];
        // Windows environment names are case insensitive.
        if !cfg!(windows) {
            server_env.push(("http_proxy", "http://LOWER_PROXY_QUERY_CANARY.invalid"));
        }
        let _fixture = Fixture {
            child: serve_with(&dir, &cwd, "tools-ci", &server_env),
            dir: root,
        };
        let path = cwd.join("private-source.duckdb");
        let file = path.to_str().unwrap();
        // Let the public binary create its canonical schema, then release the
        // writer before seeding controlled stored metadata through DuckDB.
        tools_ok(
            &dir,
            &cwd,
            &["--json", "--session", "tools-ci", "dataset", "open", file],
        );
        tools_ok(
            &dir,
            &cwd,
            &["--json", "--session", "tools-ci", "dataset", "close"],
        );
        let serial = "SERIAL_QUERY_PRIVATE_CANARY";
        let uri = "https://URI_USER_QUERY_CANARY:URI_PASSWORD_QUERY_CANARY@camera.invalid/URI_PATH_QUERY_CANARY?token=URI_TOKEN_QUERY_CANARY";
        let private_path = home.join("capture.png");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO captures(wall_ms,camera_id,serial,source,format,bytes,path,file_name,origin) VALUES(1500,'sim:privacy',?,?,'png',1,?,'capture.png','capture')",
                duckdb::params![serial, uri, private_path.to_str().unwrap()],
            )
            .unwrap();
        }
        let dataset = tools_ok(
            &dir,
            &cwd,
            &[
                "--json",
                "--session",
                "tools-ci",
                "dataset",
                "open",
                "--existing",
                file,
            ],
        );
        let raw_sql = "SELECT serial AS annotation,reverse(serial) AS alternate,hex(source) AS origin_hint,reverse(path) AS location_hint,reverse(current_setting('http_proxy')) AS engine_hint FROM captures";
        let raw_query = || {
            tools_ok(
                &dir,
                &cwd,
                &[
                    "--json",
                    "--session",
                    "tools-ci",
                    "dataset",
                    "query",
                    raw_sql,
                ],
            )
        };
        let raw_before = raw_query();
        let hex = |text: &str| {
            text.as_bytes()
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        };
        let reversed = |text: &str| text.chars().rev().collect::<String>();
        assert_eq!(
            raw_before["rows"][0],
            json!([
                serial,
                reversed(serial),
                hex(uri),
                reversed(private_path.to_str().unwrap()),
                reversed(proxy)
            ]),
            "the fixture and trusted CLI must retain the original private data"
        );
        let stored_sql = "SELECT c.camera_id AS subject,c.bytes AS measurement,c.file_name AS annotation,c.serial AS extra,replace(c.serial,'_','-') AS alternate,hex(c.serial) AS encoded,reverse(c.source) AS origin_hint,hex(c.path) AS location_hint FROM captures c";
        let engine_sql = "SELECT reverse(current_setting('http_proxy')) AS hint_a,hex(current_setting('http_proxy_username')) AS hint_b,reverse(current_setting('http_proxy_password')) AS hint_c,hex(current_setting('secret_directory')) AS hint_d,reverse(current_setting('home_directory')) AS hint_e,hex(current_setting('extension_directory')) AS hint_f,reverse(current_setting('temp_directory')) AS hint_g,(SELECT string_agg(reverse(coalesce(value,'')) || hex(coalesce(value,'')),'|') FROM duckdb_settings()) AS hint_h";
        let server = ProviderServer::new(vec![
            call(
                "private-stored-query",
                "query_dataset",
                json!({"sql":stored_sql,"limit":1,"timeout":"5s"}),
            ),
            call(
                "private-engine-query",
                "query_dataset",
                json!({"sql":engine_sql,"limit":1,"timeout":"5s"}),
            ),
            call(
                "private-finish",
                "finish_task",
                json!({"outcome":"succeeded","summary":"Verified the approved dataset."}),
            ),
        ]);
        settings(&dir, &server);
        let out = output(
            &dir,
            &cwd,
            &[
                "--json",
                "--session",
                "tools-ci",
                "assistant",
                "run",
                "Query the approved dataset and verify its capture metadata.",
                "--dataset",
                dataset["slot_id"].as_str().unwrap(),
                "--allow-data",
                "--mode",
                "autonomous",
                "--approvals",
                "deny",
                "--max-captures",
                "0",
                "--max-images",
                "0",
                "--max-rounds",
                "4",
            ],
            None,
            &[],
        );
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let result = tools_response(&out)["result"].clone();
        assert_eq!(result["outcome"], "finished", "{result}");
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let messages = requests.last().unwrap()["messages"].as_array().unwrap();
        let reply = |id: &str| -> Value {
            let message = messages
                .iter()
                .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
                .unwrap_or_else(|| panic!("missing provider tool response {id}"));
            let reply: Value = serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
            assert!(reply.get("error").is_none(), "{id}: {reply}");
            assert_ne!(reply["is_error"], true, "{id}: {reply}");
            reply
        };
        let stored = reply("private-stored-query");
        assert_eq!(stored["row_count"], 1, "{stored}");
        assert_eq!(stored["truncated"], false);
        let row = stored["rows"][0].as_array().unwrap();
        assert_eq!(
            &row[..3],
            &[json!("sim:privacy"), json!(1), json!("capture.png")]
        );
        assert!(row[3..6].iter().all(Value::is_null), "{stored}");
        assert!(row[6].is_string() && row[7].is_string());
        let engine = reply("private-engine-query");
        assert_eq!(engine["row_count"], 1, "{engine}");
        assert_eq!(engine["truncated"], false);
        for column in [0, 1, 2, 6] {
            assert_eq!(engine["rows"][0][column], "", "{engine}");
        }
        assert!(
            engine["rows"][0][7]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        let wire = serde_json::to_string(&*requests).unwrap();
        drop(requests);
        let records = assert_end_log(&dir, result["run_id"].as_str().unwrap(), "finished");
        let exposed = format!(
            "{wire}{}{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            serde_json::to_string(&records).unwrap()
        );
        for private in [
            serial,
            "URI_USER_QUERY_CANARY",
            "URI_PASSWORD_QUERY_CANARY",
            "URI_PATH_QUERY_CANARY",
            "URI_TOKEN_QUERY_CANARY",
            "HOME_QUERY_PRIVATE_CANARY",
            "PROXY_USER_QUERY_CANARY",
            "PROXY_PASSWORD_QUERY_CANARY",
            "PROXY_PATH_QUERY_CANARY",
            "LOWER_PROXY_QUERY_CANARY",
        ] {
            for spelling in [private.to_owned(), reversed(private), hex(private)] {
                assert!(
                    !exposed.contains(&spelling),
                    "private metadata reached the provider, CLI result or action log"
                );
            }
        }
        assert_eq!(
            raw_query(),
            raw_before,
            "model projection must not change the shared source or trusted CLI query path"
        );
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
        // Drain both pipes while the child runs: its stopped-run JSON can exceed
        // the pipe buffer (16 KiB on macOS), and an unread pipe would block the
        // final write so the process could never exit.
        let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
            let mut pipe = pipe.unwrap();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                pipe.read_to_end(&mut bytes).unwrap();
                bytes
            })
        };
        let child = running.0.as_mut().unwrap();
        let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
        let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
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
        unsafe extern "C" {
            fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
        }
        let pid = std::ffi::c_int::try_from(running.0.as_ref().unwrap().id()).unwrap();
        // SAFETY: this is our live child process, and 15 is SIGTERM on supported Unix targets.
        let signaled = unsafe { kill(pid, 15) };
        assert_eq!(signaled, 0, "{}", std::io::Error::last_os_error());
        let stop_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while running.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < stop_deadline,
                "SIGTERM did not stop the assistant within 10 seconds."
            );
            thread::sleep(Duration::from_millis(10));
        }
        let out = std::process::Output {
            status: running.0.take().unwrap().wait().unwrap(),
            stdout: stdout.join().unwrap(),
            stderr: stderr.join().unwrap(),
        };
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

#[test]
fn tools_schema_and_doctor_describe_shared_registry() {
    let root = temp_root("tools-schema");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("sessions");
    let schema = tools_ok(&dir, &root, &["--json", "schema"]);
    let tools = &schema["tools"];
    let catalog = tools["catalog"].as_array().unwrap();
    assert_eq!(catalog.len(), 29);
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
        ("get_dataset", "observe", true),
        ("query_dataset", "observe", true),
        ("open_dataset", "bounded", true),
        ("close_dataset", "disruptive", true),
        ("annotate_dataset", "bounded", true),
        ("export_dataset", "bounded", true),
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
    for (name, kind) in [
        ("get_dataset", "read"),
        ("query_dataset", "read"),
        ("open_dataset", "configure"),
        ("close_dataset", "control"),
        ("annotate_dataset", "control"),
        ("export_dataset", "capture"),
    ] {
        assert_eq!(
            catalog.iter().find(|tool| tool["name"] == name).unwrap()["kind"],
            kind,
            "{name}"
        );
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
        ["list", "describe", "call", "apply"]
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
    assert!(doctor["features"].get("uploads").is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn resource_limits_are_automatic_persisted_and_reject_invalid_values_without_cameras() {
    let root = temp_root("resources");
    let initial = json(&root, &["--json", "resources"]);
    assert!(initial["policy"]["cpu_threads"].is_null());
    assert!(initial["effective"]["cpu_threads"].as_u64().unwrap() >= 1);
    assert!(initial["effective"]["buffer_bytes"].as_u64().unwrap() >= 512 * 1024 * 1024);
    let configured = json(
        &root,
        &[
            "--json",
            "resources",
            "configure",
            "--cpu-threads",
            "2",
            "--camera-buffers",
            "1GiB",
        ],
    );
    assert_eq!(configured["policy"]["cpu_threads"], 2);
    assert_eq!(configured["policy"]["buffer_bytes"], 1024 * 1024 * 1024u64);
    let invalid = cmd(
        &root,
        &["--json", "resources", "configure", "--cpu-threads", "0"],
    );
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(
        json(&root, &["--json", "resources"])["policy"],
        configured["policy"]
    );
    let reset = json(&root, &["--json", "resources", "reset"]);
    assert!(reset["policy"]["cpu_threads"].is_null());
    assert!(reset["policy"]["buffer_bytes"].is_null());
    assert!(reset["policy"].get("upload_bytes_per_second").is_none());
    let _ = std::fs::remove_dir_all(root);
}

fn failure(out: &std::process::Output) -> Value {
    serde_json::from_slice::<Value>(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })["error"]
        .clone()
}

#[test]
fn discover_prints_aligned_plain_rows_and_a_next_step() {
    let root = temp_root("discover-table");
    let out = cmd(
        &root,
        &["--direct", "--simulate", "--timeout-ms", "300", "discover"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.starts_with("ID "), "{stdout}");
    let row = stdout
        .lines()
        .find(|line| line.starts_with("sim:0 "))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(
        row.contains("Simulator") && row.contains("Pattern camera"),
        "{row}"
    );
    assert!(!stdout.contains("Next:"), "hints stay off stdout: {stdout}");
    assert!(stderr.contains("Next: capturefab --camera "), "{stderr}");
    let result = json(
        &root,
        &[
            "--json",
            "--direct",
            "--simulate",
            "--timeout-ms",
            "300",
            "discover",
        ],
    );
    let sim = result["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["id"] == "sim:0")
        .cloned()
        .unwrap();
    assert_eq!(sim["transport"], "simulator");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn camera_selectors_explain_what_did_not_match() {
    let root = temp_root("selector-hints");
    let out = cmd(
        &root,
        &[
            "--json",
            "--direct",
            "--timeout-ms",
            "300",
            "--camera",
            "sim0",
            "info",
        ],
    );
    assert_eq!(out.status.code(), Some(3));
    let error = failure(&out);
    assert_eq!(error["code"], "unavailable");
    assert!(
        error["message"].as_str().unwrap().contains("sim:0"),
        "{error}"
    );
    let out = cmd(
        &root,
        &["--json", "--direct", "--camera", "./missing.mp4", "info"],
    );
    assert_eq!(out.status.code(), Some(3));
    let error = failure(&out);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("file not found"),
        "{error}"
    );
    let out = cmd(
        &root,
        &["--json", "--direct", "--camera", "192.168.1.300", "info"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        failure(&out)["message"]
            .as_str()
            .unwrap()
            .contains("not a valid IPv4 address")
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn connect_takes_a_camera_argument_or_flag_and_reads_like_prose() {
    let base = temp_root("connect-forms");
    let root = base.join("sessions");
    let cwd = base.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let _fixture = Fixture {
        child: serve_at(&root, &cwd, "conn"),
        dir: base.clone(),
    };
    let flag = json(
        &root,
        &[
            "--json",
            "--session",
            "conn",
            "connect",
            "--camera",
            "sim:0",
        ],
    );
    assert_eq!(flag["connected"]["id"], "sim:0");
    let global = json(
        &root,
        &[
            "--json",
            "--session",
            "conn",
            "--camera",
            "sim:1",
            "connect",
        ],
    );
    assert_eq!(global["connected"]["id"], "sim:1");
    let env = json_env(
        &root,
        &[("CAPTUREFAB_CAMERA", "sim:2")],
        &["--json", "--session", "conn", "connect"],
    );
    assert_eq!(env["connected"]["id"], "sim:2");
    let missing = cmd(&root, &["--json", "--session", "conn", "connect"]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(failure(&missing)["code"], "usage");
    let both = cmd(
        &root,
        &[
            "--json",
            "--session",
            "conn",
            "connect",
            "sim:0",
            "--camera",
            "sim:1",
        ],
    );
    assert_eq!(both.status.code(), Some(2));
    let human = cmd(&root, &["--session", "conn", "connect", "sim:3"]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(!text.starts_with('{'), "{text}");
    assert!(
        text.contains("Connected ") && text.contains("(sim:3"),
        "{text}"
    );
    assert!(
        text.contains(
            "Next: capturefab --session conn --camera sim:3 capture -o 'shot-{seq}.png'   (or start live acquisition: capturefab --session conn --camera sim:3 start)"
        ),
        "{text}"
    );
    let selected = json(
        &root,
        &["--json", "--session", "conn", "select", "--camera", "sim:0"],
    );
    assert_eq!(selected["active_camera"], "sim:0");
    let human = cmd(&root, &["--session", "conn", "select", "sim:1"]);
    assert_eq!(
        String::from_utf8_lossy(&human.stdout),
        "CLI calls without --camera now use sim:1.\n"
    );
    // A session name typo points at the running session.
    let typo = cmd(&root, &["--json", "--session", "conx", "status"]);
    assert_eq!(typo.status.code(), Some(3));
    let error = failure(&typo);
    assert_eq!(error["code"], "unavailable");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("did you mean --session conn?"),
        "{message}"
    );
    assert!(!message.contains("os error"), "{message}");
}

#[test]
fn json_usage_errors_carry_structured_details() {
    let root = temp_root("usage-details");
    let out = cmd(&root, &["--json", "captur"]);
    assert_eq!(out.status.code(), Some(2));
    let error = failure(&out);
    assert_eq!(error["code"], "usage");
    assert_eq!(error["message"], "unrecognized subcommand 'captur'");
    assert_eq!(error["details"]["kind"], "invalid_subcommand");
    assert_eq!(error["details"]["suggestions"][0], "capture");
    let out = cmd(&root, &["--json", "capture", "--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let error = failure(&out);
    assert_eq!(error["details"]["invalid"], "--bogus");
    assert!(
        error["details"]["usage"]
            .as_str()
            .unwrap()
            .starts_with("capturefab capture"),
        "{error}"
    );
    assert!(
        !error["message"].as_str().unwrap().contains('\n'),
        "{error}"
    );
    let out = cmd(
        &root,
        &[
            "--json",
            "capture",
            "-c",
            "sim:0",
            "-o",
            "x.png",
            "--on-exists",
            "nope",
        ],
    );
    let error = failure(&out);
    assert_eq!(error["details"]["invalid"], "nope");
    assert!(
        !error["details"]["valid_values"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let human = cmd(&root, &["captur"]);
    assert_eq!(human.status.code(), Some(2));
    assert!(human.stdout.is_empty());
    assert!(String::from_utf8_lossy(&human.stderr).contains("tip:"));
}

#[test]
fn subcommand_help_shows_examples_and_describes_every_option() {
    let root = temp_root("help-examples");
    for command in [
        "capture", "record", "discover", "set", "serve", "workflow", "connect",
    ] {
        let out = cmd(&root, &[command, "--help"]);
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Examples:"),
            "{command}"
        );
    }
    let schema = json(&root, &["--json", "schema"]);
    let command = |name: &str| {
        schema["cli"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == name)
            .cloned()
            .unwrap()
    };
    for (name, args) in [
        ("capture", &["output", "count", "format"][..]),
        ("forward", &["output", "codec", "fps", "bitrate"][..]),
        ("connect", &["target"][..]),
    ] {
        let command = command(name);
        for arg in command["arguments"].as_array().unwrap() {
            if args.contains(&arg["name"].as_str().unwrap()) {
                assert!(arg["description"].is_string(), "{name} {arg}");
            }
        }
    }
}

#[test]
fn network_scan_prints_a_readable_summary() {
    let root = temp_root("network-human");
    let out = cmd(&root, &["--direct", "--timeout-ms", "300", "network-scan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("Adapters\n"), "{text}");
    assert!(text.contains("GigE cameras"), "{text}");
    let _ = std::fs::remove_dir_all(root);
}

fn text(output: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Capture names mean what they look like, and human output summarizes.
#[test]
fn capture_output_paths_and_summaries() {
    let root = temp_root("capture-guardrails");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    let capture = |args: &[&str]| {
        let mut all = vec!["--camera", "sim:0", "capture"];
        all.extend(args);
        cmd(&dir, &all)
    };
    let json_capture = |args: &[&str]| {
        let mut all = vec!["--json", "--camera", "sim:0", "capture"];
        all.extend(args);
        let out = cmd(&dir, &all);
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
        (out.status.code(), reply)
    };
    // A human summary, not a path dump; JSON keeps its fields and adds two.
    let out = capture(&["-o", &format!("{}/", path("burst")), "-n", "3"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("Saved 3 PNG frames · "), "{stdout}");
    assert!(
        stdout.contains('×') && stdout.contains("frame-000003.png"),
        "{stdout}"
    );
    let (code, reply) = json_capture(&["-o", &path("one.png")]);
    assert_eq!(code, Some(0), "{reply}");
    let frame = &reply["result"]["frames"][0];
    assert!(frame["pixel_format"].is_u64(), "{reply}");
    assert!(frame["pixel_format_name"].is_string(), "{reply}");
    assert_eq!(
        frame["file_bytes"].as_u64().unwrap(),
        std::fs::metadata(path("one.png")).unwrap().len()
    );
    // A taken name fails, naming the one-flag fix.
    let out = capture(&["-o", &path("one.png")]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("output already exists"), "{stderr}");
    assert!(
        stderr.contains("--on-exists number") && stderr.contains("one-{seq}.png"),
        "{stderr}"
    );
    let (code, reply) = json_capture(&["-o", &path("one.png")]);
    assert_eq!(code, Some(1));
    assert_eq!(reply["error"]["code"], "operation_failed");
    assert!(
        reply["error"]["details"]["hint"]
            .as_str()
            .unwrap()
            .contains("{seq}"),
        "{reply}"
    );
    // Several frames to one image name is a usage error with a suggestion.
    let (code, reply) = json_capture(&["-o", &path("shot.jpg"), "-n", "3"]);
    assert_eq!(code, Some(2), "{reply}");
    assert_eq!(reply["error"]["code"], "usage");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("shot-{seq}.jpg"),
        "{reply}"
    );
    assert!(!root.join("shot.jpg").exists());
    let (code, reply) = json_capture(&["-o", &path("shot-{seq}.jpg"), "-n", "3"]);
    assert_eq!(code, Some(0), "{reply}");
    for n in 1..=3 {
        assert!(root.join(format!("shot-{n:04}.jpg")).is_file());
    }
    // A trailing separator is a folder; a missing extension is added; a
    // contradicting format is refused before anything is written.
    let (code, reply) = json_capture(&["-o", &format!("{}/", path("newdir"))]);
    assert_eq!(code, Some(0), "{reply}");
    assert!(root.join("newdir/frame-000001.png").is_file());
    let (code, reply) = json_capture(&["-o", &path("y.jpg"), "-f", "png"]);
    assert_eq!(
        (code, &reply["error"]["code"]),
        (Some(2), &Value::from("usage")),
        "{reply}"
    );
    assert!(!root.join("y.jpg").exists());
    let (code, reply) = json_capture(&["-o", &path("z/{seq}"), "-n", "2"]);
    assert_eq!(code, Some(0), "{reply}");
    assert!(root.join("z/0001.png").is_file() && root.join("z/0002.png").is_file());
    let (code, reply) = json_capture(&["-o", &path("noext")]);
    assert_eq!(code, Some(0), "{reply}");
    assert!(root.join("noext.png").is_file());
    // A local time lapse reports each frame on stderr and ends in one line.
    let out = capture(&[
        "-o",
        &format!("{}/", path("tl")),
        "-n",
        "2",
        "--interval",
        "200ms",
    ]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stderr.contains("1/2 saved"), "{stderr}");
    assert!(stdout.contains("Captured 2 of 2"), "{stdout}");
    assert!(
        !stdout.contains("first_at_ms") && !stdout.contains('{'),
        "{stdout}"
    );
    // Storage and info read as prose; JSON keeps every field.
    let out = cmd(&dir, &["storage"]);
    let (stdout, _) = text(&out);
    assert!(
        stdout.starts_with("Using ") && stdout.contains("of 10 GiB"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("namespace_accounting") && !stdout.contains('{'),
        "{stdout}"
    );
    assert!(json(&dir, &["--json", "storage"])["namespace_accounting"].is_string());
    let out = cmd(&dir, &["--camera", "sim:0", "info"]);
    let (stdout, _) = text(&out);
    assert!(stdout.starts_with("sim:0 · "), "{stdout}");
    assert!(!stdout.contains('{'), "{stdout}");
    // A taken recording file fails before any encoder starts or banner shows.
    std::fs::write(root.join("existing.mp4"), b"keep").unwrap();
    let out = cmd(
        &dir,
        &[
            "--camera",
            "sim:0",
            "record",
            "-o",
            &path("existing.mp4"),
            "--duration",
            "1s",
        ],
    );
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        !stderr.contains("Forwarding to") && !stderr.contains("Recording "),
        "{stderr}"
    );
    assert!(stderr.contains("existing-{seq}.mp4"), "{stderr}");
    assert_eq!(std::fs::read(root.join("existing.mp4")).unwrap(), b"keep");
    let _ = std::fs::remove_dir_all(&root);
}

/// A burst cut short by storage reports what it saved and keeps exit 6;
/// storage advice fits the command and reads in binary units.
#[test]
fn partial_bursts_and_storage_errors_say_what_happened() {
    let root = temp_root("partial-burst");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let folder = root.join("f");
    let out = cmd(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            folder.to_str().unwrap(),
            "-n",
            "3",
            "--max-files",
            "2",
        ],
    );
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(6), "{reply}");
    assert_eq!(reply["error"]["code"], "storage_full");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("saved 2 of 3 frames"),
        "{reply}"
    );
    assert!(folder.join("frame-000001.png").is_file() && folder.join("frame-000002.png").is_file());
    let fresh = root.join("fresh");
    let out = cmd(
        &fresh,
        &[
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            root.join("h.png").to_str().unwrap(),
            "--max-space",
            "1KiB",
        ],
    );
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(6), "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("--max-file-size") && message.contains("KiB"),
        "{message}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A session's scheduled capture, job list and cancel read as prose.
#[test]
fn session_jobs_read_as_prose() {
    let root = temp_root("session-jobs");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &root, "jobs"),
        dir: root.clone(),
    };
    json(&dir, &["--json", "--session", "jobs", "connect", "sim:0"]);
    let out = cmd(
        &dir,
        &[
            "--session",
            "jobs",
            "capture",
            "-n",
            "3",
            "--interval",
            "5s",
            "-o",
            root.join("tl-{seq}.png").to_str().unwrap(),
        ],
    );
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("Scheduled job "), "{stdout}");
    assert!(stdout.contains("--session jobs jobs"), "{stdout}");
    let out = cmd(&dir, &["--session", "jobs", "jobs"]);
    let (stdout, _) = text(&out);
    assert!(
        stdout.starts_with("ID ") && !stdout.contains("{\""),
        "{stdout}"
    );
    assert!(stdout.contains("\n1     pending"), "{stdout}");
    let id = json(&dir, &["--json", "--session", "jobs", "jobs"])["jobs"][0]["id"].to_string();
    let out = cmd(&dir, &["--session", "jobs", "cancel", &id]);
    let (stdout, stderr) = text(&out);
    assert!(
        stdout.contains(&format!("Cancelled capture job {id}.")),
        "{stdout}{stderr}"
    );
}

/// A timed recording that the session ends on time reports its final stats.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_timed_recording_reports_its_final_stats() {
    let root = temp_root("timed-stats");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let clip = root.join("clip.mkv");
    let result = json(
        &dir,
        &[
            "--json",
            "--camera",
            "sim:0",
            "record",
            "-o",
            clip.to_str().unwrap(),
            "--duration",
            "2s",
        ],
    );
    assert_eq!(result["stopped"]["by"], "duration", "{result}");
    assert!(
        result["stopped"]["stats"]["written"].as_u64().unwrap() > 0,
        "{result}"
    );
    assert!(
        result["stopped"]["stats"]["recording"]["bytes"]
            .as_u64()
            .unwrap()
            > 0,
        "{result}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `{tag:KEY}` sorts captures into tag folders, a missing tag is a usage
/// error, and `dataset export --media images --group-by` copies the
/// cataloged images into a self-contained folder.
#[test]
fn tag_folders_and_dataset_export_with_images() {
    let root = naming_root("tag-export");
    let dir = root.join("sessions");
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let dataset = root.join("parts.duckdb");
    let template = out.join("{tag:label}/{seq}.png");
    let template = template.to_str().unwrap();
    let capture = |label: Option<&str>| {
        let mut args = vec![
            "--json",
            "--camera",
            "sim:0",
            "capture",
            "-o",
            template,
            "--dataset",
            dataset.to_str().unwrap(),
        ];
        let context;
        if let Some(label) = label {
            context = format!("label={label}");
            args.extend(["--context", context.as_str()]);
        }
        cmd(&dir, &args)
    };
    for (label, folder) in [
        ("good", "good"),
        ("defect", "defect"),
        ("good", "good"),
        ("../x", "_.._x"),
    ] {
        let output = capture(Some(label));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        let file = std::path::PathBuf::from(v["result"]["files"][0].as_str().unwrap());
        assert_eq!(
            file.parent().unwrap().file_name().unwrap(),
            folder,
            "{file:?}"
        );
        assert!(file.is_file());
    }
    assert!(out.join("good/0002.png").is_file());
    let missing = capture(None);
    assert_eq!(missing.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(v["error"]["code"], "usage");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("{tag:label}"),
        "{v}"
    );
    // --group-by needs --media.
    let export = root.join("export");
    let refused = cmd(
        &dir,
        &[
            "--json",
            "dataset",
            "export",
            "--file",
            dataset.to_str().unwrap(),
            "-o",
            export.to_str().unwrap(),
            "--group-by",
            "label",
        ],
    );
    assert_eq!(refused.status.code(), Some(2));
    assert!(!export.exists());
    let result = json(
        &dir,
        &[
            "--json",
            "dataset",
            "export",
            "--file",
            dataset.to_str().unwrap(),
            "-o",
            export.to_str().unwrap(),
            "--format",
            "csv",
            "--media",
            "images",
            "--group-by",
            "label",
        ],
    );
    let media = &result["media"];
    assert_eq!(media["mode"], "images");
    assert_eq!(media["group_by"], "label");
    assert_eq!(media["copied"], 4);
    assert_eq!(media["skipped"], 0);
    assert!(media["bytes"].as_u64().unwrap() > 0);
    for copy in [
        "images/good/0001.png",
        "images/good/0002.png",
        "images/defect/0001.png",
        "images/_.._x/0001.png",
        "media.csv",
    ] {
        assert!(export.join(copy).is_file(), "{copy}");
    }
    assert!(
        result["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "media.csv" && f["rows"] == 4)
    );
    let help = cmd(&dir, &["dataset", "export", "--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("New folder to create; must not exist"),
        "{help}"
    );
    assert!(help.contains("--media"), "{help}");
    let _ = std::fs::remove_dir_all(root);
}

fn login_cmd(dir: &PathBuf, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env_remove("CAPTUREFAB_CAMERA_PASSWORD")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .envs(env.iter().copied())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn temp_session_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

/// Camera sign-in management never touches a keychain in these cases: every
/// one fails before saving, and listing reads only the index.
#[test]
fn login_commands_validate_before_touching_the_credential_store() {
    let dir = temp_session_dir("login");
    let out = login_cmd(&dir, &["--json", "login", "list"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["result"]["logins"], serde_json::json!([]));
    assert!(value["result"]["supported"].is_boolean());
    for (args, env) in [
        (
            vec![
                "login",
                "add",
                "rtsp://u:p@h/x",
                "--user",
                "u",
                "--password-stdin",
            ],
            vec![],
        ),
        (
            vec!["login", "add", "rtsp://h/x", "--password-stdin"],
            vec![],
        ),
        (vec!["login", "add", "rtsp://h/x", "--user", "u"], vec![]),
        (
            vec![
                "login",
                "add",
                "rtsp://h/x",
                "--user",
                "u",
                "--password-stdin",
            ],
            vec![("CAPTUREFAB_CAMERA_PASSWORD", "x")],
        ),
        (
            vec![
                "login",
                "add",
                "http://h/x",
                "--user",
                "u",
                "--password-stdin",
            ],
            vec![],
        ),
    ] {
        let mut full = vec!["--json"];
        full.extend(&args);
        let out = login_cmd(&dir, &full, &env);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["error"]["code"], "usage", "{args:?}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("u:p@"));
    }
    let out = login_cmd(&dir, &["--json", "login", "remove", "rtsp://h/x"], &[]);
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    let expected = if value["error"]["code"] == "unsupported" {
        5
    } else {
        3
    };
    assert_eq!(out.status.code(), Some(expected), "{value}");
    let schema = login_cmd(&dir, &["--json", "schema"], &[]);
    let text = String::from_utf8_lossy(&schema.stdout);
    for word in ["\"login\"", "auth_required", "auth_rejected"] {
        assert!(text.contains(word), "schema lacks {word}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// An ONVIF service that answers every request with a Digest challenge.
fn refusing_onvif() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while let Ok(n) = stream.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head) = text.find("\r\n\r\n") {
                        let length = text[..head]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if request.len() >= head + 4 + length {
                            break;
                        }
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"Camera\", nonce=\"n\", qop=\"auth\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            });
        }
    });
    address
}

#[test]
fn cameras_that_ask_for_a_sign_in_report_typed_errors() {
    let dir = temp_session_dir("auth");
    let address = refusing_onvif();
    let camera = format!("onvif:http://{address}/onvif/device_service");
    let out = login_cmd(&dir, &["--json", "--camera", &camera, "info"], &[]);
    assert_eq!(out.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "auth_required", "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("capturefab login add") || message.contains("include them in the address"),
        "{message}"
    );
    let camera = format!("onvif://admin:secret-pw@{address}");
    let out = login_cmd(&dir, &["--json", "--camera", &camera, "info"], &[]);
    assert_eq!(out.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "auth_rejected", "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("refused")
    );
    for stream in [&out.stdout, &out.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("secret-pw"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "saves to and removes from the real OS credential store"]
fn login_add_saves_in_the_real_credential_store() {
    let dir = temp_session_dir("login-real");
    let out = login_cmd(
        &dir,
        &[
            "--json",
            "login",
            "add",
            "rtsp://capturefab-test.invalid/live",
            "--user",
            "test",
            "--no-check",
        ],
        &[("CAPTUREFAB_CAMERA_PASSWORD", "capturefab-test-password")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = login_cmd(
        &dir,
        &[
            "--json",
            "login",
            "remove",
            "rtsp://capturefab-test.invalid/live",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Save a simulator's settings, change them, preview and restore the setup
/// in a session, undo it, and move it between files and workflows.
#[test]
fn saved_setups_restore_preview_and_undo_in_a_session() {
    let root = temp_root("setups");
    let dir = root.join("sessions");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &cwd, "setup-ci"),
        dir: root.clone(),
    };
    let fail = |args: &[&str]| {
        let out = cmd(&dir, args);
        assert!(!out.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        (out.status.code().unwrap(), error["error"].clone())
    };
    // Direct mode saves without a session.
    let saved = json(
        &dir,
        &[
            "--json", "--direct", "--camera", "sim:0", "setup", "save", "bench",
        ],
    );
    assert_eq!(saved["setup"]["name"], "bench");
    assert!(saved["setup"]["values"].as_u64().unwrap() >= 4);
    let list = json(&dir, &["--json", "setup", "list"]);
    assert_eq!(list["setups"][0]["name"], "bench");
    assert_eq!(list["setups"][0]["model"], "Pattern camera");
    assert!(list["dir"].as_str().unwrap().ends_with("setups"));
    let (code, error) = fail(&[
        "--json", "--direct", "--camera", "sim:0", "setup", "save", "BENCH",
    ]);
    assert_eq!((code, error["code"].as_str()), (2, Some("usage")));
    let (code, error) = fail(&["--json", "setup", "save", "CON"]);
    assert_eq!((code, error["code"].as_str()), (2, Some("usage")));
    // A non-ASCII name is a usage error, never a crash.
    for name in ["1€", "é", "€€"] {
        let (code, error) = fail(&["--json", "setup", "show", name]);
        assert_eq!((code, error["code"].as_str()), (2, Some("usage")), "{name}");
    }
    // The restore hint names the camera the save used, so it runs as is.
    let human = cmd(
        &dir,
        &["--direct", "--camera", "sim:0", "setup", "save", "hint"],
    );
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("Restore with: capturefab --camera sim:0 setup apply hint"),
        "{text}"
    );
    let shown = String::from_utf8_lossy(&cmd(&dir, &["setup", "show", "hint"]).stdout).into_owned();
    assert!(!shown.contains(".0\n"), "{shown}");
    json(&dir, &["--json", "setup", "delete", "hint"]);
    let (code, error) = fail(&["--json", "--session", "setup-ci", "setup", "apply", "nope"]);
    assert_eq!((code, error["code"].as_str()), (3, Some("unavailable")));

    let session = |args: &[&str]| {
        json(
            &dir,
            &[
                &["--json", "--session", "setup-ci", "--camera", "sim:0"],
                args,
            ]
            .concat(),
        )
    };
    session(&["connect", "sim:0"]);
    session(&["set", "ExposureTime=5000"]);
    let diff = session(&["setup", "diff", "bench"]);
    assert_eq!(diff["changes"].as_array().unwrap().len(), 1, "{diff}");
    assert_eq!(diff["changes"][0]["feature"], "ExposureTime");
    assert_eq!(diff["match"], "same_camera");
    let dry = session(&["setup", "apply", "bench", "--dry-run"]);
    assert_eq!(dry["dry_run"], true);
    let exposure = |session: &dyn Fn(&[&str]) -> Value| {
        session(&["get", "ExposureTime"])["features"][0]["value"]
            .as_f64()
            .unwrap()
    };
    assert_eq!(exposure(&session), 5000.0);
    let applied = session(&["setup", "apply", "bench"]);
    assert_eq!(applied["applied"], 1, "{applied}");
    assert_eq!(applied["camera_change"]["label"], "Restore setup bench");
    assert_eq!(exposure(&session), 10000.0);
    // Nothing differs: the human output says so plainly.
    let human = |args: &[&str]| {
        let out = cmd(
            &dir,
            &[&["--session", "setup-ci", "--camera", "sim:0"], args].concat(),
        );
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let text = human(&["setup", "diff", "bench"]);
    assert!(
        text.starts_with("Nothing to change: all ")
            && text.contains("settings in bench already match sim:0."),
        "{text}"
    );
    let text = human(&["setup", "apply", "bench"]);
    assert!(
        text.starts_with("sim:0 already matches bench; nothing was changed."),
        "{text}"
    );
    session(&["undo"]);
    assert_eq!(exposure(&session), 5000.0);
    let text = human(&["setup", "diff", "bench"]);
    assert!(text.contains("ExposureTime: 5000 → 10000"), "{text}");
    let matched = session(&["setup", "diff", "bench"]);
    assert_eq!(
        matched["matched"].as_array().unwrap().len() as u64,
        matched["unchanged"].as_u64().unwrap()
    );

    // Another model is previewed with a warning; applying it needs
    // --allow-other-model; the same model warns.
    let out = cmd(
        &dir,
        &[
            "--json",
            "--session",
            "setup-ci",
            "--camera",
            "sim:line:0",
            "setup",
            "diff",
            "bench",
        ],
    );
    assert!(out.status.success());
    let preview: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(preview["result"]["match"], "other_model");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--allow-other-model"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (code, error) = fail(&[
        "--json",
        "--session",
        "setup-ci",
        "--camera",
        "sim:line:0",
        "setup",
        "apply",
        "bench",
    ]);
    assert_eq!(code, 2);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("--allow-other-model"),
        "{error}"
    );
    let other = json(
        &dir,
        &[
            "--json",
            "--session",
            "setup-ci",
            "--camera",
            "sim:line:0",
            "setup",
            "apply",
            "bench",
            "--allow-other-model",
        ],
    );
    assert_eq!(other["match"], "other_model");
    let same_model = json(
        &dir,
        &[
            "--json",
            "--session",
            "setup-ci",
            "--camera",
            "sim:1",
            "setup",
            "apply",
            "bench",
        ],
    );
    assert_eq!(same_model["match"], "same_model");
    assert!(same_model["warnings"][0].as_str().unwrap().contains("SIM0"));

    // `setup show NAME --json > FILE` round-trips as is: import and
    // apply --file read the envelope it prints.
    let shown = cmd(&dir, &["--json", "setup", "show", "bench"]);
    assert!(shown.status.success());
    let backup = cwd.join("backup.json");
    std::fs::write(&backup, &shown.stdout).unwrap();
    let backup = backup.to_str().unwrap();
    let restored = json(
        &dir,
        &["--json", "setup", "import", backup, "--name", "backup"],
    );
    assert_eq!(restored["setup"]["name"], "backup");
    session(&["set", "ExposureTime=7000"]);
    let applied = session(&["setup", "apply", "--file", backup]);
    assert_eq!(applied["applied"], 1, "{applied}");
    assert_eq!(exposure(&session), 10000.0);
    json(&dir, &["--json", "setup", "delete", "backup"]);
    // The bare document works too.
    let document: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let file = cwd.join("bench.json");
    std::fs::write(&file, serde_json::to_vec(&document["result"]).unwrap()).unwrap();
    let file = file.to_str().unwrap();
    let imported = json(&dir, &["--json", "setup", "import", file, "--name", "copy"]);
    assert_eq!(imported["setup"]["name"], "copy");
    let (code, _) = fail(&["--json", "setup", "import", file, "--name", "copy"]);
    assert_eq!(code, 2);
    session(&["set", "ExposureTime=6000"]);
    let workflow = cwd.join("restore.json");
    std::fs::write(
        &workflow,
        r#"{"version":1,"steps":[{"camera":"sim:0","command":{"op":"apply_setup","name":"copy"}}]}"#,
    )
    .unwrap();
    json(
        &dir,
        &[
            "--json",
            "--session",
            "setup-ci",
            "workflow",
            "apply",
            workflow.to_str().unwrap(),
        ],
    );
    assert_eq!(exposure(&session), 10000.0);
    let removed = json(&dir, &["--json", "setup", "delete", "bench"]);
    assert_eq!(removed["removed"], "bench");
    let list = json(&dir, &["--json", "setup", "list"]);
    assert_eq!(list["setups"].as_array().unwrap().len(), 1);
    let schema = json(&dir, &["--json", "schema"]);
    assert!(schema["setup"]["cli"].is_string());
}
