use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    GigE,
    Usb3,
    Simulator,
    Media,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::GigE => "GigE",
            Self::Usb3 => "USB3",
            Self::Simulator => "Simulator",
            Self::Media => "Media",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraInfo {
    pub id: String,
    pub transport: Transport,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub address: Option<String>,
    pub profile: crate::camera_profile::CameraProfile,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frame {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    /// The camera's own clock when it took the frame, in nanoseconds, from a
    /// GigE Vision or USB3 Vision leader (or the simulator's clock). It is
    /// comparable only between frames of one camera, or of cameras that
    /// share a PTP clock, and is never a date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_ts_ns: Option<u64>,
    /// When the host received it, UTC Unix milliseconds; set by
    /// `Camera::next_frame`.
    #[serde(default)]
    pub wall_ms: u64,
    /// The pixels, shared by every clone of the frame.
    #[serde(skip)]
    pub data: crate::framebuf::FrameBuf,
    /// The JPEG the source sent the frame as (an MJPEG webcam, stream or
    /// file), whose decoding `data` is: a JPEG still of the frame is this,
    /// rather than a new encoding. A frame made from another never has it.
    #[serde(skip)]
    pub jpeg: Option<crate::framebuf::FrameBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TransportStats {
    pub packet_size: Option<u32>,
    pub receive_buffer_bytes: Option<u64>,
    #[serde(default)]
    pub packets_received: u64,
    #[serde(default)]
    pub resend_requested: u64,
    #[serde(default)]
    pub resend_recovered: u64,
    #[serde(default)]
    pub incomplete_frames: u64,
    #[serde(default)]
    pub lost_frames: u64,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Reads/writes bytes in the camera's address space. GenApi nodes determine byte order.
pub trait RegisterIo: Send {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>>;
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()>;
}

/// Owns transport resources; called exclusively by the camera worker thread.
pub trait Backend: RegisterIo {
    fn xml(&mut self) -> Result<String>;
    /// Forget the XML `xml` read from the disk cache, which did not parse
    /// or resolve, so the next call reads it from the device. Whether there
    /// was one.
    fn forget_xml(&mut self) -> bool {
        false
    }
    /// Configure receiving before the owner executes AcquisitionStart.
    fn start(&mut self, payload_size: usize) -> Result<()>;
    /// Request one frame from an on-demand source. Streaming transports need
    /// no explicit trigger; a paced video file starts again unpaced.
    /// Repeated requests while an acquisition is pending must be idempotent.
    fn request_frame(&mut self) -> Result<()> {
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame>;
    fn stop(&mut self) -> Result<()>;
    /// The compressed video the source sends, tapped as it arrives so a
    /// recording in `codec` can copy it instead of encoding its frames:
    /// when the source has video `wanted` takes. Tapping may reopen the
    /// source.
    fn copy_stream(
        &mut self,
        _wanted: crate::media::StreamCopy,
        _codec: &str,
    ) -> Result<Option<std::sync::Arc<crate::media::copy::Tap>>> {
        Ok(None)
    }
    /// Re-evaluate transport settings while acquisition is stopped.
    fn optimize_network(&mut self, _payload_size: usize) -> Result<crate::network::NetworkTuning> {
        anyhow::bail!("network tuning is only available for GigE Vision cameras")
    }
    fn stats(&self) -> Option<TransportStats> {
        None
    }
    /// Take in what already arrived, keeping the frames for `next_frame`,
    /// and say how many frames it holds that `next_frame` has not returned,
    /// whole or in part: frames that left the camera before now.
    fn buffered_frames(&mut self) -> u32 {
        0
    }
    /// The camera's clock now, as `Frame::device_ts_ns` reads it, where the
    /// transport can latch it.
    fn device_time_ns(&mut self) -> Option<u64> {
        None
    }
    /// Whether, started for a capture or an inspection, it should go on
    /// streaming a moment after: a live FFmpeg source, slow to start, whose
    /// next frame is the newest either way.
    fn lingers(&self) -> bool {
        false
    }
    /// Whether its frames can wait for a recording's encoder, as a video
    /// file's do: nothing goes by while the next one is not taken.
    fn frames_wait(&self) -> bool {
        false
    }
    /// Let go of what it opened to connect and still holds while it does
    /// not stream: FFmpeg, which read a source's first frame and went on
    /// decoding. The next start opens it again.
    fn rest(&mut self) {}
    /// The settings this process keeps for a camera that has nowhere else
    /// to keep them, as a simulator's registers or a webcam's video mode:
    /// a real camera keeps its settings until it is powered off. None for a
    /// camera that keeps its own.
    fn emulated_settings(&mut self) -> Option<serde_json::Value> {
        None
    }
    /// Take back settings `emulated_settings` gave in another process.
    fn restore_settings(&mut self, _settings: &serde_json::Value) -> Result<()> {
        Ok(())
    }
}

pub const MONO8: u32 = 0x0108_0001;
pub const RGB8: u32 = 0x0218_0014;

/// Why a command failed: a stable reason code scripts and agents match on,
/// and its category, the process exit code. Carried as a type from where
/// the failure happened, so a name inside a message (a camera called
/// `porch_disabled`) can never change the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    OperationFailed,
    Usage,
    InvalidDuration,
    FeatureNotFound,
    OutOfRange,
    InvalidChoice,
    NotWritable,
    FeatureLocked,
    CameraAmbiguous,
    CameraNotFound,
    CameraBusy,
    PermissionDenied,
    Disconnected,
    WorkerCrashed,
    SessionNotFound,
    FfmpegMissing,
    Unavailable,
    /// A camera asked for a user name and password and none was sent.
    AuthRequired,
    /// A camera refused the user name and password that were sent.
    AuthRejected,
    FrameTimeout,
    TriggerTimeout,
    Timeout,
    Unsupported,
    Exists,
    StorageFull,
    Incomplete,
    Interrupted,
}
impl ErrorKind {
    pub const ALL: [Self; 27] = [
        Self::OperationFailed,
        Self::Usage,
        Self::InvalidDuration,
        Self::FeatureNotFound,
        Self::OutOfRange,
        Self::InvalidChoice,
        Self::NotWritable,
        Self::FeatureLocked,
        Self::CameraAmbiguous,
        Self::CameraNotFound,
        Self::CameraBusy,
        Self::PermissionDenied,
        Self::Disconnected,
        Self::WorkerCrashed,
        Self::SessionNotFound,
        Self::FfmpegMissing,
        Self::Unavailable,
        Self::AuthRequired,
        Self::AuthRejected,
        Self::FrameTimeout,
        Self::TriggerTimeout,
        Self::Timeout,
        Self::Unsupported,
        Self::Exists,
        Self::StorageFull,
        Self::Incomplete,
        Self::Interrupted,
    ];
    /// The JSON error code and process exit code.
    pub fn code(self) -> (&'static str, i32) {
        let (code, exit, _) = self.describe();
        (code, exit)
    }
    /// What the reason means, for `schema`.
    pub fn meaning(self) -> &'static str {
        self.describe().2
    }
    fn describe(self) -> (&'static str, i32, &'static str) {
        match self {
            Self::OperationFailed => ("failed", 1, "the operation failed"),
            Self::Usage => ("usage", 2, "invalid arguments or input"),
            Self::InvalidDuration => (
                "invalid_duration",
                2,
                "not a valid duration (see grammars.duration)",
            ),
            Self::FeatureNotFound => (
                "feature_not_found",
                2,
                "no such feature or control; see details.did_you_mean",
            ),
            Self::OutOfRange => (
                "out_of_range",
                2,
                "outside the feature's range; details {feature,value,min,max,inc,nearest,unit}",
            ),
            Self::InvalidChoice => ("invalid_choice", 2, "not one of details.choices"),
            Self::NotWritable => ("not_writable", 2, "the feature is read-only"),
            Self::FeatureLocked => (
                "feature_locked",
                1,
                "another setting locks the feature now; see details.locked_by",
            ),
            Self::CameraAmbiguous => (
                "camera_ambiguous",
                2,
                "several cameras match; pick one of details.candidates with -c",
            ),
            Self::CameraNotFound => (
                "camera_not_found",
                3,
                "no camera matches; details.candidates lists what answered",
            ),
            Self::CameraBusy => (
                "camera_busy",
                3,
                "another process or job holds the camera; details.held_by when known",
            ),
            Self::PermissionDenied => ("permission_denied", 3, "the OS refused access; see hint"),
            Self::Disconnected => ("disconnected", 3, "the camera went away"),
            Self::WorkerCrashed => ("worker_crashed", 3, "the camera worker process died"),
            Self::SessionNotFound => ("session_not_found", 3, "no running session has that name"),
            Self::FfmpegMissing => ("ffmpeg_missing", 3, "FFmpeg could not be found or started"),
            Self::Unavailable => (
                "unavailable",
                3,
                "the camera, stream or service cannot be reached",
            ),
            Self::AuthRequired => (
                "auth_required",
                3,
                "the camera needs a sign-in; run login add",
            ),
            Self::AuthRejected => ("auth_rejected", 3, "the camera refused the sign-in"),
            Self::FrameTimeout => ("frame_timeout", 4, "no frame arrived in time"),
            Self::TriggerTimeout => (
                "trigger_timeout",
                4,
                "a triggered camera got no trigger in time; details.trigger",
            ),
            Self::Timeout => ("timeout", 4, "the operation did not finish in time"),
            Self::Unsupported => ("unsupported", 5, "the camera or this build cannot do that"),
            Self::Exists => (
                "exists",
                1,
                "the output exists (never overwritten); details.path",
            ),
            Self::StorageFull => (
                "storage_full",
                6,
                "disk full or below CAPTUREFAB_MIN_FREE; details {path,free_bytes?}",
            ),
            Self::Incomplete => ("incomplete", 1, "fewer results than requested; see details"),
            Self::Interrupted => (
                "interrupted",
                130,
                "stopped by a signal; details hold partial results",
            ),
        }
    }
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code().0 == code)
    }
}

