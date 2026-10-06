//! VA-API hardware JPEG encoding in an isolated helper process (see `accel`).
//!
//! The helper (`capturefab __vajpeg`) loads `libva.so.2` and `libva-drm.so.2`
//! at run time, so builds link no libva and hosts without it, or without a
//! JPEG-capable driver, simply keep libjpeg-turbo. It uses the first DRM
//! render node (or `CAPTUREFAB_VAAPI_DEVICE`) whose driver offers baseline
//! JPEG encoding, typically Intel's iHD driver.
//!
//! Capturefab writes the JPEG headers itself with the same tables
//! libjpeg-turbo uses at `QUALITY`, so files are comparable across backends,
//! and converts each frame to JFIF YCbCr 4:2:0 (or gray) directly into the
//! GPU surface. Drivers differ in how they apply the quantization tables
//! they are given, so before reporting ready the helper encodes a test image
//! in each supported way and keeps the first whose output decodes correctly
//! with libjpeg-turbo; if none does, the hardware is not used.
//! `CAPTUREFAB_VAAPI_JPEG=0` disables it.
#![allow(unsafe_code)]

use crate::accel::{self, Kind, Request, Unsupported};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    ffi::{CStr, c_char, c_void},
    sync::OnceLock,
};

pub static VAAPI: Kind = Kind {
    name: "VA-API JPEG",
    argument: "__vajpeg",
    variable: "CAPTUREFAB_VAAPI_JPEG",
    platform: cfg!(target_os = "linux"),
    // libva prints driver information on stderr; keep errors only.
    environment: &[("LIBVA_MESSAGING_LEVEL", "1")],
    accelerator: OnceLock::new(),
};

/// Encode on the VA-API device when the helper is ready; `None` means use
/// the CPU.
pub fn encode(frame: &crate::types::Frame) -> Option<Vec<u8>> {
    accel::encode(&VAAPI, frame)
}

/// The driver and device in use, or why VA-API JPEG is unavailable.
pub fn probe() -> Result<String> {
    accel::probe(&VAAPI)
}

/// Hidden `__vajpeg` mode: encode requests from stdin until it closes.
pub fn run_helper() -> Result<()> {
    accel::serve(Encoder::open)
}

// ---------------------------------------------------------------------------
// JPEG tables and headers.

/// Natural-order index of each zigzag position.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
/// ITU-T T.81 Annex K quantization tables, natural order.
const LUMA_QUANT: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA_QUANT: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
/// Annex K Huffman tables: code counts per length, then symbols.
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// An Annex K table scaled to `quality` exactly as libjpeg does for baseline
/// JPEG, in zigzag order.
fn quant_table(base: &[u8; 64], quality: i32) -> [u8; 64] {
    let quality = quality.clamp(1, 100);
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    std::array::from_fn(|i| ((base[ZIGZAG[i]] as i32 * scale + 50) / 100).clamp(1, 255) as u8)
}
fn tables() -> [[u8; 64]; 2] {
    [
        quant_table(&LUMA_QUANT, crate::jpeg::QUALITY),
        quant_table(&CHROMA_QUANT, crate::jpeg::QUALITY),
    ]
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
    out.extend_from_slice(body);
}
/// Everything before the entropy-coded data of a baseline JFIF image with
/// one interleaved scan: SOI, APP0, DQT, SOF0, DHT and SOS. Color uses
/// 4:2:0 sampling with components 1, 2, 3; gray has one component.
fn header(width: u16, height: u16, channels: usize) -> Vec<u8> {
    let color = channels == 3;
    let mut out = vec![0xFF, 0xD8];
    // JFIF 1.01, no density, no thumbnail: full-range BT.601 YCbCr.
    segment(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    let mut dqt = Vec::new();
    for (id, table) in tables().iter().enumerate().take(if color { 2 } else { 1 }) {
        dqt.push(id as u8);
        dqt.extend_from_slice(table);
    }
    segment(&mut out, 0xDB, &dqt);
    let mut sof = vec![8];
    sof.extend_from_slice(&height.to_be_bytes());
    sof.extend_from_slice(&width.to_be_bytes());
    if color {
        sof.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    } else {
        sof.extend_from_slice(&[1, 1, 0x11, 0]);
    }
    segment(&mut out, 0xC0, &sof);
    let mut dht = Vec::new();
    let mut huffman = |class_id: u8, bits: &[u8; 16], values: &[u8]| {
        dht.push(class_id);
        dht.extend_from_slice(bits);
        dht.extend_from_slice(values);
    };
    huffman(0x00, &DC_LUMA_BITS, &DC_VALUES);
    huffman(0x10, &AC_LUMA_BITS, &AC_LUMA_VALUES);
    if color {
        huffman(0x01, &DC_CHROMA_BITS, &DC_VALUES);
        huffman(0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALUES);
    }
    segment(&mut out, 0xC4, &dht);
    let sos: &[u8] = if color {
        &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]
    } else {
        &[1, 1, 0x00, 0, 63, 0]
    };
    segment(&mut out, 0xDA, sos);
    out
}

// ---------------------------------------------------------------------------
// libva 2.x ABI (va.h, va_enc_jpeg.h), loaded at run time.

