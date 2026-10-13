//! Public CLI coverage using a deterministic SANE executable in place of a
//! hardware driver. All environment settings belong to child processes only.
#![cfg(all(feature = "scanner", target_os = "linux"))]

use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEVICE: &str = "scanner:sane:airscan:e0:Office Scanner|unit";
const FAKE_SCANIMAGE: &str = r#"#!/bin/sh
set -eu
fixture=${CAPTUREFAB_SCANNER_FIXTURE_DIR:?}
case "$1" in
  --formatted-device-list=*)
    printf 'airscan:e0:Office Scanner|unit\tACME\tDocument Scanner\tflatbed scanner\n'
    exit 0
    ;;
  --device-name)
    if [ "$2" != 'airscan:e0:Office Scanner|unit' ]; then
      printf 'wrong scanner ID\n' >&2
      exit 1
    fi
    ;;
  *) printf 'unsupported invocation\n' >&2; exit 1 ;;
esac
printf 'scan\n' >> "$fixture/scans"
mode=Color
while [ "$#" -gt 0 ]; do
  printf '%s\n' "$1" >> "$fixture/arguments"
  if [ "$1" = '--mode' ]; then mode=$2; fi
  shift
done
if [ -f "$fixture/fail" ]; then
  printf 'Unsupported scanner source: no document feeder\n' >&2
  exit 1
fi
if [ -f "$fixture/hang" ]; then
  # Replace the helper process, so cancellation does not leave descendants
  # holding its stdout/stderr pipes open.
  exec /bin/sleep 10
fi
if [ "$mode" = 'Gray' ]; then
  printf 'P5\n2 1\n255\n\001\377'
else
  printf 'P6\n2 1\n255\n\377\000\000\000\377\000'
fi
"#;

struct Fixture {
    child: Child,
    dir: PathBuf,
    executable: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-scanner-cli-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let executable = dir.join("scanimage-fixture");
        fs::write(&executable, FAKE_SCANIMAGE).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .env("CAPTUREFAB_SCANIMAGE", &executable)
            .env("CAPTUREFAB_SCANNER_FIXTURE_DIR", &dir)
            .args(["--json", "serve", "--name", "scanner-ci"])
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
        Self {
            child,
            dir,
            executable,
        }
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_SESSION_DIR", &self.dir)
            .env("CAPTUREFAB_SCANIMAGE", &self.executable)
            .env("CAPTUREFAB_SCANNER_FIXTURE_DIR", &self.dir)
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true);
        value["result"].clone()
    }

    fn session(&self, args: &[&str]) -> Value {
        let mut full = vec!["--json", "--session", "scanner-ci", "--camera", DEVICE];
        full.extend_from_slice(args);
        self.json(&full)
    }

    fn scan_count(&self) -> usize {
        fs::read_to_string(self.dir.join("scans"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Ask the worker to release scanner resources before the server exits.
        let _ = self.command(&[
            "--json",
            "--session",
            "scanner-ci",
            "--camera",
            DEVICE,
            "disconnect",
        ]);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn scanners_are_discovered_and_each_saved_capture_requests_one_page() {
    let fixture = Fixture::new();
    let discovered = fixture.json(&["--json", "scanners"]);
    let scanner = discovered["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["id"] == DEVICE)
        .unwrap();
    assert_eq!(scanner["transport"], "scanner");
    assert_eq!(scanner["vendor"], "ACME");
    fixture.session(&["connect", DEVICE]);
    fixture.session(&[
        "set",
        "ScanResolution=300",
        "ScanMode=Gray",
        "ScanSource=Feeder",
    ]);
    fixture.session(&["start"]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fixture.scan_count(),
        0,
        "discovery, connection, and start must not scan"
    );

    let output_dir = fixture.dir.join("pages");
    let captured = fixture.session(&[
        "capture",
        "-n",
        "2",
        "--format",
        "pgm",
        "-o",
        output_dir.to_str().unwrap(),
    ]);
    assert_eq!(captured["count"], 2);
    assert_eq!(fixture.scan_count(), 2);
    let frames = captured["frames"].as_array().unwrap();
    assert_eq!(frames[0]["width"], 2);
    assert_eq!(frames[0]["height"], 1);
    assert_ne!(frames[0]["id"], frames[1]["id"]);
    let files = captured["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    for file in files {
        let data = fs::read(file.as_str().unwrap()).unwrap();
        assert!(data.starts_with(b"P5\n2 1\n255\n"));
        assert!(data.ends_with(&[1, 255]));
    }
    let args = fs::read_to_string(fixture.dir.join("arguments")).unwrap();
    assert!(
        args.contains("--resolution\n300\n--mode\nGray\n--source\nAutomatic Document Feeder\n")
    );
    // A later capture must ask the device for another page, even after the
    // previous page remains available to the session's preview.
    let next = fixture.dir.join("next.pgm");
    fixture.session(&["capture", "--format", "pgm", "-o", next.to_str().unwrap()]);
    assert_eq!(fixture.scan_count(), 3);
}

#[test]
fn scanner_timeout_cancels_pending_scan_and_later_capture_recovers() {
    let fixture = Fixture::new();
    fixture.session(&["connect", DEVICE]);
    fs::write(fixture.dir.join("hang"), b"").unwrap();
    let timed_out = fixture.dir.join("timed-out.png");
    let started = Instant::now();
    let output = fixture.command(&[
        "--json",
        "--session",
        "scanner-ci",
        "--camera",
        DEVICE,
        "--timeout-ms",
        "100",
        "capture",
        "-o",
        timed_out.to_str().unwrap(),
    ]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!timed_out.exists());
    assert_eq!(fixture.scan_count(), 1);
    fs::remove_file(fixture.dir.join("hang")).unwrap();
    let recovered = fixture.dir.join("recovered.png");
    fixture.session(&["capture", "-o", recovered.to_str().unwrap()]);
    assert_eq!(fixture.scan_count(), 2);
    assert!(
        fs::read(recovered)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
}

#[test]
fn unsupported_scanner_settings_reach_cli_without_saving_a_page() {
    let fixture = Fixture::new();
    fixture.session(&["connect", DEVICE]);
    fs::write(fixture.dir.join("fail"), b"").unwrap();
    let path = fixture.dir.join("unsupported.png");
    let output = fixture.command(&[
        "--json",
        "--session",
        "scanner-ci",
        "--camera",
        DEVICE,
        "capture",
        "-o",
        path.to_str().unwrap(),
    ]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unsupported scanner source")
    );
    assert!(!path.exists());
    assert_eq!(fixture.scan_count(), 1);
}
