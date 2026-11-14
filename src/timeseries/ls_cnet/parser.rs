//! Bounded XGB Cnet rSS framing over explicit addresses; no address arithmetic.
#![forbid(unsafe_code)]
#![deny(clippy::panic_in_result_fn)]

use super::malformed;
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DataType {
    Bool,
    U8,
    I8,
    #[default]
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
}

impl DataType {
    const fn bytes(self) -> usize {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 => 4,
            Self::U64 | Self::I64 => 8,
        }
    }
    fn value(self, value: u64) -> Result<Value> {
        Ok(match self {
            Self::Bool => match value {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err(malformed("X read requires exactly 00 or 01")),
            },
            Self::U8 | Self::U16 | Self::U32 | Self::U64 => json!(value),
            Self::I8 => json!(i8::from_ne_bytes(u8::try_from(value)?.to_ne_bytes())),
            Self::I16 => json!(i16::from_ne_bytes(u16::try_from(value)?.to_ne_bytes())),
            Self::I32 => json!(i32::from_ne_bytes(u32::try_from(value)?.to_ne_bytes())),
            Self::I64 => json!(i64::from_ne_bytes(value.to_ne_bytes())),
        })
    }
}

#[derive(Debug)]
pub(super) struct Plan {
    variables: Vec<String>,
    data_type: DataType,
    array: bool,
}

fn native_type(variable: &str) -> Result<u8> {
    let bytes = variable.as_bytes();
    let (area, kind, tail) = match bytes {
        [b'%', area, kind, tail @ ..] if (4..=16).contains(&bytes.len()) => {
            (area.to_ascii_uppercase(), kind.to_ascii_uppercase(), tail)
        }
        _ => {
            return Err(Usage(
                "LS Cnet variable requires a 4–16 byte literal %area/type/address".into(),
            )
            .into());
        }
    };
    // Literal spelling survives unchanged. No radix/bit/module/slot aliases or
    // arithmetic is used; the configured CPU/project validates actual ranges.
    if !b"PMLKFTCDRIQW".contains(&area)
        || !matches!(kind, b'X' | b'B' | b'W' | b'D' | b'L')
        || !tail
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() || *byte == b'.')
        || tail.split(|byte| *byte == b'.').any(<[u8]>::is_empty)
        || (tail.contains(&b'.') && !b"IQ".contains(&area))
        || (area == b'D' && !matches!(kind, b'B' | b'W'))
    {
        return Err(Usage("LS Cnet variable needs a documented read area/type and literal numeric components (dots only for I/Q); D supports B/W".into()).into());
    }
    Ok(kind)
}

impl Plan {
    pub(super) fn new(variables: &[String], data_type: DataType, array: bool) -> Result<Self> {
        if !(1..=16).contains(&variables.len()) || (!array && variables.len() != 1) {
            return Err(Usage(
                "LS Cnet requires 1–16 explicit variables, or one scalar variable".into(),
            )
            .into());
        }
        let expected = match data_type {
            DataType::Bool => b'X',
            DataType::U8 | DataType::I8 => b'B',
            DataType::U16 | DataType::I16 => b'W',
            DataType::U32 | DataType::I32 => b'D',
            DataType::U64 | DataType::I64 => b'L',
        };
        for variable in variables {
            if native_type(variable)? != expected {
                return Err(Usage(
                    "LS Cnet rSS variables must share the native type matching data_type".into(),
                )
                .into());
            }
        }
        let request_bytes = 11 + variables.iter().map(|name| 2 + name.len()).sum::<usize>();
        let response_bytes = 11 + variables.len() * (2 + 2 * data_type.bytes());
        if request_bytes > 256 || response_bytes > 256 {
            return Err(Usage(
                "LS Cnet explicit rSS request and reply must each fit 256 bytes".into(),
            )
            .into());
        }
        Ok(Self {
            variables: variables.to_vec(),
            data_type,
            array,
        })
    }
    pub(super) fn request(&self, station: u8) -> Vec<u8> {
        let mut bytes = format!("\x05{station:02X}rSS{:02X}", self.variables.len()).into_bytes();
        for variable in &self.variables {
            bytes.extend(format!("{:02X}{variable}", variable.len()).bytes());
        }
        bytes.push(4);
        bytes.extend(format!("{:02X}", sum(&bytes)).bytes());
        bytes
    }
    pub(super) fn reply_bytes(&self) -> usize {
        11 + self.variables.len() * (2 + 2 * self.data_type.bytes())
    }

