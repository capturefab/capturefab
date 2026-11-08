//! Motion JPEG sources read as the JPEGs they send. `FFmpeg` copies the
//! source's JPEGs to the pipe as they arrive (`-c:v copy -f mjpeg`), one
//! after another; each is cut out here and decoded with libjpeg-turbo into
//! the frame, which keeps the JPEG: a JPEG still of the frame is that JPEG,
//! with its comment, rather than a second encoding of a decoding.
//!
//! Frames are in the layout `FFmpeg`'s own decoder hands over: full-range
//! `Y'CbCr` planes at the JPEG's subsampling (4:2:0, 4:2:2 or 4:4:4), gray,
//! or RGB for anything else.
#![forbid(unsafe_code)]
#![deny(clippy::panic_in_result_fn)]
use crate::types::Frame;
use anyhow::{Context as _, Result, ensure};
use std::io::BufRead;

/// The largest JPEG read from a source.
const MAX_JPEG: usize = 64 * 1024 * 1024;

/// Cuts the stream of JPEGs `FFmpeg` writes into whole images.
#[derive(Default)]
pub(super) struct Splitter {
    /// The last image ended at the next one's SOI, which is read already.
    soi_read: bool,
}
impl Splitter {
    /// The next whole JPEG in `reader`, SOI to EOI; None at the end of the
    /// stream between images. The segments before a scan are skipped by
    /// their lengths, so a thumbnail inside one does not end the image
    /// early, and a scan's entropy-coded data ends at the first marker that
    /// is not a restart. An image without an EOI, which webcams send and
    /// `FFmpeg`'s decoder accepts, ends where the next one starts, or at the
    /// end of the stream after a scan, and is given one. A JPEG that is not
    /// whole is `Malformed`; the next read starts at the next image.
    pub(super) fn next(&mut self, reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
        if !std::mem::take(&mut self.soi_read) && !soi(reader)? {
            return Ok(None);
        }
        let mut jpeg = vec![0xFF, 0xD8];
        // A scan ends having read the 0xFF of the marker after it.
        let mut scanned = false;
        loop {
            match marker(reader, &mut jpeg, std::mem::take(&mut scanned))? {
                0xD9 => return Ok(Some(jpeg)),
                0xD8 => {
                    jpeg.truncate(jpeg.len() - 2);
                    append(&mut jpeg, &[0xFF, 0xD9])?;
                    self.soi_read = true;
                    return Ok(Some(jpeg));
                }
                // Standalone markers carry no length.
                0x01 | 0xD0..=0xD7 => {}
                marker => {
                    take(reader, &mut jpeg, 2)?;
                    let length = u16::from_be_bytes([jpeg[jpeg.len() - 2], jpeg[jpeg.len() - 1]]);
                    ensure!(length >= 2, Malformed("malformed JPEG segment"));
                    take(reader, &mut jpeg, usize::from(length) - 2)?;
                    if marker == 0xDA {
                        if !scan(reader, &mut jpeg)? {
                            append(&mut jpeg, &[0xFF, 0xD9])?;
                            return Ok(Some(jpeg));
                        }
                        scanned = true;
                    }
                }
            }
        }
    }
}

/// A JPEG in the stream that is not whole or not well formed.
#[derive(Debug)]
pub(super) struct Malformed(&'static str);
impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Malformed {}

/// Take the next SOI; false at the end of the stream. Anything before it,
/// which `FFmpeg` writes only after a JPEG that was not whole, is skipped, up
/// to the size of a JPEG.
fn soi(reader: &mut impl BufRead) -> Result<bool> {
    let mut skipped = 0usize;
    // The last byte skipped was 0xFF.
    let mut ff = false;
    loop {
        let available = match reader.fill_buf() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            filled => filled?,
        };
        if available.is_empty() {
            ensure!(
                skipped <= MAX_JPEG,
                Malformed("FFmpeg sent something other than JPEGs")
            );
            return Ok(false);
        }
        let found = available.iter().position(|&byte| {
            let soi = ff && byte == 0xD8;
            ff = byte == 0xFF;
            soi
        });
        let count = found.map_or(available.len(), |at| at + 1);
        reader.consume(count);
        skipped = skipped
            .checked_add(count)
            .ok_or(Malformed("FFmpeg sent something other than JPEGs"))?;
        // SOI's two bytes are not junk. Before it is complete, a final FF
        // may still be its first byte in the next buffer.
        let junk = skipped.saturating_sub(if found.is_some() { 2 } else { usize::from(ff) });
        ensure!(
            junk <= MAX_JPEG,
            Malformed("FFmpeg sent something other than JPEGs")
        );
        if found.is_some() {
            return Ok(true);
        }
    }
}

/// The length after appending `count` bytes, within the frame limit.
fn bounded_end(jpeg: &[u8], count: usize) -> Result<usize> {
    jpeg.len()
        .checked_add(count)
        .filter(|end| *end <= MAX_JPEG)
        .ok_or_else(|| Malformed("JPEG frame exceeds 64 MiB").into())
}

fn append(jpeg: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    bounded_end(jpeg, bytes.len())?;
    jpeg.extend_from_slice(bytes);
    Ok(())
}

