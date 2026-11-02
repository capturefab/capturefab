//! Where a frame's bytes are copied on their way through the simulator's
//! commands, from the counts a `copy-audit` build's camera workers write
//! (`src/copies.rs`). Each scenario runs twice, with N and 2N frames, and the
//! difference is divided by N: what one more frame costs, without start-up.
//!
//! `cargo test --features copy-audit --test copies -- --nocapture` prints
//! the per-frame numbers.
#![cfg(feature = "copy-audit")]
mod support;

use serde_json::{Map, Value, json};
use std::path::Path;
use support::cli::Home;

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

/// The counts of every worker that wrote a report to `folder`, added up.
fn counts(folder: &Path) -> Value {
    let mut total = json!({});
    for entry in std::fs::read_dir(folder).unwrap().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let report: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        add(&mut total, &report, 1);
    }
    total
}

/// `into += sign * from`, for every number in `from`; `largest` keeps the
/// most.
fn add(into: &mut Value, from: &Value, sign: i64) {
    match (into, from) {
        (Value::Object(into), Value::Object(from)) => {
            for (key, value) in from {
                let slot = into.entry(key.clone()).or_insert_with(|| match value {
                    Value::Object(_) => Value::Object(Map::new()),
                    Value::Number(_) => json!(0),
                    other => other.clone(),
                });
                if key == "largest" {
                    *slot = json!(slot.as_i64().unwrap().max(value.as_i64().unwrap()));
                } else {
                    add(slot, value, sign);
                }
            }
        }
        (into @ Value::Number(_), Value::Number(from)) => {
            *into = json!(into.as_i64().unwrap() + sign * from.as_i64().unwrap());
        }
        _ => {}
    }
}

/// Bytes copied by each class of site.
fn classes(report: &Value) -> Map<String, Value> {
    let mut classes = Map::new();
    for class in ["boundary", "necessary", "transform", "avoidable"] {
        classes.insert(class.into(), json!(0));
    }
    for site in report["sites"]
        .as_object()
        .into_iter()
        .flat_map(Map::values)
    {
        let class = site["class"].as_str().unwrap().to_owned();
        let entry = classes.entry(class).or_insert(json!(0));
        *entry = json!(entry.as_i64().unwrap() + site["bytes"].as_i64().unwrap_or(0));
    }
    classes
}
fn class(report: &Value, name: &str) -> i64 {
    classes(report)[name].as_i64().unwrap()
}
fn site(report: &Value, name: &str) -> i64 {
    report["sites"][name]["bytes"].as_i64().unwrap_or(0)
}
fn calls(report: &Value, name: &str) -> i64 {
    report["sites"][name]["calls"].as_i64().unwrap_or(0)
}
fn large(report: &Value) -> i64 {
    report["large_allocs"]["count"].as_i64().unwrap_or(0)
}
/// The largest allocation size class with any allocations in `report`.
fn largest_class(report: &Value) -> i64 {
    report["large_allocs"]["sizes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, count)| count.as_i64().unwrap() > 0)
        .map(|(size, _)| size.parse::<i64>().unwrap())
        .max()
        .unwrap_or(0)
}

/// The sim camera at 1920x1080 in `format`, as fast as it renders.
fn settings(format: &str) -> Vec<String> {
    [
        "Width=1920".into(),
        "Height=1080".into(),
        "AcquisitionFrameRate=120".into(),
        format!("PixelFormat={format}"),
    ]
    .into_iter()
    .flat_map(|setting| ["--set".into(), setting])
    .collect()
}

/// `--json ARGS` with its workers counting into a fresh folder: their
/// counts, the result, and the bytes of the files written under `out`.
fn counted(home: &Home, out: &str, args: &[&str]) -> (Value, Value, i64) {
    let folder = home
        .root
        .join(format!("report-{}", out.replace(['/', '.'], "-")));
    std::fs::create_dir_all(&folder).unwrap();
    let run = home.run_env(
        &[("CAPTUREFAB_COPY_REPORT", folder.to_str().unwrap())],
        &[&["--json"][..], args].concat(),
    );
    let result = run.success();
    let files = std::fs::read_dir(home.path(out))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.metadata().unwrap().len() as i64)
                .sum()
        })
        .unwrap_or(0);
    (counts(&folder), result, files)
}

