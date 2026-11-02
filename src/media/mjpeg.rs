//! Motion JPEG sources read as the JPEGs they send. FFmpeg copies the
//! source's JPEGs to the pipe as they arrive (`-c:v copy -f mjpeg`), one
//! after another; each is cut out here and decoded with libjpeg-turbo into
//! the frame, which keeps the JPEG: a JPEG still of the frame is that JPEG,
//! with its comment, rather than a second encoding of a decoding.
//!
//! Frames are in the layout FFmpeg's own decoder hands over: full-range
//! Y'CbCr planes at the JPEG's subsampling (4:2:0, 4:2:2 or 4:4:4), gray,
//! or RGB for anything else.
use crate::types::Frame;
use anyhow::{Context, Result, ensure};
use std::io::BufRead;

/// The largest JPEG read from a source.
const MAX_JPEG: usize = 64 * 1024 * 1024;

/// The next whole JPEG in `reader`, SOI to EOI; None at the end of the
/// stream between images. The segments before a scan are skipped by their
/// lengths, so a thumbnail inside one does not end the image early, and a
/// scan's entropy-coded data ends at the first marker that is not a
/// restart.
pub(super) fn read_jpeg(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    // Anything between images, which FFmpeg never writes, is skipped.
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        match available.iter().position(|&byte| byte == 0xFF) {
            Some(at) => {
                reader.consume(at);
                break;
            }
            None => {
                let skip = available.len();
                reader.consume(skip);
            }
        }
    }
    let mut jpeg = Vec::new();
    take(reader, &mut jpeg, 2)?;
    ensure!(
        jpeg == [0xFF, 0xD8],
        "FFmpeg sent something other than a JPEG"
    );
    // A scan ends having read the 0xFF of the marker after it.
    let mut scanned = false;
    loop {
        match marker(reader, &mut jpeg, std::mem::take(&mut scanned))? {
            0xD9 => return Ok(Some(jpeg)),
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD7 => {}
            marker => {
                take(reader, &mut jpeg, 2)?;
                let length = u16::from_be_bytes([jpeg[jpeg.len() - 2], jpeg[jpeg.len() - 1]]);
                ensure!(length >= 2, "malformed JPEG segment");
                take(reader, &mut jpeg, usize::from(length) - 2)?;
                if marker == 0xDA {
                    scan(reader, &mut jpeg)?;
                    scanned = true;
                }
            }
        }
    }
}

/// Append exactly `count` bytes of `reader` to `jpeg`.
fn take(reader: &mut impl BufRead, jpeg: &mut Vec<u8>, count: usize) -> Result<()> {
    ensure!(jpeg.len() + count <= MAX_JPEG, "JPEG frame exceeds 64 MiB");
    let start = jpeg.len();
    jpeg.resize(start + count, 0);
    reader
        .read_exact(&mut jpeg[start..])
        .context("truncated JPEG frame")
}

/// The next byte, without taking it.
fn peek(reader: &mut impl BufRead) -> Result<u8> {
    reader
        .fill_buf()?
        .first()
        .copied()
        .context("truncated JPEG frame")
}

/// The next marker's code, appended to `jpeg` with its 0xFF; fill bytes
/// before it are dropped. `ff_read`: its first 0xFF was taken already.
fn marker(reader: &mut impl BufRead, jpeg: &mut Vec<u8>, ff_read: bool) -> Result<u8> {
    if !ff_read {
        ensure!(
            peek(reader)? == 0xFF,
            "malformed JPEG: a segment does not start with a marker"
        );
        reader.consume(1);
    }
    loop {
        let code = peek(reader)?;
        reader.consume(1);
        if code != 0xFF {
            jpeg.extend_from_slice(&[0xFF, code]);
            return Ok(code);
        }
    }
}

/// Append a scan's entropy-coded data, taking the 0xFF of the marker after
/// it: 0xFF followed by 0 is a stuffed byte and by D0-D7 a restart marker,
/// both part of the data.
fn scan(reader: &mut impl BufRead, jpeg: &mut Vec<u8>) -> Result<()> {
    loop {
        let available = reader.fill_buf()?;
        ensure!(!available.is_empty(), "truncated JPEG frame");
        let at = available.iter().position(|&byte| byte == 0xFF);
        let count = at.unwrap_or(available.len());
        ensure!(jpeg.len() + count <= MAX_JPEG, "JPEG frame exceeds 64 MiB");
        jpeg.extend_from_slice(&available[..count]);
        reader.consume(count);
        if at.is_none() {
            continue;
        }
        reader.consume(1);
        let next = peek(reader)?;
        if next == 0 || (0xD0..=0xD7).contains(&next) {
            jpeg.extend_from_slice(&[0xFF, next]);
            reader.consume(1);
        } else {
            return Ok(());
        }
    }
}

