//! Read-only EtherNet/IP explicit messaging for symbolic CIP tags and Assembly data.
//!
//! Wire references: Rockwell 1756-PM020 (Read Tag, symbolic/element paths and
//! atomic values and fragmented reads, pp. 18–26), and ODVA's EtherNet/IP
//! encapsulation description:
//! <https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf>
//! <https://www.odva.org/wp-content/uploads/2022/06/2014_ODVA_Conference_Byres_Schweigert_Thomas_Securing_EtherNetIP_with_DPI_FINAL.pdf>
//! The Keyence DL-EP1 IL manual (96132E, pp. 3-8–3-12 and 5-23) describes
//! `Get_Attribute_Single` of the Assembly data attribute and its monitor layout:
//! <https://forge.codesys.com/forge/talk/Deutsch/thread/dda5c0abc3/54bc/attachment/AS_16491_DL-EP1_UM_96132E_WW_GB_2045_7.pdf>
//! Backplane routing follows the port 1 / slot model in Rockwell's developer guide:
//! <https://www.rockwellautomation.com/content/dam/rockwell-automation/sites/downloads/pdf/developerguide.pdf>
//! Omron W506 sections 7-1, 7-4, 7-6 and 7-7 define direct variable reads:
//! <https://files.omron.eu/downloads/latest/manual/en/w506_nj_nx-series_cpu_unit_built-in_ethernet_ip_port_users_manual_en.pdf?v=3>
//! Micro800 uses direct Class 3 explicit messaging with the message router.
//! <https://literature.rockwellautomation.com/idc/groups/literature/documents/um/2080-um002_-en-e.pdf>
//! <https://www.odva.org/wp-content/uploads/2020/06/PUB00123R1_Common-Industrial_Protocol_and_Family_of_CIP_Networks.pdf>
//! This adapter sends no Write Tag or cyclic I/O services.
mod class3;
pub(crate) mod dataman;
mod logix_bits;
mod logix_raw;
mod logix_string;
mod micro800_string;
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const REGISTER: u16 = 0x65;
const UNREGISTER: u16 = 0x66;
const SEND_RR: u16 = 0x6f;
const MAX_PACKET: usize = 8192;
const MAX_BATCH: usize = 1 << 20;
const MAX_ATOMIC_BYTES: usize = 256 * 8;
const MAX_ASSEMBLY_BYTES: usize = 4096;
const MAX_OMRON_UCMM: usize = 502;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Numeric IP and port, normally port 44818.
    pub address: SocketAddr,
    /// One optional port-1 backplane hop to a processor slot.
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub slot: Option<u8>,
    #[serde(default)]
    profile: Profile,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    originator_vendor_id: Option<u16>,
    pub signals: Vec<Signal>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Profile {
    #[default]
    Logix,
    OmronNjNx,
    Micro800,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    pub tag: String,
    #[serde(default = "one")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub elements: u16,
    #[serde(default)]
    data_type: logix_string::DataType,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    capacity: Option<u16>,
    #[serde(default)]
    encoding: Option<logix_string::Encoding>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    max_bytes: Option<u32>,
}
const fn one() -> u16 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssemblyConfig {
    address: SocketAddr,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    instance: u16,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    data_length: u16,
    #[serde(default)]
    raw_signal: Option<String>,
    #[serde(default)]
    signals: Vec<AssemblySignal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssemblySignal {
    name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    offset: u16,
    data_type: AssemblyDataType,
    #[serde(default)]
    byte_order: AssemblyByteOrder,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    bit: Option<u8>,
    #[serde(default = "scale_one")]
    scale: f64,
    #[serde(default)]
    offset_value: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u16)]
enum AssemblyDataType {
    Bool = 0xc1,
    I8 = 0xc2,
    I16 = 0xc3,
    I32 = 0xc4,
    I64 = 0xc5,
    U8 = 0xc6,
    U16 = 0xc7,
    U32 = 0xc8,
    U64 = 0xc9,
    F32 = 0xca,
    F64 = 0xcb,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AssemblyByteOrder {
    #[default]
    Little,
    Big,
}

const fn scale_one() -> f64 {
    1.0
}

pub(crate) fn open_assembly(config: &AssemblyConfig) -> Result<Box<dyn Source>> {
    let length = usize::from(config.data_length);
    let count = config.signals.len() + usize::from(config.raw_signal.is_some());
    if config.address.port() == 0
        || config.instance == 0
        || !(1..=MAX_ASSEMBLY_BYTES).contains(&length)
    {
        return Err(Usage("EtherNet/IP Assembly needs a nonzero port, an instance from 1 to 65535 and a data_length from 1 to 4096".into()).into());
    }
    if !(1..=256).contains(&count) {
        return Err(Usage(
            "EtherNet/IP Assembly requires 1–256 named outputs, including raw_signal when supplied"
                .into(),
        )
        .into());
    }
    let mut names = BTreeSet::new();
    if let Some(name) = config.raw_signal.as_deref() {
        super::validate_name(name, "raw signal")?;
        names.insert(name);
    }
    for signal in &config.signals {
        super::validate_name(&signal.name, "signal")?;
        if !names.insert(signal.name.as_str()) {
            return Err(Usage("EtherNet/IP Assembly signal names must be unique".into()).into());
        }
        if signal.end()? > length {
            return Err(Usage(format!(
                "EtherNet/IP Assembly signal {} exceeds data_length",
                signal.name
            ))
            .into());
        }
        if signal
            .bit
            .is_some_and(|bit| bit > 7 || !matches!(signal.data_type, AssemblyDataType::Bool))
        {
            return Err(Usage("bit must be 0–7 and is only valid for bool signals".into()).into());
        }
        if !signal.scale.is_finite()
            || !signal.offset_value.is_finite()
            || (matches!(
                signal.data_type,
                AssemblyDataType::Bool | AssemblyDataType::U64 | AssemblyDataType::I64
            ) && (signal.scale != 1.0 || signal.offset_value != 0.0))
        {
            return Err(Usage("scale/offset_value must be finite; bool and 64-bit integer signals cannot be scaled".into()).into());
        }
    }
    // Fixed service 0x0e, Assembly class 0x04, Data attribute 0x03.
    let request = u8::try_from(config.instance).map_or_else(
        |_| {
            let [low, high] = config.instance.to_le_bytes();
            vec![0x0e, 4, 0x20, 4, 0x25, 0, low, high, 0x30, 3]
        },
        |instance| vec![0x0e, 3, 0x20, 4, 0x24, instance, 0x30, 3],
    );
    Ok(Box::new(AssemblyReader {
        config: config.clone(),
        request,
        connection: None,
        context: 0,
    }))
}

impl AssemblySignal {
    fn end(&self) -> Result<usize> {
        Ok(usize::from(self.offset) + atomic_width(self.data_type as u16)?)
    }

