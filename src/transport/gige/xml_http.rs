//! Bounded HTTP retrieval for `GenTL`'s optional vendor-hosted XML URLs (4.1.2.3):
//! <https://www.emva.org/wp-content/uploads/GenICam_GenTL_1_4.pdf>.
//! HTTP framing follows RFC 9112, sections 5–7. No credentials are accepted or
//! forwarded across redirects, and no non-HTTP scheme is followed.
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs as _},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const MAX_URL: usize = 8192;
const MAX_HEADERS: usize = 65_536;
const MAX_REDIRECTS: usize = 5;
static RESOLVING: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct Url {
    host: String,
    port: u16,
    authority: String,
    target: String,
}
impl Url {
    fn parse(text: &str) -> Result<Self> {
        ensure!(
            text.len() <= MAX_URL,
            "camera XML HTTP URL exceeds size limit"
        );
        ensure!(
            text.is_ascii()
                && !text
                    .bytes()
                    .any(|byte| byte <= b' ' || byte == 127 || byte == b'\\'),
            "camera XML HTTP URL contains invalid characters; percent-encode the path"
        );
        let (scheme, rest) = text
            .split_once("://")
            .context("malformed camera XML HTTP URL")?;
        if !scheme.eq_ignore_ascii_case("http") {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "camera XML retrieval supports Local: and plain HTTP URLs; HTTPS and other schemes are unsupported",
            ));
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        ensure!(
            !authority.is_empty() && !authority.contains('@'),
            "camera XML HTTP URL needs a host and must not contain credentials"
        );
        let (host, port) = if let Some(ipv6) = authority.strip_prefix('[') {
            let (host, suffix) = ipv6
                .split_once(']')
                .context("invalid camera XML IPv6 host")?;
            host.parse::<std::net::Ipv6Addr>()
                .context("invalid camera XML IPv6 host")?;
            let port = if suffix.is_empty() {
                80
            } else {
                suffix
                    .strip_prefix(':')
                    .context("invalid camera XML HTTP port")?
                    .parse::<u16>()?
            };
            (host, port)
        } else if let Some((host, port)) = authority.split_once(':') {
            (host, port.parse::<u16>()?)
        } else {
            (authority, 80)
        };
        ensure!(
            !host.is_empty() && port != 0,
            "invalid camera XML HTTP host or port"
        );
        ensure!(
            host.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-_:".contains(&byte)),
            "invalid camera XML HTTP host"
        );
        let tail = tail.split('#').next().unwrap_or_default();
        let target = if tail.is_empty() {
            "/".into()
        } else if tail.starts_with('?') {
            format!("/{tail}")
        } else {
            tail.to_owned()
        };
        Ok(Self {
            host: host.to_owned(),
            port,
            authority: authority.to_owned(),
            target: normalize_target(&target),
        })
    }
    fn redirect(&self, location: &str) -> Result<Self> {
        if location.contains("://") {
            return Self::parse(location);
        }
        if location.starts_with("//") {
            return Self::parse(&format!("http:{location}"));
        }
        ensure!(
            !location.split('/').next().unwrap_or_default().contains(':'),
            "camera XML redirect has an unsupported scheme"
        );
        let target = if location.is_empty() || location.starts_with('#') {
            format!("{}{location}", self.target)
        } else if location.starts_with('/') {
            location.to_owned()
        } else if location.starts_with('?') {
            format!(
                "{}{location}",
                self.target.split(['?', '#']).next().unwrap_or("/")
            )
        } else {
            let path = self.target.split('?').next().unwrap_or("/");
            let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            format!("{parent}/{location}")
        };
        Self::parse(&format!("http://{}{target}", self.authority))
    }
    fn name(&self) -> &str {
        self.target
            .split('?')
            .next()
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or_default()
    }
}

