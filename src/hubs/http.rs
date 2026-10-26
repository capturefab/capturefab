//! A small blocking HTTP/1.1 client for hub APIs, used only in the processes
//! that open or list hub cameras. One request per connection
//! (`Connection: close`), bounded headers and bodies, an overall deadline for
//! the response head, a per-read idle timeout and an optional total deadline
//! for the body, no redirects, and cancellation from another thread.
//!
//! Errors use fixed wording with the hub's label and origin only: never a
//! URL path, a query, a response body or a reason phrase, which a server (or
//! a typo in a pasted URL) could fill with a secret.
use super::{
    Hub,
    registry::{Scheme, Url},
    scrub,
    secret::Secret,
    tls,
};
use crate::types::{Categorized, ErrorKind};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustls::pki_types::ServerName;
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAX_STATUS_LINE: usize = 4096;
const MAX_HEADER_LINE: usize = 8192;
const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 100;
const MAX_CHUNK_LINE: usize = 1024;
const MAX_TRAILERS: usize = 8192;
/// Each resolved address gets at most this long, so one dead address does
/// not use up the whole deadline.
const CONNECT_EACH: Duration = Duration::from_secs(3);

/// Where requests go, and how its certificate is trusted.
pub(crate) struct Endpoint {
    /// "Home Assistant hub home", for messages.
    pub label: String,
    pub url: Url,
    tls: Option<(Arc<rustls::ClientConfig>, ServerName<'static>)>,
}

impl Endpoint {
    pub(crate) fn new(
        label: impl Into<String>,
        url: Url,
        trust: &tls::Trust,
    ) -> anyhow::Result<Self> {
        let tls = match url.scheme {
            Scheme::Http => None,
            Scheme::Https => Some((
                tls::client_config(trust)?,
                tls::server_name(&url.host, trust)?,
            )),
        };
        Ok(Self {
            label: label.into(),
            url,
            tls,
        })
    }
    pub(crate) fn for_hub(hub: &Hub) -> anyhow::Result<Self> {
        Self::new(label(hub), hub.parsed_url()?, &tls::Trust::of(hub))
    }
    /// An endpoint that only records the certificate the server offers.
    pub(crate) fn capturing(
        label: impl Into<String>,
        url: Url,
    ) -> anyhow::Result<(Self, tls::Seen)> {
        let (config, seen) = tls::capture_config()?;
        let name = tls::server_name(&url.host, &tls::Trust::Platform)?;
        Ok((
            Self {
                label: label.into(),
                url,
                tls: Some((config, name)),
            },
            seen,
        ))
    }
    pub(crate) fn origin(&self) -> String {
        self.url.origin()
    }
}

/// "Home Assistant hub home".
pub(crate) fn label(hub: &Hub) -> String {
    format!("{} hub {}", hub.kind.label(), hub.name)
}

/// How a request authenticates.
pub(crate) enum Auth<'a> {
    None,
    /// `Authorization: Bearer` (Home Assistant tokens, Frigate sessions).
    Bearer(&'a str),
    /// `X-API-KEY` (UniFi Protect).
    ApiKey(&'a Secret),
    /// `Authorization: Basic`, sent with the first request (go2rtc).
    Basic {
        user: &'a str,
        password: &'a Secret,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Connecting, sending the request and receiving the response head.
    pub headers: Duration,
    /// The longest wait for any read of the body.
    pub idle: Duration,
    /// The whole exchange, or `None` for a stream that lasts until closed.
    pub total: Option<Duration>,
    /// The most body bytes accepted, or `None` for a stream.
    pub max_body: Option<u64>,
}

impl Limits {
    #[cfg(test)]
    pub(crate) fn request(timeout: Duration, max_body: u64) -> Self {
        Self {
            headers: timeout,
            idle: timeout,
            total: Some(timeout),
            max_body: Some(max_body),
        }
    }
    /// Every limit raised to at least `minimum` (management commands honour
    /// `--timeout-ms` as a floor).
    pub(crate) fn at_least(self, minimum: Duration) -> Self {
        Self {
            headers: self.headers.max(minimum),
            idle: self.idle.max(minimum),
            total: self.total.map(|total| total.max(minimum)),
            max_body: self.max_body,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
}

pub(crate) struct Request<'a> {
    pub method: Method,
    /// What is being fetched, for messages: "the camera list".
    pub route: &'static str,
    /// Path and query below the hub's base path, starting with `/`.
    pub target: String,
    pub accept: &'static str,
    /// A JSON request body.
    pub json: Option<&'a [u8]>,
    pub limits: Limits,
}

impl<'a> Request<'a> {
    pub(crate) fn get(
        route: &'static str,
        target: String,
        accept: &'static str,
        limits: Limits,
    ) -> Self {
        Self {
            method: Method::Get,
            route,
            target,
            accept,
            json: None,
            limits,
        }
    }
}

/// Lets another thread abort the request or body read in progress. Clones
/// share one flag.
#[derive(Clone, Default)]
pub(crate) struct Cancel(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    socket: Mutex<Option<TcpStream>>,
}

impl Cancel {
    /// Stop the current request and refuse new ones.
    pub(crate) fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        if let Some(socket) = self
            .0
            .socket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }
    fn attach(&self, socket: &TcpStream) -> bool {
        let mut slot = self.0.socket.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            return false;
        }
        *slot = socket.try_clone().ok();
        true
    }
}

/// What went wrong, without any server-chosen text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Problem {
    Resolve,
    Refused,
    Unreachable(&'static str),
    Timeout { phase: Phase, ms: u128 },
    Certificate(tls::CertRejected),
    Tls(&'static str),
    Protocol(&'static str),
    TooLarge(u64),
    Redirect { status: u16, host: Option<String> },
    InvalidRequest,
    Cancelled,
    Closed,
    Io(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Resolve,
    Connect,
    Response,
    Idle,
    Total,
}

#[derive(Debug, Clone)]
pub(crate) struct HttpError {
    pub problem: Problem,
    message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for HttpError {}

impl HttpError {
    fn new(endpoint: &Endpoint, route: &str, problem: Problem) -> Self {
        let label = &endpoint.label;
        let origin = endpoint.origin();
        let authority = format!("{}:{}", endpoint.url.authority_host(), endpoint.url.port);
        let message = match &problem {
            Problem::Resolve => format!(
                "cannot reach {label} at {origin}: cannot resolve {}",
                endpoint.url.host
            ),
            Problem::Refused => format!("cannot reach {label} at {origin}: connection refused"),
            Problem::Unreachable(reason) => format!("cannot reach {label} at {origin}: {reason}"),
            Problem::Timeout { phase, ms } => match phase {
                Phase::Idle => {
                    format!("{label} at {origin} sent nothing for {route} in {ms} ms (timed out)")
                }
                Phase::Total => format!(
                    "{label} at {origin} did not finish sending {route} within {ms} ms (timed out)"
                ),
                Phase::Resolve | Phase::Connect | Phase::Response => {
                    format!("{label} at {origin} did not answer within {ms} ms (timed out)")
                }
            },
            Problem::Certificate(rejected) => rejected.describe(label, &authority),
            Problem::Tls(what) => format!("{label} at {origin}: {what}"),
            Problem::Protocol(what) => {
                format!("{label} at {origin} sent an invalid HTTP response ({what})")
            }
            Problem::TooLarge(limit) => {
                format!("{label} sent more than {limit} bytes for {route}")
            }
            Problem::Redirect { status, host } => match host {
                Some(host) => format!(
                    "{label} answered with a redirect to {host} (HTTP {status}); add the hub with that address"
                ),
                None => format!(
                    "{label} answered with a redirect (HTTP {status}); add the hub with the address it redirects to"
                ),
            },
            Problem::InvalidRequest => {
                format!("refused to send an invalid request for {route} to {label}")
            }
            Problem::Cancelled => format!("the request to {label} was cancelled"),
            Problem::Closed => {
                format!("{label} at {origin} closed the connection before answering")
            }
            Problem::Io(what) => format!("{label} at {origin}: {what}"),
        };
        Self {
            problem,
            message: scrub::scrub(&message).into_owned(),
        }
    }
    pub(crate) fn kind(&self) -> ErrorKind {
        match self.problem {
            Problem::Resolve | Problem::Refused | Problem::Unreachable(_) => ErrorKind::Unavailable,
            Problem::Timeout { .. } => ErrorKind::Timeout,
            _ => ErrorKind::OperationFailed,
        }
    }
    /// Whether retrying cannot help: a refused certificate, a redirect or a
    /// request this client would never send.
    pub(crate) fn permanent(&self) -> bool {
        matches!(
            self.problem,
            Problem::Certificate(_)
                | Problem::Tls(_)
                | Problem::Redirect { .. }
                | Problem::InvalidRequest
                | Problem::Cancelled
        )
    }
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
    /// The error as a categorized `anyhow` error.
    pub(crate) fn into_error(self) -> anyhow::Error {
        Categorized::error(self.kind(), self.message)
    }
    /// The `HttpError` inside an I/O error produced by [`Body`].
    pub(crate) fn from_io(error: &io::Error) -> Option<&HttpError> {
        error.get_ref()?.downcast_ref::<HttpError>()
    }
}

/// Rearm the absolute deadline before every socket operation, including the
/// reads rustls performs inside a handshake. A timeout set just once per HTTP
/// line would restart with each successful read and let a slow drip run forever.
struct Socket {
    stream: TcpStream,
    idle: Duration,
    deadline: Option<Instant>,
    cancel: Option<Cancel>,
}

/// How often a read that can be cancelled looks at its flag.
const CANCEL_POLL: Duration = Duration::from_millis(100);

impl Socket {
    fn wait(&self) -> io::Result<Duration> {
        let left = self
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(left.map_or(self.idle, |left| left.min(self.idle)))
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wait = self.wait()?;
        // Socket timeouts can return early on Windows. Keep waiting until
        // the requested deadline so a total timeout is not reported as idle.
        // Cancellable reads also use short slices: Windows does not wake a
        // blocked read when another thread shuts the socket down.
        let started = Instant::now();
        loop {
            if self.cancel.as_ref().is_some_and(Cancel::is_cancelled) {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let remaining = wait.saturating_sub(started.elapsed());
            let slice = if self.cancel.is_some() {
                remaining.min(CANCEL_POLL)
            } else {
                remaining
            };
            if slice.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.stream.set_read_timeout(Some(slice))?;
            match self.stream.read(buf) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.wait()?))?;
        self.stream.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

enum Stream {
    Plain(Socket),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, Socket>>),
}

impl Stream {
    fn body_limits(&mut self, idle: Duration, deadline: Option<Instant>) {
        let socket = match self {
            Self::Plain(socket) => socket,
            Self::Tls(stream) => &mut stream.sock,
        };
        socket.idle = idle.max(Duration::from_millis(1));
        socket.deadline = deadline;
    }
}
impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

/// A response whose body has not been read yet.
pub(crate) struct Response {
    pub status: u16,
    /// Lowercase names, values as sent.
    headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    pub(crate) fn headers<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    /// The media type without parameters, lowercase.
    pub(crate) fn media_type(&self) -> Option<String> {
        self.header("content-type").map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
    }
}

enum Framing {
    Empty,
    Length(u64),
    Chunked { left: u64 },
    UntilEof,
}

/// A response body, read with the request's idle and total limits.
pub(crate) struct Body {
    reader: BufReader<Stream>,
    socket: TcpStream,
    framing: Framing,
    done: bool,
    received: u64,
    max: Option<u64>,
    idle: Duration,
    deadline: Option<Instant>,
    total: Option<Duration>,
    cancel: Option<Cancel>,
    context: (String, String, Url, &'static str),
}

impl Body {
    fn error(&self, problem: Problem) -> HttpError {
        let (label, _, url, route) = &self.context;
        let endpoint = Endpoint {
            label: label.clone(),
            url: url.clone(),
            tls: None,
        };
        HttpError::new(&endpoint, route, problem)
    }
    fn timeout(&self, error: &io::Error) -> Option<Problem> {
        if !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            return None;
        }
        let total_over = self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        Some(if total_over {
            Problem::Timeout {
                phase: Phase::Total,
                ms: self.total.unwrap_or_default().as_millis(),
            }
        } else {
            Problem::Timeout {
                phase: Phase::Idle,
                ms: self.idle.as_millis(),
            }
        })
    }
    fn arm(&self) -> Result<(), HttpError> {
        let mut wait = self.idle;
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(self.error(Problem::Timeout {
                    phase: Phase::Total,
                    ms: self.total.unwrap_or_default().as_millis(),
                }));
            }
            wait = wait.min(left);
        }
        self.socket
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))))
            .map_err(|_| self.error(Problem::Io("network error")))
    }
    fn io(&self, error: io::Error) -> HttpError {
        if let Some(problem) = self.timeout(&error) {
            return self.error(problem);
        }
        self.error(classify_io(&error))
    }
    fn line(&mut self, limit: usize) -> Result<Vec<u8>, HttpError> {
        self.arm()?;
        read_line(&mut self.reader, limit).map_err(|error| match error {
            LineError::Io(error) => self.io(error),
            LineError::TooLong => self.error(Problem::Protocol("line too long")),
            LineError::Eof => self.error(Problem::Protocol("truncated body")),
        })
    }
    fn read_body(&mut self, buf: &mut [u8]) -> Result<usize, HttpError> {
        let result = self.read_framed(buf);
        // A cancelled read ends with a shut-down socket, which can look like
        // the end of a body that lasts until close.
        if self.cancel.as_ref().is_some_and(Cancel::is_cancelled) {
            self.done = true;
            return Err(self.error(Problem::Cancelled));
        }
        result
    }
    fn read_framed(&mut self, buf: &mut [u8]) -> Result<usize, HttpError> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        let count = loop {
            match self.framing {
                Framing::Empty => {
                    self.done = true;
                    return Ok(0);
                }
                Framing::Length(0) => {
                    self.done = true;
                    return Ok(0);
                }
                Framing::Length(left) => {
                    let want = buf.len().min(left.min(usize::MAX as u64) as usize);
                    self.arm()?;
                    let count = self.reader.read(&mut buf[..want]).map_err(|e| self.io(e))?;
                    if count == 0 {
                        return Err(self.error(Problem::Protocol("truncated body")));
                    }
                    self.framing = Framing::Length(left - count as u64);
                    break count;
                }
                Framing::Chunked { left: 0 } => {
                    let line = self.line(MAX_CHUNK_LINE)?;
                    let size = std::str::from_utf8(&line)
                        .ok()
                        .and_then(|line| {
                            let size = line.split(';').next()?.trim();
                            (!size.is_empty() && size.len() <= 16)
                                .then(|| u64::from_str_radix(size, 16).ok())
                                .flatten()
                        })
                        .ok_or_else(|| self.error(Problem::Protocol("bad chunk size")))?;
                    if size == 0 {
                        let mut trailers = 0;
                        loop {
                            let line = self.line(MAX_HEADER_LINE)?;
                            trailers += line.len() + 2;
                            if trailers > MAX_TRAILERS {
                                return Err(self.error(Problem::Protocol("trailers too long")));
                            }
                            if line.is_empty() {
                                break;
                            }
                        }
                        self.done = true;
                        return Ok(0);
                    }
                    if self
                        .max
                        .is_some_and(|max| self.received.saturating_add(size) > max)
                    {
                        return Err(self.error(Problem::TooLarge(self.max.unwrap_or_default())));
                    }
                    self.framing = Framing::Chunked { left: size };
                }
                Framing::Chunked { left } => {
                    let want = buf.len().min(left.min(usize::MAX as u64) as usize);
                    self.arm()?;
                    let count = self.reader.read(&mut buf[..want]).map_err(|e| self.io(e))?;
                    if count == 0 {
                        return Err(self.error(Problem::Protocol("truncated chunk")));
                    }
                    let left = left - count as u64;
                    self.framing = Framing::Chunked { left };
                    if left == 0 {
                        let end = self.line(2)?;
                        if !end.is_empty() {
                            return Err(self.error(Problem::Protocol("bad chunk end")));
                        }
                    }
                    break count;
                }
                Framing::UntilEof => {
                    self.arm()?;
                    match self.reader.read(buf) {
                        Ok(0) => {
                            self.done = true;
                            return Ok(0);
                        }
                        Ok(count) => break count,
                        // A server that closes TLS without close_notify still
                        // ended this body.
                        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                            self.done = true;
                            return Ok(0);
                        }
                        Err(error) => return Err(self.io(error)),
                    }
                }
            }
        };
        self.received += count as u64;
        if let Some(max) = self.max
            && self.received > max
        {
            return Err(self.error(Problem::TooLarge(max)));
        }
        Ok(count)
    }
    /// The whole body, at most `max` bytes.
    pub(crate) fn read_all(mut self, max: usize) -> Result<Vec<u8>, HttpError> {
        self.max = Some(self.max.map_or(max as u64, |limit| limit.min(max as u64)));
        let mut data = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let count = self.read_body(&mut buf)?;
            if count == 0 {
                return Ok(data);
            }
            data.extend_from_slice(&buf[..count]);
        }
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_body(buf).map_err(io::Error::other)
    }
}

