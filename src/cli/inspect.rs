//! `capturefab inspect`: measure a fresh frame without saving a capture.
//! The camera's worker takes the frame, measures it off its camera thread
//! and writes the preview, by default to a private folder; this turns the
//! measurements into a short document with a verdict and a suggested
//! exposure.
use super::{Client, Global, args::CamerasArg, args::KeepArg, millis, output::Report, term};
use crate::{
    controls::Assignment,
    inspect::PreviewFormat,
    session::SessionCommand,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use serde_json::{Value, json};
use std::{fmt::Write as _, path::PathBuf, time::Duration};

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[command(flatten)]
    pub cameras: CamerasArg,
    /// Measure only this part of the frame: X,Y,W,H as fractions 0..1
    #[arg(long, value_name = "X,Y,W,H", value_parser = parse_region)]
    pub region: Option<[f64; 4]>,
    /// Where to write the preview (it must not exist), or none (default: a private folder doctor shows)
    #[arg(long, value_name = "PATH")]
    pub preview: Option<String>,
    /// The preview's longest edge in pixels, 64..2048
    #[arg(long, default_value_t = 768, value_name = "PX", value_parser = clap::value_parser!(u32).range(64..=2048))]
    pub size: u32,
    /// Frames to let pass after the first one exposed after the last settings change, 0..9
    #[arg(long, default_value_t = 0, value_name = "N", value_parser = clap::value_parser!(u32).range(0..=9))]
    pub fresh: u32,
    /// How long to wait for the frame, up to 10m (default: 5s, or 3 exposures and frame periods when longer)
    #[arg(long, value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Option<Duration>,
    /// How long a camera that sets its own exposure (webcam, stream, ExposureAuto=Continuous) may take to settle after it starts; 0 measures at once
    #[arg(long, default_value = "2s", value_name = "DUR", value_parser = super::args::duration)]
    pub warmup: Duration,
    /// Change settings first, as one atomic change (as set does), then measure; repeatable
    #[arg(long = "set", value_name = "NAME=VALUE")]
    pub settings: Vec<String>,
    /// Measure a frame every --every for this long (at most 50 samples), as for focusing: the series, and the sharpest frame's measurements and preview
    #[arg(long = "for", value_name = "DUR", value_parser = super::args::duration)]
    pub sample_for: Option<Duration>,
    /// Time between samples with --for (default 200ms)
    #[arg(long, value_name = "DUR", value_parser = super::args::duration, requires = "sample_for")]
    pub every: Option<Duration>,
    /// Share of the measured pixels at full scale above which the verdict says clipped_high (default: any pixel with --region, else 0.005)
    #[arg(long, value_name = "SHARE", value_parser = parse_share)]
    pub clip_threshold: Option<f64>,
    /// Draw the preview in this terminal under the measurements (kitty, iTerm2, or colour blocks)
    #[arg(long)]
    pub show: bool,
    #[command(flatten)]
    pub keep: KeepArg,
}

fn parse_share(text: &str) -> std::result::Result<f64, String> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|share| (0.0..=1.0).contains(share))
        .ok_or_else(|| "a share is a number from 0 to 1, such as 0.001".to_owned())
}

pub(super) fn parse_region(text: &str) -> std::result::Result<[f64; 4], String> {
    let parts = text
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| "a region is X,Y,W,H as fractions, such as 0.25,0.25,0.5,0.5".to_owned())?;
    let region: [f64; 4] = parts
        .try_into()
        .map_err(|_| "a region has four numbers: X,Y,W,H".to_owned())?;
    crate::inspect::validate(0, 1, 0, Some(region)).map_err(|error| error.to_string())?;
    Ok(region)
}

/// Brightness auto exposure aims for, as `auto` does.
const TARGET: f64 = 0.45;
/// The clipped share past which a frame says too little to suggest an
/// exact exposure.
const SATURATED: f64 = 0.25;

