//! `capture` end to end on the simulator: shots, bursts, time lapses,
//! names that never overwrite, stdout, sessions and interruption.
mod support;

use serde_json::{Value, json};
use std::path::Path;
use support::cli::Home;

/// The result keys that say when, not what.
fn without_timing(mut value: Value) -> Value {
    if let Some(fields) = value.as_object_mut() {
        for key in ["files", "time", "duration_s"] {
            fields.remove(key);
        }
    }
    value
}

#[test]
fn direct_and_session_results_are_the_same_apart_from_timing() {
    let home = Home::new("same-results");
    // A worker of a command keeps no settings after it, so each direct
    // command sets what it needs.
    let direct = |args: &[&str]| home.ok(&[&["--no-session"][..], args].concat());
    let get = direct(&["get", "-c", "sim:0", "Width", "exposure"]);
    let set = direct(&["set", "-c", "sim:0", "Width=320"]);
    let capture = direct(&[
        "capture",
        "-c",
        "sim:0",
        "--set",
        "Width=320",
        "-o",
        "direct.png",
    ]);
    let inspect = direct(&[
        "inspect",
        "-c",
        "sim:0",
        "--set",
        "Width=320",
        "--preview",
        "none",
    ]);
    let _session = home.serve("same", &["sim:0"]);
    let session = |args: &[&str]| home.ok(&[&["--session", "same"][..], args].concat());
    assert_eq!(session(&["get", "-c", "sim:0", "Width", "exposure"]), get);
    assert_eq!(session(&["set", "-c", "sim:0", "Width=320"]), set);
    let shot = session(&[
        "capture",
        "-c",
        "sim:0",
        "--set",
        "Width=320",
        "-o",
        "session.png",
    ]);
    assert_eq!(
        without_timing(shot.clone()),
        without_timing(capture.clone())
    );
    assert_eq!(shot["files"], json!([home.real("session.png")]));
    assert_eq!(capture["width"], 320);
    // Inspections measure two different frames; their shape is the same.
    let measured = session(&[
        "inspect",
        "-c",
        "sim:0",
        "--set",
        "Width=320",
        "--preview",
        "none",
    ]);
    let keys = |value: &Value| {
        value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&measured), keys(&inspect));
    assert_eq!(measured["frame"]["width"], inspect["frame"]["width"]);
    assert_eq!(measured["settings"], inspect["settings"]);
}

