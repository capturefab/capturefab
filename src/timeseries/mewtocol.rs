//! Read-only Panasonic FP MEWTOCOL-COM over configured bare TCP or serial.
//! Panasonic `WUME-MEWCP-03`, §§1.1–1.3, defines BCC, RD, RCS and RCP.
//! The FP7 LAN manual §8.3.1 documents the bare TCP frame.
use super::{
    Reading, Source, net,
    serial::{self, Parity, SerialSignal},
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

const MAX_ELEMENTS: u16 = 4096;
const MAX_CONTACTS: u16 = 8;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: Option<SocketAddr>,
    serial: Option<Serial>,
    #[serde(default = "unit", deserialize_with = "crate::config_number::unsigned")]
    unit: u8,
    #[serde(default)]
    frame: Frame,
    #[serde(default = "request_bcc")]
    request_bcc: bool,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    max_words: Option<u16>,
    signals: Vec<SerialSignal<Signal>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Serial {
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
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Frame {
    #[default]
    Standard,
    Extended,
}

impl Frame {
    const fn header(self) -> char {
        match self {
            Self::Standard => '%',
            Self::Extended => '<',
        }
    }

    const fn bytes(self) -> usize {
        match self {
            Self::Standard => 118,
            Self::Extended => 2048,
        }
    }

    const fn words(self) -> u16 {
        match self {
            Self::Standard => 27,
            Self::Extended => 509,
        }
    }
}

const fn unit() -> u8 {
    1
}
const fn baud() -> u32 {
    9600
}
const fn parity() -> Parity {
    Parity::Odd
}
const fn data_bits() -> u8 {
    8
}
const fn stop_bits() -> u8 {
    1
}
const fn request_bcc() -> bool {
    true
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
    area: Area,
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
enum Area {
    Dt,
    Ld,
    Fl,
    X,
    Y,
    R,
    L,
    T,
    C,
}

impl Area {
    const fn is_contact(self) -> bool {
        !matches!(self, Self::Dt | Self::Ld | Self::Fl)
    }

    const fn address_limit(self) -> u32 {
        match self {
            Self::Dt | Self::Ld | Self::Fl => 99_999,
            Self::T | Self::C => 9_999,
            Self::X | Self::Y | Self::R | Self::L => 15_999,
        }
    }

    const fn code(self) -> char {
        match self {
            Self::Dt => 'D',
            Self::Ld | Self::L => 'L',
            Self::Fl => 'F',
            Self::X => 'X',
            Self::Y => 'Y',
            Self::R => 'R',
            Self::T => 'T',
            Self::C => 'C',
        }
    }

    fn contact(self, address: u32) -> String {
        let code = self.code();
        if matches!(self, Self::T | Self::C) {
            return format!("{code}{address:04}");
        }
        format!("{code}{:03}{:X}", address / 16, address % 16)
    }
}

impl Config {
    fn word_cap(&self) -> u16 {
        self.max_words.unwrap_or_else(|| self.frame.words())
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.is_some() == config.serial.is_some()
        || config.address.is_some_and(|address| address.port() == 0)
    {
        return Err(Usage(
            "MEWTOCOL requires one TCP address with nonzero port or serial connection".into(),
        )
        .into());
    }
    if !(1..=99).contains(&config.unit) || !(1..=256).contains(&config.signals.len()) {
        return Err(Usage("MEWTOCOL requires a unit from 1 to 99 and 1–256 signals".into()).into());
    }
    let word_cap = config.word_cap();
    if !(1..=config.frame.words()).contains(&word_cap) {
        return Err(Usage("MEWTOCOL max_words exceeds the single-frame word limit".into()).into());
    }
    let mut names = BTreeSet::new();
    for entry in &config.signals {
        let signal = &entry.signal;
        super::validate_name(&signal.name, "MEWTOCOL signal")?;
        if !names.insert(&signal.name) || !(1..=99).contains(&entry.unit(config.unit)) {
            return Err(Usage(
                "MEWTOCOL signals require unique names and units from 1 to 99".into(),
            )
            .into());
        }
        let words = signal.data_type.words();
        if signal.area.is_contact() != (words == 0)
            || words > word_cap
            || !signal.data_type.valid_scale(signal.scale, signal.offset)
        {
            return Err(Usage("MEWTOCOL requires bool for contacts and a compatible numeric type, limits and scaling for registers".into()).into());
        }
        let extent = u32::from(signal.elements) * u32::from(words.max(1));
        if !(1..=MAX_ELEMENTS).contains(&signal.elements)
            || signal
                .address
                .checked_add(extent.saturating_sub(1))
                .is_none_or(|last| last > signal.area.address_limit())
        {
            return Err(Usage(
                "MEWTOCOL signal exceeds the address format or 4096 elements".into(),
            )
            .into());
        }
    }
    let link = config.serial.as_ref().map(open_serial).transpose()?;
    Ok(Box::new(Client {
        config: config.clone(),
        link,
    }))
}

fn open_serial(config: &Serial) -> Result<Link> {
    if config.device.is_empty() || !matches!(config.data_bits, 7 | 8) {
        return Err(Usage("MEWTOCOL serial requires a device and 7 or 8 data bits".into()).into());
    }
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    #[cfg(any(unix, windows))]
    {
        let port = serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            config.data_bits,
        )?;
        port.discard_input().map_err(|error| io_error(&error))?;
        Ok(Link::Serial(port))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = config.parity;
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "MEWTOCOL serial requires a Unix terminal or Windows COM port",
        ))
    }
}

enum Link {
    Tcp(Box<dyn net::Transport>),
    #[cfg(any(unix, windows))]
    Serial(serial::Port),
}

impl Link {
    fn send(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => {
                stream.set_deadline(deadline);
                stream.write_all(bytes)
            }
            #[cfg(any(unix, windows))]
            Self::Serial(port) => port.write_all(bytes, deadline),
        }
    }

    fn receive(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => {
                stream.set_deadline(deadline);
                stream.read(bytes)
            }
            #[cfg(any(unix, windows))]
            Self::Serial(port) => port.receive(bytes, deadline)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "serial read deadline expired")
            }),
        }
    }
}