/// RFC 3986 5.2.4, preserving empty path segments and the untouched query.
fn normalize_target(target: &str) -> String {
    let (mut input, query) = target
        .split_once('?')
        .map_or((target, None), |(path, query)| (path, Some(query)));
    let mut output = String::new();
    while !input.is_empty() {
        if let Some(rest) = input
            .strip_prefix("../")
            .or_else(|| input.strip_prefix("./"))
        {
            input = rest;
        } else if input.starts_with("/./") {
            input = input.strip_prefix("/.").expect("matched /./ prefix");
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = input.strip_prefix("/..").expect("matched /.. prefix");
            if input.is_empty() {
                input = "/";
            }
            output.truncate(output.rfind('/').unwrap_or(0));
        } else if matches!(input, "." | "..") {
            input = "";
        } else {
            let length = input.strip_prefix('/').map_or_else(
                || input.find('/').unwrap_or(input.len()),
                |rest| rest.find('/').map_or(input.len(), |at| at + 1),
            );
            let (segment, rest) = input.split_at(length);
            output.push_str(segment);
            input = rest;
        }
    }
    if let Some(query) = query {
        output.push('?');
        output.push_str(query);
    }
    output
}

struct ResolverSlot;
impl Drop for ResolverSlot {
    fn drop(&mut self) {
        RESOLVING.fetch_sub(1, Ordering::AcqRel);
    }
}
fn addresses(url: &Url, deadline: Instant) -> Result<Vec<SocketAddr>> {
    if let Ok(ip) = url.host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, url.port)]);
    }
    // std's system resolver has no cancellation API. Bound both the caller's
    // wait and the number of resolver threads retained by a stalled resolver.
    let mut active = RESOLVING.load(Ordering::Acquire);
    loop {
        if active >= 16 {
            return Err(Categorized::error(
                ErrorKind::Unavailable,
                "camera XML DNS resolver is busy",
            ));
        }
        match RESOLVING.compare_exchange_weak(
            active,
            active + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(current) => active = current,
        }
    }
    let slot = ResolverSlot;
    let host = url.host.clone();
    let port = url.port;
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("xml-dns".into())
        .spawn(move || {
            let _slot = slot;
            let result = (host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.take(64).collect::<Vec<_>>());
            let _ = send.send(result);
        })?;
    receive
        .recv_timeout(remaining(deadline)?)
        .map_err(|_| timeout())?
        .map_err(|error| io_error(&error))
}

struct DeadlineSocket {
    stream: TcpStream,
    deadline: Instant,
}
impl Read for DeadlineSocket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(io_remaining(self.deadline)?))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineSocket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(io_remaining(self.deadline)?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
fn connect(url: &Url, deadline: Instant) -> Result<DeadlineSocket> {
    let mut error = None;
    for address in addresses(url, deadline)? {
        match TcpStream::connect_timeout(&address, remaining(deadline)?) {
            Ok(stream) => return Ok(DeadlineSocket { stream, deadline }),
            Err(failed) => error = Some(failed),
        }
    }
    Err(error.map_or_else(
        || {
            Categorized::error(
                ErrorKind::Unavailable,
                "camera XML host resolved to no addresses",
            )
        },
        |error| io_error(&error),
    ))
}
fn io_remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "camera XML HTTP deadline expired",
        ));
    }
    Ok(left)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    io_remaining(deadline).map_err(|error| io_error(&error))
}
fn timeout() -> anyhow::Error {
    Categorized::error(ErrorKind::Timeout, "camera XML HTTP retrieval timed out")
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        ErrorKind::Timeout
    } else {
        ErrorKind::Unavailable
    };
    Categorized::error(kind, format!("camera XML HTTP I/O: {error}"))
}