/// Append exactly `count` bytes of `reader` to `jpeg`.
fn take(reader: &mut impl BufRead, jpeg: &mut Vec<u8>, count: usize) -> Result<()> {
    let end = bounded_end(jpeg, count)?;
    let start = jpeg.len();
    jpeg.resize(end, 0);
    match reader.read_exact(&mut jpeg[start..]) {
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(Malformed("truncated JPEG frame").into())
        }
        read => Ok(read?),
    }
}

/// The next byte, without taking it; None at the end of the stream.
fn peek(reader: &mut impl BufRead) -> Result<Option<u8>> {
    loop {
        let available = match reader.fill_buf() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            filled => filled?,
        };
        return Ok(available.first().copied());
    }
}

/// The next marker's code, appended to `jpeg` with its 0xFF; fill bytes
/// before it are dropped. `ff_read`: its first 0xFF was taken already.
fn marker(reader: &mut impl BufRead, jpeg: &mut Vec<u8>, ff_read: bool) -> Result<u8> {
    let truncated = || Malformed("truncated JPEG frame");
    if !ff_read {
        ensure!(
            peek(reader)?.ok_or_else(truncated)? == 0xFF,
            Malformed("malformed JPEG: a segment does not start with a marker")
        );
        reader.consume(1);
    }
    loop {
        let code = peek(reader)?.ok_or_else(truncated)?;
        reader.consume(1);
        if code != 0xFF {
            append(jpeg, &[0xFF, code])?;
            return Ok(code);
        }
    }
}

