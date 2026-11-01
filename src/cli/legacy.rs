//! Capture, recording and session commands in their current form. They take
//! the new global flags and `-c`, but keep their own options until capture,
//! record, jobs and sessions are rewritten.
use super::{Global, args::CameraArg, args::CamerasArg, output::Report, paths, summary};
use crate::{
    ipc,
    session::{SessionCommand, SessionHandle},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ImageFormat {
    Png,
    #[value(alias = "jpg")]
    Jpeg,
    Raw,
    Pgm,
    Ppm,
}
impl ImageFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Raw => "raw",
            Self::Pgm => "pgm",
            Self::Ppm => "ppm",
        }
    }
}
fn capture_format(format: Option<ImageFormat>, output: &str, count: u32) -> Result<ImageFormat> {
    if let Some(format) = format {
        return Ok(format);
    }
    if count > 1 && !crate::naming::has_placeholder(output)
        || output == "-"
        || crate::naming::names_directory(output)
        || std::path::Path::new(output).is_dir()
    {
        return Ok(ImageFormat::Png);
    }
    match std::path::Path::new(output)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("png") => Ok(ImageFormat::Png),
        Some("jpg" | "jpeg") => Ok(ImageFormat::Jpeg),
        Some("raw") => Ok(ImageFormat::Raw),
        Some("pgm") => Ok(ImageFormat::Pgm),
        Some("ppm") => Ok(ImageFormat::Ppm),
        Some(extension) => Err(Usage(format!(
            "cannot infer an image format from .{extension}; use .png, .jpg, .raw, .pgm, .ppm or specify --format"
        ))
        .into()),
    }
}

/// A CLI path belongs to the caller, even when the worker lives in a session
/// started from a different directory.
fn caller_output(command: &mut SessionCommand) -> Result<()> {
    if matches!(command, SessionCommand::Capture { output, .. } if output == "-") {
        return Ok(());
    }
    paths::resolve_outputs(command, &std::env::current_dir()?)
}

