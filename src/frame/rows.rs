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
                        for (x, rgb) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
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

#[cfg(test)]
mod tests {
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
        .filter(|&format| crate::frame::convertible(format))
        .collect()
    }

    pub(crate) fn noise_frame(format: u32, width: u32, height: u32) -> Frame {
        let mut seed = format ^ 0x2545_f491 ^ width;
        let data: Vec<u8> = (0..row_bytes(width, format) * height as usize)
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
}
