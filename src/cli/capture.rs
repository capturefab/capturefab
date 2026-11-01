//! `capturefab capture`: one shot, a burst, a time lapse or a delayed
//! capture, from one camera or several at once. Every capture to files runs
//! as a job in the camera's worker, direct or in a session, so the command
//! blocks until it ends, or with `--background` leaves it running.
use super::{
    Client, Global, Route,
    args::{CamerasArg, KeepArg, Wait},
    jobs, millis,
    output::Report,
    signals,
};
use crate::{
    controls::Assignment,
    session::SessionCommand,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    io::Write as _,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Png,
    #[value(alias = "jpeg")]
    Jpg,
    Raw,
}
impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpg => "jpeg",
            Self::Raw => "raw",
        }
    }
}

#[derive(Debug, Args)]
pub struct CaptureArgs {
    #[command(flatten)]
    pub cameras: CamerasArg,
    /// File, folder (existing, or ending in /) or template with {camera} {serial} {seq} {seq:N} {time} {date} {tag:KEY} {ext}; - writes one image to stdout (default: this folder, as {camera}-{time}-{seq}.png)
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: Option<String>,
    /// Frames to save, 1..100000
    #[arg(short = 'n', long = "count", value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100_000))]
    pub count: Option<u32>,
    /// Time between frames of a time lapse, with -n or --for
    #[arg(long, value_name = "DUR", value_parser = super::args::duration)]
    pub every: Option<Duration>,
    /// How long a time lapse runs, with --every
    #[arg(long = "for", value_name = "DUR", value_parser = super::args::duration)]
    pub duration: Option<Duration>,
    /// Begin at a time: HH:MM[:SS] here, or RFC 3339
    #[arg(long, value_name = "TIME", conflicts_with = "after", value_parser = super::args::time)]
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    /// Begin after a delay
    #[arg(long = "in", value_name = "DUR", value_parser = super::args::duration)]
    pub after: Option<Duration>,
    /// Image format when -o has no extension: png (default), jpg or raw
    #[arg(short = 'f', long, value_enum)]
    pub format: Option<Format>,
    /// JPEG quality, 1..100 (default 90)
    #[arg(long, value_name = "1-100", value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: Option<u8>,
    /// Change settings first, as one atomic change (as set does); repeatable
    #[arg(long = "set", value_name = "NAME=VALUE")]
    pub settings: Vec<String>,
    /// Settle exposure and gain first, then hold them (as auto does)
    #[arg(long)]
    pub auto: bool,
    /// Tag the files: {tag:KEY} in -o, and in each image's metadata; repeatable
    #[arg(long = "tag", value_name = "KEY=VALUE", value_parser = crate::context::parse_pair)]
    pub tags: Vec<(String, String)>,
    /// How long to wait for each frame; none waits for a trigger until stopped
    #[arg(long, default_value = "5s", value_name = "DUR", value_parser = super::args::wait)]
    pub timeout: Wait,
    /// When a file name without {seq} exists: fail, or number (-0001, -0002, ...)
    #[arg(long, value_enum, default_value_t = crate::naming::OnExists::Fail)]
    pub on_exists: crate::naming::OnExists,
    /// Leave the capture running in a session and print its job at once
    #[arg(long)]
    pub background: bool,
    #[command(flatten)]
    pub keep: KeepArg,
    /// Show the camera, files, settings and timing; capture nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// What every camera of one `capture` does, checked before any camera is
/// opened.
struct Plan {
    /// Absolute, as the caller meant it; or `-`.
    output: String,
    format: &'static str,
    count: u32,
    interval_ms: u64,
    /// None: now.
    first_at_ms: Option<u64>,
    timeout_ms: u64,
    settings: Vec<Assignment>,
    context: crate::context::Context,
}

impl Plan {
    fn new(args: &CaptureArgs) -> Result<Self> {
        let output = match args.output.as_deref() {
            Some("-") => "-".to_owned(),
            Some(output) => {
                crate::naming::validate_output(output).map_err(super::input)?;
                let mut output = output.to_owned();
                super::paths::resolve_path(&mut output, &std::env::current_dir()?)
                    .map_err(super::input)?;
                output
            }
            // This folder: numbered files in it.
            None => format!(
                "{}{}",
                std::env::current_dir()?.display(),
                std::path::MAIN_SEPARATOR
            ),
        };
        let format = format(&output, args.format)?;
        let count = match (args.count, args.every, args.duration) {
            (_, None, Some(_)) => {
                return Err(
                    Usage("--for needs --every: capture --every 10s --for 1h".into()).into(),
                );
            }
            (Some(_), _, Some(_)) => {
                return Err(Usage("give -n or --for, not both".into()).into());
            }
            (None, Some(_), None) => {
                return Err(Usage(
                    "--every needs -n or --for to know when the time lapse ends".into(),
                )
                .into());
            }
            (count, _, None) => count.unwrap_or(1),
            (None, Some(every), Some(duration)) => {
                crate::scheduling::count_for(millis(duration), millis(every).max(1))
                    .map_err(super::input)?
            }
        };
        for duration in [args.every, args.after].into_iter().flatten() {
            if duration.subsec_nanos() % 1_000_000 != 0 {
                return Err(Usage("--every and --in take whole milliseconds".into()).into());
            }
        }
        let now = crate::scheduling::now_ms();
        let first_at_ms = match (args.at, args.after) {
            (Some(at), _) => {
                let at = u64::try_from(at.timestamp_millis()).unwrap_or(0);
                if at < now {
                    return Err(Usage(format!("--at {} has passed", jobs_time(at))).into());
                }
                Some(at)
            }
            (None, Some(after)) => Some(now.saturating_add(millis(after))),
            (None, None) => None,
        };
        let settings = args
            .settings
            .iter()
            .map(|setting| Assignment::parse(setting))
            .collect::<Result<Vec<_>>>()?;
        let context = crate::context::from_pairs(&args.tags).map_err(super::input)?;
        let timeout_ms = match args.timeout {
            Wait::For(wait) if wait.is_zero() => {
                return Err(
                    Usage("--timeout must be longer than 0; none waits for ever".into()).into(),
                );
            }
            Wait::For(wait) => millis(wait).max(1),
            Wait::Forever => crate::scheduling::MAX_DURATION_MS,
        };
        let plan = Self {
            output,
            format,
            count,
            interval_ms: args.every.map(millis).unwrap_or(0),
            first_at_ms,
            timeout_ms,
            settings,
            context,
        };
        if plan.output != "-" {
            crate::engine::check_capture_name(&plan.output, count, format)?;
            plan.request(args, None).validate().map_err(super::input)?;
            crate::engine::preflight_capture_output(&plan.output, count, format, args.on_exists)
                .map_err(exists_hint)?;
        }
        Ok(plan)
    }
    /// The job request for one camera.
    fn request(&self, args: &CaptureArgs, quality: Option<u8>) -> SessionCommand {
        SessionCommand::Schedule {
            output: self.output.clone(),
            count: self.count,
            timeout_ms: self.timeout_ms,
            format: self.format.into(),
            first_at_ms: self.first_at_ms.unwrap_or_else(crate::scheduling::now_ms),
            interval_ms: self.interval_ms,
            storage: storage_policy(),
            on_exists: args.on_exists,
            context: self.context.clone(),
            utc_offset_s: Some(crate::naming::local_offset()),
            quality,
            job: None,
        }
    }
}

/// The storage budget captures count against until it is replaced.
pub(super) fn storage_policy() -> crate::storage::StoragePolicy {
    crate::storage::configured_policy().unwrap_or_default()
}

fn jobs_time(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(i64::try_from(ms).unwrap_or(i64::MAX))
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The image format: the output's extension names it; `-f` only when there
/// is none, and a different one is a mistake.
fn format(output: &str, flag: Option<Format>) -> Result<&'static str> {
    let named = (output != "-")
        .then(|| crate::naming::image_extension(output))
        .flatten()
        .map(|(_, format)| format);
    match (named, flag) {
        (Some(named), Some(flag)) if named != flag.name() => Err(Usage(
            crate::naming::format_mismatch(output, flag.name())
                .unwrap_or_else(|| format!("-o names a {named} file but -f is {}", flag.name())),
        )
        .into()),
        (Some(named), _) => Ok(named),
        (None, Some(flag)) => Ok(flag.name()),
        (None, None) => {
            let last = output.rsplit(std::path::is_separator).next().unwrap_or("");
            match last.rsplit_once('.') {
                Some((_, extension))
                    if !extension.contains(['{', '}'])
                        && !std::path::Path::new(output).is_dir() =>
                {
                    Err(Usage(format!(
                        "cannot tell an image format from .{extension}; use .png, .jpg or .raw, or -f"
                    ))
                    .into())
                }
                _ => Ok("png"),
            }
        }
    }
}

/// A taken file name, with how to keep both.
fn exists_hint(error: anyhow::Error) -> anyhow::Error {
    let classified = crate::types::classify(&error);
    if classified.kind != ErrorKind::Exists || classified.hint.is_some() {
        return error;
    }
    let mut taken = Categorized::new(ErrorKind::Exists, format!("{error:#}"))
        .hint("Capturefab never overwrites files: add {seq} to -o, or pass --on-exists number");
    taken.details = classified.details;
    taken.into()
}

pub fn run(global: &Global, args: CaptureArgs) -> Result<Option<Report>> {
    if args.output.as_deref() == Some("-") {
        // Stdout is for the image alone.
        super::output::document_to_stderr();
    }
    let plan = Plan::new(&args)?;
    if args.quality.is_some() && plan.format != "jpeg" {
        return Err(Usage("--quality is for JPEG; use a .jpg name or -f jpg".into()).into());
    }
    if plan.output == "-" {
        return to_stdout(global, &args, &plan).map(|()| None);
    }
    let cameras = match args.cameras.cameras.as_slice() {
        [] => vec![None],
        cameras => cameras.iter().map(|camera| Some(camera.as_str())).collect(),
    };
    if cameras.len() > 1
        && matches!(
            crate::naming::Plan::new(&plan.output, plan.count, plan.format),
            crate::naming::Plan::File(_)
        )
    {
        return Err(Usage(
            "several cameras need {camera} in -o, or a folder, so each saves its own files".into(),
        )
        .into());
    }
    signals::take_over();
    // A dry run uses a session that already holds the camera, but never
    // starts one.
    let route = if args.dry_run {
        Route::default()
    } else {
        Route {
            keep: args.keep.route().keep,
            background: args.background,
            connect: None,
        }
    }
    .waiting(args.timeout);
    let each = |camera: Option<&str>| -> Result<Value> {
        let client = Client::open_with(global, camera, route)?;
        if args.dry_run {
            dry_run(&client, &args, &plan)
        } else {
            shoot(&client, &args, &plan)
        }
    };
    let results = if cameras.len() == 1 {
        vec![each(cameras[0])]
    } else {
        std::thread::scope(|scope| {
            let threads = cameras
                .iter()
                .map(|camera| scope.spawn(move || each(*camera)))
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .map(|thread| {
                    thread
                        .join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("capture thread failed")))
                })
                .collect()
        })
    };
    let report = combine(results, &cameras)?;
    let text = text(&report);
    Ok(Some(Report::json(report).text(text)))
}

