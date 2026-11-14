//! SDI-12 M/MC measurement reads over the Infwin SDI12ELF20 USB COM interface.
//! The bridge handles the bus break, marking and 1200 7E1 wire.
//! <https://www.infwin.com/wp-content/uploads/SDI12ELF20-SDI-12-to-USB-Converter-User-Manual.pdf>
//! SDI-12 v1.4 §§4.4.5, 4.4.8–9 and 4.4.12 specify the transactions.
//! <https://www.sdi-12.org/specification.php?file_id=1>

use super::Source;
use crate::types::Usage;
use anyhow::Result;
use serde::{Deserialize, de::Error as _};
use std::collections::BTreeSet;

const MAX_VALUES: usize = 9;
const MAX_ADDRESSES: usize = 62;

#[derive(Debug, Clone)]
pub(crate) struct Config {
    device: String,
    sensors: Vec<SensorConfig>,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SensorList {
            sensors: Vec<SensorConfig>,
        }

        let mut value = crate::session::strict_json_value(deserializer)?;
        let fields = value
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("SDI-12 source must be an object"))?;
        let device = fields
            .remove("device")
            .ok_or_else(|| D::Error::missing_field("device"))?;
        let device = serde_json::from_value(device).map_err(D::Error::custom)?;
        // The strict JSON value keeps exact numeric spellings in nested maps.
        let sensors = if fields.contains_key("sensors") {
            serde_json::from_value::<SensorList>(value).map(|list| list.sensors)
        } else {
            serde_json::from_value::<SensorConfig>(value).map(|sensor| vec![sensor])
        }
        .map_err(D::Error::custom)?;
        Ok(Self { device, sensors })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SensorConfig {
    #[serde(deserialize_with = "deserialize_address")]
    address: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    measurement: u8,
    #[serde(default)]
    crc: bool,
    signals: Vec<Signal>,
}