#[derive(Debug, Args)]
pub struct StorageArgs {
    /// Maximum owned bytes across all outputs (default: saved global policy, initially 10GiB)
    #[arg(long, value_parser = parse_bytes_text)]
    max_space: Option<String>,
    /// Maximum owned files (default: saved global policy, initially 10000)
    #[arg(long,value_parser=clap::value_parser!(u32).range(1..=100_000))]
    max_files: Option<u32>,
    /// Explicitly delete oldest owned files when the budget is reached
    #[arg(long, conflicts_with = "on_full")]
    delete_oldest: bool,
    /// Explicit behavior when storage is full
    #[arg(long,value_parser=["stop","delete-oldest"])]
    on_full: Option<String>,
    /// Expire owned files older than this duration, such as 168h; requires the delete-oldest policy
    #[arg(long, conflicts_with = "no_max_age", value_parser = super::args::duration)]
    max_age: Option<Duration>,
    /// Clear a previously configured age limit
    #[arg(long)]
    no_max_age: bool,
}
impl StorageArgs {
    fn policy(&self) -> Result<crate::storage::StoragePolicy> {
        let policy = crate::storage::configured_policy()?;
        usage(self.apply_policy(policy))
    }
    fn apply_policy(
        &self,
        mut policy: crate::storage::StoragePolicy,
    ) -> Result<crate::storage::StoragePolicy> {
        if let Some(space) = &self.max_space {
            policy.max_bytes = crate::storage::parse_bytes(space)?;
        }
        if let Some(files) = self.max_files {
            policy.max_files = files;
        }
        if self.delete_oldest {
            policy.on_full = "delete-oldest".into();
        }
        if let Some(on_full) = &self.on_full {
            policy.on_full = on_full.clone();
        }
        if self.no_max_age {
            policy.max_age_seconds = None;
        }
        if let Some(age) = &self.max_age {
            ensure!(
                age.subsec_nanos() == 0 && age.as_secs() > 0,
                "--max-age must be a whole number of seconds (minimum 1s)"
            );
            policy.max_age_seconds = Some(age.as_secs());
        }
        ensure!(
            policy.max_age_seconds.is_none() || policy.on_full == "delete-oldest",
            "--max-age requires --delete-oldest or a saved delete-oldest policy"
        );
        policy.validate()?;
        Ok(policy)
    }
}
#[derive(Debug, Args)]
pub struct AutoArgs {
    /// Enable auto mode first so Capturefab tunes exposure, gain and frame rate
    #[arg(long)]
    auto: bool,
    /// Auto mode trade-off from 0 (image quality) to 1 (frame rate), or quality, balanced, frame-rate
    #[arg(long, requires = "auto", value_parser = parse_balance)]
    balance: Option<f64>,
}
impl AutoArgs {
    fn apply(&self, client: &LegacyClient) -> Result<()> {
        if self.auto {
            client.call(SessionCommand::Auto {
                balance: self.balance,
            })?;
        }
        Ok(())
    }
}
#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    /// Show global policy, owned files and reservations
    Status,
    /// Persist the global storage ceiling for all cameras and future sessions
    Configure {
        #[command(flatten)]
        storage: StorageArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum LegacyCommand {
    /// Capture complete frames to files; existing files are never overwritten
    #[command(
        after_help = "Examples:\n  capturefab capture -c sim:0 -o shot.png\n  capturefab capture -c sim:0 -n 50 --interval 2s -o 'line1/{camera}-{seq}.jpg'\n  capturefab capture -c sim:0 -n 10 -o frames/\n  capturefab capture --no-session -c sim:0 -o - > frame.png"
    )]
    Capture {
        #[command(flatten)]
        camera: CameraArg,
        /// File, folder (an existing one, or a name ending in /), or template with {camera} {date} {time} {datetime} {seq} {frame} {tag:KEY}; '-' writes one image to stdout
        #[arg(short = 'o', long)]
        output: String,
        /// Number of frames (1-100000). With more than 1, give a folder or a template with {seq} or {frame}
        #[arg(short='n',long,default_value_t=1,value_parser=clap::value_parser!(u32).range(1..=100_000))]
        count: u32,
        /// Image format; inferred from the file or template extension, otherwise PNG
        #[arg(short = 'f', long, value_enum)]
        format: Option<ImageFormat>,
        /// Set NAME=VALUE before acquisition, as set does; may be repeated
        #[arg(long = "set", value_name = "NAME=VALUE")]
        settings: Vec<String>,
        /// Begin at a time: HH:MM[:SS] here, or RFC 3339
        #[arg(long, conflicts_with = "delay", value_parser = super::args::time)]
        at: Option<chrono::DateTime<chrono::Utc>>,
        /// Begin after a delay, e.g. 30s or 5m
        #[arg(long, value_parser = super::args::duration)]
        delay: Option<Duration>,
        /// Time between frames, e.g. 10s; count bounds the time lapse
        #[arg(long, value_parser = super::args::duration)]
        interval: Option<Duration>,
        /// Per-frame wait, e.g. 2s (at most 60s)
        #[arg(long, default_value = "2s", value_name = "DUR", value_parser = super::args::duration)]
        timeout: Duration,
        /// When the output file exists: fail (default) or number (-0001, -0002, ...). {seq} always picks a free number
        #[arg(long, value_enum, default_value_t = crate::naming::OnExists::Fail)]
        on_exists: crate::naming::OnExists,
        /// Tag the capture, e.g. --context line=L1 (repeatable)
        #[arg(long = "context", value_name = "KEY=VALUE", value_parser = crate::context::parse_pair)]
        context: Vec<(String, String)>,
        #[command(flatten)]
        storage: StorageArgs,
        #[command(flatten)]
        auto: AutoArgs,
    },
    /// Record video or forward frames to MediaMTX/NVR (RTSP, SRT, RTMP, UDP)
    #[command(
        visible_alias = "forward",
        after_help = "Examples:\n  capturefab record -c sim:0 -o clip.mp4 --duration 30s\n  capturefab record -c sim:0 -o clip.mkv --codec h265 --fps 15 --duration 5m\n  capturefab record -c sim:0 -o rtsp://localhost:8554/cam"
    )]
    Record {
        #[command(flatten)]
        camera: CameraArg,
        /// Local file (.mp4, .mov, .mkv, .ts, or .webm with av1) or a stream URL (rtsp://, srt://, rtmp://, udp://)
        #[arg(short = 'o', long)]
        output: String,
        /// Video codec: h264, h265/hevc or av1
        #[arg(long, default_value = "h264")]
        codec: String,
        /// FFmpeg encoder name (e.g. libx264, h264_nvenc), or auto to probe available encoders
        #[arg(long, default_value = "auto")]
        encoder: String,
        /// Output frame rate
        #[arg(long, default_value_t = 30.0)]
        fps: f64,
        /// Target bitrate, e.g. 4M or 800k
        #[arg(long, default_value = "4M")]
        bitrate: String,
        /// Stop after this duration, e.g. 30s or 5m; without it a direct recording runs until Ctrl-C
        #[arg(long, value_parser = super::args::duration)]
        duration: Option<Duration>,
        /// Camera connect wait, e.g. 2s
        #[arg(long, default_value = "2s", value_name = "DUR", value_parser = super::args::duration)]
        timeout: Duration,
        /// Bounded maximum size of a local recording file
        #[arg(long, default_value = "512MiB", value_parser = parse_bytes_text)]
        max_file_size: String,
        /// When the output file exists: fail (default) or number (-0001, -0002, ...)
        #[arg(long, value_enum, default_value_t = crate::naming::OnExists::Fail)]
        on_exists: crate::naming::OnExists,
        /// Tag the recording, e.g. --context line=L1 (repeatable)
        #[arg(long = "context", value_name = "KEY=VALUE", value_parser = crate::context::parse_pair)]
        context: Vec<(String, String)>,
        #[command(flatten)]
        storage: StorageArgs,
        #[command(flatten)]
        auto: AutoArgs,
    },
    /// Stop the encoder in a session
    StopForward {
        #[command(flatten)]
        camera: CameraArg,
    },
    /// List capture schedules in a session
    Jobs {
        #[command(flatten)]
        camera: CameraArg,
    },
    /// Cancel a scheduled capture job in a session
    Cancel {
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        id: u64,
        #[command(flatten)]
        camera: CameraArg,
    },
    /// Show a session's cameras, jobs and logs
    Status {
        #[command(flatten)]
        camera: CameraArg,
    },
    /// Run a session in the foreground that keeps cameras open for other commands
    #[command(
        after_help = "Examples:\n  capturefab serve --name bench -c sim:0\n  capturefab --session bench capture -o shot.png"
    )]
    Serve {
        /// Session name other commands pass to --session
        #[arg(long, default_value = "bench")]
        name: String,
        #[command(flatten)]
        cameras: CamerasArg,
        /// Start live acquisition as soon as the camera connects
        #[arg(long)]
        stream: bool,
        #[command(flatten)]
        auto: AutoArgs,
    },
    /// Connect a camera in a session
    Connect {
        /// Camera ID, serial, IP, media URI or sim:0 (default: -c)
        #[arg(value_name = "CAMERA")]
        target: Option<String>,
        #[command(flatten)]
        camera: CameraArg,
    },
    /// Disconnect a camera in a session
    Disconnect {
        #[command(flatten)]
        camera: CameraArg,
    },
    /// Start live acquisition in a session
    Start {
        #[command(flatten)]
        camera: CameraArg,
    },
    /// Stop live acquisition in a session
    Stop {
        #[command(flatten)]
        camera: CameraArg,
    },
    /// Let Capturefab tune exposure, gain and frame rate in a session
    Auto {
        #[command(flatten)]
        camera: CameraArg,
        /// Trade-off from 0 (image quality) to 1 (frame rate), or quality, balanced, frame-rate
        #[arg(long, value_parser = parse_balance)]
        balance: Option<f64>,
    },
    /// Stop auto mode and hold the current exposure, gain and white balance in a session
    Manual {
        #[command(flatten)]
        camera: CameraArg,
        /// Restore every feature auto mode changed to its value from before auto mode
        #[arg(long)]
        revert: bool,
    },
    /// Show owned file usage and reserved recording space
    Storage {
        #[command(subcommand)]
        command: Option<StorageCommand>,
    },
}
impl LegacyCommand {
    fn camera(&self) -> Option<&str> {
        match self {
            Self::Capture { camera, .. }
            | Self::Record { camera, .. }
            | Self::StopForward { camera }
            | Self::Jobs { camera }
            | Self::Cancel { camera, .. }
            | Self::Status { camera }
            | Self::Disconnect { camera }
            | Self::Start { camera }
            | Self::Stop { camera }
            | Self::Auto { camera, .. }
            | Self::Manual { camera, .. } => camera.camera.as_deref(),
            Self::Connect { target, camera } => target.as_deref().or(camera.camera.as_deref()),
            Self::Serve { .. } | Self::Storage { .. } => None,
        }
    }
    /// Commands that act on a session only.
    fn persistent(&self) -> bool {
        !matches!(
            self,
            Self::Capture { .. } | Self::Record { .. } | Self::Serve { .. } | Self::Storage { .. }
        )
    }
    fn timeout_ms(&self) -> u64 {
        match self {
            Self::Capture { timeout, .. } | Self::Record { timeout, .. } => {
                u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)
            }
            _ => 2000,
        }
    }
}

fn parse_bytes_text(s: &str) -> std::result::Result<String, String> {
    crate::storage::parse_bytes(s)
        .map(|_| s.into())
        .map_err(|e| e.to_string())
}
fn parse_balance(s: &str) -> std::result::Result<f64, String> {
    match s {
        "quality" => Ok(0.0),
        "balanced" => Ok(0.5),
        "frame-rate" | "speed" => Ok(1.0),
        _ => s
            .parse()
            .ok()
            .filter(|b| (0.0..=1.0).contains(b))
            .ok_or_else(|| {
                "expected a number from 0 (quality) to 1 (frame rate), or quality, balanced, frame-rate".into()
            }),
    }
}

