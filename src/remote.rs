//! Same-origin HTTP control for a headless session. Camera operations retain the
//! coordinator's validation, ordering, job lifetime, and storage protections.
//! Access restrictions are supplied by the network serving the listener.
//!
//! This intentionally small HTTP/1 server closes every connection. Its parser,
//! socket deadlines, body limit, and connection limit bound untrusted clients
//! without introducing a second asynchronous runtime into the camera process.
use crate::session::{ActivityOrigin, ActorKind, SessionActor, SessionCommand, SessionHandle};
use crate::types::Frame;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_CONNECTIONS: usize = 64;
// Leave room for commands, previews and status while clients watch activity.
const MAX_ACTIVITY_WAITS: usize = 32;
const MAX_HEADERS: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;
const MAX_ASSET: u64 = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
// Out of descriptors and similar accept failures repeat at once; pause instead of spinning.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
// How long Drop waits for its wakeup connect, and then for accept to see it.
const WAKE_TIMEOUT: Duration = Duration::from_secs(1);
// One cached preview per camera and format; a viewer of a camera beyond
// this re-encodes as before.
const PREVIEW_SLOTS: usize = 16;
// Clients asking this soon after an encode get that preview, even when the
// camera has a newer frame. Under the bundled client's 250 ms poll, so tabs
// share encodes without any one of them losing its preview rate.
const PREVIEW_SHARE: Duration = Duration::from_millis(100);
/// The source build's client, relative to the working directory.
const DEFAULT_WEB_DIR: &str = "web-client/dist";

/// Browser client files built into the executable by the `embedded-web`
/// feature (see build.rs), by path inside the web root; empty without it.
static EMBEDDED: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/web-assets.rs"));

/// Where the browser client is served from.
#[derive(Debug, Clone, PartialEq)]
pub enum WebRoot {
    /// A built web-client folder, canonicalized.
    Folder(PathBuf),
    /// Files built into the executable.
    Embedded(&'static [(&'static str, &'static [u8])]),
}

/// Whether this executable carries the browser client.
pub fn embedded() -> bool {
    !EMBEDDED.is_empty()
}

/// The browser client to serve: `--web-dir` when given, otherwise the copy
/// built into the executable, a `web` folder beside the executable, then
/// the source build's `web-client/dist` in the working directory.
pub fn web_root(web_dir: Option<&Path>) -> Result<WebRoot> {
    if let Some(dir) = web_dir {
        return validate_web_root(dir).map(WebRoot::Folder);
    }
    if embedded() {
        return Ok(WebRoot::Embedded(EMBEDDED));
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("web")));
    if let Some(root) = beside.and_then(|dir| validate_web_root(&dir).ok()) {
        return Ok(WebRoot::Folder(root));
    }
    validate_web_root(Path::new(DEFAULT_WEB_DIR))
        .map(WebRoot::Folder)
        .context("this executable has no browser client built in or in a web folder beside it; pass --web-dir")
}

/// Fail early if the browser build is absent or links outside the asset root.
pub fn validate_web_root(root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize().with_context(|| {
        format!(
            "remote web root {} is unavailable; build the web-client first",
            root.display()
        )
    })?;
    ensure!(root.is_dir(), "remote web root must be a directory");
    for asset in ["index.html", "capturefab_web_bg.wasm"] {
        let path = root.join(asset).canonicalize().with_context(|| {
            format!("remote web asset {asset} is missing; build the web-client first")
        })?;
        ensure!(
            path.starts_with(&root) && path.is_file(),
            "remote web asset {asset} must be a file inside the web root"
        );
    }
    Ok(root)
}

/// Owns the listener and its bounded connection handlers. Dropping it stops
/// accepting requests and interrupts command waits; activity waits end when
/// the session stops, which is independent.
pub struct Server {
    address: SocketAddr,
    quitting: Arc<AtomicBool>,
    /// Signalled once the accept loop has stopped.
    stopped: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

struct Service {
    handle: SessionHandle,
    web_root: WebRoot,
    quitting: Arc<AtomicBool>,
    activity_waits: Arc<AtomicUsize>,
    previews: Previews,
}

impl Server {
    /// Bind an explicit address. The CLI opts in with `--listen`; publishing on
    /// a tailnet uses the server's Tailscale address or a local HTTPS proxy.
    pub fn start(handle: SessionHandle, address: SocketAddr, web_root: WebRoot) -> Result<Self> {
        let listener = TcpListener::bind(address).context("bind remote HTTP listener")?;
        let address = listener.local_addr()?;
        let quitting = Arc::new(AtomicBool::new(false));
        let service = Arc::new(Service {
            handle,
            web_root,
            quitting: quitting.clone(),
            activity_waits: Arc::new(AtomicUsize::new(0)),
            previews: Previews::default(),
        });
        let active = Arc::new(AtomicUsize::new(0));
        let (done, stopped) = mpsc::channel();
        // Accept blocks instead of polling; Drop wakes it with a loopback connect.
        // Client handlers use blocking I/O with deadlines so partial reads and
        // backpressure cannot truncate requests or responses.
        let server_thread = thread::Builder::new()
            .name("capturefab-http".into())
            .spawn(move || {
                let mut clients = Vec::<JoinHandle<()>>::new();
                for stream in listener.incoming() {
                    if service.quitting.load(Ordering::Acquire) {
                        break;
                    }
                    clients.retain(|client| !client.is_finished());
                    let mut stream = match stream {
                        Ok(stream) => stream,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => {
                            thread::sleep(ACCEPT_RETRY);
                            continue;
                        }
                    };
                    if active.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
                        let _ = write_response(
                            &mut stream,
                            error(503, "remote server is busy; retry shortly"),
                            false,
                        );
                        continue;
                    }
                    active.fetch_add(1, Ordering::AcqRel);
                    let service = service.clone();
                    let count = active.clone();
                    match thread::Builder::new()
                        .name("capturefab-http-client".into())
                        .spawn(move || {
                            let _guard = ConnectionGuard(count);
                            let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                            let _ = stream.set_nodelay(true);
                            let request = read_request(&mut stream);
                            let head = request.as_ref().is_ok_and(|r| r.method == "HEAD");
                            let response = match request {
                                Ok(request) => service.respond(request),
                                Err((status, message)) => error(status, message),
                            };
                            let _ = write_response(&mut stream, response, head);
                        }) {
                        Ok(client) => clients.push(client),
                        Err(_) => {
                            active.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                }
                let _ = done.send(());
                for client in clients {
                    let _ = client.join();
                }
            })
            .context("start remote HTTP listener")?;
        Ok(Self {
            address,
            quitting,
            stopped,
            thread: Some(server_thread),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.quitting.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // Unblock accept(). Another socket on the loopback port can take
            // that connect instead (BSD lets 127.0.0.1 bind beside 0.0.0.0),
            // so unless accept is seen to stop, detach rather than risk
            // joining a thread that can never wake.
            let woke = TcpStream::connect_timeout(&wake_address(self.address), WAKE_TIMEOUT)
                .is_ok()
                && self.stopped.recv_timeout(WAKE_TIMEOUT).is_ok();
            if woke {
                let _ = thread.join();
            }
        }
    }
}

/// An address that reaches the listener from this host: loopback of the
/// same family when it listens on every interface.
fn wake_address(address: SocketAddr) -> SocketAddr {
    let ip = match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, address.port())
}

struct ConnectionGuard(Arc<AtomicUsize>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

type HttpResult<T> = std::result::Result<T, (u16, String)>;

fn read_request(stream: &mut TcpStream) -> HttpResult<Request> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut bytes = Vec::new();
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > MAX_HEADERS {
                return Err((431, "request headers are too large".into()));
            }
            break end + 4;
        }
        if bytes.len() >= MAX_HEADERS {
            return Err((431, "request headers are too large".into()));
        }
        let mut buffer = [0u8; 1024];
        let count = read_before(stream, &mut buffer, deadline)?;
        bytes.extend_from_slice(&buffer[..count]);
    };
    let header = std::str::from_utf8(&bytes[..end - 4])
        .map_err(|_| (400, "request headers must be valid UTF-8".into()))?;
    let mut request = parse_headers(header)?;
    let length = request
        .headers
        .get("content-length")
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err((400, "invalid Content-Length".into()));
            }
            value
                .parse::<usize>()
                .map_err(|_| (413, "request body is too large".into()))
        })
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err((413, "request body is too large".into()));
    }
    if request.headers.contains_key("transfer-encoding") {
        return Err((400, "Transfer-Encoding is not supported".into()));
    }
    if request.headers.contains_key("expect") {
        return Err((417, "Expect is not supported".into()));
    }
    if request.method != "POST" && length != 0 {
        return Err((400, "only POST requests may contain a body".into()));
    }
    request.body.extend_from_slice(&bytes[end..]);
    // A single framed request per socket prevents ambiguous pipelined bodies.
    if request.body.len() > length {
        return Err((400, "unexpected data after request body".into()));
    }
    while request.body.len() < length {
        let mut buffer = [0u8; 8192];
        let remaining = (length - request.body.len()).min(buffer.len());
        let count = read_before(stream, &mut buffer[..remaining], deadline)?;
        request.body.extend_from_slice(&buffer[..count]);
    }
    Ok(request)
}

