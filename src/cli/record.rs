//! `capturefab record`: video to a file through FFmpeg, or a stream to a
//! server. It blocks until `--for` ends it, or a signal: an open-ended
//! recording stopped by a signal ends normally. With `--background` the
//! recording runs on in a session as a job.
use super::{
    Client, Global, Route,
    args::{CameraArg, KeepArg, Wait},
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
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Codec {
    H264,
    #[value(alias = "h265")]
    Hevc,
    Av1,
    Ffv1,
}
impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
            Self::Ffv1 => "ffv1",
        }
    }
}

#[derive(Debug, Args)]
pub struct RecordArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// A file (.mkv, .mp4, .mov, .ts, .webm; it must not exist, or use {seq}) or a stream URL (rtsp://, srt://, rtmp://, udp://)
    #[arg(short = 'o', long, value_name = "FILE|URL")]
    pub output: String,
    /// Stop after this long; without it, record until Ctrl-C (or jobs stop with --background)
    #[arg(long = "for", value_name = "DUR", value_parser = super::args::duration)]
    pub duration: Option<Duration>,
    /// Frames per second written (default: the camera's frame rate)
    #[arg(long, value_name = "N")]
    pub fps: Option<f64>,
    /// h264, hevc or av1 (where a hardware encoder has them), or ffv1 (lossless, .mkv)
    #[arg(long, value_enum, default_value_t = Codec::H264)]
    pub codec: Codec,
    /// Target bitrate, such as 8M or 800k (not for ffv1)
    #[arg(long, default_value = "8M", value_name = "RATE")]
    pub bitrate: String,
    /// End the recording, successfully, when the file reaches this size, such as 2G
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub max_size: Option<u64>,
    /// Change settings first, as one atomic change (as set does); repeatable
    #[arg(long = "set", value_name = "NAME=VALUE")]
    pub settings: Vec<String>,
    /// Settle exposure and gain first, then hold them (as auto does)
    #[arg(long)]
    pub auto: bool,
    /// How long to wait for the first frame; none waits for a trigger until stopped
    #[arg(long, default_value = "10s", value_name = "DUR", value_parser = super::args::wait)]
    pub timeout: Wait,
    /// Leave the recording running in a session and print its job once the first frame is encoded
    #[arg(long)]
    pub background: bool,
    #[command(flatten)]
    pub keep: KeepArg,
}

fn parse_size(text: &str) -> std::result::Result<u64, String> {
    crate::storage::parse_bytes(text).map_err(|error| error.to_string())
}

/// How often a recording's state is looked at.
const POLL: Duration = Duration::from_millis(200);
/// How long after `--for` the session may take to end a recording itself
/// before the CLI stops it.
const GRACE: Duration = Duration::from_secs(2);

/// A local file's path, absolute; a stream URL as it is.
fn output(args: &RecordArgs) -> Result<String> {
    let mut output = args.output.clone();
    crate::naming::validate_output(&output).map_err(super::input)?;
    if !crate::media::is_recording(&output) {
        if args.codec == Codec::Ffv1 {
            return Err(Usage("ffv1 recordings are .mkv files, not streams".into()).into());
        }
        return Ok(output);
    }
    let path = crate::media::local_file_path(&output).map_err(super::input)?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("mkv" | "mp4" | "mov" | "ts" | "webm") | None => {}
        Some("avi") => {
            return Err(Categorized::new(
                ErrorKind::Usage,
                "AVI cannot hold the video Capturefab records",
            )
            .hint("use .mkv, or .mp4 for players that need it")
            .into());
        }
        Some(other) => {
            return Err(Usage(format!(
                "cannot record to .{other}; use .mkv, .mp4, .mov, .ts or .webm"
            ))
            .into());
        }
    }
    if args.codec == Codec::Ffv1 && extension.as_deref() != Some("mkv") {
        return Err(Usage("ffv1 recordings are .mkv files".into()).into());
    }
    if output
        .get(..5)
        .is_none_or(|scheme| !scheme.eq_ignore_ascii_case("file:"))
    {
        super::paths::resolve_path(&mut output, &std::env::current_dir()?).map_err(super::input)?;
    }
    Ok(output)
}

/// The camera's frame rate, as `info` reports it, for `--fps` unset.
fn camera_fps(info: &Value) -> f64 {
    let number = |value: &Value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
    };
    [
        info.get("fps"),
        info.get("fps_limit"),
        info["controls"]["fps"].get("value"),
    ]
    .into_iter()
    .flatten()
    .find_map(number)
    .filter(|fps| fps.is_finite() && *fps >= 0.1)
    .map_or(30.0, |fps| fps.min(240.0))
}

