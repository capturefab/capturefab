//! Same-origin HTTP control for a headless session. Camera operations retain the
//! coordinator's validation, ordering, job lifetime, and storage protections.
//! Access restrictions are supplied by the network serving the listener.
//!
//! This intentionally small HTTP/1 server closes every connection. Its parser,
//! socket deadlines, body limit, and connection limit bound untrusted clients
//! without introducing a second asynchronous runtime into the camera process.
use crate::session::{ActivityOrigin, ActorKind, SessionActor, SessionCommand, SessionHandle};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::RecvTimeoutError,
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
const POLL_INTERVAL: Duration = Duration::from_millis(25);

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
/// accepting requests and interrupts command waits; the session is independent.
pub struct Server {
    address: SocketAddr,
    quitting: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct Service {
    handle: SessionHandle,
    web_root: PathBuf,
    quitting: Arc<AtomicBool>,
    activity_waits: Arc<AtomicUsize>,
}

impl Server {
    /// Bind an explicit address. The CLI opts in with `--listen`; publishing on
    /// a tailnet uses the server's Tailscale address or a local HTTPS proxy.
    pub fn start(handle: SessionHandle, address: SocketAddr, web_root: PathBuf) -> Result<Self> {
        let web_root = validate_web_root(&web_root)?;
        let listener = TcpListener::bind(address).context("bind remote HTTP listener")?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let quitting = Arc::new(AtomicBool::new(false));
        let service = Arc::new(Service {
            handle,
            web_root,
            quitting: quitting.clone(),
            activity_waits: Arc::new(AtomicUsize::new(0)),
        });
        let active = Arc::new(AtomicUsize::new(0));
        let server_thread = thread::Builder::new()
            .name("capturefab-http".into())
            .spawn(move || {
                let mut clients = Vec::<JoinHandle<()>>::new();
                while !service.quitting.load(Ordering::Acquire) {
                    let mut remaining = Vec::with_capacity(clients.len());
                    for client in clients.drain(..) {
                        if client.is_finished() {
                            let _ = client.join();
                        } else {
                            remaining.push(client);
                        }
                    }
                    clients = remaining;
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // Accepted sockets inherit nonblocking mode on
                            // macOS. Client handlers use blocking I/O with
                            // deadlines so partial reads and backpressure
                            // cannot truncate requests or responses.
                            if stream.set_nonblocking(false).is_err() {
                                continue;
                            }
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
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::park_timeout(POLL_INTERVAL);
                        }
                        Err(_) => thread::park_timeout(POLL_INTERVAL),
                    }
                }
                for client in clients {
                    let _ = client.join();
                }
            })
            .context("start remote HTTP listener")?;
        Ok(Self {
            address,
            quitting,
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
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
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
                ("GET" | "HEAD", "/api/analytics.svg") => match self.handle.analytics().svg() {
                    Ok(svg) => Response::new(200, "image/svg+xml; charset=utf-8", svg.into_bytes()),
                    Err(err) => error(503, format!("analytics unavailable: {err:#}")),
                },
                ("GET", "/api/destinations") => match crate::destination::list() {
                    Ok(destinations) => success(json!({
                        "destinations": destinations.iter().map(|destination| json!({
                            "name": destination.name,
                            "location": destination.describe(),
                        })).collect::<Vec<_>>()
                    })),
                    Err(err) => error(500, format!("{err:#}")),
                },
                ("GET", "/api/storage") => match crate::storage::quota_status() {
                    Ok(quota) => success(quota),
                    Err(err) => error(500, format!("{err:#}")),
                },
                ("GET", "/api/preview") => self.preview(query),
                ("POST", "/api/command") => self.command(&request),
                (_, "/api/analytics" | "/api/analytics.svg") => method_not_allowed("GET, HEAD"),
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

    fn health(&self) -> Response {
        let snapshot = self.handle.snapshot();
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
        match self.handle.analytics().metrics(&self.handle.snapshot()) {
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
        let deadline = Instant::now() + timeout;
        while self.handle.activity_revision() == after && Instant::now() < deadline {
            if self.quitting.load(Ordering::Acquire) {
                return error(503, "remote server is stopping");
            }
            self.handle.wait_for_activity(
                after,
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            );
        }
        success(self.handle.activity())
    }

    fn preview(&self, query: &str) -> Response {
        let camera = match camera_query(query) {
            Ok(camera) => camera,
            Err(message) => return error(400, message),
        };
        let frame = match camera {
            Some(camera) => self.handle.latest_frame_for(&camera),
            None => self.handle.latest_frame(),
        };
        let Some(frame) = frame else {
            return Response::new(204, "image/png", Vec::new());
        };
        let (width, height, rgba) = match crate::frame::preview_rgba(&frame, 1280, 720) {
            Ok(image) => image,
            Err(err) => return error(422, format!("preview unavailable: {err:#}")),
        };
        let mut data = Vec::new();
        let encoded = (|| -> Result<()> {
            let mut encoder = png::Encoder::new(&mut data, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&rgba)?;
            writer.finish()?;
            Ok(())
        })();
        if let Err(err) = encoded {
            return error(500, format!("could not encode preview: {err:#}"));
        }
        let mut response = Response::new(200, "image/png", data);
        response.headers.extend([
            ("X-Frame-Id", frame.id.to_string()),
            ("X-Frame-Timestamp-Ns", frame.timestamp_ns.to_string()),
            ("X-Frame-Width", frame.width.to_string()),
            ("X-Frame-Height", frame.height.to_string()),
        ]);
        response
    }

    fn asset(&self, encoded_path: &str) -> Response {
        let path = match asset_path(&self.web_root, encoded_path) {
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

fn camera_query(query: &str) -> std::result::Result<Option<String>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    let (name, value) = query
        .split_once('=')
        .ok_or_else(|| "preview query must be camera=<selector>".to_owned())?;
    if name != "camera" || value.contains('&') {
        return Err("preview query only accepts one camera selector".into());
    }
    let camera = percent_decode(value)?;
    crate::session::validate_selector(&camera).map_err(|err| format!("{err:#}"))?;
    Ok(Some(camera))
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

fn asset_path(root: &Path, encoded_path: &str) -> HttpResult<PathBuf> {
    let decoded = percent_decode(encoded_path).map_err(|message| (400, message))?;
    if !decoded.starts_with('/') || decoded.starts_with("//") || decoded.contains('\\') {
        return Err((400, "invalid web asset path".into()));
    }
    let relative = if decoded == "/" {
        Path::new("index.html")
    } else {
        Path::new(&decoded[1..])
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
        || decoded.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err((403, "web assets must stay inside the web root".into()));
    }
    let path = root
        .join(relative)
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
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
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

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct WebRoot(PathBuf);
    impl WebRoot {
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
    impl Drop for WebRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
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
    fn health_distinguishes_ready_session_from_shutdown() {
        let root = WebRoot::new();
        let handle = SessionHandle::new();
        let service = Service {
            handle: handle.clone(),
            web_root: root.0.clone(),
            quitting: Arc::new(AtomicBool::new(false)),
            activity_waits: Arc::new(AtomicUsize::new(0)),
        };
        let ready = service.health();
        assert_eq!(ready.status, 200);
        let ready: Value = serde_json::from_slice(&ready.body).unwrap();
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["cameras"]["connected"], 0);

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

    #[test]
    fn assets_cannot_escape_the_root_including_encoded_traversal() {
        let root = WebRoot::new();
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
        };
        assert!(validate_remote_command(&command).is_err());
        assert_eq!(
            camera_query("camera=simulator%3Atest").unwrap().as_deref(),
            Some("simulator:test")
        );
        assert!(camera_query("camera=a&camera=b").is_err());
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
