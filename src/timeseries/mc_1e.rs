//! Read-only MELSEC-F MC A-compatible 1E over TCP.
//! FX3U-ENET-ADP JY997D45801H sections 7.3–7.5 and FX3U-ENET
//! JY997D18101H sections 9.1–9.3 define framing, devices and read limits.
//! <https://dl.mitsubishielectric.com/dl/fa/document/manual/plc_fx/jy997d45801/jy997d45801h.pdf>
//! <https://dl.mitsubishielectric.com/dl/fa/document/manual/plc_fx/jy997d18101/jy997d18101h.pdf>
use super::{
    Reading, Source, net,
    slmp::{DataType, SizeLimit},
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    #[serde(default)]
    pub encoding: Encoding,
    #[serde(
        default = "monitoring_timer",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub monitoring_timer: u16,
    pub signals: Vec<Signal>,
}

const fn monitoring_timer() -> u16 {
    4
}

const fn one() -> u16 {
    1
}

const fn scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Encoding {
    #[default]
    Binary,
    Ascii,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    pub device: Device,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub address: u32,
    #[serde(default)]
    pub data_type: DataType,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    pub elements: u16,
    #[serde(default = "scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Device {
    D,
    R,
    Tn,
    Cn,
    X,
    Y,
    M,
    S,
    Ts,
    Cs,
}

impl Device {
    const fn code(self) -> u16 {
        u16::from_be_bytes(match self {
            Self::D => *b"D ",
            Self::R => *b"R ",
            Self::Tn => *b"TN",
            Self::Cn => *b"CN",
            Self::X => *b"X ",
            Self::Y => *b"Y ",
            Self::M => *b"M ",
            Self::S => *b"S ",
            Self::Ts => *b"TS",
            Self::Cs => *b"CS",
        })
    }

    const fn bit(self) -> bool {
        matches!(
            self,
            Self::X | Self::Y | Self::M | Self::S | Self::Ts | Self::Cs
        )
    }

