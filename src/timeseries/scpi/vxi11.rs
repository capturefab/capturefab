//! VXI-11.3 core transport for the configured SCPI queries only.
//! VXI-11 Rev1.0 §§B.4.4/B.5/B.6.1–4 and Appendix C, RFC5531 §11,
//! and RFC4506 define link lifecycle, END, RPC records and XDR respectively.
use super::super::net;
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::{
    io,
    net::{IpAddr, SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const CORE: u32 = 0x0006_07af;
const MAX_DATA: usize = 64 * 1024;
const MAX_FRAGMENTS: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    host: IpAddr,
    #[serde(default = "device")]
    device: String,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    core_port: Option<u16>,
}
fn device() -> String {
    "inst0".into()
}

struct Link {
    id: u32,
    max_write: usize,
}

pub(super) struct Client {
    config: Config,
    rpc: Option<Rpc>,
    link: Option<Link>,
    pending: Vec<u8>,
    offset: usize,
    end: bool,
    reading: bool,
    observed: Option<u64>,
}

impl Client {
    pub(super) fn new(config: &Config) -> Result<Self> {
        if config.host.is_unspecified()
            || config.core_port == Some(0)
            || config.device.is_empty()
            || config.device.len() > 255
            || !config.device.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Usage("VXI-11 requires a specific numeric host, nonzero optional core_port and a 1–255 byte ASCII device name".into()).into());
        }
        Ok(Self {
            config: config.clone(),
            rpc: None,
            link: None,
            pending: Vec::new(),
            offset: 0,
            end: false,
            reading: false,
            observed: None,
        })
    }

    pub(super) fn begin(&mut self, deadline: Instant) -> Result<()> {
        if self.rpc.is_some() {
            return Ok(());
        }
        let port = match self.config.core_port {
            Some(port) => port,
            None => portmapper(SocketAddr::new(self.config.host, 111), deadline)?,
        };
        self.rpc = Some(Rpc::connect(
            SocketAddr::new(self.config.host, port),
            deadline,
        )?);
        let mut body = Vec::new();
        words(&mut body, &[0, 0, 0]); // clientId=0, lockDevice=false, lock_timeout=0.
        opaque(&mut body, self.config.device.as_bytes())?;
        let reply = self.call(10, &body, deadline, 16)?;
        let mut reply = Xdr::new(&reply);
        let code = reply.word()?;
        let id = reply.word()?;
        let abort_port = reply.word()?;
        let max_write = reply.word()?;
        reply.done()?;
        status(code)?;
        if abort_port > u32::from(u16::MAX) || max_write < 1024 {
            return Err(protocol("invalid create_link limits"));
        }
        self.link = Some(Link {
            id,
            max_write: usize::try_from(max_write)?.min(MAX_DATA),
        });
        Ok(())
    }

    fn call(
        &mut self,
        procedure: u32,
        body: &[u8],
        deadline: Instant,
        limit: usize,
    ) -> Result<Vec<u8>> {
        self.rpc
            .as_mut()
            .ok_or_else(|| protocol("core channel is closed"))?
            .call(CORE, 1, procedure, body, deadline, limit)
    }

    pub(super) fn send_query(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
        if bytes.is_empty() || self.reading || self.offset != self.pending.len() {
            return Err(protocol("query overlaps an unfinished response"));
        }
        let link = self
            .link
            .as_ref()
            .ok_or_else(|| protocol("link is not initialized"))?;
        let (id, max_write) = (link.id, link.max_write);
        let last = bytes.len().div_ceil(max_write) - 1;
        for (index, chunk) in bytes.chunks(max_write).enumerate() {
            let mut body = Vec::new();
            words(
                &mut body,
                &[
                    id,
                    timeout_ms(deadline)?,
                    0,
                    if index == last { 8 } else { 0 },
                ],
            );
            opaque(&mut body, chunk)?;
            let reply = self.call(11, &body, deadline, 8)?;
            let mut reply = Xdr::new(&reply);
            let code = reply.word()?;
            let size = reply.word()?;
            reply.done()?;
            status(code)?;
            if usize::try_from(size)? != chunk.len() {
                return Err(protocol("device_write accepted an incomplete query"));
            }
        }
        self.pending.clear();
        self.offset = 0;
        self.end = false;
        self.reading = true;
        Ok(())
    }

    fn read_chunk(&mut self, requested: usize, deadline: Instant) -> Result<()> {
        let id = self
            .link
            .as_ref()
            .ok_or_else(|| protocol("link is not initialized"))?
            .id;
        let requested = requested.clamp(1, MAX_DATA);
        let mut body = Vec::new();
        words(
            &mut body,
            &[
                id,
                u32::try_from(requested)?,
                timeout_ms(deadline)?,
                0,
                0,
                0,
            ],
        );
        let reply = self.call(12, &body, deadline, requested + 15)?;
        let mut decoded = Xdr::new(&reply);
        let code = decoded.word()?;
        let reason = decoded.word()?;
        let data = decoded.opaque(requested)?;
        decoded.done()?;
        status(code)?;
        // No term character was requested; only REQCNT and END can be returned.
        if reason & !5 != 0 || (reason & 1 != 0 && data.len() != requested) {
            return Err(protocol("device_read returned inconsistent reason flags"));
        }
        self.pending = data.to_vec();
        self.offset = 0;
        self.end = reason & 4 != 0;
        self.observed = self.rpc.as_ref().and_then(|rpc| rpc.received);
        Ok(())
    }

    pub(super) fn receive(&mut self, output: &mut [u8], deadline: Instant) -> Result<usize> {
        if !self.reading {
            return Err(protocol("read without a query"));
        }
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            remaining(deadline)?;
            if self.offset < self.pending.len() {
                let count = (self.pending.len() - self.offset).min(output.len());
                let end = self.offset + count;
                let input = self
                    .pending
                    .get(self.offset..end)
                    .ok_or_else(|| protocol("invalid receive buffer range"))?;
                output
                    .get_mut(..count)
                    .ok_or_else(|| protocol("invalid output buffer range"))?
                    .copy_from_slice(input);
                self.offset += count;
                return Ok(count);
            }
            if self.end {
                return Ok(0);
            }
            // Successful reason=0 is a legal full-server-buffer continuation.
            self.read_chunk(output.len(), deadline)?;
        }
    }

    pub(super) fn ended(&self) -> bool {
        self.reading && self.end && self.offset == self.pending.len()
    }
    pub(super) fn observed(&self) -> Option<u64> {
        self.observed
    }

    pub(super) fn complete(&mut self, deadline: Instant) -> Result<()> {
        if !self.reading {
            return Err(protocol("missing response transaction"));
        }
        loop {
            if self.offset != self.pending.len() {
                return Err(protocol("extra data after configured SCPI response"));
            }
            if self.end {
                break;
            }
            self.read_chunk(1, deadline)?;
        }
        self.reading = false;
        self.pending.clear();
        self.offset = 0;
        Ok(())
    }

    pub(super) fn retire(&mut self) {
        // B.4.5 requires all link resources to be released on core TCP close.
        self.rpc = None;
        self.link = None;
        self.reading = false;
        self.pending.clear();
        self.offset = 0;
    }

    pub(super) fn finish(&mut self, timeout: Duration) -> Result<()> {
        let result = (|| {
            if self.rpc.is_none() || self.link.is_none() {
                return Ok(());
            }
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("VXI-11 finish timeout is too large".into()))?;
            let id = self
                .link
                .as_ref()
                .ok_or_else(|| protocol("link is not initialized"))?
                .id;
            let reply = self.call(23, &id.to_be_bytes(), deadline, 4)?;
            let mut reply = Xdr::new(&reply);
            let code = reply.word()?;
            reply.done()?;
            status(code)
        })();
        self.retire();
        result
    }
}

