//! Hand-authored component bytes; no SDK or product decoder generates expectations.
use super::*;
use crate::types::Frame;
use serde_json::json;
use std::sync::Arc;

pub(crate) fn ambiguous(native: bool) -> Frame {
    let mut parts = Vec::new();
    let mut stored = 0;
    for (index, (data_type, format, width, height, length, original)) in [
        (1, crate::types::MONO8, 2, 1, 2, 4),
        (1, crate::types::MONO8, 2, 2, 4, 16),
        (1, crate::types::RGB8, 1, 1, 3, 32),
        (1000, 0x8108_0001, 1, 1, 2, 40),
        (5, 0x0110_00b8, 1, 1, 2, 48),
        (9, 0x0108_00c6, 1, 1, 1, 56),
    ]
    .into_iter()
    .enumerate()
    {
        let mut part = json!({
            "index":index,"stored_offset":stored,"length":length,"data_type":data_type,
            "format":format,"width":width,"height":height,"delivered_height":height,
            "x_offset":0,"y_offset":0,"source_id":68,"region_id":7,
            "purpose_id":([1,65_281,2,65_282,8,6][index]),
        });
        if native {
            part["gvsp"] =
                json!({"address_offset":stored,"zone_directions":0,"x_padding":0,"y_padding":0});
        } else {
            part["original_offset"] = json!(original);
            part["namespace"] = json!(4);
            part["x_padding_pixels"] = json!(0);
        }
        stored += length;
        parts.push(part);
    }
    let mut layout = json!({"selection_required":true,"selected":0,"byte_order":"big",
        "original_filled_bytes":14,"payload_bytes":14,"parts":parts});
    if native {
        layout["data_type_namespace"] = json!("gvsp");
    } else {
        layout["original_buffer_bytes"] = json!(64);
    }
    let layout: Layout = serde_json::from_value(layout).unwrap();
    layout.validate(14).unwrap();
    Frame {
        id: 73,
        width: 2,
        height: 1,
        pixel_format: crate::types::MONO8,
        data: vec![10, 20].into(),
        device_ts_ns: Some(123_456),
        received_monotonic_ns: Some(987_654),
        wall_ms: 77,
        jpeg: Some(vec![0xff, 0xd8].into()),
        multipart: Some(Arc::new(MultipartData {
            layout,
            payload: vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 0xde, 0xad, 0x12, 0x34, 200,
            ]
            .into(),
        })),
        ..Frame::default()
    }
}

#[test]
fn same_format_jobs_select_exact_table_ordinals_without_semantic_id_inference() {
    for native in [false, true] {
        let frame = ambiguous(native);
        let error = GvspPartFormat::Mono8.project(&frame).unwrap_err();
        assert!(error.to_string().contains("multiple Mono8"));
        let first = GvspPartFormat::Mono8
            .project_index(&frame, Some(0))
            .unwrap();
        let combined = GvspPartFormat::Mono8
            .project_index(&frame, Some(1))
            .unwrap();
        assert_eq!(&*first.data, [10, 20]);
        assert_eq!((combined.width, combined.height), (2, 2));
        assert_eq!(&*combined.data, [30, 40, 50, 60]);
        assert_eq!(first.data.as_ptr(), frame.data.as_ptr());
        assert!(first.jpeg.is_none() && combined.jpeg.is_none());
        assert_eq!(
            (
                combined.id,
                combined.device_ts_ns,
                combined.received_monotonic_ns,
                combined.wall_ms
            ),
            (73, Some(123_456), Some(987_654), 77)
        );
        let original = frame.multipart.as_ref().unwrap();
        for selected in [&first, &combined] {
            let archive = selected.multipart.as_ref().unwrap();
            assert_eq!(archive.payload.as_ptr(), original.payload.as_ptr());
            assert_eq!(&*archive.payload, &*original.payload);
            assert!(!archive.layout.selection_required);
            assert_eq!(archive.layout.projected_format, Some(GvspPartFormat::Mono8));
            archive.layout.validate(14).unwrap();
        }
        assert!(original.layout.selection_required);
        assert_eq!(original.layout.selected, 0);
        assert_eq!(
            combined.multipart.as_ref().unwrap().layout.parts[1].source_id,
            Some(68)
        );
        assert_eq!(
            combined.multipart.as_ref().unwrap().layout.parts[1].purpose_id,
            Some(65_281)
        );
        let color = GvspPartFormat::Rgb8.project_index(&frame, Some(2)).unwrap();
        assert_eq!(crate::frame::rgb(&color).unwrap(), [70, 80, 90]);
        let map = GvspPartFormat::Coord3dC16
            .project_index(&frame, Some(4))
            .unwrap();
        assert_eq!(&*map.data, [0x34, 0x12]);
        assert_eq!(
            &*map.multipart.as_ref().unwrap().payload,
            &*original.payload
        );
        let confidence = GvspPartFormat::Confidence8
            .project_index(&frame, Some(5))
            .unwrap();
        assert!(crate::frame::is_measurement_frame(&confidence));
        assert!(crate::auto::meter(&confidence).is_none());
        assert_eq!(&*confidence.data, [200]);
        serde_json::to_value(&combined).unwrap();
    }
}

