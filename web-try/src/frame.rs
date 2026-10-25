//! Port of the native `src/frame.rs`: PFNC names, bounded conversions to
//! RGB/gray/RGBA, the sparse exposure statistics, the focus measures and
//! the raw/PPM/PGM still encoders.
//!
//! One deliberate difference: the native Bayer demosaic splits frames of at
//! least 512 Ki pixels into row blocks across threads. Each output row reads
//! only immutable input, so the result does not depend on the thread count;
//! this port always runs the single-thread path (there are no threads here).
use crate::Result;
use crate::types::{Frame, MONO8, RGB8};

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

/// Interleaved 8-bit RGB of any convertible frame.
pub fn rgb(frame: &Frame) -> Result<Vec<u8>> {
    let pixels = checked_pixels(frame)?;
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
            let shift = unpacked_shift(frame.pixel_format).unwrap_or(8);
            let data = &frame.data[..pixels * 2];
            gray(&mut out, |i| {
                (u16::from_le_bytes([data[2 * i], data[2 * i + 1]]) >> shift).min(255) as u8
            });
        }
        _ => return Ok(convert(frame, |p| p)?.into_flattened()),
    }
    Ok(out)
}

/// How far an unpacked 16-bit monochrome sample shifts right to its top 8
/// bits: Mono10 keeps bits 9-2, Mono12 bits 11-4, the 16-bit formats their
/// high byte. None for every other format.
fn unpacked_shift(format: u32) -> Option<u32> {
    match format {
        0x0110_0003 => Some(2),
        0x0110_0005 => Some(4),
        0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => Some(8),
        _ => None,
    }
}

/// One 8-bit sample per pixel for the monochrome family, with the same bit
/// choice as [`rgb`]; None for color and Bayer formats, which stay RGB.
pub fn gray8(frame: &Frame) -> Result<Option<Vec<u8>>> {
    let pixels = checked_pixels(frame)?;
    if matches!(frame.pixel_format, MONO8 | 0x0108_00b1 | 0x0108_00c6) {
        ensure!(frame.data.len() >= pixels, "truncated Mono8 frame");
        return Ok(Some(frame.data[..pixels].to_vec()));
    }
    let Some(shift) = unpacked_shift(frame.pixel_format) else {
        return Ok(None);
    };
    ensure!(
        frame.data.len() >= pixels * 2,
        "truncated unpacked monochrome frame"
    );
    let samples = frame.data[..pixels * 2].as_chunks::<2>().0;
    Ok(Some(
        samples
            .iter()
            .map(|p| (u16::from_le_bytes(*p) >> shift).min(255) as u8)
            .collect(),
    ))
}

/// Opaque RGBA bytes, converted in one pass without an intermediate RGB image.
pub fn rgba(frame: &Frame) -> Result<Vec<u8>> {
    Ok(convert(frame, |[r, g, b]| [r, g, b, 255])?.into_flattened())
}

/// Pixels in the frame, at most 64 Mi. Computed in 64 bits, as on the
/// native 64-bit hosts, so a huge size reports the same error here.
pub fn checked_pixels(frame: &Frame) -> Result<usize> {
    let pixels = u64::from(frame.width) * u64::from(frame.height);
    ensure!(
        pixels > 0 && pixels <= 64 * 1024 * 1024,
        "image dimensions exceed conversion limit"
    );
    Ok(pixels as usize)
}

