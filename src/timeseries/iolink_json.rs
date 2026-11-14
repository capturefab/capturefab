//! Read-only JSON Integration for IO-Link process data (GET only).
//! The manufacturer V1 endpoint is the default. Community 2.0.0 moves the
//! base path to V2; `base_path` selects a matching gateway endpoint:
//! <https://github.com/iolinkcommunity/JSON_for_IO-Link/releases/tag/2.0.0>.
//! SICK SIG200/SIG350 and Balluff BNI document this interface separately from
//! ifm's `IoT` Core POST interface. No device/port configuration is changed.
use super::{Reading, Source, http, ifm_iot, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    pub device: String,
    #[serde(default = "base_path")]
    pub base_path: String,
    #[serde(default)]
    pub format: Format,
    #[serde(default = "signal")]
    pub signal: String,
    #[serde(default)]
    pub signals: Vec<ifm_iot::Signal>,
    #[serde(default)]
    pub json_signals: Vec<JsonSignal>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub tls: Option<net::TlsConfig>,
    #[serde(default)]
    pub username_env: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default)]
    pub bearer_token_env: Option<String>,
}
fn base_path() -> String {
    "/iolink/v1".into()
}
fn signal() -> String {
    "process_data".into()
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Format {
    #[default]
    ByteArray,
    Iodd,
}
impl Format {
    const fn parameter(self) -> &'static str {
        match self {
            Self::ByteArray => "byteArray",
            Self::Iodd => "iodd",
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JsonSignal {
    pub name: String,
    /// RFC 6901 pointer in the complete process-data response, including units.
    pub pointer: String,
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 {
        return Err(Usage("iolink_json address needs a nonzero port".into()).into());
    }
    if config.device.is_empty()
        || config.device.len() > 32
        || !config
            .device
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(Usage("IO-Link device alias must be 1–32 ASCII letters, digits or underscores, for example master1port1".into()).into());
    }
    let base = config.base_path.trim_end_matches('/');
    if !config.base_path.starts_with('/')
        || config.base_path.starts_with("//")
        || config.base_path.len() > 1024
        || !config
            .base_path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte))
        || base.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(Usage("IO-Link base_path must be an absolute ASCII path without query, fragment or dot segments".into()).into());
    }
    if config.host.as_ref().is_some_and(|host| {
        host.is_empty()
            || host.len() > 255
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte))
    }) {
        return Err(Usage(
            "IO-Link host must be an HTTP host authority without credentials or a path".into(),
        )
        .into());
    }
    ifm_iot::validate_signals(&config.signal, &config.signals)?;
    if !config.signals.is_empty() && matches!(config.format, Format::Iodd) {
        return Err(Usage(
            "IO-Link byte mappings require format byte_array; use json_signals for iodd values"
                .into(),
        )
        .into());
    }
    if config.signals.len() + config.json_signals.len() > 256 {
        return Err(Usage("IO-Link accepts at most 256 mapped signals".into()).into());
    }
    let mut names = BTreeSet::from([config.signal.as_str()]);
    names.extend(config.signals.iter().map(|signal| signal.name.as_str()));
    for mapping in &config.json_signals {
        super::validate_name(&mapping.name, "IO-Link JSON signal")?;
        if !names.insert(&mapping.name) {
            return Err(Usage("IO-Link signal names must be unique".into()).into());
        }
        validate_pointer(&mapping.pointer)?;
    }
    if config.username_env.is_some() != config.password_env.is_some()
        || (config.username_env.is_some() && config.bearer_token_env.is_some())
    {
        return Err(Usage("IO-Link HTTP authentication needs a username/password environment pair or one bearer_token_env".into()).into());
    }
    for name in config
        .username_env
        .iter()
        .chain(&config.password_env)
        .chain(&config.bearer_token_env)
    {
        if name.is_empty() || name.len() > 256 || name.contains(['=', '\0']) {
            return Err(
                Usage("invalid IO-Link credential environment variable name".into()).into(),
            );
        }
    }
    Ok(Box::new(Capture {
        config: config.clone(),
        tls: net::prepare(config.tls.as_ref())?,
        target: format!(
            "{base}/devices/{}/processdata/getdata/value?format={}",
            config.device,
            config.format.parameter()
        ),
        observed: None,
        ready: false,
    }))
}
fn validate_pointer(pointer: &str) -> Result<()> {
    if pointer.len() > 1024
        || (!pointer.is_empty() && !pointer.starts_with('/'))
        || pointer.chars().any(char::is_control)
    {
        return Err(Usage(
            "IO-Link JSON signal pointer must be RFC 6901 text, at most 1024 bytes".into(),
        )
        .into());
    }
    let mut chars = pointer.chars();
    while let Some(value) = chars.next() {
        if value == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return Err(
                Usage("IO-Link JSON pointers escape '~' as ~0 and '/' as ~1".into()).into(),
            );
        }
    }
    Ok(())
}

