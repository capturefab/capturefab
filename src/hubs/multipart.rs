//! `multipart/x-mixed-replace` parsing for MJPEG streams, tolerant of the
//! dialects hubs send: Home Assistant declares `boundary=--frameboundary` and
//! delimits parts with `--frameboundary`; Frigate sends parts without
//! `Content-Length`; others quote the boundary, omit it, or lead with blank
//! lines. A part without a length is cut at the JPEG end-of-image marker, so
//! a frame is ready without waiting for the next part to begin.
use std::io::{self, BufRead, BufReader, Read};

const MAX_PREAMBLE: usize = 64 * 1024;
const MAX_PART_HEADERS: usize = 16 * 1024;
const MAX_LINE: usize = 1024;
/// The largest part accepted.
pub(crate) const MAX_PART: usize = 32 * 1024 * 1024;

pub(crate) struct Part {
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum PartError {
    Io(io::Error),
    Malformed(&'static str),
    TooLarge,
}

impl From<io::Error> for PartError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The boundary parameter of a `multipart/*` content type, unquoted.
pub(crate) fn boundary(content_type: &str) -> Option<Vec<u8>> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        (!value.is_empty() && value.len() <= 200).then(|| value.as_bytes().to_vec())
    })
}

pub(crate) struct Multipart<R: Read> {
    reader: BufReader<R>,
    boundary: Option<Vec<u8>>,
}

enum Delimiter {
    Part,
    Close,
}

impl<R: Read> Multipart<R> {
    pub(crate) fn new(body: R, content_type: &str) -> Self {
        Self {
            reader: BufReader::with_capacity(64 * 1024, body),
            boundary: boundary(content_type),
        }
    }

    /// Any `--` line starts a part, as FFmpeg's demuxer accepts: servers
    /// disagree about whether the declared boundary includes the dashes.
    fn delimiter(&mut self, line: &[u8]) -> Option<Delimiter> {
        let line = trim_end(line);
        if !line.starts_with(b"--") || line.len() < 3 {
            return None;
        }
        let closes = |marker: &[u8]| {
            line.len() == marker.len() + 2 && line.starts_with(marker) && line.ends_with(b"--")
        };
        if let Some(boundary) = &self.boundary {
            let dashed = [b"--".as_slice(), boundary].concat();
            if closes(&dashed) || (boundary.starts_with(b"--") && closes(boundary)) {
                return Some(Delimiter::Close);
            }
        } else {
            self.boundary = Some(line[2..].to_vec());
        }
        Some(Delimiter::Part)
    }

    /// The next part, or `None` when the stream ends.
    pub(crate) fn next_part(&mut self) -> Result<Option<Part>, PartError> {
        let mut skipped = 0;
        loop {
            match read_line(&mut self.reader, MAX_LINE)? {
                Line::Eof => return Ok(None),
                Line::Long(count) => skipped += count,
                Line::Text(line) => match self.delimiter(&line) {
                    Some(Delimiter::Part) => break,
                    Some(Delimiter::Close) => return Ok(None),
                    None => skipped += line.len() + 1,
                },
            }
            if skipped > MAX_PREAMBLE {
                return Err(PartError::Malformed("no part delimiter"));
            }
        }
        let mut content_type = None;
        let mut length = None;
        let mut size = 0;
        loop {
            let line = match read_line(&mut self.reader, MAX_LINE)? {
                Line::Text(line) => line,
                Line::Long(_) => return Err(PartError::Malformed("part header too long")),
                Line::Eof => return Ok(None),
            };
            if line.is_empty() {
                break;
            }
            size += line.len();
            if size > MAX_PART_HEADERS {
                return Err(PartError::Malformed("part headers too long"));
            }
            let text = String::from_utf8_lossy(&line);
            let Some((name, value)) = text.split_once(':') else {
                return Err(PartError::Malformed("bad part header"));
            };
            let value = value.trim();
            if name.trim().eq_ignore_ascii_case("content-type") {
                content_type = Some(value.to_ascii_lowercase());
            } else if name.trim().eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| PartError::Malformed("bad part length"))?,
                );
            }
        }
        let bytes = match length {
            Some(length) if length > MAX_PART => return Err(PartError::TooLarge),
            Some(length) => {
                let mut bytes = vec![0; length];
                self.reader.read_exact(&mut bytes)?;
                bytes
            }
            None if self.reader.fill_buf()?.starts_with(&[0xFF, 0xD8]) => {
                let mut bytes = Vec::new();
                read_jpeg(&mut self.reader, &mut bytes)?;
                bytes
            }
            None => self.read_until_delimiter()?,
        };
        Ok(Some(Part {
            content_type,
            bytes,
        }))
    }

    /// A part of unknown type and length: everything up to the CRLF before
    /// the next `--` line, which is left for the next call.
    fn read_until_delimiter(&mut self) -> Result<Vec<u8>, PartError> {
        let mut bytes = Vec::new();
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(index) => {
                    bytes.extend_from_slice(&available[..=index]);
                    self.reader.consume(index + 1);
                    let next = self.reader.fill_buf()?;
                    if next.starts_with(b"--") || next.is_empty() {
                        break;
                    }
                }
                None => {
                    let count = available.len();
                    bytes.extend_from_slice(available);
                    self.reader.consume(count);
                }
            }
            if bytes.len() > MAX_PART {
                return Err(PartError::TooLarge);
            }
        }
        if bytes.ends_with(b"\r\n") {
            bytes.truncate(bytes.len() - 2);
        } else if bytes.ends_with(b"\n") {
            bytes.pop();
        }
        Ok(bytes)
    }
}

