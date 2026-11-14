//! IEC 62056-21:2002 Mode C optical readout, clauses 5 and 6.
//! <https://webstore.iec.ch/en/publication/6398>

use super::{Source, validate_name};
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeSet;

const BAUDS: [u32; 7] = [300, 600, 1200, 2400, 4800, 9600, 19_200];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    device: String,
    meter_address: Option<String>,
    #[serde(
        default = "default_maximum_baud",
        deserialize_with = "crate::config_number::unsigned"
    )]
    maximum_baud: u32,
    #[serde(default)]
    allow_unchecked: bool,
    signals: Vec<Signal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    obis: String,
    #[serde(default)]
    format: Format,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Format {
    #[default]
    Text,
    Decimal,
}

const fn default_maximum_baud() -> u32 {
    19_200
}

fn identifier(value: &str) -> bool {
    value.len() <= 16
        && value
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte) && !matches!(byte, b'(' | b')' | b'/' | b'!'))
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.device.is_empty() || !BAUDS.contains(&config.maximum_baud) {
        return Err(Usage(
            "IEC 62056-21 needs device and maximum_baud of 300, 600, 1200, 2400, 4800, 9600 or 19200".into(),
        ).into());
    }
    if config.meter_address.as_ref().is_some_and(|address| {
        address.is_empty()
            || address.len() > 32
            || !address
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_alphanumeric())
    }) {
        return Err(Usage(
            "IEC 62056-21 meter_address needs 1–32 letters, digits or spaces".into(),
        )
        .into());
    }
    if !(1..=256).contains(&config.signals.len()) {
        return Err(Usage("IEC 62056-21 needs 1–256 signal mappings".into()).into());
    }
    let mut names = BTreeSet::new();
    let mut codes = BTreeSet::new();
    for signal in &config.signals {
        validate_name(&signal.name, "signal")?;
        if signal.obis.trim().is_empty() || !identifier(&signal.obis) {
            return Err(Usage(
                "IEC 62056-21 obis must be a printable 1–16 character identifier".into(),
            )
            .into());
        }
        if !names.insert(&signal.name) || !codes.insert(&signal.obis) {
            return Err(
                Usage("IEC 62056-21 signal names and OBIS mappings must be unique".into()).into(),
            );
        }
    }
    #[cfg(any(unix, windows))]
    {
        wire::open(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(crate::types::Categorized::error(
            crate::types::ErrorKind::Unsupported,
            "IEC 62056-21 needs a Unix terminal or Windows COM port",
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{BAUDS, Config, Format, Signal, Source, identifier};
    use crate::{
        timeseries::{
            Reading,
            serial::{self, Parity, Port},
        },
        types::{Categorized, ErrorKind},
    };
    use anyhow::Result;
    use serde::Serialize;
    use serde_json::{Number, json};
    use std::{
        collections::BTreeMap,
        io,
        time::{Duration, Instant},
    };

    const START_BAUD: u32 = 300;
    const ACK: u8 = 0x06;
    const STX: u8 = 0x02;
    const ETX: u8 = 0x03;
    const MAX_READOUT: usize = 64 * 1024;
    const MAX_BATCH: usize = 1 << 20;
    const MAX_REACTION: Duration = Duration::from_millis(1500);
    // ACK + 0 + Z + 0 + CR + LF is 60 wire bits at 300 baud.
    const ACK_WIRE_TIME: Duration = Duration::from_millis(200);
    const TX_MARGIN: Duration = Duration::from_millis(5);

    pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
        // A start bit, eight bits with parity in bit 7, and a stop bit are
        // electrically the same as 7E1. Check parity ourselves, including the
        // unchecked readout branch, without OS/driver-specific error markers.
        let port = serial::open(&config.device, START_BAUD, Parity::None, 1, 8)?;
        Ok(Box::new(Meter {
            port,
            baud: START_BAUD,
            maximum_baud: config.maximum_baud,
            request: format!(
                "/?{}!\r\n",
                config.meter_address.as_deref().unwrap_or_default()
            ),
            allow_unchecked: config.allow_unchecked,
            signals: config.signals.clone(),
        }))
    }

    struct Meter {
        port: Port,
        baud: u32,
        maximum_baud: u32,
        request: String,
        allow_unchecked: bool,
        signals: Vec<Signal>,
    }

    impl Source for Meter {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now() + timeout;
            if self.baud != START_BAUD {
                self.port
                    .set_baud(START_BAUD)
                    .map_err(|error| serial_error(&error))?;
                self.baud = START_BAUD;
            }
            self.port
                .discard_input()
                .map_err(|error| serial_error(&error))?;
            send(&mut self.port, self.request.as_bytes(), deadline)?;
            let mut input = Input::new(&mut self.port, deadline);
            let mut line = input.identification_line()?;
            if line == self.request.as_bytes() {
                line = input.identification_line()?;
            }
            if !input.buffered.is_empty() {
                return Err(protocol("unexpected bytes before readout acknowledgement"));
            }
            let identified_at = Instant::now();
            let (id, offered, code, reaction) = identification(&line)?;
            let (baud, code) = if offered <= self.maximum_baud {
                (offered, code)
            } else {
                (START_BAUD, b'0')
            };
            let ack = [ACK, b'0', code, b'0', b'\r', b'\n'];
            let ack_deadline = deadline.min(identified_at + MAX_REACTION);
            wait_until(identified_at + reaction, ack_deadline)?;
            let ack_started = Instant::now();
            send(input.port, &ack, ack_deadline)?;
            if baud != START_BAUD {
                // write_all may have queued the bytes rather than putting them
                // on the wire. Keep the old speed through the ACK's wire time.
                wait_until(ack_started + ACK_WIRE_TIME + TX_MARGIN, deadline)?;
                if !input
                    .port
                    .output_empty()
                    .map_err(|error| serial_error(&error))?
                {
                    return Err(protocol(
                        "ACK still queued after its 300-baud transmit window",
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(timeout_error());
                }
                input
                    .port
                    .set_baud(baud)
                    .map_err(|error| serial_error(&error))?;
                self.baud = baud;
            }
            let (data, checked) = input.readout(ack, self.allow_unchecked)?;
            let readings = readings(&data, id, baud, checked, &self.signals)?;
            if Instant::now() >= deadline {
                return Err(timeout_error());
            }
            Ok(readings)
        }

        fn end_timeout_is_idle(&self) -> bool {
            false
        }
    }

    fn protocol(message: &str) -> anyhow::Error {
        Categorized::error(
            ErrorKind::OperationFailed,
            format!("IEC 62056-21: {message}"),
        )
    }

    fn timeout_error() -> anyhow::Error {
        Categorized::error(ErrorKind::Timeout, "IEC 62056-21 readout deadline expired")
    }

    fn serial_error(error: &io::Error) -> anyhow::Error {
        if error.kind() == io::ErrorKind::TimedOut {
            timeout_error()
        } else {
            protocol(&format!("serial I/O: {error}"))
        }
    }

    fn wait_until(when: Instant, deadline: Instant) -> Result<()> {
        if when >= deadline {
            return Err(timeout_error());
        }
        std::thread::sleep(when.saturating_duration_since(Instant::now()));
        if Instant::now() >= deadline {
            return Err(timeout_error());
        }
        Ok(())
    }

    fn send(port: &mut Port, bytes: &[u8], deadline: Instant) -> Result<()> {
        let even_parity = bytes
            .iter()
            .map(|&byte| {
                if byte.count_ones().is_multiple_of(2) {
                    byte
                } else {
                    byte | 0x80
                }
            })
            .collect::<Vec<_>>();
        port.write_all(&even_parity, deadline)
            .map_err(|error| serial_error(&error))
    }

    fn identification(line: &[u8]) -> Result<(&str, u32, u8, Duration)> {
        let id = line
            .strip_suffix(b"\r\n")
            .ok_or_else(|| protocol("identification requires CRLF"))?;
        let [b'/', a, b, c, code @ b'0'..=b'6', model @ ..] = id else {
            return Err(protocol(
                "expected a Mode C identification with baud code 0–6",
            ));
        };
        if !a.is_ascii_uppercase()
            || !b.is_ascii_uppercase()
            || !c.is_ascii_alphabetic()
            || model.len() > 16
            || !model
                .iter()
                .all(|byte| (b' '..=b'~').contains(byte) && !matches!(byte, b'/' | b'!' | b'\\'))
        {
            return Err(protocol("invalid or unsupported Mode C identification"));
        }
        let baud = BAUDS
            .get(usize::from(*code - b'0'))
            .copied()
            .ok_or_else(|| protocol("unsupported baud code"))?;
        let reaction = Duration::from_millis(if c.is_ascii_lowercase() { 20 } else { 200 });
        Ok((str::from_utf8(id)?, baud, *code, reaction))
    }

    struct Input<'a> {
        port: &'a mut Port,
        deadline: Instant,
        bytes: [u8; 256],
        buffered: std::ops::Range<usize>,
    }

    impl<'a> Input<'a> {
        fn new(port: &'a mut Port, deadline: Instant) -> Self {
            Self {
                port,
                deadline,
                bytes: [0; 256],
                buffered: 0..0,
            }
        }

        fn byte(&mut self) -> Result<u8> {
            if self.buffered.is_empty() {
                let count = self
                    .port
                    .receive(&mut self.bytes, self.deadline)
                    .map_err(|error| serial_error(&error))?
                    .ok_or_else(timeout_error)?;
                if count == 0 {
                    return Err(protocol("serial device disconnected"));
                }
                self.buffered = 0..count;
            }
            let byte = self
                .buffered
                .next()
                .and_then(|index| self.bytes.get(index))
                .copied()
                .ok_or_else(|| protocol("serial input buffer exhausted"))?;
            if !byte.count_ones().is_multiple_of(2) {
                return Err(protocol("received byte with invalid even parity"));
            }
            Ok(byte & 0x7f)
        }

        fn identification_line(&mut self) -> Result<Vec<u8>> {
            // 32-character addressed request plus /?!CRLF, in case of echo.
            let mut line = Vec::with_capacity(37);
            while line.len() < 37 {
                let byte = self.byte()?;
                line.push(byte);
                if byte == b'\n' {
                    return Ok(line);
                }
                if !(b' '..=b'~').contains(&byte) && byte != b'\r' {
                    return Err(protocol("unexpected byte in identification"));
                }
            }
            Err(protocol("identification exceeds its byte limit"))
        }

        fn readout(&mut self, ack: [u8; 6], allow_unchecked: bool) -> Result<(String, bool)> {
            let mut first = self.byte()?;
            if first == ACK {
                // Some half-duplex optical heads return our own transmission.
                for &expected in ack.iter().skip(1) {
                    if self.byte()? != expected {
                        return Err(protocol("unexpected ACK echo"));
                    }
                }
                first = self.byte()?;
            }
            let checked = first == STX;
            if !checked && !allow_unchecked {
                return Err(protocol("readout requires STX and a valid BCC"));
            }
            if checked {
                first = self.byte()?;
            }
            let mut block = Vec::new();
            let mut byte = first;
            while byte != b'!' {
                if block.len() >= MAX_READOUT {
                    return Err(protocol("readout exceeds 64 KiB"));
                }
                if !(b' '..=b'~').contains(&byte) && !matches!(byte, b'\r' | b'\n') {
                    return Err(protocol("unexpected control byte in readout"));
                }
                block.push(byte);
                byte = self.byte()?;
            }
            if self.byte()? != b'\r' || self.byte()? != b'\n' {
                return Err(protocol("readout terminator must be !CRLF"));
            }
            if checked {
                if self.byte()? != ETX {
                    return Err(protocol("checked readout requires ETX before BCC"));
                }
                let bcc = block
                    .iter()
                    .fold(b'!' ^ b'\r' ^ b'\n' ^ ETX, |sum, byte| sum ^ byte);
                if self.byte()? != bcc {
                    return Err(protocol("readout BCC mismatch"));
                }
            }
            if !self.buffered.is_empty() {
                return Err(protocol("unexpected bytes after readout"));
            }
            Ok((String::from_utf8(block)?, checked))
        }
    }

    #[derive(Serialize)]
    struct Value<'a> {
        raw: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<Number>,
    }

    fn readings(
        data: &str,
        id: &str,
        baud: u32,
        checked: bool,
        signals: &[Signal],
    ) -> Result<Vec<Reading>> {
        let mut selected = signals
            .iter()
            .map(|signal| (signal.obis.as_str(), (signal, Vec::new())))
            .collect::<BTreeMap<_, _>>();
        for line in data.split("\r\n") {
            let mut rest = line;
            let mut obis = "";
            while !rest.is_empty() {
                let (code, field) = rest
                    .split_once('(')
                    .ok_or_else(|| protocol("data set has no opening parenthesis"))?;
                let (field, tail) = field
                    .split_once(')')
                    .ok_or_else(|| protocol("data set has no closing parenthesis"))?;
                if !identifier(code) {
                    return Err(protocol("invalid data set identifier"));
                }
                let explicit = !code.is_empty();
                if explicit {
                    obis = code;
                }
                rest = tail;
                let (raw, unit) = field
                    .split_once('*')
                    .map_or((field, None), |(raw, unit)| (raw, Some(unit)));
                if raw.len() > 32
                    || raw.contains(['(', '/', '\r', '\n'])
                    || unit.is_some_and(|unit| unit.is_empty() || !identifier(unit))
                {
                    return Err(protocol("invalid data set value or unit"));
                }
                if let Some((signal, values)) = selected.get_mut(obis) {
                    let number =
                        match (signal.format, explicit) {
                            (Format::Decimal, true) => Some(decimal(raw).ok_or_else(|| {
                                protocol(&format!("OBIS {obis} is not a decimal"))
                            })?),
                            _ => None,
                        };
                    values.push(Value { raw, unit, number });
                }
            }
        }
        let mut batch_size = 2;
        let mut readings = Vec::with_capacity(signals.len());
        for signal in signals {
            let (_, values) = selected
                .remove(signal.obis.as_str())
                .ok_or_else(|| protocol("missing signal mapping"))?;
            if values.is_empty() {
                return Err(protocol(&format!(
                    "OBIS {} is absent from readout",
                    signal.obis
                )));
            }
            let reading = Reading {
                signal: signal.name.clone(),
                value: json!({
                    "obis": signal.obis, "identification": id, "baud": baud,
                    "block_check": if checked { "verified" } else { "unchecked" },
                    "values": values,
                }),
                source_time: None,
            };
            batch_size += usize::from(!readings.is_empty()) + serde_json::to_vec(&reading)?.len();
            if batch_size > MAX_BATCH {
                return Err(protocol("serialized batch exceeds 1 MiB"));
            }
            readings.push(reading);
        }
        Ok(readings)
    }

    fn decimal(raw: &str) -> Option<Number> {
        let (sign, digits) = raw.strip_prefix('-').map_or_else(
            || ("", raw.strip_prefix('+').unwrap_or(raw)),
            |digits| ("-", digits),
        );
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        if (whole.is_empty() && fraction.is_empty())
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let whole = whole.trim_start_matches('0');
        let whole = if whole.is_empty() { "0" } else { whole };
        if fraction.is_empty() {
            format!("{sign}{whole}").parse().ok()
        } else {
            format!("{sign}{whole}.{fraction}").parse().ok()
        }
    }
}