pub fn run(global: &Global, args: RecordArgs) -> Result<Report> {
    let output = output(&args)?;
    let settings = args
        .settings
        .iter()
        .map(|setting| Assignment::parse(setting))
        .collect::<Result<Vec<_>>>()?;
    if let Some(fps) = args.fps
        && !(fps.is_finite() && (0.1..=240.0).contains(&fps))
    {
        return Err(Usage("--fps must be 0.1..240".into()).into());
    }
    let max_file_bytes = args.max_size.unwrap_or(crate::media::NO_SIZE_CAP);
    let request = |fps: f64| SessionCommand::Forward {
        output: output.clone(),
        codec: args.codec.name().into(),
        encoder: "auto".into(),
        fps,
        bitrate: args.bitrate.clone(),
        storage: crate::storage::StoragePolicy {
            max_bytes: u64::MAX,
            ..Default::default()
        },
        max_file_bytes,
        on_exists: Default::default(),
        context: Default::default(),
        duration_ms: args.duration.map(millis),
        utc_offset_s: Some(crate::naming::local_offset()),
    };
    // Everything that can be checked here is, before a camera is opened:
    // the request, a taken file, FFmpeg and an encoder for the codec.
    request(args.fps.unwrap_or(30.0))
        .validate()
        .map_err(super::input)?;
    if crate::media::is_recording(&output) {
        crate::engine::preflight_recording_output(&output, Default::default()).map_err(|error| {
            if crate::types::classify(&error).kind == ErrorKind::Exists {
                let mut taken = Categorized::new(ErrorKind::Exists, format!("{error:#}"))
                    .hint("Capturefab never overwrites files: add {seq} to -o for a new name each time");
                taken.details = crate::types::classify(&error).details;
                taken.into()
            } else {
                error
            }
        })?;
    }
    crate::media::executable()?;
    crate::media::check_codec(&crate::media::ForwardConfig {
        output: output.clone(),
        codec: args.codec.name().into(),
        encoder: "auto".into(),
        fps: args.fps.unwrap_or(30.0),
        bitrate: args.bitrate.clone(),
        storage: Default::default(),
        max_file_bytes,
    })?;
    signals::take_over();
    let client = Client::open_with(
        global,
        args.camera.camera.as_deref(),
        Route {
            keep: args.keep.route().keep,
            background: args.background,
            connect: None,
        }
        .waiting(args.timeout),
    )?;
    if !settings.is_empty() {
        client.call(SessionCommand::SetValues {
            values: settings,
            dry_run: false,
        })?;
    }
    if args.auto {
        super::auto::settle(&client, Duration::from_secs(5))?;
    }
    let info = client.call(SessionCommand::Info)?;
    let fps = args.fps.unwrap_or_else(|| camera_fps(&info));
    if signals::interrupted() {
        return Err(Categorized::error(
            ErrorKind::Interrupted,
            "interrupted before recording",
        ));
    }
    let started = Instant::now();
    let reply = client.call(request(fps))?;
    let id = reply["forwarding_id"]
        .as_str()
        .context("recording without an identity")?
        .to_owned();
    let recording = Recording {
        client: &client,
        session: client.session().unwrap_or("direct").to_owned(),
        id,
        job: reply["job"].as_u64().context("recording without a job")?,
        args: &args,
        info: &info,
        output: reply["path"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::media::redact_url(&output)),
        started,
        fps,
    };
    let value = if args.background {
        recording.armed()?
    } else {
        recording.wait()?
    };
    let text = text(&value);
    Ok(Report::json(value).text(text))
}

/// A recording this command started, and what it needs to report it.
struct Recording<'a> {
    client: &'a Client,
    session: String,
    id: String,
    /// Its job number in the session, whose record says how it ended.
    job: u64,
    args: &'a RecordArgs,
    info: &'a Value,
    output: String,
    started: Instant,
    fps: f64,
}

/// How a recording ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    Duration,
    Signal,
    MaxSize,
    /// Another command stopped it (jobs stop, session stop).
    Stopped,
}
impl Ended {
    fn name(self) -> &'static str {
        match self {
            Self::Duration => "duration",
            Self::Signal => "signal",
            Self::MaxSize => "max_size",
            Self::Stopped => "stopped",
        }
    }
}

