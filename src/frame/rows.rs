//! A frame's rows decoded one at a time into what a consumer reads: 8-bit
//! gray or RGB, exactly as `frame::convert` defines them. Packed samples are
//! unpacked a row at a time and wide ones shifted on the way, and a Bayer
//! mosaic of any depth is demosaicked from a ring of three 8-bit rows, so no
//! full-size intermediate image is ever built: a consumer gets each row
//! where it wants it (a preview's accumulator, an encoder's input buffer).
use super::{FrameRef, Packing, bayer_pattern, is_bayer, packing, pixel_at, row_bytes};
use crate::types::{MONO8, RGB8};

/// What a decoded row holds for each pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Channels {
    /// One 8-bit gray sample: the first channel of the RGB conversion,
    /// which monochrome formats repeat in all three.
    Gray,
    Rgb,
}

impl Channels {
    pub(crate) fn bytes(self) -> usize {
        match self {
            Channels::Gray => 1,
            Channels::Rgb => 3,
        }
    }
}

/// How a format's rows are decoded.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// One byte a pixel: the 8-bit monochrome family.
    Gray8,
    Rgb8,
    Bgr8,
    /// Little-endian 16-bit monochrome, shifted right to 8 bits.
    Wide(u32),
    /// Packed monochrome: sample bits, packing, and the shift to 8 bits.
    Packed(u32, Packing, u32),
    /// A Bayer mosaic with its color filter, demosaicked from 8-bit rows.
    Bayer([usize; 4], Mosaic),
    /// Every other layout, decoded a pixel at a time.
    Pixels,
}

/// How a Bayer mosaic's samples become 8-bit.
#[derive(Clone, Copy, Debug)]
enum Mosaic {
    Eight,
    Wide(u32),
    Packed(u32, Packing, u32),
}

fn kind(pixel_format: u32) -> Kind {
    if super::streamed(pixel_format) {
        return Kind::Pixels;
    }
    let unpacked = packing(pixel_format).map_or(pixel_format, |(unpacked, ..)| unpacked);
    if is_bayer(pixel_format) {
        let shift = super::wide_bayer(unpacked).map_or(0, |(_, shift)| shift);
        let mosaic = match packing(pixel_format) {
            Some((_, bits, packing)) => Mosaic::Packed(bits, packing, shift),
            None if shift > 0 => Mosaic::Wide(shift),
            None => Mosaic::Eight,
        };
        return Kind::Bayer(bayer_pattern(pixel_format), mosaic);
    }
    match pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => Kind::Gray8,
        RGB8 => Kind::Rgb8,
        0x0218_0015 => Kind::Bgr8,
        _ => {
            let shift = super::unpacked_shift(unpacked).unwrap_or(8);
            match packing(pixel_format) {
                Some((_, bits, packing)) => Kind::Packed(bits, packing, shift),
                None => Kind::Wide(shift),
            }
        }
    }
}

/// Reads a view's rows in order (any order works; in order is cheapest)
/// with one row of scratch, plus three 8-bit rows for a wide or packed
/// Bayer mosaic. The frame must have passed `validate_source`.
#[derive(Clone)]
pub(crate) struct RowReader<'a> {
    view: FrameRef<'a>,
    kind: Kind,
    /// The frame's row length in bytes.
    stride: usize,
    /// One source row's unpacked samples.
    unpack: Vec<[u8; 2]>,
    /// 8-bit mosaic rows of the view, and which row each holds.
    ring: [Vec<u8>; 3],
    held: [Option<usize>; 3],
    /// A demosaicked row for gray output of a mosaic.
    color: Vec<[u8; 3]>,
    /// The row `row` returns when it cannot borrow the frame's.
    line: Vec<u8>,
}

impl<'a> RowReader<'a> {
    pub(crate) fn new(view: FrameRef<'a>) -> Self {
        let pixel_format = view.pixel_format();
        Self {
            view,
            kind: kind(pixel_format),
            stride: row_bytes(view.frame.width, pixel_format),
            unpack: Vec::new(),
            ring: Default::default(),
            held: [None; 3],
            color: Vec::new(),
            line: Vec::new(),
        }
    }