/// One camera's result as the document, or several as `{cameras}` (or
/// `{jobs}` in the background). A failure carries every camera's result or
/// error in details, each with what it saved, and every file saved in
/// `files`.
fn combine(results: Vec<Result<Value>>, cameras: &[Option<&str>]) -> Result<Value> {
    if results.len() == 1 {
        return results.into_iter().next().unwrap_or_else(|| unreachable!());
    }
    let mut first = None;
    let mut shown = Vec::new();
    let mut files = Vec::new();
    let mut truncated = 0;
    let saved = |value: &Value, files: &mut Vec<Value>, truncated: &mut u64| {
        files.extend(value["files"].as_array().cloned().unwrap_or_default());
        *truncated += value["truncated"].as_u64().unwrap_or(0);
    };
    for (result, camera) in results.into_iter().zip(cameras) {
        match result {
            Ok(value) => {
                saved(&value, &mut files, &mut truncated);
                shown.push(value);
            }
            Err(error) => {
                let classified = crate::types::classify(&error);
                let mut entry = json!({
                    "camera": camera.map(crate::media::redact_url),
                    "error": {"code": classified.kind.code().0, "message": crate::scrub::scrub(&format!("{error:#}"))},
                });
                if let Some(details) = &classified.details {
                    saved(details, &mut files, &mut truncated);
                    for key in ["files", "truncated", "job"] {
                        if let Some(value) = details.get(key) {
                            entry[key] = value.clone();
                        }
                    }
                }
                shown.push(entry);
                first.get_or_insert(error);
            }
        }
    }
    match first {
        Some(error) => {
            let classified = crate::types::classify(&error);
            let mut details = json!({ "cameras": shown, "files": files });
            if truncated > 0 {
                details["truncated"] = json!(truncated);
            }
            let mut combined =
                Categorized::new(classified.kind, format!("{error:#}")).details(details);
            combined.hint = classified.hint;
            Err(combined.into())
        }
        None if shown.iter().all(|value| value.get("job").is_some()) => Ok(json!({
            "jobs": shown.into_iter().map(|value| value["job"].clone()).collect::<Vec<_>>()
        })),
        None => Ok(json!({ "cameras": shown })),
    }
}

