//! Apple JPEG engine through VideoToolbox (`videotoolbox` feature, macOS).
//!
//! Apple silicon has a dedicated JPEG encoder. A compression session that
//! requires hardware either opens on it or fails, so a host without the engine
//! (or an Intel Mac whose driver lacks it) keeps libjpeg-turbo.
//!
//! Frames are converted once, straight into the session's IOSurface-backed
//! buffer that the engine reads without a further copy, as full-range BT.601
//! YCbCr 4:2:0: the JFIF color space decoders assume, which the engine's own
//! RGB conversion does not produce (it shifts colors by several levels), at
//! 1.5 bytes per pixel instead of 4 for BGRA. Sessions are kept per frame
//! size with a reused RGB scratch buffer; encoding runs synchronously under
//! one lock.
//! `CAPTUREFAB_VIDEOTOOLBOX_JPEG=0` disables it.
#![allow(unsafe_code)]

use crate::jpeg::well_formed;
use crate::types::Frame;
use anyhow::{Result, bail, ensure};
use std::{
    ffi::c_void,
    ptr::{null, null_mut},
    sync::{Mutex, OnceLock, PoisonError},
};

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type CVPixelBufferPoolRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type VTCompressionSessionRef = *mut c_void;
type OSStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}
const VALID: u32 = 1;

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CMSampleBufferRef);

