//! Bounded provider HTTP. Requests and authentication deliberately have no
//! Debug/Clone/Serialize implementation; only scrubbed errors leave here.
use super::credentials::Auth;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
pub type Headers = BTreeMap<String, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Auth,
    Rejected,
    NotFound,
    RateLimited,
    Unavailable,
    Timeout,
    Unreachable,
    CutOff,
    Malformed,
}

#[derive(Clone)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub status: Option<u16>,
    pub retry_after_s: Option<u64>,
    /// Only a scrubbed, bounded provider message; never part of Display.
    pub excerpt: Option<String>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind) -> Self {
        Self {
            kind,
            status: None,
            retry_after_s: None,
            excerpt: None,
        }
    }
    pub fn details(&self) -> Value {
        json!({"kind":self.kind,"status":self.status,"retry_after_s":self.retry_after_s,
            "provider_message":self.excerpt.as_deref().map(|text|scrub_text(text,&[]))})
    }
    pub fn exit(&self) -> (&'static str, i32) {
        match self.kind {
            ProviderErrorKind::Auth
            | ProviderErrorKind::Rejected
            | ProviderErrorKind::CutOff
            | ProviderErrorKind::Malformed => ("operation_failed", 1),
            ProviderErrorKind::NotFound
            | ProviderErrorKind::RateLimited
            | ProviderErrorKind::Unavailable
            | ProviderErrorKind::Unreachable => ("unavailable", 3),
            ProviderErrorKind::Timeout => ("timeout", 4),
        }
    }
}
impl fmt::Debug for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.details().fmt(f)
    }
}
impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.kind {
            ProviderErrorKind::Auth=>"The provider refused the credentials. Check the API key or cloud sign-in.",
            ProviderErrorKind::Rejected=>"The provider rejected the request. Check the model and provider settings.",
            ProviderErrorKind::NotFound=>"The provider could not find the model or endpoint. Check the model and provider settings.",
            ProviderErrorKind::RateLimited=>"The provider is limiting requests. Wait and try again, or check your quota.",
            ProviderErrorKind::Unavailable=>"The provider is temporarily unavailable. Try again later.",
            ProviderErrorKind::Timeout=>"The provider request timed out. Try again or choose a faster model.",
            ProviderErrorKind::Unreachable=>"The provider could not be reached. Check the connection and endpoint.",
            ProviderErrorKind::CutOff=>"The provider response was cut off or exceeded the 16 MiB limit.",
            ProviderErrorKind::Malformed=>"The provider returned an invalid response. Try again or choose another model.",
        })
    }
}
impl std::error::Error for ProviderError {}

pub struct HttpRequest<'a> {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Zeroizing<Vec<u8>>,
    pub auth: &'a Auth,
    pub timeout: Option<Duration>,
    pub direct: bool,
    // OAuth refresh tokens, client secrets and JWT assertions can appear in
    // the form body even when Auth is None. Keep them available for scrubbing.
    secrets: Vec<Zeroizing<String>>,
}

impl<'a> HttpRequest<'a> {
    pub fn json(
        url: impl Into<String>,
        body: &Value,
        auth: &'a Auth,
    ) -> Result<Self, ProviderError> {
        let mut request = Self::get(url, auth);
        request.method = "POST".into();
        request
            .headers
            .push(("content-type".into(), "application/json".into()));
        request.body = Zeroizing::new(
            serde_json::to_vec(body)
                .map_err(|_| ProviderError::new(ProviderErrorKind::Malformed))?,
        );
        Ok(request)
    }

