//! Diagram-derived literals, independently prepared before this decoder changed.
//! Figure32 SHA256 1aaf76a8689314d13ca0a6155de77135e7ef5108cdadc2601293b79f7cbcf7e2.
//! These are synthetic layout vectors, not captured hardware or an independent demosaic oracle.
use super::*;
use crate::types::IdsPixelFormat;

#[test]
fn standard_thin_bayer_regions_keep_global_photosite_colors_at_odd_origins() {
    // RGGB row/column: R58, G171, R172, G38. An odd-origin view is G,R,G.
    // Missing colors use only in-view neighbors, or the photosite itself
    // when that color has no sample. The mean of the two greens is 104.
    let expected: [u8; 9] = [172, 171, 171, 172, 104, 172, 172, 38, 38];
    for (width, height, rect) in [(4, 1, [1, 0, 3, 1]), (1, 4, [0, 1, 1, 3])] {
        let frame = Frame {
            width,
            height,
            pixel_format: 0x0108_0009,
            data: vec![58, 171, 172, 38].into(),
            ..Frame::default()
        };
        validate_source(&frame).unwrap();
        let view = FrameRef::region(&frame, rect);
        let pixels = Pixels::new(view);
        for (i, rgb) in expected.as_chunks::<3>().0.iter().enumerate() {
            assert_eq!(pixels.at(i % view.width, i / view.width), *rgb);
        }
        let mut rgb = [0; 9];
        rows::decode(view, Channels::Rgb, &mut rgb);
        assert_eq!(rgb, expected);
    }
}

pub(crate) fn image(format: IdsPixelFormat) -> (Frame, Vec<u16>) {
    let (bytes, samples) = match format {
        IdsPixelFormat::BayerRg10g40
        | IdsPixelFormat::BayerGr10g40
        | IdsPixelFormat::BayerGb10g40
        | IdsPixelFormat::BayerBg10g40 => (
            vec![
                0xe8, 0x24, 0xb5, 0x78, 0x39, 0xc5, 0x19, 0xaa, 0x55, 0x8d, 0x2d, 0x8e, 0x70, 0xe3,
                0xc6, 0x50, 0xbb, 0x1c, 0xc1, 0x36, 0xb0, 0x44, 0xf7, 0x16, 0x87, 0x7e, 0xa9, 0x21,
                0xd7, 0x78, 0x6a, 0xd5, 0x3b, 0xa0, 0x39, 0xf1, 0x4b, 0x94, 0x25, 0x93,
            ],
            vec![
                0x3a1, 0x092, 0x2d7, 0x1e0, 0x315, 0x067, 0x2a8, 0x156, 0x0b6, 0x239, 0x1c0, 0x38f,
                0x142, 0x2ed, 0x073, 0x304, 0x2c3, 0x111, 0x3dc, 0x05a, 0x1f8, 0x2a6, 0x087, 0x35d,
                0x1a9, 0x356, 0x0ef, 0x280, 0x3c7, 0x12c, 0x251, 0x096,
            ],
        ),
        IdsPixelFormat::BayerRg12g24
        | IdsPixelFormat::BayerGr12g24
        | IdsPixelFormat::BayerGb12g24
        | IdsPixelFormat::BayerBg12g24 => (
            vec![
                0xe7, 0x2a, 0x61, 0xb4, 0x58, 0x3d, 0xcf, 0x19, 0xb2, 0xa0, 0x46, 0xf8, 0x36, 0x95,
                0xec, 0x74, 0xd1, 0x92, 0x5a, 0x8f, 0x07, 0x21, 0xbc, 0x53, 0xa9, 0x45, 0x75, 0xf2,
                0x16, 0xb0, 0x8c, 0x63, 0x9d, 0xd8, 0x28, 0xa6, 0x68, 0xc3, 0xf1, 0x1d, 0xa2, 0x64,
                0x47, 0x9b, 0x2e, 0x35, 0xe0, 0x8a,
            ],
            vec![
                0xe71, 0x2a6, 0xb4d, 0x583, 0xcf2, 0x19b, 0xa08, 0x46f, 0x36c, 0x95e, 0x742, 0xd19,
                0x5a7, 0x8f0, 0x213, 0xbc5, 0xa95, 0x457, 0xf20, 0x16b, 0x8cd, 0x639, 0xd86, 0x28a,
                0x681, 0xc3f, 0x1d4, 0xa26, 0x47e, 0x9b2, 0x35a, 0xe08,
            ],
        ),
        _ => panic!("not a Bayer literal"),
    };
    (
        Frame {
            width: 8,
            height: 4,
            pixel_format: format.code(),
            pixel_encoding: Some(format.encoding(None)),
            data: bytes.into(),
            ..Frame::default()
        },
        samples,
    )
}

