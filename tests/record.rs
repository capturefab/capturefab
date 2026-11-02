//! `record` end to end. Recording needs FFmpeg: those tests are ignored
//! unless run with `-- --ignored` and CAPTUREFAB_FFMPEG set.
mod support;

use serde_json::Value;
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use support::cli::{Home, decodes};

#[test]
fn recordings_are_checked_before_a_camera_opens() {
    let home = Home::new("record-checks");
    let avi = home.fails(2, &["record", "-c", "sim:0", "-o", "clip.avi"]);
    assert_eq!(avi["code"], "usage");
    assert!(avi["hint"].as_str().unwrap().contains(".mkv"));
    let ffv1 = home.fails(
        2,
        &["record", "-c", "sim:0", "-o", "clip.mp4", "--codec", "ffv1"],
    );
    assert_eq!(ffv1["code"], "usage");
    let stream = home.fails(
        2,
        &[
            "record",
            "-c",
            "sim:0",
            "-o",
            "rtsp://localhost:8554/x",
            "--codec",
            "ffv1",
        ],
    );
    assert_eq!(stream["code"], "usage");
    std::fs::write(home.path("taken.mkv"), b"x").unwrap();
    let taken = home.fails(1, &["record", "-c", "sim:0", "-o", "taken.mkv"]);
    assert_eq!(taken["code"], "exists");
    assert!(taken["hint"].as_str().unwrap().contains("{seq}"));
    assert_eq!(std::fs::read(home.path("taken.mkv")).unwrap(), b"x");
    home.fails(
        2,
        &["record", "-c", "sim:0", "-o", "clip.mkv", "--fps", "0"],
    );
    home.fails(
        2,
        &["record", "-c", "sim:0", "-o", "clip.mkv", "--for", "5"],
    );
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_timed_recording_ends_on_its_duration() {
    let home = Home::new("record-timed");
    let started = Instant::now();
    let result = home.ok(&[
        "record", "-c", "sim:0", "-o", "clip.mkv", "--for", "2s", "--fps", "10",
    ]);
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(result["ended"], "duration");
    assert_eq!(result["finalized"], true);
    assert_eq!(result["codec"], "h264");
    assert_eq!(result["width"], 640);
    assert_eq!(result["output"], serde_json::json!(home.real("clip.mkv")));
    assert!(result["bytes"].as_u64().unwrap() > 0);
    assert!(result["frames"].as_u64().unwrap() > 0);
    assert!(decodes(&home.path("clip.mkv")));
    // A template picks a new name each time.
    for expected in ["r-0001.mp4", "r-0002.mp4"] {
        let named = home.ok(&["record", "-c", "sim:0", "-o", "r-{seq}.mp4", "--for", "1s"]);
        assert!(
            named["output"].as_str().unwrap().ends_with(expected),
            "{named}"
        );
    }
    // Lossless FFV1 in Matroska.
    let lossless = home.ok(&[
        "record",
        "-c",
        "sim:0",
        "-o",
        "lossless.mkv",
        "--codec",
        "ffv1",
        "--for",
        "1s",
    ]);
    assert_eq!(lossless["encoder"], "ffv1");
    assert!(decodes(&home.path("lossless.mkv")));
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn lossless_recordings_keep_bayer_mosaics() {
    let home = Home::new("record-ffv1-bayer");
    // FFV1 keeps the mosaic itself as gray samples: 16-bit for wide Bayer,
    // packed or not, 8-bit for Bayer8.
    for (format, gray) in [
        ("BayerRG12p", "gray16le"),
        ("BayerRG12", "gray16le"),
        ("BayerRG8", "gray("),
    ] {
        let output = format!("{format}.mkv");
        let setting = format!("PixelFormat={format}");
        let result = home.ok(&[
            "record", "-c", "sim:0", "-o", &output, "--codec", "ffv1", "--for", "1s", "--set",
            &setting,
        ]);
        assert_eq!(result["encoder"], "ffv1", "{result}");
        assert!(result["frames"].as_u64().unwrap() > 0, "{result}");
        let path = home.path(&output);
        assert!(decodes(&path), "{format}");
        let probe = std::process::Command::new(support::cli::ffmpeg())
            .args(["-hide_banner", "-i"])
            .arg(&path)
            .output()
            .unwrap();
        let described = String::from_utf8_lossy(&probe.stderr);
        assert!(
            described.contains("ffv1") && described.contains(gray),
            "{described}"
        );
    }
}

/// A Matroska recording, which FFmpeg writes through a pipe, has its
/// length and an index as a file FFmpeg wrote itself would; and deep RGB
/// keeps every bit in FFV1.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn matroska_recordings_have_a_length_and_an_index() {
    let home = Home::new("record-mkv-index");
    for (output, extra) in [
        ("h264.mkv", &[][..]),
        ("ffv1.mkv", &["--codec", "ffv1"][..]),
        (
            "rgb12.mkv",
            &["--codec", "ffv1", "--set", "PixelFormat=RGB12"][..],
        ),
    ] {
        let mut args = vec!["record", "-c", "sim:0", "-o", output, "--for", "2s"];
        args.extend(extra);
        let result = home.ok(&args);
        assert_eq!(result["finalized"], true, "{result}");
        let path = home.path(output);
        let probe = std::process::Command::new(support::cli::ffmpeg())
            .args(["-hide_banner", "-i"])
            .arg(&path)
            .output()
            .unwrap();
        let described = String::from_utf8_lossy(&probe.stderr);
        let length = described
            .lines()
            .find_map(|line| line.trim().strip_prefix("Duration: "))
            .unwrap_or_default();
        assert!(length.starts_with("00:00:0"), "{output}: {described}");
        if output == "rgb12.mkv" {
            assert!(described.contains("gbrp12le"), "{described}");
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.windows(4).any(|id| id == [0x1C, 0x53, 0xBB, 0x6B]),
            "{output} has no Cues"
        );
        let checked = std::process::Command::new(support::cli::ffmpeg())
            .args(["-v", "error", "-err_detect", "crccheck", "-i"])
            .arg(&path)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            checked.stderr.is_empty(),
            "{output}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_recording_reports_its_warm_up() {
    let home = Home::new("record-warm-up");
    // Far too dark, with the camera's own continuous auto exposure.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "ExposureTime=50",
        "ExposureAuto=Continuous",
    ]);
    let short = home.ok(&[
        "record",
        "-c",
        "sim:0",
        "-o",
        "short.mp4",
        "--for",
        "1s",
        "--warmup",
        "50ms",
    ]);
    assert!(short["warmup_ms"].as_u64().unwrap() >= 50, "{short}");
    assert_eq!(short["settled"], false, "{short}");
    assert!(short["warnings"][0].as_str().unwrap().contains("--warmup"));
    let settled = home.ok(&["record", "-c", "sim:0", "-o", "settled.mp4", "--for", "1s"]);
    assert!(settled["warmup_ms"].is_u64(), "{settled}");
    assert!(settled.get("settled").is_none(), "{settled}");
}