#[repr(C)]
struct CFDictionaryCallBacks([usize; 6]);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
    static kCFBooleanTrue: CFTypeRef;
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: isize,
        keys: *const CFDictionaryCallBacks,
        values: *const CFDictionaryCallBacks,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: CFTypeRef, value: CFTypeRef);
    fn CFNumberCreate(allocator: CFAllocatorRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
}
const NUMBER_SINT32: isize = 3;
const NUMBER_FLOAT32: isize = 5;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: CFAllocatorRef,
        pool: CVPixelBufferPoolRef,
        out: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetPlaneCount(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
}
/// `'420f'`: full-range bi-planar YCbCr 4:2:0 (NV12), the JFIF color space.
const PIXEL_420F: i32 = 0x3432_3066;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetDataBuffer(sample: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMBlockBufferGetDataLength(block: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        block: CMBlockBufferRef,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTCompressionPropertyKey_Quality: CFStringRef;
    fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_specification: CFDictionaryRef,
        source_attributes: CFDictionaryRef,
        compressed_allocator: CFAllocatorRef,
        callback: Option<OutputCallback>,
        refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: CFTypeRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionGetPixelBufferPool(
        session: VTCompressionSessionRef,
    ) -> CVPixelBufferPoolRef;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image: CVPixelBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_refcon: *mut c_void,
        info_flags: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        until: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
}
/// `'jpeg'`
const CODEC_JPEG: u32 = 0x6A70_6567;
/// The engine quantizes in coarse steps: on an M4 Max every setting from about
/// 0.56 to 0.82 produced identical files, within 1 dB PSNR of libjpeg-turbo at
/// `QUALITY` (90) and about 10% smaller, while 0.83 and above were 1.6 times
/// larger. The middle of that step keeps file sizes, and so storage budgets,
/// comparable across backends.
const ENGINE_QUALITY: f32 = 0.75;

/// Where the output callback leaves one frame's JPEG or error status.
type Output = Result<Vec<u8>, OSStatus>;

extern "C" fn on_output(
    _: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    _: u32,
    sample: CMSampleBufferRef,
) {
    // SAFETY: `frame` is the `Option<Output>` passed to EncodeFrame, which
    // outlives the synchronous CompleteFrames call that delivers it.
    let slot = unsafe { &mut *(frame as *mut Option<Output>) };
    *slot = Some(if status != 0 || sample.is_null() {
        Err(if status != 0 { status } else { -1 })
    } else {
        // SAFETY: a successful sample carries a data buffer of the JPEG.
        unsafe {
            let block = CMSampleBufferGetDataBuffer(sample);
            if block.is_null() {
                Err(-1)
            } else {
                let mut bytes = vec![0u8; CMBlockBufferGetDataLength(block)];
                match CMBlockBufferCopyDataBytes(block, 0, bytes.len(), bytes.as_mut_ptr().cast()) {
                    0 => Ok(bytes),
                    e => Err(e),
                }
            }
        }
    });
}

/// An owned CoreFoundation object.
struct Cf(CFTypeRef);
impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created by a CF "Create" function and owned here.
            unsafe { CFRelease(self.0) }
        }
    }
}
fn dictionary() -> Cf {
    // SAFETY: standard CF type callbacks retain keys and values.
    Cf(unsafe {
        CFDictionaryCreateMutable(
            null(),
            0,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    })
}
fn set(dict: &Cf, key: CFStringRef, value: CFTypeRef) {
    // SAFETY: `dict` is a live mutable dictionary; values are retained.
    unsafe { CFDictionarySetValue(dict.0 as CFMutableDictionaryRef, key, value) }
}
fn int(value: i32) -> Cf {
    // SAFETY: reads one i32.
    Cf(unsafe { CFNumberCreate(null(), NUMBER_SINT32, (&raw const value).cast()) })
}

/// A hardware JPEG compression session for one frame size.
struct Session {
    raw: VTCompressionSessionRef,
    width: u32,
    height: u32,
    /// Demosaiced or converted RGB for formats that are not RGB8 already.
    rgb: Vec<[u8; 3]>,
}
// SAFETY: a session is only used under `ENGINE`'s lock.
unsafe impl Send for Session {}
impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session was created here and is no longer in use.
        unsafe {
            VTCompressionSessionInvalidate(self.raw);
            CFRelease(self.raw);
        }
    }
}
impl Session {
    fn open(width: u32, height: u32) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && width <= 16384 && height <= 16384,
            "frame size outside the JPEG engine's range"
        );
        let spec = dictionary();
        // SAFETY: framework constants are valid for the process lifetime.
        unsafe {
            set(
                &spec,
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                kCFBooleanTrue,
            );
        }
        let attributes = dictionary();
        let surface = dictionary();
        let (format, w, h) = (int(PIXEL_420F), int(width as i32), int(height as i32));
        // SAFETY: as above.
        unsafe {
            set(&attributes, kCVPixelBufferPixelFormatTypeKey, format.0);
            set(&attributes, kCVPixelBufferWidthKey, w.0);
            set(&attributes, kCVPixelBufferHeightKey, h.0);
            set(&attributes, kCVPixelBufferIOSurfacePropertiesKey, surface.0);
        }
        let mut raw = null_mut();
        // SAFETY: all arguments are valid; the session is returned retained.
        let status = unsafe {
            VTCompressionSessionCreate(
                null(),
                width as i32,
                height as i32,
                CODEC_JPEG,
                spec.0,
                attributes.0,
                null(),
                Some(on_output),
                null_mut(),
                &mut raw,
            )
        };
        if status != 0 || raw.is_null() {
            bail!("no hardware JPEG encoder (VideoToolbox status {status})");
        }
        let session = Self {
            raw,
            width,
            height,
            rgb: Vec::new(),
        };
        let quality = ENGINE_QUALITY;
        // SAFETY: reads one f32; the key is a framework constant.
        let status = unsafe {
            let quality = Cf(CFNumberCreate(
                null(),
                NUMBER_FLOAT32,
                (&raw const quality).cast(),
            ));
            VTSessionSetProperty(raw, kVTCompressionPropertyKey_Quality, quality.0)
        };
        ensure!(status == 0, "cannot set JPEG quality (status {status})");
        Ok(session)
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<u8>> {
        let (width, height) = (self.width as usize, self.height as usize);
        let rgb = if frame.pixel_format == crate::types::RGB8 {
            ensure!(
                frame.data.len() >= width * height * 3,
                "truncated RGB frame"
            );
            frame.data.as_chunks::<3>().0
        } else {
            crate::frame::convert_into(frame, &mut self.rgb, |p| p)?;
            &self.rgb
        };
        // SAFETY: the pool belongs to the live session; the buffer is
        // released below and its planes are written only while locked,
        // within the extents CoreVideo reports for them.
        unsafe {
            let pool = VTCompressionSessionGetPixelBufferPool(self.raw);
            ensure!(!pool.is_null(), "JPEG engine has no buffer pool");
            let mut buffer = null_mut();
            let status = CVPixelBufferPoolCreatePixelBuffer(null(), pool, &mut buffer);
            ensure!(
                status == 0 && !buffer.is_null(),
                "cannot allocate JPEG engine buffer"
            );
            let buffer = Cf(buffer);
            let pixels = buffer.0 as CVPixelBufferRef;
            ensure!(
                CVPixelBufferLockBaseAddress(pixels, 0) == 0,
                "cannot lock JPEG engine buffer"
            );
            let plane = |i| {
                let (base, stride, rows) = (
                    CVPixelBufferGetBaseAddressOfPlane(pixels, i),
                    CVPixelBufferGetBytesPerRowOfPlane(pixels, i),
                    CVPixelBufferGetHeightOfPlane(pixels, i),
                );
                (!base.is_null()).then(|| {
                    (
                        std::slice::from_raw_parts_mut(base, stride * rows),
                        stride,
                        rows,
                    )
                })
            };
            let planes = (CVPixelBufferGetPlaneCount(pixels) == 2)
                .then(|| plane(0).zip(plane(1)))
                .flatten()
                .filter(|((_, ls, lr), (_, cs, cr))| {
                    *ls >= width
                        && *lr >= height
                        && *cs >= width.div_ceil(2) * 2
                        && *cr >= height.div_ceil(2)
                });
            let fits = planes.is_some();
            if let Some(((luma, luma_stride, _), (chroma, chroma_stride, _))) = planes {
                crate::jpeg::ycbcr420(rgb, width, height, luma, luma_stride, chroma, chroma_stride);
            }
            CVPixelBufferUnlockBaseAddress(pixels, 0);
            ensure!(fits, "JPEG engine buffer has an unexpected layout");
            let mut output: Option<Output> = None;
            let pts = CMTime {
                value: 0,
                timescale: 1,
                flags: VALID,
                epoch: 0,
            };
            let invalid = CMTime { flags: 0, ..pts };
            let status = VTCompressionSessionEncodeFrame(
                self.raw,
                pixels,
                pts,
                invalid,
                null(),
                (&raw mut output).cast(),
                null_mut(),
            );
            ensure!(
                status == 0,
                "JPEG engine rejected the frame (status {status})"
            );
            let status = VTCompressionSessionCompleteFrames(self.raw, invalid);
            ensure!(status == 0, "JPEG engine failed (status {status})");
            match output {
                Some(Ok(bytes)) => Ok(bytes),
                Some(Err(status)) => bail!("JPEG engine failed (status {status})"),
                None => bail!("JPEG engine produced no output"),
            }
        }
    }
}