    pub fn form(url: impl Into<String>, pairs: &[(&str, &str)], auth: &'a Auth) -> Self {
        let mut request = Self::get(url, auth);
        request.method = "POST".into();
        request.headers.push((
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        let mut body = Zeroizing::new(String::new());
        for (index, (key, value)) in pairs.iter().enumerate() {
            if index > 0 {
                body.push('&');
            }
            body.push_str(&crate::aws::uri_encode(key, true));
            body.push('=');
            body.push_str(&crate::aws::uri_encode(value, true));
            if matches!(
                *key,
                "refresh_token"
                    | "client_secret"
                    | "assertion"
                    | "password"
                    | "access_token"
                    | "id_token"
            ) {
                request.secrets.push(Zeroizing::new((*value).into()));
            }
        }
        request.body = Zeroizing::new(std::mem::take(&mut *body).into_bytes());
        request
    }

    pub fn get(url: impl Into<String>, auth: &'a Auth) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Zeroizing::new(Vec::new()),
            auth,
            timeout: None,
            direct: false,
            secrets: Vec::new(),
        }
    }

    fn known_secrets(&self) -> Vec<&str> {
        let mut values: Vec<_> = self.secrets.iter().map(|value| value.as_str()).collect();
        match self.auth {
            Auth::None => (),
            Auth::Header { value, .. } => {
                values.push(value.as_str());
                if let Some((_, token)) = value.split_once(' ') {
                    values.push(token);
                }
            }
            Auth::Aws { keys, .. } => {
                values.push(keys.id());
                values.push(keys.secret());
                if let Some(token) = keys.token() {
                    values.push(token);
                }
            }
        }
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values
    }
}

/// An untrusted response, which can contain reflected credentials. No Debug.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Headers,
    pub body: Vec<u8>,
}
pub trait Transport: Send + Sync {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError>;
}

type Waiter = dyn Fn(Duration, &AtomicBool) -> bool + Send + Sync;
pub struct Http {
    transport: Arc<dyn Transport>,
    wait: Arc<Waiter>,
}
impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}
impl Http {
    pub fn new() -> Self {
        Self::with_transport(Arc::new(NativeTransport::new()))
    }
    pub fn with_transport(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            wait: Arc::new(wait_cancel),
        }
    }

    /// Parse a JSON response to a POST, GET or OAuth form request. Keeping
    /// one request borrowed across retries preserves its exact signed body.
    pub fn post_json(
        &self,
        request: HttpRequest<'_>,
        cancel: &AtomicBool,
    ) -> Result<(Value, Headers), ProviderError> {
        self.send_json(request, cancel)
    }

    pub fn send_json(
        &self,
        request: HttpRequest<'_>,
        cancel: &AtomicBool,
    ) -> Result<(Value, Headers), ProviderError> {
        Address::parse(&request)?;
        let known = request.known_secrets();
        for attempt in 0..=2 {
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::new(ProviderErrorKind::Rejected));
            }
            let response = self.transport.send(&request).map_err(|mut error| {
                error.excerpt = error
                    .excerpt
                    .as_deref()
                    .map(|text| scrub_text(text, &known));
                error
            })?;
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::new(ProviderErrorKind::Rejected));
            }
            if response.body.len() > MAX_BODY_BYTES {
                return Err(ProviderError {
                    status: Some(response.status),
                    ..ProviderError::new(ProviderErrorKind::CutOff)
                });
            }
            let headers = safe_headers(response.headers, &known);
            let parsed = serde_json::from_slice::<Value>(&response.body);
            let failed =
                !(200..300).contains(&response.status) || parsed.as_ref().is_ok_and(error_envelope);
            if failed {
                let mut error = ProviderError::new(parsed.as_ref().map_or_else(
                    |_| status_kind(response.status),
                    |body| response_kind(response.status, body),
                ));
                error.status = Some(response.status);
                error.retry_after_s = retry_after(&headers);
                if let Ok(body) = &parsed {
                    error.excerpt = excerpt(body, &headers).map(|text| scrub_text(&text, &known));
                }
                if attempt < 2 && (response.status == 429 || (500..600).contains(&response.status))
                {
                    let delay = error
                        .retry_after_s
                        .map(Duration::from_secs)
                        .unwrap_or(Duration::from_millis(500 * (attempt + 1)));
                    if !(self.wait)(delay, cancel) {
                        return Err(ProviderError::new(ProviderErrorKind::Rejected));
                    }
                    continue;
                }
                return Err(error);
            }
            let value = parsed.map_err(|_| ProviderError {
                status: Some(response.status),
                ..ProviderError::new(ProviderErrorKind::Malformed)
            })?;
            return Ok((value, headers));
        }
        unreachable!("the final attempt returns its result")
    }
}

fn wait_cancel(duration: Duration, cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + duration;
    while !cancel.load(Ordering::Relaxed) {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return true;
        };
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    false
}

