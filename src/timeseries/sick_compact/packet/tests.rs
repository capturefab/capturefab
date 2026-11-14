//! Independent predecoder corpus and meaningful decode admission boundaries.
use super::super::Stream;
use super::{MAX_JSON, MAX_PACKET, decode};
use crate::types::{ErrorKind, classify};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

const CORPUS: &str = include_str!("../../../../tests/fixtures/sick_compact/packet-literals.json");
const CORPUS_SHA: &str = "4773e8ee8313727aa2fcf8cdf755c28eb4fcf74b1928add0e88c9e17f50ba119";
fn corpus() -> Value {
    serde_json::from_str(CORPUS).unwrap()
}
fn bytes(text: &str) -> Vec<u8> {
    let (words, rest) = text.as_bytes().as_chunks::<2>();
    assert_eq!(rest, &[] as &[u8]);
    words
        .iter()
        .map(|word| u8::from_str_radix(str::from_utf8(word).unwrap(), 16).unwrap())
        .collect()
}
fn budget() -> Instant {
    Instant::now() + Duration::from_secs(3)
}
fn stream(raw: &[u8]) -> Stream {
    if raw.get(4) == Some(&2) {
        Stream::Imu
    } else {
        Stream::Points
    }
}
fn packed(column: &Value, encoding: &str, shape: &Value, order: &str) -> Vec<u8> {
    assert_eq!(column["encoding"], encoding);
    assert_eq!(&column["shape"], shape);
    assert_eq!(column["order"], order);
    STANDARD.decode(column["data"].as_str().unwrap()).unwrap()
}
fn integer_words(raw: &[u8], width: usize) -> Vec<u64> {
    assert_eq!(raw.len() % width, 0);
    raw.chunks(width)
        .map(|word| {
            word.iter()
                .enumerate()
                .fold(0_u64, |n, (i, b)| n | (u64::from(*b) << (i * 8)))
        })
        .collect()
}
fn metadata_column(actual: &Value, expected: &Value, field: &str, encoding: &str, width: usize) {
    let values = expected[field].as_array().unwrap();
    let raw = packed(actual, encoding, &json!([values.len()]), "line");
    assert_eq!(
        integer_words(&raw, width),
        values
            .iter()
            .map(|n| n.as_u64().unwrap())
            .collect::<Vec<_>>()
    );
}
fn check_module(actual: &Value, expected: &Value, version: u64) {
    for field in [
        "module_bytes",
        "segment_counter",
        "frame_number",
        "sender_id",
        "line_count",
        "beams_per_line",
        "echoes_per_beam",
        "next_module_size",
        "reserved_raw",
        "echo_mask_raw",
        "beam_mask_raw",
    ] {
        assert_eq!(&actual[field], &expected[field], "field {field}");
    }
    metadata_column(
        &actual["timestamp_start"],
        expected,
        "timestamp_start_raw",
        "base64_le_u64",
        8,
    );
    metadata_column(
        &actual["timestamp_stop"],
        expected,
        "timestamp_stop_raw",
        "base64_le_u64",
        8,
    );
    for key in ["timestamp_start", "timestamp_stop"] {
        assert_eq!(
            actual[key]["basis"],
            "device_timebase_microseconds_declared"
        );
        assert_eq!(actual[key]["unit"], "us");
    }
    for (key, raw) in [
        ("phi", "phi_words"),
        ("theta_start", "theta_start_words"),
        ("theta_stop", "theta_stop_words"),
    ] {
        metadata_column(&actual[key], expected, raw, "ieee754_le_u32_bits", 4);
        assert_eq!(actual[key]["unit"], "rad");
    }
    if version == 4 {
        let bits = u32::try_from(expected["distance_scale_word"].as_u64().unwrap()).unwrap();
        assert_eq!(actual["distance_scale"]["bits"], bits);
        assert_eq!(actual["distance_scale"]["classification"], "finite");
        assert_eq!(
            actual["distance_scale"]["value"].as_f64().unwrap(),
            f64::from(f32::from_bits(bits))
        );
        assert_eq!(actual["distance_relation"], "raw_times_distance_scale_mm");
    } else {
        assert!(actual.get("distance_scale").is_none());
        assert_eq!(actual["distance_relation"], "raw_mm");
    }
    let b = expected["beams_per_line"].as_u64().unwrap();
    let l = expected["line_count"].as_u64().unwrap();
    let e = expected["echoes_per_beam"].as_u64().unwrap();
    assert_eq!(actual["beam_line_count"], b * l);
    assert_eq!(actual["echo_cell_count"], b * l * e);
    let echo_mask = expected["echo_mask_raw"].as_u64().unwrap();
    for (field, mask) in [("distance", 1), ("rssi", 2)] {
        if echo_mask & mask == 0 {
            assert!(actual.get(field).is_none());
            continue;
        }
        let raw = packed(
            &actual[field],
            "base64_le_u16",
            &json!([b, l, e]),
            "beam_line_echo",
        );
        let values = integer_words(&raw, 2);
        assert_eq!(u64::try_from(values.len()).unwrap(), b * l * e);
        for item in expected["echo_values"].as_array().unwrap() {
            let at = ((item["beam"].as_u64().unwrap() * l + item["line"].as_u64().unwrap()) * e)
                + item["echo"].as_u64().unwrap();
            let key = if field == "distance" {
                "distance_raw"
            } else {
                "rssi_raw"
            };
            assert_eq!(
                values[usize::try_from(at).unwrap()],
                item[key].as_u64().unwrap()
            );
        }
    }
    let beam_mask = expected["beam_mask_raw"].as_u64().unwrap();
    for (field, mask, width, encoding, key) in [
        ("azimuth", 2, 2, "base64_le_u16", "azimuth_raw"),
        ("properties", 1, 1, "base64_u8", "properties_raw"),
    ] {
        if beam_mask & mask == 0 {
            assert!(actual.get(field).is_none());
            continue;
        }
        let raw = packed(&actual[field], encoding, &json!([b, l]), "beam_line");
        let values = integer_words(&raw, width);
        assert_eq!(u64::try_from(values.len()).unwrap(), b * l);
        for item in expected["beam_values"].as_array().unwrap() {
            let at = item["beam"].as_u64().unwrap() * l + item["line"].as_u64().unwrap();
            assert_eq!(
                values[usize::try_from(at).unwrap()],
                item[key].as_u64().unwrap()
            );
        }
    }
    assert_eq!(actual["reflector_mask"], 1);
    assert_eq!(actual["property_reserved_mask"], 254);
}
fn check_native(actual: &Value, expected: &Value) {
    assert_eq!(actual["kind"], "sick_compact");
    assert_eq!(actual["byte_order"], "little_endian");
    assert_eq!(actual["checksum"]["algorithm"], "crc32_iso_hdlc");
    assert_eq!(actual["checksum"]["scope"], "frame_without_crc");
    assert_eq!(actual["checksum"]["status"], "verified");
    assert_eq!(
        actual["checksum"]["stored"],
        actual["checksum"]["calculated"]
    );
    assert_eq!(actual["clock_sync"], "not_verified");
    assert!(actual.get("source_time").is_none());
    assert_eq!(actual["telegram_type"], expected["telegram_type"]);
    assert_eq!(actual["telegram_version"], expected["version"]);
    if expected["telegram_type"] == 1 {
        assert_eq!(actual["telegram_counter"], expected["telegram_counter"]);
        assert_eq!(
            actual["transmit_timestamp"]["raw"],
            expected["transmit_timestamp_raw"]
        );
        assert_eq!(
            actual["transmit_timestamp"]["basis"],
            "unix_microseconds_declared"
        );
        assert_eq!(actual["size_module_0"], expected["size_module_0"]);
        let modules = actual["modules"].as_array().unwrap();
        let expected_modules = expected["modules"].as_array().unwrap();
        assert_eq!(modules.len(), expected_modules.len());
        for (module, expected_module) in modules.iter().zip(expected_modules) {
            check_module(
                module,
                expected_module,
                expected["version"].as_u64().unwrap(),
            );
        }
    } else {
        assert_eq!(actual["timestamp"]["raw"], expected["timestamp_raw"]);
        assert_eq!(actual["timestamp"]["basis"], "unix_microseconds_declared");
        for (key, raw, count, order, unit) in [
            (
                "acceleration",
                "acceleration_words",
                3,
                "x_y_z",
                Some("m/s^2"),
            ),
            (
                "angular_velocity",
                "angular_velocity_words",
                3,
                "x_y_z",
                Some("rad/s"),
            ),
            ("orientation", "orientation_words_wxyz", 4, "w_x_y_z", None),
        ] {
            let data = packed(
                &actual["imu"][key],
                "ieee754_le_u32_bits",
                &json!([count]),
                order,
            );
            assert_eq!(
                integer_words(&data, 4),
                expected[raw]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap())
                    .collect::<Vec<_>>()
            );
            if let Some(unit) = unit {
                assert_eq!(actual["imu"][key]["unit"], unit);
            } else {
                assert!(actual["imu"][key].get("unit").is_none());
            }
        }
    }
    assert!(serde_json::to_vec(actual).unwrap().len() <= MAX_JSON);
}
#[test]
fn frozen_predecoder_corpus_and_every_positive_native_field_match() {
    assert_eq!(format!("{:x}", Sha256::digest(CORPUS)), CORPUS_SHA);
    let corpus = corpus();
    let cases = corpus["variants"].as_array().unwrap();
    assert_eq!(cases.len(), 43);
    let mut names = BTreeSet::new();
    let mut positive = 0;
    for case in cases {
        assert!(names.insert(case["name"].as_str().unwrap()));
        let raw = bytes(case["packet_hex"].as_str().unwrap());
        assert_eq!(
            u64::try_from(raw.len()).unwrap(),
            case["packet_bytes"].as_u64().unwrap()
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&raw)),
            case["packet_sha256"].as_str().unwrap()
        );
        if case["outcome"] == "accept" {
            let actual = decode(&raw, stream(&raw), budget()).unwrap();
            check_native(&actual, &case["oracle"]);
            positive += 1;
        }
    }
    assert_eq!(positive, 22);
}
#[test]
fn malformed_crc_extent_version_and_zero_content_work_controls_are_typed() {
    let corpus = corpus();
    let mut rejected = 0;
    for case in corpus["variants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["outcome"] != "accept")
    {
        let raw = bytes(case["packet_hex"].as_str().unwrap());
        let error = decode(&raw, stream(&raw), budget()).unwrap_err();
        let kind = if matches!(
            case["outcome"].as_str().unwrap(),
            "reject_version" | "reject_telegram_type"
        ) {
            ErrorKind::Unsupported
        } else {
            ErrorKind::Incomplete
        };
        assert_eq!(classify(&error).kind, kind, "{}: {error:#}", case["name"]);
        assert!(error.to_string().len() < 256);
        rejected += 1;
    }
    assert_eq!(rejected, 21);
}
#[test]
fn expired_original_deadline_stream_selection_and_crc_first_are_not_bypassed() {
    let corpus = corpus();
    let raw = bytes(corpus["variants"][0]["packet_hex"].as_str().unwrap());
    assert_eq!(
        classify(&decode(&raw, Stream::Points, Instant::now()).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert_eq!(
        classify(&decode(&raw, Stream::Imu, budget()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    let mut unknown = raw;
    unknown[4] = 99;
    let error = decode(&unknown, Stream::Points, budget()).unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
    assert!(error.to_string().contains("CRC32"));
    assert_eq!(
        classify(&decode(&vec![0; MAX_PACKET + 1], Stream::Points, budget()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}
fn append_crc(mut raw: Vec<u8>) -> Vec<u8> {
    raw.extend_from_slice(&crc32fast::hash(&raw).to_le_bytes());
    raw
}
#[test]
fn raw_scale_ieee_nonfinite_and_nonzero_short_decimal_keep_all_bits() {
    let corpus = corpus();
    let template = bytes(corpus["variants"][1]["packet_hex"].as_str().unwrap());
    for bits in [0x7fc0_1234_u32, 0x8000_0000, 0x3dcc_cccd] {
        let mut raw = template[..template.len() - 4].to_vec();
        raw[92..96].copy_from_slice(&bits.to_le_bytes());
        let actual = decode(&append_crc(raw), Stream::Points, budget()).unwrap();
        let scale = &actual["modules"][0]["distance_scale"];
        assert_eq!(scale["bits"], bits);
        assert_eq!(scale["sign_bit"], bits >> 31 != 0);
        if f32::from_bits(bits).is_finite() {
            assert_eq!(
                scale["value"].as_f64().unwrap().to_bits(),
                f64::from(f32::from_bits(bits)).to_bits()
            );
        } else {
            assert_eq!(scale["classification"], "nan");
            assert!(scale.get("value").is_none());
        }
    }
}
fn empty_line_modules(count: usize) -> Vec<u8> {
    let mut frame = bytes("0202020201000000010000000000000000000000000000000300000028000000");
    for index in 0..count {
        let mut module = bytes(
            "000000000000000005000000000000002a0000000000000000000000000000002800000000000000",
        );
        assert_eq!(module.len(), 40);
        if index + 1 == count {
            module[32..36].fill(0);
        }
        frame.extend_from_slice(&module);
    }
    append_crc(frame)
}
#[test]
fn valid_many_empty_modules_reach_exact_native_json_bound_before_materialization() {
    let small = decode(&empty_line_modules(1), Stream::Points, budget()).unwrap();
    assert_eq!(small["modules"][0]["line_count"], 0);
    let large = empty_line_modules(1600);
    assert_eq!(large.len(), 64_036);
    assert!(large.len() <= MAX_PACKET);
    let error = decode(&large, Stream::Points, budget()).unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
    assert!(error.to_string().contains("JSON"));
}

const ENCODER_CORPUS: &str =
    include_str!("../../../../tests/fixtures/sick_compact_encoder/literals.json");
const ENCODER_CORPUS_SHA: &str = "f4dc9cb4755012a7b24414dde613b34038c79e9b3ab3ea4fc6ed59b16f6c5e68";
fn encoder_corpus() -> Value {
    assert_eq!(
        format!("{:x}", Sha256::digest(ENCODER_CORPUS)),
        ENCODER_CORPUS_SHA
    );
    serde_json::from_str(ENCODER_CORPUS).unwrap()
}
fn encoder_clock(raw: &Value) -> Value {
    json!({"raw":raw,"unit":"us","basis":"unix_microseconds_declared"})
}
#[test]
fn encoder_v1_v2_frozen_fields_preserve_unsigned_words_and_unqualified_state() {
    let corpus = encoder_corpus();
    let positives = corpus["positive"].as_array().unwrap();
    assert_eq!(positives.len(), 3);
    for case in positives {
        let raw = bytes(case["hex"].as_str().unwrap());
        assert_eq!(raw.len(), 88);
        assert_eq!(format!("{:x}", Sha256::digest(&raw)), case["sha256"]);
        let expected = &case["expected"];
        let actual = decode(&raw, Stream::Encoder, budget()).unwrap();
        assert_eq!(actual, decode(&raw, Stream::Mixed, budget()).unwrap());
        assert_eq!(actual["kind"], "sick_compact");
        assert_eq!(actual["telegram_type"], 4);
        assert_eq!(actual["telegram_version"], expected["version"]);
        assert_eq!(actual["byte_order"], "little_endian");
        assert_eq!(actual["clock_sync"], "not_verified");
        assert_eq!(actual["telegram_counter"], expected["telegram_counter"]);
        assert_eq!(actual["sender_id"], expected["sender_id"]);
        assert_eq!(actual["payload_length"], expected["payload_length"]);
        assert_eq!(
            actual["transmit_timestamp"],
            encoder_clock(&expected["transmit_timestamp_raw"])
        );
        assert_eq!(
            actual["checksum"],
            json!({"algorithm":"crc32_iso_hdlc","scope":"frame_without_crc","stored":case["crc32"],"calculated":case["crc32"],"status":"verified"})
        );
        let mut speed = json!({"bits":expected["speed_bits"],"classification":expected["speed_classification"],"sign_bit":expected["speed_bits"].as_u64().unwrap()>>31!=0});
        if expected["speed_classification"] == "finite" {
            if expected["speed_value_annotation"] == "negative_zero" {
                speed["value"] = json!(-0.0_f64);
                assert_eq!(
                    actual["encoder"]["speed"]["value"]
                        .as_f64()
                        .unwrap()
                        .to_bits(),
                    (-0.0_f64).to_bits()
                );
            } else {
                speed["value"] = json!(expected["speed_value_annotation"].as_f64().unwrap());
            }
        }
        assert_eq!(
            actual["encoder"],
            json!({
                "frame_number":expected["frame_number"],"tick_count":expected["tick_count"],
                "tick_count_at_reference_signal_1":expected["reference_1_tick_count"],
                "tick_count_at_reference_signal_2":expected["reference_2_tick_count"],
                "speed":speed,
                "tick_count_timestamp":encoder_clock(&expected["tick_timestamp_raw"]),
                "timestamp_of_reference_signal_1":encoder_clock(&expected["reference_1_timestamp_raw"]),
                "timestamp_of_reference_signal_2":encoder_clock(&expected["reference_2_timestamp_raw"]),
                "speed_unit_status":"configured_conversion_unobserved","reference_validity":"not_observed",
                "input_mode":"not_observed","angle_conversion":"not_inferred"
            })
        );
        assert!(actual.get("source_time").is_none());
        assert!(serde_json::to_vec(&actual).unwrap().len() <= MAX_JSON);
    }
}
#[test]
fn mixed_dispatch_preserves_every_existing_selected_type1_and_type2_result() {
    let corpus = corpus();
    let mut accepted = 0;
    for case in corpus["variants"].as_array().unwrap() {
        let raw = bytes(case["packet_hex"].as_str().unwrap());
        if !raw.get(4).is_some_and(|kind| matches!(*kind, 1 | 2)) {
            continue;
        }
        match (
            decode(&raw, stream(&raw), budget()),
            decode(&raw, Stream::Mixed, budget()),
        ) {
            (Ok(original), Ok(mixed)) => {
                assert_eq!(mixed, original);
                accepted += 1;
            }
            (Err(original), Err(mixed)) => {
                assert_eq!(classify(&mixed).kind, classify(&original).kind);
                assert_eq!(mixed.to_string(), original.to_string());
            }
            _ => panic!(
                "mixed dispatch changed legacy native admission for {}",
                case["name"]
            ),
        }
    }
    assert_eq!(accepted, 22);
    assert_eq!(format!("{:x}", Sha256::digest(CORPUS)), CORPUS_SHA);
}
#[test]
fn encoder_payload_version_extent_and_crc_rejections_use_independent_oracles() {
    let corpus = encoder_corpus();
    let negatives = corpus["negative"].as_array().unwrap();
    assert_eq!(negatives.len(), 9);
    for case in negatives {
        let raw = bytes(case["hex"].as_str().unwrap());
        assert_eq!(format!("{:x}", Sha256::digest(&raw)), case["sha256"]);
        for selected in [Stream::Encoder, Stream::Mixed] {
            let error = decode(&raw, selected, budget()).unwrap_err();
            let kind = if case["expected_code"] == "unsupported" {
                ErrorKind::Unsupported
            } else {
                ErrorKind::Incomplete
            };
            assert_eq!(classify(&error).kind, kind, "{}: {error:#}", case["name"]);
        }
    }
    let raw = bytes(corpus["positive"][0]["hex"].as_str().unwrap());
    // These controls have a valid recalculated CRC, reaching the fixed-body guard
    // rather than only the whole-frame checksum rejection in the frozen corpus.
    for body_length in [83, 85] {
        let mut body = raw[..84].to_vec();
        body.resize(body_length, 0);
        let malformed = append_crc(body);
        assert_eq!(malformed.len(), body_length + 4);
        for selected in [Stream::Encoder, Stream::Mixed] {
            let error = decode(&malformed, selected, budget()).unwrap_err();
            assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
            assert!(error.to_string().contains("exactly 88 bytes"));
        }
    }
    for legacy in [Stream::Points, Stream::Imu] {
        assert_eq!(
            classify(&decode(&raw, legacy, budget()).unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        let mut bad_crc = raw.clone();
        bad_crc[84] ^= 1;
        let error = decode(&bad_crc, legacy, budget()).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        assert!(error.to_string().contains("CRC32"));
    }
    assert_eq!(
        classify(&decode(&raw, Stream::Encoder, Instant::now()).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert_eq!(
        classify(&decode(&raw, Stream::Mixed, Instant::now()).unwrap_err()).kind,
        ErrorKind::Timeout
    );
}
#[test]
fn encoder_speed_keeps_all_ieee_words_without_invented_physical_unit() {
    let corpus = encoder_corpus();
    let template = bytes(corpus["positive"][0]["hex"].as_str().unwrap());
    for bits in [
        0x8000_0000_u32,
        0x7f80_0000,
        0xff80_0000,
        0xffc0_1234,
        0x3dcc_cccd,
    ] {
        let mut raw = template[..84].to_vec();
        raw[56..60].copy_from_slice(&bits.to_le_bytes());
        let value = decode(&append_crc(raw), Stream::Encoder, budget()).unwrap();
        let speed = &value["encoder"]["speed"];
        assert_eq!(speed["bits"], bits);
        assert_eq!(speed["sign_bit"], bits >> 31 != 0);
        if f32::from_bits(bits).is_finite() {
            assert_eq!(
                speed["value"].as_f64().unwrap().to_bits(),
                f64::from(f32::from_bits(bits)).to_bits()
            );
        } else {
            assert!(speed.get("value").is_none());
        }
        assert_eq!(
            value["encoder"]["speed_unit_status"],
            "configured_conversion_unobserved"
        );
    }
}
