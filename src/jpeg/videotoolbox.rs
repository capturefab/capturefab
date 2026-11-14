//! Apple's JPEG engine through `VideoToolbox` (`videotoolbox` feature,
//! macOS), a system framework that runs in the camera worker.
//!
//! Apple silicon has a dedicated JPEG encoder. A compression session that
//! requires hardware either opens on it or fails, so a host without the
//! engine keeps libjpeg-turbo.
//!
//! A frame is converted once, from its rows straight into the session's
//! IOSurface-backed buffer that the engine reads, as full-range BT.601
//! `Y'CbCr` 4:2:0 (NV12, `'420f'`): the JFIF color space decoders assume,
//! which the engine's own RGB conversion does not produce (it shifts colors
//! by several levels). RGB8 rows are read in place; Bayer, packed and other
//! layouts are decoded a row at a time on the way. The JPEG stays in the
//! sample buffer the engine wrote it to. Sessions are kept per frame size.

#![deny(let_underscore_drop, unused_results)]

use super::backend::Backend;
use crate::frame::{Body, Chroma, FrameRef, Yuv420};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    ffi::c_void,
    ptr::{null, null_mut},
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
    fn CFRetain(object: CFTypeRef) -> CFTypeRef;
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
    fn CMBlockBufferIsRangeContiguous(block: CMBlockBufferRef, offset: usize, length: usize) -> u8;
    fn CMBlockBufferGetDataPointer(
        block: CMBlockBufferRef,
        offset: usize,
        length_at_offset: *mut usize,
        total_length: *mut usize,
        data: *mut *mut u8,
    ) -> OSStatus;
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
/// The engine quantizes in coarse steps: on an M4 Max every setting from
/// about 0.56 to 0.82 produced identical files, within 1 dB PSNR of
/// libjpeg-turbo at `QUALITY` (90) and about 10% smaller, while 0.83 and
/// above were 1.6 times larger. The middle of that step keeps file sizes
/// comparable across encoders. Other qualities stay on the CPU.
const ENGINE_QUALITY: f32 = 0.75;
/// Sessions kept open, one per frame size.
const SESSIONS: usize = 2;

/// A JPEG in the sample buffer the engine wrote it to.
pub struct Output {
    /// Retained here, and released on drop.
    _sample: Cf,
    data: *const u8,
    len: usize,
}
// SAFETY: the sample buffer is an immutable CoreFoundation object whose
// retain count is thread-safe; its bytes are only read.
unsafe impl Send for Output {}
// SAFETY: as above.
unsafe impl Sync for Output {}
impl std::ops::Deref for Output {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `data` points at `len` contiguous bytes of the block
        // buffer `sample` retains.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }
}

/// What the output callback leaves for one frame: the retained sample
/// buffer, or the engine's error status.
type Slot = Option<Result<CMSampleBufferRef, OSStatus>>;

extern "C" fn on_output(
    _: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    _: u32,
    sample: CMSampleBufferRef,
) {
    // SAFETY: `frame` is the `Slot` passed to EncodeFrame, which outlives
    // the synchronous CompleteFrames call that delivers it.
    let slot = unsafe { &mut *frame.cast::<Slot>() };
    *slot = Some(if status != 0 || sample.is_null() {
        Err(if status != 0 { status } else { -1 })
    } else {
        // SAFETY: a live sample buffer, kept until its `Output` drops.
        Ok(unsafe { CFRetain(sample).cast_mut() })
    });
}

