//! Read-only classic S7 communication over RFC 1006 / ISO-on-TCP.
//!
//! Wire fields were checked against RFC 1006 and the Snap7 protocol
//! declarations, not linked to or copied from its LGPL implementation:
//! <https://www.rfc-editor.org/rfc/rfc1006>
//! <https://github.com/SCADACS/snap7/blob/master/src/core/s7_types.h>
//! <https://snap7.sourceforge.net/plc_connection.html>
//! Only COTP connection, S7 setup communication and Read Var are emitted.
//! `STRING`/`WSTRING` layouts follow Siemens STEP 7 and S7-1200 documentation:
//! <https://docs.tia.siemens.cloud/r/en-us/v21/data-types/character-strings/character-strings/structure-of-a-string-tag>
//! <https://docs.tia.siemens.cloud/r/simatic_s7_1200_manual_collection_eses_20/plc-concepts/data-types/character-and-string-data-types>
//! Contiguous arrays use configured absolute storage, with no inferred DB layout.
//! Siemens' standard-access BOOL overlay shows bit offsets 0.0 through 0.7:
//! <https://cache.industry.siemens.com/dl/files/145/39710145/att_5787/v1/s71200_easy_book_en-US_en-US.pdf>
//! Indexed timer/counter requests use the matching S7-Any word lengths.
//! <https://github.com/SCADACS/snap7/blob/master/src/core/s7_micro_client.cpp>
//! <https://docs.tia.siemens.cloud/r/en-us/v20/data-types/timers/s5time-duration-s7-300-s7-400>
mod temporal;
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Read as _, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const MAX_PDU: usize = 960;
const MAX_VALUE: usize = 131_072;
const MAX_BATCH: usize = 1 << 20;
const MAX_TPKT: usize = 1028;
const SOURCE_REFERENCE: u16 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub rack: u8,
    #[serde(default = "default_slot")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub slot: u8,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub local_tsap: Option<u16>,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub remote_tsap: Option<u16>,
    pub signals: Vec<Signal>,
}
const fn default_slot() -> u8 {
    2
}
const fn one_element() -> u16 {
    1
}
const fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    #[serde(default)]
    pub area: Area,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub db: u16,
    /// Byte offset or the element number in a timer/counter area.
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub address: u32,
    #[serde(default)]
    pub data_type: DataType,
    /// Contiguous values; BOOL advances by bit, indexed T/C by element.
    #[serde(
        default = "one_element",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub elements: u16,
    /// Declared STRING bytes or WSTRING WCHAR units, excluding the header.
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub capacity: Option<u16>,
    /// STRING codepage only; omitted means exact raw bytes without guessed text.
    #[serde(default)]
    pub encoding: Option<TextEncoding>,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub bit: u8,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Area {
    #[default]
    Db,
    Input,
    Output,
    Merker,
    Timer,
    Counter,
}
impl Area {
    const fn code(self) -> u8 {
        match self {
            Self::Input => 0x81,
            Self::Output => 0x82,
            Self::Merker => 0x83,
            Self::Db => 0x84,
            Self::Timer => 0x1d,
            Self::Counter => 0x1c,
        }
    }
    const fn word_indexed(self) -> bool {
        matches!(self, Self::Timer | Self::Counter)
    }
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
    S5time,
    Date,
    Time,
    #[serde(alias = "time_of_day")]
    Tod,
    #[serde(alias = "dt")]
    DateAndTime,
    Dtl,
    Ltime,
    #[serde(alias = "ltime_of_day")]
    Ltod,
    #[serde(alias = "date_and_ltime")]
    Ldt,
    String,
    Wstring,
}
impl DataType {
    const fn temporal(self) -> bool {
        matches!(
            self,
            Self::Date
                | Self::Time
                | Self::Tod
                | Self::DateAndTime
                | Self::Dtl
                | Self::Ltime
                | Self::Ltod
                | Self::Ldt
        )
    }
    const fn width(self) -> u8 {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 | Self::S5time | Self::Date => 2,
            Self::U32 | Self::I32 | Self::F32 | Self::Time | Self::Tod => 4,
            Self::U64
            | Self::I64
            | Self::F64
            | Self::DateAndTime
            | Self::Ltime
            | Self::Ltod
            | Self::Ldt => 8,
            Self::Dtl => 12,
            Self::String | Self::Wstring => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TextEncoding {
    Ascii,
    Utf8,
    Windows1252,
}
impl TextEncoding {
    const fn name(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::Utf8 => "utf8",
            Self::Windows1252 => "windows1252",
        }
    }
    fn decode(self, bytes: &[u8]) -> Option<String> {
        match self {
            Self::Ascii if !bytes.is_ascii() => None,
            Self::Ascii | Self::Utf8 => std::str::from_utf8(bytes).ok().map(str::to_owned),
            Self::Windows1252 => bytes.iter().map(|&byte| windows1252(byte)).collect(),
        }
    }
}
fn windows1252(byte: u8) -> Option<char> {
    // Unicode's Microsoft CP1252 mapping leaves five byte values undefined.
    // Preserve those as raw data instead of inventing replacement characters.
    // <https://www.unicode.org/Public/MAPPINGS/VENDORS/MICSFT/WINDOWS/CP1252.TXT>
    const EXTENDED: [Option<char>; 32] = [
        Some('\u{20ac}'),
        None,
        Some('\u{201a}'),
        Some('\u{0192}'),
        Some('\u{201e}'),
        Some('\u{2026}'),
        Some('\u{2020}'),
        Some('\u{2021}'),
        Some('\u{02c6}'),
        Some('\u{2030}'),
        Some('\u{0160}'),
        Some('\u{2039}'),
        Some('\u{0152}'),
        None,
        Some('\u{017d}'),
        None,
        None,
        Some('\u{2018}'),
        Some('\u{2019}'),
        Some('\u{201c}'),
        Some('\u{201d}'),
        Some('\u{2022}'),
        Some('\u{2013}'),
        Some('\u{2014}'),
        Some('\u{02dc}'),
        Some('\u{2122}'),
        Some('\u{0161}'),
        Some('\u{203a}'),
        Some('\u{0153}'),
        None,
        Some('\u{017e}'),
        Some('\u{0178}'),
    ];
    if (0x80..=0x9f).contains(&byte) {
        EXTENDED.get(usize::from(byte - 0x80)).copied().flatten()
    } else {
        Some(char::from(byte))
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 || config.rack > 7 || config.slot > 31 {
        return Err(Usage("S7 needs a nonzero TCP port, rack 0–7 and slot 0–31".into()).into());
    }
    if config.local_tsap.is_some() != config.remote_tsap.is_some() {
        return Err(Usage("S7 local_tsap and remote_tsap must be supplied together".into()).into());
    }
    if config.signals.is_empty() || config.signals.len() > 256 {
        return Err(Usage("S7 requires 1–256 signals".into()).into());
    }
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "S7 signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("S7 signal names must be unique".into()).into());
        }
        let width = signal.width()?;
        let text = signal.header_bytes() != 0;
        if (!text && (signal.capacity.is_some() || signal.encoding.is_some()))
            || (matches!(signal.data_type, DataType::Wstring) && signal.encoding.is_some())
        {
            return Err(Usage(
                "S7 capacity only applies to text; encoding only applies to STRING".into(),
            )
            .into());
        }
        if text && !matches!(signal.area, Area::Db) {
            return Err(Usage("S7 text signals require an absolute DB layout".into()).into());
        }
        if signal.area.word_indexed() {
            if !matches!(signal.data_type, DataType::U16)
                && !matches!(
                    (signal.area, signal.data_type),
                    (Area::Timer, DataType::S5time)
                )
            {
                return Err(
                    Usage("S7 timer/counter areas require u16 or timer s5time".into()).into(),
                );
            }
            if signal
                .address
                .checked_add(u32::from(signal.elements) - 1)
                .is_none_or(|last| last > 0xff_ffff)
            {
                return Err(Usage(
                    "S7 timer/counter range exceeds the 24-bit element-address range".into(),
                )
                .into());
            }
        } else if signal
            .address
            .checked_add(u32::try_from(width)?)
            .is_none_or(|end| end > 0x20_0000)
        {
            return Err(
                Usage("S7 signal extends beyond the 24-bit bit-address range".into()).into(),
            );
        }
        if (!matches!(signal.area, Area::Db) && signal.db != 0)
            || (matches!(signal.area, Area::Db) && signal.db == 0)
        {
            return Err(Usage(
                "S7 DB signals need db 1–65535; other areas require db 0 or omitted".into(),
            )
            .into());
        }
        if signal.bit > 7 || (signal.bit != 0 && !matches!(signal.data_type, DataType::Bool)) {
            return Err(
                Usage("S7 bit must be 0–7 and is only valid for bool signals".into()).into(),
            );
        }
        let exact = signal.data_type.temporal()
            || matches!(
                signal.data_type,
                DataType::Bool
                    | DataType::U64
                    | DataType::I64
                    | DataType::S5time
                    | DataType::String
                    | DataType::Wstring
            );
        if !signal.scale.is_finite()
            || !signal.offset.is_finite()
            || (exact && (signal.scale != 1.0 || signal.offset != 0.0))
        {
            return Err(Usage(
                "S7 scale/offset must be finite and cannot scale bool, 64-bit integers, s5time, temporal values or text"
                    .into(),
            )
            .into());
        }
    }
    Ok(Box::new(Client {
        config: config.clone(),
        connection: None,
    }))
}

