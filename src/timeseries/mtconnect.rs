//! GET-only native `MTConnect` observation capture; no condition-state reconstruction.
use super::{Reading, Source, http, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    io::{self, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

mod auth;
#[cfg(test)]
mod tests;
mod xml;

const MAX_BODY: usize = 256 * 1024;
const MAX_RAW: usize = 768 * 1024;
const MAX_BATCH: usize = (1 << 20) - 16 * 1024;
const MAX_REQUEST: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Sample,
    Current,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StartKind {
    #[default]
    Current,
    BufferBegin,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resume {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    instance_id: u64,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    sequence: u64,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Start {
    Named(Box<StartKind>),
    Resume(Box<Resume>),
}
impl Default for Start {
    fn default() -> Self {
        Self::Named(Box::default())
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthKind {
    #[default]
    None,
    Basic,
    Bearer,
    Digest,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    #[serde(default)]
    base_path: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    tls: Option<net::TlsConfig>,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    start: Start,
    #[serde(default = "count", deserialize_with = "crate::config_number::unsigned")]
    count: u16,
    #[serde(default = "signal")]
    signal: String,
    #[serde(default)]
    auth: AuthKind,
    #[serde(default)]
    username_env: Option<String>,
    #[serde(default)]
    password_env: Option<String>,
    #[serde(default)]
    bearer_token_env: Option<String>,
}
const fn count() -> u16 {
    256
}
fn signal() -> String {
    "observations".into()
}
impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.address.port() == 0
            || self.address.ip().is_unspecified()
            || self.address.ip().is_multicast()
            || self.address.ip() == IpAddr::V4(Ipv4Addr::BROADCAST)
        {
            return Err(Usage(
                "MTConnect needs an explicit numeric unicast endpoint and nonzero port".into(),
            )
            .into());
        }
        let base = self.base_path.trim_end_matches('/');
        if self.base_path.len() > 1024
            || (!self.base_path.is_empty() && !self.base_path.starts_with('/'))
            || self.base_path.starts_with("//")
            || !self
                .base_path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-._~".contains(&b))
            || base.split('/').any(|p| matches!(p, "." | ".."))
        {
            return Err(Usage("MTConnect base_path must be a bounded absolute ASCII path without query, fragment, escapes or dot segments".into()).into());
        }
        if self.host.as_ref().is_some_and(|host| !valid_host(host)) {
            return Err(Usage("MTConnect host must be a bounded DNS/IPv4 or bracketed IPv6 authority with optional nonzero port".into()).into());
        }
        super::validate_name(&self.signal, "MTConnect signal")?;
        if !(1..=1024).contains(&self.count)
            || (self.mode == Mode::Current
                && !matches!(&self.start, Start::Named(kind) if **kind == StartKind::Current))
            || matches!(&self.start, Start::Resume(r) if r.sequence == 0)
        {
            return Err(Usage("MTConnect requires count 1..1024, positive resume sequence, and current-mode start current".into()).into());
        }
        let pair = self.username_env.is_some() && self.password_env.is_some();
        let credentials = match self.auth {
            AuthKind::None => {
                !pair
                    && self.username_env.is_none()
                    && self.password_env.is_none()
                    && self.bearer_token_env.is_none()
            }
            AuthKind::Basic | AuthKind::Digest => pair && self.bearer_token_env.is_none(),
            AuthKind::Bearer => {
                self.bearer_token_env.is_some()
                    && self.username_env.is_none()
                    && self.password_env.is_none()
            }
        };
        if !credentials
            || (matches!(self.auth, AuthKind::Basic | AuthKind::Bearer) && self.tls.is_none())
        {
            return Err(Usage("MTConnect auth needs only its explicit credential environment fields; Basic/Bearer require verified TLS".into()).into());
        }
        for name in self
            .username_env
            .iter()
            .chain(&self.password_env)
            .chain(&self.bearer_token_env)
        {
            if name.is_empty()
                || name.len() > 256
                || name.contains('=')
                || name.chars().any(char::is_control)
            {
                return Err(Usage("invalid MTConnect credential environment name".into()).into());
            }
        }
        #[cfg(not(feature = "mqtt-tls"))]
        if self.tls.is_some() {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "MTConnect TLS requires the mqtt-tls feature",
            ));
        }
        Ok(())
    }
}
fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 255 {
        return false;
    }
    let port = |value: &str| {
        !value.is_empty()
            && value.bytes().all(|b| b.is_ascii_digit())
            && value.parse::<u16>().is_ok_and(|n| n != 0)
    };
    if let Some(tail) = host.strip_prefix('[') {
        let Some((address, suffix)) = tail.split_once(']') else {
            return false;
        };
        return address.parse::<std::net::Ipv6Addr>().is_ok()
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(port));
    }
    let (name, suffix) = host
        .split_once(':')
        .map_or((host, None), |(name, value)| (name, Some(value)));
    if name.is_empty() || suffix.is_some_and(|p| !port(p)) {
        return false;
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    Ok(Box::new(Reader {
        config: config.clone(),
        tls: net::prepare(config.tls.as_ref())?,
        auth: auth::Session::new(config)?,
        state: None,
        failure: None,
        observed: None,
        ready: false,
    }))
}
struct Reader {
    config: Config,
    tls: net::Prepared,
    auth: auth::Session,
    state: Option<State>,
    failure: Option<Arc<anyhow::Error>>,
    observed: Option<u64>,
    ready: bool,
}
struct State {
    instance: u64,
    version: String,
    fingerprint: String,
    model_change: Option<String>,
    next: u64,
}
#[derive(Debug)]
struct Failed(Arc<anyhow::Error>);
impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MTConnect source retired after its failed poll")
    }
}
impl std::error::Error for Failed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("MTConnect: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "MTConnect original poll deadline expired",
        ));
    }
    Ok(remaining)
}
fn clock() -> Result<u64> {
    crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(ErrorKind::Unavailable, "MTConnect host clock unavailable")
    })
}
#[derive(Serialize)]
struct Window {
    start: u64,
    end: u64,
}
struct Response {
    path: String,
    body: String,
    sha: String,
    window: Window,
}
#[derive(Default)]
struct Work {
    raw: usize,
    requests: usize,
    nodes: usize,
}
impl Work {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.raw = self
            .raw
            .checked_add(bytes)
            .filter(|n| *n <= MAX_RAW)
            .ok_or_else(|| incomplete("cumulative response budget exceeded"))?;
        Ok(())
    }
}
#[derive(Serialize)]
struct Document<'a, 'input> {
    request_path: &'a str,
    header_raw: xml::Attributes<'a, 'input>,
    raw_response_xml: &'a str,
    response_sha256: &'a str,
    host_window_basis: &'static str,
    host_window_ns: &'a Window,
    observations: &'a [xml::Observation<'a, 'input>],
}
impl<'a, 'input> Document<'a, 'input> {
    fn new(
        response: &'a Response,
        header: xml::Header<'a, 'input>,
        observations: &'a [xml::Observation<'a, 'input>],
    ) -> Self {
        Self {
            request_path: &response.path,
            header_raw: xml::Attributes(header.node),
            raw_response_xml: &response.body,
            response_sha256: &response.sha,
            host_window_basis: "endpoint_start_through_body_received_including_auth",
            host_window_ns: &response.window,
            observations,
        }
    }
}
#[derive(Serialize)]
struct Record<'a, 'input> {
    kind: &'static str,
    mode: Mode,
    phase: &'static str,
    namespace_version: &'a str,
    configured_count: u16,
    effective_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<u64>,
    next: u64,
    catalogue_fingerprint: &'a str,
    catalogue: &'a [xml::Item<'a, 'input>],
    probe: Document<'a, 'input>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<Document<'a, 'input>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Document<'a, 'input>>,
}
impl Reader {
    fn fetch(&mut self, endpoint: &str, deadline: Instant, work: &mut Work) -> Result<Response> {
        let path = format!("{}{endpoint}", self.config.base_path.trim_end_matches('/'));
        let start = clock()?;
        for attempt in 0..2 {
            remaining(deadline)?;
            work.requests += 1;
            if work.requests > 6 {
                return Err(incomplete("HTTP attempt limit exceeded"));
            }
            let authorization = self.auth.authorization(&path)?;
            let host = self
                .config
                .host
                .clone()
                .unwrap_or_else(|| self.config.address.to_string());
            let mut request = Zeroizing::new(format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/xml, text/xml\r\nAccept-Encoding: identity\r\nConnection: close\r\n"
            ));
            if let Some(value) = &authorization {
                request.push_str("Authorization: ");
                request.push_str(value);
                request.push_str("\r\n");
            }
            request.push_str("\r\n");
            if request.len() > MAX_REQUEST {
                return Err(incomplete("HTTP request budget exceeded"));
            }
            let socket = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
                .map_err(|e| http::io_error(&e))?;
            let mut transport = self.tls.connect(socket, deadline)?;
            transport
                .write_all(request.as_bytes())
                .map_err(|e| http::io_error(&e))?;
            transport.flush().map_err(|e| http::io_error(&e))?;
            let mut response = http::response_with_headers(&mut transport, MAX_BODY)?;
            let end = clock()?;
            remaining(deadline)?;
            work.charge(response.body.len())?;
            if response.status == 401 && self.config.auth == AuthKind::Digest && attempt == 0 {
                self.auth.challenge(&response.headers)?;
                continue;
            }
            if response.status == 401 {
                return Err(Categorized::new(
                    ErrorKind::AuthRejected,
                    "MTConnect HTTP authentication rejected",
                )
                .details(serde_json::json!({"http_status":response.status}))
                .into());
            }
            if response.status == 403 {
                return Err(Categorized::new(
                    ErrorKind::PermissionDenied,
                    "MTConnect HTTP read forbidden",
                )
                .details(serde_json::json!({"http_status":403}))
                .into());
            }
            if response.status != 200 {
                let error = std::str::from_utf8(&response.body).map_or_else(
                    |_| Ok(xml::http_failure(response.status)),
                    |body| xml::remote_error(response.status, body, work),
                )?;
                return Err(error);
            }
            xml::media_type(&response.headers)?;
            let body = String::from_utf8(std::mem::take(&mut response.body))
                .map_err(|_| incomplete("XML is not UTF-8"))?;
            let sha = crate::types::hex(Sha256::digest(body.as_bytes()));
            remaining(deadline)?;
            return Ok(Response {
                path,
                body,
                sha,
                window: Window { start, end },
            });
        }
        Err(Categorized::error(
            ErrorKind::AuthRejected,
            "MTConnect Digest rechallenge limit exceeded",
        ))
    }
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let mut work = Work::default();
        let probe_response = self.fetch("/probe", deadline, &mut work)?;
        let probe_document = xml::document(&probe_response.body, &mut work)?;
        let model = xml::probe(&probe_document)?;
        let fingerprint = {
            let mut hash = Hash {
                digest: Sha256::new(),
                size: Size(0),
                deadline,
            };
            serde_json::to_writer(&mut hash, &model.fingerprint).map_err(|_| {
                remaining(deadline).err().unwrap_or_else(|| {
                    incomplete("canonical model exceeds its serialized work budget")
                })
            })?;
            crate::types::hex(hash.digest.finalize())
        };
        if let Some(old) = &self.state
            && (old.instance != model.header.instance
                || old.version != model.version
                || old.fingerprint != fingerprint
                || old.model_change.as_deref()
                    != model.header.node.attribute("deviceModelChangeTime"))
        {
            return Err(incomplete(
                "agent instance, namespace or device model changed",
            ));
        }
        let current_response = self.fetch("/current", deadline, &mut work)?;
        let current_document = xml::document(&current_response.body, &mut work)?;
        let current = xml::streams(&current_document, &model, None)?;
        let current_next = current
            .header
            .next
            .ok_or_else(|| incomplete("current nextSequence is missing"))?;
        let current_first = current
            .header
            .first
            .ok_or_else(|| incomplete("current firstSequence is missing"))?;
        let current_last = current
            .header
            .last
            .ok_or_else(|| incomplete("current lastSequence is missing"))?;
        if current_last.checked_add(1) != Some(current_next) {
            return Err(incomplete(
                "current nextSequence does not follow the newest assigned sequence",
            ));
        }
        let model_change = current.header.node.attribute("deviceModelChangeTime");
        if model.header.node.attribute("deviceModelChangeTime") != model_change {
            return Err(incomplete(
                "current device model timestamp differs from probe",
            ));
        }

        if let Some(old) = &self.state
            && (old.instance != model.header.instance
                || old.version != model.version
                || old.fingerprint != fingerprint
                || old.model_change.as_deref() != model_change)
        {
            return Err(incomplete(
                "agent instance, namespace or device model changed",
            ));
        }
        let effective_count = u64::from(self.config.count).min(current.header.buffer);
        let mut from = None;
        let mut phase = if self.config.mode == Mode::Current {
            "current"
        } else {
            "baseline_current"
        };
        let mut next = current_next;
        let mut checkpoint = None;
        let mut data = Some(Document::new(
            &current_response,
            current.header,
            &current.observations,
        ));
        let mut sample_response = None;
        if self.config.mode == Mode::Sample {
            let cursor = self
                .state
                .as_ref()
                .map(|s| s.next)
                .or(match &self.config.start {
                    Start::Named(kind) => match **kind {
                        StartKind::Current => None,
                        StartKind::BufferBegin => Some(current_first),
                    },
                    Start::Resume(resume) => {
                        if resume.instance_id != model.header.instance {
                            return Err(incomplete("resume instance differs from agent instance"));
                        }
                        Some(resume.sequence)
                    }
                });
            if let Some(cursor) = cursor {
                from = Some(cursor);
                if cursor < current_first
                    || cursor
                        > current_last
                            .checked_add(1)
                            .ok_or_else(|| incomplete("agent sequence exhausted"))?
                {
                    return Err(Categorized::new(ErrorKind::Incomplete, "MTConnect retained buffer no longer contains requested cursor").details(serde_json::json!({"requested":cursor,"first_sequence":current_first,"last_sequence":current_last,"instance_id":model.header.instance})).into());
                }
                checkpoint = Some(Document::new(&current_response, current.header, &[]));
                data = None;
                next = cursor;
                if cursor > current_last {
                    phase = "caught_up";
                } else {
                    phase = "sample";
                    sample_response = Some(self.fetch(
                        &format!("/sample?from={cursor}&count={effective_count}"),
                        deadline,
                        &mut work,
                    )?);
                }
            }
        }
        let sample_document = sample_response
            .as_ref()
            .map(|r| xml::document(&r.body, &mut work))
            .transpose()?;
        let sample = sample_document
            .as_ref()
            .map(|doc| xml::streams(doc, &model, from.map(|cursor| (cursor, effective_count))))
            .transpose()?;
        if let (Some(response), Some(sample)) = (&sample_response, &sample) {
            if sample.header.node.attribute("deviceModelChangeTime") != model_change {
                return Err(incomplete("sample device model changed"));
            }
            next = sample
                .header
                .next
                .ok_or_else(|| incomplete("sample nextSequence is missing"))?;
            data = Some(Document::new(response, sample.header, &sample.observations));
        }
        remaining(deadline)?;
        let record = Record {
            kind: "mtconnect",
            mode: self.config.mode,
            phase,
            namespace_version: model.version,
            configured_count: self.config.count,
            effective_count,
            from,
            next,
            catalogue_fingerprint: &fingerprint,
            catalogue: &model.items,
            probe: Document::new(&probe_response, model.header, &[]),
            checkpoint,
            data,
        };
        #[derive(Serialize)]
        struct Borrowed<'a, T> {
            signal: &'a str,
            value: T,
        }
        serde_json::to_writer(
            Size(2),
            &Borrowed {
                signal: &self.config.signal,
                value: &record,
            },
        )
        .map_err(|_| incomplete("escaped Reading exceeds 1 MiB with wrapper reserve"))?;
        remaining(deadline)?;
        let readings = vec![Reading {
            signal: self.config.signal.clone(),
            value: serde_json::to_value(record)?,
            source_time: None,
        }];
        serde_json::to_writer(Size(0), &readings)
            .map_err(|_| incomplete("Reading batch limit exceeded"))?;
        remaining(deadline)?;
        self.observed = Some(clock()?);
        remaining(deadline)?;
        self.state = Some(State {
            instance: model.header.instance,
            version: model.version.into(),
            fingerprint,
            model_change: model_change.map(str::to_owned),
            next,
        });
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
            .ok_or_else(|| Usage("MTConnect timeout is too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        result.map_err(|mut error| {
            self.auth.redact_error(&mut error);
            let error = Arc::new(error);
            self.failure = Some(error.clone());
            self.auth.clear();
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
            return Err(io::Error::other("MTConnect Reading limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Hash {
    digest: Sha256,
    size: Size,
    deadline: Instant,
}
impl io::Write for Hash {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "MTConnect model deadline expired",
            ));
        }
        self.size.write_all(bytes)?;
        self.digest.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