/// Convert a PFNC frame into any pixel representation; `pixel` maps each
/// interpolated RGB triple.
pub fn convert<P: Copy + Default>(frame: &Frame, pixel: impl Fn([u8; 3]) -> P) -> Result<Vec<P>> {
    let pixels = checked_pixels(frame)?;
    let mut out = Vec::new();
    match frame.pixel_format {
        MONO8 | 0x0108_00b1 | 0x0108_00c6 => {
            ensure!(frame.data.len() >= pixels, "truncated Mono8 frame");
            out.extend(frame.data[..pixels].iter().map(|&v| pixel([v, v, v])));
        }
        RGB8 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let source = frame.data[..pixels * 3].as_chunks::<3>().0;
            out.extend(source.iter().map(|p| pixel([p[0], p[1], p[2]])));
        }
        0x0218_0015 => {
            ensure!(frame.data.len() >= pixels * 3, "truncated RGB frame");
            let source = frame.data[..pixels * 3].as_chunks::<3>().0;
            out.extend(source.iter().map(|p| pixel([p[2], p[1], p[0]])));
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
            out.extend(source.iter().map(|&p| {
                let v = (u16::from_le_bytes(p) >> shift).min(255) as u8;
                pixel([v, v, v])
            }));
        }
        0x0108_0008..=0x0108_000b => {
            ensure!(frame.data.len() >= pixels, "truncated Bayer8 frame");
            out.resize(pixels, P::default());
            let pattern = bayer_pattern(frame.pixel_format);
            let (w, h, data) = (frame.width as usize, frame.height as usize, &frame.data);
            let color = |x: usize, y: usize| pattern[(y & 1) * 2 + (x & 1)];
            let bounded = |x: usize, y: usize| pixel(bayer_bounded(frame, pattern, x, y));
            for (y, line) in out.chunks_exact_mut(w).enumerate() {
                if y == 0 || y + 1 == h || w < 3 {
                    for (x, px) in line.iter_mut().enumerate() {
                        *px = bounded(x, y);
                    }
                    continue;
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
            }
        }
        v => {
            bail!("unsupported PFNC format 0x{v:08x}; capture with --format raw to preserve bytes")
        }
    }
    Ok(out)
}

pub fn is_bayer(pixel_format: u32) -> bool {
    matches!(pixel_format, 0x0108_0008..=0x0108_000b)
}

/// A Bayer frame read as whole 2x2 quads, one color each: red, the mean of
/// both greens, blue.
struct Quads<'a> {
    data: &'a [u8],
    width: usize,
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

