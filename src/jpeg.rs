//! JPEG stills, on the fastest encoder the host has. Apple silicon encodes
//! color frames on its JPEG engine through VideoToolbox (`videotoolbox`
//! feature); NVIDIA GPUs encode through nvJPEG and Linux VA-API drivers with
//! JPEG encoding (Intel) through libva, each in an isolated helper process
//! (`nvjpeg` and `vaapi` features); every other host, and any accelerator
//! failure, uses libjpeg-turbo's SIMD encoder in process (`jpeg` feature).
use crate::types::{Frame, MONO8};
use anyhow::Result;
#[cfg(not(feature = "jpeg"))]
use anyhow::bail;

/// Fixed quality and 4:2:0 chroma (gray for monochrome) on every backend, so
/// the same frame produces comparable files wherever it is encoded.
pub const QUALITY: i32 = 90;

pub(crate) fn monochrome(pixel_format: u32) -> bool {
    matches!(
        pixel_format,
        MONO8 | 0x0110_0003 | 0x0110_0005 | 0x0110_0007
    )
}

/// 8-bit pixels ready for compression: one gray channel or interleaved RGB.
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub data: Vec<u8>,
}

impl Raster {
    pub fn new(frame: &Frame) -> Result<Self> {
        let (channels, data) = if monochrome(frame.pixel_format) {
            // One pass straight to 8-bit gray; no RGB triplication.
            (1, crate::frame::convert(frame, |[gray, _, _]| gray)?)
        } else {
            (3, crate::frame::rgb(frame)?)
        };
        Ok(Self {
            width: frame.width,
            height: frame.height,
            channels,
            data,
        })
    }
}

/// Which encoder produced a JPEG, for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    VideoToolbox,
    NvJpeg,
    VaApi,
    TurboJpeg,
}

pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    Ok(encode_with_backend(frame)?.0)
}

pub fn encode_with_backend(frame: &Frame) -> Result<(Vec<u8>, Backend)> {
    // The engine always writes three components, so monochrome stays on the
    // CPU, where a gray JPEG is also the cheapest encode.
    #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
    if !monochrome(frame.pixel_format)
        && let Some(bytes) = crate::vtjpeg::encode(frame)
    {
        return Ok((bytes, Backend::VideoToolbox));
    }
    #[cfg(feature = "nvjpeg")]
    if let Some(bytes) = crate::nvjpeg::encode(frame) {
        return Ok((bytes, Backend::NvJpeg));
    }
    #[cfg(feature = "vaapi")]
    if let Some(bytes) = crate::vajpeg::encode(frame) {
        return Ok((bytes, Backend::VaApi));
    }
    Ok((software(&Raster::new(frame)?)?, Backend::TurboJpeg))
}

#[cfg(feature = "jpeg")]
pub fn software(raster: &Raster) -> Result<Vec<u8>> {
    use turbojpeg::{Compressor, Image, PixelFormat, Subsamp};
    let mut compressor = Compressor::new()?;
    compressor.set_quality(QUALITY)?;
    let (format, subsamp) = if raster.channels == 1 {
        (PixelFormat::GRAY, Subsamp::Gray)
    } else {
        (PixelFormat::RGB, Subsamp::Sub2x2)
    };
    compressor.set_subsamp(subsamp)?;
    Ok(compressor.compress_to_vec(Image {
        pixels: raster.data.as_slice(),
        width: raster.width as usize,
        pitch: raster.width as usize * raster.channels,
        height: raster.height as usize,
        format,
    })?)
}

#[cfg(not(feature = "jpeg"))]
pub fn software(_: &Raster) -> Result<Vec<u8>> {
    bail!("JPEG is unsupported in this build; rebuild with --features jpeg or capture png")
}

