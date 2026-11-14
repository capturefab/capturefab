//! Fixed SBI16/22 replies from the Sartorius interface note, status09/2024,
//! printed pp.2–3. Printer-format verification brackets are outside SBI mode.
#![forbid(unsafe_code)]
#![deny(clippy::panic_in_result_fn)]

use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Number, Value, json};

#[derive(Debug, Clone, Copy, Deserialize)]
pub(super) enum Format {
    #[serde(rename = "sbi16")]
    Sbi16,
    #[serde(rename = "sbi22")]
    Sbi22,
}

impl Format {
    pub(super) const fn reply_bytes(self) -> usize {
        match self {
            Self::Sbi16 => 16,
            Self::Sbi22 => 22,
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::Sbi16 => "sbi16",
            Self::Sbi22 => "sbi22",
        }
    }
}

pub(super) fn decode(frame: &[u8], format: Format) -> Result<Value> {
    if frame.len() != format.reply_bytes() {
        return Err(malformed(
            "reply length differs from configured16/22-byte format",
        ));
    }
    let body = frame
        .strip_suffix(b"\r\n")
        .ok_or_else(|| malformed("reply needs CRLF"))?;
    if !body.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
        return Err(malformed("reply body is not printable ASCII"));
    }
    let raw = std::str::from_utf8(frame).map_err(|_| malformed("reply is not ASCII"))?;
    let (id, data) = match format {
        Format::Sbi16 => (None, body),
        Format::Sbi22 => {
            let id = std::str::from_utf8(
                body.get(..6)
                    .ok_or_else(|| malformed("missing six-byte ID"))?,
            )
            .map_err(|_| malformed("ID is not ASCII"))?;
            (
                Some(id),
                body.get(6..)
                    .ok_or_else(|| malformed("missing fixed data fields"))?,
            )
        }
    };
    let mut value = json!({"format":"sartorius_sbi","line_format":format.name(),"raw":raw});
    if let Some(id) = id {
        value["id_raw"] = json!(id);
        let code = id.trim_matches(' ');
        if !code.is_empty() {
            value["id"] = json!(code);
        }
    }
    let state = match data {
        b"      High    " => Some(("overload", "High")),
        b"      Low     " => Some(("underload", "Low")),
        b"   Cal.Ext.   " => Some(("external_calibration", "Cal.Ext.")),
        b"   APP.ERR    " => Some(("device_error", "APP.ERR")),
        b"   DIS.ERR    " => Some(("device_error", "DIS.ERR")),
        b"   PRT.ERR    " => Some(("device_error", "PRT.ERR")),
        b"              " => Some(("blank", "")),
        _ => None,
    };
    if let Some((status, status_raw)) = state {
        status_id(id)?;
        value["status"] = json!(status);
        value["status_raw"] = json!(status_raw);
        return Ok(value);
    }
    let prefix = match format {
        Format::Sbi16 => &b"   Err "[..],
        Format::Sbi22 => &b"   ERR "[..],
    };
    if let Some(code) = data
        .strip_prefix(prefix)
        .and_then(|tail| tail.strip_suffix(b"    "))
    {
        if code.len() != 3 || !code.iter().all(u8::is_ascii_digit) {
            return Err(malformed("device error requires three decimal digits"));
        }
        status_id(id)?;
        let code = std::str::from_utf8(code).map_err(|_| malformed("error code is not ASCII"))?;
        value["status"] = json!("device_error");
        value["status_raw"] = json!(match format {
            Format::Sbi16 => "Err",
            Format::Sbi22 => "ERR",
        });
        value["error_code_raw"] = json!(code);
        return Ok(value);
    }
    if id.is_some_and(|id| id.trim_matches(' ') == "Stat") {
        return Err(malformed("unqualified Stat reply"));
    }
    let sign = match data.first() {
        Some(b'+') => "+",
        Some(b'-') => "-",
        Some(b' ') => " ",
        _ => return Err(malformed("numeric reply has an invalid polarity")),
    };
    if data.get(10) != Some(&b' ') {
        return Err(malformed(
            "numeric reply is missing the fixed unit separator",
        ));
    }
    let field = std::str::from_utf8(
        data.get(1..10)
            .ok_or_else(|| malformed("missing nine-byte value field"))?,
    )
    .map_err(|_| malformed("value field is not ASCII"))?;
    let unit = std::str::from_utf8(
        data.get(11..14)
            .ok_or_else(|| malformed("missing three-byte unit field"))?,
    )
    .map_err(|_| malformed("unit field is not ASCII"))?;
    let number = decimal(sign == "-", field)?;
    value["status"] = json!("numeric");
    value["sign_raw"] = json!(sign);
    value["value_raw"] = json!(field);
    value["value"] = json!(number);
    value["unit_raw"] = json!(unit);
    let code = unit.trim_matches(' ');
    if !code.is_empty() {
        value["unit"] = json!(code);
    }
    Ok(value)
}

