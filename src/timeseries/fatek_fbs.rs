//! Read-only FATEK FBs Port0 command 44/46 transactions.
//!
//! The externally configured interface owns baud, cabling and PLC memory ranges.
use super::{Reading, Source, net, serial};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

mod parser;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    profile: Profile,
    address: Option<SocketAddr>,
    serial: Option<SerialConfig>,
    #[serde(default = "unit", deserialize_with = "crate::config_number::unsigned")]
    unit: u8,
    signals: Vec<serial::SerialSignal<Signal>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    FbsPort0,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SerialConfig {
    device: String,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
    #[serde(default)]
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
}
const fn unit() -> u8 {
    1
}
const fn baud() -> u32 {
    9600
}
const fn data_bits() -> u8 {
    7
}
const fn stop_bits() -> u8 {
    1
}
const fn elements() -> u16 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    address: String,
    #[serde(default)]
    data_type: parser::DataType,
    #[serde(
        default = "elements",
        deserialize_with = "crate::config_number::unsigned"
    )]
    elements: u16,
}

struct Planned {
    name: String,
    unit: u8,
    plan: parser::Plan,
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    let Profile::FbsPort0 = config.profile;
    if !(1..=254).contains(&config.unit) || !(1..=256).contains(&config.signals.len()) {
        return Err(Usage("FATEK requires unicast unit 1–254 and 1–256 signals".into()).into());
    }
    let connection = match (config.address, config.serial.as_ref()) {
        (Some(address), None)
            if address.port() != 0
                && !address.ip().is_unspecified()
                && !address.ip().is_multicast() =>
        {
            Connection::Tcp { address, socket: None }
        }
        (None, Some(config)) => {
            if config.device.is_empty()
                || config.device.contains('\0')
                || !matches!(config.baud, 9600 | 19_200 | 38_400 | 57_600 | 115_200)
                || !matches!(config.parity, serial::Parity::Even)
                || config.data_bits != 7
                || config.stop_bits != 1
            {
                return Err(Usage("FATEK FBs Port0 requires a nonempty serial device, 9600/19200/38400/57600/115200 baud and fixed 7E1; use the documented RS232 cable and preconfigured no-flow interface".into()).into());
            }
            serial::validate(config.baud, config.stop_bits, config.data_bits)?;
            Connection::Serial {
                config: config.clone(),
                #[cfg(any(unix, windows))]
                port: None,
            }
        }
        _ => return Err(Usage("FATEK requires one serial Port0 device or one numeric, nonzero-port transparent TCP bridge address".into()).into()),
    };
    let mut names = BTreeSet::new();
    let mut signals = Vec::with_capacity(config.signals.len());
    for entry in &config.signals {
        let signal = &entry.signal;
        super::validate_name(&signal.name, "FATEK signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("FATEK signal names must be unique".into()).into());
        }
        let unit = entry.unit(config.unit);
        if !(1..=254).contains(&unit) {
            return Err(Usage("FATEK signal unit must be unicast 1–254".into()).into());
        }
        signals.push(Planned {
            name: signal.name.clone(),
            unit,
            plan: parser::Plan::new(&signal.address, signal.data_type, signal.elements)?,
        });
    }
    Ok(Box::new(Client {
        connection: Some(connection),
        signals,
    }))
}

