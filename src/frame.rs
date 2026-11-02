//! Bounded PFNC image conversion and portable output without image codecs from the OS.
use crate::types::{Frame, MONO8, RGB8};
use anyhow::{Context as _, Result, bail, ensure};
use std::borrow::Cow;

mod encoded;
mod formats;
mod rows;
mod view;
pub(crate) use encoded::write_all_vectored;
pub use encoded::{Body, Encoded};
#[cfg(all(test, feature = "jpeg"))]
pub(crate) use rows::tests::noise_frame;
pub(crate) use rows::{Channels, RowReader};
#[cfg(feature = "jpeg")]
pub(crate) use rows::{Chroma, Yuv420, ycbcr420};
pub use view::FrameRef;

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
    (u64::from(width) * u64::from((pixel_format >> 16) & 0xff)).div_ceil(8) as usize
}
/// Bytes in a `width` × `height` image: `row_bytes` for each row, or the
/// planes of a planar Y'CbCr layout. None when it overflows.
pub(crate) fn frame_len(pixel_format: u32, width: u32, height: u32) -> Option<usize> {
    formats::planes_len(pixel_format, width, height)
        .or_else(|| row_bytes(width, pixel_format).checked_mul(height as usize))
}
pub(crate) use formats::{Matrix, NV12_601, NV12_709, ycbcr_planes};
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
/// Layouts with continuous LSB packing that may need transport row alignment.
pub(crate) fn lsb_packed(pixel_format: u32) -> bool {
    matches!(packing(pixel_format), Some((_, _, Packing::Lsb)))
        || matches!(
            pixel_format,
            0x0101_0037
                | 0x0102_0038
                | 0x0104_0039
                | 0x021e_0048
                | 0x0224_0049
                | 0x021e_005c
                | 0x0224_005d
        )
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
/// `frame` in a whole-sample layout: packed 10-, 12- and 14-bit mono/Bayer
/// samples retain native precision in 16-bit words. Other transformed color
/// layouts become RGB8; low-bit and signed monochrome become Mono8.
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
    crate::copies::record(crate::copies::Site::Intermediate, data.len());
    Ok(Cow::Owned(Frame {
        pixel_format,
        data: data.into(),
        ..*frame
    }))
}
/// One row of packed samples as little-endian 16-bit samples, one per
/// element of `out`; a short last group is completed with zeros.
pub(crate) fn unpack_row(row: &[u8], bits: u32, packing: Packing, out: &mut [[u8; 2]]) {
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
        (Packing::Lsb, 14) => groups(row, out, |[b0, b1, b2, b3, b4, b5, b6]| {
            [
                b0 | (b1 & 0x3f) << 8,
                b1 >> 6 | b2 << 2 | (b3 & 0x0f) << 10,
                b3 >> 4 | b4 << 4 | (b5 & 0x03) << 12,
                b5 >> 2 | b6 << 6,
            ]
        }),
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
        .ok_or_else(|| anyhow::anyhow!("image size overflow"))?;
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
/// The 8-bit format a row reader reduces `pixel_format` to, and the shift
/// that brings its samples there: Bayer8 for a 10- to 16-bit mosaic, packed
/// or not, and Mono8 for packed monochrome. None for every other format.
fn reduced(pixel_format: u32) -> Option<(u32, u32)> {
    let unpacked = unpacked_format(pixel_format);
    wide_bayer(unpacked).or_else(|| {
        packing(pixel_format)
            .and_then(|_| unpacked_shift(unpacked))
            .map(|shift| (MONO8, shift))
    })
}
/// The 8-bit format whose samples a row reader decodes `pixel_format`'s
/// into.
fn eight_bit_format(pixel_format: u32) -> u32 {
    reduced(pixel_format).map_or_else(|| unpacked_format(pixel_format), |(format, _)| format)
}
/// Formats decoded a pixel at a time: the `formats` layouts, alpha-padded
/// RGB and YUV 4:2:2. Packed monochrome and Bayer are unpacked a row at a
/// time, which is many times faster than reading each sample on its own.
fn streamed(pixel_format: u32) -> bool {
    packing(pixel_format).is_none()
        && !is_bayer(pixel_format)
        && unpacked_format(pixel_format) != pixel_format
}
/// One-channel formats: the 8-bit monochrome family, the unpacked 16-bit one
/// of [`unpacked_shift`], and the packed formats that unpack into it.
pub(crate) fn monochrome(pixel_format: u32) -> bool {
    let pixel_format = unpacked_format(pixel_format);
    matches!(pixel_format, MONO8 | 0x0108_00b1 | 0x0108_00c6)
        || unpacked_shift(pixel_format).is_some()
}
/// The frame as 8-bit RGB, three bytes a pixel.
pub fn rgb(frame: &Frame) -> Result<Vec<u8>> {
    validate_source(frame)?;
    let view = FrameRef::whole(frame);
    let mut out = vec![0; view.pixels() * 3];
    rows::decode(view, Channels::Rgb, &mut out);
    Ok(out)
}
/// The frame decoded as `channels` into `out`, which is resized to fit and
/// every byte of which is written: a reused buffer needs no clearing.
pub(crate) fn decode_into(frame: &Frame, channels: Channels, out: &mut Vec<u8>) -> Result<()> {
    validate_source(frame)?;
    let view = FrameRef::whole(frame);
    out.resize(view.pixels() * channels.bytes(), 0);
    rows::decode(view, channels, out);
    Ok(())
}

/// Samples an encoder reads: the frame's own bytes where they already are
/// what it reads, else a conversion written once into a buffer from the
/// transform pool (`framebuf::transforms`), to which it goes back.
pub enum Samples<'a> {
    Borrowed(&'a [u8]),
    Converted(crate::framebuf::PoolVec),
}
impl std::ops::Deref for Samples<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Samples::Borrowed(samples) => samples,
            Samples::Converted(samples) => samples,
        }
    }
}
impl<'a> Samples<'a> {
    pub(crate) fn converted(&self) -> bool {
        matches!(self, Samples::Converted(_))
    }
    /// The samples as an encoded body, a conversion counted as `site`.
    pub(crate) fn into_body(self, site: crate::copies::Site) -> Body<'a> {
        match self {
            Samples::Borrowed(samples) => Body::Borrowed(samples),
            Samples::Converted(samples) => {
                crate::copies::record(site, samples.len());
                Body::Pooled(samples)
            }
        }
    }
}