fn read_before(stream: &mut TcpStream, buffer: &mut [u8], deadline: Instant) -> HttpResult<usize> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err((408, "request timed out".into()));
    }
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| (400, "could not read request".into()))?;
    match stream.read(buffer) {
        Ok(0) => Err((400, "incomplete request".into())),
        Ok(count) => Ok(count),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            Err((408, "request timed out".into()))
        }
        Err(_) => Err((400, "could not read request".into())),
    }
}

fn parse_headers(header: &str) -> HttpResult<Request> {
    let mut lines = header.split("\r\n");
    let parts: Vec<_> = lines.next().unwrap_or("").split(' ').collect();
    if parts.len() != 3
        || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1")
        || !parts[0].bytes().all(|b| b.is_ascii_uppercase())
        || parts[0].is_empty()
        || !parts[1].starts_with('/')
        || parts[1].contains('#')
        || !parts[1].bytes().all(|b| b.is_ascii_graphic())
    {
        return Err((400, "invalid HTTP request line".into()));
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| (400, "invalid HTTP header".into()))?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
        {
            return Err((400, "invalid HTTP header".into()));
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return Err((400, "duplicate HTTP header".into()));
        }
    }
    let host = headers
        .get("host")
        .ok_or_else(|| (400, "Host header is required".into()))?;
    if !valid_authority(host) {
        return Err((400, "invalid Host header".into()));
    }
    Ok(Request {
        method: parts[0].into(),
        target: parts[1].into(),
        headers,
        body: Vec::new(),
    })
}

fn valid_authority(value: &str) -> bool {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':' | b'[' | b']'))
    {
        return false;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let Some((address, suffix)) = rest.split_once(']') else {
            return false;
        };
        return address.parse::<std::net::Ipv6Addr>().is_ok()
            && (suffix.is_empty()
                || suffix
                    .strip_prefix(':')
                    .is_some_and(|port| port.parse::<u16>().is_ok()));
    }
    let (host, port) = value
        .split_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        && port.is_none_or(|port| port.parse::<u16>().is_ok())
}