pub fn run(global: &Global, args: InspectArgs) -> Result<Report> {
    if args
        .timeout
        .is_some_and(|wait| wait.is_zero() || millis(wait) > crate::inspect::MAX_WAIT_MS)
    {
        return Err(Usage("--timeout must be longer than 0 and at most 10m".into()).into());
    }
    if millis(args.warmup) > crate::session::MAX_WARMUP_MS {
        return Err(Usage("--warmup is at most 60s".into()).into());
    }
    if let Some(duration) = args.sample_for {
        let every = args.every.unwrap_or(DEFAULT_EVERY);
        if every.is_zero() {
            return Err(Usage("--every must be longer than 0".into()).into());
        }
        let samples = millis(duration) / millis(every).max(1) + 1;
        if samples > MAX_SAMPLES {
            return Err(Usage(format!(
                "--for {} --every {} takes {samples} samples; at most {MAX_SAMPLES}: use --every {}",
                super::args::duration_text(duration),
                super::args::duration_text(every),
                super::args::duration_text(Duration::from_millis(
                    millis(duration).div_ceil(MAX_SAMPLES - 1)
                ))
            ))
            .into());
        }
    }
    let settings = args
        .settings
        .iter()
        .map(|setting| Assignment::parse(setting))
        .collect::<Result<Vec<_>>>()?;
    let preview = match args.preview.as_deref() {
        Some("none") => Preview::None,
        Some(path) if args.cameras.cameras.len() > 1 => {
            return Err(Usage(format!(
                "--preview {path} names one file; with several cameras keep the default folder or use --preview none"
            ))
            .into());
        }
        Some(path) => {
            let mut path = path.to_owned();
            super::paths::resolve_path(&mut path, &std::env::current_dir()?)
                .map_err(super::input)?;
            Preview::File(PathBuf::from(path))
        }
        None => Preview::Private,
    };
    if args.show && matches!(preview, Preview::None) {
        return Err(Usage("--show draws the preview; leave out --preview none".into()).into());
    }
    // A preview drawn here is PNG, which every terminal path reads.
    let show = args.show && !global.json && term::stdout_is_tty() && term::graphics().is_some();
    let cameras = match args.cameras.cameras.as_slice() {
        [] => vec![None],
        cameras => cameras.iter().map(|camera| Some(camera.as_str())).collect(),
    };
    let each = |camera: Option<&str>| -> Result<Value> {
        let client = Client::open_with(global, camera, args.keep.route())?;
        inspect(&client, &args, &settings, &preview, global.verbose, show)
    };
    let value = if cameras.len() == 1 {
        each(cameras[0])?
    } else {
        // Each camera's thread is part of this command: a shell's Ctrl-C
        // stops what it waits for too.
        let lineage = super::signals::Lineage::here();
        let results = std::thread::scope(|scope| {
            let threads = cameras
                .iter()
                .map(|camera| {
                    scope.spawn(move || {
                        lineage.enter();
                        each(*camera)
                    })
                })
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .map(|thread| {
                    thread
                        .join()
                        .unwrap_or_else(|_| Err(anyhow!("inspection thread failed")))
                })
                .collect::<Vec<_>>()
        });
        let mut shown = Vec::new();
        let mut first = None;
        for (result, camera) in results.into_iter().zip(&cameras) {
            match result {
                Ok(value) => shown.push(value),
                Err(error) => {
                    shown.push(json!({
                        "camera": camera.map(crate::media::redact_url),
                        "error": {
                            "code": crate::types::classify(&error).kind.code().0,
                            "message": crate::scrub::scrub(&format!("{error:#}")),
                        },
                    }));
                    first.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first {
            return Err(crate::types::with_details(
                error,
                json!({ "cameras": shown }),
            ));
        }
        json!({ "cameras": shown })
    };
    let shown = |value: &Value| {
        let mut shown = text(value);
        if show && let Some(preview) = value["preview"].as_str() {
            shown.push_str(&picture(std::path::Path::new(preview)));
        }
        shown
    };
    let text = match value["cameras"].as_array() {
        Some(cameras) => cameras.iter().map(shown).collect(),
        None => shown(&value),
    };
    Ok(Report::json(value).text(text))
}

/// The preview at `path` drawn for this terminal, under text of about six
/// lines; nothing when it cannot be read.
fn picture(path: &std::path::Path) -> String {
    let Some(graphics) = term::graphics() else {
        return String::new();
    };
    let (cols, rows) = term::picture_box(8);
    std::fs::read(path)
        .ok()
        .and_then(|bytes| term::picture(&bytes, graphics, cols, rows, None).ok())
        .map(|picture| picture.text)
        .unwrap_or_default()
}

enum Preview {
    None,
    Private,
    File(PathBuf),
}

fn inspect(
    client: &Client,
    args: &InspectArgs,
    settings: &[Assignment],
    preview: &Preview,
    verbose: bool,
    show: bool,
) -> Result<Value> {
    if !settings.is_empty() {
        client.call(SessionCommand::SetValues {
            values: settings.to_vec(),
            dry_run: false,
        })?;
    }
    let info = client.call(SessionCommand::Info)?;
    let format = match preview {
        Preview::File(path) => match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => PreviewFormat::Png,
            Some("jpg" | "jpeg") => PreviewFormat::Jpeg,
            _ => PreviewFormat::Auto,
        },
        _ if show => PreviewFormat::Png,
        _ => PreviewFormat::Auto,
    };
    let threshold = args.clip_threshold.unwrap_or(if args.region.is_some() {
        0.0
    } else {
        crate::inspect::CLIP_WARN
    });
    let camera = crate::media::redact_url(info["camera"].as_str().unwrap_or("the camera"));
    let frame = |fresh: u32, preview: Option<String>, max_dimension: u32| {
        let _waiting = term::activity(format!("waiting for a frame from {camera}…"));
        client
            .call(SessionCommand::Frame {
                // The worker counts the frame measured too.
                fresh: fresh + 1,
                timeout_ms: args.timeout.map_or(0, millis),
                max_dimension,
                format,
                region: args.region,
                preview,
                warmup_ms: Some(millis(args.warmup)),
            })
            .map_err(trigger_hint)
    };
    let size = if matches!(preview, Preview::None) {
        0
    } else {
        args.size
    };
    let Some(duration) = args.sample_for else {
        let reply = frame(
            args.fresh,
            match preview {
                Preview::File(path) => Some(path.to_string_lossy().into_owned()),
                _ => None,
            },
            size,
        )?;
        let mut value = document(&info, &reply, threshold, verbose);
        // The camera's worker wrote the preview where it was asked to go.
        if let Some(path) = reply["image"]["path"].as_str() {
            value["preview"] = json!(path);
        }
        return Ok(value);
    };
    // A series: the camera streams while it lasts, so each sample is the
    // next frame rather than a new start.
    let every = args.every.unwrap_or(DEFAULT_EVERY);
    let streaming = client.status()?["streaming"] == true;
    let started = !streaming && client.call(SessionCommand::Start).is_ok();
    let sampled = sample(duration, every, args.fresh, |fresh| {
        frame(fresh, None, size)
    });
    if started {
        stop_unless_busy(client)?;
    }
    let (series, best) = sampled?;
    let mut value = document(&info, &best.reply, threshold, verbose);
    value["series"] = json!(series);
    value["best"] = json!({"t": best.t, "sharpness": best.sharpness});
    if let Some(path) = best.reply["image"]["path"].as_str() {
        match preview {
            // The best frame's preview goes where it was asked to.
            Preview::File(file) => {
                let bytes = std::fs::read(path)?;
                crate::storage::save_one(&bytes, file)?;
                value["preview"] = json!(file);
            }
            _ => value["preview"] = json!(path),
        }
    }
    Ok(value)
}

/// Stop a camera this command started streaming, unless a recording or a
/// job now uses it.
pub(super) fn stop_unless_busy(client: &Client) -> Result<()> {
    let camera = client.status()?;
    let busy = camera["forwarding"].is_string()
        || super::jobs::of_camera(&camera)
            .iter()
            .any(|job| !super::jobs::ended(job));
    if !busy {
        client.call(SessionCommand::Stop)?;
    }
    Ok(())
}

/// How often a series samples by default.
const DEFAULT_EVERY: Duration = Duration::from_millis(200);
/// The most samples a series takes.
const MAX_SAMPLES: u64 = 50;

/// The sharpest sample of a series so far.
struct Best {
    t: f64,
    sharpness: f64,
    reply: Value,
}

/// Measure a frame every `every` for `duration`, a person moving the focus
/// meanwhile: each sample's time, sharpness and brightness, and the
/// sharpest one's measurements.
fn sample(
    duration: Duration,
    every: Duration,
    fresh: u32,
    mut frame: impl FnMut(u32) -> Result<Value>,
) -> Result<(Vec<Value>, Best)> {
    let began = std::time::Instant::now();
    let mut series = Vec::new();
    let mut best: Option<Best> = None;
    for index in 0..MAX_SAMPLES {
        let due = every.saturating_mul(u32::try_from(index).unwrap_or(u32::MAX));
        if due > duration {
            break;
        }
        super::signals::sleep(due.saturating_sub(began.elapsed()));
        if super::signals::interrupted() {
            return Err(
                Categorized::new(ErrorKind::Interrupted, "interrupted while sampling")
                    .details(json!({ "series": series }))
                    .into(),
            );
        }
        // The first sample lets --fresh frames pass; later ones take the
        // next frame.
        let reply = frame(if index == 0 { fresh } else { 0 })?;
        let t = (began.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        let sharpness = reply["focus"]["laplacian"].as_f64().unwrap_or(0.0);
        series.push(json!({
            "t": t,
            "sharpness": sharpness,
            "mean": reply["exposure"]["mean"],
        }));
        if best.as_ref().is_none_or(|best| sharpness > best.sharpness) {
            best = Some(Best {
                t,
                sharpness,
                reply,
            });
        }
    }
    let best = best.context("no sample was taken")?;
    Ok((series, best))
}

/// A trigger timeout says what inspect can do about it.
fn trigger_hint(error: anyhow::Error) -> anyhow::Error {
    let classified = crate::types::classify(&error);
    if classified.kind != ErrorKind::TriggerTimeout {
        return error;
    }
    let source = classified
        .details
        .as_ref()
        .and_then(|details| details["trigger"]["source"].as_str())
        .unwrap_or("its line")
        .to_owned();
    let mut error = Categorized::new(classified.kind, format!("{error:#}")).hint(format!(
        "send a trigger on {source}, wait longer with --timeout, or inspect with --set trigger=off"
    ));
    error.details = classified.details;
    error.into()
}

/// The compact document: what an agent needs to judge the frame and what to
/// change. Under -v, the histogram, channels and every focus metric too.
/// The shares of the measured pixels clipped high and low: counted over
/// every pixel at native bit depth when the worker could, else from the
/// sampled histogram.
fn clipped(reply: &Value) -> (f64, f64) {
    let clipping = &reply["clipping"];
    let exposure = &reply["exposure"];
    match clipping["pixels"].as_f64().filter(|pixels| *pixels > 0.0) {
        Some(pixels) => (
            clipping["high_px"].as_f64().unwrap_or(0.0) / pixels,
            clipping["low_px"].as_f64().unwrap_or(0.0) / pixels,
        ),
        None => (
            exposure["highlights"].as_f64().unwrap_or(0.0),
            exposure["shadows"].as_f64().unwrap_or(0.0),
        ),
    }
}

pub(super) fn document(info: &Value, reply: &Value, threshold: f64, verbose: bool) -> Value {
    let frame = &reply["frame"];
    let exposure = &reply["exposure"];
    let measured = exposure.is_object();
    let (clipped_high, clipped_low) = clipped(reply);
    let mut verdict = Vec::new();
    if !measured {
        verdict.push("not_measured");
    } else {
        let share = |key: &str| exposure[key].as_f64().unwrap_or(0.0);
        let mean = share("mean");
        if clipped_high > threshold {
            verdict.push("clipped_high");
        }
        if clipped_low > crate::inspect::CLIP_WARN {
            verdict.push("clipped_low");
        }
        if mean < crate::inspect::DARK {
            verdict.push("dark");
        } else if mean > crate::inspect::BRIGHT {
            verdict.push("bright");
        }
    }
    if reply["settled"] == false {
        verdict.push("not_settled");
    }
    if verdict.is_empty() {
        verdict.push("ok");
    }
    let mut value = json!({
        "camera": reply["camera"],
        "frame": {
            "width": frame["width"],
            "height": frame["height"],
            "pixel_format": frame["pixel_format"],
            "bits": info["bits"],
            "id": frame["id"].as_u64().filter(|id| *id > 0),
            "device_ts_ns": frame["device_ts_ns"],
            "time": frame["wall_ms"].as_u64().filter(|ms| *ms > 0).map(crate::scheduling::rfc3339),
            "age_ms": frame["age_ms"],
        },
        "verdict": verdict,
        "settings": settings(info),
    });
    if measured {
        let round = |share: f64| (share * 1e6).round() / 1e6;
        value["exposure"] = json!({
            "mean": exposure["mean"],
            "p1": exposure["p01"],
            "p50": exposure["p50"],
            "p99": exposure["p99"],
            "clipped_low": round(clipped_low),
            "clipped_high": round(clipped_high),
            "clipped_low_px": reply["clipping"]["low_px"],
            "clipped_high_px": reply["clipping"]["high_px"],
            "max": reply["clipping"]["max"],
        });
        if let Some(suggest) = suggest(info, reply) {
            value["suggest"] = suggest;
        }
    }
    if let Some(focus) = reply["focus"].as_object() {
        value["focus"] = json!({"sharpness": focus.get("laplacian")});
        if verbose {
            value["focus"] = Value::Object(focus.clone());
            value["focus"]["sharpness"] = focus.get("laplacian").cloned().unwrap_or_default();
        }
    }
    if reply["after_change"] == true {
        value["after_change"] = json!(true);
    }
    for key in ["frames", "dropped", "incomplete", "resent"] {
        if !reply[key].is_null() {
            value[key] = reply[key].clone();
        }
    }
    if let Some(ms) = reply["warmup_ms"].as_u64() {
        value["warmup_ms"] = json!(ms);
        if reply["settled"] == false {
            value["settled"] = json!(false);
        }
    }
    if reply["warnings"].is_array() {
        value["warnings"] = reply["warnings"].clone();
    }
    if verbose {
        value["histogram"] = exposure["histogram16"].clone();
        value["channels"] = exposure["channels"].clone();
        value["region"] = reply["region"].clone();
        value["notes"] = reply["notes"].clone();
    }
    crate::types::without_nulls(value)
}

/// The compact block `capture --inspect` attaches: the measured frame's
/// exposure, verdict, suggestion, focus and preview.
pub(super) fn compact(info: &Value, reply: &Value) -> Value {
    let document = document(info, reply, crate::inspect::CLIP_WARN, false);
    let mut value = json!({});
    for key in ["exposure", "verdict", "suggest", "focus"] {
        if let Some(field) = document.get(key) {
            value[key] = field.clone();
        }
    }
    if let Some(path) = reply["image"]["path"].as_str() {
        value["preview"] = json!(path);
    }
    value
}

/// The settings that shape exposure, from `info`.
fn settings(info: &Value) -> Value {
    crate::types::without_nulls(json!({
        "exposure_us": info["exposure_us"],
        "gain_db": info["gain_db"],
        "gain": info["gain"],
        "fps": info["fps"].as_f64().or(info["fps_limit"].as_f64()),
        "pixel_format": info["pixel_format"],
    }))
}

/// How far to move exposure, in stops and as an exposure time: toward the
/// target brightness without clipping the brightest one percent, within
/// the camera's exposure range. Measured at native bit depth when the
/// format allows. None when the frame is close enough.
fn suggest(info: &Value, reply: &Value) -> Option<Value> {
    let meter = &reply["auto_meter"];
    let exposure = &reply["exposure"];
    let mean = meter["mean"].as_f64().or(exposure["mean"].as_f64())?;
    let p99 = meter["p99"].as_f64().or(exposure["p99"].as_f64())?;
    let mut ev = crate::auto::error(crate::auto::Meter {
        mean: mean.max(1.0 / 1024.0),
        p99: p99.max(1.0 / 255.0),
    });
    // Clipped highlights are lost detail whatever the mean says: go at
    // least a third of a stop darker. Past saturation nothing shows how
    // much darker, so a mostly clipped frame steps further the more of it
    // is clipped (4 stops when all of it is), and the suggestion is a
    // bound to measure again.
    let (clipped, _) = clipped(reply);
    if clipped > crate::inspect::CLIP_WARN {
        ev = ev.min(-1.0 / 3.0);
    }
    let saturated = clipped > SATURATED;
    if saturated {
        ev = ev.min(-(1.0 + 3.0 * clipped.min(1.0)));
    }
    if !ev.is_finite() || ev.abs() < 0.1 || (mean - TARGET).abs() < 0.03 && ev.abs() < 0.25 {
        return None;
    }
    let mut value = json!({"ev": (ev * 100.0).round() / 100.0});
    if let Some(current) = info["exposure_us"].as_f64().filter(|us| *us > 0.0) {
        let control = &info["controls"]["exposure"];
        let scale = match control["unit"].as_str().map(str::trim) {
            Some("ms") => 1000.0,
            Some("s") => 1e6,
            Some("ns") => 1e-3,
            _ => 1.0,
        };
        let min = control["min"]
            .as_f64()
            .map(|min| min * scale)
            .unwrap_or(0.0);
        let max = control["max"]
            .as_f64()
            .map(|max| max * scale)
            .unwrap_or(f64::INFINITY);
        let wanted = (current * ev.exp2()).clamp(min, max.max(min));
        value["exposure_us"] = json!(wanted.round());
        if wanted >= max {
            value["limited_by"] = json!("exposure_max");
        } else if wanted <= min {
            value["limited_by"] = json!("exposure_min");
        }
    }
    if saturated && value.get("limited_by").is_none() {
        value["limited_by"] = json!("clipped");
    }
    Some(value)
}

fn text(value: &Value) -> String {
    if let Some(error) = value.get("error") {
        return format!(
            "{}: {}\n",
            value["camera"].as_str().unwrap_or("camera"),
            error["message"].as_str().unwrap_or_default()
        );
    }
    let mut text = format!(
        "{}  {}x{} {}  {}\n",
        value["camera"].as_str().unwrap_or_default(),
        value["frame"]["width"],
        value["frame"]["height"],
        value["frame"]["pixel_format"].as_str().unwrap_or_default(),
        value["verdict"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let exposure = &value["exposure"];
    if exposure.is_object() {
        let percent = |key: &str| exposure[key].as_f64().unwrap_or(0.0) * 100.0;
        let _ = writeln!(
            text,
            "  brightness mean {:.2}, p1 {:.2}, p99 {:.2}; clipped {:.1}% dark, {:.1}% bright",
            exposure["mean"].as_f64().unwrap_or(0.0),
            exposure["p1"].as_f64().unwrap_or(0.0),
            exposure["p99"].as_f64().unwrap_or(0.0),
            percent("clipped_low"),
            percent("clipped_high")
        );
    }
    if let Some(suggest) = value.get("suggest") {
        let camera = crate::hint::shell_quote(value["camera"].as_str().unwrap_or_default());
        let _ = writeln!(
            text,
            "  suggest {:+} EV{}",
            suggest["ev"].as_f64().unwrap_or(0.0),
            suggest["exposure_us"]
                .as_f64()
                .map(|us| format!(": capturefab set -c {camera} exposure={}us", us.round()))
                .unwrap_or_default()
        );
    }
    if let Some(sharpness) = value["focus"]["sharpness"].as_f64() {
        let _ = writeln!(text, "  sharpness {sharpness}");
    }
    if let Some(preview) = value["preview"].as_str() {
        let _ = writeln!(text, "  preview {preview}");
    }
    text
}

/// Result fields, for `schema`.
pub const FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    (
        "frame",
        "{width,height,pixel_format,bits?,id?,device_ts_ns?,time,age_ms} — the frame measured, taken after the command began: its block ID and the camera's clock (ns, never a date) where the transport has them, and when it arrived",
    ),
    (
        "exposure",
        "{mean,p1,p50,p99,clipped_low,clipped_high,clipped_low_px?,clipped_high_px?,max?} — brightness 0..1; shares and counts of pixels at black and at full scale, counted over every pixel of the region at the camera's bit depth; max: the brightest sample as a share of full scale",
    ),
    (
        "verdict",
        "[ok|dark|bright|clipped_high|clipped_low|not_settled|not_measured]",
    ),
    (
        "suggest",
        "{ev,exposure_us?,limited_by?:exposure_max|exposure_min|clipped} — how far to move exposure, in stops, toward mid-grey without clipping highlights; absent when close enough. clipped: most of the frame is clipped, so this is a first step; inspect again",
    ),
    (
        "focus",
        "{sharpness} — variance of the Laplacian over the region; compare between frames of one camera and scene (-v adds tenengrad, brenner, variance)",
    ),
    (
        "settings",
        "{exposure_us?,gain_db?,gain?,fps?,pixel_format?} — in effect for the frame",
    ),
    (
        "preview",
        "string — the preview image's path; absent with --preview none",
    ),
    (
        "after_change",
        "true — frames exposed before the camera's last settings change were let pass",
    ),
    (
        "frames",
        "int — frames received for this inspection, the one measured included",
    ),
    (
        "dropped",
        "int — frames the transport lost meanwhile, where it counts them",
    ),
    (
        "incomplete",
        "int — frames that arrived incomplete meanwhile, where the transport counts them",
    ),
    (
        "resent",
        "int — packets the camera sent again meanwhile, where the transport counts them",
    ),
    (
        "warmup_ms",
        "int — how long a camera that sets its own exposure took to settle after it started",
    ),
    (
        "settled",
        "false — its exposure was still changing when --warmup ran out (verdict not_settled, with a warning)",
    ),
    (
        "warnings",
        "[string] — such as a camera still settling when --warmup ran out (verdict not_settled)",
    ),
    (
        "series",
        "[{t,sharpness,mean}] — with --for: every sample, t in seconds from the first; the other fields describe the sharpest frame",
    ),
    ("best", "{t,sharpness} — with --for: the sharpest sample"),
    ("histogram", "[16 ints] — with -v: luminance histogram"),
    ("channels", "{red,green,blue} — with -v, for color"),
    ("region", "{x,y,width,height,normalized} — with -v"),
    ("notes", "[string] — with -v"),
    (
        "cameras",
        "[{camera,frame,exposure?,verdict,suggest?,focus?,settings,preview?,after_change?,frames?,dropped?,incomplete?,resent?,warmup_ms?,settled?,warnings?,series?,best?,histogram?,channels?,region?,notes?}] — with -c repeated",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_aim_for_mid_grey_within_the_exposure_range() {
        let info = json!({"exposure_us": 1000.0, "controls": {"exposure": {"unit": "us", "min": 10.0, "max": 1500.0}}});
        let dark = json!({"exposure": {"mean": 0.1125, "p99": 0.2}});
        let suggested = suggest(&info, &dark).unwrap();
        assert_eq!(suggested["ev"], 2.0);
        assert_eq!(suggested["exposure_us"], 1500.0);
        assert_eq!(suggested["limited_by"], "exposure_max");
        // Highlights near clipping hold exposure back.
        let bright = json!({"exposure": {"mean": 0.45, "p99": 1.0, "highlights": 0.02}});
        assert!(suggest(&info, &bright).unwrap()["ev"].as_f64().unwrap() <= -0.33);
        let fine = json!({"exposure": {"mean": 0.45, "p99": 0.8}});
        assert!(suggest(&info, &fine).is_none());
        // A saturated frame cannot say how far to go: a big first step.
        let info = json!({"exposure_us": 1e6, "controls": {"exposure": {"unit": "us", "min": 10.0, "max": 2e6}}});
        let white = json!({"exposure": {"mean": 1.0, "p99": 1.0, "highlights": 1.0}});
        let suggested = suggest(&info, &white).unwrap();
        assert_eq!(suggested["ev"], -4.0);
        assert_eq!(suggested["exposure_us"], 62500.0);
        assert_eq!(suggested["limited_by"], "clipped");
    }

    #[test]
    fn the_suggested_command_names_its_camera_ready_to_paste() {
        let inspected = |camera: &str| {
            text(&json!({
                "camera": camera,
                "frame": {"width": 64, "height": 48, "pixel_format": "Mono8"},
                "verdict": ["dark"],
                "suggest": {"ev": 2.0, "exposure_us": 1500.0},
            }))
        };
        assert!(
            inspected("sim:0").contains("suggest +2 EV: capturefab set -c sim:0 exposure=1500us\n")
        );
        assert!(
            inspected("avfoundation:HD Camera")
                .contains("capturefab set -c 'avfoundation:HD Camera' exposure=1500us")
        );
    }

    #[test]
    fn regions_are_four_fractions() {
        assert_eq!(
            parse_region("0.25,0.25,0.5,0.5").unwrap(),
            [0.25, 0.25, 0.5, 0.5]
        );
        for bad in ["0.5,0.5", "a,b,c,d", "0.5,0.5,0.6,0.1", "0,0,0,1"] {
            assert!(parse_region(bad).is_err(), "{bad}");
        }
    }
}
