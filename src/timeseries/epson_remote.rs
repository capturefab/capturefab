//! Read-only Epson Remote Ethernet monitoring with explicit documented profiles.
use super::{Reading, Source, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use zeroize::{Zeroize as _, Zeroizing};

mod parse;
#[cfg(test)]
mod tests;
mod wire;

const MAX_REPLY: usize = 16 * 1024;
const MAX_RAW: usize = 512 * 1024;
const MAX_BATCH: usize = (1 << 20) - 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    #[serde(rename = "rc_plus_7_5")]
    RcPlus75,
    #[serde(rename = "rc_plus_8_0")]
    RcPlus80,
    #[serde(rename = "rc_plus_8_0_rc800")]
    RcPlus80Rc800,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Terminator {
    Crlf,
    Cr,
    Lf,
}
impl Terminator {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Crlf => b"\r\n",
            Self::Cr => b"\r",
            Self::Lf => b"\n",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Resource {
    Status,
    Alarms,
    CurrentRobot,
    Io,
    IoByte,
    IoWord,
    MemoryIo,
    MemoryIoByte,
    MemoryIoWord,
    Variable,
}
impl Resource {
    const fn method(self) -> &'static str {
        match self {
            Self::Status => "GetStatus",
            Self::Alarms => "GetAlm",
            Self::CurrentRobot => "GetCurRobot",
            Self::Io => "GetIO",
            Self::IoByte => "GetIOByte",
            Self::IoWord => "GetIOWord",
            Self::MemoryIo => "GetMemIO",
            Self::MemoryIoByte => "GetMemIOByte",
            Self::MemoryIoWord => "GetMemIOWord",
            Self::Variable => "GetVariable",
        }
    }
    const fn io_bits(self) -> Option<u8> {
        match self {
            Self::Io | Self::MemoryIo => Some(1),
            Self::IoByte | Self::MemoryIoByte => Some(8),
            Self::IoWord | Self::MemoryIoWord => Some(16),
            Self::Status | Self::Alarms | Self::CurrentRobot | Self::Variable => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
enum VariableType {
    Boolean,
    Byte,
    UByte,
    Short,
    UShort,
    Integer,
    Long,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Real,
    Double,
    String,
}
impl VariableType {
    const fn name(self) -> &'static str {
        match self {
            Self::Boolean => "Boolean",
            Self::Byte => "Byte",
            Self::UByte => "UByte",
            Self::Short => "Short",
            Self::UShort => "UShort",
            Self::Integer => "Integer",
            Self::Long => "Long",
            Self::Int32 => "Int32",
            Self::UInt32 => "UInt32",
            Self::Int64 => "Int64",
            Self::UInt64 => "UInt64",
            Self::Real => "Real",
            Self::Double => "Double",
            Self::String => "String",
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    signal: String,
    resource: Resource,
    #[serde(
        default,
        deserialize_with = "crate::config_number::optional_unsigned",
        skip_serializing_if = "Option::is_none"
    )]
    address: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    declared_type: Option<VariableType>,
    #[serde(
        default,
        deserialize_with = "indices",
        skip_serializing_if = "Vec::is_empty"
    )]
    indices: Vec<u16>,
    #[serde(
        default,
        deserialize_with = "crate::config_number::optional_unsigned",
        skip_serializing_if = "Option::is_none"
    )]
    count: Option<u16>,
}
fn indices<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<u16>, D::Error> {
    #[derive(Deserialize)]
    struct Index(#[serde(deserialize_with = "crate::config_number::unsigned")] u16);
    Ok(Vec::<Index>::deserialize(deserializer)?
        .into_iter()
        .map(|Index(value)| value)
        .collect())
}
impl Selection {
    fn request(&self, terminator: Terminator) -> String {
        use std::fmt::Write as _;
        let mut text = format!("${}", self.resource.method());
        if let Some(address) = self.address {
            write!(text, ",{address}").expect("String formatting is infallible");
        }
        if let Some(name) = &self.name {
            text.push(',');
            text.push_str(name);
            if !self.indices.is_empty() {
                text.push('(');
                for (index, element) in self.indices.iter().enumerate() {
                    if index != 0 {
                        text.push(',');
                    }
                    write!(text, "{element}").expect("String formatting is infallible");
                }
                text.push(')');
            }
            if let Some(kind) = self.declared_type {
                text.push(',');
                text.push_str(kind.name());
            }
            if let Some(count) = self.count {
                write!(text, ",{count}").expect("String formatting is infallible");
            }
        }
        text.push_str(match terminator {
            Terminator::Crlf => "\r\n",
            Terminator::Cr => "\r",
            Terminator::Lf => "\n",
        });
        text
    }
    fn validate(&self, profile: Profile) -> Result<()> {
        super::validate_name(&self.signal, "Epson signal")?;
        if (self.resource == Resource::CurrentRobot && profile == Profile::RcPlus75)
            || (self.resource == Resource::Alarms && profile == Profile::RcPlus80Rc800)
        {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "selected Epson GET response is not qualified for this explicit profile",
            ));
        }
        if self.resource == Resource::Variable {
            if self.address.is_some() || !self.name.as_deref().is_some_and(identifier) {
                return Err(Usage(
                    "Epson variables require a 1..128-byte ASCII identifier and no I/O address"
                        .into(),
                )
                .into());
            }
            let kind = self
                .declared_type
                .ok_or_else(|| Usage("Epson variables require an explicit declared_type".into()))?;
            if kind == VariableType::String {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "Epson String reply escaping and embedded terminators are not qualified; String arrays are forbidden",
                ));
            }
            if self.indices.len() > 3
                || self.indices.is_empty() != self.count.is_none()
                || self.count.is_some_and(|count| !(1..=100).contains(&count))
            {
                return Err(Usage(
                    "Epson arrays require 1..3 indices and count 1..100; scalar reads omit both"
                        .into(),
                )
                .into());
            }
        } else if self.name.is_some()
            || self.declared_type.is_some()
            || !self.indices.is_empty()
            || self.count.is_some()
        {
            return Err(Usage(
                "Epson name/declared_type/indices/count apply only to variable reads".into(),
            )
            .into());
        }
        if self.resource.io_bits().is_some() != self.address.is_some() {
            return Err(Usage(
                "Epson I/O reads require address; other resources must omit address".into(),
            )
            .into());
        }
        Ok(())
    }
}
fn identifier(value: &str) -> bool {
    value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    profile: Profile,
    address: SocketAddr,
    password_env: String,
    terminator: Terminator,
    reads: Vec<Selection>,
}
impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.address.port() == 0
            || self.address.ip().is_unspecified()
            || self.address.ip().is_multicast()
            || self.address.ip() == std::net::Ipv4Addr::BROADCAST
        {
            return Err(Usage(
                "Epson requires an explicit numeric unicast endpoint with nonzero port".into(),
            )
            .into());
        }
        if self.password_env.is_empty()
            || self.password_env.len() > 256
            || self.password_env.contains('=')
            || self.password_env.chars().any(char::is_control)
        {
            return Err(Usage("invalid Epson password environment variable name".into()).into());
        }
        if !(1..=32).contains(&self.reads.len()) {
            return Err(Usage("Epson requires 1..32 selected monitor reads".into()).into());
        }
        let mut signals = BTreeSet::new();
        for read in &self.reads {
            read.validate(self.profile)?;
            if !signals.insert(&read.signal) {
                return Err(Usage("Epson signal names must be unique".into()).into());
            }
        }
        Ok(())
    }
}
pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    let password = Zeroizing::new(std::env::var(&config.password_env).map_err(|_| {
        Categorized::error(
            ErrorKind::AuthRequired,
            "Epson password environment variable is missing or is not UTF-8",
        )
    })?);
    validate_password(&password)?;
    crate::scrub::register(&password);
    Ok(Box::new(Reader {
        config: config.clone(),
        password,
        link: None,
        failure: None,
        observed: None,
        ready: false,
        finished: false,
    }))
}
fn validate_password(password: &str) -> Result<()> {
    if password.len() > 256
        || !password
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte) && byte != b',')
    {
        return Err(Usage(
            "Epson password must be at most 256 printable ASCII bytes without comma or terminator"
                .into(),
        )
        .into());
    }
    Ok(())
}
struct Reader {
    config: Config,
    password: Zeroizing<String>,
    link: Option<wire::Link>,
    failure: Option<Arc<anyhow::Error>>,
    observed: Option<u64>,
    ready: bool,
    finished: bool,
}
#[derive(Debug)]
struct Failed(Arc<anyhow::Error>);
impl std::fmt::Display for Failed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Epson source retired after its failed poll")
    }
}
impl std::error::Error for Failed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::Incomplete,
        format!("Epson Remote Ethernet: {message}"),
    )
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "Epson original poll deadline expired",
        ));
    }
    Ok(left)
}
fn clock() -> Result<u64> {
    crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(
            ErrorKind::Unavailable,
            "Epson host receive clock is unavailable",
        )
    })
}
#[derive(Serialize)]
struct Record<'a> {
    kind: &'static str,
    profile: Profile,
    resource: Resource,
    method: &'static str,
    selection: &'a Selection,
    raw_reply_text: &'a str,
    raw_reply_base64: parse::Base64<'a>,
    reply_bytes: usize,
    host_window_basis: &'static str,
    host_window_ns: wire::Window,
    native: parse::Native<'a>,
}
impl Reader {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        remaining(deadline)?;
        let mut raw_bytes = 0;
        if self.link.is_none() {
            let mut link = wire::Link::connect(self.config.address, deadline)?;
            let mut request = Zeroizing::new(Vec::with_capacity(self.password.len() + 10));
            request.extend_from_slice(b"$Login,");
            request.extend_from_slice(self.password.as_bytes());
            request.extend_from_slice(self.config.terminator.bytes());
            let response =
                link.exchange(&request, self.config.terminator, deadline, &mut raw_bytes)?;
            let payload = parse::payload(&response.raw, self.config.terminator, "Login")?;
            if payload != "0" {
                return Err(incomplete("invalid Login success response"));
            }
            self.password.zeroize();
            self.link = Some(link);
        }
        let link = self
            .link
            .as_mut()
            .ok_or_else(|| incomplete("missing private monitor connection"))?;
        let mut readings = Vec::with_capacity(self.config.reads.len());
        let mut size = Size { bytes: 2, deadline };
        for read in &self.config.reads {
            remaining(deadline)?;
            let request = read.request(self.config.terminator);
            let response = link.exchange(
                request.as_bytes(),
                self.config.terminator,
                deadline,
                &mut raw_bytes,
            )?;
            let payload = parse::payload(
                &response.raw,
                self.config.terminator,
                read.resource.method(),
            )?;
            let raw_reply_text = str::from_utf8(&response.raw)
                .map_err(|_| incomplete("reply is not ASCII"))?
                .strip_suffix(match self.config.terminator {
                    Terminator::Crlf => "\r\n",
                    Terminator::Cr => "\r",
                    Terminator::Lf => "\n",
                })
                .ok_or_else(|| incomplete("missing selected reply terminator"))?;
            let record = Record {
                kind: "epson_remote",
                profile: self.config.profile,
                resource: read.resource,
                method: read.resource.method(),
                selection: read,
                raw_reply_text,
                raw_reply_base64: parse::Base64(&response.raw),
                reply_bytes: response.raw.len(),
                host_window_basis: "request_start_through_reply_received",
                host_window_ns: response.window,
                native: parse::native(read, payload)?,
            };
            #[derive(Serialize)]
            struct Borrowed<'a, T> {
                signal: &'a str,
                value: T,
            }
            serde_json::to_writer(
                &mut size,
                &Borrowed {
                    signal: &read.signal,
                    value: &record,
                },
            )
            .map_err(|_| {
                remaining(deadline).err().unwrap_or_else(|| {
                    incomplete("escaped Reading batch exceeds 1 MiB with wrapper reserve")
                })
            })?;
            size.write_all(b",").map_err(|_| {
                remaining(deadline)
                    .err()
                    .unwrap_or_else(|| incomplete("Reading batch exceeds its bound"))
            })?;
            readings.push(Reading {
                signal: read.signal.clone(),
                value: serde_json::to_value(record)?,
                source_time: None,
            });
        }
        serde_json::to_writer(Size { bytes: 0, deadline }, &readings).map_err(|_| {
            remaining(deadline)
                .err()
                .unwrap_or_else(|| incomplete("Reading batch exceeds its bound"))
        })?;
        remaining(deadline)?;
        self.observed = Some(clock()?);
        remaining(deadline)?;
        self.ready = true;
        Ok(readings)
    }
}
impl Source for Reader {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if let Some(error) = &self.failure {
            return Err(Failed(error.clone()).into());
        }
        if self.finished {
            return Err(Categorized::error(
                ErrorKind::OperationFailed,
                "Epson monitor source has finished",
            ));
        }
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("Epson timeout is too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        result.map_err(|error| {
            self.link = None;
            self.password.zeroize();
            let error = Arc::new(error);
            self.failure = Some(error.clone());
            Failed(error).into()
        })
    }
    fn ready(&self) -> bool {
        self.ready
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        self.link = None;
        self.password.zeroize();
        self.finished = true;
        Ok(())
    }
}
struct Size {
    bytes: usize,
    deadline: Instant,
}
impl io::Write for Size {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Epson serialization deadline",
            ));
        }
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_BATCH {
            return Err(io::Error::other("Epson Reading limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