/// What one more frame of `capture -n N` costs, from N and 2N frames, and
/// the bytes of the files that frame added.
fn capture_delta(home: &Home, format: &str, extra: &[&str], n: usize) -> (Value, i64) {
    let mut runs = Vec::new();
    for count in [n, 2 * n] {
        let out = format!("{format}-{}-{count}/", extra.join(""));
        let count = count.to_string();
        let settings = settings(format);
        let settings: Vec<&str> = settings.iter().map(String::as_str).collect();
        let args = [
            &["capture", "-c", "sim:0", "-n", &count, "-o", &out][..],
            extra,
            &settings,
        ]
        .concat();
        runs.push(counted(home, &out, &args));
    }
    let mut delta = runs[1].0.clone();
    add(&mut delta, &runs[0].0, -1);
    (delta, runs[1].2 - runs[0].2)
}

/// One line of per-frame numbers.
fn show(name: &str, delta: &Value, frames: i64) {
    let per = |bytes: i64| json!((bytes as f64 / frames as f64).round() as i64);
    let classes: Map<String, Value> = classes(delta)
        .into_iter()
        .map(|(class, bytes)| (class, per(bytes.as_i64().unwrap())))
        .collect();
    let sites: Map<String, Value> = delta["sites"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, site)| site["bytes"].as_i64().unwrap_or(0) != 0)
        .map(|(name, site)| (name.clone(), per(site["bytes"].as_i64().unwrap())))
        .collect();
    println!(
        "{name}: per frame: {} sites {} large allocations {:.2} ({} B), pool {}",
        Value::Object(classes),
        Value::Object(sites),
        large(delta) as f64 / frames as f64,
        delta["large_allocs"]["bytes"].as_i64().unwrap_or(0) / frames,
        delta["pool"],
    );
}

fn frame_bytes(format: &str) -> usize {
    let pixels = WIDTH * HEIGHT;
    match format {
        "Mono8" | "BayerRG8" => pixels,
        "RGB8" => pixels * 3,
        "BayerRG12p" | "Mono12p" => pixels * 3 / 2,
        _ => pixels * 2,
    }
}

#[test]
fn raw_captures_copy_nothing_in_user_space() {
    let home = Home::new("copies-raw");
    for format in ["Mono8", "RGB8", "BayerRG8", "BayerRG12p"] {
        let (delta, files) = capture_delta(&home, format, &["-f", "raw"], 20);
        show(&format!("capture -f raw {format}"), &delta, 20);
        assert!(delta["frames"].as_i64().unwrap() >= 20, "{delta}");
        assert_eq!(site(&delta, "file_write"), files, "{format}");
        assert_eq!(
            calls(&delta, "file_write"),
            40,
            "{format}: a raw file and its sidecar"
        );
        assert!(files > 20 * frame_bytes(format) as i64, "{format}");
    }
}

#[test]
fn still_captures_count_their_copies() {
    let home = Home::new("copies-still");
    for format in ["Mono8", "RGB8"] {
        for (name, extra) in [
            ("jpg", &["-f", "jpg"][..]),
            ("jpg q75", &["-f", "jpg", "--quality", "75"][..]),
            ("png", &["-f", "png"][..]),
        ] {
            let (delta, files) = capture_delta(&home, format, extra, 10);
            show(&format!("capture -f {name} {format}"), &delta, 10);
            assert_eq!(site(&delta, "file_write"), files, "{format} {name}");
        }
    }
}

