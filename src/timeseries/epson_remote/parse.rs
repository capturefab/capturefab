//! Exact finite native reply grammars; values are not converted from SPEL+ types.
use super::{Resource, Selection, Terminator, incomplete};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{display::Base64Display, engine::general_purpose::STANDARD};
use serde::{Serialize, Serializer};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub(super) struct Base64<'a>(pub(super) &'a [u8]);
impl Serialize for Base64<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&Base64Display::new(self.0, &STANDARD))
    }
}
pub(super) fn payload<'a>(
    raw: &'a [u8],
    terminator: Terminator,
    method: &'static str,
) -> Result<&'a str> {
    let raw = raw
        .strip_suffix(terminator.bytes())
        .ok_or_else(|| incomplete("missing selected reply terminator"))?;
    if !raw.iter().all(|byte| (b' '..=b'~').contains(byte)) {
        return Err(incomplete(
            "reply contains non-ASCII or framing control bytes",
        ));
    }
    let text = str::from_utf8(raw).map_err(|_| incomplete("reply is not ASCII"))?;
    let (name, value) = text
        .split_once(',')
        .ok_or_else(|| incomplete("reply has no native field separator"))?;
    let (marker, received) = name
        .split_at_checked(1)
        .ok_or_else(|| incomplete("empty reply method"))?;
    if received != method {
        return Err(incomplete(
            "reply method does not match its outstanding request",
        ));
    }
    match marker {
        "#" => Ok(value),
        "!" => {
            let code = decimal(value).ok_or_else(|| incomplete("invalid native error code"))?;
            let kind = match code {
                11 => ErrorKind::AuthRequired,
                13 | 98 => ErrorKind::AuthRejected,
                19 => ErrorKind::Timeout,
                _ => ErrorKind::OperationFailed,
            };
            let details = if method != "Login" || matches!(code, 10..=15 | 19..=21 | 98 | 99) {
                serde_json::json!({"method":method,"native_error_code":code})
            } else {
                // Private Login text can be reflected credentials, including
                // secrets too short for global report scrubbing. Do not echo it.
                serde_json::json!({"method":method,"native_error_code_status":"unqualified_omitted"})
            };
            Err(
                Categorized::new(kind, "Epson rejected the selected monitor command")
                    .details(details)
                    .into(),
            )
        }
        _ => Err(incomplete("unknown reply marker")),
    }
}
fn decimal(value: &str) -> Option<u32> {
    (!value.is_empty() && value.len() <= 10 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Native<'a> {
    Status {
        flags_raw: &'a str,
        flag_width: usize,
        flag_mapping: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        named_flags: Option<BTreeMap<&'static str, bool>>,
        code_text: &'a str,
    },
    Io {
        text: &'a str,
        value: u16,
        bits: u8,
    },
    Alarms {
        count: usize,
        alarm_codes_text: Vec<&'a str>,
    },
    CurrentRobot {
        number_text: &'a str,
        number: u32,
    },
    Variable {
        values_text: Vec<&'a str>,
        count: usize,
        datatype_provenance: &'static str,
    },
}
pub(super) fn native<'a>(selection: &Selection, value: &'a str) -> Result<Native<'a>> {
    if let Some(bits) = selection.resource.io_bits() {
        let width = match bits {
            1 => 1,
            8 => 2,
            _ => 4,
        };
        if value.len() != width
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            || (bits == 1 && value != "0" && value != "1")
        {
            return Err(incomplete("I/O reply has an invalid native bit/hex width"));
        }
        return Ok(Native::Io {
            text: value,
            value: u16::from_str_radix(value, 16)
                .map_err(|_| incomplete("invalid native I/O value"))?,
            bits,
        });
    }
    match selection.resource {
        Resource::Status => {
            let (flags, code) = value
                .split_once(',')
                .ok_or_else(|| incomplete("status requires flags and a four-digit code"))?;
            if !matches!(flags.len(), 10 | 11)
                || !flags.bytes().all(|byte| matches!(byte, b'0' | b'1'))
                || code.len() != 4
                || !code.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(incomplete("invalid status flag token or four-digit code"));
            }
            let named_flags = (flags.len() == 11).then(|| {
                [
                    "test",
                    "teach",
                    "auto",
                    "warning",
                    "serror",
                    "safeguard",
                    "estop",
                    "error",
                    "paused",
                    "running",
                    "ready",
                ]
                .into_iter()
                .zip(flags.bytes().map(|byte| byte == b'1'))
                .collect()
            });
            Ok(Native::Status {
                flags_raw: flags,
                flag_width: flags.len(),
                flag_mapping: if named_flags.is_some() {
                    "documented_11_flags"
                } else {
                    "unqualified_documented_width"
                },
                named_flags,
                code_text: code,
            })
        }
        Resource::Alarms => {
            let mut fields = value.split(',');
            let count = fields
                .next()
                .and_then(decimal)
                .ok_or_else(|| incomplete("invalid alarm count"))?;
            if count > 1024 {
                return Err(incomplete(
                    "alarm count exceeds the 1024-field capture bound",
                ));
            }
            let codes: Vec<_> = fields.take(1025).collect();
            if codes.len() != usize::try_from(count)?
                || codes.iter().any(|code| decimal(code).is_none())
            {
                return Err(incomplete("alarm reply count or decimal fields disagree"));
            }
            Ok(Native::Alarms {
                count: codes.len(),
                alarm_codes_text: codes,
            })
        }
        Resource::CurrentRobot => Ok(Native::CurrentRobot {
            number_text: value,
            number: decimal(value)
                .ok_or_else(|| incomplete("current robot is not an unsigned decimal number"))?,
        }),
        Resource::Variable => {
            let count = usize::from(selection.count.unwrap_or(1));
            let values_text: Vec<_> = value.split(',').take(count + 1).collect();
            if values_text.len() != count || values_text.iter().any(|text| text.is_empty()) {
                return Err(incomplete(
                    "variable reply has empty fields or a mismatched element count",
                ));
            }
            Ok(Native::Variable {
                values_text,
                count,
                datatype_provenance: "configured_no_numeric_conversion",
            })
        }
        Resource::Io
        | Resource::IoByte
        | Resource::IoWord
        | Resource::MemoryIo
        | Resource::MemoryIoByte
        | Resource::MemoryIoWord => Err(incomplete("invalid I/O resource")),
    }
}
