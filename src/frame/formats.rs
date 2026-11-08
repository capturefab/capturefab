//! Additional PFNC component layouts. Codes and component order follow the
//! EMVA Pixel Format Values (2025-10-07) and PFNC 2.4, sections 3, 6 and 8:
//! <https://www.emva.org/wp-content/uploads/GenICamPixelFormatValues.pdf>
//! <https://www.emva.org/wp-content/uploads/GenICam_PFNC_2_4.pdf>
//!
//! Video decoded by `FFmpeg` also arrives in planar and semiplanar `Y'CbCr`
//! that PFNC has no code for (every planar layout, limited-range
//! semiplanar 4:2:0, and full-range BT.709). Those carry Capturefab's own
//! codes, with PFNC's custom bit (31) set and its color flag and bits per
//! pixel, named in PFNC's style: `YCbCr` is full-range BT.601 (JFIF),
//! `YCbCr601` and `YCbCr709` limited-range BT.601 and BT.709, and
//! `YCbCr709Full` full-range BT.709.
#![forbid(unsafe_code)]
#![deny(clippy::missing_const_for_fn)]
#![deny(clippy::cast_precision_loss)]
use crate::types::{Frame, MONO8, RGB8};
use anyhow::{Result, ensure};

/// A `Y'CbCr` layout's color space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Matrix {
    /// Full-range BT.601, as JFIF and PFNC's YUV and YCbCr formats.
    Full,
    /// Limited-range (video) BT.601.
    Bt601,
    /// Limited-range (video) BT.709.
    Bt709,
    /// Full-range BT.709, as many network cameras tag their video.
    Full709,
}

#[derive(Clone, Copy)]
enum Layout {
    Mono {
        bits: u32,
    },
    SignedMono,
    Rgb {
        bits: u32,
        channels: usize,
        bgr: bool,
        planar: bool,
        packed: bool,
        aligned: bool,
    },
    Rgb565 {
        bgr: bool,
    },
    Yuv {
        matrix: Matrix,
        /// Pixels per chroma pair: 1 (4:4:4), 2 (4:2:2), 4 (4:1:1).
        group: usize,
        /// Explicit Cb-first order rather than the PFNC default order.
        cb_first: bool,
    },
    /// A luma plane, then the chroma subsampled by `sub` (horizontal and
    /// vertical divisors, 1 or 2): a Cb then a Cr plane, or one plane of
    /// interleaved pairs (`interleaved`: whether Cb comes first).
    Planes {
        matrix: Matrix,
        sub: (usize, usize),
        interleaved: Option<bool>,
    },
}

/// Capturefab's code for planar `Y'CbCr` subsampled `sub` in `matrix`'s
/// color space, PFNC's custom bit set.
const fn planar(sub: (usize, usize), matrix: Matrix) -> u32 {
    let bits = match sub {
        (2, 2) => 12,
        (2, 1) => 16,
        _ => 24,
    };
    0x8200_0000 | (bits << 16) | (1 + matrix as u32)
}
/// Semiplanar 4:2:0 (NV12 order) in limited-range BT.601 and BT.709 and
/// full-range BT.709: `0x820c_0010` plus `planar`'s matrix number.
pub(crate) const NV12_601: u32 = 0x820c_0011;
pub(crate) const NV12_709: u32 = 0x820c_0012;
const NV12_709_FULL: u32 = 0x820c_0013;

