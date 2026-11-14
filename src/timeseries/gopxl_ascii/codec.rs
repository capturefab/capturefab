//! Pure `GoPxL` Ethernet ASCII `measurement` request/reply grammar. No I/O.
//!
//! Reply: `OK` then `M<id>,V<value|invalid>,D<decision>` per requested ID, or `ERROR`
//! then the device's message. The value is the measurement multiplied by 1000.
use super::config::{Config, Trailing};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde_json::{Value, json};
use std::str::FromStr as _;

/// Largest reply body before its terminator; a software bound.
pub(crate) const MAX_BODY: usize = 65_536;

pub(crate) fn failure(kind: ErrorKind, reason: &str, message: &str) -> anyhow::Error {
    Categorized::new(kind, format!("GoPxL: {message}"))
        .details(json!({ "reason": reason }))
        .into()
}
fn malformed(message: &str) -> anyhow::Error {
    failure(ErrorKind::Incomplete, "malformed_reply", message)
}

/// The only command this profile ever sends.
pub(crate) fn request(config: &Config) -> Vec<u8> {
    let mut bytes = b"measurement".to_vec();
    for measurement in &config.measurements {
        bytes.push(config.delimiter);
        bytes.extend_from_slice(measurement.id.to_string().as_bytes());
    }
    bytes.extend_from_slice(config.terminator.bytes());
    bytes
}

/// Exact 1.3 manufacturer strings; anything else stays unclassified.
fn classify(text: &str) -> &'static str {
    match text {
        "There is no measurement data to output. Please confirm that the sensor is running" => {
            "no_data"
        }
        "Invalid parameter. Please verify your input" => "invalid_parameter",
        "Specified measurement ID not found. Please verify your input" => "id_not_found",
        "One or more measurement id must be provided" => "id_required",
        _ => "unclassified",
    }
}

/// Nonempty ASCII digits with leading zeros removed (at least "0").
fn trimmed(raw: &str) -> Option<&str> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let trimmed = raw.trim_start_matches('0');
    Some(if trimmed.is_empty() { "0" } else { trimmed })
}
/// Digits after a one-letter prefix: the raw text and its trimmed digits.
fn digits(field: &str, prefix: char) -> Option<(&str, &str)> {
    let raw = field.strip_prefix(prefix)?;
    Some((raw, trimmed(raw)?))
}

/// Exact decimal of `scaled / 1000` with three fractional digits, without floats.
pub(crate) fn decimal(scaled: i32) -> String {
    let magnitude = scaled.unsigned_abs();
    let sign = if scaled < 0 { "-" } else { "" };
    format!("{sign}{}.{:03}", magnitude / 1000, magnitude % 1000)
}

struct Triple<'a> {
    raw_id: &'a str,
    id: Option<u32>,
    raw_value: &'a str,
    scaled: Option<i32>,
    raw_decision: &'a str,
    decision: u8,
}

fn triple<'a>(fields: &[&'a str], invalid: &str) -> Result<Triple<'a>> {
    let [m, v, d] = fields else {
        return Err(malformed("measurement fields are not M,V,D triplets"));
    };
    let (raw_id, id) = digits(m, 'M').ok_or_else(|| malformed("measurement ID field"))?;
    let raw_value = v
        .strip_prefix('V')
        .ok_or_else(|| malformed("measurement value field"))?;
    let scaled = if raw_value == invalid {
        None
    } else {
        let (negative, unsigned) = raw_value
            .strip_prefix('-')
            .map_or((false, raw_value), |rest| (true, rest));
        let magnitude = trimmed(unsigned)
            .ok_or_else(|| malformed("measurement value is not a signed 32-bit decimal"))?;
        if magnitude.len() > 10 {
            return Err(malformed("measurement value exceeds signed 32 bits"));
        }
        let magnitude = i64::from_str(magnitude)
            .map_err(|_| malformed("measurement value is not a signed 32-bit decimal"))?;
        let value = if negative { -magnitude } else { magnitude };
        Some(
            i32::try_from(value)
                .map_err(|_| malformed("measurement value exceeds signed 32 bits"))?,
        )
    };
    let (raw_decision, decision) =
        digits(d, 'D').ok_or_else(|| malformed("measurement decision field"))?;
    if decision.len() > 3 {
        return Err(malformed("measurement decision exceeds 8 bits"));
    }
    let decision =
        u8::from_str(decision).map_err(|_| malformed("measurement decision exceeds 8 bits"))?;
    // An ID too long for 32 bits cannot match any request; it is compared below.
    let id = (id.len() <= 10).then(|| u32::from_str(id).ok()).flatten();
    Ok(Triple {
        raw_id,
        id,
        raw_value,
        scaled,
        raw_decision,
        decision,
    })
}

