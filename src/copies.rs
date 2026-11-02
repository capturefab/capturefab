//! Where a camera frame's bytes are copied on their way through a worker,
//! and the large allocations made meanwhile. Every copy site calls
//! [`record`] with the bytes it copied; the call does nothing unless the
//! `copy-audit` feature is on. A `copy-audit` worker writes what it counted
//! to `$CAPTUREFAB_COPY_REPORT/worker-PID.json` whenever it answers a request
//! or ends a job, and when it exits, and a GPU JPEG helper process
//! (`jpeg/helper.rs`) to `helper-PID.json` before each answer, for the copies
//! it makes for its worker (`tests/copies.rs` adds them all up).
//!
//! A copy is any duplication of pixel bytes in user space: a memcpy, a
//! clone, an extend, a zero fill about to be overwritten, an intermediate
//! image or a crop. Each site has one class:
//! - `boundary`: a crossing between the kernel and this process (a socket
//!   receive, a pipe or file write), which cannot be avoided;
//! - `necessary`: a user-space copy with a stated reason it stays;
//! - `transform`: a real decode, encode or conversion its consumer needs;
//! - `avoidable`: a copy that should not happen.
#![allow(unsafe_code)]
// Without the feature only the recording calls remain; the names and
// classes are for the report.
#![cfg_attr(not(feature = "copy-audit"), allow(dead_code))]

/// A place a frame's bytes are copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    /// A GVSP datagram received from the socket.
    GvspReceive,
    /// A frame read from FFmpeg's output pipe.
    PipeRead,
    /// A frame written into an encoder's input pipe.
    PipeWrite,
    /// A frame's own pages handed to an encoder's input pipe (`vmsplice`,
    /// Linux): no copy here; the encoder's read is the only one.
    PipeSplice,
    /// An encoded file written to disk.
    FileWrite,
    /// An image written to the capture command's standard output.
    StdoutWrite,
    /// A source's compressed frame passed, as it was sent, from its FFmpeg
    /// to a recording's that copies it.
    StreamCopy,
    /// A USB3 Vision payload transfer copied into its frame: transfers
    /// smaller than the frame are recycled mid-frame where the platform
    /// limits how much transfer memory one camera may hold.
    UsbAssemble,
    /// A GVSP packet that did not land where it was predicted, moved from
    /// the receive buffer: a resent, reordered or duplicate packet.
    GvspFallback,
    /// A padded GVSP row moved to close up the camera's row padding.
    GigeRowCompact,
    /// Packed or wide samples unpacked, or a layout FFmpeg cannot read
    /// converted, for an encoder's input pipe.
    FfmpegUnpack,
    /// A frame converted to the samples a still's encoder reads.
    StillTransform,
    /// A frame converted to the samples a preview is shrunk from.
    PreviewTransform,
    /// A frame converted to the layout a Jetson encoder reads.
    JetsonInput,
    /// A frame copied where a GPU JPEG encoder reads it: into shared memory
    /// its helper maps, for a frame from before the helper started or in a
    /// USB transfer's own buffer, or into a VA-API surface that could not
    /// take the frame's own memory.
    GpuUpload,
    /// A PNG encoder's output.
    PngEncode,
    /// A JPEG encoder's output.
    JpegEncode,
    /// A whole frame duplicated to hand it to two consumers.
    FrameClone,
    /// A shared frame duplicated to change it.
    FrameCow,
    /// A buffer zeroed only to be overwritten.
    ZeroFill,
    /// A GVSP payload packet copied from the receive buffer into its frame.
    GvspAssemble,
    /// A whole-frame image built only to be read by the next step.
    Intermediate,
    /// A region of a frame copied out as a frame of its own.
    Crop,
    /// A frame's bytes copied unchanged into a still.
    StillCopy,
    /// An encoded image copied again.
    OutputCopy,
}

/// What kind of copy a site makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Boundary,
    Necessary,
    Transform,
    Avoidable,
}

