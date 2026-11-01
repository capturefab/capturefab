//! Port of the native `src/frame.rs`: PFNC names, bounded conversions to
//! RGB/gray/RGBA, sparse exposure statistics, focus measures and still encoders.
//!
//! Bayer interpolation follows the same rule as the native converter and runs
//! sequentially because this WebAssembly target has no worker threads.
use crate::Result;
use crate::types::{Frame, MONO8, RGB8};
use std::borrow::Cow;

mod formats;

pub fn pixel_format_name(format: u32) -> &'static str {
    match format {
        MONO8 => "Mono8",
        RGB8 => "RGB8",
        0x0218_0015 => "BGR8",
        0x0220_0016 => "RGBa8",
        0x0220_0017 => "BGRa8",
        0x0110_0003 => "Mono10",
        0x0110_0005 => "Mono12",
        0x0110_0025 => "Mono14",
        0x0110_0007 => "Mono16",
        0x010a_0046 => "Mono10p",
        0x010c_0047 => "Mono12p",
        0x010c_0004 => "Mono10Packed",
        0x010c_0006 => "Mono12Packed",
        0x010e_0104 => "Mono14p",
        0x010e_0105 => "BayerGR14p",
        0x010e_0106 => "BayerRG14p",
        0x010e_0107 => "BayerGB14p",
        0x010e_0108 => "BayerBG14p",
        0x0110_0109 => "BayerGR14",
        0x0110_010a => "BayerRG14",
        0x0110_010b => "BayerGB14",
        0x0110_010c => "BayerBG14",
        0x0108_00b1 => "Coord3D_C8",
        0x0110_00b8 => "Coord3D_C16",
        0x0120_00bf => "Coord3D_C32f",
        0x0108_00c6 => "Confidence8",
        0x0110_00c7 => "Confidence16",
        0x0108_0008 => "BayerGR8",
        0x0108_0009 => "BayerRG8",
        0x0108_000a => "BayerGB8",
        0x0108_000b => "BayerBG8",
        0x0110_000c => "BayerGR10",
        0x0110_000d => "BayerRG10",
        0x0110_000e => "BayerGB10",
        0x0110_000f => "BayerBG10",
        0x0110_0010 => "BayerGR12",
        0x0110_0011 => "BayerRG12",
        0x0110_0012 => "BayerGB12",
        0x0110_0013 => "BayerBG12",
        0x0110_002e => "BayerGR16",
        0x0110_002f => "BayerRG16",
        0x0110_0030 => "BayerGB16",
        0x0110_0031 => "BayerBG16",
        0x010a_0056 => "BayerGR10p",
        0x010a_0058 => "BayerRG10p",
        0x010a_0054 => "BayerGB10p",
        0x010a_0052 => "BayerBG10p",
        0x010c_0057 => "BayerGR12p",
        0x010c_0059 => "BayerRG12p",
        0x010c_0055 => "BayerGB12p",
        0x010c_0053 => "BayerBG12p",
        0x010c_0026 => "BayerGR10Packed",
        0x010c_0027 => "BayerRG10Packed",
        0x010c_0028 => "BayerGB10Packed",
        0x010c_0029 => "BayerBG10Packed",
        0x010c_002a => "BayerGR12Packed",
        0x010c_002b => "BayerRG12Packed",
        0x010c_002c => "BayerGB12Packed",
        0x010c_002d => "BayerBG12Packed",
        0x0210_001f => "YUV422_8_UYVY",
        0x0210_0032 => "YUV422_8",
        0x0210_003b => "YCbCr422_8",
        0x0210_0043 => "YCbCr422_8_CbYCrY",
        v => formats::name(v).unwrap_or("Unknown"),
    }
}
/// Bytes in one row of a `width`-pixel image: rows of packed formats are
/// padded to a whole byte.
pub fn row_bytes(width: u32, pixel_format: u32) -> usize {
    // Count bits in 64 bits so a valid 64 Mi-pixel row of 64-bit samples
    // does not overflow wasm's usize before division into bytes.
    let bytes = (u64::from(width) * u64::from((pixel_format >> 16) & 0xff)).div_ceil(8);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}
