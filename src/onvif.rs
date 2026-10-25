//! ONVIF WS-Discovery and Media1/Media2 profile/RTSP URI resolution.
//! SOAP uses bounded HTTP requests, WS UsernameToken PasswordDigest, and HTTP
//! Basic/Digest authentication. HTTPS SOAP is rejected explicitly.
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Sha256, Sha512_256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SOAP: usize = 4 * 1024 * 1024;
const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
const MEDIA2_NS: &str = "http://www.onvif.org/ver20/media/wsdl";
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
fn child_element<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
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
        let Some(endpoint) = device_endpoint(&addresses, source.ip()) else {
            continue;
        };
        result.push(OnvifDevice {
            endpoint,
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
/// The device service address to connect to among those a ProbeMatch lists
/// (one per scheme and address, in no set order, Core 7.3.2.3): HTTP before
/// the unsupported HTTPS, then the address the reply came from, IPv4, host
/// names and IPv6, with an IPv6 link-local address, unusable without its
/// zone, last. An unspecified host, which some cameras report, becomes the
/// reply's source address.
fn device_endpoint(addresses: &[String], source: IpAddr) -> Option<String> {
    let rank = |url: &String| {
        let host = host_and_port(url).map(|(host, _)| host).unwrap_or_default();
        let place = match host.parse::<IpAddr>() {
            Ok(ip) if ip == source || ip.is_unspecified() => 0,
            Ok(IpAddr::V4(_)) => 1,
            Err(_) => 2,
            Ok(IpAddr::V6(ip)) if ip.is_unicast_link_local() => 4,
            Ok(IpAddr::V6(_)) => 3,
        };
        (url.starts_with("https://"), place)
    };
    let url = addresses.iter().min_by_key(|url| rank(url))?;
    let Some((host, port)) = host_and_port(url) else {
        return Some(url.clone());
    };
    if !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified()) {
        return Some(url.clone());
    }
    let (scheme, rest) = url.split_once("://")?;
    let path = &rest[rest.find(['/', '?', '#']).unwrap_or(rest.len())..];
    Some(match source {
        IpAddr::V4(ip) => format!("{scheme}://{ip}:{port}{path}"),
        IpAddr::V6(ip) => format!("{scheme}://[{ip}]:{port}{path}"),
    })
}
/// Add a discovered device unless it is already listed: the same endpoint,
/// or the same EndpointReference (stable across interfaces, Core 7.3.1)
/// with an address in common, since cloned firmware can repeat one UUID.
fn merge_device(devices: &mut Vec<OnvifDevice>, device: OnvifDevice) {
    let known = devices.iter_mut().find(|known| {
        known.endpoint == device.endpoint
            || (!device.uuid.is_empty()
                && known.uuid == device.uuid
                && known.addresses.iter().any(|a| device.addresses.contains(a)))
    });
    match known {
        Some(known) => {
            for address in device.addresses {
                if !known.addresses.contains(&address) {
                    known.addresses.push(address);
                }
            }
        }
        None => devices.push(device),
    }
}
/// A WS-Discovery Probe for one type. ONVIF devices must list tds:Device
/// (Core 7.3.2.1); Profile S devices list dn:NetworkVideoTransmitter, and
/// some list only one of them. A Probe matches only devices that list all
/// its types, so each type gets its own Probe.
fn probe_message(message_id: &str, types: &str) -> String {
    format!(
        "<?xml version='1.0'?><s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:a='http://schemas.xmlsoap.org/ws/2004/08/addressing' xmlns:d='http://schemas.xmlsoap.org/ws/2005/04/discovery' xmlns:dn='http://www.onvif.org/ver10/network/wsdl' xmlns:tds='{DEVICE_NS}'><s:Header><a:MessageID>urn:uuid:{message_id}</a:MessageID><a:To s:mustUnderstand='1'>urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To><a:Action s:mustUnderstand='1'>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</a:Action></s:Header><s:Body><d:Probe><d:Types>{types}</d:Types></d:Probe></s:Body></s:Envelope>"
    )
}
/// When to send the Probes again after the first copy, for a first delay
/// drawn from 50..=250 ms: SOAP-over-UDP Appendix I for multicast, four
/// transmissions with the delay doubling up to 500 ms. WS-Discovery 2.4
/// requires it to make up for lost datagrams; devices answer one copy.
fn repeat_offsets(first_delay_ms: u64) -> [Duration; 3] {
    let mut delay = first_delay_ms.clamp(50, 250);
    let mut at = 0;
    [(); 3].map(|_| {
        at += delay;
        delay = (delay * 2).min(500);
        Duration::from_millis(at)
    })
}
pub fn discover(timeout: Duration) -> Result<Vec<OnvifDevice>> {
    validate_timeout(timeout)?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("cannot open ONVIF discovery socket")?;
    socket.set_multicast_ttl_v4(1)?;
    let probes = [
        probe_message(&uuid()?, "dn:NetworkVideoTransmitter"),
        probe_message(&uuid()?, "tds:Device"),
    ];
    let destination = (Ipv4Addr::new(239, 255, 255, 250), 3702);
    let interfaces = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(v4) => Some(v4.ip),
            _ => None,
        })
        .collect::<Vec<_>>();
    let send = || {
        let mut sent = false;
        for ip in &interfaces {
            let sock = socket2::SockRef::from(&socket);
            if sock.set_multicast_if_v4(ip).is_ok() {
                for xml in &probes {
                    sent |= socket.send_to(xml.as_bytes(), destination).is_ok();
                }
            }
        }
        sent
    };
    let start = Instant::now();
    let deadline = start + timeout;
    if !send() {
        for xml in &probes {
            socket
                .send_to(xml.as_bytes(), destination)
                .context("cannot send ONVIF WS-Discovery probe")?;
        }
    }
    let mut first_delay = [0u8; 2];
    getrandom::fill(&mut first_delay)
        .map_err(|e| anyhow!("cannot choose an ONVIF probe delay: {e}"))?;
    let mut repeats = repeat_offsets(50 + u64::from(u16::from_le_bytes(first_delay)) % 201)
        .map(|offset| start + offset)
        .into_iter()
        // Devices wait up to 500 ms before answering, so a later copy is late.
        .filter(|at| *at + Duration::from_millis(500) <= deadline)
        .peekable();
    let mut buffer = [0u8; 65536];
    let mut devices = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if repeats.next_if(|at| *at <= now).is_some() {
            send();
        }
        let wait = repeats
            .peek()
            .map_or(deadline, |at| (*at).min(deadline))
            .saturating_duration_since(now)
            .clamp(Duration::from_millis(1), Duration::from_millis(100));
        socket.set_read_timeout(Some(wait))?;
        match socket.recv_from(&mut buffer) {
            Ok((n, source)) => {
                if let Ok(xml) = std::str::from_utf8(&buffer[..n])
                    && let Ok(found) = parse_discovery(xml, source)
                {
                    for device in found {
                        merge_device(&mut devices, device);
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
    devices.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    Ok(devices)
}
#[derive(Debug)]
struct HttpUrl {
    host: String,
    port: u16,
    authority: String,
    path: String,
}
fn parse_url(url: &str) -> Result<HttpUrl> {
    parse_endpoint(url, false)
}
fn parse_endpoint(url: &str, allow_https: bool) -> Result<HttpUrl> {
    crate::media::validate_network_url(url, "ONVIF endpoint URL")?;
    ensure!(
        !url.bytes().any(|b| b <= 32 || b == 127),
        "invalid ONVIF endpoint URL"
    );
    let (protocol, rest) = url
        .split_once("://")
        .context("ONVIF endpoint must use http://")?;
    ensure!(
        allow_https || !protocol.eq_ignore_ascii_case("https"),
        "HTTPS ONVIF SOAP is not supported in this build; use a camera's HTTP ONVIF service endpoint (HTTPS media playback remains available through FFmpeg)"
    );
    ensure!(
        protocol.eq_ignore_ascii_case("http") || protocol.eq_ignore_ascii_case("https"),
        "ONVIF endpoint must use http:// or https://"
    );
    let default_port = if protocol.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let path: String = if rest[end..].starts_with('?') {
        format!("/{}", &rest[end..])
    } else if end == rest.len() {
        "/".into()
    } else {
        rest[end..].into()
    };
    ensure!(
        !url.contains('#'),
        "ONVIF endpoint URL must not contain a fragment; percent-encode a literal # in the service path"
    );
    ensure!(!authority.is_empty(), "ONVIF endpoint URL needs a host");
    ensure!(
        !authority.contains('@'),
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
            default_port
        };
        (authority[1..end].to_string(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_string(), port.parse::<u16>()?)
    } else {
        (authority.to_string(), default_port)
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
    /// Each `WWW-Authenticate` line, kept apart.
    challenges: Vec<String>,
    body: String,
}
/// The status, headers (repeats joined) and `WWW-Authenticate` lines of an
/// HTTP or RTSP response.
fn parse_head(reader: &mut impl BufRead) -> Result<(u16, BTreeMap<String, String>, Vec<String>)> {
    let status = read_line(reader, 4096)?;
    let status = std::str::from_utf8(&status)?
        .split_whitespace()
        .nth(1)
        .context("malformed ONVIF HTTP status")?
        .parse::<u16>()?;
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut challenges = Vec::new();
    let mut header_bytes = 0;
    loop {
        let line = read_line(reader, 8192)?;
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
        if name == "www-authenticate" {
            challenges.push(value.trim().to_owned());
        }
        headers
            .entry(name)
            .and_modify(|v| {
                v.push_str(", ");
                v.push_str(value.trim())
            })
            .or_insert_with(|| value.trim().into());
    }
    Ok((status, headers, challenges))
}
fn parse_http(mut reader: impl BufRead) -> Result<HttpResponse> {
    let (status, headers, challenges) = parse_head(&mut reader)?;
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
        challenges,
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
fn open_tcp(url: &HttpUrl, timeout: Duration) -> Result<DeadlineStream> {
    validate_timeout(timeout)?;
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
    Ok(DeadlineStream { stream, deadline })
}
fn exchange(
    mut stream: DeadlineStream,
    url: &HttpUrl,
    action: &str,
    body: &str,
    authorization: Option<&str>,
) -> Result<HttpResponse> {
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
fn request(
    url: &HttpUrl,
    action: &str,
    body: &str,
    authorization: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse> {
    exchange(open_tcp(url, timeout)?, url, action, body, authorization)
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
/// Seconds since the Unix epoch of a UTC civil date and time.
fn unix_seconds(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) * 86400 + hour * 3600 + minute * 60 + second
}
fn now_seconds() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}
/// The device clock minus the host clock, in seconds, from the device's
/// UTCDateTime in an unauthenticated GetSystemDateAndTime (Core 8.3.6,
/// PRE_AUTH). A device that answers without it gives `None`; one that cannot
/// be reached is an error.
fn clock_offset(device: &str, timeout: Duration) -> Result<Option<i64>> {
    let url = parse_url(device)?;
    let stream = open_tcp(&url, timeout)?;
    let Ok(response) = exchange(
        stream,
        &url,
        &format!("{DEVICE_NS}/GetSystemDateAndTime"),
        &envelope("", "<tds:GetSystemDateAndTime/>"),
        None,
    ) else {
        return Ok(None);
    };
    let now = now_seconds()?;
    if !(200..300).contains(&response.status) {
        return Ok(None);
    }
    let Ok(doc) = xml_document(&response.body) else {
        return Ok(None);
    };
    let Some(utc) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "UTCDateTime")
    else {
        return Ok(None);
    };
    let field = |name: &str, range: std::ops::RangeInclusive<i64>| {
        child_text(utc, name)
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| range.contains(v))
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        field("Year", 1970..=9999),
        field("Month", 1..=12),
        field("Day", 1..=31),
        field("Hour", 0..=23),
        field("Minute", 0..=59),
        field("Second", 0..=60),
    ) else {
        return Ok(None);
    };
    Ok(Some(
        unix_seconds(year, month, day, hour, minute, second) - now,
    ))
}
/// A WS-Security UsernameToken with a new nonce, created at the device's
/// time: devices reject a token whose Created is outside their own clock's
/// freshness window.
fn security(credentials: &Credentials, clock_offset: i64) -> Result<String> {
    validate_credentials(credentials)?;
    let mut nonce = [0u8; 20];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("cannot generate ONVIF nonce: {e}"))?;
    let created = utc_timestamp(now_seconds()?.saturating_add(clock_offset).max(0) as u64);
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
/// One HTTP authentication challenge: its scheme and parameters.
#[derive(Debug)]
pub(crate) struct Challenge {
    pub(crate) scheme: String,
    pub(crate) params: BTreeMap<String, String>,
}
/// Split `WWW-Authenticate` values into challenges (RFC 7235 4.1): a server
/// may repeat the header, and one value may hold several challenges.
pub(crate) fn challenges(values: &[String]) -> Result<Vec<Challenge>> {
    let mut result = Vec::new();
    for value in values {
        let mut rest = value.as_str();
        let mut current: Option<Challenge> = None;
        loop {
            rest = rest.trim_start_matches(|c: char| c == ',' || c.is_ascii_whitespace());
            if rest.is_empty() {
                break;
            }
            let end = rest
                .find(|c: char| c == '=' || c == ',' || c.is_ascii_whitespace())
                .unwrap_or(rest.len());
            let token = &rest[..end];
            let after = rest[end..].trim_start_matches([' ', '\t']);
            match after
                .strip_prefix('=')
                .map(|v| v.trim_start_matches([' ', '\t']))
            {
                Some(value) if !value.is_empty() && !value.starts_with([',', '=']) => {
                    let (value, next) = if let Some(quoted) = value.strip_prefix('"') {
                        let mut text = String::new();
                        let mut escaped = false;
                        let mut end = None;
                        for (i, c) in quoted.char_indices() {
                            if escaped {
                                text.push(c);
                                escaped = false
                            } else if c == '\\' {
                                escaped = true
                            } else if c == '"' {
                                end = Some(i);
                                break;
                            } else {
                                text.push(c)
                            }
                        }
                        let end = end.context("unterminated HTTP authentication challenge")?;
                        (text, &quoted[end + 1..])
                    } else {
                        let end = value.find(',').unwrap_or(value.len());
                        (value[..end].trim().to_owned(), &value[end..])
                    };
                    if let Some(challenge) = &mut current {
                        challenge.params.insert(token.to_ascii_lowercase(), value);
                    }
                    rest = next;
                }
                // A token68 credential, as in `Negotiate abc==`.
                Some(_) => rest = after.trim_start_matches('='),
                None => {
                    result.extend(current.take());
                    current = Some(Challenge {
                        scheme: token.to_owned(),
                        params: BTreeMap::new(),
                    });
                    rest = &rest[end..];
                }
            }
        }
        result.extend(current);
    }
    Ok(result)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DigestHash {
    Md5,
    Sha256,
    Sha512_256,
}
impl DigestHash {
    /// The hash and whether it is a `-sess` variant, for an RFC 7616
    /// algorithm token; an absent algorithm means MD5.
    fn parse(algorithm: Option<&str>) -> Option<(Self, bool)> {
        let name = algorithm.unwrap_or("MD5").to_ascii_lowercase();
        let (base, sess) = match name.strip_suffix("-sess") {
            Some(base) => (base, true),
            None => (name.as_str(), false),
        };
        let hash = match base {
            "md5" => Self::Md5,
            "sha-256" => Self::Sha256,
            "sha-512-256" => Self::Sha512_256,
            _ => return None,
        };
        Some((hash, sess))
    }
    fn hex(self, text: &str) -> String {
        match self {
            Self::Md5 => format!("{:x}", Md5::digest(text.as_bytes())),
            Self::Sha256 => format!("{:x}", Sha256::digest(text.as_bytes())),
            Self::Sha512_256 => format!("{:x}", Sha512_256::digest(text.as_bytes())),
        }
    }
}
/// The Digest challenge to answer: the strongest algorithm this client can
/// compute, the server's order of preference breaking ties.
fn digest_challenge(challenges: &[Challenge]) -> Option<(&Challenge, DigestHash, bool)> {
    let mut best: Option<(&Challenge, DigestHash, bool)> = None;
    for challenge in challenges {
        if !challenge.scheme.eq_ignore_ascii_case("digest")
            || !challenge.params.contains_key("realm")
            || !challenge.params.contains_key("nonce")
            || challenge
                .params
                .get("qop")
                .is_some_and(|qop| !qop.split(',').any(|q| q.trim() == "auth"))
        {
            continue;
        }
        let Some((hash, sess)) =
            DigestHash::parse(challenge.params.get("algorithm").map(String::as_str))
        else {
            continue;
        };
        if best.is_none_or(|(_, strongest, _)| hash > strongest) {
            best = Some((challenge, hash, sess));
        }
    }
    best
}

fn validate_timeout(timeout: Duration) -> Result<()> {
    ensure!(
        !timeout.is_zero() && Instant::now().checked_add(timeout).is_some(),
        "ONVIF timeout must be positive and fit in the monotonic clock"
    );
    Ok(())
}

fn validate_credentials(credentials: &Credentials) -> Result<()> {
    ensure!(!credentials.username.is_empty(), "ONVIF username is empty");
    ensure!(
        credentials.username.len() <= 1024 && credentials.password.len() <= 4096,
        "ONVIF credentials exceed size limit"
    );
    ensure!(
        !credentials.username.chars().any(char::is_control)
            && !credentials.password.chars().any(char::is_control),
        "ONVIF credentials must not contain control characters"
    );
    Ok(())
}
fn quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
/// The Authorization value answering a 401: Digest (RFC 7616) with the
/// challenge [`digest_challenge`] picks, else Basic when offered.
fn http_authorization(
    values: &[String],
    credentials: &Credentials,
    method: &str,
    path: &str,
    cnonce: &str,
) -> Result<String> {
    ensure!(
        !values.iter().any(|v| v.contains(['\r', '\n']))
            && !credentials.username.contains(['\r', '\n']),
        "invalid HTTP authentication header characters"
    );
    let challenges = challenges(values)?;
    if let Some((challenge, hash, sess)) = digest_challenge(&challenges) {
        return Ok(digest_authorization(
            challenge,
            hash,
            sess,
            credentials,
            method,
            path,
            cnonce,
        ));
    }
    if challenges
        .iter()
        .any(|c| c.scheme.eq_ignore_ascii_case("basic"))
    {
        return Ok(format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", credentials.username, credentials.password))
        ));
    }
    if let Some(digest) = challenges
        .iter()
        .find(|c| c.scheme.eq_ignore_ascii_case("digest"))
    {
        match digest.params.get("algorithm") {
            Some(algorithm) if DigestHash::parse(Some(algorithm)).is_none() => {
                bail!("unsupported HTTP Digest algorithm {algorithm}")
            }
            _ if digest.params.contains_key("qop") => bail!("HTTP Digest auth-int is unsupported"),
            _ => bail!("HTTP Digest challenge has no realm or nonce"),
        }
    }
    bail!("unsupported ONVIF HTTP authentication method")
}
fn digest_authorization(
    challenge: &Challenge,
    hash: DigestHash,
    sess: bool,
    credentials: &Credentials,
    method: &str,
    path: &str,
    cnonce: &str,
) -> String {
    let param = |name: &str| challenge.params.get(name).map(String::as_str);
    let realm = param("realm").unwrap_or_default();
    let nonce = param("nonce").unwrap_or_default();
    let qop = param("qop").is_some();
    let mut ha1 = hash.hex(&format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    if sess {
        ha1 = hash.hex(&format!("{ha1}:{nonce}:{cnonce}"));
    }
    let ha2 = hash.hex(&format!("{method}:{path}"));
    let response = if qop {
        hash.hex(&format!("{ha1}:{nonce}:00000001:{cnonce}:auth:{ha2}"))
    } else {
        hash.hex(&format!("{ha1}:{nonce}:{ha2}"))
    };
    let mut auth = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{response}\", algorithm={}",
        quoted(&credentials.username),
        quoted(realm),
        quoted(nonce),
        quoted(path),
        param("algorithm").unwrap_or("MD5")
    );
    if qop {
        auth.push_str(&format!(
            ", qop=auth, nc=00000001, cnonce=\"{}\"",
            quoted(cnonce)
        ));
    }
    if let Some(opaque) = param("opaque") {
        auth.push_str(&format!(", opaque=\"{}\"", quoted(opaque)));
    }
    auth
}
fn envelope(header: &str, content: &str) -> String {
    format!(
        "<?xml version='1.0' encoding='UTF-8'?><s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:tds='{DEVICE_NS}' xmlns:trt='{MEDIA_NS}' xmlns:tr2='{MEDIA2_NS}' xmlns:tt='http://www.onvif.org/ver10/schema'><s:Header>{header}</s:Header><s:Body>{content}</s:Body></s:Envelope>"
    )
}
/// A SOAP fault refusing the sign-in (ter:NotAuthorized). Devices send it
/// with 400, 500 or even 200.
fn not_authorized(response: &HttpResponse) -> bool {
    response.status != 401
        && response.body.contains("NotAuthorized")
        && xml_document(&response.body).is_err()
}
/// SOAP calls to one device, which learn how it takes a sign-in.
struct Soap<'a> {
    /// The device service, which reports the device clock.
    device: &'a str,
    credentials: Option<&'a Credentials>,
    timeout: Duration,
    clock_offset: Option<i64>,
    /// Service endpoints that refused WS-Security and accepted HTTP
    /// authentication; a device's services may each take a different one.
    http_auth: std::collections::BTreeSet<String>,
}
impl<'a> Soap<'a> {
    fn new(device: &'a str, credentials: Option<&'a Credentials>, timeout: Duration) -> Self {
        Self {
            device,
            credentials,
            timeout,
            clock_offset: None,
            http_auth: Default::default(),
        }
    }
    fn token(&mut self, credentials: &Credentials) -> Result<String> {
        let offset = match self.clock_offset {
            Some(offset) => offset,
            None => *self
                .clock_offset
                .insert(clock_offset(self.device, self.timeout)?.unwrap_or(0)),
        };
        security(credentials, offset)
    }
    /// Send a WS-Security UsernameToken first, which most cameras take.
    /// When the device refuses it, answer its HTTP Digest (or Basic)
    /// challenge without a token (Core 5.9.1: not both at once), and keep
    /// using HTTP authentication for later calls to that endpoint once it works.
    fn call(
        &mut self,
        endpoint: &str,
        namespace: &str,
        operation: &str,
        content: &str,
    ) -> Result<String> {
        let url = parse_url(endpoint)?;
        let action = format!("{namespace}/{operation}");
        let timeout = self.timeout;
        // Named by the SOAP endpoint here; `connect_with` re-keys the error to
        // the ONVIF address a sign-in is saved under.
        let refused = |sent: bool| {
            crate::logins::auth_error(
                &format!("onvif:{}", crate::media::redact_url(endpoint)),
                sent,
            )
        };
        let plain = envelope("", content);
        let Some(credentials) = self.credentials else {
            let response = request(&url, &action, &plain, None, timeout)?;
            if response.status == 401 {
                return Err(refused(false));
            }
            return finish(response, || refused(false));
        };
        let mut challenge = None;
        if !self.http_auth.contains(endpoint) {
            let token = self.token(credentials)?;
            let first = request(&url, &action, &envelope(&token, content), None, timeout)?;
            if first.status != 401 && !not_authorized(&first) {
                return finish(first, || refused(true));
            }
            if first.status == 401 {
                challenge = Some(first);
            }
        }
        let challenge = match challenge {
            Some(challenge) => challenge,
            None => request(&url, &action, &plain, None, timeout)?,
        };
        if challenge.status != 401 {
            // No HTTP challenge: the device takes only the token it refused.
            return finish(challenge, || refused(true));
        }
        ensure!(
            !challenge.challenges.is_empty(),
            "ONVIF authentication challenge is missing"
        );
        let authorization = http_authorization(
            &challenge.challenges,
            credentials,
            "POST",
            &url.path,
            &random_hex()?,
        )?;
        let response = request(&url, &action, &plain, Some(&authorization), timeout)?;
        if response.status == 401 {
            return Err(refused(true));
        }
        if (200..300).contains(&response.status) {
            self.http_auth.insert(endpoint.to_owned());
        }
        finish(response, || refused(true))
    }
}
fn finish(response: HttpResponse, refused: impl Fn() -> anyhow::Error) -> Result<String> {
    // Many cameras answer GetCapabilities anonymously and refuse GetProfiles
    // with a NotAuthorized fault.
    if not_authorized(&response) {
        return Err(refused());
    }
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
    validate_timeout(timeout)?;
    parse_url(endpoint)?;
    if let Some(credentials) = credentials {
        validate_credentials(credentials)?;
    }
    if let Some(profile) = profile {
        ensure!(
            !profile.is_empty() && profile.len() <= 1024 && !profile.chars().any(char::is_control),
            "ONVIF profile token needs 1..1024 bytes without control characters"
        );
    }
    let mut soap = Soap::new(endpoint, credentials, timeout);
    let services = media_services(&mut soap, endpoint)?;
    let url = parse_url(endpoint)?;
    let reached = Reached {
        ips: (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map(|addresses| addresses.map(|a| a.ip()).collect())
            .unwrap_or_default(),
        device: services.device.as_deref().and_then(host_and_port),
        url,
    };
    let mut first_error = None;
    for (media, address) in [(Media::V1, services.media1), (Media::V2, services.media2)] {
        let Some(address) = address else {
            continue;
        };
        let address = reached.rewrite(&address);
        match media_streams(&mut soap, media, &address, profile, &reached) {
            Ok(streams) => return Ok(streams),
            // Asking again with the same sign-in risks a lockout.
            Err(error) if crate::logins::is_auth_error(&error) => return Err(error),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| anyhow!("ONVIF device exposes no media service endpoint")))
}
#[derive(Default)]
struct Services {
    device: Option<String>,
    media1: Option<String>,
    media2: Option<String>,
}
/// Where the device answered: its endpoint, the addresses that endpoint's
/// host resolves to, and the host and port the device advertised for its
/// own device service.
struct Reached {
    url: HttpUrl,
    ips: Vec<IpAddr>,
    device: Option<(String, u16)>,
}
/// Addresses reachable only from the device's own network.
fn local_only(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                // Shared address space (RFC 6598) behind carrier-grade NAT.
                || (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64)
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => local_only(IpAddr::V4(ip)),
            None => {
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
            }
        },
    }
}
/// The host and port, with the scheme's default port, of an advertised URL.
fn host_and_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => match scheme.to_ascii_lowercase().as_str() {
            "http" => 80,
            "https" => 443,
            "rtsp" => 554,
            "rtsps" => 322,
            _ => return None,
        },
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}
impl Reached {
    /// `url` at the host the device answered on, when the advertised host
    /// cannot be reached from here: an unspecified or loopback address, or a
    /// private one while the device answered at a public or loopback address
    /// (NAT, a port forward or a tunnel), as go2rtc and ONVIF Device Manager
    /// do. The advertised port and path are kept, except that the device
    /// service's own host and port become the endpoint's, since a port
    /// forward covers every service the device serves there.
    fn rewrite(&self, url: &str) -> String {
        let Some((host, port)) = host_and_port(url) else {
            return url.into();
        };
        let ip = if host.eq_ignore_ascii_case("localhost") {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            match host.parse::<IpAddr>() {
                Ok(ip) => ip,
                Err(_) => return url.into(),
            }
        };
        let unreachable = ip.is_unspecified()
            || ip.is_loopback()
            || (local_only(ip)
                && !self.ips.is_empty()
                && self
                    .ips
                    .iter()
                    .all(|ip| ip.is_loopback() || !local_only(*ip)));
        if self.ips.contains(&ip) || !unreachable {
            return url.into();
        }
        let endpoint = if self.url.host.contains(':') {
            format!("[{}]", self.url.host)
        } else {
            self.url.host.clone()
        };
        let port = if self.device.as_ref().is_some_and(|(device, device_port)| {
            device.eq_ignore_ascii_case(&host) && *device_port == port
        }) {
            self.url.port
        } else {
            port
        };
        let (scheme, rest) = url.split_once("://").unwrap_or_default();
        let path = &rest[rest.find(['/', '?', '#']).unwrap_or(rest.len())..];
        format!("{scheme}://{endpoint}:{port}{path}")
    }
}
/// The Media1 and Media2 service addresses from GetServices (Core 8.1.2.2),
/// or from GetCapabilities on older devices, which list Media1 only.
fn media_services(soap: &mut Soap, endpoint: &str) -> Result<Services> {
    let mut services = Services::default();
    match soap.call(
        endpoint,
        DEVICE_NS,
        "GetServices",
        "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
    ) {
        Ok(body) => {
            let doc = xml_document(&body)?;
            for service in doc
                .descendants()
                .filter(|n| n.is_element() && n.tag_name().name() == "Service")
            {
                let field = |name: &str| {
                    child_element(service, name)
                        .and_then(|n| n.text())
                        .map(|text| text.trim().to_owned())
                };
                let slot = match field("Namespace").as_deref() {
                    Some(DEVICE_NS) => &mut services.device,
                    Some(MEDIA_NS) => &mut services.media1,
                    Some(MEDIA2_NS) => &mut services.media2,
                    _ => continue,
                };
                if slot.is_none() {
                    *slot = field("XAddr").filter(|xaddr| !xaddr.is_empty());
                }
            }
            if services.media1.is_some() || services.media2.is_some() {
                return Ok(services);
            }
        }
        Err(error) if crate::logins::is_auth_error(&error) => return Err(error),
        Err(_) => {}
    }
    let capabilities = soap.call(
        endpoint,
        DEVICE_NS,
        "GetCapabilities",
        "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>",
    )?;
    let doc = xml_document(&capabilities)?;
    let xaddr = |name: &str| {
        doc.descendants()
            .find(|n| n.is_element() && n.tag_name().name() == name)
            .and_then(|n| child_text(n, "XAddr"))
            .map(str::to_owned)
    };
    services.device = xaddr("Device");
    services.media1 = xaddr("Media");
    Ok(services)
}
#[derive(Clone, Copy)]
enum Media {
    V1,
    V2,
}
impl Media {
    fn name(self) -> &'static str {
        match self {
            Self::V1 => "Media1",
            Self::V2 => "Media2",
        }
    }
    fn namespace(self) -> &'static str {
        match self {
            Self::V1 => MEDIA_NS,
            Self::V2 => MEDIA2_NS,
        }
    }
    fn get_profiles(self) -> &'static str {
        match self {
            Self::V1 => "<trt:GetProfiles/>",
            // Without a type, Media2 lists no configurations.
            Self::V2 => "<tr2:GetProfiles><tr2:Type>VideoEncoder</tr2:Type></tr2:GetProfiles>",
        }
    }
    fn get_stream_uri(self, token: &str) -> String {
        match self {
            Self::V1 => format!(
                "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
                escape(token)
            ),
            // RtspUnicast is the one RTSP protocol Profile T devices must offer.
            Self::V2 => format!(
                "<tr2:GetStreamUri><tr2:Protocol>RtspUnicast</tr2:Protocol><tr2:ProfileToken>{}</tr2:ProfileToken></tr2:GetStreamUri>",
                escape(token)
            ),
        }
    }
}
fn media_streams(
    soap: &mut Soap,
    media: Media,
    address: &str,
    profile: Option<&str>,
    reached: &Reached,
) -> Result<Vec<OnvifStream>> {
    let profiles = soap.call(
        address,
        media.namespace(),
        "GetProfiles",
        media.get_profiles(),
    )?;
    let doc = xml_document(&profiles)?;
    let mut result = Vec::new();
    let mut names = BTreeSet::new();
    let mut stream_error = None;
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
        // Media1 Profiles/VideoEncoderConfiguration, Media2
        // Profiles/Configurations/VideoEncoder.
        let encoder = child_element(node, "VideoEncoderConfiguration").or_else(|| {
            child_element(node, "Configurations").and_then(|n| child_element(n, "VideoEncoder"))
        });
        let encoding = encoder
            .and_then(|n| child_text(n, "Encoding"))
            .map(str::to_string);
        let width = encoder
            .and_then(|n| child_text(n, "Width"))
            .and_then(|v| v.parse().ok());
        let height = encoder
            .and_then(|n| child_text(n, "Height"))
            .and_then(|v| v.parse().ok());
        let content = media.get_stream_uri(token);
        // Cameras may advertise stale or non-streamable profiles alongside usable
        // ones. A failed profile must not hide the remaining streams. Explicit
        // selection still reports that profile's original error.
        let uri = (|| -> Result<String> {
            let stream = soap.call(address, media.namespace(), "GetStreamUri", &content)?;
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
            let uri = reached.rewrite(&uri);
            crate::media::validate_source(&uri)?;
            Ok(uri)
        })();
        let uri = match uri {
            Ok(uri) => uri,
            Err(error) if profile.is_some() => return Err(error),
            Err(error) => {
                stream_error = Some(error);
                continue;
            }
        };
        result.push(OnvifStream {
            token: token.into(),
            name,
            encoding,
            width,
            height,
            uri,
        });
    }
    if result.is_empty()
        && let Some(error) = stream_error
    {
        return Err(error.context(format!(
            "ONVIF camera exposes no usable {} stream profiles",
            media.name()
        )));
    }
    ensure!(
        !result.is_empty(),
        "ONVIF profile was not found or the camera exposes no {} profiles",
        media.name()
    );
    Ok(result)
}