fn same_origin(request: &Request) -> bool {
    if request
        .headers
        .get("sec-fetch-site")
        .is_some_and(|site| !matches!(site.as_str(), "same-origin" | "none"))
    {
        return false;
    }
    let Some(origin) = request.headers.get("origin") else {
        // Network reachability grants access to non-browser clients.
        return true;
    };
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https")
        && valid_authority(authority)
        && request
            .headers
            .get("host")
            .is_some_and(|host| authority.eq_ignore_ascii_case(host))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandRequest {
    version: u32,
    camera: Option<String>,
    actor: Option<SessionActor>,
    #[serde(deserialize_with = "crate::session::deserialize_command")]
    command: SessionCommand,
}

fn validate_remote_command(command: &SessionCommand) -> Result<()> {
    if let SessionCommand::Capture { output, .. }
    | SessionCommand::Schedule { output, .. }
    | SessionCommand::Forward { output, .. } = command
    {
        ensure!(
            output != "-",
            "remote capture requires a server-side file or destination"
        );
    }
    command.validate()
}

impl Service {
    fn respond(&self, request: Request) -> Response {
        let (path, query) = request
            .target
            .split_once('?')
            .unwrap_or((&request.target, ""));
        if matches!(path, "/health" | "/metrics") {
            if !matches!(request.method.as_str(), "GET" | "HEAD") {
                return method_not_allowed("GET, HEAD");
            }
            return match path {
                "/health" => self.health(),
                _ => self.metrics(),
            };
        }
        if path == "/api" || path.starts_with("/api/") {
            if !same_origin(&request) {
                return error(403, "cross-origin requests are not permitted");
            }
            match (request.method.as_str(), path) {
                ("GET", "/api/session") => success(self.handle.snapshot()),
                ("GET", "/api/activity") => self.activity(query),
                ("GET" | "HEAD", "/api/analytics") => match self.handle.analytics().snapshot() {
                    Ok(snapshot) => success(snapshot),
                    Err(err) => error(503, format!("analytics unavailable: {err:#}")),
                },
                ("GET" | "HEAD", "/api/analytics.svg") => self.analytics_svg(query),
                #[cfg(feature = "ai")]
                ("GET" | "HEAD", "/api/measurements") => {
                    match self.handle.analytics().measurement_history() {
                        Ok(samples) => success(samples),
                        Err(err) => error(503, format!("measurements unavailable: {err:#}")),
                    }
                }
                #[cfg(feature = "ai")]
                ("GET" | "HEAD", "/api/measurements.svg") => {
                    match self.handle.analytics().measurement_svg() {
                        Ok(svg) => {
                            Response::new(200, "image/svg+xml; charset=utf-8", svg.into_bytes())
                        }
                        Err(err) => error(503, format!("measurements unavailable: {err:#}")),
                    }
                }
                ("GET", "/api/destinations") => match crate::destination::list() {
                    Ok(destinations) => success(json!({
                        "destinations": destinations.iter().map(|destination| json!({
                            "name": destination.name,
                            "kind": destination.target.kind(),
                            "location": destination.describe(),
                        })).collect::<Vec<_>>()
                    })),
                    Err(err) => error(500, format!("{err:#}")),
                },
                ("GET", "/api/storage") => match crate::storage::quota_status() {
                    Ok(quota) => success(quota),
                    Err(err) => error(500, format!("{err:#}")),
                },
                ("GET", "/api/preview") => self.preview(
                    query,
                    request.headers.get("if-none-match").map(String::as_str),
                ),
                ("GET" | "HEAD", "/api/signals") => match signals_query(query) {
                    Ok(history) => {
                        let signals = self.handle.signals();
                        success(json!({
                            "sources": signals.status(history),
                            "persisting": signals.has_sink(),
                            "dropped": signals.dropped_total(),
                            "sink_error": signals.sink_error(),
                        }))
                    }
                    Err(message) => error(400, message),
                },
                ("POST", "/api/command") => self.command(&request),
                (_, "/api/analytics" | "/api/analytics.svg" | "/api/signals") => {
                    method_not_allowed("GET, HEAD")
                }
                #[cfg(feature = "ai")]
                (_, "/api/measurements" | "/api/measurements.svg") => {
                    method_not_allowed("GET, HEAD")
                }
                (
                    _,
                    "/api/session" | "/api/activity" | "/api/destinations" | "/api/storage"
                    | "/api/preview",
                ) => method_not_allowed("GET"),
                (_, "/api/command") => method_not_allowed("POST"),
                _ => error(404, "API route not found"),
            }
        } else if !matches!(request.method.as_str(), "GET" | "HEAD") {
            method_not_allowed("GET, HEAD")
        } else {
            self.asset(path)
        }
    }

    /// Health and metrics monitor the session without anyone viewing it, so
    /// a probe every few seconds leaves idle workers on the slow poll.
    fn health(&self) -> Response {
        let snapshot = self.handle.snapshot_unwatched();
        let analytics = self.handle.analytics().health();
        let ready =
            !self.quitting.load(Ordering::Acquire) && self.handle.is_running() && analytics.ready;
        json_response(
            if ready { 200 } else { 503 },
            json!({
                "status": if ready { "ok" } else { "unavailable" },
                "ready": ready,
                "version": env!("CARGO_PKG_VERSION"),
                "uptime_seconds": self.handle.uptime_seconds(),
                "cameras": {
                    "connected": snapshot.cameras.len(),
                    "streaming": snapshot.cameras.iter().filter(|camera| camera.streaming).count(),
                },
                "analytics": analytics,
            }),
        )
    }

    fn metrics(&self) -> Response {
        match self
            .handle
            .analytics()
            .metrics(&self.handle.snapshot_unwatched())
        {
            Ok(metrics) => Response::new(
                200,
                "text/plain; version=0.0.4; charset=utf-8",
                metrics.into_bytes(),
            ),
            Err(err) => error(503, format!("analytics unavailable: {err:#}")),
        }
    }

    fn command(&self, request: &Request) -> Response {
        if !request.headers.get("content-type").is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
        }) {
            return error(415, "commands require Content-Type: application/json");
        }
        let command: CommandRequest = match serde_json::from_slice(&request.body) {
            Ok(command) => command,
            Err(err) => return error(400, format!("invalid command: {err}")),
        };
        if command.version != 1 {
            return error(400, "unsupported protocol version; expected 1");
        }
        if let Some(camera) = &command.camera
            && let Err(err) = crate::session::validate_selector(camera)
        {
            return error(400, format!("{err:#}"));
        }
        if let Err(err) = validate_remote_command(&command.command) {
            return error(400, format!("{err:#}"));
        }
        let actor = command.actor.unwrap_or_else(|| SessionActor {
            id: "http".into(),
            name: "HTTP client".into(),
            kind: ActorKind::Human,
        });
        if let Err(err) = actor.validate() {
            return error(400, format!("{err:#}"));
        }
        let reply = match command.camera {
            Some(camera) => {
                self.handle
                    .submit_to_as(&camera, command.command, ActivityOrigin::Client, actor)
            }
            None => self
                .handle
                .submit_as(command.command, ActivityOrigin::Client, actor),
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(err) => return error(409, format!("{err:#}")),
        };
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            if self.quitting.load(Ordering::Acquire) {
                return error(503, "remote server is stopping");
            }
            if Instant::now() >= deadline {
                return error(
                    504,
                    "command is still running; check session status before retrying",
                );
            }
            match reply.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(value)) => return success(value),
                Ok(Err(err)) => return error(409, format!("{err:#}")),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return error(503, "camera coordinator stopped");
                }
            }
        }
    }

    fn analytics_svg(&self, query: &str) -> Response {
        let width = match analytics_width(query) {
            Ok(width) => width,
            Err(message) => return error(400, message),
        };
        let analytics = self.handle.analytics();
        let rendered = match width {
            Some(width) => analytics.snapshot().and_then(|snapshot| {
                crate::analytics::render_svg_with_options(
                    &snapshot,
                    crate::analytics::SvgOptions::interface(
                        width,
                        crate::analytics::SvgColors::default(),
                    ),
                )
            }),
            None => analytics.svg(),
        };
        match rendered {
            Ok(svg) => Response::new(200, "image/svg+xml; charset=utf-8", svg.into_bytes()),
            Err(err) => error(503, format!("analytics unavailable: {err:#}")),
        }
    }

    fn activity(&self, query: &str) -> Response {
        let (after, timeout) = match activity_query(query) {
            Ok(query) => query,
            Err(message) => return error(400, message),
        };
        let Some(after) = after else {
            return success(self.handle.activity());
        };
        let previous = self.activity_waits.fetch_add(1, Ordering::AcqRel);
        let _slot = ConnectionGuard(self.activity_waits.clone());
        if previous >= MAX_ACTIVITY_WAITS {
            return error(503, "too many activity watchers; retry shortly");
        }
        if self.quitting.load(Ordering::Acquire) {
            return error(503, "remote server is stopping");
        }
        // Wakes on a change, or when the session stops.
        self.handle.wait_for_activity(after, timeout);
        success(self.handle.activity())
    }

    /// The newest frame as a bounded display image. Clients share one encode
    /// per frame and format, or per `PREVIEW_SHARE` while a camera streams,
    /// and a client that already has the frame gets 304 with no encode at all.
    fn preview(&self, query: &str, if_none_match: Option<&str>) -> Response {
        let (camera, format) = match preview_query(query) {
            Ok(query) => query,
            Err(message) => return error(400, message),
        };
        let Some(camera) = camera.or_else(|| self.handle.active_camera()) else {
            return Response::new(204, "image/png", Vec::new());
        };
        let format = format.served();
        let slot = self.previews.slot(&camera, format);
        let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if cached
            .as_ref()
            .is_none_or(|preview| preview.encoded.elapsed() >= self.previews.share)
        {
            let Some((tag, frame)) = self.handle.latest_frame_tagged(&camera) else {
                return Response::new(204, "image/png", Vec::new());
            };
            let key = PreviewKey { frame: tag, format };
            // A frame larger than the camera's ring arrives downscaled; the
            // headers give the size the camera delivered.
            let (width, height, _) = self.handle.preview_of(&camera).unwrap_or((
                frame.width,
                frame.height,
                frame.pixel_format,
            ));
            let delivered = Delivered {
                id: frame.id,
                timestamp_ns: frame.timestamp_ns,
                width,
                height,
            };
            if cached.as_ref().is_none_or(|preview| preview.key != key) {
                if if_none_match.is_some_and(|tags| etag_matches(tags, &key.etag())) {
                    return preview_response(key, delivered, None);
                }
                self.previews.encodes.fetch_add(1, Ordering::Relaxed);
                let body = match encode_preview(&frame, format) {
                    Ok(body) => body,
                    Err(response) => return response,
                };
                *cached = Some(Preview {
                    key,
                    delivered,
                    body,
                    encoded: Instant::now(),
                });
            }
        }
        let preview = cached.as_ref().expect("a preview was cached above");
        let unchanged = if_none_match.is_some_and(|tags| etag_matches(tags, &preview.key.etag()));
        preview_response(
            preview.key,
            preview.delivered,
            (!unchanged).then(|| preview.body.clone()),
        )
    }

    fn asset(&self, encoded_path: &str) -> Response {
        let root = match &self.web_root {
            WebRoot::Folder(root) => root,
            WebRoot::Embedded(files) => {
                return match asset_name(encoded_path) {
                    Ok(name) => match files.iter().find(|(file, _)| *file == name) {
                        Some((_, data)) => {
                            Response::new(200, asset_type(Path::new(&name)), data.to_vec())
                        }
                        None => error(404, "web asset not found"),
                    },
                    Err((status, message)) => error(status, message),
                };
            }
        };
        let path = match asset_path(root, encoded_path) {
            Ok(path) => path,
            Err((status, message)) => return error(status, message),
        };
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(_) => return error(404, "web asset not found"),
        };
        if file
            .metadata()
            .map_or(true, |metadata| metadata.len() > MAX_ASSET)
        {
            return error(413, "web asset is too large");
        }
        let mut data = Vec::new();
        if (&mut file)
            .take(MAX_ASSET + 1)
            .read_to_end(&mut data)
            .is_err()
        {
            return error(500, "could not read web asset");
        }
        if data.len() as u64 > MAX_ASSET {
            return error(413, "web asset is too large");
        }
        Response::new(200, asset_type(&path), data)
    }
}