fn layout(format: u32) -> Option<(&'static str, Layout)> {
    use Layout::{Mono, Planes, Rgb, Rgb565, SignedMono, Yuv};
    use Matrix::{Bt601, Bt709, Full, Full709};
    let rgb = |bits, bgr, planar| Rgb {
        bits,
        channels: 3,
        bgr,
        planar,
        packed: false,
        aligned: false,
    };
    let rgba = |bits, bgr| Rgb {
        bits,
        channels: 4,
        bgr,
        planar: false,
        packed: false,
        aligned: false,
    };
    let packed = |bits, channels, bgr, aligned| Rgb {
        bits,
        channels,
        bgr,
        planar: false,
        packed: true,
        aligned,
    };
    let yuv = |matrix, group, cb_first| Yuv {
        matrix,
        group,
        cb_first,
    };
    let semi = |matrix, sub, cb_first| Planes {
        matrix,
        sub,
        interleaved: Some(cb_first),
    };
    let planes = |matrix, sub| Planes {
        matrix,
        sub,
        interleaved: None,
    };
    const P420: u32 = planar((2, 2), Full);
    const P420_601: u32 = planar((2, 2), Bt601);
    const P420_709: u32 = planar((2, 2), Bt709);
    const P422: u32 = planar((2, 1), Full);
    const P422_601: u32 = planar((2, 1), Bt601);
    const P422_709: u32 = planar((2, 1), Bt709);
    const P444: u32 = planar((1, 1), Full);
    const P444_601: u32 = planar((1, 1), Bt601);
    const P444_709: u32 = planar((1, 1), Bt709);
    const P420_709_FULL: u32 = planar((2, 2), Full709);
    const P422_709_FULL: u32 = planar((2, 1), Full709);
    const P444_709_FULL: u32 = planar((1, 1), Full709);
    Some(match format {
        0x0108_0002 => ("Mono8s", SignedMono),
        0x0101_0037 => ("Mono1p", Mono { bits: 1 }),
        0x0102_0038 => ("Mono2p", Mono { bits: 2 }),
        0x0104_0039 => ("Mono4p", Mono { bits: 4 }),
        0x0230_0018 => ("RGB10", rgb(10, false, false)),
        0x0230_0019 => ("BGR10", rgb(10, true, false)),
        0x0230_001a => ("RGB12", rgb(12, false, false)),
        0x0230_001b => ("BGR12", rgb(12, true, false)),
        0x0218_0021 => ("RGB8_Planar", rgb(8, false, true)),
        0x0230_0022 => ("RGB10_Planar", rgb(10, false, true)),
        0x0230_0023 => ("RGB12_Planar", rgb(12, false, true)),
        0x0230_0024 => ("RGB16_Planar", rgb(16, false, true)),
        0x0230_0033 => ("RGB16", rgb(16, false, false)),
        0x0210_0035 => ("RGB565p", Rgb565 { bgr: false }),
        0x0210_0036 => ("BGR565p", Rgb565 { bgr: true }),
        0x021e_0048 => ("BGR10p", packed(10, 3, true, false)),
        0x0224_0049 => ("BGR12p", packed(12, 3, true, false)),
        0x0230_004a => ("BGR14", rgb(14, true, false)),
        0x0230_004b => ("BGR16", rgb(16, true, false)),
        0x0240_004c => ("BGRa10", rgba(10, true)),
        0x0228_004d => ("BGRa10p", packed(10, 4, true, false)),
        0x0240_004e => ("BGRa12", rgba(12, true)),
        0x0230_004f => ("BGRa12p", packed(12, 4, true, false)),
        0x0240_0050 => ("BGRa14", rgba(14, true)),
        0x0240_0051 => ("BGRa16", rgba(16, true)),
        0x021e_005c => ("RGB10p", packed(10, 3, false, false)),
        0x0224_005d => ("RGB12p", packed(12, 3, false, false)),
        0x0230_005e => ("RGB14", rgb(14, false, false)),
        0x0240_005f => ("RGBa10", rgba(10, false)),
        0x0228_0060 => ("RGBa10p", packed(10, 4, false, false)),
        0x0240_0061 => ("RGBa12", rgba(12, false)),
        0x0230_0062 => ("RGBa12p", packed(12, 4, false, false)),
        0x0240_0063 => ("RGBa14", rgba(14, false)),
        0x0240_0064 => ("RGBa16", rgba(16, false)),
        0x0220_001d => ("RGB10p32", packed(10, 3, false, true)),
        0x0220_012d => ("RGB8a32", rgba(8, false)),
        0x0220_012e => ("BGR8a32", rgba(8, true)),
        0x020c_001e => ("YUV411_8_UYYVYY", yuv(Full, 4, true)),
        0x0218_0020 => ("YUV8_UYV", yuv(Full, 1, true)),
        0x0218_003a => ("YCbCr8_CbYCr", yuv(Full, 1, true)),
        0x020c_003c => ("YCbCr411_8_CbYYCrYY", yuv(Full, 4, true)),
        0x0218_003d => ("YCbCr601_8_CbYCr", yuv(Bt601, 1, true)),
        0x0210_003e => ("YCbCr601_422_8", yuv(Bt601, 2, false)),
        0x020c_003f => ("YCbCr601_411_8_CbYYCrYY", yuv(Bt601, 4, true)),
        0x0218_0040 => ("YCbCr709_8_CbYCr", yuv(Bt709, 1, true)),
        0x0210_0041 => ("YCbCr709_422_8", yuv(Bt709, 2, false)),
        0x020c_0042 => ("YCbCr709_411_8_CbYYCrYY", yuv(Bt709, 4, true)),
        0x0210_0044 => ("YCbCr601_422_8_CbYCrY", yuv(Bt601, 2, true)),
        0x0210_0045 => ("YCbCr709_422_8_CbYCrY", yuv(Bt709, 2, true)),
        0x020c_005a => ("YCbCr411_8", yuv(Full, 4, false)),
        0x0218_005b => ("YCbCr8", yuv(Full, 1, false)),
        0x020c_0112 => ("YCbCr420_8_YY_CbCr_Semiplanar", semi(Full, (2, 2), true)),
        0x020c_0114 => ("YCbCr420_8_YY_CrCb_Semiplanar", semi(Full, (2, 2), false)),
        0x0210_0113 => ("YCbCr422_8_YY_CbCr_Semiplanar", semi(Full, (2, 1), true)),
        0x0210_0115 => ("YCbCr422_8_YY_CrCb_Semiplanar", semi(Full, (2, 1), false)),
        NV12_601 => (
            "YCbCr601_420_8_YY_CbCr_Semiplanar",
            semi(Bt601, (2, 2), true),
        ),
        NV12_709 => (
            "YCbCr709_420_8_YY_CbCr_Semiplanar",
            semi(Bt709, (2, 2), true),
        ),
        NV12_709_FULL => (
            "YCbCr709Full_420_8_YY_CbCr_Semiplanar",
            semi(Full709, (2, 2), true),
        ),
        P420 => ("YCbCr420_8_Planar", planes(Full, (2, 2))),
        P420_601 => ("YCbCr601_420_8_Planar", planes(Bt601, (2, 2))),
        P420_709 => ("YCbCr709_420_8_Planar", planes(Bt709, (2, 2))),
        P422 => ("YCbCr422_8_Planar", planes(Full, (2, 1))),
        P422_601 => ("YCbCr601_422_8_Planar", planes(Bt601, (2, 1))),
        P422_709 => ("YCbCr709_422_8_Planar", planes(Bt709, (2, 1))),
        P444 => ("YCbCr444_8_Planar", planes(Full, (1, 1))),
        P444_601 => ("YCbCr601_444_8_Planar", planes(Bt601, (1, 1))),
        P444_709 => ("YCbCr709_444_8_Planar", planes(Bt709, (1, 1))),
        P420_709_FULL => ("YCbCr709Full_420_8_Planar", planes(Full709, (2, 2))),
        P422_709_FULL => ("YCbCr709Full_422_8_Planar", planes(Full709, (2, 1))),
        P444_709_FULL => ("YCbCr709Full_444_8_Planar", planes(Full709, (1, 1))),
        _ => return None,
    })
}

/// Every code `layout` knows, for looking a layout up by its name.
const CODES: [u32; 70] = [
    0x0108_0002,
    0x0101_0037,
    0x0102_0038,
    0x0104_0039,
    0x0230_0018,
    0x0230_0019,
    0x0230_001a,
    0x0230_001b,
    0x0218_0021,
    0x0230_0022,
    0x0230_0023,
    0x0230_0024,
    0x0230_0033,
    0x0210_0035,
    0x0210_0036,
    0x021e_0048,
    0x0224_0049,
    0x0230_004a,
    0x0230_004b,
    0x0240_004c,
    0x0228_004d,
    0x0240_004e,
    0x0230_004f,
    0x0240_0050,
    0x0240_0051,
    0x021e_005c,
    0x0224_005d,
    0x0230_005e,
    0x0240_005f,
    0x0228_0060,
    0x0240_0061,
    0x0230_0062,
    0x0240_0063,
    0x0240_0064,
    0x0220_001d,
    0x0220_012d,
    0x0220_012e,
    0x020c_001e,
    0x0218_0020,
    0x0218_003a,
    0x020c_003c,
    0x0218_003d,
    0x0210_003e,
    0x020c_003f,
    0x0218_0040,
    0x0210_0041,
    0x020c_0042,
    0x0210_0044,
    0x0210_0045,
    0x020c_005a,
    0x0218_005b,
    0x020c_0112,
    0x020c_0114,
    0x0210_0113,
    0x0210_0115,
    NV12_601,
    NV12_709,
    NV12_709_FULL,
    planar((2, 2), Matrix::Full),
    planar((2, 2), Matrix::Bt601),
    planar((2, 2), Matrix::Bt709),
    planar((2, 1), Matrix::Full),
    planar((2, 1), Matrix::Bt601),
    planar((2, 1), Matrix::Bt709),
    planar((1, 1), Matrix::Full),
    planar((1, 1), Matrix::Bt601),
    planar((1, 1), Matrix::Bt709),
    planar((2, 2), Matrix::Full709),
    planar((2, 1), Matrix::Full709),
    planar((1, 1), Matrix::Full709),
];