/// Change settings and settle exposure, as asked, before frames are taken.
fn prepare(client: &Client, args: &CaptureArgs, plan: &Plan) -> Result<()> {
    if !plan.settings.is_empty() {
        client.call(SessionCommand::SetValues {
            values: plan.settings.clone(),
            dry_run: false,
        })?;
    }
    if args.auto {
        super::auto::settle(client, Duration::from_secs(5))?;
    }
    if signals::interrupted() {
        return Err(Categorized::error(
            ErrorKind::Interrupted,
            "interrupted before capturing",
        ));
    }
    Ok(())
}

/// Run the capture job on one camera and wait for it, or leave it running.
fn shoot(client: &Client, args: &CaptureArgs, plan: &Plan) -> Result<Value> {
    prepare(client, args, plan)?;
    let reply = client.call(plan.request(args, args.quality))?;
    let id = reply["job"]["id"]
        .as_u64()
        .context("capture job without ID")?;
    let session = client.session().unwrap_or("direct").to_owned();
    if args.background {
        return started(client, &session, id, plan);
    }
    let mut pause = Duration::from_millis(10);
    let mut stopping: Option<Instant> = None;
    loop {
        let camera = client.status()?;
        let job = find(&camera, id).context("the capture job is gone")?;
        let active = matches!(job["status"].as_str(), Some("pending" | "running"));
        if !active {
            return finished(&session, &camera, &job, stopping.is_some());
        }
        if signals::interrupted() {
            match stopping {
                None => {
                    // The frame being saved is kept; nothing after it starts.
                    let _ = client.call(SessionCommand::CancelJob { id });
                    stopping = Some(Instant::now());
                }
                Some(since) if since.elapsed() > Duration::from_secs(2) => {
                    return Err(interrupted(&session, &camera, &job));
                }
                Some(_) => {}
            }
        }
        // Look again soon for a quick shot, less often for a long job.
        std::thread::sleep(pause);
        pause = (pause * 3 / 2).min(Duration::from_millis(250));
    }
}

