//! Port of `auto::meter` from the native `src/auto.rs`: brightness as auto
//! exposure meters it, at the camera's native bit depth.
use crate::types::{Frame, MONO8, RGB8};

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
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (bytes, bits, quad) = match frame.pixel_format {
        MONO8 => (1, 8, false),
        RGB8 | 0x0218_0015 => (3, 8, false),
        0x0110_0003 => (2, 10, false),
        0x0110_0005 => (2, 12, false),
        0x0110_0007 => (2, 16, false),
        0x0108_0008..=0x0108_000b => (1, 8, true),
        _ => return None,
    };
    let (gw, gh) = if quad { (w / 2, h / 2) } else { (w, h) };
    let needed = (w as u64)
        .checked_mul(h as u64)?
        .checked_mul(bytes as u64)?;
    if gw == 0 || gh == 0 || (frame.data.len() as u64) < needed {
        return None;
    }
    let mut stride = 1;
    while gw.div_ceil(stride) * gh.div_ceil(stride) > 16_384 {
        stride += 1;
    }
    let scale = ((1u32 << bits) - 1) as f64;
    let d = &frame.data;
    let px = |i: usize| match bytes {
        2 => u16::from_le_bytes([d[2 * i], d[2 * i + 1]]) as f64,
        _ => d[i] as f64,
    };
    let (mut sum, mut count, mut histogram) = (0.0, 0u32, [0u32; 256]);
    for y in (0..gh).step_by(stride) {
        for x in (0..gw).step_by(stride) {
            let (luma, peak) = if quad {
                let i = 2 * (y * w + x);
                let q = [px(i), px(i + 1), px(i + w), px(i + w + 1)];
                (
                    q.iter().sum::<f64>() / 4.0,
                    q.into_iter().fold(0.0, f64::max),
                )
            } else if bytes == 3 {
                let i = 3 * (y * w + x);
                let (r, g, b) = (px(i), px(i + 1), px(i + 2));
                ((r + 2.0 * g + b) / 4.0, r.max(g).max(b))
            } else {
                let v = px(y * w + x);
                (v, v)
            };
            sum += (luma / scale).min(1.0);
            count += 1;
            histogram[((peak / scale).min(1.0) * 255.0).round() as usize] += 1;
        }
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
        assert!(meter(&frame(0x010c_0047, 4, 4, vec![0; 64])).is_none());
    }
}