    fn read(&self, data: &[u8]) -> Result<Reading> {
        let bytes = data
            .get(usize::from(self.offset)..self.end()?)
            .ok_or_else(|| protocol("assembly signal lies outside returned data"))?;
        let value = if matches!(self.data_type, AssemblyDataType::Bool) {
            let byte = bytes
                .first()
                .ok_or_else(|| protocol("missing assembly bit byte"))?;
            json!(byte & (1 << self.bit.unwrap_or(0)) != 0)
        } else {
            let mut ordered = [0; 8];
            let bytes = if matches!(self.byte_order, AssemblyByteOrder::Big) {
                let ordered = ordered
                    .get_mut(..bytes.len())
                    .ok_or_else(|| protocol("assembly numeric signal exceeds eight bytes"))?;
                ordered.copy_from_slice(bytes);
                ordered.reverse();
                ordered
            } else {
                bytes
            };
            let value = decode_atomic(self.data_type as u16, bytes)?;
            if self.scale == 1.0 && self.offset_value == 0.0 {
                value
            } else {
                let raw = value
                    .as_f64()
                    .ok_or_else(|| protocol("cannot scale assembly signal"))?;
                finite(raw * self.scale + self.offset_value)?
            }
        };
        Ok(Reading {
            signal: self.name.clone(),
            value,
            source_time: None,
        })
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 || config.signals.is_empty() || config.signals.len() > 256 {
        return Err(
            Usage("EtherNet/IP requires a nonzero TCP port and 1–256 signals".into()).into(),
        );
    }
    if config.profile == Profile::OmronNjNx && config.slot.is_some() {
        return Err(Usage("Omron NJ/NX reads require a direct CPU connection".into()).into());
    }
    if config.profile == Profile::Micro800 && config.slot.is_some() {
        return Err(
            Usage("Micro800 requires a direct CPU connection without a slot".into()).into(),
        );
    }
    if config.profile != Profile::Micro800 && config.originator_vendor_id.is_some() {
        return Err(
            Usage("originator_vendor_id applies only to the Micro800 profile".into()).into(),
        );
    }
    let mut names = BTreeSet::new();
    let mut requests = Vec::with_capacity(config.signals.len());
    for signal in &config.signals {
        super::validate_name(&signal.name, "signal")?;
        if !names.insert(&signal.name) || !(1..=256).contains(&signal.elements) {
            return Err(Usage(
                "EtherNet/IP signal names must be unique; elements must be 1–256".into(),
            )
            .into());
        }
        if config.profile == Profile::OmronNjNx && signal.elements > 1 && !signal.tag.ends_with(']')
        {
            return Err(Usage(
                "Omron NJ/NX array ranges require an explicit first element index".into(),
            )
            .into());
        }
        if signal.data_type == logix_string::DataType::LogixStructRaw {
            if config.profile != Profile::Logix {
                return Err(Usage("logix_struct_raw requires the Logix profile".into()).into());
            }
            requests.push(Request::Raw(logix_raw::Plan::new(signal)?));
            continue;
        }
        if signal.max_bytes.is_some() {
            return Err(Usage("max_bytes applies only to logix_struct_raw signals".into()).into());
        }
        if matches!(
            signal.data_type,
            logix_string::DataType::LogixDword | logix_string::DataType::LogixBoolArray
        ) {
            if config.profile != Profile::Logix {
                return Err(Usage(
                    "logix_dword and logix_bool_array require the Logix profile".into(),
                )
                .into());
            }
            requests.push(Request::Dword(logix_bits::Plan::new(signal)?));
            continue;
        }
        if signal.data_type == logix_string::DataType::LogixString {
            if config.profile != Profile::Logix {
                return Err(Usage("logix_string requires the Logix profile".into()).into());
            }
            requests.push(Request::String(logix_string::Plan::new(signal)?));
            continue;
        }
        if signal.data_type == logix_string::DataType::Micro800String {
            if config.profile != Profile::Micro800 {
                return Err(Usage("micro800_string requires the Micro800 profile".into()).into());
            }
            requests.push(Request::Micro800String(micro800_string::Plan::new(signal)?));
            continue;
        }
        if signal.capacity.is_some() || signal.encoding.is_some() {
            return Err(Usage(
                "capacity and encoding apply only to explicit string signals".into(),
            )
            .into());
        }
        let path = symbolic_path(&signal.tag, config.profile)?;
        let mut request = vec![0x4c, u8::try_from(path.len() / 2)?];
        request.extend_from_slice(&path);
        request.extend_from_slice(&signal.elements.to_le_bytes());
        if config.profile == Profile::OmronNjNx && request.len() > MAX_OMRON_UCMM {
            return Err(Usage("Omron NJ/NX CIP request exceeds 502 bytes".into()).into());
        }
        if config.profile == Profile::Micro800 && request.len() + 6 > class3::MAX_PAYLOAD {
            return Err(Usage("Micro800 connected read path exceeds 504 bytes".into()).into());
        }
        requests.push(Request::Atomic(request));
    }
    Ok(Box::new(Client {
        config: config.clone(),
        requests,
        connection: None,
        context: 0,
    }))
}

fn routed_request(request: &[u8], slot: Option<u8>) -> Result<Vec<u8>> {
    let Some(slot) = slot else {
        return Ok(request.to_vec());
    };
    let mut routed = vec![0x52, 2, 0x20, 6, 0x24, 1, 0x0a, 0x0e];
    routed.extend_from_slice(&u16::try_from(request.len())?.to_le_bytes());
    routed.extend_from_slice(request);
    if !request.len().is_multiple_of(2) {
        routed.push(0);
    }
    // One word route: port 1 (backplane), selected processor slot.
    routed.extend_from_slice(&[1, 0, 1, slot]);
    Ok(routed)
}

fn bad_path() -> anyhow::Error {
    Usage("EtherNet/IP tag must use symbolic names, optional array indices and dot-separated members, e.g. Program:Main.Pressure[2]; encoded path must fit 255 words".into()).into()
}

/// Build an ANSI extended symbolic EPATH, with 8/16/32-bit element segments.
fn symbolic_path(tag: &str, profile: Profile) -> Result<Vec<u8>> {
    if profile == Profile::OmronNjNx && tag.contains(':') {
        return Err(Usage("Omron NJ/NX requires a published variable name".into()).into());
    }
    if profile == Profile::Micro800 && tag.contains(':') {
        return Err(Usage("Micro800 requires a global variable name".into()).into());
    }
    if tag.is_empty() || tag.len() > 1024 || !tag.is_ascii() {
        return Err(bad_path());
    }
    let bytes = tag.as_bytes();
    let mut at = 0;
    let mut path = Vec::new();
    loop {
        let start = at;
        if !bytes
            .get(at)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            return Err(bad_path());
        }
        at += 1;
        while bytes
            .get(at)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b':'))
        {
            at += 1;
        }
        let name = &bytes[start..at];
        if profile == Profile::Micro800
            && (name.len() > 40 || name.windows(2).any(|pair| pair == b"__"))
        {
            return Err(Usage(
                "Micro800 variable names require 1–40 characters without consecutive underscores"
                    .into(),
            )
            .into());
        }
        path.extend_from_slice(&[0x91, u8::try_from(name.len()).map_err(|_| bad_path())?]);
        path.extend_from_slice(name);
        if !name.len().is_multiple_of(2) {
            path.push(0);
        }
        while bytes.get(at) == Some(&b'[') {
            at += 1;
            loop {
                let begin = at;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                if begin == at {
                    return Err(bad_path());
                }
                let index = std::str::from_utf8(&bytes[begin..at])?
                    .parse::<u32>()
                    .map_err(|_| bad_path())?;
                if let Ok(index) = u8::try_from(index) {
                    path.extend_from_slice(&[0x28, index]);
                } else if let Ok(index) = u16::try_from(index) {
                    path.extend_from_slice(&[0x29, 0]);
                    path.extend_from_slice(&index.to_le_bytes());
                } else {
                    if profile == Profile::OmronNjNx {
                        return Err(
                            Usage("Omron NJ/NX array indices must fit 16 bits".into()).into()
                        );
                    }
                    path.extend_from_slice(&[0x2a, 0]);
                    path.extend_from_slice(&index.to_le_bytes());
                }
                match bytes.get(at) {
                    Some(b',') => at += 1,
                    Some(b']') => {
                        at += 1;
                        break;
                    }
                    _ => return Err(bad_path()),
                }
            }
        }
        if path.len() > 510 {
            return Err(bad_path());
        }
        match bytes.get(at) {
            None => return Ok(path),
            Some(b'.') => at += 1,
            _ => return Err(bad_path()),
        }
    }
}

