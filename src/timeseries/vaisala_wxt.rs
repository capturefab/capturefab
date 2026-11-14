//! Vaisala `WXT530` series ASCII composite measurement polling (`R0` / `r0`).
//! The `M211840EN-G` guide defines framing, field suffixes, and the CRC.
use super::{Source, serial};
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    device: String,
    #[serde(default = "address")]
    address: String,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
    #[serde(default = "parity")]
    parity: serial::Parity,
    #[serde(
        default = "data_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    data_bits: u8,
    #[serde(
        default = "stop_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    stop_bits: u8,
    #[serde(default)]
    crc: bool,
    signals: Vec<Signal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    field: String,
}

fn address() -> String {
    "0".into()
}
const fn baud() -> u32 {
    19_200
}
const fn parity() -> serial::Parity {
    serial::Parity::None
}
const fn data_bits() -> u8 {
    8
}
const fn stop_bits() -> u8 {
    1
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    let [address] = config.address.as_bytes() else {
        return Err(Usage("WXT address must be one ASCII letter or digit".into()).into());
    };
    if !address.is_ascii_alphanumeric() {
        return Err(Usage("WXT address must be one ASCII letter or digit".into()).into());
    }
    if config.device.is_empty()
        || config.baud < 1200
        || !matches!(config.data_bits, 7 | 8)
        || !(1..=32).contains(&config.signals.len())
    {
        return Err(Usage(
            "WXT requires a device, baud of at least 1200, 7 or 8 data bits and 1–32 signals"
                .into(),
        )
        .into());
    }
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    let mut names = BTreeSet::new();
    let mut fields = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "WXT signal")?;
        if suffixes(&signal.field).is_none() {
            return Err(Usage(format!("unknown WXT measurement field: {}", signal.field)).into());
        }
        if !names.insert(&signal.name) || !fields.insert(&signal.field) {
            return Err(Usage("WXT signal names and fields must be unique".into()).into());
        }
    }
    #[cfg(any(unix, windows))]
    {
        wire::open(config, *address)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config.crc, config.parity);
        Err(crate::types::Categorized::error(
            crate::types::ErrorKind::Unsupported,
            "WXT requires a Unix terminal or Windows COM port",
        ))
    }
}