type Display = *mut c_void;
type Status = i32;
const SUCCESS: Status = 0;
const PROFILE_JPEG_BASELINE: i32 = 12;
const ENTRYPOINT_ENC_PICTURE: i32 = 7;
const ATTRIB_RT_FORMAT: i32 = 0;
const ATTRIB_ENC_PACKED_HEADERS: i32 = 10;
const ATTRIB_MAX_PICTURE_WIDTH: i32 = 18;
const ATTRIB_MAX_PICTURE_HEIGHT: i32 = 19;
const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
const RT_FORMAT_YUV420: u32 = 0x01;
const RT_FORMAT_YUV400: u32 = 0x10;
const PACKED_HEADER_RAW_DATA: u32 = 0x10;
const PROGRESSIVE: i32 = 1;
const BUFFER_Q_MATRIX: i32 = 11;
const BUFFER_HUFFMAN_TABLE: i32 = 12;
const BUFFER_ENC_CODED: i32 = 21;
const BUFFER_ENC_PICTURE: i32 = 23;
const BUFFER_ENC_SLICE: i32 = 24;
const BUFFER_ENC_PACKED_HEADER_PARAMETER: i32 = 25;
const BUFFER_ENC_PACKED_HEADER_DATA: i32 = 26;
const PACKED_HEADER_TYPE_RAW_DATA: u32 = 4;
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const FOURCC_Y800: u32 = u32::from_le_bytes(*b"Y800");
const INVALID_ID: u32 = 0xFFFF_FFFF;