fn percent_decode(value: &str) -> std::result::Result<String, String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut source = value.bytes();
    while let Some(byte) = source.next() {
        if byte == b'%' {
            let high = source.next().and_then(|b| (b as char).to_digit(16));
            let low = source.next().and_then(|b| (b as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => bytes.push((high * 16 + low) as u8),
                _ => return Err("invalid percent encoding".into()),
            }
        } else {
            bytes.push(byte);
        }
    }
    let result =
        String::from_utf8(bytes).map_err(|_| "URL must contain valid UTF-8".to_string())?;
    if result.chars().any(char::is_control) {
        return Err("URL must not contain control characters".into());
    }
    Ok(result)
}

fn analytics_width(query: &str) -> std::result::Result<Option<u32>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    let value = query
        .strip_prefix("width=")
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or("analytics query accepts one unsigned width")?;
    value
        .parse::<u32>()
        .map(Some)
        .map_err(|_| "analytics width is too large".into())
}

/// `/api/preview` accepts one optional `camera` selector and one optional
/// `format`, in either order.
fn preview_query(query: &str) -> std::result::Result<(Option<String>, PreviewFormat), String> {
    let mut camera = None;
    let mut format = None;
    if !query.is_empty() {
        for field in query.split('&') {
            let (name, value) = field
                .split_once('=')
                .ok_or("preview query requires key=value")?;
            match name {
                "camera" if camera.is_none() => {
                    let selector = percent_decode(value)?;
                    crate::session::validate_selector(&selector)
                        .map_err(|err| format!("{err:#}"))?;
                    camera = Some(selector);
                }
                "format" if format.is_none() => {
                    format = Some(match value {
                        "png" => PreviewFormat::Png,
                        "jpeg" => PreviewFormat::Jpeg,
                        _ => return Err("preview format must be png or jpeg".into()),
                    });
                }
                _ => return Err("preview query accepts one camera and one format".into()),
            }
        }
    }
    Ok((camera, format.unwrap_or(PreviewFormat::Png)))
}

/// Whether an `If-None-Match` list names this entity tag. The header
/// compares weakly, so a `W/` prefix does not matter.
fn etag_matches(header: &str, etag: &str) -> bool {
    header
        .split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || tag.trim_start_matches("W/") == etag)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewFormat {
    Png,
    Jpeg,
}
impl PreviewFormat {
    /// JPEG needs libjpeg-turbo (the `jpeg` feature); without it previews
    /// stay PNG, and the content type says so.
    fn served(self) -> Self {
        if cfg!(feature = "jpeg") {
            self
        } else {
            Self::Png
        }
    }
    fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// One encoded preview: a frame, as `SessionHandle::latest_frame_tagged`
/// names it, in one format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewKey {
    frame: u64,
    format: PreviewFormat,
}
impl PreviewKey {
    fn etag(&self) -> String {
        let extension = match self.format {
            PreviewFormat::Png => "png",
            PreviewFormat::Jpeg => "jpeg",
        };
        format!("\"{:x}-{extension}\"", self.frame)
    }
}

/// The frame a preview shows, as the camera delivered it.
#[derive(Debug, Clone, Copy)]
struct Delivered {
    id: u64,
    timestamp_ns: u64,
    width: u32,
    height: u32,
}

/// A preview with its frame headers, or 304 without a body.
fn preview_response(key: PreviewKey, frame: Delivered, body: Option<Vec<u8>>) -> Response {
    let mut response = match body {
        Some(body) => Response::new(200, key.format.content_type(), body),
        None => Response::new(304, key.format.content_type(), Vec::new()),
    };
    response.headers.extend([
        ("ETag", key.etag()),
        ("X-Frame-Id", frame.id.to_string()),
        ("X-Frame-Timestamp-Ns", frame.timestamp_ns.to_string()),
        ("X-Frame-Width", frame.width.to_string()),
        ("X-Frame-Height", frame.height.to_string()),
    ]);
    response
}

/// The newest preview encoded for each camera and format, most recently used
/// last. Each slot has its own lock, held while encoding, so clients asking
/// for the same new frame wait for one encode while other cameras' previews
/// go ahead.
struct Previews {
    slots: Mutex<VecDeque<((String, PreviewFormat), PreviewSlot)>>,
    encodes: AtomicU64,
    share: Duration,
}
impl Default for Previews {
    fn default() -> Self {
        Self {
            slots: Mutex::default(),
            encodes: AtomicU64::new(0),
            share: PREVIEW_SHARE,
        }
    }
}
type PreviewSlot = Arc<Mutex<Option<Preview>>>;
struct Preview {
    key: PreviewKey,
    delivered: Delivered,
    body: Vec<u8>,
    encoded: Instant,
}
impl Previews {
    fn slot(&self, camera: &str, format: PreviewFormat) -> PreviewSlot {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = match slots
            .iter()
            .position(|((owner, owned), _)| owner == camera && *owned == format)
        {
            Some(index) => slots.remove(index).expect("position is in range").1,
            None => Arc::default(),
        };
        if slots.len() >= PREVIEW_SLOTS {
            slots.pop_front();
        }
        slots.push_back(((camera.to_owned(), format), slot.clone()));
        slot
    }
}

/// A frame scaled to fit 1280x720, as Gray PNG or JPEG when every pixel is
/// gray and RGB otherwise; display images need no alpha channel.
/// Largest remote preview.
const PREVIEW_BOUNDS: (u32, u32) = (1280, 720);

/// A preview's size and pixels, gray or RGB, as the encoders take them.
struct PreviewPixels {
    width: u32,
    height: u32,
    gray: bool,
    data: Vec<u8>,
}