/// The code of planar or semiplanar 8-bit `Y'CbCr` subsampled `sub` in
/// `matrix`'s color space, chroma interleaved Cb first (NV12) or in planes:
/// `FFmpeg`'s decoded layouts. None for the other semiplanar 4:2:2 and 4:4:4
/// layouts, which nothing produces.
pub(crate) const fn ycbcr_planes(
    sub: (usize, usize),
    matrix: Matrix,
    interleaved: bool,
) -> Option<u32> {
    Some(match (interleaved, sub, matrix) {
        (false, _, _) => planar(sub, matrix),
        (true, (2, 2), Matrix::Full) => 0x020c_0112,
        (true, (2, 2), Matrix::Bt601) => NV12_601,
        (true, (2, 2), Matrix::Bt709) => NV12_709,
        (true, (2, 2), Matrix::Full709) => NV12_709_FULL,
        (true, (2, 1), Matrix::Full) => 0x0210_0113,
        _ => return None,
    })
}

/// The bytes of a `width` × `height` image in a planar layout, None for
/// layouts whose rows are all alike (`row_bytes` times the rows).
pub(super) fn planes_len(format: u32, width: u32, height: u32) -> Option<usize> {
    let (_, Layout::Planes { sub, .. }) = layout(format)? else {
        return None;
    };
    let (width, height) = (width as usize, height as usize);
    let chroma = width.div_ceil(sub.0).checked_mul(height.div_ceil(sub.1))?;
    width
        .checked_mul(height)?
        .checked_add(chroma.checked_mul(2)?)
}

/// The row bytes and rows of each plane of a planar or semiplanar layout,
/// in the order they follow each other; None for one plane of rows.
pub(super) fn planes(format: u32, width: u32, height: u32) -> Option<Vec<(usize, usize)>> {
    let (width, height) = (width as usize, height as usize);
    match layout(format)?.1 {
        Layout::Planes {
            sub, interleaved, ..
        } => {
            let (row, rows) = (width.div_ceil(sub.0), height.div_ceil(sub.1));
            Some(match interleaved {
                Some(_) => vec![(width, height), (row.checked_mul(2)?, rows)],
                None => vec![(width, height), (row, rows), (row, rows)],
            })
        }
        Layout::Rgb {
            planar: true,
            bits,
            channels,
            ..
        } => {
            let row = width.checked_mul(if bits == 8 { 1 } else { 2 })?;
            Some(vec![(row, height); channels])
        }
        _ => None,
    }
}

pub(super) fn name(format: u32) -> Option<&'static str> {
    layout(format).map(|(name, _)| name)
}

/// The significant bits of each component: what the name's digits do not
/// always say (`RGB10p32` is 10, `RGB8_Planar` 8). `RGB565p` counts its widest,
/// six-bit, component; YUV and YCbCr are 8.
pub(super) fn sample_bits(format: u32) -> Option<u32> {
    Some(match layout(format)?.1 {
        Layout::Mono { bits } | Layout::Rgb { bits, .. } => bits,
        Layout::SignedMono | Layout::Yuv { .. } | Layout::Planes { .. } => 8,
        Layout::Rgb565 { .. } => 6,
    })
}

/// The component depth of an RGB layout `native_color` reads, else None.
pub(super) fn color_bits(format: u32) -> Option<u32> {
    match layout(format)?.1 {
        Layout::Rgb { bits, .. } => Some(bits),
        _ => None,
    }
}

/// `sample_bits` of the layout named `name`, such as a camera reports it.
pub(super) fn name_bits(name: &str) -> Option<u32> {
    CODES
        .into_iter()
        .find(|&code| self::name(code) == Some(name))
        .and_then(sample_bits)
}

pub(super) fn monochrome(format: u32) -> bool {
    matches!(
        layout(format),
        Some((_, Layout::Mono { .. } | Layout::SignedMono))
    )
}

pub(super) fn unpacked_format(format: u32) -> Option<u32> {
    layout(format).map(|_| if monochrome(format) { MONO8 } else { RGB8 })
}

/// Validate bytes and the grouping constraints before reading any sample.
/// Dimensions are bounded by the caller's `checked_pixels` validation.
pub(super) fn validate(frame: &Frame) -> Option<Result<()>> {
    let (name, layout) = layout(frame.pixel_format)?;
    Some((|| {
        if let Layout::Yuv { group: 4, .. } = layout {
            ensure!(
                frame.width.is_multiple_of(4),
                "{name} requires a width divisible by 4"
            );
        }
        let length = planes_len(frame.pixel_format, frame.width, frame.height)
            .or_else(|| {
                super::row_bytes(frame.width, frame.pixel_format).checked_mul(frame.height as usize)
            })
            .ok_or_else(|| anyhow::anyhow!("image size overflow"))?;
        ensure!(frame.data.len() >= length, "truncated {name} frame");
        Ok(())
    })())
}

/// A sample from an LSB-packed stream. The enclosing row has been validated;
/// only its final unused bits may lie beyond the row's last byte.
fn packed_sample(row: &[u8], bit: usize, bits: u32) -> u16 {
    let at = bit / 8;
    let mut value = 0u32;
    for (i, &byte) in row[at..].iter().take(3).enumerate() {
        value |= u32::from(byte) << (8 * i);
    }
    u16::try_from((value >> (bit & 7)) & ((1 << bits) - 1))
        .expect("packed samples have at most sixteen bits")
}

/// Native RGB components and their bit depth, after `validate` succeeds.
/// Alpha/alignment components are omitted and BGR data is reordered.
pub(super) fn native_color(frame: &Frame, x: usize, y: usize) -> Option<([u16; 3], u32)> {
    Decoder::new(frame)?.native_color(x, y)
}

/// A frame of a layout this module knows, looked up once to decode many of
/// its pixels: a measurement reads hundreds of thousands, and the lookup
/// costs about as much as decoding one. Pixels are read after `validate`
/// succeeds.
#[derive(Clone, Copy)]
pub(super) struct Decoder<'a> {
    /// The frame's bytes and size, looked up once: each read through the
    /// frame's buffer would go through its shared, pooled storage again.
    data: &'a [u8],
    width: usize,
    height: usize,
    layout: Layout,
    /// The bytes of a row of a layout of one plane.
    stride: usize,
    /// Where a planar layout keeps each pixel's chroma.
    chroma: ChromaAt,
}

/// Where the Cb and Cr covering a pixel lie in a planar layout.
#[derive(Clone, Copy, Default)]
struct ChromaAt {
    /// The subsampling as right shifts of x and y: 1 halves, 0 keeps.
    shift: (u32, u32),
    /// Chroma samples in a row.
    columns: usize,
    /// The first Cb and the first Cr, and the bytes from one to the next.
    cb: usize,
    cr: usize,
    step: usize,
}