/// A background job once it has started: armed for a later first frame,
/// or with its first frame saved; or its failure. A job that cannot take
/// its first frame fails within the frame timeout; one that may wait for a
/// trigger without a limit is returned as it is after a moment.
fn started(client: &Client, session: &str, id: u64, plan: &Plan) -> Result<Value> {
    let begun = Instant::now();
    let wait = if plan.timeout_ms >= crate::scheduling::MAX_DURATION_MS {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(plan.timeout_ms) + Duration::from_secs(1)
    };
    let mut pause = Duration::from_millis(10);
    loop {
        let camera = client.status()?;
        let job = find(&camera, id).context("the capture job is gone")?;
        let saved = job["captured"].as_u64().unwrap_or(0);
        let armed = job["first_at_ms"].as_u64().unwrap_or(0) > crate::scheduling::now_ms();
        match job["status"].as_str() {
            Some("pending" | "running")
                if saved == 0 && !armed && begun.elapsed() < wait && !signals::interrupted() =>
            {
                std::thread::sleep(pause);
                pause = (pause * 3 / 2).min(Duration::from_millis(250));
            }
            Some("pending" | "running" | "complete") => {
                return Ok(json!({ "job": jobs::capture(session, &camera, &job) }));
            }
            _ => return finished(session, &camera, &job, false),
        }
    }
}

