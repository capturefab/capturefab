//! Read-only Q transactions over an explicitly configured A&D standard-output
//! command interface. The adapter never selects the balance's format or mode.
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: Option<SocketAddr>,
    serial: Option<SerialConfig>,
    #[serde(default)]
    terminator: parser::Terminator,
    #[serde(default = "signal")]
    signal: String,
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

fn signal() -> String {
    "measurement".into()
}
const fn baud() -> u32 {
    2400
}
const fn data_bits() -> u8 {
    7
}
const fn stop_bits() -> u8 {
    1
}

impl SerialConfig {
    fn validate(&self) -> Result<()> {
        serial::validate(self.baud, self.stop_bits, self.data_bits)?;
        if self.device.is_empty()
            || !matches!(self.baud, 600 | 1200 | 2400 | 4800 | 9600 | 19_200 | 38_400)
            || self.stop_bits != 1
            || !matches!(
                (self.data_bits, self.parity),
                (7, serial::Parity::Even | serial::Parity::Odd) | (8, serial::Parity::None)
            )
        {
            return Err(Usage("A&D serial requires a nonempty device, 600–38400 baud and 7E1, 7O1 or 8N1; configure the dedicated balance interface and cable for no flow control".into()).into());
        }
        Ok(())
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    super::validate_name(&config.signal, "A&D signal")?;
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
        _ => return Err(Usage("A&D requires one numeric transparent TCP bridge address with a nonzero port or one serial device".into()).into()),
    };
    Ok(Box::new(Client {
        connection: Some(connection),
        terminator: config.terminator,
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
    fn exchange(&mut self, terminator: parser::Terminator, deadline: Instant) -> Result<Value> {
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
                let stream = socket.as_deref_mut().expect("A&D socket connected");
                stream.set_deadline(deadline);
                remaining(deadline)?;
                stream
                    .write_all(terminator.request())
                    .map_err(|error| io_error(&error))?;
                read_reply(|bytes| stream.read(bytes), terminator, deadline)
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
                    let port = port.as_mut().expect("A&D serial port opened");
                    port.write_all(terminator.request(), deadline)
                        .map_err(|error| io_error(&error))?;
                    read_reply(
                        |bytes| {
                            port.receive(bytes, deadline)?
                                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
                        },
                        terminator,
                        deadline,
                    )
                }
                #[cfg(not(any(unix, windows)))]
                {
                    let _ = config;
                    Err(Categorized::error(
                        ErrorKind::Unsupported,
                        "A&D serial requires a Unix terminal or Windows COM port",
                    ))
                }
            }
        }
    }
}

struct Client {
    connection: Option<Connection>,
    terminator: parser::Terminator,
    signal: String,
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("A&D timeout is too large".into()))?;
        let connection = self.connection.as_mut().ok_or_else(|| {
            Categorized::error(ErrorKind::Disconnected, "A&D command channel retired")
        })?;
        let value = match connection.exchange(self.terminator, deadline) {
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
    terminator: parser::Terminator,
    deadline: Instant,
) -> Result<Value> {
    // Maximum standard reply plus one byte to detect an overlong/coalesced read.
    let mut frame = [0; 18];
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
                    "A&D peer closed during reply",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        remaining(deadline)?;
        length = length
            .checked_add(count)
            .filter(|&size| size <= terminator.max_frame_bytes())
            .ok_or_else(|| malformed("reply exceeds the fixed standard-format limit"))?;
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
            let expected = terminator.bytes();
            if suffix.len() < expected.len() && expected.starts_with(suffix) {
                continue;
            }
            if suffix != expected {
                return Err(malformed("bad terminator or extra bytes after Q reply"));
            }
            let value = parser::decode(buffered, terminator)?;
            remaining(deadline)?;
            return Ok(value);
        }
        if length == terminator.max_frame_bytes() {
            return Err(malformed("missing configured reply terminator"));
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "A&D poll deadline expired",
        ));
    }
    Ok(left)
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("A&D standard: {message}"),
    )
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("A&D I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    fn reply(bytes: &[u8], terminator: parser::Terminator) -> Result<Value> {
        let mut input = io::Cursor::new(bytes);
        read_reply(
            |buffer| input.read(buffer),
            terminator,
            Instant::now() + Duration::from_secs(1),
        )
    }

    #[test]
    fn split_reply_and_coalesced_short_errors_have_exact_framing() {
        let mut input = &b"ST,+03142.06  g\r\n"[..];
        let value = read_reply(
            |buffer| {
                let byte = buffer
                    .get_mut(..1)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                input.read(byte)
            },
            parser::Terminator::CrLf,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(value["value"].to_string(), "3142.06");
        for bytes in [
            &b"EC,E02\r\nX"[..],
            b"EC,E02\r\nEC,E02\r\n",
            b"EC,E02\n",
            b"ST,+03142.06  g\r\nX",
        ] {
            let error = reply(bytes, parser::Terminator::CrLf).unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                ErrorKind::OperationFailed
            );
        }
        assert_eq!(
            reply(b"EC,E02\r", parser::Terminator::Cr).unwrap()["error_code_raw"],
            "E02"
        );
    }

    #[test]
    fn expired_read_never_calls_io_and_failure_permanently_retires_owner() {
        let error = read_reply(
            |_| panic!("expired read attempted I/O"),
            parser::Terminator::CrLf,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let error = read_reply(
            |buffer| {
                std::thread::sleep(Duration::from_millis(10));
                let raw = b"EC,E02\r\n";
                buffer
                    .get_mut(..raw.len())
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
                    .copy_from_slice(raw);
                Ok(raw.len())
            },
            parser::Terminator::CrLf,
            Instant::now() + Duration::from_millis(1),
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let config: Config = serde_json::from_value(json!({"address":"127.0.0.1:9"})).unwrap();
        let mut source = open(&config).unwrap();
        let error = source.read(Duration::ZERO).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
    }

    #[test]
    #[cfg(unix)]
    fn failed_lazy_serial_open_retires_without_another_open() {
        let path = std::env::temp_dir().join(format!(
            "capturefab-aandd-absent-{}.port",
            std::process::id()
        ));
        assert!(!path.exists());
        let config: Config = serde_json::from_value(json!({"serial":{"device":path}})).unwrap();
        let mut source = open(&config).unwrap();
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unavailable);
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
        assert!(!path.exists());
    }

    #[test]
    fn all_config_checks_precede_lazy_device_open() {
        for value in [
            json!({}),
            json!({"address":"127.0.0.1:0"}),
            json!({"address":"0.0.0.0:9000"}),
            json!({"address":"127.0.0.1:9","serial":{"device":"missing"}}),
            json!({"serial":{"device":""}}),
            json!({"serial":{"device":"missing","baud":300}}),
            json!({"serial":{"device":"missing","data_bits":8}}),
            json!({"serial":{"device":"missing","stop_bits":2}}),
        ] {
            let config: Config = serde_json::from_value(value).unwrap();
            let error = open(&config).err().unwrap();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        }
        for value in [
            json!({"serial":{"device":"missing"}}),
            json!({"serial":{"device":"missing","baud":3.84e4,"parity":"none","data_bits":8}}),
        ] {
            let config: Config = serde_json::from_value(value).unwrap();
            drop(open(&config).unwrap());
        }
    }
}
