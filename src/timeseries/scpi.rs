//! Query-only SCPI over configured TCP, native serial and END-bounded transports.
//! Keysight Truevolt/DM34460 manuals document `FETCh?`, numeric replies and raw
//! sockets; Keithley DMM6500 documents `TRACe:DATA?` and port 5025:
//! <https://download.tek.com/manual/DMM6500-901-01B_Sept_2019_Ref.pdf>.
//! Tektronix TBS2000 documents raw socket mode, `CURVe?`, waveform preambles and
//! definite blocks: <https://download.tek.com/manual/TBS2000-Programmer-077114902.pdf>.
//! R&S FSH-K40 documents ASCII vectors and REAL32 definite-length trace blocks:
//! <https://scdn.rohde-schwarz.com/ur/pws/dl_downloads/dl_manuals/dl_manual/FSH-K40_OperatingManual_en_FW3.60.pdf>.
//! Instrument setup, output control, VISA and GPIB are separate.
//! VXI-11 core transport retains END boundaries and sends only configured queries.
mod hislip;
pub(super) mod usbtmc;
mod vxi11;

use super::{Reading, Source, net, serial};
use crate::{
    storage,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    io::{self, Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_BATCH: usize = 1 << 20;
const MAX_PREFIX: usize = 16 * 1024;
const MAX_BLOCK: u64 = 1 << 30;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub connection: ConnectionConfig,
    #[serde(default)]
    pub write_terminator: Terminator,
    pub signals: Vec<Signal>,
}
#[derive(Debug, Clone)]
pub(crate) enum ConnectionConfig {
    Usbtmc(usbtmc::Config),
    Vxi11(vxi11::Config),
    Hislip(hislip::Config),
    Tcp {
        address: SocketAddr,
    },
    Serial {
        device: String,
        baud: u32,
        parity: serial::Parity,
        data_bits: u8,
        stop_bits: u8,
    },
}
impl<'de> Deserialize<'de> for ConnectionConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Tcp {
            address: SocketAddr,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Serial {
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
        // Avoid serde's tagged Content buffer turning arbitrary-precision
        // decimal numbers into private maps before the typed fields see them.
        let mut value = crate::session::strict_json_value(deserializer)?;
        let transport = value
            .as_object_mut()
            .and_then(|fields| fields.remove("transport"))
            .ok_or_else(|| serde::de::Error::custom("SCPI connection requires transport"))?;
        match transport.as_str() {
            Some("usbtmc") => serde_json::from_value::<usbtmc::Config>(value)
                .map(Self::Usbtmc)
                .map_err(serde::de::Error::custom),
            Some("hislip") => serde_json::from_value::<hislip::Config>(value)
                .map(Self::Hislip)
                .map_err(serde::de::Error::custom),
            Some("vxi11") => serde_json::from_value::<vxi11::Config>(value)
                .map(Self::Vxi11)
                .map_err(serde::de::Error::custom),
            Some("tcp") => serde_json::from_value::<Tcp>(value)
                .map(|tcp| Self::Tcp {
                    address: tcp.address,
                })
                .map_err(serde::de::Error::custom),
            Some("serial") => serde_json::from_value::<Serial>(value)
                .map(|serial| Self::Serial {
                    device: serial.device,
                    baud: serial.baud,
                    parity: serial.parity,
                    data_bits: serial.data_bits,
                    stop_bits: serial.stop_bits,
                })
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "SCPI connection transport must be tcp, serial, vxi11, hislip or usbtmc",
            )),
        }
    }
}

