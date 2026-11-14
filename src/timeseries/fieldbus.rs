//! Passive Ethernet process-data capture. No frame is ever transmitted.
//!
//! Wire references: Beckhoff `EtherCAT` Technology, sections 2.1–2.5:
//! <https://infosys.beckhoff.com/content/1033/tc3_io_intro/1257993099.html>.
//! PROFINET RT layout: PI North America, "PROFINET vs EtherNet/IP", slide 23:
//! <https://us.profinet.com/wp-content/uploads/2015/01/PROFINETvEIP.pdf>.
//! A configured PDO/IO layout is required; this does not configure a bus master.
#[cfg(any(target_os = "linux", test))]
use super::Reading;
use super::Source;
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
#[cfg(any(target_os = "linux", test))]
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EthercatConfig {
    pub interface: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub logical_address: u32,
    pub signals: Vec<Signal>,
    #[serde(default)]
    pub promiscuous: bool,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub source_mac: Option<String>,
    #[serde(
        default = "minimum_counter",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub minimum_working_counter: u16,
}
const fn minimum_counter() -> u16 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfinetConfig {
    pub interface: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub frame_id: u16,
    pub source_mac: String,
    /// All bytes between `FrameID` and the four-byte APDU status, including
    /// IOPS/IOCS and padding defined by the engineered communication relation.
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub data_length: u16,
    pub signals: Vec<Signal>,
    #[serde(default)]
    pub promiscuous: bool,
    #[serde(default)]
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    #[default]
    Incoming,
    Outgoing,
    Both,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    /// Byte offset from `logical_address` (`EtherCAT`) or after `FrameID` (PROFINET).
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub offset: u32,
    #[serde(default)]
    pub data_type: DataType,
    #[serde(default)]
    #[cfg_attr(
        not(any(target_os = "linux", test)),
        allow(
            dead_code,
            reason = "configuration is parsed on every platform; decoding is Linux-only"
        )
    )]
    pub byte_order: ByteOrder,
    /// For bool, the bit of the selected byte. Defaults to bit zero.
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub bit: Option<u8>,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default)]
    pub offset_value: f64,
    /// Optional PROFINET IOPS byte offset; bit 7 must indicate good data.
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub status_offset: Option<u32>,
}
const fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ByteOrder {
    #[default]
    Little,
    Big,
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
}
impl DataType {
    const fn width(self) -> u32 {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::Incomplete,
        format!("fieldbus capture: {message}"),
    )
}
fn interface_valid(interface: &str) -> Result<()> {
    if interface.is_empty()
        || interface.len() >= 16
        || interface.chars().any(|c| c.is_control() || c == '\0')
    {
        return Err(Usage(
            "fieldbus interface must be a local interface name of 1–15 bytes".into(),
        )
        .into());
    }
    Ok(())
}
fn mac(text: &str) -> Result<[u8; 6]> {
    let mut address = [0; 6];
    let mut parts = text.split(':');
    for byte in &mut address {
        let part = parts.next().filter(|part| part.len() == 2).ok_or_else(|| {
            Usage("MAC address must have six colon-separated hexadecimal octets".into())
        })?;
        *byte =
            u8::from_str_radix(part, 16).map_err(|_| Usage("invalid MAC address octet".into()))?;
    }
    if parts.next().is_some() {
        return Err(Usage("MAC address must have six octets".into()).into());
    }
    Ok(address)
}
fn validate_signals(signals: &[Signal], length: Option<u32>) -> Result<()> {
    if signals.is_empty() || signals.len() > 256 {
        return Err(Usage("fieldbus capture requires 1–256 signals".into()).into());
    }
    let mut names = BTreeSet::new();
    for signal in signals {
        if signal.name.is_empty()
            || signal.name.len() > 256
            || signal.name.chars().any(char::is_control)
            || !names.insert(&signal.name)
        {
            return Err(Usage(
                "fieldbus signal names must be unique nonempty text of at most 256 bytes".into(),
            )
            .into());
        }
        let end = signal
            .offset
            .checked_add(signal.data_type.width())
            .ok_or_else(|| Usage("signal byte range exceeds u32".into()))?;
        if length.is_some_and(|length| end > length) {
            return Err(
                Usage("signal byte range exceeds configured PROFINET data_length".into()).into(),
            );
        }
        if signal.bit.is_some_and(|bit| bit > 7)
            || (signal.bit.is_some() && !matches!(signal.data_type, DataType::Bool))
        {
            return Err(Usage("bit must be 0–7 and is only valid for bool signals".into()).into());
        }
        if !signal.scale.is_finite()
            || !signal.offset_value.is_finite()
            || (matches!(
                signal.data_type,
                DataType::Bool | DataType::U64 | DataType::I64
            ) && (signal.scale != 1.0 || signal.offset_value != 0.0))
        {
            return Err(Usage("scale/offset_value must be finite; bool and 64-bit integer signals cannot be scaled".into()).into());
        }
        match (length, signal.status_offset) {
            (Some(length), Some(offset)) if offset >= length => {
                return Err(Usage("status_offset exceeds PROFINET data_length".into()).into());
            }
            (None, Some(_)) => {
                return Err(
                    Usage("status_offset is only valid for PROFINET IOPS bytes".into()).into(),
                );
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
impl Signal {
    fn decode(&self, bytes: &[u8]) -> Result<Reading> {
        macro_rules! number {
            ($kind:ty, $width:expr) => {{
                let array: [u8; $width] = bytes
                    .try_into()
                    .map_err(|_| malformed("signal width mismatch"))?;
                match self.byte_order {
                    ByteOrder::Little => <$kind>::from_le_bytes(array),
                    ByteOrder::Big => <$kind>::from_be_bytes(array),
                }
            }};
        }
        let value = match self.data_type {
            DataType::Bool => Value::Bool(
                bytes
                    .first()
                    .ok_or_else(|| malformed("missing bool byte"))?
                    & (1 << self.bit.unwrap_or(0))
                    != 0,
            ),
            DataType::U8 => json!(number!(u8, 1)),
            DataType::I8 => json!(number!(i8, 1)),
            DataType::U16 => json!(number!(u16, 2)),
            DataType::I16 => json!(number!(i16, 2)),
            DataType::U32 => json!(number!(u32, 4)),
            DataType::I32 => json!(number!(i32, 4)),
            DataType::U64 => json!(number!(u64, 8)),
            DataType::I64 => json!(number!(i64, 8)),
            DataType::F32 => {
                let value = number!(f32, 4);
                if !value.is_finite() {
                    return Err(malformed("non-finite process-data float"));
                }
                json!(value)
            }
            DataType::F64 => {
                let value = number!(f64, 8);
                if !value.is_finite() {
                    return Err(malformed("non-finite process-data float"));
                }
                json!(value)
            }
        };
        let value = if self.scale == 1.0 && self.offset_value == 0.0 {
            value
        } else {
            let raw = value
                .as_f64()
                .ok_or_else(|| malformed("cannot scale this process-data value"))?;
            let scaled = raw * self.scale + self.offset_value;
            if !scaled.is_finite() {
                return Err(malformed("non-finite scaled process-data value"));
            }
            json!(scaled)
        };
        Ok(Reading {
            signal: self.name.clone(),
            value,
            source_time: None,
        })
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone)]
enum Capture {
    Ethercat {
        logical_address: u32,
        signals: Vec<Signal>,
        source_mac: Option<[u8; 6]>,
        minimum_working_counter: u16,
    },
    Profinet {
        frame_id: u16,
        source_mac: [u8; 6],
        data_length: usize,
        signals: Vec<Signal>,
    },
}
#[cfg(any(target_os = "linux", test))]
impl Capture {
    fn parse(&self, frame: &[u8]) -> Result<Vec<Reading>> {
        let Some(Ethernet {
            ether_type,
            source,
            payload,
        }) = ethernet(frame)?
        else {
            return Ok(Vec::new());
        };
        match self {
            Self::Ethercat {
                logical_address,
                signals,
                source_mac,
                minimum_working_counter,
            } => {
                if ether_type != 0x88a4 || source_mac.is_some_and(|expected| expected != source) {
                    return Ok(Vec::new());
                }
                ethercat(payload, *logical_address, signals, *minimum_working_counter)
            }
            Self::Profinet {
                frame_id,
                source_mac,
                data_length,
                signals,
            } => {
                if ether_type != 0x8892 || *source_mac != source {
                    return Ok(Vec::new());
                }
                profinet(payload, *frame_id, *data_length, signals)
            }
        }
    }
}

/// Classic socket BPF, installed before binding so unrelated camera/IP traffic
/// never occupies the fieldbus receive queue or contributes to its loss counter.
/// Linux filter ABI: <https://docs.kernel.org/networking/filter.html>.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy)]
struct PacketFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[cfg(any(target_os = "linux", test))]
fn packet_filter(capture: &Capture) -> Result<Vec<PacketFilter>> {
    #[derive(Clone, Copy)]
    enum Op {
        LoadHalf(u32),
        LoadWord(u32),
        Equal(u32, Option<usize>, Option<usize>),
        Return(u32),
        Label(usize),
    }
    const VLAN1: usize = 0;
    const VLAN2: usize = 1;
    const BODY0: usize = 2;
    const BODY1: usize = 3;
    const BODY2: usize = 4;
    const ACCEPT: usize = 5;
    const REJECT: usize = 6;
    let (protocol, source, frame_id) = match capture {
        Capture::Ethercat { source_mac, .. } => (0x88a4, *source_mac, None),
        Capture::Profinet {
            source_mac,
            frame_id,
            ..
        } => (0x8892, Some(*source_mac), Some(*frame_id)),
    };
    let mut operations = Vec::new();
    if let Some([a, b, c, d, e, f]) = source {
        operations.extend([
            Op::LoadWord(6),
            Op::Equal(u32::from_be_bytes([a, b, c, d]), None, Some(REJECT)),
            Op::LoadHalf(10),
            Op::Equal(u32::from(u16::from_be_bytes([e, f])), None, Some(REJECT)),
        ]);
    }
    operations.extend([
        Op::LoadHalf(12),
        Op::Equal(protocol, Some(BODY0), None),
        Op::Equal(0x8100, Some(VLAN1), None),
        Op::Equal(0x88a8, Some(VLAN1), Some(REJECT)),
        Op::Label(VLAN1),
        Op::LoadHalf(16),
        Op::Equal(protocol, Some(BODY1), None),
        Op::Equal(0x8100, Some(VLAN2), None),
        Op::Equal(0x88a8, Some(VLAN2), Some(REJECT)),
        Op::Label(VLAN2),
        Op::LoadHalf(20),
        Op::Equal(protocol, Some(BODY2), Some(REJECT)),
    ]);
    for (label, offset) in [(BODY0, 14), (BODY1, 18), (BODY2, 22)] {
        operations.push(Op::Label(label));
        if let Some(frame_id) = frame_id {
            operations.extend([
                Op::LoadHalf(offset),
                Op::Equal(u32::from(frame_id), Some(ACCEPT), Some(REJECT)),
            ]);
        } else {
            operations.push(Op::Return(u32::MAX));
        }
    }
    operations.extend([
        Op::Label(ACCEPT),
        Op::Return(u32::MAX),
        Op::Label(REJECT),
        Op::Return(0),
    ]);
    let mut labels = [0usize; 7];
    let mut pc = 0;
    for operation in &operations {
        if let Op::Label(label) = *operation {
            labels[label] = pc;
        } else {
            pc += 1;
        }
    }
    let mut filter = Vec::new();
    for operation in operations {
        let pc = filter.len();
        let jump = |label: Option<usize>| -> Result<u8> {
            label.map_or(Ok(0), |label| {
                let target = labels[label]
                    .checked_sub(pc + 1)
                    .ok_or_else(|| malformed("invalid capture filter jump"))?;
                Ok(u8::try_from(target)?)
            })
        };
        let instruction = match operation {
            Op::LoadHalf(k) => PacketFilter {
                code: 0x28,
                jt: 0,
                jf: 0,
                k,
            },
            Op::LoadWord(k) => PacketFilter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k,
            },
            Op::Equal(k, yes, no) => PacketFilter {
                code: 0x15,
                jt: jump(yes)?,
                jf: jump(no)?,
                k,
            },
            Op::Return(k) => PacketFilter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k,
            },
            Op::Label(_) => continue,
        };
        filter.push(instruction);
    }
    Ok(filter)
}

#[cfg(any(target_os = "linux", test))]
struct Ethernet<'a> {
    ether_type: u16,
    source: [u8; 6],
    payload: &'a [u8],
}

/// `PACKET_RAW` includes the MAC header but excludes preamble and usually FCS.
/// Two VLAN tags are accepted (802.1Q and 802.1ad); padding remains in payload.
#[cfg(any(target_os = "linux", test))]
fn ethernet(frame: &[u8]) -> Result<Option<Ethernet<'_>>> {
    if frame.len() < 14 {
        return Ok(None);
    }
    let source = frame[6..12]
        .try_into()
        .map_err(|_| malformed("invalid Ethernet source"))?;
    let mut at = 12;
    let mut ether_type = u16::from_be_bytes([frame[at], frame[at + 1]]);
    for _ in 0..2 {
        if !matches!(ether_type, 0x8100 | 0x88a8) {
            break;
        }
        at += 4;
        if frame.len() < at + 2 {
            return Err(malformed("truncated Ethernet VLAN tag"));
        }
        ether_type = u16::from_be_bytes([frame[at], frame[at + 1]]);
    }
    if matches!(ether_type, 0x8100 | 0x88a8) {
        return Ok(None);
    }
    Ok(Some(Ethernet {
        ether_type,
        source,
        payload: &frame[at + 2..],
    }))
}

