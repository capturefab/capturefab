//! ONVIF WS-Discovery and Media1 profile/RTSP URI resolution.
//! SOAP uses bounded HTTP requests, WS UsernameToken PasswordDigest, and HTTP
//! Basic/Digest authentication. HTTPS SOAP is rejected explicitly.
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SOAP: usize = 4 * 1024 * 1024;
const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnvifDevice {
    pub endpoint: String,
    pub addresses: Vec<String>,
    pub scopes: Vec<String>,
    pub uuid: String,
    pub source: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnvifStream {
    pub token: String,
    pub name: String,
    pub encoding: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub uri: String,
}
fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("cannot generate ONVIF nonce: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn uuid() -> Result<String> {
    let h = random_hex()?;
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn child_text<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(str::trim)
}
fn xml_document(body: &str) -> Result<roxmltree::Document<'_>> {
    ensure!(
        body.len() <= MAX_SOAP,
        "ONVIF XML response exceeds size limit"
    );
    let doc = roxmltree::Document::parse(body).context("invalid ONVIF SOAP XML")?;
    if let Some(fault) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Fault")
    {
        bail!(
            "ONVIF SOAP fault: {}",
            child_text(fault, "Text")
                .or_else(|| child_text(fault, "faultstring"))
                .unwrap_or("unspecified fault")
        );
    }
    Ok(doc)
}
fn parse_discovery(xml: &str, source: SocketAddr) -> Result<Vec<OnvifDevice>> {
    let doc = xml_document(xml)?;
    let mut result = Vec::new();
    for matched in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "ProbeMatch")
    {
        let addresses = child_text(matched, "XAddrs")
            .unwrap_or("")
            .split_whitespace()
            .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            continue;
        }
        result.push(OnvifDevice {
            endpoint: addresses[0].clone(),
            addresses,
            scopes: child_text(matched, "Scopes")
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            uuid: child_text(matched, "Address").unwrap_or("").into(),
            source: source.to_string(),
        });
    }
    Ok(result)
}
pub fn discover(timeout: Duration) -> Result<Vec<OnvifDevice>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("cannot open ONVIF discovery socket")?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let xml = format!(
        "<?xml version='1.0'?><s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:a='http://schemas.xmlsoap.org/ws/2004/08/addressing' xmlns:d='http://schemas.xmlsoap.org/ws/2005/04/discovery' xmlns:dn='http://www.onvif.org/ver10/network/wsdl'><s:Header><a:MessageID>urn:uuid:{}</a:MessageID><a:To s:mustUnderstand='1'>urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To><a:Action s:mustUnderstand='1'>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</a:Action></s:Header><s:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></s:Body></s:Envelope>",
        uuid()?
    );
    let destination = (Ipv4Addr::new(239, 255, 255, 250), 3702);
    let mut sent = false;
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.is_loopback() {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = interface.addr {
                let sock = socket2::SockRef::from(&socket);
                if sock.set_multicast_if_v4(&v4.ip).is_ok()
                    && socket.send_to(xml.as_bytes(), destination).is_ok()
                {
                    sent = true;
                }
            }
        }
    }
    if !sent {
        socket
            .send_to(xml.as_bytes(), destination)
            .context("cannot send ONVIF WS-Discovery probe")?;
    }
    let deadline = Instant::now() + timeout;
    let mut buffer = [0u8; 65536];
    let mut devices = BTreeMap::new();
    while Instant::now() < deadline {
        match socket.recv_from(&mut buffer) {
            Ok((n, source)) => {
                if let Ok(xml) = std::str::from_utf8(&buffer[..n])
                    && let Ok(found) = parse_discovery(xml, source)
                {
                    for device in found {
                        devices.entry(device.endpoint.clone()).or_insert(device);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(devices.into_values().collect())
}
#[derive(Debug)]
struct HttpUrl {
    host: String,
    port: u16,
    authority: String,
    path: String,
}
fn parse_url(url: &str) -> Result<HttpUrl> {
    ensure!(
        !url.bytes().any(|b| b <= 32 || b == 127),
        "invalid ONVIF endpoint URL"
    );
    ensure!(
        !url.starts_with("https://"),
        "HTTPS ONVIF SOAP is not supported in this build; use a camera's HTTP ONVIF service endpoint (HTTPS media playback remains available through FFmpeg)"
    );
    let rest = url
        .strip_prefix("http://")
        .context("ONVIF endpoint must use http://")?;
    let (authority, path) = rest
        .split_once('/')
        .map(|(a, p)| (a, format!("/{p}")))
        .unwrap_or((rest, "/".into()));
    ensure!(
        !authority.is_empty()
            && !authority.contains('@')
            && !authority.contains('#')
            && !path.contains('#'),
        "ONVIF credentials must be supplied separately from the endpoint URL"
    );
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']').context("invalid IPv6 ONVIF endpoint")?;
        let port = if let Some(suffix) = authority[end + 1..].strip_prefix(':') {
            suffix.parse::<u16>()?
        } else {
            ensure!(
                authority[end + 1..].is_empty(),
                "invalid IPv6 endpoint port"
            );
            80
        };
        (authority[1..end].to_string(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_string(), port.parse::<u16>()?)
    } else {
        (authority.to_string(), 80)
    };
    ensure!(
        !host.is_empty() && port > 0,
        "invalid ONVIF endpoint host or port"
    );
    Ok(HttpUrl {
        host,
        port,
        authority: authority.into(),
        path,
    })
}
fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.take(limit as u64 + 1).read_until(b'\n', &mut out)?;
    ensure!(out.len() <= limit, "ONVIF HTTP header line is too long");
    ensure!(!out.is_empty(), "ONVIF HTTP response ended before headers");
    Ok(out)
}
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}
fn parse_http(mut reader: impl BufRead) -> Result<HttpResponse> {
    let status = read_line(&mut reader, 4096)?;
    let status = std::str::from_utf8(&status)?
        .split_whitespace()
        .nth(1)
        .context("malformed ONVIF HTTP status")?
        .parse::<u16>()?;
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut header_bytes = 0;
    loop {
        let line = read_line(&mut reader, 8192)?;
        header_bytes += line.len();
        ensure!(
            header_bytes <= 65536,
            "ONVIF HTTP headers exceed size limit"
        );
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        let line = std::str::from_utf8(&line)?.trim_end();
        let (name, value) = line
            .split_once(':')
            .context("malformed ONVIF HTTP header")?;
        let name = name.to_ascii_lowercase();
        headers
            .entry(name)
            .and_modify(|v| {
                v.push_str(", ");
                v.push_str(value.trim())
            })
            .or_insert_with(|| value.trim().into());
    }
    let mut body = Vec::new();
    if headers
        .get("transfer-encoding")
        .is_some_and(|s| s.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            let line = read_line(&mut reader, 256)?;
            let size = usize::from_str_radix(
                std::str::from_utf8(&line)?
                    .trim()
                    .split(';')
                    .next()
                    .unwrap_or(""),
                16,
            )
            .context("malformed HTTP chunk size")?;
            if size == 0 {
                break;
            }
            ensure!(
                body.len().checked_add(size).is_some_and(|n| n <= MAX_SOAP),
                "ONVIF HTTP body exceeds size limit"
            );
            let old = body.len();
            body.resize(old + size, 0);
            reader.read_exact(&mut body[old..])?;
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf)?;
            ensure!(crlf == *b"\r\n", "malformed HTTP chunk delimiter");
        }
    } else if let Some(length) = headers.get("content-length") {
        let length = length.parse::<usize>()?;
        ensure!(length <= MAX_SOAP, "ONVIF HTTP body exceeds size limit");
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    } else {
        reader.take(MAX_SOAP as u64 + 1).read_to_end(&mut body)?;
        ensure!(body.len() <= MAX_SOAP, "ONVIF HTTP body exceeds size limit");
    }
    Ok(HttpResponse {
        status,
        headers,
        body: String::from_utf8(body).context("ONVIF response is not UTF-8")?,
    })
}
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}
impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ONVIF request deadline exceeded",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ONVIF request deadline exceeded",
            ));
        }
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}
fn request(
    url: &HttpUrl,
    action: &str,
    body: &str,
    authorization: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse> {
    let addresses = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .context("cannot resolve ONVIF host")?
        .collect::<Vec<_>>();
    ensure!(!addresses.is_empty(), "ONVIF host resolved to no addresses");
    let deadline = Instant::now() + timeout;
    let mut last = None;
    let mut stream = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "ONVIF connection timed out");
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| {
        anyhow!(
            "cannot connect to ONVIF service: {}",
            last.map(|e| e.to_string()).unwrap_or_default()
        )
    })?;
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    stream.set_read_timeout(Some(remaining))?;
    stream.set_write_timeout(Some(remaining))?;
    let mut stream = DeadlineStream { stream, deadline };
    let auth = authorization
        .map(|a| format!("Authorization: {a}\r\n"))
        .unwrap_or_default();
    let header = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/soap+xml; charset=utf-8; action=\"{}\"\r\nSOAPAction: \"{}\"\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        url.path,
        url.authority,
        action,
        action,
        body.len(),
        auth
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    parse_http(BufReader::new(stream))
}
fn utc_timestamp(seconds: u64) -> String {
    let days = (seconds / 86400) as i64;
    let day_seconds = seconds % 86400;
    // Gregorian civil date from days since Unix epoch, independent of local TZ.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60
    )
}
fn security(credentials: &Credentials) -> Result<String> {
    ensure!(
        credentials.username.len() <= 1024 && credentials.password.len() <= 4096,
        "ONVIF credentials exceed size limit"
    );
    ensure!(
        !credentials.username.chars().any(char::is_control)
            && !credentials.password.chars().any(char::is_control),
        "ONVIF credentials must not contain control characters"
    );
    let mut nonce = [0u8; 20];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("cannot generate ONVIF nonce: {e}"))?;
    let created = utc_timestamp(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
    let mut hash = Sha1::new();
    hash.update(nonce);
    hash.update(created.as_bytes());
    hash.update(credentials.password.as_bytes());
    let digest = STANDARD.encode(hash.finalize());
    Ok(format!(
        "<wsse:Security s:mustUnderstand='1' xmlns:wsse='http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd' xmlns:wsu='http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd'><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Password Type='http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest'>{digest}</wsse:Password><wsse:Nonce EncodingType='http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary'>{}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>",
        escape(&credentials.username),
        STANDARD.encode(nonce)
    ))
}
fn digest_fields(challenge: &str) -> Result<BTreeMap<String, String>> {
    let start = challenge
        .to_ascii_lowercase()
        .find("digest ")
        .context("server did not offer HTTP Digest authentication")?;
    let mut rest = &challenge[start + 7..];
    let mut result = BTreeMap::new();
    while !rest.trim().is_empty() {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_ascii_whitespace());
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let key = key.trim().to_ascii_lowercase();
        let after = after.trim_start();
        if let Some(quoted) = after.strip_prefix('"') {
            let mut value = String::new();
            let mut escaped = false;
            let mut end = None;
            for (i, c) in quoted.char_indices() {
                if escaped {
                    value.push(c);
                    escaped = false
                } else if c == '\\' {
                    escaped = true
                } else if c == '"' {
                    end = Some(i);
                    break;
                } else {
                    value.push(c)
                }
            }
            let end = end.context("unterminated HTTP Digest challenge")?;
            result.insert(key, value);
            rest = &quoted[end + 1..];
        } else {
            let (value, next) = after.split_once(',').unwrap_or((after, ""));
            result.insert(key, value.trim().into());
            rest = next;
        }
    }
    Ok(result)
}
fn md5_hex(s: &str) -> String {
    format!("{:x}", Md5::digest(s.as_bytes()))
}
fn quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
fn digest_authorization(
    challenge: &str,
    credentials: &Credentials,
    path: &str,
    cnonce: &str,
) -> Result<String> {
    ensure!(
        !challenge.contains(['\r', '\n']) && !credentials.username.contains(['\r', '\n']),
        "invalid HTTP Digest header characters"
    );
    let fields = digest_fields(challenge)?;
    let realm = fields
        .get("realm")
        .context("HTTP Digest challenge has no realm")?;
    let nonce = fields
        .get("nonce")
        .context("HTTP Digest challenge has no nonce")?;
    let algorithm = fields.get("algorithm").map(String::as_str).unwrap_or("MD5");
    ensure!(
        matches!(algorithm.to_ascii_lowercase().as_str(), "md5" | "md5-sess"),
        "unsupported HTTP Digest algorithm {algorithm}"
    );
    let qop = if let Some(qop) = fields.get("qop") {
        ensure!(
            qop.split(',').any(|q| q.trim() == "auth"),
            "HTTP Digest auth-int is unsupported"
        );
        Some("auth")
    } else {
        None
    };
    let mut ha1 = md5_hex(&format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    if algorithm.eq_ignore_ascii_case("MD5-sess") {
        ha1 = md5_hex(&format!("{ha1}:{nonce}:{cnonce}"));
    }
    let ha2 = md5_hex(&format!("POST:{path}"));
    let response = if qop.is_some() {
        md5_hex(&format!("{ha1}:{nonce}:00000001:{cnonce}:auth:{ha2}"))
    } else {
        md5_hex(&format!("{ha1}:{nonce}:{ha2}"))
    };
    let mut auth = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{response}\", algorithm={algorithm}",
        quoted(&credentials.username),
        quoted(realm),
        quoted(nonce),
        quoted(path)
    );
    if qop.is_some() {
        auth.push_str(&format!(
            ", qop=auth, nc=00000001, cnonce=\"{}\"",
            quoted(cnonce)
        ));
    }
    if let Some(opaque) = fields.get("opaque") {
        auth.push_str(&format!(", opaque=\"{}\"", quoted(opaque)));
    }
    Ok(auth)
}
fn soap(
    endpoint: &str,
    namespace: &str,
    operation: &str,
    content: &str,
    credentials: Option<&Credentials>,
    timeout: Duration,
) -> Result<String> {
    let url = parse_url(endpoint)?;
    let header = credentials.map(security).transpose()?.unwrap_or_default();
    let body = format!(
        "<?xml version='1.0' encoding='UTF-8'?><s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:tds='{DEVICE_NS}' xmlns:trt='{MEDIA_NS}' xmlns:tt='http://www.onvif.org/ver10/schema'><s:Header>{header}</s:Header><s:Body>{content}</s:Body></s:Envelope>"
    );
    let action = format!("{namespace}/{operation}");
    let first = request(&url, &action, &body, None, timeout)?;
    let response = if first.status == 401 {
        let credentials = credentials.context("ONVIF service requires credentials")?;
        let challenge = first
            .headers
            .get("www-authenticate")
            .context("ONVIF authentication challenge is missing")?;
        let authorization = if challenge.to_ascii_lowercase().contains("digest ") {
            digest_authorization(challenge, credentials, &url.path, &random_hex()?)?
        } else if challenge.to_ascii_lowercase().contains("basic") {
            format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{}", credentials.username, credentials.password))
            )
        } else {
            bail!("unsupported ONVIF HTTP authentication method")
        };
        request(&url, &action, &body, Some(&authorization), timeout)?
    } else {
        first
    };
    if !(200..300).contains(&response.status) {
        if !response.body.is_empty() {
            xml_document(&response.body)?;
        }
        bail!("ONVIF HTTP request failed with status {}", response.status)
    }
    xml_document(&response.body)?;
    Ok(response.body)
}
pub fn resolve(
    endpoint: &str,
    credentials: Option<&Credentials>,
    profile: Option<&str>,
    timeout: Duration,
) -> Result<Vec<OnvifStream>> {
    let capabilities = soap(
        endpoint,
        DEVICE_NS,
        "GetCapabilities",
        "<tds:GetCapabilities><tds:Category>Media</tds:Category></tds:GetCapabilities>",
        credentials,
        timeout,
    )?;
    let doc = xml_document(&capabilities)?;
    let media = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Media")
        .and_then(|n| child_text(n, "XAddr"))
        .context("ONVIF device exposes no Media1 service endpoint")?
        .to_string();
    let profiles = soap(
        &media,
        MEDIA_NS,
        "GetProfiles",
        "<trt:GetProfiles/>",
        credentials,
        timeout,
    )?;
    let doc = xml_document(&profiles)?;
    let mut result = Vec::new();
    let mut names = BTreeSet::new();
    for node in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Profiles")
    {
        let token = node
            .attribute("token")
            .context("ONVIF profile has no token")?;
        if profile.is_some_and(|requested| requested != token) {
            continue;
        }
        if !names.insert(token.to_string()) {
            continue;
        }
        let name = child_text(node, "Name").unwrap_or(token).to_string();
        let encoder = node
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "VideoEncoderConfiguration");
        let encoding = encoder
            .and_then(|n| child_text(n, "Encoding"))
            .map(str::to_string);
        let width = encoder
            .and_then(|n| child_text(n, "Width"))
            .and_then(|v| v.parse().ok());
        let height = encoder
            .and_then(|n| child_text(n, "Height"))
            .and_then(|v| v.parse().ok());
        let content = format!(
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
            escape(token)
        );
        let stream = soap(
            &media,
            MEDIA_NS,
            "GetStreamUri",
            &content,
            credentials,
            timeout,
        )?;
        let stream = xml_document(&stream)?;
        let uri = stream
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "Uri")
            .and_then(|n| n.text())
            .context("ONVIF GetStreamUri returned no URI")?
            .trim()
            .to_string();
        ensure!(
            matches!(
                uri.split(':').next(),
                Some("rtsp" | "rtsps" | "http" | "https")
            ),
            "ONVIF camera returned an unsupported streaming URI"
        );
        result.push(OnvifStream {
            token: token.into(),
            name,
            encoding,
            width,
            height,
            uri,
        });
    }
    ensure!(
        !result.is_empty(),
        "ONVIF profile was not found or the camera exposes no Media1 profiles"
    );
    Ok(result)
}

