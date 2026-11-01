//! JPEG stills, encoded in process by libjpeg-turbo's SIMD encoder (`jpeg`
//! feature).
use crate::types::{Frame, MONO8};
use anyhow::Result;
#[cfg(not(feature = "jpeg"))]
use anyhow::bail;

/// Fixed quality and 4:2:0 chroma (gray for monochrome), so the same frame
/// produces comparable files wherever it is encoded.
pub const QUALITY: i32 = 90;

pub(crate) fn monochrome(pixel_format: u32) -> bool {
    matches!(
        crate::frame::unpacked_format(pixel_format),
        MONO8 | 0x0110_0003 | 0x0110_0005 | 0x0110_0025 | 0x0110_0007
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

/// libjpeg-turbo reads RGB8, BGR8 and Mono8 frames in place; other formats
/// are converted into a [`Raster`] first.
#[cfg(feature = "jpeg")]
pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    use turbojpeg::PixelFormat;
    let (format, channels) = match frame.pixel_format {
        crate::types::RGB8 => (PixelFormat::RGB, 3),
        0x0218_0015 => (PixelFormat::BGR, 3),
        MONO8 => (PixelFormat::GRAY, 1),
        _ => return software(&Raster::new(frame)?),
    };
    let size = crate::frame::checked_pixels(frame)? * channels;
    anyhow::ensure!(
        frame.data.len() >= size,
        "truncated {} frame",
        crate::frame::pixel_format_name(frame.pixel_format)
    );
    compress(
        &frame.data[..size],
        frame.width,
        frame.height,
        format,
        QUALITY,
    )
}
#[cfg(not(feature = "jpeg"))]
pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    software(&Raster::new(frame)?)
}

pub fn software(raster: &Raster) -> Result<Vec<u8>> {
    software_quality(raster, QUALITY)
}

/// libjpeg-turbo at a chosen quality (1–100), for previews that trade
/// detail for size; stills use `software` and the fixed `QUALITY`.
#[cfg(feature = "jpeg")]
pub fn software_quality(raster: &Raster, quality: i32) -> Result<Vec<u8>> {
    use turbojpeg::PixelFormat;
    let format = if raster.channels == 1 {
        PixelFormat::GRAY
    } else {
        PixelFormat::RGB
    };
    compress(&raster.data, raster.width, raster.height, format, quality)
}

#[cfg(feature = "jpeg")]
fn compress(
    pixels: &[u8],
    width: u32,
    height: u32,
    format: turbojpeg::PixelFormat,
    quality: i32,
) -> Result<Vec<u8>> {
    use turbojpeg::{Compressor, Image, PixelFormat, Subsamp};
    anyhow::ensure!((1..=100).contains(&quality), "JPEG quality must be 1..100");
    let mut compressor = Compressor::new()?;
    compressor.set_quality(quality)?;
    compressor.set_subsamp(if format == PixelFormat::GRAY {
        Subsamp::Gray
    } else {
        Subsamp::Sub2x2
    })?;
    Ok(compressor.compress_to_vec(Image {
        pixels,
        width: width as usize,
        pitch: width as usize * format.size(),
        height: height as usize,
        format,
    })?)
}

#[cfg(not(feature = "jpeg"))]
pub fn software_quality(_: &Raster, _: i32) -> Result<Vec<u8>> {
    bail!("JPEG is unsupported in this build; rebuild with --features jpeg or capture png")
}

/// A complete baseline JPEG: SOI, a frame header with the expected size, EOI.
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

