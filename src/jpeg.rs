//! JPEG stills, encoded in process by libjpeg-turbo's SIMD encoder (`jpeg`
//! feature). Frames libjpeg-turbo reads (RGB8, BGR8, Mono8) are encoded in
//! place, and the JPEG stays in the buffer libjpeg-turbo wrote it to.
#![deny(clippy::missing_const_for_fn)]
use crate::{
    frame::{Body, Samples},
    types::{Frame, MONO8},
};
use anyhow::Result;
#[cfg(not(feature = "jpeg"))]
use anyhow::bail;

pub(crate) mod backend;
#[cfg(jpeg_helper)]
pub(crate) mod helper;
#[cfg(all(jpeg_helper, feature = "nvjpeg"))]
pub(crate) mod nvjpeg;
#[cfg(all(jpeg_helper, feature = "vaapi"))]
pub(crate) mod vaapi;
#[cfg(all(feature = "videotoolbox", target_os = "macos"))]
pub(crate) mod videotoolbox;

/// Fixed quality and 4:2:0 chroma (gray for monochrome), so the same frame
/// produces comparable files wherever it is encoded.
pub(crate) const QUALITY: i32 = 90;

pub(crate) fn monochrome(pixel_format: u32) -> bool {
    matches!(
        crate::frame::unpacked_format(pixel_format),
        MONO8 | 0x0110_0003 | 0x0110_0005 | 0x0110_0025 | 0x0110_0007
    )
}

/// 8-bit pixels ready for compression: one gray channel or interleaved RGB,
/// borrowed where the frame already holds them, else converted once into a
/// pooled buffer.
pub struct Raster<'a> {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub data: Samples<'a>,
}

impl<'a> Raster<'a> {
    /// Prepare a frame as a gray or RGB raster.
    ///
    /// # Errors
    /// Returns an error for invalid or oversized image dimensions, an unsupported
    /// pixel format, truncated source bytes, or a missing monochrome conversion.
    pub fn new(frame: &'a Frame) -> Result<Self> {
        let (channels, data) = if monochrome(frame.pixel_format) {
            // One pass straight to 8-bit gray; no RGB triplication.
            let gray = crate::frame::gray8_samples(frame)?
                .ok_or_else(|| anyhow::anyhow!("monochrome frame has no gray conversion"))?;
            (1, gray)
        } else {
            (3, crate::frame::rgb_samples(frame)?)
        };
        if data.converted() {
            crate::copies::record(crate::copies::Site::StillTransform, data.len());
        }
        Ok(Self {
            width: frame.width,
            height: frame.height,
            channels,
            data,
        })
    }
    /// A raster of samples already in hand, such as a preview.
    pub(crate) const fn of(width: u32, height: u32, channels: usize, data: &'a [u8]) -> Self {
        Self {
            width,
            height,
            channels,
            data: Samples::Borrowed(data),
        }
    }
}

/// A JPEG in the buffer libjpeg-turbo allocated for it.
#[cfg(feature = "jpeg")]
pub struct Jpeg(turbojpeg::OwnedBuf);

// SAFETY: the buffer is plain memory from tj3Alloc, owned by this value
// alone; tj3Free may release it on any thread.
#[cfg(feature = "jpeg")]
unsafe impl Send for Jpeg {}
// SAFETY: shared references only read the buffer.
#[cfg(feature = "jpeg")]
unsafe impl Sync for Jpeg {}
#[cfg(feature = "jpeg")]
impl std::ops::Deref for Jpeg {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}
#[cfg(feature = "jpeg")]
impl Clone for Jpeg {
    fn clone(&self) -> Self {
        Self(turbojpeg::OwnedBuf::copy_from_slice(self))
    }
}