/// The JPEG of a sample buffer the callback retained: its own bytes where
/// they are contiguous, as they normally are, else a counted copy.
fn output(sample: CMSampleBufferRef) -> Result<Body<'static>> {
    let sample = Cf(sample);
    // SAFETY: a retained sample buffer; its block buffer lives as long.
    let block = unsafe { CMSampleBufferGetDataBuffer(sample.0.cast_mut()) };
    if block.is_null() {
        bail!("the JPEG engine returned no data");
    }
    // SAFETY: the block is retained by `sample` throughout this function.
    let len = unsafe { CMBlockBufferGetDataLength(block) };
    // SAFETY: checks the complete range of that live block.
    if unsafe { CMBlockBufferIsRangeContiguous(block, 0, 0) } != 0 {
        let (mut at_offset, mut total, mut data) = (0, 0, null_mut());
        // SAFETY: the live block writes to the initialized output slots.
        let status = unsafe {
            CMBlockBufferGetDataPointer(block, 0, &raw mut at_offset, &raw mut total, &raw mut data)
        };
        if status == 0 && !data.is_null() && at_offset == len {
            crate::copies::record(crate::copies::Site::JpegEncode, len);
            return Ok(Body::Native(Output {
                _sample: sample,
                data,
                len,
            }));
        }
    }
    let mut bytes = vec![0u8; len];
    // SAFETY: the retained block copies `len` bytes into a buffer of that size.
    let status = unsafe { CMBlockBufferCopyDataBytes(block, 0, len, bytes.as_mut_ptr().cast()) };
    ensure!(
        status == 0,
        "cannot read the JPEG engine's output (status {status})"
    );
    crate::copies::record(crate::copies::Site::OutputCopy, len);
    Ok(Body::Vec(bytes))
}