impl ConnectionConfig {
    const fn end_bounded(&self) -> bool {
        matches!(self, Self::Vxi11(_) | Self::Hislip(_) | Self::Usbtmc(_))
    }
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

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Terminator {
    #[default]
    Lf,
    #[serde(rename = "crlf")]
    CrLf,
    Cr,
    None,
}
impl Terminator {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
            Self::Cr => b"\r",
            Self::None => b"",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Format {
    Text,
    #[default]
    Number,
    Numbers,
    ErrorQueue,
    BinaryBlock,
}
impl Format {
    const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Number => "number",
            Self::Numbers => "numbers",
            Self::ErrorQueue => "error_queue",
            Self::BinaryBlock => "binary_block",
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub response_prefix: String,
    #[serde(default)]
    pub response_terminator: Terminator,
    #[serde(default)]
    pub sentinels: Vec<Sentinel>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub max_bytes: Option<u64>,
    #[serde(default)]
    pub data_type: Option<DataType>,
    #[serde(default)]
    pub byte_order: Option<ByteOrder>,
    #[serde(default)]
    pub unit: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sentinel {
    pub token: String,
    pub status: String,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}
impl DataType {
    const fn width(self) -> u64 {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::U8 => "u8",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::I32 => "i32",
            Self::U32 => "u32",
            Self::I64 => "i64",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ByteOrder {
    Big,
    Little,
}
impl ByteOrder {
    const fn name(self) -> &'static str {
        match self {
            Self::Big => "big",
            Self::Little => "little",
        }
    }
}
impl Signal {
    fn limit(&self) -> u64 {
        self.max_bytes
            .unwrap_or(if self.format == Format::BinaryBlock {
                256 * 1024 * 1024
            } else {
                64 * 1024
            })
    }
}

pub(crate) fn preflight(config: &Config) -> Result<()> {
    if let ConnectionConfig::Usbtmc(selector) = &config.connection {
        selector.validate()?;
        #[cfg(not(feature = "usb"))]
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "SCPI USBTMC requires the usb feature",
        ));
    }
    Ok(())
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    preflight(config)?;
    if config.signals.is_empty()
        || config.signals.len() > 256
        || (config.write_terminator.bytes().is_empty() && !config.connection.end_bounded())
    {
        return Err(
            Usage("SCPI requires 1–256 queries and a nonempty write terminator".into()).into(),
        );
    }
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "SCPI signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("SCPI signal names must be unique".into()).into());
        }
        validate_query(&signal.query)?;
        if signal.response_prefix.len() > MAX_PREFIX
            || !signal
                .response_prefix
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || matches!(byte, b' ' | b'\t'))
        {
            return Err(Usage(
                "SCPI response_prefix must be at most16 KiB of printable ASCII".into(),
            )
            .into());
        }
        if signal.limit() == 0
            || signal.limit()
                > if signal.format == Format::BinaryBlock {
                    MAX_BLOCK
                } else {
                    u64::try_from(MAX_BATCH)?
                }
        {
            return Err(Usage(
                "SCPI max_bytes must be 1–1MiB for lines or 1–1GiB for binary blocks".into(),
            )
            .into());
        }
        if signal.format == Format::BinaryBlock {
            validate_output(
                signal
                    .output
                    .as_deref()
                    .ok_or_else(|| Usage("SCPI binary_block requires an output path".into()))?,
            )?;
        } else if signal.output.is_some()
            || signal.data_type.is_some()
            || signal.byte_order.is_some()
            || (signal.response_terminator.bytes().is_empty() && !config.connection.end_bounded())
        {
            return Err(Usage("SCPI output/data_type/byte_order apply only to binary_block; an absent line terminator requires vxi11, hislip or usbtmc".into()).into());
        }
        if signal.unit.as_ref().is_some_and(|unit| {
            unit.is_empty() || unit.len() > 128 || unit.chars().any(char::is_control)
        }) {
            return Err(Usage(
                "SCPI unit must be nonempty text up to128 bytes without controls".into(),
            )
            .into());
        }
        if signal.sentinels.len() > 64
            || (!signal.sentinels.is_empty()
                && !matches!(signal.format, Format::Number | Format::Numbers))
        {
            return Err(Usage(
                "SCPI accepts at most64 explicit sentinel tokens on numeric queries".into(),
            )
            .into());
        }
        let mut tokens = BTreeSet::new();
        for sentinel in &signal.sentinels {
            if sentinel.token.is_empty()
                || sentinel.token.len() > 256
                || sentinel.token.trim_ascii() != sentinel.token
                || !sentinel.token.bytes().all(|byte| byte.is_ascii_graphic())
                || !tokens.insert(&sentinel.token)
            {
                return Err(Usage(
                    "SCPI sentinel tokens must be unique nonempty ASCII tokens up to256 bytes"
                        .into(),
                )
                .into());
            }
            super::validate_name(&sentinel.status, "SCPI sentinel status")?;
        }
    }
    let connection = match &config.connection {
        ConnectionConfig::Usbtmc(config) => {
            config.validate()?;
            #[cfg(feature = "usb")]
            {
                Connection::Usbtmc(Box::new(usbtmc::Client::new(config)?))
            }
            #[cfg(not(feature = "usb"))]
            {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "SCPI USBTMC requires the usb feature",
                ));
            }
        }
        ConnectionConfig::Hislip(config) => {
            Connection::Hislip(Box::new(hislip::Client::new(config)?))
        }
        ConnectionConfig::Vxi11(config) => Connection::Vxi11(Box::new(vxi11::Client::new(config)?)),
        ConnectionConfig::Tcp { address } => {
            if address.port() == 0 {
                return Err(Usage("SCPI TCP port must be nonzero".into()).into());
            }
            Connection::Tcp {
                address: *address,
                stream: None,
            }
        }
        ConnectionConfig::Serial {
            device,
            baud,
            parity,
            data_bits,
            stop_bits,
        } => {
            if device.is_empty() {
                return Err(Usage("SCPI serial device must be nonempty".into()).into());
            }
            serial::validate(*baud, *stop_bits, *data_bits)?;
            #[cfg(any(unix, windows))]
            {
                Connection::Serial(serial::open(
                    device, *baud, *parity, *stop_bits, *data_bits,
                )?)
            }
            #[cfg(not(any(unix, windows)))]
            {
                let _ = parity;
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "SCPI serial requires a Unix terminal or Windows COM port",
                ));
            }
        }
    };
    Ok(Box::new(Capture {
        config: Arc::new(config.clone()),
        connection,
        reader: Reader::default(),
        sequence: 0,
        observed: None,
        failed: false,
    }))
}