/// Smaller frames encode faster on the CPU.
pub const MIN_PIXELS: usize = 512 * 1024;

enum State {
    Idle,
    Ready(Session),
    /// No engine on this host; not retried.
    Unavailable,
}
static ENGINE: Mutex<State> = Mutex::new(State::Idle);

fn wanted() -> bool {
    static WANTED: OnceLock<bool> = OnceLock::new();
    *WANTED.get_or_init(|| std::env::var("CAPTUREFAB_VIDEOTOOLBOX_JPEG").as_deref() != Ok("0"))
}

/// Encode a color frame on the JPEG engine; `None` when the engine is absent,
/// disabled or fails, so the caller encodes on the CPU instead.
pub fn encode(frame: &Frame) -> Option<Vec<u8>> {
    // About 0.25 ms per frame is fixed engine overhead: on an M4 Max
    // libjpeg-turbo was faster at 640x480 and slower from 1280x720 up.
    if !wanted() || (frame.width as usize * frame.height as usize) < MIN_PIXELS {
        return None;
    }
    let mut state = ENGINE.lock().unwrap_or_else(PoisonError::into_inner);
    if matches!(&*state, State::Unavailable) {
        return None;
    }
    let reuse =
        matches!(&*state, State::Ready(s) if (s.width, s.height) == (frame.width, frame.height));
    if !reuse {
        // Drop the old session first so only one holds the engine.
        let had_session = matches!(&*state, State::Ready(_));
        *state = State::Idle;
        match Session::open(frame.width, frame.height) {
            Ok(session) => *state = State::Ready(session),
            Err(_) if !had_session => {
                // The first open decides whether the engine exists at all;
                // later failures may be specific to one frame size.
                *state = State::Unavailable;
                return None;
            }
            Err(_) => return None,
        }
    }
    let State::Ready(session) = &mut *state else {
        return None;
    };
    match session.encode(frame) {
        Ok(bytes) if well_formed(&bytes, frame.width, frame.height) => Some(bytes),
        _ => {
            // A failed session is not reused.
            *state = State::Idle;
            None
        }
    }
}