fn status_id(id: Option<&str>) -> Result<()> {
    if id.is_some_and(|id| id != "Stat  ") {
        return Err(malformed("nonnumeric SBI22 reply requires exact Stat ID"));
    }
    Ok(())
}

fn decimal(negative: bool, raw: &str) -> Result<Number> {
    let raw = raw.trim_matches(' ');
    let (whole, fraction) = raw
        .split_once('.')
        .map_or((raw, None), |(whole, fraction)| (whole, Some(fraction)));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|digits| {
            digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(malformed("unqualified SBI decimal value field"));
    }
    let whole = whole.trim_start_matches('0');
    let mut canonical = String::with_capacity(raw.len() + 1);
    if negative {
        canonical.push('-');
    }
    canonical.push_str(if whole.is_empty() { "0" } else { whole });
    if let Some(fraction) = fraction {
        canonical.push('.');
        canonical.push_str(fraction);
    }
    canonical
        .parse()
        .map_err(|_| malformed("invalid exact SBI decimal"))
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("Sartorius SBI: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_numeric_examples_preserve_slots_and_identifier_without_stability_claim() {
        for (frame, format) in [
            (&b"+   123.56 g  \r\n"[..], Format::Sbi16),
            (b"N     +   123.56 g  \r\n", Format::Sbi22),
        ] {
            let value = decode(frame, format).unwrap();
            assert_eq!(value["status"], "numeric");
            assert_eq!(value["value"].to_string(), "123.56");
            assert_eq!(value["value_raw"], "   123.56");
            assert_eq!(value["sign_raw"], "+");
            assert_eq!(value["unit_raw"], "g  ");
            assert_eq!(value["unit"], "g");
            assert!(value.get("stable").is_none());
            assert!(value.get("source_time").is_none());
            assert_eq!(value["raw"], std::str::from_utf8(frame).unwrap());
            if matches!(format, Format::Sbi22) {
                assert_eq!(value["id_raw"], "N     ");
            }
        }
    }

    #[test]
    fn primary_status_and_error_tables_remain_nonnumeric() {
        for (frame, status) in [
            (&b"      High    \r\n"[..], "overload"),
            (b"      Low     \r\n", "underload"),
            (b"   Cal.Ext.   \r\n", "external_calibration"),
            (b"   Err 123    \r\n", "device_error"),
            (b"   APP.ERR    \r\n", "device_error"),
            (b"   DIS.ERR    \r\n", "device_error"),
            (b"   PRT.ERR    \r\n", "device_error"),
        ] {
            let value = decode(frame, Format::Sbi16).unwrap();
            assert_eq!(value["status"], status);
            assert!(value.get("value").is_none());
            assert!(value.get("unit").is_none());
        }
        assert_eq!(
            decode(b"Stat     ERR 123    \r\n", Format::Sbi22).unwrap()["error_code_raw"],
            "123"
        );
        assert_eq!(
            decode(b"Stat        High    \r\n", Format::Sbi22).unwrap()["status"],
            "overload"
        );
    }

    #[test]
    fn synthetic_signed_zero_integer_and_unlabelled_value_keep_exact_number() {
        assert_eq!(
            decode(b"-     0.00 g  \r\n", Format::Sbi16).unwrap()["value"].to_string(),
            "-0.00"
        );
        assert_eq!(
            decode(b"+      123 pcs\r\n", Format::Sbi16).unwrap()["value"],
            json!(123)
        );
        let blank_unit = decode(b"      0.01    \r\n", Format::Sbi16).unwrap();
        assert_eq!(blank_unit["sign_raw"], " ");
        assert_eq!(blank_unit["value"].to_string(), "0.01");
        assert!(blank_unit.get("unit").is_none());
    }

    #[test]
    fn wrong_format_printer_brackets_and_corrupt_fields_fail() {
        for bytes in [
            &b"+   123.56 g  \n"[..],
            b"+   123.56 g  \r\nX",
            b"+   123.56 g  \r\n+   123.56 g  \r\n",
            b"+   12,356 g  \r\n",
            b"+   1.2.3 g  \r\n",
            b"+   123.5[6]g \r\n",
            b"+  NaN    g  \r\n",
            b"   Err 1X3    \r\n",
            b"      High   \r\n",
            b"+   123.56\tg  \r\n",
        ] {
            decode(bytes, Format::Sbi16).expect_err("malformed SBI16 reply accepted");
        }
        decode(b"+   123.56 g  \r\n", Format::Sbi22).unwrap_err();
        decode(b"N           High    \r\n", Format::Sbi22).unwrap_err();
        decode(b"Stat  +   123.56 g  \r\n", Format::Sbi22).unwrap_err();
    }
}
