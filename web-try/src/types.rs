//! The frame and camera kind shapes of the native `types.rs` and
//! `camera_profile.rs`.

/// PFNC Mono8 (`types.rs`).
pub const MONO8: u32 = 0x0108_0001;
/// PFNC RGB8 (`types.rs`).
pub const RGB8: u32 = 0x0218_0014;
pub const BGR8: u32 = 0x0218_0015;
pub const MONO10: u32 = 0x0110_0003;
pub const MONO12: u32 = 0x0110_0005;
pub const MONO16: u32 = 0x0110_0007;
pub const BAYER_RG8: u32 = 0x0108_0009;

/// One image as a camera delivered it: tightly packed rows of `width`
/// pixels, 16-bit samples little-endian (`types.rs` `Frame`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub timestamp_ns: u64,
    pub data: Vec<u8>,
}

/// What a camera images (`camera_profile.rs` `CameraKind`). Only depth,
/// thermal and line-scan change what is measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraKind {
    #[default]
    Unknown,
    AreaScan,
    LineScan,
    Depth,
    Microscope,
    Thermal,
    DocumentScanner,
}

impl CameraKind {
    /// The ABI's numbering: 0 unknown, 1 area scan, 2 line scan, 3 depth,
    /// 4 microscope, 5 thermal, 6 document scanner.
    pub fn from_code(code: u32) -> Self {
        match code {
            1 => Self::AreaScan,
            2 => Self::LineScan,
            3 => Self::Depth,
            4 => Self::Microscope,
            5 => Self::Thermal,
            6 => Self::DocumentScanner,
            _ => Self::Unknown,
        }
    }
    /// The snake_case name serde gives it natively.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::AreaScan => "area_scan",
            Self::LineScan => "line_scan",
            Self::Depth => "depth",
            Self::Microscope => "microscope",
            Self::Thermal => "thermal",
            Self::DocumentScanner => "document_scanner",
        }
    }
}
