//! `capturefab inspect`: measure a fresh frame without saving a capture.
//! The session coordinator takes the frame and measures it, off the camera
//! thread; this turns the measurements into a short document with a
//! verdict, a suggested exposure and a preview image in a private folder.
use super::{Client, Global, args::CamerasArg, args::KeepArg, output::Report};
use crate::{controls::Assignment, inspect::PreviewFormat, session::SessionCommand, types::Usage};
use anyhow::{Context, Result, anyhow};
use clap::Args;
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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

/// The most previews the private folder keeps, and their total size.
const MAX_IMAGES: usize = 64;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);
/// How long the frame may take to arrive.
const WAIT: Duration = Duration::from_secs(5);
/// Brightness auto exposure aims for, as `auto` does.
const TARGET: f64 = 0.45;

/// Where previews go unless --preview says otherwise: private, bounded,
/// and shown by doctor, so a person can allow reading it once.
pub(crate) fn preview_dir() -> PathBuf {
    crate::ipc::runtime_dir().join("inspect")
}

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
    let reply = client.call(SessionCommand::Frame {
        fresh: args.fresh,
        timeout_ms: super::millis(WAIT),
        max_dimension: if matches!(preview, Preview::None) {
            0
        } else {
            args.size
        },
        format,
        region: args.region,
    })?;
    let mut value = document(&info, &reply, verbose);
    if let Some(image) = reply.get("image").filter(|image| image.is_object()) {
        let path = deliver(image, preview, &info)?;
        if let Some(path) = path {
            value["preview"] = json!(path);
        }
    }
    Ok(value)
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
    // least a third of a stop darker.
    if exposure["highlights"].as_f64().unwrap_or(0.0) > crate::inspect::CLIP_WARN {
        ev = ev.min(-1.0 / 3.0);
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
    Some(value)
}

/// Write the preview where it was asked to go: its path.
fn deliver(image: &Value, preview: &Preview, info: &Value) -> Result<Option<PathBuf>> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image["base64"].as_str().context("preview without data")?)
        .context("preview data")?;
    let extension = if image["mime"] == "image/jpeg" {
        "jpg"
    } else {
        "png"
    };
    Ok(match preview {
        Preview::None => None,
        Preview::Private => Some(private_image(
            &preview_dir(),
            &safe_label(info["camera"].as_str().unwrap_or("camera")),
            &bytes,
            extension,
        )?),
        Preview::File(path) => {
            crate::storage::save_bytes(&bytes, path, &super::capture::storage_policy())?;
            Some(path.clone())
        }
    })
}

/// A file name part made of a camera ID: credentials redacted first, then
/// anything but letters, digits, - and _ replaced.
fn safe_label(value: &str) -> String {
    // Redact before replacing punctuation: otherwise a URL password loses
    // its URL shape and would survive into the name.
    let value = crate::scrub::redact_urls(&crate::media::redact_url(value));
    let label: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if label.is_empty() {
        "camera".into()
    } else {
        label
    }
}

/// Write `bytes` as a new private file in `dir` (0700, files 0600, never a
/// symlinked folder), then keep only the newest `MAX_IMAGES` files and
/// `MAX_IMAGE_BYTES` there.
fn private_image(dir: &Path, label: &str, bytes: &[u8], extension: &str) -> Result<PathBuf> {
    crate::ipc::ensure_private_dir(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for _ in 0..100 {
        let seq = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{label}-{stamp}-{}-{seq}.{extension}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes) {
                    let _ = fs::remove_file(&path);
                    return Err(error.into());
                }
                drop(file);
                prune_images(dir, Some(&path))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!("no free preview name in {}", dir.display()))
}

/// Remove all but the newest previews, never `keep`.
fn prune_images(dir: &Path, keep: Option<&Path>) -> Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_file() && !meta.file_type().is_symlink() {
            files.push((
                entry.path(),
                meta.modified().unwrap_or(UNIX_EPOCH),
                meta.len(),
            ));
        }
    }
    files.sort_by(|a, b| {
        (keep == Some(b.0.as_path()))
            .cmp(&(keep == Some(a.0.as_path())))
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.0.cmp(&a.0))
    });
    let mut bytes = 0u64;
    for (index, (path, _, size)) in files.into_iter().enumerate() {
        bytes = bytes.saturating_add(size);
        if index >= MAX_IMAGES || bytes > MAX_IMAGE_BYTES {
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
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
        "{ev,exposure_us?,limited_by?} — how far to move exposure, in stops, toward mid-grey without clipping highlights; absent when close enough",
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
    fn private_images_are_bounded_private_and_never_follow_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-inspect-images-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for index in 0..MAX_IMAGES + 3 {
            private_image(&root, &format!("camera-{index}"), b"jpeg", "jpg").unwrap();
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), MAX_IMAGES);
        // Sparse files exercise the byte limit without consuming 80 MiB
        // on the shared build disk.
        for index in 0..2 {
            let path = private_image(&root, &format!("large-{index}"), b"jpeg", "jpg").unwrap();
            OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_len(40 * 1024 * 1024)
                .unwrap();
        }
        prune_images(&root, None).unwrap();
        let size: u64 = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(size <= MAX_IMAGE_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for file in fs::read_dir(&root).unwrap() {
                assert_eq!(
                    file.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let link = root.with_extension("link");
            symlink(&root, &link).unwrap();
            assert!(private_image(&link, "camera", b"jpeg", "jpg").is_err());
            fs::remove_file(link).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_names_carry_no_credentials() {
        for camera in [
            "rtsp://alice:private-password@host/path",
            "rtsp://host/path?password=private-password",
        ] {
            let label = safe_label(camera);
            assert!(!label.contains("private-password"), "{label}");
            assert!(
                !label.contains("alice") || camera.contains("alice"),
                "{label}"
            );
        }
        assert_eq!(safe_label(""), "camera");
    }

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
