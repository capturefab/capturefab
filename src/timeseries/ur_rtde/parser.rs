//! Exact RTDE fixed-type recipes, frames and raw IEEE values.
#![forbid(unsafe_code)]
#![deny(clippy::panic_in_result_fn)]

use super::malformed;
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) const MAX_FRAME: usize = 8192;

#[derive(Debug, Clone, Copy, Deserialize)]
pub(super) enum WireType {
    #[serde(rename = "BOOL")]
    Bool,
    #[serde(rename = "UINT8")]
    U8,
    #[serde(rename = "UINT32")]
    U32,
    #[serde(rename = "UINT64")]
    U64,
    #[serde(rename = "INT32")]
    I32,
    #[serde(rename = "DOUBLE")]
    Double,
    #[serde(rename = "VECTOR3D")]
    Vector3d,
    #[serde(rename = "VECTOR6D")]
    Vector6d,
    #[serde(rename = "VECTOR6INT32")]
    Vector6i32,
    #[serde(rename = "VECTOR6UINT32")]
    Vector6u32,
}
impl WireType {
    const fn token(self) -> &'static str {
        match self {
            Self::Bool => "BOOL",
            Self::U8 => "UINT8",
            Self::U32 => "UINT32",
            Self::U64 => "UINT64",
            Self::I32 => "INT32",
            Self::Double => "DOUBLE",
            Self::Vector3d => "VECTOR3D",
            Self::Vector6d => "VECTOR6D",
            Self::Vector6i32 => "VECTOR6INT32",
            Self::Vector6u32 => "VECTOR6UINT32",
        }
    }
    const fn bytes(self) -> usize {
        match self {
            Self::Bool | Self::U8 => 1,
            Self::U32 | Self::I32 => 4,
            Self::U64 | Self::Double => 8,
            Self::Vector3d | Self::Vector6i32 | Self::Vector6u32 => 24,
            Self::Vector6d => 48,
        }
    }
    fn decode(self, cursor: &mut &[u8]) -> Result<Value> {
        Ok(match self {
            Self::Bool => {
                let [raw] = take(cursor)?;
                json!({"raw_u8":raw,"value":raw!=0})
            }
            Self::U8 => {
                let [raw] = take(cursor)?;
                json!(raw)
            }
            Self::U32 => json!(u32::from_be_bytes(take(cursor)?)),
            Self::U64 => json!(u64::from_be_bytes(take(cursor)?)),
            Self::I32 => json!(i32::from_be_bytes(take(cursor)?)),
            Self::Double => double(take(cursor)?),
            Self::Vector3d | Self::Vector6d => {
                let count = if matches!(self, Self::Vector3d) { 3 } else { 6 };
                Value::Array(
                    std::iter::repeat_with(|| take(cursor).map(double))
                        .take(count)
                        .collect::<Result<Vec<_>>>()?,
                )
            }
            Self::Vector6i32 => Value::Array(
                std::iter::repeat_with(|| take(cursor).map(|word| json!(i32::from_be_bytes(word))))
                    .take(6)
                    .collect::<Result<Vec<_>>>()?,
            ),
            Self::Vector6u32 => Value::Array(
                std::iter::repeat_with(|| take(cursor).map(|word| json!(u32::from_be_bytes(word))))
                    .take(6)
                    .collect::<Result<Vec<_>>>()?,
            ),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Field {
    field: String,
    wire_type: WireType,
}
#[derive(Debug)]
pub(super) struct Plan {
    fields: Vec<Field>,
    names: String,
    data_bytes: usize,
}
impl Plan {
    pub(super) fn new(fields: &[Field]) -> Result<Self> {
        if !(1..=128).contains(&fields.len()) {
            return Err(Usage("RTDE requires 1–128 output fields".into()).into());
        }
        let mut unique = BTreeSet::new();
        let mut name_bytes = fields.len() - 1;
        for field in fields {
            name_bytes = name_bytes
                .checked_add(field.field.len())
                .filter(|&bytes| bytes <= 2048)
                .ok_or_else(|| {
                    Usage("RTDE comma-separated output names exceed 2048 bytes".into())
                })?;
            let mut chars = field.field.bytes();
            if !chars
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                || !chars.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || !unique.insert(&field.field)
            {
                return Err(
                    Usage("RTDE output fields must be unique ASCII identifiers".into()).into(),
                );
            }
        }
        let names = fields
            .iter()
            .map(|field| field.field.as_str())
            .collect::<Vec<_>>()
            .join(",");
        if names.len() > 2048 {
            return Err(Usage("RTDE comma-separated output names exceed 2048 bytes".into()).into());
        }
        let data_bytes = 1 + fields
            .iter()
            .map(|field| field.wire_type.bytes())
            .sum::<usize>();
        Ok(Self {
            fields: fields.to_vec(),
            names,
            data_bytes,
        })
    }
    pub(super) fn setup(&self, frequency: f64) -> Result<Vec<u8>> {
        let mut payload = frequency.to_be_bytes().to_vec();
        payload.extend(self.names.bytes());
        packet(b'O', &payload)
    }
    pub(super) fn accept_recipe(&self, payload: &[u8]) -> Result<u8> {
        let (&id, types) = payload
            .split_first()
            .ok_or_else(|| malformed("missing output recipe"))?;
        let types = std::str::from_utf8(types).map_err(|_| malformed("non-ASCII output types"))?;
        let types = types.split(',').collect::<Vec<_>>();
        if types.len() != self.fields.len() {
            return Err(malformed("output type count differs from requested fields"));
        }
        for (field, actual) in self.fields.iter().zip(types) {
            if matches!(actual, "NOT_FOUND" | "IN_USE") {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    format!("RTDE output {} unavailable: {actual}", field.field),
                ));
            }
            if actual != field.wire_type.token() {
                return Err(malformed(
                    "output type differs from explicitly requested type",
                ));
            }
        }
        if id == 0 {
            return Err(malformed("output recipe ID zero is invalid"));
        }
        Ok(id)
    }
    pub(super) fn check_data(&self, recipe: u8, payload: &[u8]) -> Result<()> {
        if payload.len() != self.data_bytes || payload.first() != Some(&recipe) {
            return Err(malformed(
                "output recipe ID or exact data extent differs from setup",
            ));
        }
        Ok(())
    }
    pub(super) fn decode_data(&self, recipe: u8, payload: &[u8]) -> Result<Value> {
        self.check_data(recipe, payload)?;
        let mut cursor = payload
            .get(1..)
            .ok_or_else(|| malformed("missing output fields"))?;
        let mut values = Vec::with_capacity(self.fields.len());
        for field in &self.fields {
            values.push(
                json!({"field":field.field,"wire_type":field.wire_type.token(),
                "value":field.wire_type.decode(&mut cursor)?}),
            );
        }
        Ok(Value::Array(values))
    }
}
fn take<const N: usize>(cursor: &mut &[u8]) -> Result<[u8; N]> {
    let (word, rest) = cursor
        .split_first_chunk::<N>()
        .ok_or_else(|| malformed("truncated native field"))?;
    let word = *word;
    *cursor = rest;
    Ok(word)
}
fn double(bytes: [u8; 8]) -> Value {
    let bits = u64::from_be_bytes(bytes);
    let value = f64::from_bits(bits);
    let classification = if value.is_nan() {
        "nan"
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            "positive_infinity"
        } else {
            "negative_infinity"
        }
    } else {
        "finite"
    };
    let mut result = json!({"ieee754_hex":format!("{bits:016X}"),"classification":classification});
    if value.is_finite() {
        result["value"] = json!(value);
    }
    result
}
pub(super) fn packet(command: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let size = payload
        .len()
        .checked_add(3)
        .filter(|&size| size <= MAX_FRAME)
        .ok_or_else(|| malformed("outgoing packet exceeds profile bound"))?;
    let mut packet = u16::try_from(size)?.to_be_bytes().to_vec();
    packet.push(command);
    packet.extend_from_slice(payload);
    Ok(packet)
}
pub(super) fn frame_bytes(header: [u8; 3]) -> Result<usize> {
    let [high, low, _] = header;
    let bytes = usize::from(u16::from_be_bytes([high, low]));
    if !(3..=MAX_FRAME).contains(&bytes) {
        return Err(malformed("packet length outside 3..8192"));
    }
    Ok(bytes)
}
pub(super) fn payload(frame: &[u8]) -> Result<(u8, &[u8])> {
    let header = frame
        .first_chunk::<3>()
        .copied()
        .ok_or_else(|| malformed("short packet header"))?;
    if frame_bytes(header)? != frame.len() {
        return Err(malformed("packet size differs from received extent"));
    }
    let [_, _, command] = header;
    Ok((
        command,
        frame
            .get(3..)
            .ok_or_else(|| malformed("missing packet payload"))?,
    ))
}
pub(super) fn software(payload: &[u8]) -> Result<Value> {
    if payload.len() != 16 {
        return Err(malformed("controller software reply requires 16 bytes"));
    }
    let mut cursor = payload;
    Ok(
        json!({"major":u32::from_be_bytes(take(&mut cursor)?),"minor":u32::from_be_bytes(take(&mut cursor)?),
        "bugfix":u32::from_be_bytes(take(&mut cursor)?),"build":u32::from_be_bytes(take(&mut cursor)?)}),
    )
}
pub(super) fn text(payload: &[u8], version: u16) -> Result<Value> {
    let (level, message, source) = if version == 1 {
        let (&level, message) = payload
            .split_first()
            .ok_or_else(|| malformed("empty v1 text message"))?;
        (level, message, &[] as &[u8])
    } else {
        let (&message_len, tail) = payload
            .split_first()
            .ok_or_else(|| malformed("missing v2 message length"))?;
        let (message, tail) = tail
            .split_at_checked(usize::from(message_len))
            .ok_or_else(|| malformed("short v2 message"))?;
        let (&source_len, tail) = tail
            .split_first()
            .ok_or_else(|| malformed("missing v2 source length"))?;
        let (source, tail) = tail
            .split_at_checked(usize::from(source_len))
            .ok_or_else(|| malformed("short v2 source"))?;
        let [level] = tail else {
            return Err(malformed("v2 text requires exact terminal level byte"));
        };
        (*level, message, source)
    };
    let name = match level {
        0 => "exception",
        1 => "error",
        2 => "warning",
        3 => "info",
        _ => "unknown",
    };
    Ok(
        json!({"message_protocol_version":version,"level_raw":level,"level":name,
        "message_base64":STANDARD.encode(message),"source_base64":STANDARD.encode(source)}),
    )
}
pub(super) fn raw(frame: &[u8]) -> Value {
    json!({"encoding":"base64","bytes":frame.len(),"data":STANDARD.encode(frame)})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(text: &str) -> Vec<u8> {
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert_eq!(rest, &[] as &[u8]);
        pairs
            .iter()
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }
    fn evidence() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/ur_rtde/protocol-literals.json"
        ))
        .unwrap()
    }
    fn plan() -> Plan {
        Plan::new(&[
            Field {
                field: "timestamp".into(),
                wire_type: WireType::Double,
            },
            Field {
                field: "robot_mode".into(),
                wire_type: WireType::I32,
            },
            Field {
                field: "actual_digital_input_bits".into(),
                wire_type: WireType::U64,
            },
        ])
        .unwrap()
    }
    #[test]
    fn literal_handshake_native_endpoints_ieee_payload_and_u64_are_exact() {
        let evidence = evidence();
        let plan = plan();
        assert_eq!(
            packet(b'V', &[0, 2]).unwrap(),
            hex(evidence["handshake"]["request_v2_hex"].as_str().unwrap())
        );
        assert_eq!(
            plan.setup(125.0).unwrap(),
            hex(evidence["handshake"]["setup_output_hex"].as_str().unwrap())
        );
        let reply = hex(evidence["handshake"]["setup_recipe7_types_hex"]
            .as_str()
            .unwrap());
        assert_eq!(plan.accept_recipe(payload(&reply).unwrap().1).unwrap(), 7);
        for case in evidence["state_cases"].as_array().unwrap() {
            let frame = hex(case["frame_hex"].as_str().unwrap());
            let decoded = plan.decode_data(7, payload(&frame).unwrap().1).unwrap();
            assert_eq!(
                decoded[0]["value"]["ieee754_hex"],
                case["timestamp"]["ieee754_hex"]
            );
            assert_eq!(
                decoded[0]["value"]["classification"],
                case["timestamp"]["classification"]
            );
            assert_eq!(decoded[1]["value"], case["robot_mode"]);
            assert_eq!(decoded[2]["value"], case["actual_digital_input_bits"]);
            if case["timestamp"]["numeric_value_absent"] == true {
                assert!(decoded[0]["value"].get("value").is_none());
            }
        }
    }
    #[test]
    fn every_fixed_type_has_exact_shape_and_nonfinite_never_becomes_null() {
        let fields = [
            WireType::Bool,
            WireType::U8,
            WireType::I32,
            WireType::U32,
            WireType::U64,
            WireType::Double,
            WireType::Vector3d,
            WireType::Vector6d,
            WireType::Vector6i32,
            WireType::Vector6u32,
        ];
        for wire_type in fields {
            let plan = Plan::new(&[Field {
                field: "synthetic".into(),
                wire_type,
            }])
            .unwrap();
            let mut body = vec![8];
            body.extend(vec![0xff; wire_type.bytes()]);
            let decoded = plan.decode_data(8, &body).unwrap();
            let value = &decoded[0]["value"];
            match wire_type {
                WireType::Bool => assert_eq!(*value, json!({"raw_u8":255,"value":true})),
                WireType::U8 => assert_eq!(*value, json!(255)),
                WireType::I32 => assert_eq!(*value, json!(-1)),
                WireType::U32 => assert_eq!(*value, json!(u32::MAX)),
                WireType::U64 => assert_eq!(*value, json!(u64::MAX)),
                WireType::Double => {
                    assert_eq!(value["ieee754_hex"], "FFFFFFFFFFFFFFFF");
                    assert_eq!(value["classification"], "nan");
                    assert!(value.get("value").is_none());
                }
                WireType::Vector3d => assert_eq!(value.as_array().unwrap().len(), 3),
                WireType::Vector6d | WireType::Vector6i32 | WireType::Vector6u32 => {
                    assert_eq!(value.as_array().unwrap().len(), 6);
                }
            }
            let mut short = body.clone();
            short.pop();
            plan.decode_data(8, &short).unwrap_err();
            body.push(0);
            plan.decode_data(8, &body).unwrap_err();
        }
        for (word, status) in [
            (0x7ff0_0000_0000_0000_u64, "positive_infinity"),
            (0xfff0_0000_0000_0000, "negative_infinity"),
            (0x7ff8_0000_0000_0001, "nan"),
        ] {
            let value = double(word.to_be_bytes());
            assert_eq!(value["classification"], status);
            assert!(value.get("value").is_none());
        }
        let value = double(0x8000_0000_0000_0000_u64.to_be_bytes());
        assert_eq!(
            value["value"].as_f64().unwrap().to_bits(),
            0x8000_0000_0000_0000
        );
    }
    #[test]
    fn negotiation_and_lengths_are_strict_and_names_have_both_bounds() {
        let plan = plan();
        for types in [
            b"\x00DOUBLE,INT32,UINT64".as_slice(),
            b"\x07DOUBLE,INT32",
            b"\x07DOUBLE,INT32,UINT64,BOOL",
            b"\x07DOUBLE,INT32,UINT32",
            b"\x07NOT_FOUND,INT32,UINT64",
        ] {
            plan.accept_recipe(types).unwrap_err();
        }
        for bytes in [0_u16, 1, 2, 8193, u16::MAX] {
            let [hi, lo] = bytes.to_be_bytes();
            frame_bytes([hi, lo, b'U']).unwrap_err();
        }
        assert_eq!(frame_bytes([0, 3, b'P']).unwrap(), 3);
        let fields = (0..128)
            .map(|index| Field {
                field: format!("f{index}"),
                wire_type: WireType::Vector6d,
            })
            .collect::<Vec<_>>();
        let largest = Plan::new(&fields).unwrap();
        assert_eq!(largest.data_bytes + 3, 6148);
        let mut fields = fields;
        fields.push(Field {
            field: "extra".into(),
            wire_type: WireType::Bool,
        });
        Plan::new(&fields).unwrap_err();
        for names in [
            vec!["same", "same"],
            vec!["bad,name"],
            vec!["bad name"],
            vec!["0bad"],
            vec![""],
        ] {
            let fields = names
                .into_iter()
                .map(|name| Field {
                    field: name.into(),
                    wire_type: WireType::Bool,
                })
                .collect::<Vec<_>>();
            Plan::new(&fields).unwrap_err();
        }
        let allowed = Field {
            field: "x".repeat(2048),
            wire_type: WireType::Bool,
        };
        Plan::new(std::slice::from_ref(&allowed)).unwrap();
        let mut too_long = allowed;
        too_long.field.push('x');
        Plan::new(&[too_long]).unwrap_err();
    }
    #[test]
    fn pre_negotiation_v1_and_post_acceptance_v2_text_never_share_layout() {
        let first = text(b"\x02old", 1).unwrap();
        assert_eq!(first["level_raw"], 2);
        assert_eq!(first["message_base64"], "b2xk");
        assert_eq!(first["source_base64"], "");
        let evidence = evidence();
        let frame = hex(evidence["text_v2"]["warning_hex"].as_str().unwrap());
        let (_, body) = payload(&frame).unwrap();
        let value = text(body, 2).unwrap();
        assert_eq!(value["message_base64"], "YnVzeQ==");
        assert_eq!(value["source_base64"], "UlRERQ==");
        for bytes in [
            b"\x04busy\x04RTDE".as_slice(),
            b"\x04busy\x04RTDE\x02extra",
            b"\xffbusy",
            b"",
        ] {
            text(bytes, 2).unwrap_err();
        }
        text(b"", 1).unwrap_err();
    }
}
