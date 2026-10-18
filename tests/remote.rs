//! Exercise the HTTP bridge through the public binary and real simulator workers.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const INDEX: &str = "<!doctype html><title>Capturefab remote fixture</title>";
const WASM: &[u8] = b"\0asm\x01\0\0\0";

struct Fixture {
    child: Option<Child>,
    root: PathBuf,
    address: SocketAddr,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn root(name: &str) -> PathBuf {
    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "capturefab-remote-{name}-{}-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
fn assets(root: &Path) {
    fs::create_dir_all(root.join("web")).unwrap();
    fs::write(root.join("web/index.html"), INDEX).unwrap();
    fs::write(root.join("web/capturefab_web_bg.wasm"), WASM).unwrap();
}
impl Fixture {
    fn start() -> Self {
        let root = root("session");
        assets(&root);
        Self::serve(
            root.clone(),
            Path::new(env!("CARGO_BIN_EXE_capturefab")),
            &root,
            &["--web-dir", "web"],
        )
    }
    /// `serve --listen` from `executable` in `cwd`, with sessions under `root`.
    fn serve(root: PathBuf, executable: &Path, cwd: &Path, args: &[&str]) -> Self {
        let child = Command::new(executable)
            .current_dir(cwd)
            .env("CAPTUREFAB_SESSION_DIR", root.join("sessions"))
            .env_remove("CAPTUREFAB_SESSION")
            .env("CAPTUREFAB_FRAME_BYTES", "1048576")
            .args([
                "--json",
                "serve",
                "--name",
                "remote-ci",
                "--listen",
                "127.0.0.1:0",
            ])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            child: Some(child),
            root,
            address: "127.0.0.1:0".parse().unwrap(),
        };
        let stdout = fixture.child.as_mut().unwrap().stdout.take().unwrap();
        let (ready, response) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = ready.send(result);
        });
        let line = response
            .recv_timeout(Duration::from_secs(10))
            .expect("remote server did not report readiness")
            .unwrap();
        let response: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("invalid server readiness {line:?}: {error}"));
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["result"]["ready"], true, "{response}");
        let url = response["result"]["remote_url"]
            .as_str()
            .expect("remote URL missing from readiness");
        fixture.address = url
            .strip_prefix("http://")
            .unwrap()
            .trim_end_matches('/')
            .parse()
            .unwrap();
        assert_ne!(fixture.address.port(), 0);
        assert!(!fixture.root.join("token").exists());
        fixture
    }
    fn http(&self, method: &str, path: &str, origin: Option<&str>, body: &[u8]) -> Response {
        let origin = origin.map(|origin| ("Origin", origin));
        self.http_with(method, path, origin.as_slice(), body)
    }
    fn http_with(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Response {
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.address,
            body.len()
        )
        .unwrap();
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        if method == "POST" {
            write!(stream, "Content-Type: application/json\r\n").unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        stream.write_all(body).unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let boundary = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTTP response missing headers");
        let headers = String::from_utf8(bytes[..boundary].to_vec()).unwrap();
        let status = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        Response {
            status,
            headers,
            body: bytes[boundary + 4..].to_vec(),
        }
    }
    fn command(&self, camera: Option<&str>, command: Value) -> Value {
        self.command_as(camera, command, None)
    }
    fn command_as(&self, camera: Option<&str>, command: Value, actor: Option<Value>) -> Value {
        let mut request = json!({"version":1,"command":command});
        if let Some(camera) = camera {
            request["camera"] = camera.into();
        }
        if let Some(actor) = actor {
            request["actor"] = actor;
        }
        self.http(
            "POST",
            "/api/command",
            Some(&format!("http://{}", self.address)),
            &serde_json::to_vec(&request).unwrap(),
        )
        .result()
    }
    fn cli(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .current_dir(&self.root)
            .env("CAPTUREFAB_SESSION_DIR", self.root.join("sessions"))
            .env_remove("CAPTUREFAB_SESSION")
            .args(["--json", "--session", "remote-ci"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], true, "{response}");
        response["result"].clone()
    }
}