struct Capture {
    config: Config,
    tls: net::Prepared,
    target: String,
    observed: Option<u64>,
    ready: bool,
}
impl Source for Capture {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("IO-Link read timeout is too large".into()))?;
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "IO-Link HTTP deadline expired",
            ));
        }
        let host = self
            .config
            .host
            .clone()
            .unwrap_or_else(|| self.config.address.to_string());
        let mut request = Zeroizing::new(format!(
            "GET {} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
            self.target
        ));
        authorization(&self.config, &mut request)?;
        request.push_str("\r\n");
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "IO-Link HTTP deadline expired",
            ));
        }
        let socket = TcpStream::connect_timeout(&self.config.address, left)
            .map_err(|error| http::io_error(&error))?;
        let mut stream = self.tls.connect(socket, deadline)?;
        stream
            .write_all(request.as_bytes())
            .map_err(|error| http::io_error(&error))?;
        stream.flush().map_err(|error| http::io_error(&error))?;
        let body = http::response(&mut stream, MAX_BODY).map_err(|error| {
            error
                .downcast_ref::<http::Status>()
                .map(|status| http_status(status.0))
                .unwrap_or(error)
        })?;
        let observed = crate::host::monotonic_ns();
        let document: Value = serde_json::from_slice(&body)
            .map_err(|_| incomplete("invalid JSON process-data response"))?;
        let object = document
            .as_object()
            .ok_or_else(|| incomplete("process-data response must be an object"))?;
        let payload = if let Some(iolink) = object.get("iolink") {
            if iolink.get("valid").and_then(Value::as_bool) != Some(true) {
                return Err(incomplete(
                    "IO-Link master reports missing or invalid process data",
                ));
            }
            let value = iolink
                .get("value")
                .ok_or_else(|| incomplete("IO-Link process value is missing"))?;
            match self.config.format {
                Format::ByteArray => Some(byte_array(value)?),
                Format::Iodd if value.is_null() => {
                    return Err(incomplete("IODD process value is null"));
                }
                Format::Iodd => None,
            }
        } else {
            if !(object.get("cqValue").is_some_and(Value::is_boolean)
                || object.get("iqValue").is_some_and(Value::is_boolean))
            {
                return Err(incomplete(
                    "process data omits IO-Link and digital input values",
                ));
            }
            None
        };
        for key in ["cqValue", "iqValue"] {
            if object.get(key).is_some_and(|value| !value.is_boolean()) {
                return Err(incomplete("digital input values must be booleans"));
            }
        }
        let mut budget = SizeLimit(2);
        count_reading(
            &mut budget,
            &self.config.signal,
            RawResponse {
                kind: "iolink_json_process_data",
                device: &self.config.device,
                response: &document,
            },
        )?;
        let mut readings =
            Vec::with_capacity(1 + self.config.signals.len() + self.config.json_signals.len());
        for signal in &self.config.signals {
            let reading = signal.decode(
                payload
                    .as_deref()
                    .ok_or_else(|| incomplete("byte mappings require IO-Link process bytes"))?,
            )?;
            count_reading(&mut budget, &reading.signal, &reading.value)?;
            readings.push(reading);
        }
        for signal in &self.config.json_signals {
            let value = document.pointer(&signal.pointer).ok_or_else(|| {
                incomplete("configured JSON signal pointer is missing from process data")
            })?;
            // Count the borrowed subtree before cloning it: repeated selections
            // cannot allocate hundreds of response-sized values before failing.
            count_reading(&mut budget, &signal.name, value)?;
            readings.push(Reading {
                signal: signal.name.clone(),
                source_time: None,
                value: value.clone(),
            });
        }
        let mut raw = json!({"kind":"iolink_json_process_data", "device":self.config.device});
        raw.as_object_mut()
            .expect("raw response is an object")
            .insert("response".into(), document);
        readings.insert(
            0,
            Reading {
                signal: self.config.signal.clone(),
                source_time: None,
                value: raw,
            },
        );
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
fn byte_array(value: &Value) -> Result<Vec<u8>> {
    let bytes = value
        .as_array()
        .filter(|bytes| bytes.len() <= 32)
        .ok_or_else(|| incomplete("IO-Link byteArray must be an array of at most 32 bytes"))?;
    bytes
        .iter()
        .map(|byte| {
            byte.as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .ok_or_else(|| incomplete("IO-Link process byte must be an integer from 0 to 255"))
        })
        .collect()
}
fn authorization(config: &Config, request: &mut String) -> Result<()> {
    if let (Some(user), Some(password)) = (&config.username_env, &config.password_env) {
        let user = secret(user)?;
        if user.contains(':') {
            return Err(Usage("HTTP Basic username must not contain a colon".into()).into());
        }
        let password = secret(password)?;
        let text = Zeroizing::new(format!("{}:{}", *user, *password));
        let encoded = Zeroizing::new(STANDARD.encode(text.as_bytes()));
        request.push_str("Authorization: Basic ");
        request.push_str(&encoded);
        request.push_str("\r\n");
    } else if let Some(name) = &config.bearer_token_env {
        let token = secret(name)?;
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(Usage(
                "HTTP bearer token must be nonempty ASCII without spaces or control characters"
                    .into(),
            )
            .into());
        }
        request.push_str("Authorization: Bearer ");
        request.push_str(&token);
        request.push_str("\r\n");
    }
    Ok(())
}
fn secret(name: &str) -> Result<Zeroizing<String>> {
    let value = std::env::var(name).map_err(|_| {
        Categorized::error(
            ErrorKind::PermissionDenied,
            format!("IO-Link credential variable {name} is missing or not UTF-8"),
        )
    })?;
    if value.len() > 4096 {
        return Err(Usage("IO-Link HTTP credential exceeds 4096 bytes".into()).into());
    }
    Ok(Zeroizing::new(value))
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("iolink_json: {message}"))
}
fn http_status(code: u16) -> anyhow::Error {
    let kind = match code {
        401 => ErrorKind::AuthRejected,
        403 => ErrorKind::PermissionDenied,
        404 | 429 | 503 => ErrorKind::Unavailable,
        300..=399 => ErrorKind::Unsupported,
        _ => ErrorKind::OperationFailed,
    };
    Categorized::new(kind, "IO-Link HTTP server rejected process-data read")
        .details(json!({"http_status":code}))
        .into()
}
#[derive(Serialize)]
struct BorrowedReading<'a, T> {
    signal: &'a str,
    value: T,
}
#[derive(Serialize)]
struct RawResponse<'a> {
    kind: &'static str,
    device: &'a str,
    response: &'a Value,
}
fn count_reading(budget: &mut SizeLimit, signal: &str, value: impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *budget, &BorrowedReading { signal, value })
        .map_err(|_| incomplete("serialized process-data batch exceeds 1 MiB"))?;
    budget
        .write_all(b",")
        .map_err(|_| incomplete("serialized process-data batch exceeds 1 MiB"))
}

struct SizeLimit(usize);
impl io::Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_BODY {
            return Err(io::Error::other("batch limit exceeded"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
