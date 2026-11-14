//! Read-only Omron FINS/TCP and FINS/UDP memory access.
//! W421 sections 7-3 and 7-4 define UDP framing and TCP node negotiation.
//! W342 section 5-3 defines MEMORY AREA READ (01 01) and CS/CJ/CP memory-area codes.
//! <https://files.omron.eu/downloads/latest/manual/en/w421_cj1w-etn21_cs1w-etn21_ethernet_units_-_construction_of_applications_operation_manual_en.pdf>
//! <https://assets.omron.eu/downloads/latest/manual/en/w342_cs_cj_cp_nsj_communications_commands_reference_manual_en.pdf>
pub(crate) mod host_link;

use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read as _, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket},
    thread,
    time::{Duration, Instant},
};

const MAX_BATCH: usize = 1 << 20;
// W342's Ethernet limit is 999 words; a response's FINS frame is at most 2012
// bytes: ten routing bytes, command and end code (four), and 1998 data bytes.
const MAX_DATA: u16 = 1998;
const MAX_FRAME: usize = 2012;
const SID_COUNT: usize = 256;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    /// Request gateway count: legacy three-level routes or configured eight-level routes.
    #[serde(
        default = "default_gateway_count",
        deserialize_with = "deserialize_gateway_count"
    )]
    pub gateway_count: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub source_network: u8,
    /// Zero requests automatic FINS/TCP client node assignment.
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub source_node: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub source_unit: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub target_network: u8,
    /// Omit to use the server's node from TCP negotiation.
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub target_node: Option<u8>,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub target_unit: u8,
    pub signals: Vec<Signal>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UdpConfig {
    pub address: SocketAddrV4,
    pub bind: Option<SocketAddrV4>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub reply_horizon_ms: Option<u64>,
    #[serde(
        default = "default_gateway_count",
        deserialize_with = "deserialize_gateway_count"
    )]
    pub gateway_count: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub source_network: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub source_node: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub source_unit: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub target_network: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub target_node: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub target_unit: u8,
    pub signals: Vec<Signal>,
}

const fn default_gateway_count() -> u8 {
    2
}
fn deserialize_gateway_count<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u8, D::Error> {
    let invalid = || serde::de::Error::custom("FINS gateway_count must be exactly 2 or 7");
    let count: u8 = crate::config_number::unsigned(deserializer).map_err(|_| invalid())?;
    if !matches!(count, 2 | 7) {
        return Err(invalid());
    }
    Ok(count)
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
    pub area: Area,
    /// Explicit EM bank index; only valid with the EM area.
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub bank: Option<u8>,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub address: u16,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub bit: Option<u8>,
    #[serde(default)]
    pub data_type: DataType,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    pub elements: u16,
    #[serde(default)]
    pub word_order: WordOrder,
    #[serde(default)]
    pub byte_order: ByteOrder,
    #[serde(default = "scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Area {
    #[default]
    Dm,
    Cio,
    Wr,
    Hr,
    Ar,
    Em,
    Timer,
    Counter,
}
impl Area {
    const fn timer_counter(self) -> bool {
        matches!(self, Self::Timer | Self::Counter)
    }