enum Connection {
    Tcp {
        address: SocketAddr,
        socket: Option<Box<dyn net::Transport>>,
    },
    Serial {
        config: SerialConfig,
        #[cfg(any(unix, windows))]
        port: Option<serial::Port>,
    },
}
impl Connection {
    fn exchange(&mut self, planned: &Planned, deadline: Instant) -> Result<Value> {
        remaining(deadline)?;
        let request = planned.plan.request(planned.unit);
        match self {
            Self::Tcp { address, socket } => {
                if socket.is_none() {
                    let stream = TcpStream::connect_timeout(address, remaining(deadline)?)
                        .map_err(|error| io_error(&error))?;
                    stream.set_nodelay(true).map_err(|error| io_error(&error))?;
                    remaining(deadline)?;
                    *socket = Some(net::plain(stream, deadline));
                }
                let stream = socket
                    .as_deref_mut()
                    .ok_or_else(|| malformed("TCP owner unavailable"))?;
                stream.set_deadline(deadline);
                remaining(deadline)?;
                stream
                    .write_all(&request)
                    .map_err(|error| io_error(&error))?;
                read_reply(|bytes| stream.read(bytes), planned, deadline)
            }
            Self::Serial {
                config,
                #[cfg(any(unix, windows))]
                port,
            } => {
                #[cfg(any(unix, windows))]
                {
                    if port.is_none() {
                        *port = Some(serial::open(
                            &config.device,
                            config.baud,
                            config.parity,
                            config.stop_bits,
                            config.data_bits,
                        )?);
                        remaining(deadline)?;
                    }
                    let port = port
                        .as_mut()
                        .ok_or_else(|| malformed("serial owner unavailable"))?;
                    remaining(deadline)?;
                    port.write_all(&request, deadline)
                        .map_err(|error| io_error(&error))?;
                    read_reply(
                        |bytes| {
                            port.receive(bytes, deadline)?
                                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
                        },
                        planned,
                        deadline,
                    )
                }
                #[cfg(not(any(unix, windows)))]
                {
                    let _ = config;
                    Err(Categorized::error(
                        ErrorKind::Unsupported,
                        "FATEK serial requires a Unix terminal or Windows COM port",
                    ))
                }
            }
        }
    }
}

struct Client {
    connection: Option<Connection>,
    signals: Vec<Planned>,
}
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        remaining(deadline)?;
        let connection = self.connection.as_mut().ok_or_else(|| {
            Categorized::error(ErrorKind::Disconnected, "FATEK command channel retired")
        })?;
        let mut readings = Vec::with_capacity(self.signals.len());
        let mut size = super::slmp::SizeLimit { bytes: 2 };
        for planned in &self.signals {
            let value = connection.exchange(planned, deadline)?;
            remaining(deadline)?;
            let reading = Reading {
                signal: planned.name.clone(),
                value,
                source_time: None,
            };
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| malformed("serialized poll exceeds 1 MiB"))?;
            size.write_all(b",")
                .map_err(|_| malformed("serialized poll exceeds 1 MiB"))?;
            readings.push(reading);
        }
        remaining(deadline)?;
        Ok(readings)
    }
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.connection.is_none() {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "FATEK command channel retired",
            ));
        }
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("FATEK timeout is too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        if result.is_err() {
            // No transaction ID/address echo permits safe recovery. Drop the
            // exact owner after any failed poll; never retry or silently reopen.
            self.connection = None;
        }
        result
    }
}