struct Client {
    config: Config,
    link: Option<Link>,
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("MEWTOCOL timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.link = None;
        }
        result
    }
}

impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.link.is_none() {
            let address = self
                .config
                .address
                .ok_or_else(|| protocol("serial link closed"))?;
            let socket = TcpStream::connect_timeout(&address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            socket.set_nodelay(true).map_err(|error| io_error(&error))?;
            self.link = Some(Link::Tcp(net::plain(socket, deadline)));
        }
        let link = self
            .link
            .as_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for entry in &self.config.signals {
            let reading = Reading {
                signal: entry.signal.name.clone(),
                value: read_signal(
                    link,
                    &self.config,
                    entry.unit(self.config.unit),
                    &entry.signal,
                    deadline,
                )?,
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
    link: &mut Link,
    config: &Config,
    unit: u8,
    signal: &Signal,
    deadline: Instant,
) -> Result<Value> {
    let words = signal.data_type.words();
    let chunk = config.word_cap().checked_div(words).unwrap_or(MAX_CONTACTS);
    let prefix = format!("{}{unit:02}", config.frame.header());
    let mut decoded =
        Vec::with_capacity(usize::from(signal.elements) * usize::from(words.max(1)) * 2);
    let mut done = 0;
    while done < signal.elements {
        let count = (signal.elements - done).min(chunk);
        let first = signal.address + u32::from(done) * u32::from(words.max(1));
        let command = if words == 0 {
            contact_request(signal.area, first, count)
        } else {
            let last = first + u32::from(count) * u32::from(words) - 1;
            format!("RD{}{first:05}{last:05}", signal.area.code())
        };
        let mut request = format!("{prefix}#{command}").into_bytes();
        if config.request_bcc {
            request.extend_from_slice(format!("{:02X}", checksum(&request)).as_bytes());
        } else {
            request.extend_from_slice(b"**");
        }
        request.push(b'\r');
        link.send(&request, deadline)
            .map_err(|error| io_error(&error))?;
        let data_bytes = usize::from(count)
            * if words == 0 {
                1
            } else {
                usize::from(words) * 4
            };
        let response = receive_frame(link, config.frame.bytes(), deadline)?;
        // Validate the CR, BCC, source and exact status/command before
        // interpreting any values.
        let echo = if words == 0 { *b"RC" } else { *b"RD" };
        let payload = response_data(&response, prefix.as_bytes(), echo, data_bytes)?;
        if words == 0 {
            if !payload.iter().all(|byte| matches!(byte, b'0' | b'1')) {
                return Err(protocol("contact data must be 0 or 1"));
            }
            decoded.extend_from_slice(payload);
        } else {
            for &pair in payload.as_chunks::<2>().0 {
                decoded.push(hex(pair)?);
            }
        }
        done += count;
    }
    if words != 0 {
        return signal.data_type.decode(
            signal.elements,
            signal.scale,
            signal.offset,
            &decoded,
            protocol,
        );
    }
    if signal.elements == 1 {
        Ok(Value::Bool(decoded[0] == b'1'))
    } else {
        Ok(Value::Array(
            decoded
                .into_iter()
                .map(|byte| Value::Bool(byte == b'1'))
                .collect(),
        ))
    }
}

fn contact_request(area: Area, first: u32, count: u16) -> String {
    if count == 1 {
        return format!("RCS{}", area.contact(first));
    }
    let mut text = format!("RCP{count}");
    for offset in 0..u32::from(count) {
        text.push_str(&area.contact(first + offset));
    }
    text
}

fn receive_frame(link: &mut Link, maximum: usize, deadline: Instant) -> Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(maximum);
    let mut bytes = [0; 512];
    while frame.len() < maximum {
        let capacity = (maximum - frame.len()).min(bytes.len());
        let count = match link.receive(&mut bytes[..capacity], deadline) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|error| io_error(&error))?,
        };
        if count == 0 {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "MEWTOCOL connection closed during reply",
            ));
        }
        let received = &bytes[..count];
        if let Some(end) = received.iter().position(|byte| *byte == b'\r') {
            if end + 1 != count {
                return Err(protocol("unexpected bytes after the reply terminator"));
            }
            frame.extend_from_slice(&received[..end]);
            return Ok(frame);
        }
        frame.extend_from_slice(received);
    }
    Err(protocol(
        "reply exceeds the configured single-frame limit or is missing CR",
    ))
}