/// Job `id` in a camera's status entry.
fn find(camera: &Value, id: u64) -> Option<Value> {
    camera["jobs"]
        .as_array()?
        .iter()
        .find(|job| job["id"] == id)
        .cloned()
}

/// What a job saved, for errors: its files and, when there were more than
/// listed, how many.
fn saved(job: &Value) -> Value {
    let files = job["files"].as_array().cloned().unwrap_or_default();
    let mut value = json!({ "files": files });
    let count = job["captured"].as_u64().unwrap_or(0);
    if count > files.len() as u64 {
        value["truncated"] = json!(count - files.len() as u64);
    }
    value
}

fn interrupted(session: &str, camera: &Value, job: &Value) -> anyhow::Error {
    let mut details = saved(job);
    details["job"] = jobs::capture(session, camera, job);
    Categorized::new(
        ErrorKind::Interrupted,
        format!(
            "interrupted after {} of {} frame(s)",
            job["captured"], job["count"]
        ),
    )
    .details(details)
    .into()
}

/// The result of a finished job, or its failure with what it saved.
fn finished(session: &str, camera: &Value, job: &Value, stopped: bool) -> Result<Value> {
    match job["status"].as_str() {
        Some("complete") => Ok(result(camera, job)),
        Some("cancelled") if stopped => Err(interrupted(session, camera, job)),
        Some("cancelled") => {
            let mut details = saved(job);
            details["job"] = jobs::capture(session, camera, job);
            Err(Categorized::new(
                ErrorKind::Incomplete,
                format!(
                    "the capture was stopped after {} of {} frame(s)",
                    job["captured"], job["count"]
                ),
            )
            .details(details)
            .into())
        }
        _ => {
            let tag =
                serde_json::from_value::<crate::types::ErrorTag>(job["error_tag"].clone()).ok();
            let message =
                crate::scrub::redact_urls(job["error"].as_str().unwrap_or("capture failed"));
            let mut details = saved(job);
            details["job"] = jobs::capture(session, camera, job);
            Err(crate::types::with_details(
                exists_hint(crate::types::error_from_tag(tag, message)),
                details,
            ))
        }
    }
}