fn validate_output(output: &str) -> Result<()> {
    let literal = output.replace("{seq}", "1");
    if output.is_empty()
        || output.len() > 16 * 1024
        || output.contains("://")
        || literal.contains(['{', '}', '\0', '\n', '\r'])
        || output.ends_with(['/', '\\'])
        || Path::new(&literal).file_name().is_none()
    {
        return Err(Usage(
            "SCPI output must be a local file path, optionally containing {seq}".into(),
        )
        .into());
    }
    crate::naming::validate_output(&literal)
}
fn validate_query(query: &str) -> Result<()> {
    let invalid = || {
        Usage("SCPI query must contain one ASCII query header ending in ?, optional arguments, and no unquoted semicolons or control characters".into())
    };
    if query.is_empty()
        || query.len() > 4096
        || !query
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || matches!(byte, b' ' | b'\t'))
    {
        return Err(invalid().into());
    }
    let header = query.split_ascii_whitespace().next().ok_or_else(invalid)?;
    let header = header.strip_suffix('?').ok_or_else(invalid)?;
    let valid = header.strip_prefix('*').map_or_else(
        || {
            header.trim_start_matches(':').split(':').all(|part| {
                part.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            }) && !header.starts_with("::")
        },
        |common| !common.is_empty() && common.bytes().all(|byte| byte.is_ascii_alphabetic()),
    );
    if !valid || query.as_bytes()[0].is_ascii_whitespace() {
        return Err(invalid().into());
    }
    let mut quote = None;
    let mut bytes = query.as_bytes()[header.len()..].iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if let Some(active) = quote {
            if byte == active {
                if bytes.peek() == Some(&active) {
                    bytes.next();
                } else {
                    quote = None;
                }
            }
        } else if byte == b';' {
            return Err(invalid().into());
        } else if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
        }
    }
    if quote.is_some() {
        return Err(invalid().into());
    }
    Ok(())
}

