//! The raw WebAssembly ABI (version [`ABI`]), used by `web/try/core.mjs`.
//!
//! # Conventions
//!
//! - Every export is `extern "C"`, prefixed `cf_`, and takes and returns
//!   only `i32`/`u32` (JavaScript numbers; read unsigned results with
//!   `>>> 0`) and `f64`. Frame ids and nanosecond timestamps travel as `f64`,
//!   exact up to 2^53.
//! - The module owns all memory. JavaScript writes input into one of two
//!   module-owned buffers obtained with [`cf_buffer`]: slot 0 receives frame
//!   pixels (for example straight from `VideoFrame.copyTo`), slot 1 receives
//!   arguments (UTF-8 strings, encoded bytes). Several arguments are packed
//!   back to back in slot 1 and their lengths passed to the export.
//! - Results are written to the output buffer: [`cf_out_ptr`] and
//!   [`cf_out_len`] locate it after the call.
//! - A status of 0 or more is success; -1 is failure, with the error
//!   message (UTF-8, the desktop app's wording) in the output buffer.
//! - Any call may grow memory, which detaches earlier `ArrayBuffer` views:
//!   create views after the call that sized the buffer, and do not call
//!   another export while an asynchronous copy writes into a view.
//! - One module-global state holds the current frame, the buffers and the
//!   Pattern camera. There are no threads; calls must not overlap.
//!
//! # Exports
//!
//! | Export | Result |
//! |---|---|
//! | `cf_abi() -> u32` | [`ABI`] |
//! | `cf_version() -> i32` | output: the crate version |
//! | `cf_buffer(slot, len) -> ptr` | a `len`-byte buffer for slot 0 or 1 (0 for another slot) |
//! | `cf_out_ptr() -> ptr`, `cf_out_len() -> u32` | the output buffer |
//! | `cf_frame_rgbx(width, height, stride, bgr, id, ts) -> i32` | slot 0 RGBA/RGBX (BGRA/BGRX when `bgr` is 1) becomes the current RGB8 frame |
//! | `cf_frame_yuv(nv12, width, height, y_off, y_stride, u_off, u_stride, v_off, v_stride, matrix, full_range, id, ts) -> i32` | slot 0 I420 (or NV12, whose UV plane is `u_off`/`u_stride`) becomes the current RGB8 frame; `matrix` 0 BT.601, 1 BT.709, 2 BT.2020 |
//! | `cf_frame_raw(format, width, height, len, id, ts) -> i32` | slot 0's first `len` bytes become the current frame as PFNC `format`, unchecked (measurements report truncation as the desktop does) |
//! | `cf_frame_width/height/format/bytes() -> u32`, `cf_frame_id/timestamp() -> f64` | the current frame (0 when there is none) |
//! | `cf_measure(kind, x, y, w, h) -> i32` | output: `inspect::measure` JSON for a pixel region; `kind` per [`CameraKind::from_code`] |
//! | `cf_inspect(kind, whole, x, y, w, h) -> i32` | output: `inspect::inspect` JSON (with `image: null`) for a normalized region, or the whole frame when `whole` is 1 |
//! | `cf_scopes(whole, x, y, w, h) -> i32` | output: binary [`frame::exposure`] of the frame or of a `crop_rect` pixel region (layout at [`cf_scopes`]) |
//! | `cf_focus(x, y, w, h) -> i32` | output: four little-endian `f32` [`frame::sharpness`] scores for a pixel region of the full frame |
//! | `cf_convert(what) -> i32` | output: 0 `rgb`, 1 `gray8` (status 1 and no output when it is not monochrome), 2 `rgba` of the current frame |
//! | `cf_encode(format) -> i32` | output: 0 raw, 1 PPM, 2 PGM file bytes |
//! | `cf_check_meta(len) -> i32` | the metadata size limit every still format checks |
//! | `cf_png_rows() -> i32` | output: Paeth-filtered scanlines for zlib; the status is the PNG color type (0 gray, 2 RGB) |
//! | `cf_png_assemble(width, height, color_type, zlib_len, meta_len, desc_len) -> i32` | slot 1 `zlib, meta, description` → output: the PNG file (`meta_len` 0: no text chunks) |
//! | `cf_jpeg_com(jpeg_len, payload_len) -> i32` | slot 1 `jpeg, payload` → output: the JPEG with a COM segment |
//! | `cf_jpeg_well_formed(len, width, height) -> u32` | 1 when slot 1 holds a complete JPEG of that size |
//! | `cf_crc32(crc, len) -> u32` | CRC-32 continued over slot 1's first `len` bytes (start from 0) |
//! | `cf_pixel_format_label(format) -> i32` | output: `Mono8`, …, or `0x…` |
//! | `cf_convertible(format) -> u32` | 1 when the format decodes |
//! | `cf_region_pixels(x, y, w, h, width, height) -> i32` | output: four `u32`, [`inspect::region_pixels`] |
//! | `cf_validate_region(x, y, w, h) -> i32` | the normalized region check |
//! | `cf_sim_reset() -> i32` | a fresh Pattern camera, stopped, with default settings |
//! | `cf_sim_info() -> i32` | output: the camera's identity JSON |
//! | `cf_sim_features() -> i32` | output: the FeatureInfo JSON array |
//! | `cf_sim_get(name_len) -> i32` | slot 1 name → output: the value JSON |
//! | `cf_sim_set(name_len, value_len) -> i32` | slot 1 `name, value` → output: `{"name","value"}` JSON |
//! | `cf_sim_start()`, `cf_sim_stop() -> i32` | acquisition control |
//! | `cf_sim_running() -> u32` | 1 while acquiring |
//! | `cf_sim_period() -> f64` | seconds until the next frame is due |
//! | `cf_sim_render(ts) -> i32` | renders the next frame into the current frame |
use crate::inspect;
use crate::json::Json;
use crate::types::{CameraKind, Frame, RGB8};
use crate::{frame, jpeg, png, simulator, yuv};
use std::cell::RefCell;

