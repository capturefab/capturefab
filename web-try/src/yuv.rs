//! Video frame pixels to tightly packed RGB8, for browsers whose
//! `VideoFrame.copyTo` cannot convert to RGBA (it copies the camera's native
//! I420 or NV12 planes instead), and for RGBA/BGRA/RGBX/BGRX copies.
//!
//! YUV follows the frame's colour space: BT.601, BT.709 or BT.2020
//! non-constant-luminance coefficients, limited (16–235 luma, 16–240
//! chroma) or full range. 4:2:0 chroma is taken from the sample covering
//! each pixel, without interpolation. Arithmetic is fixed point with 16
//! fraction bits and rounds half up, which agrees with the exact formula
//! rounded to nearest.
//!
//! Browsers convert with their own code, so the same camera picture can
//! differ by a level or so between this path and the RGBA path.
use crate::Result;

/// YUV→RGB coefficients for one matrix and range, in 1/65536 units:
/// luma scale, Cr→R, Cb→G, Cr→G, Cb→B.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Coefficients {
    luma_offset: i32,
    y: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

/// Which YUV matrix a frame uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Matrix {
    /// BT.601 (`bt470bg`, `smpte170m`).
    Bt601,
    /// BT.709, also the default when a frame reports no colour space.
    #[default]
    Bt709,
    /// BT.2020 non-constant luminance.
    Bt2020,
}

impl Matrix {
    /// The ABI's numbering: 0 BT.601, 1 BT.709, 2 BT.2020; anything else is
    /// BT.709.
    pub fn from_code(code: u32) -> Self {
        match code {
            0 => Self::Bt601,
            2 => Self::Bt2020,
            _ => Self::Bt709,
        }
    }

    fn coefficients(self, full_range: bool) -> Coefficients {
        // round(65536 × [255/219 or 1, 2(1−Kr)·s, 2Kb(1−Kb)/Kg·s,
        // 2Kr(1−Kr)/Kg·s, 2(1−Kb)·s]) with s = 255/224 (limited) or 1.
        let [y, rv, gu, gv, bu] = match (self, full_range) {
            (Self::Bt601, false) => [76309, 104597, 25675, 53279, 132201],
            (Self::Bt601, true) => [65536, 91881, 22553, 46802, 116130],
            (Self::Bt709, false) => [76309, 117489, 13975, 34925, 138438],
            (Self::Bt709, true) => [65536, 103206, 12276, 30679, 121609],
            (Self::Bt2020, false) => [76309, 110014, 12277, 42626, 140363],
            (Self::Bt2020, true) => [65536, 96639, 10784, 37444, 123299],
        };
        Coefficients {
            luma_offset: if full_range { 0 } else { 16 },
            y,
            rv,
            gu,
            gv,
            bu,
        }
    }
}

/// Where one plane starts in the copied buffer and its row pitch in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plane {
    pub offset: usize,
    pub stride: usize,
}

/// How the chroma of a 4:2:0 frame is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Separate U and V planes (I420, and I420A whose alpha is ignored).
    I420 { u: Plane, v: Plane },
    /// One plane of interleaved U, V pairs.
    Nv12 { uv: Plane },
}

fn clamp8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn check_size(width: u32, height: u32) -> Result<usize> {
    let pixels = u64::from(width) * u64::from(height);
    ensure!(
        pixels > 0 && pixels <= 64 * 1024 * 1024,
        "image dimensions exceed conversion limit"
    );
    Ok(pixels as usize)
}

/// Whether `rows` rows of `row` bytes at `plane` fit in `len` bytes.
fn fits(plane: Plane, row: usize, rows: usize, len: usize) -> bool {
    plane.stride >= row
        && plane
            .stride
            .checked_mul(rows - 1)
            .and_then(|v| v.checked_add(plane.offset))
            .and_then(|v| v.checked_add(row))
            .is_some_and(|end| end <= len)
}

const PLANES_DO_NOT_FIT: &str = "the video frame's planes do not fit its copied pixels";