#[derive(Debug)]
enum LineError {
    Io(io::Error),
    TooLong,
    Eof,
}

/// One line without its CRLF (or bare LF), at most `limit` bytes.
fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<Vec<u8>, LineError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(LineError::Io)?;
        if available.is_empty() {
            return Err(LineError::Eof);
        }
        if let Some(end) = available.iter().position(|&b| b == b'\n') {
            if line.len() + end > limit + 1 {
                return Err(LineError::TooLong);
            }
            line.extend_from_slice(&available[..end]);
            reader.consume(end + 1);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > limit {
                return Err(LineError::TooLong);
            }
            return Ok(line);
        }
        let count = available.len();
        if line.len() + count > limit + 1 {
            return Err(LineError::TooLong);
        }
        line.extend_from_slice(available);
        reader.consume(count);
    }
}

fn classify_io(error: &io::Error) -> Problem {
    if let Some(tls_error) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        if let Some(rejected) = tls::rejection(tls_error) {
            return Problem::Certificate(rejected);
        }
        return Problem::Tls(match tls_error {
            rustls::Error::PeerIncompatible(_)
            | rustls::Error::AlertReceived(rustls::AlertDescription::ProtocolVersion) => {
                "the server offered only TLS 1.0 or 1.1, which Capturefab does not support"
            }
            _ => "the TLS handshake failed",
        });
    }
    match error.kind() {
        io::ErrorKind::ConnectionRefused => Problem::Refused,
        io::ErrorKind::HostUnreachable => Problem::Unreachable("no route to host"),
        io::ErrorKind::NetworkUnreachable => Problem::Unreachable("network is unreachable"),
        io::ErrorKind::ConnectionReset => Problem::Io("the connection was reset"),
        io::ErrorKind::ConnectionAborted => Problem::Io("the connection was aborted"),
        io::ErrorKind::BrokenPipe => Problem::Io("the connection was closed"),
        io::ErrorKind::UnexpectedEof => Problem::Closed,
        _ if host_down(error) => Problem::Unreachable("host is down"),
        _ => Problem::Io("the network connection failed"),
    }
}