/// Append a scan's entropy-coded data, taking the 0xFF of the marker after
/// it: 0xFF followed by 0 is a stuffed byte and by D0-D7 a restart marker,
/// both part of the data. False when the stream ends instead.
fn scan(reader: &mut impl BufRead, jpeg: &mut Vec<u8>) -> Result<bool> {
    loop {
        let available = match reader.fill_buf() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            filled => filled?,
        };
        if available.is_empty() {
            return Ok(false);
        }
        let at = available.iter().position(|&byte| byte == 0xFF);
        let count = at.unwrap_or(available.len());
        append(jpeg, &available[..count])?;
        reader.consume(count);
        if at.is_none() {
            continue;
        }
        reader.consume(1);
        match peek(reader)? {
            Some(next) if next == 0 || (0xD0..=0xD7).contains(&next) => {
                append(jpeg, &[0xFF, next])?;
                reader.consume(1);
            }
            Some(_) => return Ok(true),
            None => return Ok(false),
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
    let w = u32::try_from(width).context("JPEG width exceeds u32")?;
    let h = u32::try_from(height).context("JPEG height exceeds u32")?;
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
    let (pixel_format, data) = if let Some((code, len, subsamp)) = planes {
        let mut data = pool.take_stale(len);
        decompressor.decompress_to_yuv(
            &jpeg,
            YuvImage {
                pixels: &mut data,
                width,
                align: 1,
                height,
                subsamp,
            },
        )?;
        (code, data)
    } else {
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
                pixels: &mut data,
                width,
                pitch: width * channels,
                height,
                format,
            },
        )?;
        (code, data)
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
    use crate::test_io::FaultReader;
    use std::io::BufReader;

    #[test]
    fn interrupted_reads_preserve_jpeg_boundaries_and_other_errors_propagate() {
        let (first, second) = (shaped(0x11), shaped(0x22));
        let stream = [first.as_slice(), second.as_slice()].concat();
        for capacity in [1, 2, 3, 64] {
            let mut reader = FaultReader::new(
                BufReader::with_capacity(capacity, stream.as_slice()),
                std::io::ErrorKind::Interrupted,
            );
            let mut splitter = Splitter::default();
            assert_eq!(splitter.next(&mut reader).unwrap(), Some(first.clone()));
            assert_eq!(splitter.next(&mut reader).unwrap(), Some(second.clone()));
            assert!(splitter.next(&mut reader).unwrap().is_none());
        }
        let mut reader = FaultReader::new(stream.as_slice(), std::io::ErrorKind::PermissionDenied);
        let error = Splitter::default().next(&mut reader).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

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

    /// Every image `stream` holds, read through a buffer of `capacity`
    /// bytes, and how the read after the last one ended.
    fn cut(stream: &[u8], capacity: usize) -> (Vec<Vec<u8>>, Result<()>) {
        let mut reader = BufReader::with_capacity(capacity, stream);
        let mut splitter = Splitter::default();
        let mut jpegs = Vec::new();
        loop {
            match splitter.next(&mut reader) {
                Ok(Some(jpeg)) => jpegs.push(jpeg),
                Ok(None) => return (jpegs, Ok(())),
                Err(error) => return (jpegs, Err(error)),
            }
        }
    }

    #[test]
    fn resynchronization_limit_does_not_depend_on_buffer_boundaries() {
        let mut stream = vec![0; MAX_JPEG];
        stream.extend_from_slice(&[0xFF, 0xD8]);
        assert!(soi(&mut &*stream).unwrap());
        // The SOI marker is split across reads exactly after the limit.
        assert!(soi(&mut BufReader::with_capacity(MAX_JPEG + 1, &*stream)).unwrap());
        stream.insert(0, 0);
        assert!(soi(&mut &*stream).unwrap_err().is::<Malformed>());
        assert!(
            soi(&mut BufReader::with_capacity(MAX_JPEG + 1, &*stream))
                .unwrap_err()
                .is::<Malformed>()
        );
    }

    #[test]
    fn markers_and_scan_escapes_respect_the_frame_limit() {
        let mut jpeg = vec![0; MAX_JPEG - 1];
        assert!(
            marker(&mut &[0xFF, 0xD9][..], &mut jpeg, false)
                .unwrap_err()
                .is::<Malformed>()
        );
        for code in [0, 0xD0, 0xD7] {
            assert!(
                scan(&mut &[0xFF, code][..], &mut jpeg)
                    .unwrap_err()
                    .is::<Malformed>()
            );
            assert_eq!(jpeg.len(), MAX_JPEG - 1);
        }
        jpeg.truncate(MAX_JPEG - 2);
        assert_eq!(
            marker(&mut &[0xFF, 0xD9][..], &mut jpeg, false).unwrap(),
            0xD9
        );
        assert_eq!(jpeg.len(), MAX_JPEG);

        let mut tiny = vec![0xFF];
        assert!(take(&mut &[][..], &mut tiny, usize::MAX).is_err());
        assert_eq!(tiny, [0xFF]);
    }

    #[test]
    fn jpegs_are_cut_from_a_stream_however_it_is_read() {
        let (first, second) = (shaped(0x11), shaped(0x22));
        let stream = [(&*first), (&*second)].concat();
        for capacity in [1, 2, 3, 5, 8, 64, 4096] {
            let (jpegs, end) = cut(&stream, capacity);
            assert_eq!(jpegs, [first.clone(), second.clone()], "{capacity}");
            end.unwrap();
        }
        // Fill bytes before a marker are dropped; bytes between images are
        // skipped.
        let mut filled = shaped(0x33);
        let eoi = filled.len() - 2;
        filled.insert(eoi, 0xFF);
        let noisy = [&[0u8, 0xFF, 2][..], &filled, &[0xFF, 0x00, 1]].concat();
        let (jpegs, end) = cut(&noisy, 3);
        assert_eq!(jpegs, [shaped(0x33)]);
        end.unwrap();
        // A stream that ends inside an image's segments.
        let (jpegs, end) = cut(&first[..20], 4096);
        assert_eq!(jpegs, [] as [Vec<u8>; 0]);
        assert!(end.unwrap_err().is::<Malformed>());
    }

    #[test]
    fn a_jpeg_without_an_eoi_ends_where_the_next_starts() {
        let (first, second, third) = (shaped(0x11), shaped(0x22), shaped(0x33));
        let open = |jpeg: &[u8]| jpeg[..jpeg.len() - 2].to_vec();
        // Its scan runs into the next image's SOI, or to the end of the
        // stream, possibly through the 0xFF of a marker it lacks the rest of.
        let stream = [(&*first), &open(&second), &third, &open(&first)].concat();
        let ended = [(&*open(&second)), &[0xFF]].concat();
        for capacity in [1, 2, 3, 5, 64, 4096] {
            let (jpegs, end) = cut(&stream, capacity);
            assert_eq!(
                jpegs,
                [first.clone(), second.clone(), third.clone(), first.clone()],
                "{capacity}"
            );
            end.unwrap();
            let (jpegs, end) = cut(&ended, capacity);
            assert_eq!(jpegs, std::slice::from_ref(&second), "{capacity}");
            end.unwrap();
        }
        // An image cut short between its segments ends there too.
        let stream = [&first[..18], &second].concat();
        let (jpegs, end) = cut(&stream, 4096);
        assert_eq!(
            jpegs,
            [[&first[..18], &[0xFF, 0xD9]].concat(), second.clone()]
        );
        end.unwrap();
        // One that is not well formed fails, and the stream goes on at the
        // next image.
        let stream = [&[0xFF, 0xD8, 0x12, 0x34][..], &second].concat();
        let mut reader = BufReader::new(&*stream);
        let mut splitter = Splitter::default();
        assert!(splitter.next(&mut reader).unwrap_err().is::<Malformed>());
        assert_eq!(splitter.next(&mut reader).unwrap().unwrap(), second);
        assert!(splitter.next(&mut reader).unwrap().is_none());
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
                    u8::try_from(x * 2 + y + channel * 30).unwrap()
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
                    pixels: &*pixels,
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
                (
                    u32::try_from(width).unwrap(),
                    u32::try_from(height).unwrap(),
                    7
                )
            );
            assert_eq!(
                frame.data.len(),
                crate::frame::frame_len(frame.pixel_format, frame.width, frame.height).unwrap()
            );
            assert_eq!(frame.jpeg.as_deref(), Some(&*jpeg));
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
        decode(&mut decompressor, vec![0xFF, 0xD8, 0xFF, 0xD9], 1, &pool).unwrap_err();
    }
}