/// A 4:2:0 frame (`width` × `height`, any parity) as RGB8.
pub fn yuv420_to_rgb(
    source: &[u8],
    width: u32,
    height: u32,
    luma: Plane,
    layout: Layout,
    matrix: Matrix,
    full_range: bool,
) -> Result<Vec<u8>> {
    let pixels = check_size(width, height)?;
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    ensure!(fits(luma, w, h, source.len()), "{PLANES_DO_NOT_FIT}");
    match layout {
        Layout::I420 { u, v } => ensure!(
            fits(u, cw, ch, source.len()) && fits(v, cw, ch, source.len()),
            "{PLANES_DO_NOT_FIT}"
        ),
        Layout::Nv12 { uv } => ensure!(fits(uv, 2 * cw, ch, source.len()), "{PLANES_DO_NOT_FIT}"),
    }
    let k = matrix.coefficients(full_range);
    let mut out = vec![0; pixels * 3];
    for (y, line) in out.chunks_exact_mut(w * 3).enumerate() {
        let luma_row = &source[luma.offset + y * luma.stride..][..w];
        let (us, vs, step): (&[u8], &[u8], usize) = match layout {
            Layout::I420 { u, v } => (
                &source[u.offset + (y / 2) * u.stride..][..cw],
                &source[v.offset + (y / 2) * v.stride..][..cw],
                1,
            ),
            Layout::Nv12 { uv } => {
                let row = &source[uv.offset + (y / 2) * uv.stride..][..2 * cw];
                (row, &row[1..], 2)
            }
        };
        for (x, rgb) in line.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let c = (x / 2) * step;
            let luma = (i32::from(luma_row[x]) - k.luma_offset) * k.y + 32768;
            let (u, v) = (i32::from(us[c]) - 128, i32::from(vs[c]) - 128);
            rgb[0] = clamp8((luma + k.rv * v) >> 16);
            rgb[1] = clamp8((luma - k.gu * u - k.gv * v) >> 16);
            rgb[2] = clamp8((luma + k.bu * u) >> 16);
        }
    }
    Ok(out)
}

