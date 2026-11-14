//! Fixed A&D standard reply parsing for an explicitly configured interface.
//! Communication Manual 1WMPD4003801B §§6.2–6.3, 7–8; the separately pinned
//! base edition corroborates the published fields and replies. No mode changes.
#![forbid(unsafe_code)]
#![deny(clippy::panic_in_result_fn)]

use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Number, Value, json};

const BODY_BYTES: usize = 15;

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub(super) enum Terminator {
    #[default]
    #[serde(rename = "crlf")]
    CrLf,
    #[serde(rename = "cr")]
    Cr,
}

impl Terminator {
    pub(super) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::CrLf => b"\r\n",
            Self::Cr => b"\r",
        }
    }

    pub(super) const fn request(self) -> &'static [u8] {
        match self {
            Self::CrLf => b"Q\r\n",
            Self::Cr => b"Q\r",
        }
    }

    pub(super) const fn max_frame_bytes(self) -> usize {
        BODY_BYTES + self.bytes().len()
    }
}

/// One complete reply, including the configured terminator. No state change,
/// retry, decimal/unit conversion, time inference or transport is performed.
pub(super) fn decode(frame: &[u8], terminator: Terminator) -> Result<Value> {
    if frame.len() > terminator.max_frame_bytes() {
        return Err(malformed("reply exceeds the fixed standard-format limit"));
    }
    let body = frame
        .strip_suffix(terminator.bytes())
        .ok_or_else(|| malformed("reply does not end with the configured terminator"))?;
    if !body.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
        return Err(malformed("reply body is not printable ASCII"));
    }
    let raw = std::str::from_utf8(frame).map_err(|_| malformed("reply is not ASCII"))?;
    if let Some(error) = body.strip_prefix(b"EC,E") {
        let [tens, units] = error else {
            return Err(malformed("device error requires two decimal code digits"));
        };
        if !tens.is_ascii_digit() || !units.is_ascii_digit() {
            return Err(malformed("device error code is not decimal"));
        }
        let code = std::str::from_utf8(
            body.get(3..6)
                .ok_or_else(|| malformed("truncated device error code"))?,
        )
        .map_err(|_| malformed("device error code is not ASCII"))?;
        let mut value = json!({"format":"aandd_standard","status":"device_error",
            "status_raw":"EC","error_code_raw":code,"raw":raw});
        if code == "E02" {
            value["device_state"] = json!("not_ready");
        }
        return Ok(value);
    }
    if body.len() != BODY_BYTES || body.get(2) != Some(&b',') {
        return Err(malformed(
            "standard reply requires exactly fifteen body bytes and a comma",
        ));
    }
    let status = body
        .get(..2)
        .ok_or_else(|| malformed("missing status header"))?;
    let field = std::str::from_utf8(
        body.get(3..12)
            .ok_or_else(|| malformed("missing nine-byte data field"))?,
    )
    .map_err(|_| malformed("data field is not ASCII"))?;
    let unit_raw = std::str::from_utf8(
        body.get(12..15)
            .ok_or_else(|| malformed("missing three-byte unit field"))?,
    )
    .map_err(|_| malformed("unit field is not ASCII"))?;
    if status == b"OL" {
        // In this format the printed overload exponent crosses the nominal
        // data/unit boundary. It is neither an actual huge value nor a unit.
        let polarity = match body {
            b"OL,+9999999E+19" => "positive",
            b"OL,-9999999E+19" => "negative",
            _ => return Err(malformed("unqualified standard overload sentinel")),
        };
        return Ok(
            json!({"format":"aandd_standard","status":"overload","status_raw":"OL",
            "overload_polarity":polarity,"value_raw":field,"unit_raw":unit_raw,"raw":raw}),
        );
    }
    let (state, status_raw) = match status {
        b"ST" => ("stable", "ST"),
        b"US" => ("unstable", "US"),
        b"QT" => ("counting_stable", "QT"),
        _ => {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "A&D standard: unqualified status header",
            ));
        }
    };
    let value = decimal(field)?;
    let mut decoded = json!({"format":"aandd_standard","status":state,"status_raw":status_raw,
        "value_raw":field,"value":value,"unit_raw":unit_raw,"raw":raw});
    let unit = unit_raw.trim_matches(' ');
    if !unit.is_empty() {
        // Report only the received code. No dimensional mapping or scaling.
        decoded["unit"] = json!(unit);
    }
    Ok(decoded)
}