#[test]
fn unresolved_representatives_never_reach_pixels_raw_passthrough_serde_or_metering() {
    for native in [false, true] {
        let frame = ambiguous(native);
        for format in ["raw", "png", "jpeg"] {
            let Err(error) = crate::frame::encode_parts(&frame, format, None, None) else {
                panic!("an unresolved representative reached an encoder")
            };
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
        }
        let error = crate::frame::rgb(&frame).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
        crate::frame::unpacked(&frame).unwrap_err();
        serde_json::to_value(&frame).unwrap_err();
        assert!(crate::auto::meter(&frame).is_none());
        let measured = crate::inspect::measure(
            &frame,
            crate::camera_profile::CameraKind::AreaScan,
            [0, 0, 2, 1],
        );
        assert!(
            measured.exposure.is_none() && measured.focus.is_none() && measured.clipping.is_none()
        );
        assert!(
            measured
                .notes
                .iter()
                .any(|note| note.contains("part selection"))
        );
        let selected = GvspPartFormat::Mono8
            .project_index(&frame, Some(1))
            .unwrap();
        assert_eq!(
            &*crate::frame::encode_parts(&selected, "raw", None, None)
                .unwrap()
                .to_vec(),
            [30, 40, 50, 60]
        );
    }
}

#[test]
fn exact_selector_rejects_wrong_index_type_namespace_geometry_and_padding() {
    for native in [false, true] {
        let frame = ambiguous(native);
        for (format, index) in [
            (GvspPartFormat::Mono8, 2),
            (GvspPartFormat::Mono8, 31),
            (GvspPartFormat::Rgb8, 1),
        ] {
            format.project_index(&frame, Some(index)).unwrap_err();
        }
        GvspPartFormat::Mono8
            .project_index(&frame, Some(32))
            .unwrap_err();
        for change in [
            "format",
            "data_type",
            "length",
            "width",
            "padding",
            "namespace",
        ] {
            let mut changed = frame.clone();
            let mut archive = (**frame.multipart.as_ref().unwrap()).clone();
            let part = &mut archive.layout.parts[1];
            match change {
                "format" => part.format = Some(u64::from(crate::types::RGB8)),
                "data_type" => part.data_type = 5,
                "length" => part.length = 3,
                "width" => part.width = Some(3),
                "padding" if native => part.gvsp.as_mut().unwrap().x_padding = Some(1),
                "padding" => part.x_padding_pixels = Some(1),
                "namespace" => part.namespace = Some(1000),
                _ => unreachable!(),
            }
            changed.multipart = Some(Arc::new(archive));
            GvspPartFormat::Mono8
                .project_index(&changed, Some(1))
                .unwrap_err();
            assert!(frame.multipart.as_ref().unwrap().layout.selection_required);
        }
        GvspPartFormat::validate_index(Some(0), None).unwrap_err();
        GvspPartFormat::validate_index(Some(32), Some(GvspPartFormat::Mono8)).unwrap_err();
    }
}

#[test]
fn indexed_gentl_bindings_cannot_be_silently_overridden_or_upgraded_to_maps() {
    let mut binding = super::super::gentl_selector::Selector {
        producer: std::env::temp_dir().join("mock.cti"),
        interface: "if0".into(),
        device: "dev0".into(),
        stream: None,
        part: None,
        part_format: None,
        gendc: None,
    };
    GvspPartFormat::Mono8
        .validate_binding(Some(1), &binding.encode().unwrap())
        .unwrap();
    binding.part = Some(1);
    let ordinary = binding.encode().unwrap();
    GvspPartFormat::Mono8
        .validate_binding(Some(1), &ordinary)
        .unwrap();
    GvspPartFormat::Mono8
        .validate_binding(None, &ordinary)
        .unwrap_err();
    GvspPartFormat::Mono8
        .validate_binding(Some(0), &ordinary)
        .unwrap_err();
    GvspPartFormat::Coord3dC16
        .validate_binding(Some(1), &ordinary)
        .unwrap_err();
    binding.part_format = Some(0x0110_00b8);
    let map = binding.encode().unwrap();
    GvspPartFormat::Coord3dC16
        .validate_binding(Some(1), &map)
        .unwrap();
    GvspPartFormat::Confidence16
        .validate_binding(Some(1), &map)
        .unwrap_err();
    GvspPartFormat::validate_transport(crate::types::Transport::Usb3).unwrap_err();
    for output in ["-", "view.jpg"] {
        GvspPartFormat::Mono8
            .validate_output(output, if output == "-" { "raw" } else { "jpeg" })
            .unwrap_err();
    }
}
