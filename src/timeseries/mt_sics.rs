//! Immediate net weight over a dedicated METTLER TOLEDO MT-SICS command port.
//! IND360 User's Guide 30654701 rev. 04 §§C.1–C.2 and `NewClassic` MT-SICS
//! Reference Manual 30014405 pp. 9, 103 document the SI transaction.
use super::{Reading, Source, net, serial};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Number, Value, json};
use std::{
    io,
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const REQUEST: &[u8] = b"SI\r\n";
const MAX_UNIT: usize = 5;
const MAX_REPLY: usize = 4 + 10 + 1 + MAX_UNIT + 2;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: Option<SocketAddr>,
    serial: Option<SerialConfig>,
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
    "net_weight".into()
}
const fn baud() -> u32 {
    9600
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
    super::validate_name(&config.signal, "MT-SICS signal")?;
    let connection =
        match (config.address, config.serial.as_ref()) {
            (Some(address), None) if address.port() != 0 && !address.ip().is_unspecified() => {
                Connection::Tcp {
                    address,
                    socket: None,
                }
            }
            (None, Some(serial)) if !serial.device.is_empty() => Connection::serial(serial)?,
            _ => return Err(Usage(
                "MT-SICS requires one numeric TCP address with a nonzero port or one serial device"
                    .into(),
            )
            .into()),
        };
    Ok(Box::new(Client {
        connection: Some(connection),
        signal: config.signal.clone(),
    }))
}

enum Connection {
    Tcp {
        address: SocketAddr,
        socket: Option<Box<dyn net::Transport>>,
    },
    #[cfg(any(unix, windows))]
    Serial(serial::Port),
}

impl Connection {
    fn serial(config: &SerialConfig) -> Result<Self> {
        serial::validate(config.baud, config.stop_bits, config.data_bits)?;
        #[cfg(any(unix, windows))]
        {
            Ok(Self::Serial(serial::open(
                &config.device,
                config.baud,
                config.parity,
                config.stop_bits,
                config.data_bits,
            )?))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = config.parity;
            Err(Categorized::error(
                ErrorKind::Unsupported,
                "MT-SICS serial requires a Unix terminal or Windows COM port",
            ))
        }
    }

    fn exchange(&mut self, deadline: Instant) -> Result<Value> {
        match self {
            Self::Tcp { address, socket } => {
                if socket.is_none() {
                    let stream = TcpStream::connect_timeout(address, remaining(deadline)?)
                        .map_err(|error| io_error(&error))?;
                    stream.set_nodelay(true).map_err(|error| io_error(&error))?;
                    *socket = Some(net::plain(stream, deadline));
                }
                let stream = socket.as_deref_mut().expect("MT-SICS socket connected");
                stream.set_deadline(deadline);
                stream
                    .write_all(REQUEST)
                    .map_err(|error| io_error(&error))?;
                read_reply(|bytes| stream.read(bytes), deadline)
            }
            #[cfg(any(unix, windows))]
            Self::Serial(port) => {
                port.write_all(REQUEST, deadline)
                    .map_err(|error| io_error(&error))?;
                read_reply(
                    |bytes| {
                        port.receive(bytes, deadline)?
                            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
                    },
                    deadline,
                )
            }
        }
    }
}

struct Client {
    connection: Option<Connection>,
    signal: String,
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("MT-SICS timeout is too large".into()))?;
        let connection = self.connection.as_mut().ok_or_else(|| {
            Categorized::error(ErrorKind::Disconnected, "MT-SICS command channel closed")
        })?;
        let value = match connection.exchange(deadline) {
            Ok(value) => value,
            Err(error) => {
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
    deadline: Instant,
) -> Result<Value> {
    let mut frame = [0; MAX_REPLY + 1];
    let mut length = 0;
    while length < MAX_REPLY {
        remaining(deadline)?;
        match read(&mut frame[length..]) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "MT-SICS peer closed during reply",
                ));
            }
            Ok(count) => length += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        }
        let buffered = &frame[..length];
        if let Some(end) = buffered.iter().position(|&byte| byte == b'\n') {
            if end + 1 != length {
                return Err(malformed("extra bytes after SI reply"));
            }
            let line = buffered
                .strip_suffix(b"\r\n")
                .ok_or_else(|| malformed("SI reply must end with CRLF"))?;
            let value = decode(line)?;
            remaining(deadline)?;
            return Ok(value);
        }
    }
    Err(malformed("SI reply exceeds its length limit"))
}

