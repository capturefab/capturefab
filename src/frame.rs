//! Bounded PFNC image conversion and portable output without image codecs from the OS.
use crate::types::{Frame, MONO8, RGB8};
use anyhow::{Result, bail, ensure};
use std::io::Write;

pub fn pixel_format_name(format: u32) -> &'static str {
    match format {
        MONO8 => "Mono8",
        RGB8 => "RGB8",
        0x0218_0015 => "BGR8",
        0x0110_0003 => "Mono10",
        0x0110_0005 => "Mono12",
        0x0110_0007 => "Mono16",
        0x0108_00b1 => "Coord3D_C8",
        0x0110_00b8 => "Coord3D_C16",
        0x0120_00bf => "Coord3D_C32f",
        0x0108_00c6 => "Confidence8",
        0x0110_00c7 => "Confidence16",
        0x0108_0008 => "BayerGR8",
        0x0108_0009 => "BayerRG8",
        0x0108_000a => "BayerGB8",
        0x0108_000b => "BayerBG8",
        _ => "Unknown",
    }
}
fn gray(out: &mut [u8], value: impl Fn(usize) -> u8) {
    for i in 0..out.len() / 3 {
        let v = value(i);
        (out[3 * i], out[3 * i + 1], out[3 * i + 2]) = (v, v, v);
    }
}
pub fn rgb(frame: &Frame) -> Result<Vec<u8>> {
    let pixels = checked_pixels(frame)?;
    // Flat byte stores vectorize as interleaved 3-byte stores, which `[u8; 3]`
    // elements from the generic converter do not; only Bayer is shared.
    let mut out = vec![0; pixels * 3];
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => {
            ensure!(frame.data.len() >= pixels, "truncated Mono8 frame");
            let data = &frame.data[..pixels];
            gray(&mut out, |i| data[i]);
        }
        RGB8 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            out.copy_from_slice(&frame.data[..pixels * 3]);
        }
        0x0218_0015 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let bgr = frame.data.as_chunks::<3>().0;
            for (dst, p) in out.as_chunks_mut::<3>().0.iter_mut().zip(bgr) {
                *dst = [p[2], p[1], p[0]];
            }
        }
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            ensure!(
                frame.data.len() >= pixels * 2,
                "truncated unpacked monochrome frame"
            );
            let shift = match frame.pixel_format {
                0x0110_0003 => 2,
                0x0110_0005 => 4,
                _ => 8,
            };
            let data = &frame.data[..pixels * 2];
            gray(&mut out, |i| {
                (u16::from_le_bytes([data[2 * i], data[2 * i + 1]]) >> shift).min(255) as u8
            });
        }
        _ => return Ok(convert(frame, |p| p)?.into_flattened()),
    }
    Ok(out)
}
/// Opaque RGBA bytes, converted in one pass without an intermediate RGB image.
pub fn rgba(frame: &Frame) -> Result<Vec<u8>> {
    Ok(convert(frame, |[r, g, b]| [r, g, b, 255])?.into_flattened())
}
pub(crate) fn checked_pixels(frame: &Frame) -> Result<usize> {
    let pixels = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .ok_or_else(|| anyhow::anyhow!("image size overflow"))?;
    ensure!(
        pixels > 0 && pixels <= 64 * 1024 * 1024,
        "image dimensions exceed conversion limit"
    );
    Ok(pixels)
}
/// Convert a PFNC frame into any pixel representation (RGB, RGBA, a GUI color
/// type) with a single allocation; `pixel` maps each interpolated RGB triple.
pub fn convert<P: Copy + Default + Send>(
    frame: &Frame,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<Vec<P>> {
    let mut out = Vec::new();
    convert_into(frame, &mut out, pixel)?;
    Ok(out)
}
/// `convert` into a reused buffer, which is replaced by the converted image;
/// repeated conversions of one frame size then allocate nothing.
pub fn convert_into<P: Copy + Default + Send>(
    frame: &Frame,
    out: &mut Vec<P>,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    out.clear();
    convert_to(frame, out, pixel)
}
/// `convert` into memory the caller already owns, exactly one pixel per
/// element, such as a buffer shared with an accelerator, so the image is
/// written where it is consumed instead of being copied there.
pub fn convert_slice<P: Copy + Default + Send>(
    frame: &Frame,
    out: &mut [P],
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    ensure!(
        out.len() == checked_pixels(frame)?,
        "conversion buffer does not match the image size"
    );
    convert_to(frame, out, pixel)
}
/// Where `convert_to` writes: an empty growable buffer, or a slice of exactly
/// the image size.
trait Sink<'a, P: 'a> {
    /// Pixels in order.
    fn stream(self, pixels: impl Iterator<Item = P>);
    /// Storage for `len` pixels written out of order.
    fn prefilled(self, len: usize) -> &'a mut [P];
}
impl<'a, P: Copy + Default + 'a> Sink<'a, P> for &'a mut Vec<P> {
    // Extending from a slice iterator knows its exact length, so it neither
    // pre-fills nor bounds-checks.
    fn stream(self, pixels: impl Iterator<Item = P>) {
        self.extend(pixels);
    }
    fn prefilled(self, len: usize) -> &'a mut [P] {
        self.resize(len, P::default());
        self
    }
}
impl<'a, P: 'a> Sink<'a, P> for &'a mut [P] {
    fn stream(self, pixels: impl Iterator<Item = P>) {
        for (dst, p) in self.iter_mut().zip(pixels) {
            *dst = p;
        }
    }
    fn prefilled(self, _: usize) -> &'a mut [P] {
        self
    }
}
fn convert_to<'a, P: Copy + Default + Send + 'a>(
    frame: &Frame,
    out: impl Sink<'a, P>,
    pixel: impl Fn([u8; 3]) -> P + Sync,
) -> Result<()> {
    let pixels = checked_pixels(frame)?;
    // Streaming formats map source to output in order.
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => {
            ensure!(frame.data.len() >= pixels, "truncated Mono8 frame");
            out.stream(frame.data[..pixels].iter().map(|&v| pixel([v, v, v])));
        }
        RGB8 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let source = frame.data[..pixels * 3].as_chunks::<3>().0;
            out.stream(source.iter().map(|p| pixel([p[0], p[1], p[2]])));
        }
        0x0218_0015 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let source = frame.data[..pixels * 3].as_chunks::<3>().0;
            out.stream(source.iter().map(|p| pixel([p[2], p[1], p[0]])));
        }
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            ensure!(
                frame.data.len() >= pixels * 2,
                "truncated unpacked monochrome frame"
            );
            let shift = match frame.pixel_format {
                0x0110_0003 => 2,
                0x0110_0005 => 4,
                _ => 8,
            };
            let source = frame.data[..pixels * 2].as_chunks::<2>().0;
            out.stream(source.iter().map(|&p| {
                let v = (u16::from_le_bytes(p) >> shift).min(255) as u8;
                pixel([v, v, v])
            }));
        }
        0x0108_0008..=0x0108_000b => {
            ensure!(frame.data.len() >= pixels, "truncated Bayer8 frame");
            // Interpolation writes edges and interiors out of order.
            let out = out.prefilled(pixels);
            let pattern = bayer_pattern(frame.pixel_format);
            let (w, h, data) = (frame.width as usize, frame.height as usize, &frame.data);
            let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
            let bounded = |x: usize, y: usize| pixel(bayer_bounded(frame, pattern, x, y));
            let demosaic_row = |y: usize, line: &mut [P]| {
                if y == 0 || y + 1 == h || w < 3 {
                    for (x, px) in line.iter_mut().enumerate() {
                        *px = bounded(x, y);
                    }
                    return;
                }
                (line[0], line[w - 1]) = (bounded(0, y), bounded(w - 1, y));
                let chroma = color(0, y) + color(1, y) - 1;
                let rows = |dy: usize| data[(y + dy - 1) * w..(y + dy) * w].windows(3);
                for (x, ((up, row), down)) in rows(0).zip(rows(1)).zip(rows(2)).enumerate() {
                    let horizontal = row[0] as u16 + row[2] as u16;
                    let vertical = up[1] as u16 + down[1] as u16;
                    let (near, green, far) = if color(x + 1, y) == 1 {
                        ((horizontal >> 1) as u8, row[1], (vertical >> 1) as u8)
                    } else {
                        let diagonal =
                            up[0] as u16 + up[2] as u16 + down[0] as u16 + down[2] as u16;
                        (
                            row[1],
                            ((horizontal + vertical) >> 2) as u8,
                            (diagonal >> 2) as u8,
                        )
                    };
                    line[x + 1] = pixel(if chroma == 0 {
                        [near, green, far]
                    } else {
                        [far, green, near]
                    });
                }
            };
            // Rows are independent (each reads three input rows), so large
            // frames are split into row blocks across cores.
            let threads = worker_threads(pixels);
            let block = h.div_ceil(threads).max(1) * w;
            if threads > 1 {
                std::thread::scope(|scope| {
                    for (i, rows) in out.chunks_mut(block).enumerate() {
                        let demosaic_row = &demosaic_row;
                        scope.spawn(move || {
                            for (j, line) in rows.chunks_exact_mut(w).enumerate() {
                                demosaic_row(i * block / w + j, line);
                            }
                        });
                    }
                });
            } else {
                for (y, line) in out.chunks_exact_mut(w).enumerate() {
                    demosaic_row(y, line);
                }
            }
        }
        v => {
            bail!("unsupported PFNC format 0x{v:08x}; capture with --format raw to preserve bytes")
        }
    }
    Ok(())
}

