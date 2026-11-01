//! `record` end to end. Recording needs FFmpeg: those tests are ignored
//! unless run with `-- --ignored` and CAPTUREFAB_FFMPEG set.
mod support;

use serde_json::Value;
use std::{
    path::Path,
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
        assert!(decodes(Path::new(&clip)), "{name}");
    }
}
