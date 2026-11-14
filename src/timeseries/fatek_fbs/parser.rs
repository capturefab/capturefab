//! Bounded FATEK 44/46 reads with literal native addresses and additive LRC.
#![forbid(unsafe_code)]
#![deny(clippy::panic_in_result_fn)]

use super::malformed;
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) const MAX_REPLY: usize = 265;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DataType {
    Bool,
    #[default]
    U16,
    I16,
    U32,
    I32,
}

impl DataType {
    const fn digits(self) -> usize {
        match self {
            Self::Bool => 1,
            Self::U16 | Self::I16 => 4,
            Self::U32 | Self::I32 => 8,
        }
    }
    fn value(self, bytes: &[u8]) -> Result<Value> {
        if matches!(self, Self::Bool) {
            return match bytes {
                b"0" => Ok(Value::Bool(false)),
                b"1" => Ok(Value::Bool(true)),
                _ => Err(malformed("discrete status requires ASCII 0 or 1")),
            };
        }
        let raw = parse_hex(bytes)?;
        Ok(match self {
            Self::U16 | Self::U32 => json!(raw),
            Self::I16 => json!(i16::from_ne_bytes(u16::try_from(raw)?.to_ne_bytes())),
            Self::I32 => json!(i32::from_ne_bytes(raw.to_ne_bytes())),
            Self::Bool => return Err(malformed("invalid native data type")),
        })
    }
}

#[derive(Debug)]
pub(super) struct Plan {
    address: String,
    data_type: DataType,
    elements: u16,
}

impl Plan {
    pub(super) fn new(address: &str, data_type: DataType, elements: u16) -> Result<Self> {
        let (start, limit, stride) = native_address(address, data_type)?;
        let count_limit = match data_type {
            DataType::Bool => 256,
            DataType::U16 | DataType::I16 => 64,
            DataType::U32 | DataType::I32 => 32,
        };
        if elements == 0 || elements > count_limit {
            return Err(Usage(
                "FATEK elements must be 1–256 for bool, 1–64 for native16 or 1–32 for native32"
                    .into(),
            )
            .into());
        }
        // Ordinary native32 consumes two words (Appendix1-5). Combined W/DW
        // views have a start bound only: no unproved next-bit address is formed.
        if start > limit
            || stride.is_some_and(|stride| start + u32::from(elements) * stride - 1 > limit)
        {
            return Err(Usage("FATEK read crosses the native protocol address range; actual CPU/project capacities may be smaller".into()).into());
        }
        Ok(Self {
            address: address.into(),
            data_type,
            elements,
        })
    }

    const fn command(&self) -> &'static str {
        if matches!(self.data_type, DataType::Bool) {
            "44"
        } else {
            "46"
        }
    }
    pub(super) fn request(&self, unit: u8) -> Vec<u8> {
        let count = self.elements % 256;
        let mut request = format!(
            "\x02{unit:02X}{}{count:02X}{}",
            self.command(),
            self.address
        )
        .into_bytes();
        request.extend(format!("{:02X}", sum(&request)).bytes());
        request.push(3);
        request
    }
    pub(super) fn reply_bytes(&self) -> usize {
        9 + usize::from(self.elements) * self.data_type.digits()
    }

    pub(super) fn decode(&self, unit: u8, frame: &[u8]) -> Result<Value> {
        if !(9..=MAX_REPLY).contains(&frame.len()) || frame.first() != Some(&2) {
            return Err(malformed("invalid STX or reply extent"));
        }
        let body_end = frame.len() - 3;
        let body = frame
            .get(..body_end)
            .ok_or_else(|| malformed("missing reply body"))?;
        let checksum = frame
            .get(body_end..frame.len() - 1)
            .ok_or_else(|| malformed("missing reply LRC"))?;
        if frame.last() != Some(&3) || parse_hex(checksum)? != u32::from(sum(body)) {
            return Err(malformed("reply ETX or additive LRC mismatch"));
        }
        let prefix = format!("\x02{unit:02X}{}", self.command());
        if !body.starts_with(prefix.as_bytes()) {
            return Err(malformed("reply station or command differs from request"));
        }
        let (&error, payload) = body
            .get(5..)
            .and_then(<[u8]>::split_first)
            .ok_or_else(|| malformed("missing device error field"))?;
        let code = parse_hex(std::slice::from_ref(&error))?;
        if code != 0 {
            if !payload.is_empty() {
                return Err(malformed("device error must replace all reply data"));
            }
            return Err(malformed(&format!("device error {code:X}")));
        }
        if frame.len() != self.reply_bytes() {
            return Err(malformed("reply data extent differs from requested count"));
        }
        let mut values = Vec::with_capacity(usize::from(self.elements));
        for bytes in payload.chunks_exact(self.data_type.digits()) {
            values.push(self.data_type.value(bytes)?);
        }
        if self.elements == 1 {
            values.pop().ok_or_else(|| malformed("missing scalar data"))
        } else {
            Ok(Value::Array(values))
        }
    }
}