/// An error whose reason was decided where it happened, with an optional
/// hint (what to do next) and JSON details (bounds, choices, candidates).
#[derive(Debug)]
pub struct Categorized {
    pub kind: ErrorKind,
    pub message: String,
    pub hint: Option<String>,
    pub details: Option<serde_json::Value>,
}
impl Categorized {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hint: None,
            details: None,
        }
    }
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> anyhow::Error {
        Self::new(kind, message).into()
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    /// JSON details; null values are dropped, since results never carry them.
    pub fn details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(without_nulls(details));
        self
    }
}
impl std::fmt::Display for Categorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Categorized {}

/// `error` with `extra` merged into its details, keeping its reason and
/// hint: partial work of a failed command goes there.
pub fn with_details(error: anyhow::Error, extra: serde_json::Value) -> anyhow::Error {
    let classified = classify(&error);
    let mut details = match classified.details {
        Some(serde_json::Value::Object(details)) => serde_json::Value::Object(details),
        Some(other) => serde_json::json!({ "cause": other }),
        None => serde_json::json!({}),
    };
    if let (Some(details), serde_json::Value::Object(extra)) = (details.as_object_mut(), extra) {
        details.extend(extra);
    }
    let mut categorized = Categorized::new(classified.kind, format!("{error:#}")).details(details);
    categorized.hint = classified.hint;
    categorized.into()
}

