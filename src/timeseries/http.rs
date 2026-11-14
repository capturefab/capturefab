//! Bounded HTTP/1 response framing shared by industrial HTTP sensor adapters.
//! RFC 9112 sections 4, 6 and 7: <https://www.rfc-editor.org/rfc/rfc9112>.
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use std::{
    fmt,
    io::{self, BufRead as _, BufReader, Read as _},
};
use zeroize::{Zeroize as _, Zeroizing};

const MAX_HEADER: usize = 16 * 1024;

#[derive(Debug)]
pub(super) struct Status(pub(super) u16);
impl fmt::Display for Status {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "HTTP server rejected read (status {})", self.0)
    }
}
impl std::error::Error for Status {}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
pub(super) fn io_error(error: &io::Error) -> anyhow::Error {
    #[cfg(feature = "mqtt-tls")]
    if error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync + 'static>::is::<rustls::Error>)
    {
        return Categorized::error(
            ErrorKind::AuthRejected,
            "HTTP TLS certificate verification or handshake failed",
        );
    }
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::UnexpectedEof => ErrorKind::Incomplete,
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::BrokenPipe => ErrorKind::Disconnected,
        _ => ErrorKind::Unavailable,
    };
    Categorized::error(kind, format!("HTTP I/O failed: {error}"))
}
pub(super) fn response(stream: &mut impl io::Read, limit: usize) -> Result<Vec<u8>> {
    let mut response = parse_response(stream, limit, false)?;
    Ok(std::mem::take(&mut response.body))
}

pub(super) struct Response {
    pub(super) status: u16,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
}

impl Drop for Response {
    fn drop(&mut self) {
        for (name, value) in &mut self.headers {
            name.zeroize();
            value.zeroize();
        }
    }
}

pub(super) fn response_with_headers(stream: &mut impl io::Read, limit: usize) -> Result<Response> {
    parse_response(stream, limit, true)
}

fn selected_header(key: &str) -> bool {
    ["content-type", "set-cookie", "www-authenticate"]
        .iter()
        .any(|name| key.eq_ignore_ascii_case(name))
}

