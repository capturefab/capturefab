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

/// The binary with isolated state.
fn binary(dir: &PathBuf) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache());
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
        serde_json::from_str::<Value>(&ready).unwrap()["ready"],
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
            "capture",
            "--camera",
            "sim:visible",
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
    // The only running session took the capture: it now holds the camera.
    let held = json(&dir, &["--json", "status"]);
    assert_eq!(held["cameras"][0]["info"]["id"], "sim:visible");
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
            "set",
            "--camera",
            "sim:visible",
            "ExposureTime=2000",
        ],
    );
    let status = json(&dir, &["--json", "status"]);
    assert_eq!(status["cameras"].as_array().unwrap().len(), 2);
    assert!(
        status["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .any(|camera| camera["worker_pid"] == worker_pid)
    );
    // With two cameras connected, a camera command must name one.
    let unnamed = cmd(&dir, &["--json", "--session", "visual", "get", "Width"]);
    assert_eq!(unnamed.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&unnamed.stdout).unwrap();
    assert_eq!(error["error"]["code"], "camera_ambiguous");
    assert_eq!(
        error["error"]["details"]["candidates"],
        serde_json::json!([{"id":"sim:other"},{"id":"sim:visible"}])
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
            "capture",
            "--camera",
            "sim:visible",
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
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn session_recording_duration_stops_its_camera_while_others_connect() {
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
            "record",
            "--camera",
            "sim:recording",
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
        let state = json(&dir, &["--json", "status", "--camera", "sim:recording"]);
        if state["cameras"][0]["forwarding"].is_string() {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "recording failed to start"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    json(&dir, &["--json", "connect", "sim:third"]);
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
    assert_eq!(state["cameras"].as_array().unwrap().len(), 3);
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
    assert!(v.get("error").is_none(), "{v}");
    v
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
        let mut args = vec!["--json", "record"];
        args.extend(
            camera
                .map(|camera| ["--camera", camera])
                .into_iter()
                .flatten(),
        );
        args.extend([
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
            !out.status.success() && reply.get("error").is_some(),
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
fn network_scan_preserves_active_non_gige_streams() {
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
            "serve",
            "--camera",
            "sim:0",
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
        serde_json::from_str::<Value>(&ready).unwrap()["ready"],
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
            "network",
            "--tune",
            "--timeout",
            "50ms",
        ],
    );
    let report = &result;
    assert!(report["interfaces"].is_array());
    assert!(report["cameras"].is_array());
    assert!(report.get("warnings").is_none_or(Value::is_array));
    assert!(report["time"].is_string());
    assert!(
        report["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .all(|camera| camera["camera"]["kind"] == "gige"
                && camera["active"] == false
                && camera["tuning"].is_null())
    );
    let after = json(&dir, &["--json", "--session", "network-ci", "status"]);
    let (after, before) = (&after["cameras"][0], &before["cameras"][0]);
    assert_eq!(after["info"]["id"], before["info"]["id"]);
    assert_eq!(after["worker_pid"], before["worker_pid"]);
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
        vec!["capture", "--camera", "sim:0", "-o", "rtsp://host/stream"],
        vec!["capture", "--camera", "sim:0", "-o", "shot-{unknown}.png"],
        vec![
            "capture",
            "--camera",
            "sim:0",
            "-o",
            "shot.png",
            "--delay",
            "1.1ms",
            "--set",
            "Width=128",
        ],
        vec![
            "record", "--camera", "sim:0", "-o", "shot.mp4", "--fps", "NaN", "--auto",
        ],
        vec![
            "debug",
            "read",
            "--camera",
            "sim:0",
            "18446744073709551615",
            "2",
        ],
        vec!["set", "--camera", "sim:0", "Width=128", "=bad"],
        vec!["get", "--camera", "gige:gige:192.0.2.1", "Width"],
        vec!["serve", "--camera", "sim:0", "--name", "../bad"],
        vec!["debug", "xml", "--camera", "sim:0", "-o", ""],
        // An empty local path never becomes the current directory.
        vec!["capture", "--camera", "sim:replay:", "-o", "unused.png"],
        vec!["capture", "--camera", "file:", "-o", "unused.png"],
        vec!["capture", "--camera", "FiLe:", "-o", "unused.png"],
        vec!["connect", "sim:replay:"],
        vec!["connect", "file:"],
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
            .args(["--json", "get", "--camera", "sim:0", "Width"])
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
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["ready"], true);
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
            "set",
            "--camera",
            "sim:0",
            "Width=128",
        ],
    );
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "set",
            "--camera",
            "sim:1",
            "Width=256",
        ],
    );
    let a = json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "get",
            "--camera",
            "sim:0",
            "Width",
        ],
    );
    let b = json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "get",
            "--camera",
            "sim:1",
            "Width",
        ],
    );
    assert_eq!(a["values"]["Width"], 128);
    assert_eq!(b["values"]["Width"], 256);
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
                "capture",
                "--camera",
                "sim:0",
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
                "capture",
                "--camera",
                "sim:1",
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
            "capture",
            "--camera",
            "sim:0",
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
        &["--json", "--session", "ci", "status", "--camera", "sim:0"],
    );
    assert_eq!(targeted["cameras"].as_array().unwrap().len(), 1);
    assert_eq!(targeted["cameras"][0]["info"]["id"], "sim:0");
    let sequence = dir.join("scheduled");
    let result = json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "capture",
            "--camera",
            "sim:0",
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
            &["--json", "--session", "ci", "jobs", "--camera", "sim:0"],
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
            "capture",
            "--camera",
            "sim:0",
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
            "cancel",
            "--camera",
            "sim:0",
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
            "capture",
            "--camera",
            "sim:0",
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
            "disconnect",
            "--camera",
            "sim:0",
        ],
    );
    json(
        &dir,
        &[
            "--json",
            "--session",
            "ci",
            "disconnect",
            "--camera",
            "sim:1",
        ],
    );
    let raw = cmd(
        &dir,
        &[
            "capture",
            "--camera",
            "sim:raw",
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
            "--json", "serve", "--camera", "sim:0", "--name", "auto-ci", "--auto",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["ready"], true);
    let _fixture = Fixture {
        child,
        dir: dir.clone(),
    };
    let session = |args: &[&str]| json(&dir, &[&["--json", "--session", "auto-ci"], args].concat());
    let status = session(&["status"]);
    assert_eq!(status["cameras"][0]["auto"]["balance"], 0.5);
    assert_eq!(
        session(&["auto", "--balance", "0.25"])["auto"]["balance"],
        0.25
    );
    assert_eq!(session(&["auto"])["auto"]["balance"], 0.25);
    assert_eq!(session(&["status"])["cameras"][0]["auto"]["balance"], 0.25);
    let before_width = session(&["get", "Width"])["values"]["Width"].clone();
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
            "record",
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
            session(&["status"])["cameras"][0]["auto"]["balance"],
            0.25,
            "invalid input changed auto mode: {args:?}"
        );
        assert_eq!(
            session(&["get", "Width"])["values"]["Width"],
            before_width,
            "invalid input changed a feature: {args:?}"
        );
    }
    assert!(session(&["manual"])["auto"].is_null());
    assert!(session(&["status"])["cameras"][0]["auto"].is_null());
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
    assert_eq!(session(&["status"])["cameras"][0]["auto"]["balance"], 0.0);
    let usage = cmd(
        &dir,
        &["--json", "--session", "auto-ci", "auto", "--balance", "2"],
    );
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&usage.stdout).unwrap()["error"]["code"],
        "usage"
    );
    let direct = cmd(
        &dir,
        &["--json", "--no-session", "auto", "--camera", "sim:0"],
    );
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
            "capture",
            "--camera",
            "sim:0",
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
        serde_json::from_str::<Value>(&line).unwrap()["ready"],
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
                "info",
                "--camera",
                selector,
            ],
        );
        assert_eq!(info["camera"], selector);
        let features = json(
            &dir,
            &[
                "--json",
                "--session",
                "types-ci",
                "features",
                "--all",
                "--camera",
                selector,
            ],
        );
        assert!(
            features["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature["name"] == control)
        );
        if kind == "line_scan" {
            assert!(
                !features["features"]
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
                "capture",
                "--camera",
                selector,
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
                    "auto",
                    "--camera",
                    selector,
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
                "status",
                "--camera",
                selector,
            ],
        );
        assert_eq!(
            status["cameras"][0]["info"]["profile"]["kind"], kind,
            "profile lost after capture"
        );
    }
    let all = json(&dir, &["--json", "--session", "types-ci", "status"]);
    assert_eq!(all["cameras"].as_array().unwrap().len(), 5);
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
fn unread_streams_report_no_losses_and_keep_frames_off_disk() {
    let root = temp_root("ring-unread");
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
            "the stream never produced enough frames: {status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let camera = &status["cameras"][0];
    let frames = camera["frames"].as_u64().unwrap();
    let dropped = camera["dropped"].as_u64().unwrap();
    assert!(frames > 30, "{camera}");
    // Frames nobody read were neither lost nor skipped.
    assert_eq!(
        (camera["lost"].as_u64(), camera["preview_skipped"].as_u64()),
        (Some(0), Some(0))
    );
    assert!(dropped * 10 < frames, "{dropped} dropped of {frames}");
    if cfg!(target_os = "linux") {
        let pid = camera["worker_pid"].as_u64().unwrap();
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
        assert!(maps.contains("/memfd:capturefab-ring"), "{maps}");
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
            "capture",
            "--camera",
            "sim:0",
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
        std::fs::create_dir_all(&captures).unwrap();
        let path = captures.join("saved.png");
        json(
            &dir,
            &[
                "--json",
                "--session",
                name,
                "capture",
                "--camera",
                "sim:0",
                "-o",
                path.to_str().unwrap(),
            ],
        );
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
/// still finishes a recording in its folder and storage ledger.
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
        std::fs::create_dir_all(&recordings).unwrap();
        let clip = recordings.join("clip.mp4");
        json(
            &dir,
            &[
                "--json",
                "--session",
                name,
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
            let status = json(&dir, &["--json", "--session", name, "status"]);
            if status["cameras"][0]["forwarding"].is_string() && start.elapsed().as_secs() >= 1 {
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
        if status["cameras"][0]["forwarding"].is_string() && start.elapsed().as_secs() >= 1 {
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
    serde_json::from_slice::<Value>(&out.stdout).unwrap()
}

#[test]
fn storage_status_reports_free_space_and_the_schema_its_floor() {
    let dir = temp_root("storage-status");
    let status = json(&dir, &["--json", "storage"]);
    assert!(status["free_bytes"].is_u64(), "{status}");
    assert_eq!(status["files"], 0);
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
            "capture",
            "--camera",
            "sim:0",
            "-o",
            shot.to_str().unwrap(),
        ],
    );
    let doctor = json_env(&dir, &env, &["--json", "doctor"]);
    let budget = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "storage_budget")
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
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    command(&["--json", "storage", "configure", "--max-space", "2GiB"]);
    let directory = state.join("capturefab");
    assert!(directory.join("storage-policy.json").is_file());
    assert!(!run.join("capturefab/storage-policy.json").exists());
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
        ready.ends_with("Try: capturefab --session banner info"),
        "{ready}"
    );
    assert_eq!(stop, "Ctrl-C stops the session.");
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
            "record",
            "--camera",
            "sim:0",
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
            "record",
            "--camera",
            "sim:0",
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
                "record",
                "--camera",
                "sim:0",
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
#[cfg_attr(not(feature = "jpeg"), ignore = "needs the jpeg feature")]
fn capture_outputs_number_templates_and_carry_context() {
    let root = naming_root("naming-cli");
    let dir = root.join("sessions");
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let capture = |args: &[&str]| {
        let mut all = vec!["--json", "capture", "--camera", "sim:0"];
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
        v["files"]
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
        let path = std::path::Path::new(v["files"][0].as_str().unwrap());
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
        assert_eq!(v["frames"][0]["file"], v["files"][0]);
        assert!(v["frames"][0]["wall_ms"].as_u64().unwrap() > 0);
        assert_eq!(v["context"]["line"], "L1");
        let frame = &v["frames"][0];
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
        let mut all = vec!["--json", "capture", "--camera", "sim:0", "-o", file];
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
#[cfg_attr(not(feature = "jpeg"), ignore = "needs the jpeg feature")]
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
                "debug",
                "rpc",
                "--camera",
                "sim:0",
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
        response.clone()
    };
    let session = |args: &[&str]| {
        json(
            &dir,
            &[
                &["--json", "--session", "frame-ci"][..],
                args,
                &["--camera", "sim:0"],
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
    assert!(capturefab::testing::jpeg::well_formed(&bytes, 256, 192));
    let status = session(&["status"]);
    assert_eq!(status["cameras"][0]["streaming"], false);

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

    // One grouped change; the next fresh frame shows it, and putting the
    // values back restores the exposure.
    let before = session(&["get", "ExposureTime", "Gain"]);
    let value = |name: &str| before["values"][name].to_string();
    let changed = ok(
        r#"{"op":"set_values","values":[{"name":"ExposureTime","value":"40000"},{"name":"Gain","value":"0"}]}"#,
    );
    assert_eq!(changed["changed"][0]["name"], "ExposureTime");
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
    session(&[
        "set",
        &format!("ExposureTime={}", value("ExposureTime")),
        &format!("Gain={}", value("Gain")),
    ]);
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
    // Grab belongs to camera workers; debug rpc takes public commands only.
    let (code, grab) = rpc(r#"{"op":"grab","timeout_ms":1000}"#);
    assert_eq!(code, Some(2), "{grab}");
    // Thirty frames a second cannot bring ten new ones within 200 ms.
    let (code, slow) = rpc(r#"{"op":"frame","fresh":10,"timeout_ms":200}"#);
    assert_eq!(code, Some(4), "{slow}");
    assert_eq!(slow["error"]["code"], "frame_timeout");
    assert!(
        slow["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("no new frame from sim:0 within 200 ms (timed out)"),
        "{slow}"
    );
    assert_eq!(session(&["status"])["cameras"][0]["streaming"], true);
}

/// A capture schedule starts a stopped camera and stops it again when done,
/// with no command in between. The last status the session saw still says
/// streaming, but an inspection afterwards grabs a frame instead of waiting
/// for live frames that never come.
#[test]
fn rpc_frame_grabs_once_a_schedule_has_stopped_the_stream() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let root = temp_root("frame-after-schedule");
    let dir = root.join("sessions");
    let cwd = root.join("server");
    let shots = root.join("shots");
    std::fs::create_dir_all(&cwd).unwrap();
    let _fixture = Fixture {
        child: serve_at(&dir, &cwd, "schedule-frame"),
        dir: root.clone(),
    };
    let session = |args: &[&str]| {
        json(
            &dir,
            &[&["--json", "--session", "schedule-frame"], args].concat(),
        )
    };
    session(&["connect", "sim:0"]);
    let output = shots.join("x-{seq}.ppm");
    // Shots due back to back keep the stream on until the last is saved.
    session(&[
        "capture",
        "-o",
        output.to_str().unwrap(),
        "-n",
        "5",
        "--interval",
        "1ms",
    ]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while session(&["status"])["cameras"][0]["streaming"] != true {
        assert!(Instant::now() < deadline, "the schedule never streamed");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Watch the files rather than status, which would ask the worker.
    while std::fs::read_dir(&shots).map_or(0, |files| files.count()) < 5 {
        assert!(Instant::now() < deadline, "the schedule did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_secs(1));

    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_CACHE_DIR", test_cache())
        .args(["--json", "--session", "schedule-frame", "debug", "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"op":"frame","max_dimension":0}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let frame: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{frame}");
    assert_eq!(frame["frame"]["source"], "grab", "{frame}");
    assert_eq!(
        session(&["status"])["cameras"][0]["streaming"],
        false,
        "a grab leaves the camera stopped"
    );
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
            "connect",
            "--camera",
            "sim:1",
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
        text.contains("Next: capturefab --session conn capture -c sim:3 -o 'shot-{seq}.png'"),
        "{text}"
    );
    // Several cameras: a camera command must name one.
    let unnamed = cmd(&root, &["--session", "conn", "get", "Width"]);
    assert_eq!(unnamed.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unnamed.stderr).contains("choose one with -c"),
        "{}",
        String::from_utf8_lossy(&unnamed.stderr)
    );
    // A session name typo points at the running session.
    let typo = cmd(&root, &["--json", "--session", "conx", "status"]);
    assert_eq!(typo.status.code(), Some(3));
    let error = failure(&typo);
    assert_eq!(error["code"], "session_not_found");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("did you mean --session conn?"),
        "{message}"
    );
    assert!(!message.contains("os error"), "{message}");
}

fn text(output: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Capture names mean what they look like, and human output summarizes.
#[test]
#[cfg_attr(not(feature = "jpeg"), ignore = "needs the jpeg feature")]
fn capture_output_paths_and_summaries() {
    let root = temp_root("capture-guardrails");
    let dir = root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    let capture = |args: &[&str]| {
        let mut all = vec!["capture", "--camera", "sim:0"];
        all.extend(args);
        cmd(&dir, &all)
    };
    let json_capture = |args: &[&str]| {
        let mut all = vec!["--json", "capture", "--camera", "sim:0"];
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
    let frame = &reply["frames"][0];
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
    assert_eq!(reply["error"]["code"], "exists");
    assert!(reply["error"]["details"]["path"].is_string(), "{reply}");
    assert!(
        reply["error"]["hint"].as_str().unwrap().contains("{seq}"),
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
    let out = cmd(&dir, &["info", "--camera", "sim:0"]);
    let (stdout, _) = text(&out);
    assert!(
        stdout.starts_with("sim:0  Capturefab Pattern camera"),
        "{stdout}"
    );
    assert!(!stdout.contains('{'), "{stdout}");
    // A taken recording file fails before any encoder starts or banner shows.
    std::fs::write(root.join("existing.mp4"), b"keep").unwrap();
    let out = cmd(
        &dir,
        &[
            "record",
            "--camera",
            "sim:0",
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
            "capture",
            "--camera",
            "sim:0",
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
            "capture",
            "--camera",
            "sim:0",
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
            "record",
            "--camera",
            "sim:0",
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
        let mut args = vec!["--json", "capture", "--camera", "sim:0", "-o", template];
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
        let file = std::path::PathBuf::from(v["files"][0].as_str().unwrap());
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