#[test]
fn bursts_time_lapses_and_names_that_never_overwrite() {
    let home = Home::new("names");
    // A burst into a folder numbers its files.
    let burst = home.ok(&["capture", "-c", "sim:0", "-n", "3", "-o", "burst/"]);
    assert_eq!(burst["count"], 3);
    let files = burst["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    for file in files {
        let file = Path::new(file.as_str().unwrap());
        assert!(file.starts_with(home.real("burst")), "{file:?}");
        assert!(std::fs::read(file).unwrap().starts_with(b"\x89PNG"));
    }
    assert!(burst["duration_s"].as_f64().unwrap() >= 0.0);
    // A time lapse: frames apart, its count from -n.
    let lapse = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--every",
        "300ms",
        "-n",
        "3",
        "-o",
        "lapse/{seq}.jpg",
    ]);
    assert_eq!(lapse["count"], 3);
    assert!(lapse["duration_s"].as_f64().unwrap() >= 0.55, "{lapse}");
    assert!(lapse["files"][2].as_str().unwrap().ends_with("0003.jpg"));
    // --for counts the frames: every 100ms for 300ms is three.
    let timed = home.ok(&[
        "capture", "-c", "sim:0", "--every", "100ms", "--for", "300ms", "-o", "timed/",
    ]);
    assert_eq!(timed["count"], 3);
    // Tags name folders and ride along in the metadata.
    let tagged = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--tag",
        "line=L1",
        "-o",
        "{tag:line}/shot.png",
    ]);
    assert_eq!(tagged["files"][0], json!(home.real("L1/shot.png")));
    // A taken name is refused, with what to do; nothing is overwritten.
    home.ok(&["capture", "-c", "sim:0", "-o", "one.png"]);
    let before = std::fs::read(home.path("one.png")).unwrap();
    let taken = home.fails(1, &["capture", "-c", "sim:0", "-o", "one.png"]);
    assert_eq!(taken["code"], "exists");
    assert_eq!(taken["details"]["path"], json!(home.real("one.png")));
    assert!(
        taken["hint"]
            .as_str()
            .unwrap()
            .contains("--on-exists number")
    );
    assert_eq!(std::fs::read(home.path("one.png")).unwrap(), before);
    let numbered = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-o",
        "one.png",
        "--on-exists",
        "number",
    ]);
    assert_eq!(numbered["files"][0], json!(home.real("one-0001.png")));
    // The extension says the format; -f only fills in, and never conflicts.
    let raw = home.ok(&["capture", "-c", "sim:0", "-o", "frame", "-f", "raw"]);
    assert!(raw["files"][0].as_str().unwrap().ends_with("frame.raw"));
    let conflict = home.fails(2, &["capture", "-c", "sim:0", "-o", "x.png", "-f", "jpg"]);
    assert_eq!(conflict["code"], "usage");
    let gif = home.fails(2, &["capture", "-c", "sim:0", "-o", "x.gif"]);
    assert_eq!(gif["code"], "usage");
    // Several frames need several names.
    let one_name = home.fails(2, &["capture", "-c", "sim:0", "-n", "2", "-o", "two.png"]);
    assert!(one_name["message"].as_str().unwrap().contains("{seq}"));
    // A time lapse needs an end, and --for needs --every.
    home.fails(2, &["capture", "-c", "sim:0", "--every", "1s", "-o", "x/"]);
    home.fails(2, &["capture", "-c", "sim:0", "--for", "1s", "-o", "x/"]);
    let passed = home.fails(
        2,
        &["capture", "-c", "sim:0", "--at", "2000-01-01T00:00:00Z"],
    );
    assert!(passed["message"].as_str().unwrap().contains("passed"));
    // Without -o, this folder.
    let here = home.ok(&["capture", "-c", "sim:0"]);
    assert!(Path::new(here["files"][0].as_str().unwrap()).starts_with(home.real("")));
    // A delayed capture waits.
    let started = std::time::Instant::now();
    home.ok(&["capture", "-c", "sim:0", "--in", "500ms", "-o", "later.png"]);
    assert!(started.elapsed() >= std::time::Duration::from_millis(500));
}

#[test]
fn several_cameras_capture_at_once() {
    let home = Home::new("several");
    let both = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-c",
        "sim:1",
        "-o",
        "both/{camera}.png",
    ]);
    let cameras = both["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 2);
    assert_eq!(cameras[0]["camera"], "sim:0");
    assert_eq!(cameras[1]["camera"], "sim:1");
    let one_file = home.fails(
        2,
        &["capture", "-c", "sim:0", "-c", "sim:1", "-o", "same.png"],
    );
    assert!(one_file["message"].as_str().unwrap().contains("{camera}"));
    // A camera that fails is reported beside the ones that worked.
    let failed = home.fails(3, &["capture", "-c", "sim:0", "-c", "sim9", "-o", "mixed/"]);
    let shown = failed["details"]["cameras"].as_array().unwrap();
    assert_eq!(shown[0]["camera"], "sim:0");
    assert_eq!(shown[1]["error"]["code"], "camera_not_found");
}

#[test]
fn dry_runs_resolve_without_capturing() {
    let home = Home::new("dry-run");
    let plan = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--every",
        "1s",
        "-n",
        "3",
        "-o",
        "plan/",
        "--set",
        "exposure=2ms",
        "--dry-run",
    ]);
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["camera"], "sim:0");
    assert_eq!(plan["count"], 3);
    assert_eq!(plan["every_s"], 1.0);
    assert!(plan["ends_at"].is_string());
    assert_eq!(plan["settings"].as_array().unwrap().len(), 1);
    assert!(Path::new(plan["output"].as_str().unwrap()).starts_with(home.real("")));
    assert!(!home.path("plan").exists());
}

