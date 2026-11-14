//! Finite PF6000 Open Protocol 2.8 application-ACK wire profile.
//!
//! Slot extents follow manufacturer 9836 4415 01 release2.8, qualified by
//! the PF6000 appendix valid from2.8.14. Model-excluded spans stay opaque.
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Header {
    pub(super) mid: u16,
    pub(super) revision: u16,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}

fn part(bytes: &[u8], range: Range<usize>) -> Result<&[u8]> {
    bytes
        .get(range)
        .ok_or_else(|| invalid("Atlas frame is shorter than its fixed field extent"))
}

fn decimal(bytes: &[u8]) -> Result<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid("Atlas numeric field is not unsigned ASCII decimal"));
    }
    std::str::from_utf8(bytes)
        .map_err(|_| invalid("Atlas numeric field is not ASCII"))?
        .parse()
        .map_err(|_| invalid("Atlas numeric field exceeds its native u32 extent"))
}

/// Declared bytes exclude the final NUL. This profile never allocates above420.
pub(super) fn length(header4: [u8; 4]) -> Result<usize> {
    let declared = decimal(&header4)?;
    if !(20..=419).contains(&declared) {
        return Err(invalid("Atlas declared frame length is outside20..419"));
    }
    usize::try_from(declared).map_err(|_| invalid("Atlas frame length does not fit this host"))
}

/// Validate one complete selected inbound frame, without interpreting its data.
pub(super) fn frame(bytes: &[u8]) -> Result<Header> {
    let header: &[u8; 20] = bytes
        .first_chunk()
        .ok_or_else(|| invalid("Atlas frame has an incomplete header"))?;
    let prefix: &[u8; 4] = header
        .first_chunk()
        .ok_or_else(|| invalid("Atlas frame has an incomplete length"))?;
    let declared = length(*prefix)?;
    let (&terminal, body) = bytes
        .split_last()
        .ok_or_else(|| invalid("Atlas frame is empty"))?;
    if body.len() != declared || terminal != 0 {
        return Err(invalid("Atlas frame extent or terminal NUL is invalid"));
    }
    if !body.iter().all(|byte| (0x20..=0x7f).contains(byte)) {
        return Err(invalid("Atlas frame is outside the static ASCII domain"));
    }
    let mid = u16::try_from(decimal(part(header, 4..8)?)?)
        .map_err(|_| invalid("Atlas MID is outside its fixed extent"))?;
    let raw_revision = part(header, 8..11)?;
    let revision = match mid {
        2 if raw_revision == b"005" => 5,
        61 if raw_revision == b"003" => 3,
        4 | 5 | 9999 if matches!(raw_revision, b"   " | b"000" | b"001") => 1,
        _ => {
            return Err(invalid(
                "Atlas MID/revision is outside the selected profile",
            ));
        }
    };
    let expected_length = match mid {
        2 => 173,
        4 => 26,
        5 => 24,
        61 => 419,
        9999 => 20,
        _ => return Err(invalid("Atlas MID is outside the selected profile")),
    };
    if declared != expected_length {
        return Err(invalid(
            "Atlas MID/revision has the wrong fixed frame length",
        ));
    }
    if mid == 61 && header[11] != b'0' {
        return Err(invalid(
            "Atlas reliable result requires ASCII0 in its ACK flag",
        ));
    }
    for range in [12..14, 14..16] {
        if !matches!(part(header, range)?, b"  " | b"01") {
            return Err(invalid("Atlas frame uses an unselected station or spindle"));
        }
    }
    if !matches!(part(header, 16..18)?, b"  " | b"00")
        || !matches!(header[18], b' ' | b'0')
        || !matches!(header[19], b' ' | b'0')
    {
        return Err(invalid(
            "Atlas link sequence or multipart mode is not supported",
        ));
    }
    Ok(Header { mid, revision })
}