/// Threads for CPU-bound per-pixel work: one below half a megapixel, where
/// spawning costs more than it saves, else `spare_cores`.
pub(crate) fn worker_threads(pixels: usize) -> usize {
    if pixels < 512 * 1024 {
        return 1;
    }
    spare_cores(std::thread::available_parallelism().map_or(1, |n| n.get()))
}
/// Up to eight of a computer's `cores`, always leaving one for the camera
/// workers and the window.
fn spare_cores(cores: usize) -> usize {
    cores.saturating_sub(1).clamp(1, 8)
}
fn is_bayer(pixel_format: u32) -> bool {
    matches!(pixel_format, 0x0108_0008..=0x0108_000b)
}
/// A Bayer frame read as whole 2x2 quads, one color each: red, the mean of
/// both greens, blue. As good as a demosaic for statistics over many samples
/// and several times cheaper.
struct Quads<'a> {
    data: &'a [u8],
    width: usize,
    /// See `quad_order`.
    order: [usize; 4],
    columns: usize,
    rows: usize,
}
impl<'a> Quads<'a> {
    /// None for other formats, or a mosaic without a whole quad. The frame
    /// must have passed `validated_bytes_per_pixel`.
    fn of(frame: &'a Frame) -> Option<Self> {
        let (width, height) = (frame.width as usize, frame.height as usize);
        (is_bayer(frame.pixel_format) && width >= 2 && height >= 2).then(|| Self {
            data: &frame.data,
            width,
            order: quad_order(bayer_pattern(frame.pixel_format)),
            columns: width / 2,
            rows: height / 2,
        })
    }
    fn color(&self, x: usize, y: usize) -> [u8; 3] {
        let (i, w, data) = (2 * y * self.width + 2 * x, self.width, self.data);
        let quad = [data[i], data[i + 1], data[i + w], data[i + w + 1]];
        let [red, green, other, blue] = self.order;
        let greens = u16::from(quad[green]) + u16::from(quad[other]);
        [quad[red], ((greens + 1) >> 1) as u8, quad[blue]]
    }
}
/// Where red, the two greens and blue sit among a quad's samples, the top
/// row's two first.
fn quad_order(pattern: [usize; 4]) -> [usize; 4] {
    let at = |channel, nth| {
        let mut found = (0..4).filter(|&i| pattern[i] == channel);
        found.nth(nth).unwrap_or(0)
    };
    [at(0, 0), at(1, 0), at(1, 1), at(2, 0)]
}
/// Color filter layout as the channel (0 R, 1 G, 2 B) at (x & 1, y & 1),
/// indexed `[(y & 1) * 2 + (x & 1)]`.
pub fn bayer_pattern(pixel_format: u32) -> [usize; 4] {
    match pixel_format {
        0x0108_0008 => [1, 0, 2, 1],
        0x0108_0009 => [0, 1, 1, 2],
        0x0108_000a => [1, 2, 0, 1],
        _ => [2, 1, 1, 0],
    }
}
/// Bilinear demosaic of one pixel using only in-bounds neighbors of each
/// missing color; the reference rule the vectorized interior must match.
fn bayer_bounded(frame: &Frame, pattern: [usize; 4], x: usize, y: usize) -> [u8; 3] {
    let (w, h, data) = (frame.width as usize, frame.height as usize, &frame.data);
    let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
    std::array::from_fn(|c| {
        if color(x, y) == c {
            return data[y * w + x];
        }
        let (mut sum, mut n) = (0u32, 0u32);
        for yy in y.saturating_sub(1)..(y + 2).min(h) {
            for xx in x.saturating_sub(1)..(x + 2).min(w) {
                if color(xx, yy) == c {
                    sum += data[yy * w + xx] as u32;
                    n += 1;
                }
            }
        }
        sum.checked_div(n).map_or(data[y * w + x], |v| v as u8)
    })
}
/// Bytes per pixel of a convertible format, after validating dimensions and
/// that the frame holds a complete image.
pub(crate) fn validated_bytes_per_pixel(frame: &Frame) -> Result<usize> {
    let pixels = checked_pixels(frame)?;
    let (bytes, name) = match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => (1, "8-bit scalar"),
        RGB8 | 0x0218_0015 => (3, "RGB"),
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            (2, "unpacked monochrome")
        }
        0x0108_0008..=0x0108_000b => (1, "Bayer8"),
        v => {
            bail!("unsupported PFNC format 0x{v:08x}; capture with --format raw to preserve bytes")
        }
    };
    ensure!(frame.data.len() >= pixels * bytes, "truncated {name} frame");
    Ok(bytes)
}
/// Whether `rgb`/`convert` can decode this pixel format.
pub fn convertible(pixel_format: u32) -> bool {
    matches!(
        pixel_format,
        MONO8
            | 0x0108_00b1
            | 0x0108_00c6
            | RGB8
            | 0x0218_0015
            | 0x0110_0003
            | 0x0110_0005
            | 0x0110_0007
            | 0x0110_00b8
            | 0x0110_00c7
            | 0x0108_0008..=0x0108_000b
    )
}
/// One pixel exactly as `convert` would produce it; for sparse sampling when
/// the full image is converted elsewhere (for example on the GPU).
/// The frame must have passed `validated_bytes_per_pixel`.
fn pixel_at(frame: &Frame, x: usize, y: usize) -> [u8; 3] {
    let i = y * frame.width as usize + x;
    let data = &frame.data;
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => [data[i]; 3],
        RGB8 => [data[3 * i], data[3 * i + 1], data[3 * i + 2]],
        0x0218_0015 => [data[3 * i + 2], data[3 * i + 1], data[3 * i]],
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => {
            let shift = match frame.pixel_format {
                0x0110_0003 => 2,
                0x0110_0005 => 4,
                _ => 8,
            };
            [(u16::from_le_bytes([data[2 * i], data[2 * i + 1]]) >> shift).min(255) as u8; 3]
        }
        _ => bayer_bounded(frame, bayer_pattern(frame.pixel_format), x, y),
    }
}
/// 8-bit luminance of an RGB pixel, with the weights the histograms use.
fn luma([r, g, b]: [u8; 3]) -> usize {
    (r as usize * 54 + g as usize * 183 + b as usize * 19) >> 8
}

/// A sample at or below this in every channel is crushed to black.
const CRUSHED: u8 = 1;
/// A sample at or above this in any channel is blown out.
const BLOWN: u8 = 254;

/// How a frame is exposed, from at most 65,536 evenly strided pixels (whole
/// quads of a Bayer mosaic) decoded individually so no full RGB image is
/// needed.
#[derive(Clone, Debug, PartialEq)]
pub struct Exposure {
    /// 64-bin luminance histogram.
    pub luma: [u32; 64],
    /// Red, green and blue histograms; None for monochrome formats.
    pub channels: Option<[[u32; 64]; 3]>,
    /// Mean luminance, 0–1.
    pub mean: f32,
    /// Shares of samples crushed to black and blown out, 0–1.
    pub shadows: f32,
    pub highlights: f32,
}