enum Connection {
    #[cfg(feature = "usb")]
    Usbtmc(Box<usbtmc::Client>),
    Vxi11(Box<vxi11::Client>),
    Hislip(Box<hislip::Client>),
    Tcp {
        address: SocketAddr,
        stream: Option<Box<dyn net::Transport>>,
    },
    #[cfg(any(unix, windows))]
    Serial(serial::Port),
}
impl Connection {
    const fn end_bounded(&self) -> bool {
        #[cfg(any(unix, windows))]
        {
            !matches!(self, Self::Tcp { .. } | Self::Serial(_))
        }
        #[cfg(not(any(unix, windows)))]
        {
            !matches!(self, Self::Tcp { .. })
        }
    }
    #[cfg(feature = "usb")]
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        if let Self::Usbtmc(client) = self {
            client.set_acquisition_end(end_ns);
        }
    }

    fn begin(&mut self, deadline: Instant) -> Result<()> {
        #[cfg(feature = "usb")]
        if let Self::Usbtmc(client) = self {
            return client.begin(deadline);
        }
        if let Self::Hislip(client) = self {
            return client.begin(deadline);
        }
        if let Self::Vxi11(client) = self {
            return client.begin(deadline);
        }
        if let Self::Tcp { address, stream } = self {
            if stream.is_none() {
                let left = remaining(deadline)?;
                let socket =
                    TcpStream::connect_timeout(address, left).map_err(|error| io_error(&error))?;
                socket.set_nodelay(true).map_err(|error| io_error(&error))?;
                *stream = Some(net::plain(socket, deadline));
            }
            if let Some(stream) = stream {
                stream.set_deadline(deadline);
            }
        }
        Ok(())
    }
    fn receive(&mut self, bytes: &mut [u8], deadline: Instant) -> Result<usize> {
        match self {
            #[cfg(feature = "usb")]
            Self::Usbtmc(client) => client.receive(bytes, deadline),
            Self::Vxi11(client) => client.receive(bytes, deadline),
            Self::Hislip(client) => client.receive(bytes, deadline),
            Self::Tcp { stream, .. } => stream
                .as_mut()
                .ok_or_else(|| {
                    Categorized::error(ErrorKind::Disconnected, "SCPI TCP session is closed")
                })?
                .read(bytes)
                .map_err(|error| io_error(&error)),
            #[cfg(any(unix, windows))]
            Self::Serial(port) => port
                .receive(bytes, deadline)
                .map_err(|error| io_error(&error))?
                .ok_or_else(|| {
                    Categorized::error(ErrorKind::Timeout, "SCPI serial read deadline expired")
                }),
        }
    }

    fn send_query(&mut self, query: &[u8], terminator: &[u8], deadline: Instant) -> Result<()> {
        #[cfg(feature = "usb")]
        if let Self::Usbtmc(client) = self {
            return client.send_query(&[query, terminator].concat(), deadline);
        }
        if let Self::Hislip(client) = self {
            return client.send_query(&[query, terminator].concat(), deadline);
        }
        if let Self::Vxi11(client) = self {
            let bytes = [query, terminator].concat();
            return client.send_query(&bytes, deadline);
        }
        self.send(query, deadline)
            .and_then(|()| self.send(terminator, deadline))
            .map_err(|error| io_error(&error))
    }

    fn ended(&self) -> bool {
        match self {
            #[cfg(feature = "usb")]
            Self::Usbtmc(client) => client.ended(),
            Self::Vxi11(client) => client.ended(),
            Self::Hislip(client) => client.ended(),
            _ => false,
        }
    }
    fn received(&self) -> Option<u64> {
        match self {
            #[cfg(feature = "usb")]
            Self::Usbtmc(client) => client.observed(),
            Self::Vxi11(client) => client.observed(),
            Self::Hislip(client) => client.observed(),
            _ => None,
        }
    }

    fn receive_clock(&self) -> Option<Result<u64>> {
        let received = self.received();
        #[cfg(feature = "usb")]
        if matches!(self, Self::Usbtmc(_)) {
            return Some(received.filter(|at| *at > 0).ok_or_else(|| {
                Categorized::error(
                    ErrorKind::Unavailable,
                    "USBTMC worker receive clock is unavailable",
                )
            }));
        }
        received.or_else(crate::host::monotonic_ns).map(Ok)
    }

    fn complete(&mut self, deadline: Instant) -> Result<()> {
        #[cfg(feature = "usb")]
        if let Self::Usbtmc(client) = self {
            client.complete(deadline)?;
        }
        if let Self::Hislip(client) = self {
            client.complete(deadline)?;
        }
        if let Self::Vxi11(client) = self {
            client.complete(deadline)?;
        }
        Ok(())
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        #[cfg(feature = "usb")]
        if let Self::Usbtmc(client) = self {
            client.finish(timeout)?;
        }
        if let Self::Hislip(client) = self {
            client.finish()?;
        }
        if let Self::Vxi11(client) = self {
            client.finish(timeout)?;
        }
        Ok(())
    }

    fn send(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<()> {
        match self {
            #[cfg(feature = "usb")]
            Self::Usbtmc(_) => Err(io::Error::other(
                "USBTMC query must be sent as one EOM-bounded transaction",
            )),
            Self::Vxi11(_) | Self::Hislip(_) => Err(io::Error::other(
                "SCPI query must be sent as one END-bounded transaction",
            )),
            Self::Tcp { stream, .. } => stream
                .as_mut()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "SCPI TCP session is closed")
                })?
                .write_all(bytes),
            #[cfg(any(unix, windows))]
            Self::Serial(port) => port.write_all(bytes, deadline),
        }
    }
    fn check(&self) -> Result<()> {
        #[cfg(feature = "usb")]
        if let Self::Usbtmc(client) = self {
            client.check()?;
        }
        if let Self::Hislip(client) = self {
            client.check()?;
        }
        Ok(())
    }
    fn retire(&mut self) {
        #[cfg(feature = "usb")]
        if let Self::Usbtmc(client) = self {
            client.retire();
        }
        if let Self::Hislip(client) = self {
            client.retire();
        }
        if let Self::Vxi11(client) = self {
            client.retire();
        }
        if let Self::Tcp { stream, .. } = self {
            *stream = None;
        }
    }
}
struct Reader {
    bytes: [u8; 8192],
    start: usize,
    end: usize,
    received: Option<u64>,
}
impl Default for Reader {
    fn default() -> Self {
        Self {
            bytes: [0; 8192],
            start: 0,
            end: 0,
            received: None,
        }
    }
}
impl Reader {
    fn byte(&mut self, connection: &mut Connection, deadline: Instant) -> Result<u8> {
        self.next_byte(connection, deadline)?
            .ok_or_else(|| incomplete("instrument ended before a complete reply"))
    }
    fn next_byte(&mut self, connection: &mut Connection, deadline: Instant) -> Result<Option<u8>> {
        if self.start == self.end {
            self.end = connection.receive(&mut self.bytes, deadline)?;
            self.received = connection.receive_clock().transpose()?;
            self.start = 0;
            if self.end == 0 {
                if connection.ended() {
                    return Ok(None);
                }
                return Err(incomplete("instrument closed before a complete reply"));
            }
        }
        let byte = *self
            .bytes
            .get(self.start)
            .ok_or_else(|| incomplete("invalid SCPI receive buffer position"))?;
        self.start += 1;
        Ok(Some(byte))
    }
    fn complete(&mut self, connection: &mut Connection, deadline: Instant) -> Result<()> {
        if connection.end_bounded() && self.start != self.end {
            return Err(incomplete("extra bytes after configured SCPI response"));
        }
        connection.complete(deadline)?;
        if let Some(received) = connection.received() {
            self.received = Some(received);
        }
        Ok(())
    }