/// Encode only the five accepted operations. No arbitrary command facility exists.
pub(super) fn command(mid: u16, revision: u16, subscribe_ack: bool) -> Result<Vec<u8>> {
    let literal: &[u8; 21] = match (mid, revision, subscribe_ack) {
        (1, 5, false) => b"00200001005         \0",
        (60, 3, true) => b"002000600030        \0",
        (62, 1, false) => b"00200062001         \0",
        (63, 1, false) => b"00200063001         \0",
        (9999, 1, false) => b"00209999001         \0",
        _ => {
            return Err(Usage("Atlas command is outside the five-operation profile".into()).into());
        }
    };
    Ok(literal.to_vec())
}

#[derive(Clone, Copy)]
enum FieldKind {
    Number,
    Text,
    TighteningId,
}

#[derive(Clone, Copy)]
struct Field {
    id: [u8; 2],
    start: usize,
    width: usize,
    name: &'static str,
    kind: FieldKind,
}

const fn field(
    id: [u8; 2],
    start: usize,
    width: usize,
    name: &'static str,
    kind: FieldKind,
) -> Field {
    Field {
        id,
        start,
        width,
        name,
        kind,
    }
}

const STARTUP_FIELDS: [Field; 6] = [
    field(*b"02", 28, 2, "channel_id", FieldKind::Number),
    field(*b"03", 32, 25, "controller_name", FieldKind::Text),
    field(*b"06", 85, 19, "controller_software", FieldKind::Text),
    field(*b"07", 106, 19, "tool_software", FieldKind::Text),
    field(*b"09", 153, 10, "controller_serial", FieldKind::Text),
    field(*b"10", 165, 3, "system_type_code", FieldKind::Number),
];

const RESULT_FIELDS: [Field; 32] = [
    field(*b"03", 32, 25, "controller_name", FieldKind::Text),
    field(*b"04", 59, 25, "identifier", FieldKind::Text),
    field(*b"05", 86, 4, "job_id", FieldKind::Number),
    field(*b"06", 92, 3, "pset_menu_index", FieldKind::Number),
    field(*b"07", 97, 2, "strategy_code", FieldKind::Number),
    field(*b"09", 108, 4, "batch_size", FieldKind::Number),
    field(*b"10", 114, 4, "batch_count", FieldKind::Number),
    field(*b"11", 120, 1, "tightening_status_code", FieldKind::Number),
    field(*b"12", 123, 1, "batch_status_code", FieldKind::Number),
    field(*b"13", 126, 1, "torque_status_code", FieldKind::Number),
    field(*b"14", 129, 1, "angle_status_code", FieldKind::Number),
    field(*b"15", 132, 1, "rundown_status_code", FieldKind::Number),
    field(*b"17", 138, 1, "selftap_status_code", FieldKind::Number),
    field(*b"21", 159, 6, "torque_min_raw", FieldKind::Number),
    field(*b"22", 167, 6, "torque_max_raw", FieldKind::Number),
    field(*b"23", 175, 6, "torque_target_raw", FieldKind::Number),
    field(*b"24", 183, 6, "torque_result_raw", FieldKind::Number),
    field(*b"25", 191, 5, "angle_min_deg", FieldKind::Number),
    field(*b"26", 198, 5, "angle_max_deg", FieldKind::Number),
    field(*b"27", 205, 5, "angle_target_deg", FieldKind::Number),
    field(*b"28", 212, 5, "angle_result_deg", FieldKind::Number),
    field(*b"29", 219, 5, "rundown_angle_min_deg", FieldKind::Number),
    field(*b"30", 226, 5, "rundown_angle_max_deg", FieldKind::Number),
    field(*b"35", 255, 6, "selftap_min_raw", FieldKind::Number),
    field(*b"36", 263, 6, "selftap_max_raw", FieldKind::Number),
    field(*b"41", 303, 10, "tightening_id", FieldKind::TighteningId),
    field(*b"44", 329, 14, "tool_serial", FieldKind::Text),
    field(*b"45", 345, 19, "tightening_time_local", FieldKind::Text),
    field(*b"46", 366, 19, "pset_change_time_local", FieldKind::Text),
    field(*b"47", 387, 25, "pset_name", FieldKind::Text),
    field(*b"48", 414, 1, "torque_unit_code", FieldKind::Number),
    field(*b"49", 417, 2, "result_type_code", FieldKind::Number),
];