fn line(reader: &mut impl BufRead, limit: usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|error| io_error(&error))?;
    ensure!(
        line.len() <= limit && line.ends_with(b"\r\n"),
        "malformed or oversized camera XML HTTP line"
    );
    line.truncate(line.len() - 2);
    Ok(line)
}
fn headers(reader: &mut impl BufRead) -> Result<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    let mut bytes = 0usize;
    loop {
        let line = line(reader, 8192)?;
        bytes += line.len() + 2;
        ensure!(
            bytes <= MAX_HEADERS,
            "camera XML HTTP headers exceed size limit"
        );
        if line.is_empty() {
            return Ok(headers);
        }
        let text = std::str::from_utf8(&line).context("invalid camera XML HTTP header")?;
        let (name, value) = text
            .split_once(':')
            .context("malformed camera XML HTTP header")?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)),
            "invalid camera XML HTTP header name"
        );
        ensure!(
            !value
                .bytes()
                .any(|byte| (byte < 32 && byte != b'\t') || byte == 127),
            "invalid camera XML HTTP header value"
        );
        let value = value.trim();
        let name = name.to_ascii_lowercase();
        if let Some(previous) = headers.get_mut(&name) {
            if name == "content-length" {
                ensure!(
                    previous == value,
                    "conflicting camera XML HTTP content lengths"
                );
            } else {
                ensure!(
                    name != "location",
                    "duplicate camera XML HTTP redirect Location"
                );
                previous.push_str(", ");
                previous.push_str(value);
            }
        } else {
            headers.insert(name, value.to_owned());
        }
    }
}
fn head(reader: &mut impl BufRead) -> Result<(u16, BTreeMap<String, String>)> {
    for _ in 0..8 {
        let status = line(reader, 4096)?;
        let fields = std::str::from_utf8(&status)?
            .splitn(3, ' ')
            .collect::<Vec<_>>();
        ensure!(
            fields.len() >= 2
                && matches!(fields[0], "HTTP/1.0" | "HTTP/1.1")
                && fields[1].len() == 3,
            "malformed camera XML HTTP status"
        );
        let status = fields[1].parse::<u16>()?;
        let headers = headers(reader)?;
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        return Ok((status, headers));
    }
    Err(Categorized::error(
        ErrorKind::OperationFailed,
        "too many camera XML HTTP informational responses",
    ))
}

fn body(
    reader: &mut impl BufRead,
    headers: &BTreeMap<String, String>,
    limit: usize,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    if let Some(encoding) = headers.get("transfer-encoding") {
        ensure!(
            !headers.contains_key("content-length"),
            "ambiguous camera XML HTTP body framing"
        );
        ensure!(
            encoding.eq_ignore_ascii_case("chunked"),
            "unsupported camera XML HTTP transfer encoding"
        );
        loop {
            remaining(deadline)?;
            let size = line(reader, 1024)?;
            let size = std::str::from_utf8(&size)?
                .split(';')
                .next()
                .unwrap_or_default()
                .trim();
            ensure!(
                !size.is_empty() && size.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid camera XML HTTP chunk size"
            );
            let size = usize::from_str_radix(size, 16)?;
            if size == 0 {
                let trailers = self::headers(reader)?;
                ensure!(
                    !trailers.contains_key("content-length")
                        && !trailers.contains_key("transfer-encoding"),
                    "invalid camera XML HTTP framing trailer"
                );
                break;
            }
            let end = data
                .len()
                .checked_add(size)
                .filter(|end| *end <= limit)
                .context("camera XML HTTP body exceeds size limit")?;
            let start = data.len();
            data.resize(end, 0);
            reader
                .read_exact(&mut data[start..])
                .map_err(|error| io_error(&error))?;
            let mut crlf = [0; 2];
            reader
                .read_exact(&mut crlf)
                .map_err(|error| io_error(&error))?;
            ensure!(
                crlf == *b"\r\n",
                "malformed camera XML HTTP chunk delimiter"
            );
        }
    } else if let Some(length) = headers.get("content-length") {
        ensure!(
            !length.is_empty() && length.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid camera XML HTTP content length"
        );
        let length = length.parse::<usize>()?;
        ensure!(length <= limit, "camera XML HTTP body exceeds size limit");
        data.resize(length, 0);
        reader
            .read_exact(&mut data)
            .map_err(|error| io_error(&error))?;
    } else {
        reader
            .take((limit + 1) as u64)
            .read_to_end(&mut data)
            .map_err(|error| io_error(&error))?;
        ensure!(
            data.len() <= limit,
            "camera XML HTTP body exceeds size limit"
        );
    }
    if let Some(encoding) = headers.get("content-encoding") {
        let mut decoded = Vec::new();
        if encoding.eq_ignore_ascii_case("gzip") {
            flate2::read::MultiGzDecoder::new(data.as_slice())
                .take((limit + 1) as u64)
                .read_to_end(&mut decoded)?;
        } else if encoding.eq_ignore_ascii_case("deflate") {
            flate2::read::ZlibDecoder::new(data.as_slice())
                .take((limit + 1) as u64)
                .read_to_end(&mut decoded)?;
        } else {
            ensure!(
                encoding.eq_ignore_ascii_case("identity"),
                "unsupported camera XML HTTP content encoding"
            );
            return Ok(data);
        }
        ensure!(
            decoded.len() <= limit,
            "decoded camera XML HTTP body exceeds size limit"
        );
        data = decoded;
    }
    Ok(data)
}