impl Site {
    pub(crate) const ALL: [Site; 25] = [
        Site::GvspReceive,
        Site::PipeRead,
        Site::PipeWrite,
        Site::PipeSplice,
        Site::FileWrite,
        Site::StdoutWrite,
        Site::StreamCopy,
        Site::UsbAssemble,
        Site::GvspFallback,
        Site::GigeRowCompact,
        Site::FfmpegUnpack,
        Site::StillTransform,
        Site::PreviewTransform,
        Site::JetsonInput,
        Site::GpuUpload,
        Site::PngEncode,
        Site::JpegEncode,
        Site::FrameClone,
        Site::FrameCow,
        Site::ZeroFill,
        Site::GvspAssemble,
        Site::Intermediate,
        Site::Crop,
        Site::StillCopy,
        Site::OutputCopy,
    ];
    pub(crate) fn name(self) -> &'static str {
        match self {
            Site::GvspReceive => "gvsp_receive",
            Site::PipeRead => "pipe_read",
            Site::PipeWrite => "pipe_write",
            Site::PipeSplice => "pipe_splice",
            Site::FileWrite => "file_write",
            Site::StdoutWrite => "stdout_write",
            Site::StreamCopy => "stream_copy",
            Site::UsbAssemble => "usb_assemble",
            Site::GvspFallback => "gvsp_fallback",
            Site::GigeRowCompact => "gige_row_compact",
            Site::FfmpegUnpack => "ffmpeg_unpack",
            Site::StillTransform => "still_transform",
            Site::PreviewTransform => "preview_transform",
            Site::JetsonInput => "jetson_input",
            Site::GpuUpload => "gpu_upload",
            Site::PngEncode => "png_encode",
            Site::JpegEncode => "jpeg_encode",
            Site::FrameClone => "frame_clone",
            Site::FrameCow => "frame_cow",
            Site::ZeroFill => "zero_fill",
            Site::GvspAssemble => "gvsp_assemble",
            Site::Intermediate => "intermediate",
            Site::Crop => "crop",
            Site::StillCopy => "still_copy",
            Site::OutputCopy => "output_copy",
        }
    }
    pub(crate) fn class(self) -> Class {
        match self {
            Site::GvspReceive
            | Site::PipeRead
            | Site::PipeWrite
            | Site::PipeSplice
            | Site::FileWrite
            | Site::StdoutWrite
            | Site::StreamCopy => Class::Boundary,
            Site::UsbAssemble | Site::GvspFallback | Site::GigeRowCompact | Site::GpuUpload => {
                Class::Necessary
            }
            Site::FfmpegUnpack
            | Site::StillTransform
            | Site::PreviewTransform
            | Site::JetsonInput
            | Site::PngEncode
            | Site::JpegEncode => Class::Transform,
            Site::FrameClone
            | Site::FrameCow
            | Site::ZeroFill
            | Site::GvspAssemble
            | Site::Intermediate
            | Site::Crop
            | Site::StillCopy
            | Site::OutputCopy => Class::Avoidable,
        }
    }
}

impl Class {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Class::Boundary => "boundary",
            Class::Necessary => "necessary",
            Class::Transform => "transform",
            Class::Avoidable => "avoidable",
        }
    }
}

/// Count `bytes` copied at `site`.
#[inline]
pub(crate) fn record(site: Site, bytes: usize) {
    #[cfg(feature = "copy-audit")]
    audit::record(site, bytes);
    #[cfg(not(feature = "copy-audit"))]
    let _ = (site, bytes);
}

/// Count a frame the camera delivered.
#[inline]
pub(crate) fn frame() {
    #[cfg(feature = "copy-audit")]
    audit::FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Write what this process counted to `$CAPTUREFAB_COPY_REPORT`, when it is
/// set in a `copy-audit` build.
pub(crate) fn report() {
    report_as("worker");
}

/// `report`, for a process in `role`: its file is `ROLE-PID.json`.
pub(crate) fn report_as(role: &str) {
    #[cfg(feature = "copy-audit")]
    audit::report(role);
    #[cfg(not(feature = "copy-audit"))]
    let _ = role;
}

#[cfg(feature = "copy-audit")]
mod audit {
    use super::Site;
    use serde_json::{Map, Value, json};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) static FRAMES: AtomicU64 = AtomicU64::new(0);
    static CALLS: [AtomicU64; Site::ALL.len()] = [const { AtomicU64::new(0) }; Site::ALL.len()];
    static BYTES: [AtomicU64; Site::ALL.len()] = [const { AtomicU64::new(0) }; Site::ALL.len()];

    pub(super) fn record(site: Site, bytes: usize) {
        CALLS[site as usize].fetch_add(1, Ordering::Relaxed);
        BYTES[site as usize].fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(super) fn counts() -> Value {
        let sites: Map<String, Value> = Site::ALL
            .iter()
            .map(|&site| {
                (
                    site.name().to_owned(),
                    json!({
                        "class": site.class().name(),
                        "calls": CALLS[site as usize].load(Ordering::Relaxed),
                        "bytes": BYTES[site as usize].load(Ordering::Relaxed),
                    }),
                )
            })
            .collect();
        let pool = crate::framebuf::pool().stats();
        json!({
            "frames": FRAMES.load(Ordering::Relaxed),
            "pool": {
                "allocations": pool.allocations,
                "reuses": pool.reuses,
                "freed": pool.freed,
            },
            "sites": sites,
            "large_allocs": super::large_allocations(),
        })
    }

    pub(super) fn report(role: &str) {
        let Some(folder) = std::env::var_os("CAPTUREFAB_COPY_REPORT") else {
            return;
        };
        let path =
            std::path::Path::new(&folder).join(format!("{role}-{}.json", std::process::id()));
        // A reader never sees half a report: it is renamed into place.
        let partial = path.with_extension("json.partial");
        let written = serde_json::to_vec(&counts())
            .map_err(std::io::Error::from)
            .and_then(|bytes| std::fs::write(&partial, bytes))
            .and_then(|()| std::fs::rename(&partial, &path));
        if let Err(error) = written {
            crate::eprintln!("capturefab: copy report {}: {error}", path.display());
        }
    }
}

/// Allocations of at least this many bytes are counted: frame-sized
/// buffers, not the small ones every command makes.
#[cfg(feature = "copy-audit")]
const LARGE: usize = 256 * 1024;

#[cfg(feature = "copy-audit")]
static LARGE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "copy-audit")]
static LARGE_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "copy-audit")]
static LARGE_MOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Large allocations by size class: the power of two each rounds up to,
/// from `LARGE` on.
#[cfg(feature = "copy-audit")]
static LARGE_SIZES: [std::sync::atomic::AtomicU64; SIZE_CLASSES] =
    [const { std::sync::atomic::AtomicU64::new(0) }; SIZE_CLASSES];