/// Decode `jpeg` into a frame that keeps it, from a pooled buffer that
/// every pixel is written to.
pub(super) fn decode(
    decompressor: &mut turbojpeg::Decompressor,
    jpeg: Vec<u8>,
    id: u64,
    pool: &crate::framebuf::FramePool,
) -> Result<Frame> {
    use crate::frame::{Matrix, ycbcr_planes};
    use turbojpeg::{Colorspace, Image, PixelFormat, Subsamp, YuvImage};
    let header = decompressor.read_header(&jpeg)?;
    let (width, height) = (header.width, header.height);
    ensure!(
        width > 0 && height > 0 && width.saturating_mul(height) <= super::MAX_FRAME / 3,
        "JPEG frame of {width}x{height} is out of bounds"
    );
    let (w, h) = (width as u32, height as u32);
    // Planes as FFmpeg's decoder lays them out, where libjpeg-turbo's are
    // the same: whole chroma samples, rows packed.
    let planes = match (header.colorspace, header.subsamp) {
        (Colorspace::YCbCr, Subsamp::Sub2x2) => Some(((2, 2), Subsamp::Sub2x2)),
        (Colorspace::YCbCr, Subsamp::Sub2x1) => Some(((2, 1), Subsamp::Sub2x1)),
        (Colorspace::YCbCr, Subsamp::None) => Some(((1, 1), Subsamp::None)),
        _ => None,
    }
    .and_then(|(sub, subsamp)| {
        let code = ycbcr_planes(sub, Matrix::Full, false)?;
        let len = crate::frame::frame_len(code, w, h)?;
        (turbojpeg::yuv_pixels_len(width, 1, height, subsamp).ok()? == len)
            .then_some((code, len, subsamp))
    });
    let (pixel_format, data) = match planes {
        Some((code, len, subsamp)) => {
            let mut data = pool.take_stale(len);
            decompressor.decompress_to_yuv(
                &jpeg,
                YuvImage {
                    pixels: &mut data[..],
                    width,
                    align: 1,
                    height,
                    subsamp,
                },
            )?;
            (code, data)
        }
        None => {
            let (code, format) = if header.colorspace == Colorspace::Gray {
                (crate::types::MONO8, PixelFormat::GRAY)
            } else {
                (crate::types::RGB8, PixelFormat::RGB)
            };
            let channels = format.size();
            let mut data = pool.take_stale(width * height * channels);
            decompressor.decompress(
                &jpeg,
                Image {
                    pixels: &mut data[..],
                    width,
                    pitch: width * channels,
                    height,
                    format,
                },
            )?;
            (code, data)
        }
    };
    crate::copies::record(crate::copies::Site::PipeRead, jpeg.len());
    Ok(Frame {
        id,
        width: w,
        height: h,
        pixel_format,
        device_ts_ns: None,
        wall_ms: 0,
        data: data.freeze(),
        jpeg: Some(jpeg.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// A JPEG-shaped stream: SOI, APP1 holding a thumbnail's SOI and EOI,
    /// DQT, SOS, entropy data with a stuffed byte and a restart marker, a
    /// second scan after a DHT (as progressive JPEG has), then EOI.
    fn shaped(fill: u8) -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend([0xFF, 0xE1, 0, 8, 0xFF, 0xD8, 0xFF, 0xD9, 1, 2]);
        jpeg.extend([0xFF, 0xDB, 0, 4, 7, 7]);
        jpeg.extend([0xFF, 0xDA, 0, 3, 9]);
        jpeg.extend([1, 0xFF, 0x00, 2, 0xFF, 0xD3, 3, fill]);
        jpeg.extend([0xFF, 0xC4, 0, 3, 5]);
        jpeg.extend([0xFF, 0xDA, 0, 3, 9, 4, 5, 6]);
        jpeg.extend([0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn jpegs_are_cut_from_a_stream_however_it_is_read() {
        let (first, second) = (shaped(0x11), shaped(0x22));
        let stream = [&first[..], &second[..]].concat();
        for capacity in [1, 2, 3, 5, 8, 64, 4096] {
            let mut reader = BufReader::with_capacity(capacity, &stream[..]);
            assert_eq!(
                read_jpeg(&mut reader).unwrap().unwrap(),
                first,
                "{capacity}"
            );
            assert_eq!(
                read_jpeg(&mut reader).unwrap().unwrap(),
                second,
                "{capacity}"
            );
            assert!(read_jpeg(&mut reader).unwrap().is_none());
        }
        // Fill bytes before a marker are dropped; bytes between images are
        // skipped.
        let mut filled = shaped(0x33);
        let eoi = filled.len() - 2;
        filled.insert(eoi, 0xFF);
        let noisy = [&[0u8, 1, 2][..], &filled].concat();
        let mut reader = BufReader::with_capacity(3, &noisy[..]);
        assert_eq!(read_jpeg(&mut reader).unwrap().unwrap(), shaped(0x33));
        // A stream that ends inside an image, or holds something else.
        let cut = &first[..first.len() - 3];
        assert!(read_jpeg(&mut BufReader::new(cut)).is_err());
        assert!(read_jpeg(&mut BufReader::new(&[0xFF, 0x00, 1][..])).is_err());
    }

    #[test]
    fn jpegs_decode_to_the_layout_ffmpeg_hands_over() {
        use crate::frame::{Matrix, ycbcr_planes};
        let pool = crate::framebuf::FramePool::new(|| None);
        let mut decompressor = turbojpeg::Decompressor::new().unwrap();
        let image = |width: usize, height: usize, channels: usize| {
            let format = if channels == 1 {
                turbojpeg::PixelFormat::GRAY
            } else {
                turbojpeg::PixelFormat::RGB
            };
            // Smooth, so chroma upsampling methods agree closely.
            let pixels: Vec<u8> = (0..width * height * channels)
                .map(|i| {
                    let (pixel, channel) = (i / channels, i % channels);
                    let (x, y) = (pixel % width, pixel / width);
                    (x * 2 + y + channel * 30) as u8
                })
                .collect();
            (pixels, format)
        };
        for (subsamp, width, height, expected) in [
            (
                turbojpeg::Subsamp::Sub2x2,
                64,
                48,
                ycbcr_planes((2, 2), Matrix::Full, false),
            ),
            (
                turbojpeg::Subsamp::Sub2x1,
                64,
                48,
                ycbcr_planes((2, 1), Matrix::Full, false),
            ),
            (
                turbojpeg::Subsamp::None,
                64,
                48,
                ycbcr_planes((1, 1), Matrix::Full, false),
            ),
            // Odd sizes and other subsamplings are RGB.
            (turbojpeg::Subsamp::Sub2x2, 63, 47, Some(crate::types::RGB8)),
            (turbojpeg::Subsamp::Sub1x2, 64, 48, Some(crate::types::RGB8)),
            (turbojpeg::Subsamp::Gray, 64, 48, Some(crate::types::MONO8)),
        ] {
            let channels = if subsamp == turbojpeg::Subsamp::Gray {
                1
            } else {
                3
            };
            let (pixels, format) = image(width, height, channels);
            let jpeg = turbojpeg::compress(
                turbojpeg::Image {
                    pixels: &pixels[..],
                    width,
                    pitch: width * channels,
                    height,
                    format,
                },
                90,
                subsamp,
            )
            .unwrap()
            .to_vec();
            let frame = decode(&mut decompressor, jpeg.clone(), 7, &pool).unwrap();
            assert_eq!(
                Some(frame.pixel_format),
                expected,
                "{subsamp:?} {width}x{height}"
            );
            assert_eq!(
                (frame.width, frame.height, frame.id),
                (width as u32, height as u32, 7)
            );
            assert_eq!(
                frame.data.len(),
                crate::frame::frame_len(frame.pixel_format, frame.width, frame.height).unwrap()
            );
            assert_eq!(frame.jpeg.as_deref(), Some(&jpeg[..]));
            // Luma as libjpeg-turbo decodes it, and the image close to its
            // RGB decoding.
            let gray = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::GRAY).unwrap();
            if frame.pixel_format != crate::types::RGB8 {
                assert_eq!(frame.data[..width * height], gray.pixels[..], "{subsamp:?}");
            }
            let rgb = crate::frame::rgb(&frame).unwrap();
            let reference = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::RGB).unwrap();
            if frame.pixel_format != crate::types::MONO8 {
                let worst = rgb
                    .iter()
                    .zip(&reference.pixels)
                    .map(|(a, b)| a.abs_diff(*b))
                    .max()
                    .unwrap();
                assert!(worst <= 3, "{subsamp:?}: off by {worst}");
            }
        }
        assert!(decode(&mut decompressor, vec![0xFF, 0xD8, 0xFF, 0xD9], 1, &pool).is_err());
    }
}