#[test]
fn ids_bayer_literal_words_unpacked_color_outputs_and_metering_match_standard_bayer() {
    for format in [IdsPixelFormat::BayerRg10g40, IdsPixelFormat::BayerRg12g24] {
        let (frame, samples) = image(format);
        validate_source(&frame).unwrap();
        assert!(frame_bayer(&frame));
        assert!(!frame_monochrome(&frame));
        assert!(gray8(&frame).unwrap().is_none());
        assert_eq!(pixel_format_name(frame.pixel_format), "Unknown");
        assert!(!convertible(frame.pixel_format));
        assert!(packing(frame.pixel_format).is_none());
        assert_eq!(frame_unpacked_format(&frame), format.unpacked_format());
        for (at, &sample) in samples.iter().enumerate() {
            assert_eq!(native_sample(&frame, at % 8, at / 8), sample);
        }
        let standard = unpacked(&frame).unwrap();
        assert!(standard.pixel_encoding.is_none());
        assert_eq!(standard.pixel_format, format.unpacked_format());
        assert_eq!(
            &*standard.data,
            samples
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert_eq!(encode(&frame, "raw").unwrap(), &*frame.data);
        assert_eq!(rgb(&frame).unwrap(), rgb(&standard).unwrap());
        let raster = crate::jpeg::Raster::new(&frame).unwrap();
        assert_eq!(raster.channels, 3);
        assert_eq!(&*raster.data, rgb(&standard).unwrap());
        let png = encode(&frame, "png").unwrap();
        assert_eq!(png, encode(&standard, "png").unwrap());
        let png = png::Decoder::new(png.as_slice()).read_info().unwrap();
        assert_eq!(png.info().color_type, png::ColorType::Rgb);
        assert_eq!(png.info().bit_depth, png::BitDepth::Eight);
        #[cfg(feature = "jpeg")]
        assert_eq!(
            encode(&frame, "jpeg").unwrap(),
            encode(&standard, "jpeg").unwrap()
        );
        assert_eq!(exposure(&frame).unwrap(), exposure(&standard).unwrap());
        assert!(exposure(&frame).unwrap().channels.is_some());
        assert_eq!(crate::auto::meter(&frame), crate::auto::meter(&standard));
        assert_eq!(
            bit_depth_with_encoding(frame.pixel_format, frame.pixel_encoding, "raw"),
            (format.bits(), None)
        );
        assert_eq!(
            bit_depth_with_encoding(frame.pixel_format, frame.pixel_encoding, "png"),
            (8, Some(format.bits()))
        );
        let cropped = crate::inspect::crop(&frame, [1, 1, 5, 2]).unwrap();
        let reference = crate::inspect::crop(&standard, [1, 1, 5, 2]).unwrap();
        assert_eq!(
            crate::inspect::crop_rect(&frame, [1, 1, 5, 2]),
            [0, 0, 6, 4]
        );
        assert!(cropped.pixel_encoding.is_none());
        assert_eq!(cropped.pixel_format, reference.pixel_format);
        assert_eq!(&*cropped.data, &*reference.data);
    }
}

#[test]
fn ids_bayer_view_preserves_all_four_cfa_phases_and_inside_group_reads() {
    for format in [IdsPixelFormat::BayerRg10g40, IdsPixelFormat::BayerRg12g24] {
        let (frame, _) = image(format);
        let cases: &[([u32; 4], u32, &[u16])] = match format {
            IdsPixelFormat::BayerRg10g40 => &[
                (
                    [1, 1, 5, 2],
                    0x0110_000f,
                    &[
                        0x239, 0x1c0, 0x38f, 0x142, 0x2ed, 0x111, 0x3dc, 0x05a, 0x1f8, 0x2a6,
                    ],
                ),
                (
                    [3, 0, 4, 3],
                    0x0110_000c,
                    &[
                        0x1e0, 0x315, 0x067, 0x2a8, 0x38f, 0x142, 0x2ed, 0x073, 0x05a, 0x1f8,
                        0x2a6, 0x087,
                    ],
                ),
                (
                    [0, 1, 4, 2],
                    0x0110_000e,
                    &[0x0b6, 0x239, 0x1c0, 0x38f, 0x2c3, 0x111, 0x3dc, 0x05a],
                ),
                (
                    [4, 2, 4, 2],
                    0x0110_000d,
                    &[0x1f8, 0x2a6, 0x087, 0x35d, 0x3c7, 0x12c, 0x251, 0x096],
                ),
            ],
            IdsPixelFormat::BayerRg12g24 => &[
                (
                    [1, 1, 5, 2],
                    0x0110_0013,
                    &[
                        0x95e, 0x742, 0xd19, 0x5a7, 0x8f0, 0x457, 0xf20, 0x16b, 0x8cd, 0x639,
                    ],
                ),
                (
                    [1, 0, 4, 3],
                    0x0110_0010,
                    &[
                        0x2a6, 0xb4d, 0x583, 0xcf2, 0x95e, 0x742, 0xd19, 0x5a7, 0x457, 0xf20,
                        0x16b, 0x8cd,
                    ],
                ),
                (
                    [0, 1, 4, 2],
                    0x0110_0012,
                    &[0x36c, 0x95e, 0x742, 0xd19, 0xa95, 0x457, 0xf20, 0x16b],
                ),
                (
                    [4, 2, 4, 2],
                    0x0110_0011,
                    &[0x8cd, 0x639, 0xd86, 0x28a, 0x47e, 0x9b2, 0x35a, 0xe08],
                ),
            ],
            _ => unreachable!(),
        };
        for &(rect, pixel_format, samples) in cases {
            let view = FrameRef::region(&frame, rect);
            let reference = Frame {
                width: rect[2],
                height: rect[3],
                pixel_format,
                data: samples
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect::<Vec<_>>()
                    .into(),
                ..Frame::default()
            };
            assert_eq!(view_bayer_pattern(view), bayer_pattern(pixel_format));
            let mut rows = RowReader::new(view);
            let mut expected = RowReader::new(FrameRef::whole(&reference));
            let pixels = Pixels::new(view);
            for y in 0..view.height {
                let row = rows.row(y, Channels::Rgb).to_vec();
                assert_eq!(row, expected.row(y, Channels::Rgb));
                for (x, rgb) in row.as_chunks::<3>().0.iter().enumerate() {
                    assert_eq!(pixels.at(x, y), *rgb);
                    assert_eq!(
                        native_sample(&frame, view.x0 + x, view.y0 + y),
                        samples[y * view.width + x]
                    );
                }
            }
            assert_eq!(exposure_in(view).unwrap(), exposure(&reference).unwrap());
            assert_eq!(crate::auto::meter_in(view), crate::auto::meter(&reference));
        }
    }
}

// Manufacturer color-filter table: each first-row pair determines both rows.
struct CfaCase {
    format: IdsPixelFormat,
    name: &'static str,
    code: u32,
    unpacked: u32,
    eight: u32,
    filter: &'static str,
    // Whole, odd x, odd y, odd x+y, independently specified standard targets.
    phases: [u32; 4],
}
const OTHER_CFA: [CfaCase; 6] = [
    CfaCase {
        format: IdsPixelFormat::BayerGr10g40,
        name: "BayerGR10g40IDS",
        code: 0x4000_0003,
        unpacked: 0x0110_000c,
        eight: 0x0108_0008,
        filter: "GRBG",
        phases: [0x0110_000c, 0x0110_000d, 0x0110_000f, 0x0110_000e],
    },
    CfaCase {
        format: IdsPixelFormat::BayerGr12g24,
        name: "BayerGR12g24IDS",
        code: 0x4000_0013,
        unpacked: 0x0110_0010,
        eight: 0x0108_0008,
        filter: "GRBG",
        phases: [0x0110_0010, 0x0110_0011, 0x0110_0013, 0x0110_0012],
    },
    CfaCase {
        format: IdsPixelFormat::BayerGb10g40,
        name: "BayerGB10g40IDS",
        code: 0x4000_0002,
        unpacked: 0x0110_000e,
        eight: 0x0108_000a,
        filter: "GBRG",
        phases: [0x0110_000e, 0x0110_000f, 0x0110_000d, 0x0110_000c],
    },
    CfaCase {
        format: IdsPixelFormat::BayerGb12g24,
        name: "BayerGB12g24IDS",
        code: 0x4000_0012,
        unpacked: 0x0110_0012,
        eight: 0x0108_000a,
        filter: "GBRG",
        phases: [0x0110_0012, 0x0110_0013, 0x0110_0011, 0x0110_0010],
    },
    CfaCase {
        format: IdsPixelFormat::BayerBg10g40,
        name: "BayerBG10g40IDS",
        code: 0x4000_0004,
        unpacked: 0x0110_000f,
        eight: 0x0108_000b,
        filter: "BGGR",
        phases: [0x0110_000f, 0x0110_000e, 0x0110_000c, 0x0110_000d],
    },
    CfaCase {
        format: IdsPixelFormat::BayerBg12g24,
        name: "BayerBG12g24IDS",
        code: 0x4000_0014,
        unpacked: 0x0110_0013,
        eight: 0x0108_000b,
        filter: "BGGR",
        phases: [0x0110_0013, 0x0110_0012, 0x0110_0010, 0x0110_0011],
    },
];

#[test]
fn ids_other_cfa_native_words_source_labels_and_standard_interpretations_are_exact() {
    for case in OTHER_CFA {
        let (frame, words) = image(case.format);
        assert_eq!(frame.pixel_format, case.code);
        assert_eq!(frame_format_name(&frame), case.name);
        assert_eq!(case.format.color_filter(), Some(case.filter));
        assert_eq!(case.format.reduced_format(), case.eight);
        assert_eq!(frame_unpacked_format(&frame), case.unpacked);
        assert_eq!(name_bits(case.name), Some(case.format.bits()));
        validate_source(&frame).unwrap();
        assert!(frame_bayer(&frame));
        assert!(!frame_monochrome(&frame));
        assert_eq!(pixel_format_name(frame.pixel_format), "Unknown");
        assert!(!convertible(frame.pixel_format));
        assert!(packing(frame.pixel_format).is_none());
        let stored = serde_json::to_value(&frame).unwrap();
        assert_eq!(stored["pixel_encoding"]["name"], case.name);
        assert_eq!(stored["pixel_encoding"]["code"], case.code);
        let restored: Frame = serde_json::from_value(stored).unwrap();
        assert_eq!(restored.pixel_encoding, frame.pixel_encoding);
        for (at, &word) in words.iter().enumerate() {
            assert_eq!(native_sample(&frame, at % 8, at / 8), word);
        }
        assert_eq!(encode(&frame, "raw").unwrap(), &*frame.data);
        let standard = unpacked(&frame).unwrap();
        assert_eq!(standard.pixel_format, case.unpacked);
        assert!(standard.pixel_encoding.is_none());
        assert_eq!(
            standard.data,
            words
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert_eq!(rgb(&frame).unwrap(), rgb(&standard).unwrap());
        assert_eq!(
            encode(&frame, "png").unwrap(),
            encode(&standard, "png").unwrap()
        );
        let raster = crate::jpeg::Raster::new(&frame).unwrap();
        assert_eq!(raster.channels, 3);
        #[cfg(feature = "jpeg")]
        assert_eq!(
            encode(&frame, "jpeg").unwrap(),
            encode(&standard, "jpeg").unwrap()
        );
        assert_eq!(exposure(&frame).unwrap(), exposure(&standard).unwrap());
        assert_eq!(crate::auto::meter(&frame), crate::auto::meter(&standard));
    }
    assert_eq!(IdsPixelFormat::Mono10g40.color_filter(), None);
    assert_eq!(IdsPixelFormat::Mono12g24.color_filter(), None);
    assert_eq!(IdsPixelFormat::BayerRg10g40.color_filter(), Some("RGGB"));
    assert_eq!(IdsPixelFormat::BayerRg12g24.color_filter(), Some("RGGB"));
}

#[test]
fn ids_other_cfa_all_view_phases_match_literal_standard_targets_inside_groups() {
    for case in OTHER_CFA {
        let (frame, words) = image(case.format);
        for (phase, [x, y]) in [[0_u32, 0_u32], [1, 0], [0, 1], [1, 1]]
            .into_iter()
            .enumerate()
        {
            let view = FrameRef::region(&frame, [x, y, 5, 3]);
            let mut values = Vec::new();
            for yy in view.y0..view.y0 + view.height {
                values.extend_from_slice(&words[yy * 8 + view.x0..yy * 8 + view.x0 + view.width]);
            }
            let reference = Frame {
                width: 5,
                height: 3,
                pixel_format: case.phases[phase],
                data: values
                    .iter()
                    .flat_map(|word| word.to_le_bytes())
                    .collect::<Vec<_>>()
                    .into(),
                ..Frame::default()
            };
            assert_eq!(view_bayer_pattern(view), bayer_pattern(case.phases[phase]));
            let mut source_rows = RowReader::new(view);
            let mut expected_rows = RowReader::new(FrameRef::whole(&reference));
            let pixels = Pixels::new(view);
            for yy in 0..view.height {
                let row = source_rows.row(yy, Channels::Rgb).to_vec();
                assert_eq!(row, expected_rows.row(yy, Channels::Rgb));
                for (xx, value) in row.as_chunks::<3>().0.iter().enumerate() {
                    assert_eq!(pixels.at(xx, yy), *value);
                }
            }
            assert_eq!(exposure_in(view).unwrap(), exposure(&reference).unwrap());
            assert_eq!(crate::auto::meter_in(view), crate::auto::meter(&reference));
        }
    }
}