/// `value` without null object members, at any depth.
pub fn without_nulls(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, without_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(without_nulls).collect()),
        value => value,
    }
}

/// No frame arrived within the requested wait, but the source is still
/// running: the engine keeps polling instead of counting a transport error.
#[derive(Debug)]
pub struct FrameWait;
impl std::fmt::Display for FrameWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("frame timeout")
    }
}
impl std::error::Error for FrameWait {}

/// A request the caller must change (exit code 2), raised while it runs.
#[derive(Debug)]
pub struct Usage(pub String);
impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Usage {}

/// Parse a size: 10GiB/10G use binary units; 10GB uses decimal units.
pub fn parse_bytes(value: &str) -> anyhow::Result<u64> {
    use anyhow::Context as _;
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..split]
        .parse()
        .context("a size must start with an integer")?;
    let multiplier: u64 = match value[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "ki" | "kib" => 1024,
        "m" | "mi" | "mib" => 1024u64.pow(2),
        "g" | "gi" | "gib" => 1024u64.pow(3),
        "t" | "ti" | "tib" => 1024u64.pow(4),
        "kb" => 1000,
        "mb" => 1000u64.pow(2),
        "gb" => 1000u64.pow(3),
        "tb" => 1000u64.pow(4),
        _ => anyhow::bail!("unknown size unit; use B, KiB, MiB, GiB, TiB, KB, MB, GB or TB"),
    };
    let bytes = number
        .checked_mul(multiplier)
        .context("the size is too large")?;
    anyhow::ensure!(bytes > 0, "a size must be positive");
    Ok(bytes)
}