#[cfg(any(target_os = "linux", test))]
fn ethercat(payload: &[u8], base: u32, signals: &[Signal], minimum: u16) -> Result<Vec<Reading>> {
    let header: [u8; 2] = payload
        .get(..2)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| malformed("truncated EtherCAT frame header"))?;
    let header = u16::from_le_bytes(header);
    if header >> 12 != 1 {
        return Ok(Vec::new());
    }
    if header & 0x0800 != 0 {
        return Err(malformed("reserved EtherCAT header bit set"));
    }
    let length = usize::from(header & 0x07ff);
    let data = payload
        .get(2..2 + length)
        .ok_or_else(|| malformed("truncated EtherCAT frame"))?;
    let mut at = 0;
    let mut readings = Vec::new();
    let mut count = 0;
    loop {
        count += 1;
        if count > 15 {
            return Err(malformed("EtherCAT frame exceeds 15 datagrams"));
        }
        let head: &[u8; 10] = data
            .get(at..at + 10)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| malformed("truncated EtherCAT datagram header"))?;
        let flags = u16::from_le_bytes([head[6], head[7]]);
        if flags & 0x3800 != 0 {
            return Err(malformed("reserved EtherCAT datagram flags set"));
        }
        let length = usize::from(flags & 0x07ff);
        let body = data
            .get(at + 10..at + 10 + length)
            .ok_or_else(|| malformed("truncated EtherCAT process data"))?;
        let counter: [u8; 2] = data
            .get(at + 10 + length..at + 12 + length)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| malformed("missing EtherCAT working counter"))?;
        let counter = u16::from_le_bytes(counter);
        if matches!(head[0], 0x0a | 0x0c) && counter >= minimum {
            let address = u32::from_le_bytes([head[2], head[3], head[4], head[5]]);
            let end = u64::from(address) + u64::try_from(body.len())?;
            for signal in signals {
                let signal_address = u64::from(base) + u64::from(signal.offset);
                let signal_end = signal_address + u64::from(signal.data_type.width());
                if signal_address >= u64::from(address) && signal_end <= end {
                    let offset = usize::try_from(signal_address - u64::from(address))?;
                    let width = usize::try_from(signal.data_type.width())?;
                    readings.push(signal.decode(&body[offset..offset + width])?);
                    if readings.len() > 256 {
                        return Err(malformed("too many mapped EtherCAT readings in one frame"));
                    }
                }
            }
        }
        at += 12 + length;
        if flags & 0x8000 == 0 {
            if at != data.len() {
                return Err(malformed(
                    "EtherCAT frame length disagrees with last datagram",
                ));
            }
            break;
        }
        if at == data.len() {
            return Err(malformed("EtherCAT more flag without next datagram"));
        }
    }
    Ok(readings)
}