fn portmapper(address: SocketAddr, deadline: Instant) -> Result<u16> {
    let mut rpc = Rpc::connect(address, deadline)?;
    let mut body = Vec::new();
    words(&mut body, &[CORE, 1, 6, 0]);
    let reply = rpc.call(100_000, 2, 3, &body, deadline, 4)?;
    let mut reply = Xdr::new(&reply);
    let port = reply.word()?;
    reply.done()?;
    if port == 0 {
        return Err(Categorized::error(
            ErrorKind::Unavailable,
            "VXI-11 core TCP service is not registered",
        ));
    }
    u16::try_from(port).map_err(|_| protocol("portmapper returned an invalid core TCP port"))
}

struct Rpc {
    stream: Box<dyn net::Transport>,
    next_xid: u32,
    received: Option<u64>,
}

impl Rpc {
    fn connect(address: SocketAddr, deadline: Instant) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        stream.set_nodelay(true).map_err(|error| io_error(&error))?;
        Ok(Self {
            stream: net::plain(stream, deadline),
            next_xid: 1,
            received: None,
        })
    }

    fn call(
        &mut self,
        program: u32,
        version: u32,
        procedure: u32,
        body: &[u8],
        deadline: Instant,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let xid = self.next_xid;
        self.next_xid = xid
            .checked_add(1)
            .ok_or_else(|| protocol("RPC transaction identifiers exhausted"))?;
        let mut request = Vec::new();
        words(
            &mut request,
            &[xid, 0, 2, program, version, procedure, 0, 0, 0, 0],
        );
        request.extend_from_slice(body);
        let marker = (u32::try_from(request.len())? | 0x8000_0000).to_be_bytes();
        remaining(deadline)?;
        self.stream.set_deadline(deadline);
        io::Write::write_all(self.stream.as_mut(), &marker)
            .and_then(|()| io::Write::write_all(self.stream.as_mut(), &request))
            .map_err(|error| io_error(&error))?;
        let response = record(self.stream.as_mut(), deadline, limit + 424)?;
        self.received = crate::host::monotonic_ns();
        accepted(&response, xid, limit)
    }
}