/// This ABI's version; `web/try/manifest.json` records it.
pub const ABI: u32 = 1;

#[derive(Default)]
struct State {
    buffers: [Vec<u8>; 2],
    out: Vec<u8>,
    frame: Option<Frame>,
    sim: simulator::Simulator,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| f(&mut state.borrow_mut()))
}

/// Store a result or error in the output buffer and give its status.
fn finish(state: &mut State, result: crate::Result<Vec<u8>>, ok: i32) -> i32 {
    state.out.clear();
    match result {
        Ok(bytes) => {
            state.out = bytes;
            ok
        }
        Err(message) => {
            state.out.extend_from_slice(message.as_bytes());
            -1
        }
    }
}

fn no_frame() -> String {
    "no frame yet".to_string()
}

fn ts(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value as u64
    } else {
        0
    }
}

/// Slot 1 split into consecutive parts of the given lengths, or an error
/// when they exceed it.
fn parts<const N: usize>(slot: &[u8], lengths: [u32; N]) -> crate::Result<[&[u8]; N]> {
    let total: u64 = lengths.iter().map(|&l| u64::from(l)).sum();
    ensure!(
        total <= slot.len() as u64,
        "arguments exceed the argument buffer"
    );
    let mut at = 0;
    Ok(lengths.map(|len| {
        let part = &slot[at..at + len as usize];
        at += len as usize;
        part
    }))
}