/// An owned CoreFoundation object.
struct Cf(CFTypeRef);
impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created or retained by a CF function and owned here.
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
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })
}
fn set(dict: &Cf, key: CFStringRef, value: CFTypeRef) {
    // SAFETY: `dict` is a live mutable dictionary; values are retained.
    unsafe { CFDictionarySetValue(dict.0.cast_mut(), key, value) }
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
}
// SAFETY: a session is used by one thread at a time, its engine's.
unsafe impl Send for Session {}
impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session was created here and is no longer in use.
        unsafe { VTCompressionSessionInvalidate(self.raw) };
        // SAFETY: invalidation finished; release this session's retained reference.
        unsafe { CFRelease(self.raw) };
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
        let hardware =
            unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder };
        // SAFETY: the framework boolean is valid for the process lifetime.
        let enabled = unsafe { kCFBooleanTrue };
        set(&spec, hardware, enabled);
        let attributes = dictionary();
        let surface = dictionary();
        let signed_width = i32::try_from(width).context("JPEG width exceeds encoder range")?;
        let signed_height = i32::try_from(height).context("JPEG height exceeds encoder range")?;
        let (format, w, h) = (int(PIXEL_420F), int(signed_width), int(signed_height));
        // SAFETY: as above.
        let pixel_format = unsafe { kCVPixelBufferPixelFormatTypeKey };
        // SAFETY: a framework key, valid for the process lifetime.
        let width_key = unsafe { kCVPixelBufferWidthKey };
        // SAFETY: a framework key, valid for the process lifetime.
        let height_key = unsafe { kCVPixelBufferHeightKey };
        // SAFETY: a framework key, valid for the process lifetime.
        let surface_key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
        set(&attributes, pixel_format, format.0);
        set(&attributes, width_key, w.0);
        set(&attributes, height_key, h.0);
        set(&attributes, surface_key, surface.0);
        let mut raw = null_mut();
        // SAFETY: all arguments are valid; the session is returned retained.
        let status = unsafe {
            VTCompressionSessionCreate(
                null(),
                signed_width,
                signed_height,
                CODEC_JPEG,
                spec.0,
                attributes.0,
                null(),
                Some(on_output),
                null_mut(),
                &raw mut raw,
            )
        };
        if status != 0 || raw.is_null() {
            bail!("this Mac has no hardware JPEG encoder (VideoToolbox status {status})");
        }
        let session = Self { raw, width, height };
        let quality = ENGINE_QUALITY;
        // SAFETY: reads one initialized f32, copied into the returned CF number.
        let quality =
            Cf(unsafe { CFNumberCreate(null(), NUMBER_FLOAT32, (&raw const quality).cast()) });
        // SAFETY: a framework key, valid for the process lifetime.
        let quality_key = unsafe { kVTCompressionPropertyKey_Quality };
        // SAFETY: the live session receives the live quality number under its quality key.
        let status = unsafe { VTSessionSetProperty(raw, quality_key, quality.0) };
        ensure!(
            status == 0,
            "cannot set the JPEG engine's quality (status {status})"
        );
        Ok(session)
    }

    fn encode(&mut self, view: FrameRef<'_>) -> Result<Body<'static>> {
        let (width, height) = (self.width as usize, self.height as usize);
        // SAFETY: the pool belongs to this live session.
        let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.raw) };
        ensure!(!pool.is_null(), "the JPEG engine has no buffer pool");
        let mut buffer = null_mut();
        // SAFETY: the live pool writes a retained buffer to the initialized output slot.
        let status = unsafe { CVPixelBufferPoolCreatePixelBuffer(null(), pool, &raw mut buffer) };
        ensure!(
            status == 0 && !buffer.is_null(),
            "cannot allocate a JPEG engine buffer (status {status})"
        );
        let buffer = Cf(buffer);
        let pixels = buffer.0.cast_mut();
        // SAFETY: `buffer` owns this live pixel buffer.
        let status = unsafe { CVPixelBufferLockBaseAddress(pixels, 0) };
        ensure!(status == 0, "cannot lock the JPEG engine's buffer");
        let plane = |i| {
            // SAFETY: only planes 0 and 1 of this locked two-plane buffer are queried.
            let base = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixels, i) };
            // SAFETY: the same live plane provides its row extent.
            let stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixels, i) };
            // SAFETY: the same live plane provides its height.
            let rows = unsafe { CVPixelBufferGetHeightOfPlane(pixels, i) };
            let len = stride
                .checked_mul(rows)
                .filter(|&len| isize::try_from(len).is_ok())?;
            (!base.is_null()).then(|| {
                // SAFETY: CoreVideo supplies a writable plane of this checked extent;
                // the buffer remains locked and its two planes are disjoint.
                let bytes = unsafe { std::slice::from_raw_parts_mut(base, len) };
                (bytes, stride, rows)
            })
        };
        // SAFETY: the pixel buffer is live and locked.
        let planes = (unsafe { CVPixelBufferGetPlaneCount(pixels) } == 2)
            .then(|| plane(0).zip(plane(1)))
            .flatten()
            .filter(
                |((_, luma_stride, luma_rows), (_, chroma_stride, chroma_rows))| {
                    *luma_stride >= width
                        && *luma_rows >= height
                        && *chroma_stride >= width.div_ceil(2) * 2
                        && *chroma_rows >= height.div_ceil(2)
                },
            );
        let fits = planes.is_some();
        if let Some(((luma, luma_stride, _), (cbcr, stride, _))) = planes {
            crate::frame::ycbcr420(
                view,
                Yuv420 {
                    luma,
                    luma_stride,
                    chroma: Chroma::Interleaved { cbcr, stride },
                },
            );
            crate::copies::record(
                crate::copies::Site::StillTransform,
                width * height + width.div_ceil(2) * 2 * height.div_ceil(2),
            );
        }
        // SAFETY: all plane slices are out of scope and this buffer was locked above.
        let status = unsafe { CVPixelBufferUnlockBaseAddress(pixels, 0) };
        ensure!(
            status == 0,
            "cannot unlock the JPEG engine's buffer (status {status})"
        );
        ensure!(fits, "the JPEG engine's buffer has an unexpected layout");
        let mut slot: Slot = None;
        let pts = CMTime {
            value: 0,
            timescale: 1,
            flags: VALID,
            epoch: 0,
        };
        let invalid = CMTime { flags: 0, ..pts };
        // SAFETY: the live session receives a live, unlocked pixel buffer;
        // `slot` stays alive through the synchronous CompleteFrames call below.
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.raw,
                pixels,
                pts,
                invalid,
                null(),
                (&raw mut slot).cast(),
                null_mut(),
            )
        };
        ensure!(
            status == 0,
            "the JPEG engine rejected the frame (status {status})"
        );
        // SAFETY: the session is live and the callback's output slot is still in scope.
        let status = unsafe { VTCompressionSessionCompleteFrames(self.raw, invalid) };
        if status != 0 {
            if let Some(Ok(sample)) = slot {
                // SAFETY: the callback retained this sample; no output will take ownership.
                unsafe { CFRelease(sample) };
            }
            bail!("the JPEG engine failed (status {status})");
        }
        match slot {
            Some(Ok(sample)) => output(sample),
            Some(Err(status)) => bail!("the JPEG engine failed (status {status})"),
            None => bail!("the JPEG engine produced no output"),
        }
    }
}

/// The engine, with a session for each of the last frame sizes it encoded.
pub(crate) struct Encoder {
    sessions: Vec<Session>,
}

