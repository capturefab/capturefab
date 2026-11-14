use super::{Format, Profile, StreamKind, decode, float_word, validate_format};
use crate::types::{ErrorKind, classify};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const PLAN: &str = include_str!("../../../../tests/fixtures/livox/plan-literals.json");
const LEGACY: &str = include_str!("../../../../tests/fixtures/livox/legacy-return-literals.json");

fn hex(raw: &str) -> Vec<u8> {
    let (pairs, tail) = raw.as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn recipe(source: &str, name: &str) -> (Vec<u8>, Value) {
    let document: Value = serde_json::from_str(source).unwrap();
    let case = &document["recipes"][name];
    let mut raw = hex(case["header_hex"].as_str().unwrap());
    raw.extend(hex(case["prefix_hex"].as_str().unwrap()));
    raw.resize(
        raw.len() + usize::try_from(case["zero_tail_bytes"].as_u64().unwrap()).unwrap(),
        0,
    );
    assert_eq!(
        raw.len(),
        usize::try_from(case["bytes"].as_u64().unwrap()).unwrap()
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(&raw)),
        case["sha256"].as_str().unwrap()
    );
    (raw, case["expected"].clone())
}

const fn format(profile: Profile, stream: StreamKind) -> Format {
    Format { profile, stream }
}

fn kind(raw: &[u8], configured: Format) -> ErrorKind {
    classify(&decode(raw, configured).unwrap_err()).kind
}

#[test]
fn independently_frozen_sdk2_vectors_keep_dense_native_fields_and_full_clocks() {
    for (name, layout, coordinate_unit) in [
        ("sdk2_cartesian32", "cartesian_i32", 1),
        ("sdk2_cartesian16", "cartesian_i16", 10),
    ] {
        let (raw, expected) = recipe(PLAN, name);
        let decoded = decode(&raw, format(Profile::Sdk2Mid360V0, StreamKind::Points)).unwrap();
        assert_eq!(decoded.elements, 96);
        let value = decoded.value;
        assert_eq!(value["generation"], "sdk2_v0");
        assert_eq!(value["checksum_status"], "crc32_verified");
        assert_eq!(value["checksum_scope"], "timestamp_and_data");
        assert_eq!(
            value["clock"]["timestamp_word_raw"],
            expected["timestamp_raw"]
        );
        assert_eq!(value["native"]["layout"], layout);
        assert_eq!(value["native"]["coordinate_unit_mm"], coordinate_unit);
        assert_eq!(value["native"]["record_count"], 96);
        assert_eq!(value["native"]["returns_per_record"], 1);
        for (axis, component) in ["x_raw", "y_raw", "z_raw"].into_iter().enumerate() {
            assert_eq!(
                value["native"][component][0][0],
                expected["first_xyz_raw"][axis]
            );
            assert_eq!(
                value["native"][component][1][0],
                expected["second_xyz_raw"][axis]
            );
            for row in value["native"][component]
                .as_array()
                .unwrap()
                .iter()
                .skip(2)
            {
                assert_eq!(row, &json!([0]));
            }
        }
        for (component, prefix) in [
            ("reflectivity", "reflectivity_prefix"),
            ("tag_raw", "tag_prefix"),
        ] {
            for index in 0..2 {
                assert_eq!(
                    value["native"][component][index][0],
                    expected[prefix][index]
                );
            }
            for row in value["native"][component]
                .as_array()
                .unwrap()
                .iter()
                .skip(2)
            {
                assert_eq!(row, &json!([0]));
            }
        }
    }
    let (raw, expected) = recipe(PLAN, "sdk2_spherical");
    let value = decode(&raw, format(Profile::Sdk2Mid360sV0, StreamKind::Points))
        .unwrap()
        .value;
    assert_eq!(value["native"]["layout"], "spherical");
    for index in 0..2 {
        assert_eq!(
            value["native"]["depth_mm"][index][0],
            expected["depth_prefix"][index]
        );
        assert_eq!(
            value["native"]["theta_centidegrees_raw"][index],
            expected["zenith_prefix"][index]
        );
        assert_eq!(
            value["native"]["phi_centidegrees_raw"][index],
            expected["azimuth_prefix"][index]
        );
    }
    assert_eq!(value["native"]["angles_within_nominal"][0], true);
    assert_eq!(value["native"]["angles_within_nominal"][1], false);
    assert_eq!(
        value["clock"]["timestamp_word_raw"],
        expected["timestamp_raw"]
    );
    assert_eq!(value["clock"]["domain"], "reported_gps_clock");
}

