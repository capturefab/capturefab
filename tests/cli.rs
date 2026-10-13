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
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .args(args)
        .output()
        .unwrap()
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
        let direct = cmd(&dir, &["--json", "--camera", "sim:0", action]);
        assert!(!direct.status.success());
        assert!(
            serde_json::from_slice::<Value>(&direct.stdout).unwrap()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("needs --session")
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
    let direct = cmd(&dir, &["--json", "--camera", "sim:0", "auto"]);
    assert_eq!(direct.status.code(), Some(1));
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
