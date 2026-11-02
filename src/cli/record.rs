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
    media::StreamCopy,
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
    /// Frames per second written (default: the camera's frame rate); re-encodes a source that would be copied
    #[arg(long, value_name = "N")]
    pub fps: Option<f64>,
    /// h264, hevc or av1 (where a hardware encoder has them), or ffv1 (lossless, .mkv). Default: an H.264 or HEVC stream, webcam or file is copied as it arrives, without re-encoding; anything else is encoded as h264
    #[arg(long, value_enum)]
    pub codec: Option<Codec>,
    /// Target bitrate, such as 8M or 800k (default 8M; not for ffv1); re-encodes a source that would be copied
    #[arg(long, value_name = "RATE")]
    pub bitrate: Option<String>,
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
    /// How long a camera that sets its own exposure (webcam, stream, ExposureAuto=Continuous) may take to settle before recording; 0 records at once
    #[arg(long, default_value = "2s", value_name = "DUR", value_parser = super::args::duration)]
    pub warmup: Duration,
    /// Leave the recording running in a session and print its job once the first frame is encoded
    #[arg(long)]
    pub background: bool,
    #[command(flatten)]
    pub progress: super::jobs::ProgressArg,
    #[command(flatten)]
    pub keep: KeepArg,
}

fn parse_size(text: &str) -> std::result::Result<u64, String> {
    crate::types::parse_bytes(text).map_err(|error| error.to_string())
}

impl RecordArgs {
    /// The codec a recording that encodes is in.
    fn codec(&self) -> Codec {
        self.codec.unwrap_or(Codec::H264)
    }
    fn bitrate(&self) -> String {
        self.bitrate.clone().unwrap_or_else(|| "8M".into())
    }
    /// Whether the source's own video may be recorded as it is: not when
    /// the frames are asked for at another rate or bitrate, nor as FFV1;
    /// in the codec asked for, or any a recording copies.
    fn copy(&self) -> StreamCopy {
        match self.codec {
            _ if self.fps.is_some() || self.bitrate.is_some() => StreamCopy::Never,
            Some(Codec::Ffv1) => StreamCopy::Never,
            Some(_) => StreamCopy::Same,
            None => StreamCopy::Any,
        }
    }
}