/// `frame` at `quality` (1–100). libjpeg-turbo reads RGB8, BGR8 and Mono8
/// frames in place. Other monochrome formats are converted to 8-bit gray,
/// and other color formats straight to the `Y'CbCr` 4:2:0 planes JPEG stores
/// (`encode_planes`).
#[cfg(feature = "jpeg")]
pub(crate) fn encode(frame: &Frame, quality: i32) -> Result<Body<'static>> {
    use turbojpeg::PixelFormat;
    let (format, channels) = match frame.pixel_format {
        crate::types::RGB8 => (PixelFormat::RGB, 3),
        0x0218_0015 => (PixelFormat::BGR, 3),
        MONO8 => (PixelFormat::GRAY, 1),
        other if monochrome(other) => return software_quality(&Raster::new(frame)?, quality),
        JFIF_420 => return compress_jfif(frame, quality),
        _ => return encode_planes(frame, quality),
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
        quality,
    )
}
#[cfg(not(feature = "jpeg"))]
pub(crate) fn encode(frame: &Frame, quality: i32) -> Result<Body<'static>> {
    software_quality(&Raster::new(frame)?, quality)
}

/// Planar full-range BT.601 `Y'CbCr` 4:2:0, as MJPEG and full-range video
/// decode to: the very planes a JPEG stores.
#[cfg(feature = "jpeg")]
const JFIF_420: u32 = 0x820c_0001;

/// A frame of JFIF `Y'CbCr` 4:2:0 planes compressed from its own buffer; one
/// of odd size goes through `encode_planes`, which pads its luma.
#[cfg(feature = "jpeg")]
fn compress_jfif(frame: &Frame, quality: i32) -> Result<Body<'static>> {
    crate::frame::validate_source(frame)?;
    let (width, height) = (frame.width as usize, frame.height as usize);
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return encode_planes(frame, quality);
    }
    let (luma, chroma) = frame.data.split_at(width * height);
    let (cb, cr) = chroma.split_at(width * height / 4);
    compress_yuv(
        luma,
        cb,
        &cr[..width * height / 4],
        width,
        height,
        width,
        quality,
    )
}

/// libjpeg-turbo compressing `Y'CbCr` 4:2:0 planes, luma `luma_stride` wide.
#[cfg(feature = "jpeg")]
fn compress_yuv(
    luma: &[u8],
    cb: &[u8],
    cr: &[u8],
    width: usize,
    height: usize,
    luma_stride: usize,
    quality: i32,
) -> Result<Body<'static>> {
    anyhow::ensure!((1..=100).contains(&quality), "JPEG quality must be 1..100");
    let mut compressor = turbojpeg::Compressor::new()?;
    compressor.set_quality(quality)?;
    let jpeg = compressor.compress_yuv_planes_to_owned(&turbojpeg::YuvPlanesImage {
        y_plane: luma,
        u_plane: cb,
        v_plane: cr,
        width,
        height,
        y_stride: luma_stride,
        u_stride: luma_stride / 2,
        v_stride: luma_stride / 2,
        subsamp: turbojpeg::Subsamp::Sub2x2,
    })?;
    crate::copies::record(crate::copies::Site::JpegEncode, jpeg.len());
    Ok(Body::Jpeg(Jpeg(jpeg)))
}

/// A color frame as JFIF `Y'CbCr` 4:2:0 planes written straight from its rows
/// into one pooled buffer (1.5 bytes a pixel, where RGB would take 3) and
/// compressed without libjpeg-turbo's own color conversion and
/// downsampling, which the rows' conversion does across cores: a 12 MP
/// Bayer frame encodes in about 17.5 ms instead of 19.4 through RGB.
#[cfg(feature = "jpeg")]
fn encode_planes(frame: &Frame, quality: i32) -> Result<Body<'static>> {
    crate::frame::validate_source(frame)?;
    let view = crate::frame::FrameRef::whole(frame);
    let layout = Planes::of(view.width, view.height);
    let mut planes = crate::framebuf::transforms().take_stale(layout.len());
    layout.write(view, &mut planes);
    crate::copies::record(crate::copies::Site::StillTransform, planes.len());
    let (luma, chroma) = planes.split_at(layout.luma_len());
    let (cb, cr) = chroma.split_at(layout.chroma_len());
    compress_yuv(
        luma,
        cb,
        cr,
        view.width,
        view.height,
        layout.luma_width,
        quality,
    )
}

/// The layout of a frame's JFIF `Y'CbCr` 4:2:0 planes in one buffer, as
/// libjpeg-turbo and nvJPEG read them: luma in whole 2x2 blocks (an odd
/// last column or row repeated into the padding, as libjpeg-turbo pads RGB
/// itself), then Cb, then Cr.
#[cfg(any(feature = "jpeg", all(jpeg_helper, feature = "nvjpeg")))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Planes {
    pub(crate) luma_width: usize,
    pub(crate) luma_rows: usize,
}

