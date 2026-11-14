//! Read-only XGB dedicated Cnet serial transactions with mandatory BCC.
//!
//! XBL-C21A/C41A manual V2.0, chapter 7: rSS reads explicit native
//! X/B/W/D/L addresses. The configured PLC owns address/range semantics.
use super::{
    Source,
    serial::{self, Parity, SerialSignal},
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeSet;

mod parser;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    profile: Profile,
    device: String,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
    #[serde(default = "parity")]
    parity: Parity,
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
    #[serde(default = "unit", deserialize_with = "crate::config_number::unsigned")]
    unit: u8,
    signals: Vec<SerialSignal<Signal>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    XgbDedicated,
}

const fn baud() -> u32 {
    9600
}
const fn parity() -> Parity {
    Parity::None
}
const fn data_bits() -> u8 {
    8
}
const fn stop_bits() -> u8 {
    1
}
const fn unit() -> u8 {
    1
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    variable: Option<String>,
    variables: Option<Vec<String>>,
    #[serde(default)]
    data_type: parser::DataType,
}

impl Signal {
    fn plan(&self) -> Result<parser::Plan> {
        match (&self.variable, &self.variables) {
            (Some(variable), None) => {
                parser::Plan::new(std::slice::from_ref(variable), self.data_type, false)
            }
            (None, Some(variables)) => parser::Plan::new(variables, self.data_type, true),
            _ => Err(Usage(
                "LS Cnet signal requires exactly one variable or variables list".into(),
            )
            .into()),
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    let Profile::XgbDedicated = config.profile;
    if config.device.is_empty()
        || config.device.contains('\0')
        || !matches!(
            config.baud,
            1200 | 2400 | 4800 | 9600 | 19_200 | 38_400 | 57_600 | 115_200
        )
        || !matches!(config.data_bits, 7 | 8)
        || !(1..=256).contains(&config.signals.len())
    {
        return Err(Usage("LS Cnet requires a device, 1–256 signals, 1200/2400/4800/9600/19200/38400/57600/115200 baud and 7/8 data bits".into()).into());
    }
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    let mut names = BTreeSet::new();
    for entry in &config.signals {
        let signal = &entry.signal;
        super::validate_name(&signal.name, "LS Cnet signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("LS Cnet signal names must be unique".into()).into());
        }
        signal.plan()?;
    }
    #[cfg(any(unix, windows))]
    {
        Ok(Box::new(wire::Client::new(config.clone())))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "LS Cnet requires a Unix terminal or Windows COM port",
        ))
    }
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("LS Cnet: {message}"))
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn lazy_owner_zero_deadline_retires_before_open_or_request() {
        let config: Config=serde_json::from_value(serde_json::json!({"profile":"xgb_dedicated","device":"/not-a-real-cnet-terminal","signals":[{"name":"word","variable":"%MW0"}]})).unwrap();
        let mut source = open(&config).unwrap();
        let error = source.read(Duration::ZERO).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{Config, Source, Usage, malformed, parser, serial};
    use crate::{
        timeseries::{Reading, slmp::SizeLimit},
        types::{Categorized, ErrorKind},
    };
    use anyhow::Result;
    use std::{
        io::{self, Write as _},
        time::{Duration, Instant},
    };

    pub(super) struct Client {
        config: Config,
        port: Option<serial::Port>,
        retired: bool,
    }
    impl Client {
        pub(super) fn new(config: Config) -> Self {
            Self {
                config,
                port: None,
                retired: false,
            }
        }

        fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
            check_deadline(deadline)?;
            if self.port.is_none() {
                self.port = Some(serial::open(
                    &self.config.device,
                    self.config.baud,
                    self.config.parity,
                    self.config.stop_bits,
                    self.config.data_bits,
                )?);
                check_deadline(deadline)?;
            }
            let port = self
                .port
                .as_mut()
                .ok_or_else(|| malformed("serial owner unavailable"))?;
            let mut readings = Vec::with_capacity(self.config.signals.len());
            let mut size = SizeLimit { bytes: 2 };
            for entry in &self.config.signals {
                let signal = &entry.signal;
                let plan = signal.plan()?;
                let station = entry.unit(self.config.unit);
                let request = plan.request(station);
                check_deadline(deadline)?;
                port.write_all(&request, deadline)
                    .map_err(|error| io_error(&error))?;
                let value = read_reply(port, &plan, station, deadline)?;
                check_deadline(deadline)?;
                let reading = Reading {
                    signal: signal.name.clone(),
                    value,
                    source_time: None,
                };
                serde_json::to_writer(&mut size, &reading)
                    .map_err(|_| malformed("serialized poll exceeds 1 MiB"))?;
                size.write_all(b",")
                    .map_err(|_| malformed("serialized poll exceeds 1 MiB"))?;
                readings.push(reading);
            }
            check_deadline(deadline)?;
            Ok(readings)
        }
    }
    impl Source for Client {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            if self.retired {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "LS Cnet serial owner retired",
                ));
            }
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("LS Cnet timeout is too large".into()))?;
            let result = self.poll(deadline);
            if result.is_err() {
                self.port = None;
                self.retired = true;
            }
            result
        }
    }

    fn read_reply(
        port: &mut serial::Port,
        plan: &parser::Plan,
        station: u8,
        deadline: Instant,
    ) -> Result<serde_json::Value> {
        // One extra byte catches an overlong/coalesced read at the 256-byte bound.
        let mut frame = [0; 257];
        let mut length = 0;
        loop {
            check_deadline(deadline)?;
            let buffer = frame
                .get_mut(length..)
                .ok_or_else(|| malformed("reply buffer exhausted"))?;
            let count = match port.receive(buffer, deadline) {
                Ok(None) => {
                    return Err(Categorized::error(
                        ErrorKind::Timeout,
                        "LS Cnet reply deadline expired",
                    ));
                }
                Ok(Some(0)) => {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "LS Cnet peer closed during reply",
                    ));
                }
                Ok(Some(count)) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error(&error)),
            };
            check_deadline(deadline)?;
            length = length
                .checked_add(count)
                .filter(|&length| length <= 256)
                .ok_or_else(|| malformed("reply exceeds 256 bytes"))?;
            let bytes = frame
                .get(..length)
                .ok_or_else(|| malformed("invalid reply extent"))?;
            let expected = match bytes.first() {
                Some(6) => plan.reply_bytes(),
                Some(0x15) => 13,
                _ => return Err(malformed("expected ACK or NAK at start of reply")),
            };
            if length > expected {
                return Err(malformed("overlong or coalesced reply"));
            }
            if length == expected {
                let value = plan.decode(station, bytes)?;
                check_deadline(deadline)?;
                return Ok(value);
            }
        }
    }
    fn check_deadline(deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "LS Cnet transaction deadline expired",
            ));
        }
        Ok(())
    }
    fn io_error(error: &io::Error) -> anyhow::Error {
        let kind = match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Disconnected,
        };
        Categorized::error(kind, format!("LS Cnet serial I/O: {error}"))
    }
}