    fn exact(
        &mut self,
        connection: &mut Connection,
        mut bytes: &mut [u8],
        deadline: Instant,
    ) -> Result<()> {
        while !bytes.is_empty() {
            let count = if self.start < self.end {
                let count = (self.end - self.start).min(bytes.len());
                bytes[..count].copy_from_slice(&self.bytes[self.start..self.start + count]);
                self.start += count;
                count
            } else {
                let count = connection.receive(bytes, deadline)?;
                self.received = connection.receive_clock().transpose()?;
                count
            };
            if count == 0 {
                return Err(incomplete("instrument closed inside a reply"));
            }
            bytes = &mut bytes[count..];
        }
        Ok(())
    }
    fn observed(&self) -> Result<u64> {
        self.received.ok_or_else(|| {
            Categorized::error(
                ErrorKind::Unavailable,
                "SCPI host receive clock is unavailable",
            )
        })
    }
    fn line(
        &mut self,
        connection: &mut Connection,
        signal: &Signal,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            let Some(byte) = self.next_byte(connection, deadline)? else {
                if signal.response_terminator.bytes().is_empty() {
                    return Ok(bytes);
                }
                return Err(incomplete(
                    "instrument END preceded the configured response terminator",
                ));
            };
            bytes.push(byte);
            if u64::try_from(bytes.len())? > signal.limit() {
                return Err(incomplete("line reply exceeds configured max_bytes"));
            }
            if !signal.response_terminator.bytes().is_empty()
                && bytes.ends_with(signal.response_terminator.bytes())
            {
                return Ok(bytes);
            }
        }
    }
}

struct Capture {
    config: Arc<Config>,
    connection: Connection,
    reader: Reader,
    sequence: u64,
    observed: Option<u64>,
    failed: bool,
}
struct Reply {
    reading: Reading,
    artifacts: Vec<Artifact>,
}
struct Artifact {
    stage: Option<storage::StagedRecording>,
    path: PathBuf,
}
impl Source for Capture {
    #[cfg(feature = "usb")]
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.connection.set_acquisition_end(end_ns);
    }
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.failed {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "SCPI session was retired after a failed poll",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("SCPI timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.failed = true;
            self.connection.retire();
        }
        result
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn ready(&self) -> bool {
        self.sequence > 0
    }
    fn end_timeout_is_idle(&self) -> bool {
        false
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        self.connection.finish(timeout)
    }
}

