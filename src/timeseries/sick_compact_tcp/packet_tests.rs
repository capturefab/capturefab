//! Independent frozen recipes, framing boundaries, and bounded materialization.
use super::super::{Profile, Stream};
use super::{
    Decoded, Encoded, Format, FrameNeed, MAX_FRAME_BYTES, MAX_PART_JSON, MAX_TOTAL_JSON,
    frame_need, linear, materialize, serialize_checked, validate, validate_format,
};
use crate::types::{ErrorKind, classify, hex};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
const CORPUS: &str = include_str!("../../../tests/fixtures/sick_compact_tcp/literals.json");
fn failure<T>(result: anyhow::Result<T>) -> anyhow::Error {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected typed packet refusal"),
    }
}
fn budget() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
fn bytes(text: &str) -> Vec<u8> {
    crate::types::decode_hex(text, MAX_FRAME_BYTES).unwrap()
}
fn corpus() -> Value {
    assert_eq!(
        hex(Sha256::digest(CORPUS.as_bytes())),
        "c1bb32d0cbde59f0ccd7432dfe9dbcdef88da80f23f3a46f817015fc0e476af2"
    );
    serde_json::from_str(CORPUS).unwrap()
}
fn piece(value: &Value) -> Vec<u8> {
    if let Some(text) = value["hex"].as_str() {
        return bytes(text);
    }
    if let Some(text) = value["repeat_hex"].as_str() {
        return bytes(text).repeat(usize::try_from(value["repeat"].as_u64().unwrap()).unwrap());
    }
    let width = match value["encoding"].as_str().unwrap() {
        "u8" => 1,
        "u16_le" => 2,
        "u32_le" | "ieee_u32_le" => 4,
        other => panic!("unknown independent recipe {other}"),
    };
    let count = usize::try_from(value["count"].as_u64().unwrap()).unwrap();
    let prefix = value["prefix"].as_array().unwrap();
    let suffix = value["suffix"].as_array().unwrap();
    let mut result = Vec::new();
    for index in 0..count {
        let word = if index < prefix.len() {
            prefix[index].as_u64().unwrap()
        } else if index >= count - suffix.len() {
            suffix[index - (count - suffix.len())].as_u64().unwrap()
        } else {
            value["fill"].as_u64().unwrap()
        };
        result.extend_from_slice(&word.to_le_bytes()[..width]);
    }
    result
}
fn raw(case: &Value) -> Vec<u8> {
    let mut raw = Vec::new();
    for value in case["recipe"].as_array().unwrap() {
        raw.extend(piece(value));
    }
    raw.extend(bytes(case["crc32_le_hex"].as_str().unwrap()));
    assert_eq!(raw.len() as u64, case["packet_bytes"].as_u64().unwrap());
    assert_eq!(
        hex(Sha256::digest(&raw)),
        case["packet_sha256"].as_str().unwrap()
    );
    raw
}
fn format(kind: u64) -> Format {
    Format {
        profile: if kind == 1 {
            Profile::Lrs4000
        } else {
            Profile::Multiscan200
        },
        stream: match kind {
            1 | 6 => Stream::Points,
            3 => Stream::Ambient,
            7 => Stream::Imu,
            _ => panic!("unknown oracle type"),
        },
    }
}
fn word_bytes(values: &[Value], width: usize) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.as_u64().unwrap().to_le_bytes()[..width].to_vec())
        .collect()
}
fn columns(plan: &Decoded, raw: &[u8]) -> BTreeMap<(Option<u64>, String), Vec<u8>> {
    let mut columns = BTreeMap::new();
    let mut total = 0;
    for index in 0..plan.parts {
        let part = materialize(plan, raw, index, budget()).unwrap();
        assert_eq!(part["part_index"], index);
        assert_eq!(part["part_count"], plan.parts);
        assert_eq!(part["native_elements_declared"], plan.elements);
        assert_eq!(
            part["native_elements_scope"],
            "complete_telegram_not_per_part"
        );
        assert_eq!(part.get("native_metadata").is_some(), index == 0);
        assert_eq!(
            serde_json::to_vec(&part).unwrap().len(),
            plan.json_bytes[index]
        );
        assert!(plan.json_bytes[index] <= MAX_PART_JSON);
        total += plan.json_bytes[index];
        for column in part["native_columns"].as_array().unwrap() {
            let key = (
                column["module_index"].as_u64(),
                column["id"].as_str().unwrap().to_owned(),
            );
            let output: &mut Vec<u8> = columns.entry(key).or_default();
            assert_eq!(
                column["global_byte_range"][0].as_u64().unwrap(),
                output.len() as u64
            );
            let data = STANDARD.decode(column["data"].as_str().unwrap()).unwrap();
            assert_eq!(
                column["global_byte_range"][1].as_u64().unwrap(),
                (output.len() + data.len()) as u64
            );
            output.extend(data);
        }
    }
    assert!(total <= MAX_TOTAL_JSON);
    columns
}
#[test]
fn all_independent_recipes_preserve_full_native_columns_and_full_geometry() {
    for case in corpus()["cases"].as_array().unwrap() {
        let raw = raw(case);
        let kind = case["telegram_type"].as_u64().unwrap();
        let plan = validate(&raw, format(kind), budget()).unwrap();
        let columns = columns(&plan, &raw);
        assert!(plan.parts <= 64);
        if kind == 3 || kind == 6 {
            for recipe in case["recipe"].as_array().unwrap() {
                let id = match recipe["field"].as_str().unwrap() {
                    "ambient_light_data" => "ambient_light",
                    "elevation_angles" => "elevation",
                    "azimuth_angles" => "azimuth",
                    "relative_time_stamps" => "relative_timestamp",
                    "column_properties" => "column_properties",
                    "distance" => "distance",
                    "intensity_packed" => "intensity_packed",
                    "pulse_width" => "pulse_width",
                    "echo_properties" => "echo_properties",
                    _ => continue,
                };
                assert_eq!(
                    &columns[&(None, id.to_owned())],
                    &piece(recipe),
                    "{} {id}",
                    case["name"]
                );
            }
            if kind == 6 {
                assert_eq!(plan.elements, case["oracle"]["N"].as_u64().unwrap());
            }
        } else if kind == 7 {
            for (id, key) in [
                ("acceleration", "acceleration_words"),
                ("angular_velocity", "angular_velocity_words"),
                ("orientation", "orientation_words_wxyz"),
            ] {
                assert_eq!(
                    &columns[&(None, id.to_owned())],
                    &word_bytes(case["oracle"][key].as_array().unwrap(), 4)
                );
            }
        } else if let Some(modules) = case["oracle"]["native"]["modules"].as_array() {
            for (index, module) in modules.iter().enumerate() {
                let index = Some(index as u64);
                for (id, key, width) in [
                    ("timestamp_start", "timestamp_start_raw", 8),
                    ("timestamp_stop", "timestamp_stop_raw", 8),
                    ("phi", "phi_words", 4),
                    ("theta_start", "theta_start_words", 4),
                    ("theta_stop", "theta_stop_words", 4),
                ] {
                    assert_eq!(
                        &columns[&(index, id.to_owned())],
                        &word_bytes(module[key].as_array().unwrap(), width)
                    );
                }
                for (id, key, array, width) in [
                    ("distance", "distance_raw", "echo_values", 2),
                    ("rssi", "rssi_raw", "echo_values", 2),
                    ("azimuth", "azimuth_raw", "beam_values", 2),
                    ("properties", "properties_raw", "beam_values", 1),
                ] {
                    let values: Vec<_> = module[array]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|v| v.get(key).cloned())
                        .collect();
                    if !values.is_empty() {
                        assert_eq!(
                            &columns[&(index, id.to_owned())],
                            &word_bytes(&values, width)
                        );
                    }
                }
            }
        } else {
            assert_eq!(plan.elements, 27000);
            assert!(raw.len() > 65507);
            let oracle = &case["oracle"];
            for (id, offset, width, echoes) in [
                ("distance", "distance_word_offset", 2, 3),
                ("rssi", "rssi_word_offset", 2, 3),
                ("azimuth", "azimuth_offset", 2, 1),
                ("properties", "properties_offset", 1, 1),
            ] {
                let mut expected = Vec::new();
                for row in 0..9000 {
                    for echo in 0..echoes {
                        let at = usize::try_from(oracle["record_data_start"].as_u64().unwrap())
                            .unwrap()
                            + row
                                * usize::try_from(oracle["record_stride"].as_u64().unwrap())
                                    .unwrap()
                            + echo
                                * usize::try_from(oracle["echo_stride"].as_u64().unwrap()).unwrap()
                            + usize::try_from(oracle[offset].as_u64().unwrap()).unwrap();
                        expected.extend_from_slice(&raw[at..at + width]);
                    }
                }
                assert_eq!(&columns[&(Some(0), id.to_owned())], &expected);
            }
        }
    }
}
#[test]
fn framing_handles_every_small_prefix_and_unequal_modules_without_resync() {
    for case in corpus()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["packet_bytes"].as_u64().unwrap() < 1000)
    {
        let raw = raw(case);
        let format = format(case["telegram_type"].as_u64().unwrap());
        for count in 0..=raw.len() {
            let need = frame_need(&raw[..count], format, budget()).unwrap();
            if need.complete {
                assert_eq!(need.next, raw.len());
            } else {
                assert!(need.next > count);
            }
        }
        let mut coalesced = raw.clone();
        coalesced.extend_from_slice(&raw);
        assert_eq!(
            frame_need(&coalesced, format, budget()).unwrap(),
            FrameNeed {
                next: raw.len(),
                complete: true
            }
        );
        assert_eq!(
            classify(&failure(validate(&coalesced, format, budget()))).kind,
            ErrorKind::Incomplete
        );
    }
}
#[test]
fn odd_rssi_retains_available_bytes_without_invented_nibble_or_samples() {
    let corpus = corpus();
    let case = corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "multiscan200_type6_odd_count_opaque")
        .unwrap();
    let raw = raw(case);
    let plan = validate(&raw, format(6), budget()).unwrap();
    let part = materialize(&plan, &raw, 0, budget()).unwrap();
    let rssi = part["native_columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["id"] == "intensity_packed")
        .unwrap();
    assert_eq!(rssi["packed_rssi"]["declared_samples"], 3);
    assert_eq!(rssi["packed_rssi"]["declared_sample_bit_count"], 36);
    assert_eq!(rssi["packed_rssi"]["available_bit_count"], 32);
    assert_eq!(rssi["packed_rssi"]["global_bit_range"], json!([0, 32]));
    assert!(rssi.get("global_element_range").is_none());
    assert_eq!(
        STANDARD.decode(rssi["data"].as_str().unwrap()).unwrap(),
        bytes("23c1ab56")
    );
}
#[test]
fn whole_crc_immutable_plan_and_original_deadline_gate_publication() {
    let corpus = corpus();
    let raw = raw(&corpus["cases"][0]);
    let format = format(1);
    let plan = validate(&raw, format, budget()).unwrap();
    let mut changed = raw.clone();
    changed[40] ^= 1;
    assert_eq!(
        classify(&failure(validate(&changed, format, budget()))).kind,
        ErrorKind::Incomplete
    );
    assert_eq!(
        classify(&materialize(&plan, &changed, 0, budget()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    assert_eq!(
        classify(&failure(validate(&raw, format, Instant::now()))).kind,
        ErrorKind::Timeout
    );
    assert_eq!(
        classify(&materialize(&plan, &raw, 0, Instant::now()).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert_eq!(
        classify(
            &validate_format(Format {
                profile: Profile::Lrs4000,
                stream: Stream::Ambient
            })
            .unwrap_err()
        )
        .kind,
        ErrorKind::Unsupported
    );
}
#[test]
fn structural_capacities_and_empty_geometry_do_not_iterate_untrusted_counts() {
    let mut empty = bytes("0202020201000000000000000000000000000000000000000300000000000000");
    empty.extend_from_slice(&crc32fast::hash(&empty).to_le_bytes());
    let plan = validate(&empty, format(1), budget()).unwrap();
    assert_eq!(plan.elements, 0);
    assert_eq!(plan.parts, 1);
    let mut oversized = bytes("02020202060000000000000000000000000000000000000001000000ffffffff");
    assert_eq!(
        classify(&frame_need(&oversized, format(6), budget()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    oversized[0] = 3;
    assert_eq!(
        classify(&frame_need(&oversized, format(6), budget()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn expiry_after_precheck_returns_a_value_serializer_error_without_display_panic() {
    let raw = [0_u8, 1, 2];
    let encoded = Encoded {
        raw: &raw,
        layout: linear(0, 3, 1),
        start: 0,
        bytes: 3,
        deadline: Instant::now(),
    };
    // The first production check passed; the original deadline then expires
    // during collection. The old formatter would return fmt::Error here and
    // serde_json's actual Value collector would panic inside ToString.
    let mut checks = 0;
    let error = serialize_checked(serde_json::value::Serializer, &encoded, || {
        checks += 1;
        if checks == 1 {
            Ok(())
        } else {
            Err(crate::types::Categorized::error(
                ErrorKind::Timeout,
                "injected original deadline after collection",
            ))
        }
    })
    .unwrap_err();
    assert_eq!(checks, 2);
    assert!(
        error
            .to_string()
            .contains("injected original deadline after collection")
    );
    assert_eq!(
        serialize_checked(serde_json::value::Serializer, &encoded, || Ok(())).unwrap(),
        json!("AAEC")
    );
    let mut output = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut output);
    let mut writer_checks = 0;
    let error = serialize_checked(&mut serializer, &encoded, || {
        writer_checks += 1;
        if writer_checks == 1 {
            Ok(())
        } else {
            Err(crate::types::Categorized::error(
                ErrorKind::Timeout,
                "injected writer deadline after collection",
            ))
        }
    })
    .unwrap_err();
    assert_eq!(writer_checks, 2);
    assert!(
        error
            .to_string()
            .contains("injected writer deadline after collection")
    );
    assert_eq!(output.as_slice(), b"\"AAEC\"");
}