fn parse_response(
    stream: &mut impl io::Read,
    limit: usize,
    retain_headers: bool,
) -> Result<Response> {
    let mut reader = BufReader::new(stream);
    let mut framing = 0;
    let status = line(&mut reader, &mut framing)?;
    if status
        .iter()
        .any(|byte| (*byte < 32 && *byte != b'\t') || *byte == 127)
    {
        return Err(malformed("invalid HTTP status characters"));
    }
    let mut words = status
        .split(|byte| matches!(byte, b' ' | b'\t'))
        .filter(|word| !word.is_empty());
    if !matches!(words.next(), Some(b"HTTP/1.0" | b"HTTP/1.1")) {
        return Err(malformed("invalid HTTP response version"));
    }
    let code = words
        .next()
        .filter(|word| word.len() == 3)
        .and_then(|word| std::str::from_utf8(word).ok())
        .and_then(|word| word.parse::<u16>().ok())
        .ok_or_else(|| malformed("invalid HTTP response status"))?;
    if !retain_headers && code != 200 {
        return Err(Status(code).into());
    }
    let mut response = Response {
        status: code,
        headers: Vec::new(),
        body: Vec::new(),
    };
    let mut length = None;
    let mut chunked = false;
    let mut fields = 0;
    loop {
        let header = line(&mut reader, &mut framing)?;
        if header.is_empty() {
            break;
        }
        fields += 1;
        if fields > 64 {
            return Err(malformed("too many HTTP headers"));
        }
        let (key, value) = field(&header)?;
        if retain_headers && selected_header(key) {
            let value = std::str::from_utf8(value)
                .map_err(|_| malformed("non-UTF-8 selected HTTP header"))?
                .trim_matches([' ', '\t']);
            response
                .headers
                .push((key.to_ascii_lowercase(), value.to_owned()));
        }
        // Unknown field values can contain RFC 9110 obs-text (opaque bytes).
        // Only the framing fields below require ASCII parsing.
        let known = ["content-length", "transfer-encoding", "content-encoding"]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name));
        if !known {
            continue;
        }
        if !value.is_ascii() {
            return Err(malformed("non-ASCII HTTP framing value"));
        }
        let value = std::str::from_utf8(value)
            .map_err(|_| malformed("non-ASCII HTTP framing value"))?
            .trim();
        if key.eq_ignore_ascii_case("content-length") {
            if length.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(malformed("invalid or duplicate HTTP content length"));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| malformed("HTTP content length overflows"))?,
            );
        } else if key.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(malformed("unsupported or duplicate HTTP transfer encoding"));
            }
            chunked = true;
        } else if key.eq_ignore_ascii_case("content-encoding")
            && !value.eq_ignore_ascii_case("identity")
        {
            return Err(malformed("compressed HTTP responses are unsupported"));
        }
    }
    if chunked && length.is_some() {
        return Err(malformed("ambiguous HTTP body framing"));
    }
    let mut body = Vec::new();
    if chunked {
        // Each positive chunk consumes at least one byte of the bounded body;
        // one final iteration permits the terminating zero-size chunk. Chunk
        // metadata has its own per-line budget, independent of initial headers.
        for _ in 0..=limit {
            let mut chunk_framing = 0;
            let size = line(&mut reader, &mut chunk_framing)?;
            let size = chunk_size(&size)?;
            if size == 0 {
                // RFC 9112 section 7.1.2: trailers are a separate field section.
                // The aggregate budget also bounds the number of field lines.
                let mut trailer_framing = 0;
                loop {
                    let trailer = line(&mut reader, &mut trailer_framing)?;
                    if trailer.is_empty() {
                        response.body = body;
                        return Ok(response);
                    }
                    let (key, _) = field(&trailer)?;
                    if ["content-length", "transfer-encoding", "content-encoding"]
                        .iter()
                        .any(|name| key.eq_ignore_ascii_case(name))
                    {
                        return Err(malformed(
                            "HTTP trailer cannot redefine body framing or encoding",
                        ));
                    }
                    if retain_headers && selected_header(key) {
                        return Err(malformed(
                            "HTTP trailer cannot supply content type, cookies or authentication",
                        ));
                    }
                }
            }
            append(&mut reader, &mut body, size, limit)?;
            let mut end = [0; 2];
            reader
                .read_exact(&mut end)
                .map_err(|error| io_error(&error))?;
            if end != *b"\r\n" {
                return Err(malformed("invalid HTTP chunk delimiter"));
            }
        }
        return Err(malformed("HTTP response body exceeds size limit"));
    }
    if let Some(length) = length {
        append(&mut reader, &mut body, length, limit)?;
    } else {
        reader
            .take(u64::try_from(limit + 1)?)
            .read_to_end(&mut body)
            .map_err(|error| io_error(&error))?;
        if body.len() > limit {
            return Err(malformed("HTTP response body exceeds size limit"));
        }
    }
    response.body = body;
    Ok(response)
}
fn tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}
fn token_end(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|byte| !tchar(*byte))
        .unwrap_or(bytes.len())
}
fn whitespace(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    &bytes[end..]
}
fn chunk_size(bytes: &[u8]) -> Result<usize> {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_hexdigit())
        .unwrap_or(bytes.len());
    if end == 0 {
        return Err(malformed("invalid HTTP chunk size"));
    }
    let size = usize::from_str_radix(std::str::from_utf8(&bytes[..end])?, 16)
        .map_err(|_| malformed("HTTP chunk size overflows"))?;
    let invalid = || malformed("invalid HTTP chunk extension");
    let mut rest = &bytes[end..];
    // RFC 9112 section 7.1.1: ignore unknown extension values after validating
    // their bounded token/quoted-string syntax. Quoted strings permit obs-text.
    while !rest.is_empty() {
        rest = whitespace(rest).strip_prefix(b";").ok_or_else(invalid)?;
        rest = whitespace(rest);
        let end = token_end(rest);
        if end == 0 {
            return Err(invalid());
        }
        rest = &rest[end..];
        if let Some(value) = whitespace(rest).strip_prefix(b"=") {
            rest = whitespace(value);
            if let Some(quoted) = rest.strip_prefix(b"\"") {
                rest = quoted;
                loop {
                    let (&byte, next) = rest.split_first().ok_or_else(invalid)?;
                    rest = next;
                    if byte == b'"' {
                        break;
                    }
                    let byte = if byte == b'\\' {
                        let (&escaped, next) = rest.split_first().ok_or_else(invalid)?;
                        rest = next;
                        escaped
                    } else {
                        byte
                    };
                    if (byte < 32 && byte != b'\t') || byte == 127 {
                        return Err(invalid());
                    }
                }
            } else {
                let end = token_end(rest);
                if end == 0 {
                    return Err(invalid());
                }
                rest = &rest[end..];
            }
        }
    }
    Ok(size)
}
fn field(bytes: &[u8]) -> Result<(&str, &[u8])> {
    let colon = bytes
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| malformed("malformed HTTP field"))?;
    let (key, rest) = bytes.split_at(colon);
    if key.is_empty() || !key.iter().all(|byte| tchar(*byte)) {
        return Err(malformed("invalid HTTP field name"));
    }
    let value = &rest[1..];
    if value
        .iter()
        .any(|byte| (*byte < 32 && *byte != b'\t') || *byte == 127)
    {
        return Err(malformed("invalid HTTP field value"));
    }
    Ok((std::str::from_utf8(key)?, value))
}
fn append(
    reader: &mut impl io::Read,
    body: &mut Vec<u8>,
    count: usize,
    limit: usize,
) -> Result<()> {
    let end = body
        .len()
        .checked_add(count)
        .filter(|end| *end <= limit)
        .ok_or_else(|| malformed("HTTP response body exceeds size limit"))?;
    let start = body.len();
    body.resize(end, 0);
    reader
        .read_exact(&mut body[start..])
        .map_err(|error| io_error(&error))?;
    Ok(())
}
fn line(reader: &mut impl io::BufRead, framing: &mut usize) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(u64::try_from(MAX_HEADER.saturating_sub(*framing) + 1)?)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| io_error(&error))?;
    *framing += bytes.len();
    if *framing > MAX_HEADER || !bytes.ends_with(b"\r\n") {
        return Err(malformed("truncated or oversized HTTP response framing"));
    }
    let length = bytes.len() - 2;
    bytes.truncate(length);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(bytes: &[u8], limit: usize) {
        let mut input = bytes;
        let Err(error) = response_with_headers(&mut input, limit) else {
            panic!("invalid HTTP response was accepted");
        };
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    }

    #[test]
    fn legacy_status_returns_without_requesting_headers_or_body() {
        struct OnlyStatus {
            reads: usize,
        }
        impl io::Read for OnlyStatus {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads != 1 {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "no headers"));
                }
                let status = b"HTTP/1.1 401 Unauthorized\r\n";
                bytes[..status.len()].copy_from_slice(status);
                Ok(status.len())
            }
        }
        let mut legacy = OnlyStatus { reads: 0 };
        let error = response(&mut legacy, 16).unwrap_err();
        assert_eq!(error.downcast_ref::<Status>().unwrap().0, 401);
        assert_eq!(legacy.reads, 1);
        let mut selected = OnlyStatus { reads: 0 };
        let Err(error) = response_with_headers(&mut selected, 16) else {
            panic!("incomplete selected-header response was accepted");
        };
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        assert_eq!(selected.reads, 2);
    }

    #[test]
    fn selected_headers_preserve_repetition_and_non_success_body() {
        let bytes = b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/xhtml+xml;v=2.0\r\nSet-Cookie: ABBCX=one; Path=/\r\nset-cookie: http-session=two; Expires=Wed, 09 Jun 2027 10:18:14 GMT\r\nWWW-Authenticate: Digest realm=\"robot\", nonce=\"n\"\r\nwww-authenticate: Basic realm=\"robot\"\r\nX-Opaque: \xff\r\nContent-Length: 3\r\n\r\nbad";
        let result = response_with_headers(&mut &bytes[..], 3).unwrap();
        assert_eq!(result.status, 401);
        assert_eq!(result.body, b"bad");
        assert_eq!(
            result.headers,
            [
                ("content-type", "application/xhtml+xml;v=2.0"),
                ("set-cookie", "ABBCX=one; Path=/"),
                (
                    "set-cookie",
                    "http-session=two; Expires=Wed, 09 Jun 2027 10:18:14 GMT"
                ),
                ("www-authenticate", "Digest realm=\"robot\", nonce=\"n\""),
                ("www-authenticate", "Basic realm=\"robot\""),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        );
        let opaque = b"HTTP/1.1 200 OK\r\nContent-Type: \xff\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(response(&mut &opaque[..], 0).unwrap(), b"");
        rejected(opaque, 0);
        let unknown = b"HTTP/1.1 200 OK\r\nX-Opaque: \xff\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(
            response_with_headers(&mut &unknown[..], 0).unwrap().body,
            b""
        );
    }

    #[test]
    fn selected_header_mode_keeps_body_framing_and_trailer_bounds() {
        let good = b"HTTP/1.1 200 OK\r\nContent-Type: application/xhtml+xml\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\nX-Checksum: opaque\r\n\r\n";
        let parsed = response_with_headers(&mut &good[..], 2).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, b"ok");
        assert_eq!(parsed.headers.len(), 1);
        for field in [
            "Set-Cookie: ABBCX=trailer",
            "WWW-Authenticate: Basic realm=robot",
            "Content-Type: text/plain",
        ] {
            let bytes = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n{field}\r\n\r\n"
            );
            rejected(bytes.as_bytes(), 0);
            // The old body-only API still ignores these otherwise valid trailers.
            assert_eq!(response(&mut bytes.as_bytes(), 0).unwrap(), b"");
        }
        for bytes in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\nx"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\n\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"[..],
        ] {
            rejected(bytes, 2);
        }
        let fields = format!(
            "HTTP/1.1 200 OK\r\n{}Content-Length: 0\r\n\r\n",
            "X-Extra: x\r\n".repeat(64)
        );
        rejected(fields.as_bytes(), 0);
        let headers = format!(
            "HTTP/1.1 200 OK\r\nSet-Cookie: {}\r\n\r\n",
            "x".repeat(MAX_HEADER)
        );
        rejected(headers.as_bytes(), 0);
    }
}