#[test]
fn multiplayer_capture_broadcasts_actors_and_queued_work_without_blocking_observers() {
    let fixture = Fixture::start();
    fixture.command(
        None,
        json!({"op":"connect","camera":"sim:0","timeout_ms":2000}),
    );
    for (feature, value) in [
        ("Width", "64"),
        ("Height", "64"),
        ("AcquisitionFrameRate", "2"),
    ] {
        fixture.command(
            Some("sim:0"),
            json!({"op":"set","feature":feature,"value":value}),
        );
    }
    let initial = fixture.http("GET", "/api/activity", None, b"").result();
    let revision = initial["revision"].as_u64().unwrap();
    let local_revision = revision.to_string();
    let watch = format!("/api/activity?after={revision}&timeout_ms=3000");
    std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.http("GET", &watch, None, b"").result());
        let second = scope.spawn(|| fixture.http("GET", &watch, None, b"").result());
        let local =
            scope.spawn(|| fixture.cli(&["activity", "--after", &local_revision, "--wait", "3s"]));
        let agent = scope.spawn(|| fixture.command_as(
            Some("sim:0"),
            json!({"op":"capture","output":"agent-frames","count":8,"timeout_ms":2000,"format":"png"}),
            Some(json!({"id":"focus-agent","name":"Focus agent","kind":"agent"})),
        ));
        for observer in [first, second] {
            let update = observer.join().unwrap();
            assert!(update["revision"].as_u64().unwrap() > revision);
            assert!(
                update["activity"].as_array().unwrap().iter().any(|entry| {
                    entry["actor"]["id"] == "focus-agent"
                        && matches!(entry["state"].as_str(), Some("queued" | "running"))
                }),
                "observers must see an actor while its capture is still active: {update}"
            );
        }
        // Submit the second actor over the already-ready HTTP endpoint.
        // Starting a new CLI process here can outlast the first capture on a
        // loaded runner; the private IPC observer is joined after queuing.
        let human = scope.spawn(|| {
            fixture.command_as(
            Some("sim:0"),
            json!({"op":"capture","output":"human.png","count":1,"timeout_ms":2000,"format":"png"}),
            Some(json!({"id":"camera-operator","name":"Camera operator","kind":"human"})),
        )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let update = fixture.http("GET", "/api/activity", None, b"").result();
            if update["activity"].as_array().unwrap().iter().any(|entry| {
                entry["actor"]["name"] == "Camera operator" && entry["state"] == "queued"
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "human request was not visible while queued: {update}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        // Measure command responsiveness, rather than CLI process startup.
        let status = fixture.command(Some("sim:0"), json!({"op":"status"}));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "targeted status waited behind capture"
        );
        assert!(
            status["activity"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["actor"]["id"] == "focus-agent")
        );
        let local_update = local.join().unwrap();
        assert!(local_update["revision"].as_u64().unwrap() > revision);
        assert!(
            local_update["activity"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| {
                    entry["actor"]["id"] == "focus-agent" && entry["operation"] == "capture"
                }),
            "private IPC observer did not receive shared work: {local_update}"
        );
        agent.join().unwrap();
        human.join().unwrap();
    });
    assert_eq!(
        fs::read_dir(fixture.root.join("agent-frames"))
            .unwrap()
            .count(),
        8
    );
    assert!(
        fs::read(fixture.root.join("human.png"))
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    let status = fixture.http("GET", "/api/session", None, b"").result();
    for name in ["Focus agent", "Camera operator"] {
        assert!(
            status["activity"].as_array().unwrap().iter().any(|entry| {
                entry["actor"]["name"] == name
                    && entry["operation"] == "capture"
                    && entry["state"] == "succeeded"
            }),
            "completed capture attribution missing: {status}"
        );
    }
    assert_eq!(status["cameras"].as_array().unwrap().len(), 1);
}

#[test]
fn multiplayer_overlapping_schedules_keep_actor_and_job_progress_visible() {
    let fixture = Fixture::start();
    fixture.command(
        None,
        json!({"op":"connect","camera":"sim:0","timeout_ms":2000}),
    );
    for (feature, value) in [("Width", "64"), ("Height", "64")] {
        fixture.command(
            Some("sim:0"),
            json!({"op":"set","feature":feature,"value":value}),
        );
    }
    let first_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 500;
    std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            fixture.command_as(
                Some("sim:0"),
                json!({
                    "op":"schedule","output":"alice","count":3,"timeout_ms":2000,"format":"png",
                    "first_at_ms":first_at,"interval_ms":100
                }),
                Some(json!({"id":"alice","name":"Alice","kind":"human"})),
            )
        });
        let b = scope.spawn(|| {
            fixture.command_as(
                Some("sim:0"),
                json!({
                    "op":"schedule","output":"agent","count":3,"timeout_ms":2000,"format":"png",
                    "first_at_ms":first_at,"interval_ms":100
                }),
                Some(json!({"id":"agent","name":"Capture agent","kind":"agent"})),
            )
        });
        assert_ne!(
            a.join().unwrap()["job"]["id"],
            b.join().unwrap()["job"]["id"]
        );
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut update = fixture.http("GET", "/api/activity", None, b"").result();
    loop {
        let jobs: Vec<_> = update["activity"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["operation"] == "schedule")
            .collect();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|entry| entry["actor"]["id"] == "alice"));
        assert!(jobs.iter().any(|entry| entry["actor"]["id"] == "agent"));
        if jobs
            .iter()
            .all(|entry| entry["job"]["status"] == "complete")
        {
            assert!(jobs.iter().all(|entry| entry["job"]["captured"] == 3));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shared job progress never completed: {update}"
        );
        let revision = update["revision"].as_u64().unwrap();
        update = fixture
            .http(
                "GET",
                &format!("/api/activity?after={revision}&timeout_ms=500"),
                None,
                b"",
            )
            .result();
    }
    for folder in ["alice", "agent"] {
        assert_eq!(fs::read_dir(fixture.root.join(folder)).unwrap().count(), 3);
    }
}

