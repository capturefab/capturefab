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
fn class_bytes(report: &Value, name: &str) -> i64 {
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

/// Allocations of a frame's size or more, per frame: buffers come from the
/// pool, so only the first frames allocate.
const LARGE_PER_FRAME: f64 = 0.05;

#[test]
fn raw_captures_copy_nothing_in_user_space() {
    let home = Home::new("copies-raw");
    for format in ["Mono8", "RGB8", "BayerRG8", "BayerRG12p"] {
        let (delta, files) = capture_delta(&home, format, &["-f", "raw"], 20);
        show(&format!("capture -f raw {format}"), &delta, 20);
        assert!(delta["frames"].as_i64().unwrap() >= 20, "{delta}");
        // The file write, from the frame's own buffer, is the only copy.
        for class in ["avoidable", "necessary", "transform"] {
            assert_eq!(class_bytes(&delta, class), 0, "{format} {class}: {delta}");
        }
        assert_eq!(site(&delta, "file_write"), files, "{format}");
        assert_eq!(
            calls(&delta, "file_write"),
            40,
            "{format}: a raw file and its sidecar"
        );
        assert!(files > 20 * frame_bytes(format) as i64, "{format}");
        assert!(
            large(&delta) as f64 / 20.0 <= LARGE_PER_FRAME,
            "{format}: {delta}"
        );
    }
}

#[test]
fn still_captures_encode_from_the_frame_in_place() {
    let home = Home::new("copies-still");
    for format in ["Mono8", "RGB8"] {
        for (name, extra) in [
            ("jpg", &["-f", "jpg"][..]),
            ("jpg q75", &["-f", "jpg", "--quality", "75"][..]),
            ("png", &["-f", "png"][..]),
        ] {
            let (delta, files) = capture_delta(&home, format, extra, 10);
            show(&format!("capture -f {name} {format}"), &delta, 10);
            assert_eq!(
                class_bytes(&delta, "avoidable"),
                0,
                "{format} {name}: {delta}"
            );
            assert_eq!(site(&delta, "file_write"), files, "{format} {name}");
            let per_file = large(&delta) as f64 / 10.0;
            if name == "png" {
                // The PNG encoder's own compressed-data buffer grows by
                // doubling inside the png crate, beside the one output
                // buffer: one allocation per doubling past 256 KiB.
                let doublings = (files / 10 / (256 << 10)).max(1).ilog2() + 2;
                assert!(per_file <= f64::from(doublings) + 1.0, "{format}: {delta}");
            } else {
                // libjpeg-turbo writes into its own (C) buffer.
                assert!(per_file <= LARGE_PER_FRAME, "{format} {name}: {delta}");
            }
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
    assert_eq!(class_bytes(&report, "avoidable"), 0, "{report}");
}

/// A session whose camera worker counts into `folder`.
fn counting_session(home: &Home, name: &str, folder: &Path) -> support::cli::Session {
    std::fs::create_dir_all(folder).unwrap();
    let mut command = home.command();
    command.env("CAPTUREFAB_COPY_REPORT", folder);
    home.serve_in(&mut command, name, &["sim:0"], &[])
}

/// What `inspections` more inspections of a session camera in `format`
/// cost, after the first started its stream.
fn inspect_delta(home: &Home, format: &str, inspections: i64) -> Value {
    let folder = home.root.join(format!("report-{format}"));
    let name = format!("copies-{format}");
    let session = counting_session(home, &name, &folder);
    let settings = settings(format);
    let values: Vec<&str> = settings
        .iter()
        .skip(1)
        .step_by(2)
        .map(String::as_str)
        .collect();
    home.ok(&[&["--session", &name, "set", "-c", "sim:0"][..], &values].concat());
    let inspect = || home.ok(&["--session", &name, "inspect", "-c", "sim:0"]);
    inspect();
    let first = counts(&folder);
    for _ in 0..inspections {
        inspect();
    }
    let mut delta = counts(&folder);
    add(&mut delta, &first, -1);
    drop(session);
    delta
}

#[test]
fn inspections_shrink_the_frame_in_place() {
    let home = Home::new("copies-inspect");
    for (format, channels) in [("Mono8", 1), ("RGB8", 3)] {
        let delta = inspect_delta(&home, format, 4);
        show(&format!("inspect {format}"), &delta, 4);
        assert_eq!(class_bytes(&delta, "avoidable"), 0, "{format}: {delta}");
        // Nothing as large as the frame: the preview raster, a quarter of
        // a 1080p frame at most, is the largest allocation.
        let preview = WIDTH * HEIGHT * channels / 4;
        println!(
            "inspect {format}: largest allocation class {} B",
            largest_class(&delta)
        );
        assert!(
            largest_class(&delta) <= preview.next_power_of_two() as i64,
            "{format}: {delta}"
        );
    }
}

/// Formats that are converted for their stills and previews still build
/// whole-frame intermediates: the numbers the next stage starts from. What
/// this stage removed stays removed.
#[test]
fn converted_formats_copy_only_to_convert() {
    let home = Home::new("copies-converted");
    for format in ["BayerRG8", "BayerRG12p", "Mono12p"] {
        for (name, extra) in [("jpg", &["-f", "jpg"][..]), ("png", &["-f", "png"][..])] {
            let (delta, _) = capture_delta(&home, format, extra, 5);
            show(&format!("capture -f {name} {format}"), &delta, 5);
            for removed in [
                "frame_clone",
                "frame_cow",
                "zero_fill",
                "still_copy",
                "output_copy",
            ] {
                assert_eq!(site(&delta, removed), 0, "{format} {name} {removed}");
            }
        }
    }
    let delta = inspect_delta(&home, "BayerRG12p", 2);
    show("inspect BayerRG12p", &delta, 2);
    for removed in ["frame_clone", "frame_cow", "zero_fill", "output_copy"] {
        assert_eq!(site(&delta, removed), 0, "inspect BayerRG12p {removed}");
    }
}

/// `record --for 2s` of each format: what one recorded frame costs, from
/// the frames the recording wrote.
#[test]
#[ignore = "needs FFmpeg"]
fn recordings_pipe_the_frame_or_unpack_it_once() {
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
        assert_eq!(class_bytes(&report, "avoidable"), 0, "{format}: {report}");
        // Frames are unpacked on the writer thread, once per frame written;
        // 8-bit frames go to FFmpeg as they are.
        let unpacked = if format == "BayerRG12p" { written } else { 0 };
        assert_eq!(
            calls(&report, "ffmpeg_unpack"),
            unpacked,
            "{format}: {report}"
        );
        assert_eq!(calls(&report, "pipe_write"), written, "{format}: {report}");
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
    let values: Vec<&str> = settings
        .iter()
        .skip(1)
        .step_by(2)
        .map(String::as_str)
        .collect();
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
    assert_eq!(site(&report, "frame_cow"), 0, "{report}");
    assert_eq!(site(&report, "frame_clone"), 0, "{report}");
    assert_eq!(class_bytes(&report, "avoidable"), 0, "{report}");
    drop(session);
}
