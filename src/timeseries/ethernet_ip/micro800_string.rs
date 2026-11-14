//! Counted `SHORT_STRING` values on a direct `Micro800` Class 3 connection.
//!
//! Rockwell 2080-RM001M p.174 maps STRING to DA. 2080-QS002B p.29
//! describes the byte-count-plus-characters format; 2080-QS003A p.59
//! documents indexed arrays. `FactoryTalk` Design Workbench's STRING data
//! type documentation bounds the declared character capacity to 252.
use super::{
    Connection, Profile, ReadShape, Signal, class3, logix_string, protocol, remaining,
    symbolic_path,
};
use crate::types::Usage;
use anyhow::Result;
use base64::Engine as _;
use serde_json::{Value, json};
use std::time::Instant;

pub(super) struct Plan {
    tag: String,
    elements: u16,
    capacity: u16,
    encoding: Option<logix_string::Encoding>,
}
impl Plan {
    pub(super) fn new(signal: &Signal) -> Result<Self> {
        let capacity = signal
            .capacity
            .filter(|capacity| (1..=252).contains(capacity))
            .ok_or_else(|| Usage("Micro800 STRING requires capacity from 1 to 252".into()))?;
        symbolic_path(&signal.tag, Profile::Micro800)?;
        for index in [0, signal.elements - 1] {
            request(&logix_string::element(&signal.tag, signal.elements, index)?)?;
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
        context: &mut u64,
        deadline: Instant,
        budget: &mut logix_string::Budget,
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
            // Bound the smallest possible result before issuing the query.
            budget.ensure(logix_string::Budget::size(&self.value(&[])?)?)?;
            let tag = logix_string::element(&self.tag, self.elements, index)?;
            let bytes = connection.read_tag_raw(
                &request(&tag)?,
                None,
                1,
                context,
                deadline,
                ReadShape::ShortString {
                    capacity: usize::from(self.capacity),
                },
            )?;
            let [0xda, 0, length, data @ ..] = bytes.as_slice() else {
                return Err(protocol("Micro800 STRING is missing its byte count"));
            };
            if data.len() != usize::from(*length) {
                return Err(protocol(
                    "Micro800 STRING length differs from its byte count",
                ));
            }
            let value = self.value(data)?;
            budget.value(&value)?;
            remaining(deadline)?;
            values.push(value);
        }
        if array {
            Ok(Value::Array(values))
        } else {
            values
                .pop()
                .ok_or_else(|| protocol("missing Micro800 STRING element"))
        }
    }
    fn value(&self, data: &[u8]) -> Result<Value> {
        let mut value = json!({"kind":"micro800_string","length":data.len(),"configured_capacity":self.capacity,
            "raw":{"encoding":"base64","scope":"active_data","data":base64::engine::general_purpose::STANDARD.encode(data)}});
        if let Some(encoding) = self.encoding {
            let name = match encoding {
                logix_string::Encoding::Ascii => "ascii",
                logix_string::Encoding::Utf8 => "utf8",
            };
            if matches!(encoding, logix_string::Encoding::Ascii) && !data.is_ascii() {
                return Err(protocol("Micro800 STRING active bytes are not ASCII"));
            }
            let text = std::str::from_utf8(data)
                .map_err(|_| protocol("Micro800 STRING active bytes are not valid UTF-8"))?;
            value["encoding"] = json!(name);
            value["text"] = json!(text);
        }
        Ok(value)
    }
}
fn request(tag: &str) -> Result<Vec<u8>> {
    let path = symbolic_path(tag, Profile::Micro800)?;
    let mut request = vec![0x4c, u8::try_from(path.len() / 2)?];
    request.extend(path);
    request.extend([1, 0]);
    if request.len() + 6 > class3::MAX_PAYLOAD {
        return Err(Usage("Micro800 connected STRING path exceeds 504 bytes".into()).into());
    }
    Ok(request)
}
