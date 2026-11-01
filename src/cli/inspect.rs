//! `capturefab inspect`: measure a fresh frame without saving a capture.
//! The camera's worker takes the frame, measures it off its camera thread
//! and writes the preview, by default to a private folder; this turns the
//! measurements into a short document with a verdict and a suggested
//! exposure.
use super::{Client, Global, args::CamerasArg, args::KeepArg, output::Report};
use crate::{
    controls::Assignment,
    inspect::PreviewFormat,
    session::SessionCommand,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Result, anyhow};
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
    /// Frames to let pass before the one measured, 0..10 (1: a frame exposed after this command began)
    #[arg(long, default_value_t = 1, value_name = "N", value_parser = clap::value_parser!(u32).range(0..=10))]
    pub fresh: u32,
    /// Change settings first, as one atomic change (as set does), then measure; repeatable
    #[arg(long = "set", value_name = "NAME=VALUE")]
    pub settings: Vec<String>,
    #[command(flatten)]
    pub keep: KeepArg,
}

fn parse_region(text: &str) -> std::result::Result<[f64; 4], String> {
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

/// How long the frame may take to arrive.
const WAIT: Duration = Duration::from_secs(5);
/// Brightness auto exposure aims for, as `auto` does.
const TARGET: f64 = 0.45;
/// The clipped share past which a frame says too little to suggest an
/// exact exposure.
const SATURATED: f64 = 0.25;

pub fn run(global: &Global, args: InspectArgs) -> Result<Report> {
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
    let cameras = match args.cameras.cameras.as_slice() {
        [] => vec![None],
        cameras => cameras.iter().map(|camera| Some(camera.as_str())).collect(),
    };
    let each = |camera: Option<&str>| -> Result<Value> {
        let client = Client::open_with(global, camera, args.keep.route())?;
        inspect(&client, &args, &settings, &preview, global.verbose)
    };
    let value = if cameras.len() == 1 {
        each(cameras[0])?
    } else {
        let results = std::thread::scope(|scope| {
            let threads = cameras
                .iter()
                .map(|camera| scope.spawn(move || each(*camera)))
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
    let text = match value["cameras"].as_array() {
        Some(cameras) => cameras.iter().map(text).collect(),
        None => text(&value),
    };
    Ok(Report::json(value).text(text))
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
        _ => PreviewFormat::Auto,
    };
    let reply = client
        .call(SessionCommand::Frame {
            fresh: args.fresh,
            timeout_ms: super::millis(WAIT),
            max_dimension: if matches!(preview, Preview::None) {
                0
            } else {
                args.size
            },
            format,
            region: args.region,
            preview: match preview {
                Preview::File(path) => Some(path.to_string_lossy().into_owned()),
                _ => None,
            },
        })
        .map_err(trigger_hint)?;
    let mut value = document(&info, &reply, verbose);
    // The camera's worker wrote the preview where it was asked to go.
    if let Some(path) = reply["image"]["path"].as_str() {
        value["preview"] = json!(path);
    }
    Ok(value)
}

/// A trigger timeout says what inspect can do about it: it has no
/// --timeout of its own.
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
        "send a trigger on {source} within {}, or inspect with --set trigger=off",
        super::args::duration_text(WAIT)
    ));
    error.details = classified.details;
    error.into()
}

/// The compact document: what an agent needs to judge the frame and what to
/// change. Under -v, the histogram, channels and every focus metric too.
fn document(info: &Value, reply: &Value, verbose: bool) -> Value {
    let frame = &reply["frame"];
    let exposure = &reply["exposure"];
    let measured = exposure.is_object();
    let mut verdict = Vec::new();
    if !measured {
        verdict.push("not_measured");
    } else {
        let share = |key: &str| exposure[key].as_f64().unwrap_or(0.0);
        let mean = share("mean");
        if share("highlights") > crate::inspect::CLIP_WARN {
            verdict.push("clipped_high");
        }
        if share("shadows") > crate::inspect::CLIP_WARN {
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
            "age_ms": frame["age_ms"],
        },
        "verdict": verdict,
        "settings": settings(info),
    });
    if measured {
        value["exposure"] = json!({
            "mean": exposure["mean"],
            "p1": exposure["p01"],
            "p50": exposure["p50"],
            "p99": exposure["p99"],
            "clipped_low": exposure["shadows"],
            "clipped_high": exposure["highlights"],
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
    if verbose {
        value["histogram"] = exposure["histogram16"].clone();
        value["channels"] = exposure["channels"].clone();
        value["region"] = reply["region"].clone();
        value["notes"] = reply["notes"].clone();
    }
    crate::types::without_nulls(value)
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
    let clipped = exposure["highlights"].as_f64().unwrap_or(0.0);
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
        let _ = writeln!(
            text,
            "  suggest {:+} EV{}",
            suggest["ev"].as_f64().unwrap_or(0.0),
            suggest["exposure_us"]
                .as_f64()
                .map(|us| format!(": capturefab set exposure={}us", us.round()))
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
        "{width,height,pixel_format,bits?,age_ms} — the frame measured, taken after the command began",
    ),
    (
        "exposure",
        "{mean,p1,p50,p99,clipped_low,clipped_high} — brightness 0..1 and shares of pixels at black and full scale",
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
    ("histogram", "[16 ints] — with -v: luminance histogram"),
    ("channels", "{red,green,blue} — with -v, for color"),
    ("region", "{x,y,width,height,normalized} — with -v"),
    ("notes", "[string] — with -v"),
    (
        "cameras",
        "[{camera,frame,exposure?,verdict,suggest?,focus?,settings,preview?,histogram?,channels?,region?,notes?}] — with -c repeated",
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
