//! Read-only PLC values over Beckhoff ADS/AMS TCP, without a vendor runtime.
//!
//! AMS/TCP framing and commands follow Beckhoff's ADS specification:
//! <https://infosys.beckhoff.com/content/1033/tcadscommon/12440289931.html>
//! <https://infosys.beckhoff.com/content/1033/tcadscommon/12495372427.html>
//! <https://download.beckhoff.com/download/document/automation/twincat3/TwinCAT_3_ADS_INTRO_EN.pdf>
//! `TwinCAT` STRING storage and encoding: <https://infosys.beckhoff.com/content/1033/tc3_plc_intro/2529410443.html>
//! Symbol handle creation/release are metadata services only. No PLC data,
//! state, route, notification or program-writing services are emitted.
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read as _, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const READ: u16 = 2;
const WRITE: u16 = 3;
const READ_WRITE: u16 = 9;
const HANDLE_BY_NAME: u32 = 0xf003;
const VALUE_BY_HANDLE: u32 = 0xf005;
const RELEASE_HANDLE: u32 = 0xf006;
const MAX_READ: usize = 65_536;
const MAX_BATCH: usize = 1 << 20;
const AMS_HEADER: usize = 32;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Numeric TCP endpoint, normally port 48898.
    pub address: SocketAddr,
    pub target_net_id: String,
    #[serde(default = "target_port")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub target_port: u16,
    pub source_net_id: String,
    #[serde(default = "source_port")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub source_port: u16,
    pub signals: Vec<Signal>,
}
const fn target_port() -> u16 {
    851
}
const fn source_port() -> u16 {
    32_905
}
const fn one() -> u16 {
    1
}
const fn scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub index_group: Option<u32>,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub index_offset: Option<u32>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub data_type: DataType,
    #[serde(default = "one")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub elements: u16,
    /// Byte width of an opaque buffer or one string slot, including termination.
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub length: Option<u32>,
    #[serde(default)]
    pub encoding: Option<StringEncoding>,
    #[serde(default = "scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataType {
    Bool,
    U8,
    I8,
    #[default]
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Bytes,
    String,
    Wstring,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StringEncoding {
    Utf8,
    Latin1,
}

impl DataType {
    const fn width(self) -> Option<usize> {
        match self {
            Self::Bool | Self::U8 | Self::I8 => Some(1),
            Self::U16 | Self::I16 => Some(2),
            Self::U32 | Self::I32 | Self::F32 => Some(4),
            Self::U64 | Self::I64 | Self::F64 => Some(8),
            Self::Bytes | Self::String | Self::Wstring => None,
        }
    }
}

fn endpoint(net_id: &str, port: u16) -> Result<[u8; 8]> {
    if port == 0 {
        return Err(Usage("ADS AMS ports must be nonzero".into()).into());
    }
    let parts: Vec<_> = net_id.split('.').collect();
    if parts.len() != 6 {
        return Err(Usage(
            "ADS AMS Net IDs require six decimal octets, e.g. 192.0.2.10.1.1".into(),
        )
        .into());
    }
    let mut address = [0; 8];
    for (output, part) in address[..6].iter_mut().zip(parts) {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Usage("ADS AMS Net IDs require six decimal octets".into()).into());
        }
        *output = part
            .parse()
            .map_err(|_| Usage("ADS AMS Net ID octets must be 0–255".into()))?;
    }
    address[6..].copy_from_slice(&port.to_le_bytes());
    Ok(address)
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 || config.signals.is_empty() || config.signals.len() > 256 {
        return Err(Usage("ADS requires a nonzero TCP port and 1–256 signals".into()).into());
    }
    let target = endpoint(&config.target_net_id, config.target_port)?;
    let source = endpoint(&config.source_net_id, config.source_port)?;
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "ADS signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("ADS signal names must be unique".into()).into());
        }
        match (&signal.symbol, signal.index_group, signal.index_offset) {
            (Some(symbol), None, None) => {
                if symbol.is_empty() || symbol.len() > 1024 || symbol.chars().any(char::is_control)
                {
                    return Err(Usage("ADS symbols must be nonempty UTF-8 text up to 1024 bytes without control characters".into()).into());
                }
            }
            (None, Some(_), Some(_)) => {}
            _ => {
                return Err(Usage(
                    "ADS signals require either symbol or both index_group/index_offset".into(),
                )
                .into());
            }
        }
        signal.bytes()?;
        let unscaled = matches!(
            signal.data_type,
            DataType::Bool
                | DataType::U64
                | DataType::I64
                | DataType::Bytes
                | DataType::String
                | DataType::Wstring
        );
        if !signal.scale.is_finite()
            || !signal.offset.is_finite()
            || (unscaled && (signal.scale != 1.0 || signal.offset != 0.0))
        {
            return Err(Usage("ADS scale/offset must be finite; bool, 64-bit integers, bytes and strings cannot be scaled".into()).into());
        }
    }
    Ok(Box::new(Client {
        config: config.clone(),
        target,
        source,
        connection: None,
    }))
}

