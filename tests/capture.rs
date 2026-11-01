//! `capture` end to end on the simulator: shots, bursts, time lapses,
//! names that never overwrite, stdout, sessions and interruption.
mod support;

use serde_json::{Value, json};
use std::path::Path;
use support::cli::Home;

/// The result keys that say when, not what.
fn without_timing(mut value: Value) -> Value {
    if let Some(fields) = value.as_object_mut() {
        for key in [
            "files",
            "time",
            "duration_s",
            "device_ts_ns",
            "frames",
            "fps",
        ] {
            fields.remove(key);
        }
    }
    value
}

#[test]
fn direct_and_session_results_are_the_same_apart_from_timing() {
    let home = Home::new("same-results");
    // A camera keeps its settings between commands, so each direct and
    // session command finds the camera as the one before left it.
    let direct = |args: &[&str]| home.ok(&[&["--no-session"][..], args].concat());
    let set = direct(&["set", "-c", "sim:0", "Width=320"]);
    let get = direct(&["get", "-c", "sim:0", "Width", "exposure"]);
    assert_eq!(get["values"]["Width"], 320);
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
    // Back as the session will find it.
    direct(&["set", "-c", "sim:0", "Width=640"]);
    let _session = home.serve("same", &["sim:0"]);
    let session = |args: &[&str]| home.ok(&[&["--session", "same"][..], args].concat());
    assert_eq!(session(&["set", "-c", "sim:0", "Width=320"]), set);
    assert_eq!(session(&["get", "-c", "sim:0", "Width", "exposure"]), get);
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
    // A burst into a folder names its files by camera, time and number.
    let burst = home.ok(&["capture", "-c", "sim:0", "-n", "3", "-o", "burst/"]);
    assert_eq!(burst["count"], 3);
    let files = burst["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    for (index, file) in files.iter().enumerate() {
        let file = Path::new(file.as_str().unwrap());
        assert!(file.starts_with(home.real("burst")), "{file:?}");
        let name = file.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("Pattern-camera-SIM0-"), "{name}");
        assert!(name.ends_with(&format!("-{:04}.png", index + 1)), "{name}");
        assert!(std::fs::read(file).unwrap().starts_with(b"\x89PNG"));
    }
    // A templated folder holds several frames too; {serial}, {seq:N} and
    // {ext} fill in.
    let folders = home.ok(&["capture", "-c", "sim:0", "-n", "2", "-o", "by/{camera}/"]);
    assert_eq!(folders["count"], 2);
    assert!(
        Path::new(folders["files"][1].as_str().unwrap())
            .starts_with(home.real("by/Pattern-camera-SIM0"))
    );
    let named = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-o",
        "{serial}-{seq:6}.{ext}",
        "-f",
        "jpg",
    ]);
    assert_eq!(named["files"][0], json!(home.real("SIM0-000001.jpg")));
    // {time} names differ by the millisecond, and a name taken in one is
    // numbered, never a failure.
    let timed = home.ok(&["capture", "-c", "sim:0", "-n", "2", "-o", "t/{time}.png"]);
    assert_eq!(timed["count"], 2);
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
    // Both were streaming before either was asked for a frame, at the same
    // moment: their first frames are less than a frame period apart.
    assert!(both["skew_ms"].as_u64().unwrap() < 40, "{both}");
    for camera in cameras {
        assert!(camera["device_ts_ns"].is_u64(), "{camera}");
    }
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
fn several_cameras_stream_only_for_their_shots() {
    let home = Home::new("several-timelapse");
    let timelapse = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-c",
        "sim:1",
        "-n",
        "2",
        "--every",
        "1s",
        "-o",
        "lapse/{camera}-{seq}.png",
    ]);
    // Armed together for the first shot, then stopped between shots: at
    // 30 fps a camera streaming throughout would send about 30 frames.
    for camera in timelapse["cameras"].as_array().unwrap() {
        assert_eq!(camera["count"], 2, "{camera}");
        assert!(camera["frames"].as_u64().unwrap() < 15, "{camera}");
    }
}