/// Recognize ONVIF locators before the general media URL handler.
pub fn is_source(source: &str) -> bool {
    source
        .get(..6)
        .is_some_and(|protocol| protocol.eq_ignore_ascii_case("onvif:"))
}
fn percent_decode(text: &str) -> Result<String> {
    crate::logins::percent_decode(text)
}
fn source_endpoint(source: &str) -> Result<(String, Option<Credentials>)> {
    source_endpoint_impl(source, false)
}
fn source_endpoint_impl(source: &str, allow_https: bool) -> Result<(String, Option<Credentials>)> {
    ensure!(
        source.len() <= 16384 && !source.chars().any(|c| c.is_control() || c.is_whitespace()),
        "invalid ONVIF source URL; percent-encode credentials and path values"
    );
    ensure!(
        is_source(source),
        "ONVIF source must use onvif:http://HOST/service or onvif://USER:PASS@HOST/service"
    );
    let endpoint = &source[6..];
    if endpoint.split_once("://").is_some_and(|(protocol, _)| {
        protocol.eq_ignore_ascii_case("http") || protocol.eq_ignore_ascii_case("https")
    }) {
        parse_endpoint(endpoint, allow_https)?;
        return Ok((endpoint.into(), None));
    }
    let rest = endpoint.strip_prefix("//").context(
        "ONVIF source must use onvif:http://HOST/service or onvif://USER:PASS@HOST/service",
    )?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let path = if rest[end..].starts_with('?') || end == rest.len() {
        format!("/onvif/device_service{}", &rest[end..])
    } else {
        rest[end..].to_owned()
    };
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
    parse_endpoint(&endpoint, allow_https)?;
    if let Some(credentials) = &credentials {
        validate_credentials(credentials)?;
    }
    Ok((endpoint, credentials))
}