enum Request {
    Atomic(Vec<u8>),
    String(logix_string::Plan),
    Raw(logix_raw::Plan),
    Dword(logix_bits::Plan),
    Micro800String(micro800_string::Plan),
}
#[derive(Clone, Copy)]
enum ReadShape {
    Atomic,
    Exact { kind: u16, bytes: usize },
    ShortString { capacity: usize },
}

struct Client {
    config: Config,
    requests: Vec<Request>,
    connection: Option<Connection>,
    context: u64,
}

struct AssemblyReader {
    config: AssemblyConfig,
    request: Vec<u8>,
    connection: Option<Connection>,
    context: u64,
}

struct Connection {
    stream: TcpStream,
    session: u32,
    class3: Option<class3::Class3>,
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("EtherNet/IP timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.connection = None;
        }
        result
    }

    fn finish(&mut self, timeout: Duration) -> Result<()> {
        if self.config.profile == Profile::Micro800
            && let Some(mut connection) = self.connection.take()
        {
            connection.finish_class3(&mut self.context, timeout);
        }
        Ok(())
    }
}
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.config.profile == Profile::Micro800 {
            class3::ensure(
                &mut self.connection,
                self.config.address,
                self.config.originator_vendor_id.unwrap_or_default(),
                &mut self.context,
                deadline,
            )?;
        }
        let connection = Connection::get(&mut self.connection, self.config.address, deadline)?;
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut budget = logix_string::Budget::new();
        for (index, (signal, request)) in self.config.signals.iter().zip(&self.requests).enumerate()
        {
            remaining(deadline)?;
            budget.separator(index)?;
            let value = match request {
                Request::Raw(plan) => plan.read(
                    connection,
                    self.config.slot,
                    &mut self.context,
                    deadline,
                    &mut budget,
                    &signal.name,
                )?,
                Request::Dword(plan) => plan.read(
                    connection,
                    self.config.slot,
                    &mut self.context,
                    deadline,
                    &mut budget,
                    &signal.name,
                )?,
                Request::String(plan) => plan.read(
                    connection,
                    self.config.slot,
                    &mut self.context,
                    deadline,
                    &mut budget,
                    &signal.name,
                )?,
                Request::Micro800String(plan) => plan.read(
                    connection,
                    &mut self.context,
                    deadline,
                    &mut budget,
                    &signal.name,
                )?,
                Request::Atomic(request) => match self.config.profile {
                    Profile::Logix | Profile::Micro800 => connection.read_tag(
                        request,
                        self.config.slot,
                        signal.elements,
                        &mut self.context,
                        deadline,
                    )?,
                    Profile::OmronNjNx => connection.read_omron_tag(
                        request,
                        signal.elements,
                        &mut self.context,
                        deadline,
                    )?,
                },
            };
            let reading = Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            };
            if matches!(request, Request::Atomic(_)) {
                budget.value(&reading)?;
            }
            readings.push(reading);
        }
        remaining(deadline)?;
        Ok(readings)
    }
}

