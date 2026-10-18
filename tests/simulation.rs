//! Public simulation CLI: presets, argument validation and owned server lifetime.
use serde_json::Value;
use std::{
    path::PathBuf,
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Spawned binaries keep their cache, such as dataset staging, out of the
/// user's.
fn test_cache() -> PathBuf {
    std::env::temp_dir().join("capturefab-test-cache")
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
                "capturefab-simulation-cli-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )))
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
        command
            .env("CAPTUREFAB_SESSION_DIR", &self.0)
            .env("CAPTUREFAB_CACHE_DIR", test_cache());
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn simulation_presets_are_available_without_camera_resources() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["--json", "simulate", "presets"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"], true);
    let presets = json["result"]["presets"].as_array().unwrap();
    assert_eq!(
        presets
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["gige", "rtsp", "onvif", "vision"]
    );
    assert!(
        presets
            .iter()
            .all(|p| p["purpose"].as_str().is_some_and(|s| !s.is_empty()))
    );
    assert!(!fixture.0.exists());
}

#[test]
fn invalid_simulation_options_fail_before_source_or_session_creation() {
    let fixture = Fixture::new();
    for args in [
        vec!["simulate", "run", "--fps", "NaN"],
        vec!["simulate", "run", "--bind", "0.0.0.0"],
        vec!["simulate", "run", "--width", "641"],
        vec!["simulate", "run", "--source", ""],
        vec!["--camera", "sim:0", "simulate", "run"],
        vec!["--camera", "sim:replay:", "capture", "-o", "unused.png"],
        vec!["--camera", "file:", "capture", "-o", "unused.png"],
        vec!["--camera", "FiLe:", "capture", "-o", "unused.png"],
        vec!["connect", "sim:replay:"],
        vec!["connect", "file:"],
        vec!["select", "sim:replay:"],
        vec!["select", "file:"],
    ] {
        let output = fixture
            .command()
            .arg("--json")
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["error"]["code"], "usage");
        assert!(!fixture.0.exists(), "{args:?} created session resources");
    }
}

#[test]
fn bounded_gige_simulation_reports_ready_and_releases_its_socket() {
    let fixture = Fixture::new();
    let start = Instant::now();
    let output = fixture
        .command()
        .args([
            "--json",
            "simulate",
            "run",
            "--preset",
            "gige",
            "--gige-port",
            "0",
            "--duration",
            "150ms",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["result"]["ready"], true);
    assert_eq!(json["result"]["preset"], "gige");
    let port = json["result"]["gige"]["port"].as_u64().unwrap();
    assert!(port > 0);
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port as u16)).unwrap();
    drop(socket);
    assert!(!fixture.0.exists());
}