/// A failure the caller must fix, whatever caused it.
fn usage<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.chain().any(|cause| cause.is::<std::io::Error>())
            || crate::types::classify(&error).kind != ErrorKind::OperationFailed
        {
            error
        } else {
            Usage(format!("{error:#}")).into()
        }
    })
}

/// Refuse to dump image bytes into an interactive terminal, as curl does.
fn binary_stdout_allowed(output: &str, stdout_is_terminal: bool) -> Result<()> {
    if output == "-" && stdout_is_terminal {
        return Err(Usage(
            "refusing to write a binary image to the terminal; redirect it (capture -o - > frame.png), pipe it, or use -o frame.png".into(),
        )
        .into());
    }
    Ok(())
}

struct LegacyClient {
    session: Option<String>,
    local: Option<SessionHandle>,
    target: Option<String>,
}
impl Drop for LegacyClient {
    fn drop(&mut self) {
        if let Some(h) = &self.local {
            h.shutdown();
        }
    }
}
impl LegacyClient {
    fn new(
        session: Option<String>,
        camera: Option<&str>,
        one_shot: bool,
        needs_target: bool,
        timeout_ms: u64,
    ) -> Result<Self> {
        let mut client = Self {
            local: session.is_none().then(SessionHandle::new),
            session,
            target: None,
        };
        if let Some(name) = &client.session {
            let state = ipc::call(name, SessionCommand::Status, Duration::from_secs(60))?;
            if let Some(selector) = camera {
                if let Some(held) = super::held_camera(&state, selector) {
                    client.target = Some(held);
                } else if one_shot {
                    let result = ipc::call_to(
                        name,
                        None,
                        SessionCommand::connect(selector.to_owned(), timeout_ms),
                        Duration::from_secs(60),
                    )?;
                    client.target = result["connected"]["id"].as_str().map(str::to_owned);
                } else {
                    // The session refuses a camera it does not hold rather
                    // than acting on its only one.
                    client.target = Some(selector.to_owned());
                }
            } else if needs_target && state["cameras"].as_array().is_some_and(|c| !c.is_empty()) {
                client.target = Some(super::only_camera(name, &state)?);
            }
        } else if let Some(selector) = camera {
            client.call(SessionCommand::connect(selector.to_owned(), timeout_ms))?;
        }
        Ok(client)
    }
    fn call(&self, command: SessionCommand) -> Result<Value> {
        command.validate()?;
        if let Some(name) = &self.session {
            let target = match &command {
                SessionCommand::Connect { .. } => None,
                _ => self.target.as_deref(),
            };
            ipc::call_to(
                name,
                target,
                command.clone(),
                crate::session::default_request_timeout(&command),
            )
        } else {
            self.local.as_ref().unwrap().request(command)
        }
    }
}

