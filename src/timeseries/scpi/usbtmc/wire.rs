//! Bounded private USB worker messages; no native USB calls occur here.
use super::Config;
use crate::types::{Categorized, ErrorKind, ErrorTag};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::io::{Read, Write};

pub(crate) const VERSION: u32 = 1;
pub(crate) const MAX_FRAME: usize = 131_072;
pub(crate) const MAX_DATA: usize = 65_536;

pub(crate) fn invalid(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
pub(crate) fn now() -> Result<u64> {
    crate::host::monotonic_ns()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            Categorized::error(
                ErrorKind::Unavailable,
                "USB worker monotonic clock is unavailable",
            )
        })
}
pub(crate) fn check_deadline(deadline_ns: u64, cutoff: Option<u64>) -> Result<()> {
    let current = now()?;
    if current >= deadline_ns || cutoff.is_some_and(|end| current >= end) {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "USB original acquisition deadline expired",
        ));
    }
    Ok(())
}
fn optional<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn optional_unsigned<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<u64>, D::Error> {
    crate::config_number::unsigned(d).map(Some)
}
fn details<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    use serde::de::Error as _;
    let value = crate::session::strict_json_value(d)?;
    if !value.is_object() {
        return Err(D::Error::custom("error details must be an object"));
    }
    Ok(Some(value))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) version: u32,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) request_id: u64,
    pub(crate) command: Command,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Open(Box<Open>),
    SendQuery(Box<SendQuery>),
    Receive(Box<Receive>),
    Complete(Box<Deadline>),
    Finish(Box<Deadline>),
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Open {
    pub(crate) selector: Config,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) deadline_ns: u64,
    #[serde(
        default,
        deserialize_with = "optional_unsigned",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) acquisition_end_ns: Option<u64>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendQuery {
    pub(crate) bytes_base64: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) deadline_ns: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receive {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) max_bytes: u32,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) deadline_ns: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Deadline {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) deadline_ns: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Empty;
impl Serialize for Empty {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        serializer.serialize_struct("Empty", 0)?.end()
    }
}
impl<'de> Deserialize<'de> for Empty {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        if let Value::Object(fields) = crate::session::strict_json_value(deserializer)?
            && fields.is_empty()
        {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(
                "USB acknowledgement must be an empty object",
            ))
        }
    }
}
impl Command {
    pub(crate) fn deadline(&self) -> u64 {
        match self {
            Self::Open(value) => value.deadline_ns,
            Self::SendQuery(value) => value.deadline_ns,
            Self::Receive(value) => value.deadline_ns,
            Self::Complete(value) | Self::Finish(value) => value.deadline_ns,
        }
    }
}
impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != VERSION || self.request_id == 0 || self.command.deadline() == 0 {
            return Err(invalid("invalid USB request version, identity or deadline"));
        }
        match &self.command {
            Command::Open(value) => value.selector.validate(),
            Command::SendQuery(value) => decode(&value.bytes_base64, 1, 4098).map(|_| ()),
            Command::Receive(value) if !(1..=65_536).contains(&value.max_bytes) => {
                Err(invalid("invalid USB receive credit"))
            }
            _ => Ok(()),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) version: u32,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) request_id: u64,
    pub(crate) result: Response,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Response {
    Ready(Box<Ready>),
    QuerySent(Empty),
    Data(Box<Data>),
    Complete(Empty),
    Finished(Box<Finished>),
    Failed(Box<Failed>),
}

