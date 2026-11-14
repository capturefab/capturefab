//! PFNC `BiColor_LMNO`, with two received components at every pixel location.
//!
//! PFNC2.3/2.5 section3.1.8 restarts the LM/NO sequence on every row. Unpacked
//! codes use byte or individually normalized little-endian u16 components.
//! Packed codes use the LSB-first component stream from section6.3.1. Only
//! even-width10p is qualified: section6.7 leaves non-byte-aligned line/image
//! boundaries to the transport. Every12p location occupies three bytes.
//!
//! Display interpolation is Capturefab policy: keep both measured components,
//! interpolate the absent red/blue from immediate in-view horizontal neighbors,
//! use one neighbor at an edge, and the measured green for a one-column view.
//! This is not a claim of calibrated color or sensor-stage registration.
#![forbid(unsafe_code)]
#![deny(clippy::missing_const_for_fn)]
use crate::types::{Categorized, ErrorKind};
use anyhow::{Result, ensure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Rgbg8,
    Bgrg8,
    Rgbg10,
    Bgrg10,
    Rgbg12,
    Bgrg12,
    Rgbg10p,
    Bgrg10p,
    Rgbg12p,
    Bgrg12p,
}

impl Format {
    pub(crate) const fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0x0210_00a5 => Self::Rgbg8,
            0x0210_00a6 => Self::Bgrg8,
            0x0220_00a7 => Self::Rgbg10,
            0x0220_00a9 => Self::Bgrg10,
            0x0220_00ab => Self::Rgbg12,
            0x0220_00ad => Self::Bgrg12,
            0x0214_00a8 => Self::Rgbg10p,
            0x0214_00aa => Self::Bgrg10p,
            0x0218_00ac => Self::Rgbg12p,
            0x0218_00ae => Self::Bgrg12p,
            _ => return None,
        })
    }

    #[cfg(test)]
    pub(crate) const fn code(self) -> u32 {
        match self {
            Self::Rgbg8 => 0x0210_00a5,
            Self::Bgrg8 => 0x0210_00a6,
            Self::Rgbg10 => 0x0220_00a7,
            Self::Bgrg10 => 0x0220_00a9,
            Self::Rgbg12 => 0x0220_00ab,
            Self::Bgrg12 => 0x0220_00ad,
            Self::Rgbg10p => 0x0214_00a8,
            Self::Bgrg10p => 0x0214_00aa,
            Self::Rgbg12p => 0x0218_00ac,
            Self::Bgrg12p => 0x0218_00ae,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Rgbg8 => "BiColorRGBG8",
            Self::Bgrg8 => "BiColorBGRG8",
            Self::Rgbg10 => "BiColorRGBG10",
            Self::Bgrg10 => "BiColorBGRG10",
            Self::Rgbg12 => "BiColorRGBG12",
            Self::Bgrg12 => "BiColorBGRG12",
            Self::Rgbg10p => "BiColorRGBG10p",
            Self::Bgrg10p => "BiColorBGRG10p",
            Self::Rgbg12p => "BiColorRGBG12p",
            Self::Bgrg12p => "BiColorBGRG12p",
        }
    }

    pub(crate) const fn bits(self) -> u32 {
        match self {
            Self::Rgbg8 | Self::Bgrg8 => 8,
            Self::Rgbg10 | Self::Bgrg10 | Self::Rgbg10p | Self::Bgrg10p => 10,
            Self::Rgbg12 | Self::Bgrg12 | Self::Rgbg12p | Self::Bgrg12p => 12,
        }
    }

    pub(crate) const fn unpacked_pixel_bytes(self) -> usize {
        if self.bits() == 8 { 2 } else { 4 }
    }

    const fn packed(self) -> bool {
        matches!(
            self,
            Self::Rgbg10p | Self::Bgrg10p | Self::Rgbg12p | Self::Bgrg12p
        )
    }

    /// Positive source geometry is checked by the caller. A borrowed view
    /// may have odd width; only the original source row has this restriction.
    pub(crate) fn validate_width(self, width: u32) -> Result<()> {
        ensure!(
            !matches!(self, Self::Rgbg10p | Self::Bgrg10p) || width.is_multiple_of(2),
            Categorized::error(
                ErrorKind::Unsupported,
                "packed BiColor10p requires an even source width; odd-width row and image packing are not qualified"
            )
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) const fn unpacked(self) -> Self {
        match self {
            Self::Rgbg10p => Self::Rgbg10,
            Self::Bgrg10p => Self::Bgrg10,
            Self::Rgbg12p => Self::Rgbg12,
            Self::Bgrg12p => Self::Bgrg12,
            _ => self,
        }
    }

    /// A materialized crop changes only the horizontal alternating phase.
    #[cfg(test)]
    pub(crate) const fn shifted(self, x: usize) -> Self {
        if x.is_multiple_of(2) {
            return self;
        }
        match self {
            Self::Rgbg8 => Self::Bgrg8,
            Self::Bgrg8 => Self::Rgbg8,
            Self::Rgbg10 => Self::Bgrg10,
            Self::Bgrg10 => Self::Rgbg10,
            Self::Rgbg12 => Self::Bgrg12,
            Self::Bgrg12 => Self::Rgbg12,
            Self::Rgbg10p => Self::Bgrg10p,
            Self::Bgrg10p => Self::Rgbg10p,
            Self::Rgbg12p => Self::Bgrg12p,
            Self::Bgrg12p => Self::Rgbg12p,
        }
    }

    /// The received non-green and green components. Caller validates the row
    /// and pixel index first. Padding bits are not part of a native sample;
    /// normalized frame bytes remain unchanged in RAW output. Original wire
    /// bytes and transport padding, when archived, stay in the chunk archive.
    pub(crate) fn pair(self, row: &[u8], x: usize) -> [u16; 2] {
        if self.packed() {
            let bit = 2 * x * self.bits() as usize;
            let &[a, b, c] = row
                .get(bit / 8..)
                .and_then(|row| row.first_chunk::<3>())
                .expect("a complete validated packed BiColor pair");
            let value = u32::from(a) | (u32::from(b) << 8) | (u32::from(c) << 16);
            let mask = u32::from(u16::MAX >> (16 - self.bits()));
            return [
                value >> (bit % 8),
                value >> (bit % 8 + self.bits() as usize),
            ]
            .map(|word| u16::try_from(word & mask).expect("a masked BiColor sample"));
        }
        let at = self.unpacked_pixel_bytes() * x;
        let row = row.get(at..).expect("pixel lies in the validated row");
        if self.bits() == 8 {
            let &[color, green] = row
                .first_chunk::<2>()
                .expect("two validated byte components");
            [u16::from(color), u16::from(green)]
        } else {
            let &[color_lo, color_hi, green_lo, green_hi] = row
                .first_chunk::<4>()
                .expect("two validated sixteen-bit components");
            let mask = u16::MAX >> (16 - self.bits());
            [
                u16::from_le_bytes([color_lo, color_hi]) & mask,
                u16::from_le_bytes([green_lo, green_hi]) & mask,
            ]
        }
    }

    /// Native RGB with only the absent component interpolated. Bounds are
    /// absolute row coordinates of the validated view, and contain x.
    pub(crate) fn rgb(self, row: &[u8], x: usize, bounds: (usize, usize)) -> [u16; 3] {
        let [color, green] = self.pair(row, x);
        let left = x.checked_sub(1).filter(|&x| x >= bounds.0);
        let right = x.checked_add(1).filter(|&x| x < bounds.1);
        let absent = match (left, right) {
            (Some(left), Some(right)) => self.pair(row, left)[0].midpoint(self.pair(row, right)[0]),
            (Some(x), None) | (None, Some(x)) => self.pair(row, x)[0],
            (None, None) => green,
        };
        let red_first = matches!(
            self,
            Self::Rgbg8 | Self::Rgbg10 | Self::Rgbg12 | Self::Rgbg10p | Self::Rgbg12p
        );
        if red_first == x.is_multiple_of(2) {
            [color, green, absent]
        } else {
            [absent, green, color]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{frame, frame::FrameRef, types::Frame};

    fn image(format: Format) -> Frame {
        // Three pixels on each of two rows. Every row restarts its color
        // phase, including the second row after the first row's odd width.
        let pairs = [
            [10_u16, 20],
            [30, 40],
            [50, 60],
            [70, 80],
            [90, 100],
            [110, 120],
        ];
        let data = if format.bits() == 8 {
            pairs
                .into_iter()
                .flatten()
                .map(|word| u8::try_from(word).unwrap())
                .collect()
        } else {
            pairs
                .into_iter()
                .flatten()
                .flat_map(u16::to_le_bytes)
                .collect()
        };
        Frame {
            width: 3,
            height: 2,
            pixel_format: format.code(),
            data,
            ..Frame::default()
        }
    }

    #[test]
    fn finite_pfnc_codes_keep_occupancy_component_depth_and_horizontal_phase() {
        for (code, name, bits, bytes) in [
            (0x0210_00a5, "BiColorRGBG8", 8, 2),
            (0x0210_00a6, "BiColorBGRG8", 8, 2),
            (0x0220_00a7, "BiColorRGBG10", 10, 4),
            (0x0220_00a9, "BiColorBGRG10", 10, 4),
            (0x0220_00ab, "BiColorRGBG12", 12, 4),
            (0x0220_00ad, "BiColorBGRG12", 12, 4),
        ] {
            let format = Format::from_code(code).unwrap();
            assert_eq!(format.code(), code);
            assert_eq!(format.name(), name);
            assert_eq!(format.bits(), bits);
            assert_eq!(format.unpacked_pixel_bytes(), bytes);
            assert_eq!(((code >> 16) & 0xff) as usize, bytes * 8);
            assert_eq!(format.shifted(2), format);
            assert_eq!(format.shifted(1).shifted(1), format);
            assert_ne!(format.shifted(1), format);
        }
        for code in [0x8210_00a5, 0x8214_00a8, 0x8218_00ac] {
            assert_eq!(Format::from_code(code), None);
        }
    }

    #[test]
    fn measured_pairs_are_exact_and_missing_colors_use_only_same_row_view_neighbors() {
        // Three complete pixels: RG, BG, RG. The following bytes are padding.
        let row = [10, 20, 30, 40, 50, 60, 201, 202];
        assert_eq!(Format::Rgbg8.pair(&row, 0), [10, 20]);
        assert_eq!(Format::Rgbg8.rgb(&row, 0, (0, 3)), [10, 20, 30]);
        assert_eq!(Format::Rgbg8.rgb(&row, 1, (0, 3)), [30, 40, 30]);
        assert_eq!(Format::Rgbg8.rgb(&row, 2, (0, 3)), [50, 60, 30]);
        assert_eq!(Format::Bgrg8.rgb(&row, 0, (0, 3)), [30, 20, 10]);
        assert_eq!(Format::Bgrg8.rgb(&row, 2, (0, 3)), [30, 60, 50]);
        // One-column ROI does not read the otherwise available outside pixels.
        assert_eq!(Format::Rgbg8.rgb(&row, 1, (1, 2)), [40, 40, 30]);
        assert_eq!(Format::Rgbg8.rgb(&[7, 9], 0, (0, 1)), [7, 9, 9]);
        assert_eq!(Format::Bgrg8.rgb(&[7, 9], 0, (0, 1)), [9, 9, 7]);
    }

    #[test]
    fn wide_words_mask_only_non_sample_bits_without_mutating_original_bytes() {
        let row = [0xff, 0xff, 0x23, 0xf1, 0x67, 0xf5, 0xab, 0xf9];
        let original = row;
        assert_eq!(Format::Rgbg10.pair(&row, 0), [1023, 291]);
        assert_eq!(Format::Rgbg10.pair(&row, 1), [359, 427]);
        assert_eq!(Format::Rgbg12.pair(&row, 0), [4095, 291]);
        assert_eq!(Format::Rgbg12.pair(&row, 1), [1383, 2475]);
        assert_eq!(row, original);
    }

    #[test]
    fn all_six_formats_convert_rows_views_and_stills_without_changing_raw_bytes() {
        for format in [
            Format::Rgbg8,
            Format::Bgrg8,
            Format::Rgbg10,
            Format::Bgrg10,
            Format::Rgbg12,
            Format::Bgrg12,
        ] {
            let frame = image(format);
            frame::validate_source(&frame).unwrap();
            assert_eq!(frame::frame_format_name(&frame), format.name());
            assert_eq!(frame::frame_sample_bits(&frame), Some(format.bits()));
            assert_eq!(
                frame::row_bytes(3, format.code()),
                3 * format.unpacked_pixel_bytes()
            );
            let mut expected = [
                [10_u16, 20, 30],
                [30, 40, 30],
                [50, 60, 30],
                [70, 80, 90],
                [90, 100, 90],
                [110, 120, 90],
            ];
            if matches!(format, Format::Bgrg8 | Format::Bgrg10 | Format::Bgrg12) {
                for rgb in &mut expected {
                    rgb.swap(0, 2);
                }
            }
            let display = expected
                .iter()
                .flatten()
                .map(|word| u8::try_from(word >> (format.bits() - 8)).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(frame::rgb(&frame).unwrap(), display);
            assert_eq!(frame::encode(&frame, "raw").unwrap(), frame.data.as_ref());
            let png = frame::encode(&frame, "png").unwrap();
            let mut png = png::Decoder::new(png.as_slice()).read_info().unwrap();
            assert_eq!(png.info().color_type, png::ColorType::Rgb);
            let mut decoded = vec![0; png.output_buffer_size()];
            let output = png.next_frame(&mut decoded).unwrap();
            decoded.truncate(output.buffer_size());
            assert_eq!(decoded, display);
            let view = FrameRef::region(&frame, [1, 1, 1, 1]);
            let pixels = frame::Pixels::new(view);
            let expected = if matches!(format, Format::Rgbg8 | Format::Rgbg10 | Format::Rgbg12) {
                [100, 100, 90]
            } else {
                [90, 100, 100]
            };
            assert_eq!(pixels.native_color(0, 0), Some((expected, format.bits())));
            let crop = crate::inspect::crop(&frame, [1, 1, 1, 1]).unwrap();
            assert_eq!(crop.pixel_format, format.shifted(1).code());
            assert_eq!(
                crop.data.as_ref(),
                &frame.data[4 * format.unpacked_pixel_bytes()..5 * format.unpacked_pixel_bytes()]
            );
            assert_eq!(
                frame::native_color(&crop, 0, 0),
                Some((expected, format.bits()))
            );
            #[cfg(feature = "jpeg")]
            {
                let encoded = frame::encode(&frame, "jpeg").unwrap();
                let image = turbojpeg::decompress(&encoded, turbojpeg::PixelFormat::RGB).unwrap();
                assert_eq!((image.width, image.height), (3, 2));
            }
            if format.bits() > 8 {
                let mut gbr = Vec::new();
                frame::gbr_planes(&frame, &mut gbr).unwrap();
                let mut expected_gbr = Vec::new();
                for channel in [1, 2, 0] {
                    for pixel in &expected_native(format) {
                        expected_gbr.extend_from_slice(&pixel[channel].to_le_bytes());
                    }
                }
                assert_eq!(gbr, expected_gbr);
            }
        }
    }

    fn expected_native(format: Format) -> [[u16; 3]; 6] {
        let mut rgb = [
            [10, 20, 30],
            [30, 40, 30],
            [50, 60, 30],
            [70, 80, 90],
            [90, 100, 90],
            [110, 120, 90],
        ];
        if matches!(format, Format::Bgrg8 | Format::Bgrg10 | Format::Bgrg12) {
            for pixel in &mut rgb {
                pixel.swap(0, 2);
            }
        }
        rgb
    }

    #[test]
    fn native_clipping_counts_received_pairs_without_interpolating_neighbor_saturation() {
        let mut source = image(Format::Rgbg10);
        source.width = 2;
        source.height = 1;
        // The blue location's interpolated red is1023 too, but only the
        // first received pair contains a saturated sample.
        source.data = [1023_u16, 2, 2, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        let clipping = frame::clipping(&source).unwrap();
        assert_eq!(clipping.high_px, 1);
        assert_eq!(clipping.low_px, 0);
        assert_eq!(clipping.pixels, 2);
        assert_eq!(clipping.max, 1.0);
        assert!(crate::auto::meter(&source).is_some());
        let raw = frame::encode(&source, "raw").unwrap();
        assert_eq!(raw, source.data.as_ref());
        source.data = raw[..raw.len() - 1].to_vec().into();
        assert!(
            frame::rgb(&source)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
    }

    #[test]
    fn independent_two_component_literals_preserve_odd_x_and_odd_y_views() {
        let literal: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/bicolor/pfnc-literals.json"
        ))
        .unwrap();
        let raw: Vec<u8> = serde_json::from_value(literal["raw_u8"].clone()).unwrap();
        for (format, key) in [(Format::Rgbg8, "rgbg"), (Format::Bgrg8, "bgrg")] {
            let source = Frame {
                width: 3,
                height: 2,
                pixel_format: format.code(),
                data: raw.clone().into(),
                ..Frame::default()
            };
            let expected: Vec<Vec<[u8; 3]>> =
                serde_json::from_value(literal[key]["display_rgb_rows"].clone()).unwrap();
            assert_eq!(
                frame::rgb(&source).unwrap(),
                expected.into_iter().flatten().flatten().collect::<Vec<_>>()
            );
            let cropped = crate::inspect::crop(&source, [1, 0, 2, 2]).unwrap();
            assert_eq!(cropped.pixel_format, format.shifted(1).code());
            assert_eq!(
                crate::types::hex(&cropped.data),
                literal["odd_x_view"]["raw_tight_hex"].as_str().unwrap()
            );
            let name = format!("{key}_source_view_display_rgb_rows");
            let expected: Vec<Vec<[u8; 3]>> =
                serde_json::from_value(literal["odd_x_view"][name].clone()).unwrap();
            let view = FrameRef::region(&source, [1, 0, 2, 2]);
            let pixels = frame::Pixels::new(view);
            for (y, row) in expected.iter().enumerate() {
                for (x, pixel) in row.iter().enumerate() {
                    assert_eq!(pixels.at(x, y), *pixel);
                }
            }
            assert_eq!(
                frame::rgb(&cropped).unwrap(),
                expected.into_iter().flatten().flatten().collect::<Vec<_>>()
            );
            let cropped = crate::inspect::crop(&source, [0, 1, 3, 1]).unwrap();
            assert_eq!(cropped.pixel_format, format.code());
            assert_eq!(
                crate::types::hex(&cropped.data),
                literal["odd_y_view"]["raw_tight_hex"].as_str().unwrap()
            );
        }
    }

    fn packed_literals() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/bicolor/packed-literals.json"
        ))
        .unwrap()
    }

    #[test]
    fn independent_packed_pairs_stills_views_and_native_planes_are_exact() {
        let literal = packed_literals();
        for case in literal["cases"].as_array().unwrap() {
            let width = u32::try_from(case["width"].as_u64().unwrap()).unwrap();
            let height = u32::try_from(case["height"].as_u64().unwrap()).unwrap();
            let raw = crate::types::decode_hex(case["tight_hex"].as_str().unwrap(), 128).unwrap();
            let pairs: Vec<Vec<[u16; 2]>> = serde_json::from_value(case["pairs"].clone()).unwrap();
            let native: Vec<Vec<[u16; 3]>> =
                serde_json::from_value(case["rgbg_native_rgb"].clone()).unwrap();
            for name in case["formats"].as_array().unwrap() {
                let descriptor = literal["formats"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|format| format["name"] == *name)
                    .unwrap();
                let code = u32::try_from(descriptor["code"].as_u64().unwrap()).unwrap();
                let format = Format::from_code(code).unwrap();
                let blue = descriptor["first_measured_color"] == "blue";
                let source = Frame {
                    width,
                    height,
                    pixel_format: code,
                    data: raw.clone().into(),
                    ..Frame::default()
                };
                frame::validate_source(&source).unwrap();
                assert_eq!(format.name(), name.as_str().unwrap());
                assert_eq!(frame::frame_sample_bits(&source), Some(format.bits()));
                assert_eq!(
                    frame::frame_row_bytes(&source),
                    usize::try_from(case["row_bytes"].as_u64().unwrap()).unwrap()
                );
                assert_eq!((code >> 16) & 0xff, 2 * format.bits());
                assert_eq!(frame::encode(&source, "raw").unwrap(), raw);
                let mut expected = native.clone();
                if blue {
                    for pixel in expected.iter_mut().flatten() {
                        pixel.swap(0, 2);
                    }
                }
                let stride = frame::frame_row_bytes(&source);
                for (y, row) in pairs.iter().enumerate() {
                    for (x, pair) in row.iter().enumerate() {
                        assert_eq!(format.pair(&raw[y * stride..][..stride], x), *pair);
                        assert_eq!(
                            frame::native_color(&source, x, y),
                            Some((expected[y][x], format.bits()))
                        );
                    }
                }
                let display = expected
                    .iter()
                    .flatten()
                    .flatten()
                    .map(|word| u8::try_from(word >> (format.bits() - 8)).unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(frame::rgb(&source).unwrap(), display);
                let png = frame::encode(&source, "png").unwrap();
                let mut png = png::Decoder::new(png.as_slice()).read_info().unwrap();
                assert_eq!(png.info().color_type, png::ColorType::Rgb);
                let mut decoded = vec![0; png.output_buffer_size()];
                let output = png.next_frame(&mut decoded).unwrap();
                decoded.truncate(output.buffer_size());
                assert_eq!(decoded, display);
                let mut planes = Vec::new();
                frame::gbr_planes(&source, &mut planes).unwrap();
                let mut expected_planes = Vec::new();
                for channel in [1, 2, 0] {
                    for pixel in expected.iter().flatten() {
                        expected_planes.extend_from_slice(&pixel[channel].to_le_bytes());
                    }
                }
                assert_eq!(planes, expected_planes);
                assert!(crate::auto::meter(&source).is_some());
                if let Some(view) = case.get("odd_x_view") {
                    let roi: [u32; 4] = serde_json::from_value(view["roi"].clone()).unwrap();
                    let mut expected: Vec<Vec<[u16; 3]>> =
                        serde_json::from_value(view["rgbg_native_rgb"].clone()).unwrap();
                    if blue {
                        for pixel in expected.iter_mut().flatten() {
                            pixel.swap(0, 2);
                        }
                    }
                    let pixels = frame::Pixels::new(FrameRef::region(&source, roi));
                    for (y, row) in expected.iter().enumerate() {
                        for (x, pixel) in row.iter().enumerate() {
                            assert_eq!(pixels.native_color(x, y), Some((*pixel, format.bits())));
                        }
                    }
                    let crop = crate::inspect::crop(&source, roi).unwrap();
                    assert_eq!(
                        crop.pixel_format,
                        format.unpacked().shifted(roi[0] as usize).code()
                    );
                    let expected_bytes = view["normalized_unpacked_le_rows_hex"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| row.as_str().unwrap())
                        .collect::<String>();
                    assert_eq!(crate::types::hex(&crop.data), expected_bytes);
                    assert_eq!(crop.pixel_encoding, None);
                    assert_eq!(source.data.as_ref(), raw);
                    let display = expected
                        .iter()
                        .flatten()
                        .flatten()
                        .map(|word| u8::try_from(word >> (format.bits() - 8)).unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(frame::rgb(&crop).unwrap(), display);
                }
                #[cfg(feature = "jpeg")]
                {
                    let jpeg = frame::encode(&source, "jpeg").unwrap();
                    let decoded =
                        turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::RGB).unwrap();
                    assert_eq!(
                        (decoded.width, decoded.height),
                        (width as usize, height as usize)
                    );
                }
            }
        }
    }

    #[test]
    fn odd10p_sources_reject_before_reading_but_odd_views_keep_native_pair_clipping() {
        for format in [Format::Rgbg10p, Format::Bgrg10p] {
            for width in [1, 3, 5] {
                for height in [1, 2] {
                    let source = Frame {
                        width,
                        height,
                        pixel_format: format.code(),
                        data: vec![0; 64].into(),
                        ..Frame::default()
                    };
                    let error = frame::validate_source(&source).unwrap_err();
                    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
                    assert!(error.to_string().contains("even source width"));
                    frame::rgb(&source).unwrap_err();
                    frame::unpacked(&source).unwrap_err();
                }
            }
        }
        let literal = packed_literals();
        let raw = crate::types::decode_hex(literal["cases"][0]["tight_hex"].as_str().unwrap(), 128)
            .unwrap();
        let source = Frame {
            width: 4,
            height: 2,
            pixel_format: Format::Rgbg10p.code(),
            data: raw.clone().into(),
            ..Frame::default()
        };
        let view = FrameRef::region(&source, [0, 1, 2, 1]);
        assert_eq!(
            frame::Pixels::new(view).native_color(1, 0),
            Some(([1023, 341, 0], 10))
        );
        let clipping = frame::clipping_in(view).unwrap();
        assert_eq!((clipping.high_px, clipping.pixels), (1, 2));
        assert_eq!(source.data.as_ref(), raw);
        let mut truncated = source;
        truncated.data = raw[..raw.len() - 1].to_vec().into();
        assert!(
            frame::validate_source(&truncated)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
    }
}