impl Source for AssemblyReader {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("EtherNet/IP timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.connection = None;
        }
        result
    }
}

impl AssemblyReader {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let connection = Connection::get(&mut self.connection, self.config.address, deadline)?;
        let response =
            connection.exchange_ucmm(&self.request, None, &mut self.context, deadline)?;
        let reply = cip_reply(cpf(&response)?, 0x8e, false)?;
        if reply.partial {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "EtherNet/IP Assembly read returned partial data",
            ));
        }
        if reply.data.len() != usize::from(self.config.data_length) {
            return Err(protocol(&format!(
                "assembly reply contains {} bytes; expected {}",
                reply.data.len(),
                self.config.data_length
            )));
        }
        let mut readings = self
            .config
            .signals
            .iter()
            .map(|signal| signal.read(reply.data))
            .collect::<Result<Vec<_>>>()?;
        if let Some(name) = &self.config.raw_signal {
            readings.push(Reading {
                signal: name.clone(),
                value: json!({"encoding":"base64", "data":base64::engine::general_purpose::STANDARD.encode(reply.data)}),
                source_time: None,
            });
        }
        remaining(deadline)?;
        Ok(readings)
    }
}

impl Connection {
    fn get(
        connection: &mut Option<Self>,
        address: SocketAddr,
        deadline: Instant,
    ) -> Result<&mut Self> {
        if connection.is_none() {
            *connection = Some(Self::connect(address, deadline)?);
        }
        connection
            .as_mut()
            .ok_or_else(|| protocol("session missing"))
    }