fn record(stream: &mut dyn net::Transport, deadline: Instant, limit: usize) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    for _ in 0..MAX_FRAGMENTS {
        remaining(deadline)?;
        let mut marker = [0; 4];
        io::Read::read_exact(stream, &mut marker).map_err(|error| io_error(&error))?;
        let marker = u32::from_be_bytes(marker);
        let count = usize::try_from(marker & 0x7fff_ffff)?;
        let length = result
            .len()
            .checked_add(count)
            .filter(|length| *length <= limit)
            .ok_or_else(|| protocol("RPC response exceeds bounded record size"))?;
        let start = result.len();
        result.resize(length, 0);
        let tail = result
            .get_mut(start..)
            .ok_or_else(|| protocol("invalid RPC fragment range"))?;
        io::Read::read_exact(stream, tail).map_err(|error| io_error(&error))?;
        if marker & 0x8000_0000 != 0 {
            return Ok(result);
        }
    }
    Err(protocol("RPC response exceeds fragment count limit"))
}

fn accepted(response: &[u8], xid: u32, limit: usize) -> Result<Vec<u8>> {
    let mut decoded = Xdr::new(response);
    if decoded.word()? != xid || decoded.word()? != 1 || decoded.word()? != 0 {
        return Err(protocol(
            "RPC XID, reply type or acceptance differs from request",
        ));
    }
    let flavor = decoded.word()?;
    let verifier = decoded.opaque(400)?;
    if flavor != 0 || !verifier.is_empty() {
        return Err(protocol("RPC server requires unsupported authentication"));
    }
    if decoded.word()? != 0 {
        return Err(protocol("RPC procedure was not accepted"));
    }
    let body = decoded.rest();
    if body.len() > limit {
        return Err(protocol("RPC result exceeds requested size"));
    }
    Ok(body.to_vec())
}

struct Xdr<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Xdr<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn word(&mut self) -> Result<u32> {
        let end = self
            .offset
            .checked_add(4)
            .ok_or_else(|| protocol("XDR field overflow"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .ok_or_else(|| protocol("truncated XDR integer"))?;
        self.offset = end;
        Ok(u32::from_be_bytes(bytes))
    }
    fn opaque(&mut self, limit: usize) -> Result<&'a [u8]> {
        let count = usize::try_from(self.word()?)?;
        if count > limit {
            return Err(protocol("XDR opaque field exceeds requested size"));
        }
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| protocol("XDR opaque length overflow"))?;
        let padded = end
            .checked_add((4 - count % 4) % 4)
            .ok_or_else(|| protocol("XDR padding overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| protocol("truncated XDR opaque field"))?;
        if self
            .bytes
            .get(end..padded)
            .is_none_or(|bytes| bytes.iter().any(|byte| *byte != 0))
        {
            return Err(protocol("invalid XDR padding"));
        }
        self.offset = padded;
        Ok(value)
    }
    fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.offset..).unwrap_or_default()
    }
    fn done(&self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(protocol("extra XDR result bytes"))
        }
    }
}