/// A frame shrunk to the preview's bounds is gray or RGB already, so only
/// frames that need no shrinking, or cannot be shrunk, are converted to an
/// RGBA preview and packed again.
fn preview_pixels(frame: &Frame) -> Result<PreviewPixels> {
    let small = crate::frame::shrink_to(frame, PREVIEW_BOUNDS);
    if let Some(small) = &small
        && small.width <= PREVIEW_BOUNDS.0
        && small.height <= PREVIEW_BOUNDS.1
    {
        let (width, height) = (small.width, small.height);
        match small.pixel_format {
            crate::types::MONO8 => {
                return Ok(PreviewPixels {
                    width,
                    height,
                    gray: true,
                    data: small.data.clone(),
                });
            }
            crate::types::RGB8 => {
                let pixels = small.data.as_chunks::<3>().0;
                let gray = pixels.iter().all(|[r, g, b]| r == g && g == b);
                let data = if gray {
                    pixels.iter().map(|p| p[0]).collect()
                } else {
                    small.data.clone()
                };
                return Ok(PreviewPixels {
                    width,
                    height,
                    gray,
                    data,
                });
            }
            _ => {}
        }
    }
    let (width, height, rgba) = crate::frame::preview_rgba(
        small.as_ref().unwrap_or(frame),
        PREVIEW_BOUNDS.0,
        PREVIEW_BOUNDS.1,
    )?;
    Ok(packed(width, height, &rgba))
}

/// An RGBA preview as gray bytes when every pixel is gray, else RGB.
fn packed(width: u32, height: u32, rgba: &[u8]) -> PreviewPixels {
    let pixels = rgba.as_chunks::<4>().0;
    let gray = pixels.iter().all(|[r, g, b, _]| r == g && g == b);
    let data: Vec<u8> = if gray {
        pixels.iter().map(|p| p[0]).collect()
    } else {
        let mut rgb = Vec::with_capacity(pixels.len() * 3);
        for pixel in pixels {
            rgb.extend_from_slice(&pixel[..3]);
        }
        rgb
    };
    PreviewPixels {
        width,
        height,
        gray,
        data,
    }
}

fn encode_preview(frame: &Frame, format: PreviewFormat) -> std::result::Result<Vec<u8>, Response> {
    let PreviewPixels {
        width,
        height,
        gray,
        data,
    } = preview_pixels(frame).map_err(|err| error(422, format!("preview unavailable: {err:#}")))?;
    let encoded = match format {
        PreviewFormat::Jpeg => crate::jpeg::software(&crate::jpeg::Raster {
            width,
            height,
            channels: if gray { 1 } else { 3 },
            data,
        }),
        PreviewFormat::Png => (|| -> Result<Vec<u8>> {
            let mut png = Vec::new();
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(if gray {
                png::ColorType::Grayscale
            } else {
                png::ColorType::Rgb
            });
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&data)?;
            writer.finish()?;
            Ok(png)
        })(),
    };
    encoded.map_err(|err| error(500, format!("could not encode preview: {err:#}")))
}

fn activity_query(query: &str) -> std::result::Result<(Option<u64>, Duration), String> {
    let mut after = None;
    let mut timeout = None;
    if !query.is_empty() {
        for field in query.split('&') {
            let (name, value) = field
                .split_once('=')
                .ok_or("activity query requires key=value")?;
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("activity query values must be unsigned integers".into());
            }
            let value = value
                .parse::<u64>()
                .map_err(|_| "activity query integer is too large")?;
            match name {
                "after" if after.is_none() => after = Some(value),
                "timeout_ms" if timeout.is_none() && value <= 25_000 => timeout = Some(value),
                _ => {
                    return Err(
                        "activity query accepts one after and one timeout_ms (0..25000)".into(),
                    );
                }
            }
        }
    }
    Ok((after, Duration::from_millis(timeout.unwrap_or(20_000))))
}

/// `/api/signals` accepts only an optional `history=0|1`.
fn signals_query(query: &str) -> std::result::Result<bool, &'static str> {
    match query {
        "" | "history=0" => Ok(false),
        "history=1" => Ok(true),
        _ => Err("signals query accepts only history=0 or history=1"),
    }
}

/// A request path as a file path inside the web root, `/` naming index.html.
fn asset_name(encoded_path: &str) -> HttpResult<String> {
    let decoded = percent_decode(encoded_path).map_err(|message| (400, message))?;
    if !decoded.starts_with('/') || decoded.starts_with("//") || decoded.contains('\\') {
        return Err((400, "invalid web asset path".into()));
    }
    let relative = if decoded == "/" {
        "index.html"
    } else {
        &decoded[1..]
    };
    if Path::new(relative)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
        || decoded.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err((403, "web assets must stay inside the web root".into()));
    }
    Ok(relative.to_owned())
}

fn asset_path(root: &Path, encoded_path: &str) -> HttpResult<PathBuf> {
    let path = root
        .join(asset_name(encoded_path)?)
        .canonicalize()
        .map_err(|_| (404, "web asset not found".into()))?;
    if !path.starts_with(root) || !path.is_file() {
        return Err((403, "web assets must stay inside the web root".into()));
    }
    Ok(path)
}

