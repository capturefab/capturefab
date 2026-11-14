//! Read-only ifm `IoT` Core process input, AL1352 manual sections 9.2.1/9.2.7/9.2.11:
//! <https://www.ifm.com/mounting/80284138UK.pdf>.
//! A port's IODD supplies its mapping; payload bytes are always retained.
use super::{Reading, Source, net as transport};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};
use zeroize::Zeroize as _;

const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub port: u8,
    #[serde(default)]
    pub code_format: CodeFormat,
    #[serde(default = "raw_name")]
    pub signal: String,
    #[serde(default)]
    pub signals: Vec<Signal>,
    #[serde(default)]
    pub tls: Option<transport::TlsConfig>,
    #[serde(default)]
    pub username_env: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodeFormat {
    #[default]
    Text,
    Numeric,
}
fn raw_name() -> String {
    "process_data".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub offset: u8,
    #[serde(default)]
    pub data_type: DataType,
    #[serde(default)]
    pub byte_order: ByteOrder,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub bit: Option<u8>,
    /// Explicit arithmetic shift for signed integers; logical shift for unsigned integers.
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub right_shift: u8,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default)]
    pub offset_value: f64,
}
const fn one() -> f64 {
    1.0
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
    const fn width(self) -> usize {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if !(1..=8).contains(&config.port) {
        return Err(Usage(
            "ifm_iot port must be 1–8; configure one physical port per source".into(),
        )
        .into());
    }
    validate_signals(&config.signal, &config.signals)?;
    if config.username_env.is_some() != config.password_env.is_some() {
        return Err(Usage(
            "ifm_iot username_env and password_env must be supplied together".into(),
        )
        .into());
    }
    if config.username_env.is_some() && config.tls.is_none() {
        return Err(Usage("ifm_iot authenticated security mode requires TLS".into()).into());
    }
    for name in config.username_env.iter().chain(&config.password_env) {
        if name.is_empty() || name.len() > 256 || name.contains(['=', '\0']) {
            return Err(
                Usage("invalid ifm_iot credential environment variable name".into()).into(),
            );
        }
    }
    Ok(Box::new(Capture {
        config: config.clone(),
        tls: transport::prepare(config.tls.as_ref())?,
        cid: 0,
        observed: None,
        ready: false,
    }))
}

pub(super) fn validate_signals(raw_signal: &str, signals: &[Signal]) -> Result<()> {
    super::validate_name(raw_signal, "IO-Link raw signal")?;
    if signals.len() > 256 {
        return Err(Usage("IO-Link accepts at most 256 mapped signals".into()).into());
    }
    let mut names = BTreeSet::from([raw_signal]);
    for signal in signals {
        super::validate_name(&signal.name, "IO-Link mapped signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage(
                "IO-Link signal names must be unique and differ from the raw signal".into(),
            )
            .into());
        }
        if usize::from(signal.offset) + signal.data_type.width() > 32 {
            return Err(Usage(
                "IO-Link signal range exceeds the 32-byte process input limit".into(),
            )
            .into());
        }
        if signal.bit.is_some_and(|bit| bit > 7)
            || (signal.bit.is_some() && !matches!(signal.data_type, DataType::Bool))
            || usize::from(signal.right_shift) >= signal.data_type.width() * 8
            || (signal.right_shift != 0
                && matches!(
                    signal.data_type,
                    DataType::Bool | DataType::F32 | DataType::F64
                ))
        {
            return Err(Usage("IO-Link bit applies only to bool (0–7); right_shift applies only to integers and must fit their width".into()).into());
        }
        if !signal.scale.is_finite()
            || !signal.offset_value.is_finite()
            || (matches!(
                signal.data_type,
                DataType::Bool | DataType::U64 | DataType::I64
            ) && (signal.scale != 1.0 || signal.offset_value != 0.0))
        {
            return Err(Usage(
                "IO-Link scaling must be finite; bool and 64-bit integers cannot be scaled".into(),
            )
            .into());
        }
    }
    Ok(())
}