    pub(super) fn decode(&self, station: u8, frame: &[u8]) -> Result<Value> {
        if frame.len() > 256 || frame.len() < 13 {
            return Err(malformed("invalid reply extent"));
        }
        let (body, bcc) = frame.split_at(frame.len() - 2);
        if parse_hex(bcc)? != u64::from(sum(body)) {
            return Err(malformed("BCC mismatch"));
        }
        if body.last() != Some(&3) {
            return Err(malformed("reply requires ETX"));
        }
        if parse_hex(
            frame
                .get(1..3)
                .ok_or_else(|| malformed("missing station"))?,
        )? != u64::from(station)
            || frame.get(3..6) != Some(b"rSS".as_slice())
        {
            return Err(malformed("reply station or command differs from request"));
        }
        match frame.first() {
            Some(0x15) => {
                if frame.len() != 13 {
                    return Err(malformed("NAK requires exactly four error digits"));
                }
                let error = parse_hex(
                    frame
                        .get(6..10)
                        .ok_or_else(|| malformed("missing NAK code"))?,
                )?;
                return Err(malformed(&format!("device NAK {error:04X}")));
            }
            Some(6) => {}
            _ => return Err(malformed("reply requires ACK or NAK")),
        }
        if frame.len() != self.reply_bytes()
            || parse_hex(
                frame
                    .get(6..8)
                    .ok_or_else(|| malformed("missing block count"))?,
            )? != u64::try_from(self.variables.len())?
        {
            return Err(malformed(
                "reply block count or extent differs from request",
            ));
        }
        let payload = frame
            .get(8..frame.len() - 3)
            .ok_or_else(|| malformed("invalid block extent"))?;
        let width = self.data_type.bytes();
        let mut values = Vec::with_capacity(self.variables.len());
        for block in payload.chunks_exact(2 + 2 * width) {
            let (count, data) = block.split_at(2);
            if parse_hex(count)? != u64::try_from(width)? {
                return Err(malformed("reply native byte count differs from request"));
            }
            values.push(self.data_type.value(parse_hex(data)?)?);
        }
        if self.array {
            Ok(Value::Array(values))
        } else {
            values.pop().ok_or_else(|| malformed("missing scalar"))
        }
    }
}

fn sum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0, |total, byte| total.wrapping_add(*byte))
}
fn parse_hex(bytes: &[u8]) -> Result<u64> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err(malformed("invalid hexadecimal field width"));
    }
    bytes.iter().try_fold(0, |value, byte| {
        let digit = match byte {
            b'0'..=b'9' => *byte - b'0',
            b'A'..=b'F' => *byte - b'A' + 10,
            b'a'..=b'f' => *byte - b'a' + 10,
            _ => return Err(malformed("non-hexadecimal reply field")),
        };
        Ok((value << 4) | u64::from(digit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(text: &str) -> Vec<u8> {
        let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
        assert_eq!(remainder, &[] as &[u8]);
        pairs
            .iter()
            .map(|bytes| u8::try_from(parse_hex(bytes).unwrap()).unwrap())
            .collect()
    }
    #[test]
    fn actual_vendor_checksum_anchor_and_spec_derived_multi_word_reply() {
        let evidence: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ls_cnet/primary-literals.json"
        ))
        .unwrap();
        let plan = Plan::new(&["%MW100".into()], DataType::U16, false).unwrap();
        assert_eq!(
            plan.request(0x20),
            hex(evidence["vendor_request_7_6"]["hex"].as_str().unwrap())
        );
        let plan = Plan::new(&["%MW020".into(), "%PW001".into()], DataType::U16, true).unwrap();
        assert_eq!(
            plan.request(1),
            hex(evidence["reconstructed_7_8_request"]["hex"]
                .as_str()
                .unwrap())
        );
        assert_eq!(
            plan.decode(
                1,
                &hex(evidence["reconstructed_7_8_ack"]["hex"].as_str().unwrap())
            )
            .unwrap(),
            json!([4660, 22136])
        );
        plan.decode(
            1,
            &hex(evidence["reconstructed_nak"]["hex"].as_str().unwrap()),
        )
        .unwrap_err();
    }
    #[test]
    fn explicit_blocks_have_both_wire_caps_and_same_native_type() {
        let sixteen = vec!["%MW0".into(); 16];
        let plan = Plan::new(&sixteen, DataType::U16, true).unwrap();
        assert_eq!(plan.reply_bytes(), 107);
        Plan::new(&vec!["%MW0".into(); 17], DataType::U16, true).unwrap_err();
        Plan::new(&["%MW0".into(), "%MB0".into()], DataType::U16, true).unwrap_err();
        Plan::new(&vec!["%ML0".into(); 14], DataType::U64, true).unwrap_err();
        assert_eq!(
            Plan::new(&vec!["%ML0".into(); 13], DataType::U64, true)
                .unwrap()
                .reply_bytes(),
            245
        );
        let long = "%MW0000000000000".to_owned();
        assert_eq!(long.len(), 16);
        Plan::new(&vec![long.clone(); 14], DataType::U16, true).unwrap_err();
        assert_eq!(
            Plan::new(&vec![long; 13], DataType::U16, true)
                .unwrap()
                .request(255)
                .len(),
            245
        );
        Plan::new(&["%DW0".into()], DataType::U16, false).unwrap();
        Plan::new(&["%DB0".into()], DataType::U8, false).unwrap();
        Plan::new(&["%DD0".into()], DataType::U32, false).unwrap_err();
        for variable in ["%NW0", "%UW00.00", "%ZW0", "%MW0.0", "%IW0..0"] {
            Plan::new(&[variable.into()], DataType::U16, false).unwrap_err();
        }
    }
}
