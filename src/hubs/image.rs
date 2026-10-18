//! Decode hub images (JPEG stills and MJPEG parts, PNG stills) to RGB8
//! frames in the camera worker. Dimensions are checked from the header before
//! any pixel buffer is allocated, so a crafted header cannot exhaust memory.
use crate::types::{Frame, RGB8};

/// The largest side accepted, as for native video modes.
const MAX_SIDE: usize = 8192;
/// Matches the media decoder's frame bound.
const MAX_FRAME: usize = 256 * 1024 * 1024;

/// Why an image produced no frame. The stream goes on; the pump counts these.
pub(crate) type Skip = &'static str;

fn checked(width: usize, height: usize) -> Result<usize, Skip> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err("image dimensions are outside 1..8192");
    }
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .filter(|bytes| *bytes <= MAX_FRAME)
        .ok_or("image exceeds the frame size limit")
}

fn frame(width: usize, height: usize, data: Vec<u8>) -> Frame {
    Frame {
        id: 0,
        width: width as u32,
        height: height as u32,
        pixel_format: RGB8,
        timestamp_ns: 0,
        data,
    }
}

/// Decode a JPEG or PNG by its signature.
pub(crate) fn decode(bytes: &[u8]) -> Result<Frame, Skip> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        jpeg(bytes)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(bytes)
    } else {
        Err("the image is neither JPEG nor PNG")
    }
}

fn jpeg(bytes: &[u8]) -> Result<Frame, Skip> {
    use turbojpeg::{Colorspace, Decompressor, Image, PixelFormat};
    let mut decompressor = Decompressor::new().map_err(|_| "the JPEG decoder is unavailable")?;
    let header = decompressor
        .read_header(bytes)
        .map_err(|_| "the JPEG header is damaged")?;
    if matches!(header.colorspace, Colorspace::CMYK | Colorspace::YCCK) {
        return Err("unsupported JPEG colour space");
    }
    let size = checked(header.width, header.height)?;
    let mut pixels = vec![0u8; size];
    decompressor
        .decompress(
            bytes,
            Image {
                pixels: pixels.as_mut_slice(),
                width: header.width,
                pitch: header.width * 3,
                height: header.height,
                format: PixelFormat::RGB,
            },
        )
        .map_err(|_| "the JPEG data is damaged")?;
    Ok(frame(header.width, header.height, pixels))
}

fn png(bytes: &[u8]) -> Result<Frame, Skip> {
    use png::{ColorType, Decoder, Limits, Transformations};
    let mut decoder = Decoder::new(bytes);
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    decoder.set_limits(Limits { bytes: MAX_FRAME });
    let mut reader = decoder
        .read_info()
        .map_err(|_| "the PNG header is damaged")?;
    let (width, height) = {
        let info = reader.info();
        (info.width as usize, info.height as usize)
    };
    let size = checked(width, height)?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let output = reader
        .next_frame(&mut buffer)
        .map_err(|_| "the PNG data is damaged")?;
    let channels = match output.color_type {
        ColorType::Grayscale => 1,
        ColorType::GrayscaleAlpha => 2,
        ColorType::Rgb => 3,
        ColorType::Rgba => 4,
        ColorType::Indexed => return Err("the PNG palette could not be expanded"),
    };
    let mut pixels = Vec::with_capacity(size);
    for row in buffer.chunks_exact(output.line_size).take(height) {
        for pixel in row[..width * channels].chunks_exact(channels) {
            match channels {
                1 | 2 => pixels.extend([pixel[0]; 3]),
                _ => pixels.extend(&pixel[..3]),
            }
        }
    }
    if pixels.len() != size {
        return Err("the PNG data is damaged");
    }
    Ok(frame(width, height, pixels))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A JPEG of `width`x`height` whose pixels are `value`.
    pub(crate) fn jpeg_of(width: u32, height: u32, value: u8) -> Vec<u8> {
        let frame = Frame {
            id: 1,
            width,
            height,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data: vec![value; (width * height * 3) as usize],
        };
        crate::jpeg::software(&crate::jpeg::Raster::new(&frame).unwrap()).expect("encode test JPEG")
    }

    #[test]
    fn jpeg_and_png_decode_to_rgb() {
        let frame = decode(&jpeg_of(64, 48, 200)).unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.pixel_format),
            (64, 48, RGB8)
        );
        assert_eq!(frame.data.len(), 64 * 48 * 3);
        assert!(frame.data.iter().all(|&v| (195..=205).contains(&v)));
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 3, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[
                    1, 2, 3, 255, 4, 5, 6, 0, 7, 8, 9, 9, 10, 11, 12, 1, 13, 14, 15, 2, 16, 17, 18,
                    3,
                ])
                .unwrap();
        }
        let frame = decode(&encoded).unwrap();
        assert_eq!((frame.width, frame.height), (3, 2));
        assert_eq!(frame.data, (1..=18).collect::<Vec<u8>>());
        let mut gray = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut gray, 2, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0x12, 0x34, 0xAB, 0xCD]).unwrap();
        }
        assert_eq!(
            decode(&gray).unwrap().data,
            [0x12, 0x12, 0x12, 0xAB, 0xAB, 0xAB]
        );
    }

    #[test]
    fn damaged_or_oversized_images_are_skipped_before_allocating() {
        assert!(decode(b"GIF89a").is_err());
        let mut damaged = jpeg_of(32, 32, 10);
        damaged.truncate(40);
        assert!(decode(&damaged).is_err());
        // A PNG header claiming 100000x100000 pixels.
        let mut bomb = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(100_000u32.to_be_bytes());
        ihdr.extend(100_000u32.to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        bomb.extend(&ihdr);
        let crc = {
            let mut data = b"IHDR".to_vec();
            data.extend(&ihdr);
            crc32(&data)
        };
        bomb.extend(crc.to_be_bytes());
        assert!(decode(&bomb).is_err());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
}