/// JFIF (full-range BT.601) YCbCr 4:2:0 from RGB, written straight into an
/// encoder's luma and interleaved CbCr planes (NV12 layout) at their row
/// strides. Chroma is the 2x2 block average, with edges replicated for odd
/// sizes; large images are split by row pairs across cores.
pub fn ycbcr420(
    rgb: &[[u8; 3]],
    width: usize,
    height: usize,
    luma: &mut [u8],
    luma_stride: usize,
    chroma: &mut [u8],
    chroma_stride: usize,
) {
    let (chroma_width, chroma_rows) = (width.div_ceil(2), height.div_ceil(2));
    assert!(
        rgb.len() >= width * height
            && luma_stride >= width
            && chroma_stride >= chroma_width * 2
            && luma.len() >= (height.max(1) - 1) * luma_stride + width
            && chroma.len() >= (chroma_rows.max(1) - 1) * chroma_stride + chroma_width * 2
    );
    // Fixed point with 16 fractional bits, as libjpeg's jccolor.c.
    let y = |[r, g, b]: [u8; 3]| {
        ((19595 * r as u32 + 38470 * g as u32 + 7471 * b as u32 + 32768) >> 16) as u8
    };
    let pair = |row: usize, luma: &mut [u8], chroma: &mut [u8]| {
        let top = &rgb[2 * row * width..][..width];
        let bottom = &rgb[(2 * row + 1).min(height - 1) * width..][..width];
        for (dst, src) in luma[..width].iter_mut().zip(top) {
            *dst = y(*src);
        }
        if 2 * row + 1 < height {
            for (dst, src) in luma[luma_stride..][..width].iter_mut().zip(bottom) {
                *dst = y(*src);
            }
        }
        let blocks = top.chunks(2).zip(bottom.chunks(2));
        for (cbcr, (t, d)) in chroma[..chroma_width * 2]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(blocks)
        {
            // An odd final column repeats its own pixel.
            let quad = [t[0], t[t.len() - 1], d[0], d[d.len() - 1]];
            let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
            for p in quad {
                (r, g, b) = (r + p[0] as i32, g + p[1] as i32, b + p[2] as i32);
            }
            // Four summed pixels: shift by 18 and round, offset by 128.
            let scale = |v: i32| ((v + (128 << 18) + (1 << 17)) >> 18).clamp(0, 255) as u8;
            cbcr[0] = scale(-11059 * r - 21709 * g + 32768 * b);
            cbcr[1] = scale(32768 * r - 27439 * g - 5329 * b);
        }
    };
    let threads = crate::frame::worker_threads(width * height);
    let block = chroma_rows.div_ceil(threads).max(1);
    if threads > 1 {
        std::thread::scope(|scope| {
            for (i, (luma, chroma)) in luma
                .chunks_mut(2 * block * luma_stride)
                .zip(chroma.chunks_mut(block * chroma_stride))
                .enumerate()
            {
                let pair = &pair;
                scope.spawn(move || {
                    for j in 0..block.min(chroma_rows - i * block) {
                        pair(
                            i * block + j,
                            &mut luma[2 * j * luma_stride..],
                            &mut chroma[j * chroma_stride..],
                        );
                    }
                });
            }
        });
    } else {
        for row in 0..chroma_rows {
            pair(
                row,
                &mut luma[2 * row * luma_stride..],
                &mut chroma[row * chroma_stride..],
            );
        }
    }
}

