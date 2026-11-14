//! Explicit Logix STRING member reads, with no whole-structure layout guess.
//!
//! Rockwell 1756-PM020I pp. 12–13, 45 and 51–59 distinguishes structure
//! handles from template IDs and permits symbolic atomic-member access.
//! 1756-RM094N p. 90 describes LEN and DATA and custom string capacities.
use super::{
    Connection, MAX_BATCH, Profile, ReadShape, Signal, protocol, remaining, symbolic_path,
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, time::Instant};

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum DataType {
    #[default]
    Atomic,
    LogixString,
    LogixStructRaw,
    LogixDword,
    LogixBoolArray,
    Micro800String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Encoding {
    Ascii,
    Utf8,
}
impl Encoding {
    const fn name(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::Utf8 => "utf8",
        }
    }
    fn text(self, bytes: &[u8]) -> Result<&str> {
        if matches!(self, Self::Ascii) && !bytes.is_ascii() {
            return Err(protocol("Logix STRING active DATA is not ASCII"));
        }
        std::str::from_utf8(bytes)
            .map_err(|_| protocol("Logix STRING active DATA is not valid UTF-8"))
    }
}

pub(super) struct Plan {
    tag: String,
    elements: u16,
    capacity: u16,
    encoding: Option<Encoding>,
}
impl Plan {
    pub(super) fn new(signal: &Signal) -> Result<Self> {
        let capacity = signal
            .capacity
            .filter(|capacity| *capacity != 0)
            .ok_or_else(|| Usage("Logix STRING requires capacity from 1 to 65535".into()))?;
        // Validate the original configured address before deriving any member.
        symbolic_path(&signal.tag, Profile::Logix)?;
        // The last index has the largest encoding; check both ends without
        // allocating a request pair for every configured array element.
        for index in [0, signal.elements - 1] {
            let tag = element(&signal.tag, signal.elements, index)?;
            request(&format!("{tag}.LEN"), 1)?;
            request(&format!("{tag}.DATA[0]"), capacity)?;
        }
        Ok(Self {
            tag: signal.tag.clone(),
            elements: signal.elements,
            capacity,
            encoding: signal.encoding,
        })
    }

    pub(super) fn read(
        &self,
        connection: &mut Connection,
        slot: Option<u8>,
        context: &mut u64,
        deadline: Instant,
        budget: &mut Budget,
        signal: &str,
    ) -> Result<Value> {
        let array = self.elements > 1;
        budget.begin_string(signal, array)?;
        let mut values = Vec::with_capacity(usize::from(self.elements));
        for index in 0..self.elements {
            remaining(deadline)?;
            if index != 0 {
                budget.add(1)?;
            }
            let tag = element(&self.tag, self.elements, index)?;
            let length_request = request(&format!("{tag}.LEN"), 1)?;
            let data_request = request(&format!("{tag}.DATA[0]"), self.capacity)?;
            let length = read_length(connection, &length_request, slot, context, deadline)?;
            let length = usize::try_from(length)
                .ok()
                .filter(|length| *length <= usize::from(self.capacity))
                .ok_or_else(|| {
                    protocol("Logix STRING LEN is negative or exceeds configured capacity")
                })?;
            // Even raw-only output needs this many bytes. Stop before a large
            // DATA read when it cannot possibly fit the remaining poll budget.
            let empty = value(length, self.capacity, &[], self.encoding)?;
            let base = Budget::size(&empty)?;
            let raw_bytes = length.div_ceil(3) * 4;
            let text_bytes = self.encoding.map_or(0, |_| length);
            budget.ensure(base.saturating_add(raw_bytes).saturating_add(text_bytes))?;
            let bytes = connection.read_tag_raw(
                &data_request,
                slot,
                self.capacity,
                context,
                deadline,
                ReadShape::Exact {
                    kind: 0xc2,
                    bytes: usize::from(self.capacity),
                },
            )?;
            let after = read_length(connection, &length_request, slot, context, deadline)?;
            if i32::try_from(length)? != after {
                return Err(protocol("Logix STRING LEN changed during member reads"));
            }
            let active = bytes
                .get(2..2 + length)
                .ok_or_else(|| protocol("Logix STRING DATA is shorter than LEN"))?;
            let value = value(length, self.capacity, active, self.encoding)?;
            budget.value(&value)?;
            remaining(deadline)?;
            values.push(value);
        }
        if array {
            Ok(Value::Array(values))
        } else {
            values
                .pop()
                .ok_or_else(|| protocol("missing Logix STRING element"))
        }
    }
}

fn request(tag: &str, elements: u16) -> Result<Vec<u8>> {
    let path = symbolic_path(tag, Profile::Logix)?;
    let mut request = vec![0x4c, u8::try_from(path.len() / 2)?];
    request.extend_from_slice(&path);
    request.extend_from_slice(&elements.to_le_bytes());
    Ok(request)
}

