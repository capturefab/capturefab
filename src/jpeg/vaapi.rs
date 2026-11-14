//! VA-API JPEG encoding, in the JPEG helper process (`helper.rs`).
//!
//! The helper loads `libva.so.2` and `libva-drm.so.2` when it starts, so a
//! build links no libva, and uses the first DRM render node (or
//! `CAPTUREFAB_VAAPI_DEVICE`) whose driver offers baseline JPEG encoding,
//! typically Intel's iHD.
//!
//! Capturefab writes the JPEG headers itself, with the tables libjpeg-turbo
//! uses at the frame's quality, so files are comparable across encoders.
//! Drivers differ in how they apply the quantization tables they are
//! given, so before the helper says it is ready it encodes a test image in
//! each supported way and keeps the first whose output libjpeg-turbo
//! decodes faithfully; if none does, the hardware is not used.
//!
//! An 8-bit gray frame whose rows suit the driver is encoded from the
//! segment the camera wrote it to, which the driver takes as a surface of
//! user memory (USERPTR): no copy. Other frames are written once into a
//! surface the driver derives an image of: 8-bit gray rows copied
//! (`gpu_upload`), other monochrome formats converted to gray, and color
//! converted from its rows straight to NV12. The coded data is copied
//! from the driver's buffer into the worker's output segment.
#![deny(clippy::missing_const_for_fn)]
#![cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "the Linux encoder and its fake VA-API driver are compiled on other Unix hosts for tests"
    )
)]
use super::{
    backend::Unsupported,
    helper::{Encoder, Hello, Library, Opened},
    huffman::{
        AC_CHROMINANCE, AC_CHROMINANCE_VALUES, AC_LUMINANCE, AC_LUMINANCE_VALUES, DC_CHROMINANCE,
        DC_LUMINANCE, DC_VALUES,
    },
};
use crate::{framebuf::shared::Segment, types::Frame};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    sync::{Mutex, PoisonError},
};

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

/// Why this host has no device VA-API could use, if it has none.
pub(crate) fn absent() -> Option<String> {
    devices()
        .is_empty()
        .then(|| "no DRM render node found (/dev/dri/renderD*)".into())
}