    /// Row `y` of the view: the frame's own bytes where they already are
    /// what was asked for (8-bit gray, RGB8), else decoded into scratch.
    pub(crate) fn row(&mut self, y: usize, channels: Channels) -> &[u8] {
        let width = self.view.width;
        match (self.kind, channels) {
            (Kind::Gray8, Channels::Gray) => &self.source(y)[self.view.x0..][..width],
            (Kind::Rgb8, Channels::Rgb) => &self.source(y)[3 * self.view.x0..][..3 * width],
            _ => {
                let mut line = std::mem::take(&mut self.line);
                line.resize(width * channels.bytes(), 0);
                self.read(y, channels, &mut line);
                self.line = line;
                &self.line
            }
        }
    }

    /// Whether `row` returns the frame's own bytes for `channels`.
    pub(crate) fn borrows(&self, channels: Channels) -> bool {
        matches!(
            (self.kind, channels),
            (Kind::Gray8, Channels::Gray) | (Kind::Rgb8, Channels::Rgb)
        )
    }

    /// The frame's row `y` of the view, whole.
    fn source(&self, y: usize) -> &'a [u8] {
        let at = (self.view.y0 + y) * self.stride;
        &self.view.frame.data[at..at + self.stride]
    }

    /// Row `y` decoded into `out`: `width * channels.bytes()` bytes.
    pub(crate) fn read(&mut self, y: usize, channels: Channels, out: &mut [u8]) {
        let (x0, width) = (self.view.x0, self.view.width);
        match self.kind {
            Kind::Gray8 => gray(&self.source(y)[x0..][..width], channels, out),
            Kind::Rgb8 => {
                let source = &self.source(y)[3 * x0..][..3 * width];
                match channels {
                    Channels::Rgb => out.copy_from_slice(source),
                    Channels::Gray => {
                        for (value, pixel) in out.iter_mut().zip(source.as_chunks::<3>().0) {
                            *value = pixel[0];
                        }
                    }
                }
            }
            Kind::Bgr8 => {
                let source = self.source(y)[3 * x0..][..3 * width].as_chunks::<3>().0;
                match channels {
                    Channels::Rgb => {
                        for (rgb, bgr) in out.as_chunks_mut::<3>().0.iter_mut().zip(source) {
                            *rgb = [bgr[2], bgr[1], bgr[0]];
                        }
                    }
                    Channels::Gray => {
                        for (value, bgr) in out.iter_mut().zip(source) {
                            *value = bgr[2];
                        }
                    }
                }
            }
            Kind::Wide(shift) => {
                let source = self.source(y)[2 * x0..][..2 * width].as_chunks::<2>().0;
                shifted(source, shift, channels, out);
            }
            Kind::Packed(bits, packing, shift) => {
                let samples = self.unpacked(y, bits, packing);
                shifted(&samples[x0..][..width], shift, channels, out);
            }
            Kind::Bayer(pattern, mosaic) => self.demosaic(y, pattern, mosaic, channels, out),
            Kind::Pixels => {
                let view = self.view;
                match channels {
                    Channels::Rgb => {
                        let out = out.as_chunks_mut::<3>().0;
                        if super::formats::decode_row(view.frame, x0, view.y0 + y, out) {
                            return;
                        }
                        for (x, rgb) in out.iter_mut().enumerate() {
                            *rgb = pixel_at(view, x, y);
                        }
                    }
                    Channels::Gray => {
                        for (x, value) in out.iter_mut().enumerate() {
                            *value = pixel_at(view, x, y)[0];
                        }
                    }
                }
            }
        }
    }

    /// Every sample of the frame's row `y` of the view, unpacked.
    fn unpacked(&mut self, y: usize, bits: u32, packing: Packing) -> &[[u8; 2]] {
        let source = self.source(y);
        self.unpack.resize(self.view.frame.width as usize, [0; 2]);
        super::unpack_row(source, bits, packing, &mut self.unpack);
        &self.unpack
    }

    /// Make the ring hold the 8-bit mosaic row `y`, unless the frame's own
    /// row already is one.
    fn hold(&mut self, y: usize, mosaic: Mosaic) {
        let (x0, width) = (self.view.x0, self.view.width);
        let slot = y % 3;
        if matches!(mosaic, Mosaic::Eight) || self.held[slot] == Some(y) {
            return;
        }
        let mut row = std::mem::take(&mut self.ring[slot]);
        row.resize(width, 0);
        match mosaic {
            Mosaic::Eight => unreachable!(),
            Mosaic::Wide(shift) => {
                let source = self.source(y)[2 * x0..][..2 * width].as_chunks::<2>().0;
                shifted(source, shift, Channels::Gray, &mut row);
            }
            Mosaic::Packed(bits, packing, shift) => {
                let samples = self.unpacked(y, bits, packing);
                shifted(&samples[x0..][..width], shift, Channels::Gray, &mut row);
            }
        }
        self.ring[slot] = row;
        self.held[slot] = Some(y);
    }

    /// The 8-bit mosaic row `y`, after `hold`.
    fn mosaic_row(&self, y: usize, mosaic: Mosaic) -> &[u8] {
        match mosaic {
            Mosaic::Eight => &self.source(y)[self.view.x0..][..self.view.width],
            _ => &self.ring[y % 3],
        }
    }

    fn demosaic(
        &mut self,
        y: usize,
        pattern: [usize; 4],
        mosaic: Mosaic,
        channels: Channels,
        out: &mut [u8],
    ) {
        let height = self.view.height;
        for row in y.saturating_sub(1)..(y + 2).min(height) {
            self.hold(row, mosaic);
        }
        let mut color = std::mem::take(&mut self.color);
        let above = (y > 0).then(|| self.mosaic_row(y - 1, mosaic));
        let below = (y + 1 < height).then(|| self.mosaic_row(y + 1, mosaic));
        let row = self.mosaic_row(y, mosaic);
        match channels {
            Channels::Rgb => demosaic_row(above, row, below, pattern, y, out.as_chunks_mut().0),
            Channels::Gray => {
                color.resize(self.view.width, [0; 3]);
                demosaic_row(above, row, below, pattern, y, &mut color);
                for (value, rgb) in out.iter_mut().zip(&color) {
                    *value = rgb[0];
                }
            }
        }
        self.color = color;
    }
}