fn host_down(error: &io::Error) -> bool {
    // EHOSTDOWN: 64 on the BSDs and macOS, 112 on Linux.
    let code = if cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )) {
        64
    } else if cfg!(target_os = "linux") {
        112
    } else {
        return false;
    };
    error.raw_os_error() == Some(code)
}

fn valid_target(target: &str) -> bool {
    target.starts_with('/') && target.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn valid_header(value: &str) -> bool {
    !value.bytes().any(|b| b < 0x20 || b == 0x7f)
}

pub(crate) fn resolve(
    host: &str,
    port: u16,
    deadline: Instant,
) -> Result<Vec<SocketAddr>, Problem> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let (sender, receiver) = mpsc::channel();
    let name = host.to_owned();
    // getaddrinfo has no timeout; a stuck lookup finishes on its own thread.
    std::thread::Builder::new()
        .name("capturefab-hub-dns".into())
        .spawn(move || {
            let _ = sender.send(
                (name.as_str(), port)
                    .to_socket_addrs()
                    .map(|addrs| addrs.collect::<Vec<_>>()),
            );
        })
        .map_err(|_| Problem::Io("cannot start name resolution"))?;
    let wait = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(wait) {
        Ok(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
        Ok(_) => Err(Problem::Resolve),
        Err(_) => Err(Problem::Timeout {
            phase: Phase::Resolve,
            ms: 0,
        }),
    }
}

fn connect(
    addrs: &[SocketAddr],
    deadline: Instant,
    budget: Duration,
) -> Result<TcpStream, Problem> {
    let mut last = Problem::Refused;
    for addr in addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Problem::Timeout {
                phase: Phase::Connect,
                ms: budget.as_millis(),
            });
        }
        match TcpStream::connect_timeout(addr, left.min(CONNECT_EACH)) {
            Ok(stream) => return Ok(stream),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                last = Problem::Timeout {
                    phase: Phase::Connect,
                    ms: budget.as_millis(),
                };
            }
            Err(error) => {
                last = match classify_io(&error) {
                    Problem::Unreachable(_)
                        if crate::network::local_network_hint(&error, Some(addr.ip()))
                            .is_some() =>
                    {
                        Problem::Unreachable(crate::network::NO_ROUTE_LOCAL_NETWORK)
                    }
                    problem => problem,
                }
            }
        }
    }
    Err(last)
}