/// How a packed format stores its samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Packing {
    /// PFNC lsb packing (the `p` formats): samples follow each other from
    /// the least significant bit of the first byte.
    Lsb,
    /// GigE Vision `Packed`: two samples in three bytes, the high bits of
    /// each in the outer bytes and both low parts in the middle one, the
    /// first sample's in its low nibble.
    GigE,
}
/// The unpacked 16-bit format of a packed format's family, its sample bits
/// and its packing.
pub(crate) fn packing(pixel_format: u32) -> Option<(u32, u32, Packing)> {
    use Packing::{GigE, Lsb};
    Some(match pixel_format {
        0x010a_0046 => (0x0110_0003, 10, Lsb),
        0x010c_0047 => (0x0110_0005, 12, Lsb),
        0x010c_0004 => (0x0110_0003, 10, GigE),
        0x010c_0006 => (0x0110_0005, 12, GigE),
        0x010e_0104 => (0x0110_0025, 14, Lsb),
        v @ 0x010e_0105..=0x010e_0108 => (v - 0x010e_0105 + 0x0110_0109, 14, Lsb),
        0x010a_0056 => (0x0110_000c, 10, Lsb),
        0x010a_0058 => (0x0110_000d, 10, Lsb),
        0x010a_0054 => (0x0110_000e, 10, Lsb),
        0x010a_0052 => (0x0110_000f, 10, Lsb),
        0x010c_0057 => (0x0110_0010, 12, Lsb),
        0x010c_0059 => (0x0110_0011, 12, Lsb),
        0x010c_0055 => (0x0110_0012, 12, Lsb),
        0x010c_0053 => (0x0110_0013, 12, Lsb),
        v @ 0x010c_0026..=0x010c_0029 => (v - 0x010c_0026 + 0x0110_000c, 10, GigE),
        v @ 0x010c_002a..=0x010c_002d => (v - 0x010c_002a + 0x0110_0010, 12, GigE),
        _ => return None,
    })
}
/// YUV 4:2:2 formats: the byte offsets of the first pixel's luma, the shared
/// Cb (U), the second pixel's luma and the shared Cr (V) in each two pixels.
fn yuv422(pixel_format: u32) -> Option<[usize; 4]> {
    match pixel_format {
        0x0210_001f | 0x0210_0043 => Some([1, 0, 3, 2]),
        0x0210_0032 | 0x0210_003b => Some([0, 1, 2, 3]),
        _ => None,
    }
}
/// The format `unpacked` turns `pixel_format` into.
pub fn unpacked_format(pixel_format: u32) -> u32 {
    match pixel_format {
        0x0220_0016 | 0x0220_0017 => RGB8,
        v if yuv422(v).is_some() => RGB8,
        v => formats::unpacked_format(v)
            .unwrap_or_else(|| packing(v).map_or(v, |(unpacked, ..)| unpacked)),
    }
}
/// `frame` with whole samples at their native depth: packed 10- and 12-bit
/// samples as the unpacked 16-bit format of their family, YUV 4:2:2 and
/// RGBa8/BGRa8 as RGB8, and any other format as it is.
pub fn unpacked(frame: &Frame) -> Result<Cow<'_, Frame>> {
    let pixel_format = unpacked_format(frame.pixel_format);
    if pixel_format == frame.pixel_format {
        return Ok(Cow::Borrowed(frame));
    }
    let pixels = checked_pixels(frame)?;
    if let Some(converted) = formats::unpacked(frame)? {
        return Ok(Cow::Owned(converted));
    }
    let (width, height) = (frame.width as usize, frame.height as usize);
    let row = row_bytes(frame.width, frame.pixel_format);
    ensure!(
        frame.data.len() >= row * height,
        "truncated {} frame",
        pixel_format_name(frame.pixel_format)
    );
    let rows = frame.data[..row * height].chunks_exact(row);
    let data = if let Some((_, bits, packing)) = packing(frame.pixel_format) {
        let mut data = vec![0; pixels * 2];
        for (line, out) in rows.zip(data.chunks_exact_mut(width * 2)) {
            unpack_row(line, bits, packing, out.as_chunks_mut::<2>().0);
        }
        data
    } else if let Some([y0, cb, y1, cr]) = yuv422(frame.pixel_format) {
        let mut data = Vec::with_capacity(pixels * 3);
        for line in rows {
            // An odd last pixel has no Cr; it is taken as neutral.
            for (x, bytes) in (0..width).step_by(2).zip(line.chunks(4)) {
                let mut pair = [128; 4];
                pair[..bytes.len()].copy_from_slice(bytes);
                data.extend(ycbcr(pair[y0], pair[cb], pair[cr]));
                if x + 1 < width {
                    data.extend(ycbcr(pair[y1], pair[cb], pair[cr]));
                }
            }
        }
        data
    } else {
        let order = if frame.pixel_format == 0x0220_0016 {
            [0, 1, 2]
        } else {
            [2, 1, 0]
        };
        let source = frame.data[..pixels * 4].as_chunks::<4>().0;
        source.iter().flat_map(|p| order.map(|c| p[c])).collect()
    };
    Ok(Cow::Owned(Frame {
        pixel_format,
        data,
        ..*frame
    }))
}
/// One row of packed samples as little-endian 16-bit samples, one per
/// element of `out`; a short last group is completed with zeros.
fn unpack_row(row: &[u8], bits: u32, packing: Packing, out: &mut [[u8; 2]]) {
    fn groups<const BYTES: usize, const SAMPLES: usize>(
        row: &[u8],
        out: &mut [[u8; 2]],
        unpack: impl Fn([u16; BYTES]) -> [u16; SAMPLES],
    ) {
        for (samples, bytes) in out.chunks_mut(SAMPLES).zip(row.chunks(BYTES)) {
            let mut group = [0; BYTES];
            for (value, &byte) in group.iter_mut().zip(bytes) {
                *value = u16::from(byte);
            }
            for (sample, value) in samples.iter_mut().zip(unpack(group)) {
                *sample = value.to_le_bytes();
            }
        }
    }
    match (packing, bits) {
        (Packing::Lsb, 14) => {
            for (x, sample) in out.iter_mut().enumerate() {
                *sample = packed_sample(row, x, bits, packing).to_le_bytes();
            }
        }
        (Packing::Lsb, 10) => groups(row, out, |[b0, b1, b2, b3, b4]| {
            [
                b0 | (b1 & 0x03) << 8,
                b1 >> 2 | (b2 & 0x0f) << 6,
                b2 >> 4 | (b3 & 0x3f) << 4,
                b3 >> 6 | b4 << 2,
            ]
        }),
        (Packing::Lsb, _) => groups(row, out, |[b0, b1, b2]| {
            [b0 | (b1 & 0x0f) << 8, b1 >> 4 | b2 << 4]
        }),
        (Packing::GigE, 10) => groups(row, out, |[b0, b1, b2]| {
            [b0 << 2 | b1 & 0x03, b2 << 2 | b1 >> 4 & 0x03]
        }),
        (Packing::GigE, _) => groups(row, out, |[b0, b1, b2]| {
            [b0 << 4 | b1 & 0x0f, b2 << 4 | b1 >> 4]
        }),
    }
}

/// One native sample without expanding the image. Packed rows start on a
/// fresh byte; partial final groups never read into the next row.
fn packed_sample(row: &[u8], x: usize, bits: u32, packing: Packing) -> u16 {
    match packing {
        Packing::Lsb => {
            let bit = x * bits as usize;
            let at = bit / 8;
            let word = u32::from(row[at])
                | (u32::from(*row.get(at + 1).unwrap_or(&0)) << 8)
                | (u32::from(*row.get(at + 2).unwrap_or(&0)) << 16);
            ((word >> (bit % 8)) & ((1 << bits) - 1)) as u16
        }
        Packing::GigE => {
            let at = x / 2 * 3;
            let shift = bits - 8;
            let high = u16::from(row[at + if x.is_multiple_of(2) { 0 } else { 2 }]);
            let low = u16::from(row[at + 1]) >> (x % 2 * 4);
            (high << shift) | (low & ((1 << shift) - 1))
        }
    }
}

/// Validate the original sensor layout, before any conversion or allocation.
pub(crate) fn validate_source(frame: &Frame) -> Result<()> {
    checked_pixels(frame)?;
    ensure!(
        convertible(frame.pixel_format),
        "unsupported PFNC format 0x{:08x}; capture with --format raw to preserve bytes",
        frame.pixel_format
    );
    if let Some(result) = formats::validate(frame) {
        return result;
    }
    let length = row_bytes(frame.width, frame.pixel_format)
        .checked_mul(frame.height as usize)
        .ok_or_else(|| "image size overflow".to_string())?;
    ensure!(
        frame.data.len() >= length,
        "truncated {} frame",
        pixel_format_name(frame.pixel_format)
    );
    Ok(())
}

/// Display sample of a monochrome or Bayer source at its native packing.
fn sample8(frame: &Frame, x: usize, y: usize) -> u8 {
    let format = frame.pixel_format;
    if matches!(
        format,
        MONO8 | 0x0108_00b1 | 0x0108_00c6 | 0x0108_0008..=0x0108_000b
    ) {
        return frame.data[y * frame.width as usize + x];
    }
    let unpacked = packing(format).map_or(format, |(v, ..)| v);
    let shift = wide_bayer(unpacked)
        .map(|(_, shift)| shift)
        .or_else(|| unpacked_shift(unpacked));
    let Some(shift) = shift else {
        return frame.data[y * frame.width as usize + x];
    };
    (native_sample(frame, x, y) >> shift).min(255) as u8
}

