//! Omron CompoWay/F Variable Area Read for E5CC/E5EC, G3ZA and G3PW controllers.
//! <https://files.omron.eu/downloads/latest/manual/en/h175_e5_c_communications_manual_en.pdf?v=1>
//! <https://files.omron.eu/downloads/latest/manual/en/z200_g3za_users_manual_en.pdf?v=1>
//! <https://files.omron.eu/downloads/latest/manual/en/z280_g3pw_power_controllers_users_manual_en.pdf?v=1>

use super::{
    Source,
    serial::{self, Parity, SerialSignal},
    validate_name,
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeSet;

const MAX_ELEMENTS: usize = 8;
const MAX_UNIT: u8 = 99;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    device: String,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    unit: u8,
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
    signals: Vec<SerialSignal<Signal>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    variable_type: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    address: u16,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    elements: u8,
    data_type: DataType,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DataType {
    U16,
    I16,
    U32,
    I32,
}

impl DataType {
    const fn digits(self) -> usize {
        match self {
            Self::U16 | Self::I16 => 4,
            Self::U32 | Self::I32 => 8,
        }
    }
}

const fn one() -> u8 {
    1
}

const fn default_baud() -> u32 {
    9600
}

const fn default_stop_bits() -> u8 {
    2
}

const fn default_data_bits() -> u8 {
    7
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.device.is_empty()
        || config.unit > MAX_UNIT
        || !(1..=256).contains(&config.signals.len())
    {
        return Err(Usage("CompoWay/F needs a device, unit 0–99 and 1–256 signals".into()).into());
    }
    if !matches!(config.baud, 9600 | 19_200 | 38_400 | 57_600) || !matches!(config.data_bits, 7 | 8)
    {
        return Err(Usage(
            "CompoWay/F needs baud 9600, 19200, 38400 or 57600 and data_bits 7 or 8".into(),
        )
        .into());
    }
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    let mut names = BTreeSet::new();
    for entry in &config.signals {
        let signal = &entry.signal;
        validate_name(&signal.name, "CompoWay/F signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("CompoWay/F signal names must be unique".into()).into());
        }
        if entry.unit(config.unit) > MAX_UNIT {
            return Err(Usage(format!(
                "CompoWay/F signal {} unit must be 0–99",
                signal.name
            ))
            .into());
        }
        if !matches!(
            (signal.variable_type, signal.data_type.digits()),
            (0x80..=0x8f, 4) | (0xc0..=0xcf, 8)
        ) {
            return Err(Usage(
                "CompoWay/F requires variable_type 0x80–0x8F for u16/i16 or 0xC0–0xCF for u32/i32"
                    .into(),
            )
            .into());
        }
        if !(1..=MAX_ELEMENTS).contains(&usize::from(signal.elements))
            || u32::from(signal.address) + u32::from(signal.elements) > u32::from(u16::MAX) + 1
        {
            return Err(
                Usage("CompoWay/F needs 1–8 elements within addresses 0–65535".into()).into(),
            );
        }
    }
    #[cfg(any(unix, windows))]
    {
        wire::open(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "CompoWay/F ({:?} parity) requires a Unix terminal or Windows COM port",
                config.parity
            ),
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{Categorized, Config, DataType, ErrorKind, MAX_ELEMENTS, Signal, Source, serial};
    use crate::timeseries::Reading;
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::{
        io,
        time::{Duration, Instant},
    };

    const STX: u8 = 2;
    const ETX: u8 = 3;
    // STX + unit/sub-address/end code + command/status + up to eight
    // double-word values + ETX + the raw checksum byte.
    const MAX_FRAME: usize = 17 + 8 * MAX_ELEMENTS;

    pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
        let port = serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            config.data_bits,
        )?;
        Ok(Box::new(Client {
            port,
            config: config.clone(),
        }))
    }

    struct Client {
        port: serial::Port,
        config: Config,
    }

    impl Source for Client {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now() + timeout;
            self.port
                .discard_input()
                .map_err(|error| io_error(&error))?;
            // Signal names and at most eight fixed-width integers per signal
            // keep all 256 readings below the shared 1 MiB batch limit.
            let mut readings = Vec::with_capacity(self.config.signals.len());
            let mut buffer = [0; MAX_FRAME];
            for entry in &self.config.signals {
                let request = request(entry.unit(self.config.unit), &entry.signal);
                self.port
                    .write_all(&request, deadline)
                    .map_err(|error| io_error(&error))?;
                let body = read_frame(&mut self.port, deadline, &mut buffer)?;
                let value = decode(&entry.signal, &request[1..5], body)?;
                if Instant::now() >= deadline {
                    return Err(timed_out());
                }
                readings.push(Reading {
                    signal: entry.signal.name.clone(),
                    value,
                    source_time: None,
                });
            }
            Ok(readings)
        }

        fn end_timeout_is_idle(&self) -> bool {
            false
        }
    }

    fn protocol(message: &str) -> anyhow::Error {
        Categorized::error(ErrorKind::OperationFailed, format!("CompoWay/F: {message}"))
    }

    fn timed_out() -> anyhow::Error {
        Categorized::error(ErrorKind::Timeout, "CompoWay/F poll deadline expired")
    }

    fn io_error(error: &io::Error) -> anyhow::Error {
        let kind = match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Unavailable,
        };
        Categorized::error(kind, format!("CompoWay/F serial I/O: {error}"))
    }

    fn bcc(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0, |sum, byte| sum ^ byte)
    }

    fn request(unit: u8, signal: &Signal) -> Vec<u8> {
        // Sub-address 00, SID 0, Read Variable Area 0101, and bit position 00.
        let mut frame = format!(
            "\x02{unit:02}0000101{:02X}{:04X}00{:04X}\x03",
            signal.variable_type, signal.address, signal.elements
        )
        .into_bytes();
        frame.push(bcc(&frame[1..]));
        frame
    }

    fn read_frame<'a>(
        port: &mut serial::Port,
        deadline: Instant,
        bytes: &'a mut [u8; MAX_FRAME],
    ) -> Result<&'a [u8]> {
        let mut used = 0;
        while used < bytes.len() {
            let count = port
                .receive(&mut bytes[used..], deadline)
                .map_err(|error| io_error(&error))?
                .ok_or_else(timed_out)?;
            if count == 0 {
                return Err(Categorized::error(
                    ErrorKind::Unavailable,
                    "CompoWay/F serial device disconnected",
                ));
            }
            used += count;
            let frame = &bytes[..used];
            if frame[0] != STX {
                return Err(protocol("response requires STX"));
            }
            if let Some(etx) = frame.iter().position(|&byte| byte == ETX) {
                if used == etx + 1 {
                    continue;
                }
                if used != etx + 2 {
                    return Err(protocol("unexpected bytes after response BCC"));
                }
                if frame[etx + 1] != bcc(&frame[1..=etx]) {
                    return Err(protocol("response BCC mismatch"));
                }
                return Ok(&bytes[1..etx]);
            }
        }
        Err(protocol(
            "response exceeds the frame bound or lacks ETX/BCC",
        ))
    }

    fn hex(digits: &[u8]) -> Result<u32> {
        digits.iter().try_fold(0_u32, |value, &byte| {
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(protocol("response contains a non-hex digit")),
            };
            Ok((value << 4) | u32::from(digit))
        })
    }

    fn status(pdu: &[u8]) -> Result<u32> {
        let header = pdu
            .first_chunk::<8>()
            .ok_or_else(|| protocol("truncated FINS-mini response"))?;
        if header[..4] != *b"0101" {
            return Err(protocol("unexpected FINS-mini response service"));
        }
        hex(&header[4..])
    }

    fn rejected(signal: &Signal, end_code: u32, status: Option<u32>) -> anyhow::Error {
        let mut details = json!({"signal":signal.name, "end_code":end_code});
        if let Some(status) = status {
            details["response_code"] = json!(status);
        }
        Categorized::new(
            ErrorKind::OperationFailed,
            "CompoWay/F controller rejected Variable Area Read",
        )
        .details(details)
        .into()
    }

    fn decode(signal: &Signal, target: &[u8], body: &[u8]) -> Result<Value> {
        let head = body
            .first_chunk::<6>()
            .ok_or_else(|| protocol("truncated response header"))?;
        if &head[..4] != target {
            return Err(protocol("unexpected unit or sub-address"));
        }
        let end_code = hex(&head[4..])?;
        let pdu = &body[head.len()..];
        if end_code != 0 {
            return Err(rejected(signal, end_code, status(pdu).ok()));
        }
        let response_code = status(pdu)?;
        if response_code != 0 {
            return Err(rejected(signal, end_code, Some(response_code)));
        }
        let digits = signal.data_type.digits();
        let payload = &pdu[8..];
        if payload.len() != usize::from(signal.elements) * digits {
            return Err(protocol(
                "response length differs from the requested variable count",
            ));
        }
        if signal.elements == 1 {
            value(signal.data_type, payload)
        } else {
            let values = payload
                .chunks_exact(digits)
                .map(|digits| value(signal.data_type, digits))
                .collect::<Result<Vec<_>>>()?;
            Ok(Value::Array(values))
        }
    }

    fn value(data_type: DataType, digits: &[u8]) -> Result<Value> {
        let bits = hex(digits)?;
        Ok(match data_type {
            DataType::U16 => json!(u16::try_from(bits)?),
            DataType::I16 => json!(i16::from_be_bytes(u16::try_from(bits)?.to_be_bytes())),
            DataType::U32 => json!(bits),
            DataType::I32 => json!(i32::from_be_bytes(bits.to_be_bytes())),
        })
    }
}