#[cfg(any(target_os = "linux", test))]
fn profinet(
    payload: &[u8],
    frame_id: u16,
    data_length: usize,
    signals: &[Signal],
) -> Result<Vec<Reading>> {
    let Some(header): Option<[u8; 2]> = payload.get(..2).and_then(|bytes| bytes.try_into().ok())
    else {
        return Err(malformed("truncated PROFINET FrameID"));
    };
    if u16::from_be_bytes(header) != frame_id {
        return Ok(Vec::new());
    }
    let body = payload
        .get(2..2 + data_length)
        .ok_or_else(|| malformed("truncated PROFINET IO data"))?;
    let status: &[u8; 4] = payload
        .get(2 + data_length..6 + data_length)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| malformed("truncated PROFINET APDU status"))?;
    if status[2] & 0x04 == 0 || status[3] != 0 {
        return Err(malformed(
            "PROFINET provider marked the frame invalid or unsuccessful",
        ));
    }
    let mut readings = Vec::with_capacity(signals.len());
    for signal in signals {
        if let Some(offset) = signal.status_offset {
            let iops = body
                .get(usize::try_from(offset)?)
                .ok_or_else(|| malformed("PROFINET IOPS outside IO data"))?;
            if iops & 0x80 == 0 {
                return Err(malformed("PROFINET submodule IOPS indicates bad data"));
            }
        }
        let offset = usize::try_from(signal.offset)?;
        let width = usize::try_from(signal.data_type.width())?;
        let bytes = body
            .get(offset..offset + width)
            .ok_or_else(|| malformed("PROFINET signal outside IO data"))?;
        readings.push(signal.decode(bytes)?);
    }
    Ok(readings)
}

