//! Mitsubishi MC/SLMP 3E binary and ASCII TCP, limited to read-only device command 0401.
//! Framing, device codes, word order and bit nibble order follow the official
//! SLMP Reference Manual (SH-080956), chapters 4 and 5.2:
//! <https://dl.mitsubishielectric.com/dl/fa/document/manual/plc/sh080956eng/sh080956engm.pdf>.
mod ascii;
mod timer;
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Read as _, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const MAX_BATCH: usize = 1 << 20;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub network: u8,
    #[serde(default = "station")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub station: u8,
    #[serde(default = "module_io")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub module_io: u16,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub multidrop: u8,
    /// PLC monitoring timer, in 250 ms units; the host deadline is independent.
    #[serde(default = "monitoring_timer")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub monitoring_timer: u16,
    #[serde(default)]
    pub device_format: DeviceFormat,
    #[serde(default)]
    encoding: Encoding,
    pub signals: Vec<Signal>,
}
const fn station() -> u8 {
    255
}
const fn module_io() -> u16 {
    1023
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
pub(crate) enum DeviceFormat {
    #[default]
    Standard,
    IqR,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    #[default]
    Binary,
    Ascii,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    pub device: Device,
    /// Numeric device number. Convert the device's hexadecimal notation first.
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub address: u32,
    #[serde(default)]
    pub data_type: SignalType,
    #[serde(default = "one")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub elements: u16,
    #[serde(default = "scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Device {
    Sm,
    Sd,
    X,
    Y,
    M,
    L,
    F,
    V,
    B,
    S,
    D,
    W,
    Ts,
    Tc,
    Tn,
    #[serde(alias = "SS")]
    Sts,
    #[serde(alias = "SC")]
    Stc,
    #[serde(alias = "SN")]
    Stn,
    Ltn,
    Lstn,
    Cs,
    Cc,
    Cn,
    Sb,
    Sw,
    Z,
    R,
    Zr,
}
impl Device {
    const fn code(self) -> u8 {
        match self {
            Self::Sm => 0x91,
            Self::Sd => 0xa9,
            Self::X => 0x9c,
            Self::Y => 0x9d,
            Self::M => 0x90,
            Self::L => 0x92,
            Self::F => 0x93,
            Self::V => 0x94,
            Self::B => 0xa0,
            Self::S => 0x98,
            Self::D => 0xa8,
            Self::W => 0xb4,
            Self::Ts => 0xc1,
            Self::Tc => 0xc0,
            Self::Tn => 0xc2,
            Self::Sts => 0xc7,
            Self::Stc => 0xc6,
            Self::Stn => 0xc8,
            Self::Ltn => 0x52,
            Self::Lstn => 0x5a,
            Self::Cs => 0xc4,
            Self::Cc => 0xc3,
            Self::Cn => 0xc5,
            Self::Sb => 0xa1,
            Self::Sw => 0xb5,
            Self::Z => 0xcc,
            Self::R => 0xaf,
            Self::Zr => 0xb0,
        }
    }
    const fn bit(self) -> bool {
        matches!(
            self,
            Self::Sm
                | Self::X
                | Self::Y
                | Self::M
                | Self::L
                | Self::F
                | Self::V
                | Self::B
                | Self::S
                | Self::Sts
                | Self::Stc
                | Self::Ts
                | Self::Tc
                | Self::Cs
                | Self::Cc
                | Self::Sb
        )
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
pub(crate) enum SignalType {
    Numeric(DataType),
    Timer(timer::Kind),
}
impl Default for SignalType {
    fn default() -> Self {
        Self::Numeric(DataType::default())
    }
}
impl SignalType {
    const fn is_timer(self) -> bool {
        matches!(self, Self::Timer(timer::Kind::LongTimer))
    }
    const fn is_bool(self) -> bool {
        matches!(self, Self::Numeric(DataType::Bool))
    }
    const fn words(self) -> u16 {
        match self {
            Self::Numeric(kind) => kind.words(),
            Self::Timer(_) => 4,
        }
    }
    fn valid_scale(self, scale: f64, offset: f64) -> bool {
        match self {
            Self::Numeric(kind) => kind.valid_scale(scale, offset),
            Self::Timer(_) => scale == 1.0 && offset == 0.0,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataType {
    Bool,
    #[default]
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
}
impl DataType {
    pub(super) const fn words(self) -> u16 {
        match self {
            Self::Bool => 0,
            Self::U16 | Self::I16 => 1,
            Self::U32 | Self::I32 | Self::F32 => 2,
            Self::U64 | Self::I64 | Self::F64 => 4,
        }
    }
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        let config = self;
        if config.address.port() == 0 || config.signals.is_empty() || config.signals.len() > 256 {
            return Err(Usage("SLMP requires a nonzero TCP port and 1–256 signals".into()).into());
        }
        if config.network > 239
            || !matches!(config.station, 1..=120 | 125 | 126 | 255)
            || config.multidrop > 31
            || config.monitoring_timer == 0
        {
            return Err(Usage("SLMP requires network 0–239, station 1–120/125/126/255, multidrop 0–31 and a nonzero monitoring_timer".into()).into());
        }
        let mut names = BTreeSet::new();
        for signal in &config.signals {
            super::validate_name(&signal.name, "SLMP signal")?;
            if !names.insert(&signal.name) {
                return Err(Usage("SLMP signal names must be unique".into()).into());
            }
            if signal.data_type.is_timer() != matches!(signal.device, Device::Ltn | Device::Lstn)
                || (signal.data_type.is_timer()
                    && !matches!(config.device_format, DeviceFormat::IqR))
            {
                return Err(Usage(
                    "SLMP LTN/LSTN require iq_r device_format and explicit long_timer data_type"
                        .into(),
                )
                .into());
            }
            if signal.device.bit() != signal.data_type.is_bool() {
                return Err(Usage(
                    "SLMP bit devices require bool; word devices require numeric data_type".into(),
                )
                .into());
            }
            let points = signal.points()?;
            if matches!(config.encoding, Encoding::Ascii) && signal.device.bit() && points > 3584 {
                return Err(Usage("SLMP 3E ASCII reads support up to 3584 bits".into()).into());
            }
            let maximum = match (
                config.device_format,
                config.encoding,
                signal.device.decimal_address(),
            ) {
                (DeviceFormat::Standard, Encoding::Ascii, true) => 999_999,
                (DeviceFormat::IqR, Encoding::Ascii, true) => 99_999_999,
                (DeviceFormat::Standard, _, _) => 0xff_ffff,
                (DeviceFormat::IqR, _, _) => u32::MAX,
            };
            if signal
                .address
                .checked_add(
                    u32::from(if signal.data_type.is_timer() {
                        signal.elements
                    } else {
                        points
                    }) - 1,
                )
                .is_none_or(|end| end > maximum)
            {
                return Err(Usage(
                    "SLMP signal range exceeds the configured device address format".into(),
                )
                .into());
            }
            if !signal.data_type.valid_scale(signal.scale, signal.offset) {
                return Err(Usage(
                    "SLMP scale/offset must be finite; bool and 64-bit integers cannot be scaled"
                        .into(),
                )
                .into());
            }
        }
        timer::Budget::new(&config.signals)?.check(2)?;
        Ok(())
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    Ok(Box::new(Client {
        config: config.clone(),
        stream: None,
        retired: false,
    }))
}

impl Config {
    fn route(&self) -> [u8; 5] {
        let io = self.module_io.to_le_bytes();
        [self.network, self.station, io[0], io[1], self.multidrop]
    }
    fn subcommand(&self, signal: &Signal) -> u16 {
        u16::from(signal.device.bit())
            | if matches!(self.device_format, DeviceFormat::IqR) {
                2
            } else {
                0
            }
    }
    fn request(&self, signal: &Signal) -> Result<Vec<u8>> {
        if matches!(self.encoding, Encoding::Ascii) {
            return ascii::request(self, signal);
        }
        let wide = matches!(self.device_format, DeviceFormat::IqR);
        let mut data = self.monitoring_timer.to_le_bytes().to_vec();
        data.extend_from_slice(&0x0401u16.to_le_bytes());
        data.extend_from_slice(&self.subcommand(signal).to_le_bytes());
        let address = signal.address.to_le_bytes();
        data.extend_from_slice(if wide { &address } else { &address[..3] });
        data.push(signal.device.code());
        if wide {
            data.push(0);
        }
        data.extend_from_slice(&signal.points()?.to_le_bytes());
        let mut packet = vec![0x50, 0];
        packet.extend_from_slice(&self.route());
        packet.extend_from_slice(&u16::try_from(data.len())?.to_le_bytes());
        packet.extend_from_slice(&data);
        Ok(packet)
    }
}
impl Signal {
    fn points(&self) -> Result<u16> {
        let maximum = if self.device.bit() { 7168 } else { 960 };
        let points = self.elements.checked_mul(self.data_type.words().max(1));
        match points {
            Some(points) if (1..=maximum).contains(&points) => Ok(points),
            _ => Err(Usage("SLMP read must contain 1–960 words or 1–7168 bits".into()).into()),
        }
    }
    fn bytes(&self) -> Result<usize> {
        let points = usize::from(self.points()?);
        Ok(if self.device.bit() {
            points.div_ceil(2)
        } else {
            points * 2
        })
    }
    fn decode(&self, data: &[u8], deadline: Instant) -> Result<Value> {
        match self.data_type {
            SignalType::Numeric(kind) => {
                kind.decode(self.elements, self.scale, self.offset, data, protocol)
            }
            SignalType::Timer(_) => timer::decode(self, data, deadline),
        }
    }
}

impl DataType {
    pub(super) fn valid_scale(self, scale: f64, offset: f64) -> bool {
        scale.is_finite()
            && offset.is_finite()
            && (!matches!(self, Self::Bool | Self::U64 | Self::I64)
                || (scale == 1.0 && offset == 0.0))
    }

    pub(super) fn decode(
        self,
        elements: u16,
        scale: f64,
        offset: f64,
        data: &[u8],
        invalid: fn(&str) -> anyhow::Error,
    ) -> Result<Value> {
        let bits = matches!(self, Self::Bool);
        let elements = usize::from(elements);
        let bytes = if bits {
            elements.div_ceil(2)
        } else {
            elements * usize::from(self.words()) * 2
        };
        if data.len() != bytes {
            return Err(invalid(
                "read response length differs from configured signal width",
            ));
        }
        let values = if bits {
            let mut values = Vec::with_capacity(elements);
            for index in 0..elements {
                let value = (data[index / 2] >> if index % 2 == 0 { 4 } else { 0 }) & 15;
                if value > 1 {
                    return Err(invalid("bit response nibble must be 0 or 1"));
                }
                values.push(json!(value == 1));
            }
            if !elements.is_multiple_of(2) && data.last().is_some_and(|last| last & 15 != 0) {
                return Err(invalid("odd bit response has nonzero padding nibble"));
            }
            values
        } else {
            data.chunks_exact(usize::from(self.words()) * 2)
                .map(|bytes| self.scalar(bytes, scale, offset, invalid))
                .collect::<Result<Vec<_>>>()?
        };
        if elements == 1 {
            values
                .into_iter()
                .next()
                .ok_or_else(|| invalid("missing scalar"))
        } else {
            Ok(Value::Array(values))
        }
    }
    fn scalar(
        self,
        bytes: &[u8],
        scale: f64,
        offset: f64,
        invalid: fn(&str) -> anyhow::Error,
    ) -> Result<Value> {
        let value = match self {
            Self::U16 => json!(u16::from_le_bytes(bytes.try_into()?)),
            Self::I16 => json!(i16::from_le_bytes(bytes.try_into()?)),
            Self::U32 => json!(u32::from_le_bytes(bytes.try_into()?)),
            Self::I32 => json!(i32::from_le_bytes(bytes.try_into()?)),
            Self::U64 => return Ok(json!(u64::from_le_bytes(bytes.try_into()?))),
            Self::I64 => return Ok(json!(i64::from_le_bytes(bytes.try_into()?))),
            Self::F32 => {
                let value = f32::from_le_bytes(bytes.try_into()?);
                if !value.is_finite() {
                    return Err(invalid("non-finite floating-point value"));
                }
                json!(value)
            }
            Self::F64 => {
                let value = f64::from_le_bytes(bytes.try_into()?);
                if !value.is_finite() {
                    return Err(invalid("non-finite floating-point value"));
                }
                json!(value)
            }
            Self::Bool => return Err(invalid("BOOL decoded as a word")),
        };
        if scale == 1.0 && offset == 0.0 {
            return Ok(value);
        }
        let scaled = value
            .as_f64()
            .ok_or_else(|| invalid("invalid numeric value"))?
            * scale
            + offset;
        if !scaled.is_finite() {
            return Err(invalid("non-finite scaled value"));
        }
        Ok(json!(scaled))
    }
}

struct Client {
    config: Config,
    stream: Option<TcpStream>,
    retired: bool,
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "SLMP source retired after a failed poll",
            ));
        }
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("SLMP timeout is too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        if result.is_err() {
            self.stream = None;
            self.retired = true;
        }
        result
    }
}

impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let mut minimum = timer::Budget::new(&self.config.signals)?;
        minimum.check(2)?;
        remaining(deadline)?;
        if self.stream.is_none() {
            let stream = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            stream.set_nodelay(true).map_err(|error| io_error(&error))?;
            self.stream = Some(stream);
        }
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        // 3E has no transaction ID. Only one outstanding request is allowed,
        // and a failed read ends acquisition, so no late frame can be reused.
        for signal in &self.config.signals {
            minimum.check(size.bytes)?;
            remaining(deadline)?;
            write(stream, &self.config.request(signal)?, deadline)?;
            let value = match self.config.encoding {
                Encoding::Binary => binary_reply(&self.config, signal, stream, deadline)?,
                Encoding::Ascii => ascii::reply(&self.config, signal, stream, deadline)?,
            };
            let reading = Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            };
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            size.write_all(b",")
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            minimum.advance();
            minimum.check(size.bytes)?;
            remaining(deadline)?;
            readings.push(reading);
        }
        remaining(deadline)?;
        Ok(readings)
    }
}