/// 8-bit gray samples as `channels`.
fn gray(samples: &[u8], channels: Channels, out: &mut [u8]) {
    match channels {
        Channels::Gray => out.copy_from_slice(samples),
        Channels::Rgb => {
            for (rgb, &value) in out.as_chunks_mut::<3>().0.iter_mut().zip(samples) {
                *rgb = [value; 3];
            }
        }
    }
}

/// Little-endian 16-bit samples shifted right to 8 bits, as `channels`.
fn shifted(samples: &[[u8; 2]], shift: u32, channels: Channels, out: &mut [u8]) {
    let eight = |sample: &[u8; 2]| (u16::from_le_bytes(*sample) >> shift).min(255) as u8;
    match channels {
        Channels::Gray => {
            for (value, sample) in out.iter_mut().zip(samples) {
                *value = eight(sample);
            }
        }
        Channels::Rgb => {
            for (rgb, sample) in out.as_chunks_mut::<3>().0.iter_mut().zip(samples) {
                *rgb = [eight(sample); 3];
            }
        }
    }
}

/// Row `y` of an 8-bit Bayer mosaic demosaicked bilinearly into `out`, from
/// the row and the ones above and below it where the image has them. Each
/// missing color is the mean of its in-bounds neighbors of that color
/// (`bounded`); the interior's vectorizable form gives the same values.
pub(crate) fn demosaic_row(
    above: Option<&[u8]>,
    row: &[u8],
    below: Option<&[u8]>,
    pattern: [usize; 4],
    y: usize,
    out: &mut [[u8; 3]],
) {
    let w = row.len();
    let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
    let bounded = |x: usize| bounded(above, row, below, pattern, x, y);
    let (Some(up), Some(down)) = (above, below) else {
        for (x, pixel) in out.iter_mut().enumerate() {
            *pixel = bounded(x);
        }
        return;
    };
    if w < 3 {
        for (x, pixel) in out.iter_mut().enumerate() {
            *pixel = bounded(x);
        }
        return;
    }
    (out[0], out[w - 1]) = (bounded(0), bounded(w - 1));
    let chroma = color(0, y) + color(1, y) - 1;
    let windows = up.windows(3).zip(row.windows(3)).zip(down.windows(3));
    for (x, ((up, row), down)) in windows.enumerate() {
        let horizontal = row[0] as u16 + row[2] as u16;
        let vertical = up[1] as u16 + down[1] as u16;
        let (near, green, far) = if color(x + 1, y) == 1 {
            ((horizontal >> 1) as u8, row[1], (vertical >> 1) as u8)
        } else {
            let diagonal = up[0] as u16 + up[2] as u16 + down[0] as u16 + down[2] as u16;
            (
                row[1],
                ((horizontal + vertical) >> 2) as u8,
                (diagonal >> 2) as u8,
            )
        };
        out[x + 1] = if chroma == 0 {
            [near, green, far]
        } else {
            [far, green, near]
        };
    }
}

