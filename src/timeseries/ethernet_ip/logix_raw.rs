//! Explicit opaque Logix structure transfers; no template or layout inference.
//!
//! Rockwell 1756-PM020I pp.12–13 defines the four-byte type descriptor;
//! pp.18–21 defines data-byte fragment offsets and status 06 continuation.
use super::{
    Connection, MAX_BATCH, Profile, Signal, cip_reply, cpf, fragment_request, logix_string,
    protocol, remaining, symbolic_path,
};
use crate::types::Usage;
use anyhow::Result;
use base64::Engine as _;
use serde_json::{Value, json};
use std::time::Instant;

pub(super) struct Plan {
    request: Vec<u8>,
    elements: u16,
    max_bytes: usize,
}
impl Plan {
    pub(super) fn new(signal: &Signal) -> Result<Self> {
        if signal.capacity.is_some() || signal.encoding.is_some() {
            return Err(
                Usage("capacity and encoding do not apply to logix_struct_raw".into()).into(),
            );
        }
        let max_bytes = signal
            .max_bytes
            .and_then(|size| usize::try_from(size).ok())
            .filter(|size| (1..=MAX_BATCH).contains(size))
            .ok_or_else(|| Usage("logix_struct_raw requires max_bytes from 1 to 1048576".into()))?;
        let path = symbolic_path(&signal.tag, Profile::Logix)?;
        let mut request = vec![0x4c, u8::try_from(path.len() / 2)?];
        request.extend_from_slice(&path);
        request.extend_from_slice(&signal.elements.to_le_bytes());
        Ok(Self {
            request,
            elements: signal.elements,
            max_bytes,
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
        // Arrays remain one opaque value: no per-element layout is known.
        budget.begin_string(signal, false)?;
        budget.ensure(self.size([0xa0, 2, 0, 0], 0)?)?;
        let mut request = self.request.clone();
        let mut descriptor = None;
        let mut bytes = Vec::new();
        loop {
            remaining(deadline)?;
            let service = request
                .first()
                .copied()
                .ok_or_else(|| protocol("empty raw read request"))?;
            let response = connection.exchange_ucmm(&request, slot, context, deadline)?;
            let reply = cip_reply(cpf(&response)?, service | 0x80, slot.is_some())?;
            let Some((kind, data)) = reply.data.split_first_chunk::<4>() else {
                return Err(protocol(
                    "Logix RAW reply is missing its complete structure descriptor",
                ));
            };
            if kind[..2] != [0xa0, 2] {
                return Err(protocol(
                    "Logix RAW requires the exact A0 02 structure descriptor",
                ));
            }
            if descriptor.is_some_and(|previous| previous != *kind) {
                return Err(protocol(
                    "Logix RAW structure descriptor changed between fragments",
                ));
            }
            descriptor = Some(*kind);
            if data.is_empty() {
                return Err(protocol(
                    "Logix RAW structure fragment contains no value bytes",
                ));
            }
            let next = bytes
                .len()
                .checked_add(data.len())
                .ok_or_else(|| protocol("Logix RAW byte count overflow"))?;
            if next > self.max_bytes {
                return Err(protocol("Logix RAW transfer exceeds configured max_bytes"));
            }
            if reply.partial && next == self.max_bytes {
                return Err(protocol(
                    "Logix RAW partial transfer reached configured max_bytes",
                ));
            }
            // No large base64 temporary is needed to count exact output cost.
            // Check before assembly growth and before any continuation query.
            budget.ensure(self.size(*kind, next)?)?;
            if next > bytes.capacity() {
                // Amortized growth stays within the explicit raw byte ceiling.
                let capacity = next
                    .max(bytes.capacity().saturating_mul(2))
                    .min(self.max_bytes);
                bytes.try_reserve_exact(capacity - bytes.len())?;
            }
            bytes.extend_from_slice(data);
            remaining(deadline)?;
            if !reply.partial {
                let value = self.value(
                    *kind,
                    bytes.len(),
                    &base64::engine::general_purpose::STANDARD.encode(&bytes),
                );
                budget.value(&value)?;
                remaining(deadline)?;
                return Ok(value);
            }
            fragment_request(&mut request, &self.request, bytes.len())?;
        }
    }
    fn size(&self, descriptor: [u8; 4], bytes: usize) -> Result<usize> {
        let fixed = logix_string::Budget::size(&self.value(descriptor, bytes, ""))?;
        Ok(fixed + bytes.div_ceil(3) * 4)
    }
    fn value(&self, descriptor: [u8; 4], bytes: usize, data: &str) -> Value {
        json!({
            "kind":"logix_struct_raw", "layout":"opaque", "elements_requested":self.elements,
            "tag_type_hex":crate::types::hex(descriptor),
            "structure_handle":u16::from_le_bytes([descriptor[2],descriptor[3]]),
            "bytes":bytes, "configured_max_bytes":self.max_bytes,
            "raw":{"encoding":"base64","scope":"structure_data","data":data}
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_budget_includes_exact_base64_and_metadata_at_every_boundary() {
        let plan = Plan {
            request: vec![],
            elements: 256,
            max_bytes: MAX_BATCH,
        };
        for length in [0, 1, 2, 3, 4, 99, 100, 8191, 65535, 786_432] {
            let bytes = vec![0xff; length];
            let value = plan.value(
                [0xa0, 2, 0xff, 0xff],
                length,
                &base64::engine::general_purpose::STANDARD.encode(&bytes),
            );
            assert_eq!(
                plan.size([0xa0, 2, 0xff, 0xff], length).unwrap(),
                serde_json::to_vec(&value).unwrap().len()
            );
        }
    }
}