#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigAttrib {
    kind: i32,
    value: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    reserved: [u32; 4],
}
#[repr(C)]
#[derive(Default)]
struct Image {
    image_id: u32,
    format: ImageFormat,
    buf: u32,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    reserved: [u32; 4],
}
#[repr(C)]
struct PictureParameter {
    reconstructed_picture: u32,
    picture_width: u16,
    picture_height: u16,
    coded_buf: u32,
    /// Bit fields: profile 0..1, progressive 2, huffman 3, interleaved 4,
    /// differential 5.
    pic_flags: u32,
    sample_bit_depth: u8,
    num_scan: u8,
    num_components: u16,
    component_id: [u8; 4],
    quantiser_table_selector: [u8; 4],
    quality: u8,
    reserved: [u32; 4],
}
const PICTURE_HUFFMAN: u32 = 1 << 3;
#[repr(C)]
struct QMatrix {
    load_lum_quantiser_matrix: i32,
    load_chroma_quantiser_matrix: i32,
    lum_quantiser_matrix: [u8; 64],
    chroma_quantiser_matrix: [u8; 64],
    reserved: [u32; 4],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct HuffmanTable {
    num_dc_codes: [u8; 16],
    dc_values: [u8; 12],
    num_ac_codes: [u8; 16],
    ac_values: [u8; 162],
    pad: [u8; 2],
}
#[repr(C)]
struct HuffmanBuffer {
    load_huffman_table: [u8; 2],
    huffman_table: [HuffmanTable; 2],
    reserved: [u32; 4],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct SliceComponent {
    component_selector: u8,
    dc_table_selector: u8,
    ac_table_selector: u8,
}
#[repr(C)]
struct SliceParameter {
    restart_interval: u16,
    num_components: u16,
    components: [SliceComponent; 4],
    reserved: [u32; 4],
}
#[repr(C)]
struct PackedHeaderParameter {
    kind: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    reserved: [u32; 4],
}
#[repr(C)]
struct CodedSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut c_void,
    va_reserved: [u32; 4],
}
// The sizes libva 2.x compiles these structures to on LP64 Linux.
const _: () = {
    assert!(size_of::<ConfigAttrib>() == 8);
    assert!(size_of::<ImageFormat>() == 48);
    assert!(size_of::<Image>() == 120);
    assert!(size_of::<PictureParameter>() == 48);
    assert!(size_of::<QMatrix>() == 152);
    assert!(size_of::<HuffmanBuffer>() == 436);
    assert!(size_of::<SliceParameter>() == 32);
    assert!(size_of::<PackedHeaderParameter>() == 28);
    assert!(size_of::<CodedSegment>() == 48);
};

struct Api {
    get_display_drm: unsafe extern "C" fn(i32) -> Display,
    initialize: unsafe extern "C" fn(Display, *mut i32, *mut i32) -> Status,
    terminate: unsafe extern "C" fn(Display) -> Status,
    error_str: unsafe extern "C" fn(Status) -> *const c_char,
    vendor_string: unsafe extern "C" fn(Display) -> *const c_char,
    max_entrypoints: unsafe extern "C" fn(Display) -> i32,
    query_entrypoints: unsafe extern "C" fn(Display, i32, *mut i32, *mut i32) -> Status,
    get_config_attributes:
        unsafe extern "C" fn(Display, i32, i32, *mut ConfigAttrib, i32) -> Status,
    create_config:
        unsafe extern "C" fn(Display, i32, i32, *mut ConfigAttrib, i32, *mut u32) -> Status,
    destroy_config: unsafe extern "C" fn(Display, u32) -> Status,
    create_surfaces:
        unsafe extern "C" fn(Display, u32, u32, u32, *mut u32, u32, *mut c_void, u32) -> Status,
    destroy_surfaces: unsafe extern "C" fn(Display, *mut u32, i32) -> Status,
    create_context:
        unsafe extern "C" fn(Display, u32, i32, i32, i32, *mut u32, i32, *mut u32) -> Status,
    destroy_context: unsafe extern "C" fn(Display, u32) -> Status,
    create_buffer:
        unsafe extern "C" fn(Display, u32, i32, u32, u32, *mut c_void, *mut u32) -> Status,
    destroy_buffer: unsafe extern "C" fn(Display, u32) -> Status,
    map_buffer: unsafe extern "C" fn(Display, u32, *mut *mut c_void) -> Status,
    unmap_buffer: unsafe extern "C" fn(Display, u32) -> Status,
    derive_image: unsafe extern "C" fn(Display, u32, *mut Image) -> Status,
    max_image_formats: unsafe extern "C" fn(Display) -> i32,
    query_image_formats: unsafe extern "C" fn(Display, *mut ImageFormat, *mut i32) -> Status,
    create_image: unsafe extern "C" fn(Display, *mut ImageFormat, i32, i32, *mut Image) -> Status,
    put_image:
        unsafe extern "C" fn(Display, u32, u32, i32, i32, u32, u32, i32, i32, u32, u32) -> Status,
    destroy_image: unsafe extern "C" fn(Display, u32) -> Status,
    begin_picture: unsafe extern "C" fn(Display, u32, u32) -> Status,
    render_picture: unsafe extern "C" fn(Display, u32, *mut u32, i32) -> Status,
    end_picture: unsafe extern "C" fn(Display, u32) -> Status,
    sync_surface: unsafe extern "C" fn(Display, u32) -> Status,
    // Keep the libraries loaded for as long as the copied function pointers live.
    _libraries: (libloading::Library, libloading::Library),
}
impl Api {
    fn load() -> Result<Self> {
        // SAFETY: loading the system's libva runs its initializers, which is
        // its documented use. Only the stable libva 2 ABI (`.so.2`) is used.
        let (va, drm) = unsafe {
            (
                libloading::Library::new("libva.so.2"),
                libloading::Library::new("libva-drm.so.2"),
            )
        };
        let (va, drm) = match (va, drm) {
            (Ok(va), Ok(drm)) => (va, drm),
            (Err(e), _) | (_, Err(e)) => bail!("libva not found ({e})"),
        };
        // SAFETY: each signature mirrors the libva 2 C prototype (handles and
        // IDs are pointers or unsigned ints, enums are C ints).
        unsafe {
            macro_rules! symbol {
                ($library:expr, $name:literal) => {
                    *$library
                        .get(concat!($name, "\0").as_bytes())
                        .with_context(|| concat!("missing symbol ", $name))?
                };
            }
            Ok(Self {
                get_display_drm: symbol!(drm, "vaGetDisplayDRM"),
                initialize: symbol!(va, "vaInitialize"),
                terminate: symbol!(va, "vaTerminate"),
                error_str: symbol!(va, "vaErrorStr"),
                vendor_string: symbol!(va, "vaQueryVendorString"),
                max_entrypoints: symbol!(va, "vaMaxNumEntrypoints"),
                query_entrypoints: symbol!(va, "vaQueryConfigEntrypoints"),
                get_config_attributes: symbol!(va, "vaGetConfigAttributes"),
                create_config: symbol!(va, "vaCreateConfig"),
                destroy_config: symbol!(va, "vaDestroyConfig"),
                create_surfaces: symbol!(va, "vaCreateSurfaces"),
                destroy_surfaces: symbol!(va, "vaDestroySurfaces"),
                create_context: symbol!(va, "vaCreateContext"),
                destroy_context: symbol!(va, "vaDestroyContext"),
                create_buffer: symbol!(va, "vaCreateBuffer"),
                destroy_buffer: symbol!(va, "vaDestroyBuffer"),
                map_buffer: symbol!(va, "vaMapBuffer"),
                unmap_buffer: symbol!(va, "vaUnmapBuffer"),
                derive_image: symbol!(va, "vaDeriveImage"),
                max_image_formats: symbol!(va, "vaMaxNumImageFormats"),
                query_image_formats: symbol!(va, "vaQueryImageFormats"),
                create_image: symbol!(va, "vaCreateImage"),
                put_image: symbol!(va, "vaPutImage"),
                destroy_image: symbol!(va, "vaDestroyImage"),
                begin_picture: symbol!(va, "vaBeginPicture"),
                render_picture: symbol!(va, "vaRenderPicture"),
                end_picture: symbol!(va, "vaEndPicture"),
                sync_surface: symbol!(va, "vaSyncSurface"),
                _libraries: (va, drm),
            })
        }
    }
    /// Turn a VA status into an error naming the call.
    fn check(&self, what: &str, status: Status) -> Result<()> {
        if status == SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static string for any status.
        let text = unsafe { (self.error_str)(status) };
        let text = if text.is_null() {
            "unknown error".into()
        } else {
            // SAFETY: non-null, NUL-terminated and static.
            unsafe { CStr::from_ptr(text) }.to_string_lossy()
        };
        bail!("{what} failed: {text} (status {status})")
    }
}

// ---------------------------------------------------------------------------
// The encoder.

/// How a driver applies the quantization tables it is given.
#[derive(Clone, Copy, Debug)]
struct Variant {
    /// The picture's `quality` field. 50 leaves supplied tables unscaled in
    /// drivers that scale them by quality; `QUALITY` suits drivers that
    /// ignore supplied tables and scale their own Annex K tables.
    quality: u8,
    /// The matrix buffer in natural rather than zigzag order.
    natural: bool,
}
const VARIANTS: [Variant; 3] = [
    Variant {
        quality: 50,
        natural: false,
    },
    Variant {
        quality: 50,
        natural: true,
    },
    Variant {
        quality: crate::jpeg::QUALITY as u8,
        natural: false,
    },
];

/// Surface, context and output buffer for one frame size and layout.
struct Session {
    width: u32,
    height: u32,
    channels: usize,
    surface: u32,
    context: u32,
    coded: u32,
    coded_size: usize,
}

struct Encoder {
    api: Api,
    display: Display,
    /// The open render node; the display uses its descriptor.
    _device: std::fs::File,
    /// Configurations for 4:2:0 color and, when the driver has it, gray.
    color: u32,
    gray: Option<u32>,
    /// The driver accepts application-written headers.
    packed_headers: bool,
    max_width: u32,
    max_height: u32,
    variant: Variant,
    session: Option<Session>,
}

/// Render nodes to try, in order.
fn devices() -> Vec<std::path::PathBuf> {
    if let Some(device) = std::env::var_os("CAPTUREFAB_VAAPI_DEVICE") {
        return vec![device.into()];
    }
    (128..192)
        .map(|n| std::path::PathBuf::from(format!("/dev/dri/renderD{n}")))
        .filter(|path| path.exists())
        .collect()
}

impl Encoder {
    /// Open the first device with JPEG encoding and verify its output.
    fn open() -> Result<(Self, String)> {
        // Report a missing or incomplete libva before looking for devices.
        drop(Api::load()?);
        let mut reasons = Vec::new();
        for path in devices() {
            // Loading again per device is cheap: the libraries stay mapped.
            let attempt =
                Self::open_device(Api::load()?, &path).and_then(|(mut encoder, vendor)| {
                    let variant = encoder.calibrate()?;
                    let gray = if encoder.gray.is_some() { ", gray" } else { "" };
                    let version = format!("{vendor} on {} ({variant:?}{gray})", path.display());
                    Ok((encoder, version.chars().take(400).collect()))
                });
            match attempt {
                Ok(ready) => return Ok(ready),
                Err(error) => reasons.push(format!("{}: {error:#}", path.display())),
            }
        }
        if reasons.is_empty() {
            bail!("no DRM render node found");
        }
        bail!("no VA-API JPEG encoder: {}", reasons.join("; "))
    }