fn keepalive(socket: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(5));
    let socket = socket2::SockRef::from(socket);
    let _ = socket.set_tcp_keepalive(&keepalive);
    let _ = socket.set_tcp_nodelay(true);
}

/// The host of a redirect target, if it is safe to show.
fn redirect_host(location: Option<&str>) -> Option<String> {
    let location = location?;
    let (_, rest) = location.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    (!host.is_empty()
        && host.len() <= 260
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]_".contains(&b)))
    .then(|| host.to_owned())
}

/// Send `request` and read the response head.
pub(crate) fn send(
    endpoint: &Endpoint,
    request: &Request,
    auth: &Auth,
    cancel: Option<&Cancel>,
) -> Result<Response, HttpError> {
    let fail = |problem| HttpError::new(endpoint, request.route, problem);
    let start = Instant::now();
    let limits = request.limits;
    let head_deadline = start + limits.headers;
    let target = format!("{}{}", endpoint.url.path, request.target);
    if !valid_target(&target) || !valid_header(request.accept) {
        return Err(fail(Problem::InvalidRequest));
    }
    let mut head: Zeroizing<String> = Zeroizing::new(String::with_capacity(512));
    let method = match request.method {
        Method::Get => "GET",
        Method::Post => "POST",
    };
    head.push_str(&format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nUser-Agent: capturefab/{}\r\nAccept: {}\r\nConnection: close\r\n",
        endpoint.url.authority(),
        env!("CARGO_PKG_VERSION"),
        request.accept
    ));
    match auth {
        Auth::None => {}
        Auth::Bearer(token) => {
            if !valid_header(token) {
                return Err(fail(Problem::InvalidRequest));
            }
            head.push_str("Authorization: Bearer ");
            head.push_str(token);
            head.push_str("\r\n");
        }
        Auth::ApiKey(key) => {
            head.push_str("X-API-KEY: ");
            head.push_str(key.expose());
            head.push_str("\r\n");
        }
        Auth::Basic { user, password } => {
            if !valid_header(user) {
                return Err(fail(Problem::InvalidRequest));
            }
            let credentials = Zeroizing::new(format!("{user}:{}", password.expose()));
            let encoded = Zeroizing::new(STANDARD.encode(credentials.as_bytes()));
            scrub::register(&encoded);
            head.push_str("Authorization: Basic ");
            head.push_str(&encoded);
            head.push_str("\r\n");
        }
    }
    if let Some(body) = request.json {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    let addrs =
        resolve(&endpoint.url.host, endpoint.url.port, head_deadline).map_err(|problem| {
            fail(match problem {
                Problem::Timeout { phase, .. } => Problem::Timeout {
                    phase,
                    ms: limits.headers.as_millis(),
                },
                other => other,
            })
        })?;
    if cancel.is_some_and(Cancel::is_cancelled) {
        return Err(fail(Problem::Cancelled));
    }
    let socket = connect(&addrs, head_deadline, limits.headers).map_err(fail)?;
    keepalive(&socket);
    if let Some(cancel) = cancel
        && !cancel.attach(&socket)
    {
        return Err(fail(Problem::Cancelled));
    }
    let io_error = |error: io::Error| {
        if cancel.is_some_and(Cancel::is_cancelled) {
            return fail(Problem::Cancelled);
        }
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            return fail(Problem::Timeout {
                phase: Phase::Response,
                ms: limits.headers.as_millis(),
            });
        }
        fail(classify_io(&error))
    };
    let arm = |socket: &TcpStream| -> Result<(), HttpError> {
        let left = head_deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(fail(Problem::Timeout {
                phase: Phase::Response,
                ms: limits.headers.as_millis(),
            }));
        }
        socket
            .set_read_timeout(Some(left))
            .and_then(|()| socket.set_write_timeout(Some(left)))
            .map_err(|_| fail(Problem::Io("the network connection failed")))
    };
    arm(&socket)?;
    let clone = Socket {
        stream: socket.try_clone().map_err(io_error)?,
        idle: limits.headers.max(Duration::from_millis(1)),
        deadline: Some(head_deadline),
        cancel: cancel.cloned(),
    };
    let mut stream = match &endpoint.tls {
        None => Stream::Plain(clone),
        Some((config, name)) => {
            let connection = rustls::ClientConnection::new(config.clone(), name.clone())
                .map_err(|_| fail(Problem::Tls("the TLS handshake failed")))?;
            Stream::Tls(Box::new(rustls::StreamOwned::new(connection, clone)))
        }
    };
    stream.write_all(head.as_bytes()).map_err(io_error)?;
    if let Some(body) = request.json {
        stream.write_all(body).map_err(io_error)?;
    }
    stream.flush().map_err(io_error)?;
    let mut reader = BufReader::with_capacity(16 * 1024, stream);
    let (status, headers) = loop {
        arm(&socket)?;
        let line = read_line(&mut reader, MAX_STATUS_LINE).map_err(|error| match error {
            LineError::Io(error) => io_error(error),
            LineError::TooLong => fail(Problem::Protocol("status line too long")),
            LineError::Eof => fail(Problem::Closed),
        })?;
        let status =
            parse_status(&line).ok_or_else(|| fail(Problem::Protocol("bad status line")))?;
        let mut headers = Vec::new();
        let mut size = line.len();
        loop {
            arm(&socket)?;
            let line = read_line(&mut reader, MAX_HEADER_LINE).map_err(|error| match error {
                LineError::Io(error) => io_error(error),
                LineError::TooLong => fail(Problem::Protocol("header too long")),
                LineError::Eof => fail(Problem::Closed),
            })?;
            if line.is_empty() {
                break;
            }
            size += line.len() + 2;
            if size > MAX_HEAD || headers.len() >= MAX_HEADERS {
                return Err(fail(Problem::Protocol("too many headers")));
            }
            if line[0] == b' ' || line[0] == b'\t' {
                return Err(fail(Problem::Protocol("folded header")));
            }
            let text =
                String::from_utf8(line).map_err(|_| fail(Problem::Protocol("non-UTF-8 header")))?;
            let (name, value) = text
                .split_once(':')
                .filter(|(name, _)| {
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                })
                .ok_or_else(|| fail(Problem::Protocol("bad header")))?;
            headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        }
        // Interim responses come before the real one.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        break (status, headers);
    };
    let find = |name: &'static str| {
        headers
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, v)| v.as_str())
    };
    if (300..400).contains(&status) && status != 304 {
        return Err(fail(Problem::Redirect {
            status,
            host: redirect_host(find("location").next()),
        }));
    }
    if find("content-encoding").any(|coding| !coding.eq_ignore_ascii_case("identity")) {
        return Err(fail(Problem::Protocol("compressed response")));
    }
    let framing = if status == 204 || status == 304 || status == 101 {
        Framing::Empty
    } else if let Some(coding) = find("transfer-encoding").next() {
        if find("transfer-encoding").count() != 1 || !coding.eq_ignore_ascii_case("chunked") {
            return Err(fail(Problem::Protocol("unsupported transfer coding")));
        }
        Framing::Chunked { left: 0 }
    } else {
        let mut lengths = find("content-length")
            .flat_map(|value| value.split(','))
            .map(|value| value.trim().parse::<u64>());
        match lengths.next() {
            None => Framing::UntilEof,
            Some(Ok(length)) => {
                if lengths.any(|other| other.as_ref().ok() != Some(&length)) {
                    return Err(fail(Problem::Protocol("conflicting lengths")));
                }
                if limits.max_body.is_some_and(|max| length > max) {
                    return Err(fail(Problem::TooLarge(limits.max_body.unwrap_or_default())));
                }
                Framing::Length(length)
            }
            Some(Err(_)) => return Err(fail(Problem::Protocol("bad length"))),
        }
    };
    let deadline = limits.total.map(|total| start + total);
    reader.get_mut().body_limits(limits.idle, deadline);
    Ok(Response {
        status,
        headers,
        body: Body {
            reader,
            socket,
            framing,
            done: false,
            received: 0,
            max: limits.max_body,
            idle: limits.idle,
            deadline,
            total: limits.total,
            cancel: cancel.cloned(),
            context: (
                endpoint.label.clone(),
                endpoint.origin(),
                endpoint.url.clone(),
                request.route,
            ),
        },
    })
}