/// Recognize ONVIF locators before the general media URL handler.
pub fn is_source(source: &str) -> bool {
    source.starts_with("onvif:")
}
fn percent_decode(text: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut index = 0;
    let input = text.as_bytes();
    while index < input.len() {
        if input[index] == b'%' {
            ensure!(
                index + 2 < input.len(),
                "truncated ONVIF credential percent escape"
            );
            let hex = std::str::from_utf8(&input[index + 1..index + 3])?;
            bytes.push(
                u8::from_str_radix(hex, 16).context("invalid ONVIF credential percent escape")?,
            );
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(bytes).context("ONVIF credentials are not UTF-8")?;
    ensure!(
        !decoded.chars().any(char::is_control),
        "ONVIF credentials must not contain control characters"
    );
    Ok(decoded)
}
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}
fn source_endpoint(source: &str) -> Result<(String, Option<Credentials>)> {
    if let Some(endpoint) = source.strip_prefix("onvif:")
        && (endpoint.starts_with("http://") || endpoint.starts_with("https://"))
    {
        parse_url(endpoint)?;
        return Ok((endpoint.into(), None));
    }
    let rest = source.strip_prefix("onvif://").context(
        "ONVIF source must use onvif:http://HOST/service or onvif://USER:PASS@HOST/service",
    )?;
    let (authority, path) = rest
        .split_once('/')
        .map(|(a, p)| (a, format!("/{p}")))
        .unwrap_or((rest, "/onvif/device_service".into()));
    let (host, credentials) = if let Some((userinfo, host)) = authority.rsplit_once('@') {
        let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let username = percent_decode(username)?;
        ensure!(!username.is_empty(), "ONVIF username is empty");
        (
            host,
            Some(Credentials {
                username,
                password: percent_decode(password)?,
            }),
        )
    } else {
        (authority, None)
    };
    let endpoint = format!("http://{host}{path}");
    parse_url(&endpoint)?;
    Ok((endpoint, credentials))
}
fn authenticated_uri(uri: &str, credentials: Option<&Credentials>) -> Result<String> {
    let Some(credentials) = credentials else {
        return Ok(uri.into());
    };
    let (scheme, rest) = uri
        .split_once("://")
        .context("ONVIF stream URI has no authority")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    Ok(format!(
        "{scheme}://{}:{}@{host}{}",
        percent_encode(&credentials.username),
        percent_encode(&credentials.password),
        &rest[end..]
    ))
}
pub fn connect(
    source: &str,
    timeout: Duration,
) -> Result<(crate::types::CameraInfo, Box<dyn crate::types::Backend>)> {
    let (endpoint, credentials) = source_endpoint(source)?;
    let streams = resolve(&endpoint, credentials.as_ref(), None, timeout)?;
    let profile = &streams[0];
    let uri = authenticated_uri(&profile.uri, credentials.as_ref())?;
    let backend = crate::media::open_url(&uri, timeout)?;
    let safe_endpoint = crate::media::redact_url(&endpoint);
    let hash = safe_endpoint.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    let info = crate::types::CameraInfo {
        id: format!("onvif:{safe_endpoint}"),
        transport: crate::types::Transport::Media,
        vendor: "ONVIF".into(),
        model: profile.name.clone(),
        serial: format!("{hash:016x}"),
        address: Some(safe_endpoint),
    };
    Ok((info, backend))
}
pub fn camera_infos(timeout: Duration) -> Result<Vec<crate::types::CameraInfo>> {
    Ok(discover(timeout)?
        .into_iter()
        .map(|device| {
            let endpoint = crate::media::redact_url(&device.endpoint);
            let name = device
                .scopes
                .iter()
                .find_map(|scope| scope.strip_prefix("onvif://www.onvif.org/name/"))
                .and_then(|name| percent_decode(name).ok())
                .unwrap_or_else(|| "Network camera".into());
            crate::types::CameraInfo {
                id: format!("onvif:{endpoint}"),
                transport: crate::types::Transport::Media,
                vendor: "ONVIF".into(),
                model: name,
                serial: device.uuid,
                address: Some(endpoint),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_ignores_prefix_variations() {
        let xml = "<s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:d='http://schemas.xmlsoap.org/ws/2005/04/discovery' xmlns:a='http://schemas.xmlsoap.org/ws/2004/08/addressing'><s:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>urn:uuid:test</a:Address></a:EndpointReference><d:XAddrs>http://192.168.1.3/onvif/device_service http://host/service</d:XAddrs><d:Scopes>onvif://www.onvif.org/name/Camera</d:Scopes></d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>";
        let devices = parse_discovery(xml, "192.168.1.3:3702".parse().unwrap()).unwrap();
        assert_eq!(devices[0].uuid, "urn:uuid:test");
        assert_eq!(devices[0].addresses.len(), 2);
    }
    #[test]
    fn http_fixed_and_chunked_body_limits() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntest";
        assert_eq!(parse_http(BufReader::new(&fixed[..])).unwrap().body, "test");
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nte\r\n2\r\nst\r\n0\r\n\r\n";
        assert_eq!(
            parse_http(BufReader::new(&chunked[..])).unwrap().body,
            "test"
        );
        assert!(
            parse_http(BufReader::new(
                &b"HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n"[..]
            ))
            .is_err()
        );
    }
    #[test]
    fn ws_security_and_timestamps() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(951782400), "2000-02-29T00:00:00Z");
        let token = security(&Credentials {
            username: "a<b".into(),
            password: "never-print-me".into(),
        })
        .unwrap();
        assert!(token.contains("a&lt;b"));
        assert!(!token.contains("never-print-me"));
        assert!(
            roxmltree::Document::parse(&format!(
                "<Root xmlns:s='http://www.w3.org/2003/05/soap-envelope'>{token}</Root>"
            ))
            .is_ok()
        );
    }
    #[test]
    fn digest_auth_handles_quoted_commas() {
        let challenge =
            "Digest realm=\"camera, network\", nonce=\"abc\", qop=\"auth,auth-int\", algorithm=MD5";
        let fields = digest_fields(challenge).unwrap();
        assert_eq!(fields["realm"], "camera, network");
        let auth = digest_authorization(
            challenge,
            &Credentials {
                username: "user".into(),
                password: "pass".into(),
            },
            "/onvif/device_service",
            "random",
        )
        .unwrap();
        assert!(auth.contains("qop=auth"));
        assert!(!auth.contains("pass"));
        assert!(parse_url("https://camera/service").is_err());
        assert!(parse_url("http://user:pass@camera/service").is_err());
    }
    #[test]
    fn soap_fault_is_an_error() {
        assert!(xml_document("<Envelope><Body><Fault><Reason><Text>NotAuthorized</Text></Reason></Fault></Body></Envelope>").unwrap_err().to_string().contains("NotAuthorized"));
    }
    fn mock_request(stream: &TcpStream) -> (String, BTreeMap<String, String>, String) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let line = read_line(&mut reader, 4096).unwrap();
        let path = std::str::from_utf8(&line)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_string();
        let mut headers = BTreeMap::new();
        loop {
            let line = read_line(&mut reader, 8192).unwrap();
            if line == b"\r\n" {
                break;
            }
            let text = std::str::from_utf8(&line).unwrap();
            let (name, value) = text.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
        }
        let len = headers["content-length"].parse::<usize>().unwrap();
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        (path, headers, String::from_utf8(body).unwrap())
    }
    fn assert_username_token(body: &str, password: &str) {
        let doc = roxmltree::Document::parse(body).unwrap();
        let nonce = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "Nonce")
            .and_then(|n| n.text())
            .unwrap();
        let created = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "Created")
            .and_then(|n| n.text())
            .unwrap();
        let password_digest = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "Password")
            .and_then(|n| n.text())
            .unwrap();
        let mut digest = Sha1::new();
        digest.update(STANDARD.decode(nonce).unwrap());
        digest.update(created.as_bytes());
        digest.update(password.as_bytes());
        assert_eq!(password_digest, STANDARD.encode(digest.finalize()));
        assert!(!body.contains(password));
    }
    #[test]
    fn soap_http_digest_roundtrip_resolves_media_profile() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = format!("http://{address}/onvif/device_service");
        let media = format!("http://{address}/onvif/media");
        let credentials = Credentials {
            username: "admin".into(),
            password: "hidden-password".into(),
        };
        let server_credentials = credentials.clone();
        let server = std::thread::spawn(move || {
            let challenge =
                "Digest realm=\"Camera\", nonce=\"fixed-nonce\", qop=\"auth\", algorithm=MD5";
            for operation in ["GetCapabilities", "GetProfiles", "GetStreamUri"] {
                for authenticated in [false, true] {
                    let (mut stream, _) = listener.accept().unwrap();
                    let (path, headers, body) = mock_request(&stream);
                    assert_username_token(&body, &server_credentials.password);
                    assert!(headers["soapaction"].contains(operation));
                    if authenticated {
                        let supplied = &headers["authorization"];
                        let fields = digest_fields(supplied).unwrap();
                        let expected = digest_authorization(
                            challenge,
                            &server_credentials,
                            &path,
                            &fields["cnonce"],
                        )
                        .unwrap();
                        assert_eq!(*supplied, expected);
                        let response=match operation {
                            "GetCapabilities"=>format!("<Envelope><Body><GetCapabilitiesResponse><Capabilities><Media><XAddr>{media}</XAddr></Media></Capabilities></GetCapabilitiesResponse></Body></Envelope>"),
                            "GetProfiles"=>"<Envelope><Body><GetProfilesResponse><Profiles token='main'><Name>Main camera</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>640</Width><Height>480</Height></Resolution></VideoEncoderConfiguration></Profiles><Profiles token='small'><Name>Substream</Name></Profiles></GetProfilesResponse></Body></Envelope>".into(),
                            _=>{assert!(body.contains("<trt:ProfileToken>main</trt:ProfileToken>"));"<Envelope><Body><GetStreamUriResponse><MediaUri><Uri>rtsp://camera.example/live</Uri></MediaUri></GetStreamUriResponse></Body></Envelope>".into()},
                        };
                        write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
                    } else {
                        assert!(!headers.contains_key("authorization"));
                        write!(stream,"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {challenge}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    }
                }
            }
        });
        let streams = resolve(
            &endpoint,
            Some(&credentials),
            Some("main"),
            Duration::from_secs(2),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].uri, "rtsp://camera.example/live");
        assert_eq!(streams[0].encoding.as_deref(), Some("H264"));
        assert_eq!(
            (streams[0].width, streams[0].height),
            (Some(640), Some(480))
        );
    }
    #[test]
    fn soap_http_basic_and_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/service", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for authenticated in [false, true] {
                let (mut stream, _) = listener.accept().unwrap();
                let (_, headers, _) = mock_request(&stream);
                if authenticated {
                    assert_eq!(
                        headers["authorization"],
                        format!("Basic {}", STANDARD.encode("user:secret"))
                    );
                    let body = "<Envelope><Body><Response/></Body></Envelope>";
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                } else {
                    write!(stream,"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"camera\"\r\nContent-Length: 0\r\n\r\n").unwrap();
                }
            }
        });
        soap(
            &endpoint,
            DEVICE_NS,
            "Test",
            "<Test/>",
            Some(&Credentials {
                username: "user".into(),
                password: "secret".into(),
            }),
            Duration::from_secs(1),
        )
        .unwrap();
        server.join().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/service", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let start = Instant::now();
        assert!(
            soap(
                &endpoint,
                DEVICE_NS,
                "Test",
                "<Test/>",
                None,
                Duration::from_millis(50)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
    }
    #[test]
    fn locator_credentials_roundtrip_and_never_publish() {
        let (endpoint, credentials) =
            source_endpoint("onvif://admin:p%40ss%3Aword%2F%25@camera:8080/onvif/device_service")
                .unwrap();
        assert_eq!(endpoint, "http://camera:8080/onvif/device_service");
        let credentials = credentials.unwrap();
        assert_eq!(credentials.password, "p@ss:word/%");
        let uri = authenticated_uri("rtsp://camera/live", Some(&credentials)).unwrap();
        assert_eq!(uri, "rtsp://admin:p%40ss%3Aword%2F%25@camera/live");
        assert!(
            !crate::media::info(&uri)
                .unwrap()
                .address
                .unwrap()
                .contains("p%40ss")
        );
        assert!(source_endpoint("onvif://user:p%0Aass@camera/service").is_err());
        assert!(source_endpoint("onvif:https://camera/service").is_err());
        assert!(
            source_endpoint("onvif://user:pass@camera")
                .unwrap()
                .0
                .ends_with("/onvif/device_service")
        );
        assert!(is_source("onvif:http://camera/service"));
    }
}