/// A finished job's result.
fn result(camera: &Value, job: &Value) -> Value {
    let files = job["files"].as_array().cloned().unwrap_or_default();
    let count = job["captured"].as_u64().unwrap_or(0);
    let frame = &job["frame"];
    let first = job["first_ms"].as_u64();
    let mut value = json!({
        "camera": crate::media::redact_url(camera["info"]["id"].as_str().unwrap_or_default()),
        "files": files,
        "count": count,
        "width": frame["width"],
        "height": frame["height"],
        "pixel_format": frame["pixel_format"].as_u64().map(|format| crate::inspect::pixel_format_label(format as u32)),
        "settings": settings(&job["settings"]),
        "time": first.map(jobs_time),
    });
    if count > 1
        && let (Some(first), Some(ended)) = (first, job["ended_ms"].as_u64())
    {
        value["duration_s"] = json!(ended.saturating_sub(first) as f64 / 1000.0);
    }
    if count > files.len() as u64 {
        value["truncated"] = json!(count - files.len() as u64);
    }
    crate::types::without_nulls(value)
}

/// The camera's settings when the first frame was taken (`features`, the
/// GenICam values the worker read) under the names `inspect` and `info`
/// use, with units in the names: SFNC gives ExposureTime in microseconds
/// and Gain in dB.
fn settings(features: &Value) -> Value {
    let number = |names: &[&str]| names.iter().find_map(|name| features[*name].as_f64());
    let mode = |name: &str| features[name].as_str().map(str::to_ascii_lowercase);
    let trigger = match features["TriggerMode"].as_str() {
        Some(mode) if mode.eq_ignore_ascii_case("off") => Some("off".to_owned()),
        Some(_) => Some(
            features["TriggerSource"]
                .as_str()
                .map_or_else(|| "on".to_owned(), str::to_ascii_lowercase),
        ),
        None => None,
    };
    let binning = number(&["BinningHorizontal"])
        .zip(number(&["BinningVertical"]))
        .map(|(x, y)| format!("{x}x{y}"));
    crate::types::without_nulls(json!({
        "exposure_us": number(&["ExposureTime", "ExposureTimeAbs"]),
        "auto_exposure": mode("ExposureAuto"),
        "gain_db": number(&["Gain"]),
        "gain": features["Gain"].is_null().then(|| number(&["GainRaw"])).flatten(),
        "auto_gain": mode("GainAuto"),
        "auto_wb": mode("BalanceWhiteAuto"),
        "fps": number(&["AcquisitionFrameRate", "AcquisitionFrameRateAbs"]),
        "trigger": trigger,
        "black_level": number(&["BlackLevel"]),
        "gamma": number(&["Gamma"]),
        "binning": binning,
        "offset_x": number(&["OffsetX"]),
        "offset_y": number(&["OffsetY"]),
        "reverse_x": features["ReverseX"].as_bool(),
        "reverse_y": features["ReverseY"].as_bool(),
    }))
}

fn dry_run(client: &Client, args: &CaptureArgs, plan: &Plan) -> Result<Value> {
    let camera = client.status()?;
    let mut value = json!({
        "dry_run": true,
        "camera": crate::media::redact_url(camera["info"]["id"].as_str().unwrap_or_default()),
        "output": plan.output,
        "format": crate::naming::format_extension(plan.format),
        "count": plan.count,
    });
    if plan.interval_ms > 0 {
        value["every_s"] = json!(plan.interval_ms as f64 / 1000.0);
    }
    let first = plan.first_at_ms.unwrap_or_else(crate::scheduling::now_ms);
    if plan.first_at_ms.is_some() {
        value["first_at"] = json!(jobs_time(first));
    }
    if plan.interval_ms > 0 {
        value["ends_at"] = json!(jobs_time(
            first.saturating_add(
                plan.interval_ms
                    .saturating_mul(u64::from(plan.count.saturating_sub(1)))
            )
        ));
    }
    if !plan.settings.is_empty() {
        let planned = client.call(SessionCommand::SetValues {
            values: plan.settings.clone(),
            dry_run: true,
        })?;
        value["settings"] = planned["plan"].clone();
    }
    if args.auto {
        value["auto"] = json!(true);
    }
    Ok(crate::types::without_nulls(value))
}