fn deserialize_address<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u8, D::Error> {
    let address = String::deserialize(deserializer)?;
    match address.as_bytes() {
        [address] if address.is_ascii_alphanumeric() => Ok(*address),
        _ => Err(D::Error::custom(
            "SDI-12 address must be one ASCII letter or digit",
        )),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    index: usize,
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.device.is_empty() || !(1..=MAX_ADDRESSES).contains(&config.sensors.len()) {
        return Err(
            Usage("SDI-12 needs one bridge device and 1–62 configured sensors".into()).into(),
        );
    }
    let mut names = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    for sensor in &config.sensors {
        if !addresses.insert(sensor.address) {
            return Err(Usage("SDI-12 sensor addresses must be unique on a bridge".into()).into());
        }
        if sensor.measurement > 9 || !(1..=MAX_VALUES).contains(&sensor.signals.len()) {
            return Err(
                Usage("SDI-12 each sensor needs measurement 0–9 and 1–9 signals".into()).into(),
            );
        }
        let mut indices = BTreeSet::new();
        for signal in &sensor.signals {
            super::validate_name(&signal.name, "SDI-12 signal")?;
            if signal.index >= MAX_VALUES {
                return Err(Usage("SDI-12 signal index must be 0–8".into()).into());
            }
            if !names.insert(&signal.name) || !indices.insert(signal.index) {
                return Err(Usage(
                    "SDI-12 signal names must be unique on a bridge and indices unique per sensor"
                        .into(),
                )
                .into());
            }
        }
    }
    #[cfg(any(unix, windows))]
    {
        wire::open(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        for sensor in &config.sensors {
            let _ = sensor.crc;
        }
        Err(crate::types::Categorized::error(
            crate::types::ErrorKind::Unsupported,
            "SDI-12 needs a Unix terminal or Windows COM port",
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{Config, SensorConfig, Signal, Source};
    use crate::{
        timeseries::{
            Reading,
            serial::{self, Parity, Port},
        },
        types::{Categorized, ErrorKind, Usage},
    };
    use anyhow::{Context as _, Result};
    use serde_json::{Number, json};
    use std::{
        io,
        time::{Duration, Instant},
    };

    const MAX_VALUE_CHARS: usize = 35;
    const MAX_LINE: usize = 1 + MAX_VALUE_CHARS + 3 + 2;

    pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
        let sensors = config.sensors.iter().map(Sensor::new).collect();
        Ok(Box::new(Bus {
            wire: Wire {
                port: serial::open(&config.device, 9600, Parity::None, 1, 8)?,
                pending: Vec::new(),
            },
            sensors,
        }))
    }

    struct Bus {
        wire: Wire,
        sensors: Vec<Sensor>,
    }

    struct Sensor {
        address: u8,
        crc: bool,
        command: Vec<u8>,
        signals: Vec<Signal>,
    }

    impl Sensor {
        fn new(config: &SensorConfig) -> Self {
            let mut command = vec![config.address, b'M'];
            if config.crc {
                command.push(b'C');
            }
            if config.measurement != 0 {
                command.push(b'0' + config.measurement);
            }
            command.push(b'!');
            Self {
                address: config.address,
                crc: config.crc,
                command,
                signals: config.signals.clone(),
            }
        }
    }

    struct Wire {
        port: Port,
        pending: Vec<u8>,
    }

    impl Source for Bus {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("SDI-12 timeout is too large".into()))?;
            self.wire.pending.clear();
            self.wire
                .port
                .discard_input()
                .map_err(|error| io_error(&error))?;
            let mut readings = Vec::new();
            for sensor in &self.sensors {
                let values = self
                    .wire
                    .poll(sensor, deadline)
                    .with_context(|| format!("SDI-12 address {}", char::from(sensor.address)))?;
                for signal in &sensor.signals {
                    let value = values.get(signal.index).ok_or_else(|| {
                        malformed("configured signal is outside the measured value count")
                    })?;
                    readings.push(Reading {
                        signal: signal.name.clone(),
                        value: json!({"raw":value.raw,"value":value.number}),
                        source_time: None,
                    });
                }
            }
            remaining(deadline)?;
            Ok(readings)
        }
    }

    impl Wire {
        fn poll(&mut self, sensor: &Sensor, deadline: Instant) -> Result<Vec<Measurement>> {
            self.port
                .write_all(&sensor.command, deadline)
                .map_err(|error| io_error(&error))?;
            let announced = self.required_line(deadline)?;
            let (delay, count) = measurement_response(&announced, sensor.address)?;
            if sensor.signals.iter().any(|signal| signal.index >= count) {
                return Err(malformed(
                    "configured signal is outside the announced value count",
                ));
            }
            let mut allow_late_service = false;
            if delay != 0 {
                let ready_at = Instant::now() + Duration::from_secs(u64::from(delay));
                match self.line(ready_at.min(deadline))? {
                    Some(service) if service == [sensor.address] => {}
                    Some(_) => return Err(malformed("invalid measurement service request")),
                    None => allow_late_service = true,
                }
            }

            let mut values = Vec::with_capacity(count);
            for packet in b'0'..=b'9' {
                remaining(deadline)?;
                let command = [sensor.address, b'D', packet, b'!'];
                self.port
                    .write_all(&command, deadline)
                    .map_err(|error| io_error(&error))?;
                let mut response = self.required_line(deadline)?;
                if allow_late_service && response == [sensor.address] {
                    // A service request can cross the ttt boundary on the USB
                    // link. It can occur only once, before the first data reply.
                    response = self.required_line(deadline)?;
                }
                allow_late_service = false;
                let data = data_response(&response, sensor.address, sensor.crc)?;
                append_values(data, &mut values, count)?;
                if values.len() == count {
                    if !self.pending.is_empty() {
                        return Err(malformed("extra bytes after the final data response"));
                    }
                    remaining(deadline)?;
                    return Ok(values);
                }
            }
            Err(malformed(
                "data responses did not supply the announced count",
            ))
        }

        fn required_line(&mut self, deadline: Instant) -> Result<Vec<u8>> {
            self.line(deadline)?.ok_or_else(timeout)
        }

        fn line(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>> {
            loop {
                if let Some(newline) = self.pending.iter().position(|&byte| byte == b'\n') {
                    if newline + 1 > MAX_LINE {
                        return Err(malformed("serial response exceeds its length limit"));
                    }
                    let Some(line) = self.pending[..=newline].strip_suffix(b"\r\n") else {
                        return Err(malformed("serial response must end with CRLF"));
                    };
                    let line = line.to_vec();
                    self.pending.drain(..=newline);
                    return Ok(Some(line));
                }
                if self.pending.len() >= MAX_LINE {
                    return Err(malformed("serial response exceeds its length limit"));
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                let mut bytes = [0; 64];
                match self.port.receive(&mut bytes, deadline) {
                    Ok(None) => {}
                    Ok(Some(0)) => {
                        return Err(Categorized::error(
                            ErrorKind::Disconnected,
                            "SDI-12 bridge closed",
                        ));
                    }
                    Ok(Some(count)) => self.pending.extend_from_slice(&bytes[..count]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => return Err(io_error(&error)),
                }
            }
        }
    }

    fn measurement_response(line: &[u8], address: u8) -> Result<(u16, usize)> {
        let [received, first, second, third, count] = line else {
            return Err(malformed("measurement acknowledgement must be atttn"));
        };
        if *received != address
            || ![first, second, third, count]
                .into_iter()
                .all(u8::is_ascii_digit)
        {
            return Err(malformed(
                "measurement acknowledgement address or count differs",
            ));
        }
        let delay =
            u16::from(first - b'0') * 100 + u16::from(second - b'0') * 10 + u16::from(third - b'0');
        let count = usize::from(count - b'0');
        if count == 0 {
            return Err(malformed("sensor announced no measurement values"));
        }
        Ok((delay, count))
    }

    fn data_response(line: &[u8], address: u8, check_crc: bool) -> Result<&[u8]> {
        if line.first() != Some(&address) {
            return Err(malformed("data response address differs from request"));
        }
        let payload = if check_crc {
            let Some((body, checksum)) = line.split_last_chunk::<3>() else {
                return Err(malformed("missing data response CRC"));
            };
            if checksum != &serial::crc16_ascii(body) {
                return Err(malformed("data response CRC mismatch"));
            }
            body
        } else {
            line
        };
        let data = payload
            .get(1..)
            .filter(|data| !data.is_empty() && data.len() <= MAX_VALUE_CHARS)
            .ok_or_else(|| malformed("empty, aborted or oversized data response"))?;
        Ok(data)
    }

    struct Measurement {
        raw: String,
        number: Number,
    }

    fn append_values(mut data: &[u8], values: &mut Vec<Measurement>, count: usize) -> Result<()> {
        while let Some((&sign, tail)) = data.split_first() {
            if !matches!(sign, b'+' | b'-') {
                return Err(malformed("measurement values must carry a sign"));
            }
            let end = tail
                .iter()
                .position(|&byte| matches!(byte, b'+' | b'-'))
                .map_or(data.len(), |offset| offset + 1);
            let (raw, rest) = data.split_at(end);
            if values.len() == count {
                return Err(malformed("data response exceeds the announced value count"));
            }
            let raw =
                std::str::from_utf8(raw).map_err(|_| malformed("measurement is not ASCII"))?;
            let number =
                decimal(raw).ok_or_else(|| malformed("invalid signed measurement decimal"))?;
            values.push(Measurement {
                raw: raw.into(),
                number,
            });
            data = rest;
        }
        Ok(())
    }

    fn decimal(raw: &str) -> Option<Number> {
        let (sign, digits) = raw.strip_prefix('-').map_or_else(
            || ("", raw.strip_prefix('+').unwrap_or(raw)),
            |digits| ("-", digits),
        );
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        if !(1..=7).contains(&(whole.len() + fraction.len()))
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

    fn malformed(message: &str) -> anyhow::Error {
        Categorized::error(ErrorKind::OperationFailed, format!("SDI-12: {message}"))
    }

    fn timeout() -> anyhow::Error {
        Categorized::error(ErrorKind::Timeout, "SDI-12 poll deadline expired")
    }

    fn remaining(deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        Ok(())
    }

    fn io_error(error: &io::Error) -> anyhow::Error {
        let kind = match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Disconnected,
        };
        Categorized::error(kind, format!("SDI-12 serial I/O: {error}"))
    }
}