impl Signal {
    fn slot_bytes(&self) -> Result<usize> {
        if let Some(width) = self.data_type.width() {
            if self.length.is_some() {
                return Err(Usage("ADS numeric/bool signals do not accept length".into()).into());
            }
            return Ok(width);
        }
        let bytes = usize::try_from(self.length.ok_or_else(|| {
            Usage("ADS bytes/string/wstring signals require length in bytes".into())
        })?)?;
        if matches!(self.data_type, DataType::Wstring) && (bytes < 2 || !bytes.is_multiple_of(2)) {
            return Err(
                Usage("ADS wstring length must be an even byte count, at least 2".into()).into(),
            );
        }
        Ok(bytes)
    }
    fn bytes(&self) -> Result<usize> {
        if !(1..=4096).contains(&self.elements) {
            return Err(Usage("ADS signal elements must be 1–4096".into()).into());
        }
        if self.elements != 1 && matches!(self.data_type, DataType::Bytes) {
            return Err(Usage("ADS bytes signals require elements 1".into()).into());
        }
        if self.encoding.is_some() && !matches!(self.data_type, DataType::String) {
            return Err(Usage("ADS encoding only applies to data_type string".into()).into());
        }
        self.slot_bytes()?
            .checked_mul(usize::from(self.elements))
            .filter(|bytes| (1..=MAX_READ).contains(bytes))
            .ok_or_else(|| Usage("ADS signal read length must be 1–65536 bytes".into()).into())
    }
    fn decode(&self, bytes: &[u8]) -> Result<Value> {
        let total = self.bytes()?;
        if bytes.len() != total {
            return Err(protocol(
                "response length differs from configured signal width",
            ));
        }
        if self.elements == 1 {
            return self.scalar(bytes);
        }
        let width = total / usize::from(self.elements);
        let values = bytes
            .chunks_exact(width)
            .map(|value| self.scalar(value))
            .collect::<Result<Vec<_>>>()?;
        Ok(Value::Array(values))
    }
    fn scalar(&self, bytes: &[u8]) -> Result<Value> {
        let value = match self.data_type {
            DataType::Bool => {
                return match bytes {
                    [0] => Ok(json!(false)),
                    [1] => Ok(json!(true)),
                    _ => Err(protocol("BOOL must be encoded as 0 or 1")),
                };
            }
            DataType::U8 => json!(u8::from_le_bytes(bytes.try_into()?)),
            DataType::I8 => json!(i8::from_le_bytes(bytes.try_into()?)),
            DataType::U16 => json!(u16::from_le_bytes(bytes.try_into()?)),
            DataType::I16 => json!(i16::from_le_bytes(bytes.try_into()?)),
            DataType::U32 => json!(u32::from_le_bytes(bytes.try_into()?)),
            DataType::I32 => json!(i32::from_le_bytes(bytes.try_into()?)),
            DataType::U64 => return Ok(json!(u64::from_le_bytes(bytes.try_into()?))),
            DataType::I64 => return Ok(json!(i64::from_le_bytes(bytes.try_into()?))),
            DataType::F32 => {
                let value = f32::from_le_bytes(bytes.try_into()?);
                if !value.is_finite() {
                    return Err(protocol("non-finite floating-point value"));
                }
                json!(value)
            }
            DataType::F64 => {
                let value = f64::from_le_bytes(bytes.try_into()?);
                if !value.is_finite() {
                    return Err(protocol("non-finite floating-point value"));
                }
                json!(value)
            }
            DataType::Bytes => {
                return Ok(
                    json!({"encoding":"base64", "data":base64::engine::general_purpose::STANDARD.encode(bytes)}),
                );
            }
            DataType::String => {
                let end = bytes
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(|| protocol("STRING is missing its NUL terminator"))?;
                let text = match self.encoding.unwrap_or(StringEncoding::Utf8) {
                    StringEncoding::Utf8 => std::str::from_utf8(&bytes[..end])
                        .map_err(|_| protocol("STRING is not UTF-8. Configure encoding latin1 for a Latin-1 variable or use data_type bytes."))?
                        .to_owned(),
                    StringEncoding::Latin1 => bytes[..end].iter().copied().map(char::from).collect(),
                };
                return Ok(Value::String(text));
            }
            DataType::Wstring => {
                let mut units = Vec::new();
                for pair in bytes.as_chunks::<2>().0 {
                    let unit = u16::from_le_bytes(*pair);
                    if unit == 0 {
                        let text = String::from_utf16(&units)
                            .map_err(|_| protocol("WSTRING contains invalid UTF-16"))?;
                        return Ok(Value::String(text));
                    }
                    units.push(unit);
                }
                return Err(protocol("WSTRING is missing its NUL terminator"));
            }
        };
        if self.scale == 1.0 && self.offset == 0.0 {
            return Ok(value);
        }
        let scaled = value
            .as_f64()
            .ok_or_else(|| protocol("invalid numeric value"))?
            * self.scale
            + self.offset;
        if !scaled.is_finite() {
            return Err(protocol("non-finite scaled value"));
        }
        Ok(json!(scaled))
    }
}