#[cfg(any(feature = "jpeg", all(jpeg_helper, feature = "nvjpeg")))]
impl Planes {
    pub(crate) const fn of(width: usize, height: usize) -> Self {
        Self {
            luma_width: width.next_multiple_of(2),
            luma_rows: height.next_multiple_of(2),
        }
    }
    pub(crate) const fn luma_len(&self) -> usize {
        self.luma_width * self.luma_rows
    }
    pub(crate) const fn chroma_width(&self) -> usize {
        self.luma_width / 2
    }
    pub(crate) const fn chroma_len(&self) -> usize {
        self.chroma_width() * self.luma_rows / 2
    }
    pub(crate) const fn len(&self) -> usize {
        self.luma_len() + 2 * self.chroma_len()
    }
    /// `view`'s planes, written from its rows into `planes`.
    pub(crate) fn write(&self, view: crate::frame::FrameRef<'_>, planes: &mut [u8]) {
        use crate::frame::{Chroma, Yuv420};
        let (width, height) = (view.width, view.height);
        let (luma, chroma) = planes[..self.len()].split_at_mut(self.luma_len());
        let (cb, cr) = chroma.split_at_mut(self.chroma_len());
        crate::frame::ycbcr420(
            view,
            Yuv420 {
                luma: &mut *luma,
                luma_stride: self.luma_width,
                chroma: Chroma::Planar {
                    cb,
                    cr,
                    stride: self.chroma_width(),
                },
            },
        );
        if width < self.luma_width {
            for row in luma.chunks_exact_mut(self.luma_width).take(height) {
                row[width] = row[width - 1];
            }
        }
        if height < self.luma_rows {
            luma.copy_within(
                (height - 1) * self.luma_width..height * self.luma_width,
                height * self.luma_width,
            );
        }
    }
}

