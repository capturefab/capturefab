//! Public CLI coverage for the hub kinds beyond Home Assistant.
#![cfg(feature = "hubs")]

#[path = "support/fake_http.rs"]
mod fake_http;
#[path = "support/fake_hub.rs"]
mod fake_hub;

use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

const SECRET: &str = "CANARY-protocol-secret";
const VARIABLE: &str = "CAPTUREFAB_HUB_PROTOCOL_SECRET";

struct Bench {
    dir: PathBuf,
    output: Vec<u8>,
}

impl Bench {
    fn new(kind: &str) -> Self {
        Self {
            dir: std::env::temp_dir().join(format!(
                "capturefab-hub-protocol-{kind}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
            output: Vec::new(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
        command
            .env_remove("CAPTUREFAB_SESSION")
            .env_remove("CAPTUREFAB_CAMERA")
            .env("CAPTUREFAB_SESSION_DIR", &self.dir);
        for (name, _) in std::env::vars() {
            if name.starts_with("CAPTUREFAB_HUB_") {
                command.env_remove(name);
            }
        }
        command.env(VARIABLE, SECRET).arg("--json").args(args);
        command
    }

    fn run(&mut self, args: &[&str]) -> (i32, Value) {
        let output = self.command(args).output().unwrap();
        self.output.extend(&output.stdout);
        self.output.extend(&output.stderr);
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?}: {error}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        (output.status.code().unwrap_or(-1), value)
    }

    fn json(&mut self, args: &[&str]) -> Value {
        let (code, value) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {value}");
        assert_eq!(value["ok"], true, "{value}");
        value["result"].clone()
    }

    fn snapshot(&mut self, selector: &str) {
        let output = self.dir.join("snapshot.png");
        let result = self.json(&[
            "--camera",
            selector,
            "capture",
            "-n",
            "1",
            // Windows tries ::1 for localhost first and reports a refusal
            // there only after about two seconds.
            "--timeout-ms",
            "10000",
            "-o",
            output.to_str().unwrap(),
        ]);
        assert_eq!(result["count"], 1, "{result}");
        let decoder = png::Decoder::new(std::fs::File::open(output).unwrap());
        let image = decoder.read_info().unwrap();
        assert_eq!((image.info().width, image.info().height), (40, 30));
    }

    fn assert_no_secret(&self) {
        assert!(!String::from_utf8_lossy(&self.output).contains("CANARY"));
        fn scan(path: &Path) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                assert!(!path.to_string_lossy().contains("CANARY"));
                if path.is_dir() {
                    scan(&path);
                } else {
                    assert!(
                        !String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains("CANARY"),
                        "a secret was stored in {}",
                        path.display()
                    );
                }
            }
        }
        scan(&self.dir);
    }

    fn serve(&mut self) -> Running {
        let mut child = self
            .command(&["serve", "--name", "protocol"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let running = Running(child);
        stdout.read_line(&mut ready).unwrap();
        self.output.extend(ready.as_bytes());
        assert!(ready.contains("\"ready\":true"), "{ready}");
        running
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn frames() -> Vec<Vec<u8>> {
    (0..3)
        .map(|value| {
            let frame = capturefab::types::Frame {
                id: 1,
                width: 40,
                height: 30,
                pixel_format: capturefab::types::RGB8,
                timestamp_ns: 0,
                data: vec![value * 80; 40 * 30 * 3],
            };
            capturefab::jpeg::software(&capturefab::jpeg::Raster::new(&frame).unwrap()).unwrap()
        })
        .collect()
}

#[test]
fn frigate_authentication_camera_permissions_and_snapshots_use_the_public_cli() {
    let mut config = fake_hub::Frigate::new(frames());
    config.identity = Some(fake_http::localhost());
    config.login = Some(("viewer".into(), SECRET.into()));
    config.allowed_cameras = Some(vec!["driveway".into()]);
    let fake = config.start();
    let mut bench = Bench::new("frigate");
    let added = bench.json(&[
        "hub",
        "add-frigate",
        "nvr",
        "--url",
        &fake.url(),
        "--user",
        "viewer",
        "--secret-env",
        VARIABLE,
        "--accept-certificate",
    ]);
    assert_eq!(added["check"]["product"], "Frigate");
    assert_eq!(added["check"]["cameras"], 1);
    let cameras = bench.json(&["hub", "cameras", "nvr"]);
    assert_eq!(cameras["cameras"].as_array().unwrap().len(), 1);
    assert_eq!(cameras["cameras"][0]["id"], "hub:nvr/driveway");
    assert_eq!(cameras["cameras"][0]["name"], "Driveway");
    let (code, failure) = bench.run(&["--camera", "hub:nvr/back_yard?stream=rtsp", "info"]);
    assert_eq!(code, 3, "{failure}");
    assert_eq!(failure["error"]["code"], "unavailable");
    bench.snapshot("hub:nvr/driveway?stream=snapshot");
    assert!(
        fake.requests()
            .iter()
            .any(|request| request.path() == "/api/login")
    );
    assert!(
        fake.requests()
            .iter()
            .filter(|request| request.path() != "/api/login")
            .all(|request| request.bearer().is_some())
    );
    bench.assert_no_secret();
}

#[test]
fn go2rtc_basic_auth_names_and_snapshots_use_the_public_cli() {
    let mut config = fake_hub::Go2rtc::new(frames());
    config.basic = Some(("viewer".into(), SECRET.into()));
    let fake = config.start();
    let mut bench = Bench::new("go2rtc");
    let added = bench.json(&[
        "hub",
        "add-go2rtc",
        "streams",
        "--url",
        &fake.url(),
        "--user",
        "viewer",
        "--secret-env",
        VARIABLE,
    ]);
    assert_eq!(added["check"]["product"], "go2rtc");
    assert_eq!(added["check"]["cameras"], 2);
    let cameras = bench.json(&["hub", "cameras", "streams"]);
    assert_eq!(cameras["cameras"][0]["id"], "hub:streams/garage%20cam");
    bench.snapshot("hub:streams/garage%20cam?stream=snapshot");
    assert!(fake.requests().iter().all(|request| {
        request.path() != "/api/config"
            && request
                .header("authorization")
                .is_some_and(|auth| auth.starts_with("Basic "))
    }));
    assert!(
        fake.requests()
            .iter()
            .any(|request| request.target == "/api/frame.jpeg?src=garage%20cam")
    );
    bench.assert_no_secret();
}

#[test]
fn protect_pins_lists_snapshots_and_reuses_canonical_camera_ids() {
    let fake = fake_hub::Protect::new(SECRET, frames(), fake_http::localhost()).start();
    let mut bench = Bench::new("protect");
    let added = bench.json(&[
        "hub",
        "add-unifi-protect",
        "protect",
        "--url",
        &fake.url(),
        "--secret-env",
        VARIABLE,
        "--accept-certificate",
    ]);
    assert_eq!(added["check"]["product"], "UniFi Protect");
    assert_eq!(
        added["certificate"]["fingerprint"],
        fake_http::LOCALHOST_SHA256
    );
    let cameras = bench.json(&["hub", "cameras", "protect"]);
    assert_eq!(cameras["cameras"][0]["name"], "Doorbell");
    assert_eq!(cameras["cameras"][0]["online"], true);
    assert_eq!(cameras["cameras"][1]["online"], false);
    bench.snapshot("hub:protect/66d025b301ebc903e80003ea?stream=snapshot");
    let _server = bench.serve();
    let lower = "hub:protect/66d025b301ebc903e80003ea?stream=snapshot&fps=1";
    let upper = "hub:protect/66D025B301EBC903E80003EA?stream=snapshot&fps=1";
    let first = bench.json(&["--session", "protocol", "connect", lower]);
    let same = bench.json(&["--session", "protocol", "connect", upper]);
    assert_eq!(same["worker_pid"], first["worker_pid"]);
    assert_eq!(
        same["connected"]["id"],
        "hub:protect/66d025b301ebc903e80003ea"
    );
    let (code, failure) = bench.run(&[
        "--session",
        "protocol",
        "connect",
        "hub:protect/66D025B301EBC903E80003EA?stream=snapshot&fps=0.5",
    ]);
    assert_eq!(code, 3, "{failure}");
    assert_eq!(failure["error"]["code"], "unavailable");
    let unchanged = bench.json(&["--session", "protocol", "connect", lower]);
    assert_eq!(unchanged["worker_pid"], first["worker_pid"]);
    bench.json(&["--session", "protocol", "disconnect"]);
    assert!(fake.requests().iter().all(|request| {
        request.header("x-api-key") == Some(SECRET) && !request.path().contains("rtsps-stream")
    }));
    bench.assert_no_secret();
}