/// Open the first device that encodes JPEG faithfully.
pub(crate) fn open() -> Opened {
    // A missing or incomplete libva is reported before any device.
    drop(Api::load()?);
    let mut reasons = Vec::new();
    for path in devices() {
        let attempt = Va::open_device(Api::load()?, &path).and_then(|va| {
            #[cfg(feature = "jpeg")]
            let mut va = va;
            #[cfg(feature = "jpeg")]
            va.calibrate()?;
            #[cfg(not(feature = "jpeg"))]
            Va::calibrate()?;
            Ok(va)
        });
        match attempt {
            Ok(va) => {
                let hello = Hello {
                    ok: true,
                    gray: va.gray.is_some(),
                    about: va.about(&path),
                    ..Hello::default()
                };
                return Ok((Box::new(va), hello));
            }
            Err(error) => reasons.push(format!("{}: {error:#}", path.display())),
        }
    }
    if reasons.is_empty() {
        bail!("no DRM render node found");
    }
    bail!("no VA-API JPEG encoder: {}", reasons.join("; "))
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

/// An Annex K table scaled to `quality` exactly as libjpeg does for baseline
/// JPEG, in zigzag order.
fn quant_table(base: &[u8; 64], quality: i32) -> [u8; 64] {
    let quality = quality.clamp(1, 100);
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    std::array::from_fn(|i| {
        u8::try_from(((i32::from(base[ZIGZAG[i]]) * scale + 50) / 100).clamp(1, 255))
            .expect("clamped baseline quantizer fits u8")
    })
}
fn tables(quality: i32) -> [[u8; 64]; 2] {
    [
        quant_table(&LUMA_QUANT, quality),
        quant_table(&CHROMA_QUANT, quality),
    ]
}

/// The raw-size bound plus header room, within vaCreateBuffer's size field.
fn coded_buffer_size(width: u32, height: u32) -> Result<c_uint> {
    let size = (u64::from(width) * u64::from(height))
        .checked_mul(3)
        .map(|size| size.max(1 << 16))
        .and_then(|size| size.checked_add(4096))
        .context("coded JPEG buffer size overflow")?;
    c_uint::try_from(size).context("coded JPEG buffer exceeds the VA-API size limit")
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(
        &u16::try_from(body.len() + 2)
            .expect("fixed JPEG header segments fit u16")
            .to_be_bytes(),
    );
    out.extend_from_slice(body);
}
/// Everything before the entropy-coded data of a baseline JFIF image with
/// one interleaved scan: SOI, APP0, DQT, SOF0, DHT and SOS. Color uses
/// 4:2:0 sampling with components 1, 2, 3; gray has one component.
fn header(width: u16, height: u16, channels: usize, quality: i32) -> Vec<u8> {
    let color = channels == 3;
    let mut out = vec![0xFF, 0xD8];
    // JFIF 1.01, no density, no thumbnail: full-range BT.601 YCbCr.
    segment(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    let mut dqt = Vec::new();
    for (id, table) in tables(quality)
        .iter()
        .enumerate()
        .take(if color { 2 } else { 1 })
    {
        dqt.push(u8::try_from(id).expect("there are only two quantization tables"));
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
    huffman(0x00, &DC_LUMINANCE, &DC_VALUES);
    huffman(0x10, &AC_LUMINANCE, &AC_LUMINANCE_VALUES);
    if color {
        huffman(0x01, &DC_CHROMINANCE, &DC_VALUES);
        huffman(0x11, &AC_CHROMINANCE, &AC_CHROMINANCE_VALUES);
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
type Status = c_int;
/// `VAMessageCallback`: a message for the callback's user context.
type MessageCallback = unsafe extern "C" fn(*mut c_void, *const c_char);
/// vaSetErrorCallback and vaSetInfoCallback: per display since libva 2.
type SetCallback =
    unsafe extern "C" fn(Display, Option<MessageCallback>, *mut c_void) -> Option<MessageCallback>;
const SUCCESS: Status = 0;
const PROFILE_JPEG_BASELINE: c_int = 12;
const ENTRYPOINT_ENC_PICTURE: c_int = 7;
const ATTRIB_RT_FORMAT: c_int = 0;
const ATTRIB_ENC_PACKED_HEADERS: c_int = 10;
const ATTRIB_MAX_PICTURE_WIDTH: c_int = 18;
const ATTRIB_MAX_PICTURE_HEIGHT: c_int = 19;
const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
const RT_FORMAT_YUV420: u32 = 0x01;
const RT_FORMAT_YUV400: u32 = 0x10;
const PACKED_HEADER_RAW_DATA: u32 = 0x10;
const PROGRESSIVE: c_int = 1;
const BUFFER_Q_MATRIX: c_int = 11;
const BUFFER_HUFFMAN_TABLE: c_int = 12;
const BUFFER_ENC_CODED: c_int = 21;
const BUFFER_ENC_PICTURE: c_int = 23;
const BUFFER_ENC_SLICE: c_int = 24;
const BUFFER_ENC_PACKED_HEADER_PARAMETER: c_int = 25;
const BUFFER_ENC_PACKED_HEADER_DATA: c_int = 26;
const PACKED_HEADER_TYPE_RAW_DATA: u32 = 4;
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const FOURCC_Y800: u32 = u32::from_le_bytes(*b"Y800");
const INVALID_ID: u32 = 0xFFFF_FFFF;
const SURFACE_ATTRIB_MEMORY_TYPE: c_int = 6;
const SURFACE_ATTRIB_EXTERNAL_BUFFERS: c_int = 7;
const SURFACE_ATTRIB_SETTABLE: u32 = 2;
const GENERIC_VALUE_INTEGER: c_int = 1;
const GENERIC_VALUE_POINTER: c_int = 3;
const MEM_TYPE_USER_PTR: i32 = 4;
/// Rows of user memory the driver can address: whole cache lines.
const USER_PITCH: usize = 64;

#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigAttrib {
    kind: c_int,
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
/// `VAGenericValue`'s value.
#[repr(C)]
#[derive(Clone, Copy)]
union GenericUnion {
    i: i32,
    p: *mut c_void,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct GenericValue {
    kind: c_int,
    value: GenericUnion,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct SurfaceAttrib {
    kind: c_int,
    flags: u32,
    value: GenericValue,
}
/// `VASurfaceAttribExternalBuffers`: memory a surface is made of.
#[repr(C)]
struct ExternalBuffers {
    pixel_format: u32,
    width: u32,
    height: u32,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 4],
    offsets: [u32; 4],
    buffers: *mut usize,
    num_buffers: u32,
    flags: u32,
    private_data: *mut c_void,
    reserved: [u32; 4],
}
// The sizes libva 2.x compiles these structures to.
const POINTER: usize = size_of::<usize>();
const _: () = {
    assert!(size_of::<ConfigAttrib>() == 8);
    assert!(size_of::<ImageFormat>() == 48);
    assert!(size_of::<Image>() == 120);
    assert!(size_of::<PictureParameter>() == 48);
    assert!(size_of::<QMatrix>() == 152);
    assert!(size_of::<HuffmanBuffer>() == 436);
    assert!(size_of::<SliceParameter>() == 32);
    assert!(size_of::<PackedHeaderParameter>() == 28);
    assert!(size_of::<CodedSegment>() == 32 + 2 * POINTER);
    assert!(size_of::<GenericValue>() == 2 * POINTER);
    assert!(size_of::<SurfaceAttrib>() == 8 + 2 * POINTER);
    assert!(size_of::<ExternalBuffers>() == if POINTER == 8 { 96 } else { 84 });
};

/// The libva entry points the encoder uses.
pub(crate) struct Api {
    get_display_drm: unsafe extern "C" fn(c_int) -> Display,
    initialize: unsafe extern "C" fn(Display, *mut c_int, *mut c_int) -> Status,
    terminate: unsafe extern "C" fn(Display) -> Status,
    error_str: unsafe extern "C" fn(Status) -> *const c_char,
    vendor_string: unsafe extern "C" fn(Display) -> *const c_char,
    max_entrypoints: unsafe extern "C" fn(Display) -> c_int,
    query_entrypoints: unsafe extern "C" fn(Display, c_int, *mut c_int, *mut c_int) -> Status,
    get_config_attributes:
        unsafe extern "C" fn(Display, c_int, c_int, *mut ConfigAttrib, c_int) -> Status,
    create_config:
        unsafe extern "C" fn(Display, c_int, c_int, *mut ConfigAttrib, c_int, *mut u32) -> Status,
    destroy_config: unsafe extern "C" fn(Display, u32) -> Status,
    query_surface_attributes:
        unsafe extern "C" fn(Display, u32, *mut SurfaceAttrib, *mut c_uint) -> Status,
    create_surfaces: unsafe extern "C" fn(
        Display,
        c_uint,
        c_uint,
        c_uint,
        *mut u32,
        c_uint,
        *mut SurfaceAttrib,
        c_uint,
    ) -> Status,
    destroy_surfaces: unsafe extern "C" fn(Display, *mut u32, c_int) -> Status,
    create_context: unsafe extern "C" fn(
        Display,
        u32,
        c_int,
        c_int,
        c_int,
        *mut u32,
        c_int,
        *mut u32,
    ) -> Status,
    destroy_context: unsafe extern "C" fn(Display, u32) -> Status,
    create_buffer:
        unsafe extern "C" fn(Display, u32, c_int, c_uint, c_uint, *mut c_void, *mut u32) -> Status,
    destroy_buffer: unsafe extern "C" fn(Display, u32) -> Status,
    map_buffer: unsafe extern "C" fn(Display, u32, *mut *mut c_void) -> Status,
    unmap_buffer: unsafe extern "C" fn(Display, u32) -> Status,
    derive_image: unsafe extern "C" fn(Display, u32, *mut Image) -> Status,
    max_image_formats: unsafe extern "C" fn(Display) -> c_int,
    query_image_formats: unsafe extern "C" fn(Display, *mut ImageFormat, *mut c_int) -> Status,
    create_image:
        unsafe extern "C" fn(Display, *mut ImageFormat, c_int, c_int, *mut Image) -> Status,
    put_image: unsafe extern "C" fn(
        Display,
        u32,
        u32,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_int,
        c_int,
        c_uint,
        c_uint,
    ) -> Status,
    destroy_image: unsafe extern "C" fn(Display, u32) -> Status,
    begin_picture: unsafe extern "C" fn(Display, u32, u32) -> Status,
    render_picture: unsafe extern "C" fn(Display, u32, *mut u32, c_int) -> Status,
    end_picture: unsafe extern "C" fn(Display, u32) -> Status,
    sync_surface: unsafe extern "C" fn(Display, u32) -> Status,
    set_error_callback: SetCallback,
    set_info_callback: SetCallback,
    /// What libva has said of the displays opened with these entry points
    /// since the last call was checked.
    messages: Box<Messages>,
    /// Kept loaded while the entry points are used.
    _libraries: Option<[Library; 2]>,
}

/// libva's error messages, the last few. libva would print them on
/// stderr, which the worker passes on to the user: under `auto`, a host
/// with a render node whose driver does not load would print them with
/// every capture. They go in the error of the call that failed instead,
/// which `doctor` and a named engine's failure show.
#[derive(Default)]
struct Messages(Mutex<VecDeque<String>>);

impl Messages {
    const KEPT: usize = 8;
    /// What was said since the last call, and no longer kept.
    fn take(&self) -> Vec<String> {
        let mut kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        kept.drain(..).collect()
    }
}

/// Keep one of libva's error messages; `context` is the `Messages` of the
/// display's entry points.
unsafe extern "C" fn keep_message(context: *mut c_void, message: *const c_char) {
    if context.is_null() || message.is_null() {
        return;
    }
    // SAFETY: the context is the `Messages` boxed in the display's `Api`,
    // which outlives the display (`Va` terminates it first).
    let messages = unsafe { &*context.cast::<Messages>() };
    // SAFETY: libva passes a NUL-terminated message.
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    let text: String = text.trim().chars().take(300).collect();
    let mut kept = messages.0.lock().unwrap_or_else(PoisonError::into_inner);
    if kept.len() == Messages::KEPT {
        kept.pop_front();
    }
    kept.push_back(text);
}

/// Drop one of libva's informational messages (its version, the driver it
/// tries).
const unsafe extern "C" fn drop_message(_: *mut c_void, _: *const c_char) {}

impl Api {
    fn load() -> Result<Self> {
        let (va, drm) = match (Library::open("libva.so.2"), Library::open("libva-drm.so.2")) {
            (Ok(va), Ok(drm)) => (va, drm),
            (Err(error), _) | (_, Err(error)) => bail!("libva was not found ({error:#})"),
        };
        Ok(Self {
            // SAFETY: the field type matches vaGetDisplayDRM in the libva 2 C API.
            get_display_drm: unsafe { drm.symbol(c"vaGetDisplayDRM") }?,
            // SAFETY: the field type matches vaInitialize in the libva 2 C API.
            initialize: unsafe { va.symbol(c"vaInitialize") }?,
            // SAFETY: the field type matches vaTerminate in the libva 2 C API.
            terminate: unsafe { va.symbol(c"vaTerminate") }?,
            // SAFETY: the field type matches vaErrorStr in the libva 2 C API.
            error_str: unsafe { va.symbol(c"vaErrorStr") }?,
            // SAFETY: the field type matches vaQueryVendorString in the libva 2 C API.
            vendor_string: unsafe { va.symbol(c"vaQueryVendorString") }?,
            // SAFETY: the field type matches vaMaxNumEntrypoints in the libva 2 C API.
            max_entrypoints: unsafe { va.symbol(c"vaMaxNumEntrypoints") }?,
            // SAFETY: the field type matches vaQueryConfigEntrypoints in the libva 2 C API.
            query_entrypoints: unsafe { va.symbol(c"vaQueryConfigEntrypoints") }?,
            // SAFETY: the field type matches vaGetConfigAttributes in the libva 2 C API.
            get_config_attributes: unsafe { va.symbol(c"vaGetConfigAttributes") }?,
            // SAFETY: the field type matches vaCreateConfig in the libva 2 C API.
            create_config: unsafe { va.symbol(c"vaCreateConfig") }?,
            // SAFETY: the field type matches vaDestroyConfig in the libva 2 C API.
            destroy_config: unsafe { va.symbol(c"vaDestroyConfig") }?,
            // SAFETY: the field type matches vaQuerySurfaceAttributes in the libva 2 C API.
            query_surface_attributes: unsafe { va.symbol(c"vaQuerySurfaceAttributes") }?,
            // SAFETY: the field type matches vaCreateSurfaces in the libva 2 C API.
            create_surfaces: unsafe { va.symbol(c"vaCreateSurfaces") }?,
            // SAFETY: the field type matches vaDestroySurfaces in the libva 2 C API.
            destroy_surfaces: unsafe { va.symbol(c"vaDestroySurfaces") }?,
            // SAFETY: the field type matches vaCreateContext in the libva 2 C API.
            create_context: unsafe { va.symbol(c"vaCreateContext") }?,
            // SAFETY: the field type matches vaDestroyContext in the libva 2 C API.
            destroy_context: unsafe { va.symbol(c"vaDestroyContext") }?,
            // SAFETY: the field type matches vaCreateBuffer in the libva 2 C API.
            create_buffer: unsafe { va.symbol(c"vaCreateBuffer") }?,
            // SAFETY: the field type matches vaDestroyBuffer in the libva 2 C API.
            destroy_buffer: unsafe { va.symbol(c"vaDestroyBuffer") }?,
            // SAFETY: the field type matches vaMapBuffer in the libva 2 C API.
            map_buffer: unsafe { va.symbol(c"vaMapBuffer") }?,
            // SAFETY: the field type matches vaUnmapBuffer in the libva 2 C API.
            unmap_buffer: unsafe { va.symbol(c"vaUnmapBuffer") }?,
            // SAFETY: the field type matches vaDeriveImage in the libva 2 C API.
            derive_image: unsafe { va.symbol(c"vaDeriveImage") }?,
            // SAFETY: the field type matches vaMaxNumImageFormats in the libva 2 C API.
            max_image_formats: unsafe { va.symbol(c"vaMaxNumImageFormats") }?,
            // SAFETY: the field type matches vaQueryImageFormats in the libva 2 C API.
            query_image_formats: unsafe { va.symbol(c"vaQueryImageFormats") }?,
            // SAFETY: the field type matches vaCreateImage in the libva 2 C API.
            create_image: unsafe { va.symbol(c"vaCreateImage") }?,
            // SAFETY: the field type matches vaPutImage in the libva 2 C API.
            put_image: unsafe { va.symbol(c"vaPutImage") }?,
            // SAFETY: the field type matches vaDestroyImage in the libva 2 C API.
            destroy_image: unsafe { va.symbol(c"vaDestroyImage") }?,
            // SAFETY: the field type matches vaBeginPicture in the libva 2 C API.
            begin_picture: unsafe { va.symbol(c"vaBeginPicture") }?,
            // SAFETY: the field type matches vaRenderPicture in the libva 2 C API.
            render_picture: unsafe { va.symbol(c"vaRenderPicture") }?,
            // SAFETY: the field type matches vaEndPicture in the libva 2 C API.
            end_picture: unsafe { va.symbol(c"vaEndPicture") }?,
            // SAFETY: the field type matches vaSyncSurface in the libva 2 C API.
            sync_surface: unsafe { va.symbol(c"vaSyncSurface") }?,
            // SAFETY: the field type matches vaSetErrorCallback in the libva 2 C API.
            set_error_callback: unsafe { va.symbol(c"vaSetErrorCallback") }?,
            // SAFETY: the field type matches vaSetInfoCallback in the libva 2 C API.
            set_info_callback: unsafe { va.symbol(c"vaSetInfoCallback") }?,
            messages: Box::default(),
            _libraries: Some([va, drm]),
        })
    }

    /// Send libva's messages about `display` to `messages`, not stderr.
    fn listen(&self, display: Display) {
        let context = std::ptr::from_ref(self.messages.as_ref());
        // SAFETY: a display of this libva, before anything else is done
        // with it; `messages` lives as long as these entry points, which
        // outlive the display.
        unsafe {
            (self.set_error_callback)(display, Some(keep_message), context.cast_mut().cast())
        };
        // SAFETY: the same live display installs a callback that ignores its context.
        unsafe { (self.set_info_callback)(display, Some(drop_message), std::ptr::null_mut()) };
    }

    /// A VA status as an error naming the call, with what libva said
    /// meanwhile.
    fn check(&self, what: &str, status: Status) -> Result<()> {
        let said = self.messages.take();
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
        let said = if said.is_empty() {
            String::new()
        } else {
            format!("; libva: {}", said.join("; "))
        };
        bail!("{what} failed: {text} (status {status}){said}")
    }
}

// ---------------------------------------------------------------------------
// The encoder.

/// How a driver applies the quantization tables it is given.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Variant {
    /// The picture's `quality` field is the frame's, for drivers that
    /// ignore supplied tables and scale their own Annex K tables; else 50,
    /// which leaves supplied tables as they are in drivers that scale them.
    scaled: bool,
    /// The matrix buffer in natural rather than zigzag order.
    natural: bool,
}
const VARIANTS: [Variant; 3] = [
    Variant {
        scaled: false,
        natural: false,
    },
    Variant {
        scaled: false,
        natural: true,
    },
    Variant {
        scaled: true,
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

/// A segment's frame, imported as a surface.
struct Imported {
    surface: u32,
    width: u32,
    height: u32,
}

struct Va {
    api: Api,
    display: Display,
    /// The open render node; the display uses its descriptor.
    _device: Option<std::fs::File>,
    vendor: String,
    /// Configurations for 4:2:0 color and, when the driver has it, gray.
    color: u32,
    gray: Option<u32>,
    /// The driver accepts application-written headers.
    packed_headers: bool,
    max_width: u32,
    max_height: u32,
    variant: Variant,
    session: Option<Session>,
    /// Gray frames' segments imported as surfaces, by segment id.
    imported: HashMap<u64, Imported>,
    /// The driver takes user memory as gray surfaces.
    userptr: bool,
}

impl Va {
    /// Initialize one render node with JPEG encoding.
    fn open_device(api: Api, path: &std::path::Path) -> Result<Self> {
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .context("cannot open")?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&device);
        // SAFETY: a valid DRM descriptor that outlives the display.
        let display = unsafe { (api.get_display_drm)(fd) };
        ensure!(!display.is_null(), "no VA display");
        Self::on_display(api, display, Some(device))
    }

    /// The encoder on an opened display, which Drop terminates.
    fn on_display(api: Api, display: Display, device: Option<std::fs::File>) -> Result<Self> {
        let mut va = Self {
            api,
            display,
            _device: device,
            vendor: String::new(),
            color: INVALID_ID,
            gray: None,
            packed_headers: false,
            max_width: 16384,
            max_height: 16384,
            variant: VARIANTS[0],
            session: None,
            imported: HashMap::new(),
            userptr: false,
        };
        va.api.listen(display);
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a fresh display and valid out-pointers.
        let status = unsafe { (va.api.initialize)(display, &raw mut major, &raw mut minor) };
        va.api.check("vaInitialize", status)?;
        va.configure()?;
        // SAFETY: an initialized display; the string is libva's.
        let vendor = unsafe { (va.api.vendor_string)(display) };
        let vendor = if vendor.is_null() {
            "VA-API".to_owned()
        } else {
            // SAFETY: non-null and NUL-terminated.
            unsafe { CStr::from_ptr(vendor) }
                .to_string_lossy()
                .trim()
                .to_owned()
        };
        va.vendor = format!("{vendor} (VA-API {major}.{minor})")
            .chars()
            .take(300)
            .collect();
        Ok(va)
    }

    fn about(&self, path: &std::path::Path) -> serde_json::Map<String, serde_json::Value> {
        let upload = if self.userptr { "in-place" } else { "copy" };
        serde_json::json!({
            "driver": self.vendor,
            "device": path,
            "upload": upload,
        })
        .as_object()
        .cloned()
        .unwrap_or_default()
    }

    /// Check for baseline JPEG encoding and create its configurations.
    fn configure(&mut self) -> Result<()> {
        let (api, display) = (&self.api, self.display);
        // SAFETY: the display is initialized and owned by this encoder.
        let capacity = unsafe { (api.max_entrypoints)(display) };
        let capacity =
            usize::try_from(capacity.max(1)).context("invalid VA-API entrypoint capacity")?;
        let mut entrypoints = vec![0; capacity];
        let mut count = 0;
        api.check(
            "vaQueryConfigEntrypoints",
            // SAFETY: the list has the advertised capacity and count is writable.
            unsafe {
                (api.query_entrypoints)(
                    display,
                    PROFILE_JPEG_BASELINE,
                    entrypoints.as_mut_ptr(),
                    &raw mut count,
                )
            },
        )
        .context("no baseline JPEG profile")?;
        ensure!(
            entrypoints[..usize::try_from(count.max(0))
                .context("invalid VA-API entrypoint count")?
                .min(entrypoints.len())]
                .contains(&ENTRYPOINT_ENC_PICTURE),
            "the driver decodes JPEG but cannot encode it"
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
            // SAFETY: the initialized attribute array and its exact length are passed.
            unsafe {
                (api.get_config_attributes)(
                    display,
                    PROFILE_JPEG_BASELINE,
                    ENTRYPOINT_ENC_PICTURE,
                    attributes.as_mut_ptr(),
                    c_int::try_from(attributes.len()).context("too many VA-API attributes")?,
                )
            },
        )?;
        let supported = |a: ConfigAttrib| (a.value != ATTRIB_NOT_SUPPORTED).then_some(a.value);
        let formats = supported(attributes[0]).unwrap_or(RT_FORMAT_YUV420);
        ensure!(
            formats & RT_FORMAT_YUV420 != 0,
            "the driver cannot encode 4:2:0 JPEG"
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
            let mut attributes = [
                ConfigAttrib {
                    kind: ATTRIB_RT_FORMAT,
                    value: format,
                },
                ConfigAttrib {
                    kind: ATTRIB_ENC_PACKED_HEADERS,
                    value: PACKED_HEADER_RAW_DATA,
                },
            ];
            let count = if self.packed_headers { 2 } else { 1 };
            let mut config = INVALID_ID;
            api.check(
                "vaCreateConfig",
                // SAFETY: the initialized prefix has count entries; config is writable.
                unsafe {
                    (api.create_config)(
                        display,
                        PROFILE_JPEG_BASELINE,
                        ENTRYPOINT_ENC_PICTURE,
                        attributes.as_mut_ptr(),
                        count,
                        &raw mut config,
                    )
                },
            )?;
            Ok(config)
        };
        self.color = create(RT_FORMAT_YUV420)?;
        self.gray = (formats & RT_FORMAT_YUV400 != 0)
            .then(|| create(RT_FORMAT_YUV400).ok())
            .flatten();
        self.userptr = self.gray.is_some_and(|gray| self.takes_user_memory(gray));
        Ok(())
    }

    /// Whether surfaces of `config` can be made of user memory.
    fn takes_user_memory(&self, config: u32) -> bool {
        let (api, display) = (&self.api, self.display);
        let mut count: c_uint = 0;
        // SAFETY: this live configuration belongs to display; a null list requests its count.
        let status = unsafe {
            (api.query_surface_attributes)(display, config, std::ptr::null_mut(), &raw mut count)
        };
        if status != SUCCESS || count == 0 {
            return false;
        }
        let Ok(capacity) = usize::try_from(count) else {
            return false;
        };
        let empty = SurfaceAttrib {
            kind: 0,
            flags: 0,
            value: GenericValue {
                kind: 0,
                value: GenericUnion { i: 0 },
            },
        };
        let mut attributes = vec![empty; capacity];
        // SAFETY: the initialized list has the capacity returned by the first query.
        let status = unsafe {
            (api.query_surface_attributes)(display, config, attributes.as_mut_ptr(), &raw mut count)
        };
        if status != SUCCESS {
            return false;
        }
        let Ok(count) = usize::try_from(count) else {
            return false;
        };
        attributes
            .iter()
            .take(count)
            .find(|attribute| attribute.kind == SURFACE_ATTRIB_MEMORY_TYPE)
            .is_some_and(|attribute| {
                attribute.value.kind == GENERIC_VALUE_INTEGER
                    // SAFETY: the value's tag identifies the initialized integer member.
                    && unsafe { attribute.value.value.i } & MEM_TYPE_USER_PTR != 0
            })
    }

    /// Find how this driver applies supplied tables by encoding test frames
    /// and checking them against libjpeg-turbo; also whether gray works.
    #[cfg(feature = "jpeg")]
    fn calibrate(&mut self) -> Result<Variant> {
        use crate::jpeg::QUALITY;
        let (width, height) = (128u32, 96u32);
        let rgb: Vec<u8> = (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                let edge = if (x / 16 + y / 16) % 2 == 0 { 40 } else { 0 };
                [
                    u8::try_from(x * 2).expect("calibration colors fit u8"),
                    u8::try_from(y * 2 + edge).expect("calibration colors fit u8"),
                    u8::try_from(255 - x - y / 2).expect("calibration colors fit u8"),
                ]
            })
            .collect();
        let gray: Vec<u8> = rgb.chunks(3).map(|p| p[1]).collect();
        let frame = |format, data: Vec<u8>| Frame {
            pixel_encoding: None,
            width,
            height,
            pixel_format: format,
            data: data.into(),
            ..Frame::default()
        };
        let (color, gray) = (
            frame(crate::types::RGB8, rgb),
            frame(crate::types::MONO8, gray),
        );
        let mut out = vec![0u8; 1 << 20];
        let mut check = |va: &mut Self, frame: &Frame| -> Result<()> {
            let len = va.encode_frame(frame, None, QUALITY, &mut out)?;
            let cpu = crate::jpeg::encode(frame, QUALITY)?;
            super::backend::accurate(frame, &out[..len], &cpu).map_err(|fault| anyhow!("{fault}"))
        };
        let mut failures = Vec::new();
        for variant in VARIANTS {
            self.variant = variant;
            match check(self, &color) {
                Ok(()) => {
                    if self.gray.is_some() && check(self, &gray).is_err() {
                        // Monochrome frames then stay on the CPU.
                        self.end_session();
                        self.gray = None;
                        self.userptr = false;
                    }
                    self.end_session();
                    return Ok(variant);
                }
                Err(error) => failures.push(format!("{variant:?}: {error:#}")),
            }
        }
        self.end_session();
        bail!(
            "its JPEGs do not decode faithfully ({})",
            failures.join("; ")
        )
    }

    /// Without libjpeg-turbo the driver's output cannot be checked.
    #[cfg(not(feature = "jpeg"))]
    fn calibrate() -> Result<Variant> {
        bail!("checking its JPEGs needs libjpeg-turbo (the jpeg feature)")
    }

    fn end_session(&mut self) {
        if let Some(session) = self.session.take() {
            self.destroy_session(session);
        }
    }

    fn destroy_session(&self, mut session: Session) {
        let api = &self.api;
        if session.coded != INVALID_ID {
            // SAFETY: this buffer was created on this display and is owned by the session.
            unsafe { (api.destroy_buffer)(self.display, session.coded) };
        }
        if session.context != INVALID_ID {
            // SAFETY: the context was created on this display and its output buffer is gone.
            unsafe { (api.destroy_context)(self.display, session.context) };
        }
        if session.surface != INVALID_ID {
            // SAFETY: the sole surface ID belongs to this session and no context uses it now.
            unsafe { (api.destroy_surfaces)(self.display, &raw mut session.surface, 1) };
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
        let coded_size = coded_buffer_size(width, height)?;
        let surface_width = width
            .checked_add(width % 2)
            .context("JPEG surface width overflow")?;
        let surface_height = height
            .checked_add(height % 2)
            .context("JPEG surface height overflow")?;
        let api = &self.api;
        let mut session = Session {
            width,
            height,
            channels,
            surface: INVALID_ID,
            context: INVALID_ID,
            coded: INVALID_ID,
            // Above any baseline JPEG of the frame, which noise can push
            // past the raw size at high quality.
            coded_size: usize::try_from(coded_size)
                .context("coded JPEG buffer exceeds host address size")?,
        };
        let result = {
            api.check(
                "vaCreateSurfaces",
                // SAFETY: a live display and format; the initialized surface ID
                // slot is retained by the session and released even on later failure.
                unsafe {
                    (api.create_surfaces)(
                        self.display,
                        format,
                        surface_width,
                        surface_height,
                        &raw mut session.surface,
                        1,
                        std::ptr::null_mut(),
                        0,
                    )
                },
            )
            .and_then(|()| {
                api.check(
                    "vaCreateContext",
                    // SAFETY: the configuration and newly created surface belong
                    // to this display; the context output slot is initialized.
                    unsafe {
                        (api.create_context)(
                            self.display,
                            config,
                            c_int::try_from(surface_width)
                                .context("JPEG width exceeds encoder range")?,
                            c_int::try_from(surface_height)
                                .context("JPEG height exceeds encoder range")?,
                            PROGRESSIVE,
                            &raw mut session.surface,
                            1,
                            &raw mut session.context,
                        )
                    },
                )
            })
            .and_then(|()| {
                api.check(
                    "vaCreateBuffer",
                    // SAFETY: a live display and context receive a bounded coded
                    // buffer allocation into initialized ID storage.
                    unsafe {
                        (api.create_buffer)(
                            self.display,
                            session.context,
                            BUFFER_ENC_CODED,
                            coded_size,
                            1,
                            std::ptr::null_mut(),
                            &raw mut session.coded,
                        )
                    },
                )
            })
        };
        if result.is_err() {
            self.destroy_session(session);
            return result;
        }
        self.session = Some(session);
        Ok(())
    }

    /// The surface the driver makes of a gray frame's own segment, where it
    /// takes user memory and the rows suit it; None otherwise.
    fn import(&mut self, frame: &Frame, input: &Segment) -> Option<u32> {
        let (width, height) = (frame.width, frame.height);
        if !self.userptr
            || frame.pixel_format != crate::types::MONO8
            || !(width as usize).is_multiple_of(USER_PITCH)
            || frame.data.as_ptr() != input.as_ptr().cast_const()
        {
            return None;
        }
        if let Some(imported) = self.imported.get(&input.id()) {
            if (imported.width, imported.height) == (width, height) {
                return Some(imported.surface);
            }
            self.forget(input.id());
        }
        let mut buffer = input.as_ptr() as usize;
        let mut external = ExternalBuffers {
            pixel_format: FOURCC_Y800,
            width,
            height,
            data_size: width.checked_mul(height)?,
            num_planes: 1,
            pitches: [width, 0, 0, 0],
            offsets: [0; 4],
            buffers: &raw mut buffer,
            num_buffers: 1,
            flags: 0,
            private_data: std::ptr::null_mut(),
            reserved: [0; 4],
        };
        let mut attributes = [
            SurfaceAttrib {
                kind: SURFACE_ATTRIB_MEMORY_TYPE,
                flags: SURFACE_ATTRIB_SETTABLE,
                value: GenericValue {
                    kind: GENERIC_VALUE_INTEGER,
                    value: GenericUnion {
                        i: MEM_TYPE_USER_PTR,
                    },
                },
            },
            SurfaceAttrib {
                kind: SURFACE_ATTRIB_EXTERNAL_BUFFERS,
                flags: SURFACE_ATTRIB_SETTABLE,
                value: GenericValue {
                    kind: GENERIC_VALUE_POINTER,
                    value: GenericUnion {
                        p: (&raw mut external).cast(),
                    },
                },
            },
        ];
        let mut surface = INVALID_ID;
        // SAFETY: the descriptor and the address it names outlive the call;
        // the surface reads the segment, which stays mapped until `forget`
        // destroys the surface (on `detach`, before it is unmapped).
        let status = unsafe {
            (self.api.create_surfaces)(
                self.display,
                RT_FORMAT_YUV400,
                width,
                height,
                &raw mut surface,
                1,
                attributes.as_mut_ptr(),
                c_uint::try_from(attributes.len())
                    .expect("the fixed surface attribute list fits c_uint"),
            )
        };
        if status != SUCCESS {
            // Not for this driver: frames are copied from now on.
            self.userptr = false;
            return None;
        }
        self.imported.insert(
            input.id(),
            Imported {
                surface,
                width,
                height,
            },
        );
        Some(surface)
    }

    /// Destroy the surface made of a segment.
    fn forget(&mut self, id: u64) {
        if let Some(mut imported) = self.imported.remove(&id) {
            // SAFETY: created by `import` on this display.
            unsafe { (self.api.destroy_surfaces)(self.display, &raw mut imported.surface, 1) };
        }
    }

    /// Write the frame into the session's surface as NV12 (color) or Y800
    /// (gray): through an image derived from the surface where the driver
    /// allows, else through an image copied into it.
    fn upload(&self, frame: &Frame) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .context("VA-API encoding session is not initialized")?;
        let api = &self.api;
        let fourcc = if session.channels == 1 {
            FOURCC_Y800
        } else {
            FOURCC_NV12
        };
        let mut image = Image::default();
        // SAFETY: a valid display and surface; the image is destroyed below.
        let derived =
            unsafe { (api.derive_image)(self.display, session.surface, &raw mut image) } == SUCCESS;
        if derived && image.format.fourcc == fourcc {
            let written = self.fill(&image, session, frame);
            // SAFETY: derived above.
            unsafe { (api.destroy_image)(self.display, image.image_id) };
            return written;
        }
        if derived {
            // SAFETY: derived above.
            unsafe { (api.destroy_image)(self.display, image.image_id) };
        }
        self.upload_copy(session, frame, fourcc)
    }

    fn upload_copy(&self, session: &Session, frame: &Frame, fourcc: u32) -> Result<()> {
        let api = &self.api;
        // SAFETY: the initialized display reports its maximum number of image formats.
        let maximum = unsafe { (api.max_image_formats)(self.display) };
        let capacity =
            usize::try_from(maximum.max(1)).context("image format capacity exceeds host range")?;
        let mut format = {
            let mut formats = vec![ImageFormat::default(); capacity];
            let mut count = 0;
            api.check(
                "vaQueryImageFormats",
                // SAFETY: the driver receives space for the maximum it reported,
                // and initialized output storage for the count actually written.
                unsafe {
                    (api.query_image_formats)(self.display, formats.as_mut_ptr(), &raw mut count)
                },
            )?;
            formats
                .into_iter()
                .take(
                    usize::try_from(count.max(0))
                        .context("image format count exceeds host range")?,
                )
                .find(|f| f.fourcc == fourcc)
                .ok_or_else(|| anyhow!("the driver has no image format for the surface"))?
        };
        let (width, height) = (
            session.width.next_multiple_of(2),
            session.height.next_multiple_of(2),
        );
        let mut image = Image::default();
        // SAFETY: a valid display and format; the image is destroyed below.
        api.check("vaCreateImage", unsafe {
            (api.create_image)(
                self.display,
                &raw mut format,
                c_int::try_from(width).context("JPEG width exceeds encoder range")?,
                c_int::try_from(height).context("JPEG height exceeds encoder range")?,
                &raw mut image,
            )
        })?;
        let result = self.fill(&image, session, frame).and_then(|()| {
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

    /// Map an image's buffer and write the frame at its plane pitches, an
    /// odd last column and row repeated into the surface's padding.
    fn fill(&self, image: &Image, session: &Session, frame: &Frame) -> Result<()> {
        let api = &self.api;
        let (width, height) = (session.width as usize, session.height as usize);
        let (padded_width, padded_height) = (width.next_multiple_of(2), height.next_multiple_of(2));
        let view = crate::frame::FrameRef::whole(frame);
        let mut base = std::ptr::null_mut();
        // SAFETY: the live image buffer writes its mapping into initialized pointer storage.
        api.check("vaMapBuffer", unsafe {
            (api.map_buffer)(self.display, image.buf, &raw mut base)
        })?;
        let result = (|| -> Result<()> {
            let size = image.data_size as usize;
            ensure!(
                !base.is_null() && isize::try_from(size).is_ok(),
                "an invalid image mapping"
            );
            // SAFETY: the successful mapping provides this checked extent and remains
            // mapped until all plane slices are out of scope below.
            let data = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), size) };
            let luma_pitch = image.pitches[0] as usize;
            let luma_start = image.offsets[0] as usize;
            let luma_end = padded_height
                .checked_sub(1)
                .and_then(|rows| rows.checked_mul(luma_pitch))
                .and_then(|bytes| bytes.checked_add(padded_width))
                .and_then(|bytes| bytes.checked_add(luma_start))
                .context("an unexpected surface layout")?;
            ensure!(
                luma_pitch >= padded_width && data.len() >= luma_end,
                "an unexpected surface layout"
            );
            if session.channels == 1 {
                let luma = &mut data[luma_start..];
                let mut reader = crate::frame::RowReader::new(view);
                for y in 0..height {
                    let row = &mut luma[y * luma_pitch..][..width];
                    reader.read(y, crate::frame::Channels::Gray, row);
                }
                let site = if frame.pixel_format == crate::types::MONO8 {
                    crate::copies::Site::GpuUpload
                } else {
                    crate::copies::Site::StillTransform
                };
                crate::copies::record(site, width * height);
                pad(luma, luma_pitch, width, height);
                return Ok(());
            }
            ensure!(image.num_planes >= 2, "an unexpected surface layout");
            let chroma_start = image.offsets[1] as usize;
            ensure!(
                chroma_start >= luma_end && chroma_start <= data.len(),
                "an unexpected surface layout"
            );
            let (head, chroma) = data.split_at_mut(chroma_start);
            let luma = &mut head[luma_start..];
            let chroma_pitch = image.pitches[1] as usize;
            let (chroma_width, chroma_rows) = (width.div_ceil(2), height.div_ceil(2));
            let chroma_end = chroma_rows
                .checked_sub(1)
                .and_then(|rows| rows.checked_mul(chroma_pitch))
                .and_then(|bytes| bytes.checked_add(chroma_width * 2))
                .context("an unexpected surface layout")?;
            ensure!(
                chroma_pitch >= chroma_width * 2 && chroma.len() >= chroma_end,
                "an unexpected surface layout"
            );
            crate::frame::ycbcr420(
                view,
                crate::frame::Yuv420 {
                    luma: &mut *luma,
                    luma_stride: luma_pitch,
                    chroma: crate::frame::Chroma::Interleaved {
                        cbcr: chroma,
                        stride: chroma_pitch,
                    },
                },
            );
            crate::copies::record(
                crate::copies::Site::StillTransform,
                width * height + 2 * chroma_width * chroma_rows,
            );
            pad(luma, luma_pitch, width, height);
            Ok(())
        })();
        // SAFETY: every borrowed plane slice is out of scope; release the successful mapping.
        let status = unsafe { (api.unmap_buffer)(self.display, image.buf) };
        let unmapped = api.check("vaUnmapBuffer", status);
        result.and(unmapped)
    }

    /// Encode `frame` into `out`, from its own segment where the driver
    /// takes it as a surface, else written into the session's; the JPEG's
    /// length.
    fn encode_frame(
        &mut self,
        frame: &Frame,
        input: Option<&Segment>,
        quality: i32,
        out: &mut [u8],
    ) -> Result<usize> {
        crate::frame::validate_source(frame)?;
        let (width, height) = (frame.width, frame.height);
        if !(16..=self.max_width).contains(&width) || !(16..=self.max_height).contains(&height) {
            return Err(
                Unsupported(format!("{width}x{height} is outside the encoder's range")).into(),
            );
        }
        let channels = if crate::frame::monochrome(frame.pixel_format) {
            1
        } else {
            3
        };
        self.begin_session(width, height, channels)?;
        if let Some(input) = input
            && let Some(surface) = self.import(frame, input)
        {
            if let Ok(len) =
                self.encode_surface(surface, width, height, channels == 3, quality, out)
            {
                return Ok(len);
            }
            // The driver made the surface but cannot encode from
            // it: frames are copied from now on.
            self.forget(input.id());
            self.userptr = false;
        }
        self.upload(frame)?;
        let surface = self
            .session
            .as_ref()
            .context("VA-API encoding session is not initialized")?
            .surface;
        self.encode_surface(surface, width, height, channels == 3, quality, out)
    }

    /// Encode `surface` into `out`, returning the JPEG length.
    fn encode_surface(
        &self,
        surface: u32,
        width: u32,
        height: u32,
        color: bool,
        quality: i32,
        out: &mut [u8],
    ) -> Result<usize> {
        let session = self
            .session
            .as_ref()
            .context("VA-API encoding session is not initialized")?;
        let components = if color { 3u16 } else { 1 };
        let channels = usize::from(components);
        let [luma, chroma] = tables(quality);
        let natural = self.variant.natural;
        let order = |table: [u8; 64]| -> [u8; 64] {
            if natural {
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
            reconstructed_picture: surface,
            picture_width: u16::try_from(width).context("JPEG width exceeds u16")?,
            picture_height: u16::try_from(height).context("JPEG height exceeds u16")?,
            coded_buf: session.coded,
            pic_flags: PICTURE_HUFFMAN,
            sample_bit_depth: 8,
            num_scan: 1,
            num_components: components,
            component_id: [1, 2, 3, 0],
            quantiser_table_selector: [0, 1, 1, 0],
            quality: if self.variant.scaled {
                u8::try_from(quality.clamp(1, 100)).context("JPEG quality exceeds u8")?
            } else {
                50
            },
            reserved: [0; 4],
        };
        let mut matrix = QMatrix {
            load_lum_quantiser_matrix: 1,
            load_chroma_quantiser_matrix: i32::from(color),
            lum_quantiser_matrix: order(luma),
            chroma_quantiser_matrix: order(chroma),
            reserved: [0; 4],
        };
        let mut huffman_tables = HuffmanBuffer {
            load_huffman_table: [1, u8::from(color)],
            huffman_table: [
                huffman(&DC_LUMINANCE, &AC_LUMINANCE, &AC_LUMINANCE_VALUES),
                huffman(&DC_CHROMINANCE, &AC_CHROMINANCE, &AC_CHROMINANCE_VALUES),
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
            num_components: components,
            components: [
                component(1, 0),
                component(2, 1),
                component(3, 1),
                component(0, 0),
            ],
            reserved: [0; 4],
        };
        let mut header = header(
            u16::try_from(width).context("JPEG width exceeds u16")?,
            u16::try_from(height).context("JPEG height exceeds u16")?,
            channels,
            quality,
        );
        let mut packed = PackedHeaderParameter {
            kind: PACKED_HEADER_TYPE_RAW_DATA,
            bit_length: u32::try_from(header.len())
                .ok()
                .and_then(|length| length.checked_mul(8))
                .context("JPEG header exceeds the packed-header size limit")?,
            has_emulation_bytes: 0,
            reserved: [0; 4],
        };
        let api = &self.api;
        let (display, context, coded, coded_size) = (
            self.display,
            session.context,
            session.coded,
            session.coded_size,
        );
        let mut buffers = [INVALID_ID; 6];
        let mut buffer_count = 0;
        let result = {
            let create = |kind: c_int, size: usize, data: *mut c_void| -> Result<u32> {
                let mut id = INVALID_ID;
                api.check(
                    "vaCreateBuffer",
                    // SAFETY: all calls below supply a live parameter value with
                    // the size and type this buffer kind expects; IDs are destroyed below.
                    unsafe {
                        (api.create_buffer)(
                            display,
                            context,
                            kind,
                            c_uint::try_from(size)
                                .context("VA-API parameter buffer exceeds c_uint")?,
                            1,
                            data,
                            &raw mut id,
                        )
                    },
                )?;
                Ok(id)
            };
            (|| -> Result<usize> {
                let mut push = |id| {
                    buffers[buffer_count] = id;
                    buffer_count += 1;
                };
                push(create(
                    BUFFER_ENC_PICTURE,
                    size_of::<PictureParameter>(),
                    (&raw mut picture).cast(),
                )?);
                push(create(
                    BUFFER_Q_MATRIX,
                    size_of::<QMatrix>(),
                    (&raw mut matrix).cast(),
                )?);
                push(create(
                    BUFFER_HUFFMAN_TABLE,
                    size_of::<HuffmanBuffer>(),
                    (&raw mut huffman_tables).cast(),
                )?);
                if self.packed_headers {
                    push(create(
                        BUFFER_ENC_PACKED_HEADER_PARAMETER,
                        size_of::<PackedHeaderParameter>(),
                        (&raw mut packed).cast(),
                    )?);
                    push(create(
                        BUFFER_ENC_PACKED_HEADER_DATA,
                        header.len(),
                        header.as_mut_ptr().cast(),
                    )?);
                }
                push(create(
                    BUFFER_ENC_SLICE,
                    size_of::<SliceParameter>(),
                    (&raw mut slice).cast(),
                )?);
                api.check(
                    "vaBeginPicture",
                    // SAFETY: the context and surface belong to this initialized display.
                    unsafe { (api.begin_picture)(display, context, surface) },
                )?;
                let rendered = api.check(
                    "vaRenderPicture",
                    // SAFETY: the initialized prefix contains this picture's live buffers.
                    unsafe {
                        (api.render_picture)(
                            display,
                            context,
                            buffers.as_mut_ptr(),
                            c_int::try_from(buffer_count)
                                .context("too many VA-API picture buffers")?,
                        )
                    },
                );
                // A begun picture is always ended.
                // SAFETY: begin_picture succeeded for this live context above.
                api.check("vaEndPicture", unsafe {
                    (api.end_picture)(display, context)
                })?;
                rendered?;
                // SAFETY: the surface is live and has just been submitted for encoding.
                api.check("vaSyncSurface", unsafe {
                    (api.sync_surface)(display, surface)
                })?;
                let mut segment = std::ptr::null_mut();
                api.check(
                    "vaMapBuffer",
                    // SAFETY: the output buffer is live and synchronized; the output pointer
                    // slot remains initialized and mapped until the explicit unmap below.
                    unsafe { (api.map_buffer)(display, coded, &raw mut segment) },
                )?;
                let mut length = 0usize;
                let copied = (|| -> Result<()> {
                    let mut segments = 0;
                    while !segment.is_null() {
                        segments += 1;
                        ensure!(segments <= 64, "too many coded segments");
                        // SAFETY: mapping a coded buffer returns this driver-owned segment
                        // list, whose nodes remain readable until unmap.
                        let s = unsafe { &*segment.cast::<CodedSegment>() };
                        let size = s.size as usize;
                        ensure!(size <= coded_size, "a coded segment larger than its buffer");
                        let end = length
                            .checked_add(size)
                            .filter(|end| *end <= out.len())
                            .ok_or_else(|| {
                                Unsupported("its JPEG is longer than the room for it".into())
                            })?;
                        if size > 0 {
                            ensure!(!s.buf.is_null(), "an empty coded segment");
                            // SAFETY: the live segment reports this bounded payload; its
                            // non-null bytes remain mapped while copied into the checked output.
                            let bytes =
                                unsafe { std::slice::from_raw_parts(s.buf.cast::<u8>(), size) };
                            out[length..end].copy_from_slice(bytes);
                        }
                        length = end;
                        segment = s.next;
                    }
                    Ok(())
                })();
                // SAFETY: all segment references and byte slices are out of scope.
                let unmapped = api.check("vaUnmapBuffer", unsafe {
                    (api.unmap_buffer)(display, coded)
                });
                copied?;
                unmapped?;
                Ok(length)
            })()
        };
        for id in buffers.into_iter().take(buffer_count) {
            // SAFETY: created above on this display.
            unsafe { (api.destroy_buffer)(display, id) };
        }
        let mut length = result?;
        // Drivers differ in whether they write the final EOI marker.
        if length < 2 || out[length - 2..length] != [0xFF, 0xD9] {
            ensure!(
                length + 2 <= out.len(),
                "its JPEG is longer than the room for it"
            );
            out[length..length + 2].copy_from_slice(&[0xFF, 0xD9]);
            length += 2;
        }
        ensure!(
            super::well_formed(&out[..length], width, height),
            "the driver wrote a malformed JPEG"
        );
        Ok(length)
    }

    fn release(&mut self) {
        for (_, mut imported) in self.imported.drain() {
            // SAFETY: each surface was created by import on this display;
            // draining removes its only owner before destroying it once.
            unsafe { (self.api.destroy_surfaces)(self.display, &raw mut imported.surface, 1) };
        }
        self.end_session();
        let api = &self.api;
        if self.color != INVALID_ID {
            // SAFETY: this configuration belongs to this live display and is released once.
            unsafe { (api.destroy_config)(self.display, self.color) };
            self.color = INVALID_ID;
        }
        if let Some(gray) = self.gray.take() {
            // SAFETY: taking the configuration removes its only owner before destruction.
            unsafe { (api.destroy_config)(self.display, gray) };
        }
        if !self.display.is_null() {
            // SAFETY: all resources are released before terminating this owned display once.
            unsafe { (api.terminate)(self.display) };
            self.display = std::ptr::null_mut();
        }
    }
}

/// Repeat an odd last column and row of a plane into its padding, as
/// libjpeg-turbo pads its own input.
fn pad(plane: &mut [u8], pitch: usize, width: usize, height: usize) {
    if !width.is_multiple_of(2) {
        for row in plane.chunks_mut(pitch).take(height) {
            row[width] = row[width - 1];
        }
    }
    if !height.is_multiple_of(2) {
        plane.copy_within(
            (height - 1) * pitch..(height - 1) * pitch + width.next_multiple_of(2),
            height * pitch,
        );
    }
}

impl Encoder for Va {
    fn detach(&mut self, segment: &Segment) {
        self.forget(segment.id());
    }
    fn encode(
        &mut self,
        frame: &Frame,
        input: &Segment,
        quality: i32,
        out: &mut [u8],
    ) -> Result<usize> {
        self.encode_frame(frame, Some(input), quality, out)
    }
}

impl Drop for Va {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(all(test, not(feature = "jpeg")))]
#[test]
fn calibration_without_cpu_jpeg_reports_the_required_feature() {
    assert_eq!(
        Va::calibrate().unwrap_err().to_string(),
        "checking its JPEGs needs libjpeg-turbo (the jpeg feature)"
    );
}

#[cfg(all(test, feature = "jpeg"))]
mod tests {
    //! libva faked by functions that keep surfaces in memory, record what
    //! they are given, and encode a picture with libjpeg-turbo, as a driver
    //! would on its hardware.
    use super::*;

    use crate::{
        framebuf::PoolVec,
        jpeg::QUALITY,
        types::{MONO8, RGB8},
    };
    use std::sync::{Arc, MutexGuard};
    use turbojpeg::{Compressor, Image as TurboImage, PixelFormat, Subsamp};

    #[test]
    fn coded_buffers_fit_the_driver_size_field() {
        assert_eq!(coded_buffer_size(64, 48).unwrap(), (1 << 16) + 4096);
        assert_eq!(
            coded_buffer_size(1024, 1024).unwrap(),
            1024 * 1024 * 3 + 4096
        );
        coded_buffer_size(65_535, 65_535).unwrap_err();
        coded_buffer_size(u32::MAX, u32::MAX).unwrap_err();
    }

    /// A fake surface's memory: its own, or user memory it was made of.
    enum Memory {
        Own(Vec<u8>),
        User(*mut u8),
    }
    // SAFETY: tests hold the segments the pointers name.
    unsafe impl Send for Memory {}

    struct Surface {
        width: u32,
        height: u32,
        gray: bool,
        pitch: usize,
        memory: Memory,
    }

    #[derive(Default)]
    struct Fake {
        userptr: bool,
        invalid_memory_type: bool,
        refuse_user: bool,
        no_derive: bool,
        drop_eoi: bool,
        fail_context: bool,
        fail_coded: bool,
        next: u32,
        surfaces: HashMap<u32, Surface>,
        contexts: Vec<u32>,
        /// Image buffers: the surface derived, else memory of their own.
        images: HashMap<u32, (Option<u32>, Vec<u8>, usize, u32)>,
        coded: HashMap<u32, (Vec<u8>, Box<CodedSegment>)>,
        target: u32,
        /// The size the picture parameters name.
        picture: (usize, usize),
        /// What happened: user memory surfaces made (address, pitch),
        /// images derived, images put.
        imported: Vec<(usize, u32)>,
        derived: usize,
        put: usize,
        encoded_from: Vec<u32>,
        /// What vaInitialize reports to the error callback, and then fails.
        init_errors: Vec<&'static CStr>,
        /// The error and info callbacks, with their contexts.
        error_callback: Option<(MessageCallback, usize)>,
        info_callback: Option<(MessageCallback, usize)>,
    }
    // SAFETY: as for Memory; the coded segments point into their own
    // vectors.
    unsafe impl Send for Fake {}

    static FAKE: Mutex<Option<Fake>> = Mutex::new(None);
    static TURN: Mutex<()> = Mutex::new(());

    fn fake<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
        f(FAKE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(Fake::default))
    }

    const fn id(fake: &mut Fake) -> u32 {
        fake.next += 1;
        fake.next
    }

    const unsafe extern "C" fn get_display_drm(_: c_int) -> Display {
        std::ptr::dangling_mut()
    }
    unsafe extern "C" fn initialize(_: Display, major: *mut c_int, minor: *mut c_int) -> Status {
        let (errors, error, info) = fake(|fake| {
            let errors = std::mem::take(&mut fake.init_errors);
            (errors, fake.error_callback, fake.info_callback)
        });
        if !errors.is_empty() {
            // As libva does when a driver does not load.
            let (info, context) = info.expect("an info callback");
            // SAFETY: The registered callback and context are live, and the message is a static C string.
            unsafe { info(context as *mut c_void, c"VA-API version 1.22.0\n".as_ptr()) };
            let (error, context) = error.expect("an error callback");
            for message in errors {
                // SAFETY: The registered callback and context are live; message remains alive during the call.
                unsafe { error(context as *mut c_void, message.as_ptr()) };
            }
            return -1;
        }
        // SAFETY: the caller supplies writable major-version storage.
        unsafe { *major = 1 };
        // SAFETY: the caller supplies writable minor-version storage.
        unsafe { *minor = 22 };
        SUCCESS
    }
    unsafe extern "C" fn set_error_callback(
        _: Display,
        callback: Option<MessageCallback>,
        context: *mut c_void,
    ) -> Option<MessageCallback> {
        fake(|fake| fake.error_callback = callback.map(|callback| (callback, context as usize)));
        None
    }
    unsafe extern "C" fn set_info_callback(
        _: Display,
        callback: Option<MessageCallback>,
        context: *mut c_void,
    ) -> Option<MessageCallback> {
        fake(|fake| fake.info_callback = callback.map(|callback| (callback, context as usize)));
        None
    }
    const unsafe extern "C" fn terminate(_: Display) -> Status {
        SUCCESS
    }
    const unsafe extern "C" fn error_str(_: Status) -> *const c_char {
        c"fake error".as_ptr()
    }
    const unsafe extern "C" fn vendor_string(_: Display) -> *const c_char {
        c"Fake driver".as_ptr()
    }
    const unsafe extern "C" fn max_entrypoints(_: Display) -> c_int {
        4
    }
    unsafe extern "C" fn query_entrypoints(
        _: Display,
        _: c_int,
        list: *mut c_int,
        count: *mut c_int,
    ) -> Status {
        // SAFETY: the caller allocates the advertised entrypoint capacity.
        unsafe { *list = ENTRYPOINT_ENC_PICTURE };
        // SAFETY: the caller supplies writable count storage.
        unsafe { *count = 1 };
        SUCCESS
    }
    unsafe extern "C" fn get_config_attributes(
        _: Display,
        _: c_int,
        _: c_int,
        list: *mut ConfigAttrib,
        count: c_int,
    ) -> Status {
        // SAFETY: The caller passes count initialized, exclusively borrowed attributes.
        let list = unsafe {
            std::slice::from_raw_parts_mut(
                list,
                usize::try_from(count).expect("nonnegative attribute count"),
            )
        };
        for attribute in list {
            attribute.value = match attribute.kind {
                ATTRIB_RT_FORMAT => RT_FORMAT_YUV420 | RT_FORMAT_YUV400,
                ATTRIB_ENC_PACKED_HEADERS => PACKED_HEADER_RAW_DATA,
                _ => 8192,
            };
        }
        SUCCESS
    }
    unsafe extern "C" fn create_config(
        _: Display,
        _: c_int,
        _: c_int,
        _: *mut ConfigAttrib,
        _: c_int,
        config: *mut u32,
    ) -> Status {
        // SAFETY: The caller supplies writable storage for the configuration ID.
        unsafe { *config = fake(id) };
        SUCCESS
    }
    const unsafe extern "C" fn destroy(_: Display, _: u32) -> Status {
        SUCCESS
    }
    unsafe extern "C" fn query_surface_attributes(
        _: Display,
        _: u32,
        list: *mut SurfaceAttrib,
        count: *mut c_uint,
    ) -> Status {
        let userptr = fake(|fake| fake.userptr);
        let invalid = fake(|fake| fake.invalid_memory_type);
        // SAFETY: the caller supplies writable count storage.
        unsafe { *count = 1 };
        if !list.is_null() {
            // SAFETY: on the second call, the caller allocates one attribute.
            unsafe {
                *list = SurfaceAttrib {
                    kind: SURFACE_ATTRIB_MEMORY_TYPE,
                    flags: 1,
                    value: GenericValue {
                        kind: if invalid {
                            GENERIC_VALUE_POINTER
                        } else {
                            GENERIC_VALUE_INTEGER
                        },
                        value: GenericUnion {
                            i: 1 | if userptr { MEM_TYPE_USER_PTR } else { 0 },
                        },
                    },
                }
            };
        }
        SUCCESS
    }
    unsafe extern "C" fn create_surfaces(
        _: Display,
        format: c_uint,
        width: c_uint,
        height: c_uint,
        surfaces: *mut u32,
        _: c_uint,
        attributes: *mut SurfaceAttrib,
        count: c_uint,
    ) -> Status {
        let gray = format == RT_FORMAT_YUV400;
        let attributes = if attributes.is_null() {
            &[][..]
        } else {
            // SAFETY: The caller supplies count initialized surface attributes for this call.
            unsafe { std::slice::from_raw_parts(attributes, count as usize) }
        };
        let external = attributes
            .iter()
            .find(|attribute| attribute.kind == SURFACE_ATTRIB_EXTERNAL_BUFFERS)
            .map(|attribute| {
                // SAFETY: the external-buffer attribute holds an initialized pointer member.
                let pointer = unsafe { attribute.value.value.p };
                // SAFETY: that member points to a live ExternalBuffers for this call.
                unsafe { &*pointer.cast::<ExternalBuffers>() }
            });
        fake(|fake| {
            let (pitch, memory) = match external {
                Some(_) if fake.refuse_user => return 1,
                Some(external) => {
                    // SAFETY: The encoder supplies a live one-element array of external buffer addresses.
                    let address = unsafe { *external.buffers };
                    fake.imported.push((address, external.pitches[0]));
                    (
                        external.pitches[0] as usize,
                        Memory::User(address as *mut u8),
                    )
                }
                None => {
                    // Rows padded as drivers pad them.
                    let pitch = (width as usize).next_multiple_of(64);
                    let rows = (height as usize).next_multiple_of(2);
                    let size = if gray {
                        pitch * rows
                    } else {
                        pitch * rows * 3 / 2
                    };
                    (pitch, Memory::Own(vec![0; size]))
                }
            };
            let surface = id(fake);
            fake.surfaces.insert(
                surface,
                Surface {
                    width,
                    height,
                    gray,
                    pitch,
                    memory,
                },
            );
            // SAFETY: The caller supplies writable storage for the single surface ID.
            unsafe { *surfaces = surface };
            SUCCESS
        })
    }
    unsafe extern "C" fn destroy_surfaces(_: Display, surfaces: *mut u32, _: c_int) -> Status {
        // SAFETY: The caller passes a live pointer to the single surface ID being destroyed.
        let surface = unsafe { *surfaces };
        fake(|fake| fake.surfaces.remove(&surface));
        SUCCESS
    }
    unsafe extern "C" fn create_context(
        _: Display,
        _: u32,
        _: c_int,
        _: c_int,
        _: c_int,
        _: *mut u32,
        _: c_int,
        context: *mut u32,
    ) -> Status {
        if fake(|fake| fake.fail_context) {
            return 1;
        }
        let id = fake(|fake| {
            let id = id(fake);
            fake.contexts.push(id);
            id
        });
        // SAFETY: The caller supplies writable storage for the context ID.
        unsafe { *context = id };
        SUCCESS
    }
    unsafe extern "C" fn destroy_context(_: Display, context: u32) -> Status {
        fake(|fake| fake.contexts.retain(|&id| id != context));
        SUCCESS
    }
    unsafe extern "C" fn create_buffer(
        _: Display,
        _: u32,
        kind: c_int,
        _: c_uint,
        _: c_uint,
        data: *mut c_void,
        buffer: *mut u32,
    ) -> Status {
        if kind == BUFFER_ENC_CODED && fake(|fake| fake.fail_coded) {
            return 1;
        }
        fake(|fake| {
            let id = id(fake);
            if kind == BUFFER_ENC_PICTURE {
                // SAFETY: For BUFFER_ENC_PICTURE the caller passes an initialized PictureParameter.
                let picture = unsafe { &*data.cast::<PictureParameter>() };
                fake.picture = (picture.picture_width.into(), picture.picture_height.into());
            }
            if kind == BUFFER_ENC_CODED {
                let segment = Box::new(CodedSegment {
                    size: 0,
                    bit_offset: 0,
                    status: 0,
                    reserved: 0,
                    buf: std::ptr::null_mut(),
                    next: std::ptr::null_mut(),
                    va_reserved: [0; 4],
                });
                fake.coded.insert(id, (Vec::new(), segment));
            }
            // SAFETY: The caller supplies writable storage for the buffer ID.
            unsafe { *buffer = id };
        });
        SUCCESS
    }
    unsafe extern "C" fn destroy_buffer(_: Display, buffer: u32) -> Status {
        fake(|fake| fake.coded.remove(&buffer));
        SUCCESS
    }
    unsafe extern "C" fn map_buffer(_: Display, buffer: u32, out: *mut *mut c_void) -> Status {
        fake(|fake| {
            let address: *mut c_void = if let Some((bytes, segment)) = fake.coded.get_mut(&buffer) {
                segment.size = u32::try_from(bytes.len()).unwrap();
                segment.buf = bytes.as_mut_ptr().cast();
                (&raw mut **segment).cast()
            } else {
                let (derived, own, _, _) = fake.images.get_mut(&buffer).expect("an image buffer");
                match derived {
                    Some(surface) => match &mut fake.surfaces.get_mut(surface).unwrap().memory {
                        Memory::Own(bytes) => bytes.as_mut_ptr().cast(),
                        Memory::User(address) => (*address).cast(),
                    },
                    None => own.as_mut_ptr().cast(),
                }
            };
            // SAFETY: The caller supplies writable pointer storage; fake retains ownership of the mapped allocation.
            unsafe { *out = address };
        });
        SUCCESS
    }
    const unsafe extern "C" fn unmap_buffer(_: Display, _: u32) -> Status {
        SUCCESS
    }
    /// An image's fields for a surface's layout.
    fn image_of(surface: &Surface, buffer: u32, image: &mut Image) {
        let rows = surface.height.next_multiple_of(2) as usize;
        *image = Image {
            image_id: buffer,
            buf: buffer,
            width: u16::try_from(surface.width).unwrap(),
            height: u16::try_from(surface.height).unwrap(),
            num_planes: if surface.gray { 1 } else { 2 },
            pitches: [u32::try_from(surface.pitch).unwrap(); 3],
            offsets: [0, u32::try_from(surface.pitch * rows).unwrap(), 0],
            data_size: u32::try_from(surface.pitch * rows * if surface.gray { 2 } else { 3 } / 2)
                .unwrap(),
            ..Image::default()
        };
        image.format.fourcc = if surface.gray {
            FOURCC_Y800
        } else {
            FOURCC_NV12
        };
    }
    unsafe extern "C" fn derive_image(_: Display, surface: u32, image: *mut Image) -> Status {
        fake(|fake| {
            if fake.no_derive {
                return 1;
            }
            let buffer = id(fake);
            let found = &fake.surfaces[&surface];
            // SAFETY: The caller supplies exclusively writable storage for one Image.
            image_of(found, buffer, unsafe { &mut *image });
            fake.images
                .insert(buffer, (Some(surface), Vec::new(), 0, 0));
            fake.derived += 1;
            SUCCESS
        })
    }
    const unsafe extern "C" fn max_image_formats(_: Display) -> c_int {
        2
    }
    unsafe extern "C" fn query_image_formats(
        _: Display,
        list: *mut ImageFormat,
        count: *mut c_int,
    ) -> Status {
        // SAFETY: the caller allocates the advertised capacity of two formats.
        let list = unsafe { &mut *list.cast::<[ImageFormat; 2]>() };
        list[0] = ImageFormat {
            fourcc: FOURCC_NV12,
            ..ImageFormat::default()
        };
        list[1] = ImageFormat {
            fourcc: FOURCC_Y800,
            ..ImageFormat::default()
        };
        // SAFETY: the caller supplies writable count storage.
        unsafe { *count = 2 };
        SUCCESS
    }
    unsafe extern "C" fn create_image(
        _: Display,
        format: *mut ImageFormat,
        width: c_int,
        height: c_int,
        image: *mut Image,
    ) -> Status {
        // SAFETY: The caller supplies a live initialized ImageFormat.
        let gray = unsafe { (*format).fourcc } == FOURCC_Y800;
        fake(|fake| {
            let buffer = id(fake);
            let surface = Surface {
                width: u32::try_from(width).unwrap(),
                height: u32::try_from(height).unwrap(),
                gray,
                pitch: usize::try_from(width)
                    .expect("nonnegative image width")
                    .next_multiple_of(64),
                memory: Memory::Own(Vec::new()),
            };
            // SAFETY: The caller supplies exclusively writable storage for one Image.
            image_of(&surface, buffer, unsafe { &mut *image });
            // SAFETY: image_of initialized this Image above, and the caller keeps it live.
            let size = unsafe { (*image).data_size } as usize;
            fake.images
                .insert(buffer, (None, vec![0; size], surface.pitch, 0));
            SUCCESS
        })
    }
    unsafe extern "C" fn put_image(
        _: Display,
        surface: u32,
        image: u32,
        _: c_int,
        _: c_int,
        _: c_uint,
        _: c_uint,
        _: c_int,
        _: c_int,
        _: c_uint,
        _: c_uint,
    ) -> Status {
        fake(|fake| {
            let (_, bytes, pitch, _) = &fake.images[&image];
            let (bytes, pitch) = (bytes.clone(), *pitch);
            let target = fake.surfaces.get_mut(&surface).unwrap();
            assert_eq!(target.pitch, pitch);
            if let Memory::Own(memory) = &mut target.memory {
                let len = memory.len().min(bytes.len());
                memory[..len].copy_from_slice(&bytes[..len]);
            }
            fake.put += 1;
        });
        SUCCESS
    }
    unsafe extern "C" fn destroy_image(_: Display, image: u32) -> Status {
        fake(|fake| fake.images.remove(&image));
        SUCCESS
    }
    unsafe extern "C" fn begin_picture(_: Display, _: u32, surface: u32) -> Status {
        fake(|fake| fake.target = surface);
        SUCCESS
    }
    const unsafe extern "C" fn render_picture(
        _: Display,
        _: u32,
        buffers: *mut u32,
        count: c_int,
    ) -> Status {
        // The coded buffer is named by the picture parameters; the fake has
        // one, so the newest is it.
        assert!(count >= 0, "nonnegative buffer count");
        let count = count.cast_unsigned() as usize;
        // SAFETY: the caller supplies this nonnegative count of initialized buffer IDs.
        let _ = unsafe { std::slice::from_raw_parts(buffers, count) };
        SUCCESS
    }
    /// Encode the target surface as the hardware would: libjpeg-turbo at
    /// the default quality, from the surface's Y800 or NV12 memory.
    unsafe extern "C" fn end_picture(_: Display, _: u32) -> Status {
        fake(|fake| {
            let target = fake.target;
            let surface = &fake.surfaces[&target];
            let ((width, height), pitch) = (fake.picture, surface.pitch);
            let rows = height.next_multiple_of(2);
            let memory = match &surface.memory {
                Memory::Own(bytes) => bytes.as_ptr(),
                Memory::User(address) => *address,
            };
            let mut compressor = Compressor::new().unwrap();
            compressor.set_quality(QUALITY).unwrap();
            let jpeg = if surface.gray {
                // SAFETY: The target surface retains its owned or imported storage for pitch times height bytes.
                let pixels = unsafe { std::slice::from_raw_parts(memory, pitch * height) };
                compressor.set_subsamp(Subsamp::Gray).unwrap();
                compressor
                    .compress_to_vec(TurboImage {
                        pixels,
                        width,
                        pitch,
                        height,
                        format: PixelFormat::GRAY,
                    })
                    .unwrap()
            } else {
                // SAFETY: The target surface retains its padded luma storage for pitch times rows bytes.
                let luma = unsafe { std::slice::from_raw_parts(memory, pitch * rows) };
                // SAFETY: the chroma plane follows luma within the NV12 allocation.
                let chroma = unsafe { memory.add(pitch * rows) };
                // SAFETY: that plane has pitch times half the padded row count bytes.
                let chroma = unsafe { std::slice::from_raw_parts(chroma, pitch * rows / 2) };
                let (cw, ch) = (width.div_ceil(2), rows / 2);
                let mut cb = vec![0; cw * ch];
                let mut cr = vec![0; cw * ch];
                for y in 0..ch {
                    for x in 0..cw {
                        cb[y * cw + x] = chroma[y * pitch + 2 * x];
                        cr[y * cw + x] = chroma[y * pitch + 2 * x + 1];
                    }
                }
                compressor
                    .compress_yuv_planes_to_owned(&turbojpeg::YuvPlanesImage {
                        y_plane: luma,
                        u_plane: &cb,
                        v_plane: &cr,
                        width,
                        height,
                        y_stride: pitch,
                        u_stride: cw,
                        v_stride: cw,
                        subsamp: Subsamp::Sub2x2,
                    })
                    .unwrap()
                    .to_vec()
            };
            let mut jpeg = jpeg;
            if fake.drop_eoi {
                jpeg.truncate(jpeg.len() - 2);
            }
            fake.encoded_from.push(target);
            let coded = fake.coded.values_mut().next().expect("a coded buffer");
            coded.0 = jpeg;
        });
        SUCCESS
    }
    const unsafe extern "C" fn sync_surface(_: Display, _: u32) -> Status {
        SUCCESS
    }

    fn api() -> Api {
        Api {
            get_display_drm,
            initialize,
            terminate,
            error_str,
            vendor_string,
            max_entrypoints,
            query_entrypoints,
            get_config_attributes,
            create_config,
            destroy_config: destroy,
            query_surface_attributes,
            create_surfaces,
            destroy_surfaces,
            create_context,
            destroy_context,
            create_buffer,
            destroy_buffer,
            map_buffer,
            unmap_buffer,
            derive_image,
            max_image_formats,
            query_image_formats,
            create_image,
            put_image,
            destroy_image,
            begin_picture,
            render_picture,
            end_picture,
            sync_surface,
            set_error_callback,
            set_info_callback,
            messages: Box::default(),
            _libraries: None,
        }
    }

    /// A driver on the fake, calibrated; the driver is dropped before the
    /// turn ends.
    fn va(setup: impl FnOnce(&mut Fake)) -> (MutexGuard<'static, ()>, Va) {
        let turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
        let mut state = Fake::default();
        setup(&mut state);
        *FAKE.lock().unwrap_or_else(PoisonError::into_inner) = Some(state);
        let mut va = Va::on_display(api(), std::ptr::dangling_mut(), None).unwrap();
        va.calibrate().unwrap();
        fake(|fake| {
            fake.imported.clear();
            (fake.derived, fake.put) = (0, 0);
            fake.encoded_from.clear();
        });
        (turn, va)
    }

    /// A smooth frame in a shared memory segment, as the camera wrote it: a
    /// JPEG of noise can be a level off on average from any encoder.
    fn in_segment(format: u32, width: u32, height: u32) -> (Frame, Arc<Segment>) {
        let source = crate::frame::noise_frame(format, width, height);
        let segment = Arc::new(Segment::create(source.data.len()).unwrap());
        let mut data = PoolVec::mapped(segment.clone(), source.data.len());
        for (i, value) in data.iter_mut().enumerate() {
            *value = u8::try_from((i * 3) / 7 % 256).unwrap();
        }
        (
            Frame {
                data: data.freeze(),
                ..source
            },
            segment,
        )
    }

    fn encode(va: &mut Va, frame: &Frame, segment: &Segment) -> Vec<u8> {
        let mut out = vec![0; crate::jpeg::helper::jpeg_bound(frame)];
        let len = va.encode(frame, segment, QUALITY, &mut out).unwrap();
        out.truncate(len);
        out
    }

    #[test]
    fn noninteger_memory_attributes_do_not_enable_user_memory() {
        let (_turn, va) = va(|fake| {
            fake.userptr = true;
            fake.invalid_memory_type = true;
        });
        assert!(!va.userptr);
    }

    /// A gray frame of whole 64-byte rows is the surface: the driver reads
    /// the segment the camera wrote, and nothing is copied.
    #[test]
    fn gray_frames_are_encoded_from_their_own_memory() {
        let (_turn, mut va) = va(|fake| fake.userptr = true);
        assert!(va.userptr);
        let (frame, segment) = in_segment(MONO8, 128, 48);
        for _ in 0..2 {
            let jpeg = encode(&mut va, &frame, &segment);
            assert_eq!(jpeg, crate::jpeg::encode(&frame, QUALITY).unwrap());
        }
        fake(|fake| {
            assert_eq!(
                fake.imported,
                [(segment.as_ptr() as usize, 128)],
                "imported once"
            );
            assert_eq!((fake.derived, fake.put), (0, 0), "nothing copied");
        });
        // Its surface goes when the helper unmaps the segment.
        va.detach(&segment);
        assert!(va.imported.is_empty());
    }

    /// Other frames are written into a surface once: gray rows copied
    /// where the driver takes no user memory or the rows do not suit it,
    /// color converted to NV12 from its rows.
    #[test]
    fn other_frames_are_written_into_a_surface_once() {
        let (_turn, mut va) = va(|fake| fake.userptr = false);
        for (format, width, height) in [
            (MONO8, 128, 48),
            (MONO8, 100, 37),
            (RGB8, 96, 64),
            (0x0108_0009, 97, 61),
            (0x010c_0059, 64, 48),
            (0x0110_0005, 33, 17),
        ] {
            let (frame, segment) = in_segment(format, width, height);
            let derived = fake(|fake| fake.derived);
            let jpeg = encode(&mut va, &frame, &segment);
            assert_eq!(fake(|fake| fake.derived), derived + 1, "{format:08x}");
            let cpu = crate::jpeg::encode(&frame, QUALITY).unwrap();
            super::super::backend::accurate(&frame, &jpeg, &cpu)
                .unwrap_or_else(|fault| panic!("{format:08x} {width}x{height}: {fault}"));
        }
        assert!(fake(|fake| fake.imported.is_empty()));
    }

    /// A driver that refuses user memory, or cannot derive images, still
    /// encodes: by copying.
    #[test]
    fn refused_imports_and_underived_surfaces_fall_back_to_copies() {
        let (_turn, mut va) = va(|fake| {
            fake.userptr = true;
            fake.refuse_user = true;
            fake.no_derive = true;
        });
        let (frame, segment) = in_segment(MONO8, 128, 48);
        let jpeg = encode(&mut va, &frame, &segment);
        assert_eq!(jpeg, crate::jpeg::encode(&frame, QUALITY).unwrap());
        assert!(!va.userptr, "not tried again");
        assert_eq!(fake(|fake| fake.put), 1);
    }

    #[test]
    fn missing_end_markers_are_added_and_sizes_out_of_range_declined() {
        let (_turn, mut va) = va(|fake| fake.drop_eoi = true);
        let (frame, segment) = in_segment(RGB8, 64, 48);
        let jpeg = encode(&mut va, &frame, &segment);
        assert!(jpeg.ends_with(&[0xFF, 0xD9]));
        let (small, segment) = in_segment(RGB8, 8, 8);
        let error = va
            .encode(&small, &segment, QUALITY, &mut vec![0; 1 << 16])
            .unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
        let error = va
            .encode(&frame, &segment, QUALITY, &mut [0; 10])
            .unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
    }

    /// libva's messages do not reach stderr, which the worker passes on to
    /// the user: its errors explain the call that failed, and the rest are
    /// dropped.
    #[test]
    fn libva_messages_explain_failures_instead_of_printing() {
        let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
        *FAKE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Fake {
            init_errors: vec![
                c"/usr/lib/dri/nvidia_drv_video.so init failed\n",
                c"va_openDriver() returns 1\n",
            ],
            ..Fake::default()
        });
        let error = Va::on_display(api(), std::ptr::dangling_mut(), None)
            .err()
            .unwrap();
        let text = format!("{error:#}");
        assert!(
            text.ends_with(
                "(status -1); libva: /usr/lib/dri/nvidia_drv_video.so init failed; \
                 va_openDriver() returns 1"
            ),
            "{text}"
        );
        assert!(text.starts_with("vaInitialize failed"), "{text}");
        assert!(!text.contains("VA-API version"), "{text}");
        // What a call said is not left for the next one's error.
        let api = api();
        api.messages.0.lock().unwrap().push_back("earlier".into());
        api.check("vaSyncSurface", SUCCESS).unwrap();
        let error = api.check("vaEndPicture", -1).unwrap_err();
        assert!(!format!("{error:#}").contains("earlier"), "{error:#}");
    }

    #[test]
    fn invalid_mappings_and_overlapping_planes_fail_without_leaking_images() {
        const unsafe extern "C" fn fail_unmap(_: Display, _: u32) -> Status {
            1
        }
        unsafe extern "C" fn null_mapping(_: Display, _: u32, out: *mut *mut c_void) -> Status {
            // SAFETY: the encoder supplies initialized writable pointer storage.
            unsafe { *out = std::ptr::null_mut() };
            SUCCESS
        }
        unsafe extern "C" fn overlapping_image(
            display: Display,
            surface: u32,
            out: *mut Image,
        ) -> Status {
            // SAFETY: forward the encoder's valid display, surface, and image output.
            let status = unsafe { derive_image(display, surface, out) };
            if status == SUCCESS {
                // SAFETY: derive_image initialized the writable output image above.
                unsafe { (*out).offsets[1] = 1 };
            }
            status
        }
        for null in [false, true] {
            let (_turn, mut va) = va(|_| {});
            // A secondary cleanup error must not hide the invalid layout.
            va.api.unmap_buffer = fail_unmap;
            if null {
                va.api.map_buffer = null_mapping;
            } else {
                va.api.derive_image = overlapping_image;
            }
            let frame = crate::frame::noise_frame(RGB8, 64, 64);
            let error = va.encode_frame(&frame, None, QUALITY, &mut []).unwrap_err();
            let expected = if null {
                "an invalid image mapping"
            } else {
                "an unexpected surface layout"
            };
            assert_eq!(error.to_string(), expected);
            fake(|fake| assert_eq!(fake.images.len(), 0));
        }
    }

    #[test]
    fn failed_session_creation_releases_resources_and_can_be_retried() {
        for context_fails in [true, false] {
            let (_turn, mut va) = va(|_| {});
            va.end_session();
            fake(|fake| {
                fake.fail_context = context_fails;
                fake.fail_coded = !context_fails;
            });
            let error = va.begin_session(128, 64, 3).unwrap_err();
            let call = if context_fails {
                "vaCreateContext"
            } else {
                "vaCreateBuffer"
            };
            assert!(error.to_string().starts_with(call), "{error:#}");
            assert!(va.session.is_none());
            fake(|fake| {
                assert_eq!(fake.surfaces.len(), 0);
                assert_eq!(fake.contexts.len(), 0);
                assert_eq!(fake.coded.len(), 0);
                fake.fail_context = false;
                fake.fail_coded = false;
            });
            va.begin_session(128, 64, 3).unwrap();
            assert!(va.session.is_some());
            va.end_session();
            fake(|fake| {
                assert_eq!(fake.surfaces.len(), 0);
                assert_eq!(fake.contexts.len(), 0);
                assert_eq!(fake.coded.len(), 0);
            });
        }
    }

    #[test]
    fn failed_coded_unmap_is_reported_after_upload_succeeds() {
        unsafe extern "C" fn fail_coded_unmap(_: Display, buffer: u32) -> Status {
            fake(|fake| {
                if fake.coded.contains_key(&buffer) {
                    1
                } else {
                    SUCCESS
                }
            })
        }
        let (_turn, mut va) = va(|_| {});
        va.api.unmap_buffer = fail_coded_unmap;
        let frame = crate::frame::noise_frame(RGB8, 64, 64);
        let mut output = vec![0; 64 * 1024];
        let error = va
            .encode_frame(&frame, None, QUALITY, &mut output)
            .unwrap_err();
        assert!(
            error.to_string().starts_with("vaUnmapBuffer failed"),
            "{error:#}"
        );
        fake(|fake| {
            assert_eq!(fake.images.len(), 0);
            assert_eq!(fake.encoded_from.len(), 1);
        });
    }

    #[test]
    fn failed_upload_unmap_is_reported_and_the_image_is_destroyed() {
        const unsafe extern "C" fn fail_unmap(_: Display, _: u32) -> Status {
            1
        }
        let (_turn, mut va) = va(|_| {});
        va.api.unmap_buffer = fail_unmap;
        let frame = crate::frame::noise_frame(RGB8, 64, 64);
        let error = va.encode_frame(&frame, None, QUALITY, &mut []).unwrap_err();
        assert!(
            error.to_string().starts_with("vaUnmapBuffer failed"),
            "{error:#}"
        );
        fake(|fake| {
            assert_eq!(fake.images.len(), 0);
            assert_eq!(fake.encoded_from, [] as [u32; 0]);
        });
    }

    #[test]
    fn encoding_without_an_active_session_returns_an_error() {
        let (_turn, mut va) = va(|_| {});
        va.end_session();
        let error = va
            .encode_surface(0, 64, 64, false, QUALITY, &mut [])
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "VA-API encoding session is not initialized"
        );
        let frame = crate::frame::noise_frame(RGB8, 64, 64);
        assert_eq!(
            va.upload(&frame).unwrap_err().to_string(),
            "VA-API encoding session is not initialized"
        );
    }

    #[test]
    fn about_names_the_driver_and_how_frames_reach_it() {
        let (_turn, va) = va(|fake| fake.userptr = true);
        let about = va.about(std::path::Path::new("/dev/dri/renderD128"));
        assert_eq!(about["driver"], "Fake driver (VA-API 1.22)");
        assert_eq!(about["upload"], "in-place");
        assert_eq!(about["device"], "/dev/dri/renderD128");
    }

    /// libjpeg-turbo at `QUALITY` with standard Huffman tables; returns the
    /// JPEG and the offset of its entropy-coded data.
    fn turbo(width: usize, height: usize, channels: usize, pixels: &[u8]) -> (Vec<u8>, usize) {
        let mut compressor = Compressor::new().unwrap();
        compressor.set_quality(QUALITY).unwrap();
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
                .map(|i| (i * 7 + i / 97).to_le_bytes()[0])
                .collect();
            let (jpeg, scan) = turbo(width, height, channels, &pixels);
            let mut transplanted = header(
                u16::try_from(width).unwrap(),
                u16::try_from(height).unwrap(),
                channels,
                QUALITY,
            );
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
            assert!(super::super::well_formed(
                &transplanted,
                u32::try_from(width).unwrap(),
                u32::try_from(height).unwrap()
            ));
        }
    }

    #[test]
    fn quality_scaling_matches_libjpeg() {
        let [luma, chroma] = tables(QUALITY);
        // libjpeg at quality 90: scale 20%, rounded, at least 1.
        assert_eq!(&luma[..4], &[3, 2, 2, 3]);
        assert_eq!(chroma[63], 20);
        // Quality 50 is the Annex K table itself.
        assert_eq!(quant_table(&LUMA_QUANT, 50)[2], LUMA_QUANT[8]);
    }
}