fn text(bytes: &[u8]) -> crate::Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| "text is not valid UTF-8".to_string())
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_abi() -> u32 {
    ABI
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_version() -> i32 {
    with(|s| finish(s, Ok(env!("CARGO_PKG_VERSION").as_bytes().to_vec()), 0))
}

/// A module-owned buffer of exactly `len` bytes for slot 0 (frame pixels)
/// or slot 1 (arguments); null for any other slot. Earlier contents up to
/// the new length are kept.
#[unsafe(no_mangle)]
pub extern "C" fn cf_buffer(slot: u32, len: u32) -> *mut u8 {
    with(|s| match s.buffers.get_mut(slot as usize) {
        Some(buffer) => {
            buffer.resize(len as usize, 0);
            buffer.as_mut_ptr()
        }
        None => std::ptr::null_mut(),
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_out_ptr() -> *const u8 {
    with(|s| s.out.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_out_len() -> u32 {
    with(|s| s.out.len() as u32)
}

fn set_frame(s: &mut State, frame: crate::Result<Frame>) -> i32 {
    match frame {
        Ok(frame) => {
            s.frame = Some(frame);
            s.out.clear();
            0
        }
        Err(message) => finish(s, Err(message), 0),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_rgbx(
    width: u32,
    height: u32,
    stride: u32,
    bgr: u32,
    id: f64,
    timestamp_ns: f64,
) -> i32 {
    with(|s| {
        let frame =
            yuv::rgbx_to_rgb(&s.buffers[0], width, height, stride as usize, bgr == 1).map(|data| {
                Frame {
                    id: ts(id),
                    width,
                    height,
                    pixel_format: RGB8,
                    timestamp_ns: ts(timestamp_ns),
                    data,
                }
            });
        set_frame(s, frame)
    })
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn cf_frame_yuv(
    nv12: u32,
    width: u32,
    height: u32,
    y_offset: u32,
    y_stride: u32,
    u_offset: u32,
    u_stride: u32,
    v_offset: u32,
    v_stride: u32,
    matrix: u32,
    full_range: u32,
    id: f64,
    timestamp_ns: f64,
) -> i32 {
    let plane = |offset: u32, stride: u32| yuv::Plane {
        offset: offset as usize,
        stride: stride as usize,
    };
    let layout = if nv12 == 1 {
        yuv::Layout::Nv12 {
            uv: plane(u_offset, u_stride),
        }
    } else {
        yuv::Layout::I420 {
            u: plane(u_offset, u_stride),
            v: plane(v_offset, v_stride),
        }
    };
    with(|s| {
        let frame = yuv::yuv420_to_rgb(
            &s.buffers[0],
            width,
            height,
            plane(y_offset, y_stride),
            layout,
            yuv::Matrix::from_code(matrix),
            full_range == 1,
        )
        .map(|data| Frame {
            id: ts(id),
            width,
            height,
            pixel_format: RGB8,
            timestamp_ns: ts(timestamp_ns),
            data,
        });
        set_frame(s, frame)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_raw(
    pixel_format: u32,
    width: u32,
    height: u32,
    len: u32,
    id: f64,
    timestamp_ns: f64,
) -> i32 {
    with(|s| {
        let len = (len as usize).min(s.buffers[0].len());
        // Hand the pixels over and keep the old frame's allocation for the
        // next copy.
        let mut data = std::mem::take(&mut s.buffers[0]);
        data.truncate(len);
        if let Some(old) = s.frame.take() {
            s.buffers[0] = old.data;
            s.buffers[0].clear();
        }
        set_frame(
            s,
            Ok(Frame {
                id: ts(id),
                width,
                height,
                pixel_format,
                timestamp_ns: ts(timestamp_ns),
                data,
            }),
        )
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_width() -> u32 {
    with(|s| s.frame.as_ref().map_or(0, |f| f.width))
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_height() -> u32 {
    with(|s| s.frame.as_ref().map_or(0, |f| f.height))
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_format() -> u32 {
    with(|s| s.frame.as_ref().map_or(0, |f| f.pixel_format))
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_bytes() -> u32 {
    with(|s| s.frame.as_ref().map_or(0, |f| f.data.len() as u32))
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_id() -> f64 {
    with(|s| s.frame.as_ref().map_or(0.0, |f| f.id as f64))
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_frame_timestamp() -> f64 {
    with(|s| s.frame.as_ref().map_or(0.0, |f| f.timestamp_ns as f64))
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_measure(kind: u32, x: u32, y: u32, width: u32, height: u32) -> i32 {
    with(|s| {
        let result = s.frame.as_ref().ok_or_else(no_frame).map(|frame| {
            inspect::measure(frame, CameraKind::from_code(kind), [x, y, width, height])
                .to_json()
                .into_bytes()
        });
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_inspect(
    kind: u32,
    whole: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> i32 {
    with(|s| {
        let result = s.frame.as_ref().ok_or_else(no_frame).and_then(|frame| {
            let region = if whole == 1 {
                None
            } else {
                let region = [x, y, width, height];
                inspect::validate_region(region)?;
                Some(region)
            };
            inspect::inspect(frame, CameraKind::from_code(kind), region)
                .map(|inspection| inspection.to_json().into_bytes())
        });
        finish(s, result, 0)
    })
}

/// Binary exposure statistics, all little-endian:
///
/// | Offset | Content |
/// |---|---|
/// | 0 | samples (`u32`, the luma histogram's total) |
/// | 4 | 1 when monochrome (`u32`) |
/// | 8 | mean, shadows, highlights (`f32` each) |
/// | 20 | luma histogram, 64 × `u32` |
/// | 276 | red, green, blue histograms, 3 × 64 × `u32` (zero when monochrome) |
///
/// 1044 bytes in all. `whole` 1 measures the whole frame as the desktop's
/// histogram does; otherwise the `crop_rect` of the pixel region.
#[unsafe(no_mangle)]
pub extern "C" fn cf_scopes(whole: u32, x: u32, y: u32, width: u32, height: u32) -> i32 {
    with(|s| {
        let result = s.frame.as_ref().ok_or_else(no_frame).and_then(|frame| {
            let exposure = if whole == 1 {
                frame::exposure(frame)?
            } else {
                let rect = inspect::crop_rect(frame, [x, y, width, height]);
                let view = inspect::view(frame, rect)?;
                frame::exposure(&view)?
            };
            let mut out = Vec::with_capacity(1044);
            let samples: u32 = exposure.luma.iter().sum();
            out.extend_from_slice(&samples.to_le_bytes());
            out.extend_from_slice(&u32::from(exposure.channels.is_none()).to_le_bytes());
            for v in [exposure.mean, exposure.shadows, exposure.highlights] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            for n in exposure.luma {
                out.extend_from_slice(&n.to_le_bytes());
            }
            for bins in exposure.channels.unwrap_or([[0; 64]; 3]) {
                for n in bins {
                    out.extend_from_slice(&n.to_le_bytes());
                }
            }
            Ok(out)
        });
        finish(s, result, 0)
    })
}

/// Four `f32`: laplacian, tenengrad, brenner, variance. The region is not
/// widened to whole Bayer quads, as on the desktop's focus card.
#[unsafe(no_mangle)]
pub extern "C" fn cf_focus(x: u32, y: u32, width: u32, height: u32) -> i32 {
    with(|s| {
        let result = s.frame.as_ref().ok_or_else(no_frame).and_then(|frame| {
            let score = frame::sharpness(frame, [x, y, width, height])?;
            Ok([
                score.laplacian,
                score.tenengrad,
                score.brenner,
                score.variance,
            ]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect())
        });
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_convert(what: u32) -> i32 {
    with(|s| {
        let Some(frame) = s.frame.as_ref() else {
            return finish(s, Err(no_frame()), 0);
        };
        match what {
            0 => {
                let result = frame::rgb(frame);
                finish(s, result, 0)
            }
            1 => match frame::gray8(frame) {
                Ok(Some(gray)) => finish(s, Ok(gray), 0),
                Ok(None) => finish(s, Ok(Vec::new()), 1),
                Err(error) => finish(s, Err(error), 0),
            },
            _ => {
                let result = frame::rgba(frame);
                finish(s, result, 0)
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_encode(format: u32) -> i32 {
    with(|s| {
        let result = s.frame.as_ref().ok_or_else(no_frame).and_then(|frame| {
            frame::encode(
                frame,
                match format {
                    0 => "raw",
                    1 => "ppm",
                    2 => "pgm",
                    _ => "unknown",
                },
            )
        });
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_check_meta(len: u32) -> i32 {
    with(|s| {
        let result = frame::check_meta(len as usize).map(|()| Vec::new());
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_png_rows() -> i32 {
    with(|s| {
        let result = s.frame.as_ref().ok_or_else(no_frame).and_then(|frame| {
            let (color, samples) = png::samples(frame)?;
            let channels = if color == png::GRAYSCALE { 1 } else { 3 };
            Ok((
                color,
                png::filtered_rows(&samples, frame.width, frame.height, channels),
            ))
        });
        match result {
            Ok((color, rows)) => finish(s, Ok(rows), i32::from(color)),
            Err(error) => finish(s, Err(error), 0),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_png_assemble(
    width: u32,
    height: u32,
    color_type: u32,
    zlib_len: u32,
    meta_len: u32,
    description_len: u32,
) -> i32 {
    with(|s| {
        let result = parts(&s.buffers[1], [zlib_len, meta_len, description_len]).and_then(
            |[zlib, meta, description]| {
                let meta = (meta_len > 0).then(|| text(meta)).transpose()?;
                let description = (description_len > 0)
                    .then(|| text(description))
                    .transpose()?;
                png::assemble(width, height, color_type as u8, zlib, meta, description)
            },
        );
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_jpeg_com(jpeg_len: u32, payload_len: u32) -> i32 {
    with(|s| {
        let result = parts(&s.buffers[1], [jpeg_len, payload_len])
            .and_then(|[jpeg, payload]| jpeg::insert_com(jpeg, payload));
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_jpeg_well_formed(len: u32, width: u32, height: u32) -> u32 {
    with(|s| {
        parts(&s.buffers[1], [len])
            .is_ok_and(|[jpeg]| jpeg::well_formed(jpeg, width, height))
            .into()
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_crc32(crc: u32, len: u32) -> u32 {
    with(|s| {
        let len = (len as usize).min(s.buffers[1].len());
        crate::crc32::update(crc, &s.buffers[1][..len])
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_pixel_format_label(pixel_format: u32) -> i32 {
    with(|s| {
        let label = inspect::pixel_format_label(pixel_format).into_bytes();
        finish(s, Ok(label), 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_convertible(pixel_format: u32) -> u32 {
    frame::convertible(pixel_format).into()
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_region_pixels(x: f64, y: f64, w: f64, h: f64, width: u32, height: u32) -> i32 {
    with(|s| {
        let px = inspect::region_pixels([x, y, w, h], width, height);
        finish(s, Ok(px.iter().flat_map(|v| v.to_le_bytes()).collect()), 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_validate_region(x: f64, y: f64, w: f64, h: f64) -> i32 {
    with(|s| {
        let result = inspect::validate_region([x, y, w, h]).map(|()| Vec::new());
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_reset() -> i32 {
    with(|s| {
        s.sim = simulator::Simulator::default();
        finish(s, Ok(Vec::new()), 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_info() -> i32 {
    with(|s| finish(s, Ok(simulator::info_json().into_bytes()), 0))
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_features() -> i32 {
    with(|s| {
        let json = s.sim.features_json().into_bytes();
        finish(s, Ok(json), 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_get(name_len: u32) -> i32 {
    with(|s| {
        let State { buffers, sim, .. } = &mut *s;
        let result = parts(&buffers[1], [name_len])
            .and_then(|[name]| text(name).map(str::to_owned))
            .and_then(|name| sim.get(&name))
            .map(|value| {
                let mut json = Json::default();
                value.write(&mut json);
                json.finish().into_bytes()
            });
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_set(name_len: u32, value_len: u32) -> i32 {
    with(|s| {
        let State { buffers, sim, .. } = &mut *s;
        let result = parts(&buffers[1], [name_len, value_len])
            .and_then(|[name, value]| Ok((text(name)?.to_owned(), text(value)?.to_owned())))
            .and_then(|(name, value)| {
                let read = sim.set(&name, &value)?;
                let mut json = Json::default();
                json.begin_object();
                json.field_str("name", &name);
                json.key("value");
                read.write(&mut json);
                json.end_object();
                Ok(json.finish().into_bytes())
            });
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_start() -> i32 {
    with(|s| {
        let result = s.sim.start().map(|()| Vec::new());
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_stop() -> i32 {
    with(|s| {
        let result = s.sim.stop().map(|()| Vec::new());
        finish(s, result, 0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_running() -> u32 {
    with(|s| s.sim.running().into())
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_period() -> f64 {
    with(|s| s.sim.period())
}

#[unsafe(no_mangle)]
pub extern "C" fn cf_sim_render(timestamp_ns: f64) -> i32 {
    with(|s| match s.sim.render(ts(timestamp_ns)) {
        Ok(frame) => set_frame(s, Ok(frame)),
        Err(error) => finish(s, Err(error), 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out() -> Vec<u8> {
        with(|s| s.out.clone())
    }

    fn put(slot: u32, bytes: &[u8]) {
        let ptr = cf_buffer(slot, bytes.len() as u32);
        assert!(!ptr.is_null());
        with(|s| s.buffers[slot as usize].copy_from_slice(bytes));
    }

    #[test]
    fn frames_measure_and_encode_through_the_abi() {
        assert_eq!(cf_abi(), ABI);
        assert!(cf_buffer(2, 1).is_null());
        put(0, &[10, 20, 30, 255, 40, 50, 60, 255]);
        assert_eq!(cf_frame_rgbx(2, 1, 8, 0, 3.0, 5e9), 0);
        assert_eq!(
            (cf_frame_width(), cf_frame_height(), cf_frame_bytes()),
            (2, 1, 6)
        );
        assert_eq!((cf_frame_id(), cf_frame_timestamp()), (3.0, 5e9));
        assert_eq!(cf_frame_format(), RGB8);
        assert_eq!(cf_convert(0), 0);
        assert_eq!(out(), [10, 20, 30, 40, 50, 60]);
        assert_eq!(cf_convert(1), 1);
        assert_eq!(cf_encode(2), -1);
        assert_eq!(out(), b"PGM requires a monochrome image; use PNG or PPM");
        assert_eq!(cf_measure(1, 0, 0, 2, 1), 0);
        assert!(String::from_utf8(out()).unwrap().contains("\"focus\":null"));
        assert_eq!(cf_focus(0, 0, 2, 1), -1);
        assert_eq!(out(), b"frame is too small to measure sharpness");
        assert_eq!(cf_scopes(1, 0, 0, 0, 0), 0);
        let scopes = out();
        assert_eq!(scopes.len(), 1044);
        assert_eq!(u32::from_le_bytes(scopes[..4].try_into().unwrap()), 2);
        assert_eq!(cf_inspect(1, 0, 0.0, 0.0, 2.0, 1.0), -1);
        assert!(
            String::from_utf8(out())
                .unwrap()
                .starts_with("region must be")
        );
        assert_eq!(cf_png_rows(), 2);
        assert_eq!(out(), [4, 10, 20, 30, 30, 30, 30]);
        put(1, b"zzmeta");
        assert_eq!(cf_png_assemble(2, 1, 2, 2, 4, 0), 0);
        assert!(out().starts_with(&png::SIGNATURE));
        assert_eq!(cf_png_assemble(2, 1, 2, 2, 5, 0), -1);
        assert_eq!(out(), b"arguments exceed the argument buffer");
        put(1, b"123456789");
        assert_eq!(cf_crc32(0, 9), 0xCBF4_3926);
        assert_eq!(cf_crc32(cf_crc32(0, 4), 0), cf_crc32(0, 4));
    }

    #[test]
    fn the_pattern_camera_runs_through_the_abi() {
        assert_eq!(cf_sim_reset(), 0);
        put(1, b"Width9999");
        assert_eq!(cf_sim_set(5, 4), -1);
        assert_eq!(
            String::from_utf8(out()).unwrap(),
            "cannot set Width to \"9999\": Width violates Max=1920; enter 1920 or less"
        );
        put(1, b"PixelFormatRGB8");
        assert_eq!(cf_sim_set(11, 4), 0);
        assert_eq!(out(), br#"{"name":"PixelFormat","value":"RGB8"}"#);
        assert_eq!(cf_sim_render(0.0), -1);
        assert_eq!(out(), b"camera is not streaming");
        assert_eq!(cf_sim_start(), 0);
        assert_eq!(cf_sim_running(), 1);
        assert_eq!(cf_sim_render(1e6), 0);
        assert_eq!(
            (cf_frame_width(), cf_frame_format(), cf_frame_id()),
            (640, RGB8, 1.0)
        );
        assert_eq!(cf_sim_period(), 1.0 / 30.0);
        assert_eq!(cf_sim_features(), 0);
        assert!(
            String::from_utf8(out())
                .unwrap()
                .starts_with("[{\"name\":\"AcquisitionFrameRate\"")
        );
        assert_eq!(cf_sim_stop(), 0);
        put(1, b"Gain");
        assert_eq!(cf_sim_get(4), 0);
        assert_eq!(out(), b"0.0");
    }
}