fn read_reply(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    planned: &Planned,
    deadline: Instant,
) -> Result<Value> {
    // Selected 44/46 replies can reach 265 bytes. The extra byte catches a
    // visible overlong/coalesced receive without a protocol-wide 256B assumption.
    let mut frame = [0; parser::MAX_REPLY + 1];
    let mut length = 0;
    loop {
        remaining(deadline)?;
        let count = match read(
            frame
                .get_mut(length..)
                .ok_or_else(|| malformed("reply buffer exhausted"))?,
        ) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "FATEK peer closed during reply",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        remaining(deadline)?;
        length = length
            .checked_add(count)
            .filter(|&size| size <= parser::MAX_REPLY)
            .ok_or_else(|| malformed("reply exceeds the selected 265-byte limit"))?;
        let buffered = frame
            .get(..length)
            .ok_or_else(|| malformed("invalid reply extent"))?;
        if buffered.first() != Some(&2) {
            return Err(malformed("reply requires STX"));
        }
        let Some(&error) = buffered.get(5) else {
            continue;
        };
        let expected = if error == b'0' {
            planned.plan.reply_bytes()
        } else {
            9
        };
        if length > expected {
            return Err(malformed("overlong or coalesced reply"));
        }
        if let Some(end) = buffered.iter().position(|byte| *byte == 3)
            && end + 1 != expected
        {
            return Err(malformed("ETX precedes the requested reply extent"));
        }
        if length == expected {
            let value = planned.plan.decode(planned.unit, buffered)?;
            remaining(deadline)?;
            return Ok(value);
        }
    }
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "FATEK poll deadline expired",
        ));
    }
    Ok(left)
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("FATEK: {message}"))
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("FATEK I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    fn planned() -> Planned {
        Planned {
            name: "word".into(),
            unit: 1,
            plan: parser::Plan::new("R00012", parser::DataType::U16, 3).unwrap(),
        }
    }
    #[test]
    fn fixed_buffer_handles_one_byte_progress_and_rejects_visible_extra_bytes() {
        let planned = planned();
        let mut input = &b"\x020146010A57FC4000189\x03"[..];
        let value = read_reply(
            |buffer| {
                let byte = buffer
                    .get_mut(..1)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                input.read(byte)
            },
            &planned,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(value, json!([4261, 32708, 1]));
        for raw in [
            &b"\x020146010A57FC4000189\x03X"[..],
            b"\x020146A0E\x03X",
            b"\x020146010A5\x03",
        ] {
            let mut input = raw;
            let error = read_reply(
                |buffer| input.read(buffer),
                &planned,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                ErrorKind::OperationFailed
            );
        }
    }
    #[test]
    fn zero_budget_and_failed_lazy_open_permanently_retire_the_owner() {
        let planned = planned();
        let error = read_reply(
            |_| panic!("expired read attempted I/O"),
            &planned,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let error = read_reply(
            |buffer| {
                std::thread::sleep(Duration::from_millis(10));
                let raw = b"\x020146010A57FC4000189\x03";
                buffer
                    .get_mut(..raw.len())
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
                    .copy_from_slice(raw);
                Ok(raw.len())
            },
            &planned,
            Instant::now() + Duration::from_millis(1),
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let config: Config = serde_json::from_value(json!({"profile":"fbs_port0","serial":{"device":"/no-fatek-terminal"},"signals":[{"name":"word","address":"R00012"}]})).unwrap();
        let mut source = open(&config).unwrap();
        let error = source.read(Duration::ZERO).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
        #[cfg(any(unix, windows))]
        {
            let mut source = open(&config).unwrap();
            source.read(Duration::from_secs(1)).unwrap_err();
            let error = source.read(Duration::from_secs(1)).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
        }
    }
    #[test]
    fn every_port0_baud_is_valid_but_framing_and_unicast_are_fixed_before_open() {
        for baud in [9600, 19_200, 38_400, 57_600, 115_200] {
            let config: Config = serde_json::from_value(json!({"profile":"fbs_port0","serial":{"device":"missing","baud":baud},"signals":[{"name":"word","address":"R00012"}]})).unwrap();
            drop(open(&config).unwrap());
        }
        for extra in [
            json!({"baud":4800}),
            json!({"baud":691_200}),
            json!({"data_bits":8}),
            json!({"parity":"none"}),
            json!({"stop_bits":2}),
            json!({"device":""}),
            json!({"device":"bad\0path"}),
        ] {
            let mut value = json!({"profile":"fbs_port0","serial":{"device":"missing"},"signals":[{"name":"word","address":"R00012"}]});
            value["serial"]
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let config: Config = serde_json::from_value(value).unwrap();
            let error = open(&config).err().unwrap();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        }
        for unit in [0, 255] {
            let config: Config = serde_json::from_value(json!({"profile":"fbs_port0","address":"127.0.0.1:9","unit":unit,"signals":[{"name":"word","address":"R00012"}]})).unwrap();
            let error = open(&config).err().unwrap();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        }
    }
}