pub fn exposure(frame: &Frame) -> Result<Exposure> {
    validated_bytes_per_pixel(frame)?;
    let (w, pixels) = (frame.width as usize, checked_pixels(frame)?);
    let mono = matches!(
        frame.pixel_format,
        MONO8
            | 0x0108_00b1
            | 0x0108_00c6
            | 0x0110_0003
            | 0x0110_0005
            | 0x0110_0007
            | 0x0110_00b8
            | 0x0110_00c7
    );
    let (mut luma_bins, mut channels) = ([0; 64], [[0; 64]; 3]);
    let (mut sum, mut samples, mut shadows, mut highlights) = (0u64, 0u32, 0u32, 0u32);
    let mut add = |rgb: [u8; 3]| {
        let y = luma(rgb);
        luma_bins[y >> 2] += 1;
        for (bins, value) in channels.iter_mut().zip(rgb) {
            bins[value as usize >> 2] += 1;
        }
        sum += y as u64;
        samples += 1;
        shadows += u32::from(rgb.iter().all(|&v| v <= CRUSHED));
        highlights += u32::from(rgb.iter().any(|&v| v >= BLOWN));
    };
    match Quads::of(frame) {
        Some(quads) => {
            let count = quads.columns * quads.rows;
            for i in (0..count).step_by((count / 65_536).max(1)) {
                add(quads.color(i % quads.columns, i / quads.columns));
            }
        }
        None => {
            for i in (0..pixels).step_by((pixels / 65_536).max(1)) {
                add(pixel_at(frame, i % w, i / w));
            }
        }
    }
    let share = |count: u32| count as f32 / samples.max(1) as f32;
    Ok(Exposure {
        luma: luma_bins,
        channels: (!mono).then_some(channels),
        mean: sum as f32 / samples.max(1) as f32 / 255.0,
        shadows: share(shadows),
        highlights: share(highlights),
    })
}

/// How sharp a region is by four common focus measures, each larger when
/// sharper, in 8-bit luminance units. Absolute values depend on the scene;
/// compare them while focusing on one subject.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct Sharpness {
    /// Variance of the 4-neighbour Laplacian.
    pub laplacian: f32,
    /// Tenengrad: mean squared Sobel gradient magnitude.
    pub tenengrad: f32,
    /// Brenner: mean squared difference between pixels two apart.
    pub brenner: f32,
    /// Normalized variance: luminance variance over its mean.
    pub variance: f32,
}

