//! A loopback HTTP/1.1 server for camera hub tests, shared by unit tests
//! (`#[path]` from `src/hubs`) and the CLI tests. Every connection is served
//! on its own thread; the handler decides each reply. The request log
//! records methods, targets, header names and the authorization scheme, never
//! a credential, so tests can assert on it freely.
#![allow(dead_code)]

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

/// A request as the fake saw it.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    /// The path without its query.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
    /// The value of a query parameter, still percent-encoded.
    pub fn query(&self, key: &str) -> Option<&str> {
        self.target
            .split_once('?')?
            .1
            .split('&')
            .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
    }
    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")?.strip_prefix("Bearer ")
    }
}

/// Writes a streamed body. It may sleep between writes and returns when the
/// stream should end (or the client went away).
pub type Writer = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + Send>;

pub enum Body {
    Bytes(Vec<u8>),
    /// Sent with `Transfer-Encoding: chunked` in chunks of this size.
    Chunked(Vec<u8>, usize),
    /// Sent until the writer returns, then the connection is closed.
    Stream(Writer),
    /// Like `Stream`, with `Transfer-Encoding: chunked`: each write becomes
    /// one chunk.
    ChunkedStream(Writer),
    /// Nothing at all, not even the response head, is sent for this long;
    /// then the connection is closed.
    Stall(Duration),
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// Exactly these bytes, instead of a well-formed response.
    pub raw: Option<Vec<u8>>,
}

impl Reply {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Body::Bytes(Vec::new()),
            raw: None,
        }
    }
    pub fn json(body: &str) -> Self {
        Self::new(200)
            .header("Content-Type", "application/json")
            .bytes(body.as_bytes().to_vec())
    }
    pub fn text(status: u16, body: &str) -> Self {
        Self::new(status)
            .header("Content-Type", "text/plain")
            .bytes(body.as_bytes().to_vec())
    }
    pub fn image(content_type: &str, data: Vec<u8>) -> Self {
        Self::new(200)
            .header("Content-Type", content_type)
            .bytes(data)
    }
    pub fn raw(bytes: &[u8]) -> Self {
        let mut reply = Self::new(200);
        reply.raw = Some(bytes.to_vec());
        reply
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn bytes(mut self, data: Vec<u8>) -> Self {
        self.body = Body::Bytes(data);
        self
    }
    pub fn chunked(mut self, data: Vec<u8>, chunk: usize) -> Self {
        self.body = Body::Chunked(data, chunk);
        self
    }
    pub fn stream(mut self, writer: Writer) -> Self {
        self.body = Body::Stream(writer);
        self
    }
    pub fn chunked_stream(mut self, writer: Writer) -> Self {
        self.body = Body::ChunkedStream(writer);
        self
    }
    pub fn stall(mut self, time: Duration) -> Self {
        self.body = Body::Stall(time);
        self
    }
}

type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

/// TLS identity for an HTTPS fake: certificate and PKCS#8 key, both DER.
#[derive(Clone)]
pub struct Identity {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

pub struct FakeHttp {
    pub addr: SocketAddr,
    log: Arc<Mutex<Vec<Request>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    tls: bool,
}

impl FakeHttp {
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        Self::listen(None, Arc::new(handler))
    }
    pub fn start_tls(
        identity: Identity,
        handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    ) -> Self {
        Self::listen(Some(identity), Arc::new(handler))
    }
    fn listen(identity: Option<Identity>, handler: Arc<Handler>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake HTTP server");
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let tls = identity.is_some();
        let config = identity.map(|identity| {
            Arc::new(
                rustls::ServerConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![rustls::pki_types::CertificateDer::from(identity.cert)],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(identity.key.into()),
                )
                .expect("fake TLS identity"),
            )
        });
        {
            let (log, connections, stop) = (log.clone(), connections.clone(), stop.clone());
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    connections.fetch_add(1, Ordering::SeqCst);
                    let (log, handler, config) = (log.clone(), handler.clone(), config.clone());
                    thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        match config {
                            None => serve(stream, &log, handler.as_ref()),
                            Some(config) => {
                                let Ok(connection) = rustls::ServerConnection::new(config) else {
                                    return;
                                };
                                let tls = rustls::StreamOwned::new(connection, stream);
                                serve(tls, &log, handler.as_ref());
                            }
                        }
                    });
                }
            });
        }
        Self {
            addr,
            log,
            connections,
            stop,
            tls,
        }
    }
    /// `http://127.0.0.1:PORT`, or `https://localhost:PORT` for TLS (the
    /// fixture certificates name localhost).
    pub fn url(&self) -> String {
        if self.tls {
            format!("https://localhost:{}", self.addr.port())
        } else {
            format!("http://127.0.0.1:{}", self.addr.port())
        }
    }
    /// Every complete request so far.
    pub fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }
    /// One line per request: `GET /api/ auth=Bearer accept,host,...`.
    pub fn summary(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| {
                let scheme = request
                    .header("authorization")
                    .and_then(|value| value.split(' ').next())
                    .unwrap_or("none");
                let mut names = request
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>();
                names.sort();
                format!(
                    "{} {} auth={scheme} {}",
                    request.method,
                    request.target,
                    names.join(",")
                )
            })
            .collect()
    }
    /// TCP connections accepted, including ones that never sent a request.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the stop flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