    /// Initialize one render node with JPEG encoding.
    fn open_device(api: Api, path: &std::path::Path) -> Result<(Self, String)> {
        use std::os::fd::AsRawFd;
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .context("cannot open")?;
        // SAFETY: a valid DRM descriptor that outlives the display.
        let display = unsafe { (api.get_display_drm)(device.as_raw_fd()) };
        ensure!(!display.is_null(), "no VA display");
        // From here the display is terminated by Drop on any failure.
        let mut encoder = Self {
            api,
            display,
            _device: device,
            color: INVALID_ID,
            gray: None,
            packed_headers: false,
            max_width: 16384,
            max_height: 16384,
            variant: VARIANTS[0],
            session: None,
        };
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a fresh display and valid out-pointers.
        let status = unsafe { (encoder.api.initialize)(display, &mut major, &mut minor) };
        encoder.api.check("vaInitialize", status)?;
        encoder.configure()?;
        // SAFETY: an initialized display; the string is owned by libva.
        let vendor = unsafe { (encoder.api.vendor_string)(display) };
        let vendor = if vendor.is_null() {
            "VA-API".to_string()
        } else {
            // SAFETY: non-null and NUL-terminated.
            unsafe { CStr::from_ptr(vendor) }
                .to_string_lossy()
                .trim()
                .to_string()
        };
        Ok((encoder, format!("{vendor} (VA-API {major}.{minor})")))
    }

    fn release(&mut self) {
        self.end_session();
        let api = &self.api;
        // SAFETY: each handle was created on this display and is released once.
        unsafe {
            if self.color != INVALID_ID {
                (api.destroy_config)(self.display, self.color);
                self.color = INVALID_ID;
            }
            if let Some(gray) = self.gray.take() {
                (api.destroy_config)(self.display, gray);
            }
            if !self.display.is_null() {
                (api.terminate)(self.display);
                self.display = std::ptr::null_mut();
            }
        }
    }

    /// Check for baseline JPEG encoding and create its configurations.
    fn configure(&mut self) -> Result<()> {
        let (api, display) = (&self.api, self.display);
        // SAFETY: an initialized display; buffers are sized as libva asks.
        unsafe {
            let mut entrypoints = vec![0i32; (api.max_entrypoints)(display).max(1) as usize];
            let mut count = 0;
            api.check(
                "vaQueryConfigEntrypoints",
                (api.query_entrypoints)(
                    display,
                    PROFILE_JPEG_BASELINE,
                    entrypoints.as_mut_ptr(),
                    &mut count,
                ),
            )
            .context("no baseline JPEG profile")?;
            ensure!(
                entrypoints[..(count.max(0) as usize).min(entrypoints.len())]
                    .contains(&ENTRYPOINT_ENC_PICTURE),
                "driver decodes JPEG but cannot encode it"
            );
            let mut attributes = [
                ATTRIB_RT_FORMAT,
                ATTRIB_ENC_PACKED_HEADERS,
                ATTRIB_MAX_PICTURE_WIDTH,
                ATTRIB_MAX_PICTURE_HEIGHT,
            ]
            .map(|kind| ConfigAttrib { kind, value: 0 });
            api.check(
                "vaGetConfigAttributes",
                (api.get_config_attributes)(
                    display,
                    PROFILE_JPEG_BASELINE,
                    ENTRYPOINT_ENC_PICTURE,
                    attributes.as_mut_ptr(),
                    attributes.len() as i32,
                ),
            )?;
            let supported = |a: ConfigAttrib| (a.value != ATTRIB_NOT_SUPPORTED).then_some(a.value);
            let formats = supported(attributes[0]).unwrap_or(RT_FORMAT_YUV420);
            ensure!(
                formats & RT_FORMAT_YUV420 != 0,
                "driver cannot encode 4:2:0 JPEG"
            );
            self.packed_headers =
                supported(attributes[1]).is_some_and(|v| v & PACKED_HEADER_RAW_DATA != 0);
            if let Some(width) = supported(attributes[2]).filter(|&v| v >= 16) {
                self.max_width = width.min(65_535);
            }
            if let Some(height) = supported(attributes[3]).filter(|&v| v >= 16) {
                self.max_height = height.min(65_535);
            }
            let create = |format: u32| -> Result<u32> {
                let mut attributes = vec![ConfigAttrib {
                    kind: ATTRIB_RT_FORMAT,
                    value: format,
                }];
                if self.packed_headers {
                    attributes.push(ConfigAttrib {
                        kind: ATTRIB_ENC_PACKED_HEADERS,
                        value: PACKED_HEADER_RAW_DATA,
                    });
                }
                let mut config = INVALID_ID;
                api.check(
                    "vaCreateConfig",
                    (api.create_config)(
                        display,
                        PROFILE_JPEG_BASELINE,
                        ENTRYPOINT_ENC_PICTURE,
                        attributes.as_mut_ptr(),
                        attributes.len() as i32,
                        &mut config,
                    ),
                )?;
                Ok(config)
            };
            self.color = create(RT_FORMAT_YUV420)?;
            self.gray = (formats & RT_FORMAT_YUV400 != 0)
                .then(|| create(RT_FORMAT_YUV400).ok())
                .flatten();
        }
        Ok(())
    }