/// Where red, the two greens and blue sit among a quad's samples.
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
/// missing color.
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
pub fn validated_bytes_per_pixel(frame: &Frame) -> Result<usize> {
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

/// One pixel exactly as `convert` would produce it. The frame must have
/// passed `validated_bytes_per_pixel`.
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

/// How a frame is exposed, from evenly strided pixels (whole quads of a
/// Bayer mosaic) decoded individually.
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

/// The step is a floor division, so up to 131,071 pixels are sampled; this
/// matches the native code exactly.
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
/// sharper, in 8-bit luminance units.
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

/// Sharpness of the `[x, y, width, height]` pixel region, clamped to the
/// frame. A Bayer mosaic is measured over its 2x2 quads.
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
/// luminance at (x, y) is `at`. Coordinates are 64-bit here so a region
/// near `u32::MAX` clamps exactly as on a 64-bit host.
fn sharpness_over(
    (w, h): (usize, usize),
    region: [u32; 4],
    at: impl Fn(usize, usize) -> f64,
) -> Sharpness {
    let [x, y, width, height] = region.map(u64::from);
    let (w64, h64) = (w as u64, h as u64);
    // Keep a one-pixel border so every 3×3 neighbourhood is inside the frame.
    let x0 = x.clamp(1, w64 - 2);
    let y0 = y.clamp(1, h64 - 2);
    let x1 = (x + width).clamp(x0 + 1, w64 - 1);
    let y1 = (y + height).clamp(y0 + 1, h64 - 1);
    let (x0, y0, x1, y1) = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
    let area = (x1 - x0) as f64 * (y1 - y0) as f64;
    let step = (area / 65_536.0).sqrt().ceil().max(1.0) as usize;
    let (mut n, mut lap, mut lap2, mut ten, mut bren, mut sum, mut sum2) =
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    // Shift each row and column of the sampling grid by a different phase
    // within the stride, so periodic detail is never sampled at only one of
    // its phases.
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
    let mean = sum / n;
    let variance = (sum2 / n - mean * mean).max(0.0);
    Sharpness {
        laplacian: (lap2 / n - (lap / n).powi(2)).max(0.0) as f32,
        tenengrad: (ten / n) as f32,
        brenner: (bren / n) as f32,
        variance: if mean > 0.0 { variance / mean } else { 0.0 } as f32,
    }
}

/// The most bytes of provenance JSON a saved still carries (`frame.rs`
/// `MAX_META`).
pub const MAX_META: usize = 16 * 1024;

/// `encode_with_meta`'s first check, applied to every format.
pub fn check_meta(json_len: usize) -> Result<()> {
    ensure!(
        json_len <= MAX_META,
        "capture metadata exceeds {MAX_META} bytes"
    );
    Ok(())
}

/// The raw, PPM and PGM branches of `encode_with_meta`. PNG and JPEG are
/// assembled from browser compression; see [`crate::png`] and
/// [`crate::jpeg`].
pub fn encode(frame: &Frame, format: &str) -> Result<Vec<u8>> {
    match format {
        "raw" => Ok(frame.data.clone()),
        "ppm" => {
            let mut bytes = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            bytes.extend_from_slice(&rgb(frame)?);
            Ok(bytes)
        }
        "pgm" => {
            let Some(gray) = gray8(frame)? else {
                bail!("PGM requires a monochrome image; use PNG or PPM");
            };
            let mut bytes = format!("P5\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            bytes.extend_from_slice(&gray);
            Ok(bytes)
        }
        _ => bail!("unknown image format {format}; use png, jpeg, raw, pgm or ppm"),
    }
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
        assert_eq!(
            rgb(&f(0, vec![0; 6])).unwrap_err(),
            "unsupported PFNC format 0x00000000; capture with --format raw to preserve bytes"
        );
        for (format, shift) in [(0x0110_0003, 2), (0x0110_0005, 4), (0x0110_0007, 8)] {
            let data = [(2u16 << shift) - 1, u16::MAX]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect();
            assert_eq!(rgb(&f(format, data)).unwrap(), [1, 1, 1, 255, 255, 255]);
            assert_eq!(
                rgb(&f(format, vec![0; 3])).unwrap_err(),
                "truncated unpacked monochrome frame"
            );
        }
        assert_eq!(
            rgb(&f(RGB8, vec![1, 2, 3, 4, 5, 6, 7])).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        let huge = Frame {
            width: u32::MAX,
            height: u32::MAX,
            ..f(MONO8, vec![])
        };
        assert_eq!(
            rgb(&huge).unwrap_err(),
            "image dimensions exceed conversion limit"
        );
        for format in [0x0110_00b8, 0x0110_00c7] {
            let source = f(format, vec![0x34, 0x12, 0xff, 0xff]);
            assert_eq!(rgb(&source).unwrap(), vec![0x12, 0x12, 0x12, 255, 255, 255]);
            assert_eq!(encode(&source, "raw").unwrap(), source.data);
            assert!(convertible(format));
            assert!(exposure(&source).unwrap().channels.is_none());
        }
    }

    /// `frame.rs`' reference bilinear demosaic, which the fast path matches.
    fn reference_bayer(frame: &Frame) -> Vec<u8> {
        let pattern = bayer_pattern(frame.pixel_format);
        let (w, h) = (frame.width as i32, frame.height as i32);
        let mut out = vec![0; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let own = pattern[((y & 1) * 2 + (x & 1)) as usize];
                    let v = if own == c {
                        frame.data[(y * w + x) as usize]
                    } else {
                        let (mut sum, mut n) = (0u32, 0);
                        for dy in -1..=1 {
                            for dx in -1..=1 {
                                let (xx, yy) = (x + dx, y + dy);
                                if xx >= 0
                                    && yy >= 0
                                    && xx < w
                                    && yy < h
                                    && pattern[((yy & 1) * 2 + (xx & 1)) as usize] == c
                                {
                                    sum += frame.data[(yy * w + xx) as usize] as u32;
                                    n += 1;
                                }
                            }
                        }
                        sum.checked_div(n)
                            .map_or(frame.data[(y * w + x) as usize], |v| v as u8)
                    };
                    out[((y * w + x) * 3 + c as i32) as usize] = v;
                }
            }
        }
        out
    }

    #[test]
    fn bayer_matches_reference_bilinear() {
        let mut state = 0x2545_f491_u32;
        for format in 0x0108_0008..=0x0108_000b {
            for (w, h) in [(1, 1), (2, 2), (3, 5), (8, 6), (31, 17)] {
                let data = (0..w * h)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        state as u8
                    })
                    .collect();
                let frame = Frame {
                    width: w,
                    height: h,
                    pixel_format: format,
                    data,
                    ..Frame::default()
                };
                assert_eq!(rgb(&frame).unwrap(), reference_bayer(&frame), "{w}x{h}");
            }
        }
    }

    #[test]
    fn exposure_reports_channels_clipping_and_mean() {
        let frame = Frame {
            width: 4,
            height: 1,
            pixel_format: RGB8,
            data: vec![0, 0, 0, 255, 255, 255, 128, 128, 128, 255, 0, 0],
            ..Frame::default()
        };
        let exposure = exposure(&frame).unwrap();
        let channels = exposure.channels.unwrap();
        assert_eq!(channels[0][63], 2);
        assert_eq!(channels[1][63], 1);
        assert_eq!(channels[2][0], 2);
        assert_eq!((exposure.shadows, exposure.highlights), (0.25, 0.5));
        assert!((exposure.mean - (255.0 + 128.0 + 53.0) / 4.0 / 255.0).abs() < 1e-6);
        let mono = super::exposure(&f(MONO8, vec![0, 255])).unwrap();
        assert!(mono.channels.is_none());
        assert_eq!((mono.shadows, mono.highlights), (0.5, 0.5));
        assert!(super::exposure(&f(0, vec![0; 6])).is_err());
        assert_eq!(
            super::exposure(&f(RGB8, vec![0; 5])).unwrap_err(),
            "truncated RGB frame"
        );
    }

    #[test]
    fn sharpness_prefers_crisp_detail_in_the_region() {
        let flat = Frame {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            data: vec![90; 256],
            ..Frame::default()
        };
        assert_eq!(
            sharpness(&flat, [0, 0, 16, 16]).unwrap(),
            Sharpness::default()
        );
        assert_eq!(
            sharpness(&f(MONO8, vec![1, 2]), [0, 0, 2, 1]).unwrap_err(),
            "frame is too small to measure sharpness"
        );
        let checker = Frame {
            width: 64,
            height: 64,
            pixel_format: MONO8,
            data: (0..64 * 64)
                .map(|i| if (i % 64 + i / 64) % 2 == 0 { 0 } else { 255 })
                .collect(),
            ..Frame::default()
        };
        let crisp = sharpness(&checker, [0, 0, 64, 64]).unwrap();
        assert!(crisp.laplacian > 0.0 && crisp.variance > 0.0);
        // An out-of-range region is clamped, not rejected.
        assert_eq!(
            sharpness(&checker, [u32::MAX, u32::MAX, u32::MAX, u32::MAX]).unwrap(),
            sharpness(&checker, [62, 62, 1, 1]).unwrap()
        );
    }

    #[test]
    fn encoders_write_headers_and_refuse_color_pgm() {
        let mono = f(MONO8, vec![7, 9]);
        assert_eq!(encode(&mono, "pgm").unwrap(), b"P5\n2 1\n255\n\x07\x09");
        assert_eq!(
            encode(&mono, "ppm").unwrap(),
            b"P6\n2 1\n255\n\x07\x07\x07\x09\x09\x09"
        );
        let color = f(RGB8, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            encode(&color, "pgm").unwrap_err(),
            "PGM requires a monochrome image; use PNG or PPM"
        );
        assert_eq!(
            encode(&color, "tiff").unwrap_err(),
            "unknown image format tiff; use png, jpeg, raw, pgm or ppm"
        );
        assert_eq!(check_meta(MAX_META), Ok(()));
        assert_eq!(
            check_meta(MAX_META + 1).unwrap_err(),
            "capture metadata exceeds 16384 bytes"
        );
    }
}