/// Sharpness of the `[x, y, width, height]` pixel region, clamped to the frame.
/// At most 65,536 evenly spaced pixels are measured, each against its
/// full-resolution neighbours, so fine detail still counts in large regions.
/// A Bayer mosaic is measured over its 2x2 quads, the finest whole colors it has.
pub fn sharpness(frame: &Frame, region: [u32; 4]) -> Result<Sharpness> {
    validated_bytes_per_pixel(frame)?;
    checked_pixels(frame)?;
    if let Some(quads) = Quads::of(frame).filter(|q| q.columns >= 3 && q.rows >= 3) {
        let [x, y, w, h] = region;
        let region = [
            x / 2,
            y / 2,
            (x % 2 + w).div_ceil(2),
            (y % 2 + h).div_ceil(2),
        ];
        return Ok(sharpness_over(
            (quads.columns, quads.rows),
            region,
            |x, y| luma(quads.color(x, y)) as f64,
        ));
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    ensure!(w >= 3 && h >= 3, "frame is too small to measure sharpness");
    Ok(sharpness_over((w, h), region, |x, y| {
        luma(pixel_at(frame, x, y)) as f64
    }))
}
/// `sharpness` of a region of a `w` × `h` grid, at least 3 × 3, whose
/// luminance at (x, y) is `at`.
fn sharpness_over(
    (w, h): (usize, usize),
    region: [u32; 4],
    at: impl Fn(usize, usize) -> f64,
) -> Sharpness {
    let [x, y, width, height] = region.map(|v| v as usize);
    // Keep a one-pixel border so every 3×3 neighbourhood is inside the frame.
    let x0 = x.clamp(1, w - 2);
    let y0 = y.clamp(1, h - 2);
    let x1 = x.saturating_add(width).clamp(x0 + 1, w - 1);
    let y1 = y.saturating_add(height).clamp(y0 + 1, h - 1);
    let area = (x1 - x0) as f64 * (y1 - y0) as f64;
    let step = (area / 65_536.0).sqrt().ceil().max(1.0) as usize;
    let (mut n, mut lap, mut lap2, mut ten, mut bren, mut sum, mut sum2) =
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    // Shift each row and column of the sampling grid by a different phase
    // within the stride, so periodic detail (a one-pixel checkerboard or
    // stripes) is never sampled at only one of its phases.
    let grid = (0..(y1 - y0).div_ceil(step))
        .flat_map(|row| (0..(x1 - x0).div_ceil(step)).map(move |column| (row, column)));
    for (row, column) in grid {
        let (x, y) = (
            x0 + column * step + row % step,
            y0 + row * step + column % step,
        );
        if x >= x1 || y >= y1 {
            continue;
        }
        {
            let [a, b, c] = [at(x - 1, y - 1), at(x, y - 1), at(x + 1, y - 1)];
            let [d, e, f] = [at(x - 1, y), at(x, y), at(x + 1, y)];
            let [g, k, i] = [at(x - 1, y + 1), at(x, y + 1), at(x + 1, y + 1)];
            let laplacian = b + d + f + k - 4.0 * e;
            let gx = (c + 2.0 * f + i) - (a + 2.0 * d + g);
            let gy = (g + 2.0 * k + i) - (a + 2.0 * b + c);
            n += 1.0;
            lap += laplacian;
            lap2 += laplacian * laplacian;
            ten += gx * gx + gy * gy;
            bren += ((f - d).powi(2) + (k - b).powi(2)) / 2.0;
            sum += e;
            sum2 += e * e;
        }
    }
    let mean = sum / n;
    let variance = (sum2 / n - mean * mean).max(0.0);
    Sharpness {
        laplacian: (lap2 / n - (lap / n).powi(2)).max(0.0) as f32,
        tenengrad: (ten / n) as f32,
        brenner: (bren / n) as f32,
        variance: if mean > 0.0 { variance / mean } else { 0.0 } as f32,
    }
}
/// RGBA of at most `max_width` × `max_height` for display: the frame itself
/// when it fits, else `shrink` by the smallest factor that does, so every
/// pixel averages what it covers.
pub fn preview_rgba(frame: &Frame, max_width: u32, max_height: u32) -> Result<(u32, u32, Vec<u8>)> {
    let bounds = (max_width.max(1), max_height.max(1));
    let small = shrink_to(frame, bounds);
    let (width, height, rgba) = sampled(small.as_ref().unwrap_or(frame), bounds)?;
    Ok((width, height, rgba))
}
/// A preview of at most `max_width` × `max_height` that keeps one pixel, or
/// one Bayer quad, every few rather than averaging them. Auto capture
/// descriptors are computed from this, so sessions learned earlier stay
/// comparable; anything shown uses `preview_rgba`.
pub fn sampled_rgba(frame: &Frame, max_width: u32, max_height: u32) -> Result<(u32, u32, Vec<u8>)> {
    let (max_width, max_height) = (max_width.max(1), max_height.max(1));
    // Sample complete Bayer quads so every retained pixel keeps its CFA phase.
    // Count quads rather than pixels to keep odd requested bounds bounded too.
    let factor = if is_bayer(frame.pixel_format) {
        (frame.width / 2)
            .div_ceil((max_width / 2).max(1))
            .max((frame.height / 2).div_ceil((max_height / 2).max(1)))
    } else {
        frame
            .width
            .div_ceil(max_width)
            .max(frame.height.div_ceil(max_height))
    };
    let small = (factor > 1)
        .then(|| decimate(frame, factor as usize))
        .flatten();
    sampled(small.as_ref().unwrap_or(frame), (max_width, max_height))
}
/// `source` as RGBA, taking every few pixels of its conversion when it is
/// still larger than `bounds`: one-pixel bounds, or a Bayer source without a
/// whole quad, cannot be reduced before converting.
fn sampled(source: &Frame, (max_width, max_height): (u32, u32)) -> Result<(u32, u32, Vec<u8>)> {
    let step = source
        .width
        .div_ceil(max_width)
        .max(source.height.div_ceil(max_height));
    if step <= 1 {
        return Ok((source.width, source.height, rgba(source)?));
    }
    let rgb = rgb(source)?;
    let (width, height) = (source.width.div_ceil(step), source.height.div_ceil(step));
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in (0..source.height as usize).step_by(step as usize) {
        for x in (0..source.width as usize).step_by(step as usize) {
            let i = (y * source.width as usize + x) * 3;
            rgba.extend_from_slice(&[rgb[i], rgb[i + 1], rgb[i + 2], 255]);
        }
    }
    Ok((width, height, rgba))
}

fn decimate(frame: &Frame, factor: usize) -> Option<Frame> {
    let (unit, bytes): (usize, usize) = match frame.pixel_format {
        0x0108_0008..=0x0108_000b => (2, 1),
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => (1, 1),
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => (1, 2),
        RGB8 | 0x0218_0015 => (1, 3),
        _ => return None,
    };
    let (w, h) = (frame.width as usize, frame.height as usize);
    let step = unit.checked_mul(factor)?;
    if w < unit || h < unit || frame.data.len() < w.checked_mul(h)?.checked_mul(bytes)? {
        return None;
    }
    let (columns, rows) = ((w - unit) / step + 1, (h - unit) / step + 1);
    let mut data = Vec::with_capacity(columns * rows * unit * unit * bytes);
    let stride = step.checked_mul(bytes)?;
    for y in (0..rows).flat_map(|row| (0..unit).map(move |dy| row * step + dy)) {
        let line = &frame.data[y * w * bytes..(y + 1) * w * bytes];
        data.extend(
            line.chunks(stride)
                .take(columns)
                .flat_map(|p| &p[..unit * bytes]),
        );
    }
    Some(Frame {
        width: (columns * unit) as u32,
        height: (rows * unit) as u32,
        data,
        ..*frame
    })
}

/// Bounds for `preview_rgba` that keep at least `shown` pixels each way, so
/// a view that many pixels across never enlarges the picture, while
/// shrinking it as far as whole factors allow.
pub fn covering(frame: &Frame, (shown_width, shown_height): (u32, u32)) -> (u32, u32) {
    let factor = (frame.width / shown_width.max(1))
        .min(frame.height / shown_height.max(1))
        .max(1);
    // A mosaic shrinks by whole quads, two pixels at a time.
    let factor = if is_bayer(frame.pixel_format) {
        factor & !1
    } else {
        factor
    };
    shrunk_size(frame, factor.max(1))
}
/// The size `shrink` makes `frame` by `factor`: in whole quads for a mosaic.
fn shrunk_size(frame: &Frame, factor: u32) -> (u32, u32) {
    if factor <= 1 {
        return (frame.width, frame.height);
    }
    let unit = if is_bayer(frame.pixel_format) { 2 } else { 1 };
    let blocks = factor / unit;
    (
        (frame.width / unit).div_ceil(blocks),
        (frame.height / unit).div_ceil(blocks),
    )
}
/// `shrink` by the smallest factor that fits `frame` within `bounds`; None
/// when it already fits or cannot be shrunk.
pub(crate) fn shrink_to(frame: &Frame, (max_width, max_height): (u32, u32)) -> Option<Frame> {
    let (max_width, max_height) = (max_width.max(1), max_height.max(1));
    if frame.width <= max_width && frame.height <= max_height {
        return None;
    }
    let factor = if is_bayer(frame.pixel_format) {
        2 * (frame.width / 2)
            .div_ceil(max_width)
            .max((frame.height / 2).div_ceil(max_height))
            .max(1)
    } else {
        frame
            .width
            .div_ceil(max_width)
            .max(frame.height.div_ceil(max_height))
    };
    shrink(frame, factor)
}
/// `frame` made `factor` times smaller each way for display, as RGB8 or
/// Mono8. Each pixel is the rounded mean of the block it covers, or of four
/// of its rows spread across a taller block, so detail finer than a pixel
/// averages out instead of aliasing; in efficiency mode, the pixel amid the
/// block, at a fraction of the cost. A Bayer mosaic is read in whole quads
/// (red, the mean of both greens, blue) rather than demosaiced, so its
/// colors never mix samples from far apart, and `factor` must be even. None
/// for other formats, a factor below 2, or a frame without a whole block.
pub(crate) fn shrink(frame: &Frame, factor: u32) -> Option<Frame> {
    const MAX_ROWS: usize = 4;
    let (bytes, channels) = match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => (1, 1),
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => (2, 1),
        RGB8 | 0x0218_0015 => (3, 3),
        0x0108_0008..=0x0108_000b => (1, 3),
        _ => return None,
    };
    let bayer = is_bayer(frame.pixel_format);
    // Pixels of the source per unit: a quad for a mosaic, else one pixel.
    let unit = if bayer { 2 } else { 1 };
    let sparse = crate::efficiency::efficient();
    let pixels = checked_pixels(frame).ok()?;
    if factor < 2 || !(factor as usize).is_multiple_of(unit) || frame.data.len() < pixels * bytes {
        return None;
    }
    let (columns, rows) = (frame.width as usize / unit, frame.height as usize / unit);
    let blocks = factor as usize / unit;
    let (width, height) = (columns.div_ceil(blocks), rows.div_ceil(blocks));
    if width == 0 || height == 0 {
        return None;
    }
    let line = frame.width as usize * bytes;
    let pattern = bayer_pattern(frame.pixel_format);
    let shift = match frame.pixel_format {
        0x0110_0003 => 2,
        0x0110_0005 => 4,
        _ => 8,
    };
    // Bytes of whole units in one source line, and the samples they hold:
    // bytes, or pixels of a 16-bit format.
    let used = columns * unit * bytes;
    let samples = if bytes == 2 { used / 2 } else { used };
    let [red, green, other, blue] = quad_order(pattern);
    let quad_color = |quad: [u8; 4]| {
        let greens = u16::from(quad[green]) + u16::from(quad[other]);
        [quad[red], ((greens + 1) >> 1) as u8, quad[blue]]
    };
    let order = if frame.pixel_format == RGB8 {
        [0, 1, 2]
    } else {
        [2, 1, 0]
    };
    let mean = |sum: u64, count: u64| ((sum + count / 2) / count) as u8;
    // Totals of every `N`th sum from each of the first `N`.
    fn totals<const N: usize>(sums: &[u32]) -> [u64; N] {
        let mut totals = [0; N];
        for chunk in sums.as_chunks::<N>().0 {
            for (total, &sum) in totals.iter_mut().zip(chunk) {
                *total += u64::from(sum);
            }
        }
        totals
    }
    // Adds the block of source rows behind output row `y` sample by sample,
    // then each output pixel's columns of those sums.
    let shrink_row = |y: usize, sums: &mut Vec<u32>, out: &mut [u8]| {
        let lines =
            |row: usize| (0..unit).map(move |dy| &frame.data[(row * unit + dy) * line..][..used]);
        if bayer && blocks == 1 {
            // A reduction by two, the most common: each quad straight to its color.
            let [top, bottom] =
                [0, 1].map(|dy| frame.data[(2 * y + dy) * line..][..used].as_chunks::<2>().0);
            for ((pixel, top), bottom) in out.as_chunks_mut::<3>().0.iter_mut().zip(top).zip(bottom)
            {
                *pixel = quad_color([top[0], top[1], bottom[0], bottom[1]]);
            }
            return;
        }
        let first = y * blocks;
        let present = (first + blocks).min(rows) - first;
        if sparse {
            // The unit amid each block straight to its pixel.
            let [top, bottom] = [0, unit - 1]
                .map(|dy| &frame.data[((first + present / 2) * unit + dy) * line..][..used]);
            let middle =
                |x: usize| (x * blocks + blocks.min(columns - x * blocks) / 2) * unit * bytes;
            for (x, pixel) in out.chunks_exact_mut(channels).enumerate() {
                let i = middle(x);
                match (bayer, bytes) {
                    (true, _) => pixel.copy_from_slice(&quad_color([
                        top[i],
                        top[i + 1],
                        bottom[i],
                        bottom[i + 1],
                    ])),
                    (false, 1) => pixel[0] = top[i],
                    (false, 2) => {
                        pixel[0] =
                            (u16::from_le_bytes([top[i], top[i + 1]]) >> shift).min(255) as u8
                    }
                    _ => pixel.copy_from_slice(&order.map(|c| top[i + c])),
                }
            }
            return;
        }
        sums.clear();
        sums.resize(unit * samples, 0);
        // At most four rows of a block, spread across it: every column still
        // counts, and the work stays bounded however far a frame shrinks.
        let picked = present.min(MAX_ROWS);
        for source in (0..picked).map(|i| first + (2 * i + 1) * present / (2 * picked)) {
            for (sums, line) in sums.chunks_exact_mut(samples).zip(lines(source)) {
                if bytes == 2 {
                    for (sum, &p) in sums.iter_mut().zip(line.as_chunks::<2>().0) {
                        *sum += u32::from((u16::from_le_bytes(p) >> shift).min(255));
                    }
                } else {
                    for (sum, &v) in sums.iter_mut().zip(line) {
                        *sum += u32::from(v);
                    }
                }
            }
        }
        let rows = picked as u64;
        if bayer {
            let (top, bottom) = sums.split_at(samples);
            for ((pixel, top), bottom) in out
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .zip(top.chunks(2 * blocks))
                .zip(bottom.chunks(2 * blocks))
            {
                let count = rows * (top.len() / 2) as u64;
                let ([a, b], [c, d]) = (totals(top), totals(bottom));
                let quad = [a, b, c, d];
                // Each quad holds two greens.
                *pixel = [
                    mean(quad[red], count),
                    mean(quad[green] + quad[other], 2 * count),
                    mean(quad[blue], count),
                ];
            }
        } else if channels == 3 {
            for (pixel, block) in out
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .zip(sums.chunks(3 * blocks))
            {
                let count = rows * (block.len() / 3) as u64;
                let channels: [u64; 3] = totals(block);
                *pixel = order.map(|c| mean(channels[c], count));
            }
        } else {
            for (value, block) in out.iter_mut().zip(sums.chunks(blocks)) {
                let [sum] = totals(block);
                *value = mean(sum, rows * block.len() as u64);
            }
        }
    };
    let mut data = vec![0u8; width * height * channels];
    // Previews of up to two megapixels are made on this thread alone: often
    // several a frame, one per camera, they cost less than waking cores the
    // camera workers may need. Shrinking a sensor for a larger view is spread out.
    let threads = worker_threads(width * height / 4).min(height);
    let block = height.div_ceil(threads) * width * channels;
    std::thread::scope(|scope| {
        for (i, lines) in data.chunks_mut(block).enumerate() {
            let shrink_row = &shrink_row;
            let mut work = move || {
                let mut sums = Vec::new();
                for (j, out) in lines.chunks_exact_mut(width * channels).enumerate() {
                    shrink_row(i * block / (width * channels) + j, &mut sums, out);
                }
            };
            if threads > 1 {
                scope.spawn(work);
            } else {
                work();
            }
        }
    });
    Some(Frame {
        width: width as u32,
        height: height as u32,
        pixel_format: if channels == 3 { RGB8 } else { MONO8 },
        data,
        ..*frame
    })
}