    fn connect(address: SocketAddr, deadline: Instant) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        stream.set_nodelay(true).map_err(|error| io_error(&error))?;
        let mut opened = Self {
            stream,
            session: 0,
            class3: None,
        };
        // Sender context is opaque and sender-selected. Zero also works
        // with peers that restrict registration, while the exact echo is
        // still checked; subsequent reads use distinct correlated contexts.
        let response = opened.exchange(REGISTER, &[1, 0, 0, 0], 0, deadline)?;
        if response != [1, 0, 0, 0] || opened.session == 0 {
            return Err(protocol("invalid RegisterSession response"));
        }
        Ok(opened)
    }

    fn read_tag(
        &mut self,
        initial: &[u8],
        slot: Option<u8>,
        elements: u16,
        context: &mut u64,
        deadline: Instant,
    ) -> Result<Value> {
        let assembled = self.read_tag_raw(
            initial,
            slot,
            elements,
            context,
            deadline,
            ReadShape::Atomic,
        )?;
        decode(&assembled, elements)
    }

    fn read_tag_raw(
        &mut self,
        initial: &[u8],
        slot: Option<u8>,
        elements: u16,
        context: &mut u64,
        deadline: Instant,
        shape: ReadShape,
    ) -> Result<Vec<u8>> {
        let connected = self.class3.is_some();
        let mut request = initial.to_vec();
        if connected {
            // Pinned libplctag's Micro800 read starts with Read Tag
            // Fragmented at offset zero even for a scalar.
            fragment_request(&mut request, initial, 0)?;
        }
        let mut assembled = Vec::new();
        loop {
            remaining(deadline)?;
            let service = request
                .first()
                .copied()
                .ok_or_else(|| protocol("empty tag read request"))?;
            let response;
            let cip = if connected {
                response = self.exchange_connected(&request, deadline)?;
                &response[class3::CIP_OFFSET..]
            } else {
                response = self.exchange_ucmm(&request, slot, context, deadline)?;
                cpf(&response)?
            };
            let reply = cip_reply(cip, service | 0x80, slot.is_some())?;
            let Some(kind) = reply.data.first_chunk::<2>() else {
                return Err(protocol("missing CIP atomic type"));
            };
            let payload = &reply.data[2..];
            let code = u16::from_le_bytes(*kind);
            let expected = match shape {
                ReadShape::Atomic => {
                    let expected = atomic_width(code)? * usize::from(elements);
                    if expected == 0 || expected > MAX_ATOMIC_BYTES {
                        return Err(protocol("atomic value exceeds the configured read limit"));
                    }
                    expected
                }
                ReadShape::Exact { kind, bytes } => {
                    if code != kind {
                        return Err(protocol(
                            "Logix STRING member returned the wrong atomic type",
                        ));
                    }
                    bytes
                }
                ReadShape::ShortString { capacity } => {
                    if code != 0xda {
                        return Err(protocol(
                            "Micro800 STRING returned the wrong type; SHORT_STRING 0x00da is required",
                        ));
                    }
                    let length = if assembled.is_empty() {
                        payload.first()
                    } else {
                        assembled.get(2)
                    }
                    .copied()
                    .ok_or_else(|| protocol("Micro800 STRING reply is missing its byte count"))?;
                    if length > 252 || usize::from(length) > capacity {
                        return Err(protocol(
                            "Micro800 STRING byte count exceeds 252 or configured capacity",
                        ));
                    }
                    1 + usize::from(length)
                }
            };
            if assembled.is_empty() {
                assembled.reserve_exact(expected + 2);
                assembled.extend_from_slice(kind);
            } else if assembled.first_chunk::<2>() != Some(kind) {
                return Err(protocol("atomic type changed between read fragments"));
            }
            let received = assembled.len() - 2;
            if payload.len() > expected.saturating_sub(received) {
                return Err(protocol(
                    "atomic read fragment exceeds requested value size",
                ));
            }
            if reply.partial && payload.is_empty() {
                return Err(protocol("partial atomic read made no progress"));
            }
            assembled.extend_from_slice(payload);
            if !reply.partial {
                if assembled.len() - 2 != expected {
                    return Err(protocol(
                        "atomic value size differs from requested element count",
                    ));
                }
                remaining(deadline)?;
                return Ok(assembled);
            }
            let received = assembled.len() - 2;
            if received >= expected {
                return Err(protocol("partial status follows a complete atomic value"));
            }
            // 1756-PM020: count/path stay fixed; offset counts value bytes,
            // excluding the type descriptor repeated in every reply.
            fragment_request(&mut request, initial, received)?;
        }
    }

    fn read_omron_tag(
        &mut self,
        request: &[u8],
        elements: u16,
        context: &mut u64,
        deadline: Instant,
    ) -> Result<Value> {
        let response = self.exchange_ucmm(request, None, context, deadline)?;
        let cip = cpf(&response)?;
        if cip.len() > MAX_OMRON_UCMM {
            return Err(protocol("Omron NJ/NX CIP reply exceeds 502 bytes"));
        }
        let reply = cip_reply(cip, 0xcc, false)?;
        if reply.partial {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "Omron NJ/NX partial variable read; configure a smaller indexed range",
            ));
        }
        decode_omron(reply.data, elements)
    }

    fn exchange_ucmm(
        &mut self,
        request: &[u8],
        slot: Option<u8>,
        context: &mut u64,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        let request = routed_request(request, slot)?;
        // SendRRData: null address and unconnected CIP data; no I/O opens.
        let mut body = vec![0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
        body.extend_from_slice(&u16::try_from(request.len())?.to_le_bytes());
        body.extend_from_slice(&request);
        *context = context.wrapping_add(1);
        self.exchange(SEND_RR, &body, *context, deadline)
    }

    fn exchange(
        &mut self,
        command: u16,
        body: &[u8],
        context: u64,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        let mut request = header(command, self.session, context, u16::try_from(body.len())?);
        request.extend_from_slice(body);
        write(&mut self.stream, &request, deadline)?;
        let mut response = [0; 24];
        read(&mut self.stream, &mut response, deadline)?;
        let response_command = u16::from_le_bytes([response[0], response[1]]);
        let size = usize::from(u16::from_le_bytes([response[2], response[3]]));
        let session = u32::from_le_bytes(response[4..8].try_into()?);
        let status = u32::from_le_bytes(response[8..12].try_into()?);
        if response_command != command
            || (command != class3::SEND_UNIT && response[12..20] != context.to_le_bytes())
            || response[20..24] != [0; 4]
        {
            return Err(protocol(
                "unexpected encapsulation command, context or options",
            ));
        }
        if command == class3::SEND_UNIT && session != self.session {
            return Err(protocol("unexpected session handle"));
        }
        if status != 0 {
            return Err(Categorized::new(
                ErrorKind::Unavailable,
                "EtherNet/IP encapsulation request rejected",
            )
            .details(json!({"encapsulation_status": status}))
            .into());
        }
        if command == REGISTER {
            if self.session != 0 || session == 0 {
                return Err(protocol("invalid registered session handle"));
            }
            self.session = session;
        } else if session != self.session {
            return Err(protocol("unexpected session handle"));
        }
        if size > MAX_PACKET {
            return Err(protocol("encapsulation response exceeds 8192 bytes"));
        }
        let mut body = vec![0; size];
        read(&mut self.stream, &mut body, deadline)?;
        Ok(body)
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        if self.session != 0 {
            // UnregisterSession has no reply. Cleanup must never wait for a
            // blocked controller; failing to send is harmless when TCP closes.
            let packet = header(UNREGISTER, self.session, 0, 0);
            if self.stream.set_nonblocking(true).is_ok() {
                drop(self.stream.write(&packet));
            }
        }
    }
}
fn header(command: u16, session: u32, context: u64, size: u16) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(&command.to_le_bytes());
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&session.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&context.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes
}

fn fragment_request(request: &mut Vec<u8>, initial: &[u8], received: usize) -> Result<()> {
    request.clear();
    request.extend_from_slice(initial);
    *request
        .first_mut()
        .ok_or_else(|| protocol("empty fragmented read request"))? = 0x52;
    request.extend_from_slice(&u32::try_from(received)?.to_le_bytes());
    Ok(())
}