/// `bytes` in lowercase hex.
pub fn hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let bytes = bytes.as_ref();
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// `count` random bytes in lowercase hex: a name no other file has, a
/// nonce or a secret.
pub fn random_hex(count: usize) -> Result<String> {
    let mut random = vec![0u8; count];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("random bytes: {error}"))?;
    Ok(hex(random))
}

/// A byte count for diagnostics, with two decimals in GiB and one below.
pub fn human_bytes(value: u64) -> String {
    if value >= 1_073_741_824 {
        format!("{:.2} GiB", value as f64 / 1_073_741_824.0)
    } else if value >= 1_048_576 {
        format!("{:.1} MiB", value as f64 / 1_048_576.0)
    } else {
        format!("{:.1} KiB", value as f64 / 1024.0)
    }
}

/// What an error's chain says about it: its reason and the hint and
/// details to report. Typed causes only, never message text; anything
/// unknown is `failed`.
pub struct Classified {
    pub kind: ErrorKind,
    pub hint: Option<String>,
    pub details: Option<serde_json::Value>,
}
pub fn classify(error: &anyhow::Error) -> Classified {
    let plain = |kind| Classified {
        kind,
        hint: None,
        details: None,
    };
    // An FFmpeg that could not be started is ffmpeg_missing with the
    // install hint, unless the error says more, as a bundled copy with
    // nowhere to run does.
    if error
        .downcast_ref::<crate::media::FfmpegUnavailable>()
        .is_some()
    {
        let categorized = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<Categorized>());
        return match categorized {
            Some(categorized) => Classified {
                kind: categorized.kind,
                hint: categorized.hint.clone(),
                details: categorized.details.clone(),
            },
            None => Classified {
                kind: ErrorKind::FfmpegMissing,
                hint: Some(crate::media::ffmpeg_hint()),
                details: None,
            },
        };
    }
    for cause in error.chain() {
        if let Some(categorized) = cause.downcast_ref::<Categorized>() {
            return Classified {
                kind: categorized.kind,
                hint: categorized.hint.clone(),
                details: categorized.details.clone(),
            };
        }
        if cause.is::<Usage>() {
            return plain(ErrorKind::Usage);
        }
        if let Some(exists) = cause.downcast_ref::<crate::storage::OutputExists>() {
            return Classified {
                kind: ErrorKind::Exists,
                hint: None,
                details: Some(serde_json::json!({"path": exists.0})),
            };
        }
        if cause.is::<FrameWait>() {
            return plain(ErrorKind::FrameTimeout);
        }
        if cause.is::<crate::transport::gige::GvcpTimeout>() {
            return plain(ErrorKind::Timeout);
        }
        // A session that went away mid-request is unavailable, as one that
        // cannot be reached is.
        if cause.is::<crate::ipc::Closed>() {
            return Classified {
                kind: ErrorKind::Unavailable,
                hint: Some(crate::ipc::CLOSED_HINT.into()),
                details: None,
            };
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind as Io;
            match io.kind() {
                Io::StorageFull | Io::QuotaExceeded => {
                    return Classified {
                        kind: ErrorKind::StorageFull,
                        hint: Some(crate::session::with_environment_hint(
                            format!(
                                "free some space or write to another folder; {} sets how much Capturefab leaves free",
                                crate::storage::MIN_FREE_ENV
                            ),
                            crate::storage::MIN_FREE_ENV,
                        )),
                        details: None,
                    };
                }
                Io::TimedOut => return plain(ErrorKind::Timeout),
                Io::PermissionDenied => return plain(ErrorKind::PermissionDenied),
                Io::ConnectionRefused
                | Io::HostUnreachable
                | Io::NetworkUnreachable
                | Io::NetworkDown
                | Io::AddrNotAvailable => return plain(ErrorKind::Unavailable),
                _ => {}
            }
        }
    }
    match crate::media::stream_end(error) {
        Some(crate::media::StreamEnd::Lost) => plain(ErrorKind::Disconnected),
        Some(crate::media::StreamEnd::Abandoned) => plain(ErrorKind::AuthRejected),
        _ => plain(ErrorKind::OperationFailed),
    }
}