struct Address {
    uri: ureq::http::Uri,
    direct: bool,
}
impl Address {
    fn parse(request: &HttpRequest<'_>) -> Result<Self, ProviderError> {
        let reject = || ProviderError::new(ProviderErrorKind::Rejected);
        let uri: ureq::http::Uri = request.url.parse().map_err(|_| reject())?;
        let scheme = uri.scheme_str().ok_or_else(reject)?;
        let host = uri.host().ok_or_else(reject)?.trim_matches(['[', ']']);
        if !matches!(scheme, "https" | "http")
            || request.url.contains('#')
            || uri
                .authority()
                .is_some_and(|authority| authority.as_str().contains('@'))
            || crate::media::redact_url(&request.url) != request.url
            || !matches!(request.method.as_str(), "GET" | "POST")
        {
            return Err(reject());
        }
        // URL parsing does not decode query names. Refuse credential keys
        // even when a caller percent-encodes their spelling.
        if decode_query(uri.query().unwrap_or_default())?
            .iter()
            .any(|(name, _)| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "password"
                        | "passphrase"
                        | "passwd"
                        | "token"
                        | "key"
                        | "auth"
                        | "secret"
                        | "access_token"
                        | "streamid"
                )
            })
        {
            return Err(reject());
        }
        let ip = host.parse::<IpAddr>().ok();
        let loopback =
            host.eq_ignore_ascii_case("localhost") || ip.is_some_and(|ip| ip.is_loopback());
        let private = ip.is_some_and(|ip| match ip {
            IpAddr::V4(ip) => ip.is_private(),
            IpAddr::V6(ip) => (ip.segments()[0] & 0xfe00) == 0xfc00,
        });
        let metadata = host == "169.254.169.254";
        if scheme == "http"
            && (!loopback && !private && !metadata
                || !loopback && !matches!(request.auth, Auth::None))
        {
            return Err(reject());
        }
        let direct = request.direct || loopback || private || metadata || scheme == "http";
        Ok(Self { uri, direct })
    }
}

pub struct NativeTransport {
    cloud: ureq::Agent,
    direct: ureq::Agent,
}
impl Default for NativeTransport {
    fn default() -> Self {
        Self::new()
    }
}
impl NativeTransport {
    pub fn new() -> Self {
        fn agent(direct: bool) -> ureq::Agent {
            let tls = ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build();
            let mut config = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .https_only(!direct)
                .max_redirects(0)
                .timeout_connect(Some(Duration::from_secs(10)))
                .timeout_global(Some(Duration::from_secs(if direct { 600 } else { 120 })))
                .tls_config(tls);
            if direct {
                config = config.proxy(None);
            }
            config.build().new_agent()
        }
        Self {
            cloud: agent(false),
            direct: agent(true),
        }
    }
}

fn credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "x-goog-api-key"
            | "x-amz-security-token"
            | "cookie"
            | "set-cookie"
    )
}

fn add_header(
    headers: &mut ureq::http::HeaderMap,
    name: &str,
    value: &str,
    sensitive: bool,
) -> Result<(), ProviderError> {
    let name = ureq::http::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| ProviderError::new(ProviderErrorKind::Rejected))?;
    let mut value = ureq::http::HeaderValue::from_str(value)
        .map_err(|_| ProviderError::new(ProviderErrorKind::Auth))?;
    value.set_sensitive(sensitive);
    headers.insert(name, value);
    Ok(())
}