/// The address a sign-in for this ONVIF camera is saved under:
/// `onvif:http://HOST:PORT/PATH`, with the host lowercased, the port made
/// explicit and any query dropped, so `onvif://cam` and
/// `onvif:http://cam:80/onvif/device_service` are the same camera.
pub fn endpoint_key(source: &str) -> Result<String> {
    let (endpoint, credentials) = source_endpoint(source)?;
    if credentials.is_some() {
        return Err(crate::dataset::Usage(
            "leave the user name and password out of the address; use --user and --password-stdin"
                .into(),
        )
        .into());
    }
    let url = parse_url(&endpoint)?;
    let host = if url.host.contains(':') {
        format!("[{}]", url.host.to_ascii_lowercase())
    } else {
        url.host.to_ascii_lowercase()
    };
    let path = match url.path.split(['?', '#']).next().unwrap_or("") {
        "" => "/",
        path => path,
    };
    Ok(format!("onvif:http://{host}:{}{path}", url.port))
}
/// Validate a camera locator before discovery, authentication or camera changes.
pub fn validate_source(source: &str) -> Result<()> {
    source_endpoint(source).map(|_| ())
}
/// Check locator syntax without testing whether this build can contact the service.
pub fn validate_source_shape(source: &str) -> Result<()> {
    source_endpoint_impl(source, true).map(|_| ())
}
fn authenticated_uri(uri: &str, credentials: Option<&Credentials>) -> Result<String> {
    let Some(credentials) = credentials else {
        return Ok(uri.into());
    };
    ensure!(uri.contains("://"), "ONVIF stream URI has no authority");
    let login = crate::logins::CameraLogin::new(&credentials.username, &credentials.password)?;
    Ok(crate::logins::with_credentials(uri, &login)?.to_string())
}
/// The `WWW-Authenticate` lines an RTSP server answers an anonymous DESCRIBE
/// with; empty when it asks for no sign-in.
fn rtsp_challenges(uri: &str, timeout: Duration) -> Result<Vec<String>> {
    let (host, port) = host_and_port(uri).context("invalid RTSP address")?;
    let url = HttpUrl {
        host,
        port,
        authority: String::new(),
        path: String::new(),
    };
    let mut stream = open_tcp(&url, timeout)?;
    stream.write_all(
        format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n").as_bytes(),
    )?;
    let (status, _, challenges) = parse_head(&mut BufReader::new(stream))?;
    Ok(if status == 401 {
        challenges
    } else {
        Vec::new()
    })
}
/// Why the bundled FFmpeg (9.0) cannot answer an RTSP server's challenges,
/// if it cannot. It takes the last header that starts a Digest challenge
/// (Digest outranks Basic) and computes only MD5 and MD5-sess, spelled in
/// that case, with qop auth or none (libavformat/httpauth.c).
fn ffmpeg_cannot_answer(values: &[String], address: &str) -> Option<String> {
    let digests = values
        .iter()
        .filter_map(|value| {
            challenges(std::slice::from_ref(value))
                .ok()?
                .into_iter()
                .next()
        })
        .filter(|c| c.scheme.eq_ignore_ascii_case("digest"))
        .collect::<Vec<_>>();
    let algorithm = |c: &Challenge| c.params.get("algorithm").cloned().unwrap_or_default();
    let answerable = |c: &Challenge| {
        matches!(algorithm(c).as_str(), "" | "MD5" | "MD5-sess")
            && c.params
                .get("qop")
                .is_none_or(|qop| qop.split(',').any(|q| q.trim() == "auth"))
    };
    let last = digests.last()?;
    if answerable(last) {
        return None;
    }
    let named = |c: &Challenge| match algorithm(c) {
        name if name.is_empty() => "MD5".to_owned(),
        name => name,
    };
    let last_name = if matches!(algorithm(last).as_str(), "" | "MD5" | "MD5-sess") {
        format!("{} with qop auth-int", named(last))
    } else {
        named(last)
    };
    Some(if digests.iter().any(answerable) {
        format!(
            "{address} offers Digest sign-in with {}, but FFmpeg answers only the last challenge, {last_name}, which it cannot compute; set the camera's RTSP authentication to MD5 Digest alone",
            digests.iter().map(named).collect::<Vec<_>>().join(", ")
        )
    } else {
        format!(
            "{address} asks for a Digest sign-in with {last_name}, which FFmpeg cannot compute (it answers MD5 only); enable MD5 Digest authentication for RTSP on the camera"
        )
    })
}
/// A camera that refused the sign-in for an rtsp:// stream although its ONVIF
/// service took it may be asking for a Digest algorithm FFmpeg cannot answer;
/// say so instead of reporting a wrong password.
fn explain_rtsp_refusal(error: anyhow::Error, uri: &str, timeout: Duration) -> anyhow::Error {
    let uri = crate::logins::strip_userinfo(uri);
    if !crate::logins::is_auth_error(&error)
        || !uri
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("rtsp://"))
    {
        return error;
    }
    let address = crate::media::redact_url(&uri);
    match rtsp_challenges(&uri, timeout) {
        Ok(values) => match ffmpeg_cannot_answer(&values, &address) {
            Some(message) => {
                crate::types::Categorized::error(crate::types::ErrorKind::Unsupported, message)
            }
            None => error,
        },
        Err(_) => error,
    }
}
/// Try profiles in the camera's advertised order. Opening a media source reads
/// its first frame, so success establishes that this stream can actually decode.
fn open_stream<T>(
    streams: &[OnvifStream],
    mut open: impl FnMut(&OnvifStream) -> Result<T>,
) -> Result<(&OnvifStream, T)> {
    let mut last_error = None;
    let mut auth_error = None;
    for stream in streams {
        match open(stream) {
            Ok(backend) => return Ok((stream, backend)),
            // A sign-in problem says more than a later profile's failure.
            Err(error) if auth_error.is_none() && crate::logins::is_auth_error(&error) => {
                auth_error = Some(error)
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(auth_error
        .or(last_error)
        .unwrap_or_else(|| anyhow!("ONVIF camera exposes no stream profiles")))
    .context("could not open any ONVIF stream profile")
}

pub fn connect(
    source: &str,
    timeout: Duration,
) -> Result<(crate::types::CameraInfo, Box<dyn crate::types::Backend>)> {
    connect_with(source, None, timeout)
}
/// Open an ONVIF camera, with a sign-in given beside its address. Sign-in
/// errors always name the ONVIF address, never a stream URI it reported,
/// since the ONVIF address is what a sign-in is saved under.
pub fn connect_with(
    source: &str,
    login: Option<&crate::logins::CameraLogin>,
    timeout: Duration,
) -> Result<(crate::types::CameraInfo, Box<dyn crate::types::Backend>)> {
    let (endpoint, mut credentials) = source_endpoint(source)?;
    if let Some(login) = login {
        ensure!(
            credentials.is_none(),
            crate::dataset::Usage(
                "leave the user name and password out of the address when signing in".into()
            )
        );
        credentials = Some(Credentials {
            username: login.user.clone(),
            password: login.expose().to_owned(),
        });
    }
    let sent = credentials.is_some();
    let key = endpoint_key(source)
        .or_else(|_| endpoint_key(&format!("onvif:{endpoint}")))
        .unwrap_or_else(|_| format!("onvif:{}", crate::media::redact_url(&endpoint)));
    let rekey = |error: anyhow::Error| {
        if crate::logins::is_auth_error(&error) {
            crate::logins::auth_error(&key, sent)
        } else {
            error
        }
    };
    let streams = resolve(&endpoint, credentials.as_ref(), None, timeout).map_err(rekey)?;
    let (profile, backend) = open_stream(&streams, |profile| {
        let uri = zeroize::Zeroizing::new(authenticated_uri(&profile.uri, credentials.as_ref())?);
        crate::media::open_url(&uri, timeout).map_err(|error| {
            if sent {
                explain_rtsp_refusal(error, &profile.uri, timeout)
            } else {
                error
            }
        })
    })
    .map_err(rekey)?;
    if let Some(credentials) = &mut credentials {
        zeroize::Zeroize::zeroize(&mut credentials.password);
    }
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
        profile: Default::default(),
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
                profile: Default::default(),
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
    fn probe_match(types: &str, address: &str, xaddrs: &str) -> String {
        format!(
            "<?xml version='1.0' encoding='UTF-8'?><env:Envelope xmlns:env='http://www.w3.org/2003/05/soap-envelope' xmlns:wsadis='http://schemas.xmlsoap.org/ws/2004/08/addressing' xmlns:d='http://schemas.xmlsoap.org/ws/2005/04/discovery' xmlns:dn='http://www.onvif.org/ver10/network/wsdl' xmlns:tds='http://www.onvif.org/ver10/device/wsdl'><env:Header><wsadis:MessageID>urn:uuid:6b2f6f86-0b2c-4c19-9c5c-2f1e3f0a9d01</wsadis:MessageID><wsadis:RelatesTo>urn:uuid:0f0e9d1c-6a6b-4c5d-8e7f-9a0b1c2d3e4f</wsadis:RelatesTo><wsadis:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsadis:To><wsadis:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</wsadis:Action></env:Header><env:Body><d:ProbeMatches><d:ProbeMatch><wsadis:EndpointReference><wsadis:Address>{address}</wsadis:Address></wsadis:EndpointReference><d:Types>{types}</d:Types><d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/Profile/Streaming onvif://www.onvif.org/name/IPC</d:Scopes><d:XAddrs>{xaddrs}</d:XAddrs><d:MetadataVersion>10</d:MetadataVersion></d:ProbeMatch></d:ProbeMatches></env:Body></env:Envelope>"
        )
    }
    #[test]
    fn discovery_probes_each_onvif_type_and_repeats() {
        let ids = ["a1", "b2"];
        for (id, types, prefix, namespace) in [
            (
                ids[0],
                "dn:NetworkVideoTransmitter",
                "dn",
                "http://www.onvif.org/ver10/network/wsdl",
            ),
            (ids[1], "tds:Device", "tds", DEVICE_NS),
        ] {
            let xml = probe_message(id, types);
            let doc = roxmltree::Document::parse(&xml).unwrap();
            let node = doc
                .descendants()
                .find(|n| n.tag_name().name() == "Types")
                .unwrap();
            assert_eq!(node.text(), Some(types));
            assert_eq!(node.lookup_namespace_uri(Some(prefix)), Some(namespace));
            assert!(xml.contains(&format!("<a:MessageID>urn:uuid:{id}</a:MessageID>")));
        }
        let ms = |offsets: [Duration; 3]| offsets.map(|d| d.as_millis());
        assert_eq!(ms(repeat_offsets(50)), [50, 150, 350]);
        assert_eq!(ms(repeat_offsets(100)), [100, 300, 700]);
        assert_eq!(ms(repeat_offsets(250)), [250, 750, 1250]);
        // A device answering the tds:Device Probe lists only that type.
        let devices = parse_discovery(
            &probe_match(
                "tds:Device",
                "urn:uuid:4419c000-7b6a-11b2-8000-c0a8010a0b0c",
                "http://192.168.1.64/onvif/device_service",
            ),
            "192.168.1.64:3702".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(
            devices[0].endpoint,
            "http://192.168.1.64/onvif/device_service"
        );
    }
    #[test]
    fn discovery_picks_a_usable_address_and_merges_one_device() {
        let source: SocketAddr = "192.168.0.90:3702".parse().unwrap();
        for (xaddrs, expected) in [
            (
                "https://192.168.0.90/onvif/device_service http://192.168.0.90/onvif/device_service",
                "http://192.168.0.90/onvif/device_service",
            ),
            (
                "http://[fe80::c2a8:10ff:fe0a:b0c]/onvif/device_service http://10.1.1.90/onvif/device_service http://192.168.0.90/onvif/device_service",
                "http://192.168.0.90/onvif/device_service",
            ),
            (
                "http://[fe80::c2a8:10ff:fe0a:b0c]/onvif/device_service http://[2001:db8::90]/onvif/device_service",
                "http://[2001:db8::90]/onvif/device_service",
            ),
            (
                "http://0.0.0.0:8080/onvif/device_service",
                "http://192.168.0.90:8080/onvif/device_service",
            ),
        ] {
            let devices = parse_discovery(
                &probe_match("dn:NetworkVideoTransmitter", "urn:uuid:x", xaddrs),
                source,
            )
            .unwrap();
            assert_eq!(devices[0].endpoint, expected, "{xaddrs}");
            assert_eq!(devices[0].addresses.join(" "), xaddrs);
        }
        // One camera on two subnets answers on both with all its addresses;
        // two cameras with cloned firmware share a UUID but no address.
        let both = "http://192.168.0.90/onvif/device_service http://10.1.1.90/onvif/device_service";
        let mut devices = Vec::new();
        for (source, uuid, xaddrs) in [
            ("192.168.0.90:3702", "urn:uuid:cam", both),
            ("10.1.1.90:3702", "urn:uuid:cam", both),
            (
                "192.168.0.91:3702",
                "urn:uuid:cam",
                "http://192.168.0.91/onvif/device_service",
            ),
        ] {
            for device in parse_discovery(
                &probe_match("tds:Device", uuid, xaddrs),
                source.parse().unwrap(),
            )
            .unwrap()
            {
                merge_device(&mut devices, device);
            }
        }
        let endpoints = devices
            .iter()
            .map(|d| d.endpoint.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            endpoints,
            [
                "http://192.168.0.90/onvif/device_service",
                "http://192.168.0.91/onvif/device_service"
            ]
        );
    }
    #[test]
    fn locators_and_profiles_fail_before_network_or_credential_side_effects() {
        for source in [
            "onvif://camera",
            "onvif://user:pass@camera:80/service",
            "onvif://user%20name:p%40ss@camera",
            "onvif:http://[::1]/service",
        ] {
            assert!(validate_source(source).is_ok(), "{source}");
        }
        for source in [
            "onvif://",
            "onvif://user:secret@:80/service",
            "onvif://camera:0",
            "onvif://camera:65536",
            "onvif://[bad]/service",
            "onvif://user:%0A@camera",
            "onvif://user:pass@camera#fragment",
            "onvif://user:p%QQ@camera",
        ] {
            assert!(validate_source(source).is_err(), "{source}");
        }
        let endpoint = parse_url("http://camera?mode=media").unwrap();
        assert_eq!(endpoint.host, "camera");
        assert_eq!(endpoint.path, "/?mode=media");
        let (endpoint, _) = source_endpoint("onvif://camera?mode=media").unwrap();
        assert_eq!(endpoint, "http://camera/onvif/device_service?mode=media");
        let error = resolve(
            "http://camera/service",
            None,
            Some(""),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("profile token"));
        assert!(
            discover(Duration::ZERO)
                .unwrap_err()
                .to_string()
                .contains("positive")
        );
        assert!(discover(Duration::MAX).is_err());
    }
    #[test]
    fn endpoint_schemes_ignore_case_and_fragments_have_specific_errors() {
        let direct = "ONVIF:HTTP://Camera.Example:8080/Exact/%2f?Mode=Media";
        assert!(is_source(direct));
        let (endpoint, credentials) = source_endpoint(direct).unwrap();
        assert_eq!(endpoint, "HTTP://Camera.Example:8080/Exact/%2f?Mode=Media");
        assert!(credentials.is_none());
        let parsed = parse_url(&endpoint).unwrap();
        assert_eq!(parsed.host, "Camera.Example");
        assert_eq!(parsed.authority, "Camera.Example:8080");
        assert_eq!(parsed.path, "/Exact/%2f?Mode=Media");
        let (endpoint, credentials) =
            source_endpoint("OnViF://User:P%40ss@Camera.Example/Exact/%2f").unwrap();
        assert_eq!(endpoint, "http://Camera.Example/Exact/%2f");
        assert_eq!(credentials.unwrap().password, "P@ss");
        for source in [
            "onvif:http://camera/service#profile",
            "ONVIF://User:secret@Camera/Service#profile",
        ] {
            let error = validate_source(source).unwrap_err().to_string();
            assert!(error.contains("fragment"), "{error}");
            assert!(!error.contains("secret"));
            assert!(!error.contains("credentials"));
        }
        assert!(parse_url("http://camera/service%23profile").is_ok());
        assert!(
            validate_source("ONVIF:HTTPS://camera/service")
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        assert!(validate_source_shape("ONVIF:HTTPS://camera/service").is_ok());
        assert!(validate_source_shape("onvif:https://camera:0/service").is_err());
        assert!(validate_source_shape("onvif:https://camera/service#profile").is_err());
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
        for seconds in [0, 951782400, 981173106, 4102444799] {
            assert_eq!(created_seconds(&utc_timestamp(seconds)), seconds as i64);
        }
        let token = security(
            &Credentials {
                username: "a<b".into(),
                password: "never-print-me".into(),
            },
            0,
        )
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
            "Digest realm=\"camera, network\", nonce=\"abc\", qop=\"auth,auth-int\", algorithm=MD5"
                .to_owned();
        let parsed = challenges(std::slice::from_ref(&challenge)).unwrap();
        assert_eq!(parsed[0].params["realm"], "camera, network");
        let auth = http_authorization(
            &[challenge],
            &Credentials {
                username: "user".into(),
                password: "pass".into(),
            },
            "POST",
            "/onvif/device_service",
            "random",
        )
        .unwrap();
        assert!(auth.contains("qop=auth"));
        assert!(!auth.contains("pass"));
        assert!(parse_url("https://camera/service").is_err());
        assert!(parse_url("http://user:pass@camera/service").is_err());
    }
    fn digest_response(authorization: &str) -> BTreeMap<String, String> {
        let mut parsed = challenges(&[authorization.to_owned()]).unwrap();
        assert_eq!(parsed.len(), 1, "{authorization}");
        parsed.remove(0).params
    }
    #[test]
    fn digest_answers_the_strongest_of_several_challenges() {
        let mufasa = Credentials {
            username: "Mufasa".into(),
            password: "Circle of Life".into(),
        };
        let cnonce = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ";
        let rfc = |algorithm: &str| {
            format!(
                "Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm={algorithm}, nonce=\"7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\""
            )
        };
        // RFC 7616 3.9.1, in both header orders.
        for lines in [
            vec![rfc("SHA-256"), rfc("MD5")],
            vec![rfc("MD5"), rfc("SHA-256")],
        ] {
            let auth =
                http_authorization(&lines, &mufasa, "GET", "/dir/index.html", cnonce).unwrap();
            let fields = digest_response(&auth);
            assert_eq!(fields["algorithm"], "SHA-256");
            assert_eq!(
                fields["response"],
                "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1"
            );
            assert_eq!(
                fields["opaque"],
                "FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS"
            );
        }
        let auth =
            http_authorization(&[rfc("MD5")], &mufasa, "GET", "/dir/index.html", cnonce).unwrap();
        assert_eq!(
            digest_response(&auth)["response"],
            "8ca523f5e9506fed4657c9700eebdbec"
        );
        let credentials = Credentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        // Realm first, two challenges on one line: the unsupported one is skipped.
        let auth = http_authorization(
            &["Digest realm=\"cam\", qop=\"auth\", algorithm=MD5, nonce=\"n1\", Digest realm=\"cam\", qop=\"auth\", algorithm=SHA-1, nonce=\"n2\"".into()],
            &credentials,
            "POST",
            "/onvif/media",
            "c",
        )
        .unwrap();
        let fields = digest_response(&auth);
        assert_eq!(
            (fields["algorithm"].as_str(), fields["nonce"].as_str()),
            ("MD5", "n1")
        );
        // Realm, nonce and opaque all come from the chosen challenge.
        let auth = http_authorization(
            &[
                "Digest realm=\"one\", nonce=\"n1\", opaque=\"o1\", algorithm=MD5".into(),
                "Digest realm=\"two\", nonce=\"n2\", opaque=\"o2\", algorithm=SHA-256".into(),
            ],
            &credentials,
            "POST",
            "/onvif/media",
            "c",
        )
        .unwrap();
        let fields = digest_response(&auth);
        assert_eq!(
            [&fields["realm"], &fields["nonce"], &fields["opaque"]],
            ["two", "n2", "o2"]
        );
        let ha1 = format!("{:x}", Sha256::digest(b"admin:two:secret"));
        let ha2 = format!("{:x}", Sha256::digest(b"POST:/onvif/media"));
        assert_eq!(
            fields["response"],
            format!("{:x}", Sha256::digest(format!("{ha1}:n2:{ha2}").as_bytes()))
        );
        // Digest is preferred over Basic and token68 schemes in any order.
        for line in [
            "Basic realm=\"cam\", Digest realm=\"cam\", nonce=\"n\"",
            "Negotiate abc==, Digest realm=\"cam\", nonce=\"n\", Basic realm=\"cam\"",
        ] {
            let auth = http_authorization(&[line.into()], &credentials, "POST", "/", "c").unwrap();
            assert!(auth.starts_with("Digest "), "{auth}");
        }
        let error = http_authorization(
            &["Digest realm=\"cam\", nonce=\"n\", algorithm=SHA-1".into()],
            &credentials,
            "POST",
            "/",
            "c",
        )
        .unwrap_err();
        assert!(error.to_string().contains("SHA-1"), "{error}");
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
    fn token_field<'a>(body: &'a str, name: &str) -> Option<&'a str> {
        let start = body.find(&format!(":{name}"))?;
        let start = start + body[start..].find('>')? + 1;
        Some(&body[start..start + body[start..].find('<')?])
    }
    fn created_seconds(created: &str) -> i64 {
        let field = |range: std::ops::Range<usize>| created[range].parse::<i64>().unwrap();
        unix_seconds(
            field(0..4),
            field(5..7),
            field(8..10),
            field(11..13),
            field(14..16),
            field(17..19),
        )
    }
    fn http_reply(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
    fn date_and_time(seconds: i64) -> String {
        let text = utc_timestamp(seconds as u64);
        let field = |range: std::ops::Range<usize>| text[range].parse::<u32>().unwrap();
        format!(
            "<Envelope><Body><GetSystemDateAndTimeResponse><SystemDateAndTime><DateTimeType>Manual</DateTimeType><DaylightSavings>false</DaylightSavings><UTCDateTime><Time><Hour>{}</Hour><Minute>{}</Minute><Second>{}</Second></Time><Date><Year>{}</Year><Month>{}</Month><Day>{}</Day></Date></UTCDateTime></SystemDateAndTime></GetSystemDateAndTimeResponse></Body></Envelope>",
            field(11..13),
            field(14..16),
            field(17..19),
            field(0..4),
            field(5..7),
            field(8..10)
        )
    }
    /// Serve `count` requests, answering each from its SOAP operation,
    /// headers and body.
    fn serve(
        count: usize,
        mut reply: impl FnMut(&str, &BTreeMap<String, String>, &str) -> String + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let (_, headers, body) = mock_request(&stream);
                let action = headers["soapaction"].trim_matches('"');
                let operation = action.rsplit('/').next().unwrap().to_owned();
                stream
                    .write_all(reply(&operation, &headers, &body).as_bytes())
                    .unwrap();
            }
        });
        (address.to_string(), server)
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
            let (mut stream, _) = listener.accept().unwrap();
            let (_, headers, body) = mock_request(&stream);
            assert!(headers["soapaction"].contains("GetSystemDateAndTime"));
            assert!(!body.contains("UsernameToken") && !headers.contains_key("authorization"));
            stream
                .write_all(
                    http_reply("200 OK", "", &date_and_time(now_seconds().unwrap())).as_bytes(),
                )
                .unwrap();
            for operation in ["GetServices", "GetProfiles", "GetStreamUri"] {
                for authenticated in [false, true] {
                    let (mut stream, _) = listener.accept().unwrap();
                    let (path, headers, body) = mock_request(&stream);
                    assert!(headers["soapaction"].contains(operation));
                    // The token goes once per service; after a service asks
                    // for HTTP Digest, its requests carry Digest alone.
                    if operation != "GetStreamUri" && !authenticated {
                        assert_username_token(&body, &server_credentials.password);
                    } else {
                        assert!(!body.contains("Security"), "{body}");
                    }
                    if authenticated {
                        let supplied = &headers["authorization"];
                        let expected = http_authorization(
                            &[challenge.into()],
                            &server_credentials,
                            "POST",
                            &path,
                            &digest_response(supplied)["cnonce"],
                        )
                        .unwrap();
                        assert_eq!(*supplied, expected);
                        let response=match operation {
                            "GetServices"=>format!("<Envelope><Body><GetServicesResponse><Service><Namespace>{DEVICE_NS}</Namespace><XAddr>http://{address}/onvif/device_service</XAddr><Version><Major>2</Major><Minor>40</Minor></Version></Service><Service><Namespace>{MEDIA_NS}</Namespace><XAddr>{media}</XAddr><Version><Major>2</Major><Minor>40</Minor></Version></Service></GetServicesResponse></Body></Envelope>"),
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
    fn ws_security_tokens_use_the_device_clock_and_a_new_nonce() {
        // A camera without NTP whose clock reset to 2001, which rejects
        // tokens more than five seconds from its own time.
        let skew = unix_seconds(2001, 2, 3, 4, 5, 6) - now_seconds().unwrap();
        let credentials = Credentials {
            username: "admin".into(),
            password: "pw".into(),
        };
        let password = credentials.password.clone();
        let mut nonces = Vec::new();
        let (address, server) = serve(3, move |operation, _, body| {
            let device_now = now_seconds().unwrap() + skew;
            if operation == "GetSystemDateAndTime" {
                assert!(!body.contains("UsernameToken"));
                return http_reply("200 OK", "", &date_and_time(device_now));
            }
            assert_username_token(body, &password);
            let nonce = token_field(body, "Nonce").unwrap().to_owned();
            assert!(!nonces.contains(&nonce), "nonce reused");
            nonces.push(nonce);
            let created = created_seconds(token_field(body, "Created").unwrap());
            if (created - device_now).abs() > 5 {
                let fault = "<Envelope><Body><Fault><Code><Value>env:Sender</Value><Subcode><Value>ter:NotAuthorized</Value></Subcode></Code><Reason><Text>Sender not Authorized</Text></Reason></Fault></Body></Envelope>";
                return http_reply("400 Bad Request", "", fault);
            }
            http_reply(
                "200 OK",
                "",
                "<Envelope><Body><Response/></Body></Envelope>",
            )
        });
        let endpoint = format!("http://{address}/onvif/device_service");
        let mut soap = Soap::new(&endpoint, Some(&credentials), Duration::from_secs(2));
        for _ in 0..2 {
            soap.call(
                &endpoint,
                DEVICE_NS,
                "GetDeviceInformation",
                "<tds:GetDeviceInformation/>",
            )
            .unwrap();
        }
        server.join().unwrap();
    }
    #[test]
    fn a_refused_token_falls_back_to_http_digest_alone() {
        let credentials = Credentials {
            username: "admin".into(),
            password: "pw".into(),
        };
        let challenge =
            "WWW-Authenticate: Digest realm=\"cam\", nonce=\"n\", qop=\"auth\", algorithm=MD5\r\n";
        let fault = "<Envelope><Body><Fault><Code><Value>env:Sender</Value><Subcode><Value>ter:NotAuthorized</Value></Subcode></Code><Reason><Text>Sender not Authorized</Text></Reason></Fault></Body></Envelope>";
        let mut step = 0;
        let (address, server) = serve(7, move |operation, headers, body| {
            step += 1;
            let token = body.contains("UsernameToken");
            let digest = headers
                .get("authorization")
                .is_some_and(|a| a.starts_with("Digest "));
            match step {
                // A device that authenticates even its clock (some Axis
                // firmware) leaves the host clock in the token.
                1 => {
                    assert_eq!(operation, "GetSystemDateAndTime");
                    http_reply("401 Unauthorized", challenge, "")
                }
                2 => {
                    assert!(token && !digest);
                    http_reply("400 Bad Request", "", fault)
                }
                3 | 5 => {
                    assert!(!token && !digest);
                    http_reply("401 Unauthorized", challenge, "")
                }
                // Another service of the same device still gets a token first.
                7 => {
                    assert!(token && !digest);
                    http_reply(
                        "200 OK",
                        "",
                        "<Envelope><Body><Response/></Body></Envelope>",
                    )
                }
                _ => {
                    assert!(!token && digest);
                    http_reply(
                        "200 OK",
                        "",
                        "<Envelope><Body><Response/></Body></Envelope>",
                    )
                }
            }
        });
        let endpoint = format!("http://{address}/onvif/device_service");
        let mut soap = Soap::new(&endpoint, Some(&credentials), Duration::from_secs(2));
        for _ in 0..2 {
            soap.call(&endpoint, MEDIA_NS, "GetProfiles", "<trt:GetProfiles/>")
                .unwrap();
        }
        let media = format!("http://{address}/onvif/media_service");
        soap.call(&media, MEDIA_NS, "GetProfiles", "<trt:GetProfiles/>")
            .unwrap();
        server.join().unwrap();
        // A device with no HTTP challenge refused the only sign-in it takes.
        let (address, server) = serve(3, move |operation, _, _| match operation {
            "GetSystemDateAndTime" => {
                http_reply("200 OK", "", &date_and_time(now_seconds().unwrap()))
            }
            _ => http_reply("400 Bad Request", "", fault),
        });
        let endpoint = format!("http://{address}/onvif/device_service");
        let error = Soap::new(&endpoint, Some(&credentials), Duration::from_secs(2))
            .call(&endpoint, MEDIA_NS, "GetProfiles", "<trt:GetProfiles/>")
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(code(&error), Some("auth_rejected"), "{error:#}");
    }
    #[test]
    fn connection_tries_streams_until_one_decodes() {
        let streams =
            ["unreachable", "unsupported", "working", "unused"].map(|token| OnvifStream {
                token: token.into(),
                name: token.into(),
                encoding: None,
                width: None,
                height: None,
                uri: format!("rtsp://camera/{token}"),
            });
        let mut attempts = Vec::new();
        let (profile, backend) = open_stream(&streams, |stream| {
            attempts.push(stream.token.clone());
            match stream.token.as_str() {
                "working" => Ok(42),
                "unsupported" => bail!("unsupported codec"),
                _ => bail!("connection timed out"),
            }
        })
        .unwrap();
        assert_eq!(attempts, ["unreachable", "unsupported", "working"]);
        assert_eq!(profile.token, "working");
        assert_eq!(backend, 42);

        let error = open_stream::<()>(&streams[..2], |_| bail!("decoder failed")).unwrap_err();
        assert_eq!(error.to_string(), "could not open any ONVIF stream profile");
        assert_eq!(error.root_cause().to_string(), "decoder failed");
        assert!(open_stream::<()>(&[], |_| unreachable!()).is_err());
    }

    #[test]
    #[ignore = "requires FFmpeg and permission to bind localhost sockets"]
    fn ffmpeg_connection_falls_back_to_decodable_profile() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let streams = ["broken", "working"].map(|token| OnvifStream {
            token: token.into(),
            name: token.into(),
            encoding: None,
            width: None,
            height: None,
            uri: format!("http://{address}/{token}"),
        });
        let server = std::thread::spawn(move || {
            for path in ["/broken", "/working"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let request = read_line(&mut reader, 4096).unwrap();
                assert!(std::str::from_utf8(&request).unwrap().contains(path));
                while read_line(&mut reader, 8192).unwrap() != b"\r\n" {}
                drop(reader);
                if path == "/broken" {
                    write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                } else {
                    let mut ppm = b"P6\n16 16\n255\n".to_vec();
                    ppm.extend(vec![127; 16 * 16 * 3]);
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: image/x-portable-pixmap\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", ppm.len()).unwrap();
                    stream.write_all(&ppm).unwrap();
                }
            }
        });
        let (profile, mut backend) = open_stream(&streams, |profile| {
            crate::media::open_url(&profile.uri, Duration::from_secs(3))
        })
        .unwrap();
        server.join().unwrap();
        assert_eq!(profile.token, "working");
        backend.start(16 * 16 * 3).unwrap();
        let frame = backend.next_frame(Duration::from_secs(3)).unwrap();
        assert_eq!((frame.width, frame.height), (16, 16));
        assert_eq!(frame.data, vec![127; 16 * 16 * 3]);
        backend.stop().unwrap();
    }

    #[test]
    fn profile_failures_do_not_hide_usable_streams() {
        // Exercise failures both before and after a usable profile, explicit
        // selection, and the all-failed case through the actual SOAP transport.
        for (selected, usable) in [(None, true), (Some("broken"), true), (None, false)] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}/service", listener.local_addr().unwrap());
            let media = endpoint.clone();
            let server = std::thread::spawn(move || {
                // A device without GetServices faults it, then answers
                // GetCapabilities.
                let count = if selected.is_some() { 4 } else { 6 };
                for _ in 0..count {
                    let (mut stream, _) = listener.accept().unwrap();
                    let (_, headers, body) = mock_request(&stream);
                    let response = if headers["soapaction"].contains("GetCapabilities") {
                        format!("<Envelope><Media><XAddr>{media}</XAddr></Media></Envelope>")
                    } else if headers["soapaction"].contains("GetProfiles") {
                        "<Envelope><Profiles token='broken'/><Profiles token='main'/><Profiles token='stale'/></Envelope>".into()
                    } else if body.contains("<trt:ProfileToken>main</trt:ProfileToken>") && usable {
                        "<Envelope><Uri>rtsp://camera/live</Uri></Envelope>".into()
                    } else {
                        "<Envelope><Fault><Reason><Text>No stream available</Text></Reason></Fault></Envelope>".into()
                    };
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                }
            });
            let result = resolve(&endpoint, None, selected, Duration::from_secs(2));
            server.join().unwrap();
            if selected.is_none() && usable {
                let streams = result.unwrap();
                assert_eq!(streams.len(), 1);
                assert_eq!(streams[0].token, "main");
                assert_eq!(streams[0].uri, "rtsp://camera/live");
            } else {
                let error = format!("{:#}", result.unwrap_err());
                assert!(error.contains("No stream available"), "{error}");
                assert_eq!(error.contains("no usable Media1"), selected.is_none());
            }
        }
    }
    const MEDIA2_PROFILES: &str = "<SOAP-ENV:Envelope xmlns:SOAP-ENV='http://www.w3.org/2003/05/soap-envelope' xmlns:tt='http://www.onvif.org/ver10/schema' xmlns:tr2='http://www.onvif.org/ver20/media/wsdl'><SOAP-ENV:Body><tr2:GetProfilesResponse><tr2:Profiles token='Profile_1' fixed='true'><tr2:Name>mainStream</tr2:Name><tr2:Configurations><tr2:VideoEncoder token='VideoEncoderToken_1' GovLength='50' Profile='Main'><tt:Name>VideoEncoder_1</tt:Name><tt:UseCount>1</tt:UseCount><tt:Encoding>H265</tt:Encoding><tt:Resolution><tt:Width>2560</tt:Width><tt:Height>1440</tt:Height></tt:Resolution><tt:RateControl ConstantBitRate='false'><tt:FrameRateLimit>25</tt:FrameRateLimit><tt:BitrateLimit>4096</tt:BitrateLimit></tt:RateControl><tt:Quality>3</tt:Quality></tr2:VideoEncoder></tr2:Configurations></tr2:Profiles></tr2:GetProfilesResponse></SOAP-ENV:Body></SOAP-ENV:Envelope>";
    #[test]
    fn media2_resolves_profile_t_devices() {
        // A Media2-only device, and one whose Media1 service faults.
        for media1 in [false, true] {
            let (address, server) = serve(
                if media1 { 4 } else { 3 },
                move |operation, headers, body| {
                    let namespace = headers["soapaction"]
                        .trim_matches('"')
                        .rsplit_once('/')
                        .unwrap()
                        .0
                        .to_owned();
                    let response = match operation {
                        "GetServices" => {
                            let local = |path: &str| format!("http://127.0.0.1/{path}");
                            let mut services = format!(
                                "<tds:Service><tds:Namespace>{DEVICE_NS}</tds:Namespace><tds:XAddr>{}</tds:XAddr><tds:Version><tt:Major>21</tt:Major><tt:Minor>12</tt:Minor></tds:Version></tds:Service><tds:Service><tds:Namespace>{MEDIA2_NS}</tds:Namespace><tds:XAddr>{}</tds:XAddr><tds:Version><tt:Major>21</tt:Major><tt:Minor>12</tt:Minor></tds:Version></tds:Service>",
                                local("onvif/device_service"),
                                local("onvif/media2_service")
                            );
                            if media1 {
                                services.push_str(&format!("<tds:Service><tds:Namespace>{MEDIA_NS}</tds:Namespace><tds:XAddr>{}</tds:XAddr></tds:Service>", local("onvif/media_service")));
                            }
                            format!("<s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:tds='{DEVICE_NS}' xmlns:tt='http://www.onvif.org/ver10/schema'><s:Body><tds:GetServicesResponse>{services}</tds:GetServicesResponse></s:Body></s:Envelope>").replace("127.0.0.1", &headers["host"])
                        }
                        _ if namespace == MEDIA_NS => {
                            return http_reply(
                                "500 Internal Server Error",
                                "",
                                "<s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope'><s:Body><s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:ActionNotSupported</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Optional Action Not Implemented</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>",
                            );
                        }
                        "GetProfiles" => {
                            assert!(body.contains("<tr2:GetProfiles><tr2:Type>VideoEncoder</tr2:Type></tr2:GetProfiles>"), "{body}");
                            MEDIA2_PROFILES.into()
                        }
                        _ => {
                            assert!(body.contains("<tr2:Protocol>RtspUnicast</tr2:Protocol><tr2:ProfileToken>Profile_1</tr2:ProfileToken>"), "{body}");
                            "<s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:tr2='http://www.onvif.org/ver20/media/wsdl'><s:Body><tr2:GetStreamUriResponse><tr2:Uri>rtsp://camera.example:554/Streaming/Channels/101</tr2:Uri></tr2:GetStreamUriResponse></s:Body></s:Envelope>".into()
                        }
                    };
                    http_reply("200 OK", "", &response)
                },
            );
            let endpoint = format!("http://{address}/onvif/device_service");
            let streams = resolve(&endpoint, None, None, Duration::from_secs(2)).unwrap();
            server.join().unwrap();
            assert_eq!(streams.len(), 1);
            assert_eq!(streams[0].token, "Profile_1");
            assert_eq!(streams[0].name, "mainStream");
            assert_eq!(streams[0].encoding.as_deref(), Some("H265"));
            assert_eq!(
                (streams[0].width, streams[0].height),
                (Some(2560), Some(1440))
            );
            assert_eq!(
                streams[0].uri,
                "rtsp://camera.example:554/Streaming/Channels/101"
            );
        }
    }
    #[test]
    fn unreachable_advertised_hosts_become_the_reached_host() {
        let reached = |endpoint: &str, ips: &[&str], device: &str| Reached {
            url: parse_url(endpoint).unwrap(),
            ips: ips.iter().map(|ip| ip.parse().unwrap()).collect(),
            device: host_and_port(device),
        };
        // Through a port forward: 203.0.113.5:8080 reaches 192.168.1.64:80.
        let forward = reached(
            "http://203.0.113.5:8080/onvif/device_service",
            &["203.0.113.5"],
            "http://192.168.1.64/onvif/device_service",
        );
        for (advertised, expected) in [
            (
                "http://192.168.1.64/onvif/Media",
                "http://203.0.113.5:8080/onvif/Media",
            ),
            (
                "http://192.168.1.64:8999/onvif/media2?x=1",
                "http://203.0.113.5:8999/onvif/media2?x=1",
            ),
            (
                "rtsp://192.168.1.64:554/Streaming/Channels/101",
                "rtsp://203.0.113.5:554/Streaming/Channels/101",
            ),
            ("rtsp://192.168.1.64/live", "rtsp://203.0.113.5:554/live"),
            ("rtsp://127.0.0.1:8554/live", "rtsp://203.0.113.5:8554/live"),
            // Public, named and credential-bearing addresses stay.
            ("rtsp://198.51.100.7/live", "rtsp://198.51.100.7/live"),
            ("rtsp://camera.example/live", "rtsp://camera.example/live"),
            (
                "rtsp://u:p@192.168.1.64/live",
                "rtsp://u:p@192.168.1.64/live",
            ),
        ] {
            assert_eq!(forward.rewrite(advertised), expected);
        }
        // On the camera's LAN: only an unspecified or loopback host is wrong.
        let lan = reached(
            "http://192.168.1.64/onvif/device_service",
            &["192.168.1.64"],
            "http://0.0.0.0/onvif/device_service",
        );
        for (advertised, expected) in [
            (
                "http://0.0.0.0/onvif/media",
                "http://192.168.1.64:80/onvif/media",
            ),
            (
                "http://0.0.0.0:8999/onvif/media",
                "http://192.168.1.64:8999/onvif/media",
            ),
            ("rtsp://0.0.0.0:554/live", "rtsp://192.168.1.64:554/live"),
            ("rtsp://192.168.1.64/live", "rtsp://192.168.1.64/live"),
            // A recorder elsewhere on the network serves the stream.
            ("rtsp://10.0.0.5/live", "rtsp://10.0.0.5/live"),
        ] {
            assert_eq!(lan.rewrite(advertised), expected);
        }
        // A tunnel to IPv6, and a device behind it with a unique local address.
        let tunnel = reached(
            "http://[::1]:9000/onvif/device_service",
            &["::1"],
            "http://[fd00::40]/onvif/device_service",
        );
        assert_eq!(
            tunnel.rewrite("http://[fd00::40]/onvif/media"),
            "http://[::1]:9000/onvif/media"
        );
    }
    #[test]
    fn resolution_uses_the_reached_host_for_private_advertised_addresses() {
        // Reached at loopback (a tunnel), the device advertises its LAN address.
        let (address, server) = serve(3, |operation, headers, _| {
            let port = headers["host"].rsplit(':').next().unwrap().to_owned();
            let response = match operation {
                "GetServices" => format!(
                    "<Envelope><Body><GetServicesResponse><Service><Namespace>{DEVICE_NS}</Namespace><XAddr>http://10.255.255.1:{port}/onvif/device_service</XAddr></Service><Service><Namespace>{MEDIA_NS}</Namespace><XAddr>http://10.255.255.1:{port}/onvif/media</XAddr></Service></GetServicesResponse></Body></Envelope>"
                ),
                "GetProfiles" => "<Envelope><Body><GetProfilesResponse><Profiles token='main'><Name>Main</Name></Profiles></GetProfilesResponse></Body></Envelope>".into(),
                _ => "<Envelope><Body><GetStreamUriResponse><MediaUri><Uri>rtsp://10.255.255.1:554/live</Uri></MediaUri></GetStreamUriResponse></Body></Envelope>".into(),
            };
            http_reply("200 OK", "", &response)
        });
        let endpoint = format!("http://{address}/onvif/device_service");
        let streams = resolve(&endpoint, None, None, Duration::from_millis(500)).unwrap();
        server.join().unwrap();
        assert_eq!(streams[0].uri, "rtsp://127.0.0.1:554/live");
    }
    #[test]
    fn soap_http_basic_and_timeout() {
        let (address, server) = serve(3, |operation, headers, body| match operation {
            "GetSystemDateAndTime" => {
                http_reply("200 OK", "", &date_and_time(now_seconds().unwrap()))
            }
            _ if headers.contains_key("authorization") => {
                assert_eq!(
                    headers["authorization"],
                    format!("Basic {}", STANDARD.encode("user:secret"))
                );
                assert!(!body.contains("UsernameToken"));
                http_reply(
                    "200 OK",
                    "",
                    "<Envelope><Body><Response/></Body></Envelope>",
                )
            }
            _ => http_reply(
                "401 Unauthorized",
                "WWW-Authenticate: Basic realm=\"camera\"\r\n",
                "",
            ),
        });
        let endpoint = format!("http://{address}/service");
        let credentials = Credentials {
            username: "user".into(),
            password: "secret".into(),
        };
        Soap::new(&endpoint, Some(&credentials), Duration::from_secs(1))
            .call(&endpoint, DEVICE_NS, "Test", "<Test/>")
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
            Soap::new(&endpoint, None, Duration::from_millis(50))
                .call(&endpoint, DEVICE_NS, "Test", "<Test/>")
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
    }
    /// Answer each request in turn with a canned response.
    fn answer(responses: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                mock_request(&stream);
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("{address}"), server)
    }
    fn code(error: &anyhow::Error) -> Option<&'static str> {
        crate::types::error_transport_code(error)
    }
    #[test]
    fn sign_in_requests_and_refusals_are_typed_and_name_the_onvif_address() {
        let challenge = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"Camera\", nonce=\"n\", qop=\"auth\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned();
        let timeout = Duration::from_secs(2);
        // No credentials: auth_required, naming the ONVIF address.
        let (address, server) = answer(vec![challenge.clone()]);
        let error = connect(&format!("onvif://{address}"), timeout)
            .err()
            .unwrap();
        server.join().unwrap();
        assert_eq!(code(&error), Some("auth_required"), "{error:#}");
        let text = format!("{error:#}");
        assert!(
            text.contains(&format!("onvif:http://{address}/onvif/device_service")),
            "{text}"
        );
        // Credentials refused after the Digest challenge: auth_rejected. The
        // device authenticates its clock too.
        let (address, server) = answer(vec![challenge.clone(), challenge.clone(), challenge]);
        let login = crate::logins::CameraLogin::new("admin", "onvif-secret-pw").unwrap();
        let error = connect_with(&format!("onvif://{address}"), Some(&login), timeout)
            .err()
            .unwrap();
        server.join().unwrap();
        assert_eq!(code(&error), Some("auth_rejected"), "{error:#}");
        assert!(!format!("{error:#}").contains("onvif-secret-pw"));
        // GetServices works anonymously, GetProfiles faults.
        let media = format!("http://{address}/onvif/media");
        let ok = |body: String| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let fault = "<Envelope><Body><Fault><Code><Subcode><Value>ter:NotAuthorized</Value></Subcode></Code><Reason><Text>Sender not Authorized</Text></Reason></Fault></Body></Envelope>";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let here = listener.local_addr().unwrap();
        let media = media.replace(&address, &here.to_string());
        let responses = vec![
            ok(format!(
                "<Envelope><Body><GetServicesResponse><Service><Namespace>{MEDIA_NS}</Namespace><XAddr>{media}</XAddr></Service></GetServicesResponse></Body></Envelope>"
            )),
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fault}",
                fault.len()
            ),
        ];
        let server = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                mock_request(&stream);
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let error = connect(
            &format!("onvif:http://{here}/onvif/device_service"),
            timeout,
        )
        .err()
        .unwrap();
        server.join().unwrap();
        assert_eq!(code(&error), Some("auth_required"), "{error:#}");
        assert!(
            format!("{error:#}").contains(&format!("onvif:http://{here}/onvif/device_service"))
        );
        // Credentials in the address and beside it are a usage error.
        let error = connect_with("onvif://u:p@127.0.0.1:9", Some(&login), timeout)
            .err()
            .unwrap();
        assert_eq!(code(&error), Some("usage"));
    }
    #[test]
    fn a_sign_in_failure_outranks_later_profile_failures() {
        let streams = ["first", "second"].map(|token| OnvifStream {
            token: token.into(),
            name: token.into(),
            encoding: None,
            width: None,
            height: None,
            uri: format!("rtsp://camera/{token}"),
        });
        let error = open_stream::<()>(&streams, |stream| match stream.token.as_str() {
            "first" => Err(crate::logins::auth_error("rtsp://camera/first", false)),
            _ => bail!("decoder failed"),
        })
        .unwrap_err();
        assert_eq!(code(&error), Some("auth_required"));
        assert_eq!(
            endpoint_key("onvif://Cam:8080").unwrap(),
            "onvif:http://cam:8080/onvif/device_service"
        );
        assert_eq!(
            code(&endpoint_key("onvif://u:p@cam").unwrap_err()),
            Some("usage")
        );
    }
    #[test]
    fn rtsp_refusals_name_digest_algorithms_ffmpeg_cannot_answer() {
        let md5 =
            r#"Digest realm="IP Camera(C1234)", nonce="a1b2c3", stale="FALSE", algorithm="MD5""#;
        let sha = r#"Digest realm="IP Camera(C1234)", nonce="d4e5f6", stale="FALSE", algorithm="SHA-256""#;
        let basic = r#"Basic realm="IP Camera(C1234)""#;
        let check = |values: &[&str]| {
            let values = values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
            ffmpeg_cannot_answer(&values, "rtsp://cam/live")
        };
        // FFmpeg keeps the last Digest challenge and computes MD5 only.
        assert_eq!(check(&[sha, md5]), None);
        assert_eq!(check(&[basic]), None);
        assert_eq!(check(&[md5, basic]), None);
        assert_eq!(
            check(&[r#"Digest realm="r", nonce="n", qop="auth,auth-int""#]),
            None
        );
        let both = check(&[md5, sha]).unwrap();
        assert!(both.contains("MD5, SHA-256"), "{both}");
        assert!(both.contains("last challenge, SHA-256"), "{both}");
        let only = check(&[basic, sha]).unwrap();
        assert!(only.contains("Digest sign-in with SHA-256"), "{only}");
        assert!(only.contains("enable MD5 Digest"), "{only}");
        // Case matters to FFmpeg, and auth-int alone has no answer.
        assert!(check(&[r#"Digest realm="r", nonce="n", algorithm=md5"#]).is_some());
        assert!(
            check(&[r#"Digest realm="r", nonce="n", qop="auth-int""#])
                .unwrap()
                .contains("MD5 with qop auth-int")
        );

        let rtsp = |reply: String| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let request = read_line(&mut reader, 4096).unwrap();
                while read_line(&mut reader, 8192).unwrap() != b"\r\n" {}
                stream.write_all(reply.as_bytes()).unwrap();
                String::from_utf8(request).unwrap()
            });
            (address, server)
        };
        let refused = || crate::logins::auth_error("rtsp://cam/live", true);
        let timeout = Duration::from_secs(2);
        let (address, server) = rtsp(format!(
            "RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: {md5}\r\nWWW-Authenticate: {sha}\r\n\r\n"
        ));
        let uri = format!("rtsp://admin:pw@{address}/Streaming/Channels/101");
        let error = explain_rtsp_refusal(refused(), &uri, timeout);
        let request = server.join().unwrap();
        assert_eq!(
            request,
            format!("DESCRIBE rtsp://{address}/Streaming/Channels/101 RTSP/1.0\r\n")
        );
        assert_eq!(code(&error), Some("unsupported"), "{error:#}");
        let text = format!("{error:#}");
        assert!(
            text.starts_with(&format!(
                "rtsp://{address}/Streaming/Channels/101 offers Digest"
            )) && !text.contains("pw@"),
            "{text}"
        );
        // A challenge FFmpeg answers means the password itself was refused.
        let (address, server) = rtsp(format!(
            "RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: {md5}\r\n\r\n"
        ));
        let error = explain_rtsp_refusal(refused(), &format!("rtsp://{address}/live"), timeout);
        server.join().unwrap();
        assert_eq!(code(&error), Some("auth_rejected"));
        // Other failures and rtsps:// are left alone, without a probe.
        let error = explain_rtsp_refusal(anyhow!("decoder failed"), "rtsp://127.0.0.1:9/", timeout);
        assert_eq!(error.to_string(), "decoder failed");
        let error = explain_rtsp_refusal(refused(), "rtsps://127.0.0.1:9/", timeout);
        assert_eq!(code(&error), Some("auth_rejected"));
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