fn cpf(bytes: &[u8]) -> Result<&[u8]> {
    let [
        0,
        0,
        0,
        0,
        _,
        _,
        2,
        0,
        0,
        0,
        0,
        0,
        0xb2,
        0,
        low,
        high,
        data @ ..,
    ] = bytes
    else {
        return Err(protocol("invalid unconnected Common Packet Format"));
    };
    if data.len() != usize::from(u16::from_le_bytes([*low, *high])) {
        return Err(protocol("CPF data length differs from payload"));
    }
    Ok(data)
}
#[derive(Debug)]
struct CipReply<'a> {
    data: &'a [u8],
    partial: bool,
}
fn cip_reply(bytes: &[u8], expected: u8, routed: bool) -> Result<CipReply<'_>> {
    let [service, 0, status, extra, remaining @ ..] = bytes else {
        return Err(protocol("invalid CIP reply header"));
    };
    let extended_len = usize::from(*extra) * 2;
    let data = remaining
        .get(extended_len..)
        .ok_or_else(|| protocol("truncated CIP extended status"))?;
    // Successful Unconnected Send passes through the embedded reply. D2
    // is also Read Tag Fragmented, while CC/D2 can be unsupported atomic
    // type codes: never guess a nested header from tag payload bytes.
    let same_service = *service == expected;
    let route_error = routed && *service == 0xd2 && !matches!(*status, 0 | 6);
    if !same_service && !route_error {
        return Err(protocol("unexpected CIP reply service"));
    }
    if !matches!(*status, 0 | 6) {
        let kind = if matches!(*status, 4 | 5) {
            ErrorKind::Unavailable
        } else if *status == 8 {
            ErrorKind::Unsupported
        } else {
            ErrorKind::OperationFailed
        };
        let extended_status = remaining[..extended_len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|word| u16::from_le_bytes(*word))
            .collect::<Vec<_>>();
        let (message, hint) = match expected {
            0x8e => (
                "EtherNet/IP CIP read rejected",
                "check the configured CIP class, instance and attribute and whether the device supports unconnected Get_Attribute_Single",
            ),
            0xd4 | 0xce => (
                "Micro800 connection manager rejected the Class 3 request",
                "check controller access and the configured originator identity",
            ),
            _ => (
                "EtherNet/IP CIP read rejected",
                "check the tag path, processor slot and the tag's external read access",
            ),
        };
        return Err(Categorized::new(kind, message)
            .hint(hint)
            .details(json!({"cip_status": status, "extended_status": extended_status}))
            .into());
    }
    if *extra != 0 {
        return Err(protocol(
            "successful or partial CIP reply unexpectedly contains extended status",
        ));
    }
    Ok(CipReply {
        data,
        partial: *status == 6,
    })
}

fn atomic_width(code: u16) -> Result<usize> {
    match code {
        0xc1 | 0xc2 | 0xc6 => Ok(1),
        0xc3 | 0xc7 => Ok(2),
        0xc4 | 0xc8 | 0xca => Ok(4),
        0xc5 | 0xc9 | 0xcb => Ok(8),
        _ => Err(Categorized::new(ErrorKind::Unsupported, format!("EtherNet/IP atomic type 0x{code:04x} is not supported"))
            .hint("select an explicit Logix DWORD/BOOL-array, string or structure mode when applicable; otherwise read an atomic member or use an external helper").into()),
    }
}

fn decode(bytes: &[u8], elements: u16) -> Result<Value> {
    let [low, high, data @ ..] = bytes else {
        return Err(protocol("missing CIP atomic type"));
    };
    decode_values(u16::from_le_bytes([*low, *high]), data, elements)
}

fn decode_omron(data: &[u8], elements: u16) -> Result<Value> {
    // W506 7-6-1: an atomic reply has an eight-bit type code and a
    // separate zero AddInfo Length byte. The element count is in the request.
    let [kind, 0, data @ ..] = data else {
        return Err(protocol(
            "Omron NJ/NX atomic reply requires zero AddInfo length",
        ));
    };
    let code = match kind {
        0xc1 => {
            if elements != 1 {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "Omron NJ/NX BOOL reads require one scalar or indexed element",
                ));
            }
            let [value @ (0 | 1), forced @ (0 | 1)] = data else {
                return Err(protocol("invalid Omron NJ/NX BOOL value or force byte"));
            };
            return Ok(json!({"value": *value == 1, "forced": *forced == 1}));
        }
        // Omron IEC bit-string types have the same wire widths as these
        // unsigned atomic integers. Logix still rejects these type codes.
        0xd1 => 0xc6,
        0xd2 => 0xc7,
        0xd3 => 0xc8,
        0xd4 => 0xc9,
        kind => u16::from(*kind),
    };
    decode_values(code, data, elements)
}

fn decode_values(code: u16, data: &[u8], elements: u16) -> Result<Value> {
    let size = atomic_width(code)?;
    if data.len() != size * usize::from(elements) {
        return Err(protocol(
            "atomic value size differs from requested element count",
        ));
    }
    let mut values = Vec::with_capacity(usize::from(elements));
    for bytes in data.chunks_exact(size) {
        values.push(decode_atomic(code, bytes)?);
    }
    if elements == 1 {
        values.pop().ok_or_else(|| protocol("missing atomic value"))
    } else {
        Ok(Value::Array(values))
    }
}