#[test]
fn legacy_whole_return_groups_preserve_every_independent_slot_and_shared_angle() {
    for data_type in [2, 3, 4, 5, 7, 8] {
        let (raw, expected) = recipe(LEGACY, &format!("legacy_type_{data_type}"));
        let decoded = decode(&raw, format(Profile::LegacyAviaV5, StreamKind::Points)).unwrap();
        let native = &decoded.value["native"];
        let target = &expected["native"];
        assert_eq!(native["layout"], target["layout"]);
        assert_eq!(native["record_count"], target["record_count"]);
        assert_eq!(native["returns_per_record"], target["returns_per_record"]);
        assert_eq!(decoded.elements, if data_type >= 7 { 90 } else { 96 });
        assert_eq!(
            decoded.value["checksum_status"],
            "not_present_in_sample_protocol"
        );
        assert_eq!(decoded.value["status_raw"], expected["status_raw"]);
        assert_eq!(
            decoded.value["clock"]["timestamp_word_raw"],
            expected["timestamp_word_raw"]
        );
        let spherical = target["layout"] == "spherical";
        let components: &[&str] = if spherical {
            &["depth_mm", "reflectivity", "tag_raw"]
        } else {
            &["x_raw", "y_raw", "z_raw", "reflectivity", "tag_raw"]
        };
        for (record, returns) in target["first_records"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            for (slot, fields) in returns.as_array().unwrap().iter().enumerate() {
                for (field, &component) in components.iter().enumerate() {
                    assert_eq!(native[component][record][slot], fields[field]);
                }
            }
            if spherical {
                assert_eq!(
                    native["theta_centidegrees_raw"][record],
                    target["first_angles"][record][0]
                );
                assert_eq!(
                    native["phi_centidegrees_raw"][record],
                    target["first_angles"][record][1]
                );
                assert_eq!(
                    native["angles_within_nominal"][record],
                    target["angles_within_nominal_prefix"][record]
                );
            }
        }
        for &component in components {
            for row in native[component].as_array().unwrap().iter().skip(2) {
                for word in row.as_array().unwrap() {
                    assert_eq!(word, &json!(0));
                }
            }
        }
    }
}

#[test]
fn imu_raw_words_nonfinite_and_negative_pps_are_lossless_without_json_nan() {
    for (name, profile) in [
        ("sdk2_imu", Profile::Sdk2HapTxV0),
        ("legacy_imu_pps", Profile::LegacyHorizonV5),
    ] {
        let (raw, expected) = recipe(PLAN, name);
        let decoded = decode(&raw, format(profile, StreamKind::Imu)).unwrap();
        assert_eq!(decoded.elements, 1);
        let native = &decoded.value["native"];
        assert_eq!(native["gyro_unit"], "rad/s");
        assert_eq!(native["acceleration_unit"], "g");
        for (name, start) in [("gyro_raw_words", 0), ("acceleration_raw_words", 3)] {
            for axis in 0..3 {
                assert_eq!(
                    native[name][axis],
                    expected["float32_words_raw"][start + axis]
                );
            }
        }
        assert_eq!(native["gyro"][2]["sign_bit"], true);
        let negative_zero = native["gyro"][2]["value"].as_f64().unwrap();
        assert_eq!(negative_zero, 0.0);
        assert!(negative_zero.is_sign_negative());
        assert_eq!(native["acceleration"][2]["classification"], "nan");
        assert!(native["acceleration"][2].get("value").is_none());
        assert!(
            !serde_json::to_string(&decoded.value)
                .unwrap()
                .contains("null")
        );
        if profile == Profile::LegacyHorizonV5 {
            assert_eq!(decoded.value["clock"]["pps_offset_ns_raw"], -1000);
            assert_eq!(decoded.value["slot_raw"], 1);
            assert_eq!(decoded.value["lidar_id_raw"], 2);
            assert_eq!(decoded.value["reserved_raw"], 127);
        }
    }
    for (word, classification, sign) in [
        (0x7f80_0000, "positive_infinity", false),
        (0xff80_0000, "negative_infinity", true),
        (0xffc0_0001, "nan", true),
        (0x0000_0001, "finite", false),
        (0x8000_0001, "finite", true),
    ] {
        let value = float_word(word);
        assert_eq!(value["bits"], word);
        assert_eq!(value["classification"], classification);
        assert_eq!(value["sign_bit"], sign);
        assert_eq!(value.get("value").is_some(), classification == "finite");
    }
}