/// Bilinear demosaic of one pixel using only in-bounds neighbors of each
/// missing color; the reference rule the vectorized interior must match.
fn bounded(
    above: Option<&[u8]>,
    row: &[u8],
    below: Option<&[u8]>,
    pattern: [usize; 4],
    x: usize,
    y: usize,
) -> [u8; 3] {
    let w = row.len();
    let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
    let rows = [
        above.map(|row| (y - 1, row)),
        Some((y, row)),
        below.map(|row| (y + 1, row)),
    ];
    std::array::from_fn(|c| {
        if color(x, y) == c {
            return row[x];
        }
        let (mut sum, mut n) = (0u32, 0u32);
        for (yy, line) in rows.iter().flatten() {
            for xx in x.saturating_sub(1)..(x + 2).min(w) {
                if color(xx, *yy) == c {
                    sum += u32::from(line[xx]);
                    n += 1;
                }
            }
        }
        sum.checked_div(n).map_or(row[x], |v| v as u8)
    })
}

/// The whole view decoded into `out`, `channels.bytes()` a pixel with rows
/// packed, split into row blocks across cores for large images.
pub(crate) fn decode(view: FrameRef<'_>, channels: Channels, out: &mut [u8]) {
    let line = view.width * channels.bytes();
    debug_assert_eq!(out.len(), line * view.height);
    if line == 0 {
        return;
    }
    let threads = super::worker_threads(view.pixels());
    let block = view.height.div_ceil(threads).max(1);
    let run = |first: usize, rows: &mut [u8]| {
        let mut reader = RowReader::new(view);
        for (i, out) in rows.chunks_exact_mut(line).enumerate() {
            reader.read(first + i, channels, out);
        }
    };
    if threads > 1 {
        std::thread::scope(|scope| {
            for (i, rows) in out.chunks_mut(block * line).enumerate() {
                let run = &run;
                scope.spawn(move || run(i * block, rows));
            }
        });
    } else {
        run(0, out);
    }
}

/// The whole view mapped pixel by pixel from its RGB conversion into
/// `out`, split into row blocks across cores for large images.
pub(crate) fn map<P: Send>(
    view: FrameRef<'_>,
    out: &mut [P],
    pixel: &(impl Fn([u8; 3]) -> P + Sync),
) {
    let width = view.width;
    debug_assert_eq!(out.len(), view.pixels());
    if width == 0 {
        return;
    }
    let threads = super::worker_threads(view.pixels());
    let block = view.height.div_ceil(threads).max(1);
    let run = |first: usize, rows: &mut [P]| {
        let mut reader = RowReader::new(view);
        let mut rgb = vec![0; 3 * width];
        for (i, out) in rows.chunks_exact_mut(width).enumerate() {
            reader.read(first + i, Channels::Rgb, &mut rgb);
            for (value, source) in out.iter_mut().zip(rgb.as_chunks::<3>().0) {
                *value = pixel(*source);
            }
        }
    };
    if threads > 1 {
        std::thread::scope(|scope| {
            for (i, rows) in out.chunks_mut(block * width).enumerate() {
                let run = &run;
                scope.spawn(move || run(i * block, rows));
            }
        });
    } else {
        run(0, out);
    }
}