/// One frame to stdout, from a worker of this command: the encoded image,
/// with the result on stderr in JSON mode.
fn to_stdout(global: &Global, args: &CaptureArgs, plan: &Plan) -> Result<()> {
    use std::io::IsTerminal;
    if args.cameras.cameras.len() > 1
        || plan.count > 1
        || plan.interval_ms > 0
        || plan.first_at_ms.is_some()
        || args.background
        || (args.keep.keep.is_some() && !global.keep_from_env)
        || args.dry_run
    {
        return Err(Usage(
            "-o - writes one image now from one camera; give a file or folder for more".into(),
        )
        .into());
    }
    if global.session.is_some() && !global.session_from_env {
        return Err(Usage(
            "-o - takes the frame in a worker of this command; drop --session, or give a file"
                .into(),
        )
        .into());
    }
    if std::io::stdout().is_terminal() {
        return Err(Usage(
            "refusing to write an image to the terminal; redirect it (capture -o - > frame.png) or give a file with -o".into(),
        )
        .into());
    }
    // The frame comes from this process's own worker, whatever session
    // CAPTUREFAB_SESSION names.
    let direct = Global {
        session: None,
        no_session: true,
        session_from_env: false,
        ..global.clone()
    };
    let client = Client::open_with(
        &direct,
        args.cameras.cameras.first().map(String::as_str),
        Route::default().waiting(args.timeout),
    )?;
    let local = client
        .local()
        .context("stdout capture needs a worker")?
        .clone();
    let interrupt = local.clone();
    signals::take_over();
    signals::on_signal(move || interrupt.interrupt());
    prepare(&client, args, plan)?;
    let reply = client.call(SessionCommand::Capture {
        output: "-".into(),
        count: 1,
        timeout_ms: plan.timeout_ms,
        format: plan.format.into(),
        storage: storage_policy(),
        on_exists: Default::default(),
        context: plan.context.clone(),
        utc_offset_s: Some(crate::naming::local_offset()),
    })?;
    let frame = local.captured_frame()?;
    let meta = &reply["frames"][0];
    let image = crate::engine::encode_still(&frame, plan.format, args.quality, None)?;
    let camera = client.status()?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&image)?;
    stdout.flush()?;
    drop(stdout);
    let value = crate::types::without_nulls(json!({
        "camera": crate::media::redact_url(camera["info"]["id"].as_str().unwrap_or_default()),
        "output": "-",
        "bytes": image.len(),
        "count": 1,
        "width": frame.width,
        "height": frame.height,
        "pixel_format": crate::inspect::pixel_format_label(frame.pixel_format),
        "settings": settings(&meta["settings"]),
        "time": meta["wall_ms"].as_u64().map(jobs_time),
    }));
    // The image owns stdout; the result goes to stderr in one write.
    if global.json && super::output::claim() {
        let _ = super::output::write_json(&super::output::document(value));
    }
    Ok(())
}

fn text(value: &Value) -> String {
    let mut text = String::new();
    let one = |text: &mut String, value: &Value| {
        if let Some(job) = value.get("job") {
            let _ = writeln!(text, "Started {}", jobs::line(job));
            return;
        }
        if value["dry_run"] == true {
            let _ = writeln!(
                text,
                "Would save {} {} frame(s) from {} to {}",
                value["count"],
                value["format"].as_str().unwrap_or_default(),
                value["camera"].as_str().unwrap_or_default(),
                value["output"].as_str().unwrap_or_default()
            );
            return;
        }
        let files = value["files"].as_array().cloned().unwrap_or_default();
        match files.as_slice() {
            [file] => {
                let _ = writeln!(text, "Saved {}", file.as_str().unwrap_or_default());
            }
            files => {
                let _ = writeln!(
                    text,
                    "Saved {} frames from {}{}",
                    value["count"],
                    value["camera"].as_str().unwrap_or_default(),
                    files
                        .first()
                        .and_then(Value::as_str)
                        .map(|first| format!(", first {first}"))
                        .unwrap_or_default()
                );
            }
        }
    };
    if let Some(cameras) = value["cameras"].as_array() {
        for camera in cameras {
            one(&mut text, camera);
        }
    } else if let Some(jobs) = value["jobs"].as_array() {
        for job in jobs {
            let _ = writeln!(text, "Started {}", jobs::line(job));
        }
    } else {
        one(&mut text, value);
    }
    text
}