fn trim_end(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|b| !b" \t".contains(b))
        .map_or(0, |index| index + 1);
    &line[..end]
}

enum Line {
    Text(Vec<u8>),
    /// A line longer than the limit, consumed and discarded.
    Long(usize),
    Eof,
}

/// One line without its CRLF or LF. Lines over `limit` are skipped, but
/// never more than `MAX_PREAMBLE` bytes of one, so a stream without line
/// breaks cannot keep the reader busy forever.
fn read_line(reader: &mut impl BufRead, limit: usize) -> io::Result<Line> {
    let mut line = Vec::new();
    let mut long = 0;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if line.is_empty() && long == 0 {
                Line::Eof
            } else if long > 0 {
                Line::Long(long)
            } else {
                Line::Text(line)
            });
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(index) => (index + 1, true),
            None => (available.len(), false),
        };
        if long == 0 && line.len() + take <= limit + 2 {
            line.extend_from_slice(&available[..take]);
        } else {
            long += line.len() + take;
            line.clear();
        }
        reader.consume(take);
        if long > MAX_PREAMBLE {
            return Ok(Line::Long(long));
        }
        if done {
            if long > 0 {
                return Ok(Line::Long(long));
            }
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Line::Text(line));
        }
    }
}

/// Copy one JPEG from `reader` into `out`, ending after its EOI marker.
/// Marker segments are skipped by their lengths; in entropy-coded data
/// `FF 00` is a stuffed byte and `FF D0`..`FF D7` are restart markers.
pub(crate) fn read_jpeg(reader: &mut impl BufRead, out: &mut Vec<u8>) -> Result<(), PartError> {
    fn byte(reader: &mut impl BufRead, out: &mut Vec<u8>) -> Result<u8, PartError> {
        let available = reader.fill_buf()?;
        let Some(&value) = available.first() else {
            return Err(PartError::Malformed("JPEG ended early"));
        };
        reader.consume(1);
        if out.len() >= MAX_PART {
            return Err(PartError::TooLarge);
        }
        out.push(value);
        Ok(value)
    }
    if byte(reader, out)? != 0xFF || byte(reader, out)? != 0xD8 {
        return Err(PartError::Malformed("not a JPEG"));
    }
    let mut pending = None;
    loop {
        let marker = match pending.take() {
            Some(marker) => marker,
            None => {
                if byte(reader, out)? != 0xFF {
                    return Err(PartError::Malformed("JPEG marker expected"));
                }
                let mut marker = byte(reader, out)?;
                while marker == 0xFF {
                    marker = byte(reader, out)?;
                }
                marker
            }
        };
        match marker {
            0xD9 => return Ok(()),
            0x01 | 0xD0..=0xD7 => continue,
            0x00 | 0xD8 => return Err(PartError::Malformed("bad JPEG marker")),
            _ => {}
        }
        let length = u16::from_be_bytes([byte(reader, out)?, byte(reader, out)?]) as usize;
        if length < 2 {
            return Err(PartError::Malformed("bad JPEG segment length"));
        }
        let mut left = length - 2;
        while left > 0 {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Err(PartError::Malformed("JPEG ended early"));
            }
            let count = available.len().min(left);
            if out.len() + count > MAX_PART {
                return Err(PartError::TooLarge);
            }
            out.extend_from_slice(&available[..count]);
            reader.consume(count);
            left -= count;
        }
        if marker != 0xDA {
            continue;
        }
        // Entropy-coded data runs until a marker that is not a stuffed byte
        // or a restart.
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Err(PartError::Malformed("JPEG ended early"));
            }
            match available.iter().position(|&b| b == 0xFF) {
                None => {
                    let count = available.len();
                    if out.len() + count > MAX_PART {
                        return Err(PartError::TooLarge);
                    }
                    out.extend_from_slice(available);
                    reader.consume(count);
                }
                Some(index) => {
                    if out.len() + index > MAX_PART {
                        return Err(PartError::TooLarge);
                    }
                    out.extend_from_slice(&available[..index]);
                    reader.consume(index);
                    byte(reader, out)?;
                    let mut next = byte(reader, out)?;
                    while next == 0xFF {
                        next = byte(reader, out)?;
                    }
                    match next {
                        0x00 | 0xD0..=0xD7 => {}
                        other => {
                            pending = Some(other);
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hubs::image::tests::jpeg_of;

    fn parts(stream: &[u8], content_type: &str) -> Vec<Vec<u8>> {
        let mut multipart = Multipart::new(stream, content_type);
        let mut parts = Vec::new();
        while let Some(part) = multipart.next_part().unwrap() {
            parts.push(part.bytes);
        }
        parts
    }

    fn with_length(boundary: &str, jpegs: &[Vec<u8>]) -> Vec<u8> {
        let mut stream = Vec::new();
        for jpeg in jpegs {
            stream.extend(
                format!(
                    "{boundary}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    jpeg.len()
                )
                .as_bytes(),
            );
            stream.extend(jpeg);
            stream.extend(b"\r\n");
        }
        stream
    }

    #[test]
    fn home_assistant_and_go2rtc_style_streams_split_by_length() {
        let jpegs = [jpeg_of(16, 16, 1), jpeg_of(16, 16, 2)];
        // Home Assistant declares the dashes in the boundary itself.
        let stream = with_length("--frameboundary", &jpegs);
        assert_eq!(
            parts(
                &stream,
                "multipart/x-mixed-replace; boundary=--frameboundary"
            ),
            jpegs
        );
        let stream = with_length("--frame", &jpegs);
        assert_eq!(
            parts(&stream, "multipart/x-mixed-replace; boundary=frame"),
            jpegs
        );
        // Quoted, missing and case-varied boundary parameters, a preamble,
        // leading blank lines and a closing delimiter.
        let mut stream = b"\r\n\r\nignored preamble\r\n".to_vec();
        stream.extend(with_length("--myboundary", &jpegs));
        stream.extend(b"--myboundary--\r\n");
        for content_type in [
            "multipart/x-mixed-replace;BOUNDARY=\"myboundary\"",
            "multipart/x-mixed-replace",
        ] {
            assert_eq!(parts(&stream, content_type), jpegs, "{content_type}");
        }
    }

    #[test]
    fn frigate_style_parts_without_length_end_at_the_jpeg_marker() {
        let jpegs = [jpeg_of(24, 16, 30), jpeg_of(24, 16, 60), jpeg_of(8, 8, 90)];
        let mut stream = Vec::new();
        for jpeg in &jpegs {
            stream.extend(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n");
            stream.extend(jpeg);
            stream.extend(b"\r\n\r\n");
        }
        assert_eq!(
            parts(&stream, "multipart/x-mixed-replace;boundary=frame"),
            jpegs
        );
        // A part is complete before any byte of the next one arrives.
        let mut single = b"--frame\r\ncontent-type: image/jpeg\r\n\r\n".to_vec();
        single.extend(&jpegs[0]);
        let mut multipart = Multipart::new(
            single.as_slice(),
            "multipart/x-mixed-replace;boundary=frame",
        );
        assert_eq!(multipart.next_part().unwrap().unwrap().bytes, jpegs[0]);
    }

    #[test]
    fn the_jpeg_scan_respects_stuffing_and_restart_markers() {
        // SOI, DQT-like segment, SOS with entropy data containing FF00 and
        // FFD3, then EOI.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0xFF, 0xD9, 0xFF, 0xDA, 0x00, 0x02, 0x11, 0xFF,
            0x00, 0x22, 0xFF, 0xD3, 0x33, 0xFF, 0xD9,
        ];
        let mut out = Vec::new();
        let mut reader = BufReader::new(&jpeg[..]);
        read_jpeg(&mut reader, &mut out).unwrap();
        assert_eq!(out, jpeg);
        let mut truncated = BufReader::new(&jpeg[..12]);
        assert!(read_jpeg(&mut truncated, &mut Vec::new()).is_err());
    }

    #[test]
    fn unknown_parts_and_limits() {
        let stream =
            b"--b\r\nContent-Type: text/plain\r\n\r\nhello\r\nworld\r\n--b\r\n\r\nraw\r\n--b--\r\n";
        assert_eq!(
            parts(stream, "multipart/x-mixed-replace;boundary=b"),
            [b"hello\r\nworld".to_vec(), b"raw".to_vec()]
        );
        let huge = format!("--b\r\nContent-Length: {}\r\n\r\n", MAX_PART + 1);
        let mut multipart = Multipart::new(huge.as_bytes(), "multipart/x-mixed-replace;boundary=b");
        assert!(matches!(multipart.next_part(), Err(PartError::TooLarge)));
        let junk = vec![b'x'; MAX_PREAMBLE * 2];
        let mut multipart = Multipart::new(junk.as_slice(), "multipart/x-mixed-replace;boundary=b");
        assert!(matches!(
            multipart.next_part(),
            Err(PartError::Malformed(_))
        ));
    }
}
