//! Read-only SLC/`MicroLogix` and PLC-5 PCCC files through EtherNet/IP `Execute_PCCC`.
//! Rockwell 1770-RM516 section 7-17 defines command 0F/A2 and logical addresses:
//! <https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1770-rm516_-en-p.pdf>.
//! 22COMM-UM004 appendix C defines service 4B, class 67, requestor ID and TNS:
//! <https://literature.rockwellautomation.com/idc/groups/literature/documents/um/22comm-um004_-en-p.pdf>.
//! 1756-PM020 documents `MicroLogix` Long file type 91 (where the CPU supports it):
//! <https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf>.
//! The DF1 child module shares this typed command codec for direct serial links.
//! The PLC-5/E profile uses command 0F/68 and validates its reply descriptors.
//! The CSP child module reads SLC files over direct legacy TCP.
//! DH+ routing is outside these transport profiles.
pub(crate) mod csp;
pub(crate) mod df1;
mod plc5;
mod st;
mod tc;
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

const REGISTER: u16 = 0x65;
const UNREGISTER: u16 = 0x66;
const SEND_RR: u16 = 0x6f;
const MAX_PACKET: usize = 4096;
const MAX_BATCH: usize = 1 << 20;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    #[serde(default)]
    pub profile: Profile,
    /// One optional port-1 backplane hop; no DH+ bridge commands are sent.
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub slot: Option<u8>,
    /// Unspecified software requestor by default; does not impersonate a vendor.
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub vendor_id: u16,
    /// Omit for a random nonzero ID for this capture source.
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::optional_unsigned")]
    pub serial_number: Option<u32>,
    #[serde(default = "max_read_bytes")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub max_read_bytes: u16,
    pub signals: Vec<Signal>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Profile {
    #[default]
    Slc,
    Plc5,
}