/// Result fields, for `schema`.
pub const FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    (
        "files",
        "[string] — absolute paths, in order; at most 1000, the rest counted in truncated",
    ),
    ("count", "int — frames saved"),
    ("truncated", "int — files saved but not listed"),
    ("width", "int"),
    ("height", "int"),
    ("pixel_format", "string — the camera's PFNC format"),
    (
        "settings",
        "{exposure_us?,auto_exposure?,gain_db?,gain?,auto_gain?,auto_wb?,fps?,trigger?,black_level?,gamma?,binning?,offset_x?,offset_y?,reverse_x?,reverse_y?} — the camera's settings when the first frame was taken, as the camera has them; trigger: off, or its source (software, line0)",
    ),
    ("time", "string — RFC 3339 time of the first frame"),
    ("duration_s", "float — first to last frame, for several"),
    ("output", "\"-\" — with -o -: the image went to stdout"),
    ("bytes", "int — with -o -: the image's size"),
    (
        "cameras",
        "[{camera,files,count,truncated?,width,height,pixel_format,settings,time,duration_s?,dry_run?,output?,format?,every_s?,first_at?,ends_at?,auto?}] — with -c repeated: one result per camera; in error details, a failed camera's {camera,error,files?,truncated?,job?}",
    ),
    (
        "job",
        concat!(
            jobs::job_row!(),
            " — with --background, as jobs lists it, once armed or its first frame is saved"
        ),
    ),
    (
        "jobs",
        concat!(
            "[",
            jobs::job_row!(),
            "] — with --background and -c repeated"
        ),
    ),
    ("dry_run", "true — with --dry-run; nothing was captured"),
    ("format", "png|jpg|raw — with --dry-run"),
    ("every_s", "float — with --dry-run, for a time lapse"),
    ("first_at", "string — with --dry-run and --at/--in"),
    ("ends_at", "string — with --dry-run, for a time lapse"),
    ("auto", "true — with --dry-run and --auto"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_take_the_names_inspect_uses() {
        let shown = settings(&json!({
            "ExposureTime": 10000.0, "ExposureAuto": "Off", "Gain": 0.0,
            "AcquisitionFrameRate": 30.0, "TriggerMode": "On", "TriggerSource": "Line0",
            "BinningHorizontal": 2, "BinningVertical": 2,
        }));
        assert_eq!(
            shown,
            json!({"exposure_us": 10000.0, "auto_exposure": "off", "gain_db": 0.0,
                "fps": 30.0, "trigger": "line0", "binning": "2x2"})
        );
        assert_eq!(
            settings(&json!({"TriggerMode": "Off", "GainRaw": 4})),
            json!({"trigger": "off", "gain": 4.0})
        );
        assert_eq!(settings(&Value::Null), json!({}));
    }

    #[test]
    fn formats_come_from_the_name_and_flags_only_fill_in() {
        assert_eq!(format("/a/shot.JPG", None).unwrap(), "jpeg");
        assert_eq!(format("/a/shot.raw", None).unwrap(), "raw");
        assert_eq!(format("/a/frames/", Some(Format::Jpg)).unwrap(), "jpeg");
        assert_eq!(format("/a/{seq}", None).unwrap(), "png");
        assert_eq!(format("-", Some(Format::Raw)).unwrap(), "raw");
        assert_eq!(format("/a/shot.png", Some(Format::Png)).unwrap(), "png");
        assert!(format("/a/shot.png", Some(Format::Jpg)).is_err());
        assert!(format("/a/shot.gif", None).is_err());
    }
}