    /// Find how this driver applies supplied tables by encoding a test image
    /// and decoding it with libjpeg-turbo; also decides whether gray works.
    fn calibrate(&mut self) -> Result<Variant> {
        let (width, height) = (128u32, 96u32);
        let rgb: Vec<u8> = (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                let edge = if (x / 16 + y / 16) % 2 == 0 { 40 } else { 0 };
                [(x * 2) as u8, (y * 2 + edge) as u8, (255 - x - y / 2) as u8]
            })
            .collect();
        let gray: Vec<u8> = rgb.chunks(3).map(|p| p[1]).collect();
        let mut out = vec![0u8; 1 << 20];
        let mut failures = Vec::new();
        for variant in VARIANTS {
            self.variant = variant;
            match self
                .encode_raw(width, height, 3, &rgb, &mut out)
                .and_then(|n| accurate(&out[..n], width, height, &rgb, 3))
            {
                Ok(()) => {
                    if self.gray.is_some()
                        && self
                            .encode_raw(width, height, 1, &gray, &mut out)
                            .and_then(|n| accurate(&out[..n], width, height, &gray, 1))
                            .is_err()
                    {
                        // Monochrome then stays on the CPU.
                        self.end_session();
                        self.gray = None;
                    }
                    self.end_session();
                    return Ok(variant);
                }
                Err(error) => failures.push(format!("{variant:?}: {error:#}")),
            }
        }
        self.end_session();
        bail!(
            "hardware JPEG output did not decode correctly ({})",
            failures.join("; ")
        )
    }