impl Signal {
    fn parameters(&self, address: u32, width: u16) -> Vec<u8> {
        // C/T areas require element addressing with a matching word length.
        // Other areas read bytes, including for BOOL and S5TIME storage.
        let (transport, count, wire_address) = if self.area.word_indexed() {
            (self.area.code(), width / 2, address)
        } else {
            (2, width, address * 8)
        };
        let mut parameters = vec![4, 1, 0x12, 0x0a, 0x10, transport];
        parameters.extend_from_slice(&count.to_be_bytes());
        parameters.extend_from_slice(&self.db.to_be_bytes());
        parameters.push(self.area.code());
        parameters.extend_from_slice(&wire_address.to_be_bytes()[1..]);
        parameters
    }
    const fn header_bytes(&self) -> usize {
        match self.data_type {
            DataType::String => 2,
            DataType::Wstring => 4,
            _ => 0,
        }
    }
    fn width(&self) -> Result<usize> {
        if self.elements == 0 {
            return Err(Usage("S7 elements must be from 1 to 65535".into()).into());
        }
        let (maximum, unit) = match self.data_type {
            DataType::String => (254, 1),
            DataType::Wstring => (65_534, 2),
            _ => {
                let width = if matches!(self.data_type, DataType::Bool) {
                    (usize::from(self.bit) + usize::from(self.elements)).div_ceil(8)
                } else {
                    usize::from(self.data_type.width()) * usize::from(self.elements)
                };
                if width > MAX_VALUE {
                    return Err(Usage("S7 array exceeds the 131072-byte value bound".into()).into());
                }
                return Ok(width);
            }
        };
        if self.elements != 1 {
            return Err(Usage("S7 STRING/WSTRING signals require elements 1".into()).into());
        }
        let capacity = self.capacity.filter(|capacity| *capacity <= maximum)
            .ok_or_else(|| Usage(
                "S7 text needs capacity 0–254 bytes for STRING or 0–65534 WCHAR units for WSTRING".into()
            ))?;
        Ok(self.header_bytes() + usize::from(capacity) * unit)
    }
    fn text_header(&self, data: &[u8]) -> Result<usize> {
        // Siemens STRING/WSTRING layout: declared maximum, then active length.
        let (maximum, current) = match self.data_type {
            DataType::String => {
                let header = data
                    .first_chunk::<2>()
                    .ok_or_else(|| protocol("truncated STRING header"))?;
                (u16::from(header[0]), u16::from(header[1]))
            }
            DataType::Wstring => {
                let header = data
                    .first_chunk::<4>()
                    .ok_or_else(|| protocol("truncated WSTRING header"))?;
                (
                    u16::from_be_bytes([header[0], header[1]]),
                    u16::from_be_bytes([header[2], header[3]]),
                )
            }
            _ => return Err(protocol("text header requested for a numeric signal")),
        };
        if Some(maximum) != self.capacity || current > maximum {
            return Err(protocol(
                "text header maximum/current length differs from configured capacity",
            ));
        }
        Ok(usize::from(current))
    }
    fn text_value(&self, data: &[u8]) -> Result<Value> {
        let current = self.text_header(data)?;
        let wide = matches!(self.data_type, DataType::Wstring);
        let active_end = self.header_bytes() + current * if wide { 2 } else { 1 };
        let raw = data
            .get(..active_end)
            .ok_or_else(|| protocol("truncated active text bytes"))?;
        let payload = raw
            .get(self.header_bytes()..)
            .ok_or_else(|| protocol("text header missing"))?;
        let (encoding, text) = if wide {
            let units = payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_be_bytes(*unit))
                .collect::<Vec<_>>();
            (Some("utf16be"), String::from_utf16(&units).ok())
        } else {
            self.encoding.map_or((None, None), |encoding| {
                (Some(encoding.name()), encoding.decode(payload))
            })
        };
        let status = if encoding.is_none() {
            "raw_only"
        } else if text.is_some() {
            "decoded"
        } else {
            "invalid_encoding"
        };
        let mut value = json!({"kind":"s7_text","data_type":if wide { "wstring" } else { "string" },
            "capacity":self.capacity,"length":current,"storage_bytes":data.len(),
            "decode_status":status,
            "raw":{"encoding":"base64","scope":"header_and_active","bytes":raw.len(),"data":STANDARD.encode(raw)}});
        if let Some(encoding) = encoding {
            value["encoding"] = json!(encoding);
        }
        if let Some(text) = text {
            value["text"] = json!(text);
        }
        Ok(value)
    }
    fn array_budget(&self, size: SizeLimit) -> Result<SizeLimit> {
        let mut budget = size;
        let empty = Reading {
            signal: self.name.clone(),
            value: json!([]),
            source_time: None,
        };
        serde_json::to_writer(&mut budget, &empty)
            .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
        budget
            .write_all(b",")
            .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
        // The two empty-array brackets stay charged; values and separators
        // below replace its empty interior without allocating serialized JSON.
        Ok(budget)
    }
    fn preflight_array(&self, size: SizeLimit) -> Result<()> {
        if self.elements == 1 {
            return Ok(());
        }
        let mut budget = self.array_budget(size)?;
        let minimum = if matches!(self.data_type, DataType::Bool) {
            4
        } else if self.data_type.temporal() {
            temporal::MIN_JSON_BYTES
        } else {
            1
        };
        let count = usize::from(self.elements);
        budget
            .charge(count * minimum + count - 1)
            .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))
    }
    fn value(&self, data: &[u8], size: SizeLimit, deadline: Instant) -> Result<Value> {
        if data.len() != self.width()? {
            return Err(protocol("sample width differs from configured data type"));
        }
        if self.elements == 1 {
            return self.decode(data);
        }
        let mut budget = self.array_budget(size)?;
        let mut values = Vec::new();
        let width = usize::from(self.data_type.width());
        for index in 0..usize::from(self.elements) {
            remaining(deadline)?;
            let value = if matches!(self.data_type, DataType::Bool) {
                let bit = usize::from(self.bit) + index;
                let byte = data
                    .get(bit / 8)
                    .ok_or_else(|| protocol("packed BOOL array exceeds returned bytes"))?;
                json!(byte & (1 << (bit % 8)) != 0)
            } else {
                let start = index * width;
                let bytes = data
                    .get(start..start + width)
                    .ok_or_else(|| protocol("array element exceeds returned bytes"))?;
                self.decode(bytes)?
            };
            if index != 0 {
                budget
                    .write_all(b",")
                    .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            }
            serde_json::to_writer(&mut budget, &value)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn decode(&self, data: &[u8]) -> Result<Value> {
        let width = if self.header_bytes() == 0 {
            usize::from(self.data_type.width())
        } else {
            self.width()?
        };
        if data.len() != width {
            return Err(protocol("sample width differs from configured data type"));
        }
        let value = match self.data_type {
            DataType::String | DataType::Wstring => return self.text_value(data),
            DataType::Bool => return Ok(json!(data[0] & (1 << self.bit) != 0)),
            DataType::U8 => json!(data[0]),
            DataType::I8 => json!(i8::from_be_bytes([data[0]])),
            DataType::U16 => json!(u16::from_be_bytes(data.try_into()?)),
            DataType::I16 => json!(i16::from_be_bytes(data.try_into()?)),
            DataType::U32 => json!(u32::from_be_bytes(data.try_into()?)),
            DataType::I32 => json!(i32::from_be_bytes(data.try_into()?)),
            DataType::U64 => return Ok(json!(u64::from_be_bytes(data.try_into()?))),
            DataType::I64 => return Ok(json!(i64::from_be_bytes(data.try_into()?))),
            DataType::S5time => return Ok(json!(s5time_ms(u16::from_be_bytes(data.try_into()?))?)),
            DataType::Date
            | DataType::Time
            | DataType::Tod
            | DataType::DateAndTime
            | DataType::Dtl
            | DataType::Ltime
            | DataType::Ltod
            | DataType::Ldt => {
                return temporal::decode(self.data_type, data);
            }
            DataType::F32 => {
                let value = f32::from_be_bytes(data.try_into()?);
                if !value.is_finite() {
                    return Err(protocol("non-finite floating-point sample"));
                }
                json!(value)
            }
            DataType::F64 => {
                let value = f64::from_be_bytes(data.try_into()?);
                if !value.is_finite() {
                    return Err(protocol("non-finite floating-point sample"));
                }
                json!(value)
            }
        };
        if self.scale == 1.0 && self.offset == 0.0 {
            return Ok(value);
        }
        let scaled = value
            .as_f64()
            .ok_or_else(|| protocol("numeric sample is invalid"))?
            * self.scale
            + self.offset;
        if !scaled.is_finite() {
            return Err(protocol("non-finite scaled result"));
        }
        Ok(json!(scaled))
    }
}

fn s5time_ms(word: u16) -> Result<u32> {
    let digits = [(word >> 8) & 0xf, (word >> 4) & 0xf, word & 0xf];
    if digits.into_iter().any(|digit| digit > 9) {
        return Err(protocol("invalid S5TIME BCD digit"));
    }
    let [hundreds, tens, ones] = digits;
    let base = [10, 100, 1000, 10_000][usize::from((word >> 12) & 3)];
    Ok(u32::from(hundreds * 100 + tens * 10 + ones) * base)
}

struct Client {
    config: Config,
    connection: Option<Connection>,
}
struct Connection {
    stream: TcpStream,
    reference: u16,
    pdu: usize,
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("S7 timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.connection = None;
        }
        result
    }
}
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.connection.is_none() {
            let stream = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            stream.set_nodelay(true).map_err(|error| io_error(&error))?;
            let mut connection = Connection {
                stream,
                reference: 0,
                pdu: MAX_PDU,
            };
            let local = self.config.local_tsap.unwrap_or(0x0100);
            let remote = self.config.remote_tsap.unwrap_or_else(|| {
                0x0100 + u16::from(self.config.rack) * 32 + u16::from(self.config.slot)
            });
            let mut cotp = vec![17, 0xe0, 0, 0];
            cotp.extend_from_slice(&SOURCE_REFERENCE.to_be_bytes());
            cotp.extend_from_slice(&[0, 0xc0, 1, 10, 0xc1, 2]);
            cotp.extend_from_slice(&local.to_be_bytes());
            cotp.extend_from_slice(&[0xc2, 2]);
            cotp.extend_from_slice(&remote.to_be_bytes());
            connection.send_tpkt(&cotp, deadline)?;
            validate_confirm(&connection.tpkt(deadline)?)?;
            let (parameters, data) =
                connection.exchange(&[0xf0, 0, 0, 1, 0, 1, 3, 0xc0], deadline)?;
            if parameters.len() != 8
                || parameters[..2] != [0xf0, 0]
                || !data.is_empty()
                || parameters[2..4] == [0, 0]
                || parameters[4..6] == [0, 0]
            {
                return Err(protocol("invalid setup communication response"));
            }
            let pdu = usize::from(u16::from_be_bytes([parameters[6], parameters[7]]));
            if !(240..=MAX_PDU).contains(&pdu) {
                return Err(protocol("invalid negotiated S7 PDU length"));
            }
            connection.pdu = pdu;
            self.connection = Some(connection);
        }
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for signal in &self.config.signals {
            remaining(deadline)?;
            let width = signal.width()?;
            signal.preflight_array(size)?;
            let bytes = connection.read_bytes(signal, width, deadline)?;
            let header = signal.header_bytes();
            if header != 0 {
                signal.text_header(&bytes)?;
                if width > connection.read_payload()? {
                    let final_header = connection.read_bytes(signal, header, deadline)?;
                    if bytes.get(..header) != Some(final_header.as_slice()) {
                        return Err(protocol("text header changed during segmented read"));
                    }
                }
            }
            let reading = Reading {
                signal: signal.name.clone(),
                value: signal.value(&bytes, size, deadline)?,
                source_time: None,
            };
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            size.write_all(b",")
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            remaining(deadline)?;
            readings.push(reading);
        }
        Ok(readings)
    }
}