/// Compress a raster in software at the default JPEG quality.
///
/// # Errors
/// Returns an error if JPEG support is unavailable, the raster is invalid,
/// or the software encoder cannot initialize or compress it.
#[cfg(any(test, feature = "testing"))]
pub fn software(raster: &Raster<'_>) -> Result<Body<'static>> {
    software_quality(raster, QUALITY)
}

/// libjpeg-turbo at a chosen quality (1–100), from a converted raster: for
/// previews, and for stills of formats `encode` cannot read in place.
#[cfg(feature = "jpeg")]
pub(crate) fn software_quality(raster: &Raster<'_>, quality: i32) -> Result<Body<'static>> {
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
) -> Result<Body<'static>> {
    use turbojpeg::{Compressor, Image, PixelFormat, Subsamp};
    anyhow::ensure!((1..=100).contains(&quality), "JPEG quality must be 1..100");
    // A compressor per image: a reused one encodes large images about a
    // tenth slower.
    let mut compressor = Compressor::new()?;
    compressor.set_quality(quality)?;
    compressor.set_subsamp(if format == PixelFormat::GRAY {
        Subsamp::Gray
    } else {
        Subsamp::Sub2x2
    })?;
    let jpeg = compressor.compress_to_owned(Image {
        pixels,
        width: width as usize,
        pitch: width as usize * format.size(),
        height: height as usize,
        format,
    })?;
    crate::copies::record(crate::copies::Site::JpegEncode, jpeg.len());
    Ok(Body::Jpeg(Jpeg(jpeg)))
}

#[cfg(not(feature = "jpeg"))]
pub(crate) fn software_quality(_: &Raster<'_>, _: i32) -> Result<Body<'static>> {
    bail!("JPEG is unsupported in this build; rebuild with --features jpeg or capture png")
}

/// A complete baseline JPEG: SOI, a frame header with the expected size,
/// EOI. A hardware engine's output must be one before it is kept.
pub(crate) fn well_formed(bytes: &[u8], width: u32, height: u32) -> bool {
    if bytes.len() < 4 || bytes[..2] != [0xFF, 0xD8] || bytes[bytes.len() - 2..] != [0xFF, 0xD9] {
        return false;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() && bytes[i] == 0xFF {
        let marker = bytes[i + 1];
        let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // Baseline, extended or progressive Huffman frame header.
        if matches!(marker, 0xC0..=0xC2) {
            return bytes
                .get(i + 5..)
                .and_then(<[u8]>::first_chunk::<4>)
                .is_some_and(|size| {
                    u32::from(u16::from_be_bytes([size[0], size[1]])) == height
                        && u32::from(u16::from_be_bytes([size[2], size[3]])) == width
                });
        }
        if marker == 0xDA || length < 2 {
            return false;
        }
        i += 2 + length;
    }
    false
}

/// Whether a JPEG has Huffman tables of its own before its first scan.
/// Motion JPEG from many webcams leaves them out (AVI1) for the standard
/// ones, which a file read alone must carry.
pub(crate) fn has_huffman_tables(jpeg: &[u8]) -> bool {
    let mut i = 2;
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        match jpeg[i + 1] {
            0xC4 => return true,
            0xDA => return false,
            // A fill byte before a marker.
            0xFF => i += 1,
            _ => {
                let length = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
                if length < 2 {
                    return false;
                }
                i += 2 + length;
            }
        }
    }
    false
}

/// The typical Huffman tables of ITU-T T.81 annex K.3: code counts for each
/// length, then the values.
pub(crate) mod huffman {
    pub(crate) const DC_LUMINANCE: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
    pub(crate) const DC_CHROMINANCE: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
    pub(crate) const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    pub(crate) const AC_LUMINANCE: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
    pub(crate) const AC_LUMINANCE_VALUES: [u8; 162] = [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
    pub(crate) const AC_CHROMINANCE: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
    pub(crate) const AC_CHROMINANCE_VALUES: [u8; 162] = [
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
}

/// The typical Huffman tables of ITU-T T.81 annex K.3 as one DHT segment:
/// what a JPEG without tables of its own is decoded with.
pub(crate) fn standard_huffman_tables() -> Vec<u8> {
    use huffman::{
        AC_CHROMINANCE, AC_CHROMINANCE_VALUES, AC_LUMINANCE, AC_LUMINANCE_VALUES, DC_CHROMINANCE,
        DC_LUMINANCE, DC_VALUES,
    };
    // Table class and destination, then the code counts and values.
    let tables: [(u8, &[u8], &[u8]); 4] = [
        (0x00, &DC_LUMINANCE, &DC_VALUES),
        (0x10, &AC_LUMINANCE, &AC_LUMINANCE_VALUES),
        (0x01, &DC_CHROMINANCE, &DC_VALUES),
        (0x11, &AC_CHROMINANCE, &AC_CHROMINANCE_VALUES),
    ];
    let length = 2 + tables
        .iter()
        .map(|(_, counts, values)| 1 + counts.len() + values.len())
        .sum::<usize>();
    let mut segment = vec![0xFF, 0xC4];
    segment.extend_from_slice(
        &u16::try_from(length)
            .expect("the four fixed Huffman tables fit a JPEG segment")
            .to_be_bytes(),
    );
    for (class, counts, values) in tables {
        segment.push(class);
        segment.extend_from_slice(counts);
        segment.extend_from_slice(values);
    }
    segment
}

/// Where a COM (comment) segment goes in an encoded JPEG: after SOI and any
/// leading `APPn` segments, so a JFIF or EXIF header stays first. It works on
/// the finished bytes, so the comment does not depend on which encoder ran.
pub(crate) fn com_offset(jpeg: &[u8]) -> Result<usize> {
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
    Ok(at)
}
/// A COM segment holding `payload`.
pub(crate) fn com_segment(payload: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        payload.len() <= usize::from(u16::MAX) - 2,
        "JPEG comment is too long"
    );
    let mut segment = Vec::with_capacity(payload.len() + 4);
    segment.extend_from_slice(&[0xFF, 0xFE]);
    let length = u16::try_from(payload.len() + 2)?;
    segment.extend_from_slice(&length.to_be_bytes());
    segment.extend_from_slice(payload);
    Ok(segment)
}
/// The JPEG with a COM segment holding `payload` in its place, in one
/// buffer; stills write the two as parts instead.
///
/// # Errors
/// Returns an error if the payload exceeds a JPEG comment segment
/// or the JPEG header cannot be parsed to find the insertion position.
#[cfg(any(test, feature = "testing"))]
pub fn insert_com(jpeg: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    let segment = com_segment(payload)?;
    let at = com_offset(jpeg)?;
    Ok([&jpeg[..at], &segment, &jpeg[at..]].concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comment_segments_preserve_the_full_wire_length() {
        let mut payload = vec![0x41; usize::from(u16::MAX) - 2];
        let segment = com_segment(&payload).unwrap();
        assert_eq!(&segment[..2], &[0xFF, 0xFE]);
        assert_eq!(u16::from_be_bytes([segment[2], segment[3]]), u16::MAX);
        assert_eq!(&segment[4..], payload);
        payload.push(0x41);
        com_segment(&payload).unwrap_err();
    }

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
        insert_com(&jpeg, &vec![0; 65_534]).unwrap_err();
        insert_com(b"\x89PNG", b"x").unwrap_err();
        insert_com(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 40, 0xFF, 0xD9], b"x").unwrap_err();
    }
    use crate::types::RGB8;
    fn frame(pixel_format: u32, width: u32, height: u32, bytes: usize) -> Frame {
        Frame {
            id: 1,
            width,
            height,
            pixel_format,
            device_ts_ns: None,
            wall_ms: 0,
            data: (0..width as usize * height as usize * bytes)
                .map(|i| (i * 7 + i / 64).to_le_bytes()[0])
                .collect(),
            jpeg: None,
        }
    }
    #[test]
    fn rasters_are_gray_for_mono_and_rgb_otherwise() {
        let source = frame(0x0110_0005, 5, 3, 2);
        let mono = Raster::new(&source).unwrap();
        assert_eq!((mono.channels, mono.data.len()), (1, 15));
        let full = crate::frame::rgb(&frame(0x0110_0005, 5, 3, 2)).unwrap();
        assert!(
            mono.data
                .iter()
                .zip(full.chunks(3))
                .all(|(g, p)| *g == p[0])
        );
        let mosaic = frame(0x0108_0009, 6, 4, 1);
        let bayer = Raster::new(&mosaic).unwrap();
        assert_eq!((bayer.channels, bayer.data.len()), (3, 72));
        // 8-bit gray and RGB are the frame's own bytes.
        for (format, bytes) in [(MONO8, 1), (RGB8, 3)] {
            let source = frame(format, 6, 4, bytes);
            let raster = Raster::new(&source).unwrap();
            assert_eq!(raster.data.as_ptr(), source.data.as_ptr(), "{format:x}");
        }
        // Mono14 and packed Mono12p stay gray; packed BayerRG12p is color.
        for (format, bits) in [(0x0110_0025, 16usize), (0x010c_0047, 12), (0x010c_0059, 12)] {
            let size = (6 * 4 * bits).div_ceil(8);
            let packed = Frame {
                data: (0..size).map(|i| (i * 37).to_le_bytes()[0]).collect(),
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
            // Converted to RGB, or copied, as every frame once was.
            let samples = if format == MONO8 {
                frame.data.to_vec()
            } else {
                crate::frame::rgb(&frame).unwrap()
            };
            let copied = Raster::of(97, 61, bytes, &samples);
            for quality in [QUALITY, 75, 100] {
                assert_eq!(
                    encode(&frame, quality).unwrap(),
                    software_quality(&copied, quality).unwrap(),
                    "{format:x} at {quality}"
                );
            }
            let mut short = frame.clone();
            short.data.make_mut().pop();
            assert!(encode(&short, QUALITY).is_err(), "{format:x}");
        }
    }
    #[cfg(feature = "jpeg")]
    #[test]
    fn software_encoding_produces_valid_gray_and_color_jpegs() {
        for (format, bytes, width, height) in [(MONO8, 1, 33, 17), (RGB8, 3, 64, 48)] {
            let source = frame(format, width, height, bytes);
            let raster = Raster::new(&source).unwrap();
            let jpeg = software(&raster).unwrap();
            assert!(well_formed(&jpeg, width, height), "{format:x}");
            assert!(!well_formed(&jpeg, width + 1, height));
            // Component count in SOF0: 1 for gray, 3 for color.
            let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
            assert_eq!(jpeg[sof + 9], u8::try_from(raster.channels).unwrap());
        }
    }
    #[cfg(feature = "jpeg")]
    #[test]
    fn lower_quality_makes_smaller_jpegs_and_out_of_range_quality_fails() {
        let source = frame(RGB8, 64, 48, 3);
        let raster = Raster::new(&source).unwrap();
        let fine = software_quality(&raster, 92).unwrap();
        let coarse = software_quality(&raster, 65).unwrap();
        assert!(well_formed(&coarse, 64, 48));
        assert!(coarse.len() < fine.len());
        assert_eq!(
            software(&raster).unwrap(),
            software_quality(&raster, QUALITY).unwrap()
        );
        for quality in [0, 101] {
            software_quality(&raster, quality).unwrap_err();
        }
    }

    /// Color stills compressed from `Y'CbCr` planes look as the RGB path's
    /// did: within a fraction of a decibel of the frame, at every size.
    #[cfg(feature = "jpeg")]
    #[test]
    fn planes_encode_color_frames_as_faithfully_as_rgb() {
        let psnr = |a: &[u8], b: &[u8]| {
            let mse = a
                .iter()
                .zip(b)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum::<f64>()
                / a.len() as f64;
            10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10()
        };
        let decoded = |jpeg: &[u8]| {
            let image = turbojpeg::decompress(jpeg, turbojpeg::PixelFormat::RGB).unwrap();
            (image.width, image.height, image.pixels)
        };
        for (format, width, height) in [
            (0x0108_0009, 64, 48),
            (0x0108_0009, 63, 47),
            (0x010c_0059, 97, 61),
            (0x0110_0011, 33, 1),
            (0x0210_0041, 64, 36),
            (0x0230_001a, 1, 9),
            (0x0218_0021, 31, 17),
        ] {
            // A smooth scene in every layout: its RGB conversion is the
            // reference both paths are measured against.
            let mut frame = crate::frame::noise_frame(format, width, height);
            let smooth: Vec<u8> = frame
                .data
                .iter()
                .enumerate()
                .map(|(i, _)| u8::try_from((i * 3) / 7 % 256).unwrap())
                .collect();
            frame.data = smooth.into();
            let source = crate::frame::rgb(&frame).unwrap();
            let planes = encode_planes(&frame, QUALITY).unwrap();
            assert!(
                well_formed(&planes, width, height),
                "{format:08x} {width}x{height}"
            );
            let rgb = software_quality(&Raster::of(width, height, 3, &source), QUALITY).unwrap();
            let (w, h, from_planes) = decoded(&planes);
            assert_eq!((w, h), (width as usize, height as usize));
            let (_, _, from_rgb) = decoded(&rgb);
            let (ours, theirs) = (psnr(&from_planes, &source), psnr(&from_rgb, &source));
            assert!(
                ours > theirs - 0.5,
                "{format:08x} {width}x{height}: {ours:.2} dB against {theirs:.2} dB"
            );
            assert_eq!(encode(&frame, QUALITY).unwrap(), planes, "{format:08x}");
        }
    }

    /// Full-range planar 4:2:0, as MJPEG decodes, is compressed from the
    /// frame's own planes; odd sizes go through padded planes.
    #[cfg(feature = "jpeg")]
    #[test]
    fn jfif_planes_compress_as_they_are() {
        for (width, height) in [(64u32, 48u32), (63, 47)] {
            let (w, h) = (width as usize, height as usize);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut data: Vec<u8> = (0..w * h)
                .map(|i| u8::try_from(i % w * 255 / w).unwrap())
                .collect();
            data.extend((0..cw * ch).map(|i| u8::try_from(100 + i % cw).unwrap()));
            data.extend((0..cw * ch).map(|i| u8::try_from(150 - i / cw).unwrap()));
            let frame = Frame {
                width,
                height,
                pixel_format: JFIF_420,
                data: data.into(),
                ..Frame::default()
            };
            let jpeg = encode(&frame, QUALITY).unwrap();
            assert!(well_formed(&jpeg, width, height), "{width}x{height}");
            let decoded = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::RGB).unwrap();
            let source = crate::frame::rgb(&frame).unwrap();
            let mean = decoded
                .pixels
                .iter()
                .zip(&source)
                .map(|(&a, &b)| f64::from(a.abs_diff(b)))
                .sum::<f64>()
                / source.len() as f64;
            assert!(mean < 1.5, "{width}x{height}: mean difference {mean:.2}");
        }
    }
}