    fn end_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            let api = &self.api;
            // SAFETY: created by `begin_session` on this display.
            unsafe {
                (api.destroy_buffer)(self.display, session.coded);
                (api.destroy_context)(self.display, session.context);
                (api.destroy_surfaces)(self.display, &mut session.surface, 1);
            }
        }
    }

    fn begin_session(&mut self, width: u32, height: u32, channels: usize) -> Result<()> {
        if self
            .session
            .as_ref()
            .is_some_and(|s| (s.width, s.height, s.channels) == (width, height, channels))
        {
            return Ok(());
        }
        self.end_session();
        let (config, format) = if channels == 1 {
            (
                self.gray
                    .ok_or_else(|| Unsupported("no gray JPEG encoding".into()))?,
                RT_FORMAT_YUV400,
            )
        } else {
            (self.color, RT_FORMAT_YUV420)
        };
        // Surfaces hold whole chroma samples; the JPEG header crops.
        let (surface_width, surface_height) =
            (width.next_multiple_of(2), height.next_multiple_of(2));
        let api = &self.api;
        let mut session = Session {
            width,
            height,
            channels,
            surface: INVALID_ID,
            context: INVALID_ID,
            coded: INVALID_ID,
            // Above any baseline JPEG of the frame, which noise can push past
            // the raw size at high quality.
            coded_size: (width as usize * height as usize * 3).max(1 << 16) + 4096,
        };
        // SAFETY: valid display and configuration; IDs are written by libva
        // and released by `end_session`, including after a partial failure.
        let result = unsafe {
            api.check(
                "vaCreateSurfaces",
                (api.create_surfaces)(
                    self.display,
                    format,
                    surface_width,
                    surface_height,
                    &mut session.surface,
                    1,
                    std::ptr::null_mut(),
                    0,
                ),
            )
            .and_then(|()| {
                api.check(
                    "vaCreateContext",
                    (api.create_context)(
                        self.display,
                        config,
                        surface_width as i32,
                        surface_height as i32,
                        PROGRESSIVE,
                        &mut session.surface,
                        1,
                        &mut session.context,
                    ),
                )
            })
            .and_then(|()| {
                api.check(
                    "vaCreateBuffer",
                    (api.create_buffer)(
                        self.display,
                        session.context,
                        BUFFER_ENC_CODED,
                        session.coded_size as u32,
                        1,
                        std::ptr::null_mut(),
                        &mut session.coded,
                    ),
                )
            })
        };
        let created = (session.surface, session.context, session.coded);
        self.session = Some(session);
        if result.is_err() {
            // Release whatever was created.
            let session = self.session.take().expect("session");
            // SAFETY: only IDs libva actually returned are released.
            unsafe {
                if created.2 != INVALID_ID {
                    (api.destroy_buffer)(self.display, created.2);
                }
                if created.1 != INVALID_ID {
                    (api.destroy_context)(self.display, created.1);
                }
                if created.0 != INVALID_ID {
                    let mut surface = session.surface;
                    (api.destroy_surfaces)(self.display, &mut surface, 1);
                }
            }
        }
        result
    }

    /// Write the frame into the surface as NV12 (color) or Y800 (gray),
    /// directly through a derived image where the driver allows, else
    /// through an image copied into the surface.
    fn upload(&self, session: &Session, pixels: &[u8]) -> Result<()> {
        let api = &self.api;
        let fourcc = if session.channels == 1 {
            FOURCC_Y800
        } else {
            FOURCC_NV12
        };
        let mut image = Image::default();
        // SAFETY: valid display and surface; the image is destroyed below.
        let derived =
            unsafe { (api.derive_image)(self.display, session.surface, &mut image) } == SUCCESS;
        if derived && image.format.fourcc != fourcc {
            // SAFETY: derived above.
            unsafe { (api.destroy_image)(self.display, image.image_id) };
            return self.upload_copy(session, pixels, fourcc);
        }
        if !derived {
            return self.upload_copy(session, pixels, fourcc);
        }
        let written = self.fill(&image, session, pixels);
        // SAFETY: derived above.
        unsafe { (api.destroy_image)(self.display, image.image_id) };
        written
    }
    fn upload_copy(&self, session: &Session, pixels: &[u8], fourcc: u32) -> Result<()> {
        let api = &self.api;
        // SAFETY: valid display; the list is sized as libva asks.
        let mut format = unsafe {
            let mut formats =
                vec![ImageFormat::default(); (api.max_image_formats)(self.display).max(1) as usize];
            let mut count = 0;
            api.check(
                "vaQueryImageFormats",
                (api.query_image_formats)(self.display, formats.as_mut_ptr(), &mut count),
            )?;
            formats
                .into_iter()
                .take(count.max(0) as usize)
                .find(|f| f.fourcc == fourcc)
                .ok_or_else(|| anyhow!("driver has no image format for the surface"))?
        };
        let (width, height) = (
            session.width.next_multiple_of(2),
            session.height.next_multiple_of(2),
        );
        let mut image = Image::default();
        // SAFETY: valid display and format; the image is destroyed below.
        unsafe {
            api.check(
                "vaCreateImage",
                (api.create_image)(
                    self.display,
                    &mut format,
                    width as i32,
                    height as i32,
                    &mut image,
                ),
            )?;
        }
        let result = self.fill(&image, session, pixels).and_then(|()| {
            // SAFETY: the image and surface belong to this display.
            api.check("vaPutImage", unsafe {
                (api.put_image)(
                    self.display,
                    session.surface,
                    image.image_id,
                    0,
                    0,
                    width,
                    height,
                    0,
                    0,
                    width,
                    height,
                )
            })
        });
        // SAFETY: created above.
        unsafe { (api.destroy_image)(self.display, image.image_id) };
        result
    }
    /// Map an image's buffer and write the frame at its plane pitches.
    fn fill(&self, image: &Image, session: &Session, pixels: &[u8]) -> Result<()> {
        let api = &self.api;
        let (width, height) = (session.width as usize, session.height as usize);
        let mut base = std::ptr::null_mut();
        // SAFETY: the image's buffer is mapped for `data_size` bytes until
        // unmapped below, and written only within the plane bounds checked
        // here.
        unsafe {
            api.check(
                "vaMapBuffer",
                (api.map_buffer)(self.display, image.buf, &mut base),
            )?;
            let data = std::slice::from_raw_parts_mut(base.cast::<u8>(), image.data_size as usize);
            let result = (|| -> Result<()> {
                let luma_pitch = image.pitches[0] as usize;
                let luma_start = image.offsets[0] as usize;
                if session.channels == 1 {
                    ensure!(
                        luma_pitch >= width
                            && data.len() >= luma_start + (height - 1) * luma_pitch + width,
                        "unexpected surface layout"
                    );
                    for (row, line) in pixels.chunks_exact(width).take(height).enumerate() {
                        let at = luma_start + row * luma_pitch;
                        data[at..at + width].copy_from_slice(line);
                    }
                    return Ok(());
                }
                ensure!(image.num_planes >= 2, "unexpected surface layout");
                let chroma_start = image.offsets[1] as usize;
                ensure!(
                    chroma_start > luma_start && chroma_start <= data.len(),
                    "unexpected surface layout"
                );
                let (head, chroma) = data.split_at_mut(chroma_start);
                let luma = &mut head[luma_start..];
                let chroma_pitch = image.pitches[1] as usize;
                let (chroma_width, chroma_rows) = (width.div_ceil(2), height.div_ceil(2));
                ensure!(
                    luma_pitch >= width
                        && chroma_pitch >= chroma_width * 2
                        && luma.len() >= (height - 1) * luma_pitch + width
                        && chroma.len() >= (chroma_rows - 1) * chroma_pitch + chroma_width * 2,
                    "unexpected surface layout"
                );
                crate::jpeg::ycbcr420(
                    pixels.as_chunks::<3>().0,
                    width,
                    height,
                    luma,
                    luma_pitch,
                    chroma,
                    chroma_pitch,
                );
                Ok(())
            })();
            (api.unmap_buffer)(self.display, image.buf);
            result
        }
    }

    /// Encode `pixels` (gray or RGB) into `out`, returning the JPEG length.
    fn encode_raw(
        &mut self,
        width: u32,
        height: u32,
        channels: usize,
        pixels: &[u8],
        out: &mut [u8],
    ) -> Result<usize> {
        if !(16..=self.max_width).contains(&width) || !(16..=self.max_height).contains(&height) {
            return Err(
                Unsupported(format!("{width}x{height} outside the encoder's range")).into(),
            );
        }
        ensure!(
            pixels.len() >= width as usize * height as usize * channels,
            "truncated pixels"
        );
        self.begin_session(width, height, channels)?;
        let session = self.session.as_ref().expect("session");
        self.upload(session, pixels)?;
        let color = channels == 3;
        let [luma, chroma] = tables();
        let order = |table: [u8; 64]| -> [u8; 64] {
            if self.variant.natural {
                let mut natural = [0; 64];
                for (i, &q) in table.iter().enumerate() {
                    natural[ZIGZAG[i]] = q;
                }
                natural
            } else {
                table
            }
        };
        let huffman = |dc: &[u8; 16], ac: &[u8; 16], values: &[u8; 162]| HuffmanTable {
            num_dc_codes: *dc,
            dc_values: DC_VALUES,
            num_ac_codes: *ac,
            ac_values: *values,
            pad: [0; 2],
        };
        let mut picture = PictureParameter {
            reconstructed_picture: session.surface,
            picture_width: width as u16,
            picture_height: height as u16,
            coded_buf: session.coded,
            pic_flags: PICTURE_HUFFMAN,
            sample_bit_depth: 8,
            num_scan: 1,
            num_components: channels as u16,
            component_id: [1, 2, 3, 0],
            quantiser_table_selector: [0, 1, 1, 0],
            quality: self.variant.quality,
            reserved: [0; 4],
        };
        let mut matrix = QMatrix {
            load_lum_quantiser_matrix: 1,
            load_chroma_quantiser_matrix: color as i32,
            lum_quantiser_matrix: order(luma),
            chroma_quantiser_matrix: order(chroma),
            reserved: [0; 4],
        };
        let mut tables = HuffmanBuffer {
            load_huffman_table: [1, color as u8],
            huffman_table: [
                huffman(&DC_LUMA_BITS, &AC_LUMA_BITS, &AC_LUMA_VALUES),
                huffman(&DC_CHROMA_BITS, &AC_CHROMA_BITS, &AC_CHROMA_VALUES),
            ],
            reserved: [0; 4],
        };
        let component = |id: u8, table: u8| SliceComponent {
            component_selector: id,
            dc_table_selector: table,
            ac_table_selector: table,
        };
        let mut slice = SliceParameter {
            restart_interval: 0,
            num_components: channels as u16,
            components: [
                component(1, 0),
                component(2, 1),
                component(3, 1),
                component(0, 0),
            ],
            reserved: [0; 4],
        };
        let mut header = header(width as u16, height as u16, channels);
        let mut packed = PackedHeaderParameter {
            kind: PACKED_HEADER_TYPE_RAW_DATA,
            bit_length: header.len() as u32 * 8,
            has_emulation_bytes: 0,
            reserved: [0; 4],
        };
        let api = &self.api;
        let (display, context) = (self.display, session.context);
        let mut buffers: Vec<u32> = Vec::with_capacity(6);
        // SAFETY: each parameter buffer is created from a live local of the
        // size and type libva expects and destroyed below; the coded buffer
        // is mapped only after the surface is synchronized and read within
        // the segment sizes libva reports.
        let result = unsafe {
            let create = |kind: i32, size: usize, data: *mut c_void| -> Result<u32> {
                let mut id = INVALID_ID;
                api.check(
                    "vaCreateBuffer",
                    (api.create_buffer)(display, context, kind, size as u32, 1, data, &mut id),
                )?;
                Ok(id)
            };
            (|| -> Result<usize> {
                buffers.push(create(
                    BUFFER_ENC_PICTURE,
                    size_of::<PictureParameter>(),
                    (&raw mut picture).cast(),
                )?);
                buffers.push(create(
                    BUFFER_Q_MATRIX,
                    size_of::<QMatrix>(),
                    (&raw mut matrix).cast(),
                )?);
                buffers.push(create(
                    BUFFER_HUFFMAN_TABLE,
                    size_of::<HuffmanBuffer>(),
                    (&raw mut tables).cast(),
                )?);
                if self.packed_headers {
                    buffers.push(create(
                        BUFFER_ENC_PACKED_HEADER_PARAMETER,
                        size_of::<PackedHeaderParameter>(),
                        (&raw mut packed).cast(),
                    )?);
                    buffers.push(create(
                        BUFFER_ENC_PACKED_HEADER_DATA,
                        header.len(),
                        header.as_mut_ptr().cast(),
                    )?);
                }
                buffers.push(create(
                    BUFFER_ENC_SLICE,
                    size_of::<SliceParameter>(),
                    (&raw mut slice).cast(),
                )?);
                api.check(
                    "vaBeginPicture",
                    (api.begin_picture)(display, context, session.surface),
                )?;
                let rendered = api.check(
                    "vaRenderPicture",
                    (api.render_picture)(
                        display,
                        context,
                        buffers.as_mut_ptr(),
                        buffers.len() as i32,
                    ),
                );
                // A begun picture is always ended.
                api.check("vaEndPicture", (api.end_picture)(display, context))?;
                rendered?;
                api.check(
                    "vaSyncSurface",
                    (api.sync_surface)(display, session.surface),
                )?;
                let mut segment = std::ptr::null_mut();
                api.check(
                    "vaMapBuffer",
                    (api.map_buffer)(display, session.coded, &mut segment),
                )?;
                let mut length = 0usize;
                let copied = (|| -> Result<()> {
                    let mut segments = 0;
                    while !segment.is_null() {
                        segments += 1;
                        ensure!(segments <= 64, "too many coded segments");
                        let s = &*segment.cast::<CodedSegment>();
                        let size = s.size as usize;
                        ensure!(
                            size <= session.coded_size && length + size <= out.len(),
                            "JPEG exceeds the shared buffer"
                        );
                        if size > 0 {
                            ensure!(!s.buf.is_null(), "empty coded segment");
                            out[length..length + size].copy_from_slice(std::slice::from_raw_parts(
                                s.buf.cast::<u8>(),
                                size,
                            ));
                        }
                        length += size;
                        segment = s.next;
                    }
                    Ok(())
                })();
                (api.unmap_buffer)(display, session.coded);
                copied?;
                Ok(length)
            })()
        };
        // SAFETY: created above on this display.
        for id in buffers {
            unsafe { (api.destroy_buffer)(display, id) };
        }
        let mut length = result?;
        // Drivers differ in whether they write the final EOI marker.
        if length < 2 || out[length - 2..length] != [0xFF, 0xD9] {
            ensure!(length + 2 <= out.len(), "JPEG exceeds the shared buffer");
            out[length..length + 2].copy_from_slice(&[0xFF, 0xD9]);
            length += 2;
        }
        ensure!(
            crate::jpeg::well_formed(&out[..length], width, height),
            "driver produced a malformed JPEG"
        );
        Ok(length)
    }
}
impl accel::Encoder for Encoder {
    fn encode(&mut self, request: &Request, pixels: &[u8], out: &mut [u8]) -> Result<usize> {
        self.encode_raw(request.width, request.height, request.channels, pixels, out)
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        self.release();
    }
}