// The Vaisala table uses uppercase S for duration and its wire examples use s.
fn suffixes(field: &str) -> Option<&'static [u8]> {
    Some(match field {
        "Sn" | "Sm" | "Sx" => b"MKSN",
        "Dn" | "Dm" | "Dx" => b"D",
        "Pa" => b"HPBMI",
        "Ta" | "Tp" | "Th" | "Tr" => b"CF",
        "Ua" => b"P",
        "Rc" | "Ri" | "Rp" => b"MI",
        "Rd" | "Hd" => b"Ss",
        "Hc" | "Hi" | "Hp" => b"MIH",
        "Vh" => b"NVWF",
        "Vs" | "Vr" | "Sl" | "Sr" => b"V",
        "Ra" => b"M",
        "Rt" => b"R",
        "Id" => b"",
        _ => return None,
    })
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{Config, Signal, Source, serial, suffixes};
    use crate::{
        timeseries::Reading,
        types::{Categorized, ErrorKind, Usage},
    };
    use anyhow::Result;
    use serde_json::{Number, Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        io,
        time::{Duration, Instant},
    };

    const MAX_REPLY: usize = 512;

    pub(super) fn open(config: &Config, address: u8) -> Result<Box<dyn Source>> {
        let header = [address, if config.crc { b'r' } else { b'R' }, b'0'];
        let mut request = header.to_vec();
        if config.crc {
            request.extend_from_slice(&serial::crc16_ascii(&header));
        }
        request.extend_from_slice(b"\r\n");
        Ok(Box::new(Client {
            port: serial::open(
                &config.device,
                config.baud,
                config.parity,
                config.stop_bits,
                config.data_bits,
            )?,
            header,
            request,
            crc: config.crc,
            signals: config.signals.clone(),
        }))
    }

    struct Client {
        port: serial::Port,
        header: [u8; 3],
        request: Vec<u8>,
        crc: bool,
        signals: Vec<Signal>,
    }

    impl Source for Client {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("WXT timeout is too large".into()))?;
            self.port
                .write_all(&self.request, deadline)
                .map_err(|error| io_error(&error))?;
            let mut buffer = [0; MAX_REPLY + 1];
            let length = read_line(&mut self.port, &mut buffer, deadline)?;
            let readings = decode(&buffer[..length], self.header, self.crc, &self.signals)?;
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            Ok(readings)
        }
    }

    fn read_line(port: &mut serial::Port, buffer: &mut [u8], deadline: Instant) -> Result<usize> {
        let mut length = 0;
        while length < MAX_REPLY {
            match port.receive(&mut buffer[length..], deadline) {
                Ok(Some(0)) => {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "WXT serial device closed",
                    ));
                }
                Ok(Some(count)) => length += count,
                Ok(None) => return Err(timed_out()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error(&error)),
            }
            let frame = &buffer[..length];
            if let Some(end) = frame.iter().position(|&byte| byte == b'\n') {
                if end + 1 != length {
                    return Err(malformed("extra bytes after composite reply"));
                }
                if length <= MAX_REPLY {
                    return frame
                        .strip_suffix(b"\r\n")
                        .map(<[u8]>::len)
                        .ok_or_else(|| malformed("composite reply must end with CRLF"));
                }
            }
        }
        Err(malformed("composite reply exceeds its length limit"))
    }

    fn decode(line: &[u8], header: [u8; 3], crc: bool, signals: &[Signal]) -> Result<Vec<Reading>> {
        let body = if crc {
            let Some((body, checksum)) = line.split_last_chunk::<3>() else {
                return Err(malformed("missing composite reply CRC"));
            };
            if checksum != &serial::crc16_ascii(body) {
                return Err(malformed("composite reply CRC mismatch"));
            }
            body
        } else {
            line
        };
        let data = body
            .strip_prefix(header.as_slice())
            .and_then(|tail| tail.strip_prefix(b","))
            .ok_or_else(|| malformed("unexpected composite reply address or command"))?;
        if !data
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
        {
            return Err(malformed("composite reply must contain printable ASCII"));
        }
        let data = std::str::from_utf8(data)
            .map_err(|_| malformed("composite reply must contain printable ASCII"))?;
        let mut fields = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for pair in data.split(',') {
            let (field, raw) = pair
                .trim_start_matches(' ')
                .split_once('=')
                .ok_or_else(|| malformed("malformed composite field"))?;
            let [upper, lower] = field.as_bytes() else {
                return Err(malformed("invalid composite field code"));
            };
            if !upper.is_ascii_uppercase() || !lower.is_ascii_lowercase() {
                return Err(malformed("invalid composite field code"));
            }
            if !seen.insert(field) {
                return Err(malformed("duplicate composite field code"));
            }
            if let Some(codes) = suffixes(field) {
                fields.insert(field, decode_field(field, raw, codes)?);
            }
        }
        signals
            .iter()
            .map(|signal| {
                let value = fields.remove(signal.field.as_str()).ok_or_else(|| {
                    malformed(&format!("missing configured field {}", signal.field))
                })?;
                Ok(Reading {
                    signal: signal.name.clone(),
                    value,
                    source_time: None,
                })
            })
            .collect()
    }

    fn decode_field(field: &str, raw: &str, codes: &[u8]) -> Result<Value> {
        if field == "Id" {
            return Ok(json!({"field":field,"raw":raw,"value":raw}));
        }
        let Some((&code, digits)) = raw.as_bytes().split_last() else {
            return Err(malformed("missing composite field suffix"));
        };
        if code == b'#' {
            if !digits.is_empty() {
                decimal(digits)?;
            }
            return Ok(json!({"field":field,"raw":raw,"status":"invalid"}));
        }
        if !codes.contains(&code) {
            return Err(malformed(&format!("unexpected suffix for {field}")));
        }
        let value = decimal(digits)?;
        let code = char::from(code);
        if field == "Vh" {
            return Ok(
                json!({"field":field,"raw":raw,"value":value,"unit_code":"V","status_code":code}),
            );
        }
        Ok(json!({"field":field,"raw":raw,"value":value,"unit_code":code}))
    }

    fn decimal(bytes: &[u8]) -> Result<Number> {
        let invalid = || malformed("invalid composite field decimal");
        let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
        let (sign, digits) = text.strip_prefix('-').map_or_else(
            || ("", text.strip_prefix('+').unwrap_or(text)),
            |digits| ("-", digits),
        );
        let (whole, fraction) = digits
            .split_once('.')
            .map_or((digits, None), |(whole, fraction)| (whole, Some(fraction)));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|fraction| {
                fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(invalid());
        }
        let whole = whole.trim_start_matches('0');
        let whole = if whole.is_empty() { "0" } else { whole };
        let number = fraction.map_or_else(
            || format!("{sign}{whole}"),
            |fraction| format!("{sign}{whole}.{fraction}"),
        );
        number.parse().map_err(|_| invalid())
    }

    fn malformed(message: &str) -> anyhow::Error {
        Categorized::error(ErrorKind::OperationFailed, format!("WXT: {message}"))
    }

    fn timed_out() -> anyhow::Error {
        Categorized::error(ErrorKind::Timeout, "WXT poll deadline expired")
    }

    fn io_error(error: &io::Error) -> anyhow::Error {
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => timed_out(),
            kind => {
                let kind = if kind == io::ErrorKind::PermissionDenied {
                    ErrorKind::PermissionDenied
                } else {
                    ErrorKind::Disconnected
                };
                Categorized::error(kind, format!("WXT serial I/O: {error}"))
            }
        }
    }
}