impl Recording<'_> {
    /// The camera's status entry and the session's record of this recording.
    fn look(&self) -> Result<(Value, Value)> {
        let camera = self.client.status()?;
        let entry = jobs::recording_entry(&camera, self.job)
            .cloned()
            .context("the session no longer lists this recording")?;
        Ok((camera, entry))
    }
    /// The first frame's wait.
    fn first_frame_wait(&self) -> Option<Duration> {
        match self.args.timeout {
            Wait::For(wait) => Some(wait),
            Wait::Forever => None,
        }
    }
    fn stop(&self) -> Result<Value> {
        self.client.call(SessionCommand::StopForwardIf {
            forwarding_id: self.id.clone(),
        })
    }
    /// Wait for the first frame to be encoded, then leave the recording
    /// running: its job. A recording that could not start fails.
    fn armed(&self) -> Result<Value> {
        loop {
            let (camera, entry) = self.look()?;
            let job = jobs::recording(&self.session, &camera, &entry);
            if entry["running"] != true {
                if entry["ended"] == "failed" {
                    return Err(self.failure(&camera, &entry));
                }
                return Ok(json!({ "job": job }));
            }
            if entry["progress"]["written"].as_u64().unwrap_or(0) > 0 {
                return Ok(json!({ "job": job }));
            }
            if let Some(wait) = self.first_frame_wait()
                && self.started.elapsed() > wait
            {
                let _ = self.stop();
                return Err(self.no_frames());
            }
            if signals::interrupted() {
                let _ = self.stop();
                return Err(Categorized::error(
                    ErrorKind::Interrupted,
                    "interrupted before the recording started",
                ));
            }
            std::thread::sleep(POLL / 4);
        }
    }
    fn no_frames(&self) -> anyhow::Error {
        crate::types::frame_timeout(format!(
            "no frame reached the encoder within {}",
            super::args::duration_text(self.first_frame_wait().unwrap_or_default())
        ))
    }
    /// The recording's error, as the session recorded it, with its reason.
    fn failure(&self, camera: &Value, entry: &Value) -> anyhow::Error {
        let message = entry["error"]
            .as_str()
            .map(crate::scrub::redact_urls)
            .unwrap_or_else(|| "the recording stopped".into());
        let tag = serde_json::from_value::<crate::types::ErrorTag>(entry["error_tag"].clone()).ok();
        crate::types::with_details(
            crate::types::error_from_tag(tag, message),
            json!({ "job": jobs::recording(&self.session, camera, entry) }),
        )
    }
    /// Wait until the recording ends: its result.
    fn wait(&self) -> Result<Value> {
        let duration = self.args.duration;
        loop {
            let (camera, entry) = self.look()?;
            if entry["running"] != true {
                let ended = match entry["ended"].as_str() {
                    Some("duration") => Ended::Duration,
                    Some("max_size") if self.args.max_size.is_some() => Ended::MaxSize,
                    Some("stopped") => Ended::Stopped,
                    _ => return Err(self.failure(&camera, &entry)),
                };
                let value = self.result(&entry, ended, None);
                if ended == Ended::Stopped && duration.is_some() {
                    return Err(Categorized::new(
                        ErrorKind::Incomplete,
                        format!(
                            "the recording was stopped after {} of {}",
                            crate::scheduling::format_duration(millis(self.started.elapsed())),
                            super::args::duration_text(duration.unwrap_or_default())
                        ),
                    )
                    .details(value)
                    .into());
                }
                return Ok(value);
            }
            let written = entry["progress"]["written"].as_u64().unwrap_or(0);
            if written == 0
                && let Some(wait) = self.first_frame_wait()
                && self.started.elapsed() > wait
            {
                let _ = self.stop();
                return Err(self.no_frames());
            }
            let late = duration.is_some_and(|duration| self.started.elapsed() > duration + GRACE);
            if signals::interrupted() || late {
                let stopped = self.stop();
                let (camera, entry) = self.look()?;
                if entry["ended"] == "failed" {
                    return Err(self.failure(&camera, &entry));
                }
                let ended = if late { Ended::Duration } else { Ended::Signal };
                let value = self.result(&entry, ended, Some(&stopped));
                // A recording asked to run for a time and cut short is
                // interrupted; an open-ended one ends like this.
                if ended == Ended::Signal && duration.is_some() {
                    return Err(Categorized::new(
                        ErrorKind::Interrupted,
                        format!(
                            "interrupted after {} of the recording",
                            crate::scheduling::format_duration(millis(self.started.elapsed()))
                        ),
                    )
                    .details(value)
                    .into());
                }
                return Ok(value);
            }
            std::thread::sleep(POLL);
        }
    }
    /// The finished recording's result, from the session's record of it
    /// and the stop reply when this command stopped it.
    fn result(&self, entry: &Value, ended: Ended, stopped: Option<&Result<Value>>) -> Value {
        let progress = &entry["progress"];
        let file = &progress["recording"];
        let stats = stopped
            .and_then(|stopped| stopped.as_ref().ok())
            .map(|stopped| &stopped["stats"]);
        let duration_ms = match (file["started_ms"].as_u64(), file["ended_ms"].as_u64()) {
            (Some(start), Some(end)) => Some(end.saturating_sub(start)),
            _ => entry["ended_ms"]
                .as_u64()
                .zip(entry["since_ms"].as_u64())
                .map(|(end, start)| end.saturating_sub(start)),
        };
        let duration_s = duration_ms.map_or_else(
            || self.started.elapsed().as_secs_f64(),
            |ms| ms as f64 / 1000.0,
        );
        let frames = stats
            .and_then(|stats| stats["written"].as_u64())
            .or(progress["written"].as_u64());
        let dropped = stats
            .and_then(|stats| stats["dropped"].as_u64())
            .or(progress["dropped"].as_u64());
        let encoder = file["encoder"]
            .as_str()
            .or(stats.and_then(|stats| stats["encoder"].as_str()))
            .unwrap_or_default();
        let local = crate::media::is_recording(&self.args.output);
        let finalized = stopped.is_none_or(Result::is_ok) && (!local || file.is_object());
        let mut value = json!({
            "camera": self.info["camera"],
            "output": file["path"].as_str().map_or_else(|| self.output.clone(), str::to_owned),
            "bytes": file["bytes"],
            "duration_s": (duration_s * 1000.0).round() / 1000.0,
            "frames": frames,
            "dropped": dropped,
            "width": self.info["width"],
            "height": self.info["height"],
            "codec": self.args.codec.name(),
            "encoder": encoder,
            "ended": ended.name(),
            "finalized": finalized,
        });
        if let Some(frames) = frames.filter(|_| duration_s > 0.0) {
            value["fps_actual"] = json!((frames as f64 / duration_s * 100.0).round() / 100.0);
        }
        value["fps"] = json!(self.fps);
        crate::types::without_nulls(value)
    }
}