/// The session a legacy command uses: --session, else the only running
/// one; several running sessions need --session or --no-session.
fn legacy_session(global: &Global, command: &LegacyCommand) -> Result<Option<String>> {
    if global.no_session
        || matches!(
            command,
            LegacyCommand::Serve { .. } | LegacyCommand::Storage { .. }
        )
    {
        return Ok(None);
    }
    if let Some(session) = global.session.as_ref().filter(|_| !global.session_from_env) {
        return Ok(Some(session.clone()));
    }
    // Binary stdout belongs to a worker of this command, also when
    // CAPTUREFAB_SESSION names a session for the others.
    if matches!(command, LegacyCommand::Capture { output, .. } if output == "-") {
        return Ok(None);
    }
    if let Some(session) = &global.session {
        return Ok(Some(session.clone()));
    }
    let sessions = ipc::active_sessions()?;
    match sessions.as_slice() {
        [] => Ok(None),
        [session] => {
            global.output().note(format_args!(
                "capturefab: using session '{}'; use --no-session for a worker of this command",
                session.name
            ));
            Ok(Some(session.name.clone()))
        }
        _ => Err(Usage(format!(
            "multiple sessions are running ({}); choose --session NAME or --no-session",
            sessions
                .iter()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .into()),
    }
}

/// The capture or record request, checked before any camera is opened.
fn prepare(
    global: &Global,
    command: &LegacyCommand,
    session: Option<&str>,
) -> Result<Option<SessionCommand>> {
    let timeout_ms = command.timeout_ms();
    Ok(match command {
        LegacyCommand::Capture {
            output,
            count,
            format,
            settings,
            at,
            delay,
            interval,
            on_exists,
            context,
            ..
        } => {
            let context = crate::context::from_pairs(context)?;
            ensure!(
                output != "-" || (session.is_none() && !global.json && *count == 1),
                "binary stdout needs one frame, a worker of this command, and no --json"
            );
            {
                use std::io::IsTerminal;
                binary_stdout_allowed(output, io::stdout().is_terminal())?;
            }
            for setting in settings {
                crate::controls::Assignment::parse(setting)?;
            }
            let format = capture_format(*format, output, *count)?;
            for duration in [delay, interval].into_iter().flatten() {
                if duration.subsec_nanos() % 1_000_000 != 0 {
                    return Err(
                        Usage("--delay and --interval must be whole milliseconds".into()).into(),
                    );
                }
            }
            let millis =
                |duration: &Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
            let command = if at.is_some() || delay.is_some() || interval.is_some() {
                let first_at_ms = match at {
                    Some(at) => u64::try_from(at.timestamp_millis()).unwrap_or(0),
                    None => crate::scheduling::now_ms()
                        .checked_add(delay.as_ref().map(millis).unwrap_or(0))
                        .context("capture time overflows")?,
                };
                SessionCommand::Schedule {
                    output: output.clone(),
                    count: *count,
                    timeout_ms,
                    format: format.name().into(),
                    first_at_ms,
                    interval_ms: interval.as_ref().map(millis).unwrap_or(0),
                    storage: crate::storage::StoragePolicy::default(),
                    on_exists: *on_exists,
                    context: context.clone(),
                }
            } else {
                SessionCommand::Capture {
                    output: output.clone(),
                    count: *count,
                    timeout_ms,
                    format: format.name().into(),
                    storage: crate::storage::StoragePolicy::default(),
                    on_exists: *on_exists,
                    context,
                }
            };
            command.validate()?;
            Some(command)
        }
        LegacyCommand::Record {
            output,
            codec,
            encoder,
            fps,
            bitrate,
            duration,
            max_file_size,
            on_exists,
            context,
            ..
        } => {
            let context = crate::context::from_pairs(context)?;
            let command = SessionCommand::Forward {
                output: output.clone(),
                codec: codec.clone(),
                encoder: encoder.clone(),
                fps: *fps,
                bitrate: bitrate.clone(),
                storage: crate::storage::StoragePolicy {
                    max_bytes: u64::MAX,
                    ..Default::default()
                },
                max_file_bytes: crate::storage::parse_bytes(max_file_size)?,
                on_exists: *on_exists,
                context,
                duration_ms: duration.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            };
            command.validate()?;
            Some(command)
        }
        LegacyCommand::Serve { name, .. } => {
            ipc::validate_name(name)?;
            None
        }
        _ => None,
    })
}

fn prepare_outputs(command: &LegacyCommand, prepared: &mut Option<SessionCommand>) -> Result<()> {
    // Check names as typed, so the advice repeats the person's own spelling.
    if let (
        Some(
            SessionCommand::Capture {
                output,
                count,
                format,
                ..
            }
            | SessionCommand::Schedule {
                output,
                count,
                format,
                ..
            },
        ),
        LegacyCommand::Capture { output: typed, .. },
    ) = (prepared.as_ref(), command)
        && output != "-"
    {
        crate::engine::check_capture_name(typed, *count, format)?;
    }
    if let Some(prepared) = prepared {
        usage(caller_output(prepared))?;
    }
    if let (
        Some(prepared),
        LegacyCommand::Capture { storage, .. } | LegacyCommand::Record { storage, .. },
    ) = (prepared, command)
    {
        match prepared {
            SessionCommand::Capture {
                storage: policy, ..
            }
            | SessionCommand::Schedule {
                storage: policy, ..
            }
            | SessionCommand::Forward {
                storage: policy, ..
            } => *policy = storage.policy()?,
            _ => unreachable!(),
        }
        usage(prepared.validate())?;
        if let SessionCommand::Capture {
            output,
            count,
            format,
            on_exists,
            ..
        }
        | SessionCommand::Schedule {
            output,
            count,
            format,
            on_exists,
            ..
        } = prepared
            && output != "-"
        {
            crate::engine::preflight_capture_output(output, *count, format, *on_exists)?;
        }
        if let SessionCommand::Forward {
            output, on_exists, ..
        } = prepared
        {
            crate::engine::preflight_recording_output(output, *on_exists)?;
        }
    }
    Ok(())
}

/// A taken output name, with how to keep both.
fn exists_hint(error: anyhow::Error, command: &LegacyCommand) -> anyhow::Error {
    let output = match command {
        LegacyCommand::Capture { output, .. } | LegacyCommand::Record { output, .. }
            if output != "-" =>
        {
            output
        }
        _ => return error,
    };
    let classified = crate::types::classify(&error);
    if classified.kind != ErrorKind::Exists || classified.hint.is_some() {
        return error;
    }
    let mut taken = Categorized::new(ErrorKind::Exists, format!("{error:#}"))
        .hint(summary::exists_hint(output));
    taken.details = classified.details;
    taken.into()
}

pub fn run(global: &Global, command: LegacyCommand) -> Result<Option<Report>> {
    run_inner(global, &command).map_err(|error| exists_hint(error, &command))
}

fn run_inner(global: &Global, command: &LegacyCommand) -> Result<Option<Report>> {
    let out = global.output();
    if let LegacyCommand::Storage { command } = command {
        let result = match command {
            Some(StorageCommand::Configure { storage }) => {
                crate::storage::configure_policy(&storage.policy()?)?
            }
            _ => crate::storage::quota_status()?,
        };
        let text = summary::storage_summary(&result);
        return Ok(Some(Report::json(result).text(text)));
    }
    if let LegacyCommand::Connect {
        target: Some(target),
        camera: CameraArg {
            camera: Some(camera),
        },
    } = command
        && target != camera
    {
        return Err(Usage(format!(
            "two cameras given ({} and -c {}); name one",
            crate::media::redact_url(target),
            crate::media::redact_url(camera)
        ))
        .into());
    }
    let base = std::env::current_dir()?;
    let mut camera = command.camera().map(str::to_owned);
    if let Some(camera) = &mut camera {
        usage(paths::resolve_selector(camera, &base))?;
        crate::camera::validate_selector_shape(camera)?;
    }
    let session = legacy_session(global, command)?;
    let mut prepared = usage(prepare(global, command, session.as_deref()))?;
    if session.is_none()
        && let Some(camera) = &camera
    {
        crate::camera::validate_selector(camera)?;
    }
    prepare_outputs(command, &mut prepared)?;
    if command.persistent() && session.is_none() {
        return Err(Usage(
            "this command needs a running session; start `capturefab serve --name NAME`, then pass --session NAME".into(),
        )
        .into());
    }
    if matches!(
        command,
        LegacyCommand::Capture { .. } | LegacyCommand::Record { .. }
    ) && camera.is_none()
        && session.is_none()
    {
        return Err(Usage(
            "choose a camera with -c CAMERA, or --session NAME (find IDs with capturefab list)"
                .into(),
        )
        .into());
    }
    if let LegacyCommand::Serve {
        name,
        cameras,
        stream,
        auto,
    } = command
    {
        let handle = SessionHandle::new();
        let client = LegacyClient {
            session: None,
            local: Some(handle.clone()),
            target: None,
        };
        for selector in &cameras.cameras {
            let mut selector = selector.clone();
            usage(paths::resolve_selector(&mut selector, &base))?;
            client.call(SessionCommand::connect(selector, 2000))?;
        }
        if *stream {
            client.call(SessionCommand::Start)?;
        }
        auto.apply(&client)?;
        let _server = ipc::Server::start_with_kind(handle.clone(), name, ipc::SessionKind::Serve)?;
        // Ctrl-C, SIGTERM from `systemctl stop` or SIGHUP from a closed
        // terminal; in place before serve says it is ready, so a signal
        // sent once it is always finds the handler.
        let (stop, stopped) = std::sync::mpsc::channel();
        let h = handle.clone();
        on_quit_signal(move || {
            let _ = stop.send(());
            h.shutdown();
        })?;
        let text = format!(
            "Session {name} is ready (pid {}). Try: capturefab --session {name} info\nCtrl-C stops the session.\n",
            std::process::id()
        );
        out.result(
            Report::json(json!({"session":name,"pid":std::process::id(),"ready":true})).text(text),
        );
        // After the handler, so Ctrl-C during the checks still cleans up.
        if !global.json && !global.quiet {
            crate::doctor::print_warnings();
        }
        // Blocks until a signal rather than waking to check for it.
        let _ = stopped.recv();
        // Wait for recordings to finish saving.
        handle.shutdown();
        return Ok(None);
    }
    let one_shot = matches!(
        command,
        LegacyCommand::Capture { .. } | LegacyCommand::Record { .. }
    );
    let client = LegacyClient::new(
        session.clone(),
        camera
            .as_deref()
            .filter(|_| !matches!(command, LegacyCommand::Connect { .. })),
        one_shot,
        !matches!(
            command,
            LegacyCommand::Status { .. } | LegacyCommand::Connect { .. }
        ),
        command.timeout_ms(),
    )?;
    // Ctrl-C stops direct acquisition and releases the camera rather than leaving control held.
    if let Some(h) = &client.local {
        let h = h.clone();
        on_quit_signal(move || h.shutdown())?;
    }
    let report = |value: Value, text: String| Ok(Some(Report::json(value).text(text)));
    match command {
        LegacyCommand::Connect { .. } => {
            let selector = camera.ok_or_else(|| {
                Usage(
                    "connect needs a camera: capturefab connect ID (find IDs with capturefab list)"
                        .into(),
                )
            })?;
            let value = client.call(SessionCommand::connect(selector, 2000))?;
            let connected = &value["connected"];
            let id = crate::media::redact_url(connected["id"].as_str().unwrap_or_default());
            let session = session.as_deref().unwrap_or_default();
            let target = format!(
                "--session {} capture -c {}",
                crate::hint::shell_quote(session),
                crate::hint::shell_quote(&id)
            );
            let text = format!(
                "Connected {} ({id}) in session {session}.\nNext: capturefab {target} -o 'shot-{{seq}}.png'\n",
                connected["model"].as_str().unwrap_or("camera"),
            );
            report(value, text)
        }
        LegacyCommand::Disconnect { .. } => report(
            client.call(SessionCommand::Disconnect)?,
            "Disconnected.\n".into(),
        ),
        LegacyCommand::Start { .. } => {
            let value = client.call(SessionCommand::Start)?;
            report(value, "Started.\n".into())
        }
        LegacyCommand::Stop { .. } => {
            let value = client.call(SessionCommand::Stop)?;
            report(value, "Stopped.\n".into())
        }
        LegacyCommand::Auto { balance, .. } => {
            let value = client.call(SessionCommand::Auto { balance: *balance })?;
            let text = serde_json::to_string_pretty(&value)? + "\n";
            report(value, text)
        }
        LegacyCommand::Manual { revert, .. } => {
            let value = client.call(SessionCommand::Manual { revert: *revert })?;
            let text = serde_json::to_string_pretty(&value)? + "\n";
            report(value, text)
        }
        LegacyCommand::StopForward { .. } => {
            let value = client.call(SessionCommand::StopForward)?;
            let text = serde_json::to_string_pretty(&value)? + "\n";
            report(value, text)
        }
        LegacyCommand::Jobs { .. } => {
            let value = client.call(SessionCommand::Jobs)?;
            let jobs = value["jobs"].as_array().cloned().unwrap_or_default();
            let text = summary::jobs_table(&jobs, session.as_deref(), crate::scheduling::now_ms());
            report(value, text)
        }
        LegacyCommand::Cancel { id, .. } => {
            let value = client.call(SessionCommand::CancelJob { id: *id })?;
            let text = format!("Cancelled capture job {}.\n", value["cancelled"]);
            report(value, text)
        }
        LegacyCommand::Status { .. } => {
            let value = client.call(SessionCommand::Status)?;
            let text = status_text(session.as_deref(), &value);
            report(value, text)
        }
        LegacyCommand::Record {
            output: destination,
            duration,
            auto,
            ..
        } => record(
            global,
            &client,
            session.as_deref(),
            destination,
            *duration,
            auto,
            prepared.unwrap(),
        ),
        LegacyCommand::Capture {
            output: destination,
            format,
            count,
            settings,
            auto,
            ..
        } => {
            if !settings.is_empty() {
                let values = settings
                    .iter()
                    .map(|setting| crate::controls::Assignment::parse(setting))
                    .collect::<Result<Vec<_>>>()?;
                client.call(SessionCommand::SetValues {
                    values,
                    dry_run: false,
                })?;
            }
            auto.apply(&client)?;
            if matches!(prepared, Some(SessionCommand::Schedule { .. })) {
                let result = client.call(prepared.clone().unwrap())?;
                if session.is_some() {
                    let text = summary::job_scheduled(
                        &result["job"],
                        camera.as_deref(),
                        session.as_deref().unwrap_or_default(),
                    );
                    return report(result, text);
                }
                return timelapse(global, &client, &result);
            }
            let result = client.call(prepared.clone().unwrap())?;
            if destination == "-" {
                let frame = client.local.as_ref().unwrap().captured_frame()?;
                io::stdout().lock().write_all(&crate::frame::encode(
                    &frame,
                    capture_format(*format, destination, *count)?.name(),
                )?)?;
                return Ok(None);
            }
            let text = summary::capture_summary(&result);
            report(result, text)
        }
        LegacyCommand::Serve { .. } | LegacyCommand::Storage { .. } => unreachable!(),
    }
}

fn status_text(session: Option<&str>, value: &Value) -> String {
    let mut out = format!("Session {}\n", session.unwrap_or("temporary"));
    let cameras = value["cameras"].as_array().cloned().unwrap_or_default();
    out.push_str(&format!(
        "{:<28} {:<10} {:>8} {:>10} OUTPUT\n",
        "CAMERA", "STATE", "FPS", "FRAMES"
    ));
    for camera in &cameras {
        out.push_str(&format!(
            "{:<28} {:<10} {:>8.1} {:>10} {}\n",
            camera["info"]["id"].as_str().unwrap_or(""),
            if camera["streaming"] == true {
                "streaming"
            } else {
                "idle"
            },
            camera["fps"].as_f64().unwrap_or(0.0),
            camera["frames"].as_u64().unwrap_or(0),
            camera["forwarding"].as_str().unwrap_or("")
        ));
        if let Some(error) = camera["last_error"].as_str() {
            out.push_str(&format!("  Error: {error}\n"));
        }
    }
    if cameras.is_empty() {
        out.push_str("No cameras connected. Use capturefab connect ID.\n");
    }
    out
}

/// Wait for a direct time lapse to finish, saying each saved frame.
fn timelapse(global: &Global, client: &LegacyClient, result: &Value) -> Result<Option<Report>> {
    let out = global.output();
    let id = result["job"]["id"]
        .as_u64()
        .context("schedule returned no job ID")?;
    out.note(format_args!(
        "Capture job {id} scheduled: {} frames every {}. Keep this process running; Ctrl-C cancels.",
        result["job"]["count"],
        summary::interval(result["job"]["interval_ms"].as_u64().unwrap_or(0))
    ));
    let h = client.local.as_ref().unwrap();
    let mut missing: Option<std::time::Instant> = None;
    let mut shown = 0;
    loop {
        if h.is_shutting_down() {
            return Ok(None);
        }
        let state = h.snapshot();
        let Some(job) = state
            .cameras
            .iter()
            .flat_map(|camera| &camera.jobs)
            .find(|job| job.id == id)
        else {
            let since = *missing.get_or_insert_with(std::time::Instant::now);
            ensure!(
                since.elapsed() < Duration::from_secs(5),
                "capture job disappeared"
            );
            std::thread::sleep(check_in(Some(Duration::ZERO)));
            continue;
        };
        missing = None;
        if job.captured > shown {
            // A look can miss a frame when the interval is shorter than the
            // wait between looks; still count each one.
            for missed in shown + 1..job.captured {
                out.note(format_args!("{missed}/{} saved", job.count));
            }
            shown = job.captured;
            out.note(summary::job_progress(
                &json!(job),
                crate::scheduling::now_ms(),
            ));
        }
        match job.status.as_str() {
            "complete" => {
                let value = json!({ "job": job });
                let text = summary::job_finished(&value["job"]);
                return Ok(Some(Report::json(value).text(text)));
            }
            "failed" => anyhow::bail!(
                "{}",
                job.error.as_deref().unwrap_or("scheduled capture failed")
            ),
            "cancelled" => anyhow::bail!("capture job cancelled"),
            _ => {}
        }
        let next = job.next_at_ms.saturating_sub(crate::scheduling::now_ms());
        std::thread::sleep(check_in(Some(Duration::from_millis(next))));
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    global: &Global,
    client: &LegacyClient,
    session: Option<&str>,
    destination: &str,
    duration: Option<Duration>,
    auto: &AutoArgs,
    prepared: SessionCommand,
) -> Result<Option<Report>> {
    let out = global.output();
    let duration_ms = duration.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    auto.apply(client)?;
    let status_camera = |state: &Value| state["cameras"][0].clone();
    // The camera's newest error before the recording: one after it means
    // the recording failed, even before a status shows it.
    let errors = match duration_ms {
        None => ErrorMark::default(),
        Some(_) => ErrorMark::of(&ForwardStatus::of(&status_camera(
            &client.call(SessionCommand::Status)?,
        ))),
    };
    let mut result = client.call(prepared)?;
    // The session ends a timed recording at its end time; the CLI waits for
    // that, and stops it only on Ctrl-C or as a fallback.
    let mut timed = duration_ms
        .map(|ms| TimedRecording::new(ms, &result, errors))
        .transpose()?;
    let shown = result["path"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::media::redact_url(destination));
    let finish = |result: Value| -> Result<Option<Report>> {
        let text = if result["stopped"].is_object() {
            let (text, warnings) =
                summary::forward_summary(destination, &result, crate::scheduling::now_ms());
            for warning in warnings {
                out.note(format_args!("capturefab: {warning}"));
            }
            text
        } else {
            summary::forward_started(destination, &result, session.unwrap_or("temporary"), None)
        };
        Ok(Some(Report::json(result).text(text)))
    };
    if session.is_none() {
        out.note(summary::forward_banner(destination, &shown, duration_ms));
        let epoch = std::time::Instant::now();
        let h = client.local.as_ref().unwrap();
        loop {
            std::thread::sleep(check_in(
                timed
                    .as_ref()
                    .map(|timed| timed.duration.saturating_sub(epoch.elapsed())),
            ));
            if h.is_shutting_down() {
                return Ok(None);
            }
            let state = serde_json::to_value(h.snapshot())?;
            let camera = status_camera(&state);
            let next = match &mut timed {
                Some(timed) => timed.next(
                    ForwardStatus::of(&camera),
                    crate::scheduling::now_ms(),
                    epoch.elapsed(),
                    false,
                ),
                None if camera["forwarding"].is_string() => RecordingWait::Wait,
                None => RecordingWait::Failed,
            };
            match next {
                RecordingWait::Wait => {}
                RecordingWait::Finished => {
                    result["stopped"] = timed
                        .as_ref()
                        .unwrap()
                        .finished(&camera["forwarding_progress"]);
                    break;
                }
                RecordingWait::Stop => {
                    result["stopped"] = client.call(timed.as_ref().unwrap().stop_command())?;
                    break;
                }
                RecordingWait::Failed => anyhow::bail!(
                    "{}",
                    camera["last_error"]
                        .as_str()
                        .unwrap_or("forwarding stopped")
                ),
            }
        }
        return finish(result);
    }
    if let Some(mut timed) = timed {
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        on_quit_signal(move || flag.store(true, Ordering::Relaxed))?;
        out.note(summary::forward_banner(destination, &shown, duration_ms));
        let epoch = std::time::Instant::now();
        loop {
            std::thread::sleep(check_in(Some(
                timed.duration.saturating_sub(epoch.elapsed()),
            )));
            let state = client.call(SessionCommand::Status)?;
            let camera = status_camera(&state);
            match timed.next(
                ForwardStatus::of(&camera),
                crate::scheduling::now_ms(),
                epoch.elapsed(),
                stopped.load(Ordering::Relaxed),
            ) {
                RecordingWait::Wait => {}
                // Never stop-forward here: another client may already
                // record on this camera.
                RecordingWait::Finished => {
                    result["stopped"] = timed.finished(&camera["forwarding_progress"]);
                    break;
                }
                RecordingWait::Stop => {
                    result["stopped"] = client.call(timed.stop_command())?;
                    break;
                }
                RecordingWait::Failed => anyhow::bail!(
                    "{}",
                    camera["last_error"]
                        .as_str()
                        .unwrap_or("forwarding stopped before its duration elapsed")
                ),
            }
        }
    }
    finish(result)
}

type QuitHandler = Box<dyn FnMut() + Send>;

/// Run `handler` on Ctrl-C, SIGTERM or SIGHUP (unless the process ignores
/// SIGHUP, as under nohup). Every registered handler runs, in order.
pub(crate) fn on_quit_signal(handler: impl FnMut() + Send + 'static) -> Result<()> {
    static HANDLERS: OnceLock<Arc<Mutex<Vec<QuitHandler>>>> = OnceLock::new();
    let mut installed = Ok(());
    let handlers = HANDLERS.get_or_init(|| {
        let handlers: Arc<Mutex<Vec<QuitHandler>>> = Arc::default();
        let run = handlers.clone();
        #[cfg(unix)]
        let hangup_ignored = hangup_ignored();
        installed = ctrlc::set_handler(move || {
            for handler in run.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
                handler();
            }
        })
        .context("install signal handler");
        #[cfg(unix)]
        if hangup_ignored {
            // SAFETY: setting a standard signal's disposition to ignore has
            // no preconditions.
            unsafe {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
        }
        handlers
    });
    installed?;
    handlers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(handler));
    Ok(())
}
#[cfg(unix)]
fn hangup_ignored() -> bool {
    // SAFETY: a null new action only reads the current one into `old`.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) == 0
            && old.sa_sigaction == libc::SIG_IGN
    }
}

/// How long a wait loop sleeps before it checks again: a second, or until
/// its next deadline when that is sooner, but not under 100 ms.
fn check_in(deadline: Option<Duration>) -> Duration {
    const MOST: Duration = Duration::from_secs(1);
    deadline.map_or(MOST, |left| left.clamp(Duration::from_millis(100), MOST))
}

const RECORDING_GRACE: Duration = Duration::from_secs(2);

/// What `record`/`forward --duration` does after one look at the status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordingWait {
    /// The recording runs on; look again.
    Wait,
    /// The session ended the recording at its end time. Nothing is left to
    /// stop, and stop-forward could stop a newer recording on that camera.
    Finished,
    /// Ctrl-C, or the CLI's own timer ended while still recording: send
    /// stop-forward.
    Stop,
    /// The recording stopped before its duration elapsed, or with an error.
    Failed,
}

