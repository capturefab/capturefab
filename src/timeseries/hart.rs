//! Read-only universal HART commands through a configured managed serial modem.
//! Microflex 101-0035 manual 620-0021 R3 pp.1–2,5,15; Fisher-Rosemount
//! HART Technical Overview, second edition, chapter3 and figures3-5/3-6.
use super::{Reading, Source, serial, slmp::SizeLimit};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    io::{self, Write as _},
    time::{Duration, Instant},
};

const MAX_SKIPPED_BYTES: usize = 65_536;
const PREAMBLES: usize = 20;
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    device: String,
    modem: Modem,
    master: Master,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
    #[serde(default = "parity")]
    parity: serial::Parity,
    signals: Vec<Signal>,
}
const fn baud() -> u32 {
    1200
}
const fn parity() -> serial::Parity {
    serial::Parity::Odd
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Modem {
    #[serde(rename = "microlink_hm_101_0035")]
    MicrolinkHm1010035,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Master {
    Primary,
    Secondary,
}
impl Master {
    const fn bit(self) -> u8 {
        match self {
            Self::Primary => 0x80,
            Self::Secondary => 0,
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    polling_address: u8,
    #[serde(default)]
    read: Query,
    #[serde(default)]
    unique_address: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Query {
    PrimaryVariable,
    LoopCurrentAndPercent,
    #[default]
    DynamicVariables,
}
impl Query {
    const fn command(self) -> u8 {
        match self {
            Self::PrimaryVariable => 1,
            Self::LoopCurrentAndPercent => 2,
            Self::DynamicVariables => 3,
        }
    }
}
fn validate(config: &Config) -> Result<()> {
    if config.device.is_empty() || !(1..=256).contains(&config.signals.len()) {
        return Err(Usage(
            "HART requires one managed-modem serial device and 1–256 signals".into(),
        )
        .into());
    }
    match config.modem {
        Modem::MicrolinkHm1010035 => {
            if !matches!(
                config.baud,
                1200 | 2400 | 4800 | 9600 | 19_200 | 38_400 | 57_600
            ) {
                return Err(Usage(
                    "MicroLink-HM host baud must be 1200, 2400, 4800, 9600, 19200, 38400 or 57600"
                        .into(),
                )
                .into());
            }
        }
    }
    serial::validate(config.baud, 1, 8)?;
    let mut names = BTreeSet::new();
    let mut identities = BTreeMap::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "HART signal")?;
        if !names.insert(&signal.name) || signal.polling_address > 63 {
            return Err(Usage(
                "HART requires unique signal names and polling addresses 0–63".into(),
            )
            .into());
        }
        if let Some(value) = &signal.unique_address {
            let uid = unique_address(value)?;
            if identities
                .insert(signal.polling_address, uid)
                .is_some_and(|old| old != uid)
            {
                return Err(Usage(
                    "HART mappings for one polling address require the same unique_address".into(),
                )
                .into());
            }
        }
    }
    Ok(())
}
fn unique_address(text: &str) -> Result<[u8; 5]> {
    let bytes = text.as_bytes();
    let groups: &[[u8; 2]] = bytes.as_chunks::<2>().0;
    if bytes.len() != 10 {
        return Err(Usage(
            "HART unique_address requires ten hexadecimal digits for the 38-bit device identity"
                .into(),
        )
        .into());
    }
    let mut uid = [0; 5];
    for (to, from) in uid.iter_mut().zip(groups) {
        *to = hex(from[0])
            .zip(hex(from[1]))
            .map(|(high, low)| (high << 4) | low)
            .ok_or_else(|| Usage("HART unique_address contains a non-hexadecimal digit".into()))?;
    }
    if uid[0] & 0xc0 != 0 {
        return Err(Usage("HART unique_address excludes master and burst bits".into()).into());
    }
    Ok(uid)
}
const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    validate(config)?;
    #[cfg(any(unix, windows))]
    {
        Ok(Box::new(Client {
            config: config.clone(),
            port: Some(serial::open(
                &config.device,
                config.baud,
                config.parity,
                1,
                8,
            )?),
            identities: BTreeMap::new(),
        }))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "HART serial requires a Unix terminal or Windows COM port",
        ))
    }
}
#[derive(Clone)]
struct Identity {
    address: [u8; 5],
    preambles: usize,
    metadata: Value,
}
#[cfg(any(unix, windows))]
struct Client {
    config: Config,
    port: Option<serial::Port>,
    identities: BTreeMap<u8, Identity>,
}
#[cfg(any(unix, windows))]
impl Source for Client {
    fn end_timeout_is_idle(&self) -> bool {
        false
    }
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("HART timeout is too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        if result.is_err() {
            self.port = None;
        }
        result
    }
}
#[cfg(any(unix, windows))]
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let port = self.port.as_mut().ok_or_else(|| {
            protocol("source session was permanently retired after an error; start a new recording")
        })?;
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for signal in &self.config.signals {
            remaining(deadline)?;
            if let Entry::Vacant(slot) = self.identities.entry(signal.polling_address) {
                let reply = exchange(
                    port,
                    self.config.master,
                    &[signal.polling_address],
                    0,
                    PREAMBLES,
                    deadline,
                )?;
                let identity = identity(&reply, signal.polling_address)?;
                slot.insert(identity);
            }
            let identity = self
                .identities
                .get(&signal.polling_address)
                .ok_or_else(|| protocol("device identity missing"))?;
            if signal
                .unique_address
                .as_ref()
                .is_some_and(|expected| unique_address(expected).ok() != Some(identity.address))
            {
                return Err(protocol(
                    "command-0 unique identity differs from configured unique_address",
                ));
            }
            let reply = exchange(
                port,
                self.config.master,
                &identity.address,
                signal.read.command(),
                identity.preambles,
                deadline,
            )?;
            let mut value = measurement(&reply, signal.read)?;
            value["unique_address"] = json!(crate::types::hex(identity.address));
            value["polling_address"] = json!(signal.polling_address);
            value["identity"] = identity.metadata.clone();
            let reading = Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            };
            if !readings.is_empty() {
                size.write_all(b",")
                    .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            }
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            readings.push(reading);
        }
        remaining(deadline)?;
        Ok(readings)
    }
}
struct Reply {
    raw: Vec<u8>,
    delimiter: u8,
    address: Vec<u8>,
    command: u8,
    status: [u8; 2],
    data: Vec<u8>,
    received_ns: u64,
}
fn identity(reply: &Reply, polling_address: u8) -> Result<Identity> {
    status(reply, false)?;
    let data = &reply.data;
    let [
        254,
        type_high,
        type_low,
        preambles,
        revision,
        _,
        _,
        _,
        _,
        id_high,
        id_middle,
        id_low,
        ..,
    ] = data.as_slice()
    else {
        return Err(protocol(
            "command-0 identity prefix is truncated or unsupported",
        ));
    };
    if !matches!(revision, 5..=7) {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "HART profile requires universal revision 5, 6 or 7",
        ));
    }
    if (*revision == 5 && polling_address > 15) || !(2..=20).contains(preambles) {
        return Err(protocol(
            "command-0 revision-specific extent, polling address or preamble count is invalid",
        ));
    }
    // Command0 bytes1/2 form the address-type pair (expanded device type in
    // HART7). The separate manufacturer at17/18 is not an address.
    let address = [type_high & 0x3f, *type_low, *id_high, *id_middle, *id_low];
    let mut metadata = json!({
        "universal_revision":revision,
        "address_type_code":u16::from_be_bytes([*type_high,*type_low]),
        "device_id":u32::from_be_bytes([0,*id_high,*id_middle,*id_low]),
        "request_preambles":preambles,"raw_data":data,"raw_reply":reply.raw,
        "received_monotonic_ns":reply.received_ns,"device_status":reply.status[1]
    });
    if *revision == 5 {
        metadata["manufacturer_code"] = json!(type_high);
    }
    if *revision >= 6 {
        let Some(
            [
                response_preambles,
                _,
                counter_high,
                counter_low,
                extended_status,
            ],
        ) = data.get(12..17)
        else {
            return Err(protocol("command-0 revision-specific extent is incomplete"));
        };
        metadata["response_preambles"] = json!(response_preambles);
        metadata["configuration_change_counter"] =
            json!(u16::from_be_bytes([*counter_high, *counter_low]));
        metadata["extended_device_status"] = json!(extended_status);
    }
    if *revision >= 7 {
        let Some(
            [
                manufacturer_high,
                manufacturer_low,
                private_high,
                private_low,
                profile,
            ],
        ) = data.get(17..22)
        else {
            return Err(protocol("command-0 revision-specific extent is incomplete"));
        };
        metadata["manufacturer_code"] =
            json!(u16::from_be_bytes([*manufacturer_high, *manufacturer_low]));
        metadata["private_label_code"] = json!(u16::from_be_bytes([*private_high, *private_low]));
        metadata["device_profile"] = json!(profile);
    }
    Ok(Identity {
        address,
        preambles: usize::from(*preambles).max(5),
        metadata,
    })
}