impl Capture {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        self.connection.begin(deadline)?;
        let sequence = self
            .sequence
            .checked_add(1)
            .context("SCPI poll sequence exhausted")?;
        let mut replies = Vec::with_capacity(self.config.signals.len());
        let mut budget = JsonLimit(2);
        let config = Arc::clone(&self.config);
        let acquired = (|| -> Result<()> {
            for signal in &config.signals {
                remaining(deadline)?;
                if self.reader.start != self.reader.end {
                    return Err(incomplete(
                        "unexpected bytes remain after the preceding query reply",
                    ));
                }
                self.connection.send_query(
                    signal.query.as_bytes(),
                    self.config.write_terminator.bytes(),
                    deadline,
                )?;
                let mut reply = if signal.format == Format::BinaryBlock {
                    self.block(signal, sequence, deadline)?
                } else {
                    self.line(signal, deadline)?
                };
                if serde_json::to_writer(&mut budget, &reply.reading).is_err()
                    || budget.write_all(b",").is_err()
                {
                    return Err(abort_replies(
                        std::slice::from_mut(&mut reply),
                        incomplete("serialized poll exceeds1 MiB"),
                    ));
                }
                replies.push(reply);
            }
            if self.reader.start != self.reader.end {
                return Err(incomplete("unexpected bytes follow the final query reply"));
            }
            remaining(deadline)?;
            self.connection.check()?;
            Ok(())
        })();
        if let Err(error) = acquired {
            return Err(abort_replies(&mut replies, error));
        }
        // The clock records complete replies, before publication/storage delay.
        let observed = self.reader.received;
        let mut published = Vec::new();
        let publication = (|| -> Result<()> {
            for reply in &mut replies {
                for artifact in &mut reply.artifacts {
                    if let Some(stage) = artifact.stage.take() {
                        stage.finish()?;
                        published.push(artifact.path.clone());
                    }
                }
            }
            Ok(())
        })();
        if let Err(mut error) = publication {
            for path in &published {
                if let Err(cleanup) = std::fs::remove_file(path)
                    && cleanup.kind() != io::ErrorKind::NotFound
                {
                    error = error.context(format!(
                        "SCPI could not remove its published artifact {}: {cleanup}",
                        path.display()
                    ));
                }
            }
            return Err(abort_replies(&mut replies, error));
        }
        self.sequence = sequence;
        self.observed = observed;
        Ok(replies.into_iter().map(|reply| reply.reading).collect())
    }
    fn line(&mut self, signal: &Signal, deadline: Instant) -> Result<Reply> {
        let bytes = self.reader.line(&mut self.connection, signal, deadline)?;
        self.reader.complete(&mut self.connection, deadline)?;
        let received = self.reader.observed()?;
        let payload = &bytes[..bytes.len() - signal.response_terminator.bytes().len()];
        // LF-terminated ASCII instruments commonly send CRLF on the wire.
        let payload = if matches!(signal.response_terminator, Terminator::Lf) {
            payload.strip_suffix(b"\r").unwrap_or(payload)
        } else {
            payload
        };
        let payload = payload
            .strip_prefix(signal.response_prefix.as_bytes())
            .ok_or_else(|| incomplete("reply does not match response_prefix"))?;
        let value = match signal.format {
            Format::Text => std::str::from_utf8(payload)
                .map_or_else(|_| binary(payload), |text| Value::String(text.to_owned())),
            Format::Number => numeric(
                signal,
                std::str::from_utf8(payload)
                    .map_err(|_| incomplete("numeric reply is not ASCII"))?
                    .trim_ascii(),
            )?,
            Format::Numbers => {
                let text = std::str::from_utf8(payload)
                    .map_err(|_| incomplete("numeric vector is not ASCII"))?;
                let mut values = Vec::new();
                for token in text.split(',') {
                    if values.len() >= 65_536 {
                        return Err(incomplete("numeric vector exceeds65536 elements"));
                    }
                    values.push(numeric(signal, token.trim_ascii())?);
                }
                Value::Array(values)
            }
            Format::ErrorQueue => error_queue(payload)?,
            Format::BinaryBlock => unreachable!("binary blocks use their length framing"),
        };
        let mut output = json!({"kind":"scpi_reply","query":signal.query,"format":signal.format.name(),"raw":binary(&bytes),"value":value,"received_monotonic_ns":received});
        if signal.format == Format::ErrorQueue {
            output["status"] = if output["value"]["code"].as_i64() == Some(0) {
                "no_error".into()
            } else {
                "instrument_error".into()
            };
        }
        if let Some(unit) = &signal.unit {
            output["unit"] = unit.clone().into();
        }
        Ok(Reply {
            reading: Reading {
                signal: signal.name.clone(),
                value: output,
                source_time: None,
            },
            artifacts: Vec::new(),
        })
    }
    fn block(&mut self, signal: &Signal, sequence: u64, deadline: Instant) -> Result<Reply> {
        let mut prefix = vec![0; signal.response_prefix.len()];
        self.reader
            .exact(&mut self.connection, &mut prefix, deadline)?;
        if prefix != signal.response_prefix.as_bytes() {
            return Err(incomplete("block reply does not match response_prefix"));
        }
        // Retain optional SCPI headers/preambles. A '#' inside a quoted string
        // is metadata rather than the start of the definite-length block.
        let mut quote = None;
        loop {
            let byte = self.reader.byte(&mut self.connection, deadline)?;
            if quote.is_none() && byte == b'#' {
                break;
            }
            if !byte.is_ascii_graphic() && !matches!(byte, b' ' | b'\t') {
                return Err(incomplete(
                    "invalid or terminated prefix before binary block",
                ));
            }
            prefix.push(byte);
            if prefix.len() > MAX_PREFIX {
                return Err(incomplete("binary block prefix exceeds16 KiB"));
            }
            if let Some(active) = quote {
                if byte == active {
                    quote = None;
                }
            } else if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
            }
        }
        let digit = self.reader.byte(&mut self.connection, deadline)?;
        if digit == b'0' {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "SCPI indefinite #0 blocks require transport end-of-message signaling",
            ));
        }
        if !matches!(digit, b'1'..=b'9') {
            return Err(incomplete("invalid IEEE488.2 binary block length header"));
        }
        let mut header = vec![b'#', digit];
        let mut length = vec![0; usize::from(digit - b'0')];
        self.reader
            .exact(&mut self.connection, &mut length, deadline)?;
        if !length.iter().all(u8::is_ascii_digit) {
            return Err(incomplete("binary block length is not decimal"));
        }
        let payload = std::str::from_utf8(&length)?.parse::<u64>()?;
        if payload > signal.limit() {
            return Err(incomplete("binary payload exceeds configured max_bytes"));
        }
        if signal
            .data_type
            .is_some_and(|kind| payload % kind.width() != 0)
        {
            return Err(incomplete(
                "binary payload is not a whole number of configured elements",
            ));
        }
        header.extend_from_slice(&length);
        let offset = u64::try_from(prefix.len())?
            .checked_add(u64::try_from(header.len())?)
            .context("SCPI block offset overflow")?;
        let wire_bytes = offset
            .checked_add(payload)
            .and_then(|bytes| {
                bytes.checked_add(u64::try_from(signal.response_terminator.bytes().len()).ok()?)
            })
            .context("SCPI raw reply size overflow")?;
        let output = signal
            .output
            .as_ref()
            .context("SCPI block output is missing")?
            .replace("{seq}", &format!("{sequence:06}"));
        let path = std::path::absolute(&output)?;
        let mut stage = storage::StagedRecording::create(&path, Some(wire_bytes), None)?;
        let mut digest = Sha256::new();
        let copied = (|| -> Result<()> {
            store(&mut stage, &mut digest, &prefix)?;
            store(&mut stage, &mut digest, &header)?;
            let mut buffer = vec![0; 64 * 1024].into_boxed_slice();
            let mut pending = payload;
            while pending > 0 {
                remaining(deadline)?;
                let count = usize::try_from(pending.min(u64::try_from(buffer.len())?))?;
                self.reader
                    .exact(&mut self.connection, &mut buffer[..count], deadline)?;
                store(&mut stage, &mut digest, &buffer[..count])?;
                pending -= u64::try_from(count)?;
            }
            let mut terminator = vec![0; signal.response_terminator.bytes().len()];
            self.reader
                .exact(&mut self.connection, &mut terminator, deadline)?;
            if terminator != signal.response_terminator.bytes() {
                return Err(incomplete(
                    "binary block response terminator differs from configuration",
                ));
            }
            store(&mut stage, &mut digest, &terminator)?;
            self.reader.complete(&mut self.connection, deadline)?;
            remaining(deadline)?;
            Ok(())
        })();
        if let Err(mut error) = copied {
            if let Err(cleanup) = stage.abort() {
                error = error.context(format!("SCPI partial artifact cleanup failed: {cleanup:#}"));
            }
            return Err(error);
        }
        let received = self.reader.observed()?;
        let mut block = json!({"header":std::str::from_utf8(&header)?,"payload_offset":offset,"payload_bytes":payload});
        if !prefix.is_empty() {
            block["prefix"] = binary(&prefix);
        }
        if let Some(kind) = signal.data_type {
            block["data_type"] = kind.name().into();
            block["elements"] = (payload / kind.width()).into();
        }
        if let Some(order) = signal.byte_order {
            block["byte_order"] = order.name().into();
        }
        let mut metadata_path = path.as_os_str().to_os_string();
        metadata_path.push(".json");
        let metadata_path = PathBuf::from(metadata_path);
        let mut value = json!({"kind":"scpi_reply","query":signal.query,"format":"binary_block","raw":{"encoding":"file","path":path,"bytes":stage.bytes_written(),"sha256":hex(&digest.finalize())},"block":block,"metadata_path":metadata_path,"received_monotonic_ns":received});
        if let Some(unit) = &signal.unit {
            value["unit"] = unit.clone().into();
        }
        let mut reply = Reply {
            reading: Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            },
            artifacts: vec![Artifact {
                stage: Some(stage),
                path,
            }],
        };
        let metadata = (|| -> Result<()> {
            let bytes = serde_json::to_vec(
                &json!({"format_version":1,"signal":signal.name,"poll_sequence":sequence,"response":reply.reading.value}),
            )?;
            let mut stage = storage::StagedRecording::create(
                &metadata_path,
                Some(u64::try_from(bytes.len())?),
                None,
            )?;
            let written = stage.write_all(&bytes);
            let alarm = stage.alarm().error();
            reply.artifacts.push(Artifact {
                stage: Some(stage),
                path: metadata_path,
            });
            if let Some(error) = alarm {
                return Err(error);
            }
            written.context("write SCPI artifact metadata")?;
            Ok(())
        })();
        if let Err(error) = metadata {
            return Err(abort_replies(std::slice::from_mut(&mut reply), error));
        }
        Ok(reply)
    }
}