/// The reason, hint and details sent beside an error's message when it
/// crosses a process or socket, so the receiving side reports it the same
/// way.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ErrorTag {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}
/// The tag for `error`; none for a plain failure.
pub fn error_tag(error: &anyhow::Error) -> Option<ErrorTag> {
    let classified = classify(error);
    (classified.kind != ErrorKind::OperationFailed
        || classified.hint.is_some()
        || classified.details.is_some())
    .then(|| ErrorTag {
        code: classified.kind.code().0.into(),
        hint: classified.hint,
        details: classified.details,
    })
}
/// The reason code an error would cross a process with.
pub fn error_transport_code(error: &anyhow::Error) -> Option<&'static str> {
    let kind = classify(error).kind;
    (kind != ErrorKind::OperationFailed).then(|| kind.code().0)
}
/// Rebuild an error received with [`error_tag`].
pub fn error_from_tag(tag: Option<ErrorTag>, message: String) -> anyhow::Error {
    let Some(tag) = tag else {
        return anyhow::Error::msg(message);
    };
    match ErrorKind::from_code(&tag.code) {
        Some(ErrorKind::Usage) if tag.hint.is_none() && tag.details.is_none() => {
            Usage(message).into()
        }
        Some(kind) => Categorized {
            kind,
            message,
            hint: tag.hint,
            details: tag.details,
        }
        .into(),
        None => anyhow::Error::msg(message),
    }
}

/// Most a duration may be: ten years.
const MAX_DURATION: Duration = Duration::from_secs(315_576_000);

/// Parse a duration with the one grammar every command uses: one or more
/// number-unit pairs (`250ms`, `2s`, `1.5s`, `5m`, `1h30m`, `500us`), or
/// `0`. A bare number is refused: `5000` could mean milliseconds or
/// seconds.
pub fn parse_duration(text: &str) -> anyhow::Result<Duration> {
    let text = text.trim();
    let invalid = |message: String| -> anyhow::Error {
        Categorized::new(ErrorKind::InvalidDuration, message)
            .hint("durations need a unit: 500us, 250ms, 2s, 1.5s, 5m, 1h30m")
            .into()
    };
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    if text.is_empty() {
        return Err(invalid("a duration cannot be empty".into()));
    }
    if !text.is_empty() && text.parse::<f64>().is_ok() {
        // Which unit was meant depends on the flag or feature: an exposure
        // in microseconds, a timeout in seconds. A guess would be wrong by
        // a factor of a thousand as often as not.
        return Err(invalid(format!(
            "'{text}' has no unit; add one of us, ms, s, m or h"
        )));
    }
    let mut rest = text;
    let mut nanos = 0f64;
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let (number, after) = rest.split_at(number_end);
        let unit_end = after
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(after.len());
        let (unit, after) = after.split_at(unit_end);
        let value = number
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && !number.is_empty())
            .ok_or_else(|| invalid(format!("'{text}' is not a duration")))?;
        let scale = match unit {
            "h" => 3.6e12,
            "m" => 6e10,
            "s" => 1e9,
            "ms" => 1e6,
            "us" | "µs" | "μs" => 1e3,
            "" => return Err(invalid(format!("'{text}' needs a unit after {number}"))),
            other => {
                return Err(invalid(format!(
                    "'{other}' in '{text}' is not a unit; use h, m, s, ms or us"
                )));
            }
        };
        nanos += value * scale;
        rest = after;
    }
    if !(0.0..=MAX_DURATION.as_nanos() as f64).contains(&nanos) {
        return Err(invalid(format!("'{text}' is longer than ten years")));
    }
    Ok(Duration::from_nanos(nanos.round() as u64))
}