fn response_data<'a>(
    response: &'a [u8],
    prefix: &[u8],
    echo: [u8; 2],
    expected: usize,
) -> Result<&'a [u8]> {
    if response.len() < 8 {
        return Err(protocol("truncated reply"));
    }
    let (body, tail) = response.split_at(response.len() - 2);
    let bcc: [u8; 2] = tail.try_into()?;
    if hex(bcc)? != checksum(body) {
        return Err(protocol("reply BCC differs from received bytes"));
    }
    if &body[..3] != prefix {
        return Err(protocol("reply header or station differs from request"));
    }
    match &body[3..] {
        [b'!', high, low] => {
            let status = hex([*high, *low])?;
            Err(
                Categorized::new(ErrorKind::OperationFailed, "MEWTOCOL PLC rejected read")
                    .details(json!({"mewtocol_error_code": status}))
                    .into(),
            )
        }
        [b'$', a, b, data @ ..] if [*a, *b] == echo => {
            if data.len() != expected {
                return Err(protocol("reply value length differs from requested points"));
            }
            Ok(data)
        }
        _ => Err(protocol("reply command or status differs from request")),
    }
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, byte| sum ^ byte)
}

fn hex([high, low]: [u8; 2]) -> Result<u8> {
    let digit = |byte| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(protocol("reply contains an invalid hexadecimal digit")),
    };
    Ok((digit(high)? << 4) | digit(low)?)
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(timeout());
    }
    Ok(left)
}

fn io_error(error: &io::Error) -> anyhow::Error {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => timeout(),
        io::ErrorKind::PermissionDenied => Categorized::error(
            ErrorKind::PermissionDenied,
            format!("MEWTOCOL I/O: {error}"),
        ),
        _ => Categorized::error(ErrorKind::Disconnected, format!("MEWTOCOL I/O: {error}")),
    }
}

fn timeout() -> anyhow::Error {
    Categorized::error(ErrorKind::Timeout, "MEWTOCOL poll deadline expired")
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("MEWTOCOL: {message}"))
}