fn words(output: &mut Vec<u8>, values: &[u32]) {
    for value in values {
        output.extend_from_slice(&value.to_be_bytes());
    }
}
fn opaque(output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    words(output, &[u32::try_from(bytes.len())?]);
    output.extend_from_slice(bytes);
    output.resize(output.len() + (4 - bytes.len() % 4) % 4, 0);
    Ok(())
}
fn timeout_ms(deadline: Instant) -> Result<u32> {
    Ok(u32::try_from(
        remaining(deadline)?.as_millis().min(u128::from(u32::MAX)),
    )?)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "VXI-11 total poll deadline expired",
        ));
    }
    Ok(left)
}
fn status(code: u32) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    let kind = match code {
        15 => ErrorKind::Timeout,
        8 => ErrorKind::Unsupported,
        3 | 11 => ErrorKind::Unavailable,
        _ => ErrorKind::OperationFailed,
    };
    Err(Categorized::new(kind, "VXI-11 RPC returned a device error")
        .details(serde_json::json!({"vxi11_status":code}))
        .into())
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("VXI-11: {message}"))
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("VXI-11 I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Memory {
        input: io::Cursor<Vec<u8>>,
        writes: Arc<Mutex<Vec<u8>>>,
    }
    impl io::Read for Memory {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            io::Read::read(&mut self.input, output)
        }
    }
    impl io::Write for Memory {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes
                .lock()
                .map_err(|_| io::Error::other("mock write buffer poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl net::Transport for Memory {
        fn set_deadline(&mut self, _deadline: Instant) {}
    }

    fn memory(bytes: Vec<u8>) -> (Memory, Arc<Mutex<Vec<u8>>>) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        (
            Memory {
                input: io::Cursor::new(bytes),
                writes: Arc::clone(&writes),
            },
            writes,
        )
    }
    fn reply(xid: u32, body: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        words(&mut data, &[xid, 1, 0, 0, 0, 0]);
        data.extend_from_slice(body);
        let mut wire = (0x8000_0000 | u32::try_from(data.len()).unwrap())
            .to_be_bytes()
            .to_vec();
        wire.extend(data);
        wire
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }
    fn client(replies: Vec<u8>) -> (Client, Arc<Mutex<Vec<u8>>>) {
        let (stream, writes) = memory(replies);
        let mut client = Client::new(&Config {
            host: "127.0.0.1".parse().unwrap(),
            device: "inst0".into(),
            core_port: Some(1234),
        })
        .unwrap();
        client.rpc = Some(Rpc {
            stream: Box::new(stream),
            next_xid: 1,
            received: None,
        });
        client.link = Some(Link {
            id: 0xffff_fffe,
            max_write: 1024,
        });
        (client, writes)
    }

    #[test]
    fn xdr_literal_fields_padding_and_rpc_record_fragments_match_the_specification() {
        let mut data = Vec::new();
        words(&mut data, &[0, 0, 0]);
        opaque(&mut data, b"inst0").unwrap();
        assert_eq!(
            data,
            [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, b'i', b'n', b's', b't', b'0', 0, 0,
                0
            ]
        );
        let mut decoded = Xdr::new(&data);
        for _ in 0..3 {
            assert_eq!(decoded.word().unwrap(), 0);
        }
        assert_eq!(decoded.opaque(5).unwrap(), b"inst0");
        decoded.done().unwrap();
        let mut fragments = 3u32.to_be_bytes().to_vec();
        fragments.extend_from_slice(b"abc");
        fragments.extend_from_slice(&0x8000_0002_u32.to_be_bytes());
        fragments.extend_from_slice(b"de");
        let (mut stream, _) = memory(fragments);
        assert_eq!(record(&mut stream, deadline(), 5).unwrap(), b"abcde");
        let (mut stream, writes) = memory(0xffff_ffff_u32.to_be_bytes().to_vec());
        let error = record(&mut stream, deadline(), 512).unwrap_err();
        assert!(error.to_string().contains("bounded record"));
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn successful_reason_zero_continues_until_end_without_inventing_bytes() {
        let mut replies = reply(1, &[0, 0, 0, 0, 0, 0, 0, 1]);
        let mut first = Vec::new();
        words(&mut first, &[0, 0]);
        opaque(&mut first, b"a").unwrap();
        replies.extend(reply(2, &first));
        let mut second = Vec::new();
        words(&mut second, &[0, 4]);
        opaque(&mut second, b"bc").unwrap();
        replies.extend(reply(3, &second));
        let (mut client, _) = client(replies);
        client.send_query(b"?", deadline()).unwrap();
        let mut output = [0; 3];
        assert_eq!(client.receive(&mut output, deadline()).unwrap(), 1);
        assert_eq!(output.first(), Some(&b'a'));
        assert!(!client.ended());
        assert_eq!(client.receive(&mut output, deadline()).unwrap(), 2);
        assert_eq!(output.get(..2).unwrap(), b"bc");
        assert!(client.ended());
        client.complete(deadline()).unwrap();
        assert!(!client.reading);
    }

    #[test]
    fn write_end_is_only_on_final_chunk_and_query_response_cannot_overlap() {
        let mut responses = Vec::new();
        for (xid, count) in [(1, 1024u32), (2, 1)] {
            let mut body = Vec::new();
            words(&mut body, &[0, count]);
            responses.extend(reply(xid, &body));
        }
        let (mut client, writes) = client(responses);
        client.send_query(&vec![b'?'; 1025], deadline()).unwrap();
        let writes = writes.lock().unwrap();
        let mut offset = 0;
        for (expected_len, flags) in [(1024u32, 0), (1, 8)] {
            let mut header = Xdr::new(writes.get(offset..).unwrap());
            let marker = header.word().unwrap();
            let length = usize::try_from(marker & 0x7fff_ffff).unwrap();
            let mut call = Xdr::new(writes.get(offset + 4..offset + 4 + length).unwrap());
            let prefix = std::iter::repeat_with(|| call.word().unwrap())
                .take(10)
                .collect::<Vec<_>>();
            assert_eq!(&prefix[3..6], [CORE, 1, 11]);
            assert_eq!(call.word().unwrap(), 0xffff_fffe);
            assert!(call.word().unwrap() <= 1000);
            assert_eq!(call.word().unwrap(), 0);
            assert_eq!(call.word().unwrap(), flags);
            assert_eq!(
                call.opaque(1024).unwrap().len(),
                usize::try_from(expected_len).unwrap()
            );
            call.done().unwrap();
            offset += 4 + length;
        }
        assert_eq!(offset, writes.len());
        let error = client.send_query(b"?", deadline()).unwrap_err();
        assert!(error.to_string().contains("unfinished response"));
    }

    #[test]
    fn correlation_authentication_truncation_and_nonzero_padding_fail() {
        let good = [
            0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(accepted(&good, 7, 4).unwrap(), [0; 4]);
        let mut wrong = good;
        wrong[3] = 8;
        let mut denied = good;
        denied[11] = 1;
        let mut auth = good;
        auth[15] = 1;
        let mut unavailable = good;
        unavailable[23] = 3;
        for bad in [&wrong[..], &denied, &auth, &unavailable, &good[..23]] {
            let error = accepted(bad, 7, 4).unwrap_err();
            assert!(error.to_string().contains("VXI-11"));
        }
        let mut padding = vec![0, 0, 0, 1, b'a', 0, 0, 1];
        let error = Xdr::new(&padding).opaque(1).unwrap_err();
        assert!(error.to_string().contains("padding"));
        padding.truncate(5);
        let error = Xdr::new(&padding).opaque(1).unwrap_err();
        assert!(error.to_string().contains("padding"));
    }

    #[test]
    fn deadline_and_xid_exhaustion_prevent_replay_and_finish_always_retires() {
        let (mut client, writes) = client(Vec::new());
        let error = client.send_query(b"?", Instant::now()).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(writes.lock().unwrap().is_empty());
        client.rpc.as_mut().unwrap().next_xid = u32::MAX;
        let error = client.send_query(b"?", deadline()).unwrap_err();
        assert!(error.to_string().contains("identifiers exhausted"));
        client.rpc.as_mut().unwrap().next_xid = 1;
        let error = client.finish(Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(client.rpc.is_none());
        assert!(client.link.is_none());
    }

    #[test]
    fn portmapper_uses_only_getport_for_the_core_tcp_program() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut marker = [0; 4];
            io::Read::read_exact(&mut stream, &mut marker).unwrap();
            assert_eq!(u32::from_be_bytes(marker), 0x8000_0038);
            let mut data = [0; 56];
            io::Read::read_exact(&mut stream, &mut data).unwrap();
            let expected = [1u32, 0, 2, 100_000, 2, 3, 0, 0, 0, 0, CORE, 1, 6, 0]
                .into_iter()
                .flat_map(u32::to_be_bytes)
                .collect::<Vec<_>>();
            assert_eq!(data.as_slice(), expected);
            io::Write::write_all(&mut stream, &reply(1, &5025u32.to_be_bytes())).unwrap();
        });
        assert_eq!(portmapper(address, deadline()).unwrap(), 5025);
        server.join().unwrap();
    }
}