const fn max_read_bytes() -> u16 {
    220
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
    /// SLC logical file syntax: N7:0, B3:0, F8:0; L19:0 where supported.
    /// Whole T4:0/C5:0 elements preserve raw control/PRE/ACC words.
    /// Whole ST3:0 string elements preserve all 84 native bytes (CPU-dependent files).
    /// Scalar /bit selects 0–15 in N/B words or 0–31 in L values.
    pub address: String,
    #[serde(default = "one")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub elements: u16,
    #[serde(default)]
    pub data_type: Option<DataType>,
    /// Explicit interpretation of valid active ST bytes; absent keeps bytes only.
    #[serde(default)]
    pub encoding: Option<Encoding>,
    #[serde(default = "scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataType {
    I16,
    U16,
    I32,
    U32,
    F32,
    Bool,
    Timer,
    Counter,
    String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Encoding {
    Ascii,
    Utf8,
}
impl Encoding {
    const fn name(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::Utf8 => "utf8",
        }
    }
}
#[derive(Debug, Clone, Copy)]
struct Address {
    file_type: u8,
    file: u16,
    element: u16,
    bit: Option<u8>,
    kind: DataType,
}
impl Address {
    const fn structured(self) -> bool {
        matches!(
            self.kind,
            DataType::Timer | DataType::Counter | DataType::String
        )
    }
    const fn width(self) -> usize {
        match self.kind {
            DataType::String => 84,
            DataType::Timer | DataType::Counter => 6,
            _ if matches!(self.file_type, 0x8a | 0x91) => 4,
            _ => 2,
        }
    }
}
fn invalid_address() -> anyhow::Error {
    Usage("PCCC address must be an SLC/MicroLogix N/B/F/L file (e.g. N7:0, B3:0/5, F8:0, L19:0), or a whole T/C element (T4:0, C5:0; matching file4/5 or9–255, elements0–255), or whole ST3:0 (file3–255, elements0–255; CPU-dependent allocation), with decimal file/element 0–65535 for numeric files; scalar bit selection applies to N/B words (0–15) or L values (0–31)".into()).into()
}
fn address(signal: &Signal) -> Result<Address> {
    let bytes = signal.address.as_bytes();
    if !(4..=32).contains(&bytes.len()) || !bytes.is_ascii() {
        return Err(invalid_address());
    }
    let (prefix, file_type, default): (usize, u8, DataType) = if bytes
        .get(..2)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"ST"))
    {
        (2, 0x8d, DataType::String)
    } else {
        let (file_type, default) = match bytes[0].to_ascii_uppercase() {
            b'N' => (0x89, DataType::I16),
            b'B' => (0x85, DataType::U16),
            b'F' => (0x8a, DataType::F32),
            b'L' => (0x91, DataType::I32),
            b'T' => (0x86, DataType::Timer),
            b'C' => (0x87, DataType::Counter),
            _ => return Err(invalid_address()),
        };
        (1, file_type, default)
    };
    let (file, remainder) = signal
        .address
        .get(prefix..)
        .ok_or_else(invalid_address)?
        .split_once(':')
        .ok_or_else(invalid_address)?;
    let (element, bit) = remainder
        .split_once('/')
        .map_or((remainder, None), |(element, bit)| (element, Some(bit)));
    let number = |text: &str| -> Result<u16> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid_address());
        }
        text.parse().map_err(|_| invalid_address())
    };
    let file = number(file)?;
    let element = number(element)?;
    let bit = bit.map(number).transpose()?;
    let kind = signal.data_type.unwrap_or_else(|| {
        if bit.is_some() {
            DataType::Bool
        } else {
            default
        }
    });
    if (match file_type {
        0x86 | 0x87 | 0x8d => kind != default || bit.is_some(),
        0x8a => kind != DataType::F32 || bit.is_some(),
        0x91 => !matches!(kind, DataType::I32 | DataType::U32 | DataType::Bool),
        _ => !matches!(kind, DataType::I16 | DataType::U16 | DataType::Bool),
    }) || bit.is_some_and(|bit| bit > if file_type == 0x91 { 31 } else { 15 })
        || (bit.is_some() && kind != DataType::Bool)
        || (kind == DataType::Bool && signal.elements != 1)
    {
        return Err(invalid_address());
    }
    if !(1..=4096).contains(&signal.elements)
        || u32::from(element) + u32::from(signal.elements) > 65_536
    {
        return Err(Usage(
            "PCCC elements must be 1–4096 and fit the logical file address space".into(),
        )
        .into());
    }
    if matches!(file_type, 0x86 | 0x87)
        && (!((file_type == 0x86 && file == 4)
            || (file_type == 0x87 && file == 5)
            || (9..=255).contains(&file))
            || u32::from(element) + u32::from(signal.elements) > 256)
    {
        return Err(Usage(
            "PCCC T/C requires T4/C5 or files 9–255 and a whole-element range within 0–255".into(),
        )
        .into());
    }
    if file_type == 0x8d
        && (!(3..=255).contains(&file) || u32::from(element) + u32::from(signal.elements) > 256)
    {
        return Err(Usage("PCCC ST requires files 3–255 and a whole-element range within 0–255; the CPU must support and allocate the requested string file".into()).into());
    }
    Ok(Address {
        file_type,
        file,
        element,
        bit: bit.map(u8::try_from).transpose()?,
        kind,
    })
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 || config.signals.is_empty() || config.signals.len() > 256 {
        return Err(Usage("PCCC requires a nonzero TCP port and 1–256 signals".into()).into());
    }
    if !(4..=224).contains(&config.max_read_bytes) || config.serial_number == Some(0) {
        return Err(Usage(
            "PCCC max_read_bytes must be 4–224; serial_number must be nonzero when supplied".into(),
        )
        .into());
    }
    let addresses = signal_addresses(&config.signals)?;
    validate_read_width(&addresses, config.max_read_bytes)?;
    if config.profile == Profile::Plc5 {
        if config.slot.is_some() {
            return Err(Usage("PCCC plc5 profile requires a direct Ethernet target".into()).into());
        }
        plc5::validate(&config.signals, &addresses)?;
    }
    let serial = if let Some(serial) = config.serial_number {
        serial
    } else {
        let mut random = [0; 4];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow::anyhow!("PCCC requestor ID: {error}"))?;
        u32::from_le_bytes(random).max(1)
    };
    let mut identity = [7, 0, 0, 0, 0, 0, 0];
    identity[1..3].copy_from_slice(&config.vendor_id.to_le_bytes());
    identity[3..].copy_from_slice(&serial.to_le_bytes());
    Ok(Box::new(Client {
        config: config.clone(),
        addresses,
        identity,
        connection: None,
        context: 0,
        transaction: 0,
        retired: false,
    }))
}
fn signal_addresses(signals: &[Signal]) -> Result<Vec<Address>> {
    let mut names = BTreeSet::new();
    let mut addresses = Vec::with_capacity(signals.len());
    for signal in signals {
        super::validate_name(&signal.name, "PCCC signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("PCCC signal names must be unique".into()).into());
        }
        let parsed = address(signal)?;
        if !signal.scale.is_finite()
            || !signal.offset.is_finite()
            || ((parsed.kind == DataType::Bool || parsed.structured())
                && (signal.scale != 1.0 || signal.offset != 0.0))
        {
            return Err(Usage(
                "PCCC scaling must be finite; boolean and T/C/ST values cannot be scaled".into(),
            )
            .into());
        }
        if signal.encoding.is_some() && parsed.kind != DataType::String {
            return Err(
                Usage("PCCC encoding applies only to whole ST string values".into()).into(),
            );
        }
        addresses.push(parsed);
    }
    Ok(addresses)
}
fn validate_read_width(addresses: &[Address], max_read_bytes: u16) -> Result<()> {
    if max_read_bytes < 84
        && addresses
            .iter()
            .any(|address| address.kind == DataType::String)
    {
        return Err(Usage(
            "PCCC ST max_read_bytes must be at least 84 for one whole native string element".into(),
        )
        .into());
    }
    if max_read_bytes < 6
        && addresses
            .iter()
            .any(|address| matches!(address.kind, DataType::Timer | DataType::Counter))
    {
        return Err(Usage(
            "PCCC T/C max_read_bytes must be at least 6 for one whole element".into(),
        )
        .into());
    }
    Ok(())
}
struct Client {
    config: Config,
    addresses: Vec<Address>,
    identity: [u8; 7],
    connection: Option<Connection>,
    context: u64,
    transaction: u16,
    retired: bool,
}
struct Connection {
    stream: TcpStream,
    session: u32,
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "PCCC source retired after a failed poll",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("PCCC timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.connection = None;
            self.retired = true;
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
            let mut connection = Connection { stream, session: 0 };
            // Use conventional zero registration context; read contexts remain unique.
            let response = connection.exchange(REGISTER, &[1, 0, 0, 0], 0, deadline)?;
            if response != [1, 0, 0, 0] {
                return Err(protocol("invalid RegisterSession response"));
            }
            self.connection = Some(connection);
        }
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| protocol("session missing"))?;
        let readings = read_signals(
            &self.config.signals,
            &self.addresses,
            self.config.max_read_bytes,
            deadline,
            |address, element, bytes, data| {
                self.transaction = self.transaction.wrapping_add(1).max(1);
                let mut request = vec![0x4b, 2, 0x20, 0x67, 0x24, 1];
                request.extend_from_slice(&self.identity);
                match self.config.profile {
                    Profile::Slc => {
                        typed_request(&mut request, address, element, bytes, self.transaction)
                    }
                    Profile::Plc5 => {
                        plc5::request(&mut request, address, element, bytes, self.transaction)
                    }
                }?;
                let request = route(request, self.config.slot)?;
                let mut body = vec![0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
                body.extend_from_slice(&u16::try_from(request.len())?.to_le_bytes());
                body.extend_from_slice(&request);
                self.context = self.context.wrapping_add(1);
                let response = connection.exchange(SEND_RR, &body, self.context, deadline)?;
                let response = cip(cpf(&response)?, self.config.slot.is_some())?;
                let response = pccc(response, self.identity)?;
                let response = match self.config.profile {
                    Profile::Slc => typed_response(response, self.transaction, bytes)?,
                    Profile::Plc5 => plc5::response(response, address, self.transaction, bytes)?,
                };
                data.extend_from_slice(response);
                Ok(())
            },
        )?;
        remaining(deadline)?;
        Ok(readings)
    }
}
fn read_signals(
    signals: &[Signal],
    addresses: &[Address],
    max_read_bytes: u16,
    deadline: Instant,
    mut read_chunk: impl FnMut(Address, u16, usize, &mut Vec<u8>) -> Result<()>,
) -> Result<Vec<Reading>> {
    let mut readings = Vec::with_capacity(signals.len());
    let mut size = SizeLimit { bytes: 2 };
    for (signal, &address) in signals.iter().zip(addresses) {
        remaining(deadline)?;
        if address.kind == DataType::String {
            let value = st::read(
                signal,
                address,
                max_read_bytes,
                &mut size,
                deadline,
                &mut read_chunk,
            )?;
            remaining(deadline)?;
            readings.push(Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            });
            continue;
        }
        if address.structured() {
            tc::begin(signal, &mut size)?;
        }
        let mut data = Vec::with_capacity(usize::from(signal.elements) * address.width());
        let mut done = 0;
        while done < signal.elements {
            let points = (usize::from(max_read_bytes) / address.width())
                .min(usize::from(signal.elements - done));
            if points == 0 {
                return Err(protocol("request byte cap cannot hold one whole element"));
            }
            remaining(deadline)?;
            let bytes = points * address.width();
            let element = address
                .element
                .checked_add(done)
                .ok_or_else(|| protocol("logical element overflow"))?;
            read_chunk(address, element, bytes, &mut data)?;
            done += u16::try_from(points)?;
        }
        let reading = Reading {
            signal: signal.name.clone(),
            value: if address.structured() {
                tc::decode(signal, address, &data, &mut size, deadline)?
            } else {
                decode(signal, address, &data)?
            },
            source_time: None,
        };
        if !address.structured() {
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            size.write_all(b",")
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
        }
        remaining(deadline)?;
        readings.push(reading);
    }
    Ok(readings)
}

