//! Explicit Logix DWORD and DWORD-backed BOOL-array reads.
//!
//! 1756-PM020I pp.12/42 defines D3 as a four-byte little-endian DWORD;
//! p.58 describes BOOL arrays as DWORD arrays. The libplctag maintainer's
//! Bits-and-Booleans reference qualifies array indices as DWORD indices.
//! No tag discovery, implicit reinterpretation or write service is used.
use super::{
    Connection, Profile, Signal, cip_reply, cpf, fragment_request, logix_string, protocol,
    remaining, symbolic_path,
};
use crate::types::Usage;
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::time::Instant;

pub(super) struct Plan {
    request: Vec<u8>,
    elements: u16,
    bits: bool,
}
impl Plan {
    pub(super) fn new(signal: &Signal) -> Result<Self> {
        if !(1..=256).contains(&signal.elements) {
            return Err(Usage("explicit Logix DWORD count must be 1–256".into()).into());
        }
        if signal.capacity.is_some() || signal.encoding.is_some() || signal.max_bytes.is_some() {
            return Err(Usage(
                "capacity, encoding and max_bytes do not apply to explicit Logix DWORD reads"
                    .into(),
            )
            .into());
        }
        let path = symbolic_path(&signal.tag, Profile::Logix)?;
        let mut request = vec![0x4c, u8::try_from(path.len() / 2)?];
        request.extend_from_slice(&path);
        request.extend_from_slice(&signal.elements.to_le_bytes());
        Ok(Self {
            request,
            elements: signal.elements,
            bits: signal.data_type == logix_string::DataType::LogixBoolArray,
        })
    }

    pub(super) fn read(
        &self,
        connection: &mut Connection,
        slot: Option<u8>,
        context: &mut u64,
        deadline: Instant,
        budget: &mut logix_string::Budget,
        signal: &str,
    ) -> Result<Value> {
        // Charge the Reading envelope and a sound lower bound before I/O.
        budget.begin_string(signal, false)?;
        budget.ensure(self.minimum_size(&[])?)?;
        let expected = usize::from(self.elements) * 4;
        let mut request = self.request.clone();
        let mut bytes = Vec::with_capacity(expected);
        loop {
            remaining(deadline)?;
            let service = request
                .first()
                .copied()
                .ok_or_else(|| protocol("empty DWORD read"))?;
            let response = connection.exchange_ucmm(&request, slot, context, deadline)?;
            let reply = cip_reply(cpf(&response)?, service | 0x80, slot.is_some())?;
            let Some((descriptor, data)) = reply.data.split_first_chunk::<2>() else {
                return Err(protocol(
                    "Logix DWORD reply is missing its complete type descriptor",
                ));
            };
            if *descriptor != [0xd3, 0] {
                return Err(protocol(
                    "explicit Logix DWORD reads require exact type descriptor D3 00 on every fragment",
                ));
            }
            if data.is_empty() {
                return Err(protocol("Logix DWORD fragment contains no value bytes"));
            }
            let next = bytes
                .len()
                .checked_add(data.len())
                .ok_or_else(|| protocol("DWORD byte count overflow"))?;
            if next > expected {
                return Err(protocol(
                    "Logix DWORD fragment exceeds requested DWORD count",
                ));
            }
            if reply.partial && next == expected {
                return Err(protocol(
                    "partial Logix DWORD transfer reached requested DWORD count",
                ));
            }
            // The configured count is already validated as 1..=256; wire
            // assembly is at most 1024 bytes, independently of JSON expansion.
            bytes.extend_from_slice(data);
            // Known low bytes only increase a word's minimum decimal size;
            // known zero bits only increase minimum boolean JSON size. At EOM
            // this is exact, so large output arrays are admitted before allocation.
            budget.ensure(self.minimum_size(&bytes)?)?;
            remaining(deadline)?;
            if !reply.partial {
                if bytes.len() != expected {
                    return Err(protocol(
                        "Logix DWORD value size differs from requested DWORD count",
                    ));
                }
                let value = self.value(&bytes)?;
                budget.value(&value)?;
                remaining(deadline)?;
                return Ok(value);
            }
            fragment_request(&mut request, &self.request, bytes.len())?;
        }
    }

