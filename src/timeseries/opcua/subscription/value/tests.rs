use super::*;
use opcua::types::{BinaryDecodable as _, DateTime, StatusCode};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::{io::Cursor, time::Duration};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn native(value: Variant) -> Value {
    data_value(&DataValue::value_only(value), deadline()).unwrap()
}
fn assert_subset(actual: &Value, expected: &Value) {
    match expected {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "value_lexeme" {
                    assert_eq!(actual["value"].to_string(), value.as_str().unwrap());
                } else {
                    assert!(actual.get(key).is_some(), "missing {key}: {actual}");
                    assert_subset(&actual[key], value);
                }
            }
        }
        Value::Array(values) => {
            let actual = actual.as_array().unwrap();
            assert_eq!(actual.len(), values.len());
            for (a, e) in actual.iter().zip(values) {
                assert_subset(a, e);
            }
        }
        _ => assert_eq!(actual, expected),
    }
}
fn no_null(value: &Value) {
    assert!(!value.is_null());
    match value {
        Value::Array(values) => {
            for value in values {
                no_null(value);
            }
        }
        Value::Object(fields) => {
            for value in fields.values() {
                no_null(value);
            }
        }
        _ => (),
    }
}

#[test]
fn independent_binary_literals_keep_all_exposed_native_fields_and_normalization_limits() {
    let text = include_str!("../../../../../tests/fixtures/opcua/subscription-literals.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(text.as_bytes())),
        "91e7cd69323f907ed8500018b37af241e002f58a0194e993f13915e3362e6411"
    );
    let cases: Value = serde_json::from_str(text).unwrap();
    let context = ContextOwned::new_default(NamespaceMap::new(), DecodingOptions::default());
    assert_eq!(cases["datavalues"].as_array().unwrap().len(), 27);
    for case in cases["datavalues"].as_array().unwrap() {
        let raw = crate::types::decode_hex(case["binary_hex"].as_str().unwrap(), 65_536).unwrap();
        assert_eq!(
            raw.len(),
            usize::try_from(case["bytes"].as_u64().unwrap()).unwrap()
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&raw)),
            case["sha256"].as_str().unwrap()
        );
        let mut input = Cursor::new(raw.as_slice());
        let decoded = DataValue::decode(&mut input, &context.context()).unwrap();
        assert_eq!(usize::try_from(input.position()).unwrap(), raw.len());
        let actual = data_value(&decoded, deadline()).unwrap();
        assert_subset(&actual, &case["expected"]);
        assert_eq!(actual["value_present"], decoded.value.is_some());
        assert_eq!(actual["status_present"], decoded.status.is_some());
        assert_eq!(actual.get("status_raw").is_some(), decoded.status.is_some());
        assert_eq!(actual.get("value").is_some(), decoded.value.is_some());
        assert_eq!(
            actual.get("source_ticks").is_some(),
            decoded.source_timestamp.is_some()
        );
        assert_eq!(
            actual.get("server_ticks").is_some(),
            decoded.server_timestamp.is_some()
        );
        if let Some(classification) = actual["value"]["classification"].as_str()
            && classification != "finite"
        {
            assert!(actual["value"].get("value").is_none());
        }
        if actual["value"]["is_null"] == true {
            assert!(actual["value"].get("value").is_none());
            assert!(actual["value"].get("base64").is_none());
        }
        if actual["value"]["type"] == "Array" {
            assert_eq!(
                actual["value"]["null_empty_distinction"],
                "not_retained_by_sdk"
            );
        }
        no_null(&actual);
    }
}

