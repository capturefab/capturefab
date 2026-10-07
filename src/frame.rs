//! Bounded PFNC image conversion and portable output without image codecs from the OS.
use crate::types::{Frame, MONO8, RGB8};
use anyhow::{Result, bail, ensure};
use std::{io::Write, path::Path};

pub fn pixel_format_name(format: u32) -> &'static str {
    match format {
        MONO8 => "Mono8",
        RGB8 => "RGB8",
        0x0218_0015 => "BGR8",
        0x0110_0003 => "Mono10",
        0x0110_0005 => "Mono12",
        0x0110_0007 => "Mono16",
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
        MONO8 => {
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
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 => {
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
fn checked_pixels(frame: &Frame) -> Result<usize> {
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
        MONO8 => {
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
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 => {
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
/// spawning costs more than it saves, else up to eight available cores.
pub(crate) fn worker_threads(pixels: usize) -> usize {
    if pixels < 512 * 1024 {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
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
fn validated_bytes_per_pixel(frame: &Frame) -> Result<usize> {
    let pixels = checked_pixels(frame)?;
    let (bytes, name) = match frame.pixel_format {
        MONO8 => (1, "Mono8"),
        RGB8 | 0x0218_0015 => (3, "RGB"),
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 => (2, "unpacked monochrome"),
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
        MONO8 | RGB8 | 0x0218_0015 | 0x0110_0003 | 0x0110_0005 | 0x0110_0007 | 0x0108_0008
            ..=0x0108_000b
    )
}
/// One pixel exactly as `convert` would produce it; for sparse sampling when
/// the full image is converted elsewhere (for example on the GPU).
/// The frame must have passed `validated_bytes_per_pixel`.
fn pixel_at(frame: &Frame, x: usize, y: usize) -> [u8; 3] {
    let i = y * frame.width as usize + x;
    let data = &frame.data;
    match frame.pixel_format {
        MONO8 => [data[i]; 3],
        RGB8 => [data[3 * i], data[3 * i + 1], data[3 * i + 2]],
        0x0218_0015 => [data[3 * i + 2], data[3 * i + 1], data[3 * i]],
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 => {
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

/// How a frame is exposed, from at most 65,536 evenly strided pixels decoded
/// individually so no full RGB image is needed.
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
        MONO8 | 0x0110_0003 | 0x0110_0005 | 0x0110_0007
    );
    let (mut luma_bins, mut channels) = ([0; 64], [[0; 64]; 3]);
    let (mut sum, mut samples, mut shadows, mut highlights) = (0u64, 0u32, 0u32, 0u32);
    for i in (0..pixels).step_by((pixels / 65_536).max(1)) {
        let rgb = pixel_at(frame, i % w, i / w);
        let y = luma(rgb);
        luma_bins[y >> 2] += 1;
        for (bins, value) in channels.iter_mut().zip(rgb) {
            bins[value as usize >> 2] += 1;
        }
        sum += y as u64;
        samples += 1;
        shadows += u32::from(rgb.iter().all(|&v| v <= CRUSHED));
        highlights += u32::from(rgb.iter().any(|&v| v >= BLOWN));
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
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
pub fn sharpness(frame: &Frame, region: [u32; 4]) -> Result<Sharpness> {
    validated_bytes_per_pixel(frame)?;
    checked_pixels(frame)?;
    let (w, h) = (frame.width, frame.height);
    ensure!(w >= 3 && h >= 3, "frame is too small to measure sharpness");
    // Keep a one-pixel border so every 3×3 neighbourhood is inside the frame.
    let x0 = region[0].clamp(1, w - 2);
    let y0 = region[1].clamp(1, h - 2);
    let x1 = region[0].saturating_add(region[2]).clamp(x0 + 1, w - 1);
    let y1 = region[1].saturating_add(region[3]).clamp(y0 + 1, h - 1);
    let area = (x1 - x0) as f64 * (y1 - y0) as f64;
    let step = (area / 65_536.0).sqrt().ceil().max(1.0) as usize;
    let at = |x: usize, y: usize| luma(pixel_at(frame, x, y)) as f64;
    let (mut n, mut lap, mut lap2, mut ten, mut bren, mut sum, mut sum2) =
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    // Shift each row and column of the sampling grid by a different phase
    // within the stride, so periodic detail (a one-pixel checkerboard or
    // stripes) is never sampled at only one of its phases.
    let (x0, y0, x1, y1) = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
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
    Ok(Sharpness {
        laplacian: (lap2 / n - (lap / n).powi(2)).max(0.0) as f32,
        tenengrad: (ten / n) as f32,
        brenner: (bren / n) as f32,
        variance: if mean > 0.0 { variance / mean } else { 0.0 } as f32,
    })
}
pub fn preview_rgba(frame: &Frame, max_width: u32, max_height: u32) -> Result<(u32, u32, Vec<u8>)> {
    let (max_width, max_height) = (max_width.max(1), max_height.max(1));
    // Sample complete Bayer quads so every retained pixel keeps its CFA phase.
    // Count quads rather than pixels to keep odd requested bounds bounded too.
    let factor = if matches!(frame.pixel_format, 0x0108_0008..=0x0108_000b) {
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
    let source = small.as_ref().unwrap_or(frame);
    // One-pixel bounds or a one-row Bayer source cannot retain complete quads.
    // Finish those cases by sampling after conversion.
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
        MONO8 => (1, 1),
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 => (1, 2),
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
                    MONO8 | 0x0110_0003 | 0x0110_0005 | 0x0110_0007
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

pub fn save(frame: &Frame, path: &Path, format: &str) -> Result<()> {
    crate::storage::save(
        frame,
        path,
        format,
        &crate::storage::StoragePolicy::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
            let (w, h, rgba) = preview_rgba(&frame, 10, 10).unwrap();
            assert_eq!((w, h), (9, 5));
            for (i, p) in rgba.as_chunks::<4>().0.iter().enumerate() {
                let source = (i / 9 * 4 * 33 + i % 9 * 4) * 3;
                assert_eq!(p[..3], full[source..source + 3]);
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