fn store(stage: &mut storage::StagedRecording, digest: &mut Sha256, bytes: &[u8]) -> Result<()> {
    let written = stage.write_all(bytes);
    if let Some(error) = stage.alarm().error() {
        return Err(error);
    }
    written.context("write SCPI raw artifact")?;
    digest.update(bytes);
    Ok(())
}
fn abort_replies(replies: &mut [Reply], mut error: anyhow::Error) -> anyhow::Error {
    for reply in replies {
        for artifact in &mut reply.artifacts {
            if let Some(stage) = artifact.stage.take()
                && let Err(cleanup) = stage.abort()
            {
                error = error.context(format!("SCPI partial artifact cleanup failed: {cleanup:#}"));
            }
        }
    }
    error
}
fn binary(bytes: &[u8]) -> Value {
    json!({"encoding":"base64","data":STANDARD.encode(bytes)})
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    output
}

fn numeric(signal: &Signal, token: &str) -> Result<Value> {
    if let Some(sentinel) = signal
        .sentinels
        .iter()
        .find(|sentinel| sentinel.token == token)
    {
        return Ok(json!({"kind":"scpi_sentinel","token":token,"status":sentinel.status}));
    }
    let status = if token.eq_ignore_ascii_case("nan") {
        Some("not_a_number")
    } else if ["inf", "+inf", "infinity", "+infinity"]
        .iter()
        .any(|value| token.eq_ignore_ascii_case(value))
    {
        Some("infinity")
    } else if ["-inf", "-infinity"]
        .iter()
        .any(|value| token.eq_ignore_ascii_case(value))
    {
        Some("negative_infinity")
    } else {
        None
    };
    if let Some(status) = status {
        return Ok(json!({"kind":"scpi_special","token":token,"status":status}));
    }
    let canonical = canonical_number(token)?;
    serde_json::from_str(&canonical)
        .map_err(|_| incomplete("SCPI numeric token cannot be represented as exact JSON"))
}
fn canonical_number(token: &str) -> Result<String> {
    let invalid = || incomplete("invalid SCPI numeric token");
    let bytes = token.as_bytes();
    if bytes.is_empty() || bytes.len() > 256 {
        return Err(invalid());
    }
    let mut at = usize::from(matches!(bytes[0], b'+' | b'-'));
    let start = at;
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    let integer = token.get(start..at).ok_or_else(invalid)?;
    let fraction = if bytes.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        Some(token.get(start..at).ok_or_else(invalid)?)
    } else {
        None
    };
    if integer.is_empty() && fraction.is_none_or(str::is_empty) {
        return Err(invalid());
    }
    let exponent = at;
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if start == at {
            return Err(invalid());
        }
    }
    if at != bytes.len() {
        return Err(invalid());
    }
    let mut output = String::new();
    if bytes[0] == b'-' {
        output.push('-');
    }
    let digits = integer.trim_start_matches('0');
    output.push_str(if digits.is_empty() { "0" } else { digits });
    if let Some(fraction) = fraction {
        output.push('.');
        output.push_str(if fraction.is_empty() { "0" } else { fraction });
    }
    output.push_str(token.get(exponent..).ok_or_else(invalid)?);
    Ok(output)
}
fn error_queue(bytes: &[u8]) -> Result<Value> {
    let bytes = bytes.trim_ascii();
    let comma = bytes
        .iter()
        .position(|byte| *byte == b',')
        .ok_or_else(|| incomplete("error_queue reply requires code,quoted message"))?;
    let code = std::str::from_utf8(bytes[..comma].trim_ascii())
        .map_err(|_| incomplete("instrument error code is not ASCII"))?;
    let canonical = canonical_number(code)?;
    if canonical.contains(['.', 'e', 'E']) {
        return Err(incomplete("instrument error code must be an integer"));
    }
    let code: Value = serde_json::from_str(&canonical)?;
    let quoted = bytes[comma + 1..].trim_ascii();
    let quote = quoted
        .first()
        .copied()
        .filter(|quote| matches!(quote, b'\'' | b'"'))
        .ok_or_else(|| incomplete("instrument error message must be quoted"))?;
    if quoted.len() < 2 || quoted.last() != Some(&quote) {
        return Err(incomplete("instrument error message is unterminated"));
    }
    let mut message = Vec::new();
    let mut bytes = quoted[1..quoted.len() - 1].iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == quote && bytes.next() != Some(byte) {
            return Err(incomplete("instrument error quote is not doubled"));
        }
        if byte < 32 || byte == 127 {
            return Err(incomplete("instrument error message contains controls"));
        }
        message.push(byte);
    }
    let message = std::str::from_utf8(&message)
        .map_or_else(|_| binary(&message), |text| Value::String(text.to_owned()));
    Ok(json!({"code":code,"message":message}))
}
struct JsonLimit(usize);
impl io::Write for JsonLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_BATCH {
            return Err(io::Error::other("SCPI poll size limit exceeded"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "SCPI poll exceeded its total read deadline",
        ));
    }
    Ok(left)
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("SCPI: {message}"))
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::UnexpectedEof => ErrorKind::Incomplete,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionAborted => ErrorKind::Disconnected,
        _ => ErrorKind::Unavailable,
    };
    Categorized::error(kind, format!("SCPI I/O failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_config_is_exact_at_boundaries_and_preserves_response_numbers() {
        let config: Config = serde_json::from_str(
            r#"{"connection":{"transport":"serial","device":"meter","baud":9.6e3,"data_bits":8.0,"stop_bits":1e0},"signals":[{"name":"value","query":"FETC?","max_bytes":1e6}]}"#,
        )
        .unwrap();
        assert!(matches!(
            config.connection,
            ConnectionConfig::Serial {
                baud: 9600,
                data_bits: 8,
                stop_bits: 1,
                ..
            }
        ));
        let [signal] = config.signals.as_slice() else {
            panic!("one configured signal expected");
        };
        assert_eq!(signal.max_bytes, Some(1_000_000));
        for (token, expected) in [
            ("1.8446744073709551615e19", u64::MAX),
            ("184467440737095516150e-1", u64::MAX),
            ("9007199254740993.0", 9_007_199_254_740_993),
            ("-0.000e-999999999999999999999", 0),
        ] {
            let signal: Signal = serde_json::from_str(&format!(
                r#"{{"name":"value","query":"FETC?","max_bytes":{token}}}"#
            ))
            .unwrap();
            assert_eq!(signal.max_bytes, Some(expected), "{token}");
        }
        let connection: ConnectionConfig = serde_json::from_str(
            r#"{"transport":"serial","device":"meter","baud":4.294967295e9,"data_bits":255.0,"stop_bits":255e0}"#,
        )
        .unwrap();
        assert!(matches!(
            connection,
            ConnectionConfig::Serial {
                baud: u32::MAX,
                data_bits: u8::MAX,
                stop_bits: u8::MAX,
                ..
            }
        ));
        // Typed configuration conversion must not round numeric data.
        assert_eq!(
            numeric(signal, "1.234567890123456789E+03")
                .unwrap()
                .to_string(),
            "1.234567890123456789e+03"
        );
    }
}