/// The JPEG decodes to the source within the error libjpeg-turbo itself
/// makes at the same quality, so tables, sampling and color all match.
fn accurate(jpeg: &[u8], width: u32, height: u32, source: &[u8], channels: usize) -> Result<()> {
    use turbojpeg::PixelFormat;
    let format = if channels == 1 {
        PixelFormat::GRAY
    } else {
        PixelFormat::RGB
    };
    let decoded = turbojpeg::decompress(jpeg, format).context("output does not decode")?;
    ensure!(
        (decoded.width, decoded.height) == (width as usize, height as usize),
        "decoded size differs"
    );
    let raster = crate::jpeg::Raster {
        width,
        height,
        channels,
        data: source.to_vec(),
    };
    let reference = turbojpeg::decompress(&crate::jpeg::software(&raster)?, format)?;
    let psnr = |pixels: &[u8]| {
        let error = pixels
            .iter()
            .zip(source)
            .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
            .sum::<f64>()
            / source.len() as f64;
        10.0 * (255.0f64.powi(2) / error.max(1e-9)).log10()
    };
    let (hardware, software) = (psnr(&decoded.pixels), psnr(&reference.pixels));
    ensure!(
        hardware >= 30.0 && hardware >= software - 3.0,
        "decoded at {hardware:.1} dB PSNR against {software:.1} dB for libjpeg-turbo"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use turbojpeg::{Compressor, Image as TurboImage, PixelFormat, Subsamp};

    /// libjpeg-turbo at `QUALITY` with standard Huffman tables; returns the
    /// JPEG and the offset of its entropy-coded data.
    fn turbo(width: usize, height: usize, channels: usize, pixels: &[u8]) -> (Vec<u8>, usize) {
        let mut compressor = Compressor::new().unwrap();
        compressor.set_quality(crate::jpeg::QUALITY).unwrap();
        let (format, subsamp) = if channels == 1 {
            (PixelFormat::GRAY, Subsamp::Gray)
        } else {
            (PixelFormat::RGB, Subsamp::Sub2x2)
        };
        compressor.set_subsamp(subsamp).unwrap();
        let jpeg = compressor
            .compress_to_vec(TurboImage {
                pixels,
                width,
                pitch: width * channels,
                height,
                format,
            })
            .unwrap();
        let mut i = 2;
        loop {
            let length = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
            if jpeg[i + 1] == 0xDA {
                return (jpeg, i + 2 + length);
            }
            i += 2 + length;
        }
    }

    /// The generated header is byte-for-byte what a decoder needs for
    /// libjpeg-turbo's own scan data at the same quality: tables, zigzag
    /// order, sampling factors and component selectors all agree.
    #[test]
    fn header_matches_libjpeg_turbo_tables_and_layout() {
        for (channels, width, height) in [(3usize, 64usize, 48usize), (3, 33, 17), (1, 40, 24)] {
            let pixels: Vec<u8> = (0..width * height * channels)
                .map(|i| (i * 7 + i / 97) as u8)
                .collect();
            let (jpeg, scan) = turbo(width, height, channels, &pixels);
            let mut transplanted = header(width as u16, height as u16, channels);
            transplanted.extend_from_slice(&jpeg[scan..]);
            let format = if channels == 1 {
                PixelFormat::GRAY
            } else {
                PixelFormat::RGB
            };
            let expected = turbojpeg::decompress(&jpeg, format).unwrap();
            let actual = turbojpeg::decompress(&transplanted, format).unwrap();
            assert_eq!(
                actual.pixels, expected.pixels,
                "{channels} {width}x{height}"
            );
            assert!(crate::jpeg::well_formed(
                &transplanted,
                width as u32,
                height as u32
            ));
        }
    }

    #[test]
    fn quality_scaling_matches_libjpeg() {
        let [luma, chroma] = tables();
        // libjpeg at quality 90: scale 20%, rounded, at least 1.
        assert_eq!(&luma[..4], &[3, 2, 2, 3]);
        assert_eq!(chroma[63], 20);
        // Quality 50 is the Annex K table itself.
        assert_eq!(quant_table(&LUMA_QUANT, 50)[2], LUMA_QUANT[8]);
    }

    #[test]
    fn calibration_accepts_good_output_and_rejects_wrong_tables() {
        let (width, height) = (64u32, 48u32);
        let rgb: Vec<u8> = (0..width * height)
            .flat_map(|i| [(i % 64 * 4) as u8, (i / 64 * 5) as u8, 128])
            .collect();
        let (good, _) = turbo(64, 48, 3, &rgb);
        assert!(accurate(&good, width, height, &rgb, 3).is_ok());
        // Scan data quantized at quality 90 but labeled with quality-50
        // tables: what a driver that rescales supplied tables would produce.
        let (jpeg, scan) = turbo(64, 48, 3, &rgb);
        let mut wrong = jpeg[..scan].to_vec();
        // Every 8-bit table in every DQT segment, by its table ID.
        let mut i = 2;
        while i + 4 <= wrong.len() {
            let length = u16::from_be_bytes([wrong[i + 2], wrong[i + 3]]) as usize;
            if wrong[i + 1] == 0xDB {
                let mut at = i + 4;
                while at < i + 2 + length {
                    let base = [&LUMA_QUANT, &CHROMA_QUANT][(wrong[at] & 0x0F) as usize];
                    wrong[at + 1..at + 65].copy_from_slice(&quant_table(base, 50));
                    at += 65;
                }
            }
            i += 2 + length;
        }
        wrong.extend_from_slice(&jpeg[scan..]);
        assert!(accurate(&wrong, width, height, &rgb, 3).is_err());
    }
}