fn serve<S: Read + Write>(stream: S, log: &Mutex<Vec<Request>>, handler: &Handler) {
    let mut reader = BufReader::new(stream);
    let Some(request) = read_request(&mut reader) else {
        return;
    };
    log.lock().unwrap().push(request.clone());
    let reply = handler(&request);
    let mut stream = reader.into_inner();
    let _ = write_reply(&mut stream, reply);
    let _ = stream.flush();
}

fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.trim_end().splitn(3, ' ');
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn write_reply(stream: &mut impl Write, reply: Reply) -> io::Result<()> {
    if let Some(raw) = reply.raw {
        return stream.write_all(&raw);
    }
    if let Body::Stall(time) = reply.body {
        thread::sleep(time);
        return Ok(());
    }
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match &reply.body {
        Body::Bytes(data) => head.push_str(&format!("Content-Length: {}\r\n", data.len())),
        Body::Chunked(..) | Body::ChunkedStream(_) => {
            head.push_str("Transfer-Encoding: chunked\r\n")
        }
        Body::Stream(_) | Body::Stall(_) => {}
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    match reply.body {
        Body::Bytes(data) => stream.write_all(&data),
        Body::Chunked(data, size) => {
            for chunk in data.chunks(size.max(1)) {
                stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())?;
                stream.write_all(chunk)?;
                stream.write_all(b"\r\n")?;
                stream.flush()?;
            }
            stream.write_all(b"0\r\n\r\n")
        }
        Body::Stream(writer) => {
            let mut flushing = Flushing(stream);
            writer(&mut flushing)
        }
        Body::ChunkedStream(writer) => {
            let mut chunks = Chunks(stream);
            writer(&mut chunks)?;
            chunks.0.write_all(b"0\r\n\r\n")
        }
        Body::Stall(_) => Ok(()),
    }
}

/// Writes each write as one chunk and flushes it.
struct Chunks<'a, W: Write>(&'a mut W);
impl<W: Write> Write for Chunks<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.0
            .write_all(format!("{:x}\r\n", buf.len()).as_bytes())?;
        self.0.write_all(buf)?;
        self.0.write_all(b"\r\n")?;
        self.0.flush()?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Flushes after every write, so streamed parts reach the client at once.
struct Flushing<'a, W: Write>(&'a mut W);
impl<W: Write> Write for Flushing<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(buf)?;
        self.0.flush()?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// The fixture identities in `tests/support/tls`.
pub fn localhost() -> Identity {
    Identity {
        cert: include_bytes!("tls/localhost.cert.der").to_vec(),
        key: include_bytes!("tls/localhost.key.der").to_vec(),
    }
}
pub fn other() -> Identity {
    Identity {
        cert: include_bytes!("tls/other.cert.der").to_vec(),
        key: include_bytes!("tls/other.key.der").to_vec(),
    }
}
pub fn frigate_default() -> Identity {
    Identity {
        cert: include_bytes!("tls/frigate-default.cert.der").to_vec(),
        key: include_bytes!("tls/frigate-default.key.der").to_vec(),
    }
}
pub fn ha_example_test() -> Identity {
    Identity {
        cert: include_bytes!("tls/ha.example.test.cert.der").to_vec(),
        key: include_bytes!("tls/ha.example.test.key.der").to_vec(),
    }
}
pub fn test_ca() -> Vec<u8> {
    include_bytes!("tls/ca.cert.der").to_vec()
}
/// SHA-256 fingerprints of the fixture certificates.
pub const LOCALHOST_SHA256: &str = "SHA256:11:B0:66:DE:27:79:90:5A:73:5F:6C:CB:92:3D:A4:B1:F0:33:68:A0:91:5D:F6:67:A0:36:3F:E1:6A:82:82:71";
pub const OTHER_SHA256: &str = "SHA256:EE:B2:AF:86:F6:E8:1E:9E:90:12:37:2C:6A:6D:16:D4:50:C9:61:1D:91:14:EE:82:E8:1E:74:64:94:9D:3E:CF";
pub const FRIGATE_DEFAULT_SHA256: &str = "SHA256:0D:3D:9C:A8:93:7D:EE:1C:4C:97:F7:FA:BD:EF:8D:03:20:82:25:5F:9D:78:84:3C:16:D1:1B:42:AF:8A:FC:E2";

/// A port nothing listens on: bound, read and released.
pub fn refused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