/// A duration as [`parse_duration`] reads it back, exactly: `1h30m`, `2s`,
/// `1m30.25s`, `250ms`, `1.5ms`, `500us`, `0`. Every duration a message
/// shows is written this way, so it can be pasted into a flag.
pub fn duration_text(duration: Duration) -> String {
    /// `whole` and `part` billionths, without trailing zeros.
    fn decimal(whole: u128, part: u128) -> String {
        if part == 0 {
            return whole.to_string();
        }
        let digits = format!("{part:09}");
        format!("{whole}.{}", digits.trim_end_matches('0'))
    }
    let nanos = duration.as_nanos();
    if nanos == 0 {
        return "0".into();
    }
    if nanos < 1_000_000 {
        return format!("{}us", decimal(nanos / 1000, (nanos % 1000) * 1_000_000));
    }
    if nanos < 1_000_000_000 {
        return format!(
            "{}ms",
            decimal(nanos / 1_000_000, (nanos % 1_000_000) * 1000)
        );
    }
    let mut seconds = nanos / 1_000_000_000;
    let mut text = String::new();
    for (unit, size) in [("h", 3600), ("m", 60)] {
        if seconds >= size {
            text.push_str(&format!("{}{unit}", seconds / size));
            seconds %= size;
        }
    }
    let part = nanos % 1_000_000_000;
    if seconds > 0 || part > 0 {
        text.push_str(&format!("{}s", decimal(seconds, part)));
    }
    text
}

/// `error` as one message, `message: cause`, keeping its reason, hint and
/// details.
pub fn rephrase(error: anyhow::Error, message: impl std::fmt::Display) -> anyhow::Error {
    let text = format!("{message}: {error:#}");
    let classified = classify(&error);
    if classified.kind == ErrorKind::OperationFailed
        && classified.hint.is_none()
        && classified.details.is_none()
    {
        return anyhow::Error::msg(text);
    }
    Categorized {
        kind: classified.kind,
        message: text,
        hint: classified.hint,
        details: classified.details,
    }
    .into()
}