impl Connection {
    fn read_payload(&self) -> Result<usize> {
        // The negotiated length includes the 12-byte ACK_DATA header, two
        // ReadVar parameter bytes and the four-byte single-item data header.
        self.pdu
            .checked_sub(18)
            .filter(|width| *width != 0)
            .ok_or_else(|| protocol("negotiated S7 PDU has no read payload"))
    }
    fn read_bytes(&mut self, signal: &Signal, width: usize, deadline: Instant) -> Result<Vec<u8>> {
        remaining(deadline)?;
        if !(1..=MAX_VALUE).contains(&width) {
            return Err(protocol("read range exceeds bounded S7 value size"));
        }
        let address_step = if signal.area.word_indexed() { 2 } else { 1 };
        // Indexed timer/counter payloads contain complete two-byte elements,
        // even when the negotiated PDU leaves an odd number of payload bytes.
        let maximum = self.read_payload()? / address_step * address_step;
        let mut bytes = Vec::with_capacity(width);
        while bytes.len() < width {
            remaining(deadline)?;
            let count = (width - bytes.len()).min(maximum);
            let address = signal
                .address
                .checked_add(u32::try_from(bytes.len() / address_step)?)
                .ok_or_else(|| protocol("read address overflow"))?;
            let (parameters, data) =
                self.exchange(&signal.parameters(address, u16::try_from(count)?), deadline)?;
            if parameters != [4, 1] || data.len() < 4 {
                return Err(protocol("invalid Read Var response"));
            }
            if data[0] != 0xff {
                return Err(Categorized::new(
                    ErrorKind::OperationFailed,
                    "S7 PLC rejected a Read Var item",
                )
                .details(json!({"signal":signal.name, "return_code":data[0]}))
                .into());
            }
            let length = usize::from(u16::from_be_bytes([data[2], data[3]]));
            if !matches!((data[1], length), (4, n) if n == count * 8)
                && !matches!((data[1], length), (9, n) if n == count)
            {
                return Err(protocol(
                    "Read Var transport or data length differs from request",
                ));
            }
            let payload = data
                .get(4..)
                .ok_or_else(|| protocol("Read Var item header missing"))?;
            if payload.len() != count {
                return Err(protocol("Read Var sample width differs from request"));
            }
            bytes.extend_from_slice(payload);
            remaining(deadline)?;
        }
        Ok(bytes)
    }
    fn send_tpkt(&mut self, payload: &[u8], deadline: Instant) -> Result<()> {
        let size = u16::try_from(payload.len() + 4)?;
        let mut packet = vec![3, 0];
        packet.extend_from_slice(&size.to_be_bytes());
        packet.extend_from_slice(payload);
        write(&mut self.stream, &packet, deadline)
    }
    fn tpkt(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let mut header = [0; 4];
        read(&mut self.stream, &mut header, deadline)?;
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        if header[..2] != [3, 0] || !(7..=MAX_TPKT).contains(&length) {
            return Err(protocol("invalid or oversized RFC 1006 packet"));
        }
        let mut payload = vec![0; length - 4];
        read(&mut self.stream, &mut payload, deadline)?;
        Ok(payload)
    }
    fn exchange(&mut self, parameters: &[u8], deadline: Instant) -> Result<(Vec<u8>, Vec<u8>)> {
        self.reference = self.reference.wrapping_add(1);
        if self.reference == 0 {
            self.reference = 1;
        }
        let mut request = vec![2, 0xf0, 0x80, 0x32, 1, 0, 0];
        request.extend_from_slice(&self.reference.to_be_bytes());
        request.extend_from_slice(&u16::try_from(parameters.len())?.to_be_bytes());
        request.extend_from_slice(&[0, 0]);
        request.extend_from_slice(parameters);
        if request.len() - 3 > self.pdu {
            return Err(protocol("request exceeds negotiated S7 PDU length"));
        }
        self.send_tpkt(&request, deadline)?;
        let mut response = Vec::new();
        for _ in 0..16 {
            let fragment = self.tpkt(deadline)?;
            if fragment[..2] != [2, 0xf0] || !matches!(fragment[2], 0 | 0x80) {
                return Err(protocol("invalid COTP data TPDU"));
            }
            response.extend_from_slice(&fragment[3..]);
            if response.len() > self.pdu {
                return Err(protocol("response exceeds negotiated S7 PDU length"));
            }
            if fragment[2] == 0x80 {
                return response_parts(&response, self.reference);
            }
            if fragment.len() == 3 {
                return Err(protocol("empty nonfinal COTP fragment"));
            }
        }
        Err(protocol("too many COTP fragments"))
    }
}