fn status(reply: &Reply, allow_update_warning: bool) -> Result<()> {
    if reply.status[0] == 0 || (allow_update_warning && reply.status[0] == 8) {
        return Ok(());
    }
    Err(Categorized::new(ErrorKind::OperationFailed,"HART field device rejected read or reported a communication error").details(json!({"hart_command":reply.command,"hart_response_code":reply.status[0],"hart_device_status":reply.status[1],"hart_raw_reply":reply.raw})).into())
}
fn measurement(reply: &Reply, query: Query) -> Result<Value> {
    status(reply, true)?;
    let data = &reply.data;
    let mut value = json!({"command":reply.command,"response_code":reply.status[0],"device_status":reply.status[1],"raw_reply":reply.raw,"received_monotonic_ns":reply.received_ns});
    value["command_status"] = json!(if reply.status[0] == 8 {
        "update_failure"
    } else {
        "success"
    });
    match query {
        Query::PrimaryVariable => {
            if data.len() != 5 {
                return Err(protocol(
                    "command-1 requires one unit byte and four IEEE754 bytes",
                ));
            }
            value["primary_variable"] = variable(data[0], &data[1..])?;
        }
        Query::LoopCurrentAndPercent => {
            if data.len() != 8 {
                return Err(protocol("command-2 requires two IEEE754 values"));
            }
            value["loop_current"] = number(&data[..4])?;
            value["loop_current"]["unit"] = json!("mA");
            value["percent_of_range"] = number(&data[4..])?;
            value["percent_of_range"]["unit"] = json!("%");
        }
        Query::DynamicVariables => {
            if !(9..=24).contains(&data.len()) || !(data.len() - 4).is_multiple_of(5) {
                return Err(protocol(
                    "command-3 requires loop current and 1–4 unit/value pairs",
                ));
            }
            value["loop_current"] = number(&data[..4])?;
            value["loop_current"]["unit"] = json!("mA");
            let mut variables = Vec::with_capacity(4);
            for (bytes, name) in data[4..].as_chunks::<5>().0.iter().zip([
                "primary",
                "secondary",
                "tertiary",
                "quaternary",
            ]) {
                let mut variable = variable(bytes[0], &bytes[1..])?;
                variable["variable"] = json!(name);
                variables.push(variable);
            }
            value["variables"] = json!(variables);
        }
    }
    Ok(value)
}
fn variable(unit: u8, bytes: &[u8]) -> Result<Value> {
    let mut value = number(bytes)?;
    value["unit_code"] = json!(unit);
    Ok(value)
}
fn number(bytes: &[u8]) -> Result<Value> {
    let bits = u32::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| protocol("IEEE754 field must have four bytes"))?,
    );
    let number = f32::from_bits(bits);
    let mut value = json!({"ieee754_bits":bits,"decode_status":if number.is_finite(){"finite"}else{"non_finite"}});
    if number.is_finite() {
        value["value"] = json!(f64::from(number));
    }
    Ok(value)
}
fn request(master: Master, address: &[u8], command: u8, preambles: usize) -> Result<Vec<u8>> {
    if !matches!(address.len(), 1 | 5)
        || command > 3
        || !(5..=PREAMBLES).contains(&preambles)
        || (address.len() == 1 && command != 0)
    {
        return Err(protocol("unsupported outgoing read frame"));
    }
    let (first, rest) = address
        .split_first()
        .ok_or_else(|| protocol("request address is missing"))?;
    let mut frame = vec![0xff; preambles];
    frame.push(if address.len() == 1 { 2 } else { 0x82 });
    frame.push((first & 0x3f) | master.bit());
    frame.extend_from_slice(rest);
    frame.extend_from_slice(&[command, 0]);
    frame.push(checksum(&frame[preambles..]));
    Ok(frame)
}
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, byte| sum ^ byte)
}
#[cfg(any(unix, windows))]
fn exchange(
    port: &mut serial::Port,
    master: Master,
    address: &[u8],
    command: u8,
    preambles: usize,
    deadline: Instant,
) -> Result<Reply> {
    let request = request(master, address, command, preambles)?;
    remaining(deadline)?;
    port.write_all(&request, deadline)
        .map_err(|error| io_error(&error))?;
    let mut skipped = 0;
    loop {
        remaining(deadline)?;
        let reply = receive(port, deadline)?;
        // Burst delimiters are not solicited replies, even if address/command
        // match. Address bit6 reports burst mode on ordinary replies as well.
        let first = reply
            .address
            .first()
            .ok_or_else(|| protocol("response address is missing"))?;
        let own_master = first & 0x80 == master.bit();
        let own_device = reply.address.len() == address.len()
            && reply.address.iter().zip(address).enumerate().all(
                |(index, (observed, expected))| {
                    let mask = if index == 0 { 0x3f } else { 0xff };
                    observed & mask == expected & mask
                },
            );
        if matches!(reply.delimiter, 6 | 0x86) && own_master && own_device {
            if reply.command != command {
                return Err(protocol(
                    "own-device reply command differs from pending read",
                ));
            }
            return Ok(reply);
        }
        skipped += reply.raw.len();
        if skipped > MAX_SKIPPED_BYTES {
            return Err(protocol(
                "foreign/burst traffic exceeds bounded per-query input limit",
            ));
        }
    }
}
#[cfg(any(unix, windows))]
fn receive(port: &mut serial::Port, deadline: Instant) -> Result<Reply> {
    let mut raw = Vec::with_capacity(300);
    let delimiter = loop {
        let mut byte = [0];
        port.read_exact(&mut byte, deadline)
            .map_err(|error| io_error(&error))?;
        raw.push(byte[0]);
        if byte[0] != 0xff {
            break byte[0];
        }
        if raw.len() > 64 {
            return Err(protocol("response preamble exceeds64-byte software bound"));
        }
    };
    if raw.len() < 3 || !matches!(delimiter, 1 | 2 | 6 | 0x81 | 0x82 | 0x86) {
        return Err(protocol(
            "unsupported delimiter or missing response preamble",
        ));
    }
    // This profile supports ordinary asynchronous zero-expansion frames only.
    let address_length = if delimiter & 0x80 == 0 { 1 } else { 5 };
    let mut header = vec![0; address_length + 2];
    port.read_exact(&mut header, deadline)
        .map_err(|error| io_error(&error))?;
    raw.extend_from_slice(&header);
    let (address, command_header) = header.split_at(address_length);
    let [command, length] = command_header else {
        return Err(protocol("response command/count header is incomplete"));
    };
    let command = *command;
    let length = usize::from(*length);
    let address = address.to_vec();
    let mut body = vec![0; length + 1];
    port.read_exact(&mut body, deadline)
        .map_err(|error| io_error(&error))?;
    let received_ns = crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(ErrorKind::Incomplete, "HART receive clock is unavailable")
    })?;
    raw.extend_from_slice(&body);
    let first = raw
        .iter()
        .position(|byte| *byte != 0xff)
        .ok_or_else(|| protocol("missing frame delimiter"))?;
    if checksum(&raw[first..]) != 0 {
        return Err(protocol("response XOR checksum differs"));
    }
    if !matches!(delimiter, 1 | 6 | 0x81 | 0x86) || length < 2 {
        return Err(protocol(
            "expected slave response/burst with two status bytes",
        ));
    }
    Ok(Reply {
        raw,
        delimiter,
        address,
        command,
        status: [body[0], body[1]],
        data: body[2..length].to_vec(),
        received_ns,
    })
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "HART poll deadline expired",
        ));
    }
    Ok(left)
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("HART: {message}"))
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("HART serial I/O: {error}"))
}
