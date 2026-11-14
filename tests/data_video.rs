//! Indexed video, stills from the same camera and sensor values in one run.
mod support;
use serde_json::{Value, json};
use std::{fs, process::Command};
use support::cli::Home;

fn rows(path: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[test]
fn video_configuration_errors_do_not_create_outputs() {
    let home = Home::new("data-video-config");
    for source in [
        json!({"id":"video","protocol":"video","camera":"sim:0","output":"clip.mkv","fps":0}),
        json!({"id":"video","protocol":"video","camera":"sim:0","output":"clip.avi"}),
        json!({"id":"video","protocol":"video","camera":"sim:0","output":"clip.mkv","index":"clip.mkv"}),
        json!({"id":"video","protocol":"video","camera":"sim:0","output":"clip-{seq}.mp4"}),
    ] {
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[source]}).to_string(),
        )
        .unwrap();
        home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "1s",
                "-o",
                "run.jsonl",
            ],
        );
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("clip.mkv").exists());
        assert!(!home.path("clip.mkv.frames.jsonl").exists());
    }
    fs::write(home.path("clip.mkv"), b"existing recording").unwrap();
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[{"id":"video","protocol":"video","camera":"sim:0","output":"clip.mkv"}]}).to_string()).unwrap();
    home.fails(
        1,
        &[
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "1s",
            "-o",
            "run.jsonl",
        ],
    );
    assert_eq!(
        fs::read(home.path("clip.mkv")).unwrap(),
        b"existing recording"
    );
}

#[test]
#[ignore = "requires FFmpeg and ffprobe; set CAPTUREFAB_FFMPEG to the executable"]
fn video_and_stills_from_one_camera_share_the_sensor_timeline() {
    let home = Home::new("data-video");
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[
        {"id":"plc","protocol":"simulator","signal":"cycle","every_ms":10},
        {"id":"video","protocol":"video","camera":"sim:0","output":"clip.mkv","codec":"ffv1","fps":5,
         "settings":["Width=64","Height=64"],"timeout_ms":3000},
        {"id":"stills","protocol":"camera","camera":"sim:0","output":"frames/{seq}.png","every_ms":200,"timeout_ms":3000}
    ]}).to_string()).unwrap();
    let result = home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "2s",
        "-o",
        "run.jsonl",
    ]);
    assert_eq!(result["status"], "complete");
    let recording = rows(&home.path("run.jsonl"));
    let frames = recording
        .iter()
        .filter(|row| row["source"] == "video" && row["value"]["kind"] == "video_frame")
        .collect::<Vec<_>>();
    assert!(frames.len() >= 4, "{result}");
    assert!(recording.iter().any(|row| row["source"] == "plc"));
    let stills = recording
        .iter()
        .filter(|row| row["source"] == "stills")
        .collect::<Vec<_>>();
    assert!(stills.len() >= 2);
    for still in stills {
        assert!(std::path::Path::new(still["value"]["path"].as_str().unwrap()).is_file());
    }
    let index = rows(&home.path("clip.mkv.frames.jsonl"));
    assert_eq!(index.len(), frames.len());
    let host_origin = index[0]["received_monotonic_ns"].as_u64().unwrap()
        - frames[0]["elapsed_ns"].as_u64().unwrap();
    for (number, (indexed, sample)) in index.iter().zip(&frames).enumerate() {
        let value = &sample["value"];
        assert_eq!(value["frame_index"], number);
        assert_eq!(value["frame_index"], indexed["frame_index"]);
        assert_eq!(value["frame_id"], indexed["frame_id"]);
        assert_eq!(value["device_ts_ns"], indexed["device_ts_ns"]);
        assert_eq!(value["video_time_ns"], indexed["video_time_ns"]);
        assert_eq!(
            sample["elapsed_ns"].as_u64().unwrap(),
            indexed["received_monotonic_ns"].as_u64().unwrap() - host_origin
        );
        assert!(sample["delivery_latency_ns"].as_u64().unwrap() > 0);
    }
    let summary = recording
        .iter()
        .find(|row| row["source"] == "video" && row["signal"] == "recording")
        .unwrap();
    assert_eq!(summary["value"]["frames"], frames.len());
    assert_eq!(summary["value"]["finalized"], true);
    assert_eq!(
        summary["value"]["bytes"],
        fs::metadata(home.path("clip.mkv")).unwrap().len()
    );
    let ffprobe = std::env::var("CAPTUREFAB_FFPROBE").unwrap_or_else(|_| "ffprobe".into());
    let probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_frames",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "json",
        ])
        .arg(home.path("clip.mkv"))
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let decoded: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let decoded = decoded["frames"].as_array().unwrap();
    assert_eq!(decoded.len(), frames.len());
    for (frame, sample) in decoded.iter().zip(&frames) {
        let actual = frame["best_effort_timestamp_time"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        let expected = sample["value"]["video_time_ns"].as_u64().unwrap() as f64 / 1e9;
        assert!((actual - expected).abs() < 0.002, "{frame}: {sample}");
    }
    let exported = home.ok(&["data", "export", "run.jsonl", "-o", "run.csv"]);
    assert_eq!(exported["samples"], result["samples"]);
    let filtered = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "video.jsonl",
        "--format",
        "jsonl",
        "--source",
        "video",
    ]);
    assert_eq!(filtered["samples"], frames.len() + 1);
}
