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
/// Spawned binaries keep their cache, such as the extracted FFmpeg, out of the
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
fn automatic_session_targeting_and_caller_relative_paths() {
    let root = std::env::temp_dir().join(format!(
        "capturefab-session-paths-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let dir = root.join("sessions");
    let caller = root.join("caller");
    let server = root.join("server");
    for path in [&caller, &server] {
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

    // A relative output given with an explicit session also lands beside
    // the caller, never in the directory the session started in.
    let explicit = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .current_dir(&caller)
        .args([
            "--json",
            "--session",
            "visual",
            "--camera",
            "sim:visible",
            "capture",
            "-o",
            "frames/second.png",
        ])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stdout)
    );
    assert!(caller.join("frames/second.png").is_file());
    assert!(!server.join("frames/second.png").exists());
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
            .any(|command| command["name"] == "serve")
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
        // An empty local path never becomes the current directory.
        vec!["--camera", "sim:replay:", "capture", "-o", "unused.png"],
        vec!["--camera", "file:", "capture", "-o", "unused.png"],
        vec!["--camera", "FiLe:", "capture", "-o", "unused.png"],
        vec!["connect", "sim:replay:"],
        vec!["connect", "file:"],
        vec!["select", "file:"],
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

/// How many times a thread has gone to sleep and been woken.
#[cfg(target_os = "linux")]
fn voluntary_switches(task: &std::path::Path) -> u64 {
    std::fs::read_to_string(task.join("status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
        .expect("Linux reports context switches per thread")
        .trim()
        .parse()
        .unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn an_idle_serve_and_its_listener_sleep_until_there_is_work() {
    use std::time::{Duration, Instant};
    let root = temp_root("idle");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture {
        child: serve_with(&dir, &root, "idle", &[("CAPTUREFAB_CAMERA", "sim:0")]),
        dir: root,
    };
    let pid = fixture.child.id();
    let tasks = std::path::Path::new("/proc")
        .join(pid.to_string())
        .join("task");
    let deadline = Instant::now() + Duration::from_secs(5);
    // A thread takes its name once it first runs, which can be after serve
    // reports that it is ready.
    let listener = loop {
        let listener = std::fs::read_dir(&tasks)
            .unwrap()
            .map(|task| task.unwrap().path())
            .find(|task| {
                std::fs::read_to_string(task.join("comm"))
                    .is_ok_and(|name| name.trim() == "capturefab-rpc")
            });
        if let Some(listener) = listener {
            break listener;
        }
        assert!(
            Instant::now() < deadline,
            "serve runs a session listener thread"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    // The main thread waits for a signal and the listener for a connection;
    // neither wakes to check for them.
    let threads = [tasks.join(pid.to_string()), listener];
    std::thread::sleep(Duration::from_millis(300));
    let before = threads.each_ref().map(|task| voluntary_switches(task));
    std::thread::sleep(Duration::from_secs(1));
    for (task, before) in threads.iter().zip(before) {
        let woken = voluntary_switches(task) - before;
        assert!(
            woken <= 2,
            "{} woke {woken} times in a second",
            task.display()
        );
    }
    drop(fixture);
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
    assert!(doctor.get("graphics").is_none(), "{doctor}");
    // Timed after the first launch, which can include the OS scanning a
    // freshly linked binary.
    let started = std::time::Instant::now();
    let out = cmd(&dir, &["--json", "doctor"]);
    let elapsed = started.elapsed();
    assert!(out.status.success());
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "doctor took {elapsed:?}"
    );
    // Default builds persist saved sign-ins in the OS credential store.
    assert_eq!(
        doctor["features"]["keychain"].as_bool(),
        Some(cfg!(feature = "keychain"))
    );
    for removed in ["gui", "wgpu", "ai", "parquet", "embedded_web"] {
        assert!(doctor["features"].get(removed).is_none(), "{removed}");
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
    assert!(message.contains("127.0.0.1"), "{message}");
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

#[test]
fn a_bare_invocation_prints_help_without_creating_a_session() {
    let dir = temp_root("bare-help");
    let out = cmd(&dir, &[]);
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Usage:") && help.contains("discover"),
        "{help}"
    );
    assert!(help.contains("capturefab serve --name bench"), "{help}");
    // Printing help starts no session, which would create the directory.
    assert!(!dir.exists());
    // With --json there is no command to answer, so it is a usage error.
    let out = cmd(&dir, &["--json"]);
    assert_eq!(out.status.code(), Some(2));
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply["error"]["code"], "usage", "{reply}");
    assert!(!dir.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Needs FFmpeg: a direct recording stopped by --duration or Ctrl-C keeps a
/// finished file.
#[cfg(unix)]
#[test]
#[ignore]
fn direct_recordings_finish_on_duration_and_ctrl_c() {
    let root = temp_root("direct-record");
    let dir = root.join("sessions");
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
    for name in ["a.mp4", "b.mp4"] {
        let bytes = std::fs::metadata(root.join(name)).unwrap().len();
        assert!(bytes > 0, "{name}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Needs FFmpeg: a templated recording picks a free name in the worker and
/// reports it as `path`.
#[test]
#[ignore]
fn record_template_expands() {
    let root = temp_root("record-template");
    let dir = root.join("sessions");
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
    for command in ["capture", "record", "discover", "set", "serve", "connect"] {
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
        !stdout.contains("externally_changed_files") && !stdout.contains('{'),
        "{stdout}"
    );
    assert!(json(&dir, &["--json", "storage"])["externally_changed_files"].is_u64());
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

/// `{tag:KEY}` sorts captures into tag folders, and a missing tag is a
/// usage error.
#[test]
fn tag_folders_sort_captures_by_their_tags() {
    let root = naming_root("tag-export");
    let dir = root.join("sessions");
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let template = out.join("{tag:label}/{seq}.png");
    let template = template.to_str().unwrap();
    let capture = |label: Option<&str>| {
        let mut args = vec!["--json", "--camera", "sim:0", "capture", "-o", template];
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

/// The printed restore and undo hints name the camera they were for, so
/// running them as printed in a session with several cameras changes that
/// camera and not the session's default.
#[test]
fn setup_hints_name_the_camera_in_a_session_with_several() {
    let root = temp_root("setup-hints");
    let dir = root.join("sessions");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &cwd, "setup-two"),
        dir: root.clone(),
    };
    let on = |camera: &str, args: &[&str]| {
        json(
            &dir,
            &[
                &["--json", "--session", "setup-two", "--camera", camera],
                args,
            ]
            .concat(),
        )
    };
    let gain = |camera: &str| {
        on(camera, &["get", "Gain"])["features"][0]["value"]
            .as_f64()
            .unwrap()
    };
    // Runs a printed hint word for word; the ids need no quoting.
    let run = |hint: &str| {
        let args = hint.split_whitespace().skip(1).collect::<Vec<_>>();
        let out = cmd(&dir, &args);
        assert!(out.status.success(), "{hint}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let hint = |text: &str, prefix: &str| {
        text.lines()
            .find_map(|line| {
                line.split_once(prefix)
                    .map(|(_, hint)| hint.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("no {prefix} in {text}"))
    };
    on("sim:0", &["connect", "sim:0"]);
    on("sim:0", &["set", "Gain=3"]);
    let out = cmd(
        &dir,
        &[
            "--session",
            "setup-two",
            "--camera",
            "sim:0",
            "setup",
            "save",
            "bench",
        ],
    );
    assert!(out.status.success());
    let restore = hint(&String::from_utf8_lossy(&out.stdout), "Restore with:");
    assert_eq!(
        restore,
        "capturefab --session setup-two --camera sim:0 setup apply bench"
    );
    on("sim:0", &["set", "Gain=9"]);
    // sim:1 is now the session's active camera.
    on("sim:1", &["connect", "sim:1"]);
    on("sim:1", &["set", "Gain=4"]);
    assert_eq!(
        json(&dir, &["--json", "--session", "setup-two", "status"])["active_camera"],
        "sim:1"
    );
    let applied = run(&restore);
    assert_eq!((gain("sim:0"), gain("sim:1")), (3.0, 4.0), "{applied}");
    let undo = hint(&applied, "Undo with:");
    assert_eq!(undo, "capturefab --session setup-two --camera sim:0 undo");
    on("sim:1", &["set", "Gain=5"]);
    run(&undo);
    assert_eq!((gain("sim:0"), gain("sim:1")), (9.0, 5.0));
}

/// Restoring a change outside the known image settings needs --yes, by name and from a
/// file; the refusal changes nothing and names the commands that review
/// and confirm the same setup on the same camera.
#[test]
fn saved_setups_require_yes_for_disruptive_changes() {
    let root = temp_root("setup-yes");
    let dir = root.join("sessions");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &cwd, "setup-yes"),
        dir: root.clone(),
    };
    let base = ["--session", "setup-yes", "--camera", "sim:line:conveyor"];
    let on = |args: &[&str]| json(&dir, &[&["--json"][..], &base, args].concat());
    let refused = |args: &[&str]| {
        let out = cmd(&dir, &[&["--json"][..], &base, args].concat());
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(error["error"]["code"], "usage", "{error}");
        error["error"]["message"].as_str().unwrap().to_owned()
    };
    let rate = || {
        on(&["get", "AcquisitionLineRate"])["features"][0]["value"]
            .as_f64()
            .unwrap()
    };
    on(&["connect", "sim:line:conveyor"]);
    on(&["setup", "save", "line"]);
    let saved = rate();
    on(&["set", "AcquisitionLineRate=500"]);
    assert_ne!(saved, 500.0);
    let diff = on(&["setup", "diff", "line"]);
    assert_eq!(
        diff["changes"][0]["feature"], "AcquisitionLineRate",
        "{diff}"
    );
    assert_eq!(diff["changes"][0]["risk"], "disruptive");

    let message = refused(&["setup", "apply", "line"]);
    assert!(
        message.contains(
            "AcquisitionLineRate, which is not a known image setting and may change triggering"
        ),
        "{message}"
    );
    assert!(!message.contains("triggers or signals"), "{message}");
    assert!(
        message
            .contains("\nReview with: capturefab --session setup-yes --camera sim:line:conveyor setup diff line\n")
            && message.ends_with(
                "\nThen confirm with: capturefab --session setup-yes --camera sim:line:conveyor setup apply line --yes"
            ),
        "{message}"
    );
    assert_eq!(rate(), 500.0);
    // Human mode prints the diff before refusing.
    let out = cmd(&dir, &[&base[..], &["setup", "apply", "line"]].concat());
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Would change 1 on sim:line:conveyor")
            && text.contains("! AcquisitionLineRate: 500 → ")
            && text.contains("restoring needs --yes"),
        "{text}"
    );

    // The same gate for a file; the hint reviews that file, not the
    // saved setup its document is named after.
    let shown = cmd(&dir, &["--json", "setup", "show", "line"]);
    let file = cwd.join("line copy.json");
    std::fs::write(&file, &shown.stdout).unwrap();
    let path = file.to_str().unwrap();
    let message = refused(&["setup", "apply", "--file", path, "--strict"]);
    let quoted = format!("'{path}'");
    assert!(
        message.contains(&format!("setup diff --file {quoted}\n"))
            && message.ends_with(&format!("setup apply --file {quoted} --strict --yes")),
        "{message}"
    );
    assert!(!message.contains("setup diff line"), "{message}");
    assert_eq!(rate(), 500.0);
    let history = on(&["history"]).to_string();
    assert!(!history.contains("Restore setup line"), "{history}");

    let applied = on(&["setup", "apply", "line", "--yes"]);
    assert_eq!(applied["camera_change"]["label"], "Restore setup line");
    assert_eq!(rate(), saved);
    on(&["undo"]);
    assert_eq!(rate(), 500.0);
}

/// Checks that need no running camera: a taken name is refused before the
/// camera is read, a strict preview fails as the restore would, and the
/// diff's --allow-other-model is described as what it does.
#[test]
fn setup_save_and_preview_refuse_early_and_strictly() {
    let root = temp_root("setup-early");
    let dir = root.join("sessions");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    json(
        &dir,
        &["--json", "--camera", "sim:0", "setup", "save", "taken"],
    );
    // No GigE camera answers at this address; the name is checked first.
    let start = std::time::Instant::now();
    let out = cmd(
        &dir,
        &[
            "--json",
            "--camera",
            "gige:192.0.2.1",
            "setup",
            "save",
            "TAKEN",
        ],
    );
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "{error}");
    assert_eq!(error["error"]["code"], "usage");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("pass --replace"),
        "{error}"
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    // --replace goes on to the camera.
    let out = cmd(
        &dir,
        &[
            "--json",
            "--camera",
            "gige:192.0.2.1",
            "setup",
            "save",
            "taken",
            "--replace",
        ],
    );
    assert_ne!(out.status.code(), Some(2));
    assert_ne!(out.status.code(), Some(0));

    // A value this camera lacks: a strict preview fails like the restore.
    let shown = cmd(&dir, &["--json", "setup", "show", "taken"]);
    let mut document: Value = serde_json::from_slice(&shown.stdout).unwrap();
    document["result"]["values"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"feature":"Scan3dOutputMode","value":"Range"}));
    let file = cwd.join("extra.json");
    std::fs::write(&file, serde_json::to_vec(&document).unwrap()).unwrap();
    let path = file.to_str().unwrap();
    let apply = |extra: &[&str]| {
        cmd(
            &dir,
            &[
                &[
                    "--json", "--camera", "sim:0", "setup", "apply", "--file", path,
                ][..],
                extra,
            ]
            .concat(),
        )
    };
    assert!(apply(&["--dry-run"]).status.success());
    for flags in [&["--dry-run", "--strict"][..], &["--strict"]] {
        let out = apply(flags);
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(5), "{flags:?} {error}");
        assert!(
            error["error"]["message"].as_str().unwrap().contains(
                "cannot be applied strictly; nothing changed. Not applicable: Scan3dOutputMode"
            ),
            "{error}"
        );
    }

    let help = cmd(&dir, &["setup", "diff", "--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("a diff always compares the settings both cameras share"),
        "{help}"
    );
    // read_setup takes no fields, as web/agents.md says.
    let mut rpc = binary(&dir)
        .args(["--json", "--camera", "sim:0", "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    rpc.stdin
        .take()
        .unwrap()
        .write_all(br#"{"op":"read_setup","name":"taken"}"#)
        .unwrap();
    let out = rpc.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// Split a command line printed with `hint::shell_quote`.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                started = true;
            }
            ' ' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Damaged and newer setup files are listed and deletable, and notes stay
/// on one line.
#[test]
fn unreadable_setups_list_and_delete_and_inline_setups_take_the_envelope() {
    let root = temp_root("setup-store");
    let dir = root.join("sessions");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let run = |args: &[&str]| {
        let out = cmd(&dir, args);
        let value: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        (out.status.code().unwrap(), value)
    };
    json(
        &dir,
        &[
            "--json", "--direct", "--camera", "sim:0", "setup", "save", "low",
        ],
    );
    let setups = dir.join("setups");
    std::fs::write(setups.join("broken.json"), br#"{"name":"broken","#).unwrap();
    let mut future: Value =
        serde_json::from_slice(&std::fs::read(setups.join("low.json")).unwrap()).unwrap();
    future["version"] = serde_json::json!(2);
    future["name"] = serde_json::json!("future");
    future["lens"] = serde_json::json!("25mm");
    std::fs::write(
        setups.join("future.json"),
        serde_json::to_vec(&future).unwrap(),
    )
    .unwrap();
    let list = json(&dir, &["--json", "setup", "list"]);
    let rows = list["setups"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{list}");
    assert_eq!(rows[0]["name"], "broken");
    assert_eq!(rows[0]["status"], "unreadable");
    assert!(rows[0]["reason"].is_string());
    assert_eq!(rows[1]["status"], "newer_version");
    assert!(rows[2].get("status").is_none());
    let text = String::from_utf8_lossy(&cmd(&dir, &["setup", "list"]).stdout).into_owned();
    assert!(text.contains("cannot be read"), "{text}");
    assert!(text.contains("capturefab setup delete broken"), "{text}");
    // A newer file is a good setup: the row never suggests deleting it.
    let future_row = text
        .lines()
        .find(|line| line.starts_with("future"))
        .unwrap();
    assert!(
        future_row.contains("saved by a newer Capturefab; update Capturefab to use it"),
        "{text}"
    );
    assert!(!future_row.contains("delete"), "{text}");
    // Using it is unsupported here, from the store or from a file; a
    // damaged one is unavailable.
    for args in [
        &["--json", "setup", "show", "future"][..],
        &["--json", "--camera", "sim:0", "setup", "diff", "future"],
        &["--json", "--camera", "sim:0", "setup", "apply", "future"],
    ] {
        let (code, error) = run(args);
        assert_eq!(
            (code, error["error"]["code"].as_str()),
            (5, Some("unsupported")),
            "{args:?}: {error}"
        );
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains("update Capturefab"), "{message}");
        assert!(!message.contains("delete"), "{message}");
    }
    let future_file = cwd.join("future.json");
    std::fs::write(&future_file, serde_json::to_vec(&future).unwrap()).unwrap();
    let (code, error) = run(&["--json", "setup", "import", future_file.to_str().unwrap()]);
    assert_eq!(
        (code, error["error"]["code"].as_str()),
        (5, Some("unsupported")),
        "{error}"
    );
    let (code, error) = run(&["--json", "setup", "show", "broken"]);
    assert_eq!(
        (code, error["error"]["code"].as_str()),
        (3, Some("unavailable")),
        "{error}"
    );
    // A copied file's suggested command works as printed.
    std::fs::copy(setups.join("low.json"), setups.join("copy.json")).unwrap();
    let (code, error) = run(&["--json", "setup", "show", "copy"]);
    assert_eq!(code, 3, "{error}");
    let message = error["error"]["message"].as_str().unwrap();
    let (_, command) = message
        .split_once("Save it under this name with: capturefab ")
        .unwrap();
    let (command, _) = command.split_once(", or remove").unwrap();
    let args = shell_words(command);
    let mut args = args.iter().map(String::as_str).collect::<Vec<_>>();
    args.insert(0, "--json");
    json(&dir, &args);
    assert_eq!(
        json(&dir, &["--json", "setup", "show", "copy"])["name"],
        "copy"
    );
    json(&dir, &["--json", "setup", "delete", "copy"]);
    let removed = json(&dir, &["--json", "setup", "delete", "broken"]);
    assert_eq!(removed["removed"], "broken");
    json(&dir, &["--json", "setup", "delete", "future"]);
    assert_eq!(
        json(&dir, &["--json", "setup", "list"])["setups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Notes cannot carry line breaks or terminal escapes.
    let (code, error) = run(&[
        "--json", "--direct", "--camera", "sim:0", "setup", "save", "x", "--note", "a\nb",
    ]);
    assert_eq!((code, error["error"]["code"].as_str()), (2, Some("usage")));
    let shown = cmd(&dir, &["--json", "setup", "show", "low"]);
    assert!(shown.status.success());
    let envelope: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let mut evil = envelope["result"].clone();
    evil["name"] = serde_json::json!("evil");
    evil["note"] = serde_json::json!("\u{1b}]0;x\u{7}\nFAKE  2026-01-01  99  line");
    let evil_file = cwd.join("evil.json");
    std::fs::write(&evil_file, serde_json::to_vec(&evil).unwrap()).unwrap();
    let (code, error) = run(&["--json", "setup", "import", evil_file.to_str().unwrap()]);
    assert_eq!(code, 2);
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("control characters"), "{message}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Save a simulator's settings, change them, preview and restore the setup
/// in a session, undo it, and move it between files and RPC requests.
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
    // The note is in the JSON, not only on stderr.
    let note = preview["result"]["warnings"][0]
        .as_str()
        .unwrap_or_default();
    assert!(
        note.contains("Pattern camera") && note.contains("Pattern line scan camera"),
        "{preview}"
    );
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
    // A restore that fails says every value was put back.
    let mut bad: Value = serde_json::from_slice(&shown.stdout).unwrap();
    for value in bad["result"]["values"].as_array_mut().unwrap() {
        if value["feature"] == "Gain" {
            value["value"] = serde_json::json!(99999);
        }
    }
    let bad_file = cwd.join("bad.json");
    std::fs::write(&bad_file, serde_json::to_vec(&bad).unwrap()).unwrap();
    // The preview says this value fails the whole restore.
    let preview = session(&["setup", "diff", "--file", bad_file.to_str().unwrap()]);
    let gain = preview["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["feature"] == "Gain")
        .unwrap()
        .clone();
    assert!(
        gain["warning"]
            .as_str()
            .unwrap()
            .contains("restoring fails and puts every value back"),
        "{gain}"
    );
    let (code, error) = fail(&[
        "--json",
        "--session",
        "setup-ci",
        "--camera",
        "sim:0",
        "setup",
        "apply",
        "--file",
        bad_file.to_str().unwrap(),
    ]);
    assert_eq!(
        (code, error["code"].as_str()),
        (1, Some("operation_failed"))
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("Restoring setup bench failed; every value was put back"),
        "{error}"
    );
    assert_eq!(exposure(&session), 10000.0);
    // The bare document works too.
    let document: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let file = cwd.join("bench.json");
    std::fs::write(&file, serde_json::to_vec(&document["result"]).unwrap()).unwrap();
    let file = file.to_str().unwrap();
    let imported = json(&dir, &["--json", "setup", "import", file, "--name", "copy"]);
    assert_eq!(imported["setup"]["name"], "copy");
    let (code, _) = fail(&["--json", "setup", "import", file, "--name", "copy"]);
    assert_eq!(code, 2);
    // An inline setup in an RPC request takes the whole `setup show --json`
    // output or its result.
    for setup in [document.clone(), document["result"].clone()] {
        session(&["set", "ExposureTime=6000"]);
        let mut rpc = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .env("CAPTUREFAB_CACHE_DIR", test_cache())
            .args([
                "--json",
                "--session",
                "setup-ci",
                "--camera",
                "sim:0",
                "rpc",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        serde_json::to_writer(
            rpc.stdin.take().unwrap(),
            &serde_json::json!({"op":"apply_setup","setup":setup}),
        )
        .unwrap();
        let out = rpc.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert_eq!(exposure(&session), 10000.0);
    }
    let removed = json(&dir, &["--json", "setup", "delete", "bench"]);
    assert_eq!(removed["removed"], "bench");
    let list = json(&dir, &["--json", "setup", "list"]);
    assert_eq!(list["setups"].as_array().unwrap().len(), 1);
    let schema = json(&dir, &["--json", "schema"]);
    assert!(schema["setup"]["cli"].is_string());
}