#[test]
fn stdout_takes_one_valid_image() {
    let home = Home::new("stdout");
    let run = home.run(&["capture", "-c", "sim:0", "-o", "-"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.raw.starts_with(b"\x89PNG\r\n\x1a\n"));
    let decoded = png::Decoder::new(std::io::Cursor::new(run.raw.clone()))
        .read_info()
        .unwrap();
    assert_eq!(decoded.info().width, 640);
    // In JSON mode the image keeps stdout; the result is on stderr.
    let run = home.run(&["--json", "capture", "-c", "sim:0", "-o", "-", "-f", "raw"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.raw.len(), 640 * 480);
    let result: Value = serde_json::from_str(run.stderr.trim()).unwrap();
    assert_eq!(result["output"], "-");
    assert_eq!(result["bytes"], 640 * 480);
    let many = home.fails(2, &["capture", "-c", "sim:0", "-o", "-", "-n", "2"]);
    assert_eq!(many["code"], "usage");
}

#[test]
fn a_session_writes_where_the_caller_asked() {
    let home = Home::new("caller-paths");
    let mut command = home.command();
    command.current_dir("/");
    let _session = home.serve_in(&mut command, "rooted", &["sim:0"], &[]);
    let shot = home.ok(&["capture", "-c", "sim:0", "-o", "frames/shot.png"]);
    assert_eq!(shot["files"][0], json!(home.real("frames/shot.png")));
    assert!(home.path("frames/shot.png").is_file());
    let explicit = home.ok(&["--session", "rooted", "capture", "-o", "second.png"]);
    assert_eq!(explicit["files"][0], json!(home.real("second.png")));
}

#[test]
fn a_hardware_triggered_camera_times_out_with_its_trigger() {
    let home = Home::new("trigger");
    let _session = home.serve("trigger", &["sim:0"]);
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode=On",
        "TriggerSource=Line0",
    ]);
    let started = std::time::Instant::now();
    let timeout = home.fails(
        4,
        &[
            "capture",
            "-c",
            "sim:0",
            "--timeout",
            "300ms",
            "-o",
            "t.png",
        ],
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(timeout["code"], "trigger_timeout");
    assert_eq!(
        timeout["details"]["trigger"],
        json!({"selector":"FrameStart","mode":"On","source":"Line0","activation":"RisingEdge"})
    );
    assert!(timeout["hint"].as_str().unwrap().contains("--timeout"));
    assert_eq!(timeout["details"]["files"], json!([]));
    // An inspection says the same.
    let inspected = home.fails(4, &["inspect", "-c", "sim:0"]);
    assert_eq!(inspected["code"], "trigger_timeout");
    // Auto exposure has nothing to meter.
    assert_eq!(
        home.fails(5, &["auto", "-c", "sim:0"])["code"],
        "unsupported"
    );
    home.ok(&["set", "-c", "sim:0", "trigger=off"]);
    home.ok(&["capture", "-c", "sim:0", "--timeout", "2s", "-o", "t.png"]);
}

#[cfg(unix)]
#[test]
fn a_signal_during_a_burst_exits_with_the_files_saved() {
    use std::process::Stdio;
    for (signal, exit) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let home = Home::new("interrupted");
        let mut child = home
            .command()
            .args([
                "--json", "capture", "-c", "sim:0", "-n", "100", "--every", "100ms", "-o", "burst/",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::fs::read_dir(home.path("burst"))
            .map(|files| files.count())
            .unwrap_or(0)
            < 2
        {
            assert!(std::time::Instant::now() < deadline, "no frames saved");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        support::cli::signal_and_wait(&mut child, signal);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(exit));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "interrupted", "{error}");
        let files = error["error"]["details"]["files"].as_array().unwrap();
        assert!(files.len() >= 2 && files.len() < 100, "{error}");
        for file in files {
            assert!(Path::new(file.as_str().unwrap()).is_file());
        }
    }
}