    const fn code(self, bits: bool, bank: u8) -> u8 {
        // W342 section 5-3-2: high EM banks have separate bit codes, rather
        // than the high-bit mask used by the original memory areas.
        let (word, bit) = match self {
            Self::Dm => (0x82, 0x02),
            Self::Cio => (0xb0, 0x30),
            Self::Wr => (0xb1, 0x31),
            Self::Hr => (0xb2, 0x32),
            Self::Ar => (0xb3, 0x33),
            Self::Timer | Self::Counter => (0x89, 0x09),
            Self::Em if bank < 16 => (0xa0 + bank, 0x20 + bank),
            Self::Em => (0x60 + bank - 16, 0xe0 + bank - 16),
        };
        if bits { bit } else { word }
    }
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WordOrder {
    #[default]
    Little,
    Big,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ByteOrder {
    Little,
    #[default]
    Big,
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
    const fn words(self) -> u16 {
        match self {
            Self::Bool => 0,
            Self::U16 | Self::I16 => 1,
            Self::U32 | Self::I32 | Self::F32 => 2,
            Self::U64 | Self::I64 | Self::F64 => 4,
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 || config.signals.is_empty() || config.signals.len() > 256 {
        return Err(Usage("FINS/TCP requires a nonzero TCP port and 1–256 signals".into()).into());
    }
    if !matches!(config.gateway_count, 2 | 7) {
        return Err(Usage("FINS gateway_count must be exactly 2 or 7".into()).into());
    }
    if config.source_network > 127
        || config.target_network > 127
        || config.source_node == 255
        || config
            .target_node
            .is_some_and(|node| node == 0 || node == 255)
        || config.source_unit == 255
        || config.target_unit == 255
    {
        return Err(Usage(
            "FINS networks must be 0–127, nodes 1–254 (source_node 0 negotiates), and units 0–254"
                .into(),
        )
        .into());
    }
    validate_signals(config.signals.iter())?;
    Ok(Box::new(Client {
        config: config.clone(),
        connection: None,
    }))
}

pub(crate) fn open_udp(config: &UdpConfig, timeout_ms: u64) -> Result<Box<dyn Source>> {
    let peer = config.address.ip();
    if config.address.port() == 0
        || peer.is_unspecified()
        || peer.is_multicast()
        || peer.is_broadcast()
        || config.signals.is_empty()
        || config.signals.len() > SID_COUNT
    {
        return Err(Usage(
            "FINS/UDP requires a unicast IPv4 endpoint, nonzero port and 1–256 signals".into(),
        )
        .into());
    }
    if !matches!(config.gateway_count, 2 | 7) {
        return Err(Usage("FINS gateway_count must be exactly 2 or 7".into()).into());
    }
    if config.source_network > 127
        || config.target_network > 127
        || !(1..=254).contains(&config.source_node)
        || !(1..=254).contains(&config.target_node)
        || config.source_unit == 255
        || config.target_unit == 255
    {
        return Err(Usage(
            "FINS/UDP networks must be 0–127, explicit nodes 1–254, and units 0–254".into(),
        )
        .into());
    }
    let horizon = config.reply_horizon_ms.unwrap_or(timeout_ms);
    if !(timeout_ms..=30_000).contains(&horizon) {
        return Err(Usage(
            "FINS/UDP reply_horizon_ms must be at least timeout_ms and at most 30000".into(),
        )
        .into());
    }
    let bind = config
        .bind
        .unwrap_or(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
    if bind.ip().is_multicast() || bind.ip().is_broadcast() {
        return Err(Usage(
            "FINS/UDP bind must be a local unicast or wildcard IPv4 endpoint".into(),
        )
        .into());
    }
    validate_signals(config.signals.iter())?;
    let binding = if bind.port() == 0 {
        UdpBinding::Ephemeral {
            rotate: false,
            retired: BTreeMap::new(),
        }
    } else {
        UdpBinding::Fixed(Box::new([None; SID_COUNT]))
    };
    Ok(Box::new(UdpClient {
        signals: config.signals.clone(),
        datagrams: Datagrams {
            address: config.address,
            bind,
            source: [
                config.source_network,
                config.source_node,
                config.source_unit,
            ],
            target: [
                config.target_network,
                config.target_node,
                config.target_unit,
            ],
            gateway_count: config.gateway_count,
            horizon: Duration::from_millis(horizon),
            binding,
            socket: None,
            sid: 0,
        },
    }))
}

fn validate_signals<'a, I>(signals: I) -> Result<()>
where
    I: Iterator<Item = &'a Signal>,
{
    let mut names = BTreeSet::new();
    for signal in signals {
        super::validate_name(&signal.name, "FINS signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("FINS signal names must be unique".into()).into());
        }
        let em = matches!(signal.area, Area::Em);
        if em {
            if signal.bank.is_none_or(|bank| bank > 24) {
                return Err(Usage("FINS EM requires an explicit bank from 0 to 24".into()).into());
            }
        } else if signal.bank.is_some() {
            return Err(Usage("FINS bank only applies to the EM area".into()).into());
        }
        let timer_counter = signal.area.timer_counter();
        if timer_counter
            && (!matches!(
                signal.data_type,
                DataType::Bool | DataType::U16 | DataType::I16
            ) || signal.bit.is_some())
        {
            return Err(Usage(
                "FINS timer/counter requires u16/i16 PVs or bool completion flags without a bit field"
                    .into(),
            )
            .into());
        }
        let bits = signal.bits();
        if signal.bit.is_some_and(|bit| bit > 15) || (!bits && signal.bit.is_some()) {
            return Err(Usage("FINS bit must be 0–15 and only applies to bool".into()).into());
        }
        let count = u32::from(signal.count()?);
        let maximum_word = if timer_counter {
            4095
        } else if em {
            32767
        } else {
            u16::MAX
        };
        let (start, maximum) = if bits && !timer_counter {
            (
                u32::from(signal.address) * 16 + u32::from(signal.bit.unwrap_or(0)),
                u32::from(maximum_word) * 16 + 15,
            )
        } else {
            (u32::from(signal.address), u32::from(maximum_word))
        };
        if start + count - 1 > maximum {
            let message = if timer_counter {
                "FINS timer/counter range must stay within logical numbers 0–4095"
            } else if em {
                "FINS EM signal range exceeds the bank's 0–32767 word address space"
            } else {
                "FINS signal range exceeds the 16-bit word address space"
            };
            return Err(Usage(message.into()).into());
        }
        if !signal.scale.is_finite()
            || !signal.offset.is_finite()
            || (matches!(
                signal.data_type,
                DataType::Bool | DataType::U64 | DataType::I64
            ) && (signal.scale != 1.0 || signal.offset != 0.0))
        {
            return Err(Usage(
                "FINS scale/offset must be finite; bool and 64-bit integers cannot be scaled"
                    .into(),
            )
            .into());
        }
    }
    Ok(())
}
impl Signal {
    const fn bits(&self) -> bool {
        matches!(self.data_type, DataType::Bool)
    }
    fn count(&self) -> Result<u16> {
        let count = self.elements.checked_mul(self.data_type.words().max(1));
        let maximum = if self.bits() { MAX_DATA } else { MAX_DATA / 2 };
        match count {
            Some(count) if (1..=maximum).contains(&count) => Ok(count),
            _ => Err(Usage("FINS read must contain 1–999 words or 1–1998 bits".into()).into()),
        }
    }
    fn bytes(&self) -> Result<usize> {
        Ok(usize::from(self.count()?) * if self.bits() { 1 } else { 2 })
    }
    fn decode(&self, data: &[u8]) -> Result<Value> {
        if data.len() != self.bytes()? {
            return Err(protocol(
                "response length differs from configured signal width",
            ));
        }
        let values = if self.bits() {
            data.iter()
                .map(|byte| match byte {
                    0 => Ok(json!(false)),
                    1 => Ok(json!(true)),
                    _ => Err(protocol("bit value must be encoded as 0 or 1")),
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            data.chunks_exact(usize::from(self.data_type.words()) * 2)
                .map(|bytes| self.scalar(bytes))
                .collect::<Result<Vec<_>>>()?
        };
        if self.elements == 1 {
            values
                .into_iter()
                .next()
                .ok_or_else(|| protocol("scalar value missing"))
        } else {
            Ok(Value::Array(values))
        }
    }
    fn scalar(&self, data: &[u8]) -> Result<Value> {
        let mut ordered = [0; 8];
        for (index, word) in data.as_chunks::<2>().0.iter().enumerate() {
            let position = match self.word_order {
                WordOrder::Little => index * 2,
                WordOrder::Big => data.len() - index * 2 - 2,
            };
            let pair = match self.byte_order {
                ByteOrder::Little => *word,
                ByteOrder::Big => [word[1], word[0]],
            };
            ordered[position..position + 2].copy_from_slice(&pair);
        }
        let bytes = &ordered[..data.len()];
        let value = match self.data_type {
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
            DataType::Bool => return Err(protocol("BOOL decoded as a word")),
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
    connection: Option<Connection>,
}
struct Connection {
    stream: TcpStream,
    source: [u8; 3],
    target: [u8; 3],
    sid: u8,
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = poll_deadline(timeout)?;
        if self.connection.is_none() {
            self.connection = Some(Connection::connect(&self.config, deadline)?);
        }
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        let readings = read_poll(
            self.config.signals.iter().map(|signal| (signal, ())),
            |signal, ()| {
                connection.sid = connection.sid.wrapping_add(1).max(1);
                let request = memory_request(
                    signal,
                    self.config.gateway_count,
                    connection.source,
                    connection.target,
                    connection.sid,
                )?;
                send(&mut connection.stream, 2, &request, deadline)?;
                let data = receive(
                    &mut connection.stream,
                    2,
                    8 + 14 + signal.bytes()?,
                    deadline,
                )?;
                signal.decode(memory_reply(&request, &data)?)
            },
        )?;
        // Match UDP and direct FA: native decoding and whole-poll JSON sizing
        // consume the same original deadline as setup and transport I/O.
        remaining(deadline)?;
        Ok(readings)
    }
}

fn poll_deadline(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Usage("FINS timeout is too large".into()).into())
}

fn read_poll<'a, T>(
    signals: impl ExactSizeIterator<Item = (&'a Signal, T)>,
    mut read: impl FnMut(&'a Signal, T) -> Result<Value>,
) -> Result<Vec<Reading>> {
    let mut readings = Vec::with_capacity(signals.len());
    let mut size = SizeLimit { bytes: 2 };
    for (signal, target) in signals {
        let value = read(signal, target)?;
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

fn memory_request(
    signal: &Signal,
    gateway_count: u8,
    source: [u8; 3],
    target: [u8; 3],
    sid: u8,
) -> Result<[u8; 18]> {
    let mut request = [0; 18];
    request[..3].copy_from_slice(&[0x80, 0, gateway_count]);
    request[3..6].copy_from_slice(&target);
    request[6..9].copy_from_slice(&source);
    request[9] = sid;
    request[10..].copy_from_slice(&memory_command(signal)?);
    Ok(request)
}

fn memory_command(signal: &Signal) -> Result<[u8; 8]> {
    // W342 §5-2-2: completion flags use a logical timer/counter number,
    // never the ordinary word*16 bit address. Counters occupy 8000–8FFF.
    let address = if matches!(signal.area, Area::Counter) {
        signal.address | 0x8000
    } else {
        signal.address
    };
    let [address_high, address_low] = address.to_be_bytes();
    let [count_high, count_low] = signal.count()?.to_be_bytes();
    Ok([
        1,
        1,
        signal.area.code(signal.bits(), signal.bank.unwrap_or(0)),
        address_high,
        address_low,
        signal.bit.unwrap_or(0),
        count_high,
        count_low,
    ])
}

fn memory_reply<'a>(request: &[u8; 18], data: &'a [u8]) -> Result<&'a [u8]> {
    let Some(header) = data.first_chunk::<14>() else {
        return Err(protocol("truncated FINS read response"));
    };
    // W342 section 3-3-3: response ICF bits 1–5, RSV and GCT are
    // system-owned and must not be checked by host applications. Retain
    // ICF's fixed bit 7, response classification and response-request bit.
    if header[0] & 0xc1 != 0xc0
        || header[3..6] != request[6..9]
        || header[6..9] != request[3..6]
        || header[9..12] != request[9..12]
    {
        return Err(protocol(
            "response flags, routing, SID or command differ from request",
        ));
    }
    memory_result(&data[12..])
}

fn memory_result(data: &[u8]) -> Result<&[u8]> {
    let Some((&end_code, payload)) = data.split_first_chunk::<2>() else {
        return Err(protocol("truncated FINS read response"));
    };
    let status = u16::from_be_bytes(end_code);
    if status != 0 {
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            "FINS target rejected memory read",
        )
        .details(json!({"fins_end_code":status}))
        .into());
    }
    Ok(payload)
}

impl Connection {
    fn connect(config: &Config, deadline: Instant) -> Result<Self> {
        let mut stream = TcpStream::connect_timeout(&config.address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        stream.set_nodelay(true).map_err(|error| io_error(&error))?;
        send(
            &mut stream,
            0,
            &u32::from(config.source_node).to_be_bytes(),
            deadline,
        )?;
        let data = receive(&mut stream, 1, 16, deadline)?;
        if data.len() != 8 {
            return Err(protocol(
                "node negotiation response must contain exactly two node addresses",
            ));
        }
        let source = u32::from_be_bytes(data[..4].try_into()?);
        let target = u32::from_be_bytes(data[4..].try_into()?);
        if !(1..=254).contains(&source)
            || !(1..=254).contains(&target)
            || (config.source_node != 0 && source != u32::from(config.source_node))
        {
            return Err(protocol("invalid or unexpected negotiated FINS nodes"));
        }
        Ok(Self {
            stream,
            source: [
                config.source_network,
                u8::try_from(source)?,
                config.source_unit,
            ],
            target: [
                config.target_network,
                config.target_node.unwrap_or(u8::try_from(target)?),
                config.target_unit,
            ],
            sid: 0,
        })
    }
}

struct UdpClient {
    signals: Vec<Signal>,
    datagrams: Datagrams,
}

struct Datagrams {
    address: SocketAddrV4,
    bind: SocketAddrV4,
    source: [u8; 3],
    target: [u8; 3],
    gateway_count: u8,
    horizon: Duration,
    binding: UdpBinding,
    socket: Option<UdpSocket>,
    sid: u8,
}

enum UdpBinding {
    Ephemeral {
        rotate: bool,
        retired: BTreeMap<u16, Instant>,
    },
    Fixed(Box<[Option<Instant>; SID_COUNT]>),
}

impl Source for UdpClient {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = poll_deadline(timeout)?;
        let result = read_poll(
            self.signals.iter().map(|signal| (signal, ())),
            |signal, ()| self.datagrams.read(signal, deadline),
        )
        .and_then(|readings| {
            remaining(deadline)?;
            Ok(readings)
        });
        if result.is_err()
            && let UdpBinding::Ephemeral { rotate, .. } = &mut self.datagrams.binding
        {
            *rotate = true;
        }
        result
    }
}

impl Datagrams {
    fn prepare_socket(&mut self, deadline: Instant) -> Result<()> {
        let Some(socket) = self.socket.as_ref() else {
            self.socket = Some(udp_connect(self.bind, self.address, deadline)?);
            return Ok(());
        };
        let UdpBinding::Ephemeral { rotate, retired } = &mut self.binding else {
            return Ok(());
        };
        if !*rotate {
            return Ok(());
        }
        let previous = udp_port(socket)?;
        let now = Instant::now();
        retired.retain(|_, expires| *expires > now);
        // Keep the old socket open while binding its replacement. Recently
        // retired ports stay unavailable for one reply horizon after closing.
        for _ in 0..16 {
            let replacement = udp_connect(self.bind, self.address, deadline)?;
            let port = udp_port(&replacement)?;
            if port == previous || retired.contains_key(&port) {
                continue;
            }
            retired.insert(previous, Instant::now() + self.horizon);
            self.socket = Some(replacement);
            self.sid = 0;
            *rotate = false;
            return Ok(());
        }
        Err(protocol(
            "no fresh UDP source port available within reply horizon",
        ))
    }

    fn read(&mut self, signal: &Signal, deadline: Instant) -> Result<Value> {
        self.prepare_socket(deadline)?;
        let sid = self.sid;
        let request = memory_request(signal, self.gateway_count, self.source, self.target, sid)?;
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| protocol("UDP socket missing"))?;
        if let UdpBinding::Fixed(available) = &self.binding
            && let Some(reusable_at) = available[usize::from(sid)]
        {
            if reusable_at >= deadline {
                return Err(Categorized::new(
                    ErrorKind::Timeout,
                    "FINS/UDP SID cannot be safely reused within the poll deadline",
                )
                .details(json!({"fins_sid":sid,"reply_horizon_ms":self.horizon.as_millis()}))
                .into());
            }
            if let Some(wait) = reusable_at.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            // An older datagram can still be queued even after the horizon.
            drain_udp(socket, deadline)?;
        }
        let sent = send_udp(socket, &request, deadline);
        self.sid = sid.wrapping_add(1);
        match &mut self.binding {
            UdpBinding::Ephemeral { rotate, .. } => *rotate = self.sid == 0,
            UdpBinding::Fixed(available) => {
                available[usize::from(sid)] = Some(Instant::now() + self.horizon);
            }
        }
        sent?;
        let mut frame = [0; MAX_FRAME + 1];
        loop {
            socket
                .set_read_timeout(Some(remaining(deadline)?))
                .map_err(|error| io_error(&error))?;
            match recv_udp(socket, &mut frame) {
                Ok(count) => {
                    let data = frame[..MAX_FRAME]
                        .get(..count)
                        .ok_or_else(|| protocol("oversized FINS/UDP datagram"))?;
                    remaining(deadline)?;
                    if data.get(9).is_some_and(|received| *received != sid) {
                        // A late or duplicated reply to another request cannot
                        // finish this read or extend its deadline.
                        continue;
                    }
                    return signal.decode(memory_reply(&request, data)?);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(&error)),
            }
        }
    }
}

fn udp_connect(bind: SocketAddrV4, peer: SocketAddrV4, deadline: Instant) -> Result<UdpSocket> {
    remaining(deadline)?;
    let socket = UdpSocket::bind(bind).map_err(|error| io_error(&error))?;
    remaining(deadline)?;
    socket.connect(peer).map_err(|error| io_error(&error))?;
    Ok(socket)
}

fn udp_port(socket: &UdpSocket) -> Result<u16> {
    Ok(socket
        .local_addr()
        .map_err(|error| io_error(&error))?
        .port())
}

fn send_udp(socket: &UdpSocket, request: &[u8], deadline: Instant) -> Result<()> {
    loop {
        socket
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        match socket.send(request) {
            Ok(count) if count == request.len() => return Ok(()),
            Ok(_) => return Err(protocol("incomplete FINS/UDP datagram send")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
}

fn recv_udp(socket: &UdpSocket, frame: &mut [u8; MAX_FRAME + 1]) -> io::Result<usize> {
    // Winsock consumes a truncated datagram and reports WSAEMSGSIZE. Expose
    // the same oversize sentinel used by sockets that return the copied length.
    const WSAEMSGSIZE: i32 = 10040;
    match socket.recv(frame) {
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(WSAEMSGSIZE) => Ok(frame.len()),
        result => result,
    }
}

fn drain_udp(socket: &UdpSocket, deadline: Instant) -> Result<()> {
    socket
        .set_nonblocking(true)
        .map_err(|error| io_error(&error))?;
    let result = (|| {
        let mut discarded = [0; MAX_FRAME + 1];
        loop {
            remaining(deadline)?;
            match recv_udp(socket, &mut discarded) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(io_error(&error)),
            }
        }
    })();
    let restored = socket
        .set_nonblocking(false)
        .map_err(|error| io_error(&error));
    result.and(restored)
}

fn send(stream: &mut TcpStream, command: u32, data: &[u8], deadline: Instant) -> Result<()> {
    let mut packet = b"FINS".to_vec();
    packet.extend_from_slice(&u32::try_from(data.len() + 8)?.to_be_bytes());
    packet.extend_from_slice(&command.to_be_bytes());
    packet.extend_from_slice(&[0; 4]);
    packet.extend_from_slice(data);
    write(stream, &packet, deadline)
}
fn receive(
    stream: &mut TcpStream,
    command: u32,
    maximum: usize,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut header = [0; 8];
    read(stream, &mut header, deadline)?;
    let length = usize::try_from(u32::from_be_bytes(header[4..8].try_into()?))?;
    if header[..4] != *b"FINS" || !(8..=maximum).contains(&length) {
        return Err(protocol("invalid or oversized FINS/TCP frame"));
    }
    let mut data = vec![0; length];
    read(stream, &mut data, deadline)?;
    let received_command = u32::from_be_bytes(data[..4].try_into()?);
    let status = u32::from_be_bytes(data[4..8].try_into()?);
    if status != 0 {
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            "FINS/TCP server rejected request",
        )
        .details(json!({"fins_tcp_error":status}))
        .into());
    }
    if received_command != command {
        return Err(protocol("unexpected FINS/TCP command"));
    }
    Ok(data[8..].to_vec())
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("FINS: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "FINS operation deadline expired",
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
    Categorized::error(kind, format!("FINS I/O: {error}"))
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
                    "FINS connection closed during response",
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
                    "FINS connection closed during request",
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
            return Err(io::Error::other("FINS batch exceeds size limit"));
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
    fn request_gateway_count_accepts_only_documented_exact_numeric_values() {
        let base = r#"{"address":"127.0.0.1:9600","signals":[{"name":"value","address":0}]}"#;
        let config: Config = serde_json::from_str(base).unwrap();
        assert_eq!(config.gateway_count, 2);
        for (token, expected) in [
            ("2", 2),
            ("2.0", 2),
            ("2e0", 2),
            ("200e-2", 2),
            ("0.02e2", 2),
            ("7", 7),
            ("7.0", 7),
            ("7e0", 7),
            ("7000e-3", 7),
        ] {
            let text = base.replacen('{', &format!("{{\"gateway_count\":{token},"), 1);
            let config: Config = serde_json::from_str(&text).unwrap();
            assert_eq!(config.gateway_count, expected, "{token}");
        }
        for token in [
            "0",
            "1",
            "3",
            "6",
            "8",
            "255",
            "-2",
            "2.5",
            "7.00000000000000000001",
            "2e200",
            "2e-200",
            "\"7\"",
            "null",
        ] {
            let text = base.replacen('{', &format!("{{\"gateway_count\":{token},"), 1);
            let error = serde_json::from_str::<Config>(&text).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("gateway_count must be exactly 2 or 7"),
                "{token}: {error}"
            );
        }
    }

    #[test]
    fn timer_counter_commands_and_native_values_match_frozen_primary_literals() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/fins/timer-literals.json"
        ))
        .unwrap();
        let unhex = |text: &str| {
            assert!(text.len().is_multiple_of(2));
            text.as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect::<Vec<_>>()
        };
        for case in corpus["cases"].as_array().unwrap() {
            let signal: Signal = serde_json::from_value(case["signal"].clone()).unwrap();
            validate_signals(std::iter::once(&signal)).unwrap();
            assert_eq!(
                memory_command(&signal).unwrap().as_slice(),
                unhex(case["command_hex"].as_str().unwrap())
            );
            assert_eq!(
                signal
                    .decode(&unhex(case["response_data_hex"].as_str().unwrap()))
                    .unwrap(),
                case["expected"]
            );
        }
        for case in corpus["limits"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let signal: Signal = serde_json::from_value(json!({
                "name":"limit", "area":if name.contains("counter") { "counter" } else { "timer" },
                "address":0, "data_type":if name.contains("flags") { "bool" } else { "u16" },
                "elements":case["elements"]
            }))
            .unwrap();
            validate_signals(std::iter::once(&signal)).unwrap();
            assert_eq!(
                memory_command(&signal).unwrap().as_slice(),
                unhex(case["command_hex"].as_str().unwrap())
            );
            assert_eq!(
                signal.bytes().unwrap(),
                usize::try_from(case["response_bytes"].as_u64().unwrap()).unwrap()
            );
        }
    }

    #[test]
    fn timer_counter_range_is_logical_and_rejects_unsupported_views_before_io() {
        for area in ["timer", "counter"] {
            for (address, elements, kind, valid) in [
                (4095, 1, "bool", true),
                (4094, 2, "bool", true),
                (4095, 2, "bool", false),
                (4094, 3, "bool", false),
                (4095, 1, "u16", true),
                (4095, 2, "u16", false),
                (4096, 1, "u16", false),
                (0, 0, "u16", false),
                (0, 1000, "u16", false),
                (0, 1999, "bool", false),
                (0, 1, "i16", true),
                (0, 1, "u32", false),
                (0, 1, "i32", false),
                (0, 1, "u64", false),
                (0, 1, "i64", false),
                (0, 1, "f32", false),
                (0, 1, "f64", false),
            ] {
                let signal: Signal = serde_json::from_value(json!({"name":"value", "area":area,
                    "address":address,"elements":elements,"data_type":kind}))
                .unwrap();
                match validate_signals(std::iter::once(&signal)) {
                    Ok(()) => assert!(valid, "{area} {address} {elements} {kind}"),
                    Err(error) => {
                        assert!(!valid, "{error}");
                        assert!(error.downcast_ref::<Usage>().is_some(), "{error}");
                    }
                }
            }
            for patch in [
                json!({"bit":0}),
                json!({"bank":0}),
                json!({"data_type":"bool","bit":0}),
            ] {
                let mut value = json!({"name":"value","area":area,"address":0});
                for (key, field) in patch.as_object().unwrap() {
                    value[key].clone_from(field);
                }
                let signal: Signal = serde_json::from_value(value).unwrap();
                let error = validate_signals(std::iter::once(&signal)).unwrap_err();
                assert!(error.downcast_ref::<Usage>().is_some(), "{error}");
            }
        }
    }

    #[test]
    fn timer_pv_preserves_explicit_transforms_without_guessing_timebase() {
        let signal: Signal = serde_json::from_value(json!({"name":"value","area":"timer",
            "address":0,"byte_order":"little","word_order":"big","scale":0.5,"offset":1}))
        .unwrap();
        validate_signals(std::iter::once(&signal)).unwrap();
        assert_eq!(signal.decode(&[0x34, 0x12]).unwrap(), 2331.0);
        // Same raw 8001 word as the frozen counter_pv_last_pair vector;
        // signedness is explicitly requested by the operator, not inferred.
        let signed: Signal = serde_json::from_value(json!({"name":"value","area":"counter",
            "address":4095,"data_type":"i16"}))
        .unwrap();
        validate_signals(std::iter::once(&signed)).unwrap();
        assert_eq!(signed.decode(&[0x80, 1]).unwrap(), -32767);
        assert_eq!(signed.decode(&[0xff, 0xff]).unwrap(), -1);
        let flag: Signal = serde_json::from_value(json!({"name":"value","area":"counter",
            "address":0,"data_type":"bool"}))
        .unwrap();
        assert!(
            flag.decode(&[2])
                .unwrap_err()
                .to_string()
                .contains("0 or 1")
        );
        assert!(
            flag.decode(&[])
                .unwrap_err()
                .to_string()
                .contains("response length")
        );
    }
}