struct Client {
    config: Config,
    target: [u8; 8],
    source: [u8; 8],
    connection: Option<Connection>,
}
struct Connection {
    stream: TcpStream,
    target: [u8; 8],
    source: [u8; 8],
    invoke: u32,
    handles: BTreeMap<String, u32>,
    usable: bool,
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("ADS timeout is too large".into()))?;
        if self.connection.is_none() {
            let stream = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            stream.set_nodelay(true)?;
            self.connection = Some(Connection {
                stream,
                target: self.target,
                source: self.source,
                invoke: 0,
                handles: BTreeMap::new(),
                usable: true,
            });
        }
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for signal in &self.config.signals {
            let (group, offset) = if let Some(symbol) = &signal.symbol {
                (VALUE_BY_HANDLE, connection.handle(symbol, deadline)?)
            } else {
                (
                    signal
                        .index_group
                        .ok_or_else(|| protocol("index group missing"))?,
                    signal
                        .index_offset
                        .ok_or_else(|| protocol("index offset missing"))?,
                )
            };
            let mut request = Vec::new();
            request.extend_from_slice(&group.to_le_bytes());
            request.extend_from_slice(&offset.to_le_bytes());
            request.extend_from_slice(&u32::try_from(signal.bytes()?)?.to_le_bytes());
            let response = connection.exchange(READ, &request, signal.bytes()? + 8, deadline)?;
            let value = signal.decode(read_response(&response)?)?;
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
        Ok(readings)
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        let Some(mut connection) = self.connection.take() else {
            return Ok(());
        };
        if connection.handles.is_empty() {
            return Ok(());
        }
        if !connection.usable {
            return Err(protocol(
                "connection failed before symbol handles could be released",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout.min(Duration::from_secs(1)))
            .ok_or_else(|| protocol("cleanup deadline overflow"))?;
        let handles = std::mem::take(&mut connection.handles);
        let mut failure = None;
        for handle in handles.into_values() {
            if !connection.usable {
                break;
            }
            let mut request = Vec::new();
            request.extend_from_slice(&RELEASE_HANDLE.to_le_bytes());
            request.extend_from_slice(&0u32.to_le_bytes());
            request.extend_from_slice(&4u32.to_le_bytes());
            request.extend_from_slice(&handle.to_le_bytes());
            let result = connection
                .exchange(WRITE, &request, 4, deadline)
                .and_then(|data| write_response(&data));
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Connection {
    fn handle(&mut self, symbol: &str, deadline: Instant) -> Result<u32> {
        if let Some(handle) = self.handles.get(symbol) {
            return Ok(*handle);
        }
        let mut request = Vec::new();
        request.extend_from_slice(&HANDLE_BY_NAME.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes());
        request.extend_from_slice(&4u32.to_le_bytes());
        request.extend_from_slice(&u32::try_from(symbol.len() + 1)?.to_le_bytes());
        request.extend_from_slice(symbol.as_bytes());
        request.push(0);
        let response = self.exchange(READ_WRITE, &request, 12, deadline)?;
        let data = read_response(&response)?;
        let bytes: [u8; 4] = data
            .try_into()
            .map_err(|_| protocol("symbol handle response must contain exactly four bytes"))?;
        let handle = u32::from_le_bytes(bytes);
        self.handles.insert(symbol.to_owned(), handle);
        Ok(handle)
    }
    fn exchange(
        &mut self,
        command: u16,
        data: &[u8],
        max_response: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        if !self.usable {
            return Err(protocol(
                "connection is no longer usable after a framing/I/O failure",
            ));
        }
        self.invoke = self.invoke.wrapping_add(1).max(1);
        let mut packet = vec![0, 0];
        packet.extend_from_slice(&u32::try_from(AMS_HEADER + data.len())?.to_le_bytes());
        packet.extend_from_slice(&self.target);
        packet.extend_from_slice(&self.source);
        packet.extend_from_slice(&command.to_le_bytes());
        packet.extend_from_slice(&4u16.to_le_bytes());
        packet.extend_from_slice(&u32::try_from(data.len())?.to_le_bytes());
        packet.extend_from_slice(&0u32.to_le_bytes());
        packet.extend_from_slice(&self.invoke.to_le_bytes());
        packet.extend_from_slice(data);
        // Mark unusable until an entire matching response has been consumed.
        // A timed-out partial frame cannot be mistaken for another operation.
        self.usable = false;
        write(&mut self.stream, &packet, deadline)?;
        let mut tcp = [0; 6];
        read(&mut self.stream, &mut tcp, deadline)?;
        let length = usize::try_from(u32::from_le_bytes(tcp[2..].try_into()?))?;
        if tcp[..2] != [0, 0] || !(AMS_HEADER..=AMS_HEADER + max_response).contains(&length) {
            return Err(protocol("invalid or oversized AMS/TCP frame length"));
        }
        let mut packet = vec![0; length];
        read(&mut self.stream, &mut packet, deadline)?;
        let response = response(&packet, self.source, self.target, command, self.invoke)?;
        self.usable = true;
        Ok(response.to_vec())
    }
}

fn response(
    packet: &[u8],
    target: [u8; 8],
    source: [u8; 8],
    command: u16,
    invoke: u32,
) -> Result<&[u8]> {
    let Some(header) = packet.first_chunk::<AMS_HEADER>() else {
        return Err(protocol("truncated AMS header"));
    };
    if header[..8] != target
        || header[8..16] != source
        || header[16..18] != command.to_le_bytes()
        || header[18..20] != 5u16.to_le_bytes()
        || header[28..32] != invoke.to_le_bytes()
    {
        return Err(protocol(
            "AMS response addresses, command, flags or invoke ID differ from request",
        ));
    }
    let length = usize::try_from(u32::from_le_bytes(header[20..24].try_into()?))?;
    if length != packet.len() - AMS_HEADER {
        return Err(protocol("AMS header length differs from packet"));
    }
    let result = u32::from_le_bytes(header[24..28].try_into()?);
    if result != 0 {
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            "AMS router or target rejected request",
        )
        .details(json!({"ams_error":result}))
        .into());
    }
    Ok(&packet[AMS_HEADER..])
}
fn read_response(data: &[u8]) -> Result<&[u8]> {
    let Some(header) = data.first_chunk::<8>() else {
        return Err(protocol("truncated ADS read response"));
    };
    ads_result(u32::from_le_bytes(header[..4].try_into()?))?;
    let length = usize::try_from(u32::from_le_bytes(header[4..].try_into()?))?;
    if length != data.len() - 8 {
        return Err(protocol("ADS returned length differs from payload"));
    }
    Ok(&data[8..])
}
fn write_response(data: &[u8]) -> Result<()> {
    let bytes: [u8; 4] = data
        .try_into()
        .map_err(|_| protocol("symbol release response must contain exactly four bytes"))?;
    ads_result(u32::from_le_bytes(bytes))
}
fn ads_result(result: u32) -> Result<()> {
    if result == 0 {
        return Ok(());
    }
    Err(
        Categorized::new(ErrorKind::OperationFailed, "ADS target rejected service")
            .details(json!({"ads_error":result}))
            .into(),
    )
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("ADS: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "ADS operation deadline expired",
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
    Categorized::error(kind, format!("ADS I/O: {error}"))
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
                    "ADS connection closed during response",
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
                    "ADS connection closed during request",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Ok(())
}
struct SizeLimit {
    bytes: usize,
}
impl Write for SizeLimit {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(data.len());
        if self.bytes > MAX_BATCH {
            return Err(io::Error::other("ADS batch exceeds size limit"));
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_config_addresses_and_optional_lengths_preserve_ads_read_limits() {
        let config: Config = serde_json::from_str(
            r#"{"address":"127.0.0.1:48898","target_net_id":"192.0.2.1.1.1","target_port":8.51e2,"source_net_id":"192.0.2.2.1.1","source_port":3.2905e4,"signals":[{"name":"counter","index_group":4.294967295e9,"index_offset":4.294967294e9,"elements":1.0,"length":null,"data_type":"u32"},{"name":"raw","index_group":1e0,"index_offset":-0.0,"length":6.5536e4,"data_type":"bytes"}]}"#,
        )
        .unwrap();
        assert_eq!(config.target_port, 851);
        assert_eq!(config.source_port, 32905);
        let [counter, raw] = config.signals.as_slice() else {
            panic!("two configured signals expected");
        };
        assert_eq!(counter.index_group, Some(u32::MAX));
        assert_eq!(counter.index_offset, Some(u32::MAX - 1));
        assert_eq!(counter.length, None);
        assert_eq!(raw.index_offset, Some(0));
        assert_eq!(raw.elements, 1);
        assert_eq!(raw.bytes().unwrap(), MAX_READ);
        open(&config).unwrap();
        let mut invalid = config.clone();
        invalid.signals[1].length = Some(u32::try_from(MAX_READ + 1).unwrap());
        open(&invalid).err().unwrap();
        invalid.signals[1].length = Some(0);
        open(&invalid).err().unwrap();
    }
}