impl ChromaAt {
    /// Saturating, so that a frame too large to validate makes a decoder
    /// that `validate` then refuses.
    fn new(frame: &Frame, sub: (usize, usize), interleaved: Option<bool>) -> Self {
        debug_assert!(sub.0.is_power_of_two() && sub.1.is_power_of_two());
        let (width, height) = (frame.width as usize, frame.height as usize);
        let (columns, rows) = (width.div_ceil(sub.0), height.div_ceil(sub.1));
        let luma = width.saturating_mul(height);
        let (cb, cr, step) = match interleaved {
            None => (luma, luma.saturating_add(columns.saturating_mul(rows)), 1),
            Some(true) => (luma, luma.saturating_add(1), 2),
            Some(false) => (luma.saturating_add(1), luma, 2),
        };
        Self {
            shift: (sub.0.trailing_zeros(), sub.1.trailing_zeros()),
            columns,
            cb,
            cr,
            step,
        }
    }

    /// The Cb and Cr of pixel (x, y), whose chroma sample covers its block.
    #[inline(always)]
    const fn at(&self, data: &[u8], x: usize, y: usize) -> (u8, u8) {
        let i = ((y >> self.shift.1) * self.columns + (x >> self.shift.0)) * self.step;
        (data[self.cb + i], data[self.cr + i])
    }
}

impl<'a> Decoder<'a> {
    /// None for a format this module does not know.
    pub(super) fn new(frame: &'a Frame) -> Option<Self> {
        let (_, layout) = layout(frame.pixel_format)?;
        let chroma = match layout {
            Layout::Planes {
                sub, interleaved, ..
            } => ChromaAt::new(frame, sub, interleaved),
            _ => ChromaAt::default(),
        };
        Some(Self {
            data: &frame.data,
            width: frame.width as usize,
            height: frame.height as usize,
            layout,
            stride: super::row_bytes(frame.width, frame.pixel_format),
            chroma,
        })
    }

    /// Row `y` of a layout of one plane.
    #[inline(always)]
    fn row(&self, y: usize) -> &'a [u8] {
        &self.data[y * self.stride..(y + 1) * self.stride]
    }

    /// Native RGB components of pixel (x, y) and their bit depth, for an
    /// RGB layout; None for the others.
    pub(super) fn native_color(&self, x: usize, y: usize) -> Option<([u16; 3], u32)> {
        let Layout::Rgb {
            bits,
            channels,
            bgr,
            planar,
            packed,
            aligned,
        } = self.layout
        else {
            return None;
        };
        let (width, height, data) = (self.width, self.height, self.data);
        let i = y * width + x;
        let sample = |channel| {
            if packed {
                let pixel_bits = if aligned {
                    32
                } else {
                    channels * bits as usize
                };
                packed_sample(self.row(y), x * pixel_bits + channel * bits as usize, bits)
            } else {
                let at = if planar {
                    channel * width * height + i
                } else {
                    channels * i + channel
                };
                if bits == 8 {
                    u16::from(data[at])
                } else {
                    u16::from_le_bytes([data[2 * at], data[2 * at + 1]])
                }
            }
        };
        let rgb = [sample(0), sample(1), sample(2)];
        Some((if bgr { [rgb[2], rgb[1], rgb[0]] } else { rgb }, bits))
    }

    /// Pixel (x, y) as 8-bit R'G'B'. `Y'CbCr`, which decoded video arrives
    /// in, is read here; other layouts in `other_rgb`, so that this stays
    /// small enough to inline into a caller's sampling loop.
    #[inline(always)]
    pub(super) fn rgb(&self, x: usize, y: usize) -> [u8; 3] {
        match self.layout {
            Layout::Yuv { matrix, .. } | Layout::Planes { matrix, .. } => {
                let [luma, cb, cr] = self.ycbcr(x, y);
                to_rgb(matrix, luma, cb, cr)
            }
            _ => self.other_rgb(x, y),
        }
    }

    /// The brightest of pixel (x, y)'s components as `rgb` gives them,
    /// without working out the other two of a `Y'CbCr` layout.
    #[inline(always)]
    pub(super) fn brightest(&self, x: usize, y: usize) -> u8 {
        match self.layout {
            Layout::Yuv { matrix, .. } | Layout::Planes { matrix, .. } => {
                let [luma, cb, cr] = self.ycbcr(x, y);
                brightest(matrix, luma, cb, cr)
            }
            _ => {
                let [r, g, b] = self.other_rgb(x, y);
                r.max(g).max(b)
            }
        }
    }

    /// `brightest` of each pixel of row `y` from column `x0`, into `out`.
    pub(super) fn brightest_row(&self, x0: usize, y: usize, out: &mut [u8]) {
        match self.layout {
            // A loop for each color space, which none then decides per pixel.
            Layout::Yuv { matrix, .. } | Layout::Planes { matrix, .. } => match matrix {
                Matrix::Full => self.ycbcr_peaks(Matrix::Full, x0, y, out),
                Matrix::Bt601 => self.ycbcr_peaks(Matrix::Bt601, x0, y, out),
                Matrix::Bt709 => self.ycbcr_peaks(Matrix::Bt709, x0, y, out),
                Matrix::Full709 => self.ycbcr_peaks(Matrix::Full709, x0, y, out),
            },
            _ => {
                for (x, peak) in (x0..).zip(out) {
                    *peak = self.brightest(x, y);
                }
            }
        }
    }

    /// `brightest_row` of a `Y'CbCr` layout in `matrix`'s color space. The
    /// pixels a planar layout's chroma sample covers share its chroma terms.
    #[inline(always)]
    fn ycbcr_peaks(&self, matrix: Matrix, x0: usize, y: usize, out: &mut [u8]) {
        let Layout::Planes { .. } = self.layout else {
            for (x, peak) in (x0..).zip(out) {
                let [luma, cb, cr] = self.ycbcr(x, y);
                *peak = brightest(matrix, luma, cb, cr);
            }
            return;
        };
        let chroma = self.chroma;
        let luma = &self.data[y * self.width + x0..][..out.len()];
        let row = (y >> chroma.shift.1) * chroma.columns * chroma.step;
        let (cb, cr) = (&self.data[chroma.cb + row..], &self.data[chroma.cr + row..]);
        let (mut held, mut top) = (usize::MAX, 0);
        for (x, (peak, &luma)) in (x0..).zip(out.iter_mut().zip(luma)) {
            let at = (x >> chroma.shift.0) * chroma.step;
            if at != held {
                (held, top) = (at, top_term(matrix, cb[at], cr[at]));
            }
            *peak = component(matrix, luma_term(matrix, luma) + top);
        }
    }

    /// `rgb` of the layouts other than `Y'CbCr`.
    #[inline(never)]
    fn other_rgb(&self, x: usize, y: usize) -> [u8; 3] {
        let data = self.data;
        let i = y * self.width + x;
        match self.layout {
            Layout::Mono { bits } => {
                let value = packed_sample(self.row(y), x * bits as usize, bits);
                [u8::try_from(u32::from(value) * 255 / ((1 << bits) - 1))
                    .expect("scaled sample fits eight bits"); 3]
            }
            Layout::SignedMono => [data[i] ^ 128; 3],
            Layout::Rgb { .. } => {
                let (rgb, bits) = self.native_color(x, y).expect("an RGB layout");
                rgb.map(|v| super::clamp8(v >> bits.saturating_sub(8)))
            }
            Layout::Rgb565 { bgr } => {
                let value = u16::from_le_bytes([data[2 * i], data[2 * i + 1]]);
                let expand = |v: u16, max: u16| {
                    u8::try_from((u32::from(v) * 255 + u32::from(max) / 2) / u32::from(max))
                        .expect("expanded RGB565 component fits eight bits")
                };
                let rgb = [
                    expand(value & 31, 31),
                    expand((value >> 5) & 63, 63),
                    expand(value >> 11, 31),
                ];
                if bgr { [rgb[2], rgb[1], rgb[0]] } else { rgb }
            }
            Layout::Yuv { .. } | Layout::Planes { .. } => unreachable!("Y'CbCr is read by `rgb`"),
        }
    }

    /// The luma, Cb and Cr of pixel (x, y) of a `Y'CbCr` layout: planar and
    /// 4:2:2, what decoded video and webcams send, here; the rest in
    /// `other_ycbcr`.
    #[inline(always)]
    fn ycbcr(&self, x: usize, y: usize) -> [u8; 3] {
        match self.layout {
            Layout::Planes { .. } => {
                let (cb, cr) = self.chroma.at(self.data, x, y);
                [self.data[y * self.width + x], cb, cr]
            }
            Layout::Yuv {
                group: 2, cb_first, ..
            } => {
                let p = &self.row(y)[(x / 2) * 4..];
                let &[first, second] = p.first_chunk::<2>().expect("one packed 4:2:2 pixel");
                // Match the existing 4:2:2 decoder for a lone last pixel.
                if cb_first {
                    [p[1 + 2 * (x & 1)], first, p.get(2).copied().unwrap_or(128)]
                } else {
                    [p[2 * (x & 1)], second, p.get(3).copied().unwrap_or(128)]
                }
            }
            _ => self.other_ycbcr(x, y),
        }
    }

    /// `ycbcr` of 4:4:4 and 4:1:1 packed in rows.
    #[inline(never)]
    fn other_ycbcr(&self, x: usize, y: usize) -> [u8; 3] {
        let Layout::Yuv {
            group, cb_first, ..
        } = self.layout
        else {
            unreachable!("not a Y'CbCr layout")
        };
        let row = self.row(y);
        if group == 1 {
            let p = row[3 * x..]
                .first_chunk::<3>()
                .expect("one packed 4:4:4 pixel");
            return if cb_first {
                [p[1], p[0], p[2]]
            } else {
                [p[0], p[1], p[2]]
            };
        }
        let p = &row[(x / 4) * 6..];
        // Default: Y0 Y1 Cb Y2 Y3 Cr. Explicit: Cb Y0 Y1 Cr Y2 Y3.
        let offsets = if cb_first { [1, 2, 4, 5] } else { [0, 1, 3, 4] };
        [
            p[offsets[x & 3]],
            p[if cb_first { 0 } else { 2 }],
            p[if cb_first { 3 } else { 5 }],
        ]
    }
}

