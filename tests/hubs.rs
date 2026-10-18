//! Camera hubs through the public binary: saving hubs, checking and listing
//! them, and capturing from their cameras, against loopback fakes of Home
//! Assistant, Frigate and go2rtc. Secrets come from `CAPTUREFAB_HUB_*`
//! variables, so the OS keychain is never touched; every secret contains
//! `CANARY`, and no output or session file may.
#![cfg(feature = "hubs")]

#[path = "support/fake_http.rs"]
mod fake_http;
#[path = "support/fake_hub.rs"]
mod fake_hub;

use fake_hub::{Camera, HomeAssistant};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TOKEN: &str = "CANARY-hub-cli-token";

fn temp_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "capturefab-hubs-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn command(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_CAMERA")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", dir.join("cache"));
    for (name, _) in std::env::vars() {
        if name.starts_with("CAPTUREFAB_HUB_") {
            command.env_remove(name);
        }
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command.args(args);
    command
}

/// Runs the binary and remembers its output for the leak scan.
struct Bench {
    dir: PathBuf,
    env: Vec<(String, String)>,
    outputs: Vec<u8>,
}

impl Bench {
    fn new(name: &str) -> Self {
        Self {
            dir: temp_root(name),
            env: vec![("CAPTUREFAB_HUB_HOME_TOKEN".into(), TOKEN.into())],
            outputs: Vec::new(),
        }
    }
    fn run(&mut self, args: &[&str]) -> Output {
        let env = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect::<Vec<_>>();
        let output = command(&self.dir, &env, args).output().unwrap();
        self.outputs.extend(&output.stdout);
        self.outputs.extend(&output.stderr);
        output
    }
    fn json(&mut self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend(args);
        let output = self.run(&all);
        assert!(
            output.status.success(),
            "{args:?}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone()
    }
    /// The exit code and JSON error code of a failing command.
    fn fails(&mut self, args: &[&str]) -> (i32, String, String) {
        let mut all = vec!["--json"];
        all.extend(args);
        let output = self.run(&all);
        assert!(!output.status.success(), "{args:?} succeeded");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
        (
            output.status.code().unwrap_or(-1),
            value["error"]["code"].as_str().unwrap_or("").into(),
            value["error"]["message"].as_str().unwrap_or("").into(),
        )
    }
    fn add_home(&mut self, url: &str) -> Value {
        self.json(&[
            "hub",
            "add-home-assistant",
            "home",
            "--url",
            url,
            "--secret-env",
            "CAPTUREFAB_HUB_HOME_TOKEN",
        ])
    }
    /// Nothing the binary printed, and no file it wrote, holds a secret.
    fn assert_no_canary(&self) {
        let text = String::from_utf8_lossy(&self.outputs);
        assert!(!text.contains("CANARY"), "a secret was printed:\n{text}");
        fn scan(path: &Path) {
            for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
                let path = entry.path();
                assert!(
                    !path.to_string_lossy().contains("CANARY"),
                    "{}",
                    path.display()
                );
                if path.is_dir() {
                    scan(&path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    assert!(
                        !String::from_utf8_lossy(&bytes).contains("CANARY"),
                        "a secret was stored in {}",
                        path.display()
                    );
                }
            }
        }
        scan(&self.dir);
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn jpeg(width: u32, height: u32, value: u8) -> Vec<u8> {
    let frame = capturefab::types::Frame {
        id: 1,
        width,
        height,
        pixel_format: capturefab::types::RGB8,
        timestamp_ns: 0,
        data: vec![value; (width * height * 3) as usize],
    };
    capturefab::jpeg::software(&capturefab::jpeg::Raster::new(&frame).unwrap()).unwrap()
}

fn frames() -> Vec<Vec<u8>> {
    (0..4).map(|value| jpeg(64, 36, value * 60)).collect()
}

fn png_size(path: &Path) -> (u32, u32) {
    let decoder = png::Decoder::new(std::fs::File::open(path).unwrap());
    let reader = decoder.read_info().unwrap();
    (reader.info().width, reader.info().height)
}

#[test]
fn saved_hubs_never_hold_secrets() {
    let mut bench = Bench::new("lifecycle");
    let saved = bench.json(&[
        "hub",
        "add-home-assistant",
        "home",
        "--url",
        "http://homeassistant.invalid:8123",
        "--secret-env",
        "CAPTUREFAB_HUB_HOME_TOKEN",
        "--no-check",
    ]);
    assert_eq!(saved["hub"]["kind"], "home_assistant");
    assert_eq!(saved["hub"]["secret"], "environment");
    assert_eq!(saved["hub"]["insecure"], true);
    assert!(saved.get("check").is_none());
    let listed = bench.json(&["hub", "list"]);
    assert_eq!(listed["hubs"][0]["name"], "home");
    assert_eq!(listed["hubs"][0]["secret_env"], "CAPTUREFAB_HUB_HOME_TOKEN");
    let file = bench.dir.join("hubs.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let removed = bench.json(&["hub", "remove", "home"]);
    assert_eq!(removed["removed"], "home");
    assert!(
        bench.json(&["hub", "list"])["hubs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Hub commands ignore a stale camera selector.
    let output = command(
        &bench.dir,
        &[("CAPTUREFAB_CAMERA", "hub:missing/x")],
        &["hub", "list"],
    )
    .output()
    .unwrap();
    assert!(output.status.success());
    bench.assert_no_canary();
}

#[test]
fn the_schema_and_doctor_describe_hubs() {
    let mut bench = Bench::new("schema");
    let schema = bench.json(&["schema"]);
    let text = schema.to_string();
    for needle in [
        "add-home-assistant",
        "add-frigate",
        "add-go2rtc",
        "add-unifi-protect",
        "secret_env",
        "accept_certificate",
        "tls_server_name",
        "allow_hubs",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
    assert_eq!(schema["hubs"]["compiled"], true);
    assert!(
        schema["selection"]["direct"]
            .as_str()
            .unwrap()
            .contains("hub:NAME/CAMERA")
    );
    let doctor = bench.json(&["doctor"]);
    assert_eq!(doctor["features"]["hubs"], true);
}

#[test]
fn bad_input_is_refused_before_anything_is_saved() {
    let mut bench = Bench::new("usage");
    for args in [
        &[
            "hub",
            "add-home-assistant",
            "home",
            "--url",
            "http://u:CANARY@ha:8123",
            "--secret-env",
            "CAPTUREFAB_HUB_HOME_TOKEN",
        ][..],
        &[
            "hub",
            "add-home-assistant",
            ".home",
            "--url",
            "http://ha:8123",
            "--secret-env",
            "CAPTUREFAB_HUB_HOME_TOKEN",
        ],
        &[
            "hub",
            "add-home-assistant",
            "home",
            "--url",
            "http://ha:8123",
            "--secret-env",
            "CANARY-not-a-variable",
        ],
        &[
            "hub",
            "add-home-assistant",
            "home",
            "--url",
            "http://ha:8123",
            "--token",
            "CANARY",
        ],
        &[
            "hub",
            "add-frigate",
            "nvr",
            "--url",
            "http://nvr:5000",
            "--user",
            "admin",
            "--secret-env",
            "CAPTUREFAB_HUB_NVR",
        ],
        &[
            "hub",
            "add-unifi-protect",
            "p",
            "--url",
            "http://console",
            "--secret-env",
            "CAPTUREFAB_HUB_P",
        ],
        &[
            "hub",
            "add-home-assistant",
            "home",
            "--url",
            "https://ha",
            "--secret-env",
            "CAPTUREFAB_HUB_HOME_TOKEN",
            "--certificate",
            "SHA256:00",
        ],
    ] {
        let (exit, code, message) = bench.fails(args);
        assert_eq!((exit, code.as_str()), (2, "usage"), "{args:?}: {message}");
    }
    assert!(!bench.dir.join("hubs.json").exists());
    // Selectors are checked against the saved hubs before any connection.
    let (exit, _, message) = bench.fails(&["--camera", "hub:nohub/camera.x", "info"]);
    assert_eq!(exit, 3, "{message}");
    bench.json(&[
        "hub",
        "add-home-assistant",
        "home",
        "--url",
        "http://ha.invalid:8123",
        "--secret-env",
        "CAPTUREFAB_HUB_HOME_TOKEN",
        "--no-check",
    ]);
    for (selector, exit) in [
        ("hub:home/camera.x?stream=rtsp", 2),
        ("hub:home/light.kitchen", 2),
        ("hub:home/camera.x?fps=1", 2),
        ("hub:home/camera%0Ax", 2),
    ] {
        let (code, _, message) = bench.fails(&["--camera", selector, "info"]);
        assert_eq!(code, exit, "{selector}: {message}");
    }
    bench.assert_no_canary();
}

#[test]
fn adding_checks_the_server_and_its_token() {
    let fake = HomeAssistant::new(TOKEN, frames()).start();
    let mut bench = Bench::new("check");
    let added = bench.add_home(&fake.url());
    assert_eq!(added["check"]["product"], "Home Assistant");
    assert_eq!(added["check"]["version"], "2026.10.2");
    assert_eq!(added["check"]["cameras"], 2);
    assert_eq!(added["replaced"], false);
    let test = bench.json(&["hub", "test", "home"]);
    assert_eq!(
        (test["ok"].clone(), test["name"].clone()),
        (Value::Bool(true), "Test Home".into())
    );
    // A wrong token: one refused request, exit 1, recorded so discovery stops.
    let before = fake.requests().len();
    bench.env = vec![(
        "CAPTUREFAB_HUB_HOME_TOKEN".into(),
        "CANARY-wrong-token".into(),
    )];
    let (exit, code, message) = bench.fails(&["hub", "test", "home"]);
    assert_eq!((exit, code.as_str()), (1, "operation_failed"), "{message}");
    assert!(
        message.contains("rejected the access token (HTTP 401)"),
        "{message}"
    );
    assert_eq!(fake.requests().len(), before + 1);
    let listed = bench.json(&["hub", "list"]);
    assert_eq!(listed["hubs"][0]["rejected"]["status"], 401);
    let found = bench.json(&["discover", "--timeout-ms", "300"]);
    assert!(
        found["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("skipped by discovery")),
        "{found}"
    );
    assert_eq!(fake.requests().len(), before + 1, "discovery sent nothing");
    // An unreachable server is unavailable.
    bench.env = vec![("CAPTUREFAB_HUB_HOME_TOKEN".into(), TOKEN.into())];
    let refused = format!("http://127.0.0.1:{}", fake_http::refused_port());
    let (exit, _, message) = bench.fails(&[
        "hub",
        "add-home-assistant",
        "away",
        "--url",
        &refused,
        "--secret-env",
        "CAPTUREFAB_HUB_HOME_TOKEN",
    ]);
    assert_eq!(exit, 3, "{message}");
    assert!(message.contains("save with --no-check"), "{message}");
    bench.assert_no_canary();
}

#[test]
fn cameras_are_listed_and_discovered() {
    let fake = HomeAssistant::new(TOKEN, frames()).start();
    let mut bench = Bench::new("cameras");
    bench.add_home(&fake.url());
    let listed = bench.json(&["hub", "cameras", "home"]);
    let cameras = listed["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 2);
    assert_eq!(cameras[0]["id"], "hub:home/camera.front_door");
    assert_eq!(cameras[0]["name"], "Front door");
    assert_eq!(cameras[0]["kind"], "home_assistant");
    assert_eq!(
        cameras[0]["streams"],
        serde_json::json!(["mjpeg", "snapshot"])
    );
    let found = bench.json(&["discover", "--timeout-ms", "2000"]);
    let ids = found["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        ids.contains(&"hub:home/camera.front_door".to_owned()),
        "{ids:?}"
    );
    bench.assert_no_canary();
}

#[test]
fn hub_cameras_capture_from_mjpeg_and_snapshots() {
    let mut config = HomeAssistant::new(TOKEN, frames());
    config
        .cameras
        .push(Camera::new("camera.porch", "Porch").status(503));
    let fake = config.start();
    let mut bench = Bench::new("capture");
    bench.add_home(&fake.url());
    let out = bench.dir.join("frames");
    let pattern = out.join("f-{seq}.png").to_string_lossy().into_owned();
    let saved = bench.json(&[
        "--camera",
        "hub:home/camera.front_door",
        "capture",
        "-n",
        "3",
        "-o",
        &pattern,
    ]);
    assert_eq!(saved["count"], 3, "{saved}");
    let files = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 3);
    let mut bytes = files
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect::<Vec<_>>();
    bytes.dedup();
    assert_eq!(bytes.len(), 3, "the repeated first frame was dropped");
    assert_eq!(png_size(&files[0]), (64, 36));
    let snapshot = out.join("s-{seq}.png").to_string_lossy().into_owned();
    bench.json(&[
        "--camera",
        "hub:home/camera.garage?stream=snapshot&fps=2",
        "capture",
        "-n",
        "2",
        "-o",
        &snapshot,
    ]);
    assert!(out.join("s-0002.png").exists());
    // Stdout captures retrieve the forced full-size frame from the worker's
    // anonymous ring even though no live preview is watching it.
    let stdout = bench.run(&[
        "--camera",
        "hub:home/camera.garage?stream=snapshot&fps=2",
        "capture",
        "-n",
        "1",
        "-f",
        "png",
        "-o",
        "-",
    ]);
    assert!(stdout.status.success(), "{:?}", stdout.stderr);
    let image = png::Decoder::new(std::io::Cursor::new(&stdout.stdout))
        .read_info()
        .unwrap();
    assert_eq!((image.info().width, image.info().height), (64, 36));
    let (exit, _, message) = bench.fails(&["--camera", "hub:home/camera.attic", "info"]);
    assert_eq!(exit, 3, "{message}");
    assert!(message.contains("not found on hub home"), "{message}");
    let (exit, _, message) =
        bench.fails(&["--camera", "hub:home/camera.porch?stream=snapshot", "info"]);
    assert_eq!(exit, 3, "{message}");
    assert!(message.contains("is offline (HTTP 503)"), "{message}");
    for request in fake.requests() {
        assert!(request.bearer().is_some(), "{}", request.target);
        assert!(!request.target.contains("token="), "{}", request.target);
    }
    bench.assert_no_canary();
}

#[test]
fn pinned_certificates_are_shown_checked_and_enforced() {
    let mut config = HomeAssistant::new(TOKEN, frames());
    config.identity = Some(fake_http::localhost());
    let fake = config.start();
    let mut bench = Bench::new("tls");
    // The OS store does not trust the test certificate.
    let (exit, _, message) = bench.fails(&[
        "hub",
        "add-home-assistant",
        "home",
        "--url",
        &fake.url(),
        "--secret-env",
        "CAPTUREFAB_HUB_HOME_TOKEN",
    ]);
    assert_eq!(exit, 1, "{message}");
    assert!(message.contains(fake_http::LOCALHOST_SHA256), "{message}");
    assert!(
        fake.requests().is_empty(),
        "nothing is sent before the certificate is trusted"
    );
    let added = bench.json(&[
        "hub",
        "add-home-assistant",
        "home",
        "--url",
        &fake.url(),
        "--secret-env",
        "CAPTUREFAB_HUB_HOME_TOKEN",
        "--accept-certificate",
    ]);
    assert_eq!(
        added["certificate"]["fingerprint"],
        fake_http::LOCALHOST_SHA256
    );
    assert_eq!(added["certificate"]["trust"], "accepted_on_first_use");
    assert_eq!(bench.json(&["hub", "test", "home"])["ok"], true);
    // A different pinned certificate is refused before the token is sent.
    let before = fake.requests().len();
    let (exit, _, message) = bench.fails(&[
        "hub",
        "add-home-assistant",
        "home",
        "--url",
        &fake.url(),
        "--secret-env",
        "CAPTUREFAB_HUB_HOME_TOKEN",
        "--certificate",
        fake_http::OTHER_SHA256,
    ]);
    assert_eq!(exit, 1, "{message}");
    assert!(
        message.contains("presented a different certificate"),
        "{message}"
    );
    assert_eq!(fake.requests().len(), before);
    bench.assert_no_canary();
}

#[test]
fn sessions_reconnect_streams_and_stop_promptly() {
    let mut config = HomeAssistant::new(TOKEN, frames());
    // Each stream ends after a few parts; the camera keeps going.
    config.mjpeg = fake_hub::Mjpeg {
        parts: 4,
        delay: Duration::from_millis(50),
        stall: None,
    };
    let fake = config.start();
    let mut bench = Bench::new("session");
    bench.add_home(&fake.url());
    let env = bench
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect::<Vec<_>>();
    let mut serve: Child = command(&bench.dir, &env, &["--json", "serve", "--name", "bench"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(serve.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert!(ready.contains("\"ready\":true"), "{ready}");
    bench.json(&[
        "--session",
        "bench",
        "connect",
        "hub:home/camera.front_door",
    ]);
    bench.json(&["--session", "bench", "start"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    let frames = loop {
        let status = bench.json(&["--session", "bench", "status"]);
        let frames = status["frames"].as_u64().unwrap_or(0);
        if frames > 8 || Instant::now() > deadline {
            break frames;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        frames > 8,
        "frames kept coming over several streams: {frames}"
    );
    let streams = fake
        .requests()
        .iter()
        .filter(|r| r.path() == "/api/camera_proxy_stream/camera.front_door")
        .count();
    assert!(streams >= 2, "the stream was reopened: {streams}");
    // Connecting again with other options does not replace the stream.
    let (exit, _, message) = bench.fails(&[
        "--session",
        "bench",
        "connect",
        "hub:home/camera.front_door?stream=snapshot",
    ]);
    assert_eq!(exit, 3, "{message}");
    assert!(message.contains("already connected"), "{message}");
    let start = Instant::now();
    bench.json(&["--session", "bench", "stop"]);
    bench.json(&["--session", "bench", "disconnect"]);
    assert!(start.elapsed() < Duration::from_secs(5));
    let _ = serve.kill();
    let _ = serve.wait();
    bench.assert_no_canary();
}

#[test]
#[ignore = "requires FFmpeg with libx264; set CAPTUREFAB_FFMPEG to the executable"]
fn hub_restreams_play_through_ffmpeg() {
    // Capturefab's own RTSP simulator stands in for go2rtc's restream.
    let port = fake_http::refused_port();
    let mut bench = Bench::new("restream");
    let mut simulator = command(
        &bench.dir,
        &[],
        &[
            "--json",
            "simulate",
            "run",
            "--preset",
            "rtsp",
            "--rtsp-port",
            &port.to_string(),
            "--width",
            "320",
            "--height",
            "240",
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let mut ready = String::new();
    BufReader::new(simulator.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let mut config = fake_hub::Go2rtc::new(frames());
    config.streams = vec!["live".into()];
    config.rtsp_listen = format!(":{port}");
    let fake = config.start();
    bench.json(&["hub", "add-go2rtc", "g", "--url", &fake.url()]);
    let out = bench
        .dir
        .join("restream.png")
        .to_string_lossy()
        .into_owned();
    bench.json(&["--camera", "hub:g/live", "capture", "-n", "1", "-o", &out]);
    let _ = simulator.kill();
    let _ = simulator.wait();
    // The restream, not go2rtc's 64x36 snapshots, delivered the frames.
    assert_eq!(png_size(Path::new(&out)), (320, 240));
    assert!(
        fake.requests()
            .iter()
            .all(|r| r.path() != "/api/frame.jpeg")
    );
    bench.assert_no_canary();
}
