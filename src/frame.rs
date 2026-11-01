//! Bounded PFNC image conversion and portable output without image codecs from the OS.
use crate::types::{Frame, MONO8, RGB8};
use anyhow::{Context as _, Result, bail, ensure};
use std::{borrow::Cow, io::Write};

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
        _ => "Unknown",
    }
}
/// Bytes in one row of a `width`-pixel image: rows of packed formats are
/// padded to a whole byte.
pub fn row_bytes(width: u32, pixel_format: u32) -> usize {
    (width as usize * ((pixel_format >> 16) & 0xff) as usize).div_ceil(8)
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
        v => packing(v).map_or(v, |(unpacked, ..)| unpacked),
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
    let frame = unpacked(frame)?;
    let Some((pixel_format, shift)) = wide_bayer(frame.pixel_format) else {
        return Ok(frame);
    };
    let pixels = checked_pixels(&frame)?;
    ensure!(
        frame.data.len() >= pixels * 2,
        "truncated {} frame",
        pixel_format_name(frame.pixel_format)
    );
    let source = frame.data[..pixels * 2].as_chunks::<2>().0;
    let data = source
        .iter()
        .map(|&p| (u16::from_le_bytes(p) >> shift).min(255) as u8)
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
/// The significant bits of each sample of `pixel_format`, from its PFNC
/// name: 12 for Mono12, Mono12p and BayerRG12, 8 for RGB8. None for a
/// format this does not know.
pub fn sample_bits(pixel_format: u32) -> Option<u32> {
    name_bits(pixel_format_name(pixel_format))
}
/// The bits per sample a PFNC pixel format name ends with, before any
/// packing suffix: 12 for Mono12p, 16 for Coord3D_C16.
pub(crate) fn name_bits(name: &str) -> Option<u32> {
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
/// 16-bit PNG samples with the significant bits at the top, and how many
/// bits are significant; None for any other format.
fn gray16(frame: &Frame) -> Result<Option<(Vec<u8>, u32)>> {
    let Some(bits) = wide_gray_bits(unpacked_format(frame.pixel_format)) else {
        return Ok(None);
    };
    let pixels = checked_pixels(frame)?;
    let frame = &*unpacked(frame)?;
    ensure!(
        frame.data.len() >= pixels * 2,
        "truncated {} frame",
        pixel_format_name(frame.pixel_format)
    );
    let mask = u16::MAX >> (16 - bits);
    let samples = frame.data[..pixels * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|sample| ((u16::from_le_bytes(*sample) & mask) << (16 - bits)).to_be_bytes())
        .collect();
    Ok(Some((samples, bits)))
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
/// repeated conversions of one frame size then allocate nothing.
pub fn convert_into<P: Copy + Default + Send>(
    frame: &Frame,
    out: &mut Vec<P>,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    out.clear();
    convert_to(frame, out, pixel)
}
/// Where `convert_to` writes: an empty growable buffer.
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
fn convert_to<'a, P: Copy + Default + Send + 'a>(
    frame: &Frame,
    out: impl Sink<'a, P>,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
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
            // Rows are independent (each reads three input rows), so large
            // frames are split into row blocks across cores.
            let threads = worker_threads(pixels);
            let block = h.div_ceil(threads).max(1) * w;
            if threads > 1 {
                std::thread::scope(|scope| {
                    for (i, rows) in out.chunks_mut(block).enumerate() {
                        let demosaic_row = &demosaic_row;
                        scope.spawn(move || {
                            for (j, line) in rows.chunks_exact_mut(w).enumerate() {
                                demosaic_row(i * block / w + j, line);
                            }
                        });
                    }
                });
            } else {
                for (y, line) in out.chunks_exact_mut(w).enumerate() {
                    demosaic_row(y, line);
                }
            }
        }
        v => {
            bail!("unsupported PFNC format 0x{v:08x}; capture with --format raw to preserve bytes")
        }
    }
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
    data: &'a [u8],
    width: usize,
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
        let bayer8 = matches!(frame.pixel_format, 0x0108_0008..=0x0108_000b);
        (bayer8 && width >= 2 && height >= 2).then(|| Self {
            data: &frame.data,
            width,
            order: quad_order(bayer_pattern(frame.pixel_format)),
            columns: width / 2,
            rows: height / 2,
        })
    }
    fn color(&self, x: usize, y: usize) -> [u8; 3] {
        let (i, w, data) = (2 * y * self.width + 2 * x, self.width, self.data);
        let quad = [data[i], data[i + 1], data[i + w], data[i + w + 1]];
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
    let (w, h, data) = (frame.width as usize, frame.height as usize, &frame.data);
    let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
    std::array::from_fn(|c| {
        if color(x, y) == c {
            return data[y * w + x];
        }
        let (mut sum, mut n) = (0u32, 0u32);
        for yy in y.saturating_sub(1)..(y + 2).min(h) {
            for xx in x.saturating_sub(1)..(x + 2).min(w) {
                if color(xx, yy) == c {
                    sum += data[yy * w + xx] as u32;
                    n += 1;
                }
            }
        }
        sum.checked_div(n).map_or(data[y * w + x], |v| v as u8)
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
    let i = y * frame.width as usize + x;
    let data = &frame.data;
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => [data[i]; 3],
        RGB8 => [data[3 * i], data[3 * i + 1], data[3 * i + 2]],
        0x0218_0015 => [data[3 * i + 2], data[3 * i + 1], data[3 * i]],
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

/// How a frame is exposed, from at most 65,536 evenly strided pixels (whole
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
    let frame = &*eight_bit(frame)?;
    validated_bytes_per_pixel(frame)?;
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
            for i in (0..count).step_by((count / 65_536).max(1)) {
                add(quads.color(i % quads.columns, i / quads.columns));
            }
        }
        None => {
            for i in (0..pixels).step_by((pixels / 65_536).max(1)) {
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
/// clip at 1023, a Mono8 or RGB8 one at 255, each Bayer sample on its own.
pub fn clipping(frame: &Frame) -> Result<Clipping> {
    let pixels = checked_pixels(frame)?;
    let frame = &*unpacked(frame)?;
    let format = frame.pixel_format;
    let wide = wide_gray_bits(format).or_else(|| wide_bayer(format).map(|(_, shift)| 8 + shift));
    let (channels, bits) = match format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 | 0x0108_0008..=0x0108_000b => (1, 8),
        RGB8 | 0x0218_0015 => (3, 8),
        _ => match wide {
            Some(bits) => (1, bits),
            None => bail!("cannot count clipping in {}", pixel_format_name(format)),
        },
    };
    let full = (1u32 << bits) - 1;
    let (mut high, mut low, mut top) = (0u64, 0u64, 0u32);
    let truncated = || format!("truncated {} frame", pixel_format_name(format));
    if bits > 8 {
        ensure!(frame.data.len() >= pixels * 2, truncated());
        for sample in frame.data[..pixels * 2].as_chunks::<2>().0 {
            // Bits above the format's are not part of the sample.
            let sample = u32::from(u16::from_le_bytes(*sample)) & full;
            top = top.max(sample);
            high += u64::from(sample == full);
            low += u64::from(sample == 0);
        }
    } else if channels == 1 {
        ensure!(frame.data.len() >= pixels, truncated());
        for &sample in &frame.data[..pixels] {
            top = top.max(u32::from(sample));
            high += u64::from(sample == 255);
            low += u64::from(sample == 0);
        }
    } else {
        ensure!(frame.data.len() >= pixels * 3, truncated());
        for pixel in frame.data[..pixels * 3].as_chunks::<3>().0 {
            let brightest = pixel[0].max(pixel[1]).max(pixel[2]);
            top = top.max(u32::from(brightest));
            high += u64::from(brightest == 255);
            low += u64::from(brightest == 0);
        }
    }
    Ok(Clipping {
        high_px: high,
        low_px: low,
        pixels: pixels as u64,
        max: f64::from(top) / f64::from(full),
    })
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
    let frame = &*eight_bit(frame)?;
    validated_bytes_per_pixel(frame)?;
    checked_pixels(frame)?;
    if let Some(quads) = Quads::of(frame).filter(|q| q.columns >= 3 && q.rows >= 3) {
        let [x, y, w, h] = region;
        let region = [
            x / 2,
            y / 2,
            (x % 2 + w).div_ceil(2),
            (y % 2 + h).div_ceil(2),
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
/// The most bytes of provenance JSON a saved still carries. Context (16
/// entries of at most 256 bytes) and the fixed allowlist of settings keep it
/// well under this.
pub const MAX_META: usize = 16 * 1024;

pub fn encode(frame: &Frame, format: &str) -> Result<Vec<u8>> {
    encode_with_meta(frame, format, None)
}

/// Encode a still, writing `meta` (compact provenance JSON) into PNG as an
/// iTXt chunk named `capturefab` and into JPEG as a COM segment. Raw, PPM and
/// PGM have no place for it and stay plain. Monochrome PNG of more than 8
/// bits is 16-bit gray with the samples at the top bits and an sBIT chunk
/// saying how many are significant; other monochrome PNG and PGM are one
/// 8-bit gray channel; color and Bayer are 8-bit RGB. PNG is compressed
/// fast: deflate would otherwise take most of a large frame's capture time.
pub fn encode_with_meta(
    frame: &Frame,
    format: &str,
    meta: Option<&serde_json::Value>,
) -> Result<Vec<u8>> {
    let json = meta.map(serde_json::to_string).transpose()?;
    if let Some(json) = &json {
        ensure!(
            json.len() <= MAX_META,
            "capture metadata exceeds {MAX_META} bytes"
        );
    }
    if format == "raw" {
        return Ok(frame.data.clone());
    }
    if format == "jpeg" {
        let bytes = crate::jpeg::encode(frame)?;
        return match &json {
            Some(json) => crate::jpeg::insert_com(&bytes, json.as_bytes()),
            None => Ok(bytes),
        };
    }
    let mut bytes = Vec::new();
    match format {
        "png" => {
            let (color, depth, samples, significant) = match gray16(frame)? {
                Some((samples, bits)) => (
                    png::ColorType::Grayscale,
                    png::BitDepth::Sixteen,
                    samples,
                    Some(bits),
                ),
                None => match gray8(frame)? {
                    Some(gray) => (png::ColorType::Grayscale, png::BitDepth::Eight, gray, None),
                    None => (png::ColorType::Rgb, png::BitDepth::Eight, rgb(frame)?, None),
                },
            };
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
        }
        "ppm" => {
            write!(bytes, "P6\n{} {}\n255\n", frame.width, frame.height)?;
            bytes.extend_from_slice(&rgb(frame)?);
        }
        "pgm" => {
            let gray = gray8(frame)?.context("PGM requires a monochrome image; use PNG or PPM")?;
            write!(bytes, "P5\n{} {}\n255\n", frame.width, frame.height)?;
            bytes.extend_from_slice(&gray);
        }
        _ => bail!("unknown image format {format}; use png, jpeg, raw, pgm or ppm"),
    }
    Ok(bytes)
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
                frame.data = vec![255; width * height];
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
                data: rgb(&mosaic).unwrap(),
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
                        data: rgb(&soft).unwrap(),
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
                        assert_eq!(pixel_at(&frame, x, y), full[i..i + 3], "{format:x} {x},{y}");
                    }
                }
                // Bayer exposure reads whole quads; see quad_scopes_agree_with_the_demosaic.
                if is_bayer(format) {
                    continue;
                }
                let mut bins = [0u32; 64];
                let pixels = full.as_chunks::<3>().0;
                for p in pixels.iter().step_by((pixels.len() / 65_536).max(1)) {
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
            rows.data.truncate(8);
            assert_eq!(rgb(&rows).unwrap(), expected[..12], "{name}");
            rows.data.pop();
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