fn native_address(address: &str, data_type: DataType) -> Result<(u32, u32, Option<u32>)> {
    let bytes = address.as_bytes();
    let (digits, limit, stride) = match (data_type, bytes) {
        (DataType::Bool, [b'X' | b'Y' | b'M' | b'S' | b'T' | b'C', tail @ ..])
        | (DataType::U16 | DataType::I16, [b'R', b'T' | b'C', tail @ ..])
            if tail.len() == 4 =>
        {
            (tail, 9999, Some(1))
        }
        (DataType::U16 | DataType::I16, [b'R' | b'D' | b'F', tail @ ..])
            if tail.len() == 5 =>
        {
            (tail, 65_535, Some(1))
        }
        (DataType::U32 | DataType::I32, [b'D', b'R', b'T' | b'C', tail @ ..])
            if tail.len() == 4 =>
        {
            (tail, 9999, Some(2))
        }
        (DataType::U32 | DataType::I32, [b'D', b'R' | b'D' | b'F', tail @ ..])
            if tail.len() == 5 =>
        {
            (tail, 65_535, Some(2))
        }
        (DataType::U16 | DataType::I16, [b'W', b'X' | b'Y' | b'M' | b'S' | b'T' | b'C', tail @ ..])
            if tail.len() == 4 =>
        {
            (tail, 9984, None)
        }
        (DataType::U32 | DataType::I32, [b'D', b'W', b'X' | b'Y' | b'M' | b'S' | b'T' | b'C', tail @ ..])
            if tail.len() == 4 =>
        {
            (tail, 9968, None)
        }
        _ => return Err(Usage("FATEK address must be literal uppercase X/Y/M/S/T/C +4 decimal digits for bool; R/D/F +5, RT/RC +4 or W[XYMSTC] +4 for native16; DR/DD/DF +5, DRT/DRC +4 or DW[XYMSTC] +4 for native32".into()).into()),
    };
    if !digits.iter().all(u8::is_ascii_digit) {
        return Err(Usage("FATEK address suffix must be decimal digits".into()).into());
    }
    let start = digits
        .iter()
        .fold(0_u32, |value, byte| value * 10 + u32::from(byte - b'0'));
    if matches!(
        bytes,
        [b'W', b'X' | b'Y' | b'M' | b'S', ..] | [b'D', b'W', b'X' | b'Y' | b'M' | b'S', ..]
    ) && !start.is_multiple_of(8)
    {
        return Err(
            Usage("FATEK combined X/Y/M/S views require a start divisible by 8".into()).into(),
        );
    }
    Ok((start, limit, stride))
}