fn decimal(raw: &str) -> Result<Number> {
    let (negative, magnitude) = if let Some(value) = raw.strip_prefix('+') {
        (false, value)
    } else if let Some(value) = raw.strip_prefix('-') {
        (true, value)
    } else {
        return Err(malformed("standard decimal needs an explicit polarity"));
    };
    let (whole, fraction) = magnitude
        .split_once('.')
        .map_or((magnitude, None), |(whole, fraction)| {
            (whole, Some(fraction))
        });
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|digits| {
            digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(malformed("invalid standard decimal data field"));
    }
    let integer = whole.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let mut canonical = String::with_capacity(raw.len());
    if negative {
        canonical.push('-');
    }
    canonical.push_str(integer);
    if let Some(fraction) = fraction {
        canonical.push('.');
        canonical.push_str(fraction);
    }
    canonical
        .parse()
        .map_err(|_| malformed("invalid exact standard decimal"))
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("A&D standard: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_stable_and_unstable_fields_keep_original_decimal_spelling() {
        for (frame, status, raw, number) in [
            (
                &b"ST,+03142.06  g\r\n"[..],
                "stable",
                "+03142.06",
                "3142.06",
            ),
            (b"US,-00295.87  g\r\n", "unstable", "-00295.87", "-295.87"),
        ] {
            let value = decode(frame, Terminator::CrLf).unwrap();
            assert_eq!(value["status"], status);
            assert_eq!(value["value_raw"], raw);
            assert_eq!(value["value"].to_string(), number);
            assert_eq!(value["unit_raw"], "  g");
            assert_eq!(value["unit"], "g");
            assert_eq!(value["raw"], std::str::from_utf8(frame).unwrap());
            assert!(value.get("source_time").is_none());
        }
    }

    #[test]
    fn published_overloads_never_become_numeric_values_or_mass_units() {
        for (frame, polarity) in [
            (&b"OL,+9999999E+19\r\n"[..], "positive"),
            (b"OL,-9999999E+19\r\n", "negative"),
        ] {
            let value = decode(frame, Terminator::CrLf).unwrap();
            assert_eq!(value["status"], "overload");
            assert_eq!(value["overload_polarity"], polarity);
            assert_eq!(value["unit_raw"], "+19");
            assert!(value.get("value").is_none());
            assert!(value.get("unit").is_none());
        }
    }

    #[test]
    fn complete_device_error_is_a_nonnumeric_observation() {
        assert_eq!(
            decode(b"EC,E02\r\n", Terminator::CrLf).unwrap(),
            json!({"format":"aandd_standard","status":"device_error","status_raw":"EC",
                "error_code_raw":"E02","device_state":"not_ready","raw":"EC,E02\r\n"})
        );
        let unknown = decode(b"EC,E99\r\n", Terminator::CrLf).unwrap();
        assert_eq!(unknown["error_code_raw"], "E99");
        assert!(unknown.get("device_state").is_none());
        assert!(unknown.get("value").is_none());
    }

    #[test]
    fn configured_terminator_is_exact_and_only_q_is_exposed() {
        assert_eq!(Terminator::CrLf.request(), b"Q\r\n");
        assert_eq!(Terminator::Cr.request(), b"Q\r");
        assert_eq!(Terminator::CrLf.max_frame_bytes(), 17);
        assert_eq!(Terminator::Cr.max_frame_bytes(), 16);
        assert_eq!(
            decode(b"ST,+00000.00  g\r", Terminator::Cr).unwrap()["value"].to_string(),
            "0.00"
        );
        decode(b"ST,+00000.00  g\r", Terminator::CrLf).unwrap_err();
        decode(b"ST,+00000.00  g\r\n", Terminator::Cr).unwrap_err();
    }

    #[test]
    fn derived_counting_integer_fraction_and_missing_unit_keep_exact_fields() {
        let count = decode(b"QT,+00000123pcs\r\n", Terminator::CrLf).unwrap();
        assert_eq!(count["status"], "counting_stable");
        assert_eq!(count["value"], json!(123));
        assert_eq!(count["unit"], "pcs");
        let fraction = decode(b"US,-000.0001   \r\n", Terminator::CrLf).unwrap();
        assert_eq!(fraction["value_raw"], "-000.0001");
        assert_eq!(fraction["value"].to_string(), "-0.0001");
        assert_eq!(fraction["unit_raw"], "   ");
        assert!(fraction.get("unit").is_none());
    }

    #[test]
    fn malformed_complete_or_partial_frames_do_not_produce_values() {
        for raw in [
            &b"ST,+03142.06  g\r"[..],
            b"ST,+03142.06  g\n",
            b"ST,+03142.06 g\r\n",
            b"ST;+03142.06  g\r\n",
            b"ST, 03142.06  g\r\n",
            b"ST,+0314E.06  g\r\n",
            b"ST,+03142,06  g\r\n",
            b"ST,+03142.06\tg\r\n",
            b"ST,+03142.06  g\r\nST,+03142.06  g\r\n",
            b"OL,+1234567E+19\r\n",
            b"EC,E2\r\n",
            b"EC,E0X\r\n",
            b"\x06\r\n",
        ] {
            decode(raw, Terminator::CrLf).expect_err("malformed standard reply accepted");
        }
        let unknown = decode(b"XX,+03142.06  g\r\n", Terminator::CrLf).unwrap_err();
        assert_eq!(
            crate::types::classify(&unknown).kind,
            ErrorKind::Unsupported
        );
    }
}