#[test]
fn a_software_triggered_burst_fires_again_for_frames_lost_on_the_way() {
    let home = Home::new("trigger-loss");
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode=On",
        "TriggerSource=Software",
        "ExposureTime=1000",
    ]);
    let burst = home.ok(&[
        "capture",
        "-c",
        "sim:0?drop=0.1",
        "-n",
        "30",
        "-o",
        "lossy/t-{seq}.png",
    ]);
    assert_eq!(burst["count"], 30, "{burst}");
    let lost = burst["dropped"].as_u64().unwrap() + burst["incomplete"].as_u64().unwrap();
    assert!(lost > 0, "{burst}");
    // A line that never fires still ends the capture, saying so.
    home.ok(&["set", "-c", "sim:0", "TriggerSource=Line1"]);
    let silent = home.fails(
        4,
        &[
            "capture",
            "-c",
            "sim:0",
            "--timeout",
            "200ms",
            "-o",
            "none.png",
        ],
    );
    assert_eq!(silent["code"], "trigger_timeout", "{silent}");
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
    // A dry run never starts a session, whatever else it is asked.
    home.ok(&["capture", "-c", "sim:0", "--background", "--dry-run"]);
    home.ok(&["capture", "-c", "sim:0", "--keep", "1m", "--dry-run"]);
    assert_eq!(home.ok(&["session", "list"])["sessions"], json!([]));
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
    // So does an error: stdout holds nothing but an image.
    let many = home.run(&["--json", "capture", "-c", "sim:0", "-o", "-", "-n", "2"]);
    assert_eq!(many.code, Some(2));
    assert!(many.raw.is_empty());
    let error: Value = serde_json::from_str(many.stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "usage");
    let triggered = home.run(&[
        "--json",
        "capture",
        "-c",
        "sim:0",
        "-o",
        "-",
        "--set",
        "TriggerMode=On",
        "--set",
        "TriggerSource=Line0",
        "--timeout",
        "300ms",
    ]);
    assert_eq!(triggered.code, Some(4));
    assert!(triggered.raw.is_empty());
    let error: Value = serde_json::from_str(triggered.stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "trigger_timeout");
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
    // An inspection says the same, with what it can do.
    let inspected = home.fails(4, &["inspect", "-c", "sim:0", "--timeout", "300ms"]);
    assert_eq!(inspected["code"], "trigger_timeout");
    let hint = inspected["hint"].as_str().unwrap();
    assert!(
        hint.contains("--timeout") && hint.contains("trigger=off"),
        "{hint}"
    );
    // Auto exposure has nothing to meter.
    assert_eq!(
        home.fails(5, &["auto", "-c", "sim:0"])["code"],
        "unsupported"
    );
    home.ok(&["set", "-c", "sim:0", "trigger=off"]);
    home.ok(&["capture", "-c", "sim:0", "--timeout", "2s", "-o", "t.png"]);
    // A line that starts the acquisition holds every frame back too.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode[TriggerSelector=AcquisitionStart]=On",
        "TriggerSource[TriggerSelector=AcquisitionStart]=Line0",
    ]);
    let held = home.fails(
        4,
        &[
            "capture",
            "-c",
            "sim:0",
            "--timeout",
            "300ms",
            "-o",
            "held.png",
        ],
    );
    assert_eq!(held["code"], "trigger_timeout", "{held}");
    assert_eq!(held["details"]["trigger"]["selector"], "AcquisitionStart");
    assert!(
        held["message"]
            .as_str()
            .unwrap()
            .contains("AcquisitionStart"),
        "{held}"
    );
    assert_eq!(
        home.ok(&["info", "-c", "sim:0"])["trigger"]["selector"],
        "AcquisitionStart"
    );
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "trigger"])["values"]["trigger"],
        "line0:rising"
    );
    assert_eq!(
        home.fails(5, &["auto", "-c", "sim:0"])["code"],
        "unsupported"
    );
    // trigger=off lets every start trigger go.
    let off = home.ok(&["set", "-c", "sim:0", "trigger=off"]);
    assert_eq!(
        off["changed"],
        json!([{"name":"TriggerMode[AcquisitionStart]","from":"On","to":"Off"}]),
        "{off}"
    );
    home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--timeout",
        "2s",
        "-o",
        "free.png",
    ]);
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