fn typed_request(
    request: &mut Vec<u8>,
    address: Address,
    element: u16,
    bytes: usize,
    transaction: u16,
) -> Result<()> {
    request.extend_from_slice(&[0x0f, 0]);
    request.extend_from_slice(&transaction.to_le_bytes());
    request.extend_from_slice(&[0xa2, u8::try_from(bytes)?]);
    logical(request, address.file);
    request.push(address.file_type);
    logical(request, element);
    logical(request, 0);
    Ok(())
}
fn logical(bytes: &mut Vec<u8>, value: u16) {
    if let Ok(value) = u8::try_from(value)
        && value != 0xff
    {
        bytes.push(value);
    } else {
        bytes.push(0xff);
        bytes.extend_from_slice(&value.to_le_bytes());
    }
}
fn route(request: Vec<u8>, slot: Option<u8>) -> Result<Vec<u8>> {
    let Some(slot) = slot else {
        return Ok(request);
    };
    let mut routed = vec![0x52, 2, 0x20, 6, 0x24, 1, 0x0a, 0x0e];
    routed.extend_from_slice(&u16::try_from(request.len())?.to_le_bytes());
    routed.extend_from_slice(&request);
    if !request.len().is_multiple_of(2) {
        routed.push(0);
    }
    routed.extend_from_slice(&[1, 0, 1, slot]);
    Ok(routed)
}
fn decode(signal: &Signal, address: Address, data: &[u8]) -> Result<Value> {
    if data.len() != usize::from(signal.elements) * address.width() {
        return Err(protocol(
            "typed file data size differs from requested elements",
        ));
    }
    let mut values = Vec::with_capacity(usize::from(signal.elements));
    for bytes in data.chunks_exact(address.width()) {
        let value = match address.kind {
            DataType::Bool => {
                let word = if address.width() == 4 {
                    u32::from_le_bytes(bytes.try_into()?)
                } else {
                    u32::from(u16::from_le_bytes(bytes.try_into()?))
                };
                json!(word & (1_u32 << address.bit.unwrap_or(0)) != 0)
            }
            DataType::I16 => json!(i16::from_le_bytes(bytes.try_into()?)),
            DataType::U16 => json!(u16::from_le_bytes(bytes.try_into()?)),
            DataType::I32 => json!(i32::from_le_bytes(bytes.try_into()?)),
            DataType::U32 => json!(u32::from_le_bytes(bytes.try_into()?)),
            DataType::Timer | DataType::Counter | DataType::String => {
                return Err(protocol("structured value used numeric decoder"));
            }
            DataType::F32 => {
                let value = f32::from_le_bytes(bytes.try_into()?);
                if !value.is_finite() {
                    return Err(protocol("non-finite floating-point file value"));
                }
                json!(value)
            }
        };
        let value = if signal.scale == 1.0 && signal.offset == 0.0 {
            value
        } else {
            let value = value
                .as_f64()
                .ok_or_else(|| protocol("invalid numeric file value"))?
                * signal.scale
                + signal.offset;
            if !value.is_finite() {
                return Err(protocol("non-finite scaled file value"));
            }
            json!(value)
        };
        values.push(value);
    }
    if signal.elements == 1 {
        values
            .pop()
            .ok_or_else(|| protocol("missing scalar file value"))
    } else {
        Ok(Value::Array(values))
    }
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
fn cip(bytes: &[u8], routed: bool) -> Result<&[u8]> {
    let [service, 0, status, extra, remaining @ ..] = bytes else {
        return Err(protocol("invalid CIP response header"));
    };
    if *service != 0xcb && !(routed && *service == 0xd2) {
        return Err(protocol("unexpected CIP reply service"));
    }
    let extended = usize::from(*extra) * 2;
    let data = remaining
        .get(extended..)
        .ok_or_else(|| protocol("truncated CIP extended status"))?;
    if *status != 0 {
        return Err(Categorized::new(if matches!(*status, 4 | 5) { ErrorKind::Unavailable } else if *status == 8 { ErrorKind::Unsupported } else { ErrorKind::OperationFailed }, "PCCC CIP request rejected")
            .details(json!({"cip_status":status,"extended_status":remaining[..extended].as_chunks::<2>().0.iter().map(|word|u16::from_le_bytes(*word)).collect::<Vec<_>>()})).into());
    }
    if *extra != 0 {
        return Err(protocol(
            "successful CIP response unexpectedly contains extended status",
        ));
    }
    if *service == 0xd2 {
        cip(data, false)
    } else {
        Ok(data)
    }
}
fn pccc(bytes: &[u8], identity: [u8; 7]) -> Result<&[u8]> {
    let Some(id) = bytes.first_chunk::<7>() else {
        return Err(protocol("truncated requestor identity"));
    };
    if *id != identity {
        return Err(protocol(
            "PCCC response requestor identity differs from request",
        ));
    }
    Ok(&bytes[7..])
}

fn typed_response(bytes: &[u8], transaction: u16, expected: usize) -> Result<&[u8]> {
    let data = typed_data(bytes, transaction)?;
    if data.len() != expected {
        return Err(protocol(
            "PCCC response data length differs from requested bytes",
        ));
    }
    Ok(data)
}

fn typed_data(bytes: &[u8], transaction: u16) -> Result<&[u8]> {
    let [0x4f, status, low, high, data @ ..] = bytes else {
        return Err(protocol("invalid PCCC response command or header"));
    };
    if u16::from_le_bytes([*low, *high]) != transaction {
        return Err(protocol("PCCC response transaction differs from request"));
    }
    if *status != 0 {
        let extended = if *status == 0xf0 {
            Some(
                *data
                    .first()
                    .ok_or_else(|| protocol("missing extended PCCC status"))?,
            )
        } else {
            None
        };
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            "PCCC target rejected typed file read",
        )
        .details(crate::types::without_nulls(
            json!({"pccc_status":status,"extended_status":extended}),
        ))
        .into());
    }
    Ok(data)
}
impl Connection {
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
        let Some(prefix) = response.first_chunk::<24>() else {
            return Err(protocol("truncated encapsulation header"));
        };
        let size = usize::from(u16::from_le_bytes([prefix[2], prefix[3]]));
        let session = u32::from_le_bytes(prefix[4..8].try_into()?);
        if u16::from_le_bytes([prefix[0], prefix[1]]) != command
            || prefix[12..20] != context.to_le_bytes()
            || prefix[20..24] != [0; 4]
        {
            return Err(protocol(
                "unexpected encapsulation command, context or options",
            ));
        }
        let status = u32::from_le_bytes(prefix[8..12].try_into()?);
        if status != 0 {
            return Err(Categorized::new(
                ErrorKind::Unavailable,
                "PCCC encapsulation request rejected",
            )
            .details(json!({"encapsulation_status":status}))
            .into());
        }
        if command == REGISTER {
            if self.session != 0 || session == 0 {
                return Err(protocol("invalid registered session handle"));
            }
            self.session = session;
        } else if session != self.session {
            return Err(protocol("unexpected encapsulation session handle"));
        }
        if size > MAX_PACKET {
            return Err(protocol("encapsulation response exceeds 4096 bytes"));
        }
        let mut bytes = vec![0; size];
        read(&mut self.stream, &mut bytes, deadline)?;
        Ok(bytes)
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        if self.session != 0 && self.stream.set_nonblocking(true).is_ok() {
            drop(self.stream.write(&header(UNREGISTER, self.session, 0, 0)));
        }
    }
}
fn header(command: u16, session: u32, context: u64, length: u16) -> Vec<u8> {
    let mut bytes = command.to_le_bytes().to_vec();
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&session.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&context.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("PCCC: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "PCCC poll deadline expired",
        ));
    }
    Ok(left)
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::UnexpectedEof => ErrorKind::Incomplete,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("PCCC I/O: {error}"))
}
fn read(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        match stream.read(bytes) {
            Ok(0) => return Err(protocol("TCP closed during response")),
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
            Ok(0) => return Err(protocol("TCP writer made no progress")),
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
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_BATCH {
            return Err(io::Error::other("PCCC serialized batch exceeds 1 MiB"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
