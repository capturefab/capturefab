//! SBI print requests over a dedicated, preconfigured manual-output interface.
//! Autoprint, post-print tare, GLP and additional lines must already be off.
use super::{Reading, Source, net, serial};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::{
    io,
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

mod parser;

const REQUEST: &[u8] = b"\x1bP\r\n";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: Option<SocketAddr>,
    serial: Option<SerialConfig>,
    format: parser::Format,
    #[serde(default = "signal")]
    signal: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SerialConfig {
    device: String,
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
}

fn signal() -> String {
    "measurement".into()
}
const fn baud() -> u32 {
    9600
}
const fn parity() -> serial::Parity {
    serial::Parity::Odd
}
const fn data_bits() -> u8 {
    8
}
const fn stop_bits() -> u8 {
    1
}

impl SerialConfig {
    fn validate(&self) -> Result<()> {
        serial::validate(self.baud, self.stop_bits, self.data_bits)?;
        if self.device.is_empty()
            || !matches!(
                self.baud,
                600 | 1200 | 2400 | 4800 | 9600 | 19_200 | 38_400 | 57_600 | 115_200
            )
            || self.stop_bits != 1
            || !matches!(self.data_bits, 7 | 8)
        {
            return Err(Usage("Sartorius SBI serial requires a nonempty device, 600–115200 baud, 7 or 8 data bits and one stop bit; preconfigure no handshake and manual single-line printing without automatic tare".into()).into());
        }
        Ok(())
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    super::validate_name(&config.signal, "Sartorius SBI signal")?;
    let connection = match (config.address, config.serial.as_ref()) {
        (Some(address), None) if address.port() != 0 && !address.ip().is_unspecified() => {
            Connection::Tcp { address, socket: None }
        }
        (None, Some(config)) => {
            config.validate()?;
            Connection::Serial {
                config: config.clone(),
                #[cfg(any(unix, windows))]
                port: None,
            }
        }
        _ => return Err(Usage("Sartorius SBI requires one numeric transparent TCP bridge address with a nonzero port or one serial device".into()).into()),
    };
    Ok(Box::new(Client {
        connection: Some(connection),
        format: config.format,
        signal: config.signal.clone(),
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
    fn exchange(&mut self, format: parser::Format, deadline: Instant) -> Result<Value> {
        remaining(deadline)?;
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
                    .expect("Sartorius SBI socket connected");
                stream.set_deadline(deadline);
                remaining(deadline)?;
                stream
                    .write_all(REQUEST)
                    .map_err(|error| io_error(&error))?;
                read_reply(|bytes| stream.read(bytes), format, deadline)
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
                    let port = port.as_mut().expect("Sartorius SBI serial port opened");
                    port.write_all(REQUEST, deadline)
                        .map_err(|error| io_error(&error))?;
                    read_reply(
                        |bytes| {
                            port.receive(bytes, deadline)?
                                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
                        },
                        format,
                        deadline,
                    )
                }
                #[cfg(not(any(unix, windows)))]
                {
                    let _ = config;
                    Err(Categorized::error(
                        ErrorKind::Unsupported,
                        "Sartorius SBI serial requires a Unix terminal or Windows COM port",
                    ))
                }
            }
        }
    }
}

struct Client {
    connection: Option<Connection>,
    format: parser::Format,
    signal: String,
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("Sartorius SBI timeout is too large".into()))?;
        let connection = self.connection.as_mut().ok_or_else(|| {
            Categorized::error(
                ErrorKind::Disconnected,
                "Sartorius SBI command channel retired",
            )
        })?;
        let value = match connection.exchange(self.format, deadline) {
            Ok(value) => value,
            Err(error) => {
                // A damaged transaction has no request ID with which to recover.
                // Retire the owner; neither this nor a later poll reconnects/retries.
                self.connection = None;
                return Err(error);
            }
        };
        Ok(vec![Reading {
            signal: self.signal.clone(),
            value,
            source_time: None,
        }])
    }
}