fn fields(bytes: &[u8], selected: &[Field]) -> Result<(Map<String, Value>, Vec<&'static str>)> {
    let mut values = Map::new();
    let mut unavailable = Vec::new();
    for field in selected {
        if part(bytes, field.start.saturating_sub(2)..field.start)? != field.id.as_slice() {
            return Err(invalid(format!(
                "Atlas supported parameter ID does not match {}",
                field.name
            )));
        }
        let bytes = part(bytes, field.start..field.start.saturating_add(field.width))?;
        let text =
            std::str::from_utf8(bytes).map_err(|_| invalid("Atlas field is outside ASCII"))?;
        let value = match field.kind {
            FieldKind::Text => Value::from(text.trim_end_matches(' ')),
            FieldKind::Number | FieldKind::TighteningId => {
                if bytes.iter().all(|byte| *byte == b' ') {
                    unavailable.push(field.name);
                    continue;
                }
                let number = decimal(bytes)?;
                if matches!(field.kind, FieldKind::TighteningId) {
                    values.insert("tightening_id_raw".into(), Value::from(text));
                }
                Value::from(number)
            }
        };
        values.insert(field.name.into(), value);
    }
    Ok((values, unavailable))
}

pub(super) fn startup(bytes: &[u8]) -> Result<Value> {
    if frame(bytes)?.mid != 2 {
        return Err(invalid("Atlas startup expected MID0002"));
    }
    let (mut controller, unavailable) = fields(bytes, &STARTUP_FIELDS)?;
    if controller.get("system_type_code") != Some(&json!(3)) {
        return Err(invalid(
            "Atlas selected PF6000 profile requires system type003",
        ));
    }
    controller.insert("system_type_raw".into(), Value::from("003"));
    controller.insert("raw_hex".into(), Value::from(crate::types::hex(bytes)));
    if !unavailable.is_empty() {
        controller.insert("unavailable_fields".into(), json!(unavailable));
    }
    Ok(Value::Object(controller))
}

pub(super) fn result(bytes: &[u8], controller: &Value) -> Result<Value> {
    let header = frame(bytes)?;
    if header.mid != 61 {
        return Err(invalid("Atlas result expected MID0061"));
    }
    let (mut result, unavailable) = fields(bytes, &RESULT_FIELDS)?;
    result.insert("torque_scale".into(), Value::from(100));
    let unit = match result.get("torque_unit_code").and_then(Value::as_u64) {
        Some(1) => Some("Nm"),
        Some(2) => Some("lbf.ft"),
        Some(3) => Some("lbf.in"),
        Some(4) => Some("kpm"),
        Some(5) => Some("kgf.cm"),
        Some(6) => Some("ozf.in"),
        Some(7) => Some("percent"),
        Some(8) => Some("Ncm"),
        _ => None,
    };
    if let Some(unit) = unit {
        result.insert("torque_unit".into(), Value::from(unit));
    }
    Ok(json!({
        "kind":"atlas_open_protocol_result",
        "profile":"pf6000_v2_8",
        "startup_revision":5,
        "result_revision":header.revision,
        "ack_policy":"jsonl_checkpoint_before_send",
        "raw_hex":crate::types::hex(bytes),
        "controller":controller,
        "result":result,
        "unavailable_fields":unavailable,
        "unsupported_fields":["01","02","08","16","18","19","20","31","32","33","34","37","38","39","40","42","43"]
    }))
}