/// What one status says about the camera's forward.
#[derive(Clone, Copy)]
struct ForwardStatus<'a> {
    forwarding: bool,
    forwarding_id: Option<&'a str>,
    last_error: Option<&'a str>,
    /// When the camera last logged an error (UTC Unix ms), which tells the
    /// same error logged again from the one before.
    error_logged_ms: Option<u64>,
}
impl<'a> ForwardStatus<'a> {
    /// The camera's entry in a `status` reply.
    fn of(camera: &'a Value) -> Self {
        Self {
            forwarding: !camera["forwarding"].is_null(),
            forwarding_id: camera["forwarding_id"].as_str(),
            last_error: camera["last_error"].as_str(),
            error_logged_ms: camera["error_logged_ms"].as_u64(),
        }
    }
}

/// The camera's newest error as one status shows it.
#[derive(Clone, Debug, Default, PartialEq)]
struct ErrorMark {
    text: Option<String>,
    logged_ms: Option<u64>,
}
impl ErrorMark {
    fn of(status: &ForwardStatus) -> Self {
        Self {
            text: status.last_error.map(str::to_owned),
            logged_ms: status.error_logged_ms,
        }
    }
    /// Whether `status` shows an error logged after this one: another
    /// error, or the same one again.
    fn newer_in(&self, status: &ForwardStatus) -> bool {
        status.last_error != self.text.as_deref() || status.error_logged_ms > self.logged_ms
    }
}