/// A whole monochrome/Bayer sample at native precision, from a validated frame.
pub(crate) fn native_sample(frame: &Frame, x: usize, y: usize) -> u16 {
    let format = frame.pixel_format;
    if let Some((_, bits, packing)) = packing(format) {
        let stride = row_bytes(frame.width, format);
        packed_sample(&frame.data[y * stride..(y + 1) * stride], x, bits, packing)
    } else if ((format >> 16) & 0xff) == 8 {
        u16::from(frame.data[y * frame.width as usize + x])
    } else {
        let at = (y * frame.width as usize + x) * 2;
        u16::from_le_bytes([frame.data[at], frame.data[at + 1]])
    }
}

/// Native color samples and their full scale for precision-preserving metering.
pub(crate) fn native_color(frame: &Frame, x: usize, y: usize) -> Option<([u16; 3], u32)> {
    formats::native_color(frame, x, y)
}
/// Full-range BT.601 Y'CbCr as R'G'B', as PFNC defines its YCbCr formats
/// and, with U as Cb and V as Cr, its YUV ones; 16-bit fixed point as JFIF
/// decoders compute it.
pub(crate) fn ycbcr(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let (y, cb, cr) = (i32::from(y), i32::from(cb) - 128, i32::from(cr) - 128);
    [
        y + ((91_881 * cr + 32_768) >> 16),
        y + ((-22_554 * cb - 46_802 * cr + 32_768) >> 16),
        y + ((116_130 * cb + 32_768) >> 16),
    ]
    .map(|v| v.clamp(0, 255) as u8)
}
/// The Bayer8 format with the color filter of an unpacked 10-, 12- or 16-bit
/// mosaic, and the shift that brings its samples to 8 bits.
fn wide_bayer(pixel_format: u32) -> Option<(u32, u32)> {
    let (first, shift) = match pixel_format {
        0x0110_000c..=0x0110_000f => (0x0110_000c, 2),
        0x0110_0010..=0x0110_0013 => (0x0110_0010, 4),
        0x0110_0109..=0x0110_010c => (0x0110_0109, 6),
        0x0110_002e..=0x0110_0031 => (0x0110_002e, 8),
        _ => return None,
    };
    Some((0x0108_0008 + pixel_format - first, shift))
}
/// The Bayer8 format with the color filter of any Bayer format.
fn bayer8(pixel_format: u32) -> Option<u32> {
    match unpacked_format(pixel_format) {
        v @ 0x0108_0008..=0x0108_000b => Some(v),
        v => wide_bayer(v).map(|(bayer8, _)| bayer8),
    }
}
/// The format `eight_bit` turns `pixel_format` into.
fn eight_bit_format(pixel_format: u32) -> u32 {
    let unpacked = unpacked_format(pixel_format);
    wide_bayer(unpacked).map_or(unpacked, |(bayer8, _)| bayer8)
}
/// `frame` in a format the converters read directly: `unpacked`, then a
/// 10- to 16-bit Bayer mosaic reduced to Bayer8.
fn eight_bit(frame: &Frame) -> Result<Cow<'_, Frame>> {
    let Some((pixel_format, _)) = wide_bayer(unpacked_format(frame.pixel_format)) else {
        return unpacked(frame);
    };
    validate_source(frame)?;
    let pixels = checked_pixels(frame)?;
    let width = frame.width as usize;
    let data = (0..pixels)
        .map(|i| sample8(frame, i % width, i / width))
        .collect();
    Ok(Cow::Owned(Frame {
        pixel_format,
        data,
        ..*frame
    }))
}
/// One-channel formats: the 8-bit monochrome family, the unpacked 16-bit one
/// of [`unpacked_shift`], and the packed formats that unpack into it.
pub(crate) fn monochrome(pixel_format: u32) -> bool {
    let pixel_format = unpacked_format(pixel_format);
    matches!(pixel_format, MONO8 | 0x0108_00b1 | 0x0108_00c6)
        || unpacked_shift(pixel_format).is_some()
}
fn gray(out: &mut [u8], value: impl Fn(usize) -> u8) {
    for i in 0..out.len() / 3 {
        let v = value(i);
        (out[3 * i], out[3 * i + 1], out[3 * i + 2]) = (v, v, v);
    }
}
pub fn rgb(frame: &Frame) -> Result<Vec<u8>> {
    validate_source(frame)?;
    // Bayer conversion owns its output; avoid allocating an unused RGB image
    // before dispatching to the demosaic.
    if is_bayer(frame.pixel_format) || unpacked_format(frame.pixel_format) != frame.pixel_format {
        return Ok(convert(frame, |p| p)?.into_flattened());
    }
    let frame = &*eight_bit(frame)?;
    let pixels = checked_pixels(frame)?;
    // Flat byte stores vectorize as interleaved 3-byte stores, which `[u8; 3]`
    // elements from the generic converter do not; only Bayer is shared.
    let mut out = vec![0; pixels * 3];
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => {
            ensure!(frame.data.len() >= pixels, "truncated Mono8 frame");
            let data = &frame.data[..pixels];
            gray(&mut out, |i| data[i]);
        }
        RGB8 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            out.copy_from_slice(&frame.data[..pixels * 3]);
        }
        0x0218_0015 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let bgr = frame.data.as_chunks::<3>().0;
            for (dst, p) in out.as_chunks_mut::<3>().0.iter_mut().zip(bgr) {
                *dst = [p[2], p[1], p[0]];
            }
        }
        0x0110_0003 | 0x0110_0005 | 0x0110_0025 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            ensure!(
                frame.data.len() >= pixels * 2,
                "truncated unpacked monochrome frame"
            );
            let shift = unpacked_shift(frame.pixel_format).unwrap_or(8);
            let data = &frame.data[..pixels * 2];
            gray(&mut out, |i| {
                (u16::from_le_bytes([data[2 * i], data[2 * i + 1]]) >> shift).min(255) as u8
            });
        }
        _ => return Ok(convert(frame, |p| p)?.into_flattened()),
    }
    Ok(out)
}
/// How far an unpacked 16-bit monochrome sample shifts right to its top 8
/// bits: Mono10 keeps bits 9-2, Mono12 bits 11-4, Mono14 bits 13-6, and the
/// full-range 16-bit formats their high byte. None for every other format.
fn unpacked_shift(format: u32) -> Option<u32> {
    match format {
        0x0110_0003 => Some(2),
        0x0110_0005 => Some(4),
        0x0110_0025 => Some(6),
        0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => Some(8),
        _ => None,
    }
}
/// One 8-bit sample per pixel for the monochrome family, packed formats
/// unpacked first, with the same bit choice as [`rgb`]; None for color,
/// Bayer, YUV and RGBa formats, which stay RGB.
pub fn gray8(frame: &Frame) -> Result<Option<Vec<u8>>> {
    let pixels = checked_pixels(frame)?;
    if !monochrome(frame.pixel_format) {
        return Ok(None);
    }
    if unpacked_format(frame.pixel_format) != frame.pixel_format {
        validate_source(frame)?;
        let width = frame.width as usize;
        return Ok(Some(
            (0..pixels)
                .map(|i| pixel_at(frame, i % width, i / width)[0])
                .collect(),
        ));
    }
    let frame = &*unpacked(frame)?;
    if matches!(frame.pixel_format, MONO8 | 0x0108_00b1 | 0x0108_00c6) {
        ensure!(frame.data.len() >= pixels, "truncated Mono8 frame");
        return Ok(Some(frame.data[..pixels].to_vec()));
    }
    let Some(shift) = unpacked_shift(frame.pixel_format) else {
        return Ok(None);
    };
    ensure!(
        frame.data.len() >= pixels * 2,
        "truncated unpacked monochrome frame"
    );
    let samples = frame.data[..pixels * 2].as_chunks::<2>().0;
    Ok(Some(
        samples
            .iter()
            .map(|p| (u16::from_le_bytes(*p) >> shift).min(255) as u8)
            .collect(),
    ))
}
/// Opaque RGBA bytes, converted in one pass without an intermediate RGB image.
pub fn rgba(frame: &Frame) -> Result<Vec<u8>> {
    Ok(convert(frame, |[r, g, b]| [r, g, b, 255])?.into_flattened())
}
pub(crate) fn checked_pixels(frame: &Frame) -> Result<usize> {
    // Match native 64-bit validation before converting to wasm's usize.
    let pixels = u64::from(frame.width) * u64::from(frame.height);
    ensure!(
        pixels > 0 && pixels <= 64 * 1024 * 1024,
        "image dimensions exceed conversion limit"
    );
    Ok(pixels as usize)
}
/// Convert a PFNC frame into any pixel representation (RGB, RGBA, a GUI color
/// type) with a single allocation; `pixel` maps each interpolated RGB triple.
pub fn convert<P: Copy + Default + Send>(
    frame: &Frame,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<Vec<P>> {
    let mut out = Vec::new();
    convert_into(frame, &mut out, pixel)?;
    Ok(out)
}
/// `convert` into a reused buffer, which is replaced by the converted image;
/// repeated conversions of one frame size then allocate nothing.
pub fn convert_into<P: Copy + Default + Send>(
    frame: &Frame,
    out: &mut Vec<P>,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    out.clear();
    convert_to(frame, out, pixel)
}
/// `convert` into memory the caller already owns, exactly one pixel per
/// element, such as a buffer shared with an accelerator, so the image is
/// written where it is consumed instead of being copied there.
pub fn convert_slice<P: Copy + Default + Send>(
    frame: &Frame,
    out: &mut [P],
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    ensure!(
        out.len() == checked_pixels(frame)?,
        "conversion buffer does not match the image size"
    );
    convert_to(frame, out, pixel)
}
/// Where `convert_to` writes: an empty growable buffer, or a slice of exactly
/// the image size.
trait Sink<'a, P: 'a> {
    /// Pixels in order.
    fn stream(self, pixels: impl Iterator<Item = P>);
    /// Storage for `len` pixels written out of order.
    fn prefilled(self, len: usize) -> &'a mut [P];
}
impl<'a, P: Copy + Default + 'a> Sink<'a, P> for &'a mut Vec<P> {
    // Extending from a slice iterator knows its exact length, so it neither
    // pre-fills nor bounds-checks.
    fn stream(self, pixels: impl Iterator<Item = P>) {
        self.extend(pixels);
    }
    fn prefilled(self, len: usize) -> &'a mut [P] {
        self.resize(len, P::default());
        self
    }
}
impl<'a, P: 'a> Sink<'a, P> for &'a mut [P] {
    fn stream(self, pixels: impl Iterator<Item = P>) {
        for (dst, p) in self.iter_mut().zip(pixels) {
            *dst = p;
        }
    }
    fn prefilled(self, _: usize) -> &'a mut [P] {
        self
    }
}
fn convert_to<'a, P: Copy + Default + Send + 'a>(
    frame: &Frame,
    out: impl Sink<'a, P>,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    // Packed monochrome, alpha-padded RGB and YUV stream straight to the
    // caller's storage. Reused conversion buffers need no temporary image.
    if !is_bayer(frame.pixel_format) && unpacked_format(frame.pixel_format) != frame.pixel_format {
        validate_source(frame)?;
        let width = frame.width as usize;
        out.stream(
            (0..checked_pixels(frame)?).map(|i| pixel(pixel_at(frame, i % width, i / width))),
        );
        return Ok(());
    }
    let frame = &*eight_bit(frame)?;
    let pixels = checked_pixels(frame)?;
    // Streaming formats map source to output in order.
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => {
            ensure!(frame.data.len() >= pixels, "truncated Mono8 frame");
            out.stream(frame.data[..pixels].iter().map(|&v| pixel([v, v, v])));
        }
        RGB8 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let source = frame.data[..pixels * 3].as_chunks::<3>().0;
            out.stream(source.iter().map(|p| pixel([p[0], p[1], p[2]])));
        }
        0x0218_0015 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let source = frame.data[..pixels * 3].as_chunks::<3>().0;
            out.stream(source.iter().map(|p| pixel([p[2], p[1], p[0]])));
        }
        0x0110_0003 | 0x0110_0005 | 0x0110_0025 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            ensure!(
                frame.data.len() >= pixels * 2,
                "truncated unpacked monochrome frame"
            );
            let shift = unpacked_shift(frame.pixel_format).unwrap_or(8);
            let source = frame.data[..pixels * 2].as_chunks::<2>().0;
            out.stream(source.iter().map(|&p| {
                let v = (u16::from_le_bytes(p) >> shift).min(255) as u8;
                pixel([v, v, v])
            }));
        }
        0x0108_0008..=0x0108_000b => {
            ensure!(frame.data.len() >= pixels, "truncated Bayer8 frame");
            // Interpolation writes edges and interiors out of order.
            let out = out.prefilled(pixels);
            let pattern = bayer_pattern(frame.pixel_format);
            let (w, h, data) = (frame.width as usize, frame.height as usize, &frame.data);
            let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
            let bounded = |x: usize, y: usize| pixel(bayer_bounded(frame, pattern, x, y));
            let demosaic_row = |y: usize, line: &mut [P]| {
                if y == 0 || y + 1 == h || w < 3 {
                    for (x, px) in line.iter_mut().enumerate() {
                        *px = bounded(x, y);
                    }
                    return;
                }
                (line[0], line[w - 1]) = (bounded(0, y), bounded(w - 1, y));
                let chroma = color(0, y) + color(1, y) - 1;
                let rows = |dy: usize| data[(y + dy - 1) * w..(y + dy) * w].windows(3);
                for (x, ((up, row), down)) in rows(0).zip(rows(1)).zip(rows(2)).enumerate() {
                    let horizontal = row[0] as u16 + row[2] as u16;
                    let vertical = up[1] as u16 + down[1] as u16;
                    let (near, green, far) = if color(x + 1, y) == 1 {
                        ((horizontal >> 1) as u8, row[1], (vertical >> 1) as u8)
                    } else {
                        let diagonal =
                            up[0] as u16 + up[2] as u16 + down[0] as u16 + down[2] as u16;
                        (
                            row[1],
                            ((horizontal + vertical) >> 2) as u8,
                            (diagonal >> 2) as u8,
                        )
                    };
                    line[x + 1] = pixel(if chroma == 0 {
                        [near, green, far]
                    } else {
                        [far, green, near]
                    });
                }
            };
            // WebAssembly has no worker threads; each row follows the same
            // interpolation rule as the native converter.
            for (y, line) in out.chunks_exact_mut(w).enumerate() {
                demosaic_row(y, line);
            }
        }
        v => {
            bail!("unsupported PFNC format 0x{v:08x}; capture with --format raw to preserve bytes")
        }
    }
    Ok(())
}