#[test]
fn dense_ieee_arrays_preserve_payload_words_and_dimensions_without_json_expansion() {
    let words = [0x7fc0_1234_u32, 0x8000_0000, 0x7f80_0000, 1];
    let array = Array::new_multi(
        VariantScalarTypeId::Float,
        words
            .iter()
            .map(|word| Variant::Float(f32::from_bits(*word)))
            .collect::<Vec<_>>(),
        vec![2, 2],
    )
    .unwrap();
    let actual = native(Variant::Array(Box::new(array)));
    assert_eq!(actual["value"]["element_type"], "Float");
    assert_eq!(actual["value"]["element_encoding"], "ieee754_le_u32_bits");
    assert_eq!(actual["value"]["data_base64"], "NBLAfwAAAIAAAIB/AQAAAA==");
    assert_eq!(actual["value"]["dimensions"], json!([2, 2]));
    assert_eq!(actual["value"]["length"], 4);
    let words = [
        0x7ff8_0000_0000_1234_u64,
        0x8000_0000_0000_0000,
        0xfff0_0000_0000_0000,
    ];
    let array = Array::new(
        VariantScalarTypeId::Double,
        words
            .iter()
            .map(|word| Variant::Double(f64::from_bits(*word)))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let actual = native(Variant::Array(Box::new(array)));
    assert_eq!(actual["value"]["element_encoding"], "ieee754_le_u64_bits");
    assert_eq!(
        actual["value"]["data_base64"],
        "NBIAAAAA+H8AAAAAAAAAgAAAAAAAAPD/"
    );
    assert!(actual["value"].get("dimensions").is_none());
    let array = Array::new(
        VariantScalarTypeId::Double,
        vec![Variant::Double(f64::from_bits(words[0])); 65_536],
    )
    .unwrap();
    let actual = native(Variant::Array(Box::new(array)));
    assert_eq!(actual["value"]["length"], 65_536);
    assert!(serde_json::to_vec(&actual).unwrap().len() < MAX_JSON);
}

#[test]
fn large_bytes_fit_without_redundant_canonical_copy_and_expansion_is_refused() {
    let actual = native(Variant::ByteString(vec![0xaf; 700_000].into()));
    assert_eq!(actual["value"]["type"], "ByteString");
    assert_ne!(actual["value"]["base64"].as_str().unwrap(), "");
    assert!(actual.get("value_binary_base64").is_none());
    assert!(serde_json::to_vec(&actual).unwrap().len() < MAX_JSON);
    let value = DataValue::value_only(Variant::ByteString(vec![0; MAX_BINARY].into()));
    assert_eq!(
        crate::types::classify(&data_value(&value, deadline()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    let value = DataValue::value_only(Variant::String("\\\"".repeat(300_000).into()));
    assert_eq!(
        crate::types::classify(&data_value(&value, deadline()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn exact_precharge_matches_materialized_bytes_and_keeps_opaque_and_diagnostic_context() {
    let id = NodeId::new(2, 1234_u32);
    let extension = ExtensionObject::new(ByteStringBody::new(vec![0, 255, 17].into(), id));
    let value = DataValue {
        value: Some(Variant::ExtensionObject(extension)),
        status: Some(StatusCode::from(0x4092_0080_u32)),
        source_timestamp: Some(DateTime::from(-1_i64)),
        server_timestamp: Some(DateTime::from(1_i64)),
        source_picoseconds: Some(10_001),
        server_picoseconds: Some(65_535),
    };
    let mut budget = Budget::new(deadline());
    let actual = budget.data(&value, 0).unwrap();
    assert_eq!(
        serde_json::to_vec(&actual).unwrap().len(),
        MAX_JSON - budget.remaining
    );
    assert_eq!(actual["source_ticks"], -1);
    assert_eq!(actual["source_picoseconds_raw"], 10_001);
    assert_eq!(actual["source_picoseconds_effective"], 9999);
    assert_eq!(actual["value"]["encoding"], "binary");
    assert_eq!(actual["value"]["encoding_type_id"]["namespace"], 2);
    assert_eq!(actual["value"]["encoding_type_id"]["identifier"], 1234);
    assert_eq!(actual["value"]["body"]["base64"], "AP8R");
    let diagnostic = DiagnosticInfo {
        symbolic_id: Some(-1),
        namespace_uri: Some(0),
        additional_info: Some("private\0\"\\☃".into()),
        inner_status_code: Some(StatusCode::BadNoCommunication),
        inner_diagnostic_info: Some(Box::new(DiagnosticInfo {
            localized_text: Some(7),
            ..Default::default()
        })),
        ..Default::default()
    };
    let mut budget = Budget::new(deadline());
    let actual = budget.diagnostic(&diagnostic, 0).unwrap();
    assert_eq!(
        serde_json::to_vec(&actual).unwrap().len(),
        MAX_JSON - budget.remaining
    );
    assert_eq!(actual["symbolic_id"], -1);
    assert_eq!(actual["additional_info"]["value"], "private\0\"\\☃");
    assert_eq!(
        actual["inner_status_raw"],
        StatusCode::BadNoCommunication.bits()
    );
    assert_eq!(actual["inner_diagnostic_info"]["localized_text"], 7);
    assert_eq!(diagnostic_info(&diagnostic, deadline()).unwrap(), actual);
    no_null(&actual);
}

#[test]
fn malformed_shapes_depth_deadline_and_tick_endpoints_fail_or_preserve_explicitly() {
    let bad = Array {
        value_type: VariantScalarTypeId::Int32,
        values: vec![Variant::UInt32(1)],
        dimensions: None,
    };
    data_value(
        &DataValue::value_only(Variant::Array(Box::new(bad))),
        deadline(),
    )
    .unwrap_err();
    let bad = Array {
        value_type: VariantScalarTypeId::Int32,
        values: vec![Variant::Int32(1)],
        dimensions: Some(vec![u32::MAX; 3]),
    };
    data_value(
        &DataValue::value_only(Variant::Array(Box::new(bad))),
        deadline(),
    )
    .unwrap_err();
    let mut deep = Variant::Int32(1);
    for _ in 0..40 {
        deep = Variant::Variant(Box::new(deep));
    }
    assert_eq!(
        crate::types::classify(&data_value(&DataValue::value_only(deep), deadline()).unwrap_err())
            .kind,
        ErrorKind::Incomplete
    );
    assert_eq!(
        crate::types::classify(&data_value(&DataValue::default(), Instant::now()).unwrap_err())
            .kind,
        ErrorKind::Timeout
    );
    for original in [i64::MIN, -1, 0, 1] {
        let time = DateTime::from(original);
        assert_eq!(ticks(&time).unwrap(), original);
    }
    // The SDK maps the maximal wire sentinel to its end-times date. Preserve
    // that decoded time honestly, rather than pretend the original word survived.
    let normalized = DateTime::from(i64::MAX);
    assert!(ticks(&normalized).unwrap() < i64::MAX);
}