/// `view`'s 8-bit samples as `channels`: borrowed where the frame holds
/// exactly them in whole rows, else decoded once into a pooled buffer.
pub(crate) fn samples(view: FrameRef<'_>, channels: Channels) -> Result<Samples<'_>> {
    validate_source(view.frame)?;
    let line = view.width * channels.bytes();
    if RowReader::new(view).borrows(channels) && view.width == view.frame.width as usize {
        let at = view.y0 * line;
        return Ok(Samples::Borrowed(
            &view.frame.data[at..at + line * view.height],
        ));
    }
    let mut out = crate::framebuf::transforms().take_stale(line * view.height);
    rows::decode(view, channels, &mut out);
    Ok(Samples::Converted(out))
}
/// The frame's RGB samples, as `rgb` gives them: an RGB8 frame's own bytes,
/// else a conversion.
pub(crate) fn rgb_samples(frame: &Frame) -> Result<Samples<'_>> {
    samples(FrameRef::whole(frame), Channels::Rgb)
}
/// The frame's 8-bit gray samples, as `gray8` gives them: the frame's own
/// bytes for the 8-bit monochrome formats, else a conversion; None for
/// color formats.
pub(crate) fn gray8_samples(frame: &Frame) -> Result<Option<Samples<'_>>> {
    checked_pixels(frame)?;
    if !monochrome(frame.pixel_format) {
        return Ok(None);
    }
    samples(FrameRef::whole(frame), Channels::Gray).map(Some)
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
    checked_pixels(frame)?;
    if !monochrome(frame.pixel_format) {
        return Ok(None);
    }
    validate_source(frame)?;
    let view = FrameRef::whole(frame);
    let mut out = vec![0; view.pixels()];
    rows::decode(view, Channels::Gray, &mut out);
    Ok(Some(out))
}
/// The significant bits of each sample of `pixel_format`, from its PFNC
/// name: 12 for Mono12, Mono12p and BayerRG12, 8 for RGB8, 10 for RGB10p32.
/// None for a format this does not know.
pub fn sample_bits(pixel_format: u32) -> Option<u32> {
    formats::sample_bits(pixel_format).or_else(|| name_bits(pixel_format_name(pixel_format)))
}
/// The bits per sample of a PFNC pixel format name: the digits it ends
/// with before any packing suffix (12 for Mono12p, 16 for Coord3D_C16) or
/// component order (8 for YUV422_8_UYVY), or the layout's own for names
/// whose digits say something else (10 for RGB10p32, 8 for RGB8a32).
pub(crate) fn name_bits(name: &str) -> Option<u32> {
    if let Some(bits) = formats::name_bits(name) {
        return Some(bits);
    }
    let mut name = name;
    while let Some((size, order)) = name.rsplit_once('_')
        && !order.is_empty()
        && order.chars().all(|c| c.is_ascii_alphabetic())
    {
        name = size;
    }
    let digits = name
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.chars().rev().collect::<String>().parse().ok()
}
/// The bit depth of `format` (png, jpeg, raw, pgm, ppm) for a frame of
/// `pixel_format`, and the frame's own when the file keeps fewer: PNG keeps
/// up to 16 bits of a monochrome format, raw every bit, the rest 8.
pub fn bit_depth(pixel_format: u32, format: &str) -> (u32, Option<u32>) {
    let source = sample_bits(pixel_format).unwrap_or(8);
    let depth = match format {
        "raw" => source,
        "png" if wide_gray_bits(unpacked_format(pixel_format)).is_some() => 16,
        _ => 8,
    };
    (depth, (source > depth).then_some(source))
}
/// The significant bits of an unpacked monochrome format wider than 8
/// bits, which a PNG keeps as 16-bit gray.
fn wide_gray_bits(pixel_format: u32) -> Option<u32> {
    match pixel_format {
        0x0110_0003 => Some(10),
        0x0110_0005 => Some(12),
        0x0110_0025 => Some(14),
        0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => Some(16),
        _ => None,
    }
}
/// A monochrome frame of more than 8 bits, packed or not, as big-endian
/// 16-bit PNG samples with the significant bits at the top, written once
/// into a pooled buffer a row at a time, and how many bits are
/// significant; None for any other format.
fn gray16(frame: &Frame) -> Result<Option<(crate::framebuf::PoolVec, u32)>> {
    let Some(bits) = wide_gray_bits(unpacked_format(frame.pixel_format)) else {
        return Ok(None);
    };
    validate_source(frame)?;
    let (width, height) = (frame.width as usize, frame.height as usize);
    let stride = row_bytes(frame.width, frame.pixel_format);
    let packed = packing(frame.pixel_format);
    let mut unpack = vec![[0; 2]; if packed.is_some() { width } else { 0 }];
    let mut out = crate::framebuf::transforms().take_stale(width * height * 2);
    let mask = u16::MAX >> (16 - bits);
    let lines = frame.data[..stride * height].chunks_exact(stride);
    for (line, out) in lines.zip(out.chunks_exact_mut(width * 2)) {
        let samples = match packed {
            Some((_, bits, packing)) => {
                unpack_row(line, bits, packing, &mut unpack);
                &unpack[..]
            }
            None => line.as_chunks::<2>().0,
        };
        for (out, sample) in out.as_chunks_mut::<2>().0.iter_mut().zip(samples) {
            *out = ((u16::from_le_bytes(*sample) & mask) << (16 - bits)).to_be_bytes();
        }
    }
    Ok(Some((out, bits)))
}
/// Opaque RGBA bytes, converted in one pass without an intermediate RGB image.
pub fn rgba(frame: &Frame) -> Result<Vec<u8>> {
    Ok(convert(frame, |[r, g, b]| [r, g, b, 255])?.into_flattened())
}
pub(crate) fn checked_pixels(frame: &Frame) -> Result<usize> {
    let pixels = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .ok_or_else(|| anyhow::anyhow!("image size overflow"))?;
    ensure!(
        pixels > 0 && pixels <= 64 * 1024 * 1024,
        "image dimensions exceed conversion limit"
    );
    Ok(pixels)
}
/// Convert a PFNC frame into any pixel representation (RGB, RGBA, gray) with
/// a single allocation; `pixel` maps each interpolated RGB triple.
pub fn convert<P: Copy + Default + Send>(
    frame: &Frame,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<Vec<P>> {
    let mut out = Vec::new();
    convert_into(frame, &mut out, pixel)?;
    Ok(out)
}
/// `convert` into a reused buffer, which is replaced by the converted image;
/// repeated conversions of one frame size then allocate nothing, and
/// write every element without clearing it first.
pub fn convert_into<P: Copy + Default + Send>(
    frame: &Frame,
    out: &mut Vec<P>,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    if let Err(error) = validate_source(frame) {
        out.clear();
        return Err(error);
    }
    let view = FrameRef::whole(frame);
    if out.len() != view.pixels() {
        out.clear();
        out.resize(view.pixels(), P::default());
    }
    rows::map(view, out, &pixel);
    Ok(())
}

/// Threads for CPU-bound per-pixel work: one below half a megapixel, where
/// spawning costs more than it saves, else `spare_cores`.
pub(crate) fn worker_threads(pixels: usize) -> usize {
    if pixels < 512 * 1024 {
        return 1;
    }
    crate::host::threads(spare_cores(crate::host::cpus()))
}
/// Up to eight of a computer's `cores`, always leaving one for the camera
/// threads.
fn spare_cores(cores: usize) -> usize {
    cores.saturating_sub(1).clamp(1, 8)
}
/// Any Bayer mosaic, whatever its depth or packing.
pub(crate) fn is_bayer(pixel_format: u32) -> bool {
    bayer8(pixel_format).is_some()
}
/// A Bayer frame read as whole 2x2 quads, one color each: red, the mean of
/// both greens, blue. As good as a demosaic for statistics over many samples
/// and several times cheaper.
struct Quads<'a> {
    view: FrameRef<'a>,
    /// See `quad_order`.
    order: [usize; 4],
    columns: usize,
    rows: usize,
}
impl<'a> Quads<'a> {
    /// None for other formats, or a mosaic without a whole quad. The frame
    /// must have passed `validate_source`.
    fn of(view: FrameRef<'a>) -> Option<Self> {
        (is_bayer(view.pixel_format()) && view.width >= 2 && view.height >= 2).then(|| Self {
            view,
            order: quad_order(bayer_pattern(view.pixel_format())),
            columns: view.width / 2,
            rows: view.height / 2,
        })
    }
    fn color(&self, x: usize, y: usize) -> [u8; 3] {
        let (frame, x, y) = (self.view.frame, self.view.x0 + 2 * x, self.view.y0 + 2 * y);
        let quad = [
            sample8(frame, x, y),
            sample8(frame, x + 1, y),
            sample8(frame, x, y + 1),
            sample8(frame, x + 1, y + 1),
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
/// Bilinear demosaic of one pixel of a view using only its in-bounds
/// neighbors of each missing color, as `rows::demosaic_row` computes it.
fn bayer_bounded(view: FrameRef<'_>, pattern: [usize; 4], x: usize, y: usize) -> [u8; 3] {
    let (w, h) = (view.width, view.height);
    let sample = |x: usize, y: usize| sample8(view.frame, view.x0 + x, view.y0 + y);
    let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
    std::array::from_fn(|c| {
        if color(x, y) == c {
            return sample(x, y);
        }
        let (mut sum, mut n) = (0u32, 0u32);
        for yy in y.saturating_sub(1)..(y + 2).min(h) {
            for xx in x.saturating_sub(1)..(x + 2).min(w) {
                if color(xx, yy) == c {
                    sum += u32::from(sample(xx, yy));
                    n += 1;
                }
            }
        }
        sum.checked_div(n).map_or_else(|| sample(x, y), |v| v as u8)
    })
}
/// Bytes per pixel of a convertible format, after validating dimensions and
/// that the frame holds a complete image.
#[cfg(test)]
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
/// One pixel of a view exactly as `convert` would produce it, for sparse
/// sampling. The frame must have passed `validate_source`.
pub(crate) fn pixel_at(view: FrameRef<'_>, x: usize, y: usize) -> [u8; 3] {
    let frame = view.frame;
    let (fx, fy) = (view.x0 + x, view.y0 + y);
    if let Some(rgb) = formats::pixel_at(frame, fx, fy) {
        return rgb;
    }
    let i = fy * frame.width as usize + fx;
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
            let line = &data[fy * stride..(fy + 1) * stride];
            let at = fx / 2 * 4;
            let luma = line[at + if fx.is_multiple_of(2) { y0 } else { y1 }];
            ycbcr(luma, line[at + cb], *line.get(at + cr).unwrap_or(&128))
        }
        v if packing(v).is_some() && !is_bayer(v) => [sample8(frame, fx, fy); 3],
        0x0110_0003 | 0x0110_0005 | 0x0110_0025 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            let shift = unpacked_shift(frame.pixel_format).unwrap_or(8);
            [(u16::from_le_bytes([data[2 * i], data[2 * i + 1]]) >> shift).min(255) as u8; 3]
        }
        _ => bayer_bounded(view, bayer_pattern(frame.pixel_format), x, y),
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
    exposure_in(FrameRef::whole(frame))
}

/// `exposure` of a view, read where it lies.
pub(crate) fn exposure_in(view: FrameRef<'_>) -> Result<Exposure> {
    validate_source(view.frame)?;
    let (w, pixels) = (view.width, view.pixels());
    let mono = monochrome(view.pixel_format());
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
    match Quads::of(view) {
        Some(quads) => {
            let count = quads.columns * quads.rows;
            for i in sample_indices(count, 65_536) {
                add(quads.color(i % quads.columns, i / quads.columns));
            }
        }
        None => {
            for i in sample_indices(pixels, 65_536) {
                add(pixel_at(view, i % w, i / w));
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

/// Pixels at the ends of a frame's range, counted over every pixel at the
/// frame's own bit depth: a 2,000-pixel highlight on a 20-megapixel sensor
/// counts whole, where a sampled histogram would miss it.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct Clipping {
    /// Pixels at full scale in any channel.
    pub high_px: u64,
    /// Pixels at zero in every channel.
    pub low_px: u64,
    pub pixels: u64,
    /// The brightest sample, as a fraction of full scale.
    pub max: f64,
}

/// Count `frame`'s clipped pixels at native depth: samples of a Mono10 frame
/// clip at 1023, a Mono8 or RGB8 one at 255, an RGB12 one at 4095 in any
/// component, each Bayer sample on its own. Packed samples and wide color
/// components are read where they lie, without unpacking the frame.
pub fn clipping(frame: &Frame) -> Result<Clipping> {
    clipping_in(FrameRef::whole(frame))
}

/// `clipping` of a view, a row at a time: packed rows are unpacked into one
/// reused row, other layouts read in place. Large views are counted in row
/// blocks across cores.
pub(crate) fn clipping_in(view: FrameRef<'_>) -> Result<Clipping> {
    let frame = view.frame;
    checked_pixels(frame)?;
    let format = frame.pixel_format;
    let (x0, y0, width) = (view.x0, view.y0, view.width);
    let pixels = view.pixels();
    // Wide, packed, planar and padded color: each pixel's brightest component.
    if let Some(bits) = formats::color_bits(format) {
        validate_source(frame)?;
        let full = (1u32 << bits) - 1;
        let tally = Tally::rows(view, |tally, y| {
            for x in x0..x0 + width {
                let (rgb, _) =
                    formats::native_color(frame, x, y).expect("a validated color layout");
                // Bits above the format's are not part of the sample.
                tally.add(
                    rgb.into_iter()
                        .map(|v| u32::from(v) & full)
                        .max()
                        .unwrap_or(0),
                    full,
                );
            }
        });
        return Ok(tally.finish(full, pixels));
    }
    // Packed monochrome and Bayer samples, each at its own depth.
    if let Some((_, bits, packing)) = packing(format) {
        validate_source(frame)?;
        let full = (1u32 << bits) - 1;
        let stride = row_bytes(frame.width, format);
        let mut samples = Vec::new();
        let tally = Tally::rows(view, move |tally, y| {
            samples.resize(frame.width as usize, [0; 2]);
            unpack_row(
                &frame.data[y * stride..][..stride],
                bits,
                packing,
                &mut samples,
            );
            for sample in &samples[x0..x0 + width] {
                tally.add(u32::from(u16::from_le_bytes(*sample)), full);
            }
        });
        return Ok(tally.finish(full, pixels));
    }
    if !convertible(format) {
        bail!("cannot count clipping in {}", pixel_format_name(format));
    }
    validate_source(frame)?;
    let wide = wide_gray_bits(format).or_else(|| wide_bayer(format).map(|(_, shift)| 8 + shift));
    let (bytes, bits) = match format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 | 0x0108_0008..=0x0108_000b => (1, 8),
        RGB8 | 0x0218_0015 => (3, 8),
        _ => match wide {
            Some(bits) => (2, bits),
            // Every other layout as its 8-bit conversion, a row at a time.
            None => {
                let mut reader = RowReader::new(view);
                let mut rgb = Vec::new();
                let tally = Tally::rows(view, move |tally, y| {
                    rgb.resize(3 * width, 0);
                    reader.read(y - y0, Channels::Rgb, &mut rgb);
                    for pixel in rgb.as_chunks::<3>().0 {
                        tally.add(u32::from(pixel[0].max(pixel[1]).max(pixel[2])), 255);
                    }
                });
                return Ok(tally.finish(255, pixels));
            }
        },
    };
    let full = (1u32 << bits) - 1;
    let stride = frame.width as usize * bytes;
    let tally = Tally::rows(view, |tally, y| {
        let line = &frame.data[y * stride..][x0 * bytes..(x0 + width) * bytes];
        match bytes {
            1 => line.iter().for_each(|&v| tally.add(u32::from(v), full)),
            // Bits above the format's are not part of the sample.
            2 => line.as_chunks::<2>().0.iter().for_each(|sample| {
                tally.add(u32::from(u16::from_le_bytes(*sample)) & full, full);
            }),
            _ => line.as_chunks::<3>().0.iter().for_each(|pixel| {
                tally.add(u32::from(pixel[0].max(pixel[1]).max(pixel[2])), full);
            }),
        }
    });
    Ok(tally.finish(full, pixels))
}

/// Clipped samples counted one pixel's brightest sample at a time.
#[derive(Default)]
struct Tally {
    high: u64,
    low: u64,
    top: u32,
}

impl Tally {
    /// Every frame row of `view` counted by `count`, in blocks of rows
    /// across cores for a large view, each block with its own `count`.
    fn rows(view: FrameRef<'_>, count: impl FnMut(&mut Tally, usize) + Clone + Send) -> Self {
        let rows = view.y0..view.y0 + view.height;
        let threads = worker_threads(view.pixels());
        let block = view.height.div_ceil(threads).max(1);
        fn run(rows: std::ops::Range<usize>, mut count: impl FnMut(&mut Tally, usize)) -> Tally {
            let mut tally = Tally::default();
            for y in rows {
                count(&mut tally, y);
            }
            tally
        }
        if threads == 1 {
            return run(rows, count);
        }
        std::thread::scope(|scope| {
            let blocks: Vec<_> = rows
                .step_by(block)
                .map(|start| {
                    let count = count.clone();
                    let end = (start + block).min(view.y0 + view.height);
                    scope.spawn(move || run(start..end, count))
                })
                .collect();
            blocks
                .into_iter()
                .map(|block| block.join().expect("a clipping count panicked"))
                .fold(Tally::default(), |total, block| Tally {
                    high: total.high + block.high,
                    low: total.low + block.low,
                    top: total.top.max(block.top),
                })
        })
    }
    fn add(&mut self, value: u32, full: u32) {
        self.top = self.top.max(value);
        self.high += u64::from(value == full);
        self.low += u64::from(value == 0);
    }
    /// The `Clipping` of `pixels` pixels, of which `full` is full scale.
    fn finish(self, full: u32, pixels: usize) -> Clipping {
        Clipping {
            high_px: self.high,
            low_px: self.low,
            pixels: pixels as u64,
            max: f64::from(self.top) / f64::from(full),
        }
    }
}

/// How sharp a region is by four common focus measures, each larger when
/// sharper, in 8-bit luminance units. Absolute values depend on the scene;
/// compare them while focusing on one subject.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
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
    if let Some(quads) = Quads::of(FrameRef::whole(frame)).filter(|q| q.columns >= 3 && q.rows >= 3)
    {
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
        luma(pixel_at(FrameRef::whole(frame), x, y)) as f64
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
/// The most bytes of provenance JSON a saved still carries. Context (16
/// entries of at most 256 bytes) and the fixed allowlist of settings keep it
/// well under this.
pub const MAX_META: usize = 16 * 1024;

pub fn encode(frame: &Frame, format: &str) -> Result<Vec<u8>> {
    encode_with_meta(frame, format, None)
}

/// `encode_parts` at the default quality, in one buffer.
pub fn encode_with_meta(
    frame: &Frame,
    format: &str,
    meta: Option<&serde_json::Value>,
) -> Result<Vec<u8>> {
    Ok(encode_parts(frame, format, None, meta)?.to_vec())
}

/// Encode a still, writing `meta` (compact provenance JSON) into PNG as an
/// iTXt chunk named `capturefab` and into JPEG as a COM segment. Raw, PPM and
/// PGM have no place for it and stay plain. Monochrome PNG of more than 8
/// bits is 16-bit gray with the samples at the top bits and an sBIT chunk
/// saying how many are significant; other monochrome PNG and PGM are one
/// 8-bit gray channel; color and Bayer are 8-bit RGB. PNG is compressed
/// fast: deflate would otherwise take most of a large frame's capture time.
/// JPEG is at `quality`, or `jpeg::QUALITY`.
///
/// Raw, and PGM of 8-bit gray and PPM of RGB8, borrow the frame's bytes,
/// so nothing is copied before they are written.
pub fn encode_parts<'a>(
    frame: &'a Frame,
    format: &str,
    quality: Option<u8>,
    meta: Option<&serde_json::Value>,
) -> Result<Encoded<'a>> {
    let json = meta.map(serde_json::to_string).transpose()?;
    if let Some(json) = &json {
        ensure!(
            json.len() <= MAX_META,
            "capture metadata exceeds {MAX_META} bytes"
        );
    }
    let header = |magic: &str| format!("{magic}\n{} {}\n255\n", frame.width, frame.height);
    match format {
        "raw" => Ok(Encoded::new(Body::Borrowed(&frame.data))),
        "jpeg" => {
            let jpeg = crate::jpeg::backend::encode(frame, quality)?;

            Ok(match json {
                Some(json) => {
                    let at = crate::jpeg::com_offset(&jpeg)?;
                    Encoded::with_insert(jpeg, at, crate::jpeg::com_segment(json.as_bytes())?)
                }
                None => Encoded::new(jpeg),
            })
        }
        "png" => {
            let (color, depth, samples, significant) = match gray16(frame)? {
                Some((samples, bits)) => (
                    png::ColorType::Grayscale,
                    png::BitDepth::Sixteen,
                    Samples::Converted(samples),
                    Some(bits),
                ),
                None => match gray8_samples(frame)? {
                    Some(gray) => (png::ColorType::Grayscale, png::BitDepth::Eight, gray, None),
                    None => (
                        png::ColorType::Rgb,
                        png::BitDepth::Eight,
                        rgb_samples(frame)?,
                        None,
                    ),
                },
            };
            if samples.converted() {
                crate::copies::record(crate::copies::Site::StillTransform, samples.len());
            }
            // Room for camera images compressed fast, so the output is
            // usually allocated once.
            let room = samples.len() / 2 + json.as_ref().map_or(0, |json| 2 * json.len()) + 4096;
            let mut bytes = Vec::with_capacity(room);
            let mut encoder = png::Encoder::new(&mut bytes, frame.width, frame.height);
            encoder.set_color(color);
            encoder.set_depth(depth);
            encoder.set_compression(png::Compression::Fast);
            // A fixed Paeth filter reduces camera-texture output size without
            // running all five filters for every scanline.
            encoder.set_filter(png::FilterType::Paeth);
            if let Some(json) = json {
                // Tags also go in the standard Description, which general
                // image tools display.
                if let Some(description) = meta.and_then(description) {
                    encoder.add_itxt_chunk("Description".into(), description)?;
                }
                encoder.add_itxt_chunk("capturefab".into(), json)?;
            }
            let mut writer = encoder.write_header()?;
            if let Some(bits) = significant {
                writer.write_chunk(png::chunk::sBIT, &[bits as u8])?;
            }
            writer.write_image_data(&samples)?;
            drop(writer);
            crate::copies::record(crate::copies::Site::PngEncode, bytes.len());
            Ok(Encoded::new(Body::Vec(bytes)))
        }
        "ppm" => Ok(Encoded::with_insert(
            rgb_samples(frame)?.into_body(crate::copies::Site::StillTransform),
            0,
            header("P6").into_bytes(),
        )),
        "pgm" => {
            let gray =
                gray8_samples(frame)?.context("PGM requires a monochrome image; use PNG or PPM")?;
            Ok(Encoded::with_insert(
                gray.into_body(crate::copies::Site::StillTransform),
                0,
                header("P5").into_bytes(),
            ))
        }
        _ => bail!("unknown image format {format}; use png, jpeg, raw, pgm or ppm"),
    }
}

/// The context tags of provenance JSON as `key=value; ...`, if it has any.
fn description(meta: &serde_json::Value) -> Option<String> {
    let context = meta.get("context")?.as_object()?;
    let tags = context
        .iter()
        .filter_map(|(key, value)| Some(format!("{key}={}", value.as_str()?)))
        .collect::<Vec<_>>();
    (!tags.is_empty()).then(|| tags.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stills_borrow_the_frame_where_they_store_its_bytes() {
        let frame = |pixel_format, bytes: usize| Frame {
            width: 7,
            height: 5,
            pixel_format,
            data: (0..35 * bytes).map(|i| (i * 13) as u8).collect(),
            ..Frame::default()
        };
        for (format, encoding, header) in [
            (MONO8, "raw", ""),
            (0x010c_0047, "raw", ""),
            (MONO8, "pgm", "P5\n7 5\n255\n"),
            (0x0108_00b1, "pgm", "P5\n7 5\n255\n"),
            (0x0108_00c6, "pgm", "P5\n7 5\n255\n"),
            (RGB8, "ppm", "P6\n7 5\n255\n"),
        ] {
            let source = frame(format, if format == RGB8 { 3 } else { 1 });
            let encoded = encode_parts(&source, encoding, None, None).unwrap();
            let parts = encoded.parts();
            let body = parts.iter().rfind(|part| !part.is_empty()).unwrap();
            assert_eq!(body.as_ptr(), source.data.as_ptr(), "{format:x} {encoding}");
            assert!(parts.contains(&header.as_bytes()));
            // The same bytes as a conversion of the samples would give.
            let samples = if encoding == "ppm" {
                rgb(&source).unwrap()
            } else if encoding == "pgm" {
                gray8(&source).unwrap().unwrap()
            } else {
                source.data.to_vec()
            };
            assert_eq!(encoded.to_vec(), [header.as_bytes(), &samples].concat());
            assert_eq!(encoded.len(), header.len() + samples.len());
        }
        // Formats that are converted are not borrowed.
        let mono12 = frame(0x0110_0005, 2);
        let pgm = encode_parts(&mono12, "pgm", None, None).unwrap();
        assert!(
            pgm.parts()
                .iter()
                .all(|part| part.as_ptr() != mono12.data.as_ptr())
        );
        assert!(encode_parts(&frame(RGB8, 3), "pgm", None, None).is_err());
        let mut short = frame(RGB8, 3);
        short.data.make_mut().pop();
        assert!(encode_parts(&short, "ppm", None, None).is_err());
    }
    #[cfg(feature = "jpeg")]
    #[test]
    fn jpeg_comments_go_in_as_a_part_where_insert_com_puts_them() {
        let frame = Frame {
            width: 31,
            height: 17,
            pixel_format: MONO8,
            data: (0..31 * 17).map(|i| (i * 7) as u8).collect(),
            ..Frame::default()
        };
        let meta = serde_json::json!({"camera": "sim:0", "context": {"lot": "7"}});
        for quality in [None, Some(75)] {
            let plain = encode_parts(&frame, "jpeg", quality, None).unwrap();
            assert_eq!(plain.parts()[1], b"");
            let parts = encode_parts(&frame, "jpeg", quality, Some(&meta)).unwrap();
            let json = serde_json::to_string(&meta).unwrap();
            assert_eq!(
                parts.to_vec(),
                crate::jpeg::insert_com(&plain.to_vec(), json.as_bytes()).unwrap()
            );
            let mut written = Vec::new();
            parts.write_to(&mut written).unwrap();
            assert_eq!(written, parts.to_vec());
        }
        let wide = serde_json::json!({ "note": "x".repeat(MAX_META) });
        assert!(encode_parts(&frame, "jpeg", None, Some(&wide)).is_err());
    }
    #[test]
    fn coordinate_and_confidence_formats_convert_but_raw_preserves_precision() {
        for format in [0x0110_00b8, 0x0110_00c7] {
            let source = f(format, vec![0x34, 0x12, 0xff, 0xff]);
            assert_eq!(rgb(&source).unwrap(), vec![0x12, 0x12, 0x12, 255, 255, 255]);
            assert_eq!(encode(&source, "raw").unwrap(), source.data);
            assert!(convertible(format));
            assert!(exposure(&source).unwrap().channels.is_none());
        }
        for format in [0x0108_00b1, 0x0108_00c6] {
            assert_eq!(
                rgb(&f(format, vec![20, 40])).unwrap(),
                vec![20, 20, 20, 40, 40, 40]
            );
        }
    }
    fn f(fmt: u32, data: Vec<u8>) -> Frame {
        Frame {
            id: 1,
            width: 2,
            height: 1,
            pixel_format: fmt,
            device_ts_ns: None,
            wall_ms: 0,
            data: data.into(),
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
        assert!(rgb(&f(0, vec![0; 6])).is_err());
        for (format, shift) in [(0x0110_0003, 2), (0x0110_0005, 4), (0x0110_0007, 8)] {
            let data = [(2u16 << shift) - 1, u16::MAX]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect();
            assert_eq!(rgb(&f(format, data)).unwrap(), [1, 1, 1, 255, 255, 255]);
            assert!(rgb(&f(format, vec![0; 3])).is_err());
        }
        assert_eq!(
            rgb(&f(RGB8, vec![1, 2, 3, 4, 5, 6, 7])).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        assert!(
            rgba(&Frame {
                width: u32::MAX,
                ..f(MONO8, vec![])
            })
            .is_err()
        );
    }
    fn reference_bayer(frame: &Frame) -> Vec<u8> {
        let pattern = match frame.pixel_format {
            0x0108_0008 => [1, 0, 2, 1],
            0x0108_0009 => [0, 1, 1, 2],
            0x0108_000a => [1, 2, 0, 1],
            _ => [2, 1, 1, 0],
        };
        let w = frame.width as i32;
        let h = frame.height as i32;
        let mut out = vec![0; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let own = pattern[((y & 1) * 2 + (x & 1)) as usize];
                    let v = if own == c {
                        frame.data[(y * w + x) as usize]
                    } else {
                        let mut sum = 0u32;
                        let mut n = 0;
                        for dy in -1..=1 {
                            for dx in -1..=1 {
                                let xx = x + dx;
                                let yy = y + dy;
                                if xx >= 0
                                    && xx < w
                                    && yy >= 0
                                    && yy < h
                                    && pattern[((yy & 1) * 2 + (xx & 1)) as usize] == c
                                {
                                    sum += frame.data[(yy * w + xx) as usize] as u32;
                                    n += 1;
                                }
                            }
                        }
                        sum.checked_div(n)
                            .map(|v| v as u8)
                            .unwrap_or(frame.data[(y * w + x) as usize])
                    };
                    out[((y * w + x) * 3 + c) as usize] = v;
                }
            }
        }
        out
    }
    fn random(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }
    #[test]
    fn bayer_matches_reference_bilinear() {
        let sizes = [
            (1, 1),
            (2, 1),
            (1, 2),
            (2, 2),
            (3, 3),
            (4, 3),
            (5, 7),
            (33, 17),
            (64, 48),
            // Above the threading threshold, with uneven row blocks.
            (1031, 517),
        ];
        for (seed, (width, height)) in sizes.into_iter().enumerate() {
            for pixel_format in 0x0108_0008..=0x0108_000b {
                let mut frame = f(pixel_format, random(seed as u64 + 1, width * height));
                (frame.width, frame.height) = (width as u32, height as u32);
                assert_eq!(
                    rgb(&frame).unwrap(),
                    reference_bayer(&frame),
                    "{width}x{height} {pixel_format:x}"
                );
                frame.data = vec![255; width * height].into();
                assert_eq!(rgb(&frame).unwrap(), reference_bayer(&frame));
            }
        }
    }
    #[test]
    fn quad_scopes_agree_with_the_demosaic() {
        // A smooth scene with fine texture, sampled through each mosaic, against
        // the same pixels demosaiced, which is what the scopes measured before.
        let (width, height) = (640usize, 480usize);
        let scene = |x: usize, y: usize, c: usize| {
            // Blown out along the top, crushed along the bottom.
            if !(48..432).contains(&y) {
                return if y < 48 { 255 } else { 0 };
            }
            let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
            let base = [
                90.0 + 70.0 * (fx * 3.1 + fy * 1.7).sin(),
                110.0 + 60.0 * (fy * 4.3 - fx * 0.9).cos(),
                80.0 + 50.0 * (fx * fy * 9.0).sin(),
            ][c];
            (base + 18.0 * (x as f32 * 0.21).sin() * (y as f32 * 0.17).cos()).clamp(0.0, 255.0)
                as u8
        };
        for format in 0x0108_0008..=0x0108_000b {
            let pattern = bayer_pattern(format);
            let mosaic = Frame {
                width: width as u32,
                height: height as u32,
                ..f(
                    format,
                    (0..width * height)
                        .map(|i| {
                            scene(
                                i % width,
                                i / width,
                                pattern[((i / width) & 1) * 2 + ((i % width) & 1)],
                            )
                        })
                        .collect(),
                )
            };
            let demosaiced = Frame {
                pixel_format: RGB8,
                data: rgb(&mosaic).unwrap().into(),
                ..mosaic.clone()
            };
            let (quads, bilinear) = (exposure(&mosaic).unwrap(), exposure(&demosaiced).unwrap());
            assert!((quads.mean - bilinear.mean).abs() < 0.005, "{format:x}");
            assert!(quads.highlights > 0.05 && quads.shadows > 0.05);
            assert!((quads.highlights - bilinear.highlights).abs() < 0.01);
            assert!((quads.shadows - bilinear.shadows).abs() < 0.01);
            let share = |bins: &[u32; 64]| {
                let total = bins.iter().sum::<u32>() as f32;
                bins.map(|n| n as f32 / total)
            };
            let (a, b) = (share(&quads.luma), share(&bilinear.luma));
            // Earth mover's distance between the two, in bins.
            let (mut carried, mut distance) = (0.0f32, 0.0f32);
            for (a, b) in a.iter().zip(&b) {
                carried += a - b;
                distance += carried.abs();
            }
            assert!(
                distance < 0.25,
                "{format:x}: histograms {distance} bins apart"
            );
            // Focus scores rank a focus sweep the same way.
            let sweep: Vec<(f32, f32)> = [0usize, 1, 2, 4]
                .into_iter()
                .map(|blur| {
                    let soft = Frame {
                        data: (0..width * height)
                            .map(|i| {
                                let (x, y) = (i % width, i / width);
                                let c = pattern[(y & 1) * 2 + (x & 1)];
                                let (x0, x1) = (x.saturating_sub(blur), (x + blur).min(width - 1));
                                let sum: usize = (x0..=x1)
                                    .map(|x| (x / 8 % 2) * 200 + scene(x, y, c) as usize / 8)
                                    .sum();
                                (sum / (x1 - x0 + 1)) as u8
                            })
                            .collect(),
                        ..mosaic.clone()
                    };
                    let region = [100, 100, 400, 300];
                    let reference = Frame {
                        pixel_format: RGB8,
                        data: rgb(&soft).unwrap().into(),
                        ..soft.clone()
                    };
                    (
                        sharpness(&soft, region).unwrap().tenengrad,
                        sharpness(&reference, region).unwrap().tenengrad,
                    )
                })
                .collect();
            for pair in sweep.windows(2) {
                assert!(
                    pair[0].0 > pair[1].0 && pair[0].1 > pair[1].1,
                    "{format:x}: {sweep:?}"
                );
            }
        }
        // Too small for quads: measured pixel by pixel as before.
        let tiny = Frame {
            width: 4,
            height: 4,
            ..f(0x0108_0009, (0..16).map(|i| i * 15).collect())
        };
        assert!(sharpness(&tiny, [0, 0, 4, 4]).is_ok());
    }
    #[test]
    fn conversions_leave_a_core_free() {
        assert_eq!(worker_threads(1000), 1);
        assert_eq!([1, 2, 4, 8, 9, 16].map(spare_cores), [1, 1, 3, 7, 8, 8]);
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert!(
            (1..=spare_cores(cores)).contains(&worker_threads(64 << 20)),
            "CAPTUREFAB_THREADS may reduce parallel work, but never exceed available spare cores"
        );
    }
    #[test]
    fn direct_conversions_match_rgb() {
        let formats = [
            (MONO8, 1),
            (RGB8, 3),
            (0x0218_0015, 3),
            (0x0110_0003, 2),
            (0x0110_0005, 2),
            (0x0110_0025, 2),
            (0x0110_0007, 2),
            (0x0108_0008, 1),
            (0x0108_0009, 1),
            (0x0108_000a, 1),
            (0x0108_000b, 1),
        ];
        for (width, height) in [(1, 1), (2, 1), (5, 7), (33, 17)] {
            for (format, bytes) in formats {
                let frame = Frame {
                    width,
                    height,
                    ..f(
                        format,
                        random(format as u64, (width * height) as usize * bytes),
                    )
                };
                let full = rgb(&frame).unwrap();
                assert_eq!(convert(&frame, |p| p).unwrap().into_flattened(), full);
                // Into a reused buffer.
                let mut reused = vec![[1u8; 3]; 3];
                convert_into(&frame, &mut reused, |p| p).unwrap();
                assert_eq!(reused.into_flattened(), full);
                let rgba = rgba(&frame).unwrap();
                assert_eq!(rgba.len(), full.len() / 3 * 4);
                assert!(
                    rgba.as_chunks::<4>()
                        .0
                        .iter()
                        .zip(full.as_chunks::<3>().0)
                        .all(|(a, b)| a[..3] == b[..] && a[3] == 255),
                    "{width}x{height} {format:x}"
                );
            }
        }
        assert!(rgba(&f(RGB8, vec![0])).is_err());
        assert!(convert(&f(0, vec![0; 6]), |p| p).is_err());
    }
    #[test]
    fn packed_and_wide_conversions_match_the_per_pixel_reference() {
        // Every packed mono/Bayer layout and every unpacked wide mosaic goes
        // through `eight_bit`'s group-wise rows; `pixel_at` reads each
        // sample on its own and is the reference for all of them.
        let formats = (0x0100_0000..=0x0110_0200)
            .filter(|&v| packing(v).is_some() || wide_bayer(v).is_some())
            .collect::<Vec<_>>();
        assert_eq!(formats.len(), 41);
        for format in formats {
            assert!(!streamed(format) && convertible(format), "{format:x}");
            for (width, height) in [(1, 1), (2, 3), (3, 2), (5, 7), (33, 17)] {
                let bytes = row_bytes(width, format) * height as usize;
                let frame = Frame {
                    width,
                    height,
                    ..f(format, random(u64::from(format ^ width), bytes))
                };
                let reference = (0..height as usize)
                    .flat_map(|y| (0..width as usize).map(move |x| (x, y)))
                    .flat_map(|(x, y)| pixel_at(FrameRef::whole(&frame), x, y))
                    .collect::<Vec<_>>();
                assert_eq!(
                    rgb(&frame).unwrap(),
                    reference,
                    "{format:x} {width}x{height}"
                );
                let mut reused = vec![[9; 4]; 3];
                convert_into(&frame, &mut reused, |[r, g, b]| [r, g, b, 255]).unwrap();
                let opaque = reference.as_chunks::<3>().0.iter();
                let expected = opaque
                    .flat_map(|&[r, g, b]| [r, g, b, 255])
                    .collect::<Vec<_>>();
                assert_eq!(reused.into_flattened(), expected, "{format:x}");
                let gray = reference.iter().step_by(3).copied().collect();
                let expected = (!is_bayer(format)).then_some(gray);
                assert_eq!(gray8(&frame).unwrap(), expected, "{format:x}");
                let mut short = frame;
                short.data.make_mut().pop();
                assert!(rgb(&short).is_err(), "{format:x}");
                assert!(is_bayer(format) || gray8(&short).is_err(), "{format:x}");
            }
        }
    }
    #[test]
    fn sparse_sampling_matches_full_conversion() {
        for (width, height) in [(1, 1), (2, 3), (5, 7), (33, 17), (400, 400)] {
            for (format, bytes) in [
                (MONO8, 1),
                (RGB8, 3),
                (0x0218_0015, 3),
                (0x0110_0005, 2),
                (0x0110_0025, 2),
                (0x0110_0007, 2),
                (0x0108_0008, 1),
                (0x0108_0009, 1),
                (0x0108_000a, 1),
                (0x0108_000b, 1),
            ] {
                let frame = Frame {
                    width,
                    height,
                    ..f(
                        format,
                        random(
                            format as u64 ^ width as u64,
                            (width * height) as usize * bytes,
                        ),
                    )
                };
                assert!(convertible(format));
                let full = rgb(&frame).unwrap();
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let i = (y * width as usize + x) * 3;
                        assert_eq!(
                            pixel_at(FrameRef::whole(&frame), x, y),
                            full[i..i + 3],
                            "{format:x} {x},{y}"
                        );
                    }
                }
                // Bayer exposure reads whole quads; see quad_scopes_agree_with_the_demosaic.
                if is_bayer(format) {
                    continue;
                }
                let mut bins = [0u32; 64];
                let pixels = full.as_chunks::<3>().0;
                for i in sample_indices(pixels.len(), 65_536) {
                    let p = &pixels[i];
                    bins[(p[0] as usize * 54 + p[1] as usize * 183 + p[2] as usize * 19) >> 10] +=
                        1;
                }
                assert_eq!(exposure(&frame).unwrap().luma, bins);
            }
        }
        assert!(!convertible(0));
        assert!(exposure(&f(0, vec![0; 6])).is_err());
        assert!(exposure(&f(RGB8, vec![0; 5])).is_err());
    }
    #[test]
    fn clipping_counts_every_pixel_at_native_depth() {
        let clipped = |frame: Frame| {
            let clipping = clipping(&frame).unwrap();
            (clipping.high_px, clipping.low_px, clipping.max)
        };
        let sized = |fmt, data: Vec<u8>| Frame {
            width: 4,
            height: 1,
            ..f(fmt, data)
        };
        assert_eq!(clipped(sized(MONO8, vec![0, 255, 254, 9])), (1, 1, 1.0));
        // 4095 is full scale for Mono12, whatever the bits above it say.
        assert_eq!(
            clipped(sized(0x0110_0005, le16(&[0x0FFF, 0xFFFF, 0x0FFE, 0]))),
            (2, 1, 1.0)
        );
        // Packed Mono10p: 1023 is full scale.
        assert_eq!(clipped(sized(0x010a_0046, vec![0xFF; 5])), (4, 0, 1.0));
        // Color clips in any channel and is black in all of them.
        assert_eq!(
            clipped(sized(RGB8, vec![255, 0, 0, 0, 0, 0, 1, 2, 3, 0, 0, 0])),
            (1, 2, 1.0)
        );
        // A Bayer mosaic counts each sample at its own depth.
        let (high, low, max) = clipped(sized(0x0110_0011, le16(&[2047, 0, 4095, 1])));
        assert_eq!((high, low), (1, 1));
        assert!((max - 1.0).abs() < 1e-9);
        assert!(clipping(&sized(0x0110_00b8, vec![0; 2])).is_err());
    }
    #[test]
    fn exposure_reports_channels_clipping_and_mean() {
        // Black, white, mid grey and pure red.
        let frame = Frame {
            width: 4,
            ..f(RGB8, vec![0, 0, 0, 255, 255, 255, 128, 128, 128, 255, 0, 0])
        };
        let exposure = exposure(&frame).unwrap();
        let channels = exposure.channels.unwrap();
        assert_eq!(
            (channels[0][63], channels[1][63], channels[2][0]),
            (2, 1, 2)
        );
        assert_eq!((exposure.shadows, exposure.highlights), (0.25, 0.5));
        // Luminance of black, white, grey and red.
        let expected = (255 + 128 + ((255 * 54) >> 8)) as f32 / 4.0 / 255.0;
        assert!((exposure.mean - expected).abs() < 1e-6);
        let mono = super::exposure(&f(MONO8, vec![0, 255])).unwrap();
        assert!(mono.channels.is_none());
        assert_eq!((mono.shadows, mono.highlights), (0.5, 0.5));
    }
    #[test]
    fn sharpness_prefers_crisp_detail_in_the_region() {
        let (w, h) = (64u32, 48u32);
        let image = |sharp: bool| Frame {
            width: w,
            height: h,
            ..f(
                MONO8,
                (0..w * h)
                    .map(|i| {
                        let (x, y) = (i % w, i / w);
                        let edge = |v: u32| match (sharp, v % 8) {
                            (true, p) => {
                                if p < 4 {
                                    40
                                } else {
                                    220
                                }
                            }
                            // The same pattern as a soft ramp.
                            (false, p) => [130, 160, 190, 160, 130, 100, 70, 100][p as usize],
                        };
                        // Detail only on the left; the right half is flat.
                        if x < w / 2 { edge(x + y) } else { 128 }
                    })
                    .collect(),
            )
        };
        let left = [0, 0, w / 2, h];
        let crisp = sharpness(&image(true), left).unwrap();
        let soft = sharpness(&image(false), left).unwrap();
        for (crisp, soft) in [
            (crisp.laplacian, soft.laplacian),
            (crisp.tenengrad, soft.tenengrad),
            (crisp.brenner, soft.brenner),
            (crisp.variance, soft.variance),
        ] {
            assert!(crisp > soft && soft > 0.0, "{crisp} > {soft}");
        }
        let flat = sharpness(&image(true), [w / 2 + 2, 0, w / 2, h]).unwrap();
        assert_eq!(flat, Sharpness::default());
        // Regions past the frame are clamped rather than rejected.
        assert!(sharpness(&image(true), [u32::MAX, u32::MAX, 9, 9]).is_ok());
        assert!(
            sharpness(&image(true), [0, 0, u32::MAX, u32::MAX])
                .unwrap()
                .laplacian
                > 0.0
        );
        assert!(sharpness(&f(MONO8, vec![0, 0]), [0, 0, 2, 1]).is_err());
        // One-pixel detail over a region large enough for an even stride.
        let (w, h) = (512u32, 512u32);
        for pattern in [|x: u32, y: u32| (x + y) % 2, |_x: u32, y: u32| y % 2] {
            let fine = Frame {
                width: w,
                height: h,
                ..f(
                    MONO8,
                    (0..w * h)
                        .map(|i| pattern(i % w, i / w) as u8 * 200)
                        .collect(),
                )
            };
            let score = sharpness(&fine, [0, 0, w, h]).unwrap();
            assert!(score.laplacian > 0.0 && score.variance > 0.0, "{score:?}");
        }
    }
    fn decode_png(data: &[u8]) -> (png::ColorType, png::BitDepth, Vec<u8>, png::Info<'static>) {
        let mut reader = png::Decoder::new(data).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let out = reader.next_frame(&mut buf).unwrap();
        buf.truncate(out.buffer_size());
        let info = reader.info().clone();
        (out.color_type, out.bit_depth, buf, info)
    }
    #[test]
    fn png_is_gray_for_mono_and_rgb_otherwise() {
        let sized = |fmt, data: Vec<u8>| Frame {
            width: 4,
            height: 2,
            ..f(fmt, data)
        };
        let mono = sized(MONO8, (0..8).map(|i| i * 30).collect());
        let (color, depth, samples, _) = decode_png(&encode(&mono, "png").unwrap());
        assert_eq!(
            (color, depth),
            (png::ColorType::Grayscale, png::BitDepth::Eight)
        );
        assert_eq!(samples, mono.data);
        // Monochrome of more than 8 bits keeps every bit: 16-bit gray with
        // the samples at the top and an sBIT chunk saying how many count.
        let sixteen = |frame: &Frame| {
            let (color, depth, samples, info) = decode_png(&encode(frame, "png").unwrap());
            assert_eq!(
                (color, depth),
                (png::ColorType::Grayscale, png::BitDepth::Sixteen),
                "{}",
                pixel_format_name(frame.pixel_format)
            );
            let samples = samples
                .as_chunks::<2>()
                .0
                .iter()
                .map(|sample| u16::from_be_bytes(*sample))
                .collect::<Vec<_>>();
            (samples, info.sbit.map(|bits| bits.into_owned()))
        };
        let mono12 = sized(0x0110_0005, le16(&[0x0ABC; 8]));
        assert_eq!(sixteen(&mono12), (vec![0xABC0; 8], Some(vec![12])));
        // Packed Mono12p and Mono12Packed rows of 0xABC unpack to the same
        // samples; bits above the format's are not samples.
        for mono in [
            sized(0x010c_0047, [0xBC, 0xCA, 0xAB].repeat(4)),
            sized(0x010c_0006, [0xAB, 0xCC, 0xAB].repeat(4)),
            sized(0x0110_0005, le16(&[0xFABC; 8])),
        ] {
            assert_eq!(sixteen(&mono), (vec![0xABC0; 8], Some(vec![12])));
        }
        assert_eq!(
            sixteen(&sized(0x0110_0025, le16(&[0x2ABC; 8]))),
            (vec![0xAAF0; 8], Some(vec![14]))
        );
        assert_eq!(
            sixteen(&sized(0x010a_0046, [0xFF; 10].to_vec())),
            (vec![0xFFC0; 8], Some(vec![10]))
        );
        assert_eq!(
            sixteen(&sized(0x0110_0007, le16(&[0x1234; 8]))),
            (vec![0x1234; 8], Some(vec![16]))
        );
        // JPEG and PGM keep the top 8 bits, as the preview does.
        assert_eq!(
            encode(&mono12, "pgm").unwrap()[b"P5\n4 2\n255\n".len()..],
            [0xAB; 8]
        );
        assert_eq!(bit_depth(0x0110_0005, "png"), (16, None));
        assert_eq!(bit_depth(0x010c_0047, "raw"), (12, None));
        assert_eq!(bit_depth(0x010c_0047, "jpeg"), (8, Some(12)));
        assert_eq!(bit_depth(0x0110_0011, "png"), (8, Some(12)));
        assert_eq!(bit_depth(RGB8, "png"), (8, None));
        // Bayer at any depth or packing, YUV 4:2:2 and RGBa stay RGB.
        for color_frame in [
            sized(RGB8, vec![7; 24]),
            sized(0x0108_0009, vec![7; 8]),
            sized(0x0110_002f, vec![7; 16]),
            sized(0x010c_0059, vec![7; 12]),
            sized(0x0210_0032, vec![7; 16]),
            sized(0x0220_0016, vec![7; 32]),
        ] {
            let name = pixel_format_name(color_frame.pixel_format);
            let (color, depth, samples, _) = decode_png(&encode(&color_frame, "png").unwrap());
            assert_eq!(
                (color, depth),
                (png::ColorType::Rgb, png::BitDepth::Eight),
                "{name}"
            );
            assert_eq!(samples, rgb(&color_frame).unwrap(), "{name}");
        }
    }
    #[test]
    fn pgm_matches_the_red_channel_of_rgb() {
        for fmt in [
            MONO8,
            0x0108_00b1,
            0x0108_00c6,
            0x0110_0003,
            0x0110_0005,
            0x0110_0025,
            0x0110_0007,
            0x0110_00b8,
            0x0110_00c7,
            0x010a_0046,
            0x010c_0047,
            0x010c_0004,
            0x010c_0006,
        ] {
            let bytes = row_bytes(2, fmt);
            let frame = f(fmt, (0..bytes).map(|i| i as u8 * 77 + 3).collect());
            let mut expected = b"P5\n2 1\n255\n".to_vec();
            expected.extend(rgb(&frame).unwrap().as_chunks::<3>().0.iter().map(|p| p[0]));
            assert_eq!(encode(&frame, "pgm").unwrap(), expected, "{fmt:#x}");
            assert!(monochrome(fmt), "{fmt:#x}");
        }
        for (fmt, bytes) in [
            (RGB8, 6),
            (0x0108_0009, 2),
            (0x0110_002f, 4),
            (0x010c_0059, 3),
            (0x0210_0032, 4),
            (0x0220_0016, 8),
        ] {
            assert!(encode(&f(fmt, vec![0; bytes]), "pgm").is_err(), "{fmt:#x}");
            assert!(!monochrome(fmt), "{fmt:#x}");
        }
    }
    #[test]
    fn metadata_goes_into_png_and_jpeg() {
        let meta = serde_json::json!({"frame_id":1,"context":{"part":"B-7","line":"L1"}});
        let frame = f(MONO8, vec![0, 255]);
        let (_, _, _, info) = decode_png(&encode_with_meta(&frame, "png", Some(&meta)).unwrap());
        let text = |key: &str| {
            info.utf8_text
                .iter()
                .find(|chunk| chunk.keyword == key)
                .map(|chunk| chunk.get_text().unwrap())
        };
        let embedded: serde_json::Value =
            serde_json::from_str(&text("capturefab").unwrap()).unwrap();
        assert_eq!(embedded, meta);
        assert_eq!(text("Description").unwrap(), "line=L1; part=B-7");
        // Without tags only the provenance chunk is written.
        let plain = serde_json::json!({"frame_id":1});
        let (_, _, _, info) = decode_png(&encode_with_meta(&frame, "png", Some(&plain)).unwrap());
        assert_eq!(info.utf8_text.len(), 1);
        assert!(
            decode_png(&encode(&frame, "png").unwrap())
                .3
                .utf8_text
                .is_empty()
        );
        // Formats without a metadata field stay byte-identical.
        for format in ["raw", "pgm", "ppm"] {
            assert_eq!(
                encode_with_meta(&frame, format, Some(&meta)).unwrap(),
                encode(&frame, format).unwrap()
            );
        }
        let huge = serde_json::json!({"context":{"note":"x".repeat(MAX_META)}});
        assert!(encode_with_meta(&frame, "png", Some(&huge)).is_err());
    }
    #[cfg(feature = "jpeg")]
    #[test]
    fn jpeg_carries_metadata_as_a_comment() {
        let meta = serde_json::json!({"context":{"part":"B-7"}});
        let frame = Frame {
            width: 16,
            height: 16,
            ..f(MONO8, vec![90; 256])
        };
        let bytes = encode_with_meta(&frame, "jpeg", Some(&meta)).unwrap();
        assert!(crate::jpeg::well_formed(&bytes, 16, 16));
        let image = turbojpeg::decompress(&bytes, turbojpeg::PixelFormat::GRAY).unwrap();
        assert_eq!((image.width, image.height), (16, 16));
        let json = meta.to_string();
        let at = bytes.windows(2).position(|w| w == [0xFF, 0xFE]).unwrap();
        let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        assert_eq!(&bytes[at + 4..at + 2 + length], json.as_bytes());
    }
    fn sized(pixel_format: u32, width: u32, height: u32, data: Vec<u8>) -> Frame {
        Frame {
            width,
            height,
            ..f(pixel_format, data)
        }
    }
    fn le16(samples: &[u16]) -> Vec<u8> {
        samples.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    #[test]
    fn native_packed_sampling_matches_whole_sample_images_at_every_row_phase() {
        // Independent bit-at-a-time writer: exercises every possible row
        // tail and distinguishes a row boundary from a continuous bitstream.
        let pack = |samples: &[u16], bits: usize| {
            let mut bytes = vec![0u8; (samples.len() * bits).div_ceil(8)];
            for (i, sample) in samples.iter().enumerate() {
                for bit in 0..bits {
                    bytes[(i * bits + bit) / 8] |=
                        (((sample >> bit) & 1) as u8) << ((i * bits + bit) % 8);
                }
            }
            bytes
        };
        for (packed, whole, bits) in [
            (0x010a_0046, 0x0110_0003, 10),
            (0x010c_0047, 0x0110_0005, 12),
            (0x010e_0104, 0x0110_0025, 14),
            (0x010a_0058, 0x0110_000d, 10),
            (0x010c_0059, 0x0110_0011, 12),
            (0x010e_0105, 0x0110_0109, 14),
            (0x010e_0106, 0x0110_010a, 14),
            (0x010e_0107, 0x0110_010b, 14),
            (0x010e_0108, 0x0110_010c, 14),
        ] {
            for width in 1..=11 {
                let samples: Vec<u16> = (0..width * 9)
                    .map(|i| ((i * 719 + 91) & ((1 << bits) - 1)) as u16)
                    .collect();
                let bytes = samples
                    .chunks(width as usize)
                    .flat_map(|row| pack(row, bits))
                    .collect();
                let source = sized(packed, width, 9, bytes);
                let reference = sized(whole, width, 9, le16(&samples));
                assert_eq!(
                    unpacked(&source).unwrap().data,
                    reference.data,
                    "{packed:x}/{width}"
                );
                assert_eq!(
                    rgb(&source).unwrap(),
                    rgb(&reference).unwrap(),
                    "{packed:x}/{width}"
                );
                assert_eq!(gray8(&source).unwrap(), gray8(&reference).unwrap());
                assert_eq!(exposure(&source).unwrap(), exposure(&reference).unwrap());
                assert_eq!(crate::auto::meter(&source), crate::auto::meter(&reference));
                if width >= 3 {
                    assert_eq!(
                        sharpness(&source, [0, 0, width, 9]).unwrap(),
                        sharpness(&reference, [0, 0, width, 9]).unwrap()
                    );
                }
                assert_eq!(
                    clipping(&source).unwrap(),
                    clipping(&reference).unwrap(),
                    "{packed:x}/{width}"
                );
                assert_eq!(
                    encode(&source, "png").unwrap(),
                    encode(&reference, "png").unwrap(),
                    "{packed:x}/{width}"
                );
                let mut direct = vec![[7; 3]; 2];
                convert_into(&source, &mut direct, |p| p).unwrap();
                assert_eq!(direct.into_flattened(), rgb(&reference).unwrap());
                let mut short = source;
                short.data.make_mut().pop();
                assert!(exposure(&short).is_err());
                assert!(rgba(&short).is_err());
                assert!(clipping(&short).is_err());
            }
        }
    }

    #[test]
    fn exposure_never_exceeds_its_sample_budget() {
        for count in [65_535, 65_536, 65_537, 131_071, 131_072] {
            for frame in [
                sized(MONO8, count, 1, vec![127; count as usize]),
                sized(0x0108_0009, 2 * count, 2, vec![127; 4 * count as usize]),
            ] {
                let exposure = exposure(&frame).unwrap();
                let samples: u32 = exposure.luma.iter().sum();
                assert!(samples > 0 && samples <= 65_536, "{samples}");
                if let Some(channels) = exposure.channels {
                    for channel in channels {
                        assert_eq!(channel.iter().sum::<u32>(), samples);
                    }
                }
            }
        }
    }

    #[test]
    fn stratified_scopes_measure_periodic_detail_without_phase_bias() {
        assert_eq!(sample_indices(0, 65_536).count(), 0);
        assert_eq!(
            sample_indices(7, 65_536).collect::<Vec<_>>(),
            (0..7).collect::<Vec<_>>()
        );
        for count in [65_535, 65_536, 65_537, 1_048_576] {
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
            let frame = sized(MONO8, 1024, 1024, data);
            let measured = exposure(&frame).unwrap();
            assert!(
                (measured.mean - 0.5).abs() < 0.01,
                "pattern {pattern}: {}",
                measured.mean
            );
            // The same strata of the converted image bin the same luminance.
            let rgba = rgba(&frame).unwrap();
            let pixels = rgba.as_chunks::<4>().0;
            let mut bins = [0u32; 64];
            for i in sample_indices(pixels.len(), 65_536) {
                let p = pixels[i];
                bins[(p[0] as usize * 54 + p[1] as usize * 183 + p[2] as usize * 19) >> 10] += 1;
            }
            assert_eq!(bins, measured.luma);
        }
    }
    #[test]
    fn packed_samples_unpack_as_pfnc_and_gige_vision_lay_them_out() {
        // Odd widths: each row ends with a half-filled group and the next
        // row starts on a fresh byte.
        let twelve = [0xabc, 0x123, 0xfed, 0x001, 0x800, 0x7ff];
        // Mono12p: the first sample from bit 0 of byte 0, its high nibble in
        // the low nibble of byte 1.
        let mono12p = vec![
            0xbc, 0x3a, 0x12, 0xed, 0x0f, //
            0x01, 0x00, 0x80, 0xff, 0x07,
        ];
        // Mono12Packed: high bits in the outer bytes, low nibbles in the middle.
        let mono12_packed = vec![
            0xab, 0x3c, 0x12, 0xfe, 0x0d, //
            0x00, 0x01, 0x80, 0x7f, 0x0f,
        ];
        for (format, data) in [(0x010c_0047, mono12p), (0x010c_0006, mono12_packed)] {
            let frame = sized(format, 3, 2, data);
            let unpacked = unpacked(&frame).unwrap();
            assert_eq!(unpacked.pixel_format, 0x0110_0005, "{format:x}");
            assert_eq!(unpacked.data, le16(&twelve), "{format:x}");
            let gray: Vec<u8> = twelve.iter().flat_map(|v| [(v >> 4) as u8; 3]).collect();
            assert_eq!(rgb(&frame).unwrap(), gray, "{format:x}");
        }
        // Mono10p: four samples in five bytes; a fifth starts a new group.
        let mono10p = vec![0x55, 0xa9, 0x0a, 0xcf, 0xc3, 0xe1, 0x03];
        let frame = unpacked(&sized(0x010a_0046, 5, 1, mono10p))
            .unwrap()
            .into_owned();
        assert_eq!(frame.pixel_format, 0x0110_0003);
        assert_eq!(frame.data, le16(&[0x155, 0x2aa, 0x0f0, 0x30f, 0x3e1]));
        // Mono10Packed: two samples in three bytes, their two low bits at
        // bits 0-1 and 4-5 of the middle byte.
        let frame = sized(0x010c_0004, 3, 1, vec![0x55, 0x21, 0xaa, 0xf8, 0x01]);
        assert_eq!(unpacked(&frame).unwrap().data, le16(&[0x155, 0x2aa, 0x3e1]));
        assert_eq!(
            rgb(&frame).unwrap(),
            [0x55, 0x55, 0x55, 0xaa, 0xaa, 0xaa, 0xf8, 0xf8, 0xf8]
        );
        assert_eq!([3, 5].map(|w| row_bytes(w, 0x010a_0046)), [4, 7]);
        assert_eq!([3, 4].map(|w| row_bytes(w, 0x010c_0004)), [5, 6]);
        let short = sized(0x010c_0047, 3, 2, vec![0; 9]);
        let error = rgb(&short).unwrap_err().to_string();
        assert!(error.contains("truncated Mono12p frame"), "{error}");
        assert!(exposure(&short).is_err());
    }
    #[test]
    fn every_bayer_depth_and_packing_keeps_its_color_filter() {
        // Codes from GenICam's pixel format values, with the unpacked format
        // and the 8-bit mosaic of the same filter.
        let formats = [
            (0x0110_000c, "BayerGR10", 0x0110_000c, 0x0108_0008),
            (0x0110_000d, "BayerRG10", 0x0110_000d, 0x0108_0009),
            (0x0110_000e, "BayerGB10", 0x0110_000e, 0x0108_000a),
            (0x0110_000f, "BayerBG10", 0x0110_000f, 0x0108_000b),
            (0x0110_0010, "BayerGR12", 0x0110_0010, 0x0108_0008),
            (0x0110_0011, "BayerRG12", 0x0110_0011, 0x0108_0009),
            (0x0110_0012, "BayerGB12", 0x0110_0012, 0x0108_000a),
            (0x0110_0013, "BayerBG12", 0x0110_0013, 0x0108_000b),
            (0x0110_002e, "BayerGR16", 0x0110_002e, 0x0108_0008),
            (0x0110_002f, "BayerRG16", 0x0110_002f, 0x0108_0009),
            (0x0110_0030, "BayerGB16", 0x0110_0030, 0x0108_000a),
            (0x0110_0031, "BayerBG16", 0x0110_0031, 0x0108_000b),
            (0x010a_0056, "BayerGR10p", 0x0110_000c, 0x0108_0008),
            (0x010a_0058, "BayerRG10p", 0x0110_000d, 0x0108_0009),
            (0x010a_0054, "BayerGB10p", 0x0110_000e, 0x0108_000a),
            (0x010a_0052, "BayerBG10p", 0x0110_000f, 0x0108_000b),
            (0x010c_0057, "BayerGR12p", 0x0110_0010, 0x0108_0008),
            (0x010c_0059, "BayerRG12p", 0x0110_0011, 0x0108_0009),
            (0x010c_0055, "BayerGB12p", 0x0110_0012, 0x0108_000a),
            (0x010c_0053, "BayerBG12p", 0x0110_0013, 0x0108_000b),
            (0x010c_0026, "BayerGR10Packed", 0x0110_000c, 0x0108_0008),
            (0x010c_0027, "BayerRG10Packed", 0x0110_000d, 0x0108_0009),
            (0x010c_0028, "BayerGB10Packed", 0x0110_000e, 0x0108_000a),
            (0x010c_0029, "BayerBG10Packed", 0x0110_000f, 0x0108_000b),
            (0x010c_002a, "BayerGR12Packed", 0x0110_0010, 0x0108_0008),
            (0x010c_002b, "BayerRG12Packed", 0x0110_0011, 0x0108_0009),
            (0x010c_002c, "BayerGB12Packed", 0x0110_0012, 0x0108_000a),
            (0x010c_002d, "BayerBG12Packed", 0x0110_0013, 0x0108_000b),
        ];
        for (format, name, unpacked, bayer8) in formats {
            assert_eq!(pixel_format_name(format), name);
            assert_eq!(unpacked_format(format), unpacked, "{name}");
            assert_eq!(bayer_pattern(format), bayer_pattern(bayer8), "{name}");
            assert!(convertible(format) && is_bayer(format), "{name}");
        }
        // A 12-bit BayerRG mosaic, unpacked and packed both ways, converts
        // as the 8-bit mosaic of its high bits.
        let (width, height) = (6u32, 4u32);
        let samples: Vec<u16> = random(3, 24 * 2)
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| u16::from_le_bytes(p) & 0xfff)
            .collect();
        let pairs = samples.as_chunks::<2>().0;
        let pfnc = pairs
            .iter()
            .flat_map(|&[a, b]| [a as u8, (a >> 8) as u8 | (b << 4) as u8, (b >> 4) as u8]);
        let gige = pairs.iter().flat_map(|&[a, b]| {
            [
                (a >> 4) as u8,
                (a & 0xf) as u8 | (b << 4) as u8,
                (b >> 4) as u8,
            ]
        });
        let reference = sized(
            0x0108_0009,
            width,
            height,
            samples.iter().map(|v| (v >> 4) as u8).collect(),
        );
        for frame in [
            sized(0x0110_0011, width, height, le16(&samples)),
            sized(0x010c_0059, width, height, pfnc.collect()),
            sized(0x010c_002b, width, height, gige.collect()),
        ] {
            let name = pixel_format_name(frame.pixel_format);
            assert_eq!(rgb(&frame).unwrap(), rgb(&reference).unwrap(), "{name}");
            assert_eq!(exposure(&frame).unwrap(), exposure(&reference).unwrap());
            assert_eq!(
                sharpness(&frame, [0, 0, 6, 4]).unwrap(),
                sharpness(&reference, [0, 0, 6, 4]).unwrap()
            );
        }
        // 10- and 16-bit mosaics shift by their own depth.
        for (format, shift) in [(0x0110_000f, 2), (0x0110_0031, 8)] {
            let data = le16(&[1 << shift, 2 << shift, 3 << shift, u16::MAX]);
            assert_eq!(
                rgb(&sized(format, 2, 2, data)).unwrap(),
                rgb(&sized(0x0108_000b, 2, 2, vec![1, 2, 3, 255])).unwrap()
            );
        }
    }
    #[test]
    fn yuv_422_decodes_as_full_range_bt601_in_either_byte_order() {
        // A gray pair and a red pair (JFIF Y'CbCr of R'G'B' 255, 0, 0), then
        // an odd last pixel without Cr.
        let yuyv = vec![200, 128, 50, 128, 76, 85, 76, 255, 128, 255];
        let uyvy = vec![128, 200, 128, 50, 85, 76, 255, 76, 255, 128];
        assert_eq!(ycbcr(128, 255, 128), [128, 84, 255]);
        let expected = [
            [200, 200, 200],
            [50, 50, 50],
            [254, 0, 0],
            [254, 0, 0],
            [128, 84, 255],
        ]
        .concat();
        for (format, data) in [
            (0x0210_0032, yuyv.clone()),
            (0x0210_003b, yuyv),
            (0x0210_001f, uyvy.clone()),
            (0x0210_0043, uyvy),
        ] {
            let name = pixel_format_name(format);
            let frame = sized(format, 5, 1, data);
            assert_eq!(rgb(&frame).unwrap(), expected, "{name}");
            assert_eq!(unpacked(&frame).unwrap().pixel_format, RGB8);
            assert!(exposure(&frame).unwrap().channels.is_some());
            let mut rows = frame.clone();
            (rows.width, rows.height) = (2, 2);
            rows.data.make_mut().truncate(8);
            assert_eq!(rgb(&rows).unwrap(), expected[..12], "{name}");
            rows.data.make_mut().pop();
            assert!(rgb(&rows).is_err());
        }
    }
    #[test]
    fn mono14_and_alpha_padded_rgb_convert() {
        let mono14 = f(0x0110_0025, le16(&[0x3fff, 0x0040]));
        assert_eq!(rgb(&mono14).unwrap(), [255, 255, 255, 1, 1, 1]);
        assert!(exposure(&mono14).unwrap().channels.is_none());
        assert_eq!(encode(&mono14, "pgm").unwrap()[11..], [255, 1]);
        let mono12p = f(0x010c_0047, vec![0xff, 0x0f, 0x00]);
        assert_eq!(encode(&mono12p, "pgm").unwrap()[11..], [255, 0]);
        let rgba = f(0x0220_0016, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let bgra = f(0x0220_0017, vec![3, 2, 1, 4, 7, 6, 5, 8]);
        for frame in [rgba, bgra] {
            assert_eq!(rgb(&frame).unwrap(), [1, 2, 3, 5, 6, 7]);
            assert!(encode(&frame, "pgm").is_err());
        }
        assert!(rgb(&f(0x0220_0016, vec![0; 7])).is_err());
    }
}