#[test]
fn multiplayer_observation_and_invalid_identity_do_not_create_activity() {
    let fixture = Fixture::start();
    let initial = fixture.http("GET", "/api/activity", None, b"").result();
    assert_eq!(initial["revision"], 0);
    for actor in [
        json!({"id":"bad id","name":"Operator","kind":"human"}),
        json!({"id":"agent","name":"","kind":"agent"}),
        json!({"id":"agent","name":"bad\nname","kind":"agent"}),
        json!({"id":"agent","name":"Agent","kind":"unknown"}),
    ] {
        let request = json!({"version":1,"actor":actor,"command":{"op":"connect","camera":"sim:0","timeout_ms":2000}});
        assert_eq!(
            fixture
                .http(
                    "POST",
                    "/api/command",
                    None,
                    &serde_json::to_vec(&request).unwrap()
                )
                .status,
            400
        );
    }
    assert_eq!(
        fixture
            .http("GET", "/api/activity?after=0&timeout_ms=0", None, b"")
            .result(),
        initial
    );
    let before = Instant::now();
    assert_eq!(
        fixture
            .http("GET", "/api/activity?after=999&timeout_ms=25000", None, b"")
            .result(),
        initial
    );
    assert!(before.elapsed() < Duration::from_secs(1));
    assert_eq!(fixture.cli(&["activity"]), initial);
    let denied = fixture.http(
        "GET",
        "/api/activity",
        Some("https://untrusted.example"),
        b"",
    );
    assert_eq!(denied.status, 403);
    assert_eq!(fixture.http("POST", "/api/activity", None, b"").status, 405);
    for query in [
        "after=0&after=1",
        "timeout_ms=25001",
        "after=-1",
        "invalid=0",
    ] {
        assert_eq!(
            fixture
                .http("GET", &format!("/api/activity?{query}"), None, b"")
                .status,
            400
        );
    }
    let current = fixture.http("GET", "/api/session", None, b"").result();
    assert_eq!(current["activity_revision"], 0);
    assert!(current["activity"].as_array().unwrap().is_empty());
    assert!(current["cameras"].as_array().unwrap().is_empty());
}
struct Response {
    status: u16,
    headers: String,
    body: Vec<u8>,
}
impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then_some(value.trim())
        })
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "invalid JSON HTTP {}: {}: {error}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }
    fn result(&self) -> Value {
        assert_eq!(self.status, 200, "{}", String::from_utf8_lossy(&self.body));
        let response = self.json();
        assert_eq!(response["version"], 1, "{response}");
        assert_eq!(response["ok"], true, "{response}");
        response["result"].clone()
    }
}

fn metric(text: &str, name: &str) -> f64 {
    text.lines()
        .find_map(|line| {
            line.strip_prefix(name)
                .and_then(|value| value.strip_prefix(' '))
                .map(|value| value.parse::<f64>().unwrap())
        })
        .unwrap_or_else(|| panic!("metric {name} missing from {text}"))
}