/// A timed recording or forward that the CLI waits on.
struct TimedRecording {
    /// The requested duration, timed by the CLI from the forward reply.
    duration: Duration,
    /// When the session ends it (the reply's `until_ms`, UTC Unix ms).
    until_ms: u64,
    /// The worker-issued identity is authoritative even when a replacement
    /// starts in the same millisecond with the same output name.
    forwarding_id: String,
    /// The camera's newest error from before the recording started, then
    /// as of the last status that showed it running. A newer one once it
    /// has stopped means it failed. A recording that wrote video retains
    /// that file; one that failed before writing may have none. This holds
    /// for a recording that fails before any status
    /// shows it running, as one does when FFmpeg cannot start.
    errors: ErrorMark,
}
impl TimedRecording {
    fn new(duration_ms: u64, reply: &Value, errors: ErrorMark) -> Result<Self> {
        Ok(Self {
            duration: Duration::from_millis(duration_ms),
            until_ms: reply["until_ms"]
                .as_u64()
                .context("The timed recording reply is missing its end time.")?,
            forwarding_id: reply["forwarding_id"]
                .as_str()
                .filter(|id| crate::session::valid_forwarding_id(id))
                .context("The recording reply is missing a valid forwarding identity.")?
                .to_owned(),
            errors,
        })
    }
    /// The `stopped` result of a recording the session ended on time, with
    /// its final counters when `progress` (the status's
    /// `forwarding_progress`) is this recording's, not a later one's.
    fn finished(&self, progress: &Value) -> Value {
        let mut stopped = json!({"forwarding": null, "by": "duration"});
        if progress["id"].as_str() == Some(self.forwarding_id.as_str()) {
            let mut stats = progress.clone();
            if let Some(stats) = stats.as_object_mut() {
                stats.remove("id");
            }
            stopped["stats"] = stats;
        }
        stopped
    }
    fn stop_command(&self) -> SessionCommand {
        SessionCommand::StopForwardIf {
            forwarding_id: self.forwarding_id.clone(),
        }
    }
    /// Decide from a `status` read at `now_ms` (UTC Unix ms), `elapsed`
    /// after the forward reply.
    fn next(
        &mut self,
        status: ForwardStatus,
        now_ms: u64,
        elapsed: Duration,
        interrupted: bool,
    ) -> RecordingWait {
        let ours = status.forwarding && status.forwarding_id == Some(self.forwarding_id.as_str());
        if !ours {
            let grace = RECORDING_GRACE.as_millis() as u64;
            let on_time = now_ms.saturating_add(grace) >= self.until_ms || elapsed >= self.duration;
            return if on_time && !self.errors.newer_in(&status) {
                RecordingWait::Finished
            } else {
                RecordingWait::Failed
            };
        }
        // An error while it still runs did not end it. The worker logs the
        // error that ends a forward in the step that clears it.
        self.errors = ErrorMark::of(&status);
        // When the session keeps the end time, give it the grace to act
        // first; stop-forward is then only a fallback.
        let timer = self.duration.saturating_add(RECORDING_GRACE);
        if interrupted || elapsed >= timer {
            RecordingWait::Stop
        } else {
            RecordingWait::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_images_never_go_to_an_interactive_terminal() {
        let error = binary_stdout_allowed("-", true).unwrap_err();
        assert!(error.to_string().contains("terminal"), "{error:#}");
        binary_stdout_allowed("-", false).unwrap();
        binary_stdout_allowed("frame.png", true).unwrap();
    }
    #[test]
    fn wait_loops_check_in_once_a_second_or_at_their_deadline() {
        let second = Duration::from_secs(1);
        assert_eq!(check_in(None), second);
        assert_eq!(check_in(Some(Duration::from_secs(3600))), second);
        assert_eq!(
            check_in(Some(Duration::from_millis(400))),
            Duration::from_millis(400)
        );
        assert_eq!(check_in(Some(Duration::ZERO)), Duration::from_millis(100));
    }
    #[test]
    fn formats_follow_files_templates_and_explicit_overrides() {
        for (path, count, expected) in [
            ("shot.JPG", 1, "jpeg"),
            ("shot.raw", 1, "raw"),
            ("frame-{frame}.ppm", 10, "ppm"),
            ("frames.raw", 10, "png"),
            ("-", 1, "png"),
        ] {
            assert_eq!(capture_format(None, path, count).unwrap().name(), expected);
        }
        assert!(capture_format(None, "shot.gif", 1).is_err());
        assert_eq!(
            capture_format(Some(ImageFormat::Png), "shot.raw", 1)
                .unwrap()
                .name(),
            "png"
        );
    }
    #[test]
    fn balance_values() {
        for (text, balance) in [
            ("0", 0.0),
            ("0.25", 0.25),
            ("1", 1.0),
            ("quality", 0.0),
            ("balanced", 0.5),
            ("frame-rate", 1.0),
            ("speed", 1.0),
        ] {
            assert_eq!(parse_balance(text), Ok(balance));
        }
        for text in ["-0.1", "1.01", "inf", "NaN", "fast", ""] {
            assert!(
                parse_balance(text)
                    .unwrap_err()
                    .contains("quality, balanced, frame-rate")
            );
        }
    }
    #[test]
    fn timed_recordings_end_in_the_session_and_stop_forward_is_a_fallback() {
        use RecordingWait::{Failed, Finished, Stop, Wait};
        let since = 1_000_000;
        let id = "11111111111111111111111111111111";
        let replacement = "22222222222222222222222222222222";
        let reply = json!({"forwarding":"clip.mkv","forwarding_id":id,"since_ms":since,"until_ms":since + 30_000});
        let earlier = Some("an earlier failure");
        let logged = Some(since - 5_000);
        let status = |forwarding, forwarding_id, last_error, error_logged_ms| ForwardStatus {
            forwarding,
            forwarding_id,
            last_error,
            error_logged_ms,
        };
        // The camera's error from before the recording started.
        let before = ErrorMark::of(&status(false, None, earlier, logged));
        let recording = status(true, Some(id), earlier, logged);
        let stopped = status(false, None, earlier, logged);
        let s = Duration::from_secs;
        let mut timed = TimedRecording::new(30_000, &reply, before.clone()).unwrap();
        // Still recording: wait out the duration and the session's grace.
        assert_eq!(timed.next(recording, since + 10_000, s(10), false), Wait);
        assert_eq!(timed.next(recording, since + 31_000, s(31), false), Wait);
        assert_eq!(timed.next(recording, since + 32_000, s(32), false), Stop);
        // Ctrl-C stops it at once.
        assert_eq!(timed.next(recording, since + 1_000, s(1), true), Stop);
        // The session ended it on time, or just before: nothing to stop. An
        // error from before the recording does not count against it.
        for now in [since + 28_000, since + 30_000, since + 45_000] {
            assert_eq!(timed.next(stopped, now, s(30), false), Finished);
        }
        assert_eq!(timed.next(stopped, since + 30_000, s(30), true), Finished);
        // The worker's monotonic timer can finish while UTC is still behind
        // its nominal deadline after a system-clock rollback.
        assert_eq!(timed.next(stopped, since - 60_000, s(30), false), Finished);
        // Another client's newer recording is not this one to stop.
        let newer = status(true, Some(replacement), earlier, logged);
        assert_eq!(timed.next(newer, since + 31_000, s(33), false), Finished);
        // Stopped early, or with a new error, is a failure even if video was kept.
        assert_eq!(timed.next(stopped, since + 27_999, s(28), false), Failed);
        let early = status(true, Some(replacement), None, None);
        assert_eq!(timed.next(early, since + 6_000, s(6), false), Failed);
        let full = Some("Forwarding stopped: Storage full");
        let storage_full = status(false, None, full, Some(since + 29_000));
        assert_eq!(
            timed.next(storage_full, since + 29_500, s(29), false),
            Failed
        );
        assert_eq!(
            timed.next(storage_full, since + 30_000, s(30), false),
            Failed
        );
        // The same error logged again is a new error, and a line that has
        // left the bounded log is not.
        let again = status(false, None, earlier, Some(since + 29_000));
        assert_eq!(timed.next(again, since + 30_000, s(30), false), Failed);
        let trimmed = status(false, None, earlier, None);
        assert_eq!(timed.next(trimmed, since + 30_000, s(30), false), Finished);
        // An error while it still ran did not end it.
        let mut noted = TimedRecording::new(30_000, &reply, before.clone()).unwrap();
        let busy = Some("Shared frame buffer: busy");
        let warned = status(true, Some(id), busy, Some(since + 10_000));
        assert_eq!(noted.next(warned, since + 10_000, s(10), false), Wait);
        let ended = status(false, None, busy, Some(since + 10_000));
        assert_eq!(noted.next(ended, since + 30_000, s(30), false), Finished);
        // A short recording can fail, or end, before any status shows it
        // running (FFmpeg starts with the first frame). Only an error
        // logged since it started tells the two apart, even the same one.
        let one_second = json!({"forwarding_id":id,"since_ms":since,"until_ms":since + 1_000});
        let short = || TimedRecording::new(1_000, &one_second, before.clone()).unwrap();
        let at = since + 300;
        assert_eq!(short().next(stopped, at, s(0), false), Finished);
        let no_ffmpeg = Some("Forwarding stopped: no FFmpeg");
        let failed = status(false, None, no_ffmpeg, Some(since + 50));
        assert_eq!(short().next(failed, at, s(0), false), Failed);
        let retried = status(false, None, earlier, Some(since + 50));
        assert_eq!(short().next(retried, at, s(0), false), Failed);
        // Final counters are this recording's only, never a replacement's,
        // and an older worker without them reports none rather than zeros.
        let progress =
            json!({"id":id,"accepted":30,"written":30,"dropped":0,"late":0,"stalled":false});
        assert_eq!(
            short().finished(&progress),
            json!({"forwarding":null,"by":"duration","stats":{"accepted":30,"written":30,"dropped":0,"late":0,"stalled":false}})
        );
        let mut other = progress.clone();
        other["id"] = json!("22222222222222222222222222222222");
        for absent in [other, Value::Null] {
            assert_eq!(
                short().finished(&absent),
                json!({"forwarding":null,"by":"duration"})
            );
        }
    }
    #[test]
    fn timed_recording_fallback_cannot_stop_a_same_millisecond_replacement() {
        let first = "11111111111111111111111111111111";
        let replacement = "22222222222222222222222222222222";
        let reply = json!({"forwarding_id":first,"since_ms":1000,"until_ms":31000});
        let mut timed = TimedRecording::new(30_000, &reply, ErrorMark::default()).unwrap();
        let status = |id| ForwardStatus {
            forwarding: true,
            forwarding_id: Some(id),
            last_error: None,
            error_logged_ms: None,
        };
        assert_eq!(
            timed.next(status(first), 2000, Duration::from_secs(1), true),
            RecordingWait::Stop
        );
        assert!(
            matches!(timed.stop_command(), SessionCommand::StopForwardIf { forwarding_id } if forwarding_id == first)
        );
        assert_eq!(
            timed.next(status(replacement), 32000, Duration::from_secs(32), true),
            RecordingWait::Finished
        );
        assert_eq!(
            timed.next(status(replacement), 2000, Duration::from_secs(1), true),
            RecordingWait::Failed
        );
        for incomplete in [
            json!({"until_ms":31000}),
            json!({"forwarding_id":first}),
            json!({"forwarding_id":"invalid","until_ms":31000}),
        ] {
            assert!(TimedRecording::new(30_000, &incomplete, ErrorMark::default()).is_err());
        }
    }
    #[test]
    fn a_timed_recording_reads_its_cameras_status() {
        let camera = json!({"forwarding": "clip.mkv", "forwarding_id": "a", "last_error": "x", "error_logged_ms": 5});
        let status = ForwardStatus::of(&camera);
        assert!(status.forwarding);
        assert_eq!(
            (
                status.forwarding_id,
                status.last_error,
                status.error_logged_ms
            ),
            (Some("a"), Some("x"), Some(5))
        );
        assert!(!ForwardStatus::of(&json!({"forwarding": null})).forwarding);
    }
    #[test]
    fn storage_flags_take_durations_with_units() {
        use clap::Parser;
        #[derive(Parser)]
        struct Probe {
            #[command(flatten)]
            storage: StorageArgs,
        }
        let probe = Probe::try_parse_from(["x", "--max-age", "168h", "--delete-oldest"]).unwrap();
        let policy = probe
            .storage
            .apply_policy(crate::storage::StoragePolicy::default())
            .unwrap();
        assert_eq!(policy.max_age_seconds, Some(168 * 3600));
        assert!(Probe::try_parse_from(["x", "--max-age", "7"]).is_err());
    }
}