struct Capture {
    config: Config,
    tls: transport::Prepared,
    cid: u16,
    observed: Option<u64>,
    ready: bool,
}
impl Source for Capture {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if timeout.is_zero() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| malformed("read deadline overflow"))?;
        self.cid = self.cid % 32767 + 1;
        let mut request = json!({
            "code":match self.config.code_format { CodeFormat::Text => json!("request"), CodeFormat::Numeric => json!(10) }, "cid":self.cid,
            "adr":format!("/iolinkmaster/port[{}]/iolinkdevice/pdin/getdata", self.config.port),
        });
        if let (Some(user), Some(password)) = (&self.config.username_env, &self.config.password_env)
        {
            request["auth"] = json!({"user":STANDARD.encode(credential(user)?.as_bytes()),
                                    "passwd":STANDARD.encode(credential(password)?.as_bytes())});
        }
        let body = zeroize::Zeroizing::new(serde_json::to_vec(&request)?);
        if let Some(auth) = request.get_mut("auth").and_then(Value::as_object_mut) {
            for value in auth.values_mut() {
                if let Value::String(value) = value {
                    value.zeroize();
                }
            }
        }
        drop(request);
        let head = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            self.config.address,
            body.len()
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io_error(&io::Error::new(
                io::ErrorKind::TimedOut,
                "request deadline expired",
            )));
        }
        let socket = TcpStream::connect_timeout(&self.config.address, remaining)
            .map_err(|error| io_error(&error))?;
        let mut stream = self.tls.connect(socket, deadline)?;
        stream
            .write_all(head.as_bytes())
            .map_err(|error| io_error(&error))?;
        stream.write_all(&body).map_err(|error| io_error(&error))?;
        stream.flush().map_err(|error| io_error(&error))?;
        let body = super::http::response(&mut stream, MAX_BODY).map_err(|error| {
            error
                .downcast_ref::<super::http::Status>()
                .map(|status| diagnostic(status.0))
                .unwrap_or(error)
        })?;
        let observed = crate::host::monotonic_ns();
        let reply: Reply = serde_json::from_slice(&body)
            .map_err(|_| malformed("invalid IoT Core JSON response"))?;
        if reply.cid != self.cid {
            return Err(malformed(
                "IoT Core response correlation ID does not match the request",
            ));
        }
        if reply.code != 200 {
            return Err(diagnostic(reply.code));
        }
        let data: ProcessData = serde_json::from_value(
            reply
                .data
                .ok_or_else(|| malformed("IoT Core response omits process data"))?,
        )
        .map_err(|_| malformed("invalid IoT Core process data"))?;
        let payload = decode_hex(&data.value)?;
        let mut readings = vec![Reading {
            signal: self.config.signal.clone(),
            value: json!({"kind":"iolink_process_data","port":self.config.port,"data_hex":crate::types::hex(&payload),"length":payload.len()}),
            source_time: None,
        }];
        for signal in &self.config.signals {
            readings.push(signal.decode(&payload)?);
        }
        self.observed = observed;
        self.ready = true;
        Ok(readings)
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn ready(&self) -> bool {
        self.ready
    }
}

#[derive(Deserialize)]
struct Reply {
    cid: u16,
    code: u16,
    data: Option<Value>,
}
#[derive(Deserialize)]
struct ProcessData {
    value: String,
}

fn credential(name: &str) -> Result<zeroize::Zeroizing<String>> {
    let value = std::env::var(name).map_err(|_| {
        Categorized::error(
            ErrorKind::PermissionDenied,
            format!("ifm_iot credential variable {name} is missing or is not UTF-8"),
        )
    })?;
    if value.len() > 1024 {
        return Err(Usage("ifm_iot credentials must be at most 1024 UTF-8 bytes".into()).into());
    }
    Ok(zeroize::Zeroizing::new(value))
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("ifm_iot: {message}"))
}
fn diagnostic(code: u16) -> anyhow::Error {
    let kind = match code {
        401 => ErrorKind::AuthRejected,
        403 => ErrorKind::PermissionDenied,
        503 => ErrorKind::Unavailable,
        530 | 531 => ErrorKind::Incomplete,
        _ => ErrorKind::OperationFailed,
    };
    Categorized::new(
        kind,
        format!("ifm IoT Core rejected process input read (diagnostic {code})"),
    )
    .details(json!({"diagnostic_code":code}))
    .into()
}
fn io_error(error: &io::Error) -> anyhow::Error {
    #[cfg(feature = "mqtt-tls")]
    if error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<rustls::Error>)
    {
        return Categorized::error(
            ErrorKind::AuthRejected,
            "ifm_iot TLS certificate verification or handshake failed",
        );
    }
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::UnexpectedEof => ErrorKind::Incomplete,
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::BrokenPipe => ErrorKind::Disconnected,
        _ => ErrorKind::Unavailable,
    };
    Categorized::error(kind, format!("ifm_iot HTTP I/O failed: {error}"))
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if value.len() > 64
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(malformed(
            "process input must be at most 32 bytes of hexadecimal data",
        ));
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair)?;
            Ok(u8::from_str_radix(text, 16)?)
        })
        .collect()
}

