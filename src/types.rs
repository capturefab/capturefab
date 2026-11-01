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
    pub timestamp_ns: u64,
    #[serde(skip)]
    pub data: Vec<u8>,
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
    /// Re-evaluate transport settings while acquisition is stopped.
    fn optimize_network(&mut self, _payload_size: usize) -> Result<crate::network::NetworkTuning> {
        anyhow::bail!("network tuning is only available for GigE Vision cameras")
    }
    fn stats(&self) -> Option<TransportStats> {
        None
    }
}

pub const MONO8: u32 = 0x0108_0001;
pub const RGB8: u32 = 0x0218_0014;

/// How a failure is reported to scripts. Carried as a type, so a name inside a
/// message (a camera called `porch_disabled`) cannot change the exit code that
/// `cli::error_code` would otherwise infer from the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    OperationFailed,
    Unavailable,
    Timeout,
    Unsupported,
    /// A camera asked for a user name and password and none was sent.
    AuthRequired,
    /// A camera refused the user name and password that were sent.
    AuthRejected,
}
impl ErrorKind {
    /// The JSON error code and process exit code.
    pub fn code(self) -> (&'static str, i32) {
        match self {
            Self::OperationFailed => ("operation_failed", 1),
            Self::Unavailable => ("unavailable", 3),
            Self::Timeout => ("timeout", 4),
            Self::Unsupported => ("unsupported", 5),
            Self::AuthRequired => ("auth_required", 1),
            Self::AuthRejected => ("auth_rejected", 1),
        }
    }
    fn from_code(code: &str) -> Option<Self> {
        [
            Self::OperationFailed,
            Self::Unavailable,
            Self::Timeout,
            Self::Unsupported,
            Self::AuthRequired,
            Self::AuthRejected,
        ]
        .into_iter()
        .find(|kind| kind.code().0 == code)
    }
}

/// An error whose category was decided where it happened.
#[derive(Debug)]
pub struct Categorized {
    pub kind: ErrorKind,
    pub message: String,
}
impl Categorized {
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> anyhow::Error {
        Self {
            kind,
            message: message.into(),
        }
        .into()
    }
}
impl std::fmt::Display for Categorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Categorized {}

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

/// Overrides the processing thread limit, for example `CAPTUREFAB_THREADS=1`
/// on a computer that also runs inference.
pub const THREADS_ENV: &str = "CAPTUREFAB_THREADS";

/// Threads for CPU-bound processing: `wanted`, at most the computer's cores
/// (up to eight), or at most `CAPTUREFAB_THREADS` when that is set.
pub fn processing_threads(wanted: usize) -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        thread_limit(
            std::env::var(THREADS_ENV).ok().as_deref(),
            std::thread::available_parallelism().map_or(1, |cores| cores.get()),
        )
    });
    wanted.clamp(1, limit)
}

fn thread_limit(setting: Option<&str>, cores: usize) -> usize {
    setting
        .and_then(|setting| setting.trim().parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .map_or(cores.clamp(1, 8), |threads| threads.min(256))
}

/// The category to send with an error that crosses a process or socket, so
/// the receiving side reports it the same way: `usage` or an [`ErrorKind`]
/// code.
pub fn error_transport_code(error: &anyhow::Error) -> Option<&'static str> {
    error.chain().find_map(|cause| {
        if cause.is::<Usage>() {
            Some("usage")
        } else {
            cause
                .downcast_ref::<Categorized>()
                .map(|categorized| categorized.kind.code().0)
        }
    })
}

/// Rebuild an error received with [`error_transport_code`].
pub fn error_from_transport(code: Option<&str>, message: String) -> anyhow::Error {
    match code {
        Some("usage") => Usage(message).into(),
        Some(code) => match ErrorKind::from_code(code) {
            Some(kind) => Categorized::error(kind, message),
            None => anyhow::Error::msg(message),
        },
        None => anyhow::Error::msg(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_categories_survive_transport() {
        let usage: anyhow::Error = Usage("bad option".into()).into();
        let timeout = Categorized::error(ErrorKind::Timeout, "camera timed out");
        let wrapped = anyhow::Error::from(Categorized {
            kind: ErrorKind::Unavailable,
            message: "camera porch_disabled is offline".into(),
        })
        .context("connect");
        let required = Categorized::error(ErrorKind::AuthRequired, "rtsp://cam needs a sign-in");
        let rejected = Categorized::error(ErrorKind::AuthRejected, "rtsp://cam refused it")
            .context("open camera");
        for (error, code) in [
            (required, Some("auth_required")),
            (rejected, Some("auth_rejected")),
            (usage, Some("usage")),
            (timeout, Some("timeout")),
            (wrapped, Some("unavailable")),
            (anyhow::anyhow!("plain"), None),
        ] {
            assert_eq!(error_transport_code(&error), code);
            let rebuilt = error_from_transport(code, format!("{error:#}"));
            assert_eq!(error_transport_code(&rebuilt), code);
            assert_eq!(format!("{rebuilt:#}"), format!("{error:#}"));
        }
        assert!(error_transport_code(&error_from_transport(Some("bogus"), "x".into())).is_none());
    }

    #[test]
    fn thread_limits_follow_the_cores_or_the_setting() {
        assert_eq!(thread_limit(None, 1), 1);
        assert_eq!(thread_limit(None, 6), 6);
        assert_eq!(thread_limit(None, 64), 8);
        assert_eq!(thread_limit(Some("2"), 64), 2);
        assert_eq!(thread_limit(Some(" 12 "), 4), 12);
        for ignored in ["0", "-1", "many", ""] {
            assert_eq!(thread_limit(Some(ignored), 4), 4, "{ignored}");
        }
        assert_eq!(processing_threads(0), 1);
        assert!(processing_threads(usize::MAX) <= 256);
    }
}