pub(super) fn element(tag: &str, elements: u16, index: u16) -> Result<String> {
    if elements == 1 {
        return Ok(tag.to_owned());
    }
    let (prefix, last) = tag
        .strip_suffix(']')
        .and_then(|tag| tag.rsplit_once(['[', ',']))
        .ok_or_else(|| {
            Usage("STRING arrays require an explicit final element index, e.g. Labels[0]".into())
        })?;
    let first = last.parse::<u32>().map_err(|_| super::bad_path())?;
    let last_index = first
        .checked_add(u32::from(elements) - 1)
        .ok_or_else(|| Usage("STRING array index range exceeds u32".into()))?;
    let current = last_index - u32::from(elements - 1 - index);
    // The delimiter is retained from the already-validated symbolic address.
    let delimiter = tag
        .as_bytes()
        .get(prefix.len())
        .copied()
        .ok_or_else(super::bad_path)?;
    Ok(format!("{prefix}{}{current}]", char::from(delimiter)))
}

fn read_length(
    connection: &mut Connection,
    request: &[u8],
    slot: Option<u8>,
    context: &mut u64,
    deadline: Instant,
) -> Result<i32> {
    let bytes = connection.read_tag_raw(
        request,
        slot,
        1,
        context,
        deadline,
        ReadShape::Exact {
            kind: 0xc4,
            bytes: 4,
        },
    )?;
    let length = bytes
        .get(2..)
        .and_then(|bytes| bytes.first_chunk::<4>())
        .ok_or_else(|| protocol("missing Logix STRING DINT LEN"))?;
    Ok(i32::from_le_bytes(*length))
}

fn value(length: usize, capacity: u16, active: &[u8], encoding: Option<Encoding>) -> Result<Value> {
    let mut value = json!({
        "kind":"logix_string", "length":length, "configured_capacity":capacity,
        "raw":{"encoding":"base64", "scope":"active_data", "data":base64::engine::general_purpose::STANDARD.encode(active)}
    });
    if let Some(encoding) = encoding {
        value["encoding"] = json!(encoding.name());
        value["text"] = json!(encoding.text(active)?);
    }
    Ok(value)
}

/// Count the complete serialized Reading array incrementally. A string's
/// envelope is charged before querying its first element; each retained value
/// is charged before another element or sibling is queried.
pub(super) struct Budget(usize);
impl Budget {
    pub(super) const fn new() -> Self {
        Self(2)
    }
    fn error() -> anyhow::Error {
        Categorized::error(
            ErrorKind::Incomplete,
            "EtherNet/IP source batch exceeds 1 MiB; configure fewer signals or elements",
        )
    }
    pub(super) fn ensure(&self, extra: usize) -> Result<()> {
        if self.0.saturating_add(extra) > MAX_BATCH {
            return Err(Self::error());
        }
        Ok(())
    }
    pub(super) fn add(&mut self, bytes: usize) -> Result<()> {
        self.ensure(bytes)?;
        self.0 += bytes;
        Ok(())
    }
    pub(super) fn separator(&mut self, index: usize) -> Result<()> {
        self.add(usize::from(index != 0))
    }
    pub(super) fn begin_string(&mut self, signal: &str, array: bool) -> Result<()> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            signal: &'a str,
            value: (),
        }
        // A unit value serializes as null; replace those four bytes by the
        // separately counted value, including its optional array brackets.
        self.add(Self::size(&Envelope { signal, value: () })? - 4 + usize::from(array) * 2)
    }
    pub(super) fn size(value: &impl Serialize) -> Result<usize> {
        let mut count = Self(0);
        serde_json::to_writer(&mut count, value).map_err(|_| Self::error())?;
        Ok(count.0)
    }
    pub(super) fn value(&mut self, value: &impl Serialize) -> Result<()> {
        serde_json::to_writer(self, value).map_err(|_| Self::error())
    }
}
impl io::Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.saturating_add(bytes.len()) > MAX_BATCH {
            return Err(io::Error::other("EtherNet/IP batch exceeds size limit"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_indices_keep_member_and_dimension_boundaries() {
        for (tag, expected) in [
            ("Labels[255]", "Labels[257]"),
            ("Program:Main.Rows[2,65535]", "Program:Main.Rows[2,65537]"),
            ("Pack[3].Labels[65535]", "Pack[3].Labels[65537]"),
        ] {
            symbolic_path(tag, Profile::Logix).unwrap();
            assert_eq!(element(tag, 3, 2).unwrap(), expected);
            symbolic_path(expected, Profile::Logix).unwrap();
        }
        assert!(
            element("Labels", 2, 0)
                .unwrap_err()
                .to_string()
                .contains("index")
        );
        assert!(
            element("Labels[4294967295]", 2, 0)
                .unwrap_err()
                .to_string()
                .contains("u32")
        );
    }

    #[test]
    fn incremental_budget_matches_the_actual_reading_array_shape() {
        let value = value(3, 82, b"A\0B", Some(Encoding::Ascii)).unwrap();
        let mut budget = Budget::new();
        budget.begin_string("quoted\"name", true).unwrap();
        budget.value(&value).unwrap();
        budget.add(1).unwrap();
        budget.value(&value).unwrap();
        let readings = vec![crate::timeseries::Reading {
            signal: "quoted\"name".into(),
            value: json!([value.clone(), value]),
            source_time: None,
        }];
        assert_eq!(budget.0, serde_json::to_vec(&readings).unwrap().len());
    }
}
