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
    scheduling::rfc3339,
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
    /// Image format when -o has no extension: png (default; 16-bit for Mono10..Mono16), jpg or raw (the exact bytes, with NAME.json beside)
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
    /// Measure the last frame as inspect does, and preview it: the result's inspect block
    #[arg(long)]
    pub inspect: bool,
    /// Tag the files: {tag:KEY} in -o, and in each image's metadata; repeatable
    #[arg(long = "tag", value_name = "KEY=VALUE", value_parser = crate::context::parse_pair)]
    pub tags: Vec<(String, String)>,
    /// How long to wait for each frame (default: 5s, or 3 exposures and frame periods when longer); none waits for a trigger until stopped
    #[arg(long, value_name = "DUR", value_parser = super::args::wait)]
    pub timeout: Option<Wait>,
    /// Frames to let pass after the first one exposed after the last settings change, 0..10
    #[arg(long, default_value_t = 0, value_name = "N", value_parser = clap::value_parser!(u32).range(0..=10))]
    pub fresh: u32,
    /// How long a camera that sets its own exposure (webcam, stream, ExposureAuto=Continuous) may take to settle after it starts; 0 takes frames at once
    #[arg(long, default_value = "2s", value_name = "DUR", value_parser = super::args::duration)]
    pub warmup: Duration,
    /// When a file name without {seq} exists: fail, or number (-0001, -0002, ...)
    #[arg(long, value_enum, default_value_t = crate::naming::OnExists::Fail)]
    pub on_exists: crate::naming::OnExists,
    /// Append one JSON line per saved file (path, time, size, SHA-256, settings, tags) to this file
    #[arg(long, value_name = "FILE")]
    pub manifest: Option<String>,
    /// file: each file reaches the disk before it is reported; none: skip that, for scratch captures
    #[arg(long, value_enum, default_value_t = crate::storage::SyncMode::File)]
    pub sync: crate::storage::SyncMode,
    /// Leave the capture running in a session and print its job at once
    #[arg(long)]
    pub background: bool,
    #[command(flatten)]
    pub progress: super::jobs::ProgressArg,
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
    /// 0: the camera's default.
    timeout_ms: u64,
    warmup_ms: u64,
    settings: Vec<Assignment>,
    context: crate::context::Context,
    /// Absolute, as the caller meant it.
    manifest: Option<String>,
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
                    return Err(Usage(format!("--at {} has passed", rfc3339(at))).into());
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
            Some(Wait::For(wait)) if wait.is_zero() => {
                return Err(
                    Usage("--timeout must be longer than 0; none waits for ever".into()).into(),
                );
            }
            Some(Wait::For(wait)) => millis(wait).max(1),
            Some(Wait::Forever) => crate::scheduling::MAX_DURATION_MS,
            None => 0,
        };
        if args.warmup > Duration::from_millis(crate::session::MAX_WARMUP_MS) {
            return Err(Usage("--warmup is at most 60s".into()).into());
        }
        let manifest = match args.manifest.as_deref() {
            None => None,
            Some("-") => {
                return Err(Usage("--manifest needs a file; stdout is for results".into()).into());
            }
            Some(_) if output == "-" => {
                return Err(Usage("-o - saves no file for --manifest to list".into()).into());
            }
            Some(manifest) => {
                crate::naming::validate_output(manifest).map_err(super::input)?;
                let mut manifest = manifest.to_owned();
                super::paths::resolve_path(&mut manifest, &std::env::current_dir()?)
                    .map_err(super::input)?;
                if std::path::Path::new(&manifest).is_dir()
                    || manifest.ends_with(std::path::is_separator)
                {
                    return Err(Usage(format!(
                        "--manifest {manifest} is a folder; name a file such as captures.jsonl"
                    ))
                    .into());
                }
                Some(manifest)
            }
        };
        let plan = Self {
            output,
            format,
            count,
            interval_ms: args.every.map(millis).unwrap_or(0),
            first_at_ms,
            timeout_ms,
            warmup_ms: millis(args.warmup),
            settings,
            context,
            manifest,
        };
        if plan.output != "-" {
            crate::engine::check_capture_name(&plan.output, count, format)?;
            plan.request(args, None, None)
                .validate()
                .map_err(super::input)?;
            crate::engine::preflight_capture_output(&plan.output, count, format, args.on_exists)
                .map_err(exists_hint)?;
        }
        Ok(plan)
    }
    /// The job request for one camera; a capture this command waits for
    /// holds a lease, so the session stops it if this command goes away.
    fn request(
        &self,
        args: &CaptureArgs,
        quality: Option<u8>,
        first_at_ms: Option<u64>,
    ) -> SessionCommand {
        SessionCommand::Schedule {
            output: self.output.clone(),
            count: self.count,
            timeout_ms: self.timeout_ms,
            format: self.format.into(),
            first_at_ms: first_at_ms
                .or(self.first_at_ms)
                .unwrap_or_else(crate::scheduling::now_ms),
            interval_ms: self.interval_ms,
            on_exists: args.on_exists,
            manifest: self.manifest.clone(),
            sync: args.sync,
            context: self.context.clone(),
            utc_offset_s: Some(crate::naming::local_offset()),
            quality,
            fresh: args.fresh,
            warmup_ms: Some(self.warmup_ms),
            inspect: args.inspect,
            job: None,
            lease: !args.background,
        }
    }
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
            background: args.background,
            ..args.keep.route()
        }
    }
    .waiting(args.timeout.unwrap_or(Wait::For(DEFAULT_WAIT)));
    let each = |camera: Option<&str>| -> Result<Value> {
        let client = Client::open_with(global, camera, route)?;
        if args.dry_run {
            dry_run(&client, &args, &plan)
        } else {
            shoot(&client, &args, &plan)
        }
    };
    // Each camera's thread is part of this command, as a shell sees it.
    let lineage = signals::Lineage::here();
    let results = if cameras.len() == 1 {
        vec![each(cameras[0])]
    } else if !args.dry_run && !args.background {
        let together = Together::new(cameras.len());
        std::thread::scope(|scope| {
            let threads = cameras
                .iter()
                .enumerate()
                .map(|(index, camera)| {
                    let (args, plan, together) = (&args, &plan, &together);
                    scope.spawn(move || {
                        lineage.enter();
                        shoot_together(index, *camera, global, route, args, plan, together)
                    })
                })
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
    } else {
        std::thread::scope(|scope| {
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
        None => {
            let mut value = json!({ "cameras": shown });
            if let Some(skew) = skew_ms(&value["cameras"]) {
                value["skew_ms"] = json!(skew);
            }
            Ok(value)
        }
    }
}

/// How far apart the cameras' first frames arrived, in milliseconds, by
/// the host's clock.
fn skew_ms(cameras: &Value) -> Option<u64> {
    let times = cameras
        .as_array()?
        .iter()
        .map(|camera| {
            chrono::DateTime::parse_from_rfc3339(camera["time"].as_str()?)
                .ok()
                .map(|time| time.timestamp_millis())
        })
        .collect::<Option<Vec<_>>>()?;
    let (first, last) = (times.iter().min()?, times.iter().max()?);
    u64::try_from(last - first).ok()
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
        super::auto::settle(client, Duration::from_secs(5), &Default::default())?;
    }
    if signals::interrupted() {
        return Err(Categorized::error(
            ErrorKind::Interrupted,
            "interrupted before capturing",
        ));
    }
    Ok(())
}

/// How long a stopped capture may take to save the frames it took.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// The frame wait of most cameras when none is given.
const DEFAULT_WAIT: Duration = Duration::from_secs(5);

/// Run the capture job on one camera and wait for it, or leave it running.
fn shoot(client: &Client, args: &CaptureArgs, plan: &Plan) -> Result<Value> {
    prepare(client, args, plan)?;
    run_job(client, args, plan, None)
}

/// How long after every camera of a capture is armed their first frames
/// are due: time to send each its job.
const SYNC_LEAD_MS: u64 = 300;
/// How long before a later first frame the cameras are armed: time to start
/// streaming, FFmpeg's included, beside their warm-up.
const ARM_AHEAD_MS: u64 = 3000;

/// What the threads of a capture from several cameras share as they line
/// up their cameras.
struct Together {
    /// Every camera is connected and set.
    prepared: std::sync::Barrier,
    /// Every camera is armed.
    ready: std::sync::Barrier,
    /// The GigE cameras, by thread.
    gige: std::sync::Mutex<Vec<Option<crate::types::CameraInfo>>>,
    /// Each one's share of a host interface it shares with others.
    shares: std::sync::OnceLock<Vec<Option<crate::network::Share>>>,
    /// When the first frames are due.
    start: std::sync::OnceLock<u64>,
}
impl Together {
    fn new(cameras: usize) -> Self {
        Self {
            prepared: std::sync::Barrier::new(cameras),
            ready: std::sync::Barrier::new(cameras),
            gige: std::sync::Mutex::new(vec![None; cameras]),
            shares: std::sync::OnceLock::new(),
            start: std::sync::OnceLock::new(),
        }
    }
    /// Camera `index`'s share of its host interface, once every thread
    /// said which camera it holds: GigE cameras that stream on one
    /// interface at once split its bandwidth, or lose packets.
    fn share(&self, index: usize) -> Option<crate::network::Share> {
        self.shares
            .get_or_init(|| {
                let gige = self.gige.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let cameras = gige.iter().flatten().cloned().collect::<Vec<_>>();
                if cameras.len() < 2 {
                    return vec![None; gige.len()];
                }
                let Ok(scan) = crate::network::scan(&[], &cameras, NETWORK_PROBE) else {
                    return vec![None; gige.len()];
                };
                let shares = crate::network::shares(&scan);
                gige.iter()
                    .map(|camera| {
                        let camera = camera.as_ref()?;
                        scan.cameras
                            .iter()
                            .position(|scanned| scanned.camera.id == camera.id)
                            .and_then(|position| shares[position].clone())
                    })
                    .collect()
            })
            .get(index)
            .cloned()
            .flatten()
    }
}

/// How long finding the host interfaces of GigE cameras may take.
const NETWORK_PROBE: Duration = Duration::from_secs(1);

/// The camera `client` holds, when it is a GigE camera.
fn gige_camera(client: &Client) -> Option<crate::types::CameraInfo> {
    let status = client.status().ok()?;
    serde_json::from_value::<crate::types::CameraInfo>(status["info"].clone())
        .ok()
        .filter(|info| info.transport == crate::types::Transport::GigE)
}

/// Capture from one of several cameras together: connected, set, paced to
/// their share of a network interface they share and streaming (warmed
/// up) like the others before any takes a frame, then each takes its first
/// frame at or after the same moment. Every camera's thread reaches each
/// barrier whatever happens to it, so none waits for ever.
fn shoot_together(
    index: usize,
    camera: Option<&str>,
    global: &Global,
    route: Route,
    args: &CaptureArgs,
    plan: &Plan,
    together: &Together,
) -> Result<Value> {
    let prepared = Client::open_with(global, camera, route).and_then(|client| {
        prepare(&client, args, plan)?;
        Ok(client)
    });
    if let Ok(client) = &prepared {
        together.gige.lock().unwrap_or_else(|e| e.into_inner())[index] = gige_camera(client);
    }
    together.prepared.wait();
    let share = together.share(index);
    let armed = prepared.and_then(|client| {
        // A camera streams only from shortly before a later first frame.
        if let Some(first_at) = plan.first_at_ms {
            sleep_until(first_at.saturating_sub(ARM_AHEAD_MS + plan.warmup_ms))?;
        }
        let arm = client.call(SessionCommand::Arm {
            warmup_ms: Some(plan.warmup_ms),
            bytes_per_second: share.as_ref().map(|share| share.bytes_per_second),
        })?;
        Ok((client, arm))
    });
    together.ready.wait();
    let (client, arm) = armed?;
    let first_at = plan.first_at_ms.unwrap_or_else(|| {
        *together
            .start
            .get_or_init(|| crate::scheduling::now_ms() + SYNC_LEAD_MS)
    });
    let mut result = run_job(&client, args, plan, Some(first_at));
    // A camera armed for this streams no longer, unless it has work.
    if arm["started"] == true {
        let camera = client.status()?;
        let busy = camera["forwarding"].is_string()
            || jobs::of_camera(&camera).iter().any(|job| !jobs::ended(job));
        if !busy {
            client.call(SessionCommand::Stop)?;
        }
    }
    if let Ok(value) = &mut result {
        freshness(value, &arm);
        paced(value, &arm, share.as_ref());
    }
    result
}

/// What a result says of the pace a GigE camera was held to, sharing a
/// host interface with others of the capture, or why it could not be.
fn paced(value: &mut Value, arm: &Value, share: Option<&crate::network::Share>) {
    let Some(share) = share else {
        return;
    };
    if arm["network"].is_object() {
        value["bandwidth_mbps"] =
            json!((share.bytes_per_second as f64 * 8.0 / 1e6 * 10.0).round() / 10.0);
    } else if let Some(error) = arm["network_error"].as_str() {
        let warning = format!(
            "it shares {} with {} other camera(s) but could not be paced to its share: {error}",
            share.interface,
            share.sharers - 1
        );
        let mut warnings = value["warnings"].as_array().cloned().unwrap_or_default();
        warnings.push(json!(crate::scrub::redact_urls(&warning)));
        value["warnings"] = json!(warnings);
    }
}

/// Wait until `at_ms`, UTC Unix milliseconds, unless a signal comes first.
fn sleep_until(at_ms: u64) -> Result<()> {
    loop {
        if signals::interrupted() {
            return Err(Categorized::error(
                ErrorKind::Interrupted,
                "interrupted before capturing",
            ));
        }
        let left = at_ms.saturating_sub(crate::scheduling::now_ms());
        if left == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(left.min(100)));
    }
}

/// Run the capture job on one camera, its first frame at `first_at_ms` or
/// as planned, and wait for it, or leave it running.
fn run_job(
    client: &Client,
    args: &CaptureArgs,
    plan: &Plan,
    first_at_ms: Option<u64>,
) -> Result<Value> {
    let reply = client.call(plan.request(args, args.quality, first_at_ms))?;
    let id = reply["job"]["id"]
        .as_str()
        .context("capture job without ID")?
        .to_owned();
    if args.background {
        return started(client, &id, plan);
    }
    // A signal stops the job: the frames it took are saved while there is
    // time, and nothing after them starts.
    client.stop_on_signal(&id);
    let began = Instant::now();
    let mut shown = jobs::Progress::new("capturing…", args.progress);
    let mut seq = 0;
    let mut stopping: Option<Instant> = None;
    loop {
        let wait = match stopping {
            Some(since) => STOP_GRACE.saturating_sub(since.elapsed()),
            None => shown.step(Duration::from_secs(25)),
        };
        let reply = client.wait_job(&id, seq, wait.max(Duration::from_millis(1)))?;
        seq = reply["seq"].as_u64().unwrap_or(seq);
        let (job, detail) = (&reply["job"], &reply["detail"]);
        shown.show(job, began);
        if jobs::ended(job) {
            let mut value = finished(
                job,
                detail,
                plan.format,
                stopping.is_some() || signals::interrupted(),
            )?;
            if args.inspect && detail["inspect"].is_object() {
                let info = client.call(SessionCommand::Info)?;
                value["inspect"] = super::inspect::compact(&info, &detail["inspect"]);
            }
            return Ok(value);
        }
        if signals::interrupted() {
            let since = *stopping.get_or_insert_with(Instant::now);
            if since.elapsed() >= STOP_GRACE {
                return Err(interrupted(job, detail));
            }
        }
    }
}

/// A background job once it has started: armed for a later first frame,
/// or with its first frame saved; or its failure. A job that cannot take
/// its first frame fails within the frame timeout; one that may wait for a
/// trigger without a limit is returned as it is after a moment.
fn started(client: &Client, id: &str, plan: &Plan) -> Result<Value> {
    let begun = Instant::now();
    let wait = match plan.timeout_ms {
        crate::scheduling::MAX_DURATION_MS.. => Duration::from_secs(2),
        0 => DEFAULT_WAIT + Duration::from_millis(plan.warmup_ms) + Duration::from_secs(1),
        timeout_ms => Duration::from_millis(timeout_ms + plan.warmup_ms) + Duration::from_secs(1),
    };
    let mut seq = 0;
    loop {
        let left = wait.saturating_sub(begun.elapsed());
        let reply = client.wait_job(id, seq, left.max(Duration::from_millis(1)))?;
        seq = reply["seq"].as_u64().unwrap_or(seq);
        let job = &reply["job"];
        match job["state"].as_str() {
            Some("failed" | "cancelled" | "interrupted") => {
                return finished(job, &reply["detail"], plan.format, false);
            }
            Some("running" | "stopping")
                if job["saved"].as_u64().unwrap_or(0) == 0
                    && !left.is_zero()
                    && !signals::interrupted() => {}
            _ => return Ok(json!({ "job": job })),
        }
    }
}

/// What a job saved, for errors: its files and, when there were more than
/// listed, how many; and the frames that came, were dropped or arrived
/// incomplete, which may say why it failed.
fn saved(job: &Value, detail: &Value) -> Value {
    let mut value = json!({ "files": detail["files"].as_array().cloned().unwrap_or_default() });
    if let Some(truncated) = detail["truncated"].as_u64() {
        value["truncated"] = json!(truncated);
    }
    for key in ["frames", "dropped", "incomplete"] {
        if detail[key].is_u64() {
            value[key] = detail[key].clone();
        }
    }
    value["job"] = job.clone();
    value
}

fn interrupted(job: &Value, detail: &Value) -> anyhow::Error {
    Categorized::new(
        ErrorKind::Interrupted,
        format!(
            "interrupted after {} of {} frame(s)",
            job["saved"], job["planned"]
        ),
    )
    .details(saved(job, detail))
    .into()
}

/// The result of a finished job, or its failure with what it saved.
fn finished(job: &Value, detail: &Value, format: &str, stopped: bool) -> Result<Value> {
    match job["state"].as_str() {
        Some("done") => Ok(result(job, detail, format)),
        Some("cancelled") if stopped => Err(interrupted(job, detail)),
        Some("cancelled" | "interrupted") => Err(Categorized::new(
            ErrorKind::Incomplete,
            format!(
                "the capture was stopped after {} of {} frame(s)",
                job["saved"], job["planned"]
            ),
        )
        .details(saved(job, detail))
        .into()),
        _ => Err(crate::types::with_details(
            exists_hint(job_error(job, detail)),
            saved(job, detail),
        )),
    }
}

/// The error a failed job ended with: in full in its account, with its
/// reason, hint and details.
pub(super) fn job_error(job: &Value, detail: &Value) -> anyhow::Error {
    let error = if detail["error"].is_object() {
        &detail["error"]
    } else {
        &job["error"]
    };
    let mut error = error.clone();
    if let Some(message) = error["message"].as_str() {
        error["message"] = json!(crate::scrub::redact_urls(message));
    }
    crate::session::protocol::from_wire(&error)
}

/// A finished job's result, from its document and its worker's account.
fn result(job: &Value, detail: &Value, format: &str) -> Value {
    let files = detail["files"].as_array().cloned().unwrap_or_default();
    let count = job["saved"].as_u64().unwrap_or(0);
    let frame = &detail["frame"];
    let first = detail["first_ms"].as_u64();
    let pixel_format = frame["pixel_format"].as_u64().map(|format| format as u32);
    let mut value = json!({
        "camera": job["camera"],
        "files": files,
        "count": count,
        "width": frame["width"],
        "height": frame["height"],
        "pixel_format": pixel_format.map(crate::inspect::pixel_format_label),
        "settings": crate::engine::saved::settings(&detail["settings"]),
        "time": first.map(rfc3339),
        "device_ts_ns": detail["first_ts_ns"],
    });
    if let Some(pixel_format) = pixel_format {
        depth(&mut value, pixel_format, format);
    }
    integrity(&mut value, detail, count);
    if let Some(truncated) = detail["truncated"].as_u64() {
        value["truncated"] = json!(truncated);
    }
    freshness(&mut value, detail);
    crate::types::without_nulls(value)
}

/// The bit depth of the files, and the frames' own when the files keep
/// fewer bits, so a loss of precision is never silent.
fn depth(value: &mut Value, pixel_format: u32, format: &str) {
    let (bit_depth, source_bits) = crate::frame::bit_depth(pixel_format, format);
    value["bit_depth"] = json!(bit_depth);
    if let Some(bits) = source_bits {
        value["source_bits"] = json!(bits);
    }
}

/// What a result says of the frames' integrity, from the worker's account:
/// frames delivered, dropped and incomplete while the capture ran, and
/// packets resent; for several frames the time and rate from the first to
/// the last (by the camera's clock when it has one), and for a burst
/// whether its frames' block IDs follow one another.
fn integrity(value: &mut Value, detail: &Value, count: u64) {
    value["frames"] = detail["frames"].clone();
    value["dropped"] = json!(detail["dropped"].as_u64().unwrap_or(0));
    for key in ["incomplete", "resent"] {
        value[key] = detail[key].clone();
    }
    if count < 2 {
        return;
    }
    let span = |first: &str, last: &str| {
        detail[first]
            .as_u64()
            .zip(detail[last].as_u64())
            .map(|(first, last)| last.saturating_sub(first))
    };
    if let Some(ms) = span("first_ms", "last_ms") {
        value["duration_s"] = json!(ms as f64 / 1000.0);
    }
    let seconds = span("first_ts_ns", "last_ts_ns")
        .map(|ns| ns as f64 / 1e9)
        .or_else(|| span("first_ms", "last_ms").map(|ms| ms as f64 / 1000.0))
        .filter(|seconds| *seconds > 0.0);
    if let Some(seconds) = seconds {
        value["fps"] = json!(((count - 1) as f64 / seconds * 100.0).round() / 100.0);
    }
    if detail["interval_ms"].as_u64() == Some(0)
        && let (Some(first), Some(last)) = (detail["first_id"].as_u64(), detail["last_id"].as_u64())
    {
        value["first_id"] = json!(first);
        value["last_id"] = json!(last);
        // Block IDs that wrapped around say nothing of gaps.
        if last >= first {
            let gaps = (last - first + 1).saturating_sub(count);
            value["gaps"] = json!(gaps);
            value["consecutive"] = json!(gaps == 0);
        }
    }
}

/// What a result says of the frames' freshness, from the worker's account:
/// `after_change` when frames exposed before the last settings change were
/// let pass, and a warm-up's time, with a warning when the camera had not
/// settled.
fn freshness(value: &mut Value, detail: &Value) {
    if detail["after_change"] == true {
        value["after_change"] = json!(true);
    }
    if let Some(ms) = detail["warmup_ms"].as_u64() {
        value["warmup_ms"] = json!(ms);
        if detail["settled"] == false {
            value["settled"] = json!(false);
            value["warnings"] = json!([crate::engine::unsettled_warning(ms)]);
        }
    }
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
        value["first_at"] = json!(rfc3339(first));
    }
    if plan.interval_ms > 0 {
        value["ends_at"] = json!(rfc3339(
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

/// One frame to stdout: the encoded image, with the result on stderr in
/// JSON mode. A worker of this command writes it to a pipe this copies; a
/// session that holds the camera, or a typed --session, writes it to a
/// private file this streams and deletes. A closed stdout (`| head`) ends
/// the command quietly.
fn to_stdout(global: &Global, args: &CaptureArgs, plan: &Plan) -> Result<()> {
    use std::io::IsTerminal;
    if args.inspect {
        return Err(Usage(
            "--inspect measures a saved frame; give a file or folder with -o".into(),
        )
        .into());
    }
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
    if std::io::stdout().is_terminal() {
        return Err(Usage(
            "refusing to write an image to the terminal; redirect it (capture -o - > frame.png) or give a file with -o".into(),
        )
        .into());
    }
    // CAPTUREFAB_SESSION and CAPTUREFAB_KEEP never take the camera into a
    // session for this; a session that holds it already is used.
    let routed = Global {
        session: global.session.clone().filter(|_| !global.session_from_env),
        session_from_env: false,
        ..global.clone()
    };
    let mut client = Client::open_with(
        &routed,
        args.cameras.cameras.first().map(String::as_str),
        Route {
            stdout: true,
            ..Route::default()
        }
        .waiting(args.timeout.unwrap_or(Wait::For(DEFAULT_WAIT))),
    )?;
    // A worker of this command is told to end the capture on a signal,
    // which then reports it. A session's capture ends as this command exits
    // on the signal: the session cancels it when the connection closes.
    if client.session().is_none() {
        signals::take_over();
    }
    prepare(&client, args, plan)?;
    let request = |stdout: Option<String>| SessionCommand::Capture {
        output: "-".into(),
        count: 1,
        timeout_ms: plan.timeout_ms,
        format: plan.format.into(),
        on_exists: Default::default(),
        sync: args.sync,
        context: plan.context.clone(),
        utc_offset_s: Some(crate::naming::local_offset()),
        quality: args.quality,
        fresh: args.fresh,
        warmup_ms: Some(plan.warmup_ms),
        stdout,
    };
    let copied = if client.session().is_some() {
        let file = stdout_file()?;
        signals::on_signal({
            let file = file.clone();
            move || {
                let _ = std::fs::remove_file(&file);
            }
        });
        let reply = client.call(request(Some(file.to_string_lossy().into_owned())));
        let copied = reply.as_ref().ok().map(|_| {
            std::fs::File::open(&file)
                .and_then(|mut image| std::io::copy(&mut image, &mut std::io::stdout().lock()))
        });
        let _ = std::fs::remove_file(&file);
        (reply?, copied)
    } else {
        let mut pipe = client
            .take_stdout()
            .context("stdout capture needs a worker of this command")?;
        let local = client
            .local()
            .context("stdout capture needs a worker of this command")?
            .clone();
        signals::on_signal(move || local.interrupt());
        // Copied as the worker writes, so an image larger than the pipe
        // never waits on this command. A reader that goes away is noted
        // before the pipe closes, so the worker's failed write after it is
        // known for what it is.
        let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let copier = {
            let closed = closed.clone();
            std::thread::Builder::new()
                .name("capturefab-stdout".into())
                .spawn(move || {
                    let copied = std::io::copy(&mut pipe, &mut std::io::stdout().lock());
                    if copied
                        .as_ref()
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
                    {
                        closed.store(true, std::sync::atomic::Ordering::Release);
                    }
                    copied
                })
                .context("start the stdout copier")?
        };
        let reply = match client.call(request(None)) {
            Ok(reply) => reply,
            Err(_) if closed.load(std::sync::atomic::Ordering::Acquire) => return Ok(()),
            Err(error) => return Err(error),
        };
        let copied = copier
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("the stdout copier stopped")));
        (reply, Some(copied))
    };
    let (reply, copied) = copied;
    match copied {
        Some(Ok(_)) => {}
        Some(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            // The reader went away, as `| head` does: nothing to report.
            if let Some(local) = client.local() {
                local.interrupt();
            }
            return Ok(());
        }
        Some(Err(error)) => return Err(error).context("write the image to stdout"),
        None => {}
    }
    let _ = std::io::stdout().flush();
    let meta = &reply["frame"];
    let camera = client.status()?;
    let mut value = crate::types::without_nulls(json!({
        "camera": crate::media::redact_url(camera["info"]["id"].as_str().unwrap_or_default()),
        "output": "-",
        "bytes": meta["file_bytes"],
        "count": 1,
        "width": meta["width"],
        "height": meta["height"],
        "pixel_format": meta["pixel_format"].as_u64().map(|format| crate::inspect::pixel_format_label(format as u32)),
        "bit_depth": meta["pixel_format"].as_u64().map(|format| crate::frame::bit_depth(format as u32, plan.format).0),
        "source_bits": meta["pixel_format"].as_u64().and_then(|format| crate::frame::bit_depth(format as u32, plan.format).1),
        "settings": crate::engine::saved::settings(&meta["settings"]),
        "time": meta["wall_ms"].as_u64().map(rfc3339),
        "device_ts_ns": meta["device_ts_ns"],
        "frames": reply["frames"],
        "dropped": reply["dropped"],
        "incomplete": reply["incomplete"],
        "resent": reply["resent"],
    }));
    freshness(&mut value, &reply);
    // The image owns stdout; the result goes to stderr in one write.
    if global.json && super::output::claim() {
        let _ = super::output::write_json(&super::output::document(value));
    }
    Ok(())
}

/// A new private file for an image a session captures to stdout, in the
/// runtime folder's `out`, or the system's temporary folder when that is
/// unusable. Files left there by commands that died are swept first.
fn stdout_file() -> Result<std::path::PathBuf> {
    let dir = crate::session::stdout_dir();
    let dir = if crate::ipc::ensure_private_dir(&dir).is_ok() {
        crate::session::sweep_stdout_files(&dir);
        dir
    } else {
        std::env::temp_dir()
    };
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("random file name: {error}"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(dir.join(format!("{}-{suffix}", std::process::id())))
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
        "bit_depth",
        "int — bits per sample in the files: 16 for PNG of Mono10..Mono16 (samples at the top bits, sBIT says how many count), every bit for raw, else 8",
    ),
    (
        "source_bits",
        "int — the camera's bits per sample, when the files keep fewer (color or Bayer PNG, JPEG)",
    ),
    (
        "settings",
        "{exposure_us?,auto_exposure?,gain_db?,gain?,auto_gain?,auto_wb?,fps?,trigger?,black_level?,gamma?,binning?,offset_x?,offset_y?,reverse_x?,reverse_y?} — the camera's settings when the first frame was taken, as the camera has them; trigger: off, or its source (software, line0)",
    ),
    ("time", "string — RFC 3339 time of the first frame"),
    (
        "device_ts_ns",
        "int — the camera's clock when the first frame was taken (GigE, U3V, sim); never a date",
    ),
    (
        "frames",
        "int — frames the camera delivered while the capture ran, including those let pass",
    ),
    (
        "dropped",
        "int — frames lost while it ran: the encoders' queue was full, or the transport lost them",
    ),
    (
        "incomplete",
        "int — frames that arrived incomplete, where the transport counts them",
    ),
    (
        "resent",
        "int — packets the camera sent again, where the transport counts them",
    ),
    (
        "fps",
        "float — frames per second from the first frame to the last, by the camera's clock when it has one",
    ),
    ("first_id", "int — a burst's first frame's block ID"),
    ("last_id", "int — a burst's last frame's block ID"),
    (
        "gaps",
        "int — block IDs missing between a burst's first and last frame",
    ),
    (
        "consecutive",
        "bool — a burst's frames follow one another with no frame missing",
    ),
    (
        "after_change",
        "true — frames exposed before the camera's last settings change were let pass",
    ),
    (
        "warmup_ms",
        "int — how long a camera that sets its own exposure took to settle after it started",
    ),
    (
        "settled",
        "false — its exposure was still changing when --warmup ran out (with a warning)",
    ),
    ("warnings", "[string]"),
    (
        "inspect",
        "{exposure,verdict,suggest?,focus?,preview?} — with --inspect: the last frame measured as inspect measures it",
    ),
    ("duration_s", "float — first to last frame, for several"),
    ("output", "\"-\" — with -o -: the image went to stdout"),
    ("bytes", "int — with -o -: the image's size"),
    (
        "skew_ms",
        "int — with -c repeated: how far apart the cameras' first frames arrived; all were armed first and asked for a frame at the same moment",
    ),
    (
        "bandwidth_mbps",
        "float — with -c repeated, for GigE cameras that share a host network interface: the rate this one was paced to, its share of the interface (a warning when it could not be paced)",
    ),
    (
        "cameras",
        "[{camera,files,count,truncated?,width,height,pixel_format,bit_depth,source_bits?,settings,time,device_ts_ns?,frames,dropped,incomplete?,resent?,duration_s?,fps?,first_id?,last_id?,gaps?,consecutive?,after_change?,warmup_ms?,settled?,bandwidth_mbps?,warnings?,inspect?,dry_run?,output?,format?,every_s?,first_at?,ends_at?,auto?}] — with -c repeated: one result per camera; in error details, a failed camera's {camera,error,files?,truncated?,job?}",
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
