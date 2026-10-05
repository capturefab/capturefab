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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame>;
    fn stop(&mut self) -> Result<()>;
    fn stats(&self) -> Option<TransportStats> {
        None
    }
}

pub const MONO8: u32 = 0x0108_0001;
pub const RGB8: u32 = 0x0218_0014;