/// A `Y'CbCr` sample as R'G'B'.
#[inline(always)]
pub(super) fn to_rgb(matrix: Matrix, luma: u8, cb: u8, cr: u8) -> [u8; 3] {
    let (luma, chroma) = terms(matrix, luma, cb, cr);
    chroma.map(|term| component(matrix, luma + term))
}

/// The brightest of `to_rgb`'s three components. Each is the same
/// nondecreasing function of the luma term plus its own chroma term, so the
/// brightest is the one with the largest chroma term, and only it is
/// rounded and clamped.
#[inline(always)]
pub(super) fn brightest(matrix: Matrix, luma: u8, cb: u8, cr: u8) -> u8 {
    let (luma, [r, g, b]) = terms(matrix, luma, cb, cr);
    component(matrix, luma + r.max(g).max(b))
}

/// A `Y'CbCr` sample's luma term and the chroma terms of its R', G' and B',
/// each of which `component` turns, added to the luma term, into one.
#[inline(always)]
fn terms(matrix: Matrix, luma: u8, cb: u8, cr: u8) -> (i32, [i32; 3]) {
    let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
    match matrix {
        // Whole numbers: each chroma term rounded on its own, as JFIF
        // decoders compute it.
        Matrix::Full => (
            i32::from(luma),
            [
                (91_881 * cr + 32_768) >> 16,
                (-22_554 * cb - 46_802 * cr + 32_768) >> 16,
                (116_130 * cb + 32_768) >> 16,
            ],
        ),
        // 2(1 - Kr), 2Kb(1 - Kb)/Kg, 2Kr(1 - Kr)/Kg and 2(1 - Kb) for
        // BT.709's Kr 0.2126 and Kb 0.0722, in 16.16 fixed point.
        Matrix::Full709 => (
            i32::from(luma) << 16,
            [103_206 * cr, -12_276 * cb - 30_679 * cr, 121_609 * cb],
        ),
        Matrix::Bt601 | Matrix::Bt709 => {
            let [r, gu, gv, b] = match matrix {
                Matrix::Bt601 => [104_597, -25_675, -53_279, 132_201],
                _ => [117_489, -13_975, -34_925, 138_438],
            };
            (
                (i32::from(luma) - 16) * 76_309,
                [r * cr, gu * cb + gv * cr, b * cb],
            )
        }
    }
}

/// The luma term of `terms` on its own, for a caller that knows `matrix`
/// (the rest then folds away).
#[inline(always)]
fn luma_term(matrix: Matrix, luma: u8) -> i32 {
    terms(matrix, luma, 128, 128).0
}

/// The largest chroma term of `terms`, for a caller that knows `matrix`.
#[inline(always)]
fn top_term(matrix: Matrix, cb: u8, cr: u8) -> i32 {
    let (_, [r, g, b]) = terms(matrix, 0, cb, cr);
    r.max(g).max(b)
}

/// One component from the sum of its luma and chroma terms, rounded from
/// 16.16 fixed point where `terms` keeps it, and clamped to 8 bits.
#[inline(always)]
fn component(matrix: Matrix, sum: i32) -> u8 {
    let rounded = match matrix {
        Matrix::Full => sum,
        _ => (sum + 32_768) >> 16,
    }
    .clamp(0, 255);
    u8::try_from(rounded).expect("clamped component fits eight bits")
}