/// Four bytes per pixel, rows `stride` bytes apart, as RGB8: alpha (or the
/// padding byte) is dropped; `bgr` swaps the first and third channels.
pub fn rgbx_to_rgb(
    source: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    bgr: bool,
) -> Result<Vec<u8>> {
    let pixels = check_size(width, height)?;
    let (w, h) = (width as usize, height as usize);
    ensure!(
        fits(Plane { offset: 0, stride }, 4 * w, h, source.len()),
        "{PLANES_DO_NOT_FIT}"
    );
    let mut out = vec![0; pixels * 3];
    for (y, line) in out.chunks_exact_mut(w * 3).enumerate() {
        let row = &source[y * stride..][..4 * w];
        let pixels = row.as_chunks::<4>().0;
        for (rgb, p) in line.as_chunks_mut::<3>().0.iter_mut().zip(pixels) {
            *rgb = if bgr {
                [p[2], p[1], p[0]]
            } else {
                [p[0], p[1], p[2]]
            };
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(y: u8, u: u8, v: u8, matrix: Matrix, full: bool) -> [u8; 3] {
        let source = [y, u, v];
        let out = yuv420_to_rgb(
            &source,
            1,
            1,
            Plane {
                offset: 0,
                stride: 1,
            },
            Layout::I420 {
                u: Plane {
                    offset: 1,
                    stride: 1,
                },
                v: Plane {
                    offset: 2,
                    stride: 1,
                },
            },
            matrix,
            full,
        )
        .unwrap();
        [out[0], out[1], out[2]]
    }

    /// Expected values from the floating-point formulas, rounded by hand.
    #[test]
    fn matrices_and_ranges_match_the_formulas() {
        use Matrix::*;
        for (yuv, matrix, full, rgb) in [
            ((16, 128, 128), Bt709, false, [0, 0, 0]),
            ((235, 128, 128), Bt709, false, [255, 255, 255]),
            ((126, 128, 128), Bt709, false, [128, 128, 128]),
            ((63, 102, 240), Bt709, false, [255, 1, 0]),
            ((173, 42, 26), Bt709, false, [0, 255, 1]),
            ((32, 240, 118), Bt709, false, [1, 0, 255]),
            ((63, 102, 240), Bt709, true, [239, 15, 15]),
            ((173, 42, 26), Bt709, true, [12, 237, 13]),
            ((16, 128, 128), Bt709, true, [16, 16, 16]),
            ((63, 102, 240), Bt601, false, [233, 0, 2]),
            ((173, 42, 26), Bt601, false, [20, 255, 9]),
            ((32, 240, 118), Bt601, true, [18, 1, 230]),
            ((63, 102, 240), Bt2020, false, [243, 0, 0]),
            ((32, 240, 118), Bt2020, true, [17, 19, 243]),
            ((0, 0, 0), Bt709, false, [0, 77, 0]),
            ((255, 255, 255), Bt709, true, [255, 172, 255]),
        ] {
            assert_eq!(
                one(yuv.0, yuv.1, yuv.2, matrix, full),
                rgb,
                "{yuv:?} {matrix:?} {full}"
            );
        }
        assert_eq!(Matrix::from_code(9), Matrix::Bt709);
    }

    #[test]
    fn nv12_and_i420_agree_with_odd_sizes_and_padding() {
        // 3 × 3 luma with a stride of 4; 2 × 2 chroma.
        let (w, h) = (3u32, 3u32);
        let luma: Vec<u8> = (0..12).map(|i| 40 + 10 * i as u8).collect();
        let (u, v) = ([90u8, 100, 110, 120], [140u8, 150, 160, 170]);
        let mut i420 = luma.clone();
        i420.extend_from_slice(&[u[0], u[1], 0, u[2], u[3], 0]);
        i420.extend_from_slice(&[v[0], v[1], v[2], v[3]]);
        let mut nv12 = luma.clone();
        nv12.extend_from_slice(&[u[0], v[0], u[1], v[1], 0, 0, u[2], v[2], u[3], v[3], 0, 0]);
        let luma_plane = Plane {
            offset: 0,
            stride: 4,
        };
        let a = yuv420_to_rgb(
            &i420,
            w,
            h,
            luma_plane,
            Layout::I420 {
                u: Plane {
                    offset: 12,
                    stride: 3,
                },
                v: Plane {
                    offset: 18,
                    stride: 2,
                },
            },
            Matrix::Bt601,
            false,
        )
        .unwrap();
        let b = yuv420_to_rgb(
            &nv12,
            w,
            h,
            luma_plane,
            Layout::Nv12 {
                uv: Plane {
                    offset: 12,
                    stride: 6,
                },
            },
            Matrix::Bt601,
            false,
        )
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 27);
        // Pixel (2, 2) uses luma 40 + 10 × 10 and the last chroma pair.
        assert_eq!(&a[24..27], &one(140, u[3], v[3], Matrix::Bt601, false));
        // A plane past the end is refused.
        let short = yuv420_to_rgb(
            &nv12[..20],
            w,
            h,
            luma_plane,
            Layout::Nv12 {
                uv: Plane {
                    offset: 12,
                    stride: 6,
                },
            },
            Matrix::Bt601,
            false,
        );
        assert_eq!(short.unwrap_err(), PLANES_DO_NOT_FIT);
    }

    #[test]
    fn four_byte_pixels_drop_alpha_and_honour_stride() {
        let source = [
            1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 7, 8, 9, 0, 10, 11, 12, 0, 9, 9,
        ];
        assert_eq!(
            rgbx_to_rgb(&source, 2, 2, 10, false).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(
            rgbx_to_rgb(&source, 2, 2, 10, true).unwrap(),
            [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]
        );
        assert!(rgbx_to_rgb(&source, 2, 2, 7, false).is_err());
        assert!(rgbx_to_rgb(&source, 3, 2, 10, false).is_err());
    }
}