pub(super) fn accepted_or_error(bytes: &[u8], pending_mid: u16) -> Result<()> {
    let header = frame(bytes)?;
    if !matches!(header.mid, 4 | 5) {
        return Err(invalid("Atlas control expected MID0004 or MID0005"));
    }
    let echoed = decimal(part(bytes, 20..24)?)?;
    if echoed != u32::from(pending_mid) {
        return Err(invalid(
            "Atlas control response does not match its pending MID",
        ));
    }
    if header.mid == 4 {
        let native = part(bytes, 24..26)?;
        let code = decimal(native)?;
        let raw_code = std::str::from_utf8(native)
            .map_err(|_| invalid("Atlas command error code is not ASCII"))?;
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            format!("Atlas MID{pending_mid:04} rejected with native error{raw_code}"),
        )
        .details(json!({
            "failed_mid":pending_mid,
            "error_code":code,
            "error_code_raw":raw_code,
            "raw_hex":crate::types::hex(bytes)
        }))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::classify;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/atlas_open_protocol/pf6000.json"
        ))
        .unwrap()
    }

    fn wire(case: &Value) -> Vec<u8> {
        crate::types::decode_hex(case["wire_hex"].as_str().unwrap(), 420).unwrap()
    }

    #[test]
    fn only_the_five_canonical_operations_can_be_encoded() {
        let oracle = oracle();
        for case in oracle["outbound_frames"].as_array().unwrap() {
            let mid = case["mid"].as_str().unwrap().parse::<u16>().unwrap();
            let revision = u16::try_from(case["revision"].as_u64().unwrap()).unwrap();
            assert_eq!(command(mid, revision, mid == 60).unwrap(), wire(case));
        }
        for tuple in [
            (3, 1, false),
            (60, 3, false),
            (62, 3, false),
            (9999, 0, false),
        ] {
            command(tuple.0, tuple.1, tuple.2).unwrap_err();
        }
    }

    #[test]
    fn startup_matches_every_literal_qualified_and_opaque_span() {
        let oracle = oracle();
        for case in oracle["valid_startups"].as_array().unwrap() {
            assert_eq!(startup(&wire(case)).unwrap(), case["expected_controller"]);
        }
    }

    #[test]
    fn results_preserve_all_native_fields_unknown_codes_blanks_and_raw_frames() {
        let oracle = oracle();
        for case in oracle["valid_results"].as_array().unwrap() {
            let startup_case = oracle["valid_startups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|startup| startup["name"] == case["startup_frame"])
                .unwrap();
            let controller = startup(&wire(startup_case)).unwrap();
            assert_eq!(
                result(&wire(case), &controller).unwrap(),
                case["expected_value"]
            );
        }
    }

    #[test]
    fn control_aliases_correlate_and_keep_original_native_failure_codes() {
        let oracle = oracle();
        for case in oracle["control_frames"].as_array().unwrap() {
            let bytes = wire(case);
            assert_eq!(frame(&bytes).unwrap().revision, 1);
            let expected = &case["expected"];
            match expected["mid"].as_str().unwrap() {
                "0005" => {
                    let pending = expected["accepted_mid"].as_str().unwrap().parse().unwrap();
                    accepted_or_error(&bytes, pending).unwrap();
                    assert_eq!(
                        classify(&accepted_or_error(&bytes, 1).unwrap_err()).kind,
                        ErrorKind::Incomplete
                    );
                }
                "0004" => {
                    let pending = expected["failed_mid"].as_str().unwrap().parse().unwrap();
                    let error = accepted_or_error(&bytes, pending).unwrap_err();
                    let error = classify(&error);
                    assert_eq!(error.kind, ErrorKind::OperationFailed);
                    let details = error.details.as_ref().unwrap();
                    assert_eq!(details["failed_mid"], pending);
                    assert_eq!(details["error_code"], expected["error_code"]);
                    assert_eq!(details["error_code_raw"], expected["error_code_raw"]);
                    assert_eq!(details["raw_hex"], case["wire_hex"]);
                }
                "9999" => {
                    accepted_or_error(&bytes, 9999).unwrap_err();
                }
                other => panic!("unexpected literal control {other}"),
            }
        }
    }

    #[test]
    fn malformed_literal_frames_never_produce_a_selected_observation() {
        let oracle = oracle();
        let controller = &oracle["valid_startups"][0]["expected_controller"];
        for case in oracle["malformed_frames"].as_array().unwrap() {
            let bytes = wire(case);
            let failure = match frame(&bytes) {
                Err(error) => error,
                Ok(Header { mid: 2, .. }) => startup(&bytes).unwrap_err(),
                Ok(Header { mid: 61, .. }) => result(&bytes, controller).unwrap_err(),
                Ok(other) => panic!("malformed literal passed framing: {other:?}"),
            };
            assert_eq!(classify(&failure).kind, ErrorKind::Incomplete);
        }
        for prefix in [*b"0000", *b"0019", *b"0420", *b"9999", *b" 020"] {
            length(prefix).unwrap_err();
        }
    }
}