#[cfg(unix)]
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_signal_ends_an_open_ended_recording_normally() {
    let home = Home::new("record-signal");
    let mut child = home
        .command()
        .args(["--json", "record", "-c", "sim:0", "-o", "clip.mp4"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    support::cli::signal_and_wait(&mut child, libc::SIGINT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ended"], "signal");
    assert_eq!(result["finalized"], true);
    // An MP4 is unreadable until FFmpeg writes its index at the end.
    assert!(decodes(&home.path("clip.mp4")));
    // A recording asked to run longer is interrupted, its file kept.
    let mut child = home
        .command()
        .args([
            "--json", "record", "-c", "sim:0", "-o", "cut.mkv", "--for", "1m",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    support::cli::signal_and_wait(&mut child, libc::SIGTERM);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "interrupted");
    assert_eq!(error["error"]["details"]["ended"], "signal");
    assert!(decodes(&home.path("cut.mkv")));
}

/// A recording that ends before its first frame, as on a camera that waits
/// for a trigger, recorded nothing: it says so, and names no file.
#[cfg(unix)]
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_recording_ended_before_its_first_frame_reports_no_file() {
    let home = Home::new("record-no-frame");
    let triggered = [
        "--set",
        "TriggerMode=On",
        "--set",
        "TriggerSource=Software",
        "--timeout",
        "none",
    ];
    let mut child = home
        .command()
        .args(["--json", "record", "-c", "sim:5", "-o", "signal.mkv"])
        .args(triggered)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    support::cli::signal_and_wait(&mut child, libc::SIGINT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let error = &serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"];
    assert_eq!(error["code"], "interrupted", "{error}");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("nothing was recorded")
    );
    assert!(error["details"].get("output").is_none(), "{error}");
    assert!(!home.path("signal.mkv").exists());
    // Stopped by another command, it is incomplete.
    let _session = home.serve("bench", &["sim:6"]);
    let child = home
        .command()
        .args([
            "--json",
            "--session",
            "bench",
            "record",
            "-c",
            "sim:6",
            "-o",
            "stopped.mkv",
        ])
        .args(triggered)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while home.ok(&["jobs", "list", "--running"])["jobs"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(Instant::now() < deadline, "the recording did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    home.ok(&["jobs", "stop", "all"]);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = &serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"];
    assert_eq!(error["code"], "incomplete", "{error}");
    assert!(!home.path("stopped.mkv").exists());
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_background_recording_is_a_job_until_stopped() {
    let home = Home::new("record-background");
    let started = home.ok(&["record", "-c", "sim:0", "-o", "bg.mkv", "--background"]);
    let job = &started["job"];
    assert_eq!(job["kind"], "record");
    assert_eq!(job["state"], "running");
    let id = job["id"].as_str().unwrap().to_owned();
    std::thread::sleep(Duration::from_secs(1));
    // While it runs, its job names the file it writes, and its size so far.
    let listed = home.ok(&["jobs"]);
    let running = listed["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == id.as_str())
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(running["state"], "running", "{running}");
    assert_eq!(
        running["last_file"],
        serde_json::json!(home.real("bg.mkv")),
        "{running}"
    );
    assert!(running["bytes"].as_u64().unwrap() > 0, "{running}");
    let stopped = home.ok(&["jobs", "stop", &id]);
    assert_eq!(stopped["stopped"][0]["state"], "done", "{stopped}");
    assert!(stopped["stopped"][0]["bytes"].as_u64().unwrap() > 0);
    let done = home.ok(&["jobs", "wait", &id]);
    assert_eq!(
        done["job"]["last_file"],
        serde_json::json!(home.real("bg.mkv"))
    );
    assert!(decodes(&home.path("bg.mkv")));
}

/// A recording that runs when serve gets SIGTERM, or its terminal's
/// SIGINT or SIGHUP, is finished and playable.
#[cfg(unix)]
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn serve_finishes_a_recording_on_a_signal() {
    for (signal, name) in [
        (libc::SIGTERM, "term"),
        (libc::SIGINT, "int"),
        (libc::SIGHUP, "hup"),
    ] {
        let home = Home::new(&format!("serve-record-{name}"));
        let mut serve = home.serve(name, &["sim:rec"]);
        home.ok(&["set", "-c", "sim:rec", "Width=64", "Height=64"]);
        let clip = home.path("clip.mp4");
        let started = home.ok(&[
            "record",
            "-c",
            "sim:rec",
            "-o",
            "clip.mp4",
            "--fps",
            "10",
            "--background",
        ]);
        assert_eq!(started["job"]["state"], "running");
        std::thread::sleep(Duration::from_secs(1));
        let exit = serve.signal(signal);
        assert!(exit.success(), "{name}: {exit:?}");
        assert!(std::fs::metadata(&clip).unwrap().len() > 0);
        assert!(decodes(std::path::Path::new(&clip)), "{name}");
    }
}

/// A session records with its own FFmpeg: a shell whose FFmpeg setting
/// would not work, as one without FFmpeg, still records through it. A
/// command's own worker records with the command's, which is checked
/// before its camera opens.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_session_records_with_its_own_ffmpeg() {
    let home = Home::new("record-session-ffmpeg");
    let _session = home.serve("svc", &["sim:0"]);
    // A relative path is no FFmpeg this command could run.
    let unusable = [("CAPTUREFAB_FFMPEG", "ffmpeg")];
    let recorded = home
        .run_env(
            &unusable,
            &[
                "--json",
                "--session",
                "svc",
                "record",
                "-c",
                "sim:0",
                "-o",
                "clip.mkv",
                "--for",
                "1s",
            ],
        )
        .success();
    assert_eq!(recorded["ended"], "duration", "{recorded}");
    assert!(decodes(&home.path("clip.mkv")));
    let alone = home.run_env(
        &unusable,
        &[
            "--json",
            "--no-session",
            "record",
            "-c",
            "sim:1",
            "-o",
            "alone.mkv",
            "--for",
            "1s",
        ],
    );
    assert_eq!(alone.failure(2)["code"], "usage");
    assert!(!home.path("alone.mkv").exists());
}

/// A timed recording in a session ends on time, by the session, while other
/// cameras connect to that session.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_session_recording_ends_on_time_while_other_cameras_connect() {
    let home = Home::new("record-session-timed");
    let _serve = home.serve("rec", &["sim:recording"]);
    home.ok(&["set", "-c", "sim:recording", "Width=64", "Height=64"]);
    let started = Instant::now();
    let recording = home
        .command()
        .args([
            "--json",
            "record",
            "-c",
            "sim:recording",
            "-o",
            "clip.mkv",
            "--for",
            "2s",
            "--fps",
            "10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !home.ok(&["jobs"])["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|job| job["kind"] == "record" && job["state"] == "running")
    {
        assert!(Instant::now() < deadline, "the recording did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    home.ok(&["--session", "rec", "get", "-c", "sim:other", "Width"]);
    home.ok(&["--session", "rec", "get", "-c", "sim:third", "Width"]);
    let finished = recording.wait_with_output().unwrap();
    assert_eq!(
        finished.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(started.elapsed() >= Duration::from_secs(2));
    let result: Value = serde_json::from_slice(&finished.stdout).unwrap();
    assert_eq!(result["ended"], "duration", "{result}");
    assert!(decodes(&home.path("clip.mkv")));
    let status = home.ok(&["session", "status", "rec"]);
    let cameras = status["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 3, "{status}");
    assert!(
        cameras.iter().all(|camera| camera["state"] != "recording"),
        "{status}"
    );
}

/// A short timed recording that fails as it starts fails the command, however
/// close to its end it fails.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_short_timed_recording_that_cannot_start_fails() {
    let home = Home::new("record-cannot-start");
    for duration in ["1s", "2s", "10s"] {
        let refused = home.fails(
            1,
            &[
                "record",
                "-c",
                "sim:0",
                "-o",
                "rtsp://127.0.0.1:9/refused",
                "--for",
                duration,
            ],
        );
        assert_eq!(refused["code"], "failed", "{duration}: {refused}");
        assert!(
            refused["message"].as_str().unwrap().contains("refused"),
            "{duration}: {refused}"
        );
        assert_eq!(refused["details"]["job"]["state"], "failed");
    }
}

/// Captures and other recordings run beside a recording without
/// --max-size; and every recording stays a job once it ends, however many
/// follow on its camera.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn recordings_leave_room_and_stay_listed() {
    let home = Home::new("record-room");
    let first = home.ok(&["record", "-c", "sim:0", "-o", "open.mkv", "--background"]);
    let open = first["job"]["id"].as_str().unwrap().to_owned();
    home.ok(&["capture", "-c", "sim:0", "-o", "beside.png"]);
    home.ok(&["capture", "-c", "sim:1", "-o", "other.png"]);
    home.ok(&["record", "-c", "sim:1", "-o", "second.mkv", "--for", "1s"]);
    home.ok(&["jobs", "stop", &open]);
    let short = home.ok(&[
        "record",
        "-c",
        "sim:0",
        "-o",
        "r1.mkv",
        "--for",
        "500ms",
        "--background",
    ]);
    let r1 = short["job"]["id"].as_str().unwrap().to_owned();
    home.ok(&["jobs", "wait", &r1]);
    let again = home.ok(&[
        "record",
        "-c",
        "sim:0",
        "-o",
        "r2.mkv",
        "--for",
        "500ms",
        "--background",
    ]);
    let r2 = again["job"]["id"].as_str().unwrap().to_owned();
    assert_ne!(r1, r2);
    for id in [&open, &r1, &r2] {
        let done = home.ok(&["jobs", "wait", id, "--timeout", "10s"]);
        assert_eq!(done["job"]["state"], "done", "{id}: {done}");
    }
    assert_eq!(
        home.ok(&["jobs", "wait", &r1])["job"]["last_file"],
        serde_json::json!(home.real("r1.mkv"))
    );
}

/// A recording that fails for lack of room says so with its own reason,
/// as a capture would, and leaves no file.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_recording_without_room_fails_as_storage_full() {
    let home = Home::new("record-full");
    let full = home
        .run_env(
            &[("CAPTUREFAB_MIN_FREE", "1000T")],
            &[
                "--json", "record", "-c", "sim:0", "-o", "big.mkv", "--for", "2s",
            ],
        )
        .failure(6);
    assert_eq!(full["code"], "storage_full", "{full}");
    assert_eq!(
        full["details"]["path"],
        serde_json::json!(home.real("big.mkv"))
    );
    assert!(full["details"]["free_bytes"].as_u64().is_some(), "{full}");
    assert_eq!(full["details"]["job"]["error"]["code"], "storage_full");
    assert!(!home.path("big.mkv").exists());
}

/// A recording a session makes leaves the floor its command set, as the
/// session's own environment has none.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_session_recording_leaves_its_commands_free_space_floor() {
    let home = Home::new("record-full-session");
    let record = |floor: Option<&str>, name: &str| {
        let args = ["--json", "record", "-c", "sim:0", "-o", name, "--for", "1s"];
        let mut command = home.warm_command();
        match floor {
            Some(floor) => command.env("CAPTUREFAB_MIN_FREE", floor),
            None => command.env_remove("CAPTUREFAB_MIN_FREE"),
        };
        support::cli::Run::of_output(command.args(args).output().unwrap(), &args)
    };
    home.warm(&["--json", "get", "-c", "sim:0", "Width"])
        .success();
    let full = record(Some("1000T"), "big.mkv").failure(6);
    assert_eq!(full["code"], "storage_full", "{full}");
    assert!(!home.path("big.mkv").exists());
    record(None, "fits.mkv").success();
    assert!(home.path("fits.mkv").exists());
}

/// --max-size ends a recording successfully before that size, its file
/// finished by FFmpeg, every frame it reports in it; one too small for
/// FFmpeg to finish within is cut at the size and says so. Without it
/// nothing caps a recording.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_recording_ends_as_asked_at_its_maximum_size() {
    let home = Home::new("record-max-size");
    let played = |name: &str| {
        let played = std::process::Command::new(support::cli::ffmpeg())
            .args(["-v", "error", "-i"])
            .arg(home.path(name))
            .args(["-f", "framemd5", "-"])
            .output()
            .unwrap();
        let frames = String::from_utf8_lossy(&played.stdout)
            .lines()
            .filter(|line| line.starts_with("0,"))
            .count() as u64;
        (frames, String::from_utf8_lossy(&played.stderr).into_owned())
    };
    for (name, limit) in [("capped.mkv", 3u64 << 20), ("capped.mp4", 3 << 20)] {
        let capped = home.ok(&[
            "record",
            "-c",
            "sim:0",
            "-o",
            name,
            "--max-size",
            "3MiB",
            "--for",
            "30s",
        ]);
        assert_eq!(capped["ended"], "max_size", "{capped}");
        assert_eq!(capped["finalized"], true, "{capped}");
        let bytes = capped["bytes"].as_u64().unwrap();
        assert!(bytes <= limit, "{capped}");
        assert_eq!(std::fs::metadata(home.path(name)).unwrap().len(), bytes);
        let (frames, errors) = played(name);
        assert!(errors.is_empty(), "{name}: {errors}");
        // Every frame handed to the encoder is in the file, but for one a
        // loaded host's encoder may drop now and then.
        let reported = capped["frames"].as_u64().unwrap();
        assert!(
            frames <= reported && reported <= frames + 3,
            "{frames} frames: {capped}"
        );
    }
    // Too small for FFmpeg to finish: cut at a byte, it plays up to its
    // last complete part, and reports no frame count it cannot vouch for.
    let cut = home.ok(&[
        "record",
        "-c",
        "sim:0",
        "-o",
        "cut.mp4",
        "--max-size",
        "16KiB",
        "--for",
        "20s",
    ]);
    assert_eq!(cut["ended"], "max_size", "{cut}");
    assert_eq!(cut["bytes"], 16 * 1024, "{cut}");
    assert_eq!(cut["finalized"], false, "{cut}");
    assert!(cut.get("frames").is_none(), "{cut}");
    assert!(
        cut["warnings"][0].as_str().unwrap().contains("--max-size"),
        "{cut}"
    );
    let open = home.ok(&["record", "-c", "sim:0", "-o", "open.mkv", "--for", "1s"]);
    assert_eq!(open["ended"], "duration", "{open}");
}

/// A second signal ends the command at once, still with one document,
/// and ends the camera worker's process group with it.
#[cfg(unix)]
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_second_signal_ends_at_once_with_a_document() {
    let home = Home::new("record-twice");
    let child = home
        .command()
        .args(["--json", "record", "-c", "sim:0", "-o", "clip.mp4"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(home.path("clip.mp4")).map_or(0, |meta| meta.len()) == 0 {
        assert!(Instant::now() < deadline, "the recording did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The command's own worker (it keeps no session) leads a process
    // group with its FFmpeg. Stopped, it cannot finish the recording the
    // first signal asks it to, however fast it would: only the second
    // signal can end the command.
    let children = std::process::Command::new("pgrep")
        .args(["-P", &child.id().to_string()])
        .output()
        .unwrap();
    let worker = String::from_utf8_lossy(&children.stdout)
        .lines()
        .next()
        .expect("the command's camera worker")
        .trim()
        .parse::<i32>()
        .unwrap();
    // SAFETY: kill only sends signals to processes this test started:
    // the command, and the group its worker leads.
    unsafe {
        assert_eq!(libc::kill(-worker, libc::SIGSTOP), 0);
        assert_eq!(libc::kill(child.id() as i32, libc::SIGINT), 0);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(libc::kill(child.id() as i32, libc::SIGINT), 0);
    }
    let output = child.wait_with_output().unwrap();
    // SAFETY: as above; signal 0 only asks whether the group has members.
    let lives = || unsafe { libc::kill(-worker, 0) } == 0;
    let ended = Instant::now();
    while lives() && ended.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let left = lives();
    if left {
        // SAFETY: as above.
        unsafe { libc::kill(-worker, libc::SIGKILL) };
    }
    assert_eq!(output.status.code(), Some(130));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let document: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["error"]["code"], "interrupted", "{document}");
    assert!(!left, "the worker's group outlived the command");
}

/// An H.264 file `name` in the test's home, 10 s at 30 fps with a key frame
/// each second and no B-frames, as a camera sends.
fn h264_clip(home: &Home, name: &str) {
    h264_clip_of(home, name, 10);
}
/// `h264_clip`, `seconds` long.
fn h264_clip_of(home: &Home, name: &str, seconds: u32) {
    let made = std::process::Command::new(support::cli::ffmpeg())
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .args([
            "testsrc2=size=320x240:rate=30",
            "-t",
            &seconds.to_string(),
            "-c:v",
            "libx264",
        ])
        .args(["-g", "30", "-bf", "0", "-pix_fmt", "yuv420p"])
        .arg(home.path(name))
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
}

/// A recording of a video file ends with the file, finished, rather than
/// holding its frames in the encoder until a signal; so does one asked to
/// run longer than the file.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_recording_ends_when_its_video_file_does() {
    let home = Home::new("record-source-end");
    h264_clip_of(&home, "short.mp4", 2);
    for (output, extra) in [
        ("copy.mp4", &[][..]),
        (
            "encoded.mp4",
            &["--codec", "h264", "--bitrate", "1M", "--for", "1m"][..],
        ),
    ] {
        let mut child = home
            .command()
            .args(["--json", "record", "-c", "short.mp4", "-o", output])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("recording {output} went on after its source ended");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let output_of = child.wait_with_output().unwrap();
        assert_eq!(output_of.status.code(), Some(0), "{output}");
        let result: Value = serde_json::from_slice(&output_of.stdout).unwrap();
        assert_eq!(result["ended"], "source_ended", "{result}");
        assert_eq!(result["finalized"], true, "{result}");
        assert!(result["frames"].as_u64().unwrap() >= 30, "{result}");
        assert!(decodes(&home.path(output)), "{output}");
    }
}

/// An H.264 source is recorded as it arrives, in its own codec and with no
/// encoder, up to a maximum size as asked; a rate or bitrate encodes it.
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn compressed_sources_are_copied_unless_asked_to_encode() {
    let home = Home::new("record-copy");
    h264_clip(&home, "clip.mp4");
    for (output, codec) in [("copy.mkv", None), ("same.mp4", Some("h264"))] {
        let mut args = vec!["record", "-c", "clip.mp4", "-o", output, "--for", "2s"];
        args.extend(codec.map(|codec| ["--codec", codec]).into_iter().flatten());
        let copied = home.ok(&args);
        assert_eq!(copied["encoder"], "copy", "{copied}");
        assert_eq!(copied["codec"], "h264", "{copied}");
        assert_eq!(copied["finalized"], true, "{copied}");
        assert!(copied["frames"].as_u64().unwrap() >= 30, "{copied}");
        assert!(decodes(&home.path(output)), "{output}");
    }
    let capped = home.ok(&[
        "record",
        "-c",
        "clip.mp4",
        "-o",
        "capped.mkv",
        "--max-size",
        "48KiB",
        "--for",
        "20s",
    ]);
    assert_eq!(capped["ended"], "max_size", "{capped}");
    assert_eq!(capped["encoder"], "copy", "{capped}");
    assert!(capped["bytes"].as_u64().unwrap() <= 48 * 1024, "{capped}");
    for asked in [["--bitrate", "1M"], ["--fps", "15"]] {
        let mut args = vec![
            "record",
            "-c",
            "clip.mp4",
            "-o",
            "encoded-{seq}.mkv",
            "--for",
            "1s",
        ];
        args.extend(asked);
        let encoded = home.ok(&args);
        assert_ne!(encoded["encoder"], "copy", "{encoded}");
        assert_eq!(encoded["codec"], "h264", "{encoded}");
    }
}

/// A copy stopped by a signal ends normally, its file finished.
#[cfg(unix)]
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_signal_ends_a_copied_recording_normally() {
    let home = Home::new("record-copy-signal");
    h264_clip(&home, "clip.mp4");
    let mut child = home
        .command()
        .args(["--json", "record", "-c", "clip.mp4", "-o", "copy.mp4"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    support::cli::signal_and_wait(&mut child, libc::SIGINT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ended"], "signal", "{result}");
    assert_eq!(result["encoder"], "copy", "{result}");
    assert_eq!(result["finalized"], true, "{result}");
    assert!(decodes(&home.path("copy.mp4")));
}

/// Naming the source's own codec copies it, though no encoder for that
/// codec works here; a source that is not copied then fails as unsupported
/// before it records.
#[cfg(unix)]
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn naming_the_sources_codec_copies_it_without_an_encoder() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new("record-copy-no-encoder");
    h264_clip(&home, "clip.mp4");
    // FFmpeg, with every H.264 encoder failing.
    let ffmpeg = home.root.join("ffmpeg");
    let script = format!(
        r#"#!/bin/sh
previous=
for arg in "$@"; do
  case "$previous $arg" in
    "-c:v libx264"|"-c:v h264_"*) echo 'no H.264 encoder here' >&2; exit 1 ;;
  esac
  previous=$arg
done
exec '{}' "$@"
"#,
        support::cli::ffmpeg()
    );
    std::fs::write(&ffmpeg, script).unwrap();
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("CAPTUREFAB_FFMPEG", ffmpeg.to_str().unwrap())];
    let copied = home
        .run_env(
            &env,
            &[
                "--json", "record", "-c", "clip.mp4", "-o", "same.mkv", "--codec", "h264", "--for",
                "2s",
            ],
        )
        .success();
    assert_eq!(copied["encoder"], "copy", "{copied}");
    assert_eq!(copied["codec"], "h264", "{copied}");
    assert!(decodes(&home.path("same.mkv")));
    let encoded = home
        .run_env(
            &env,
            &[
                "--json", "record", "-c", "sim:0", "-o", "sim.mkv", "--codec", "h264", "--for",
                "1s",
            ],
        )
        .failure(5);
    assert_eq!(encoded["code"], "unsupported", "{encoded}");
    assert!(!home.path("sim.mkv").exists());
}