#[cfg(any(feature = "jpeg", jpeg_helper))]
/// Where `ycbcr420` writes JFIF (full-range BT.601) Y'CbCr 4:2:0: a luma
/// plane and the chroma either as two planes (as libjpeg-turbo reads them)
/// or interleaved (NV12, as a video encoder's surface holds it), each plane
/// at its own row stride.
pub(crate) struct Yuv420<'a> {
    pub(crate) luma: &'a mut [u8],
    pub(crate) luma_stride: usize,
    pub(crate) chroma: Chroma<'a>,
}

#[cfg(any(feature = "jpeg", jpeg_helper))]
/// A 4:2:0 image's chroma planes.
pub(crate) enum Chroma<'a> {
    // libjpeg-turbo and nvJPEG read planar chroma.
    #[cfg_attr(
        not(any(feature = "jpeg", all(jpeg_helper, feature = "nvjpeg"))),
        allow(dead_code)
    )]
    Planar {
        cb: &'a mut [u8],
        cr: &'a mut [u8],
        stride: usize,
    },
    // Only Apple's JPEG engine and VA-API write NV12 surfaces.
    #[cfg_attr(
        not(any(
            all(feature = "videotoolbox", target_os = "macos"),
            all(jpeg_helper, feature = "vaapi")
        )),
        allow(dead_code)
    )]
    Interleaved { cbcr: &'a mut [u8], stride: usize },
}

#[cfg(any(feature = "jpeg", jpeg_helper))]
/// One block of chroma rows of a `Yuv420` image, for one thread.
enum ChromaRows<'a> {
    Planar(&'a mut [u8], &'a mut [u8], usize),
    Interleaved(&'a mut [u8], usize),
}

#[cfg(any(feature = "jpeg", jpeg_helper))]
/// `view` as JFIF Y'CbCr 4:2:0, written straight into `out` from its RGB
/// rows two at a time: luma for each pixel and chroma from each 2x2 block's
/// mean, with the fixed-point coefficients of libjpeg's jccolor.c. An odd
/// last column or row repeats its own pixels. Large views are split into
/// blocks of row pairs across cores.
pub(crate) fn ycbcr420(view: FrameRef<'_>, out: Yuv420<'_>) {
    let (width, height) = (view.width, view.height);
    let (chroma_width, chroma_rows) = (width.div_ceil(2), height.div_ceil(2));
    if width == 0 || height == 0 {
        return;
    }
    let Yuv420 {
        luma,
        luma_stride,
        chroma,
    } = out;
    assert!(luma_stride >= width && luma.len() >= (height - 1) * luma_stride + width);
    let threads = super::worker_threads(view.pixels());
    let block = chroma_rows.div_ceil(threads).max(1);
    let run = |first: usize, luma: &mut [u8], mut chroma: ChromaRows<'_>| {
        let mut reader = RowReader::new(view);
        let mut rows = [vec![0; 3 * width], vec![0; 3 * width]];
        let pairs = block.min(chroma_rows - first);
        for pair in 0..pairs {
            let y = 2 * (first + pair);
            let [top, bottom] = &mut rows;
            reader.read(y, Channels::Rgb, top);
            let two = y + 1 < height;
            if two {
                reader.read(y + 1, Channels::Rgb, bottom);
            }
            let bottom = if two { &*bottom } else { &*top };
            let luma = &mut luma[2 * pair * luma_stride..];
            write_luma(top, &mut luma[..width]);
            if two {
                write_luma(bottom, &mut luma[luma_stride..][..width]);
            }
            let blocks = top.chunks(6).zip(bottom.chunks(6));
            match &mut chroma {
                ChromaRows::Planar(cb, cr, stride) => {
                    let (cb, cr) = (&mut cb[pair * *stride..], &mut cr[pair * *stride..]);
                    for (x, (t, b)) in blocks.enumerate() {
                        [cb[x], cr[x]] = chroma_of(t, b);
                    }
                }
                ChromaRows::Interleaved(cbcr, stride) => {
                    let cbcr = &mut cbcr[pair * *stride..][..2 * chroma_width];
                    for (out, (t, b)) in cbcr.as_chunks_mut::<2>().0.iter_mut().zip(blocks) {
                        *out = chroma_of(t, b);
                    }
                }
            }
        }
    };
    let luma_block = 2 * block * luma_stride;
    match chroma {
        Chroma::Planar { cb, cr, stride } => {
            assert!(
                stride >= chroma_width
                    && cb.len().min(cr.len()) >= (chroma_rows - 1) * stride + chroma_width
            );
            let blocks = luma.chunks_mut(luma_block).zip(
                cb.chunks_mut(block * stride)
                    .zip(cr.chunks_mut(block * stride)),
            );
            std::thread::scope(|scope| {
                for (i, (luma, (cb, cr))) in blocks.enumerate().take(threads) {
                    let run = &run;
                    let rows = ChromaRows::Planar(cb, cr, stride);
                    if threads == 1 {
                        run(0, luma, rows);
                    } else {
                        scope.spawn(move || run(i * block, luma, rows));
                    }
                }
            });
        }
        Chroma::Interleaved { cbcr, stride } => {
            assert!(
                stride >= 2 * chroma_width
                    && cbcr.len() >= (chroma_rows - 1) * stride + 2 * chroma_width
            );
            let blocks = luma
                .chunks_mut(luma_block)
                .zip(cbcr.chunks_mut(block * stride));
            std::thread::scope(|scope| {
                for (i, (luma, cbcr)) in blocks.enumerate().take(threads) {
                    let run = &run;
                    let rows = ChromaRows::Interleaved(cbcr, stride);
                    if threads == 1 {
                        run(0, luma, rows);
                    } else {
                        scope.spawn(move || run(i * block, luma, rows));
                    }
                }
            });
        }
    }
}

