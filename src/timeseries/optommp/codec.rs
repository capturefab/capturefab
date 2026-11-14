//! Pure `OptoMMP` `ReadQuadlet` framing (form 1465 printed pp97–106). No I/O.
use super::model::{self, Resource};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde_json::{Value, json};

pub(crate) const REQUEST: usize = 12;
pub(crate) const PREFIX: usize = 12;
pub(crate) const WORD: usize = 4;
const READ_QUADLET: u8 = 0x40;
const READ_QUADLET_RESPONSE: u8 = 0x60;
/// Transaction labels are six bits.
pub(crate) const LABELS: u8 = 64;

/// Exactly twelve lowercase hex digits without `0x`.
pub(crate) fn address_hex(address: u64) -> String {
    format!("{address:012x}")
}

pub(crate) fn failure(kind: ErrorKind, reason: &str, message: &str) -> anyhow::Error {
    Categorized::new(kind, format!("OptoMMP: {message}"))
        .details(json!({ "reason": reason }))
        .into()
}

/// One `ReadQuadlet` request. Only fixed nondestructive roles are encodable.
pub(crate) fn request(label: u8, address: u64) -> Result<[u8; REQUEST]> {
    if label >= LABELS || address >> 48 != 0 || !model::readable(address) {
        return Err(failure(
            ErrorKind::Unsupported,
            "address_not_allowed",
            "refusing a request outside the read-only input allowlist",
        ));
    }
    let mut bytes = [0; REQUEST];
    bytes[2] = label << 2;
    bytes[3] = READ_QUADLET;
    // Source ID bytes 4..5 stay zero; bytes 6..11 are the BE48 offset.
    bytes[4..].copy_from_slice(&address.to_be_bytes());
    bytes[4] = 0;
    bytes[5] = 0;
    Ok(bytes)
}

/// A validated, correlated response prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Prefix {
    /// Opaque observed field, never correlation or authentication.
    pub(crate) source_id: u16,
    pub(crate) response_code: u8,
}

/// Validate every documented zero, the response type and the pending label.
/// A nonzero code is trusted as native only after this whole check passes.
pub(crate) fn prefix(bytes: &[u8; PREFIX], label: u8) -> Result<Prefix> {
    let [d0, d1, tl, tcode, s0, s1, rcode, b7, r0, r1, r2, r3] = *bytes;
    let malformed = |message: &str| failure(ErrorKind::Incomplete, "malformed_response", message);
    if d0 != 0 || d1 != 0 {
        return Err(malformed("response destination ID is not zero"));
    }
    if tl & 0x03 != 0 {
        return Err(malformed("response retry bits are not zero"));
    }
    if tcode != READ_QUADLET_RESPONSE {
        return Err(malformed(
            "response is not a ReadQuadlet response with zero priority",
        ));
    }
    if rcode & 0x0F != 0 || b7 != 0 || [r0, r1, r2, r3] != [0; 4] {
        return Err(malformed("response reserved bits are not zero"));
    }
    if tl >> 2 != label {
        return Err(failure(
            ErrorKind::Incomplete,
            "label_mismatch",
            "response transaction label does not match the outstanding request",
        ));
    }
    Ok(Prefix {
        source_id: u16::from_be_bytes([s0, s1]),
        response_code: rcode >> 4,
    })
}

/// Native NAK evidence: the validated prefix only, never an assumed tail.
pub(crate) fn native_error(
    header: &[u8; PREFIX],
    prefix: Prefix,
    label: u8,
    address: u64,
) -> anyhow::Error {
    Categorized::new(
        ErrorKind::OperationFailed,
        format!(
            "OptoMMP: device returned response code {} for address {}",
            prefix.response_code,
            address_hex(address)
        ),
    )
    .details(json!({
        "reason":"optommp_response",
        "optommp_response":{
            "raw_header_hex":crate::types::hex(header),
            "response_code":prefix.response_code,
            "transaction_label":label,
            "source_id":prefix.source_id,
            "requested_address":address_hex(address),
        }
    }))
    .into()
}

/// The native interpretation of one value word; raw bits are always kept by the caller.
pub(crate) fn value(resource: Resource, word: u32) -> Value {
    match resource {
        Resource::DigitalState => json!({"encoding":"unsigned32_be","value":word}),
        Resource::AnalogEu => {
            let number = f32::from_bits(word);
            let classification = if number.is_nan() {
                "nan"
            } else if number.is_infinite() && number.is_sign_positive() {
                "positive_infinity"
            } else if number.is_infinite() {
                "negative_infinity"
            } else {
                "finite"
            };
            let mut value =
                json!({"encoding":"ieee754_binary32_be","classification":classification});
            if number.is_finite() {
                // Exact widening; the raw word keeps the sign of zero.
                value["value"] = json!(f64::from(number));
            }
            value
        }
    }
}