fn read_reply(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    format: parser::Format,
    deadline: Instant,
) -> Result<Value> {
    // Maximum22-byte SBI reply plus one byte to detect an overlong/coalesced read.
    let mut frame = [0; 23];
    let mut length = 0;
    loop {
        remaining(deadline)?;
        let buffer = frame
            .get_mut(length..)
            .ok_or_else(|| malformed("reply buffer exhausted"))?;
        let count = match read(buffer) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "Sartorius SBI peer closed during reply",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        remaining(deadline)?;
        length = length
            .checked_add(count)
            .filter(|&size| size <= format.reply_bytes())
            .ok_or_else(|| malformed("reply exceeds the configured SBI line limit"))?;
        let buffered = frame
            .get(..length)
            .ok_or_else(|| malformed("invalid reply extent"))?;
        if let Some(end) = buffered
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
        {
            let suffix = buffered
                .get(end..)
                .ok_or_else(|| malformed("invalid terminator extent"))?;
            let expected = &b"\r\n"[..];
            if suffix.len() < expected.len() && expected.starts_with(suffix) {
                continue;
            }
            if suffix != expected {
                return Err(malformed("bad terminator or extra bytes after print reply"));
            }
            let value = parser::decode(buffered, format)?;
            remaining(deadline)?;
            return Ok(value);
        }
        if length == format.reply_bytes() {
            return Err(malformed("missing reply terminator"));
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "Sartorius SBI poll deadline expired",
        ));
    }
    Ok(left)
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("Sartorius SBI standard: {message}"),
    )
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("Sartorius SBI I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    #[test]
    fn one_byte_segments_and_coalesced_reply_limits_are_exact() {
        let mut input = &b"N     +   123.56 g  \r\n"[..];
        let value = read_reply(
            |buffer| {
                let byte = buffer
                    .get_mut(..1)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                input.read(byte)
            },
            parser::Format::Sbi22,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(value["value"].to_string(), "123.56");
        assert_eq!(REQUEST, b"\x1bP\r\n");
        for bytes in [
            &b"+   123.56 g  \r\nX"[..],
            b"+   123.56 g  \rX",
            b"+   123.56 g  \n",
        ] {
            let mut input = io::Cursor::new(bytes);
            let error = read_reply(
                |buffer| input.read(buffer),
                parser::Format::Sbi16,
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
    fn expired_budget_sends_nothing_and_permanently_retires_the_owner() {
        let config: Config =
            serde_json::from_value(json!({"address":"127.0.0.1:9","format":"sbi16"})).unwrap();
        let mut source = open(&config).unwrap();
        let error = source.read(Duration::ZERO).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
    }

    #[test]
    fn configuration_is_validated_before_lazy_open() {
        for value in [
            json!({"format":"sbi16"}),
            json!({"format":"sbi16","address":"127.0.0.1:0"}),
            json!({"format":"sbi22","address":"0.0.0.0:9000"}),
            json!({"format":"sbi16","address":"127.0.0.1:9","serial":{"device":"missing"}}),
            json!({"format":"sbi16","serial":{"device":""}}),
            json!({"format":"sbi16","serial":{"device":"missing","baud":300}}),
            json!({"format":"sbi16","serial":{"device":"missing","data_bits":6}}),
            json!({"format":"sbi16","serial":{"device":"missing","stop_bits":2}}),
        ] {
            let config: Config = serde_json::from_value(value).unwrap();
            let error = open(&config).err().unwrap();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        }
        for value in [
            json!({"format":"sbi16","serial":{"device":"missing"}}),
            json!({"format":"sbi22","serial":{"device":"missing","baud":1.152e5,"parity":"none","data_bits":7}}),
        ] {
            let config: Config = serde_json::from_value(value).unwrap();
            drop(open(&config).unwrap());
        }
    }
}