pub(super) fn get(text: &str, timeout: Duration) -> Result<(String, Vec<u8>)> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("camera XML HTTP timeout overflow")?;
    get_until(text, deadline)
}

/// Fetch within one caller-owned absolute operation budget.
pub(super) fn get_until(text: &str, deadline: Instant) -> Result<(String, Vec<u8>)> {
    remaining(deadline)?;
    let mut url = Url::parse(text)?;
    for redirects in 0..=MAX_REDIRECTS {
        let mut socket = connect(&url, deadline)?;
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/xml, application/zip, */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
            url.target, url.authority
        );
        socket
            .write_all(request.as_bytes())
            .map_err(|error| io_error(&error))?;
        let mut reader = BufReader::new(socket);
        let (status, headers) = head(&mut reader)?;
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            ensure!(
                redirects < MAX_REDIRECTS,
                "too many camera XML HTTP redirects"
            );
            url = url.redirect(
                headers
                    .get("location")
                    .context("camera XML HTTP redirect has no Location")?,
            )?;
            continue;
        }
        if matches!(status, 401 | 407) {
            return Err(Categorized::error(
                ErrorKind::AuthRequired,
                "camera XML HTTP server requires authentication, which this retrieval path does not support",
            ));
        }
        ensure!(
            status == 200,
            "camera XML HTTP server returned status {status}"
        );
        let data = body(&mut reader, &headers, super::MAX_XML, deadline)?;
        remaining(deadline)?;
        return Ok((url.name().to_owned(), data));
    }
    Err(Categorized::error(
        ErrorKind::OperationFailed,
        "camera XML HTTP redirect limit exceeded",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_absolute_expiry_is_checked_before_dns_or_connect() {
        let error = get_until("http://127.0.0.1:9/never", Instant::now()).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    }

    use std::{io::Cursor, net::TcpListener, thread};

    fn server(responses: Vec<Vec<u8>>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "HTTP XML reader did not connect");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = Vec::new();
                    reader.read_until(b'\n', &mut line).unwrap();
                    assert!(!line.is_empty() && request.len() + line.len() <= 16_384);
                    request.extend_from_slice(&line);
                    if line == b"\r\n" {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                stream.write_all(&response).unwrap();
            }
            requests
        });
        (url, worker)
    }
    fn response(headers: &str, data: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{headers}\r\n",
            data.len()
        )
        .into_bytes();
        response.extend_from_slice(data);
        response
    }

    #[test]
    fn downloads_plain_chunked_compressed_and_zipped_xml_without_credentials() {
        let xml = b"<RegisterDescription/>";
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(xml).unwrap();
        let gzip = gzip.finish().unwrap();
        let mut deflate =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(xml).unwrap();
        let deflate = deflate.finish().unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("Camera.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(xml).unwrap();
        let zip = zip.finish().unwrap().into_inner();
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: ChUnKeD\r\n\r\n{:x};part=1\r\n",
            xml.len()
        )
        .into_bytes();
        chunked.extend_from_slice(xml);
        chunked.extend_from_slice(b"\r\n0\r\nX-Finish: yes\r\n\r\n");
        for (name, response) in [
            (
                "camera.xml",
                response("Set-Cookie: a=1\r\nSet-Cookie: b=2\r\n", xml),
            ),
            ("camera.xml", chunked),
            (
                "camera.xml",
                [b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), xml].concat(),
            ),
            ("camera.xml", response("Content-Encoding: gzip\r\n", &gzip)),
            (
                "camera.xml",
                response("Content-Encoding: deflate\r\n", &deflate),
            ),
            ("camera.zip", response("", &zip)),
        ] {
            let (url, worker) = server(vec![response]);
            // Exercise bounded hostname resolution as well as numeric URLs.
            let url = url.replace("127.0.0.1", "localhost");
            let (name, bytes) = get(
                &format!("{url}/{name}?SchemaVersion=1.1.0"),
                // Resolving and connecting to `localhost` is slow on some runners.
                crate::test_time::millis(2000),
            )
            .unwrap();
            assert_eq!(
                super::super::decode_xml(&name, bytes).unwrap(),
                "<RegisterDescription/>"
            );
            let requests = worker.join().unwrap();
            assert!(
                requests[0].starts_with(&format!("GET /{name}?SchemaVersion=1.1.0 HTTP/1.1\r\n"))
            );
            assert!(!requests[0].contains("Authorization:"));
        }
    }

    #[test]
    fn redirects_across_origins_and_resolves_relative_dot_segments() {
        let (target, final_server) = server(vec![response("", b"<XML/>")]);
        let (origin, redirects) = server(vec![
            b"HTTP/1.1 302 Found\r\nLocation: ../next.xml\r\nContent-Length: 0\r\n\r\n".to_vec(),
            format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {target}/camera.xml\r\nContent-Length: 0\r\n\r\n").into_bytes(),
        ]);
        assert_eq!(
            get(&format!("{origin}/first/start.xml"), Duration::from_secs(2))
                .unwrap()
                .1,
            b"<XML/>"
        );
        let requests = redirects.join().unwrap();
        assert!(requests[0].starts_with("GET /first/start.xml "));
        assert!(requests[1].starts_with("GET /next.xml "));
        assert!(final_server.join().unwrap()[0].starts_with("GET /camera.xml "));
        let base = Url::parse("http://camera/a/b?token=abc").unwrap();
        assert_eq!(base.redirect("#part").unwrap().target, "/a/b?token=abc");
        assert_eq!(base.redirect("?next=1").unwrap().target, "/a/b?next=1");
        assert_eq!(base.redirect("").unwrap().target, "/a/b?token=abc");
        for (input, expected) in [
            ("/a/b/../x?key=..", "/a/x?key=.."),
            ("/a//b/./x", "/a//b/x"),
            ("/a/../../x", "/x"),
        ] {
            assert_eq!(normalize_target(input), expected);
        }
    }

    #[test]
    fn malformed_framing_oversize_status_and_unsafe_redirects_fail_explicitly() {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: xchunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 16777217\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nshort",
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 302 Found\r\nLocation: file:///tmp/private\r\n\r\n",
            b"HTTP/1.1 302 Found\r\nLocation: http://user:password@example.com/xml\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nxXX",
        ] {
            let (url, worker) = server(vec![response.to_vec()]);
            get(&format!("{url}/camera.xml"), Duration::from_secs(2)).unwrap_err();
            worker.join().unwrap();
        }
        for url in [
            "http://host:0/xml",
            "http://a/b\r\nHeader:x",
            "http://user:password@host/xml",
            "http://host\\path/xml",
            "http://[bad]/xml",
        ] {
            Url::parse(url).unwrap_err();
        }
        let error = Url::parse("https://host/xml").unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
    }

    #[test]
    fn one_deadline_bounds_a_trickling_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/camera.xml", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                if line(&mut reader, 8192).unwrap().is_empty() {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .unwrap();
            for _ in 0..100 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let began = Instant::now();
        let error = get(&url, Duration::from_millis(80)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        assert!(began.elapsed() < Duration::from_millis(400));
        worker.join().unwrap();
    }
}
