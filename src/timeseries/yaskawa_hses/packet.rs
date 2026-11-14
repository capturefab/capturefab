//! HW1483358 rev12 finite single-response robot-control reads.
use super::{Resource, incomplete};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy)]
pub(super) struct Request {
    pub id: u8,
    pub command: u16,
    pub instance: u16,
    pub attribute: u8,
    pub service: u8,
    pub bytes: usize,
}
impl Request {
    pub(super) fn encode(self) -> [u8; 32] {
        let mut data = [0; 32];
        data[..4].copy_from_slice(b"YERC");
        data[4..6].copy_from_slice(&32_u16.to_le_bytes());
        data[8..12].copy_from_slice(&[3, 1, 0, self.id]);
        data[16..24].copy_from_slice(b"99999999");
        data[24..26].copy_from_slice(&self.command.to_le_bytes());
        data[26..28].copy_from_slice(&self.instance.to_le_bytes());
        data[28] = self.attribute;
        data[29] = self.service;
        data
    }
}
#[derive(Debug, Clone, Copy)]
pub(super) struct Reply<'a> {
    pub body: &'a [u8],
    pub service: u8,
    pub status: u8,
    pub added_words: u8,
    pub added: u32,
    pub added_bytes: [u8; 4],
}
pub(super) fn reply(request: Request, data: &[u8]) -> Result<Reply<'_>> {
    let header = data
        .first_chunk::<32>()
        .ok_or_else(|| incomplete("truncated single-response header"))?;
    if data.len() > 511
        || &header[..4] != b"YERC"
        || header[4..6] != 32_u16.to_le_bytes()
        || usize::from(u16::from_le_bytes([header[6], header[7]])) != data.len() - 32
        || header[8..12] != [3, 1, 1, request.id]
        || header[12..16] != 0x8000_0000_u32.to_le_bytes()
        || &header[16..24] != b"99999999"
        || header[24] != request.service + 0x80
        || header[26] > 2
    {
        return Err(incomplete(
            "reply extent, identity, division, final block, ID or service is invalid",
        ));
    }
    let added_bytes: [u8; 4] = header[28..32].try_into()?;
    let added = match header[26] {
        0 => 0,
        1 => u32::from(u16::from_le_bytes([added_bytes[0], added_bytes[1]])),
        _ => u32::from_le_bytes(added_bytes),
    };
    if header[25] != 0 || added != 0 {
        return Err(Categorized::new(ErrorKind::OperationFailed,"HSES controller rejected its read request")
            .details(json!({"hses_status":header[25],"hses_added_status_words":header[26],"hses_added_status_raw":added,"hses_added_status_bytes_hex":crate::types::hex(added_bytes),"hses_raw_response_hex":crate::types::hex(data)})).into());
    }
    let body = &data[32..];
    if body.len() != request.bytes {
        return Err(incomplete(
            "successful reply body width differs from the selected read",
        ));
    }
    Ok(Reply {
        body,
        service: header[24],
        status: header[25],
        added_words: header[26],
        added,
        added_bytes,
    })
}
pub(super) fn value(resource: Resource, index: u16, data: &[u8]) -> Result<Value> {
    if data.len() != resource.bytes() {
        return Err(incomplete("native payload width is invalid"));
    }
    let bytes4 = |data: &[u8]| -> Result<[u8; 4]> {
        data.try_into().map_err(|_| incomplete("native word width"))
    };
    match resource {
        Resource::Status => {
            let words = data.as_chunks::<4>().0;
            let [first, second] = words else {
                return Err(incomplete("status requires exactly two words"));
            };
            let first = u32::from_le_bytes(*first);
            let second = u32::from_le_bytes(*second);
            Ok(json!({"words_raw":[first,second],"flags":{
                "step":first&1!=0,"one_cycle":first&2!=0,"auto":first&4!=0,"running":first&8!=0,
                "teach":first&0x20!=0,"play":first&0x40!=0,"command_remote":first&0x80!=0,
                "pendant_hold":second&2!=0,"command_hold":second&8!=0,"alarm":second&0x10!=0,"error":second&0x20!=0,"servo_on":second&0x40!=0},
                "word1_bit4_raw":first&0x10!=0,"word2_bit2_raw":second&4!=0,"unqualified_bits":[first&!0xef,second&!0x7a],"conflicting_semantics":"unqualified_manual_version"}))
        }
        Resource::BasePosition => {
            let words = data.as_chunks::<4>().0;
            let [
                kind,
                figure,
                tool,
                user,
                extended,
                x,
                y,
                z,
                rx,
                ry,
                rz,
                seventh,
                eighth,
            ] = words
            else {
                return Err(incomplete("position metadata words missing"));
            };
            let coordinate_type = u32::from_le_bytes(*kind);
            let coordinates =
                [*x, *y, *z, *rx, *ry, *rz, *seventh, *eighth].map(i32::from_le_bytes);
            let mut value = json!({"coordinate_type_raw":coordinate_type,"figure_raw":u32::from_le_bytes(*figure),"tool_number_raw":u32::from_le_bytes(*tool),"user_coordinate_raw":u32::from_le_bytes(*user),"extended_figure_raw":u32::from_le_bytes(*extended),"coordinates_raw":coordinates,"units_status":if coordinate_type==16 {"documented_base_units"} else {"unqualified_returned_type"}});
            if coordinate_type == 16 {
                value["position_um"] = json!(&coordinates[..3]);
                value["orientation_1e4_degree"] = json!(&coordinates[3..6]);
                value["remaining_slots_raw"] = json!(&coordinates[6..]);
            }
            Ok(value)
        }
        Resource::Byte => {
            Ok(json!({"index":index,"allocation_origin":"configured_unverified","value":data[0]}))
        }
        Resource::Integer => Ok(
            json!({"index":index,"allocation_origin":"configured_unverified","value":i16::from_le_bytes(data.try_into()?)}),
        ),
        Resource::DoubleInteger => Ok(
            json!({"index":index,"allocation_origin":"configured_unverified","value":i32::from_le_bytes(bytes4(data)?)}),
        ),
        Resource::RealRaw => Ok(
            json!({"index":index,"allocation_origin":"configured_unverified","word_raw":u32::from_le_bytes(bytes4(data)?),"interpretation":"unqualified_float_wire_encoding"}),
        ),
        Resource::String32 => Ok(
            json!({"index":index,"allocation_origin":"configured_unverified","bytes_raw":data,"encoding":"unqualified"}),
        ),
    }
}