/// A successfully parsed reply: one structured value per requested measurement.
pub(crate) struct Parsed {
    pub(crate) trailing_observed: bool,
    pub(crate) values: Vec<Value>,
}

/// Parse a reply body (bytes before the terminator).
pub(crate) fn parse(body: &[u8], config: &Config) -> Result<Parsed> {
    if body.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(malformed(
            "reply contains a line break before its terminator",
        ));
    }
    let text = std::str::from_utf8(body).map_err(|_| malformed("reply is not text"))?;
    let delimiter = char::from(config.delimiter);
    let mut fields = text.split(delimiter).collect::<Vec<_>>();
    let trailing_observed = fields.len() > 1 && fields.last() == Some(&"");
    match (config.trailing_delimiter, trailing_observed) {
        (Trailing::Absent, true) => return Err(malformed("unexpected trailing delimiter")),
        (Trailing::Present, false) => return Err(malformed("missing trailing delimiter")),
        _ => {}
    }
    if trailing_observed {
        fields.pop();
    }
    let Some((status, rest)) = fields.split_first() else {
        return Err(malformed("empty reply"));
    };
    match *status {
        "ERROR" => {
            let message = rest.join(&delimiter.to_string());
            let classification = classify(&message);
            Err(Categorized::new(
                ErrorKind::OperationFailed,
                format!("GoPxL: sensor returned ERROR: {message}"),
            )
            .details(json!({
                "reason":"device_error",
                "gopxl_reply":{"error_text":message,"classification":classification}
            }))
            .into())
        }
        "OK" => {
            let (triples, tail) = rest.as_chunks::<3>();
            if !tail.is_empty() || triples.len() != config.measurements.len() {
                return Err(malformed(
                    "reply does not contain exactly one M,V,D triplet per requested ID",
                ));
            }
            // Every field's syntax is checked before any ID correlation.
            let parsed = triples
                .iter()
                .map(|fields| triple(fields, &config.invalid_token))
                .collect::<Result<Vec<_>>>()?;
            let mut values = Vec::with_capacity(parsed.len());
            for (measurement, triple) in config.measurements.iter().zip(parsed) {
                if triple.id != Some(measurement.id) {
                    return Err(failure(
                        ErrorKind::Incomplete,
                        "id_mismatch",
                        "reply measurement ID does not match the request at that position",
                    ));
                }
                let mut value = json!({
                    "kind":"gopxl_measurement",
                    "id":measurement.id,
                    "raw_id_text":triple.raw_id,
                    "raw_value_text":triple.raw_value,
                    "raw_decision_text":triple.raw_decision,
                    "value_invalid":triple.scaled.is_none(),
                    "decision_raw":triple.decision,
                    "pass":triple.decision & 1 != 0,
                    "decision_invalid":triple.decision & 2 != 0,
                    "decision_reserved_bits":triple.decision & 0xFC,
                    "scaling":"documented_value_times_1000",
                    "units_provenance":"externally_configured_unqueried",
                });
                if let Some(scaled) = triple.scaled {
                    value["scaled_i32"] = json!(scaled);
                    // Arbitrary-precision JSON keeps the exact decimal text.
                    value["value"] = Value::Number(
                        serde_json::Number::from_str(&decimal(scaled))
                            .map_err(|_| malformed("decimal conversion"))?,
                    );
                }
                values.push(value);
            }
            Ok(Parsed {
                trailing_observed,
                values,
            })
        }
        _ => Err(malformed("reply status is neither OK nor ERROR")),
    }
}