pub(crate) fn open_ethercat(config: &EthercatConfig) -> Result<Box<dyn Source>> {
    interface_valid(&config.interface)?;
    validate_signals(&config.signals, None)?;
    if config.minimum_working_counter == 0 {
        return Err(Usage(
            "minimum_working_counter must be positive to exclude unprocessed requests".into(),
        )
        .into());
    }
    for signal in &config.signals {
        if config
            .logical_address
            .checked_add(signal.offset)
            .and_then(|address| address.checked_add(signal.data_type.width() - 1))
            .is_none()
        {
            return Err(
                Usage("EtherCAT signal exceeds 32-bit logical address space".into()).into(),
            );
        }
    }
    let source_mac = config.source_mac.as_deref().map(mac).transpose()?;
    #[cfg(target_os = "linux")]
    {
        let capture = Capture::Ethercat {
            logical_address: config.logical_address,
            signals: config.signals.clone(),
            source_mac,
            minimum_working_counter: config.minimum_working_counter,
        };
        Ok(Box::new(linux::PacketSource::new(
            &config.interface,
            config.promiscuous,
            config.direction,
            capture,
        )))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = source_mac;
        unsupported(&config.interface, config.promiscuous, config.direction)
    }
}
pub(crate) fn open_profinet(config: &ProfinetConfig) -> Result<Box<dyn Source>> {
    interface_valid(&config.interface)?;
    if !(0x8000..=0xfbff).contains(&config.frame_id) || !(40..=1440).contains(&config.data_length) {
        return Err(Usage(
            "PROFINET requires a cyclic RT FrameID 0x8000–0xfbff and data_length 40–1440 bytes"
                .into(),
        )
        .into());
    }
    validate_signals(&config.signals, Some(u32::from(config.data_length)))?;
    let source_mac = mac(&config.source_mac)?;
    #[cfg(target_os = "linux")]
    {
        let capture = Capture::Profinet {
            frame_id: config.frame_id,
            source_mac,
            data_length: usize::from(config.data_length),
            signals: config.signals.clone(),
        };
        Ok(Box::new(linux::PacketSource::new(
            &config.interface,
            config.promiscuous,
            config.direction,
            capture,
        )))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = source_mac;
        unsupported(&config.interface, config.promiscuous, config.direction)
    }
}