fn text(value: &Value) -> String {
    if let Some(job) = value.get("job") {
        return format!("Recording in the background: {}\n", jobs::line(job));
    }
    format!(
        "Recorded {} s to {}{} ({}).\n",
        value["duration_s"],
        value["output"].as_str().unwrap_or_default(),
        value["bytes"]
            .as_u64()
            .map(|bytes| format!(", {}", crate::types::human_bytes(bytes)))
            .unwrap_or_default(),
        value["ended"].as_str().unwrap_or_default()
    )
}

/// Result fields, for `schema`.
pub const FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    (
        "output",
        "string — the file written (template expanded), or the stream URL with credentials redacted",
    ),
    ("bytes", "int — the file's size"),
    ("duration_s", "float"),
    ("frames", "int — frames encoded"),
    ("dropped", "int — frames the encoder could not take"),
    ("width", "int"),
    ("height", "int"),
    ("fps_actual", "float — frames per second encoded"),
    (
        "fps",
        "float — the frame rate written: --fps, or the camera's",
    ),
    ("codec", "h264|hevc|av1|ffv1"),
    ("encoder", "string — the FFmpeg encoder used"),
    (
        "ended",
        "duration|signal|max_size|stopped — signal is a normal end without --for; stopped: jobs stop or session stop ended it",
    ),
    ("finalized", "bool — the file was closed and is complete"),
    (
        "job",
        concat!(
            jobs::job_row!(),
            " — with --background: the job, as jobs lists it, once its first frame is encoded"
        ),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_camera_rate_is_the_default() {
        assert_eq!(camera_fps(&json!({"fps": 12.5})), 12.5);
        assert_eq!(camera_fps(&json!({"fps_limit": 20})), 20.0);
        assert_eq!(
            camera_fps(&json!({"controls": {"fps": {"value": "15"}}})),
            15.0
        );
        assert_eq!(camera_fps(&json!({})), 30.0);
        assert_eq!(camera_fps(&json!({"fps": 0.0})), 30.0);
    }
}
