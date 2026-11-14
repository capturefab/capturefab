//! USBTMC 1.0 Tables 1/3/4/8/9; independent literals were frozen first.
use super::wire;
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;

pub(crate) const HEADER: usize = 12;

pub(crate) const fn advance(tag: u8) -> u8 {
    if tag == 255 { 1 } else { tag + 1 }
}
fn header(id: u8, tag: u8, size: u32, attributes: u8) -> Result<Vec<u8>> {
    if tag == 0 || size == 0 {
        return Err(wire::invalid("invalid USBTMC tag or transfer size"));
    }
    let mut bytes = vec![id, tag, !tag, 0];
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&[attributes, 0, 0, 0]);
    Ok(bytes)
}
pub(crate) fn command(tag: u8, payload: &[u8]) -> Result<Vec<u8>> {
    if !(1..=4098).contains(&payload.len()) {
        return Err(wire::invalid("USB query exceeds its bounded length"));
    }
    let mut bytes = header(1, tag, u32::try_from(payload.len())?, 1)?;
    bytes.extend_from_slice(payload);
    bytes.resize(bytes.len().div_ceil(4) * 4, 0);
    Ok(bytes)
}
pub(crate) fn request(tag: u8, maximum: u32) -> Result<Vec<u8>> {
    if !(1..=65_536).contains(&maximum) {
        return Err(wire::invalid("invalid USBTMC read credit"));
    }
    header(2, tag, maximum, 0)
}
pub(crate) fn capacity(maximum: u32, packet: u16) -> Result<usize> {
    if !(1..=65_536).contains(&maximum) || !(1..=1024).contains(&packet) {
        return Err(wire::invalid("invalid USBTMC input buffer geometry"));
    }
    let packet = usize::from(packet);
    // Strictly beyond the greatest header+payload+alignment extent. Thus a
    // successful short native completion also observes an exact-MPS ZLP.
    Ok((HEADER + usize::try_from(maximum)?).div_ceil(packet) * packet + packet)
}
#[derive(Clone, Copy)]
struct Header {
    size: usize,
    eom: bool,
}
fn input_header(bytes: &[u8], tag: u8, maximum: u32) -> Result<Header> {
    let bytes: &[u8; HEADER] = bytes
        .get(..HEADER)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| wire::invalid("incomplete USBTMC input header"))?;
    if tag == 0
        || bytes[0] != 2
        || bytes[1] != tag
        || bytes[2] != !tag
        || bytes[3] != 0
        || bytes[8] & !1 != 0
        || bytes[9] != 0
        || bytes[10] != 0
        || bytes[11] != 0
    {
        return Err(wire::invalid("invalid USBTMC input header/tag/attributes"));
    }
    let size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if size == 0 || size > maximum {
        return Err(wire::invalid(
            "USB input declared payload is outside its read credit",
        ));
    }
    Ok(Header {
        size: usize::try_from(size)?,
        eom: bytes[8] & 1 != 0,
    })
}
pub(crate) fn trusted_prefix(bytes: &[u8], tag: u8, maximum: u32) -> Option<&[u8]> {
    let head = input_header(bytes, tag, maximum).ok()?;
    bytes.get(HEADER..HEADER + head.size.min(bytes.len().saturating_sub(HEADER)))
}
#[derive(Clone, Copy)]
pub(crate) struct Chunk<'a> {
    pub(crate) payload: &'a [u8],
    pub(crate) eom: bool,
}
pub(crate) fn response(
    bytes: &[u8],
    short: bool,
    tag: u8,
    maximum: u32,
    packet: u16,
) -> Result<Chunk<'_>> {
    let head = input_header(bytes, tag, maximum)?;
    let actual = bytes.len() - HEADER;
    if !(1..=1024).contains(&packet)
        || !short
        || actual < head.size
        || actual - head.size >= usize::from(packet)
    {
        return Err(Categorized::new(
            ErrorKind::Incomplete,
            "USB input payload/short termination/padding is incomplete",
        )
        .details(serde_json::json!({"declared":head.size,"actual":actual,
                "short_packet":short,"max_packet_size":packet}))
        .into());
    }
    Ok(Chunk {
        payload: bytes
            .get(HEADER..HEADER + head.size)
            .ok_or_else(|| wire::invalid("USB payload extent is missing"))?,
        eom: head.eom,
    })
}
pub(crate) fn class_status(bytes: &[u8], expected: usize) -> Result<u8> {
    if bytes.len() != expected || expected == 0 {
        return Err(wire::invalid("incomplete USBTMC class response"));
    }
    let status = *bytes
        .first()
        .ok_or_else(|| wire::invalid("missing USB class status"))?;
    if status >= 0x80 {
        // Table16 requires full length but makes trailing bytes undefined.
        return Err(class_error("USBTMC class request failed", status));
    }
    if !matches!(status, 1 | 2) {
        return Err(class_error("invalid USBTMC class status", status));
    }
    Ok(status)
}
pub(crate) fn class_error(message: &'static str, status: u8) -> anyhow::Error {
    Categorized::new(ErrorKind::OperationFailed, message)
        .details(serde_json::json!({"usbtmc_status":status}))
        .into()
}
pub(crate) fn initiate(bytes: &[u8], tag: u8) -> Result<()> {
    let status = class_status(bytes, 2)?;
    if status != 1 {
        return Err(class_error(
            "unexpected warning in USB abort initiation",
            status,
        ));
    }
    if bytes.get(1) != Some(&tag) {
        return Err(wire::invalid(
            "USB abort tag disagrees with pending transfer",
        ));
    }
    Ok(())
}
pub(crate) fn abort_status(bytes: &[u8], input: bool) -> Result<(bool, bool)> {
    let status = class_status(bytes, 8)?;
    let bytes: &[u8; 8] = bytes
        .try_into()
        .map_err(|_| wire::invalid("incomplete USB abort status"))?;
    if bytes[2] != 0
        || bytes[3] != 0
        || bytes[1] & !u8::from(input) != 0
        || (status == 1 && bytes[1] != 0)
    {
        return Err(wire::invalid("invalid USB abort status fields"));
    }
    Ok((status == 1, input && bytes[1] & 1 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    fn unhex(text: &str) -> Vec<u8> {
        let (pairs, tail) = text.as_bytes().as_chunks::<2>();
        assert_eq!(tail, &[] as &[u8]);
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    fn corpus() -> Value {
        serde_json::from_str(include_str!("../../../../tests/fixtures/usbtmc/wire.json")).unwrap()
    }
    #[test]
    fn frozen_headers_tags_and_zero_alignment_match_manual_wire() {
        for case in corpus()["cases"].as_array().unwrap() {
            let tag = u8::try_from(case["tag"].as_u64().unwrap_or(1)).unwrap();
            match case["direction"].as_str().unwrap() {
                "out_command" if !case["padding_hex"].as_str().unwrap().contains(|c| c != '0') => {
                    assert_eq!(
                        command(tag, &unhex(case["payload_hex"].as_str().unwrap())).unwrap(),
                        unhex(case["wire_hex"].as_str().unwrap())
                    );
                }
                "in_request" => assert_eq!(
                    request(
                        tag,
                        u32::try_from(case["transfer_size"].as_u64().unwrap()).unwrap()
                    )
                    .unwrap(),
                    unhex(case["wire_hex"].as_str().unwrap())
                ),
                _ => {}
            }
        }
        assert_eq!(advance(255), 1);
        assert_eq!(advance(1), 2);
    }
    #[test]
    fn frozen_input_cases_keep_payload_and_real_short_evidence_separate() {
        for case in corpus()["cases"].as_array().unwrap() {
            if case["direction"] != "in_response" {
                continue;
            }
            let raw = unhex(case["wire_hex"].as_str().unwrap());
            let tag = u8::try_from(case["tag"].as_u64().unwrap_or(92)).unwrap();
            let maximum = u32::try_from(case["request_max"].as_u64().unwrap_or(32)).unwrap();
            let packet = u16::try_from(case["max_packet_size"].as_u64().unwrap_or(64)).unwrap();
            let result = response(
                &raw,
                case["short_observed"].as_bool().unwrap_or(true),
                tag,
                maximum,
                packet,
            );
            assert_eq!(
                result.is_ok(),
                case["valid"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            if let Ok(chunk) = result {
                assert_eq!(chunk.payload, unhex(case["payload_hex"].as_str().unwrap()));
                assert_eq!(chunk.eom, case["eom"].as_bool().unwrap());
            }
            if let Some(text) = case["trusted_payload_hex"].as_str() {
                assert_eq!(
                    trusted_prefix(&raw, tag, maximum),
                    Some(unhex(text).as_slice())
                );
            }
        }
        assert_eq!(capacity(52, 64).unwrap(), 128);
        assert!(capacity(65_536, 1024).unwrap() > 12 + 65_536 + 1023);
    }
    #[test]
    fn class_failure_trailing_bytes_are_ignored_but_initiation_warning_is_not() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/usbtmc/selector-control.json"
        ))
        .unwrap();
        for case in value["abort_responses"].as_array().unwrap() {
            let raw = unhex(case["hex"].as_str().unwrap());
            let status = *raw.first().expect("frozen class response has a status");
            if status >= 0x80 {
                let error = class_status(&raw, raw.len()).unwrap_err();
                assert_eq!(
                    crate::types::classify(&error).details.unwrap()["usbtmc_status"],
                    status
                );
            }
        }
        initiate(&[2, 92], 92).unwrap_err();
        assert_eq!(
            abort_status(&[2, 1, 0, 0, 239, 190, 173, 222], true).unwrap(),
            (false, true)
        );
    }
}