/// A local file's path, absolute; a stream URL as it is.
fn output(args: &RecordArgs) -> Result<String> {
    let mut output = args.output.clone();
    crate::naming::validate_output(&output).map_err(super::input)?;
    if !crate::media::is_recording(&output) {
        if args.codec == Some(Codec::Ffv1) {
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
    if args.codec == Some(Codec::Ffv1) && extension.as_deref() != Some("mkv") {
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
    let max_file_bytes = args.max_size;
    let min_free = crate::storage::min_free()?;
    let request = |fps: f64| SessionCommand::Forward {
        output: output.clone(),
        codec: args.codec().name().into(),
        encoder: "auto".into(),
        fps,
        bitrate: args.bitrate(),
        max_file_bytes,
        min_free,
        on_exists: Default::default(),
        context: Default::default(),
        duration_ms: args.duration.map(millis),
        utc_offset_s: Some(crate::naming::local_offset()),
        warmup_ms: Some(millis(args.warmup)),
        job: None,
        // A recording this command waits for ends when it goes away.
        lease: !args.background,
        copy: args.copy(),
    };
    // Everything that can be checked here is, before a camera is opened:
    // the request, a taken file, FFmpeg and an encoder for the codec. A
    // source that may be copied needs no encoder: the worker checks for one
    // once it is not.
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
    if args.copy() == StreamCopy::Never {
        crate::media::check_codec(&crate::media::ForwardConfig {
            output: output.clone(),
            codec: args.codec().name().into(),
            encoder: "auto".into(),
            fps: args.fps.unwrap_or(30.0),
            bitrate: args.bitrate(),
            max_file_bytes,
            min_free,
        })?;
    }
    signals::take_over();
    let client = Client::open_with(
        global,
        args.camera.camera.as_deref(),
        Route {
            background: args.background,
            ..args.keep.route()
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
        super::auto::settle(&client, Duration::from_secs(5), &Default::default())?;
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
    let recording = Recording {
        client: &client,
        job: reply["job"]["id"]
            .as_str()
            .context("recording without a job")?
            .to_owned(),
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
    /// Its job, whose events say how it went and ended.
    job: String,
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
    /// It failed, after recording what its error's details show.
    Failed,
}
impl Ended {
    fn name(self) -> &'static str {
        match self {
            Self::Duration => "duration",
            Self::Signal => "signal",
            Self::MaxSize => "max_size",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// How long a stopped recording may take to finish its file.
const FINALIZE: Duration = Duration::from_secs(15);

impl Recording<'_> {
    /// The job once it changed after `seq` or ended: its new sequence
    /// number, document and its worker's account.
    fn next(&self, seq: u64, wait: Duration) -> Result<(u64, Value, Value)> {
        let reply = self
            .client
            .wait_job(&self.job, seq, wait.max(Duration::from_millis(1)))?;
        Ok((
            reply["seq"].as_u64().unwrap_or(seq),
            reply["job"].clone(),
            reply["detail"].clone(),
        ))
    }
    /// The first frame's wait.
    fn first_frame_wait(&self) -> Option<Duration> {
        match self.args.timeout {
            Wait::For(wait) => Some(wait),
            Wait::Forever => None,
        }
    }
    /// Stop the recording and wait for its end: its job and account.
    fn stop(&self) -> Result<(Value, Value)> {
        let reply = self.client.host().stop(&self.job)?;
        Ok((reply["job"].clone(), reply["detail"].clone()))
    }
    /// How long until the first frame's wait is over.
    fn first_frame_left(&self) -> Duration {
        self.first_frame_wait()
            .map_or(Duration::from_secs(25), |wait| {
                wait.saturating_sub(self.started.elapsed())
            })
    }
    /// Wait for the first frame to be encoded, then leave the recording
    /// running: its job. A recording that could not start fails.
    fn armed(&self) -> Result<Value> {
        let mut seq = 0;
        loop {
            let (now, job, detail) = self.next(seq, self.first_frame_left())?;
            seq = now;
            if super::jobs::ended(&job) {
                if job["state"] == "failed" {
                    return Err(self.failure(&job, &detail));
                }
                return Ok(warmed(json!({ "job": job }), &detail));
            }
            if job["state"] != "armed" {
                return Ok(warmed(json!({ "job": job }), &detail));
            }
            if self.first_frame_wait().is_some() && self.first_frame_left().is_zero() {
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
        }
    }
    fn no_frames(&self) -> anyhow::Error {
        crate::types::frame_timeout(format!(
            "no frame reached the encoder within {}",
            super::args::duration_text(self.first_frame_wait().unwrap_or_default())
        ))
    }
    /// The recording's error, as its job ended with it, with its reason,
    /// and what it recorded before, as a disk below its floor leaves, or a
    /// worker that died leaves unfinished.
    fn failure(&self, job: &Value, detail: &Value) -> anyhow::Error {
        let mut details = json!({ "job": job });
        if (detail["recording"].is_object() || job["last_file"].is_string())
            && let (Some(details), Value::Object(result)) = (
                details.as_object_mut(),
                self.result(job, detail, Ended::Failed),
            )
        {
            details.extend(result);
        }
        crate::types::with_details(super::capture::job_error(job, detail), details)
    }
    /// Wait until the recording ends: its result. A signal stops it, and
    /// the file it finished is the result.
    fn wait(&self) -> Result<Value> {
        let duration = self.args.duration;
        self.client.stop_on_signal(&self.job);
        let mut shown =
            super::jobs::Progress::new(format!("recording {}…", self.output), self.args.progress);
        let mut seq = 0;
        let mut stopping: Option<Instant> = None;
        let mut armed = true;
        loop {
            let wait = match stopping {
                Some(since) => FINALIZE.saturating_sub(since.elapsed()),
                None if armed && self.first_frame_wait().is_some() => self.first_frame_left(),
                None => Duration::from_secs(25),
            };
            let wait = shown.step(wait.min(Duration::from_secs(25)));
            let (now, job, detail) = self.next(seq, wait)?;
            seq = now;
            if !armed || job["state"] != "armed" {
                shown.show(&job, self.started);
            }
            armed = job["state"] == "armed";
            let signal = signals::interrupted();
            if super::jobs::ended(&job) {
                let ended = match detail["ended"].as_str() {
                    _ if job["state"] == "failed" => return Err(self.failure(&job, &detail)),
                    Some("duration") => Ended::Duration,
                    Some("max_size") => Ended::MaxSize,
                    _ if signal => Ended::Signal,
                    _ => Ended::Stopped,
                };
                let value = self.result(&job, &detail, ended);
                // A recording asked to run for a time and cut short is
                // interrupted, by a signal, or incomplete; an open-ended
                // one ends like this.
                if let Some(duration) = duration
                    && matches!(ended, Ended::Signal | Ended::Stopped)
                {
                    let ran = crate::scheduling::format_duration(millis(self.started.elapsed()));
                    return Err(if ended == Ended::Signal {
                        Categorized::new(
                            ErrorKind::Interrupted,
                            format!("interrupted after {ran} of the recording"),
                        )
                    } else {
                        Categorized::new(
                            ErrorKind::Incomplete,
                            format!(
                                "the recording was stopped after {ran} of {}",
                                super::args::duration_text(duration)
                            ),
                        )
                    }
                    .details(value)
                    .into());
                }
                return Ok(value);
            }
            if signal {
                let since = *stopping.get_or_insert_with(Instant::now);
                if since.elapsed() >= FINALIZE {
                    return Err(Categorized::new(
                        ErrorKind::Interrupted,
                        "interrupted; the recording did not finish in time",
                    )
                    .details(json!({ "job": job }))
                    .into());
                }
                continue;
            }
            if armed && self.first_frame_wait().is_some() && self.first_frame_left().is_zero() {
                let _ = self.stop();
                return Err(self.no_frames());
            }
        }
    }
    /// The finished recording's result, from its job and its worker's
    /// account of it.
    fn result(&self, job: &Value, detail: &Value, ended: Ended) -> Value {
        let file = &detail["recording"];
        let duration_ms = match (file["started_ms"].as_u64(), file["ended_ms"].as_u64()) {
            (Some(start), Some(end)) => Some(end.saturating_sub(start)),
            _ => detail["ended_ms"]
                .as_u64()
                .zip(detail["since_ms"].as_u64())
                .map(|(end, start)| end.saturating_sub(start)),
        };
        let duration_s = duration_ms.map_or_else(
            || self.started.elapsed().as_secs_f64(),
            |ms| ms as f64 / 1000.0,
        );
        let frames = detail["written"].as_u64();
        let encoder = file["encoder"]
            .as_str()
            .or(detail["encoder"].as_str())
            .unwrap_or_default();
        let mut value = json!({
            "camera": self.info["camera"],
            "output": file["path"]
                .as_str()
                .or(job["last_file"].as_str())
                .map_or_else(|| self.output.clone(), str::to_owned),
            "bytes": file["bytes"].as_u64().or(job["bytes"].as_u64()),
            "duration_s": (duration_s * 1000.0).round() / 1000.0,
            "frames": frames,
            "dropped": detail["dropped"],
            "incomplete": detail["incomplete"],
            "resent": detail["resent"],
            "width": self.info["width"],
            "height": self.info["height"],
            "codec": file["codec"]
                .as_str()
                .or(detail["codec"].as_str())
                .unwrap_or(self.args.codec().name()),
            "encoder": encoder,
            "ended": ended.name(),
            "finalized": detail["finalized"].as_bool().unwrap_or(false),
        });
        if let Some(frames) = frames.filter(|_| duration_s > 0.0) {
            value["fps_actual"] = json!((frames as f64 / duration_s * 100.0).round() / 100.0);
        }
        value["fps"] = json!(self.fps);
        crate::types::without_nulls(warmed(value, detail))
    }
}

/// A result with the warm-up the recording waited for, from its worker's
/// account, and a warning when the camera had not settled.
fn warmed(mut value: Value, detail: &Value) -> Value {
    if let Some(ms) = detail["warmup_ms"].as_u64() {
        value["warmup_ms"] = json!(ms);
        if detail["settled"] == false {
            value["settled"] = json!(false);
            value["warnings"] = json!([crate::engine::unsettled_warning(ms)]);
        }
    }
    value
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
    (
        "frames",
        "int — frames encoded, or copied; for a stream, the camera's frames handed to the encoder, which repeats them to keep --fps",
    ),
    (
        "dropped",
        "int — frames the encoder could not take, and frames the transport lost",
    ),
    (
        "incomplete",
        "int — frames that arrived incomplete, where the transport counts them",
    ),
    (
        "resent",
        "int — packets the camera sent again, where the transport counts them",
    ),
    ("width", "int"),
    ("height", "int"),
    (
        "fps_actual",
        "float — frames per second, as frames counts them",
    ),
    (
        "fps",
        "float — the frame rate written: --fps, or the camera's",
    ),
    ("codec", "h264|hevc|av1|ffv1 — the source's own when copied"),
    (
        "encoder",
        "string — the FFmpeg encoder used; copy: an H.264 or HEVC source recorded as it arrived, without re-encoding",
    ),
    (
        "ended",
        "duration|signal|max_size|stopped — signal is a normal end without --for; stopped: jobs stop or session stop ended it",
    ),
    ("finalized", "bool — the file was closed and is complete"),
    (
        "warmup_ms",
        "int — how long a camera that sets its own exposure took to settle before recording",
    ),
    (
        "settled",
        "false — its exposure was still changing when --warmup ran out (with a warning)",
    ),
    ("warnings", "[string]"),
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