/// Any Bayer mosaic, whatever its depth or packing.
pub(crate) fn is_bayer(pixel_format: u32) -> bool {
    bayer8(pixel_format).is_some()
}
/// A Bayer frame read as whole 2x2 quads, one color each: red, the mean of
/// both greens, blue. As good as a demosaic for statistics over many samples
/// and several times cheaper.
struct Quads<'a> {
    frame: &'a Frame,
    /// See `quad_order`.
    order: [usize; 4],
    columns: usize,
    rows: usize,
}
impl<'a> Quads<'a> {
    /// None for other formats, or a mosaic without a whole quad. The frame
    /// must have passed `validated_bytes_per_pixel`.
    fn of(frame: &'a Frame) -> Option<Self> {
        let (width, height) = (frame.width as usize, frame.height as usize);
        (is_bayer(frame.pixel_format) && width >= 2 && height >= 2).then(|| Self {
            frame,
            order: quad_order(bayer_pattern(frame.pixel_format)),
            columns: width / 2,
            rows: height / 2,
        })
    }
    fn color(&self, x: usize, y: usize) -> [u8; 3] {
        let quad = [
            sample8(self.frame, 2 * x, 2 * y),
            sample8(self.frame, 2 * x + 1, 2 * y),
            sample8(self.frame, 2 * x, 2 * y + 1),
            sample8(self.frame, 2 * x + 1, 2 * y + 1),
        ];
        let [red, green, other, blue] = self.order;
        let greens = u16::from(quad[green]) + u16::from(quad[other]);
        [quad[red], ((greens + 1) >> 1) as u8, quad[blue]]
    }
}
/// Where red, the two greens and blue sit among a quad's samples, the top
/// row's two first.
fn quad_order(pattern: [usize; 4]) -> [usize; 4] {
    let at = |channel, nth| {
        let mut found = (0..4).filter(|&i| pattern[i] == channel);
        found.nth(nth).unwrap_or(0)
    };
    [at(0, 0), at(1, 0), at(1, 1), at(2, 0)]
}
/// Color filter layout as the channel (0 R, 1 G, 2 B) at (x & 1, y & 1),
/// indexed `[(y & 1) * 2 + (x & 1)]`.
pub fn bayer_pattern(pixel_format: u32) -> [usize; 4] {
    match bayer8(pixel_format).unwrap_or(pixel_format) {
        0x0108_0008 => [1, 0, 2, 1],
        0x0108_0009 => [0, 1, 1, 2],
        0x0108_000a => [1, 2, 0, 1],
        _ => [2, 1, 1, 0],
    }
}
/// Bilinear demosaic of one pixel using only in-bounds neighbors of each
/// missing color; the reference rule the vectorized interior must match.
fn bayer_bounded(frame: &Frame, pattern: [usize; 4], x: usize, y: usize) -> [u8; 3] {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
    std::array::from_fn(|c| {
        if color(x, y) == c {
            return sample8(frame, x, y);
        }
        let (mut sum, mut n) = (0u32, 0u32);
        for yy in y.saturating_sub(1)..(y + 2).min(h) {
            for xx in x.saturating_sub(1)..(x + 2).min(w) {
                if color(xx, yy) == c {
                    sum += u32::from(sample8(frame, xx, yy));
                    n += 1;
                }
            }
        }
        sum.checked_div(n)
            .map_or_else(|| sample8(frame, x, y), |v| v as u8)
    })
}
/// Bytes per pixel of a convertible format, after validating dimensions and
/// that the frame holds a complete image.
pub(crate) fn validated_bytes_per_pixel(frame: &Frame) -> Result<usize> {
    let pixels = checked_pixels(frame)?;
    let (bytes, name) = match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => (1, "8-bit scalar"),
        RGB8 | 0x0218_0015 => (3, "RGB"),
        0x0110_0003 | 0x0110_0005 | 0x0110_0025 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            (2, "unpacked monochrome")
        }
        0x0108_0008..=0x0108_000b => (1, "Bayer8"),
        v if wide_bayer(v).is_some() => (2, "unpacked Bayer"),
        v => {
            bail!("unsupported PFNC format 0x{v:08x}; capture with --format raw to preserve bytes")
        }
    };
    ensure!(frame.data.len() >= pixels * bytes, "truncated {name} frame");
    Ok(bytes)
}
/// Whether `rgb`/`convert` can decode this pixel format.
pub fn convertible(pixel_format: u32) -> bool {
    matches!(
        eight_bit_format(pixel_format),
        MONO8
            | 0x0108_00b1
            | 0x0108_00c6
            | RGB8
            | 0x0218_0015
            | 0x0110_0003
            | 0x0110_0005
            | 0x0110_0025
            | 0x0110_0007
            | 0x0110_00b8
            | 0x0110_00c7
            | 0x0108_0008..=0x0108_000b
    )
}
/// One pixel exactly as `convert` would produce it; for sparse sampling when
/// the full image is converted elsewhere (for example on the GPU).
/// The frame must have passed `validated_bytes_per_pixel`.
fn pixel_at(frame: &Frame, x: usize, y: usize) -> [u8; 3] {
    if let Some(rgb) = formats::pixel_at(frame, x, y) {
        return rgb;
    }
    let i = y * frame.width as usize + x;
    let data = &frame.data;
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => [data[i]; 3],
        RGB8 => [data[3 * i], data[3 * i + 1], data[3 * i + 2]],
        0x0218_0015 => [data[3 * i + 2], data[3 * i + 1], data[3 * i]],
        0x0220_0016 => [data[4 * i], data[4 * i + 1], data[4 * i + 2]],
        0x0220_0017 => [data[4 * i + 2], data[4 * i + 1], data[4 * i]],
        v if yuv422(v).is_some() => {
            let [y0, cb, y1, cr] = yuv422(v).unwrap();
            let stride = row_bytes(frame.width, v);
            let line = &data[y * stride..(y + 1) * stride];
            let at = x / 2 * 4;
            let luma = line[at + if x.is_multiple_of(2) { y0 } else { y1 }];
            ycbcr(luma, line[at + cb], *line.get(at + cr).unwrap_or(&128))
        }
        v if packing(v).is_some() && !is_bayer(v) => [sample8(frame, x, y); 3],
        0x0110_0003 | 0x0110_0005 | 0x0110_0025 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            let shift = unpacked_shift(frame.pixel_format).unwrap_or(8);
            [(u16::from_le_bytes([data[2 * i], data[2 * i + 1]]) >> shift).min(255) as u8; 3]
        }
        _ => bayer_bounded(frame, bayer_pattern(frame.pixel_format), x, y),
    }
}
/// 8-bit luminance of an RGB pixel, with the weights the histograms use.
fn luma([r, g, b]: [u8; 3]) -> usize {
    (r as usize * 54 + g as usize * 183 + b as usize * 19) >> 8
}