#[test]
fn sdk2_crc_domain_is_timestamp_and_body_and_zero_never_bypasses_it() {
    let configured = format(Profile::Sdk2Mid360V0, StreamKind::Points);
    let (raw, _) = recipe(PLAN, "sdk2_cartesian32");
    for offset in [28, 36, 1379] {
        let mut wrong = raw.clone();
        wrong[offset] ^= 1;
        assert_eq!(kind(&wrong, configured), ErrorKind::Incomplete);
    }
    let mut zero = raw.clone();
    zero[24..28].fill(0);
    assert_eq!(kind(&zero, configured), ErrorKind::Incomplete);
    let mut header = raw;
    header[7] ^= 1;
    let value = decode(&header, configured).unwrap().value;
    assert_eq!(value["udp_cnt_raw"], 65_534);
    assert_eq!(value["checksum_status"], "crc32_verified");
}

#[test]
fn model_stream_version_and_whole_extent_gates_remain_distinct() {
    let points = format(Profile::Sdk2Mid360V0, StreamKind::Points);
    let (raw, _) = recipe(PLAN, "sdk2_cartesian32");
    for offset in [1, 5] {
        let mut wrong = raw.clone();
        wrong[offset] ^= 1;
        assert_eq!(kind(&wrong, points), ErrorKind::Incomplete);
    }
    let mut wrong_version = raw.clone();
    wrong_version[0] = 5;
    assert_eq!(kind(&wrong_version, points), ErrorKind::Unsupported);
    let mut unknown = raw.clone();
    unknown[10] = 0x11;
    assert_eq!(kind(&unknown, points), ErrorKind::Unsupported);
    assert_eq!(kind(&raw[..raw.len() - 1], points), ErrorKind::Incomplete);
    let mut extra = raw;
    extra.push(0);
    assert_eq!(kind(&extra, points), ErrorKind::Incomplete);
    let (sphere, _) = recipe(PLAN, "sdk2_spherical");
    for profile in [
        Profile::Sdk2Mid360lV0,
        Profile::Sdk2HapTxV0,
        Profile::Sdk2HapT1V0,
    ] {
        assert_eq!(
            kind(&sphere, format(profile, StreamKind::Points)),
            ErrorKind::Unsupported
        );
    }
    let (imu, _) = recipe(PLAN, "sdk2_imu");
    assert_eq!(kind(&imu, points), ErrorKind::Unsupported);
    assert_eq!(
        classify(&validate_format(format(Profile::Sdk2HapT1V0, StreamKind::Imu)).unwrap_err()).kind,
        ErrorKind::Unsupported
    );
    let (triple, _) = recipe(LEGACY, "legacy_type_8");
    assert_eq!(
        kind(
            &triple,
            format(Profile::LegacyHorizonV5, StreamKind::Points)
        ),
        ErrorKind::Unsupported
    );
    assert_eq!(
        kind(&triple, format(Profile::LegacyAviaV5, StreamKind::Imu)),
        ErrorKind::Unsupported
    );
}

#[test]
fn legacy_utc_fields_remain_raw_and_nominal_ranges_do_not_become_calendar_time() {
    let (mut raw, _) = recipe(PLAN, "legacy_imu_pps");
    raw[8] = 3;
    raw[10..18].copy_from_slice(&[255, 12, 31, 23, 0xff, 0xa3, 0x93, 0xd6]);
    let configured = format(Profile::LegacyAviaV5, StreamKind::Imu);
    let value = decode(&raw, configured).unwrap().value;
    assert_eq!(value["clock"]["utc_fields"]["year_raw"], 255);
    assert_eq!(
        value["clock"]["utc_fields"]["microseconds_within_hour_raw"],
        3_599_999_999_u64
    );
    assert_eq!(
        value["clock"]["utc_fields"]["field_ranges_status"],
        "within_nominal"
    );
    assert!(value["clock"].get("nanoseconds_raw").is_none());
    for (index, replacement) in [(11, 0), (12, 32), (13, 24)] {
        let mut invalid = raw.clone();
        invalid[index] = replacement;
        let value = decode(&invalid, configured).unwrap().value;
        assert_eq!(
            value["clock"]["utc_fields"]["field_ranges_status"],
            "out_of_nominal"
        );
    }
    raw[14..18].copy_from_slice(&3_600_000_000_u32.to_le_bytes());
    let value = decode(&raw, configured).unwrap().value;
    assert_eq!(
        value["clock"]["utc_fields"]["field_ranges_status"],
        "out_of_nominal"
    );
    raw[8] = 5;
    let value = decode(&raw, configured).unwrap().value;
    assert_eq!(value["clock"]["domain"], "unqualified");
    assert!(value["clock"].get("utc_fields").is_none());
}