fn validate_confirm(payload: &[u8]) -> Result<()> {
    let Some(header) = payload.first_chunk::<7>() else {
        return Err(protocol("truncated COTP connection confirmation"));
    };
    if usize::from(header[0]) + 1 != payload.len()
        || header[1] != 0xd0
        || header[2..4] != SOURCE_REFERENCE.to_be_bytes()
        || header[6] != 0
    {
        return Err(protocol("invalid COTP connection confirmation"));
    }
    let mut at = 7;
    let mut seen = BTreeSet::new();
    while at < payload.len() {
        if payload.len() - at < 2 {
            return Err(protocol("truncated COTP parameter"));
        }
        let code = payload[at];
        let length = usize::from(payload[at + 1]);
        at += 2;
        if length > payload.len() - at || !seen.insert(code) {
            return Err(protocol("invalid COTP parameter length or duplicate"));
        }
        match (code, length) {
            (0xc0, 1) if (7..=10).contains(&payload[at]) => {}
            (0xc1 | 0xc2, 2) => {}
            (0xc0..=0xc2, _) => return Err(protocol("invalid COTP connection parameter")),
            _ => {}
        }
        at += length;
    }
    Ok(())
}
fn response_parts(response: &[u8], reference: u16) -> Result<(Vec<u8>, Vec<u8>)> {
    let Some(header) = response.first_chunk::<12>() else {
        return Err(protocol("truncated S7 response header"));
    };
    if header[0] != 0x32
        || !matches!(header[1], 2 | 3)
        || header[2..4] != [0, 0]
        || header[4..6] != reference.to_be_bytes()
    {
        return Err(protocol("invalid S7 response header or PDU reference"));
    }
    let parameters = usize::from(u16::from_be_bytes([header[6], header[7]]));
    let data = usize::from(u16::from_be_bytes([header[8], header[9]]));
    if response.len() != 12 + parameters + data {
        return Err(protocol("S7 header lengths do not match payload"));
    }
    if header[10..12] != [0, 0] {
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            "S7 PLC returned a service error",
        )
        .details(json!({"error_class":header[10],"error_code":header[11]}))
        .into());
    }
    if header[1] != 3 {
        return Err(protocol("S7 response contains no acknowledged data"));
    }
    Ok((
        response[12..12 + parameters].to_vec(),
        response[12 + parameters..].to_vec(),
    ))
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("S7: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "S7 poll deadline expired",
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
    Categorized::error(kind, format!("S7 I/O: {error}"))
}
fn read(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        match stream.read(bytes) {
            Ok(0) => {
                return Err(protocol(
                    "PLC closed the connection before the response completed",
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
            Ok(0) => return Err(protocol("PLC closed the connection while sending")),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct SizeLimit {
    bytes: usize,
}
impl SizeLimit {
    fn charge(&mut self, count: usize) -> io::Result<()> {
        self.bytes = self.bytes.saturating_add(count);
        if self.bytes > MAX_BATCH {
            return Err(io::Error::other("S7 batch exceeds size limit"));
        }
        Ok(())
    }
}
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.charge(bytes.len())?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signal(data_type: &str) -> Signal {
        serde_json::from_value(json!({"name":"value","db":1,"address":0,"data_type":data_type}))
            .unwrap()
    }
    #[test]
    fn decoding_preserves_exact_integers_and_checks_floats_and_scaling() {
        assert_eq!(
            signal("u64").decode(&u64::MAX.to_be_bytes()).unwrap(),
            json!(u64::MAX)
        );
        assert_eq!(
            signal("i64").decode(&i64::MIN.to_be_bytes()).unwrap(),
            json!(i64::MIN)
        );
        assert_eq!(
            signal("i16").decode(&(-123_i16).to_be_bytes()).unwrap(),
            json!(-123)
        );
        assert_eq!(
            signal("f64").decode(&12.5_f64.to_be_bytes()).unwrap(),
            json!(12.5)
        );
        signal("f32").decode(&f32::NAN.to_be_bytes()).unwrap_err();
        signal("f64")
            .decode(&f64::INFINITY.to_be_bytes())
            .unwrap_err();
        let mut scaled = signal("u16");
        scaled.scale = 0.01;
        scaled.offset = -1.0;
        assert_eq!(scaled.decode(&250_u16.to_be_bytes()).unwrap(), json!(1.5));
        let mut bit = signal("bool");
        bit.bit = 7;
        assert_eq!(bit.decode(&[0x80]).unwrap(), json!(true));
        assert_eq!(bit.decode(&[0x7f]).unwrap(), json!(false));
    }
    #[test]
    fn read_parameters_are_byte_addressed_and_never_a_write() {
        let config: Config = serde_json::from_str(
            r#"{"address":"127.0.0.1:102","rack":0.0,"slot":2e0,"local_tsap":2.56e2,"remote_tsap":2.58e2,"signals":[{"name":"value","db":6.5535e4,"address":1.193046e6,"bit":-0.0,"data_type":"u64"}]}"#,
        )
        .unwrap();
        open(&config).unwrap();
        let [value] = config.signals.as_slice() else {
            panic!("one configured signal expected");
        };
        assert_eq!(
            value.parameters(value.address, u16::from(value.data_type.width())),
            [
                4, 1, 0x12, 10, 0x10, 2, 0, 8, 0xff, 0xff, 0x84, 0x91, 0xa2, 0xb0
            ]
        );
    }
    #[test]
    fn array_limits_and_integral_count_notation_are_checked_without_connecting() {
        let config: Config = serde_json::from_str(
            r#"{"address":"127.0.0.1:102","signals":[{"name":"array","db":1,"address":0,"data_type":"u64","elements":1.6384e4}]}"#,
        ).unwrap();
        drop(open(&config).unwrap());
        let [array] = config.signals.as_slice() else {
            panic!("one configured array expected");
        };
        assert_eq!(array.width().unwrap(), MAX_VALUE);
        array.preflight_array(SizeLimit { bytes: 2 }).unwrap();
        let mut changed = array.clone();
        changed.elements += 1;
        changed.width().unwrap_err();
        changed.elements = 0;
        changed.width().unwrap_err();
        let mut bit = signal("bool");
        bit.elements = u16::MAX;
        bit.bit = 7;
        assert_eq!(bit.width().unwrap(), 8193);
        bit.preflight_array(SizeLimit {
            bytes: MAX_BATCH - 1024,
        })
        .unwrap_err();
        let mut text = signal("string");
        text.capacity = Some(1);
        text.elements = 2;
        text.width().unwrap_err();
    }
    #[test]
    fn malformed_headers_cotp_and_mismatched_references_are_rejected() {
        for payload in [
            &[][..],
            &[6, 0xd0, 0, 2, 0, 1, 0],
            &[7, 0xd0, 0, 1, 0, 1, 0],
            &[9, 0xd0, 0, 1, 0, 1, 0, 0xc0, 1, 11],
        ] {
            validate_confirm(payload).unwrap_err();
        }
        validate_confirm(&[6, 0xd0, 0, 1, 0, 1, 0]).unwrap();
        let response = [0x32, 3, 0, 0, 0, 1, 0, 2, 0, 0, 0, 0, 4, 1];
        assert_eq!(response_parts(&response, 1).unwrap(), (vec![4, 1], vec![]));
        response_parts(&response, 2).unwrap_err();
        response_parts(&response[..13], 1).unwrap_err();
    }
    #[test]
    fn validates_tsaps_ranges_bits_and_exact_scaling_before_connecting() {
        let base =
            json!({"address":"127.0.0.1:102","signals":[{"name":"value","db":1,"address":0}]});
        let config: Config = serde_json::from_value(base.clone()).unwrap();
        open(&config).unwrap();
        for (field, value) in [
            ("rack", json!(8)),
            ("slot", json!(32)),
            ("local_tsap", json!(256)),
        ] {
            let mut changed = base.clone();
            changed[field] = value;
            let config: Config = serde_json::from_value(changed).unwrap();
            drop(open(&config).err().unwrap());
        }
        for patch in [
            json!({"db":0}),
            json!({"area":"input","db":1}),
            json!({"address":0x1f_ffff}),
            json!({"bit":1}),
            json!({"data_type":"u64","scale":0.1}),
            json!({"data_type":"s5time","scale":0.1}),
            json!({"area":"counter","db":0,"data_type":"s5time"}),
            json!({"area":"timer","db":0,"data_type":"bool"}),
            json!({"area":"timer","db":0,"address":0x100_0000}),
        ] {
            let mut changed = base.clone();
            for (key, value) in patch.as_object().unwrap() {
                changed["signals"][0][key] = value.clone();
            }
            let config: Config = serde_json::from_value(changed).unwrap();
            drop(open(&config).err().unwrap());
        }
    }
}
