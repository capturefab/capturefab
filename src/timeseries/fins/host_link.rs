//! Omron read-only serial Host Link. W342 sections 2-2, 3-5-3 and 4-3-7
//! define the FCS, direct FA envelopes and single-frame C-mode RD.
//! <https://assets.omron.eu/downloads/latest/manual/en/w342_cs_cj_cp_nsj_communications_commands_reference_manual_en.pdf>

use super::{Area, Signal, validate_signals};
use crate::{
    timeseries::{
        Source,
        serial::{self, SerialSignal},
    },
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use serde::{Deserialize, Deserializer, de::Error as _};

mod cpm;

#[derive(Debug, Clone)]
pub(crate) enum Config {
    Legacy(LegacyConfig),
    Cpm(cpm::Config),
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = crate::session::strict_json_value(deserializer)?;
        if value.get("mode").and_then(serde_json::Value::as_str) == Some("c_mode_cpm") {
            serde_json::from_value(value)
                .map(Self::Cpm)
                .map_err(D::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::Legacy)
                .map_err(D::Error::custom)
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    match config {
        Config::Legacy(config) => open_legacy(config),
        Config::Cpm(config) => cpm::open(config),
    }
}

const MAX_RD_WORDS: u16 = 30;
const MAX_FA_DATA: usize = 538;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyConfig {
    pub device: String,
    pub mode: Mode,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub unit: u8,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    pub baud: u32,
    #[serde(default)]
    pub parity: serial::Parity,
    #[serde(
        default = "stop_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub stop_bits: u8,
    #[serde(
        default = "data_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub data_bits: u8,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub response_wait_ms: Option<u8>,
    pub signals: Vec<SerialSignal<Signal>>,
}

const fn baud() -> u32 {
    9_600
}

const fn stop_bits() -> u8 {
    2
}

const fn data_bits() -> u8 {
    7
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    CModeRd,
    FinsDirect,
}

fn open_legacy(config: &LegacyConfig) -> Result<Box<dyn Source>> {
    if config.device.is_empty() || config.unit > 31 || !(1..=256).contains(&config.signals.len()) {
        return Err(Usage(
            "Omron Host Link requires a device, unit 0–31, and 1–256 signals".into(),
        )
        .into());
    }
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    if !matches!(config.data_bits, 7 | 8) {
        return Err(Usage("Omron Host Link data_bits must be 7 or 8".into()).into());
    }
    if let Some(wait) = config.response_wait_ms
        && (!matches!(config.mode, Mode::FinsDirect) || wait > 150 || !wait.is_multiple_of(10))
    {
        return Err(Usage(
            "Omron Host Link response_wait_ms applies to fins_direct and must be 0–150 in steps of 10"
                .into(),
        )
        .into());
    }
    validate_signals(config.signals.iter().map(|entry| &entry.signal))?;
    for entry in &config.signals {
        let signal = &entry.signal;
        if entry.unit(config.unit) > 31 {
            return Err(Usage(format!(
                "Omron Host Link signal {} unit must be 0–31",
                signal.name
            ))
            .into());
        }
        match config.mode {
            Mode::CModeRd => {
                let words = signal.count()?;
                if !matches!(signal.area, Area::Dm)
                    || signal.bits()
                    || words > MAX_RD_WORDS
                    || u32::from(signal.address) + u32::from(words) > 10_000
                {
                    return Err(Usage(
                        "Omron Host Link c_mode_rd requires 1–30 words within D0–D9999 per signal"
                            .into(),
                    )
                    .into());
                }
            }
            Mode::FinsDirect => {
                if signal.bytes()? > MAX_FA_DATA {
                    return Err(Usage(
                        "Omron Host Link fins_direct supports up to 269 words or 538 bits per signal"
                            .into(),
                    )
                    .into());
                }
            }
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
                "Omron Host Link ({:?} parity) requires a Unix terminal or Windows COM port",
                config.parity
            ),
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::super::{
        Signal, io_error, memory_command, memory_result, poll_deadline, read_poll, remaining,
    };
    use super::{Categorized, ErrorKind, LegacyConfig as Config, Mode, Source, serial};
    use crate::timeseries::Reading;
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::{
        fmt::Write as _,
        time::{Duration, Instant},
    };

    const RD_FRAME: usize = 131;
    const FA_FRAME: usize = 1_115;

    pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
        let port = serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            config.data_bits,
        )?;
        port.discard_input().map_err(|error| io_error(&error))?;
        Ok(Box::new(Client {
            port,
            config: config.clone(),
            sid: 0,
        }))
    }

    struct Client {
        port: serial::Port,
        config: Config,
        sid: u8,
    }

    impl Source for Client {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = poll_deadline(timeout)?;
            let Self { port, config, sid } = self;
            let signals = config
                .signals
                .iter()
                .map(|entry| (&entry.signal, entry.unit(config.unit)));
            let readings = read_poll(signals, |signal, unit| {
                let value = read_signal(port, config, signal, unit, *sid, deadline)?;
                *sid = sid.wrapping_add(1);
                Ok(value)
            })?;
            remaining(deadline)?;
            Ok(readings)
        }
    }

    fn read_signal(
        port: &mut serial::Port,
        config: &Config,
        signal: &Signal,
        unit: u8,
        sid: u8,
        deadline: Instant,
    ) -> Result<Value> {
        let (command, limit) = match config.mode {
            Mode::CModeRd => ("RD", RD_FRAME),
            Mode::FinsDirect => ("FA", FA_FRAME),
        };
        let header = format!("@{unit:02}{command}");
        let mut request = header.clone();
        match config.mode {
            Mode::CModeRd => write!(request, "{:04}{:04}", signal.address, signal.count()?)?,
            Mode::FinsDirect => {
                // Direct FINS uses the shortened ICF/DA2/SA2/SID header.
                let wait = config.response_wait_ms.unwrap_or_default() / 10;
                write!(request, "{wait:X}000000{sid:02X}")?;
                for byte in memory_command(signal)? {
                    write!(request, "{byte:02X}")?;
                }
            }
        }
        let fcs = checksum(request.as_bytes());
        write!(request, "{fcs:02X}*\r")?;
        port.write_all(request.as_bytes(), deadline)
            .map_err(|error| io_error(&error))?;

        let mut frame = [0; FA_FRAME];
        let response = receive_frame(port, &mut frame[..limit], deadline)?;
        let payload = response_payload(response, header.as_bytes())?;
        let data = decode_hex(payload)?;
        match config.mode {
            Mode::CModeRd => signal.decode(&data),
            Mode::FinsDirect => {
                let Some((fields, data)) = data.split_first_chunk::<6>() else {
                    return Err(protocol("truncated direct FINS reply"));
                };
                let [icf, routing_and_command @ ..] = *fields;
                // W342 section 3-3-3 reserves ICF bits 1–5 for the device.
                if icf & 0xc1 != 0x40 || routing_and_command != [0, 0, sid, 1, 1] {
                    return Err(protocol("unexpected direct FINS header, SID or command"));
                }
                signal.decode(memory_result(data)?)
            }
        }
    }

    fn receive_frame<'a>(
        port: &mut serial::Port,
        buffer: &'a mut [u8],
        deadline: Instant,
    ) -> Result<&'a [u8]> {
        let mut length = 0;
        while length < buffer.len() {
            let count = match port
                .receive(&mut buffer[length..], deadline)
                .map_err(|error| io_error(&error))?
            {
                None => {
                    return Err(Categorized::error(
                        ErrorKind::Timeout,
                        "Omron Host Link response deadline expired",
                    ));
                }
                Some(0) => {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "Omron Host Link serial device disconnected",
                    ));
                }
                Some(count) => count,
            };
            let end = length + count;
            if let Some(delimiter) = buffer[length..end].iter().position(|&byte| byte == b'\r') {
                if delimiter + 1 != count {
                    return Err(protocol("unexpected bytes after response terminator"));
                }
                return Ok(&buffer[..end]);
            }
            length = end;
        }
        Err(protocol("response exceeds frame limit"))
    }

    fn response_payload<'a>(frame: &'a [u8], header: &[u8]) -> Result<&'a [u8]> {
        let frame = frame
            .strip_suffix(b"*\r")
            .ok_or_else(|| protocol("expected a single frame ending in *CR"))?;
        let Some((body, &fcs)) = frame.split_last_chunk::<2>() else {
            return Err(protocol("truncated response FCS"));
        };
        if hex_byte(fcs)? != checksum(body) {
            return Err(protocol("response FCS mismatch"));
        }
        let payload = body
            .strip_prefix(header)
            .ok_or_else(|| protocol("unexpected response unit or command"))?;
        let Some((status, data)) = payload.split_first_chunk::<2>() else {
            return Err(protocol("truncated response end code"));
        };
        let status = hex_byte(*status)?;
        if status != 0 {
            return Err(Categorized::new(
                ErrorKind::OperationFailed,
                "Omron Host Link target rejected request",
            )
            .details(json!({"host_link_end_code":status}))
            .into());
        }
        Ok(data)
    }

    fn checksum(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0, |fcs, &byte| fcs ^ byte)
    }

    fn decode_hex(bytes: &[u8]) -> Result<Vec<u8>> {
        let (pairs, remainder) = bytes.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(protocol("odd-length hexadecimal response"));
        }
        pairs.iter().copied().map(hex_byte).collect()
    }

    fn hex_byte([high, low]: [u8; 2]) -> Result<u8> {
        let digit = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let (Some(high), Some(low)) = (digit(high), digit(low)) else {
            return Err(protocol("invalid hexadecimal byte"));
        };
        Ok((high << 4) | low)
    }

    fn protocol(message: &str) -> anyhow::Error {
        Categorized::error(
            ErrorKind::OperationFailed,
            format!("Omron Host Link: {message}"),
        )
    }
}