/// Every camera's saved files are in an interrupted capture's details.
#[cfg(unix)]
#[test]
fn a_signal_during_a_capture_from_several_cameras_lists_all_their_files() {
    use std::process::Stdio;
    let home = Home::new("interrupted-several");
    let mut child = home
        .command()
        .args([
            "--json",
            "capture",
            "-c",
            "sim:0",
            "-c",
            "sim:1",
            "-n",
            "1000",
            "-o",
            "{camera}-{seq}.png",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let saved = || {
        std::fs::read_dir(&home.work)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "png"))
            .count()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while saved() < 4 {
        assert!(std::time::Instant::now() < deadline, "no frames saved");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    support::cli::signal_and_wait(&mut child, libc::SIGINT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    let details = &error["error"]["details"];
    let files = details["files"].as_array().unwrap();
    assert_eq!(files.len(), saved(), "{error}");
    for camera in details["cameras"].as_array().unwrap() {
        assert_eq!(camera["error"]["code"], "interrupted", "{camera}");
        assert!(!camera["files"].as_array().unwrap().is_empty(), "{camera}");
    }
}

/// What is in the work folder, other than folders.
fn names(home: &Home) -> Vec<String> {
    let mut names = std::fs::read_dir(&home.work)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Two commands numbering into one folder at the same time never share a
/// name: each file is published without replacing another's.
#[test]
fn concurrent_captures_numbering_one_folder_never_overwrite() {
    use std::process::Stdio;
    let home = Home::new("concurrent-names");
    let spawn = || {
        home.command()
            .args([
                "--json",
                "--no-session",
                "capture",
                "-c",
                "sim:0",
                "-n",
                "15",
                "-o",
                "shot-{seq}.png",
                "--on-exists",
                "number",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let children = [spawn(), spawn()];
    let mut files = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        for file in result["files"].as_array().unwrap() {
            files.push(file.as_str().unwrap().to_owned());
        }
    }
    assert_eq!(files.len(), 30);
    files.sort();
    files.dedup();
    assert_eq!(files.len(), 30, "two captures reported one name");
    // Exactly those files, whole, and nothing half written left behind.
    let on_disk = names(&home);
    assert_eq!(on_disk.len(), 30, "{on_disk:?}");
    for file in &files {
        assert!(
            std::fs::read(file).unwrap().starts_with(b"\x89PNG"),
            "{file}"
        );
    }
}

/// A capture's manifest gets one line per saved file, with its size and
/// SHA-256; later captures, and several cameras, append to it.
#[test]
fn a_manifest_lists_each_saved_file_with_its_hash() {
    use sha2::{Digest, Sha256};
    let home = Home::new("manifest");
    let burst = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-n",
        "3",
        "-o",
        "m-{seq}.png",
        "--tag",
        "lot=7",
        "--manifest",
        "runs/manifest.jsonl",
    ]);
    let several = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-c",
        "sim:1",
        "-o",
        "{camera}-{seq}.jpg",
        "--manifest",
        "runs/manifest.jsonl",
        "--sync",
        "none",
    ]);
    let text = std::fs::read_to_string(home.path("runs/manifest.jsonl")).unwrap();
    let lines = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{text}");
    let mut saved = burst["files"].as_array().unwrap().clone();
    for camera in several["cameras"].as_array().unwrap() {
        saved.extend(camera["files"].as_array().unwrap().iter().cloned());
    }
    let listed = lines
        .iter()
        .map(|line| line["file"].clone())
        .collect::<Vec<_>>();
    assert_eq!(listed[..3], saved[..3], "a burst's lines are in order");
    let sorted = |mut files: Vec<Value>| {
        files.sort_by_key(|file| file.to_string());
        files
    };
    assert_eq!(sorted(listed), sorted(saved));
    for line in &lines {
        let bytes = std::fs::read(line["file"].as_str().unwrap()).unwrap();
        assert_eq!(line["bytes"], bytes.len(), "{line}");
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(line["sha256"], sha256, "{line}");
        assert!(line["time"].as_str().unwrap().ends_with('Z'), "{line}");
        assert_eq!(line["pixel_format"], "Mono8", "{line}");
        assert!(line["width"].as_u64().unwrap() > 0 && line["settings"].is_object());
    }
    assert_eq!(lines[0]["camera"], burst["camera"]);
    assert_eq!(lines[0]["tags"], json!({"lot": "7"}));
    assert!(lines[3].get("tags").is_none_or(|tags| tags == &json!({})));
    // A folder is no manifest, and an image to stdout saves no file.
    let folder = home.fails(
        2,
        &[
            "capture",
            "-c",
            "sim:0",
            "-o",
            "x.png",
            "--manifest",
            "runs/",
        ],
    );
    assert_eq!(folder["code"], "usage");
    let stdout = home.run(&["capture", "-c", "sim:0", "-o", "-", "--manifest", "x.jsonl"]);
    assert_eq!(stdout.code, Some(2), "{}", stdout.stderr);
}

/// A capture that would leave less than the free-space floor is refused,
/// with the disk's free space, and leaves nothing behind.
#[test]
fn a_disk_below_the_free_space_floor_refuses_captures() {
    let home = Home::new("floor");
    let full = home
        .run_env(
            &[("CAPTUREFAB_MIN_FREE", "1000T")],
            &[
                "--json",
                "capture",
                "-c",
                "sim:0",
                "-n",
                "2",
                "-o",
                "f-{seq}.png",
            ],
        )
        .failure(6);
    assert_eq!(full["code"], "storage_full", "{full}");
    assert_eq!(full["details"]["path"], json!(home.real("f-0001.png")));
    assert!(full["details"]["free_bytes"].as_u64().is_some(), "{full}");
    assert_eq!(full["details"]["files"], json!([]));
    assert!(
        full["hint"]
            .as_str()
            .unwrap()
            .contains("CAPTUREFAB_MIN_FREE")
    );
    assert_eq!(names(&home), Vec::<String>::new());
    // 0 turns the floor off.
    home.run_env(
        &[("CAPTUREFAB_MIN_FREE", "0")],
        &["--json", "capture", "-c", "sim:0", "-o", "free.png"],
    )
    .success();
}

/// The mean of a saved 8-bit grayscale PNG, 0..1.
fn png_mean(path: &Path) -> f64 {
    let decoder = png::Decoder::new(std::fs::File::open(path).unwrap());
    let mut reader = decoder.read_info().unwrap();
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data).unwrap();
    data.truncate(info.buffer_size());
    data.iter().map(|&v| f64::from(v)).sum::<f64>() / data.len() as f64 / 255.0
}

#[test]
fn frames_are_taken_after_the_last_settings_change() {
    let home = Home::new("after-change");
    let _session = home.serve_with("fresh", &["sim:0"], &["--live"]);
    // A slow stream: the frames being exposed and read out when a setting
    // changes take a second to pass.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "AcquisitionFrameRate=2",
        "ExposureTime=2000",
    ]);
    let bright = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--preview",
        "none",
        "--set",
        "ExposureTime=20000",
    ]);
    assert_eq!(bright["after_change"], true, "{bright}");
    assert!(
        bright["exposure"]["mean"].as_f64().unwrap() > 0.6,
        "{bright}"
    );
    let dark = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--set",
        "ExposureTime=2000",
        "-o",
        "dark.png",
    ]);
    assert_eq!(dark["after_change"], true, "{dark}");
    assert!(png_mean(&home.path("dark.png")) < 0.2);
    // Without a change, the next frame is taken; --fresh lets more pass.
    let started = std::time::Instant::now();
    let next = home.ok(&["capture", "-c", "sim:0", "-o", "next.png"]);
    assert!(next.get("after_change").is_none(), "{next}");
    let later = home.ok(&["capture", "-c", "sim:0", "--fresh", "2", "-o", "later.png"]);
    assert!(later.get("after_change").is_none(), "{later}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(1000));
    // A software-triggered frame fired after the change is new at once.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode=On",
        "TriggerSource=Software",
    ]);
    let started = std::time::Instant::now();
    let triggered = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--preview",
        "none",
        "--set",
        "ExposureTime=20000",
    ]);
    assert!(
        triggered["exposure"]["mean"].as_f64().unwrap() > 0.6,
        "{triggered}"
    );
    assert!(started.elapsed() < std::time::Duration::from_millis(1500));
}