impl Transport for NativeTransport {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError> {
        let address = Address::parse(request)?;
        let agent = if address.direct {
            &self.direct
        } else {
            &self.cloud
        };
        let mut wire = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(address.uri.clone())
            .body(request.body.as_slice())
            .map_err(|_| ProviderError::new(ProviderErrorKind::Rejected))?;
        for (name, value) in &request.headers {
            if credential_header(name) || name.eq_ignore_ascii_case("host") {
                return Err(ProviderError::new(ProviderErrorKind::Rejected));
            }
            add_header(wire.headers_mut(), name, value, false)?;
        }
        match request.auth {
            Auth::None => (),
            Auth::Header {
                name,
                value,
                quota_project,
            } => {
                add_header(wire.headers_mut(), name, value, true)?;
                if let Some(project) = quota_project {
                    add_header(wire.headers_mut(), "x-goog-user-project", project, false)?;
                }
            }
            Auth::Aws { keys, region } => {
                let hash = format!("{:x}", Sha256::digest(request.body.as_slice()));
                let host = address
                    .uri
                    .authority()
                    .expect("validated authority")
                    .as_str();
                let date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                let query = decode_query(address.uri.query().unwrap_or_default())?;
                let query_refs: Vec<_> = query
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect();
                let signed: Vec<_> = wire
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str(),
                            value.to_str().expect("validated header").to_owned(),
                        )
                    })
                    .collect();
                let (authorization, extra) = crate::aws::sign(
                    keys,
                    region,
                    "bedrock",
                    crate::aws::UriMode::Standard,
                    &date,
                    &crate::aws::Request {
                        method: &request.method,
                        host,
                        path: address.uri.path(),
                        query: &query_refs,
                        headers: &signed,
                        payload_sha256: &hash,
                    },
                );
                let authorization = Zeroizing::new(authorization);
                add_header(wire.headers_mut(), "authorization", &authorization, true)?;
                for (name, value) in extra {
                    let value = Zeroizing::new(value);
                    add_header(wire.headers_mut(), &name, &value, true)?;
                }
            }
        }
        let maximum = Duration::from_secs(if address.direct { 600 } else { 120 });
        let timeout = request.timeout.unwrap_or(maximum).min(maximum);
        if timeout.is_zero() {
            return Err(ProviderError::new(ProviderErrorKind::Rejected));
        }
        let wire = agent
            .configure_request(wire)
            .timeout_global(Some(timeout))
            .timeout_connect(Some(timeout.min(Duration::from_secs(10))))
            .build();
        let mut response = agent.run(wire).map_err(network_error)?;
        let status = response.status().as_u16();
        let mut headers = Headers::new();
        for (name, value) in response.headers() {
            if let Ok(value) = value.to_str() {
                headers.insert(name.as_str().into(), value.into());
            }
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES as u64)
            .read_to_vec()
            .map_err(|error| {
                let mut error = network_error(error);
                error.status = Some(status);
                error
            })?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

fn network_error(error: ureq::Error) -> ProviderError {
    use ureq::Error;
    let kind = match error {
        Error::Timeout(_) => ProviderErrorKind::Timeout,
        Error::BodyExceedsLimit(_) => ProviderErrorKind::CutOff,
        Error::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut => {
            ProviderErrorKind::Timeout
        }
        Error::Io(ref error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            ProviderErrorKind::CutOff
        }
        Error::BadUri(_)
        | Error::RequireHttpsOnly(_)
        | Error::Http(_)
        | Error::TooManyRedirects
        | Error::RedirectFailed => ProviderErrorKind::Rejected,
        Error::Protocol(_) => ProviderErrorKind::Malformed,
        _ => ProviderErrorKind::Unreachable,
    };
    ProviderError::new(kind)
}

fn status_kind(status: u16) -> ProviderErrorKind {
    match status {
        401 | 403 => ProviderErrorKind::Auth,
        404 => ProviderErrorKind::NotFound,
        429 => ProviderErrorKind::RateLimited,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Rejected,
    }
}

fn error_envelope(body: &Value) -> bool {
    body["success"].as_bool() == Some(false)
        || body.get("error").is_some_and(|value| !value.is_null())
        || body["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice["finish_reason"].as_str() == Some("error")
                    || choice.get("error").is_some_and(|error| !error.is_null())
            })
        })
}

fn response_kind(status: u16, body: &Value) -> ProviderErrorKind {
    if (status == 400 && body["error"]["code"].as_str() == Some("tool_use_failed"))
        || (status == 200
            && body["choices"].as_array().is_some_and(|choices| {
                choices
                    .iter()
                    .any(|choice| choice["finish_reason"].as_str() == Some("error"))
            }))
    {
        ProviderErrorKind::Malformed
    } else {
        status_kind(status)
    }
}