/// A sample at or below this in every channel is crushed to black.
const CRUSHED: u8 = 1;
/// A sample at or above this in any channel is blown out.
const BLOWN: u8 = 254;

/// One sample in each evenly spaced stratum, with deterministic jitter to
/// avoid locking onto one phase of periodic sensor detail. Every index is
/// in bounds and unique, and small inputs are read in full.
pub(crate) fn sample_indices(count: usize, limit: usize) -> impl Iterator<Item = usize> {
    let samples = count.min(limit);
    (0..samples).map(move |i| {
        let start = i as u64 * count as u64 / samples as u64;
        let end = (i as u64 + 1) * count as u64 / samples as u64;
        // SplitMix64 mixes the stratum index independently of its location.
        let mut phase = (i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
        phase = (phase ^ (phase >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        phase = (phase ^ (phase >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        phase ^= phase >> 31;
        (start + phase % (end - start)) as usize
    })
}

/// How a frame is exposed, from at most 65,536 stratified pixels (whole
/// quads of a Bayer mosaic) decoded individually so no full RGB image is
/// needed.
#[derive(Clone, Debug, PartialEq)]
pub struct Exposure {
    /// 64-bin luminance histogram.
    pub luma: [u32; 64],
    /// Red, green and blue histograms; None for monochrome formats.
    pub channels: Option<[[u32; 64]; 3]>,
    /// Mean luminance, 0–1.
    pub mean: f32,
    /// Shares of samples crushed to black and blown out, 0–1.
    pub shadows: f32,
    pub highlights: f32,
}

pub fn exposure(frame: &Frame) -> Result<Exposure> {
    validate_source(frame)?;
    let (w, pixels) = (frame.width as usize, checked_pixels(frame)?);
    let mono = monochrome(frame.pixel_format);
    let (mut luma_bins, mut channels) = ([0; 64], [[0; 64]; 3]);
    let (mut sum, mut samples, mut shadows, mut highlights) = (0u64, 0u32, 0u32, 0u32);
    let mut add = |rgb: [u8; 3]| {
        let y = luma(rgb);
        luma_bins[y >> 2] += 1;
        for (bins, value) in channels.iter_mut().zip(rgb) {
            bins[value as usize >> 2] += 1;
        }
        sum += y as u64;
        samples += 1;
        shadows += u32::from(rgb.iter().all(|&v| v <= CRUSHED));
        highlights += u32::from(rgb.iter().any(|&v| v >= BLOWN));
    };
    match Quads::of(frame) {
        Some(quads) => {
            let count = quads.columns * quads.rows;
            for i in sample_indices(count, 65_536) {
                add(quads.color(i % quads.columns, i / quads.columns));
            }
        }
        None => {
            for i in sample_indices(pixels, 65_536) {
                add(pixel_at(frame, i % w, i / w));
            }
        }
    }
    let share = |count: u32| count as f32 / samples.max(1) as f32;
    Ok(Exposure {
        luma: luma_bins,
        channels: (!mono).then_some(channels),
        mean: sum as f32 / samples.max(1) as f32 / 255.0,
        shadows: share(shadows),
        highlights: share(highlights),
    })
}

/// How sharp a region is by four common focus measures, each larger when
/// sharper, in 8-bit luminance units. Absolute values depend on the scene;
/// compare them while focusing on one subject.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sharpness {
    /// Variance of the 4-neighbour Laplacian.
    pub laplacian: f32,
    /// Tenengrad: mean squared Sobel gradient magnitude.
    pub tenengrad: f32,
    /// Brenner: mean squared difference between pixels two apart.
    pub brenner: f32,
    /// Normalized variance: luminance variance over its mean.
    pub variance: f32,
}

/// Sharpness of the `[x, y, width, height]` pixel region, clamped to the frame.
/// At most 65,536 evenly spaced pixels are measured, each against its
/// full-resolution neighbours, so fine detail still counts in large regions.
/// A Bayer mosaic is measured over its 2x2 quads, the finest whole colors it has.
pub fn sharpness(frame: &Frame, region: [u32; 4]) -> Result<Sharpness> {
    validate_source(frame)?;
    checked_pixels(frame)?;
    if let Some(quads) = Quads::of(frame).filter(|q| q.columns >= 3 && q.rows >= 3) {
        let [x, y, w, h] = region;
        let region = [
            x / 2,
            y / 2,
            w.saturating_add(x % 2).div_ceil(2),
            h.saturating_add(y % 2).div_ceil(2),
        ];
        return Ok(sharpness_over(
            (quads.columns, quads.rows),
            region,
            |x, y| luma(quads.color(x, y)) as f64,
        ));
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    ensure!(w >= 3 && h >= 3, "frame is too small to measure sharpness");
    Ok(sharpness_over((w, h), region, |x, y| {
        luma(pixel_at(frame, x, y)) as f64
    }))
}
/// `sharpness` of a region of a `w` × `h` grid, at least 3 × 3, whose
/// luminance at (x, y) is `at`.
fn sharpness_over(
    (w, h): (usize, usize),
    region: [u32; 4],
    at: impl Fn(usize, usize) -> f64,
) -> Sharpness {
    let [x, y, width, height] = region.map(|v| v as usize);
    // Keep a one-pixel border so every 3×3 neighbourhood is inside the frame.
    let x0 = x.clamp(1, w - 2);
    let y0 = y.clamp(1, h - 2);
    let x1 = x.saturating_add(width).clamp(x0 + 1, w - 1);
    let y1 = y.saturating_add(height).clamp(y0 + 1, h - 1);
    let area = (x1 - x0) as f64 * (y1 - y0) as f64;
    let step = (area / 65_536.0).sqrt().ceil().max(1.0) as usize;
    let (mut n, mut lap, mut lap2, mut ten, mut bren, mut sum, mut sum2) =
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    // Shift each row and column of the sampling grid by a different phase
    // within the stride, so periodic detail (a one-pixel checkerboard or
    // stripes) is never sampled at only one of its phases.
    let grid = (0..(y1 - y0).div_ceil(step))
        .flat_map(|row| (0..(x1 - x0).div_ceil(step)).map(move |column| (row, column)));
    for (row, column) in grid {
        let (x, y) = (
            x0 + column * step + row % step,
            y0 + row * step + column % step,
        );
        if x >= x1 || y >= y1 {
            continue;
        }
        {
            let [a, b, c] = [at(x - 1, y - 1), at(x, y - 1), at(x + 1, y - 1)];
            let [d, e, f] = [at(x - 1, y), at(x, y), at(x + 1, y)];
            let [g, k, i] = [at(x - 1, y + 1), at(x, y + 1), at(x + 1, y + 1)];
            let laplacian = b + d + f + k - 4.0 * e;
            let gx = (c + 2.0 * f + i) - (a + 2.0 * d + g);
            let gy = (g + 2.0 * k + i) - (a + 2.0 * b + c);
            n += 1.0;
            lap += laplacian;
            lap2 += laplacian * laplacian;
            ten += gx * gx + gy * gy;
            bren += ((f - d).powi(2) + (k - b).powi(2)) / 2.0;
            sum += e;
            sum2 += e * e;
        }
    }
    let mean = sum / n;
    let variance = (sum2 / n - mean * mean).max(0.0);
    Sharpness {
        laplacian: (lap2 / n - (lap / n).powi(2)).max(0.0) as f32,
        tenengrad: (ten / n) as f32,
        brenner: (bren / n) as f32,
        variance: if mean > 0.0 { variance / mean } else { 0.0 } as f32,
    }
}
/// The most bytes of provenance JSON a saved still carries (`frame.rs`
/// `MAX_META`).
pub const MAX_META: usize = 16 * 1024;

/// `encode_with_meta`'s first check, applied to every format.
pub fn check_meta(json_len: usize) -> Result<()> {
    ensure!(
        json_len <= MAX_META,
        "capture metadata exceeds {MAX_META} bytes"
    );
    Ok(())
}

/// The raw, PPM and PGM branches of `encode_with_meta`. PNG and JPEG are
/// assembled from browser compression; see [`crate::png`] and
/// [`crate::jpeg`].
pub fn encode(frame: &Frame, format: &str) -> Result<Vec<u8>> {
    match format {
        "raw" => Ok(frame.data.clone()),
        "ppm" => {
            let mut bytes = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            bytes.extend_from_slice(&rgb(frame)?);
            Ok(bytes)
        }
        "pgm" => {
            let Some(gray) = gray8(frame)? else {
                bail!("PGM requires a monochrome image; use PNG or PPM");
            };
            let mut bytes = format!("P5\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            bytes.extend_from_slice(&gray);
            Ok(bytes)
        }
        _ => bail!("unknown image format {format}; use png, jpeg, raw, pgm or ppm"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(fmt: u32, data: Vec<u8>) -> Frame {
        Frame {
            id: 1,
            width: 2,
            height: 1,
            pixel_format: fmt,
            timestamp_ns: 0,
            data,
        }
    }

    #[test]
    fn conversions_and_truncation() {
        assert_eq!(rgb(&f(MONO8, vec![4, 8])).unwrap(), vec![4, 4, 4, 8, 8, 8]);
        assert_eq!(
            rgb(&f(0x0218_0015, vec![1, 2, 3, 4, 5, 6])).unwrap(),
            vec![3, 2, 1, 6, 5, 4]
        );
        assert!(rgb(&f(RGB8, vec![0])).is_err());
        assert_eq!(
            rgb(&f(0, vec![0; 6])).unwrap_err(),
            "unsupported PFNC format 0x00000000; capture with --format raw to preserve bytes"
        );
        for (format, shift) in [(0x0110_0003, 2), (0x0110_0005, 4), (0x0110_0007, 8)] {
            let data = [(2u16 << shift) - 1, u16::MAX]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect();
            assert_eq!(rgb(&f(format, data)).unwrap(), [1, 1, 1, 255, 255, 255]);
            assert_eq!(
                rgb(&f(format, vec![0; 3])).unwrap_err(),
                format!("truncated {} frame", pixel_format_name(format))
            );
        }
        assert_eq!(
            rgb(&f(RGB8, vec![1, 2, 3, 4, 5, 6, 7])).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        let huge = Frame {
            width: u32::MAX,
            height: u32::MAX,
            ..f(MONO8, vec![])
        };
        assert_eq!(
            rgb(&huge).unwrap_err(),
            "image dimensions exceed conversion limit"
        );
        for format in [0x0110_00b8, 0x0110_00c7] {
            let source = f(format, vec![0x34, 0x12, 0xff, 0xff]);
            assert_eq!(rgb(&source).unwrap(), vec![0x12, 0x12, 0x12, 255, 255, 255]);
            assert_eq!(encode(&source, "raw").unwrap(), source.data);
            assert!(convertible(format));
            assert!(exposure(&source).unwrap().channels.is_none());
        }
    }

    /// `frame.rs`' reference bilinear demosaic, which the fast path matches.
    fn reference_bayer(frame: &Frame) -> Vec<u8> {
        let pattern = bayer_pattern(frame.pixel_format);
        let (w, h) = (frame.width as i32, frame.height as i32);
        let mut out = vec![0; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let own = pattern[((y & 1) * 2 + (x & 1)) as usize];
                    let v = if own == c {
                        frame.data[(y * w + x) as usize]
                    } else {
                        let (mut sum, mut n) = (0u32, 0);
                        for dy in -1..=1 {
                            for dx in -1..=1 {
                                let (xx, yy) = (x + dx, y + dy);
                                if xx >= 0
                                    && yy >= 0
                                    && xx < w
                                    && yy < h
                                    && pattern[((yy & 1) * 2 + (xx & 1)) as usize] == c
                                {
                                    sum += frame.data[(yy * w + xx) as usize] as u32;
                                    n += 1;
                                }
                            }
                        }
                        sum.checked_div(n)
                            .map_or(frame.data[(y * w + x) as usize], |v| v as u8)
                    };
                    out[((y * w + x) * 3 + c as i32) as usize] = v;
                }
            }
        }
        out
    }

    #[test]
    fn bayer_matches_reference_bilinear() {
        let mut state = 0x2545_f491_u32;
        for format in 0x0108_0008..=0x0108_000b {
            for (w, h) in [(1, 1), (2, 2), (3, 5), (8, 6), (31, 17)] {
                let data = (0..w * h)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        state as u8
                    })
                    .collect();
                let frame = Frame {
                    width: w,
                    height: h,
                    pixel_format: format,
                    data,
                    ..Frame::default()
                };
                assert_eq!(rgb(&frame).unwrap(), reference_bayer(&frame), "{w}x{h}");
            }
        }
    }

    #[test]
    fn exposure_reports_channels_clipping_and_mean() {
        let frame = Frame {
            width: 4,
            height: 1,
            pixel_format: RGB8,
            data: vec![0, 0, 0, 255, 255, 255, 128, 128, 128, 255, 0, 0],
            ..Frame::default()
        };
        let exposure = exposure(&frame).unwrap();
        let channels = exposure.channels.unwrap();
        assert_eq!(channels[0][63], 2);
        assert_eq!(channels[1][63], 1);
        assert_eq!(channels[2][0], 2);
        assert_eq!((exposure.shadows, exposure.highlights), (0.25, 0.5));
        assert!((exposure.mean - (255.0 + 128.0 + 53.0) / 4.0 / 255.0).abs() < 1e-6);
        let mono = super::exposure(&f(MONO8, vec![0, 255])).unwrap();
        assert!(mono.channels.is_none());
        assert_eq!((mono.shadows, mono.highlights), (0.5, 0.5));
        assert!(super::exposure(&f(0, vec![0; 6])).is_err());
        assert_eq!(
            super::exposure(&f(RGB8, vec![0; 5])).unwrap_err(),
            "truncated RGB8 frame"
        );
    }

    #[test]
    fn packed_mono_wide_bayer_and_alpha_formats_match_native_pixels() {
        let mono = f(0x010e_0104, vec![0x34, 0x12, 0xaf, 0x0e]);
        assert_eq!(gray8(&mono).unwrap().unwrap(), [0x48, 0xea]);
        assert_eq!(
            rgb(&f(0x0220_0017, vec![3, 2, 1, 0, 6, 5, 4, 255])).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        let bayer = Frame {
            width: 2,
            height: 2,
            pixel_format: 0x010c_0059,
            data: vec![0x00, 0x0c, 0x80, 0x00, 0x08, 0x40],
            ..Frame::default()
        };
        assert_eq!(rgb(&bayer).unwrap(), [192, 128, 64].repeat(4));
        assert_eq!(exposure(&bayer).unwrap().channels.unwrap()[0][48], 1);
        let normalized = unpacked(&bayer).unwrap();
        assert_eq!(normalized.pixel_format, 0x0110_0011);
        assert_eq!(bayer_pattern(normalized.pixel_format), [0, 1, 1, 2]);
        assert!(rgb(&f(0x010e_0104, vec![0; 3])).is_err());
        assert_eq!(row_bytes(64 * 1024 * 1024, 0x0240_0064), 512 * 1024 * 1024);
    }

    #[test]
    fn stratified_exposure_avoids_periodic_phase_bias_with_a_bounded_sample_count() {
        assert_eq!(sample_indices(0, 65_536).count(), 0);
        for count in [7, 65_536, 65_537, 1_048_576, 64 * 1024 * 1024] {
            let indices = sample_indices(count, 65_536).collect::<Vec<_>>();
            assert_eq!(indices.len(), count.min(65_536));
            assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(indices.iter().all(|&i| i < count));
        }
        for pattern in 0..3 {
            let data = (0..1024 * 1024)
                .map(|i| {
                    let (x, y) = (i % 1024, i / 1024);
                    let phase = match pattern {
                        0 => x,
                        1 => y,
                        _ => x + y,
                    };
                    if phase % 2 == 0 { 0 } else { 255 }
                })
                .collect();
            let source = Frame {
                width: 1024,
                height: 1024,
                pixel_format: MONO8,
                data,
                ..Frame::default()
            };
            let measured = exposure(&source).unwrap();
            assert_eq!(measured.luma.iter().sum::<u32>(), 65_536);
            assert!(
                (measured.mean - 0.5).abs() < 0.01,
                "{pattern}: {measured:?}"
            );
        }
    }

    #[test]
    fn sharpness_prefers_crisp_detail_in_the_region() {
        let flat = Frame {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            data: vec![90; 256],
            ..Frame::default()
        };
        assert_eq!(
            sharpness(&flat, [0, 0, 16, 16]).unwrap(),
            Sharpness::default()
        );
        assert_eq!(
            sharpness(&f(MONO8, vec![1, 2]), [0, 0, 2, 1]).unwrap_err(),
            "frame is too small to measure sharpness"
        );
        let checker = Frame {
            width: 64,
            height: 64,
            pixel_format: MONO8,
            data: (0..64 * 64)
                .map(|i| if (i % 64 + i / 64) % 2 == 0 { 0 } else { 255 })
                .collect(),
            ..Frame::default()
        };
        let crisp = sharpness(&checker, [0, 0, 64, 64]).unwrap();
        assert!(crisp.laplacian > 0.0 && crisp.variance > 0.0);
        // An out-of-range region is clamped, not rejected.
        assert_eq!(
            sharpness(&checker, [u32::MAX, u32::MAX, u32::MAX, u32::MAX]).unwrap(),
            sharpness(&checker, [62, 62, 1, 1]).unwrap()
        );
    }

    #[test]
    fn encoders_write_headers_and_refuse_color_pgm() {
        let mono = f(MONO8, vec![7, 9]);
        assert_eq!(encode(&mono, "pgm").unwrap(), b"P5\n2 1\n255\n\x07\x09");
        assert_eq!(
            encode(&mono, "ppm").unwrap(),
            b"P6\n2 1\n255\n\x07\x07\x07\x09\x09\x09"
        );
        let color = f(RGB8, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            encode(&color, "pgm").unwrap_err(),
            "PGM requires a monochrome image; use PNG or PPM"
        );
        assert_eq!(
            encode(&color, "tiff").unwrap_err(),
            "unknown image format tiff; use png, jpeg, raw, pgm or ppm"
        );
        assert_eq!(check_meta(MAX_META), Ok(()));
        assert_eq!(
            check_meta(MAX_META + 1).unwrap_err(),
            "capture metadata exceeds 16384 bytes"
        );
    }
}