#[cfg(feature = "copy-audit")]
const SIZE_CLASSES: usize = 24;

/// The size class of an allocation of `size` bytes, at least `LARGE`.
#[cfg(feature = "copy-audit")]
fn size_class(size: usize) -> usize {
    let first = LARGE.trailing_zeros();
    (size.next_power_of_two().trailing_zeros() - first).min(SIZE_CLASSES as u32 - 1) as usize
}

/// The large allocations so far: how many, their bytes, the largest, and
/// how many of each size class (keyed by the power of two they round up
/// to), which says what an interval allocated when two reports are
/// subtracted.
#[cfg(feature = "copy-audit")]
pub(crate) fn large_allocations() -> serde_json::Value {
    use std::sync::atomic::Ordering;
    let sizes: serde_json::Map<String, serde_json::Value> = LARGE_SIZES
        .iter()
        .enumerate()
        .map(|(class, count)| {
            (
                (LARGE << class).to_string(),
                count.load(Ordering::Relaxed).into(),
            )
        })
        .collect();
    serde_json::json!({
        "count": LARGE_COUNT.load(Ordering::Relaxed),
        "bytes": LARGE_BYTES.load(Ordering::Relaxed),
        "largest": LARGE_MOST.load(Ordering::Relaxed),
        "sizes": sizes,
    })
}

/// The system allocator, counting the allocations and reallocations of at
/// least [`LARGE`] bytes.
#[cfg(feature = "copy-audit")]
pub struct Counting;

#[cfg(feature = "copy-audit")]
impl Counting {
    #[inline]
    fn count(size: usize) {
        use std::sync::atomic::Ordering;
        if size >= LARGE {
            LARGE_COUNT.fetch_add(1, Ordering::Relaxed);
            LARGE_BYTES.fetch_add(size as u64, Ordering::Relaxed);
            LARGE_MOST.fetch_max(size as u64, Ordering::Relaxed);
            LARGE_SIZES[size_class(size)].fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: every method forwards to the system allocator with the caller's
// arguments, and counting touches only atomics, never the heap.
#[cfg(feature = "copy-audit")]
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        Self::count(layout.size());
        // SAFETY: the caller's contract for `alloc` is the system's.
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        Self::count(layout.size());
        // SAFETY: as for `alloc`.
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: `ptr` came from this allocator, which is the system's.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            Self::count(new_size);
        }
        // SAFETY: as for `dealloc`, with the caller's new size.
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_site_has_one_name_and_class() {
        let mut names: Vec<_> = Site::ALL.iter().map(|site| site.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Site::ALL.len());
        for (index, site) in Site::ALL.iter().enumerate() {
            assert_eq!(*site as usize, index, "{site:?}");
        }
        assert_eq!(Site::GvspAssemble.class(), Class::Avoidable);
        assert_eq!(Site::UsbAssemble.class(), Class::Necessary);
        assert_eq!(Site::FileWrite.class().name(), "boundary");
        // Without the feature, recording is free and reports nothing.
        record(Site::FrameCow, 1);
        frame();
        report();
    }
}