    fn range_valid(self, start: u32, end: u32) -> bool {
        // Union of the address ranges for the documented FX CPUs. Availability
        // within those ranges depends on the CPU and its application settings.
        match self {
            Self::D => end <= 8511 && (end <= 7999 || start >= 8000),
            Self::R => end <= 32767,
            Self::Tn | Self::Ts => end <= 511,
            Self::Cn | Self::Cs | Self::X | Self::Y => end <= 255,
            Self::M => end <= 8511 && (end <= 7679 || start >= 8000),
            Self::S => end <= 4095,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ReadSpec {
    command: u8,
    count: u8,
    bytes: usize,
}

impl Signal {
    fn read_spec(&self) -> Result<ReadSpec> {
        let bits = matches!(self.data_type, DataType::Bool);
        if bits && !self.device.bit() {
            return Err(Usage("MC 1E bool requires a bit device".into()).into());
        }
        let bit_words = !bits && self.device.bit();
        if bit_words && !self.address.is_multiple_of(16) {
            return Err(Usage(
                "MC 1E word reads of bit devices require a 16-point-aligned address".into(),
            )
            .into());
        }
        let words = self.data_type.words();
        let points = u32::from(self.elements) * u32::from(words.max(1));
        let maximum = if bits {
            256
        } else if bit_words {
            32
        } else {
            64
        };
        if !(1..=maximum).contains(&points) {
            return Err(Usage(
                "MC 1E read requires 1–256 bits, 1–64 words, or 1–32 words from bit devices".into(),
            )
            .into());
        }
        let extent = if matches!(self.device, Device::Cn) {
            let wide = self.address >= 200;
            let valid_type = if wide {
                matches!(self.data_type, DataType::U32 | DataType::I32)
            } else {
                matches!(self.data_type, DataType::U16 | DataType::I16)
            };
            if !valid_type || (!wide && self.address + u32::from(self.elements) > 200) {
                return Err(Usage("MC 1E CN reads require 16-bit integers below C200 or 32-bit integers from C200, within one range".into()).into());
            }
            u32::from(self.elements)
        } else if bit_words {
            points * 16
        } else {
            points
        };
        if self
            .address
            .checked_add(extent - 1)
            .is_none_or(|end| !self.device.range_valid(self.address, end))
        {
            return Err(
                Usage("MC 1E signal range exceeds the documented FX device area".into()).into(),
            );
        }
        let count = if points == 256 {
            0
        } else {
            u8::try_from(points)?
        };
        let points = usize::try_from(points)?;
        Ok(ReadSpec {
            command: u8::from(!bits),
            count,
            bytes: if bits { points.div_ceil(2) } else { points * 2 },
        })
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 || config.signals.is_empty() || config.signals.len() > 256 {
        return Err(Usage("MC 1E requires a nonzero TCP port and 1–256 signals".into()).into());
    }
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "MC 1E signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("MC 1E signal names must be unique".into()).into());
        }
        signal.read_spec()?;
        if !signal.data_type.valid_scale(signal.scale, signal.offset) {
            return Err(Usage(
                "MC 1E scale/offset must be finite; bool and 64-bit integers cannot be scaled"
                    .into(),
            )
            .into());
        }
    }
    Ok(Box::new(Client {
        config: config.clone(),
        stream: None,
    }))
}

impl Encoding {
    fn request(self, signal: &Signal, spec: ReadSpec, timer: u16) -> Vec<u8> {
        let ReadSpec { command, count, .. } = spec;
        let device = signal.device.code();
        let address = signal.address;
        match self {
            Self::Binary => {
                let mut bytes = Vec::with_capacity(12);
                bytes.extend([command, 0xff]);
                bytes.extend(timer.to_le_bytes());
                bytes.extend(address.to_le_bytes());
                bytes.extend(device.to_le_bytes());
                bytes.extend([count, 0]);
                bytes
            }
            Self::Ascii => {
                format!("{command:02X}FF{timer:04X}{device:04X}{address:08X}{count:02X}00")
                    .into_bytes()
            }
        }
    }

    fn read_pair(self, stream: &mut dyn net::Transport) -> Result<[u8; 2]> {
        match self {
            Self::Binary => {
                let mut bytes = [0; 2];
                stream
                    .read_exact(&mut bytes)
                    .map_err(|error| io_error(&error))?;
                Ok(bytes)
            }
            Self::Ascii => {
                let mut raw = [0; 4];
                stream
                    .read_exact(&mut raw)
                    .map_err(|error| io_error(&error))?;
                Ok([hex_pair([raw[0], raw[1]])?, hex_pair([raw[2], raw[3]])?])
            }
        }
    }

    fn read_data(self, stream: &mut dyn net::Transport, spec: ReadSpec) -> Result<Vec<u8>> {
        let mut bytes = vec![0; spec.bytes * if matches!(self, Self::Binary) { 1 } else { 2 }];
        stream
            .read_exact(&mut bytes)
            .map_err(|error| io_error(&error))?;
        match self {
            Self::Binary => Ok(bytes),
            Self::Ascii if spec.command == 0 => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(hex_pair)
                .collect(),
            Self::Ascii => {
                let mut decoded = Vec::with_capacity(spec.bytes);
                for &[a, b, c, d] in bytes.as_chunks::<4>().0 {
                    // ASCII renders each 16-bit word high digit first. The
                    // shared value decoder expects little-endian wire words.
                    decoded.extend([hex_pair([c, d])?, hex_pair([a, b])?]);
                }
                Ok(decoded)
            }
        }
    }
}

fn hex_pair([high, low]: [u8; 2]) -> Result<u8> {
    fn nibble(byte: u8) -> Result<u8> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(protocol("response contains a non-hexadecimal digit")),
        }
    }
    Ok((nibble(high)? << 4) | nibble(low)?)
}