#[test]
fn a_camera_that_sets_its_own_exposure_settles_before_frames_are_taken() {
    let home = Home::new("warm-up");
    // Far too dark, with the camera's own continuous auto exposure.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "ExposureTime=500",
        "ExposureAuto=Continuous",
    ]);
    let warm = home.ok(&["capture", "-c", "sim:0", "-o", "warm.png"]);
    assert!(warm["warmup_ms"].as_u64().unwrap() > 0, "{warm}");
    assert!(warm.get("settled").is_none() && warm.get("warnings").is_none());
    let mean = png_mean(&home.path("warm.png"));
    assert!((0.35..0.55).contains(&mean), "{mean}");
    let inspected = home.ok(&["inspect", "-c", "sim:0", "--preview", "none"]);
    assert!(inspected["warmup_ms"].is_u64(), "{inspected}");
    assert_eq!(inspected["verdict"], json!(["ok"]), "{inspected}");
    assert!(inspected.get("settled").is_none(), "{inspected}");
    // Too short a warm-up for three steady frames says so.
    let short = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--warmup",
        "20ms",
        "-o",
        "short.png",
    ]);
    assert_eq!(short["settled"], false, "{short}");
    assert!(short["warnings"][0].as_str().unwrap().contains("--warmup"));
    home.ok(&["set", "-c", "sim:0", "ExposureAuto=Off"]);
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "ExposureTime=50",
        "ExposureAuto=Continuous",
    ]);
    let unsettled = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--warmup",
        "20ms",
        "--preview",
        "none",
    ]);
    assert_eq!(unsettled["settled"], false, "{unsettled}");
    assert!(
        unsettled["verdict"]
            .as_array()
            .unwrap()
            .contains(&json!("not_settled")),
        "{unsettled}"
    );
    let none = home.ok(&["capture", "-c", "sim:0", "--warmup", "0", "-o", "none.png"]);
    assert!(none.get("warmup_ms").is_none(), "{none}");
    // A camera with manual exposure needs none.
    home.ok(&["set", "-c", "sim:0", "ExposureAuto=Off"]);
    let manual = home.ok(&["capture", "-c", "sim:0", "-o", "manual.png"]);
    assert!(manual.get("warmup_ms").is_none(), "{manual}");
}