impl Signal {
    pub(super) fn decode(&self, payload: &[u8]) -> Result<Reading> {
        let start = usize::from(self.offset);
        let bytes = payload
            .get(start..start + self.data_type.width())
            .ok_or_else(|| {
                malformed("process input is shorter than its configured signal mapping")
            })?;
        macro_rules! number {
            ($kind:ty, $width:expr) => {{
                let array: [u8; $width] = bytes.try_into()?;
                match self.byte_order {
                    ByteOrder::Little => <$kind>::from_le_bytes(array),
                    ByteOrder::Big => <$kind>::from_be_bytes(array),
                }
            }};
        }
        let value = match self.data_type {
            DataType::Bool => Value::Bool(bytes[0] & (1 << self.bit.unwrap_or(0)) != 0),
            DataType::U8 => json!(number!(u8, 1) >> self.right_shift),
            DataType::I8 => json!(number!(i8, 1) >> self.right_shift),
            DataType::U16 => json!(number!(u16, 2) >> self.right_shift),
            DataType::I16 => json!(number!(i16, 2) >> self.right_shift),
            DataType::U32 => json!(number!(u32, 4) >> self.right_shift),
            DataType::I32 => json!(number!(i32, 4) >> self.right_shift),
            DataType::U64 => json!(number!(u64, 8) >> self.right_shift),
            DataType::I64 => json!(number!(i64, 8) >> self.right_shift),
            DataType::F32 => {
                let value = number!(f32, 4);
                if !value.is_finite() {
                    return Err(malformed("non-finite mapped float"));
                }
                json!(value)
            }
            DataType::F64 => {
                let value = number!(f64, 8);
                if !value.is_finite() {
                    return Err(malformed("non-finite mapped float"));
                }
                json!(value)
            }
        };
        let value = if self.scale == 1.0 && self.offset_value == 0.0 {
            value
        } else {
            let value = value
                .as_f64()
                .ok_or_else(|| malformed("cannot scale mapped value"))?
                * self.scale
                + self.offset_value;
            if !value.is_finite() {
                return Err(malformed("non-finite scaled signal"));
            }
            json!(value)
        };
        Ok(Reading {
            signal: self.name.clone(),
            value,
            source_time: None,
        })
    }
}

#[cfg(test)]
mod config_tests {
    use super::Config;

    #[test]
    fn integer_configuration_preserves_process_data_mapping_and_defaults() {
        let configured: Config = serde_json::from_str(
            r#"{"address":"127.0.0.1:80","port":1e0,"signals":[{"name":"value","offset":2.0,"bit":null,"right_shift":3e0}]}"#,
        )
        .unwrap();
        assert_eq!(configured.port, 1);
        assert_eq!(configured.signal, "process_data");
        assert_eq!(configured.signals[0].bit, None);
        assert_eq!(
            configured.signals[0]
                .decode(&[0, 0, 0x12, 0x38])
                .unwrap()
                .value
                .as_u64(),
            Some(583)
        );
        for raw in [
            r#"{"address":"127.0.0.1:80","port":256.0}"#,
            r#"{"address":"127.0.0.1:80","port":1,"signals":[{"name":"value","offset":0,"right_shift":3.5}]}"#,
        ] {
            serde_json::from_str::<Config>(raw).unwrap_err();
        }
    }
}