pub fn luminance_histogram(rgba: &[u8]) -> [u32; 64] {
    let pixels = rgba.as_chunks::<4>().0;
    let mut bins = [0; 64];
    for p in pixels.iter().step_by(pixels.len().div_ceil(65_536).max(1)) {
        bins[(p[0] as usize * 54 + p[1] as usize * 183 + p[2] as usize * 19) >> 10] += 1;
    }
    bins
}

pub fn encode(frame: &Frame, format: &str) -> Result<Vec<u8>> {
    if format == "raw" {
        return Ok(frame.data.clone());
    }
    if format == "jpeg" {
        return crate::jpeg::encode(frame);
    }
    let rgb = rgb(frame)?;
    let mut bytes = Vec::new();
    match format {
        "png" => {
            let mut encoder = png::Encoder::new(&mut bytes, frame.width, frame.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            // A fixed Paeth filter reduces camera-texture output size without
            // running all five filters for every scanline.
            encoder.set_filter(png::FilterType::Paeth);
            encoder.write_header()?.write_image_data(&rgb)?;
        }
        "ppm" => {
            write!(bytes, "P6\n{} {}\n255\n", frame.width, frame.height)?;
            bytes.extend_from_slice(&rgb);
        }
        "pgm" => {
            ensure!(
                matches!(
                    frame.pixel_format,
                    MONO8
                        | 0x0108_00b1
                        | 0x0108_00c6
                        | 0x0110_0003
                        | 0x0110_0005
                        | 0x0110_0007
                        | 0x0110_00b8
                        | 0x0110_00c7
                ),
                "PGM requires a monochrome image; use PNG or PPM"
            );
            write!(bytes, "P5\n{} {}\n255\n", frame.width, frame.height)?;
            bytes.extend(rgb.as_chunks::<3>().0.iter().map(|p| p[0]));
        }
        _ => bail!("unknown image format {format}; use png, jpeg, raw, pgm or ppm"),
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coordinate_and_confidence_formats_convert_but_raw_preserves_precision() {
        for format in [0x0110_00b8, 0x0110_00c7] {
            let source = f(format, vec![0x34, 0x12, 0xff, 0xff]);
            assert_eq!(rgb(&source).unwrap(), vec![0x12, 0x12, 0x12, 255, 255, 255]);
            assert_eq!(encode(&source, "raw").unwrap(), source.data);
            assert!(convertible(format));
            assert!(exposure(&source).unwrap().channels.is_none());
            assert_eq!(preview_rgba(&source, 1, 1).unwrap().0, 1);
        }
        for format in [0x0108_00b1, 0x0108_00c6] {
            assert_eq!(
                rgb(&f(format, vec![20, 40])).unwrap(),
                vec![20, 20, 20, 40, 40, 40]
            );
        }
    }
    fn f(fmt: u32, data: Vec<u8>) -> Frame {
        Frame {
            id: 1,
            width: 2,
            height: 1,
            pixel_format: fmt,
            timestamp_ns: 0,
            data,
        }
    }
    #[test]
    fn conversions_and_truncation() {
        assert_eq!(rgb(&f(MONO8, vec![4, 8])).unwrap(), vec![4, 4, 4, 8, 8, 8]);
        assert_eq!(
            rgb(&f(0x0218_0015, vec![1, 2, 3, 4, 5, 6])).unwrap(),
            vec![3, 2, 1, 6, 5, 4]
        );
        assert!(rgb(&f(RGB8, vec![0])).is_err());
        assert!(rgb(&f(0, vec![0; 6])).is_err());
        for (format, shift) in [(0x0110_0003, 2), (0x0110_0005, 4), (0x0110_0007, 8)] {
            let data = [(2u16 << shift) - 1, u16::MAX]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect();
            assert_eq!(rgb(&f(format, data)).unwrap(), [1, 1, 1, 255, 255, 255]);
            assert!(rgb(&f(format, vec![0; 3])).is_err());
        }
        assert_eq!(
            rgb(&f(RGB8, vec![1, 2, 3, 4, 5, 6, 7])).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        assert!(
            preview_rgba(
                &Frame {
                    width: u32::MAX,
                    ..f(MONO8, vec![])
                },
                1,
                1
            )
            .is_err()
        );
    }
    fn reference_bayer(frame: &Frame) -> Vec<u8> {
        let pattern = match frame.pixel_format {
            0x0108_0008 => [1, 0, 2, 1],
            0x0108_0009 => [0, 1, 1, 2],
            0x0108_000a => [1, 2, 0, 1],
            _ => [2, 1, 1, 0],
        };
        let w = frame.width as i32;
        let h = frame.height as i32;
        let mut out = vec![0; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let own = pattern[((y & 1) * 2 + (x & 1)) as usize];
                    let v = if own == c {
                        frame.data[(y * w + x) as usize]
                    } else {
                        let mut sum = 0u32;
                        let mut n = 0;
                        for dy in -1..=1 {
                            for dx in -1..=1 {
                                let xx = x + dx;
                                let yy = y + dy;
                                if xx >= 0
                                    && xx < w
                                    && yy >= 0
                                    && yy < h
                                    && pattern[((yy & 1) * 2 + (xx & 1)) as usize] == c
                                {
                                    sum += frame.data[(yy * w + xx) as usize] as u32;
                                    n += 1;
                                }
                            }
                        }
                        sum.checked_div(n)
                            .map(|v| v as u8)
                            .unwrap_or(frame.data[(y * w + x) as usize])
                    };
                    out[((y * w + x) * 3 + c) as usize] = v;
                }
            }
        }
        out
    }
    fn random(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }
    #[test]
    fn bayer_matches_reference_bilinear() {
        let sizes = [
            (1, 1),
            (2, 1),
            (1, 2),
            (2, 2),
            (3, 3),
            (4, 3),
            (5, 7),
            (33, 17),
            (64, 48),
            // Above the threading threshold, with uneven row blocks.
            (1031, 517),
        ];
        for (seed, (width, height)) in sizes.into_iter().enumerate() {
            for pixel_format in 0x0108_0008..=0x0108_000b {
                let mut frame = f(pixel_format, random(seed as u64 + 1, width * height));
                (frame.width, frame.height) = (width as u32, height as u32);
                assert_eq!(
                    rgb(&frame).unwrap(),
                    reference_bayer(&frame),
                    "{width}x{height} {pixel_format:x}"
                );
                frame.data = vec![255; width * height];
                assert_eq!(rgb(&frame).unwrap(), reference_bayer(&frame));
            }
        }
    }
    #[test]
    fn preview_matches_conversion_and_preserves_bayer_phase() {
        crate::efficiency::pin(false);
        let sized = |pixel_format, width, height, data| Frame {
            width,
            height,
            ..f(pixel_format, data)
        };
        for (format, bytes) in [(MONO8, 1), (RGB8, 3), (0x0218_0015, 3), (0x0110_0005, 2)] {
            let frame = sized(format, 33, 17, random(format as u64, 33 * 17 * bytes));
            let full = rgb(&frame).unwrap();
            let (w, h, rgba) = preview_rgba(&frame, 33, 17).unwrap();
            assert_eq!((w, h, rgba.len()), (33, 17, full.len() / 3 * 4));
            assert!(
                rgba.as_chunks::<4>()
                    .0
                    .iter()
                    .zip(full.as_chunks::<3>().0)
                    .all(|(a, b)| a[..3] == b[..] && a[3] == 255)
            );
            // Shrunk 4 times: each pixel the mean of the block it covers.
            let (w, h, rgba) = preview_rgba(&frame, 10, 10).unwrap();
            assert_eq!((w, h), (9, 5));
            let expected = box_mean(&full, 33, 17, 4);
            for (p, want) in rgba
                .as_chunks::<4>()
                .0
                .iter()
                .zip(expected.as_chunks::<3>().0)
            {
                assert_eq!((&p[..3], p[3]), (&want[..], 255));
            }
        }
        let patterns = [[1, 0, 2, 1], [0, 1, 1, 2], [1, 2, 0, 1], [2, 1, 1, 0]];
        for (pattern, cfa) in (0x0108_0008..=0x0108_000b).zip(patterns) {
            let data = (0..1201 * 1921)
                .map(|i| [200, 100, 50][cfa[((i / 1921) & 1) * 2 + ((i % 1921) & 1)]])
                .collect();
            let (w, h, rgba) = preview_rgba(&sized(pattern, 1921, 1201, data), 640, 480).unwrap();
            assert!(w <= 640 && h <= 480 && w * h * 4 == rgba.len() as u32);
            assert!(
                rgba.as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| p == &[200, 100, 50, 255])
            );
        }
        for (width, height) in [(1, 9), (9, 1), (6, 6), (33, 17)] {
            for format in 0x0108_0008..=0x0108_000b {
                let frame = sized(format, width, height, vec![127; (width * height) as usize]);
                for (max_width, max_height) in [(0, 0), (1, 3), (3, 3), (9, 5)] {
                    let (w, h, rgba) = preview_rgba(&frame, max_width, max_height).unwrap();
                    assert!(w <= max_width.max(1) && h <= max_height.max(1));
                    assert_eq!(rgba.len(), (w * h * 4) as usize);
                    assert!(
                        rgba.as_chunks::<4>()
                            .0
                            .iter()
                            .all(|p| p == &[127, 127, 127, 255])
                    );
                }
            }
        }
        assert!(preview_rgba(&f(MONO8, vec![1]), 1, 1).is_err());
        assert!(preview_rgba(&f(0, vec![0; 6]), 1, 1).is_err());
    }
    /// The rows `shrink` sums of a block starting at `first` with `present`
    /// rows: all of them, or four spread across it.
    fn picked(first: usize, present: usize) -> Vec<usize> {
        let picked = present.min(4);
        (0..picked)
            .map(|i| first + (2 * i + 1) * present / (2 * picked))
            .collect()
    }
    /// Rounded means of `factor` × `factor` blocks of an RGB image, edge
    /// blocks over the pixels they have.
    fn box_mean(rgb: &[u8], width: usize, height: usize, factor: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for by in (0..height).step_by(factor) {
            for bx in (0..width).step_by(factor) {
                let mut sums = [0usize; 3];
                let mut n = 0;
                for y in picked(by, (by + factor).min(height) - by) {
                    for x in bx..(bx + factor).min(width) {
                        for c in 0..3 {
                            sums[c] += rgb[(y * width + x) * 3 + c] as usize;
                        }
                        n += 1;
                    }
                }
                out.extend(sums.map(|sum| ((sum + n / 2) / n) as u8));
            }
        }
        out
    }
    #[test]
    fn shrinking_averages_blocks_and_reads_mosaics_in_whole_quads() {
        crate::efficiency::pin(false);
        let formats = [
            (MONO8, 1),
            (0x0108_00b1, 1),
            (RGB8, 3),
            (0x0218_0015, 3),
            (0x0110_0005, 2),
            (0x0110_0007, 2),
        ];
        for (format, bytes) in formats {
            let frame = Frame {
                width: 33,
                height: 17,
                ..f(format, random(format as u64, 33 * 17 * bytes))
            };
            let full = rgb(&frame).unwrap();
            for factor in [2, 3, 4, 5, 40] {
                let small = shrink(&frame, factor).unwrap();
                let size = (33u32.div_ceil(factor), 17u32.div_ceil(factor));
                assert_eq!((small.width, small.height), size, "{format:x} by {factor}");
                assert_eq!(small.pixel_format, if bytes == 3 { RGB8 } else { MONO8 });
                assert_eq!(
                    rgb(&small).unwrap(),
                    box_mean(&full, 33, 17, factor as usize),
                    "{format:x} by {factor}"
                );
            }
            assert!(shrink(&frame, 1).is_none());
            let mut short = frame.clone();
            short.data.pop();
            assert!(shrink(&short, 2).is_none());
        }
        // Each quad its own color: R, two different greens, B.
        let (width, height) = (35usize, 23usize);
        let colors = random(7, (width / 2) * (height / 2) * 4);
        for (format, cfa) in (0x0108_0008..=0x0108_000b).zip(0..) {
            let pattern = bayer_pattern(format);
            let data = (0..width * height)
                .map(|i| {
                    let (x, y) = (i % width, i / width);
                    let quad =
                        (y / 2).min(height / 2 - 1) * (width / 2) + (x / 2).min(width / 2 - 1);
                    let phase = (y & 1) * 2 + (x & 1);
                    // Which of the quad's four samples: red, first green, second green, blue.
                    let slot = match pattern[phase] {
                        0 => 0,
                        2 => 3,
                        _ if pattern[..phase].contains(&1) => 2,
                        _ => 1,
                    };
                    colors[quad * 4 + slot]
                })
                .collect();
            let frame = Frame {
                width: width as u32,
                height: height as u32,
                ..f(format, data)
            };
            let quad = |i: usize| {
                let c = &colors[i * 4..i * 4 + 4];
                [c[0] as usize, c[1] as usize + c[2] as usize, c[3] as usize]
            };
            for blocks in [1, 2, 3, 11] {
                let small = shrink(&frame, 2 * blocks as u32).unwrap();
                let (columns, rows) = (width / 2, height / 2);
                assert_eq!(
                    (small.width as usize, small.height as usize),
                    (columns.div_ceil(blocks), rows.div_ceil(blocks))
                );
                for (i, p) in small.data.as_chunks::<3>().0.iter().enumerate() {
                    let (bx, by) = (i % small.width as usize, i / small.width as usize);
                    let mut sums = [0; 3];
                    let mut n = 0;
                    let first = by * blocks;
                    for y in picked(first, ((by + 1) * blocks).min(rows) - first) {
                        for x in bx * blocks..((bx + 1) * blocks).min(columns) {
                            let q = quad(y * columns + x);
                            for c in 0..3 {
                                sums[c] += q[c];
                            }
                            n += 1;
                        }
                    }
                    let mean = |sum: usize, n: usize| ((sum + n / 2) / n) as u8;
                    let want = [mean(sums[0], n), mean(sums[1], 2 * n), mean(sums[2], n)];
                    assert_eq!(*p, want, "cfa {cfa} blocks {blocks} at {bx},{by}");
                }
            }
            assert!(
                shrink(&frame, 3).is_none(),
                "a mosaic shrinks by whole quads"
            );
            assert!(
                shrink(
                    &Frame {
                        width: 1,
                        ..frame.clone()
                    },
                    2
                )
                .is_none()
            );
        }
        assert!(shrink(&f(0x0120_00bf, vec![0; 8]), 2).is_none());
    }
    #[test]
    fn efficiency_mode_shrinks_to_the_pixel_amid_each_block() {
        crate::efficiency::pin(true);
        // The middle of the block from `start` of `size` units, up to `end`.
        let middle = |start: usize, size: usize, end: usize| start + size.min(end - start) / 2;
        let formats = [(MONO8, 1), (RGB8, 3), (0x0218_0015, 3), (0x0110_0005, 2)];
        for (format, bytes) in formats {
            let frame = Frame {
                width: 33,
                height: 17,
                ..f(format, random(format as u64, 33 * 17 * bytes))
            };
            let full = rgb(&frame).unwrap();
            for factor in [2, 3, 5, 40] {
                let small = shrink(&frame, factor).unwrap();
                let factor = factor as usize;
                let expected: Vec<u8> = (0..17)
                    .step_by(factor)
                    .flat_map(|y| (0..33).step_by(factor).map(move |x| (x, y)))
                    .flat_map(|(x, y)| {
                        let i = middle(y, factor, 17) * 33 + middle(x, factor, 33);
                        full[i * 3..i * 3 + 3].to_vec()
                    })
                    .collect();
                assert_eq!(rgb(&small).unwrap(), expected, "{format:x} by {factor}");
            }
        }
        // A grey mosaic, each quad its row and column: whole quads are kept.
        let (width, height) = (34, 22);
        let data = (0..width * height)
            .map(|i| (i / width / 2 * 20 + i % width / 2) as u8)
            .collect();
        let frame = Frame {
            width,
            height,
            ..f(0x0108_0009, data)
        };
        let small = shrink(&frame, 6).unwrap();
        assert_eq!((small.width, small.height), (6, 4));
        for (i, pixel) in small.data.as_chunks::<3>().0.iter().enumerate() {
            let (x, y) = (i % 6 * 3, i / 6 * 3);
            let value = (middle(y, 3, 11) * 20 + middle(x, 3, 17)) as u8;
            assert_eq!(*pixel, [value; 3], "at {x},{y}");
        }
    }
    #[test]
    fn covering_bounds_never_enlarge_a_view() {
        for format in [MONO8, RGB8, 0x0108_0009] {
            let bytes = if format == RGB8 { 3 } else { 1 };
            let frame = Frame {
                width: 2448,
                height: 2048,
                ..f(format, vec![90; 2448 * 2048 * bytes])
            };
            for shown in [
                (2448, 2048),
                (2000, 1700),
                (1224, 1024),
                (1000, 900),
                (700, 600),
                (300, 200),
                (1, 1),
            ] {
                let bounds = covering(&frame, shown);
                let (w, h, rgba) = preview_rgba(&frame, bounds.0, bounds.1).unwrap();
                assert_eq!((w, h), bounds, "{format:x} {shown:?}");
                assert!(
                    w >= shown.0 && h >= shown.1,
                    "{format:x} {shown:?}: {w}x{h}"
                );
                // As small as whole factors allow: twice as many is too many.
                assert!(
                    w < 2 * shown.0.max(1224) || h < 2 * shown.1,
                    "{format:x} {shown:?}: {w}x{h}"
                );
                assert!(rgba.iter().all(|&v| v == 90 || v == 255));
            }
            assert_eq!(covering(&frame, (3000, 3000)), (2448, 2048));
        }
        // A mosaic shown at more than half its size keeps every pixel.
        let bayer = Frame {
            width: 2448,
            height: 2048,
            ..f(0x0108_0009, vec![])
        };
        assert_eq!(covering(&bayer, (1300, 1100)), (2448, 2048));
        assert_eq!(covering(&bayer, (800, 700)), (1224, 1024));
    }
    #[test]
    fn quad_scopes_agree_with_the_demosaic() {
        // A smooth scene with fine texture, sampled through each mosaic, against
        // the same pixels demosaiced, which is what the scopes measured before.
        let (width, height) = (640usize, 480usize);
        let scene = |x: usize, y: usize, c: usize| {
            // Blown out along the top, crushed along the bottom.
            if !(48..432).contains(&y) {
                return if y < 48 { 255 } else { 0 };
            }
            let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
            let base = [
                90.0 + 70.0 * (fx * 3.1 + fy * 1.7).sin(),
                110.0 + 60.0 * (fy * 4.3 - fx * 0.9).cos(),
                80.0 + 50.0 * (fx * fy * 9.0).sin(),
            ][c];
            (base + 18.0 * (x as f32 * 0.21).sin() * (y as f32 * 0.17).cos()).clamp(0.0, 255.0)
                as u8
        };
        for format in 0x0108_0008..=0x0108_000b {
            let pattern = bayer_pattern(format);
            let mosaic = Frame {
                width: width as u32,
                height: height as u32,
                ..f(
                    format,
                    (0..width * height)
                        .map(|i| {
                            scene(
                                i % width,
                                i / width,
                                pattern[((i / width) & 1) * 2 + ((i % width) & 1)],
                            )
                        })
                        .collect(),
                )
            };
            let demosaiced = Frame {
                pixel_format: RGB8,
                data: rgb(&mosaic).unwrap(),
                ..mosaic.clone()
            };
            let (quads, bilinear) = (exposure(&mosaic).unwrap(), exposure(&demosaiced).unwrap());
            assert!((quads.mean - bilinear.mean).abs() < 0.005, "{format:x}");
            assert!(quads.highlights > 0.05 && quads.shadows > 0.05);
            assert!((quads.highlights - bilinear.highlights).abs() < 0.01);
            assert!((quads.shadows - bilinear.shadows).abs() < 0.01);
            let share = |bins: &[u32; 64]| {
                let total = bins.iter().sum::<u32>() as f32;
                bins.map(|n| n as f32 / total)
            };
            let (a, b) = (share(&quads.luma), share(&bilinear.luma));
            // Earth mover's distance between the two, in bins.
            let (mut carried, mut distance) = (0.0f32, 0.0f32);
            for (a, b) in a.iter().zip(&b) {
                carried += a - b;
                distance += carried.abs();
            }
            assert!(
                distance < 0.25,
                "{format:x}: histograms {distance} bins apart"
            );
            // Focus scores rank a focus sweep the same way.
            let sweep: Vec<(f32, f32)> = [0usize, 1, 2, 4]
                .into_iter()
                .map(|blur| {
                    let soft = Frame {
                        data: (0..width * height)
                            .map(|i| {
                                let (x, y) = (i % width, i / width);
                                let c = pattern[(y & 1) * 2 + (x & 1)];
                                let (x0, x1) = (x.saturating_sub(blur), (x + blur).min(width - 1));
                                let sum: usize = (x0..=x1)
                                    .map(|x| (x / 8 % 2) * 200 + scene(x, y, c) as usize / 8)
                                    .sum();
                                (sum / (x1 - x0 + 1)) as u8
                            })
                            .collect(),
                        ..mosaic.clone()
                    };
                    let region = [100, 100, 400, 300];
                    let reference = Frame {
                        pixel_format: RGB8,
                        data: rgb(&soft).unwrap(),
                        ..soft.clone()
                    };
                    (
                        sharpness(&soft, region).unwrap().tenengrad,
                        sharpness(&reference, region).unwrap().tenengrad,
                    )
                })
                .collect();
            for pair in sweep.windows(2) {
                assert!(
                    pair[0].0 > pair[1].0 && pair[0].1 > pair[1].1,
                    "{format:x}: {sweep:?}"
                );
            }
        }
        // Too small for quads: measured pixel by pixel as before.
        let tiny = Frame {
            width: 4,
            height: 4,
            ..f(0x0108_0009, (0..16).map(|i| i * 15).collect())
        };
        assert!(sharpness(&tiny, [0, 0, 4, 4]).is_ok());
    }
    #[test]
    fn conversions_leave_a_core_free() {
        assert_eq!(worker_threads(1000), 1);
        assert_eq!([1, 2, 4, 8, 9, 16].map(spare_cores), [1, 1, 3, 7, 8, 8]);
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert_eq!(worker_threads(64 << 20), spare_cores(cores));
    }
    #[test]
    fn direct_conversions_match_rgb() {
        let formats = [
            (MONO8, 1),
            (RGB8, 3),
            (0x0218_0015, 3),
            (0x0110_0003, 2),
            (0x0110_0005, 2),
            (0x0110_0007, 2),
            (0x0108_0008, 1),
            (0x0108_0009, 1),
            (0x0108_000a, 1),
            (0x0108_000b, 1),
        ];
        for (width, height) in [(1, 1), (2, 1), (5, 7), (33, 17)] {
            for (format, bytes) in formats {
                let frame = Frame {
                    width,
                    height,
                    ..f(
                        format,
                        random(format as u64, (width * height) as usize * bytes),
                    )
                };
                let full = rgb(&frame).unwrap();
                assert_eq!(convert(&frame, |p| p).unwrap().into_flattened(), full);
                // Into caller-owned memory, and into a reused buffer.
                let mut slice = vec![[7u8; 3]; (width * height) as usize];
                convert_slice(&frame, &mut slice, |p| p).unwrap();
                assert_eq!(slice.into_flattened(), full);
                assert!(convert_slice(&frame, &mut [[0u8; 3]; 1][..0], |p| p).is_err());
                let mut reused = vec![[1u8; 3]; 3];
                convert_into(&frame, &mut reused, |p| p).unwrap();
                assert_eq!(reused.into_flattened(), full);
                let rgba = rgba(&frame).unwrap();
                assert_eq!(rgba.len(), full.len() / 3 * 4);
                assert!(
                    rgba.as_chunks::<4>()
                        .0
                        .iter()
                        .zip(full.as_chunks::<3>().0)
                        .all(|(a, b)| a[..3] == b[..] && a[3] == 255),
                    "{width}x{height} {format:x}"
                );
            }
        }
        assert!(rgba(&f(RGB8, vec![0])).is_err());
        assert!(convert(&f(0, vec![0; 6]), |p| p).is_err());
    }
    #[test]
    fn sparse_sampling_matches_full_conversion() {
        for (width, height) in [(1, 1), (2, 3), (5, 7), (33, 17), (400, 400)] {
            for (format, bytes) in [
                (MONO8, 1),
                (RGB8, 3),
                (0x0218_0015, 3),
                (0x0110_0005, 2),
                (0x0110_0007, 2),
                (0x0108_0008, 1),
                (0x0108_0009, 1),
                (0x0108_000a, 1),
                (0x0108_000b, 1),
            ] {
                let frame = Frame {
                    width,
                    height,
                    ..f(
                        format,
                        random(
                            format as u64 ^ width as u64,
                            (width * height) as usize * bytes,
                        ),
                    )
                };
                assert!(convertible(format));
                let full = rgb(&frame).unwrap();
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let i = (y * width as usize + x) * 3;
                        assert_eq!(pixel_at(&frame, x, y), full[i..i + 3], "{format:x} {x},{y}");
                    }
                }
                // Bayer exposure reads whole quads; see quad_scopes_agree_with_the_demosaic.
                if is_bayer(format) {
                    continue;
                }
                let mut bins = [0u32; 64];
                let pixels = full.as_chunks::<3>().0;
                for p in pixels.iter().step_by((pixels.len() / 65_536).max(1)) {
                    bins[(p[0] as usize * 54 + p[1] as usize * 183 + p[2] as usize * 19) >> 10] +=
                        1;
                }
                assert_eq!(exposure(&frame).unwrap().luma, bins);
            }
        }
        assert!(!convertible(0));
        assert!(exposure(&f(0, vec![0; 6])).is_err());
        assert!(exposure(&f(RGB8, vec![0; 5])).is_err());
    }
    #[test]
    fn exposure_reports_channels_clipping_and_mean() {
        // Black, white, mid grey and pure red.
        let frame = Frame {
            width: 4,
            ..f(RGB8, vec![0, 0, 0, 255, 255, 255, 128, 128, 128, 255, 0, 0])
        };
        let exposure = exposure(&frame).unwrap();
        let channels = exposure.channels.unwrap();
        assert_eq!(
            (channels[0][63], channels[1][63], channels[2][0]),
            (2, 1, 2)
        );
        assert_eq!((exposure.shadows, exposure.highlights), (0.25, 0.5));
        // Luminance of black, white, grey and red.
        let expected = (255 + 128 + ((255 * 54) >> 8)) as f32 / 4.0 / 255.0;
        assert!((exposure.mean - expected).abs() < 1e-6);
        let mono = super::exposure(&f(MONO8, vec![0, 255])).unwrap();
        assert!(mono.channels.is_none());
        assert_eq!((mono.shadows, mono.highlights), (0.5, 0.5));
    }
    #[test]
    fn sharpness_prefers_crisp_detail_in_the_region() {
        let (w, h) = (64u32, 48u32);
        let image = |sharp: bool| Frame {
            width: w,
            height: h,
            ..f(
                MONO8,
                (0..w * h)
                    .map(|i| {
                        let (x, y) = (i % w, i / w);
                        let edge = |v: u32| match (sharp, v % 8) {
                            (true, p) => {
                                if p < 4 {
                                    40
                                } else {
                                    220
                                }
                            }
                            // The same pattern as a soft ramp.
                            (false, p) => [130, 160, 190, 160, 130, 100, 70, 100][p as usize],
                        };
                        // Detail only on the left; the right half is flat.
                        if x < w / 2 { edge(x + y) } else { 128 }
                    })
                    .collect(),
            )
        };
        let left = [0, 0, w / 2, h];
        let crisp = sharpness(&image(true), left).unwrap();
        let soft = sharpness(&image(false), left).unwrap();
        for (crisp, soft) in [
            (crisp.laplacian, soft.laplacian),
            (crisp.tenengrad, soft.tenengrad),
            (crisp.brenner, soft.brenner),
            (crisp.variance, soft.variance),
        ] {
            assert!(crisp > soft && soft > 0.0, "{crisp} > {soft}");
        }
        let flat = sharpness(&image(true), [w / 2 + 2, 0, w / 2, h]).unwrap();
        assert_eq!(flat, Sharpness::default());
        // Regions past the frame are clamped rather than rejected.
        assert!(sharpness(&image(true), [u32::MAX, u32::MAX, 9, 9]).is_ok());
        assert!(
            sharpness(&image(true), [0, 0, u32::MAX, u32::MAX])
                .unwrap()
                .laplacian
                > 0.0
        );
        assert!(sharpness(&f(MONO8, vec![0, 0]), [0, 0, 2, 1]).is_err());
        // One-pixel detail over a region large enough for an even stride.
        let (w, h) = (512u32, 512u32);
        for pattern in [|x: u32, y: u32| (x + y) % 2, |_x: u32, y: u32| y % 2] {
            let fine = Frame {
                width: w,
                height: h,
                ..f(
                    MONO8,
                    (0..w * h)
                        .map(|i| pattern(i % w, i / w) as u8 * 200)
                        .collect(),
                )
            };
            let score = sharpness(&fine, [0, 0, w, h]).unwrap();
            assert!(score.laplacian > 0.0 && score.variance > 0.0, "{score:?}");
        }
    }
    #[test]
    fn histogram_bins_sampled_luminance() {
        let bins = luminance_histogram(&[0, 0, 0, 255, 255, 255, 255, 255, 128, 128, 128, 255]);
        assert_eq!(
            (bins[0], bins[32], bins[63], bins.iter().sum::<u32>()),
            (1, 1, 1, 3)
        );
        assert_eq!(
            luminance_histogram(&vec![9; 4 * 65_536 * 3])
                .iter()
                .sum::<u32>(),
            65_536
        );
        assert!(
            luminance_histogram(&vec![9; 4 * 131_071])
                .iter()
                .sum::<u32>()
                <= 65_536
        );
    }
    #[test]
    fn png_roundtrip() {
        let data = encode(&f(MONO8, vec![0, 255]), "png").unwrap();
        let mut reader = png::Decoder::new(data.as_slice()).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).unwrap();
        assert_eq!(buf, vec![0, 0, 0, 255, 255, 255]);
    }
}