fn excerpt(body: &Value, headers: &Headers) -> Option<String> {
    if let Some(message) = body["error"]["message"].as_str() {
        return Some(message.into());
    }
    if let Some(message) = body["message"]
        .as_str()
        .or_else(|| body["Message"].as_str())
    {
        return Some(
            headers
                .get("x-amzn-errortype")
                .map_or_else(|| message.into(), |kind| format!("{kind}: {message}")),
        );
    }
    if body["error"].is_string()
        && let Some(message) = body["error_description"].as_str()
    {
        return Some(message.into());
    }
    if let Some(message) = body["errors"][0]["message"].as_str() {
        return Some(message.into());
    }
    if let Some(message) = body["detail"].as_str() {
        return Some(message.into());
    }
    body["choices"].as_array()?.iter().find_map(|choice| {
        choice["error"]["message"]
            .as_str()
            .or_else(|| choice["error"].as_str())
            .map(str::to_owned)
    })
}

fn retry_after(headers: &Headers) -> Option<u64> {
    let value = headers.get("retry-after")?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds.min(20));
    }
    let until = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .timestamp();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(u64::try_from(until).ok()?.saturating_sub(now).min(20))
}

fn safe_headers(headers: Headers, known: &[&str]) -> Headers {
    headers
        .into_iter()
        .filter(|(name, _)| !credential_header(name))
        .take(128)
        .map(|(name, value)| (name.to_ascii_lowercase(), scrub_text(&value, known)))
        .collect()
}