#[cfg(any(test, feature = "testing"))]
pub(super) fn unpacked(frame: &Frame) -> Result<Option<Frame>> {
    let Some(decoder) = Decoder::new(frame) else {
        return Ok(None);
    };
    let pixels = super::checked_pixels(frame)?;
    validate(frame).expect("known format")?;
    let mono = monochrome(frame.pixel_format);
    let mut data = Vec::with_capacity(pixels * if mono { 1 } else { 3 });
    for y in 0..frame.height as usize {
        for x in 0..frame.width as usize {
            let rgb = decoder.rgb(x, y);
            if mono {
                data.push(rgb[0]);
            } else {
                data.extend_from_slice(&rgb);
            }
        }
    }
    Ok(Some(Frame {
        data: data.into(),
        pixel_format: if mono { MONO8 } else { RGB8 },
        jpeg: None,
        ..*frame
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(format: u32, width: u32, height: u32, data: Vec<u8>) -> Frame {
        Frame {
            width,
            height,
            pixel_format: format,
            data: data.into(),
            ..Frame::default()
        }
    }

    fn decoded(frame: &Frame) -> Vec<u8> {
        unpacked(frame).unwrap().unwrap().data.to_vec()
    }

    #[test]
    fn brightest_is_the_largest_component() {
        // Every chroma pair, with lumas at and around both ends of both
        // ranges and a spread between them.
        let lumas: Vec<u8> = (0..=255)
            .filter(|v| v % 17 == 0 || [1, 15, 16, 17, 234, 235, 236, 254].contains(v))
            .collect();
        for matrix in [Matrix::Full, Matrix::Bt601, Matrix::Bt709, Matrix::Full709] {
            for cb in 0..=255 {
                for cr in 0..=255 {
                    let top = top_term(matrix, cb, cr);
                    for &luma in &lumas {
                        let [r, g, b] = to_rgb(matrix, luma, cb, cr);
                        let expected = r.max(g).max(b);
                        let shared = component(matrix, luma_term(matrix, luma) + top);
                        assert_eq!(
                            [brightest(matrix, luma, cb, cr), shared],
                            [expected; 2],
                            "{matrix:?} {luma} {cb} {cr}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wide_rgb_channel_order_depth_and_planes() {
        let colors = [[1u16, 100, 200], [255, 16, 32], [99, 77, 55], [5, 10, 15]];
        let expected = colors
            .into_iter()
            .flatten()
            .map(|v| u8::try_from(v).unwrap())
            .collect::<Vec<_>>();
        for (format, bits, bgr, planar, alpha) in [
            (0x0230_0018, 10, false, false, false),
            (0x0230_0019, 10, true, false, false),
            (0x0230_001a, 12, false, false, false),
            (0x0230_001b, 12, true, false, false),
            (0x0230_005e, 14, false, false, false),
            (0x0230_004a, 14, true, false, false),
            (0x0230_0033, 16, false, false, false),
            (0x0230_004b, 16, true, false, false),
            (0x0218_0021, 8, false, true, false),
            (0x0230_0022, 10, false, true, false),
            (0x0230_0023, 12, false, true, false),
            (0x0230_0024, 16, false, true, false),
            (0x0240_005f, 10, false, false, true),
            (0x0240_004c, 10, true, false, true),
            (0x0240_0061, 12, false, false, true),
            (0x0240_004e, 12, true, false, true),
            (0x0240_0063, 14, false, false, true),
            (0x0240_0050, 14, true, false, true),
            (0x0240_0064, 16, false, false, true),
            (0x0240_0051, 16, true, false, true),
            (0x0220_012d, 8, false, false, true),
            (0x0220_012e, 8, true, false, true),
        ] {
            let mut samples = Vec::new();
            if planar {
                for channel in 0..3 {
                    samples.extend(colors.iter().map(|c| c[channel]));
                }
            } else {
                for rgb in colors {
                    samples.extend(if bgr { [rgb[2], rgb[1], rgb[0]] } else { rgb });
                    if alpha {
                        samples.push(0);
                    }
                }
            }
            let data = samples
                .into_iter()
                .flat_map(|sample| {
                    let v = sample << (bits - 8);
                    if bits == 8 {
                        vec![u8::try_from(v).unwrap()]
                    } else {
                        v.to_le_bytes().to_vec()
                    }
                })
                .collect();
            let mut source = frame(format, 2, 2, data);
            assert_eq!(decoded(&source), expected, "{}", name(format).unwrap());
            assert_eq!(Decoder::new(&source).unwrap().rgb(1, 1), [5, 10, 15]);
            source.data.make_mut().pop();
            unpacked(&source).unwrap_err();
        }
    }

    /// Independent bit-at-a-time encoder, with each row started on a fresh byte.
    fn pack_rows(
        rows: &[Vec<u16>],
        bits: usize,
        pixel_components: usize,
        aligned: bool,
    ) -> Vec<u8> {
        let mut output = Vec::new();
        for row in rows {
            let pixel_bits = if aligned { 32 } else { bits * pixel_components };
            let mut bytes = vec![0; (row.len() / pixel_components * pixel_bits).div_ceil(8)];
            for (i, value) in row.iter().enumerate() {
                let first = (i / pixel_components) * pixel_bits + (i % pixel_components) * bits;
                for bit in 0..bits {
                    if (value >> bit) & 1 != 0 {
                        bytes[(first + bit) / 8] |= 1 << ((first + bit) % 8);
                    }
                }
            }
            output.extend(bytes);
        }
        output
    }

    #[test]
    fn packed_rgb_samples_cross_bytes_and_restart_each_row() {
        for (format, bits, channels, bgr, aligned) in [
            (0x021e_005c, 10, 3, false, false),
            (0x021e_0048, 10, 3, true, false),
            (0x0224_005d, 12, 3, false, false),
            (0x0224_0049, 12, 3, true, false),
            (0x0228_0060, 10, 4, false, false),
            (0x0228_004d, 10, 4, true, false),
            (0x0230_0062, 12, 4, false, false),
            (0x0230_004f, 12, 4, true, false),
            (0x0220_001d, 10, 3, false, true),
        ] {
            for width in [1, 2, 3, 5] {
                let mut expected = Vec::new();
                let rows = (0..2)
                    .map(|y| {
                        (0..width)
                            .flat_map(|x| {
                                let rgb =
                                    [u16::try_from((x * 97 + y * 31) % 256).unwrap(), 77, 200];
                                expected.extend(rgb.map(|v| u8::try_from(v).unwrap()));
                                let mut samples = if bgr {
                                    vec![rgb[2], rgb[1], rgb[0]]
                                } else {
                                    rgb.to_vec()
                                };
                                if channels == 4 {
                                    samples.push(255);
                                }
                                samples
                                    .into_iter()
                                    .map(|v| (v << (bits - 8)) | 3)
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                let bytes = pack_rows(&rows, bits, channels, aligned);
                let source = frame(format, width, 2, bytes);
                assert_eq!(
                    decoded(&source),
                    expected,
                    "{} width {width}",
                    name(format).unwrap()
                );
            }
        }
        // The upper two bits of RGB10p32 are padding, not blue sample bits.
        assert_eq!(
            decoded(&frame(0x0220_001d, 1, 1, vec![255, 3, 0, 192])),
            [255, 0, 0]
        );
    }

    #[test]
    fn low_bit_mono_uses_full_display_range_and_preserves_row_boundaries() {
        assert_eq!(
            decoded(&frame(0x0101_0037, 5, 2, vec![0b10101, 0b01010])),
            [255, 0, 255, 0, 255, 0, 255, 0, 255, 0]
        );
        assert_eq!(
            decoded(&frame(0x0102_0038, 4, 1, vec![0b1110_0100])),
            [0, 85, 170, 255]
        );
        assert_eq!(
            decoded(&frame(0x0104_0039, 3, 2, vec![0x50, 0x0f, 0xaf, 0x00])),
            [0, 85, 255, 255, 170, 0]
        );
        assert_eq!(
            decoded(&frame(0x0108_0002, 4, 1, vec![128, 255, 0, 127])),
            [0, 127, 128, 255]
        );
        for format in [0x0101_0037, 0x0102_0038, 0x0104_0039, 0x0108_0002] {
            assert!(monochrome(format));
            assert_eq!(unpacked_format(format), Some(MONO8));
        }
    }

    #[test]
    fn rgb565_uses_pfnc_lsb_first_component_order() {
        let rgb = frame(0x0210_0035, 4, 1, vec![31, 0, 224, 7, 0, 248, 255, 255]);
        assert_eq!(
            decoded(&rgb),
            [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]
        );
        let bgr = Frame {
            pixel_format: 0x0210_0036,
            ..rgb
        };
        assert_eq!(
            decoded(&bgr),
            [0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255]
        );
    }

    #[test]
    fn native_rgb_preserves_bits_below_display_precision() {
        let source = frame(0x0230_001b, 1, 1, vec![0xff, 0x0f, 0x01, 0x08, 0x03, 0x00]);
        assert_eq!(native_color(&source, 0, 0), Some(([3, 2049, 4095], 12)));
        assert_eq!(decoded(&source), [0, 128, 255]);
        let packed = frame(0x0224_005d, 1, 1, vec![0x03, 0x10, 0x80, 0xff, 0x0f]);
        assert_eq!(native_color(&packed, 0, 0), Some(([3, 2049, 4095], 12)));
        assert_eq!(decoded(&packed), [0, 128, 255]);
        let source = frame(0x0230_001a, 1, 1, vec![255; 6]);
        assert_eq!(
            decoded(&source),
            [255; 3],
            "out-of-range native bits clamp rather than wrap"
        );
    }

    #[test]
    fn additional_layouts_reject_incomplete_images_and_invalid_dimensions() {
        for format in [
            0x0101_0037,
            0x0102_0038,
            0x0104_0039,
            0x0108_0002,
            0x0210_0035,
            0x0218_0020,
            0x020c_001e,
            0x0210_0044,
            0x0210_0045,
        ] {
            let length = super::super::row_bytes(4, format) * 2;
            let mut source = frame(format, 4, 2, vec![128; length]);
            unpacked(&source).unwrap();
            source.data.make_mut().pop();
            assert!(unpacked(&source).is_err(), "{format:#x}");
            source.width = 0;
            unpacked(&source).unwrap_err();
        }
        assert!(unpacked(&frame(0, 1, 1, vec![0])).unwrap().is_none());
        assert!(validate(&frame(0, 1, 1, vec![0])).is_none());
        assert!(name(0).is_none());
    }

    #[test]
    fn yuv444_and_411_component_order_and_row_boundaries() {
        for (format, data) in [
            (0x0218_0020, vec![85, 76, 255]),
            (0x0218_003a, vec![85, 76, 255]),
            (0x0218_005b, vec![76, 85, 255]),
        ] {
            assert_eq!(decoded(&frame(format, 1, 1, data)), [254, 0, 0]);
        }
        let expected = [10, 20, 30, 40, 50, 60, 70, 80]
            .into_iter()
            .flat_map(|v| [v; 3])
            .collect::<Vec<_>>();
        for (format, data) in [
            (
                0x020c_001e,
                vec![128, 10, 20, 128, 30, 40, 128, 50, 60, 128, 70, 80],
            ),
            (
                0x020c_003c,
                vec![128, 10, 20, 128, 30, 40, 128, 50, 60, 128, 70, 80],
            ),
            (
                0x020c_005a,
                vec![10, 20, 128, 30, 40, 128, 50, 60, 128, 70, 80, 128],
            ),
        ] {
            let mut source = frame(format, 4, 2, data);
            assert_eq!(decoded(&source), expected);
            source.width = 3;
            assert!(
                unpacked(&source)
                    .unwrap_err()
                    .to_string()
                    .contains("divisible by 4")
            );
        }
    }

    #[test]
    fn broadcast_ycbcr_expands_limited_range_and_uses_its_matrix() {
        for format in [0x0218_003d, 0x0218_0040] {
            let data = vec![128, 16, 128, 128, 235, 128];
            assert_eq!(
                decoded(&frame(format, 2, 1, data)),
                [0, 0, 0, 255, 255, 255]
            );
        }
        for (format, y, cb, cr) in [(0x0218_003d, 81, 90, 240), (0x0218_0040, 63, 102, 240)] {
            let rgb = decoded(&frame(format, 1, 1, vec![cb, y, cr]));
            assert!(
                rgb[0] >= 254 && rgb[1] <= 1 && rgb[2] <= 1,
                "{format:#x}: {rgb:?}"
            );
        }
        for (format, data) in [
            (0x0210_003e, vec![16, 128, 235, 128]),
            (0x0210_0041, vec![16, 128, 235, 128]),
            (0x0210_0044, vec![128, 16, 128, 235]),
            (0x0210_0045, vec![128, 16, 128, 235]),
        ] {
            assert_eq!(
                decoded(&frame(format, 2, 1, data)),
                [0, 0, 0, 255, 255, 255]
            );
        }
        for format in [0x020c_003f, 0x020c_0042] {
            assert_eq!(
                decoded(&frame(format, 4, 1, vec![128, 16, 235, 128, 16, 235])),
                [0, 0, 0, 255, 255, 255, 0, 0, 0, 255, 255, 255]
            );
        }
    }

    /// Planar and semiplanar `Y'CbCr` expand their range and use their
    /// matrix: black, white and red in each color space.
    #[test]
    fn decoded_video_planes_use_their_range_and_matrix() {
        // Y, Cb, Cr of pure red.
        for (matrix, black, white, red) in [
            (Matrix::Full, 0, 255, [76, 85, 255]),
            (Matrix::Bt601, 16, 235, [81, 90, 240]),
            (Matrix::Bt709, 16, 235, [63, 102, 240]),
            (Matrix::Full709, 0, 255, [54, 99, 255]),
        ] {
            for interleaved in [false, true] {
                let format = ycbcr_planes((2, 2), matrix, interleaved).unwrap();
                let label = name(format).unwrap();
                // A 2x2 image's luma, then its one Cb and Cr: as two planes
                // or one pair, the same bytes.
                for ([y, cb, cr], expected) in [
                    ([black, 128, 128], [0, 0, 0]),
                    ([white, 128, 128], [255, 255, 255]),
                    (red, [255, 0, 0]),
                ] {
                    let rgb = decoded(&frame(format, 2, 2, vec![y, y, y, y, cb, cr]));
                    for pixel in rgb.chunks(3) {
                        assert!(
                            pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                            "{label}: {pixel:?} for {expected:?}"
                        );
                    }
                }
            }
        }
        assert_eq!(
            name(ycbcr_planes((2, 1), Matrix::Full709, false).unwrap()),
            Some("YCbCr709Full_422_8_Planar")
        );
        assert_eq!(ycbcr_planes((2, 1), Matrix::Full709, true), None);
    }

    #[test]
    fn planar_layouts_name_each_planes_rows() {
        let rows = |format, width, height| {
            crate::frame::planes(format, width, height)
                .unwrap()
                .iter()
                .map(|p| (p.offset, p.row_bytes, p.rows))
                .collect::<Vec<_>>()
        };
        // Decoded 4:2:0 video: a 320-byte luma row, then 160-byte chroma
        // rows, never the 480-byte average PFNC's bits per pixel gives.
        assert_eq!(
            rows(planar((2, 2), Matrix::Bt709), 320, 240),
            [(0, 320, 240), (76_800, 160, 120), (96_000, 160, 120)]
        );
        assert_eq!(
            rows(planar((2, 1), Matrix::Full), 321, 3),
            [(0, 321, 3), (963, 161, 3), (1446, 161, 3)]
        );
        assert_eq!(
            rows(NV12_709, 320, 240),
            [(0, 320, 240), (76_800, 320, 120)]
        );
        assert_eq!(
            rows(0x0230_0022, 4, 2),
            [(0, 8, 2), (16, 8, 2), (32, 8, 2)],
            "RGB10_Planar"
        );
        assert!(crate::frame::planes(MONO8, 320, 240).is_none());
        assert!(crate::frame::planes(RGB8, 320, 240).is_none());
        // The planes make up the whole frame.
        for format in CODES.into_iter().filter(|&format| name(format).is_some()) {
            if let Some(planes) = crate::frame::planes(format, 13, 7) {
                let last = planes.last().unwrap();
                assert_eq!(
                    Some(last.offset + last.row_bytes * last.rows),
                    crate::frame::frame_len(format, 13, 7),
                    "{format:#x}"
                );
            }
        }
    }
    #[test]
    fn every_additional_layout_flows_through_image_consumers() {
        use crate::frame as conversion;
        use crate::{camera_profile::CameraKind, inspect::PreviewFormat};
        let mut exercised = 0;
        // Cover every identifier in each byte-size family declared above,
        // including the newer alignment codes beyond the first 256 IDs, and
        // Capturefab's own planar codes.
        for format in [
            0x0101, 0x0102, 0x0104, 0x0108, 0x020c, 0x0210, 0x0218, 0x021e, 0x0220, 0x0224, 0x0228,
            0x0230, 0x0240,
        ]
        .into_iter()
        .flat_map(|size| (0..=0x12e).map(move |id| (size << 16) | id))
        .chain(CODES.into_iter().filter(|code| code >> 31 == 1))
        .filter(|&format| name(format).is_some())
        {
            exercised += 1;
            let length = conversion::frame_len(format, 12, 8).unwrap();
            let data = (0..length + 3)
                .map(|i| i.to_le_bytes()[0].wrapping_mul(71).wrapping_add(29))
                .collect();
            let source = frame(format, 12, 8, data);
            let normalized = unpacked(&source).unwrap().unwrap();
            let label = name(format).unwrap();
            assert!(conversion::convertible(format), "{label}");
            assert_eq!(conversion::pixel_format_name(format), label);
            assert_eq!(
                conversion::rgb(&source).unwrap(),
                conversion::rgb(&normalized).unwrap(),
                "RGB {label}"
            );
            assert_eq!(
                conversion::rgba(&source).unwrap(),
                conversion::rgba(&normalized).unwrap(),
                "RGBA {label}"
            );
            assert_eq!(
                conversion::gray8(&source).unwrap(),
                conversion::gray8(&normalized).unwrap(),
                "gray {label}"
            );
            assert_eq!(
                conversion::exposure(&source).unwrap(),
                conversion::exposure(&normalized).unwrap(),
                "exposure {label}"
            );
            assert_eq!(
                conversion::sharpness(&source, [1, 1, 10, 6]).unwrap(),
                conversion::sharpness(&normalized, [1, 1, 10, 6]).unwrap(),
                "focus {label}"
            );
            let preview = |frame| {
                crate::inspect::inspect(frame, CameraKind::AreaScan, None, 5, PreviewFormat::Png)
                    .unwrap()
                    .image
            };
            assert_eq!(preview(&source), preview(&normalized), "preview {label}");
            // Components the 8-bit image keeps clip as it does; wider ones
            // clip at their own full scale.
            let clipped = conversion::clipping(&source).unwrap();
            assert_eq!(clipped.pixels, 96, "clipping {label}");
            if color_bits(format).is_none_or(|bits| bits == 8) {
                assert_eq!(
                    clipped,
                    conversion::clipping(&normalized).unwrap(),
                    "clipping {label}"
                );
            }
            assert!(CODES.contains(&format), "{label}");
            assert_eq!(
                conversion::name_bits(label),
                conversion::sample_bits(format),
                "bits {label}"
            );
            let cropped = crate::inspect::crop(&source, [3, 2, 5, 4]).unwrap();
            let expected = crate::inspect::crop(&normalized, [3, 2, 5, 4]).unwrap();
            assert_eq!(
                (
                    cropped.width,
                    cropped.height,
                    conversion::rgb(&cropped).unwrap()
                ),
                (
                    expected.width,
                    expected.height,
                    conversion::rgb(&expected).unwrap()
                ),
                "crop {label}"
            );
            assert_eq!(
                conversion::encode(&source, "png").unwrap(),
                conversion::encode(&normalized, "png").unwrap(),
                "PNG {label}"
            );
            assert_eq!(
                conversion::encode(&source, "raw").unwrap(),
                source.data,
                "raw {label}"
            );
        }
        assert_eq!(
            exercised,
            CODES.len(),
            "all declared additional layouts are exercised"
        );
    }

    #[test]
    fn bayer_focus_clamps_maximum_size_regions_without_overflow() {
        let source = frame(0x0108_0009, 8, 8, (0..64).map(|i| i * 3).collect());
        let actual = crate::frame::sharpness(&source, [1, 1, u32::MAX, u32::MAX]).unwrap();
        let expected = crate::frame::sharpness(&source, [1, 1, 7, 7]).unwrap();
        assert_eq!(actual, expected);
    }
}