/// Whether this host's JPEG engine opens, for `doctor`.
pub fn probe() -> Result<&'static str> {
    if !wanted() {
        bail!("disabled by CAPTUREFAB_VIDEOTOOLBOX_JPEG=0");
    }
    Session::open(64, 64)?;
    Ok("hardware")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RGB8;
    fn frame(pixel_format: u32, width: u32, height: u32, bytes: usize) -> Frame {
        Frame {
            id: 1,
            width,
            height,
            pixel_format,
            timestamp_ns: 0,
            data: (0..width as usize * height as usize * bytes)
                .map(|i| (i * 7 + i / 64) as u8)
                .collect(),
        }
    }
    /// Hosts without the engine skip; with it, output is a valid JPEG of the
    /// frame's size for odd and changing dimensions, and decodes to the
    /// source colors (the engine's own RGB conversion shifted them).
    #[test]
    fn engine_encodes_valid_jpegs_when_present() {
        if probe().is_err() {
            eprintln!("no VideoToolbox JPEG engine; skipped");
            return;
        }
        assert!(
            encode(&frame(RGB8, 64, 48, 3)).is_none(),
            "small frames use the CPU"
        );
        for (format, bytes, width, height) in [
            (RGB8, 3, 1280, 720),
            (RGB8, 3, 1031, 517),
            (0x0108_0009, 1, 1920, 1200),
            (RGB8, 3, 1280, 720),
        ] {
            let jpeg = encode(&frame(format, width, height, bytes)).expect("engine encode");
            assert!(well_formed(&jpeg, width, height), "{width}x{height}");
        }
        #[cfg(feature = "jpeg")]
        {
            // A smooth, saturated image: a matrix or range mismatch shows as
            // a mean shift of several levels per channel.
            let (width, height) = (1280usize, 720usize);
            let data: Vec<u8> = (0..width * height)
                .flat_map(|i| {
                    let (x, y) = (i % width, i / width);
                    [(x * 255 / width) as u8, (y * 255 / height) as u8, 200]
                })
                .collect();
            let source = Frame {
                data: data.clone(),
                ..frame(RGB8, width as u32, height as u32, 3)
            };
            let jpeg = encode(&source).expect("engine encode");
            let decoded = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::RGB).unwrap();
            let mut bias = [0f64; 3];
            for (i, (a, b)) in decoded.pixels.iter().zip(&data).enumerate() {
                bias[i % 3] += *a as f64 - *b as f64;
            }
            for (channel, total) in bias.iter().enumerate() {
                let mean = total / (width * height) as f64;
                assert!(mean.abs() < 1.0, "channel {channel} shifted by {mean:.2}");
            }
        }
    }
}