#[test]
fn a_camera_keeps_its_settings_between_direct_commands() {
    let home = Home::new("kept");
    let direct = |args: &[&str]| home.ok(&[&["--no-session"][..], args].concat());
    direct(&["set", "-c", "sim:2", "Width=320", "PixelFormat=Mono12"]);
    let shot = direct(&["capture", "-c", "sim:2", "-o", "kept.png"]);
    assert_eq!(
        (shot["width"].clone(), shot["pixel_format"].clone()),
        (json!(320), json!("Mono12"))
    );
    // Options of the selector set up the connection, not the camera.
    let read = direct(&["get", "-c", "sim:2?drop=0.01", "Width"]);
    assert_eq!(read["values"]["Width"], 320);
    // Another camera has its own.
    assert_eq!(
        direct(&["get", "-c", "sim:3", "Width"])["values"]["Width"],
        640
    );
}

#[test]
fn hardware_triggers_wait_as_long_as_asked() {
    let home = Home::new("line0");
    let started = std::time::Instant::now();
    let shots = home.ok(&[
        "capture",
        "-c",
        "sim:0?line0=every:300ms",
        "--set",
        "trigger=line0",
        "-n",
        "2",
        "--timeout",
        "none",
        "-o",
        "line/",
    ]);
    assert_eq!(shots["count"], 2, "{shots}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(300));
    // Auto exposure has nothing to meter between triggers.
    assert_eq!(
        home.fails(5, &["auto", "-c", "sim:0?line0=every:300ms"])["code"],
        "unsupported"
    );
}

#[test]
fn bursts_report_the_integrity_of_their_frames() {
    let home = Home::new("integrity");
    let clean = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--set",
        "AcquisitionFrameRate=60",
        "-n",
        "10",
        "-o",
        "clean/",
    ]);
    assert_eq!(clean["count"], 10, "{clean}");
    assert_eq!(
        (
            clean["dropped"].clone(),
            clean["gaps"].clone(),
            clean["consecutive"].clone()
        ),
        (json!(0), json!(0), json!(true)),
        "{clean}"
    );
    assert_eq!(
        clean["last_id"].as_u64().unwrap() - clean["first_id"].as_u64().unwrap(),
        9
    );
    assert!(clean["frames"].as_u64().unwrap() >= 10, "{clean}");
    // Measured by the camera's clock.
    let fps = clean["fps"].as_f64().unwrap();
    assert!((55.0..65.0).contains(&fps), "{clean}");
    assert!(clean["device_ts_ns"].is_u64(), "{clean}");
    // A transport that loses frames: the burst still saves every frame
    // asked for, and says which were missing.
    let lossy = home.ok(&[
        "capture",
        "-c",
        "sim:0?drop=0.1",
        "-n",
        "20",
        "-o",
        "lossy/",
    ]);
    assert_eq!(lossy["count"], 20, "{lossy}");
    assert_eq!(lossy["consecutive"], false, "{lossy}");
    let lost = lossy["dropped"].as_u64().unwrap() + lossy["incomplete"].as_u64().unwrap();
    assert!(lost > 0 && lossy["gaps"].as_u64().unwrap() > 0, "{lossy}");
    assert_eq!(lossy["resent"], 0, "{lossy}");
    // A session says the same of its camera.
    let _session = home.serve_with("lossy", &["sim:5?drop=0.2"], &["--live"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let status = home.ok(&["session", "status", "lossy"]);
    let camera = &status["cameras"][0];
    assert!(camera["frames"].as_u64().unwrap() > 0, "{status}");
    // The rate frames arrive at, by the camera's clock: the 30 fps it
    // streams less those lost.
    let fps = camera["fps"].as_f64().unwrap();
    assert!((5.0..32.0).contains(&fps), "{status}");
    assert!(
        camera["lost"].as_u64().unwrap() + camera["incomplete"].as_u64().unwrap() > 0,
        "{status}"
    );
    assert_eq!(camera["resent"], 0, "{status}");
}

#[test]
fn wide_samples_keep_their_bits_or_say_they_did_not() {
    let home = Home::new("depth");
    let shot = |format: &str, output: &str| {
        home.ok(&[
            "capture",
            "-c",
            "sim:0",
            "--set",
            &format!("PixelFormat={format}"),
            "-o",
            output,
        ])
    };
    let mono = shot("Mono12p", "mono.png");
    assert_eq!(
        (mono["bit_depth"].clone(), mono.get("source_bits")),
        (json!(16), None)
    );
    let decoder = png::Decoder::new(std::fs::File::open(home.path("mono.png")).unwrap());
    let reader = decoder.read_info().unwrap();
    assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
    assert_eq!(reader.info().sbit.as_deref(), Some(&[12u8][..]));
    let bayer = shot("BayerRG12", "bayer.png");
    assert_eq!(
        (bayer["bit_depth"].clone(), bayer["source_bits"].clone()),
        (json!(8), json!(12))
    );
    let jpeg = shot("Mono16", "mono.jpg");
    assert_eq!(jpeg["source_bits"], 16, "{jpeg}");
    // Raw keeps the exact bytes, with what reading them back needs beside.
    let raw = shot("Mono12p", "frame.raw");
    assert_eq!(raw["bit_depth"], 12, "{raw}");
    let bytes = std::fs::read(home.path("frame.raw")).unwrap();
    let sidecar: Value =
        serde_json::from_slice(&std::fs::read(home.path("frame.raw.json")).unwrap()).unwrap();
    assert_eq!(sidecar["pixel_format"], "Mono12p");
    assert_eq!(sidecar["pfnc_code"], 0x010c_0047);
    assert_eq!(
        (sidecar["width"].clone(), sidecar["height"].clone()),
        (json!(640), json!(480))
    );
    assert_eq!(sidecar["row_bytes"], 960);
    assert_eq!(sidecar["bytes"], bytes.len());
    assert!(sidecar["frame_id"].is_u64() && sidecar["device_ts_ns"].is_u64());
    assert!(sidecar["time"].is_string() && sidecar["settings"]["exposure_us"].is_f64());
}