fn scrub_text(text: &str, known: &[&str]) -> String {
    let mut text: String = text.chars().filter(|c| !c.is_control()).collect();
    for secret in known.iter().filter(|secret| !secret.is_empty()) {
        let clean: String = secret.chars().filter(|c| !c.is_control()).collect();
        let encoded = crate::aws::uri_encode(secret, true);
        text = text
            .replace(secret, "<redacted>")
            .replace(&encoded, "<redacted>")
            .replace(&lower_percent(&encoded), "<redacted>");
        if !clean.is_empty() {
            text = text.replace(&clean, "<redacted>");
        }
    }
    text = crate::tools::redact::for_output_text(&text);
    let mut out = String::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let length = rest
            .find(|c: char| {
                !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-' | '.' | '/' | '+' | '=' | ':')
            })
            .unwrap_or(rest.len());
        if length == 0 {
            let c = rest.chars().next().expect("not empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let token = &rest[..length];
        let key = (token.starts_with("sk-") && token.len() >= 23)
            || (token.starts_with("AIza") && token.len() >= 39)
            || (token.starts_with("eyJ")
                && token.len() >= 40
                && token.bytes().filter(|c| *c == b'.').count() == 2)
            || (token.len() >= 80
                && token.bytes().any(|c| c.is_ascii_uppercase())
                && token.bytes().any(|c| c.is_ascii_lowercase()));
        if key {
            out.push_str("<redacted>");
        } else {
            out.push_str(token);
        }
        rest = &rest[length..];
        if token.eq_ignore_ascii_case("bearer") {
            let spaces = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
            if spaces > 0 {
                out.push_str(&rest[..spaces]);
                rest = &rest[spaces..];
                let end = rest
                    .find(|c: char| {
                        c.is_whitespace()
                            || matches!(c, '"' | '\'' | ',' | ';' | '<' | '>' | ')' | ']' | '}')
                    })
                    .unwrap_or(rest.len());
                if end > 0 {
                    out.push_str("<redacted>");
                    rest = &rest[end..];
                }
            }
        }
    }
    out.chars().take(300).collect()
}

fn lower_percent(value: &str) -> String {
    let mut out = String::new();
    let mut digits = 0;
    for c in value.chars() {
        if c == '%' {
            digits = 2;
            out.push(c);
        } else if digits > 0 {
            out.push(c.to_ascii_lowercase());
            digits -= 1;
        } else {
            out.push(c);
        }
    }
    out
}

fn decode_query(query: &str) -> Result<Vec<(String, String)>, ProviderError> {
    fn decode(value: &str) -> Result<String, ProviderError> {
        let mut bytes = Vec::new();
        let mut iter = value.bytes();
        while let Some(byte) = iter.next() {
            match byte {
                b'%' => {
                    let a = iter.next().and_then(|a| (a as char).to_digit(16));
                    let b = iter.next().and_then(|b| (b as char).to_digit(16));
                    let (Some(a), Some(b)) = (a, b) else {
                        return Err(ProviderError::new(ProviderErrorKind::Rejected));
                    };
                    bytes.push((a * 16 + b) as u8);
                }
                b'+' => bytes.push(b' '),
                _ => bytes.push(byte),
            }
        }
        String::from_utf8(bytes).map_err(|_| ProviderError::new(ProviderErrorKind::Rejected))
    }
    if query.is_empty() {
        return Ok(Vec::new());
    }
    query
        .split('&')
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((decode(name)?, decode(value)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{Read as _, Write as _},
        net::TcpListener,
        process::Command,
        sync::{Mutex, atomic::AtomicUsize},
    };

    struct Script {
        responses: Mutex<VecDeque<Result<HttpResponse, ProviderError>>>,
        calls: AtomicUsize,
        cancel_after: Option<Arc<AtomicBool>>,
    }
    impl Script {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(Ok).collect()),
                calls: AtomicUsize::new(0),
                cancel_after: None,
            }
        }
    }
    impl Transport for Script {
        fn send(&self, _request: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(cancel) = &self.cancel_after {
                cancel.store(true, Ordering::Relaxed);
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response")
        }
    }
    fn response(status: u16, body: Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: Headers::new(),
            body: serde_json::to_vec(&body).unwrap(),
        }
    }
    fn no_wait(http: &mut Http) {
        http.wait = Arc::new(|_, cancel| !cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn status_codes_and_error_envelopes_are_typed_and_display_ignores_provider_text() {
        for (status, kind, body) in [
            (
                400,
                ProviderErrorKind::Rejected,
                json!({"error":{"message":"storage full not found disabled timed out"}}),
            ),
            (
                400,
                ProviderErrorKind::Malformed,
                json!({"error":{"code":"tool_use_failed","message":"invalid arguments"}}),
            ),
            (
                401,
                ProviderErrorKind::Auth,
                json!({"error":{"message":"key refused"}}),
            ),
            (403, ProviderErrorKind::Auth, json!({"detail":"forbidden"})),
            (
                404,
                ProviderErrorKind::NotFound,
                json!({"error":{"message":"unknown model"}}),
            ),
            (
                429,
                ProviderErrorKind::RateLimited,
                json!({"error":{"message":"quota"}}),
            ),
            (
                503,
                ProviderErrorKind::Unavailable,
                json!({"message":"busy"}),
            ),
            (
                200,
                ProviderErrorKind::Rejected,
                json!({"success":false,"errors":[{"message":"cloudflare failure"}]}),
            ),
            (
                200,
                ProviderErrorKind::Rejected,
                json!({"choices":[{"error":{"message":"openrouter failure"}}]}),
            ),
            (
                200,
                ProviderErrorKind::Malformed,
                json!({"choices":[{"finish_reason":"error","error":{"message":"invalid call"}}]}),
            ),
            (
                200,
                ProviderErrorKind::Malformed,
                json!({"choices":[{"finish_reason":"error"}]}),
            ),
        ] {
            let count = if status == 429 || status >= 500 { 3 } else { 1 };
            let script = Arc::new(Script::new(
                (0..count).map(|_| response(status, body.clone())).collect(),
            ));
            let mut http = Http::with_transport(script.clone());
            no_wait(&mut http);
            let auth = Auth::None;
            let error = http
                .post_json(
                    HttpRequest::json("https://example.test/v1", &json!({}), &auth).unwrap(),
                    &AtomicBool::new(false),
                )
                .err()
                .unwrap();
            assert_eq!(error.kind, kind);
            assert_eq!(error.status, Some(status));
            assert_eq!(script.calls.load(Ordering::Relaxed), count);
            let fixed = ProviderError::new(kind).to_string();
            assert_eq!(error.to_string(), fixed);
            let mut other = error.clone();
            other.excerpt =
                Some("PROVIDER_SENTINEL: disabled, not found, storage full, timed out".into());
            assert_eq!(other.to_string(), fixed);
            assert!(!other.to_string().contains("PROVIDER_SENTINEL"));
        }
    }

    #[test]
    fn excerpts_use_documented_sources_and_scrub_credentials_controls_and_length() {
        let mut headers = Headers::new();
        headers.insert("x-amzn-errortype".into(), "ValidationException".into());
        for (body, expected) in [
            (
                json!({"error":{"message":"first"},"message":"second"}),
                "first",
            ),
            (json!({"Message":"bedrock"}), "ValidationException: bedrock"),
            (
                json!({"error":"invalid_grant","error_description":"oauth"}),
                "oauth",
            ),
            (json!({"errors":[{"message":"cloudflare"}]}), "cloudflare"),
            (json!({"detail":"fastapi"}), "fastapi"),
        ] {
            assert_eq!(excerpt(&body, &headers).as_deref(), Some(expected));
        }
        let secret = "private/key+value";
        let text = format!(
            "\0{} {} {} Bearer unrelated-bearer AKIA1234567890ABCDEF sk-proj-abcdefghijklmnopqrstuvwxyz123456 rtsp://alice:password@host/path {}",
            secret,
            crate::aws::uri_encode(secret, true),
            lower_percent(&crate::aws::uri_encode(secret, true)),
            "文".repeat(400)
        );
        let scrubbed = scrub_text(&text, &[secret]);
        for forbidden in [
            secret,
            "unrelated-bearer",
            "AKIA1234567890ABCDEF",
            "sk-proj-abcdefghijklmnopqrstuvwxyz123456",
            "password",
        ] {
            assert!(!scrubbed.contains(forbidden), "{scrubbed}");
        }
        assert!(scrubbed.chars().count() <= 300);
        assert!(!scrubbed.chars().any(char::is_control));
    }

    #[test]
    fn raw_and_encoded_auth_and_oauth_form_secrets_are_scrubbed_from_errors() {
        let auth = Auth::Header {
            name: "authorization",
            value: Zeroizing::new("Bearer arbitrary-reflected-key".into()),
            quota_project: None,
        };
        let script = Arc::new(Script::new(vec![response(
            400,
            json!({"error":{"message":"Bearer arbitrary-reflected-key and arbitrary-reflected-key"}}),
        )]));
        let error = Http::with_transport(script)
            .post_json(
                HttpRequest::json("https://example.test", &json!({}), &auth).unwrap(),
                &AtomicBool::new(false),
            )
            .err()
            .unwrap();
        assert!(
            !error
                .details()
                .to_string()
                .contains("arbitrary-reflected-key")
        );
        let auth = Auth::None;
        let script = Arc::new(Script::new(vec![response(
            400,
            json!({"error":"invalid_grant","error_description":"private/refresh and private%2frefresh and jwt-assertion"}),
        )]));
        let error = Http::with_transport(script)
            .send_json(
                HttpRequest::form(
                    "https://oauth2.googleapis.com/token",
                    &[
                        ("refresh_token", "private/refresh"),
                        ("assertion", "jwt-assertion"),
                    ],
                    &auth,
                ),
                &AtomicBool::new(false),
            )
            .err()
            .unwrap();
        assert!(!error.details().to_string().contains("private/refresh"));
        assert!(!error.details().to_string().contains("private%2frefresh"));
        assert!(!error.details().to_string().contains("jwt-assertion"));
    }

    #[test]
    fn retries_honor_capped_retry_after_and_stop_at_cancellation() {
        let mut limited = response(429, json!({"error":{"message":"quota"}}));
        limited.headers.insert("Retry-After".into(), "900".into());
        let script = Arc::new(Script::new(vec![
            limited,
            response(200, json!({"ok":true})),
        ]));
        let waited = Arc::new(Mutex::new(Vec::new()));
        let record = waited.clone();
        let mut http = Http::with_transport(script.clone());
        http.wait = Arc::new(move |duration, _| {
            record.lock().unwrap().push(duration);
            true
        });
        let auth = Auth::None;
        assert_eq!(
            http.send_json(
                HttpRequest::get("https://example.test", &auth),
                &AtomicBool::new(false)
            )
            .unwrap()
            .0["ok"],
            true
        );
        assert_eq!(*waited.lock().unwrap(), [Duration::from_secs(20)]);
        assert_eq!(script.calls.load(Ordering::Relaxed), 2);
        let script = Arc::new(Script::new(vec![response(
            503,
            json!({"error":{"message":"busy"}}),
        )]));
        let mut http = Http::with_transport(script.clone());
        http.wait = Arc::new(|_, cancel| {
            cancel.store(true, Ordering::Relaxed);
            false
        });
        let cancel = AtomicBool::new(false);
        assert!(
            http.send_json(HttpRequest::get("https://example.test", &auth), &cancel)
                .is_err()
        );
        assert_eq!(script.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancel_discards_late_success_and_body_limits_apply_to_injected_transports() {
        let cancel = Arc::new(AtomicBool::new(false));
        let script = Arc::new(Script {
            responses: Mutex::new([Ok(response(200, json!({"late":true})))].into()),
            calls: AtomicUsize::new(0),
            cancel_after: Some(cancel.clone()),
        });
        let auth = Auth::None;
        assert!(
            Http::with_transport(script)
                .send_json(HttpRequest::get("https://example.test", &auth), &cancel)
                .is_err()
        );
        let script = Arc::new(Script::new(vec![HttpResponse {
            status: 200,
            headers: Headers::new(),
            body: vec![b' '; MAX_BODY_BYTES + 1],
        }]));
        let error = Http::with_transport(script)
            .send_json(
                HttpRequest::get("https://example.test", &auth),
                &AtomicBool::new(false),
            )
            .err()
            .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::CutOff);
        let script = Arc::new(Script::new(vec![HttpResponse {
            status: 200,
            headers: Headers::new(),
            body: b"not json".to_vec(),
        }]));
        let error = Http::with_transport(script)
            .send_json(
                HttpRequest::get("https://example.test", &auth),
                &AtomicBool::new(false),
            )
            .err()
            .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Malformed);
    }

    #[test]
    fn routing_blocks_plaintext_keys_and_header_debug_hides_auth() {
        let none = Auth::None;
        for (url, direct) in [
            ("https://example.test", false),
            ("http://127.0.0.1:1234/v1", true),
            ("http://[::1]:1234", true),
            ("http://10.0.0.2/v1", true),
            ("https://192.168.1.2/v1", true),
            ("http://169.254.169.254/token", true),
        ] {
            assert_eq!(
                Address::parse(&HttpRequest::get(url, &none))
                    .unwrap()
                    .direct,
                direct
            );
        }
        let auth = Auth::Header {
            name: "x-api-key",
            value: Zeroizing::new("private-key".into()),
            quota_project: None,
        };
        for url in [
            "http://10.0.0.2/v1",
            "http://example.test",
            "https://alice:password@example.test",
            "https://example.test?key=private-key",
            "https://example.test?%6b%65%79=private-key",
        ] {
            assert!(Address::parse(&HttpRequest::get(url, &auth)).is_err());
        }
        let mut headers = ureq::http::HeaderMap::new();
        add_header(&mut headers, "authorization", "Bearer private-key", true).unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert!(!format!("{headers:?}").contains("private-key"));
        let mut headers = Headers::new();
        headers.insert("Set-Cookie".into(), "private-cookie".into());
        headers.insert("X-Request-Id".into(), "private-key".into());
        let safe = safe_headers(headers, &["private-key"]);
        assert!(!safe.contains_key("set-cookie"));
        assert_eq!(safe["x-request-id"], "<redacted>");
    }

    #[test]
    fn direct_agent_ignores_proxy_environment_for_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(value) => break value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "child did not reach loopback");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
                assert!(bytes.len() < 8192);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMetadata-Flavor: Google\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
        });
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "assistant::http::tests::direct_proxy_child",
                "--nocapture",
            ])
            .env("CAPTUREFAB_TEST_PROXY_HTTP_URL", url)
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("http_proxy", "http://127.0.0.1:9")
            .env("https_proxy", "http://127.0.0.1:9")
            .env("all_proxy", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        server.join().unwrap();
    }

    #[test]
    fn direct_proxy_child() {
        let Ok(url) = std::env::var("CAPTUREFAB_TEST_PROXY_HTTP_URL") else {
            return;
        };
        let transport = NativeTransport::new();
        assert!(transport.cloud.config().proxy().is_some());
        assert!(transport.direct.config().proxy().is_none());
        let auth = Auth::None;
        let mut request = HttpRequest::get(url, &auth);
        request.timeout = Some(Duration::from_secs(2));
        let (body, headers) = Http::with_transport(Arc::new(transport))
            .send_json(request, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(headers["metadata-flavor"], "Google");
    }
}