impl Encoder {
    /// The engine, if this Mac has one: a session that requires hardware
    /// opens only on it.
    pub(crate) fn open() -> Result<Self> {
        drop(Session::open(64, 64)?);
        Ok(Self {
            sessions: Vec::new(),
        })
    }
}

impl Backend for Encoder {
    fn accepts(view: FrameRef<'_>, quality: i32) -> bool {
        super::backend::color_at_default_quality(view, quality)
    }
    fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>> {
        ensure!(
            Self::accepts(view, quality),
            "the JPEG engine takes only color frames at the default quality"
        );
        crate::frame::validate_source(view.frame)?;
        let size = (
            u32::try_from(view.width).context("JPEG view width exceeds u32")?,
            u32::try_from(view.height).context("JPEG view height exceeds u32")?,
        );
        let index = if let Some(index) = self
            .sessions
            .iter()
            .position(|session| (session.width, session.height) == size)
        {
            index
        } else {
            if self.sessions.len() == SESSIONS {
                // Close the oldest first, so fewer hold the engine.
                drop(self.sessions.remove(0));
            }
            self.sessions.push(Session::open(size.0, size.1)?);
            self.sessions.len() - 1
        };
        let result = self.sessions[index].encode(view);
        if result.is_err() {
            // A session that failed is not reused.
            drop(self.sessions.remove(index));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        jpeg::{
            QUALITY,
            backend::{Engine, Kind, accurate},
        },
        types::{Frame, RGB8},
    };

    fn frame(pixel_format: u32, width: u32, height: u32) -> Frame {
        crate::frame::noise_frame(pixel_format, width, height)
    }

    /// A smooth color scene in `format`, made from an RGB gradient.
    fn scene(format: u32, width: u32, height: u32) -> Frame {
        let (w, h) = (width as usize, height as usize);
        let rgb: Vec<[u8; 3]> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                [
                    u8::try_from(x * 255 / w).unwrap(),
                    u8::try_from(y * 255 / h).unwrap(),
                    u8::try_from((x + y) * 255 / (w + h)).unwrap(),
                ]
            })
            .collect();
        let pattern = crate::frame::bayer_pattern(format);
        let data: Vec<u8> = match format {
            RGB8 => rgb.as_flattened().to_vec(),
            // Each sample takes its filter's channel, packed for 12p.
            0x0108_0009 => (0..w * h)
                .map(|i| rgb[i][usize::from(pattern[((i / w) & 1) * 2 + ((i % w) & 1)])])
                .collect(),
            0x010c_0059 => {
                let samples: Vec<u16> = (0..w * h)
                    .map(|i| {
                        u16::from(rgb[i][usize::from(pattern[((i / w) & 1) * 2 + ((i % w) & 1)])])
                            << 4
                    })
                    .collect();
                samples
                    .chunks(2)
                    .flat_map(|pair| {
                        let (a, b) = (pair[0], *pair.get(1).unwrap_or(&0));
                        let [a_low, a_high] = a.to_le_bytes();
                        let [b_low, b_high] = b.to_le_bytes();
                        [
                            a_low,
                            (a_high & 0x0f) | ((b_low & 0x0f) << 4),
                            (b_low >> 4) | (b_high << 4),
                        ]
                    })
                    .collect()
            }
            _ => unreachable!(),
        };
        Frame {
            pixel_encoding: None,
            width,
            height,
            pixel_format: format,
            data: data.into(),
            ..Frame::default()
        }
    }

    fn engine() -> Option<Engine> {
        let engine = Engine::new(Kind::VideoToolbox, Encoder::open);
        engine.unavailable().map_or_else(
            || Some(engine),
            |reason| {
                eprintln!("no VideoToolbox JPEG engine ({reason}); skipped");
                None
            },
        )
    }

    /// On this Mac's engine, color frames of every kind and size pass the
    /// check against libjpeg-turbo and come out well formed.
    #[test]
    fn the_engine_writes_accurate_jpegs_of_color_frames() {
        let Some(_) = engine() else { return };
        let mut encoder = Encoder::open().unwrap();
        for (format, width, height) in [
            (RGB8, 1920, 1080),
            (0x0108_0009, 1920, 1080),
            (0x010c_0059, 1920, 1080),
            (RGB8, 4096, 3000),
            (0x0108_0009, 4096, 3000),
            (0x010c_0059, 4096, 3000),
            (RGB8, 1031, 517),
        ] {
            let frame = scene(format, width, height);
            let jpeg = encoder.encode(FrameRef::whole(&frame), QUALITY).unwrap();
            assert!(
                super::super::well_formed(&jpeg, width, height),
                "{format:08x} {width}x{height}"
            );
            assert!(
                matches!(jpeg, Body::Native(_)),
                "the engine's buffer is kept"
            );
            let cpu = super::super::encode(&frame, QUALITY).unwrap();
            accurate(&frame, &jpeg, &cpu)
                .unwrap_or_else(|reason| panic!("{format:08x} {width}x{height}: {reason}"));
        }
    }

    #[test]
    fn the_engine_takes_only_color_at_the_default_quality() {
        let Some(_) = engine() else { return };
        let mono = frame(crate::types::MONO8, 64, 48);
        let color = frame(RGB8, 64, 48);
        assert!(!Encoder::accepts(FrameRef::whole(&mono), QUALITY));
        assert!(!Encoder::accepts(FrameRef::whole(&color), 75));
        assert!(Encoder::accepts(FrameRef::whole(&color), QUALITY));
    }

    /// Two encoder threads at once: one gets the engine, the other the CPU,
    /// and both stills are whole.
    #[test]
    fn concurrent_stills_share_the_engine_and_the_cpu() {
        let Some(engine) = engine() else { return };
        let engine = std::sync::Arc::new(engine);
        let frame = scene(0x0108_0009, 1920, 1080);
        // Checked once, so later frames go straight to the engine when it
        // is faster here, else to the CPU.
        drop(engine.encode(&frame, QUALITY, true).unwrap());
        let threads: Vec<_> = std::iter::repeat_with(|| {
            let (engine, frame) = (engine.clone(), frame.clone());
            std::thread::spawn(move || {
                std::iter::repeat_with(|| engine.encode(&frame, QUALITY, false).unwrap())
                    .take(20)
                    .collect::<Vec<_>>()
            })
        })
        .take(2)
        .collect();
        let mut kinds = std::collections::HashSet::new();
        for thread in threads {
            for jpeg in thread.join().unwrap() {
                assert!(super::super::well_formed(&jpeg, 1920, 1080));
                let _ = kinds.insert(matches!(jpeg, Body::Native(_)));
            }
        }
        assert!(!kinds.is_empty());
        assert_eq!(engine.verified(), Some(true));
    }

    /// Sessions are kept for two sizes and replaced beyond them; odd sizes
    /// keep their last column and row.
    #[test]
    fn sessions_follow_the_frame_size() {
        let Some(_) = engine() else { return };
        let mut encoder = Encoder::open().unwrap();
        for (width, height) in [
            (1280, 720),
            (1031, 517),
            (1280, 720),
            (641, 479),
            (1031, 517),
        ] {
            let frame = scene(RGB8, width, height);
            let jpeg = encoder.encode(FrameRef::whole(&frame), QUALITY).unwrap();
            assert!(super::super::well_formed(&jpeg, width, height));
            assert!(encoder.sessions.len() <= SESSIONS);
        }
    }

    /// A 200-frame burst through the engine leaves nothing behind: `leaks`
    /// finds no leaked CoreFoundation or Rust memory in this process.
    #[test]
    #[ignore = "runs macOS leaks on the test process; about 10 s"]
    fn a_burst_through_the_engine_leaks_nothing() {
        let Some(engine) = engine() else { return };
        let frame = scene(0x0108_0009, 1920, 1080);
        for _ in 0..200 {
            let jpeg = engine.encode(&frame, QUALITY, true).unwrap();
            assert!(super::super::well_formed(&jpeg, 1920, 1080));
        }
        drop(engine);
        let output = std::process::Command::new("/usr/bin/leaks")
            .arg(std::process::id().to_string())
            .output()
            .unwrap();
        let report = String::from_utf8_lossy(&output.stdout);
        let summary = report
            .lines()
            .find(|line| line.contains("leaks for"))
            .unwrap_or_default()
            .to_owned();
        eprintln!("{summary}");
        assert!(
            summary.contains(" 0 leaks for 0 total leaked bytes"),
            "{report}"
        );
    }
}