impl Reply {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != VERSION || self.request_id == 0 {
            return Err(invalid("invalid USB reply envelope"));
        }
        match &self.result {
            Response::Ready(value) => {
                let d = &value.descriptor;
                d.validate(&Config {
                    vendor_id: d.vendor_id,
                    product_id: d.product_id,
                    serial_number: d.serial_number.clone(),
                    interface_number: d.interface_number,
                })
            }
            Response::Data(value) => {
                decode(&value.payload_base64, 1, MAX_DATA)?;
                if value.observed_ns == 0 {
                    return Err(invalid("USB Data has no completion clock"));
                }
                Ok(())
            }
            Response::Finished(value) => {
                if let Some(error) = &value.error {
                    error.validate()?;
                }
                if value.cleanup.needs_error() && value.error.is_none() {
                    return Err(invalid("USB cleanup uncertainty requires an error"));
                }
                Ok(())
            }
            Response::Failed(value) => {
                value.error.validate()?;
                if let Some(evidence) = &value.evidence {
                    evidence.validate()?;
                }
                Ok(())
            }
            Response::QuerySent(_) | Response::Complete(_) => Ok(()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ready {
    pub(crate) descriptor: Descriptor,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Data {
    pub(crate) payload_base64: String,
    pub(crate) eom: bool,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) observed_ns: u64,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Finished {
    pub(crate) cleanup: Cleanup,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) error: Option<Failure>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Failed {
    pub(crate) error: Failure,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) evidence: Option<Evidence>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) vendor_id: u16,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) product_id: u16,
    pub(crate) serial_number: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) interface_number: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) configuration: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) alternate_setting: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) class: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) subclass: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) protocol: u8,
    pub(crate) bulk_in: Endpoint,
    pub(crate) bulk_out: Endpoint,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) interrupt_in: Option<Endpoint>,
    pub(crate) capabilities_base64: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Endpoint {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) address: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) max_packet_size: u16,
}
impl Endpoint {
    fn validate(self, input: bool) -> Result<()> {
        if self.address & 0x70 != 0
            || self.address & 0x0f == 0
            || (self.address & 0x80 != 0) != input
            || !(1..=1024).contains(&self.max_packet_size)
        {
            return Err(invalid("invalid USB endpoint descriptor"));
        }
        Ok(())
    }
}
impl Descriptor {
    pub(crate) fn validate(&self, selector: &Config) -> Result<()> {
        selector.validate()?;
        if self.vendor_id != selector.vendor_id
            || self.product_id != selector.product_id
            || self.serial_number != selector.serial_number
            || self.interface_number != selector.interface_number
            || self.configuration == 0
            || self.alternate_setting != 0
            || self.class != 0xfe
            || self.subclass != 3
            || !matches!(self.protocol, 0 | 1)
        {
            return Err(invalid(
                "USB ready descriptor disagrees with the selected interface",
            ));
        }
        self.bulk_in.validate(true)?;
        self.bulk_out.validate(false)?;
        if self.bulk_out.max_packet_size < 4 || !self.bulk_out.max_packet_size.is_multiple_of(4) {
            return Err(invalid(
                "USB OUT packet size must be at least four and divisible by four",
            ));
        }
        if let Some(endpoint) = self.interrupt_in {
            endpoint.validate(true)?;
            if endpoint.address == self.bulk_in.address {
                return Err(invalid("duplicate USB endpoint address"));
            }
        }
        let bytes = decode(&self.capabilities_base64, 24, 24)?;
        let caps: [u8; 24] = bytes
            .try_into()
            .map_err(|_| invalid("invalid capabilities length"))?;
        validate_capabilities(&caps, self.protocol, self.interrupt_in.is_some())
    }
}
fn bcd(value: u16) -> bool {
    (0..4).all(|shift| (value >> (shift * 4)) & 0x0f <= 9)
}
pub(crate) fn validate_capabilities(c: &[u8; 24], protocol: u8, interrupt: bool) -> Result<()> {
    if c[0] != 1 {
        return Err(
            Categorized::new(ErrorKind::OperationFailed, "USB GET_CAPABILITIES failed")
                .details(serde_json::json!({"usbtmc_status":c[0]}))
                .into(),
        );
    }
    let version = u16::from_le_bytes([c[2], c[3]]);
    if !bcd(version) {
        return Err(invalid("invalid USBTMC version"));
    }
    if version < 0x0100 {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "unsupported USBTMC version",
        ));
    }
    if c[1] != 0 || c[4] & !7 != 0 || c[5] & !1 != 0 || c[6..12].iter().any(|v| *v != 0) {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "unsupported USBTMC capability extensions",
        ));
    }
    if c[4] & 3 != 0 {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "USB query/reply requires both directions",
        ));
    }
    match protocol {
        0 if c[12..24].iter().any(|v| *v != 0) => Err(Categorized::error(
            ErrorKind::Unsupported,
            "unsupported base-interface capability extensions",
        )),
        0 => Ok(()),
        1 => {
            let version = u16::from_le_bytes([c[12], c[13]]);
            if !bcd(version) {
                return Err(invalid("invalid USB488 version"));
            }
            if version < 0x0100 {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "unsupported USB488 version",
                ));
            }
            if c[14] & !7 != 0 || c[15] & !15 != 0 || c[16..24].iter().any(|v| *v != 0) {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "unsupported USB488 capability extensions",
                ));
            }
            if (c[15] & 1 != 0 && c[14] & 1 == 0)
                || (c[15] & 2 != 0 && c[14] & 2 == 0)
                || (c[14] & 4 != 0 && c[15] & 4 == 0)
                || (c[15] & 8 != 0 && (c[15] & 4 == 0 || c[14] & 4 == 0))
                || (c[15] & 4 != 0 && !interrupt)
            {
                return Err(invalid("inconsistent USB488 capabilities"));
            }
            Ok(())
        }
        _ => Err(invalid("invalid USBTMC interface protocol")),
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Failure {
    pub(crate) message: String,
    pub(crate) tag: Tag,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tag {
    pub(crate) code: String,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) hint: Option<String>,
    #[serde(
        default,
        deserialize_with = "details",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) details: Option<Value>,
}
fn bounded(value: &str, cap: usize) -> String {
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .take_while(|index| *index <= cap)
        .last()
        .unwrap_or(0);
    value.get(..end).unwrap_or("").to_owned()
}
impl Failure {
    pub(crate) fn from_error(error: &anyhow::Error) -> Self {
        let classified = crate::types::classify(error);
        let details = classified.details.map(|value| {
            let value = if value.is_object() {
                value
            } else {
                serde_json::json!({"native_details":value})
            };
            let bytes = serde_json::to_vec(&value).map_or(MAX_FRAME, |v| v.len());
            if bytes > 16_384 {
                serde_json::json!({"details_truncated":true,"original_details_bytes":bytes})
            } else {
                value
            }
        });
        Self {
            message: bounded(&error.to_string(), 4096),
            tag: Tag {
                code: classified.kind.code().0.into(),
                hint: classified.hint.map(|value| bounded(&value, 1024)),
                details,
            },
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.message.len() > 4096
            || self
                .tag
                .hint
                .as_ref()
                .is_some_and(|value| value.len() > 1024)
            || ErrorKind::from_code(&self.tag.code).is_none()
        {
            return Err(invalid("invalid USB worker error record"));
        }
        if let Some(value) = &self.tag.details
            && (!value.is_object() || serde_json::to_vec(value)?.len() > 16_384)
        {
            return Err(invalid("invalid USB worker error details"));
        }
        Ok(())
    }
    pub(crate) fn error(&self) -> anyhow::Error {
        crate::types::error_from_tag(
            Some(ErrorTag {
                code: self.tag.code.clone(),
                hint: self.tag.hint.clone(),
                details: self.tag.details.clone(),
            }),
            self.message.clone(),
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Evidence {
    pub(crate) payload_prefix_base64: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) payload_prefix_total: u32,
    pub(crate) payload_prefix_truncated: bool,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) observed_ns: u64,
}
impl Evidence {
    pub(crate) fn validate(&self) -> Result<()> {
        let prefix = decode(&self.payload_prefix_base64, 0, 4096)?;
        let total = usize::try_from(self.payload_prefix_total)?;
        if total > MAX_DATA
            || prefix.len() != total.min(4096)
            || self.payload_prefix_truncated != (total > 4096)
            || self.observed_ns == 0
        {
            return Err(invalid("invalid USB worker payload evidence"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClassAbort {
    NotNeeded,
    Succeeded,
    Failed,
    Unconfirmed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageState {
    NotStarted,
    CompletedEom,
    Unconfirmed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cleanup {
    pub(crate) host_resources_released: bool,
    pub(crate) native_transfers_settled: bool,
    pub(crate) class_abort: ClassAbort,
    pub(crate) message_state: MessageState,
}
impl Cleanup {
    pub(crate) const fn needs_error(self) -> bool {
        !self.host_resources_released
            || !self.native_transfers_settled
            || matches!(
                self.class_abort,
                ClassAbort::Failed | ClassAbort::Unconfirmed
            )
            || matches!(self.message_state, MessageState::Unconfirmed)
    }
}
pub(crate) fn decode(text: &str, minimum: usize, maximum: usize) -> Result<Vec<u8>> {
    if text.len() > maximum.div_ceil(3).saturating_mul(4) {
        return Err(invalid("USB payload exceeds its bounded credit"));
    }
    let bytes = STANDARD
        .decode(text)
        .map_err(|_| invalid("invalid USB payload base64"))?;
    if !(minimum..=maximum).contains(&bytes.len()) {
        return Err(invalid("USB payload length is outside its bounded credit"));
    }
    Ok(bytes)
}
pub(crate) fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn record(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("USB IPC records must be objects"))?;
    for key in [
        "selector",
        "descriptor",
        "bulk_in",
        "bulk_out",
        "interrupt_in",
        "error",
        "tag",
        "cleanup",
        "evidence",
    ] {
        if let Some(value) = object.get(key) {
            record(value)?;
        }
    }
    if let Some(value) = object.get("details")
        && !value.is_object()
    {
        return Err(invalid("USB error details must be an object"));
    }
    Ok(())
}
fn shape(value: &Value) -> Result<()> {
    let envelope = value
        .as_object()
        .ok_or_else(|| invalid("USB IPC envelope must be an object"))?;
    for key in ["command", "result"] {
        if let Some(value) = envelope.get(key) {
            let variants = value
                .as_object()
                .ok_or_else(|| invalid("USB IPC variant must be an object"))?;
            if variants.len() != 1 {
                return Err(invalid("USB IPC requires exactly one variant"));
            }
            for payload in variants.values() {
                record(payload)?;
            }
        }
    }
    Ok(())
}
pub(crate) fn read<T: DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut length = [0; 4];
    let first = loop {
        match input.read(&mut length[..1]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            result => break result?,
        }
    };
    if first == 0 {
        return Ok(None);
    }
    input.read_exact(&mut length[1..])?;
    let length = usize::try_from(u32::from_le_bytes(length))?;
    if !(1..=MAX_FRAME).contains(&length) {
        return Err(invalid("USB IPC frame length is outside its bound"));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let value = crate::session::strict_json_value(&mut deserializer)
        .map_err(|_| invalid("invalid or duplicate USB IPC JSON field"))?;
    deserializer
        .end()
        .map_err(|_| invalid("trailing USB IPC JSON bytes"))?;
    shape(&value)?;
    serde_json::from_value(value)
        .map(Some)
        .map_err(|_| invalid("invalid USB IPC JSON frame"))
}
struct Frame(Vec<u8>);
impl Write for Frame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > MAX_FRAME)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "USB IPC output exceeds its bound",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn write<T: Serialize>(output: &mut impl Write, value: &T) -> Result<()> {
    let mut frame = Frame(Vec::new());
    serde_json::to_writer(&mut frame, value)
        .map_err(|_| invalid("USB IPC serialization exceeds its bound"))?;
    let bytes = frame.0;
    if !(1..=MAX_FRAME).contains(&bytes.len()) {
        return Err(invalid("USB IPC output frame exceeds its bound"));
    }
    output.write_all(&u32::try_from(bytes.len())?.to_le_bytes())?;
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::classify;
    use std::io::Cursor;

    #[test]
    fn independent_ipc_literals_round_trip_and_reject_semantic_faults() {
        let oracle: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/usbtmc/ipc.json"))
                .unwrap();
        for row in oracle["frames"].as_array().unwrap() {
            let body = row["body_utf8"].as_str().unwrap().as_bytes();
            let mut frame = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
            frame.extend_from_slice(body);
            let valid = row["valid"].as_bool().unwrap();
            let mut output = Vec::new();
            let result = if row["direction"] == "request" {
                read::<Request>(&mut Cursor::new(&frame)).and_then(|value| {
                    let value = value.ok_or_else(|| invalid("missing fixture request"))?;
                    value.validate()?;
                    write(&mut output, &value)
                })
            } else {
                read::<Reply>(&mut Cursor::new(&frame)).and_then(|value| {
                    let value = value.ok_or_else(|| invalid("missing fixture reply"))?;
                    value.validate()?;
                    write(&mut output, &value)
                })
            };
            if valid {
                result.unwrap_or_else(|error| panic!("{}: {error}", row["name"]));
                let (prefix, encoded) = output.split_at_checked(4).unwrap();
                let length = u32::from_le_bytes(prefix.try_into().unwrap());
                assert_eq!(usize::try_from(length).unwrap(), encoded.len());
                let actual: Value = serde_json::from_slice(encoded).unwrap();
                let expected: Value = serde_json::from_slice(body).unwrap();
                assert_eq!(actual, expected, "{}", row["name"]);
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    classify(&error).kind,
                    ErrorKind::Incomplete,
                    "{}",
                    row["name"]
                );
            }
        }
    }
    #[test]
    fn frame_bounds_and_nulls_fail_before_payload_allocation() {
        for length in [0_u32, u32::try_from(MAX_FRAME + 1).unwrap(), u32::MAX] {
            let error = read::<Reply>(&mut Cursor::new(length.to_le_bytes())).unwrap_err();
            assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        }
        for input in [
            r#"{"version":1,"request_id":1.0,"command":{"finish":{"deadline_ns":2e0}}}"#,
            r#"{"version":1e0,"request_id":1,"command":{"finish":{"deadline_ns":2}}}"#,
        ] {
            let request: Request = serde_json::from_str(input).unwrap();
            request.validate().unwrap();
            assert_eq!(request.request_id, 1);
        }
        let mut frame = Frame(Vec::new());
        let error = write(&mut frame, &"x".repeat(MAX_FRAME)).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        assert_eq!(frame.0, Vec::<u8>::new());
    }
    #[test]
    fn independent_capabilities_and_endpoint_policies_remain_distinct() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/usbtmc/selector-control.json"
        ))
        .unwrap();
        let config: Config = serde_json::from_value(oracle["selector"].clone()).unwrap();
        let descriptor: Descriptor = serde_json::from_value(oracle["descriptor"].clone()).unwrap();
        descriptor.validate(&config).unwrap();
        for size in [1, 2, 6] {
            let mut changed = descriptor.clone();
            changed.bulk_out.max_packet_size = size;
            assert_eq!(
                classify(&changed.validate(&config).unwrap_err()).kind,
                ErrorKind::Incomplete
            );
        }
        let mut changed = descriptor;
        changed.bulk_in.max_packet_size = 1;
        changed.validate(&config).unwrap();
        for row in oracle["capabilities"].as_array().unwrap() {
            let text = row["hex"].as_str().unwrap();
            let mut bytes = Vec::new();
            for pair in text.as_bytes().as_chunks::<2>().0 {
                bytes.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
            }
            let result = <[u8; 24]>::try_from(bytes)
                .map_err(|_| invalid("short capabilities"))
                .and_then(|bytes| {
                    validate_capabilities(
                        &bytes,
                        u8::try_from(row["protocol"].as_u64().unwrap()).unwrap(),
                        true,
                    )
                });
            if row["valid"].as_bool().unwrap() {
                result.unwrap_or_else(|error| panic!("{}: {error}", row["name"]));
            } else {
                let _ = result.unwrap_err();
            }
        }
    }
    #[test]
    fn positional_records_duplicates_and_incomplete_evidence_prefix_are_rejected() {
        for (request, body) in [
            (true, r#"[1,1,{"finish":{"deadline_ns":2}}]"#),
            (
                true,
                r#"{"version":1,"request_id":1,"command":{"finish":[2]}}"#,
            ),
            (
                false,
                r#"{"version":1,"request_id":1,"result":{"query_sent":[]}}"#,
            ),
            (
                false,
                r#"{"version":1,"request_id":1,"result":{"ready":{"descriptor":[]}}}"#,
            ),
            (
                false,
                r#"{"version":1,"request_id":1,"result":{"failed":{"error":{"message":"x","tag":{"code":"failed","details":{"a":1,"a":2}}}}}}"#,
            ),
        ] {
            let mut bytes = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
            bytes.extend_from_slice(body.as_bytes());
            let result = if request {
                read::<Request>(&mut Cursor::new(bytes)).map(|_| ())
            } else {
                read::<Reply>(&mut Cursor::new(bytes)).map(|_| ())
            };
            assert_eq!(classify(&result.unwrap_err()).kind, ErrorKind::Incomplete);
        }
        let mut evidence = Evidence {
            payload_prefix_base64: String::new(),
            payload_prefix_total: 4,
            payload_prefix_truncated: true,
            observed_ns: 1,
        };
        assert_eq!(
            classify(&evidence.validate().unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        evidence.payload_prefix_base64 = "QUJDRA==".into();
        evidence.payload_prefix_truncated = false;
        evidence.validate().unwrap();
        evidence.payload_prefix_total = 4097;
        evidence.payload_prefix_base64 = encode(&[0; 4096]);
        evidence.payload_prefix_truncated = true;
        evidence.validate().unwrap();
        evidence.payload_prefix_base64 = encode(&[0; 4095]);
        assert_eq!(
            classify(&evidence.validate().unwrap_err()).kind,
            ErrorKind::Incomplete
        );
    }
    #[test]
    fn error_transport_preserves_typed_cause_and_utf8_bounds() {
        let original = Categorized::new(ErrorKind::PermissionDenied, "é".repeat(3000))
            .hint("driver association")
            .details(serde_json::json!({"native_code":5}));
        let failure = Failure::from_error(&original.into());
        failure.validate().unwrap();
        assert_eq!(failure.message.len(), 4096);
        let reconstructed = failure.error();
        let value = classify(&reconstructed);
        assert_eq!(value.kind, ErrorKind::PermissionDenied);
        assert_eq!(value.hint.as_deref(), Some("driver association"));
        assert_eq!(value.details.unwrap()["native_code"], 5);
    }
}