fn asset_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}
impl Response {
    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            body,
        }
    }
}
fn success(result: impl serde::Serialize) -> Response {
    json_response(200, json!({"version": 1, "ok": true, "result": result}))
}
fn error(status: u16, message: impl Into<String>) -> Response {
    json_response(
        status,
        json!({"version": 1, "ok": false, "error": {"message": message.into()}}),
    )
}
fn json_response(status: u16, value: Value) -> Response {
    Response::new(
        status,
        "application/json; charset=utf-8",
        serde_json::to_vec(&value).expect("JSON values always serialize"),
    )
}
fn method_not_allowed(allow: &str) -> Response {
    let mut response = error(405, "HTTP method is not allowed for this route");
    response.headers.push(("Allow", allow.into()));
    response
}
fn write_response(stream: &mut TcpStream, response: Response, head: bool) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        422 => "Unprocessable Content",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\n",
        response.status, reason, response.content_type
    )?;
    // These responses never have a body; a length would describe the one a
    // 200 sends.
    if !matches!(response.status, 204 | 304) {
        write!(stream, "Content-Length: {}\r\n", response.body.len())?;
    }
    write!(
        stream,
        "Connection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\n",
    )?;
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    if !head {
        stream.write_all(&response.body)?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn shrunk_previews_skip_the_rgba_copy_and_match_it() {
        crate::efficiency::pin(false);
        let cases = [
            (crate::types::MONO8, 1, 2560, 1440),
            (crate::types::RGB8, 3, 2600, 1500),
            (0x0108_0009, 1, 5472, 3648),
            (crate::types::MONO8, 1, 640, 480),
        ];
        for (pixel_format, bytes, width, height) in cases {
            let frame = Frame {
                id: 1,
                width,
                height,
                pixel_format,
                timestamp_ns: 0,
                data: (0..(width * height) as usize * bytes)
                    .map(|i| (i * 7 % 251) as u8)
                    .collect(),
            };
            let fast = preview_pixels(&frame).unwrap();
            let (w, h, rgba) = crate::frame::preview_rgba(&frame, 1280, 720).unwrap();
            let slow = packed(w, h, &rgba);
            assert_eq!(
                (fast.width, fast.height, fast.gray),
                (slow.width, slow.height, slow.gray),
                "{pixel_format:#x}"
            );
            assert!(fast.data == slow.data, "{pixel_format:#x}");
            assert!(fast.width <= 1280 && fast.height <= 720);
        }
    }

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TempWebRoot(PathBuf);
    impl TempWebRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "capturefab-remote-unit-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("index.html"), "<html>capturefab</html>").unwrap();
            fs::write(path.join("capturefab_web_bg.wasm"), b"\0asm").unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for TempWebRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn service(handle: SessionHandle, web_root: WebRoot) -> Service {
        Service {
            handle,
            web_root,
            quitting: Arc::new(AtomicBool::new(false)),
            activity_waits: Arc::new(AtomicUsize::new(0)),
            // Every request sees the newest frame unless a test shares encodes.
            previews: Previews {
                share: Duration::ZERO,
                ..Previews::default()
            },
        }
    }

    #[test]
    fn headers_reject_smuggling_and_invalid_authorities() {
        for header in [
            "GET / HTTP/1.1\r\nHost: localhost\r\nHOST: localhost",
            "GET / HTTP/1.1\r\nHost: localhost\r\n Content-Length: 1",
            "GET / HTTP/1.1\r\nHost: localhost@evil.test",
            "GET / HTTP/1.1\r\nHost: [::1]x",
            "GET / HTTP/1.1\r\nHost: localhost\r\nX: value\nInjected: x",
            "GET http://localhost/ HTTP/1.1\r\nHost: localhost",
            "GET / HTTP/1.1",
        ] {
            assert!(parse_headers(header).is_err(), "{header}");
        }
        assert!(parse_headers("GET / HTTP/1.1\r\nHost: [::1]:7878").is_ok());
    }

    #[test]
    fn browser_origin_is_enforced_without_authentication() {
        let request = parse_headers(
            "GET /api/session HTTP/1.1\r\nHost: camera:7878\r\nOrigin: http://camera:7878\r\nSec-Fetch-Site: same-origin"
        ).unwrap();
        assert!(same_origin(&request));
        let request = parse_headers("GET /api/session HTTP/1.1\r\nHost: camera:7878").unwrap();
        assert!(same_origin(&request));
        for origin in ["null", "https://evil.test", "http://camera:7878/path"] {
            let mut request =
                parse_headers("GET /api/session HTTP/1.1\r\nHost: camera:7878").unwrap();
            request.headers.insert("origin".into(), origin.into());
            assert!(!same_origin(&request));
        }
        for site in ["same-site", "cross-site"] {
            let request = parse_headers(&format!(
                "GET /api/session HTTP/1.1\r\nHost: camera:7878\r\nSec-Fetch-Site: {site}"
            ))
            .unwrap();
            assert!(!same_origin(&request));
        }
    }

    #[test]
    fn previews_report_the_size_the_camera_delivered() {
        let root = TempWebRoot::new();
        let (handle, mut writer) = SessionHandle::fed(4096);
        let service = service(handle.clone(), WebRoot::Folder(root.0.clone()));
        assert_eq!(service.preview("", None).status, 204);
        // Larger than the ring, so the ring holds a downscaled preview.
        let large = crate::types::Frame {
            id: 1,
            width: 80,
            height: 60,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![7; 4800],
        };
        writer.write(&large, true).unwrap();
        for query in ["", "camera=cam"] {
            let response = service.preview(query, None);
            assert_eq!(response.status, 200);
            let header = |name| {
                response
                    .headers
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.as_str())
            };
            assert_eq!(header("X-Frame-Width"), Some("80"));
            assert_eq!(header("X-Frame-Height"), Some("60"));
        }
        handle.shutdown();
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response
            .headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    fn color(id: u64) -> Frame {
        Frame {
            id,
            width: 64,
            height: 48,
            pixel_format: crate::types::RGB8,
            timestamp_ns: id * 1000,
            data: (0..64 * 48 * 3).map(|i| (i * 7 % 251) as u8).collect(),
        }
    }

    #[test]
    fn previews_are_encoded_once_per_frame_for_every_client() {
        let root = TempWebRoot::new();
        let (handle, mut writer) = SessionHandle::fed(1 << 16);
        let service = service(handle.clone(), WebRoot::Folder(root.0.clone()));
        writer.write(&color(1), true).unwrap();
        let first = service.preview("camera=cam", None);
        let second = service.preview("", None);
        assert_eq!((first.status, second.status), (200, 200));
        assert_eq!(first.body, second.body);
        let etag = header(&first, "ETag").unwrap().to_owned();
        assert_eq!(etag, "\"1-png\"");
        assert_eq!(header(&second, "ETag"), Some(etag.as_str()));
        assert_eq!(service.previews.encodes.load(Ordering::Relaxed), 1);
        // A client that has the frame gets its headers and no body.
        for tags in [etag.clone(), format!("\"other\", W/{etag}"), "*".into()] {
            let unchanged = service.preview("camera=cam", Some(&tags));
            assert_eq!(unchanged.status, 304, "{tags}");
            assert!(unchanged.body.is_empty());
            assert_eq!(header(&unchanged, "ETag"), Some(etag.as_str()));
            assert_eq!(header(&unchanged, "X-Frame-Id"), Some("1"));
        }
        assert_eq!(service.previews.encodes.load(Ordering::Relaxed), 1);
        assert_eq!(
            service.preview("camera=cam", Some("\"1-jpeg\"")).status,
            200
        );
        writer.write(&color(2), true).unwrap();
        let next = service.preview("camera=cam", Some(&etag));
        assert_eq!(next.status, 200);
        assert_eq!(header(&next, "ETag"), Some("\"2-png\""));
        handle.shutdown();
    }

    #[test]
    fn frames_that_repeat_an_id_and_timestamp_still_get_a_new_preview() {
        let root = TempWebRoot::new();
        let (handle, mut writer) = SessionHandle::fed(1 << 16);
        let service = service(handle.clone(), WebRoot::Folder(root.0.clone()));
        // Scanned pages all carry id 0 and timestamp 0.
        let page = |shade: u8| Frame {
            id: 0,
            width: 64,
            height: 48,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![shade; 64 * 48],
        };
        writer.write(&page(10), true).unwrap();
        let first = service.preview("", None);
        writer.write(&page(200), true).unwrap();
        let second = service.preview("", header(&first, "ETag"));
        assert_eq!(second.status, 200);
        assert_ne!(second.body, first.body);
        assert_ne!(header(&second, "ETag"), header(&first, "ETag"));
        assert_eq!(header(&second, "X-Frame-Id"), Some("0"));
        handle.shutdown();
    }

    #[test]
    fn clients_asking_for_the_same_new_frame_share_one_encode() {
        let root = TempWebRoot::new();
        let (handle, mut writer) = SessionHandle::fed(1 << 16);
        let service = Arc::new(service(handle.clone(), WebRoot::Folder(root.0.clone())));
        writer.write(&color(1), true).unwrap();
        let clients: Vec<_> = (0..4)
            .map(|_| {
                let service = service.clone();
                thread::spawn(move || service.preview("camera=cam&format=png", None))
            })
            .collect();
        let bodies: Vec<_> = clients
            .into_iter()
            .map(|client| client.join().unwrap().body)
            .collect();
        assert!(bodies.iter().all(|body| *body == bodies[0]));
        assert_eq!(service.previews.encodes.load(Ordering::Relaxed), 1);
        handle.shutdown();
    }

    #[test]
    fn clients_share_a_recent_encode_while_the_camera_streams() {
        let root = TempWebRoot::new();
        let (handle, mut writer) = SessionHandle::fed(1 << 16);
        let mut service = service(handle.clone(), WebRoot::Folder(root.0.clone()));
        service.previews.share = Duration::from_millis(200);
        writer.write(&color(1), true).unwrap();
        let first = service.preview("", None);
        writer.write(&color(2), true).unwrap();
        // A newer frame, but the last encode is recent.
        let shared = service.preview("", None);
        assert_eq!(header(&shared, "ETag"), header(&first, "ETag"));
        assert_eq!(header(&shared, "X-Frame-Id"), Some("1"));
        assert_eq!(
            service.preview("", header(&first, "ETag")).status,
            304,
            "a client that has the shared preview needs nothing new"
        );
        assert_eq!(service.previews.encodes.load(Ordering::Relaxed), 1);
        thread::sleep(Duration::from_millis(250));
        let next = service.preview("", None);
        assert_eq!(header(&next, "X-Frame-Id"), Some("2"));
        assert_eq!(service.previews.encodes.load(Ordering::Relaxed), 2);
        handle.shutdown();
    }

    #[test]
    fn each_camera_and_format_has_a_cached_preview_until_newer_ones_need_room() {
        let previews = Previews::default();
        let first = previews.slot("sim:0", PreviewFormat::Png);
        assert!(Arc::ptr_eq(
            &first,
            &previews.slot("sim:0", PreviewFormat::Png)
        ));
        assert!(!Arc::ptr_eq(
            &first,
            &previews.slot("sim:0", PreviewFormat::Jpeg)
        ));
        for camera in 0..PREVIEW_SLOTS {
            previews.slot(&format!("other:{camera}"), PreviewFormat::Png);
        }
        assert_eq!(previews.slots.lock().unwrap().len(), PREVIEW_SLOTS);
        assert!(!Arc::ptr_eq(
            &first,
            &previews.slot("sim:0", PreviewFormat::Png)
        ));
    }

    #[test]
    fn previews_are_gray_for_mono_frames_and_rgb_for_color() {
        let root = TempWebRoot::new();
        let (handle, mut writer) = SessionHandle::fed(1 << 16);
        let service = service(handle.clone(), WebRoot::Folder(root.0.clone()));
        // The IHDR color type: 0 gray, 2 RGB, 6 RGBA.
        let color_type = |response: &Response| response.body[25];
        let mono = Frame {
            id: 1,
            width: 64,
            height: 48,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: (0..64 * 48).map(|i| i as u8).collect(),
        };
        writer.write(&mono, true).unwrap();
        let gray = service.preview("", None);
        assert_eq!(gray.content_type, "image/png");
        assert_eq!(color_type(&gray), 0);
        let decoded = png::Decoder::new(std::io::Cursor::new(&gray.body))
            .read_info()
            .unwrap()
            .info()
            .clone();
        assert_eq!((decoded.width, decoded.height), (64, 48));
        writer.write(&color(2), true).unwrap();
        assert_eq!(color_type(&service.preview("", None)), 2);
        let jpeg = service.preview("format=jpeg&camera=cam", None);
        assert_eq!(jpeg.status, 200);
        if cfg!(feature = "jpeg") {
            assert_eq!(jpeg.content_type, "image/jpeg");
            assert!(jpeg.body.starts_with(&[0xff, 0xd8]));
            assert_eq!(header(&jpeg, "ETag"), Some("\"2-jpeg\""));
        } else {
            assert_eq!(jpeg.content_type, "image/png");
        }
        handle.shutdown();
    }

    #[test]
    fn preview_queries_take_one_camera_and_one_format_in_either_order() {
        assert_eq!(preview_query("").unwrap(), (None, PreviewFormat::Png));
        assert_eq!(
            preview_query("format=jpeg&camera=sim%3A0").unwrap(),
            (Some("sim:0".into()), PreviewFormat::Jpeg)
        );
        assert_eq!(
            preview_query("format=png").unwrap(),
            (None, PreviewFormat::Png)
        );
        for query in [
            "format=gif",
            "format=jpeg&format=png",
            "camera=a&camera=b",
            "camera=a&",
            "size=1",
            "camera",
        ] {
            assert!(preview_query(query).is_err(), "{query}");
        }
        assert!(etag_matches("\"a\", \"b\"", "\"b\""));
        assert!(etag_matches("W/\"b\"", "\"b\""));
        assert!(!etag_matches("\"a\"", "\"b\""));
    }

    #[test]
    fn embedded_assets_are_served_by_path_and_stay_inside_the_root() {
        static FILES: &[(&str, &[u8])] = &[
            ("index.html", b"<html>embedded</html>"),
            ("pkg/app.js", b"export {}"),
        ];
        let service = service(SessionHandle::new(), WebRoot::Embedded(FILES));
        let index = service.asset("/");
        assert_eq!(index.status, 200);
        assert_eq!(index.body, b"<html>embedded</html>");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        let script = service.asset("/pkg/app.js");
        assert_eq!(script.content_type, "text/javascript; charset=utf-8");
        assert_eq!(service.asset("/missing.js").status, 404);
        for path in ["/../index.html", "/%2e%2e/index.html", "//index.html"] {
            assert!(matches!(service.asset(path).status, 400 | 403), "{path}");
        }
        service.handle.shutdown();
    }

    #[test]
    fn dropping_the_server_wakes_its_blocked_accept() {
        let root = TempWebRoot::new();
        let handle = SessionHandle::new();
        let server = Server::start(
            handle.clone(),
            "127.0.0.1:0".parse().unwrap(),
            WebRoot::Folder(root.0.clone()),
        )
        .unwrap();
        let address = server.local_addr();
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
        assert_eq!(
            wake_address("0.0.0.0:8787".parse().unwrap()),
            "127.0.0.1:8787".parse().unwrap()
        );
        assert_eq!(
            wake_address("[::]:8787".parse().unwrap()),
            "[::1]:8787".parse().unwrap()
        );
        assert_eq!(
            wake_address("100.64.0.1:8787".parse().unwrap()),
            "100.64.0.1:8787".parse().unwrap()
        );
        handle.shutdown();
    }

    #[test]
    fn dropping_the_server_returns_when_another_socket_takes_its_wakeup() {
        let root = TempWebRoot::new();
        let handle = SessionHandle::new();
        let server = Server::start(
            handle.clone(),
            "0.0.0.0:0".parse().unwrap(),
            WebRoot::Folder(root.0.clone()),
        )
        .unwrap();
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, server.local_addr().port()));
        // BSD lets this bind beside 0.0.0.0 and take its loopback connects;
        // Linux refuses it, so there the wakeup cannot go astray.
        let Ok(shadow) = TcpListener::bind(loopback) else {
            drop(server);
            handle.shutdown();
            return;
        };
        let (dropped, done) = mpsc::channel();
        thread::spawn(move || {
            drop(server);
            let _ = dropped.send(());
        });
        assert!(
            done.recv_timeout(Duration::from_secs(5)).is_ok(),
            "drop waited for a wakeup another socket accepted"
        );
        // A connect of its own lets the detached thread finish.
        drop(shadow);
        let _ = TcpStream::connect_timeout(&loopback, Duration::from_millis(200));
        handle.shutdown();
    }

    #[test]
    fn health_distinguishes_ready_session_from_shutdown() {
        let root = TempWebRoot::new();
        let handle = SessionHandle::new();
        let service = service(handle.clone(), WebRoot::Folder(root.0.clone()));
        let ready = service.health();
        assert_eq!(ready.status, 200);
        let ready: Value = serde_json::from_slice(&ready.body).unwrap();
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["cameras"]["connected"], 0);
        // Monitoring probes are nobody viewing: idle workers stay on the
        // slow poll.
        service.metrics();
        assert!(!handle.watching());

        service.quitting.store(true, Ordering::Release);
        let stopping = service.health();
        assert_eq!(stopping.status, 503);
        assert_eq!(
            serde_json::from_slice::<Value>(&stopping.body).unwrap()["ready"],
            false
        );
        service.quitting.store(false, Ordering::Release);
        handle.shutdown();
        let stopped = service.health();
        assert_eq!(stopped.status, 503);
        assert_eq!(
            serde_json::from_slice::<Value>(&stopped.body).unwrap()["status"],
            "unavailable"
        );
    }

    #[cfg(feature = "ai")]
    #[test]
    fn measurement_routes_expose_structured_history_and_enforce_origin() {
        let root = TempWebRoot::new();
        let handle = SessionHandle::new();
        let service = service(handle.clone(), WebRoot::Folder(root.0.clone()));
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        handle
            .analytics()
            .record_measurements(&[crate::analytics::MeasurementSample {
                timestamp_ms,
                camera_id: "sim:0".into(),
                frame_id: 1,
                definition: crate::ai::MeasurementDefinition {
                    id: "ribbon".into(),
                    label: "Ribbon length".into(),
                    prompt: "Estimate the length of ribbon in meters".into(),
                    unit: "m".into(),
                },
                provider: crate::ai::Provider::OpenAi,
                model: crate::ai::Provider::OpenAi.default_model().into(),
                result: crate::ai::MeasurementOutput {
                    id: "ribbon".into(),
                    value: None,
                    confidence: None,
                    note: "No scale reference".into(),
                },
            }])
            .unwrap();
        let request = parse_headers("GET /api/measurements HTTP/1.1\r\nHost: camera:7878").unwrap();
        let response = service.respond(request);
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["result"][0]["result"]["value"], Value::Null);
        assert_eq!(
            body["result"][0]["definition"]["prompt"],
            "Estimate the length of ribbon in meters"
        );
        for path in ["/api/measurements", "/api/measurements.svg"] {
            let get = parse_headers(&format!("GET {path} HTTP/1.1\r\nHost: camera:7878")).unwrap();
            assert_eq!(service.respond(get).status, 200);
            let cross_origin = parse_headers(&format!(
                "GET {path} HTTP/1.1\r\nHost: camera:7878\r\nOrigin: https://evil.test"
            ))
            .unwrap();
            assert_eq!(service.respond(cross_origin).status, 403);
            let post =
                parse_headers(&format!("POST {path} HTTP/1.1\r\nHost: camera:7878")).unwrap();
            assert_eq!(service.respond(post).status, 405);
        }
        handle.shutdown();
    }

    #[test]
    fn analytics_chart_route_adapts_to_the_requested_width_and_rejects_ambiguous_queries() {
        let root = TempWebRoot::new();
        let service = service(SessionHandle::new(), WebRoot::Folder(root.0.clone()));
        for (query, width, height) in [
            ("width=320", 320, 1040),
            ("width=0", 180, 1040),
            ("width=4294967295", 2048, 520),
        ] {
            let request = parse_headers(&format!(
                "GET /api/analytics.svg?{query} HTTP/1.1\r\nHost: camera:7878"
            ))
            .unwrap();
            let response = service.respond(request);
            assert_eq!(response.status, 200);
            let svg = std::str::from_utf8(&response.body).unwrap();
            let doc = roxmltree::Document::parse(svg).unwrap();
            assert_eq!(
                doc.root_element().attribute("width"),
                Some(width.to_string().as_str())
            );
            assert_eq!(
                doc.root_element().attribute("height"),
                Some(height.to_string().as_str())
            );
        }
        let original = service.handle.analytics().svg().unwrap();
        let response = service.respond(
            parse_headers("HEAD /api/analytics.svg HTTP/1.1\r\nHost: camera:7878").unwrap(),
        );
        assert_eq!(response.status, 200);
        assert_eq!(response.body, original.as_bytes());
        for query in [
            "width=",
            "width=-1",
            "width=2.5",
            "width=320&width=640",
            "width=320&",
            "other=320",
            "width=4294967296",
        ] {
            let request = parse_headers(&format!(
                "GET /api/analytics.svg?{query} HTTP/1.1\r\nHost: camera:7878"
            ))
            .unwrap();
            assert_eq!(service.respond(request).status, 400, "{query}");
        }
    }

    #[test]
    fn assets_cannot_escape_the_root_including_encoded_traversal() {
        let root = TempWebRoot::new();
        assert_eq!(validate_web_root(&root.0).unwrap(), root.0);
        assert_eq!(asset_path(&root.0, "/").unwrap(), root.0.join("index.html"));
        for path in [
            "/../secret",
            "/%2e%2e/secret",
            "/pkg/../../secret",
            "/pkg%2f..%2f..%2fsecret",
            "/%5c..%5csecret",
            "//etc/passwd",
            "/%00index.html",
        ] {
            assert!(asset_path(&root.0, path).is_err(), "{path}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(std::env::temp_dir(), root.0.join("outside")).unwrap();
            assert!(asset_path(&root.0, "/outside").is_err());
        }
    }

    #[test]
    fn commands_reject_extra_fields_and_stdout_before_dispatch() {
        assert!(
            serde_json::from_str::<CommandRequest>(
                r#"{"version":1,"command":{"op":"start","extra":true}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<CommandRequest>(
                r#"{"version":1,"extra":true,"command":{"op":"start"}}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<CommandRequest>(
            r#"{"version":1,"command":{"op":"set","feature":"Width","feature":"Height","value":"12"}}"#
        ).is_err());
        let command = SessionCommand::Capture {
            output: "-".into(),
            count: 1,
            timeout_ms: 1000,
            format: "png".into(),
            storage: Default::default(),
            destination: None,
            on_exists: Default::default(),
            context: Default::default(),
        };
        assert!(validate_remote_command(&command).is_err());
        assert_eq!(
            preview_query("camera=simulator%3Atest").unwrap(),
            (Some("simulator:test".into()), PreviewFormat::Png)
        );
        assert!(preview_query("camera=a&camera=b").is_err());
    }
}
#[test]
fn signals_query_accepts_only_history() {
    assert_eq!(signals_query(""), Ok(false));
    assert_eq!(signals_query("history=1"), Ok(true));
    for query in ["history=2", "history=1&history=1", "x=1", "history"] {
        assert!(signals_query(query).is_err(), "{query}");
    }
}

#[test]
fn activity_query_is_bounded_and_rejects_ambiguous_fields() {
    assert_eq!(activity_query("").unwrap().0, None);
    assert_eq!(
        activity_query("after=9&timeout_ms=0").unwrap(),
        (Some(9), Duration::ZERO)
    );
    for query in [
        "after=1&after=2",
        "timeout_ms=25001",
        "after=-1",
        "other=1",
        "after=",
        "after=1&",
    ] {
        assert!(activity_query(query).is_err(), "{query}");
    }
}
