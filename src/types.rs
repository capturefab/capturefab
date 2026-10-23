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
    Scanner,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::GigE => "GigE",
            Self::Usb3 => "USB3",
            Self::Simulator => "Simulator",
            Self::Media => "Media",
            Self::Scanner => "Scanner",
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
    /// no explicit trigger; scanners begin one page only when capture asks.
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
}
impl ErrorKind {
    /// The JSON error code and process exit code.
    pub fn code(self) -> (&'static str, i32) {
        match self {
            Self::OperationFailed => ("operation_failed", 1),
            Self::Unavailable => ("unavailable", 3),
            Self::Timeout => ("timeout", 4),
            Self::Unsupported => ("unsupported", 5),
        }
    }
    fn from_code(code: &str) -> Option<Self> {
        [
            Self::OperationFailed,
            Self::Unavailable,
            Self::Timeout,
            Self::Unsupported,
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

/// The category to send with an error that crosses a process or socket, so
/// the receiving side reports it the same way: `usage` or an [`ErrorKind`]
/// code.
pub fn error_transport_code(error: &anyhow::Error) -> Option<&'static str> {
    error.chain().find_map(|cause| {
        if cause.is::<crate::dataset::Usage>() {
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
        Some("usage") => crate::dataset::Usage(message).into(),
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
        let usage: anyhow::Error = crate::dataset::Usage("bad option".into()).into();
        let timeout = Categorized::error(ErrorKind::Timeout, "camera timed out");
        let wrapped = anyhow::Error::from(Categorized {
            kind: ErrorKind::Unavailable,
            message: "camera porch_disabled is offline".into(),
        })
        .context("connect");
        for (error, code) in [
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
}