    fn minimum_size(&self, bytes: &[u8]) -> Result<usize> {
        let count = usize::from(self.elements);
        let mut digits = 0;
        for index in 0..count {
            let mut word = 0_u32;
            for byte in 0..4 {
                if let Some(value) = bytes.get(index * 4 + byte) {
                    word |= u32::from(*value) << (byte * 8);
                }
            }
            digits += usize::try_from(word.checked_ilog10().unwrap_or(0))? + 1;
        }
        if !self.bits {
            return Ok(digits + if count == 1 { 0 } else { count + 1 });
        }
        let fixed = logix_string::Budget::size(&self.packed("", &[], &[]))?;
        let bits = count * 32;
        let known_zero_bits = bytes
            .iter()
            .try_fold(0_usize, |count, byte| -> Result<usize> {
                Ok(count + usize::try_from(byte.count_zeros())?)
            })?;
        Ok(
            fixed + (count * 4).div_ceil(3) * 4 + digits + count - 1 + bits * 5 - 1
                + known_zero_bits,
        )
    }

    fn value(&self, bytes: &[u8]) -> Result<Value> {
        let (chunks, remainder) = bytes.as_chunks::<4>();
        if !remainder.is_empty() || chunks.len() != usize::from(self.elements) {
            return Err(protocol("incomplete Logix DWORD value"));
        }
        let words: Vec<u32> = chunks
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect();
        if !self.bits {
            return if self.elements == 1 {
                words
                    .first()
                    .copied()
                    .map(|word| json!(word))
                    .ok_or_else(|| protocol("missing DWORD"))
            } else {
                Ok(json!(words))
            };
        }
        let bits: Vec<bool> = words
            .iter()
            .flat_map(|word| (0..32).map(move |bit| word & (1_u32 << bit) != 0))
            .collect();
        Ok(self.packed(&STANDARD.encode(bytes), &words, &bits))
    }

    fn packed(&self, raw: &str, words: &[u32], bits: &[bool]) -> Value {
        json!({
            "kind":"logix_bool_array", "type_descriptor_raw":[0xd3,0],
            "dword_count":self.elements, "bit_count":usize::from(self.elements)*32,
            "bit_order":"lsb0", "bit_index_base":"requested_range",
            "words":words, "bits":bits,
            "raw":{"encoding":"base64","bytes":usize::from(self.elements)*4,"data":raw}
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(elements: u16, bits: bool) -> Plan {
        Plan {
            request: Vec::new(),
            elements,
            bits,
        }
    }

    #[test]
    fn symbolic_index_and_request_count_are_dword_units() {
        let signal: Signal = serde_json::from_value(json!({
            "name":"bits", "tag":"Flags[3]", "elements":2, "data_type":"logix_bool_array"
        }))
        .unwrap();
        let plan = Plan::new(&signal).unwrap();
        assert_eq!(
            plan.request,
            [
                0x4c, 5, 0x91, 5, b'F', b'l', b'a', b'g', b's', 0, 0x28, 3, 2, 0
            ]
        );
    }

    #[test]
    fn literal_bit_positions_are_lsb_first_per_exact_little_endian_dword() {
        // Independent words: 0x80000081 and 0x01000003, with six set bits.
        let value = plan(2, true)
            .value(&[0x81, 0, 0, 0x80, 3, 0, 0, 1])
            .unwrap();
        assert_eq!(value["type_descriptor_raw"], json!([0xd3, 0]));
        assert_eq!(value["words"], json!([2_147_483_777_u32, 16_777_219_u32]));
        let expected: Vec<bool> = (0..64)
            .map(|index| [0, 7, 31, 32, 33, 56].contains(&index))
            .collect();
        assert_eq!(value["bits"], json!(expected));
        assert_eq!(value["raw"]["data"], "gQAAgAMAAAE=");
        assert_eq!(plan(1, false).value(&[0xff; 4]).unwrap(), json!(u32::MAX));
    }

    #[test]
    fn json_admission_is_exact_at_eom_and_monotonic_before_large_arrays() {
        for bits in [false, true] {
            for count in [1, 2, 256] {
                let plan = plan(count, bits);
                let bytes: Vec<u8> = (0..usize::from(count) * 4)
                    .map(|i| u8::try_from(i % 256).unwrap())
                    .collect();
                let full = logix_string::Budget::size(&plan.value(&bytes).unwrap()).unwrap();
                let mut previous = 0;
                for end in 0..=bytes.len() {
                    let minimum = plan.minimum_size(&bytes[..end]).unwrap();
                    assert!(minimum >= previous && minimum <= full);
                    previous = minimum;
                }
                assert_eq!(previous, full);
                assert!(full < 128 * 1024);
            }
        }
    }
}