#[test]
fn monitoring_empty_session_methods_and_browser_origins() {
    let fixture = Fixture::start();
    let health = fixture.http("GET", "/health", None, b"");
    assert_eq!(health.status, 200);
    assert_eq!(
        health.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    let health = health.json();
    assert_eq!(health["ready"], true);
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert!(health["uptime_seconds"].as_f64().unwrap() >= 0.0);
    assert_eq!(health["cameras"]["connected"], 0);
    assert_eq!(health["cameras"]["streaming"], 0);
    assert_eq!(health["analytics"]["ready"], true);
    assert_eq!(health["analytics"]["retention_seconds"], 86_400);

    let report = fixture.http("GET", "/api/analytics", None, b"").result();
    assert_eq!(report["retention_seconds"], 86_400);
    assert_eq!(report["totals"]["frames"], 0);
    assert_eq!(report["totals"]["dropped"], 0);
    assert_eq!(report["totals"]["bytes"], 0);
    assert!(report["cameras"].as_array().unwrap().is_empty());
    let dashboard = fixture.http("GET", "/api/analytics.svg", None, b"");
    assert_eq!(dashboard.status, 200);
    assert_eq!(
        dashboard.header("content-type"),
        Some("image/svg+xml; charset=utf-8")
    );
    let svg = std::str::from_utf8(&dashboard.body).unwrap();
    assert_eq!(
        roxmltree::Document::parse(svg)
            .unwrap()
            .root_element()
            .tag_name()
            .name(),
        "svg"
    );
    assert!(svg.contains("Ready for your first frame"), "{svg}");

    let metrics = fixture.http("GET", "/metrics", None, b"");
    assert_eq!(metrics.status, 200);
    assert_eq!(
        metrics.header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let text = std::str::from_utf8(&metrics.body).unwrap();
    assert_eq!(metric(text, "capturefab_analytics_ready"), 1.0);
    assert_eq!(metric(text, "capturefab_session_frames_total"), 0.0);
    assert_eq!(metric(text, "capturefab_session_dropped_frames_total"), 0.0);
    assert_eq!(metric(text, "capturefab_session_received_bytes_total"), 0.0);
    assert!(text.contains("# TYPE capturefab_session_frames_total counter"));

    for path in [
        "/health",
        "/metrics",
        "/api/analytics",
        "/api/analytics.svg",
    ] {
        let head = fixture.http("HEAD", path, None, b"");
        assert_eq!(head.status, 200, "{path}");
        assert!(head.body.is_empty(), "HEAD included a body: {path}");
        assert!(
            head.header("content-length")
                .unwrap()
                .parse::<usize>()
                .unwrap()
                > 0
        );
        let post = fixture.http("POST", path, None, b"");
        assert_eq!(post.status, 405, "{path}");
        assert_eq!(post.header("allow"), Some("GET, HEAD"), "{path}");
    }
    for path in ["/api/analytics", "/api/analytics.svg"] {
        for method in ["GET", "HEAD"] {
            let hostile = fixture.http(method, path, Some("https://untrusted.example"), b"");
            assert_eq!(hostile.status, 403, "{method} {path}");
        }
        assert_eq!(
            fixture
                .http(
                    "GET",
                    path,
                    Some(&format!("http://{}", fixture.address)),
                    b""
                )
                .status,
            200,
            "same-origin request rejected: {path}"
        );
    }
}

#[test]
fn analytics_collects_without_scraping_and_charts_current_capture_session() {
    let fixture = Fixture::start();
    fixture.command(
        None,
        json!({"op":"connect","camera":"sim:0","timeout_ms":2000}),
    );
    for (feature, value) in [
        ("Width", "128"),
        ("Height", "96"),
        ("AcquisitionFrameRate", "30"),
    ] {
        fixture.command(
            Some("sim:0"),
            json!({"op":"set","feature":feature,"value":value}),
        );
    }
    // A command error remains available in camera diagnostics; it does not
    // make a healthy session or its analytics collector unavailable.
    let failed = fixture.http(
        "POST",
        "/api/command",
        None,
        br#"{"version":1,"camera":"sim:0","command":{"op":"get","feature":"NoSuchFeature"}}"#,
    );
    assert_eq!(failed.status, 409);
    assert_eq!(fixture.http("GET", "/health", None, b"").status, 200);
    fixture.command(Some("sim:0"), json!({"op":"start"}));
    // No HTTP requests or IPC commands run while acquisition is active.
    // The session's own collector must record this interval independently.
    std::thread::sleep(Duration::from_millis(2_200));
    fixture.command(Some("sim:0"), json!({"op":"stop"}));
    std::thread::sleep(Duration::from_millis(1_100));
    let report = fixture.http("GET", "/api/analytics", None, b"").result();
    assert!(report["samples"].as_u64().unwrap() >= 2, "{report}");
    assert!(report["totals"]["frames"].as_u64().unwrap() > 0, "{report}");
    assert!(report["totals"]["bytes"].as_u64().unwrap() > 0, "{report}");
    // Mono8 payloads are counted as received, independent of PNG preview
    // encoding or any subsequent camera configuration.
    assert_eq!(
        report["totals"]["bytes"].as_u64().unwrap(),
        report["totals"]["frames"].as_u64().unwrap() * 128 * 96,
        "received bytes must match the actual acquired payloads: {report}"
    );
    let started = report["started_at_ms"].as_u64().unwrap();
    let collected = report["collected_at_ms"].as_u64().unwrap();
    assert!(collected >= started + 2_000, "{report}");
    let cameras = report["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 1, "{report}");
    assert_eq!(cameras[0]["id"], "sim:0");
    assert_eq!(cameras[0]["streaming"], false);
    assert_eq!(cameras[0]["frames"], report["totals"]["frames"]);
    let points = report["points"].as_array().unwrap();
    assert!(points.len() >= 2, "{report}");
    assert!(
        points
            .iter()
            .any(|point| point["fps"].as_f64().unwrap() > 0.0),
        "{report}"
    );
    for point in points {
        let timestamp = point["timestamp_ms"].as_u64().unwrap();
        assert!(timestamp >= started && timestamp <= collected, "{point}");
        assert!(collected.saturating_sub(timestamp) <= 86_400_000, "{point}");
    }

    let metrics = fixture.http("GET", "/metrics", None, b"");
    assert_eq!(metrics.status, 200);
    let metrics = std::str::from_utf8(&metrics.body).unwrap();
    for (name, field) in [
        ("capturefab_session_frames_total", "frames"),
        ("capturefab_session_dropped_frames_total", "dropped"),
        ("capturefab_session_received_bytes_total", "bytes"),
    ] {
        assert_eq!(
            metric(metrics, name),
            report["totals"][field].as_u64().unwrap() as f64,
            "{name}"
        );
    }
    assert!(
        metrics.contains("capturefab_camera_frames_total{camera_id=\"sim:0\""),
        "{metrics}"
    );
    let health = fixture.http("GET", "/health", None, b"").json();
    assert_eq!(health["ready"], true);
    assert_eq!(health["cameras"]["connected"], 1);
    assert_eq!(health["cameras"]["streaming"], 0);

    let dashboard = fixture.http("GET", "/api/analytics.svg", None, b"");
    assert_eq!(dashboard.status, 200);
    let svg = std::str::from_utf8(&dashboard.body).unwrap();
    let document = roxmltree::Document::parse(svg).unwrap();
    assert_eq!(document.root_element().tag_name().name(), "svg");
    for caption in [
        "Frame rate",
        "Throughput",
        "Missed frame rate",
        "Camera data volume",
    ] {
        assert!(svg.contains(caption), "dashboard lacks {caption}: {svg}");
    }
    assert!(
        document.descendants().any(|node| {
            node.has_tag_name("polyline")
                && node
                    .attribute("points")
                    .is_some_and(|points| points.split_whitespace().count() >= 2)
        }),
        "dashboard lacks plotted capture history: {svg}"
    );

    // A newly launched capture session cannot inherit the previous process's
    // totals or in-memory history.
    let next_session = Fixture::start();
    let next = next_session
        .http("GET", "/api/analytics", None, b"")
        .result();
    assert_eq!(next["totals"]["frames"], 0);
    assert!(next["cameras"].as_array().unwrap().is_empty());
}

#[test]
fn analytics_keeps_short_capture_and_same_camera_reconnect_totals() {
    let fixture = Fixture::start();
    let connect_small_camera = || {
        fixture.command(
            None,
            json!({"op":"connect","camera":"sim:burst","timeout_ms":2000}),
        );
        for feature in ["Width", "Height"] {
            fixture.command(
                Some("sim:burst"),
                json!({"op":"set","feature":feature,"value":"64"}),
            );
        }
    };
    connect_small_camera();
    fixture.command(Some("sim:burst"), json!({"op":"start"}));
    // This acquisition can begin and end between the 250 ms worker polls,
    // and is shorter than the 1 s history interval. Disconnect must retain
    // its final counters before dropping the worker's camera identity.
    std::thread::sleep(Duration::from_millis(80));
    fixture.command(Some("sim:burst"), json!({"op":"disconnect"}));
    let first = fixture.http("GET", "/api/analytics", None, b"").result();
    let first_frames = first["totals"]["frames"].as_u64().unwrap();
    let first_bytes = first["totals"]["bytes"].as_u64().unwrap();
    assert!(first_frames > 0, "short acquisition vanished: {first}");
    assert_eq!(first_bytes, first_frames * 64 * 64, "{first}");
    assert_eq!(
        fixture.http("GET", "/health", None, b"").json()["cameras"]["connected"],
        0
    );

    connect_small_camera();
    fixture.command(
        Some("sim:burst"),
        json!({"op":"capture","output":"reconnected.png","count":1,"timeout_ms":2000,"format":"png"}),
    );
    fixture.command(Some("sim:burst"), json!({"op":"disconnect"}));
    let next = fixture.http("GET", "/api/analytics", None, b"").result();
    assert_eq!(next["totals"]["frames"], first_frames + 1, "{next}");
    assert_eq!(next["totals"]["bytes"], first_bytes + 64 * 64, "{next}");
    let metrics = fixture.http("GET", "/metrics", None, b"");
    let text = std::str::from_utf8(&metrics.body).unwrap();
    assert_eq!(
        metric(text, "capturefab_session_frames_total"),
        (first_frames + 1) as f64
    );
    assert_eq!(
        metric(text, "capturefab_session_received_bytes_total"),
        (first_bytes + 64 * 64) as f64
    );
}

#[test]
fn remote_requires_web_assets_before_creating_session_resources() {
    let root = root("validation");
    assets(&root);
    let _cleanup = Fixture {
        child: None,
        root: root.clone(),
        address: "127.0.0.1:0".parse().unwrap(),
    };
    for missing in ["capturefab_web_bg.wasm", "index.html"] {
        fs::remove_file(root.join("web").join(missing)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_capturefab"))
            .current_dir(&root)
            .env("CAPTUREFAB_SESSION_DIR", root.join("sessions"))
            .env_remove("CAPTUREFAB_SESSION")
            .args([
                "--json",
                "serve",
                "--name",
                "invalid-remote",
                "--listen",
                "127.0.0.1:0",
                "--web-dir",
                "web",
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], false);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(missing)
        );
        assert!(!root.join("sessions").exists());
        assets(&root);
    }
}

#[test]
fn remote_network_access_and_wasm_assets() {
    let fixture = Fixture::start();
    let index = fixture.http("GET", "/", None, b"");
    assert_eq!(index.status, 200);
    assert_eq!(index.body, INDEX.as_bytes());
    let wasm = fixture.http("GET", "/capturefab_web_bg.wasm", None, b"");
    assert_eq!(wasm.status, 200);
    assert_eq!(wasm.body, WASM);
    assert!(
        wasm.headers
            .to_lowercase()
            .contains("content-type: application/wasm")
    );
    // Exceed ordinary socket buffers to exercise response backpressure.
    let large_asset = vec![0xab; 2 * 1024 * 1024];
    fs::write(fixture.root.join("web/large.bin"), &large_asset).unwrap();
    let response = fixture.http("GET", "/large.bin", None, b"");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, large_asset);
    let hostile = fixture.http(
        "GET",
        "/api/session",
        Some("https://untrusted.example"),
        b"",
    );
    assert_eq!(hostile.status, 403);
    assert_eq!(hostile.json()["ok"], false);
    let hostile = fixture.http(
        "POST",
        "/api/command",
        Some("https://untrusted.example"),
        br#"{"version":1,"command":{"op":"connect","camera":"sim:0","timeout_ms":2000}}"#,
    );
    assert_eq!(hostile.status, 403);
    assert_eq!(hostile.json()["ok"], false);
    let state = fixture.http("GET", "/api/session", None, b"").result();
    assert!(state["cameras"].as_array().unwrap().is_empty());
    let destinations = fixture.http("GET", "/api/destinations", None, b"").result();
    assert!(destinations["destinations"].is_array());
    assert!(
        fixture
            .http("GET", "/api/storage", None, b"")
            .result()
            .is_object()
    );
    let outside = "outside web root";
    fs::write(fixture.root.join("private.txt"), outside).unwrap();
    for path in ["/../private.txt", "/%2e%2e/private.txt"] {
        let response = fixture.http("GET", path, None, b"");
        assert_ne!(response.status, 200, "static path escaped web root: {path}");
        assert!(!String::from_utf8_lossy(&response.body).contains(outside));
    }
    for body in [
        b"{".as_slice(),
        br#"{"version":2,"command":{"op":"status"}}"#.as_slice(),
        br#"{"version":1,"command":{"op":"status","ignored":true}}"#.as_slice(),
        br#"{"version":1,"command":{"op":"status","op":"start"}}"#.as_slice(),
    ] {
        let response = fixture.http("POST", "/api/command", None, body);
        assert_eq!(
            response.status,
            400,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        assert_eq!(response.json()["ok"], false);
    }
}

#[test]
fn previews_answer_a_frame_the_client_has_with_304_and_send_jpeg_on_request() {
    let fixture = Fixture::start();
    fixture.command(
        None,
        json!({"op":"connect","camera":"sim:0","timeout_ms":2000}),
    );
    for (feature, value) in [("Width", "64"), ("Height", "64")] {
        fixture.command(
            Some("sim:0"),
            json!({"op":"set","feature":feature,"value":value}),
        );
    }
    fixture.command(Some("sim:0"), json!({"op":"start"}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture
        .http("GET", "/api/preview?camera=sim%3A0", None, b"")
        .status
        != 200
    {
        assert!(Instant::now() < deadline, "preview did not receive a frame");
        std::thread::sleep(Duration::from_millis(50));
    }
    fixture.command(Some("sim:0"), json!({"op":"stop"}));
    for (query, content_type) in [
        ("camera=sim%3A0", "image/png"),
        (
            "camera=sim%3A0&format=jpeg",
            if cfg!(feature = "jpeg") {
                "image/jpeg"
            } else {
                "image/png"
            },
        ),
    ] {
        let path = format!("/api/preview?{query}");
        // A stopped stream ends on its newest frame, which the next polls keep.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (frame, unchanged) = loop {
            let frame = fixture.http("GET", &path, None, b"");
            assert_eq!(frame.status, 200);
            let etag = frame.header("ETag").unwrap().to_owned();
            let unchanged = fixture.http_with("GET", &path, &[("If-None-Match", &etag)], b"");
            if unchanged.status == 304 {
                break (frame, unchanged);
            }
            assert!(
                Instant::now() < deadline,
                "the stopped preview kept changing"
            );
        };
        assert_eq!(frame.header("Content-Type"), Some(content_type));
        assert_eq!(unchanged.header("ETag"), frame.header("ETag"));
        assert_eq!(unchanged.header("X-Frame-Id"), frame.header("X-Frame-Id"));
        assert_eq!(unchanged.header("Content-Length"), None);
        assert!(unchanged.body.is_empty());
        if content_type == "image/jpeg" {
            assert!(frame.body.starts_with(&[0xff, 0xd8]));
            assert!(frame.header("ETag").unwrap().ends_with("-jpeg\""));
        } else {
            assert!(frame.body.starts_with(b"\x89PNG\r\n\x1a\n"));
        }
    }
}

#[test]
fn serve_finds_the_browser_client_beside_its_executable_from_any_directory() {
    let root = root("beside");
    let bin = root.join("bin");
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let source = Path::new(env!("CARGO_BIN_EXE_capturefab"));
    let executable = bin.join(source.file_name().unwrap());
    if fs::hard_link(source, &executable).is_err() {
        fs::copy(source, &executable).unwrap();
    }
    assets(&bin);
    let embedded = Command::new(&executable)
        .env("CAPTUREFAB_SESSION_DIR", root.join("sessions"))
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    let embedded = serde_json::from_slice::<Value>(&embedded.stdout).unwrap()["result"]["features"]
        ["embedded_web"]
        .as_bool()
        .expect("doctor reports whether the browser client is built in");
    let fixture = Fixture::serve(root, &executable, &elsewhere, &[]);
    let index = fixture.http("GET", "/", None, b"");
    assert_eq!(index.status, 200);
    assert_eq!(
        index.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    let wasm = fixture.http("GET", "/capturefab_web_bg.wasm", None, b"");
    assert_eq!(wasm.header("Content-Type"), Some("application/wasm"));
    assert!(wasm.body.starts_with(b"\0asm"));
    if !embedded {
        assert_eq!(index.body, INDEX.as_bytes());
        assert_eq!(wasm.body, WASM);
    }
}

#[test]
fn browser_capture_schedule_preview_and_local_cli_share_a_session() {
    let fixture = Fixture::start();
    fixture.command(
        None,
        json!({"op":"connect","camera":"sim:0","timeout_ms":2000}),
    );
    fixture.command(
        Some("sim:0"),
        json!({"op":"set","feature":"Width","value":"128"}),
    );
    fixture.command(
        Some("sim:0"),
        json!({"op":"set","feature":"Height","value":"96"}),
    );
    assert_eq!(
        fixture.cli(&["--camera", "sim:0", "get", "Width"])["features"][0]["value"],
        128
    );
    fixture.cli(&["--camera", "sim:0", "set", "ExposureTime=5000"]);
    assert_eq!(
        fixture.command(Some("sim:0"), json!({"op":"get","feature":"ExposureTime"}))["value"]
            .as_f64(),
        Some(5000.0)
    );
    let stdout_capture = fixture.http(
        "POST",
        "/api/command",
        None,
        br#"{"version":1,"camera":"sim:0","command":{"op":"capture","output":"-","count":1,"timeout_ms":2000,"format":"png"}}"#,
    );
    assert_eq!(stdout_capture.status, 400);
    assert_eq!(stdout_capture.json()["ok"], false);
    fixture.command(
        Some("sim:0"),
        json!({"op":"capture","output":"browser.png","count":1,"timeout_ms":2000,"format":"png"}),
    );
    assert!(
        fs::read(fixture.root.join("browser.png"))
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    fixture.command(Some("sim:0"), json!({"op":"start"}));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let preview = fixture.http("GET", "/api/preview?camera=sim%3A0", None, b"");
        if preview.status == 200 {
            assert!(preview.body.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert!(
                preview
                    .headers
                    .to_lowercase()
                    .contains("content-type: image/png")
            );
            break;
        }
        assert_eq!(preview.status, 204);
        assert!(Instant::now() < deadline, "preview did not receive a frame");
        std::thread::sleep(Duration::from_millis(50));
    }
    let first_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 150;
    let scheduled = fixture.command(
        Some("sim:0"),
        json!({
            "op":"schedule","output":"sequence","count":3,"timeout_ms":2000,
            "format":"png","first_at_ms":first_at_ms,"interval_ms":100
        }),
    );
    let id = scheduled["job"]["id"].as_u64().unwrap();
    // Every HTTP request above has closed; the server owns the running job.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let jobs = fixture.command(Some("sim:0"), json!({"op":"jobs"}));
        let job = jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["id"] == id)
            .unwrap();
        if job["status"] == "complete" {
            assert_eq!(job["captured"], 3);
            break;
        }
        assert_ne!(job["status"], "failed", "schedule failed: {job}");
        assert!(
            Instant::now() < deadline,
            "schedule did not complete: {job}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        fs::read_dir(fixture.root.join("sequence")).unwrap().count(),
        3
    );
    let pending = fixture.command(
        Some("sim:0"),
        json!({
            "op":"schedule","output":"cancelled.png","count":1,"timeout_ms":2000,
            "format":"png","first_at_ms":first_at_ms + 3_600_000,"interval_ms":0
        }),
    );
    fixture.command(
        Some("sim:0"),
        json!({"op":"cancel_job","id":pending["job"]["id"]}),
    );
    assert!(!fixture.root.join("cancelled.png").exists());
    assert_eq!(
        fixture.cli(&["status"])["cameras"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