fn binary_reply(
    config: &Config,
    signal: &Signal,
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<Value> {
    let mut header = [0; 9];
    read(stream, &mut header, deadline)?;
    if header[..2] != [0xd0, 0] || header[2..7] != config.route() {
        return Err(protocol(
            "3E response subheader or route differs from request",
        ));
    }
    let length = usize::from(u16::from_le_bytes(header[7..9].try_into()?));
    if !(2..=(signal.bytes()? + 2).max(11)).contains(&length) {
        return Err(protocol("invalid or oversized 3E response length"));
    }
    let mut data = vec![0; length];
    read(stream, &mut data, deadline)?;
    let status = u16::from_le_bytes(data[..2].try_into()?);
    if status != 0 {
        return Err(target_error(status, &data[2..]));
    }
    signal.decode(&data[2..], deadline)
}

fn target_error(status: u16, data: &[u8]) -> anyhow::Error {
    Categorized::new(ErrorKind::OperationFailed, "SLMP target rejected read")
        .details(json!({"slmp_end_code":status,"error_data_hex":crate::types::hex(data)}))
        .into()
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("SLMP: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "SLMP operation deadline expired",
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
    Categorized::error(kind, format!("SLMP I/O: {error}"))
}
fn read(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        match stream.read(bytes) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "SLMP connection closed during response",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Ok(())
}
fn write(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "SLMP connection closed during request",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Ok(())
}
pub(super) struct SizeLimit {
    pub(super) bytes: usize,
}
impl Write for SizeLimit {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(data.len());
        if self.bytes > MAX_BATCH {
            return Err(io::Error::other("SLMP batch exceeds size limit"));
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