fn decode(line: &[u8]) -> Result<Value> {
    let status = match line {
        [b'S', b' ', status @ (b'S' | b'D'), b' ', weight @ ..] => {
            return decode_weight(*status, weight);
        }
        b"S I" => "unavailable",
        b"S +" => "overload",
        b"S -" => "underload",
        b"S L" | b"ES" | b"ET" | b"EL" | b"EI" => {
            let reply = String::from_utf8_lossy(line);
            return Err(
                Categorized::new(ErrorKind::OperationFailed, "MT-SICS device rejected SI")
                    .details(json!({"reply":reply}))
                    .into(),
            );
        }
        _ => return Err(malformed("unexpected SI response ID or status")),
    };
    Ok(json!({"status":status}))
}

fn decode_weight(status: u8, weight: &[u8]) -> Result<Value> {
    let Some((field, [b' ', unit @ ..])) = weight.split_first_chunk::<10>() else {
        return Err(malformed(
            "SI weight requires a ten-character field followed by a unit",
        ));
    };
    if !(1..=MAX_UNIT).contains(&unit.len()) || !unit.iter().all(u8::is_ascii_graphic) {
        return Err(malformed(
            "SI weight unit must be one to five printable ASCII characters",
        ));
    }
    let field = std::str::from_utf8(field).map_err(|_| malformed("SI weight is not ASCII"))?;
    // DeltaRange balances replace the last decimal digit with a blank in the coarse range.
    let raw = field
        .strip_suffix(' ')
        .unwrap_or(field)
        .trim_start_matches(' ');
    if !raw
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'.'))
    {
        return Err(malformed("invalid SI decimal weight"));
    }
    let value = raw
        .parse::<Number>()
        .map_err(|_| malformed("invalid SI decimal weight"))?;
    let unit = std::str::from_utf8(unit).map_err(|_| malformed("SI weight unit is not ASCII"))?;
    let status = if status == b'S' { "stable" } else { "dynamic" };
    Ok(json!({"status":status,"raw":raw,"value":value,"unit":unit}))
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("MT-SICS: {message}"))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "MT-SICS poll deadline expired",
        ));
    }
    Ok(left)
}

fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("MT-SICS I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn reply(bytes: &[u8]) -> Result<Value> {
        let mut input = io::Cursor::new(bytes);
        read_reply(
            |buffer| input.read(buffer),
            Instant::now() + Duration::from_secs(1),
        )
    }

    #[test]
    fn si_preserves_three_nonnumeric_states() {
        for (line, status) in [
            (b"S I\r\n", "unavailable"),
            (b"S +\r\n", "overload"),
            (b"S -\r\n", "underload"),
        ] {
            assert_eq!(reply(line).unwrap(), json!({"status":status}));
        }
    }

    #[test]
    fn si_preserves_exact_signed_weight_and_coarse_range_padding() {
        let value = reply(b"S D    -0.2500 kg\r\n").unwrap();
        assert_eq!(value["raw"], "-0.2500");
        assert_eq!(value["value"].to_string(), "-0.2500");
        let value = reply(b"S S    4875.2  g\r\n").unwrap();
        assert_eq!(
            value,
            json!({"status":"stable","raw":"4875.2","value":4875.2,"unit":"g"})
        );
    }

    #[test]
    fn si_rejects_bad_framing_wrong_id_weight_and_device_errors() {
        for line in [
            &b"S I\n"[..],
            b"S I\r\nS +\r\n",
            b"SI S      0.256 kg\r\n",
            b"S I      0.256 kg\r\n",
            b"S S 0.256 kg\r\n",
            b"S S       1e-2 kg\r\n",
            b"S S      0.256\r\n",
            b"S S      0.256 kgrams\r\n",
        ] {
            reply(line).expect_err("invalid SI response accepted");
        }
        for line in [&b"S L\r\n"[..], b"ES\r\n", b"ET\r\n", b"EL\r\n", b"EI\r\n"] {
            let error = reply(line).unwrap_err();
            let classified = crate::types::classify(&error);
            assert_eq!(classified.kind, ErrorKind::OperationFailed);
            assert_eq!(
                classified.details.unwrap()["reply"],
                std::str::from_utf8(&line[..line.len() - 2]).unwrap()
            );
        }
    }
}