struct Client {
    config: Config,
    stream: Option<Box<dyn net::Transport>>,
}

impl Client {
    fn connect(&self, deadline: Instant) -> Result<Box<dyn net::Transport>> {
        let stream = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        stream.set_nodelay(true).map_err(|error| io_error(&error))?;
        Ok(net::plain(stream, deadline))
    }

    fn read_signal(&self, signal: &Signal, stream: &mut dyn net::Transport) -> Result<Value> {
        let spec = signal.read_spec()?;
        let encoding = self.config.encoding;
        stream
            .write_all(&encoding.request(signal, spec, self.config.monitoring_timer))
            .map_err(|error| io_error(&error))?;
        let [subheader, status] = encoding.read_pair(stream)?;
        if subheader != (0x80 | spec.command) {
            return Err(protocol("response subheader differs from the read command"));
        }
        if status != 0 {
            let mut details = json!({"mc_complete_code":status});
            if status == 0x5b {
                let [abnormal, reserved] = encoding.read_pair(stream)?;
                if reserved != 0 {
                    return Err(protocol("invalid reserved byte in 5B response"));
                }
                details["mc_abnormal_code"] = json!(abnormal);
            }
            return Err(
                Categorized::new(ErrorKind::OperationFailed, "MC 1E target rejected read")
                    .details(details)
                    .into(),
            );
        }
        let data = encoding.read_data(stream, spec)?;
        signal.data_type.decode(
            signal.elements,
            signal.scale,
            signal.offset,
            &data,
            protocol,
        )
    }

    fn read_poll(
        &self,
        stream: &mut dyn net::Transport,
        deadline: Instant,
    ) -> Result<Vec<Reading>> {
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for signal in &self.config.signals {
            let value = self.read_signal(signal, stream)?;
            let reading = Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            };
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            size.write_all(b",")
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            readings.push(reading);
        }
        remaining(deadline)?;
        Ok(readings)
    }
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("MC 1E timeout is too large".into()))?;
        let mut stream = self
            .stream
            .take()
            .map_or_else(|| self.connect(deadline), Ok)?;
        stream.set_deadline(deadline);
        // 1E has no transaction ID. Keep one outstanding read and abandon
        // the connection after any failed poll, so late bytes cannot carry over.
        let result = self.read_poll(stream.as_mut(), deadline);
        if result.is_ok() {
            self.stream = Some(stream);
        }
        result
    }
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("MC 1E: {message}"))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "MC 1E operation deadline expired",
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
    Categorized::error(kind, format!("MC 1E I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read as _, net::TcpListener, thread};

    #[test]
    fn a_failed_poll_reconnects_before_any_new_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let config: Config = serde_json::from_value(json!({"address":address.to_string(),
            "signals":[{"name":"value","device":"D","address":0}]}))
        .unwrap();
        let mut client = open(&config).unwrap();
        let server = thread::spawn(move || {
            for response in [&[0x80, 0, 0x81, 0, 0xad, 0xde][..], &[0x81, 0, 42, 0]] {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "MC 1E did not reconnect");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 12];
                stream.read_exact(&mut request).unwrap();
                assert_eq!(request, [1, 255, 4, 0, 0, 0, 0, 0, 0x20, 0x44, 1, 0]);
                stream.write_all(response).unwrap();
                if response.len() > 4 {
                    // The first socket also contains a plausible future read
                    // response. The client must close it rather than reuse it.
                    match stream.read(&mut [0]) {
                        Ok(0) => {}
                        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                        other => panic!("failed MC 1E socket was not closed: {other:?}"),
                    }
                }
            }
        });
        let timeout = Duration::from_secs(2);
        let error = client.read(timeout).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            ErrorKind::OperationFailed
        );
        let readings = client.read(timeout).unwrap();
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value, 42);
        server.join().unwrap();
    }
}
