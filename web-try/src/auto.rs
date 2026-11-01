//! Port of `auto::meter` from the native `src/auto.rs`: brightness as auto
//! exposure meters it, at the camera's native bit depth.
use crate::types::Frame;

/// The mean luminance and the 99th percentile of each sample's brightest
/// channel, 0–1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meter {
    pub mean: f64,
    pub p99: f64,
}

/// None for formats auto exposure does not meter (Coord3D, Confidence,
/// Mono16-family scalars other than Mono16) and for truncated frames. Sizes
/// are 64-bit as on native hosts.
pub fn meter(frame: &Frame) -> Option<Meter> {
    // A meter only reads its sampling grid. Packed mono and Bayer samples
    // can be decoded there directly, without a full-sized temporary image.
    crate::frame::validate_source(frame).ok()?;
    let packed = crate::frame::packing(frame.pixel_format);
    let color_bits = crate::frame::native_color(frame, 0, 0).map(|(_, bits)| bits);
    let normalized;
    let frame = if packed.is_some() || color_bits.is_some() {
        frame
    } else {
        normalized = crate::frame::unpacked(frame).ok()?;
        &*normalized
    };
    let (w, h) = (frame.width as usize, frame.height as usize);
    let format = packed.map_or(frame.pixel_format, |(format, ..)| format);
    let (bytes, bits, quad) = if let Some(bits) = color_bits {
        (3, bits, false)
    } else {
        match format {
            crate::types::MONO8 => (1, 8, false),
            crate::types::RGB8 | 0x0218_0015 => (3, 8, false),
            0x0110_0003 => (2, 10, false),
            0x0110_0005 => (2, 12, false),
            0x0110_0025 => (2, 14, false),
            0x0110_0007 => (2, 16, false),
            0x0108_0008..=0x0108_000b => (1, 8, true),
            0x0110_000c..=0x0110_000f => (2, 10, true),
            0x0110_0010..=0x0110_0013 => (2, 12, true),
            0x0110_002e..=0x0110_0031 => (2, 16, true),
            0x0110_0109..=0x0110_010c => (2, 14, true),
            _ => return None,
        }
    };
    let (gw, gh) = if quad { (w / 2, h / 2) } else { (w, h) };
    if gw == 0 || gh == 0 {
        return None;
    }
    let scale = ((1u32 << bits) - 1) as f64;
    let d = &frame.data;
    let sample = |x, y| f64::from(crate::frame::native_sample(frame, x, y));
    let (mut sum, mut count, mut histogram) = (0.0, 0u32, [0u32; 256]);
    for i in crate::frame::sample_indices(gw * gh, 16_384) {
        let (x, y) = (i % gw, i / gw);
        let (luma, peak) = if quad {
            let (x, y) = (2 * x, 2 * y);
            let q = [
                sample(x, y),
                sample(x + 1, y),
                sample(x, y + 1),
                sample(x + 1, y + 1),
            ];
            (
                q.iter().sum::<f64>() / 4.0,
                q.into_iter().fold(0.0, f64::max),
            )
        } else if bytes == 3 {
            let [r, g, b] = if color_bits.is_some() {
                crate::frame::native_color(frame, x, y)?.0.map(f64::from)
            } else {
                let i = 3 * (y * w + x);
                [d[i], d[i + 1], d[i + 2]].map(f64::from)
            };
            ((r + 2.0 * g + b) / 4.0, r.max(g).max(b))
        } else {
            let v = sample(x, y);
            (v, v)
        };
        sum += (luma / scale).min(1.0);
        count += 1;
        histogram[((peak / scale).min(1.0) * 255.0).round() as usize] += 1;
    }
    let mut seen = 0;
    let p99 = histogram
        .iter()
        .position(|n| {
            seen += n;
            seen as f64 >= 0.99 * count as f64
        })
        .unwrap_or(255);
    Some(Meter {
        mean: (sum / count as f64).max(1.0 / 1024.0),
        p99: p99 as f64 / 255.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MONO8, RGB8};

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * b.abs().max(1.0)
    }

    fn frame(pixel_format: u32, width: u32, height: u32, data: Vec<u8>) -> Frame {
        Frame {
            width,
            height,
            pixel_format,
            data,
            ..Frame::default()
        }
    }

    /// `auto.rs` `meter_handles_formats_without_bayer_aliasing`.
    #[test]
    fn meter_handles_formats_without_bayer_aliasing() {
        let m = meter(&frame(MONO8, 4, 4, vec![128; 16])).unwrap();
        assert!(near(m.mean, 128.0 / 255.0) && near(m.p99, 128.0 / 255.0));
        let m = meter(&frame(RGB8, 2, 1, vec![255, 0, 0, 255, 0, 0])).unwrap();
        assert!(near(m.mean, 0.25) && near(m.p99, 1.0));
        let mosaic = (0..512 * 512)
            .map(|i| {
                let (x, y) = (i % 512, i / 512);
                if x % 2 == 0 && y % 2 == 0 { 255 } else { 0 }
            })
            .collect();
        let m = meter(&frame(0x0108_0009, 512, 512, mosaic)).unwrap();
        assert!(near(m.mean, 0.25) && near(m.p99, 1.0));
        let m = meter(&frame(MONO8, 2, 2, vec![0; 4])).unwrap();
        assert_eq!(m.mean, 1.0 / 1024.0);
        assert!(meter(&frame(MONO8, 4, 4, vec![0; 15])).is_none());
        let packed = meter(&frame(0x010c_0047, 4, 4, vec![0; 24])).unwrap();
        assert_eq!(packed.mean, 1.0 / 1024.0);
        assert_eq!(packed.p99, 0.0);
        assert!(meter(&frame(0x010c_0047, 4, 4, vec![0; 23])).is_none());
    }

    #[test]
    fn meter_does_not_alias_checkerboards_or_single_pixel_stripes() {
        let (width, height) = (1024usize, 1024usize);
        for (format, channels, unit) in [(MONO8, 1, 1), (RGB8, 3, 1), (0x0108_0009, 1, 2)] {
            // Bayer detail is one color quad per cell, so its metering grid
            // sees the same alternating cells as the scalar/color grids.
            for pattern in 0..3 {
                for phase in 0..2 {
                    let data = (0..width * height)
                        .flat_map(|i| {
                            let (x, y) = (i % width / unit, i / width / unit);
                            let cell = match pattern {
                                0 => x + y,
                                1 => x,
                                _ => y,
                            };
                            let value = if (cell + phase).is_multiple_of(2) {
                                0
                            } else {
                                255
                            };
                            std::iter::repeat_n(value, channels)
                        })
                        .collect();
                    let measured =
                        meter(&frame(format, width as u32, height as u32, data)).unwrap();
                    assert!(
                        (measured.mean - 0.5).abs() < 0.02,
                        "{format:08x}, pattern {pattern}, phase {phase}: {measured:?}"
                    );
                    assert_eq!(measured.p99, 1.0);
                }
            }
        }
    }

    #[test]
    fn wide_rgb_meter_keeps_precision_below_the_display_byte() {
        let data = [0x8001u16; 6]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        let measurement = meter(&frame(0x0230_0033, 2, 1, data)).unwrap();
        assert!(near(measurement.mean, 32769.0 / 65535.0));
    }
}
