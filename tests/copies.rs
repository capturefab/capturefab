//! Where a frame's bytes are copied on their way through the simulator's
//! commands, from the counts a `copy-audit` build's camera workers, and
//! their GPU JPEG helpers, write (`src/copies.rs`). Each scenario runs
//! twice, with N and 2N frames, and the difference is divided by N: what
//! one more frame costs, without start-up.
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

/// The counts of every worker and helper that wrote a report to `folder`,
/// added up.
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
    counted_env(home, &[], out, args)
}
/// `counted` with more variables set.
fn counted_env(home: &Home, env: &[(&str, &str)], out: &str, args: &[&str]) -> (Value, Value, i64) {
    let folder = home
        .root
        .join(format!("report-{}", out.replace(['/', '.'], "-")));
    std::fs::create_dir_all(&folder).unwrap();
    let run = home.run_env(
        &[
            &[("CAPTUREFAB_COPY_REPORT", folder.to_str().unwrap())][..],
            env,
        ]
        .concat(),
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
    capture_delta_env(home, &[], format, extra, n)
}
/// `capture_delta` with more variables set.
fn capture_delta_env(
    home: &Home,
    env: &[(&str, &str)],
    format: &str,
    extra: &[&str],
    n: usize,
) -> (Value, i64) {
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
        runs.push(counted_env(home, env, &out, &args));
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
            // Enough frames that one more frame buffer, for a burst whose
            // encoder fell behind on a busy host, stays within the budget.
            let n = if name == "png" { 10 } else { 20 };
            let (delta, files) = capture_delta(&home, format, extra, n);
            show(&format!("capture -f {name} {format}"), &delta, n as i64);
            assert_eq!(
                class_bytes(&delta, "avoidable"),
                0,
                "{format} {name}: {delta}"
            );
            assert_eq!(site(&delta, "file_write"), files, "{format} {name}");
            let per_file = large(&delta) as f64 / n as f64;
            if name == "png" {
                // The PNG encoder's own compressed-data buffer grows by
                // doubling inside the png crate, beside the one output
                // buffer: one allocation per doubling past 256 KiB.
                let doublings = (files / n as i64 / (256 << 10)).max(1).ilog2() + 2;
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
fn inspect_delta(home: &Home, format: &str, region: Option<&str>, inspections: i64) -> Value {
    let tag = format!("{format}-{}", region.map_or(0, |_| 1));
    let folder = home.root.join(format!("report-{tag}"));
    let name = format!("copies-{tag}");
    let session = counting_session(home, &name, &folder);
    let settings = settings(format);
    let values: Vec<&str> = settings
        .iter()
        .skip(1)
        .step_by(2)
        .map(String::as_str)
        .collect();
    home.ok(&[&["--session", &name, "set", "-c", "sim:0"][..], &values].concat());
    let inspect = || {
        let mut args = vec!["--session", &name, "inspect", "-c", "sim:0"];
        if let Some(region) = region {
            args.extend(["--region", region]);
        }
        home.ok(&args)
    };
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
        let delta = inspect_delta(&home, format, None, 4);
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

/// The simulator's sensor formats: every one converted for a still or a
/// preview is decoded once, straight into the consumer's input.
const FORMATS: [&str; 8] = [
    "Mono8",
    "RGB8",
    "Mono12",
    "Mono12p",
    "Mono16",
    "BayerRG8",
    "BayerRG12",
    "BayerRG12p",
];

/// Copies of a frame no path makes: whole-frame intermediates, crops,
/// clones, copies on write, zero fills and copied output.
fn assert_no_avoidable(name: &str, delta: &Value) {
    assert_eq!(class_bytes(delta, "avoidable"), 0, "{name}: {delta}");
    for site in [
        "intermediate",
        "crop",
        "frame_clone",
        "frame_cow",
        "zero_fill",
    ] {
        assert_eq!(calls(delta, site), 0, "{name} {site}: {delta}");
    }
}

/// Stills of every format: converted formats are decoded once, into a
/// pooled buffer, so they allocate nothing large once warm.
#[test]
fn every_format_converts_once_into_its_encoder() {
    let home = Home::new("copies-converted");
    for format in FORMATS {
        for (name, extra) in [
            ("raw", &["-f", "raw"][..]),
            ("jpg", &["-f", "jpg"][..]),
            ("png", &["-f", "png"][..]),
        ] {
            // Enough frames that one more buffer for a burst that happened
            // to encode more frames at once stays within the budget.
            let n = if name == "png" { 5 } else { 20 };
            let (delta, files) = capture_delta(&home, format, extra, n);
            let label = format!("capture -f {name} {format}");
            show(&label, &delta, n as i64);
            assert_no_avoidable(&label, &delta);
            assert_eq!(site(&delta, "file_write"), files, "{label}");
            let per_file = large(&delta) as f64 / n as f64;
            match name {
                "raw" => assert_eq!(class_bytes(&delta, "transform"), 0, "{label}"),
                // The png crate's compressed-data buffer grows by doubling
                // beside the one output buffer (see above).
                "png" => {
                    let doublings = (files / n as i64 / (256 << 10)).max(1).ilog2() + 2;
                    assert!(per_file <= f64::from(doublings) + 1.0, "{label}: {delta}");
                }
                _ => assert!(per_file <= LARGE_PER_FRAME, "{label}: {delta}"),
            }
        }
    }
}

/// Apple's JPEG engine, named: color stills are written from the frame's
/// rows straight into the engine's surface (`still_transform`) and kept in
/// its own buffer, so a frame copies nothing it need not and allocates
/// nothing large. Skipped where the engine is missing or fails its check.
#[test]
#[cfg(target_os = "macos")]
fn the_jpeg_engine_encodes_from_the_frame() {
    let home = Home::new("copies-videotoolbox");
    let engine = [("CAPTUREFAB_JPEG", "videotoolbox")];
    let settings = settings("RGB8");
    let settings: Vec<&str> = settings.iter().map(String::as_str).collect();
    let probe = home.run_env(
        &engine,
        &[
            &["--json", "capture", "-c", "sim:0", "-o", "probe.jpg"][..],
            &settings,
        ]
        .concat(),
    );
    if probe.code != Some(0) {
        eprintln!(
            "no VideoToolbox JPEG engine here; skipped: {}",
            probe.stdout
        );
        return;
    }
    for format in ["RGB8", "BayerRG8"] {
        let n = 20;
        let (delta, files) = capture_delta_env(&home, &engine, format, &["-f", "jpg"], n);
        let label = format!("capture -f jpg {format} (VideoToolbox)");
        show(&label, &delta, n as i64);
        assert_no_avoidable(&label, &delta);
        assert_eq!(calls(&delta, "output_copy"), 0, "{label}: {delta}");
        assert_eq!(site(&delta, "file_write"), files, "{label}");
        assert!(
            large(&delta) as f64 / n as f64 <= LARGE_PER_FRAME,
            "{label}: {delta}"
        );
    }
}

/// The reports GPU JPEG helpers wrote under `home`'s report folders.
#[cfg(target_os = "linux")]
fn helper_reports(home: &Home) -> usize {
    std::fs::read_dir(&home.root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("report-"))
        .flat_map(|folder| std::fs::read_dir(folder.path()).unwrap().flatten())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("helper-") && name.ends_with(".json")
        })
        .count()
}

/// nvJPEG and VA-API, named: frames reach the helper in the shared memory
/// the camera wrote them to, and the copies the helper makes (a Bayer
/// frame converted once for the GPU) are in its own report. Skipped where
/// the engine is missing or fails its check.
#[test]
#[cfg(target_os = "linux")]
fn the_gpu_jpeg_engines_encode_from_the_frame() {
    for name in ["nvjpeg", "vaapi"] {
        let home = Home::new(&format!("copies-{name}"));
        let engine = [("CAPTUREFAB_JPEG", name)];
        let settings = settings("RGB8");
        let settings: Vec<&str> = settings.iter().map(String::as_str).collect();
        let probe = home.run_env(
            &engine,
            &[
                &["--json", "capture", "-c", "sim:0", "-o", "probe.jpg"][..],
                &settings,
            ]
            .concat(),
        );
        if probe.code != Some(0) {
            eprintln!("no {name} JPEG engine here; skipped: {}", probe.stdout);
            continue;
        }
        for format in ["RGB8", "BayerRG8"] {
            let n = 20;
            let (delta, files) = capture_delta_env(&home, &engine, format, &["-f", "jpg"], n);
            let label = format!("capture -f jpg {format} ({name})");
            show(&label, &delta, n as i64);
            assert_no_avoidable(&label, &delta);
            assert_eq!(site(&delta, "file_write"), files, "{label}");
            // Frames are read where the camera wrote them: none is copied
            // for the helper once it runs.
            assert!(
                site(&delta, "gpu_upload") <= frame_bytes(format) as i64,
                "{label}: {delta}"
            );
        }
        assert!(helper_reports(&home) > 0, "{name}: no helper report");
    }
}

/// Whole-frame and region inspections of every format: measured where the
/// pixels lie and previewed a row at a time, nothing larger than the
/// preview raster is allocated.
#[test]
fn every_format_inspects_in_place() {
    let home = Home::new("copies-inspect-all");
    for format in FORMATS {
        for region in [None, Some("0.25,0.25,0.5,0.5")] {
            let delta = inspect_delta(&home, format, region, 2);
            let label = format!("inspect {format} {}", region.unwrap_or("whole"));
            show(&label, &delta, 2);
            assert_no_avoidable(&label, &delta);
            let preview = WIDTH * HEIGHT * 3 / 4;
            assert!(
                largest_class(&delta) <= preview.next_power_of_two() as i64,
                "{label}: {delta}"
            );
        }
    }
}

/// Capturefab's own auto exposure over a region meters the frames where
/// they lie.
#[test]
fn auto_exposure_meters_in_place() {
    let home = Home::new("copies-auto");
    for format in ["Mono12p", "BayerRG12p", "RGB8"] {
        // The simulator keeps settings between commands, as a camera does.
        let settings = settings(format);
        let values: Vec<&str> = settings
            .iter()
            .skip(1)
            .step_by(2)
            .map(String::as_str)
            .collect();
        home.ok(&[&["set", "-c", "sim:0"][..], &values].concat());
        let (report, _, _) = counted(
            &home,
            &format!("auto-{format}"),
            &[
                "auto",
                "-c",
                "sim:0",
                "--region",
                "0.25,0.25,0.5,0.5",
                "--timeout",
                "2s",
            ],
        );
        let label = format!("auto --region {format}");
        show(&label, &report, 1);
        assert!(report["frames"].as_i64().unwrap() > 0, "{label}: {report}");
        assert_no_avoidable(&label, &report);
    }
}

/// `record --for 2s` of each format: what one recorded frame costs, from
/// the frames the recording wrote.
#[test]
#[ignore = "needs FFmpeg"]
fn recordings_pipe_the_frame_or_unpack_it_once() {
    let home = Home::new("copies-record");
    for format in FORMATS {
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
        assert_no_avoidable(&format!("record {format}"), &report);
        // Packed and widened frames are converted on the writer thread,
        // once per frame written, and narrowed to 8 bits for the 8-bit
        // encoder on the way; the rest go to FFmpeg as they are.
        let converted = matches!(format, "Mono12p" | "BayerRG12" | "BayerRG12p");
        let unpacked = if converted { written } else { 0 };
        assert_eq!(
            calls(&report, "ffmpeg_unpack"),
            unpacked,
            "{format}: {report}"
        );
        if converted {
            let narrowed = (WIDTH * HEIGHT) as i64 * written;
            assert_eq!(
                site(&report, "ffmpeg_unpack"),
                narrowed,
                "{format}: {report}"
            );
            assert_eq!(site(&report, "pipe_write"), narrowed, "{format}: {report}");
        }
        // On Linux a frame's own bytes are spliced into the pipe, not
        // written: no copy on this side.
        let piped = calls(&report, "pipe_write") + calls(&report, "pipe_splice");
        assert_eq!(piped, written, "{format}: {report}");
        let spliced = calls(&report, "pipe_splice") > 0;
        assert_eq!(
            spliced,
            cfg!(target_os = "linux") && !converted,
            "{format}: {report}"
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
