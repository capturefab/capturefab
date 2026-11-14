//! Read-only Keyence KV Host Link TCP RD/RDS commands.
//! KV-EP21V/KV-8000/KV-7500/KV-5500/KV-NC1EP manual, chapter 8.
//! <https://www.keyence.com/support/user/controls/plc/manual/building/>
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
    io::{self, BufRead as _, BufReader, Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    #[serde(
        default = "max_points",
        deserialize_with = "crate::config_number::unsigned"
    )]
    max_points: u16,
    signals: Vec<Signal>,
}

const fn max_points() -> u16 {
    256
}
const fn one() -> u16 {
    1
}
const fn scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    device: Device,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    address: u32,
    #[serde(default)]
    data_type: DataType,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    elements: u16,
    #[serde(default = "scale")]
    scale: f64,
    #[serde(default)]
    offset: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Device {
    R,
    B,
    Mr,
    Lr,
    Cr,
    Vb,
    Dm,
    Em,
    Zf,
    W,
    Tm,
    Cm,
    Vm,
    Tc,
    Ts,
    Cc,
    Cs,
}

impl Device {
    const fn bit(self) -> bool {
        matches!(
            self,
            Self::R | Self::B | Self::Mr | Self::Lr | Self::Cr | Self::Vb
        )
    }
    const fn native32(self) -> bool {
        matches!(self, Self::Tc | Self::Ts | Self::Cc | Self::Cs)
    }
    const fn maximum(self) -> u32 {
        match self {
            Self::R => 31_999,
            Self::B | Self::W => 0x7fff,
            Self::Mr | Self::Vb => 63_999,
            Self::Lr => 15_999,
            Self::Cr => 1279,
            Self::Dm | Self::Em => 65_534,
            Self::Zf => 524_287,
            Self::Tm => 511,
            Self::Cm => 5999,
            Self::Vm => 589_823,
            Self::Tc | Self::Ts | Self::Cc | Self::Cs => 3999,
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::R => "R",
            Self::B => "B",
            Self::Mr => "MR",
            Self::Lr => "LR",
            Self::Cr => "CR",
            Self::Vb => "VB",
            Self::Dm => "DM",
            Self::Em => "EM",
            Self::Zf => "ZF",
            Self::W => "W",
            Self::Tm => "TM",
            Self::Cm => "CM",
            Self::Vm => "VM",
            Self::Tc => "TC",
            Self::Ts => "TS",
            Self::Cc => "CC",
            Self::Cs => "CS",
        }
    }
    fn label(self, address: u32) -> String {
        let name = self.name();
        match self {
            Self::R | Self::Mr | Self::Lr | Self::Cr => {
                let native = address / 16 * 100 + address % 16;
                format!("{name}{native}")
            }
            Self::B | Self::Vb | Self::W => format!("{name}{address:X}"),
            _ => format!("{name}{address}"),
        }
    }
    fn point_cap(self, configured: u16) -> u16 {
        if self.native32() {
            configured.min(64)
        } else if matches!(self, Self::Tm) {
            configured.min(512)
        } else {
            configured
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Format {
    Bit,
    U16,
    I16,
    U32,
    I32,
    Hex,
}

impl Format {
    const fn for_type(data_type: DataType) -> Self {
        match data_type {
            DataType::Bool => Self::Bit,
            DataType::U16 => Self::U16,
            DataType::I16 => Self::I16,
            DataType::U32 => Self::U32,
            DataType::I32 => Self::I32,
            DataType::U64 | DataType::I64 | DataType::F32 | DataType::F64 => Self::Hex,
        }
    }
    const fn suffix(self) -> &'static str {
        match self {
            Self::Bit => "",
            Self::U16 => ".U",
            Self::I16 => ".S",
            Self::U32 => ".D",
            Self::I32 => ".L",
            Self::Hex => ".H",
        }
    }
    const fn maximum_width(self) -> usize {
        match self {
            Self::Bit => 1,
            Self::U16 => 5,
            Self::I16 => 6,
            Self::U32 => 10,
            Self::I32 => 11,
            Self::Hex => 4,
        }
    }
}

impl Signal {
    fn address_stride(&self) -> u16 {
        if self.device.native32() {
            1
        } else {
            self.data_type.words().max(1)
        }
    }
    fn tokens_per_element(&self) -> u16 {
        if matches!(Format::for_type(self.data_type), Format::Hex) {
            self.data_type.words()
        } else {
            1
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0
        || !(1..=1000).contains(&config.max_points)
        || !(1..=256).contains(&config.signals.len())
    {
        return Err(Usage(
            "Keyence KV requires a nonzero TCP port, max_points 1–1000 and 1–256 signals".into(),
        )
        .into());
    }
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "Keyence KV signal")?;
        if !names.insert(&signal.name)
            || !(1..=4096).contains(&signal.elements)
            || signal.device.bit() != matches!(signal.data_type, DataType::Bool)
            || (signal.device.native32()
                && !matches!(
                    signal.data_type,
                    DataType::U16 | DataType::I16 | DataType::U32 | DataType::I32
                ))
            || !signal.data_type.valid_scale(signal.scale, signal.offset)
        {
            return Err(Usage("Keyence KV needs unique signals, 1–4096 elements, bool contacts, integer timer values, and compatible numeric scaling".into()).into());
        }
        let stride = signal.address_stride();
        let extent = u32::from(signal.elements) * u32::from(stride);
        if stride > signal.device.point_cap(config.max_points)
            || signal
                .address
                .checked_add(extent - 1)
                .is_none_or(|last| last > signal.device.maximum())
        {
            return Err(Usage(
                "Keyence KV signal exceeds the documented address or request limit".into(),
            )
            .into());
        }
    }
    Ok(Box::new(Client {
        config: config.clone(),
        stream: None,
    }))
}

struct Client {
    config: Config,
    stream: Option<BufReader<Box<dyn net::Transport>>>,
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("Keyence KV timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.stream = None;
        }
        result
    }
}

impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.stream.is_none() {
            let socket = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            socket.set_nodelay(true).map_err(|error| io_error(&error))?;
            self.stream = Some(BufReader::with_capacity(4096, net::plain(socket, deadline)));
        }
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        stream.get_mut().set_deadline(deadline);
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for signal in &self.config.signals {
            let reading = Reading {
                signal: signal.name.clone(),
                value: read_signal(stream, signal, self.config.max_points, deadline)?,
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

fn read_signal(
    stream: &mut BufReader<Box<dyn net::Transport>>,
    signal: &Signal,
    max_points: u16,
    deadline: Instant,
) -> Result<Value> {
    let format = Format::for_type(signal.data_type);
    let stride = signal.address_stride();
    let tokens_per_element = signal.tokens_per_element();
    let maximum = signal.device.point_cap(max_points) / stride;
    let mut decoded = Vec::with_capacity(
        usize::from(signal.elements) * usize::from(signal.data_type.words().max(1)) * 2,
    );
    let mut done = 0;
    while done < signal.elements {
        remaining(deadline)?;
        let elements = (signal.elements - done).min(maximum);
        let tokens = elements * tokens_per_element;
        let address = signal.address + u32::from(done) * u32::from(stride);
        let label = signal.device.label(address);
        let suffix = format.suffix();
        let request = if tokens == 1 {
            format!("RD {label}{suffix}\r")
        } else {
            format!("RDS {label}{suffix} {tokens}\r")
        };
        stream
            .get_mut()
            .write_all(request.as_bytes())
            .map_err(|error| io_error(&error))?;
        let maximum = (usize::from(tokens) * (format.maximum_width() + 1) + 1).max(4);
        let frame = line(stream, maximum)?;
        if let [b'E', code @ b'0'..=b'6'] = frame.as_slice() {
            return Err(Categorized::new(
                ErrorKind::OperationFailed,
                "Keyence KV PLC rejected read",
            )
            .details(json!({"keyence_error_code": format!("E{}", char::from(*code))}))
            .into());
        }
        let mut count = 0;
        for token in frame.split(|byte| *byte == b' ') {
            value_bytes(format, token, &mut decoded)?;
            count += 1;
        }
        if count != usize::from(tokens) {
            return Err(protocol("reply token count differs from requested points"));
        }
        done += elements;
    }
    if matches!(format, Format::Bit) {
        if signal.elements == 1 {
            return Ok(Value::Bool(decoded[0] == 1));
        }
        return Ok(Value::Array(
            decoded
                .into_iter()
                .map(|value| Value::Bool(value == 1))
                .collect(),
        ));
    }
    signal.data_type.decode(
        signal.elements,
        signal.scale,
        signal.offset,
        &decoded,
        protocol,
    )
}

fn line(stream: &mut BufReader<Box<dyn net::Transport>>, maximum: usize) -> Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(maximum);
    let count = (&mut *stream)
        .take(u64::try_from(maximum + 1)?)
        .read_until(b'\n', &mut frame)
        .map_err(|error| io_error(&error))?;
    if count == 0 {
        return Err(Categorized::error(
            ErrorKind::Disconnected,
            "Keyence KV connection closed during reply",
        ));
    }
    if count > maximum || !frame.ends_with(b"\r\n") {
        return Err(protocol(
            "reply exceeds the configured width or is missing CRLF",
        ));
    }
    if !stream.buffer().is_empty() {
        return Err(protocol("unexpected bytes after the reply terminator"));
    }
    frame.truncate(frame.len() - 2);
    Ok(frame)
}

fn value_bytes(format: Format, token: &[u8], output: &mut Vec<u8>) -> Result<()> {
    match format {
        Format::Bit => match token {
            [value @ (b'0' | b'1')] => output.push(value - b'0'),
            _ => return Err(protocol("bit response must be one 0 or 1")),
        },
        Format::Hex => {
            if token.len() != 4 {
                return Err(protocol("hex word requires exactly four digits"));
            }
            let value = token.iter().try_fold(0_u16, |value, byte| {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => return Err(protocol("invalid hexadecimal word digit")),
                };
                Ok((value << 4) | u16::from(digit))
            })?;
            output.extend(value.to_le_bytes());
        }
        Format::U16 => output.extend(
            u16::try_from(unsigned(token, 5)?)
                .map_err(|_| protocol("unsigned value exceeds 16 bits"))?
                .to_le_bytes(),
        ),
        Format::I16 => output.extend(
            i16::try_from(signed(token, 5, false)?)
                .map_err(|_| protocol("signed value exceeds 16 bits"))?
                .to_le_bytes(),
        ),
        Format::U32 => output.extend(
            u32::try_from(unsigned(token, 10)?)
                .map_err(|_| protocol("unsigned value exceeds 32 bits"))?
                .to_le_bytes(),
        ),
        Format::I32 => output.extend(
            i32::try_from(signed(token, 10, true)?)
                .map_err(|_| protocol("signed value exceeds 32 bits"))?
                .to_le_bytes(),
        ),
    }
    Ok(())
}

fn unsigned(token: &[u8], width: usize) -> Result<u64> {
    if token.len() != width || !token.iter().all(u8::is_ascii_digit) {
        return Err(protocol(
            "decimal response width or digits differ from the selected format",
        ));
    }
    token.iter().try_fold(0_u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or_else(|| protocol("decimal response overflow"))
    })
}

fn signed(token: &[u8], digits: usize, bare_zero: bool) -> Result<i64> {
    // The vendor documents signless zero for the .L format.
    if bare_zero && token.len() == digits && token.iter().all(|byte| *byte == b'0') {
        return Ok(0);
    }
    let [sign @ (b'+' | b'-'), data @ ..] = token else {
        return Err(protocol("signed response requires a leading sign"));
    };
    let value = i64::try_from(unsigned(data, digits)?)
        .map_err(|_| protocol("signed decimal response overflow"))?;
    Ok(if *sign == b'-' { -value } else { value })
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "Keyence KV poll deadline expired",
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
    Categorized::error(kind, format!("Keyence KV I/O: {error}"))
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("Keyence KV: {message}"))
}
