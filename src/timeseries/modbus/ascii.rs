//! Read-only serial Modbus ASCII framing, per Serial Line Guide V1.02 §2.5.2.
//! <https://www.modbus.org/file/secure/modbusoverserial.pdf>

use super::{
    Parity, Signal, Source, default_stop_bits, default_unit, validate, validate_serial_units,
};
use crate::{
    timeseries::serial::{self, SerialSignal},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    device: String,
    #[serde(
        default = "default_baud",
        deserialize_with = "crate::config_number::unsigned"
    )]
    baud: u32,
    #[serde(default)]
    parity: Parity,
    #[serde(
        default = "default_stop_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    stop_bits: u8,
    #[serde(
        default = "default_data_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    data_bits: u8,
    #[serde(
        default = "default_unit",
        deserialize_with = "crate::config_number::unsigned"
    )]
    unit: u8,
    signals: Vec<SerialSignal<Signal>>,
}

const fn default_baud() -> u32 {
    9600
}

const fn default_data_bits() -> u8 {
    7
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    validate(config.signals.iter().map(|entry| &entry.signal))?;
    validate_serial_units(config.unit, &config.signals, "ASCII")?;
    if config.device.is_empty() || !matches!(config.data_bits, 7 | 8) {
        return Err(Usage("Modbus ASCII needs device and data_bits 7 or 8".into()).into());
    }
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    #[cfg(any(unix, windows))]
    {
        wire::open(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "Modbus ASCII ({:?} parity) requires a Unix terminal or Windows COM port",
                config.parity
            ),
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{Categorized, Config, ErrorKind, SerialSignal, Signal, Source, serial};
    use crate::timeseries::Reading;
    use crate::timeseries::modbus::{BatchSize, io_error, protocol, remaining};
    use anyhow::Result;
    use std::time::{Duration, Instant};

    // Address + up to 253 PDU bytes + LRC, then encode each byte as two
    // hex digits and add the delimiters.
    const MAX_BODY: usize = 255;
    const MAX_FRAME: usize = 2 * MAX_BODY + 3;

    pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
        let port = serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            config.data_bits,
        )?;
        Ok(Box::new(Ascii {
            port,
            unit: config.unit,
            signals: config.signals.clone(),
        }))
    }

    struct Ascii {
        port: serial::Port,
        unit: u8,
        signals: Vec<SerialSignal<Signal>>,
    }

    impl Source for Ascii {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now() + timeout;
            self.port
                .discard_input()
                .map_err(|error| io_error(&error))?;
            let mut readings = Vec::with_capacity(self.signals.len());
            let mut size = BatchSize(2);
            let mut body = [0; MAX_BODY];
            for entry in &self.signals {
                let unit = entry.unit(self.unit);
                let frame = request_frame(unit, entry.signal.request()?);
                self.port
                    .write_all(&frame, deadline)
                    .map_err(|error| io_error(&error))?;
                let response = read_frame(&mut self.port, deadline, &mut body)?;
                if response[0] != unit {
                    return Err(protocol("unexpected unit in ASCII response"));
                }
                let reading = entry.signal.decode(&response[1..])?;
                size.charge(&reading, !readings.is_empty())?;
                remaining(deadline)?;
                readings.push(reading);
            }
            Ok(readings)
        }
    }

    fn lrc(bytes: &[u8]) -> u8 {
        bytes.iter().copied().fold(0_u8, u8::wrapping_sub)
    }

    fn request_frame(unit: u8, pdu: [u8; 5]) -> [u8; 17] {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut bytes = [0; 7];
        bytes[0] = unit;
        bytes[1..6].copy_from_slice(&pdu);
        bytes[6] = lrc(&bytes[..6]);
        let mut frame = *b":00000000000000\r\n";
        let (digits, _) = frame[1..15].as_chunks_mut::<2>();
        for (pair, byte) in digits.iter_mut().zip(bytes) {
            *pair = [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0xf)]];
        }
        frame
    }

    fn read_frame<'a>(
        port: &mut serial::Port,
        deadline: Instant,
        body: &'a mut [u8; MAX_BODY],
    ) -> Result<&'a [u8]> {
        let mut frame = [0; MAX_FRAME];
        let mut used = 0;
        let mut skipped = 0;
        let mut input = [0; MAX_FRAME];
        loop {
            let count = port
                .receive(&mut input, deadline)
                .map_err(|error| io_error(&error))?
                .ok_or_else(|| {
                    Categorized::error(ErrorKind::Timeout, "Modbus ASCII response timed out")
                })?;
            if count == 0 {
                return Err(Categorized::error(
                    ErrorKind::Unavailable,
                    "Modbus serial device disconnected",
                ));
            }
            for &byte in &input[..count] {
                // A new start delimiter abandons any incomplete frame.
                if byte == b':' {
                    skipped += used;
                    used = 0;
                } else if used == 0 {
                    skipped += 1;
                }
                if skipped > MAX_FRAME {
                    return Err(protocol("ASCII response is missing its start delimiter"));
                }
                if used == 0 && byte != b':' {
                    continue;
                }
                frame[used] = byte;
                used += 1;
                if byte == b'\n' {
                    return decode_frame(&frame[..used], body);
                }
                if used == MAX_FRAME {
                    return Err(protocol(
                        "ASCII response exceeds 513 characters or lacks CRLF",
                    ));
                }
            }
        }
    }

    fn decode_frame<'a>(frame: &[u8], body: &'a mut [u8; MAX_BODY]) -> Result<&'a [u8]> {
        let digits = frame
            .strip_prefix(b":")
            .and_then(|frame| frame.strip_suffix(b"\r\n"))
            .ok_or_else(|| protocol("ASCII response must end with CRLF"))?;
        let (pairs, tail) = digits.as_chunks::<2>();
        if !(3..=MAX_BODY).contains(&pairs.len()) || !tail.is_empty() {
            return Err(protocol("invalid ASCII response length"));
        }
        for (byte, &[high, low]) in body.iter_mut().zip(pairs) {
            *byte = (hex_digit(high)? << 4) | hex_digit(low)?;
        }
        let (checksum, response) = body[..pairs.len()].split_last().expect("validated length");
        if *checksum != lrc(response) {
            return Err(protocol("ASCII response LRC mismatch"));
        }
        Ok(response)
    }

    fn hex_digit(byte: u8) -> Result<u8> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(protocol("invalid hex digit in ASCII response")),
        }
    }
}