#[cfg(any(feature = "jpeg", jpeg_helper))]
/// JFIF luma of each RGB pixel of `rgb`.
fn write_luma(rgb: &[u8], out: &mut [u8]) {
    for (y, p) in out.iter_mut().zip(rgb.as_chunks::<3>().0) {
        let [r, g, b] = p.map(u32::from);
        *y = ((19_595 * r + 38_470 * g + 7_471 * b + 32_768) >> 16) as u8;
    }
}

#[cfg(any(feature = "jpeg", jpeg_helper))]
/// JFIF Cb and Cr of the 2x2 block whose top and bottom halves are `top`
/// and `bottom` (one or two RGB pixels each), from the block's mean.
fn chroma_of(top: &[u8], bottom: &[u8]) -> [u8; 2] {
    // An odd final column repeats its own pixel.
    let last = top.len() - 3;
    let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
    for p in [&top[..3], &top[last..], &bottom[..3], &bottom[last..]] {
        (r, g, b) = (
            r + i32::from(p[0]),
            g + i32::from(p[1]),
            b + i32::from(p[2]),
        );
    }
    // Four summed pixels: shift by 18 and round, offset by 128.
    let scale = |v: i32| ((v + (128 << 18) + (1 << 17)) >> 18).clamp(0, 255) as u8;
    [
        scale(-11_059 * r - 21_709 * g + 32_768 * b),
        scale(32_768 * r - 27_439 * g - 5_329 * b),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::Frame;

    /// Every layout `convert` decodes, as the golden test enumerates them.
    pub(crate) fn convertible_formats() -> Vec<u32> {
        [
            0x0101, 0x0102, 0x0104, 0x0108, 0x010a, 0x010c, 0x010e, 0x0110, 0x020c, 0x0210, 0x0218,
            0x021e, 0x0220, 0x0224, 0x0228, 0x0230, 0x0240,
        ]
        .into_iter()
        .flat_map(|prefix| (0..=0x12e).map(move |id| prefix << 16 | id))
        // Capturefab's own planar and semiplanar Y'CbCr codes.
        .chain(
            [0x820c, 0x8210, 0x8218]
                .into_iter()
                .flat_map(|prefix| [1, 2, 3, 4, 0x11, 0x12, 0x13].map(move |id| prefix << 16 | id)),
        )
        .filter(|&format| crate::frame::convertible(format))
        .collect()
    }

    pub(crate) fn noise_frame(format: u32, width: u32, height: u32) -> Frame {
        let mut seed = format ^ 0x2545_f491 ^ width;
        let len = crate::frame::frame_len(format, width, height).unwrap();
        let data: Vec<u8> = (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        Frame {
            width,
            height,
            pixel_format: format,
            data: data.into(),
            ..Frame::default()
        }
    }

    #[test]
    fn rows_decode_every_layout_as_its_pixels_do() {
        for format in convertible_formats() {
            for (width, height) in [(12, 8), (13, 7), (2, 2), (1, 5)] {
                let frame = noise_frame(format, width, height);
                if crate::frame::validate_source(&frame).is_err() {
                    continue;
                }
                let regions = [
                    [0, 0, width, height],
                    [2, 2, width.saturating_sub(2), height.saturating_sub(2)],
                    [0, 2, 3, 3],
                ];
                for rect in regions {
                    let view = FrameRef::region(&frame, rect);
                    if view.width == 0 || view.height == 0 {
                        continue;
                    }
                    let mut reader = RowReader::new(view);
                    for y in 0..view.height {
                        let expected: Vec<u8> =
                            (0..view.width).flat_map(|x| pixel_at(view, x, y)).collect();
                        assert_eq!(
                            reader.row(y, Channels::Rgb),
                            expected,
                            "{format:08x} {width}x{height} {rect:?} row {y}"
                        );
                        let gray: Vec<u8> = expected.iter().step_by(3).copied().collect();
                        assert_eq!(reader.row(y, Channels::Gray), gray, "{format:08x} gray");
                    }
                }
            }
        }
    }

    #[test]
    fn rows_read_backwards_and_out_of_order_as_in_order() {
        let frame = noise_frame(0x010c_0059, 16, 10);
        let view = FrameRef::whole(&frame);
        let mut forward = RowReader::new(view);
        let rows: Vec<Vec<u8>> = (0..10)
            .map(|y| forward.row(y, Channels::Rgb).to_vec())
            .collect();
        let mut backward = RowReader::new(view);
        for y in (0..10).rev().chain([3, 7, 0, 9]) {
            assert_eq!(backward.row(y, Channels::Rgb), rows[y], "row {y}");
        }
    }

    #[test]
    fn large_frames_decode_the_same_in_blocks_across_cores() {
        // Above `worker_threads`' half megapixel, so blocks run in parallel.
        for format in [
            0x010c_0059,
            0x0108_0009,
            0x0110_0011,
            0x010c_0047,
            RGB8,
            0x0210_0032,
        ] {
            let width = 1031;
            let frame = noise_frame(format, width as u32, 520);
            let view = FrameRef::whole(&frame);
            let mut whole = vec![0; view.pixels() * 3];
            decode(view, Channels::Rgb, &mut whole);
            let mut reader = RowReader::new(view);
            for (y, row) in whole.chunks_exact(3 * width).enumerate() {
                assert_eq!(row, reader.row(y, Channels::Rgb), "{format:08x} row {y}");
            }
            let mut mapped = vec![[0u8; 4]; view.pixels()];
            map(view, &mut mapped, &|[r, g, b]| [r, g, b, 255]);
            let rgba: Vec<u8> = whole
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|&[r, g, b]| [r, g, b, 255])
                .collect();
            assert_eq!(mapped.as_flattened(), rgba, "{format:08x}");
        }
    }

    #[test]
    fn only_eight_bit_gray_and_rgb8_rows_are_borrowed() {
        for (format, channels, borrowed) in [
            (MONO8, Channels::Gray, true),
            (MONO8, Channels::Rgb, false),
            (RGB8, Channels::Rgb, true),
            (RGB8, Channels::Gray, false),
            (0x0108_0009, Channels::Rgb, false),
            (0x0110_0005, Channels::Gray, false),
        ] {
            let frame = noise_frame(format, 8, 4);
            let mut reader = RowReader::new(FrameRef::region(&frame, [2, 1, 4, 2]));
            assert_eq!(reader.borrows(channels), borrowed, "{format:08x}");
            let row = reader.row(1, channels).as_ptr();
            let inside = frame.data.as_ptr_range().contains(&row);
            assert_eq!(inside, borrowed, "{format:08x}");
        }
    }

    /// JFIF 4:2:0 from RGB rows, as libjpeg-turbo's planes and as an NV12
    /// surface, at odd sizes and with rows wider than the image.
    #[test]
    #[cfg(feature = "jpeg")]
    fn ycbcr420_matches_jfif_at_any_size_and_stride() {
        let jfif = |[r, g, b]: [f64; 3]| {
            [
                0.299 * r + 0.587 * g + 0.114 * b,
                128.0 - 0.168736 * r - 0.331264 * g + 0.5 * b,
                128.0 + 0.5 * r - 0.418688 * g - 0.081312 * b,
            ]
        };
        // Odd sizes repeat their edges; 1031x517 is split across threads.
        for (width, height) in [(1, 1), (2, 2), (3, 1), (5, 7), (33, 17), (1031, 517)] {
            let rgb: Vec<[u8; 3]> = (0..width * height)
                .map(|i| {
                    [
                        (i * 7) as u8,
                        (i * 13 + i / width) as u8,
                        (255 - i % 256) as u8,
                    ]
                })
                .collect();
            let frame = Frame {
                width: width as u32,
                height: height as u32,
                pixel_format: RGB8,
                data: rgb.as_flattened().to_vec().into(),
                ..Frame::default()
            };
            let at = |x: usize, y: usize| {
                rgb[y.min(height - 1) * width + x.min(width - 1)].map(f64::from)
            };
            let (chroma_width, chroma_rows) = (width.div_ceil(2), height.div_ceil(2));
            let luma_stride = width + 3;
            for interleaved in [false, true] {
                // Each row ends in padding the writer must leave alone.
                let mut luma = vec![7; luma_stride * height];
                let (used, chroma_stride) = if interleaved {
                    (2 * chroma_width, 2 * chroma_width + 6)
                } else {
                    (chroma_width, chroma_width + 3)
                };
                let mut planes = [
                    vec![7; chroma_stride * chroma_rows],
                    vec![
                        7;
                        if interleaved {
                            0
                        } else {
                            chroma_stride * chroma_rows
                        }
                    ],
                ];
                let [first, second] = &mut planes;
                let chroma = if interleaved {
                    Chroma::Interleaved {
                        cbcr: first,
                        stride: chroma_stride,
                    }
                } else {
                    Chroma::Planar {
                        cb: first,
                        cr: second,
                        stride: chroma_stride,
                    }
                };
                ycbcr420(
                    FrameRef::whole(&frame),
                    Yuv420 {
                        luma: &mut luma,
                        luma_stride,
                        chroma,
                    },
                );
                let case = format!("{width}x{height} interleaved {interleaved}");
                for (y, row) in luma.chunks(luma_stride).enumerate() {
                    for (x, &value) in row[..width].iter().enumerate() {
                        let expected = jfif(at(x, y))[0];
                        assert!(
                            (f64::from(value) - expected).abs() <= 0.51,
                            "{case}: Y {x},{y}"
                        );
                    }
                    assert!(row[width..].iter().all(|&v| v == 7), "{case}: row {y}");
                }
                for y in 0..chroma_rows {
                    let row = y * chroma_stride;
                    for x in 0..chroma_width {
                        let mean = [(0, 0), (1, 0), (0, 1), (1, 1)]
                            .map(|(dx, dy)| jfif(at(2 * x + dx, 2 * y + dy)))
                            .iter()
                            .fold([0.0; 2], |s, p| [s[0] + p[1] / 4.0, s[1] + p[2] / 4.0]);
                        let (cb, cr) = if interleaved {
                            (planes[0][row + 2 * x], planes[0][row + 2 * x + 1])
                        } else {
                            (planes[0][row + x], planes[1][row + x])
                        };
                        for (component, got) in [cb, cr].into_iter().enumerate() {
                            assert!(
                                (f64::from(got) - mean[component].clamp(0.0, 255.0)).abs() <= 0.51,
                                "{case}: C{component} {x},{y}"
                            );
                        }
                    }
                    for plane in planes.iter().filter(|plane| !plane.is_empty()) {
                        let padding = &plane[row + used..row + chroma_stride];
                        assert!(padding.iter().all(|&v| v == 7), "{case}: chroma row {y}");
                    }
                }
            }
        }
    }
}