/// A complete baseline JPEG: SOI, a frame header with the expected size, EOI.
/// Used to reject output from a misbehaving accelerator before it is saved.
pub fn well_formed(bytes: &[u8], width: u32, height: u32) -> bool {
    if bytes.len() < 4 || bytes[..2] != [0xFF, 0xD8] || bytes[bytes.len() - 2..] != [0xFF, 0xD9] {
        return false;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() && bytes[i] == 0xFF {
        let marker = bytes[i + 1];
        let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // Baseline, extended or progressive Huffman frame header.
        if matches!(marker, 0xC0..=0xC2) {
            return bytes.get(i + 5..i + 9).is_some_and(|size| {
                u16::from_be_bytes([size[0], size[1]]) as u32 == height
                    && u16::from_be_bytes([size[2], size[3]]) as u32 == width
            });
        }
        if marker == 0xDA || length < 2 {
            return false;
        }
        i += 2 + length;
    }
    false
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
    #[test]
    fn rasters_are_gray_for_mono_and_rgb_otherwise() {
        let mono = Raster::new(&frame(0x0110_0005, 5, 3, 2)).unwrap();
        assert_eq!((mono.channels, mono.data.len()), (1, 15));
        let full = crate::frame::rgb(&frame(0x0110_0005, 5, 3, 2)).unwrap();
        assert!(
            mono.data
                .iter()
                .zip(full.chunks(3))
                .all(|(g, p)| *g == p[0])
        );
        let bayer = Raster::new(&frame(0x0108_0009, 6, 4, 1)).unwrap();
        assert_eq!((bayer.channels, bayer.data.len()), (3, 72));
        assert!(Raster::new(&frame(0, 2, 2, 1)).is_err());
    }
    #[test]
    fn ycbcr420_matches_jfif_at_any_size_and_stride() {
        let jfif = |[r, g, b]: [f64; 3]| {
            [
                0.299 * r + 0.587 * g + 0.114 * b,
                128.0 - 0.168736 * r - 0.331264 * g + 0.5 * b,
                128.0 + 0.5 * r - 0.418688 * g - 0.081312 * b,
            ]
        };
        // Odd sizes replicate edges; 1031x517 exceeds the threading threshold.
        for (width, height) in [
            (1usize, 1usize),
            (2, 2),
            (3, 1),
            (5, 7),
            (33, 17),
            (1031, 517),
        ] {
            let rgb: Vec<[u8; 3]> = (0..width * height)
                .map(|i| {
                    [
                        (i * 7) as u8,
                        (i * 13 + i / width) as u8,
                        (255 - i % 256) as u8,
                    ]
                })
                .collect();
            let (luma_stride, chroma_stride) = (width + 3, width.div_ceil(2) * 2 + 6);
            let mut luma = vec![0; luma_stride * height];
            let mut chroma = vec![0; chroma_stride * height.div_ceil(2)];
            ycbcr420(
                &rgb,
                width,
                height,
                &mut luma,
                luma_stride,
                &mut chroma,
                chroma_stride,
            );
            let at = |x: usize, y: usize| {
                rgb[y.min(height - 1) * width + x.min(width - 1)].map(f64::from)
            };
            for y in 0..height {
                for x in 0..width {
                    let expected = jfif(at(x, y))[0];
                    assert!(
                        (luma[y * luma_stride + x] as f64 - expected).abs() <= 0.51,
                        "{width}x{height} Y {x},{y}"
                    );
                }
            }
            for y in 0..height.div_ceil(2) {
                for x in 0..width.div_ceil(2) {
                    let mean = [(0, 0), (1, 0), (0, 1), (1, 1)]
                        .map(|(dx, dy)| jfif(at(2 * x + dx, 2 * y + dy)))
                        .iter()
                        .fold([0.0; 2], |s, p| [s[0] + p[1] / 4.0, s[1] + p[2] / 4.0]);
                    for c in 0..2 {
                        let got = chroma[y * chroma_stride + 2 * x + c] as f64;
                        assert!(
                            (got - mean[c].clamp(0.0, 255.0)).abs() <= 0.51,
                            "{width}x{height} C{c} {x},{y}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn marker_validation_requires_matching_frame_header() {
        assert!(!well_formed(&[0xFF, 0xD8, 0xFF, 0xD9], 1, 1));
        assert!(!well_formed(&[], 1, 1));
    }
    #[cfg(feature = "jpeg")]
    #[test]
    fn software_encoding_produces_valid_gray_and_color_jpegs() {
        for (format, bytes, width, height) in [(MONO8, 1, 33, 17), (RGB8, 3, 64, 48)] {
            let raster = Raster::new(&frame(format, width, height, bytes)).unwrap();
            let jpeg = software(&raster).unwrap();
            assert!(well_formed(&jpeg, width, height), "{format:x}");
            assert!(!well_formed(&jpeg, width + 1, height));
            // Component count in SOF0: 1 for gray, 3 for color.
            let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
            assert_eq!(jpeg[sof + 9], raster.channels as u8);
        }
    }
}
