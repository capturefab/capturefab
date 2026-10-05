//! JPEG stills. NVIDIA GPUs encode through nvJPEG in an isolated helper process
//! (`nvjpeg` feature); every other host, and any helper failure, uses
//! libjpeg-turbo's SIMD encoder in process (`jpeg` feature).
use crate::types::{Frame, MONO8};
use anyhow::Result;
#[cfg(not(feature = "jpeg"))]
use anyhow::bail;

/// Fixed quality and 4:2:0 chroma (gray for monochrome) on every backend, so
/// the same frame produces comparable files wherever it is encoded.
pub const QUALITY: i32 = 90;

/// 8-bit pixels ready for compression: one gray channel or interleaved RGB.
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub data: Vec<u8>,
}

impl Raster {
    pub fn new(frame: &Frame) -> Result<Self> {
        let monochrome = matches!(
            frame.pixel_format,
            MONO8 | 0x0110_0003 | 0x0110_0005 | 0x0110_0007
        );
        let (channels, data) = if monochrome {
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
    NvJpeg,
    TurboJpeg,
}

pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    Ok(encode_with_backend(frame)?.0)
}

pub fn encode_with_backend(frame: &Frame) -> Result<(Vec<u8>, Backend)> {
    let raster = std::sync::Arc::new(Raster::new(frame)?);
    #[cfg(feature = "nvjpeg")]
    if let Some(bytes) = crate::nvjpeg::encode(&raster) {
        return Ok((bytes, Backend::NvJpeg));
    }
    Ok((software(&raster)?, Backend::TurboJpeg))
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