/// Add a COM (comment) segment holding `payload` to an encoded JPEG, after
/// SOI and any leading APPn segments so a JFIF or EXIF header stays first.
/// It works on the finished bytes, so the comment does not depend on which
/// encoder ran.
pub fn insert_com(jpeg: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        payload.len() <= usize::from(u16::MAX) - 2,
        "JPEG comment is too long"
    );
    anyhow::ensure!(
        jpeg.len() >= 4 && jpeg[..2] == [0xFF, 0xD8],
        "not a JPEG image"
    );
    let mut at = 2;
    while let [0xFF, 0xE0..=0xEF, high, low, ..] = jpeg[at..] {
        let length = usize::from(u16::from_be_bytes([high, low]));
        anyhow::ensure!(
            length >= 2 && at + 2 + length <= jpeg.len(),
            "malformed JPEG header"
        );
        at += 2 + length;
    }
    let mut out = Vec::with_capacity(jpeg.len() + payload.len() + 4);
    out.extend_from_slice(&jpeg[..at]);
    out.extend_from_slice(&[0xFF, 0xFE]);
    out.extend_from_slice(&(payload.len() as u16 + 2).to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&jpeg[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments_follow_app_segments() {
        // SOI, APP0 of length 4, a frame header, EOI.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xC0, 0, 2, 0xFF, 0xD9,
        ];
        let out = insert_com(&jpeg, b"{\"a\":1}").unwrap();
        assert_eq!(&out[..8], &jpeg[..8]);
        assert_eq!(&out[8..12], &[0xFF, 0xFE, 0, 9]);
        assert_eq!(&out[12..19], b"{\"a\":1}");
        assert_eq!(&out[19..], &jpeg[8..]);
        // Straight after SOI when there is no APPn.
        let bare = insert_com(&[0xFF, 0xD8, 0xFF, 0xD9], b"x").unwrap();
        assert_eq!(bare, [0xFF, 0xD8, 0xFF, 0xFE, 0, 3, b'x', 0xFF, 0xD9]);
        assert!(insert_com(&jpeg, &vec![0; 65_534]).is_err());
        assert!(insert_com(b"\x89PNG", b"x").is_err());
        assert!(insert_com(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 40, 0xFF, 0xD9], b"x").is_err());
    }
    #[cfg(feature = "jpeg")]
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
        // Mono14 and packed Mono12p stay gray; packed BayerRG12p is color.
        for (format, bytes) in [(0x0110_0025, 2.0), (0x010c_0047, 1.5), (0x010c_0059, 1.5)] {
            let size = (6.0 * 4.0 * bytes) as usize;
            let packed = Frame {
                data: (0..size).map(|i| (i * 37) as u8).collect(),
                ..frame(format, 6, 4, 0)
            };
            let raster = Raster::new(&packed).unwrap();
            let channels = if format == 0x010c_0059 { 3 } else { 1 };
            assert_eq!(raster.data.len(), 24 * channels, "{format:x}");
        }
        assert!(Raster::new(&frame(0, 2, 2, 1)).is_err());
    }
    #[test]
    fn marker_validation_requires_matching_frame_header() {
        assert!(!well_formed(&[0xFF, 0xD8, 0xFF, 0xD9], 1, 1));
        assert!(!well_formed(&[], 1, 1));
    }
    #[cfg(feature = "jpeg")]
    #[test]
    fn borrowed_frames_encode_byte_identical_to_rasters() {
        for (format, bytes) in [(RGB8, 3), (0x0218_0015, 3), (MONO8, 1)] {
            let frame = frame(format, 97, 61, bytes);
            let borrowed = encode(&frame).unwrap();
            assert_eq!(
                borrowed,
                software(&Raster::new(&frame).unwrap()).unwrap(),
                "{format:x}"
            );
            let mut short = frame.clone();
            short.data.pop();
            assert!(encode(&short).is_err(), "{format:x}");
        }
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
    #[cfg(feature = "jpeg")]
    #[test]
    fn lower_quality_makes_smaller_jpegs_and_out_of_range_quality_fails() {
        let raster = Raster::new(&frame(RGB8, 64, 48, 3)).unwrap();
        let fine = software_quality(&raster, 92).unwrap();
        let coarse = software_quality(&raster, 65).unwrap();
        assert!(well_formed(&coarse, 64, 48));
        assert!(coarse.len() < fine.len());
        assert_eq!(
            software(&raster).unwrap(),
            software_quality(&raster, QUALITY).unwrap()
        );
        for quality in [0, 101] {
            assert!(software_quality(&raster, quality).is_err());
        }
    }
}