fn sum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0, |value, byte| value.wrapping_add(*byte))
}
fn parse_hex(bytes: &[u8]) -> Result<u32> {
    if bytes.is_empty() || bytes.len() > 8 {
        return Err(malformed("invalid hexadecimal field width"));
    }
    bytes.iter().try_fold(0, |value, byte| {
        let digit = match byte {
            b'0'..=b'9' => *byte - b'0',
            b'A'..=b'F' => *byte - b'A' + 10,
            _ => {
                return Err(malformed(
                    "reply fields require uppercase ASCII hexadecimal",
                ));
            }
        };
        Ok((value << 4) | u32::from(digit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(text: &str) -> Vec<u8> {
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert_eq!(rest, &[] as &[u8]);
        pairs
            .iter()
            .map(|bytes| u8::try_from(parse_hex(bytes).unwrap()).unwrap())
            .collect()
    }
    fn evidence() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/fatek_fbs/primary-literals.json"
        ))
        .unwrap()
    }
    #[test]
    fn coherent_published_word_exchange_and_corrected_discrete_literal() {
        let evidence = evidence();
        for (key, address, data_type) in [
            ("published_command46", "R00012", DataType::U16),
            ("corrected_command44", "X0050", DataType::Bool),
        ] {
            let case = &evidence[key];
            let values = if matches!(data_type, DataType::Bool) {
                &case["expected_bool"]
            } else {
                &case["expected_u16"]
            };
            let plan = Plan::new(
                address,
                data_type,
                u16::try_from(values.as_array().unwrap().len()).unwrap(),
            )
            .unwrap();
            assert_eq!(plan.request(1), hex(case["request_hex"].as_str().unwrap()));
            assert_eq!(
                plan.decode(1, &hex(case["reply_hex"].as_str().unwrap()))
                    .unwrap(),
                *values
            );
        }
        let plan = Plan::new("X0050", DataType::Bool, 6).unwrap();
        plan.decode(
            1,
            &hex(
                evidence["corrected_command44"]["unchanged_bad_printed_hex_row_reply"]
                    .as_str()
                    .unwrap(),
            ),
        )
        .unwrap_err();
    }
    #[test]
    fn selected_maxima_are_265_bytes_and_native32_is_not_two_wire_items() {
        let evidence = evidence();
        for (case, address, data_type, count, expected) in [
            (
                &evidence["spec_derived_maxima"][0],
                "X0000",
                DataType::Bool,
                256,
                json!(vec![false; 256]),
            ),
            (
                &evidence["spec_derived_maxima"][1],
                "R00000",
                DataType::U16,
                64,
                json!(vec![65_535; 64]),
            ),
            (
                &evidence["spec_derived_maxima"][2],
                "DR00000",
                DataType::U32,
                32,
                json!(vec![u32::MAX; 32]),
            ),
        ] {
            let plan = Plan::new(address, data_type, count).unwrap();
            assert_eq!(
                plan.request(1),
                case["request_ascii"].as_str().unwrap().as_bytes()
            );
            let mut reply = hex(case["reply_prefix_hex"].as_str().unwrap());
            for _ in 0..256 {
                reply.extend(hex(case["reply_repeated_hex"].as_str().unwrap()));
            }
            reply.extend(hex(case["reply_suffix_hex"].as_str().unwrap()));
            assert_eq!(reply.len(), MAX_REPLY);
            assert_eq!(plan.reply_bytes(), MAX_REPLY);
            assert_eq!(plan.decode(1, &reply).unwrap(), expected);
        }
        let case = &evidence["spec_derived_u32_endpoints"];
        let bytes = hex(case["reply_hex"].as_str().unwrap());
        let plan = Plan::new("DR00012", DataType::I32, 2).unwrap();
        assert_eq!(plan.decode(1, &bytes).unwrap(), json!([-1, i32::MIN]));
        plan.decode(
            1,
            &hex(evidence["spec_derived_error"]["reply_hex"]
                .as_str()
                .unwrap()),
        )
        .unwrap_err();
    }
    #[test]
    fn literal_types_counts_and_protocol_endpoints_are_checked() {
        for (address, data_type, count) in [
            ("X9999", DataType::Bool, 1),
            ("R65535", DataType::U16, 1),
            ("RT9999", DataType::I16, 1),
            ("DR65534", DataType::U32, 1),
            ("DRC9998", DataType::I32, 1),
        ] {
            Plan::new(address, data_type, count).unwrap();
            Plan::new(address, data_type, count + 1).unwrap_err();
        }
        for (address, data_type, count) in [
            ("X0000", DataType::Bool, 0),
            ("X0000", DataType::Bool, 257),
            ("R00000", DataType::U16, 65),
            ("DR00000", DataType::U32, 33),
            ("DR65535", DataType::U32, 1),
            ("R65536", DataType::U16, 1),
            ("r00012", DataType::U16, 1),
            ("R12", DataType::U16, 1),
            ("X000A", DataType::Bool, 1),
            ("DR00012", DataType::U16, 1),
            ("R00012", DataType::Bool, 1),
            ("WX0001", DataType::U16, 1),
            ("DWX0012", DataType::U32, 1),
            ("WX9992", DataType::U16, 1),
            ("DWX9976", DataType::U32, 1),
            ("WX0000", DataType::U32, 1),
            ("DWX0000", DataType::U16, 1),
        ] {
            Plan::new(address, data_type, count).unwrap_err();
        }
    }
    #[test]
    fn combined_views_forward_native_counts_without_constructing_next_addresses() {
        let evidence: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/fatek_fbs/combined-literals.json"
        ))
        .unwrap();
        for case in evidence["cases"].as_array().unwrap() {
            let data_type = if case["native_bits"] == 16 {
                DataType::U16
            } else {
                DataType::U32
            };
            let plan = Plan::new(
                case["address"].as_str().unwrap(),
                data_type,
                u16::try_from(case["elements"].as_u64().unwrap()).unwrap(),
            )
            .unwrap();
            let unit = u8::try_from(case["unit"].as_u64().unwrap()).unwrap();
            assert_eq!(
                plan.request(unit),
                hex(case["request_hex"].as_str().unwrap())
            );
            assert_eq!(
                plan.decode(unit, &hex(case["reply_hex"].as_str().unwrap()))
                    .unwrap(),
                case["expected"]
            );
        }
        for address in ["WX9984", "WY9984", "WM9984", "WS9984", "WT0001", "WC9984"] {
            // Whole-run availability remains the PLC's decision, not a guessed
            // step between overlapping bit-indexed views.
            assert_eq!(
                Plan::new(address, DataType::U16, 64).unwrap().reply_bytes(),
                MAX_REPLY
            );
        }
        for address in [
            "DWX9968", "DWY9968", "DWM9968", "DWS9968", "DWT0001", "DWC9968",
        ] {
            assert_eq!(
                Plan::new(address, DataType::U32, 32).unwrap().reply_bytes(),
                MAX_REPLY
            );
        }
    }
}