fn decode_atomic(code: u16, bytes: &[u8]) -> Result<Value> {
    let value = match code {
        0xc1 => {
            let [byte] = bytes else {
                return Err(protocol("invalid BOOL value size"));
            };
            if !matches!(*byte, 0 | 1 | 0xff) {
                return Err(protocol("invalid BOOL value"));
            }
            json!(*byte != 0)
        }
        0xc2 => json!(i8::from_le_bytes(bytes.try_into()?)),
        0xc3 => json!(i16::from_le_bytes(bytes.try_into()?)),
        0xc4 => json!(i32::from_le_bytes(bytes.try_into()?)),
        0xc5 => json!(i64::from_le_bytes(bytes.try_into()?)),
        0xc6 => json!(u8::from_le_bytes(bytes.try_into()?)),
        0xc7 => json!(u16::from_le_bytes(bytes.try_into()?)),
        0xc8 => json!(u32::from_le_bytes(bytes.try_into()?)),
        0xc9 => json!(u64::from_le_bytes(bytes.try_into()?)),
        0xca => finite(f64::from(f32::from_le_bytes(bytes.try_into()?)))?,
        0xcb => finite(f64::from_le_bytes(bytes.try_into()?))?,
        _ => return Err(protocol("atomic type changed during decoding")),
    };
    Ok(value)
}
fn finite(value: f64) -> Result<Value> {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| protocol("non-finite CIP floating point value"))
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("EtherNet/IP: {message}"),
    )
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "EtherNet/IP poll deadline expired",
        ));
    }
    Ok(left)
}
fn io_error(error: &std::io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Unavailable,
    };
    Categorized::error(kind, format!("EtherNet/IP I/O: {error}"))
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
                    "EtherNet/IP peer closed TCP",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
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
            Ok(0) => return Err(protocol("TCP writer made no progress")),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn config(address: SocketAddr, slot: Option<u8>) -> Config {
        Config {
            address,
            slot,
            profile: Profile::Logix,
            originator_vendor_id: None,
            signals: vec![Signal {
                name: "level".into(),
                tag: "Tank[2].Level".into(),
                elements: 1,
                data_type: logix_string::DataType::Atomic,
                capacity: None,
                encoding: None,
                max_bytes: None,
            }],
        }
    }
    fn request(stream: &mut TcpStream) -> (Vec<u8>, Vec<u8>) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = vec![0; 24];
        stream.read_exact(&mut header).unwrap();
        let mut body = vec![0; usize::from(u16::from_le_bytes([header[2], header[3]]))];
        stream.read_exact(&mut body).unwrap();
        (header, body)
    }
    fn reply(stream: &mut TcpStream, request: &[u8], session: u32, body: &[u8], fragments: bool) {
        let mut header = request.to_vec();
        header[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
        header[4..8].copy_from_slice(&session.to_le_bytes());
        header.extend_from_slice(body);
        if fragments {
            for chunk in header.chunks(3) {
                stream.write_all(chunk).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        } else {
            stream.write_all(&header).unwrap();
        }
    }
    fn rr(cip: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
        bytes.extend_from_slice(&u16::try_from(cip.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(cip);
        bytes
    }
    fn register(stream: &mut TcpStream) {
        let (header, body) = request(stream);
        assert_eq!(header[..12], [0x65, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(body, [1, 0, 0, 0]);
        assert_eq!(header[12..20], [0; 8]);
        assert_eq!(header[20..24], [0; 4]);
        reply(stream, &header, 0x4433_2211, &[1, 0, 0, 0], false);
    }

    #[test]
    fn symbolic_paths_encode_program_names_members_and_wide_indices() {
        assert_eq!(
            symbolic_path("TotalCount", Profile::Logix).unwrap(),
            [
                0x91, 10, b'T', b'o', b't', b'a', b'l', b'C', b'o', b'u', b'n', b't'
            ]
        );
        assert_eq!(
            symbolic_path("a[1,256,65536].b", Profile::Logix).unwrap(),
            [
                0x91, 1, b'a', 0, 0x28, 1, 0x29, 0, 0, 1, 0x2a, 0, 0, 0, 1, 0, 0x91, 1, b'b', 0
            ]
        );
        assert_eq!(
            symbolic_path("Program:Main.tag", Profile::Logix).unwrap()[..14],
            [
                0x91, 12, b'P', b'r', b'o', b'g', b'r', b'a', b'm', b':', b'M', b'a', b'i', b'n'
            ]
        );
        for bad in [
            "",
            "a.",
            "a[]",
            "a[1,]",
            "a[4294967296]",
            "a[x]",
            "a[1]x",
            "a..b",
            "a.0",
            "a b",
            "a\n",
            "aé",
        ] {
            symbolic_path(bad, Profile::Logix).unwrap_err();
        }
        symbolic_path(&"a".repeat(256), Profile::Logix).unwrap_err();
        let address = "127.0.0.1:44818".parse().unwrap();
        let mut cfg = config(address, None);
        cfg.signals[0].elements = 0;
        assert!(matches!(
            crate::types::classify(&open(&cfg).err().unwrap()).kind,
            ErrorKind::Usage
        ));
        cfg.signals[0].elements = 1;
        cfg.signals.push(cfg.signals[0].clone());
        open(&cfg).err().unwrap();
    }

    #[test]
    fn decodes_scalar_and_array_atomic_types_without_losing_integer_precision() {
        assert_eq!(decode(&[0xc1, 0, 0xff], 1).unwrap(), true);
        assert_eq!(decode(&[0xc1, 0, 1], 1).unwrap(), true);
        assert_eq!(decode(&[0xc1, 0, 0], 1).unwrap(), false);
        assert_eq!(decode(&[0xc2, 0, 0xfe], 1).unwrap(), -2);
        assert_eq!(decode(&[0xc3, 0, 0, 0x80], 1).unwrap(), i16::MIN);
        let mut wide = vec![0xc5, 0];
        wide.extend_from_slice(&i64::MIN.to_le_bytes());
        assert_eq!(decode(&wide, 1).unwrap(), i64::MIN);
        let mut wide = vec![0xc9, 0];
        wide.extend_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(decode(&wide, 1).unwrap(), u64::MAX);
        assert_eq!(
            decode(&[0xc4, 0, 1, 0, 0, 0, 2, 0, 0, 0], 2).unwrap(),
            json!([1, 2])
        );
        assert_eq!(decode(&[0xca, 0, 0, 0, 0xc0, 0x3f], 1).unwrap(), 1.5);
        let mut double = vec![0xcb, 0];
        double.extend_from_slice(&(-7.25_f64).to_le_bytes());
        assert_eq!(decode(&double, 1).unwrap(), -7.25);
        decode(&[0xc1, 0, 2], 1).unwrap_err();
        decode(&[0xc4, 0, 1, 0], 1).unwrap_err();
        decode(&[0xca, 0, 0, 0, 0x80, 0x7f], 1).unwrap_err();
        let error = decode(&[0xa0, 2, 0, 0], 1).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
    }

    #[test]
    fn registers_reads_split_tcp_reply_and_unregisters_without_tag_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            register(&mut stream);
            let (header, body) = request(&mut stream);
            assert_eq!(header[..8], [0x6f, 0, 36, 0, 0x11, 0x22, 0x33, 0x44]);
            assert_eq!(
                body[..16],
                [0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0, 20, 0]
            );
            assert_eq!(
                body[16..],
                [
                    0x4c, 8, 0x91, 4, b'T', b'a', b'n', b'k', 0x28, 2, 0x91, 5, b'L', b'e', b'v',
                    b'e', b'l', 0, 1, 0
                ]
            );
            reply(
                &mut stream,
                &header,
                0x4433_2211,
                &rr(&[0xcc, 0, 0, 0, 0xc4, 0, 0x2a, 0xff, 0xff, 0xff]),
                true,
            );
            let (header, body) = request(&mut stream);
            assert_eq!(header[..8], [0x66, 0, 0, 0, 0x11, 0x22, 0x33, 0x44]);
            assert_eq!(body, [] as [u8; 0]);
        });
        let mut source = open(&config(address, None)).unwrap();
        let values = source.read(Duration::from_secs(1)).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].signal, "level");
        assert_eq!(values[0].value, -214);
        drop(source);
        server.join().unwrap();
    }

    #[test]
    fn routes_to_one_backplane_slot_and_preserves_cip_status() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            register(&mut stream);
            let (header, body) = request(&mut stream);
            assert_eq!(
                &body[16..26],
                &[0x52, 2, 0x20, 6, 0x24, 1, 0x0a, 0x0e, 20, 0]
            );
            assert_eq!(&body[body.len() - 4..], &[1, 0, 1, 3]);
            reply(
                &mut stream,
                &header,
                0x4433_2211,
                &rr(&[0xcc, 0, 5, 1, 0x12, 0x34]),
                false,
            );
        });
        let mut source = open(&config(address, Some(3))).unwrap();
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, ErrorKind::Unavailable);
        assert_eq!(classified.details.unwrap()["cip_status"], 5);
        server.join().unwrap();
        assert_eq!(
            cip_reply(&[0xcc, 0, 0, 0, 0xc1, 0, 1], 0xcc, true)
                .unwrap()
                .data,
            [0xc1, 0, 1]
        );
        assert!(
            cip_reply(&[0xcc, 0, 6, 0, 0xc4, 0], 0xcc, false)
                .unwrap()
                .partial
        );
    }

    #[test]
    fn rejects_mismatched_context_invalid_lengths_and_truncated_status() {
        cpf(&[0; 16]).unwrap_err();
        cpf(&rr(&[1])[..16]).unwrap_err();
        cip_reply(&[0xcc, 0, 0, 1, 0], 0xcc, false).unwrap_err();
        cip_reply(&[0xcd, 0, 0, 0], 0xcc, false).unwrap_err();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            register(&mut stream);
            let (mut header, _) = request(&mut stream);
            header[12] ^= 1;
            reply(
                &mut stream,
                &header,
                0x4433_2211,
                &rr(&[0xcc, 0, 0, 0, 0xc1, 0, 1]),
                false,
            );
        });
        let mut source = open(&config(address, None)).unwrap();
        assert_eq!(
            crate::types::classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
            ErrorKind::OperationFailed
        );
        server.join().unwrap();
    }

    #[test]
    fn rejects_oversized_encapsulation_before_allocating_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (mut header, _) = request(&mut stream);
            header[2..4].copy_from_slice(&u16::MAX.to_le_bytes());
            header[4..8].copy_from_slice(&1_u32.to_le_bytes());
            stream.write_all(&header).unwrap();
        });
        let mut source = open(&config(address, None)).unwrap();
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            ErrorKind::OperationFailed
        );
        server.join().unwrap();
    }

    #[test]
    fn a_trickling_response_cannot_extend_the_whole_poll_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            register(&mut stream);
            let (mut header, _) = request(&mut stream);
            let body = rr(&[0xcc, 0, 0, 0, 0xc4, 0, 1, 0, 0, 0]);
            header[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
            header.extend_from_slice(&body);
            for byte in header {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let mut source = open(&config(address, None)).unwrap();
        let start = Instant::now();
        let error = source.read(Duration::from_millis(50)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        assert!(start.elapsed() < Duration::from_millis(250));
        server.join().unwrap();
    }
}