fn parse_status(line: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(line).ok()?;
    let rest = text
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| text.strip_prefix("HTTP/1.0 "))?;
    let code = rest.get(..3)?;
    if !code.bytes().all(|b| b.is_ascii_digit()) || rest.len() > 3 && !rest[3..].starts_with(' ') {
        return None;
    }
    code.parse().ok().filter(|code| (100..=599).contains(code))
}

/// Connect, complete the TLS handshake far enough to see the server's
/// certificate, and return its fingerprint. No request is sent.
pub(crate) fn fetch_fingerprint(
    label: &str,
    url: &Url,
    timeout: Duration,
) -> anyhow::Result<[u8; 32]> {
    let (endpoint, seen) = Endpoint::capturing(label, url.clone())?;
    let fail = |problem| HttpError::new(&endpoint, "the certificate", problem).into_error();
    let deadline = Instant::now() + timeout;
    let addrs = resolve(&url.host, url.port, deadline).map_err(|problem| {
        fail(match problem {
            Problem::Timeout { phase, .. } => Problem::Timeout {
                phase,
                ms: timeout.as_millis(),
            },
            other => other,
        })
    })?;
    let socket = connect(&addrs, deadline, timeout).map_err(fail)?;
    let Some((config, name)) = &endpoint.tls else {
        unreachable!("capturing endpoints use TLS")
    };
    let mut connection = rustls::ClientConnection::new(config.clone(), name.clone())
        .map_err(|_| fail(Problem::Tls("the TLS handshake failed")))?;
    let mut socket = Socket {
        stream: socket,
        idle: timeout.max(Duration::from_millis(1)),
        deadline: Some(deadline),
        cancel: None,
    };
    // Drive the handshake until the capture verifier aborts it.
    while connection.is_handshaking() {
        if let Err(error) = connection.complete_io(&mut socket) {
            if seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()
            {
                break;
            }
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                return Err(fail(Problem::Timeout {
                    phase: Phase::Response,
                    ms: timeout.as_millis(),
                }));
            }
            return Err(fail(classify_io(&error)));
        }
    }
    let offered = *seen.lock().unwrap_or_else(PoisonError::into_inner);
    offered.ok_or_else(|| fail(Problem::Tls("the server presented no certificate")))
}

#[cfg(test)]
pub(crate) mod tests;