/// A frame wait that ended without a frame, as the reason agents match.
pub fn frame_timeout(message: impl Into<String>) -> anyhow::Error {
    Categorized::error(ErrorKind::FrameTimeout, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bytes_are_lowercase_hex_and_random_names_differ() {
        assert_eq!(hex([0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex([]), "");
        let (one, two) = (random_hex(6).unwrap(), random_hex(6).unwrap());
        assert!(one.len() == 12 && one.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(one, two);
    }
    #[test]
    fn error_categories_survive_transport() {
        let usage: anyhow::Error = Usage("bad option".into()).into();
        let timeout = Categorized::error(ErrorKind::Timeout, "camera timed out");
        let wrapped = anyhow::Error::from(Categorized::new(
            ErrorKind::Unavailable,
            "camera porch_disabled is offline",
        ))
        .context("connect");
        let required = Categorized::error(ErrorKind::AuthRequired, "rtsp://cam needs a sign-in");
        let rejected = Categorized::error(ErrorKind::AuthRejected, "rtsp://cam refused it")
            .context("open camera");
        let range = anyhow::Error::from(
            Categorized::new(ErrorKind::OutOfRange, "Width must be at most 1920")
                .hint("use 1920 or less")
                .details(serde_json::json!({"feature":"Width","max":1920,"unit":null})),
        )
        .context("set");
        for (error, code) in [
            (required, Some("auth_required")),
            (rejected, Some("auth_rejected")),
            (usage, Some("usage")),
            (timeout, Some("timeout")),
            (wrapped, Some("unavailable")),
            (range, Some("out_of_range")),
            (anyhow::anyhow!("plain"), None),
        ] {
            assert_eq!(error_transport_code(&error), code);
            let tag = error_tag(&error);
            // The tag survives JSON, as it does on the worker pipe and IPC.
            let tag: Option<ErrorTag> =
                serde_json::from_value(serde_json::to_value(&tag).unwrap()).unwrap();
            let rebuilt = error_from_tag(tag, format!("{error:#}"));
            assert_eq!(error_transport_code(&rebuilt), code);
            assert_eq!(error_tag(&rebuilt), error_tag(&error));
            assert_eq!(format!("{rebuilt:#}"), format!("{error:#}"));
        }
        let tag = |code: &str| {
            Some(ErrorTag {
                code: code.into(),
                ..Default::default()
            })
        };
        let range = error_tag(&error_from_tag(tag("out_of_range"), "x".into()));
        assert_eq!(range.unwrap().code, "out_of_range");
        assert!(error_transport_code(&error_from_tag(tag("bogus"), "x".into())).is_none());
    }

    #[test]
    fn durations_need_units() {
        let ms = Duration::from_millis;
        for (text, expected) in [
            ("250ms", ms(250)),
            ("2s", ms(2000)),
            ("1.5s", ms(1500)),
            ("5m", ms(300_000)),
            ("1h30m", ms(5_400_000)),
            ("500us", Duration::from_micros(500)),
            ("0", Duration::ZERO),
            (" 10ms ", ms(10)),
        ] {
            assert_eq!(parse_duration(text).unwrap(), expected, "{text}");
        }
        for text in [
            "5000",
            "1.5",
            "",
            "ms",
            "5x",
            "-1s",
            "1s5",
            "11y",
            "100000000h",
        ] {
            let error = parse_duration(text).unwrap_err();
            assert_eq!(classify(&error).kind, ErrorKind::InvalidDuration, "{text}");
        }
        // A bare number gets no guessed unit: for an exposure in
        // microseconds, 5000ms or 5000s would both be wrong.
        let bare = parse_duration("5000").unwrap_err();
        assert!(
            bare.to_string()
                .contains("'5000' has no unit; add one of us, ms, s, m or h"),
            "{bare}"
        );
        assert!(classify(&bare).hint.unwrap().contains("500us"));
    }

    #[test]
    fn durations_are_written_as_flags_read_them() {
        let ns = Duration::from_nanos;
        for (duration, text) in [
            (Duration::ZERO, "0"),
            (Duration::from_micros(500), "500us"),
            (ns(1500), "1.5us"),
            (Duration::from_micros(1500), "1.5ms"),
            (Duration::from_millis(250), "250ms"),
            (Duration::from_secs(2), "2s"),
            (Duration::from_millis(3971), "3.971s"),
            (Duration::from_millis(90_250), "1m30.25s"),
            (Duration::from_secs(5400), "1h30m"),
            (Duration::from_secs(3605), "1h5s"),
            (Duration::from_secs(86_401), "24h1s"),
        ] {
            assert_eq!(duration_text(duration), text, "{duration:?}");
            assert_eq!(parse_duration(text).unwrap(), duration, "{text}");
        }
        // Whatever it writes reads back as it was, to the nanosecond.
        for nanos in [1u64, 999, 1_000_001, 59_999_999_999, 3_600_000_000_001] {
            let duration = ns(nanos);
            assert_eq!(parse_duration(&duration_text(duration)).unwrap(), duration);
        }
    }

    #[test]
    fn typed_causes_pick_the_reason_and_nulls_never_reach_details() {
        let exists =
            anyhow::Error::from(crate::storage::OutputExists("/tmp/x.png".into())).context("save");
        let classified = classify(&exists);
        assert_eq!(classified.kind, ErrorKind::Exists);
        assert_eq!(classified.details.unwrap()["path"], "/tmp/x.png");
        let full = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::StorageFull));
        let classified = classify(&full);
        assert_eq!(classified.kind.code(), ("storage_full", 6));
        assert!(classified.hint.unwrap().contains("CAPTUREFAB_MIN_FREE"));
        assert_eq!(
            classify(&anyhow::Error::from(FrameWait)).kind,
            ErrorKind::FrameTimeout
        );
        // English that sounds like a timeout is still a plain failure.
        assert_eq!(
            classify(&anyhow::anyhow!("camera timed_out not found")).kind,
            ErrorKind::OperationFailed
        );
        let details = Categorized::new(ErrorKind::OutOfRange, "x")
            .details(serde_json::json!({"a":null,"b":[{"c":null,"d":1}]}))
            .details
            .unwrap();
        assert_eq!(details, serde_json::json!({"b":[{"d":1}]}));
        for kind in ErrorKind::ALL {
            assert_eq!(ErrorKind::from_code(kind.code().0), Some(kind));
        }
    }

    #[test]
    fn sizes_take_binary_and_decimal_units() {
        assert_eq!(parse_bytes("10GiB").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_bytes("10G").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_bytes("10GB").unwrap(), 10_000_000_000);
        assert_eq!(parse_bytes(" 5 KB ").unwrap(), 5000);
        assert_eq!(parse_bytes("512").unwrap(), 512);
        for invalid in ["0", "-1", "1.5GB", "10words", "18446744073709551615TB", ""] {
            assert!(parse_bytes(invalid).is_err(), "{invalid}");
        }
    }
}