#[test]
fn stdout_captures_write_the_frame_once() {
    let home = Home::new("copies-stdout");
    let folder = home.root.join("report");
    std::fs::create_dir_all(&folder).unwrap();
    let settings = settings("Mono8");
    let settings: Vec<&str> = settings.iter().map(String::as_str).collect();
    let run = home.run_env(
        &[("CAPTUREFAB_COPY_REPORT", folder.to_str().unwrap())],
        &[
            &["capture", "-c", "sim:0", "-o", "-", "-f", "raw"][..],
            &settings,
        ]
        .concat(),
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let report = counts(&folder);
    show("capture -o - -f raw Mono8", &report, 1);
    assert_eq!(run.raw.len(), WIDTH * HEIGHT);
    assert_eq!(site(&report, "stdout_write"), (WIDTH * HEIGHT) as i64);
    assert_eq!(calls(&report, "stdout_write"), 1);
}

/// A session whose camera worker counts into `folder`.
fn counting_session(home: &Home, name: &str, folder: &Path) -> support::cli::Session {
    std::fs::create_dir_all(folder).unwrap();
    let mut command = home.command();
    command.env("CAPTUREFAB_COPY_REPORT", folder);
    home.serve_in(&mut command, name, &["sim:0"], &[])
}

#[test]
fn inspections_count_their_copies() {
    let home = Home::new("copies-inspect");
    for format in ["Mono8", "RGB8"] {
        let folder = home.root.join(format!("report-{format}"));
        let name = format!("copies-{format}");
        let session = counting_session(&home, &name, &folder);
        let settings = settings(format);
        let settings: Vec<&str> = settings.iter().map(String::as_str).collect();
        let values: Vec<&str> = settings.iter().skip(1).step_by(2).copied().collect();
        home.ok(&[&["--session", &name, "set", "-c", "sim:0"][..], &values].concat());
        let inspect = || home.ok(&["--session", &name, "inspect", "-c", "sim:0"]);
        // The first inspection starts the stream; the next ones cost what an
        // inspection costs.
        inspect();
        let first = counts(&folder);
        for _ in 0..4 {
            inspect();
        }
        let mut delta = counts(&folder);
        add(&mut delta, &first, -1);
        show(&format!("inspect {format}"), &delta, 4);
        println!(
            "inspect {format}: largest allocation class {} B",
            largest_class(&delta)
        );
        drop(session);
    }
}

/// `record --for 2s` of each format: what one recorded frame costs, from
/// the frames the recording wrote.
#[test]
#[ignore = "needs FFmpeg"]
fn recordings_count_their_copies() {
    let home = Home::new("copies-record");
    for format in ["Mono8", "BayerRG8", "BayerRG12p"] {
        let out = format!("{format}.mkv");
        let settings = settings(format);
        let settings: Vec<&str> = settings.iter().map(String::as_str).collect();
        let (report, result, _) = counted(
            &home,
            &out,
            &[
                &["record", "-c", "sim:0", "--for", "2s", "-o", &out][..],
                &settings,
            ]
            .concat(),
        );
        let written = result["frames"].as_i64().unwrap();
        show(&format!("record {format}"), &report, written.max(1));
        println!(
            "record {format}: {written} frames written, ffmpeg_unpack {} calls",
            calls(&report, "ffmpeg_unpack")
        );
    }
}

/// A recording, a burst and inspections of one session camera at once.
#[test]
#[ignore = "needs FFmpeg"]
fn concurrent_consumers_share_frames() {
    let home = Home::new("copies-concurrent");
    let folder = home.root.join("report");
    let session = counting_session(&home, "copies", &folder);
    let settings = settings("Mono8");
    let settings: Vec<&str> = settings.iter().map(String::as_str).collect();
    let values: Vec<&str> = settings.iter().skip(1).step_by(2).copied().collect();
    home.ok(&[&["--session", "copies", "set", "-c", "sim:0"][..], &values].concat());
    let mut record = home.command();
    record.args([
        "--json",
        "--session",
        "copies",
        "record",
        "-c",
        "sim:0",
        "--for",
        "4s",
        "-o",
        "concurrent.mkv",
    ]);
    let record = record.stdout(std::process::Stdio::piped()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    home.ok(&[
        "--session",
        "copies",
        "capture",
        "-c",
        "sim:0",
        "-n",
        "20",
        "-o",
        "burst/",
        "-f",
        "raw",
    ]);
    for _ in 0..3 {
        home.ok(&["--session", "copies", "inspect", "-c", "sim:0"]);
    }
    let output = record.wait_with_output().unwrap();
    assert!(output.status.success());
    let report = counts(&folder);
    show("record + capture + inspect Mono8", &report, 1);
    println!(
        "concurrent: frame_clone {} B in {} calls, frame_cow {} B",
        site(&report, "frame_clone"),
        calls(&report, "frame_clone"),
        site(&report, "frame_cow")
    );
    drop(session);
}

/// The classes of a report, so tests can say which may copy.
#[allow(dead_code)]
fn avoidable(report: &Value) -> i64 {
    class(report, "avoidable")
}
