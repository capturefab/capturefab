//! GET-only ABB RWS state capture with explicit API generation and original deadlines.
use super::{Reading, Source, http, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::{SocketAddr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

mod auth;
mod parser;
#[cfg(test)]
mod tests;

const MAX_BODY: usize = 256 * 1024;
const MAX_BATCH: usize = (1 << 20) - 16 * 1024;
const MAX_RAW: usize = 1 << 20;
const MAX_REQUEST: usize = 16 * 1024;
const PACE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Api {
    Rws1,
    Rws2,
}
impl Api {
    const fn accept(self) -> &'static str {
        match self {
            Self::Rws1 => "application/xhtml+xml",
            Self::Rws2 => "application/xhtml+xml;v=2.0",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Resource {
    ControllerState,
    OperationMode,
    SystemInfo,
    RapidExecution,
    RapidSymbol,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    signal: String,
    resource: Resource,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
}
impl Selection {
    fn symbol_url(&self) -> Option<String> {
        Some(format!(
            "RAPID/{}/{}/{}",
            self.task.as_ref()?,
            self.module.as_ref()?,
            self.symbol.as_ref()?
        ))
    }
    fn path(&self, api: Api, properties: bool) -> String {
        match self.resource {
            Resource::ControllerState => match api {
                Api::Rws1 => "/rw/panel/ctrlstate".into(),
                Api::Rws2 => "/rw/panel/ctrl-state".into(),
            },
            Resource::OperationMode => "/rw/panel/opmode".into(),
            Resource::SystemInfo => "/rw/system".into(),
            Resource::RapidExecution => "/rw/rapid/execution".into(),
            Resource::RapidSymbol => {
                let parts: Vec<_> = [&self.task, &self.module, &self.symbol]
                    .into_iter()
                    .flatten()
                    .map(|part| segment(part))
                    .collect();
                let symbol = format!("RAPID/{}", parts.join("/"));
                let kind = if properties { "properties" } else { "data" };
                match api {
                    Api::Rws1 => format!("/rw/rapid/symbol/{kind}/{symbol}"),
                    Api::Rws2 => format!("/rw/rapid/symbol/{symbol}/{kind}"),
                }
            }
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    api: Api,
    address: SocketAddr,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    tls: Option<net::TlsConfig>,
    username_env: String,
    password_env: String,
    reads: Vec<Selection>,
}
impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.address.is_ipv4()
            || self.address.port() == 0
            || self.address.ip().is_unspecified()
            || self.address.ip().is_multicast()
            || self.address.ip() == std::net::Ipv4Addr::BROADCAST
        {
            return Err(Usage(
                "ABB RWS requires an explicit numeric unicast IPv4 endpoint and nonzero port"
                    .into(),
            )
            .into());
        }
        if self.api == Api::Rws2 && self.tls.is_none() {
            return Err(Usage("ABB RWS2 requires verified TLS configuration".into()).into());
        }
        #[cfg(not(feature = "mqtt-tls"))]
        if self.tls.is_some() {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "ABB RWS TLS requires the mqtt-tls build feature",
            ));
        }
        if self.host.as_ref().is_some_and(|host| !valid_host(host)) {
            return Err(Usage("ABB RWS host must be a bounded ASCII hostname or IPv4 authority without userinfo, path or query".into()).into());
        }
        for name in [&self.username_env, &self.password_env] {
            if name.is_empty()
                || name.len() > 256
                || name.chars().any(char::is_control)
                || name.contains('=')
            {
                return Err(
                    Usage("invalid ABB credential environment variable name".into()).into(),
                );
            }
        }
        if self.reads.is_empty() || self.reads.len() > 32 {
            return Err(Usage("ABB RWS requires 1..32 selected reads".into()).into());
        }
        let mut signals = BTreeSet::new();
        for read in &self.reads {
            super::validate_name(&read.signal, "ABB RWS signal")?;
            if !signals.insert(&read.signal) {
                return Err(Usage("ABB RWS signal names must be unique".into()).into());
            }
            let parts = [&read.task, &read.module, &read.symbol];
            if read.resource == Resource::RapidSymbol {
                for part in parts {
                    let part = part.as_deref().ok_or_else(|| {
                        Usage("ABB RAPID symbols require task, module and symbol".into())
                    })?;
                    if part.is_empty()
                        || part.len() > 128
                        || matches!(part, "." | "..")
                        || part.chars().any(char::is_control)
                        || part.contains(['/', '\\', '?', '#', '[', ']'])
                    {
                        return Err(Usage("ABB symbol identifiers must be 1..128 UTF-8 bytes without path/query separators, array selectors or dot segments".into()).into());
                    }
                }
            } else if parts.iter().any(|part| part.is_some()) {
                return Err(
                    Usage("ABB task/module/symbol apply only to rapid_symbol".into()).into(),
                );
            }
        }
        Ok(())
    }
}
fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 255 {
        return false;
    }
    let (name, port) = host
        .split_once(':')
        .map_or((host, None), |(name, port)| (name, Some(port)));
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
        && port.is_none_or(|port| {
            !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|p| p != 0)
        })
}
fn segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}
pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    Ok(Box::new(Reader {
        config: config.clone(),
        tls: net::prepare(config.tls.as_ref())?,
        session: auth::Session::new(&config.username_env, &config.password_env)?,
        last_send: None,
        failure: None,
        observed: None,
        ready: false,
    }))
}
struct Reader {
    config: Config,
    tls: net::Prepared,
    session: auth::Session,
    last_send: Option<Instant>,
    failure: Option<Arc<anyhow::Error>>,
    observed: Option<u64>,
    ready: bool,
}
#[derive(Debug)]
struct Failed(Arc<anyhow::Error>);
impl std::fmt::Display for Failed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ABB RWS source retired after its failed poll")
    }
}
impl std::error::Error for Failed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "ABB RWS original poll deadline expired",
        ));
    }
    Ok(left)
}
fn clock() -> Result<u64> {
    crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(
            ErrorKind::Unavailable,
            "ABB RWS host receive clock is unavailable",
        )
    })
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("ABB RWS: {message}"))
}
#[derive(Serialize)]
struct Window {
    start: u64,
    end: u64,
}
#[derive(Serialize)]
struct Observation {
    host_window_basis: &'static str,
    request_path: String,
    fields: Value,
    raw_response_xml: String,
    response_sha256: String,
    host_window_ns: Window,
}
#[derive(Serialize)]
struct Symbol<'a> {
    properties: &'a Observation,
    value: &'a Observation,
}
#[derive(Serialize)]
struct Record<'a> {
    kind: &'static str,
    api: Api,
    resource: Resource,
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<&'a Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observations: Option<Symbol<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    datatype_provenance: Option<&'static str>,
}
impl Reader {
    fn exchange(
        &mut self,
        path: &str,
        authorization: Option<&str>,
        deadline: Instant,
    ) -> Result<http::Response> {
        remaining(deadline)?;
        if let Some(last) = self.last_send {
            let next = last + PACE;
            if next >= deadline {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "ABB RWS deadline cannot admit its next paced GET",
                ));
            }
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        remaining(deadline)?;
        let host = self
            .config
            .host
            .as_deref()
            .map_or_else(|| self.config.address.to_string(), str::to_owned);
        let cookies = self.session.cookies.header(path, self.config.tls.is_some());
        let base = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
            self.config.api.accept()
        );
        let total = base.len() + cookies.len() + authorization.map_or(0, str::len) + 64;
        if total > MAX_REQUEST {
            return Err(incomplete("outgoing GET headers exceed 16 KiB"));
        }
        let mut request = Zeroizing::new(base);
        if !cookies.is_empty() {
            request.push_str("Cookie: ");
            request.push_str(&cookies);
            request.push_str("\r\n");
        }
        if let Some(authorization) = authorization {
            request.push_str("Authorization: ");
            request.push_str(authorization);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        let socket = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
            .map_err(|e| http::io_error(&e))?;
        let mut stream = self.tls.connect(socket, deadline)?;
        remaining(deadline)?;
        self.last_send = Some(Instant::now());
        stream
            .write_all(request.as_bytes())
            .map_err(|e| http::io_error(&e))?;
        stream.flush().map_err(|e| http::io_error(&e))?;
        let response = http::response_with_headers(&mut stream, MAX_BODY)?;
        remaining(deadline)?;
        Ok(response)
    }
    fn fetch(
        &mut self,
        path: String,
        resource: Resource,
        properties: bool,
        symbol: Option<&str>,
        deadline: Instant,
        raw_bytes: &mut usize,
    ) -> Result<Observation> {
        let start = clock()?;
        let mut authorization = self.session.authorization(self.config.api, &path)?;
        for attempt in 0..2 {
            let mut response = self.exchange(
                &path,
                authorization.as_deref().map(String::as_str),
                deadline,
            )?;
            *raw_bytes = raw_bytes
                .checked_add(response.body.len())
                .filter(|size| *size <= MAX_RAW)
                .ok_or_else(|| incomplete("aggregate HTTP response bodies exceed 1 MiB"))?;
            let host = self.config.host.as_deref().map_or_else(
                || self.config.address.ip().to_string(),
                |host| host.split(':').next().unwrap_or(host).to_owned(),
            );
            let received = Instant::now();
            if response.status == 401 && attempt == 0 && self.config.api == Api::Rws2 {
                self.session.cookies.clear();
            }
            let cookies = response
                .headers
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .map(|(_, value)| value.as_str());
            let cookie_result = self.session.cookies.receive(
                cookies,
                &host,
                &path,
                self.config.tls.is_some(),
                received,
            );
            if response.status == 401 && attempt == 0 {
                cookie_result?;
                if self.config.api == Api::Rws1 {
                    self.session.challenge(&response.headers)?;
                }
                authorization = self.session.authorization(self.config.api, &path)?;
                continue;
            }
            let status = response.status;
            let media = response
                .headers
                .iter()
                .filter(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>();
            let media_result = parser::media_type(self.config.api, &media);
            cookie_result?;
            if status != 200 {
                return Err(parser::http_error(status, &response.body, |text| {
                    self.session
                        .redact(text, authorization.as_deref().map(String::as_str))
                }));
            }
            media_result?;
            let end = clock()?;
            let xml = String::from_utf8(std::mem::take(&mut response.body))
                .map_err(|_| incomplete("response is not UTF-8 XHTML"))?;
            let fields = parser::parse(self.config.api, resource, properties, symbol, &xml)?;
            remaining(deadline)?;
            return Ok(Observation {
                host_window_basis: "endpoint_start_through_body_received_including_pacing_and_auth",
                request_path: path,
                fields,
                response_sha256: format!("{:x}", Sha256::digest(xml.as_bytes())),
                raw_response_xml: xml,
                host_window_ns: Window { start, end },
            });
        }
        Err(Categorized::error(
            ErrorKind::AuthRejected,
            "ABB RWS authentication was refused",
        ))
    }
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let mut raw_bytes = 0;
        let mut budget = Size(2);
        let mut readings = Vec::with_capacity(self.config.reads.len());
        for index in 0..self.config.reads.len() {
            remaining(deadline)?;
            let read = &self.config.reads[index];
            let signal = read.signal.clone();
            let resource = read.resource;
            let symbol = read.symbol_url();
            let value_path = read.path(self.config.api, false);
            let properties_path = read.path(self.config.api, true);
            let properties = if resource == Resource::RapidSymbol {
                Some(self.fetch(
                    properties_path,
                    resource,
                    true,
                    symbol.as_deref(),
                    deadline,
                    &mut raw_bytes,
                )?)
            } else {
                None
            };
            let value = self.fetch(
                value_path,
                resource,
                false,
                symbol.as_deref(),
                deadline,
                &mut raw_bytes,
            )?;
            let record = Record {
                kind: "abb_rws",
                api: self.config.api,
                resource,
                observation: properties.is_none().then_some(&value),
                observations: properties.as_ref().map(|properties| Symbol {
                    properties,
                    value: &value,
                }),
                datatype_provenance: properties.as_ref().map(|_| "separate_properties_request"),
            };
            #[derive(Serialize)]
            struct Borrowed<'a, T> {
                signal: &'a str,
                value: T,
            }
            serde_json::to_writer(
                &mut budget,
                &Borrowed {
                    signal: &signal,
                    value: &record,
                },
            )
            .map_err(|_| incomplete("escaped Reading batch exceeds 1 MiB with wrapper reserve"))?;
            budget
                .write_all(b",")
                .map_err(|_| incomplete("Reading batch limit exceeded"))?;
            readings.push(Reading {
                signal,
                value: serde_json::to_value(record)?,
                source_time: None,
            });
        }
        serde_json::to_writer(Size(0), &readings)
            .map_err(|_| incomplete("Reading batch limit exceeded"))?;
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
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("ABB RWS timeout is too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        result.map_err(|error| {
            let error = Arc::new(error);
            self.failure = Some(error.clone());
            self.session.clear();
            Failed(error).into()
        })
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn ready(&self) -> bool {
        self.ready
    }
}
struct Size(usize);
impl io::Write for Size {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_BATCH {
            return Err(io::Error::other("ABB Reading limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