#[cfg(not(target_os = "linux"))]
fn unsupported(
    interface: &str,
    promiscuous: bool,
    direction: Direction,
) -> Result<Box<dyn Source>> {
    Err(Categorized::new(ErrorKind::Unsupported, "native passive EtherCAT/PROFINET capture requires Linux AF_PACKET")
        .hint("use a Linux host connected to a network TAP/mirror port, or configure an external protocol helper")
        .details(json!({"interface":interface,"promiscuous":promiscuous,"direction":format!("{direction:?}").to_lowercase()}))
        .into())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Capture, Direction, packet_filter};
    use crate::timeseries::{Reading, Source};
    use crate::types::{Categorized, ErrorKind};
    use anyhow::Result;
    use std::{
        collections::BTreeSet,
        ffi::CString,
        io, mem,
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        time::{Duration, Instant},
    };

    const MAX_FRAME: usize = 65_536;
    pub(super) struct PacketSource {
        interface: String,
        promiscuous: bool,
        direction: Direction,
        capture: Capture,
        socket: Option<OwnedFd>,
        unobserved: BTreeSet<String>,
        frame: Vec<u8>,
    }
    impl PacketSource {
        pub(super) fn new(
            interface: &str,
            promiscuous: bool,
            direction: Direction,
            capture: Capture,
        ) -> Self {
            let unobserved = match &capture {
                Capture::Ethercat { signals, .. } | Capture::Profinet { signals, .. } => {
                    signals.iter().map(|signal| signal.name.clone()).collect()
                }
            };
            Self {
                interface: interface.into(),
                promiscuous,
                direction,
                capture,
                socket: None,
                unobserved,
                frame: vec![0; MAX_FRAME],
            }
        }
        fn connect(&mut self) -> Result<()> {
            let interface = CString::new(self.interface.as_bytes())?;
            // SAFETY: interface is NUL-terminated and lives through the call.
            let index = unsafe { libc::if_nametoindex(interface.as_ptr()) };
            if index == 0 {
                return Err(failure("find interface", &io::Error::last_os_error()));
            }
            let protocol = u16::try_from(libc::ETH_P_ALL)?.to_be();
            // SAFETY: creates an independent nonblocking packet socket; no pointers.
            let fd = unsafe {
                libc::socket(
                    libc::AF_PACKET,
                    libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0, // No reception until the filter is installed and bind enables ETH_P_ALL.
                )
            };
            if fd < 0 {
                return Err(failure(
                    "open passive capture socket",
                    &io::Error::last_os_error(),
                ));
            }
            // SAFETY: socket returned a new owned descriptor above.
            let socket = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut filter = packet_filter(&self.capture)?
                .into_iter()
                .map(|instruction| libc::sock_filter {
                    code: instruction.code,
                    jt: instruction.jt,
                    jf: instruction.jf,
                    k: instruction.k,
                })
                .collect::<Vec<_>>();
            let program = libc::sock_fprog {
                len: u16::try_from(filter.len())?,
                filter: filter.as_mut_ptr(),
            };
            // SAFETY: program points to a live, fully initialized instruction array;
            // the kernel validates and copies it before setsockopt returns.
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ATTACH_FILTER,
                    (&raw const program).cast(),
                    libc::socklen_t::try_from(size_of_val(&program))?,
                )
            } < 0
            {
                return Err(failure(
                    "install fieldbus capture filter",
                    &io::Error::last_os_error(),
                ));
            }
            // SAFETY: sockaddr_ll is a plain C structure valid when zeroed.
            let mut address: libc::sockaddr_ll = unsafe { mem::zeroed() };
            address.sll_family = u16::try_from(libc::AF_PACKET)?;
            address.sll_protocol = protocol;
            address.sll_ifindex = i32::try_from(index)?;
            // SAFETY: address is initialized and its pointer/length match sockaddr_ll.
            if unsafe {
                libc::bind(
                    fd,
                    (&raw const address).cast(),
                    libc::socklen_t::try_from(size_of_val(&address))?,
                )
            } < 0
            {
                return Err(failure(
                    "bind capture interface",
                    &io::Error::last_os_error(),
                ));
            }
            if self.promiscuous {
                // SAFETY: packet_mreq is a C structure valid when zeroed.
                let mut membership: libc::packet_mreq = unsafe { mem::zeroed() };
                membership.mr_ifindex = i32::try_from(index)?;
                membership.mr_type = u16::try_from(libc::PACKET_MR_PROMISC)?;
                // SAFETY: membership is initialized, with a matching pointer/length;
                // closing this socket removes this membership automatically.
                if unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_PACKET,
                        libc::PACKET_ADD_MEMBERSHIP,
                        (&raw const membership).cast(),
                        libc::socklen_t::try_from(size_of_val(&membership))?,
                    )
                } < 0
                {
                    return Err(failure(
                        "enable socket promiscuous membership",
                        &io::Error::last_os_error(),
                    ));
                }
            }
            self.socket = Some(socket);
            Ok(())
        }
        fn dropped(&self) -> Result<()> {
            let Some(socket) = &self.socket else {
                return Ok(());
            };
            // PACKET_STATISTICS resets the counter after each read. A kernel
            // queue overflow is an acquisition failure, never silent data loss.
            // SAFETY: tpacket_stats is a plain C structure valid when zeroed.
            let mut stats: libc::tpacket_stats = unsafe { mem::zeroed() };
            let mut length = libc::socklen_t::try_from(size_of_val(&stats))?;
            // SAFETY: stats and length are writable initialized allocations.
            if unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_PACKET,
                    libc::PACKET_STATISTICS,
                    (&raw mut stats).cast(),
                    &raw mut length,
                )
            } < 0
            {
                return Err(failure(
                    "read capture loss counter",
                    &io::Error::last_os_error(),
                ));
            }
            if stats.tp_drops != 0 {
                return Err(Categorized::new(
                    ErrorKind::Incomplete,
                    "passive fieldbus socket lost packets; recording stopped",
                )
                .details(serde_json::json!({"dropped_packets": stats.tp_drops}))
                .into());
            }
            Ok(())
        }
    }
    fn failure(action: &str, error: &io::Error) -> anyhow::Error {
        let kind = if error.kind() == io::ErrorKind::PermissionDenied {
            ErrorKind::PermissionDenied
        } else {
            ErrorKind::Unavailable
        };
        Categorized::new(kind, format!("cannot {action}: {error}"))
            .hint("native passive capture needs Linux CAP_NET_RAW (or root) and an interface connected to the fieldbus TAP/mirror port")
            .into()
    }
    impl Source for PacketSource {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now() + timeout;
            if self.socket.is_none() {
                self.connect()?;
            }
            let socket = self.socket.as_ref().ok_or_else(|| {
                Categorized::error(ErrorKind::Unavailable, "capture socket missing")
            })?;
            let fd = socket.as_raw_fd();
            let frame = &mut self.frame;
            let mut inspected = 0;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    self.dropped()?;
                    return Ok(Vec::new());
                }
                let mut event = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let millis = i32::try_from(remaining.as_millis())
                    .unwrap_or(i32::MAX)
                    .max(1);
                // SAFETY: event is one initialized pollfd; fd remains owned and open.
                let count = unsafe { libc::poll(&raw mut event, 1, millis) };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(failure("wait for fieldbus frame", &error));
                }
                if count == 0 {
                    self.dropped()?;
                    return Ok(Vec::new());
                }
                if event.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "passive capture interface became unavailable",
                    ));
                }
                // SAFETY: sockaddr_ll is a plain C structure valid when zeroed.
                let mut address: libc::sockaddr_ll = unsafe { mem::zeroed() };
                let mut length = libc::socklen_t::try_from(size_of_val(&address))?;
                // SAFETY: frame is writable for its reported size; address and
                // length describe a writable sockaddr_ll. MSG_TRUNC reports
                // the full wire length so oversize frames cannot be accepted.
                let count = unsafe {
                    libc::recvfrom(
                        fd,
                        frame.as_mut_ptr().cast(),
                        frame.len(),
                        libc::MSG_TRUNC,
                        (&raw mut address).cast(),
                        &raw mut length,
                    )
                };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) {
                        continue;
                    }
                    return Err(failure("receive fieldbus frame", &error));
                }
                let count = usize::try_from(count)?;
                if count > frame.len() {
                    return Err(Categorized::error(
                        ErrorKind::Incomplete,
                        "captured Ethernet frame exceeded 64 KiB",
                    ));
                }
                let outgoing = address.sll_pkttype == libc::PACKET_OUTGOING;
                let accept = match self.direction {
                    Direction::Incoming => !outgoing,
                    Direction::Outgoing => outgoing,
                    Direction::Both => true,
                };
                if accept {
                    let readings = self.capture.parse(&frame[..count])?;
                    if !readings.is_empty() {
                        for reading in &readings {
                            self.unobserved.remove(&reading.signal);
                        }
                        self.dropped()?;
                        return Ok(readings);
                    }
                }
                inspected += 1;
                if inspected == 256 {
                    self.dropped()?;
                    return Ok(Vec::new());
                }
            }
        }
        fn ready(&self) -> bool {
            self.unobserved.is_empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_fieldbus_integer_configuration_preserves_defaults_and_optionals() {
        let ethercat: EthercatConfig = serde_json::from_str(
            r#"{"interface":"eth0","logical_address":4.294967295e9,"minimum_working_counter":1e0,"signals":[{"name":"ready","offset":1.0,"data_type":"bool","bit":7e0,"status_offset":2.0}]}"#,
        )
        .unwrap();
        assert_eq!(ethercat.logical_address, u32::MAX);
        assert_eq!(ethercat.minimum_working_counter, 1);
        assert_eq!(ethercat.signals[0].offset, 1);
        assert_eq!(ethercat.signals[0].bit, Some(7));
        assert_eq!(ethercat.signals[0].status_offset, Some(2));
        let profinet: ProfinetConfig = serde_json::from_value(
            serde_json::from_str(
                r#"{"interface":"eth0","frame_id":3.2768e4,"source_mac":"02:00:00:00:00:01","data_length":16.0,"signals":[{"name":"ready","offset":0e0,"bit":null,"status_offset":null}]}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(profinet.frame_id, 0x8000);
        assert_eq!(profinet.data_length, 16);
        assert_eq!(profinet.signals[0].offset, 0);
        assert_eq!(profinet.signals[0].bit, None);
        assert_eq!(profinet.signals[0].status_offset, None);
        let defaulted: EthercatConfig =
            serde_json::from_str(r#"{"interface":"eth0","logical_address":0.0,"signals":[]}"#)
                .unwrap();
        assert_eq!(defaulted.minimum_working_counter, 1);
        serde_json::from_str::<ProfinetConfig>(
            r#"{"interface":"eth0","frame_id":65536.0,"source_mac":"02:00:00:00:00:01","data_length":16,"signals":[]}"#,
        ).unwrap_err();
    }

    fn signal(name: &str, offset: u32, kind: &str) -> Signal {
        serde_json::from_value(json!({"name":name,"offset":offset,"data_type":kind})).unwrap()
    }
    fn ethernet_frame(kind: u16, payload: &[u8], tags: usize) -> Vec<u8> {
        let mut frame = vec![0xff; 6];
        frame.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        for index in 0..tags {
            let tag: u16 = if index == 0 && tags == 2 {
                0x88a8
            } else {
                0x8100
            };
            frame.extend_from_slice(&tag.to_be_bytes());
            frame.extend_from_slice(&[0xc0, 0]);
        }
        frame.extend_from_slice(&kind.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }
    fn datagram(cmd: u8, address: u32, data: &[u8], wkc: u16, more: bool) -> Vec<u8> {
        let mut datagram = vec![cmd, 1];
        datagram.extend_from_slice(&address.to_le_bytes());
        let flags = u16::try_from(data.len()).unwrap() | if more { 0x8000 } else { 0 };
        datagram.extend_from_slice(&flags.to_le_bytes());
        datagram.extend_from_slice(&[0, 0]);
        datagram.extend_from_slice(data);
        datagram.extend_from_slice(&wkc.to_le_bytes());
        datagram
    }
    fn ecat_payload(datagrams: &[Vec<u8>]) -> Vec<u8> {
        let length: usize = datagrams.iter().map(Vec::len).sum();
        let mut payload = (u16::try_from(length).unwrap() | 0x1000)
            .to_le_bytes()
            .to_vec();
        for datagram in datagrams {
            payload.extend_from_slice(datagram);
        }
        payload
    }
    fn ecat_capture(signals: Vec<Signal>) -> Capture {
        Capture::Ethercat {
            logical_address: 0x10000,
            signals,
            source_mac: None,
            minimum_working_counter: 1,
        }
    }
    fn pn_capture() -> Capture {
        let mut pressure = signal("pressure", 0, "u16");
        pressure.byte_order = ByteOrder::Big;
        pressure.scale = 0.01;
        pressure.status_offset = Some(2);
        Capture::Profinet {
            frame_id: 0x8000,
            source_mac: [2, 0, 0, 0, 0, 1],
            data_length: 40,
            signals: vec![pressure],
        }
    }
    fn pn_payload() -> Vec<u8> {
        let mut payload = vec![0; 46];
        payload[..2].copy_from_slice(&0x8000_u16.to_be_bytes());
        payload[2..5].copy_from_slice(&[0x04, 0xd2, 0x80]);
        payload[42..].copy_from_slice(&[0x12, 0x34, 0x35, 0]);
        payload
    }

    /// A reference interpreter for the three classic-BPF operations emitted
    /// above, including the kernel's reject-on-out-of-bounds load behavior.
    fn filter_accepts(capture: &Capture, packet: &[u8]) -> bool {
        let filter = packet_filter(capture).unwrap();
        let mut accumulator = 0;
        let mut pc = 0;
        while let Some(instruction) = filter.get(pc) {
            match instruction.code {
                0x20 => {
                    let offset = usize::try_from(instruction.k).unwrap();
                    let Some(value) = packet.get(offset..offset + 4) else {
                        return false;
                    };
                    accumulator = u32::from_be_bytes(value.try_into().unwrap());
                }
                0x28 => {
                    let offset = usize::try_from(instruction.k).unwrap();
                    let Some(value) = packet.get(offset..offset + 2) else {
                        return false;
                    };
                    accumulator = u32::from(u16::from_be_bytes(value.try_into().unwrap()));
                }
                0x15 => {
                    pc += usize::from(if accumulator == instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    });
                }
                0x06 => return instruction.k != 0,
                operation => panic!("unexpected classic BPF operation: {operation}"),
            }
            pc += 1;
        }
        panic!("capture filter has no return");
    }

    #[test]
    fn socket_filter_admits_only_selected_provider_protocol_and_frame_id() {
        let ethercat = ecat_capture(vec![signal("x", 0, "u16")]);
        let profinet = pn_capture();
        for tags in 0..=2 {
            let ethercat_frame = ethernet_frame(0x88a4, &[0; 30], tags);
            assert!(filter_accepts(&ethercat, &ethercat_frame));
            assert!(!filter_accepts(&profinet, &ethercat_frame));
            let profinet_frame = ethernet_frame(0x8892, &pn_payload(), tags);
            assert!(filter_accepts(&profinet, &profinet_frame));
            assert!(!filter_accepts(&ethercat, &profinet_frame));
            let ip_frame = ethernet_frame(0x0800, &[0; 1500], tags);
            assert!(!filter_accepts(&ethercat, &ip_frame));
            assert!(!filter_accepts(&profinet, &ip_frame));
            let mut wrong_provider = profinet_frame.clone();
            wrong_provider[6] = 0x04;
            assert!(!filter_accepts(&profinet, &wrong_provider));
            let mut wrong_frame_id = profinet_frame.clone();
            wrong_frame_id[14 + 4 * tags] ^= 1;
            assert!(!filter_accepts(&profinet, &wrong_frame_id));
            for len in 0..16 + 4 * tags {
                assert!(!filter_accepts(&profinet, &profinet_frame[..len]));
            }
        }
        let source_filtered = Capture::Ethercat {
            logical_address: 0x10000,
            signals: vec![signal("x", 0, "u16")],
            source_mac: Some([2, 0, 0, 0, 0, 1]),
            minimum_working_counter: 1,
        };
        let frame = ethernet_frame(0x88a4, &[0; 30], 2);
        assert!(filter_accepts(&source_filtered, &frame));
        let mut wrong_source = frame;
        wrong_source[11] = 2;
        assert!(!filter_accepts(&source_filtered, &wrong_source));
        assert!(!filter_accepts(
            &ethercat,
            &ethernet_frame(0x88a4, &[0; 30], 3)
        ));
    }

    #[test]
    fn ethercat_maps_completed_logical_reads_across_datagrams_and_vlan_tags() {
        let mut temperature = signal("temperature", 0, "i16");
        temperature.scale = 0.1;
        let ready = signal("ready", 3, "bool");
        let payload = ecat_payload(&[
            datagram(0x0b, 0x10000, &[0, 0], 1, true), // logical write is not an input observation
            datagram(0x0a, 0x10000, &[0x38, 0xff, 0, 1], 1, true),
            datagram(0x0c, 0x20000, &[0, 0, 0, 0], 3, false),
        ]);
        for tags in 0..=2 {
            let mut frame = ethernet_frame(0x88a4, &payload, tags);
            frame.extend_from_slice(&[0; 32]); // Ethernet padding is not another datagram
            let readings = ecat_capture(vec![temperature.clone(), ready.clone()])
                .parse(&frame)
                .unwrap();
            assert_eq!(readings.len(), 2);
            assert_eq!(readings[0].signal, "temperature");
            assert_eq!(readings[0].value, json!(-20.0));
            assert_eq!(readings[1].value, true);
        }
    }

    #[test]
    fn ethercat_filters_unprocessed_requests_other_addresses_and_source_macs() {
        let capture = ecat_capture(vec![signal("x", 0, "u16")]);
        for (address, wkc) in [(0x10000, 0), (0x20000, 1)] {
            let payload = ecat_payload(&[datagram(0x0a, address, &[42, 0], wkc, false)]);
            assert!(
                capture
                    .parse(&ethernet_frame(0x88a4, &payload, 0))
                    .unwrap()
                    .is_empty()
            );
        }
        let payload = ecat_payload(&[datagram(0x0a, 0x10000, &[42, 0], 1, false)]);
        let capture = Capture::Ethercat {
            logical_address: 0x10000,
            signals: vec![signal("x", 0, "u16")],
            source_mac: Some([0; 6]),
            minimum_working_counter: 1,
        };
        assert!(
            capture
                .parse(&ethernet_frame(0x88a4, &payload, 0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ethercat_rejects_truncated_lengths_reserved_flags_and_missing_following_datagrams() {
        let capture = ecat_capture(vec![signal("x", 0, "u16")]);
        let payload = ecat_payload(&[datagram(0x0a, 0x10000, &[42, 0], 1, false)]);
        let frame = ethernet_frame(0x88a4, &payload, 0);
        for length in 14..frame.len() {
            capture.parse(&frame[..length]).unwrap_err();
        }
        let mut reserved = payload.clone();
        reserved[8] |= 0x08;
        capture
            .parse(&ethernet_frame(0x88a4, &reserved, 0))
            .unwrap_err();
        let missing = ecat_payload(&[datagram(0x0a, 0x10000, &[42, 0], 1, true)]);
        capture
            .parse(&ethernet_frame(0x88a4, &missing, 0))
            .unwrap_err();
        let mut too_long = payload;
        too_long.push(0);
        let length = u16::try_from(too_long.len() - 2).unwrap() | 0x1000;
        too_long[..2].copy_from_slice(&length.to_le_bytes());
        capture
            .parse(&ethernet_frame(0x88a4, &too_long, 0))
            .unwrap_err();
    }

    #[test]
    fn profinet_matches_frame_and_provider_and_validates_global_and_submodule_status() {
        let payload = pn_payload();
        let readings = pn_capture()
            .parse(&ethernet_frame(0x8892, &payload, 1))
            .unwrap();
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value, json!(12.34));
        let mut wrong = ethernet_frame(0x8892, &payload, 0);
        wrong[6] = 4;
        assert!(pn_capture().parse(&wrong).unwrap().is_empty());
        let mut wrong = payload.clone();
        wrong[1] = 1;
        assert!(
            pn_capture()
                .parse(&ethernet_frame(0x8892, &wrong, 0))
                .unwrap()
                .is_empty()
        );
        for (offset, value) in [(44, 0x31), (45, 1), (4, 0)] {
            let mut bad = payload.clone();
            bad[offset] = value;
            pn_capture()
                .parse(&ethernet_frame(0x8892, &bad, 0))
                .unwrap_err();
        }
        for length in 14..60 {
            pn_capture()
                .parse(&ethernet_frame(0x8892, &payload, 0)[..length])
                .unwrap_err();
        }
    }

    #[test]
    fn typed_values_preserve_large_integers_bits_and_endianness_and_reject_nan() {
        let wide = signal("count", 0, "u64");
        assert_eq!(
            wide.decode(&u64::MAX.to_le_bytes()).unwrap().value,
            json!(u64::MAX)
        );
        let signed = signal("signed", 0, "i64");
        assert_eq!(
            signed.decode(&i64::MIN.to_le_bytes()).unwrap().value,
            json!(i64::MIN)
        );
        let mut bit = signal("flag", 0, "bool");
        bit.bit = Some(7);
        assert_eq!(bit.decode(&[0x80]).unwrap().value, true);
        let mut float = signal("float", 0, "f32");
        float.byte_order = ByteOrder::Big;
        assert_eq!(
            float.decode(&12.5_f32.to_be_bytes()).unwrap().value,
            json!(12.5)
        );
        float.decode(&f32::NAN.to_be_bytes()).unwrap_err();
    }

    #[test]
    fn invalid_signal_ranges_scaling_and_quality_mapping_are_rejected() {
        let mut signal = signal("x", 0, "u64");
        signal.scale = 0.1;
        validate_signals(&[signal.clone()], None).unwrap_err();
        signal.scale = 1.0;
        signal.status_offset = Some(2);
        validate_signals(&[signal.clone()], None).unwrap_err();
        signal.status_offset = Some(40);
        validate_signals(&[signal.clone()], Some(40)).unwrap_err();
        signal.status_offset = None;
        signal.offset = 39;
        validate_signals(&[signal.clone()], Some(40)).unwrap_err();
        signal.offset = 0;
        validate_signals(&[signal.clone(), signal], Some(40)).unwrap_err();
        mac("00:00:00:00:00:gg").unwrap_err();
        interface_valid("").unwrap_err();
    }
}
