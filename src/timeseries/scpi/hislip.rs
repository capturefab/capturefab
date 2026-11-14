//! Query-only `HiSLIP` 1.0 transport; IVI-6.1 §§2.3,3,6.1–4,6.10–14.
//! Both server-selected modes are supported without changing instrument state.
use super::super::net;
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::{
    io::{self, Read as _},
    net::{Shutdown, SocketAddr, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const HEADER: usize = 16;
const MAX_DATA: usize = 64 * 1024;
const MAX_MESSAGE: usize = HEADER + MAX_DATA;
const MAX_ASYNC: usize = 4096;
/// How often the asynchronous reader wakes to check for stop.
const ASYNC_POLL: Duration = Duration::from_millis(100);
const FIRST_ID: u32 = 0xffff_ff00;
const UNKNOWN_ID: u32 = u32::MAX;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    #[serde(default = "device")]
    device: String,
}
fn device() -> String {
    "hislip0".into()
}

#[derive(Clone)]
struct Failure {
    kind: ErrorKind,
    detail: String,
}
impl Failure {
    fn error(&self) -> anyhow::Error {
        Categorized::error(self.kind, self.detail.clone())
    }
}
type SharedFailure = Arc<Mutex<Option<Failure>>>;

struct AsyncReader {
    control: TcpStream,
    failure: SharedFailure,
    reader: Option<JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
}
impl AsyncReader {
    fn start(mut stream: TcpStream, wake: TcpStream) -> Result<Self> {
        // This socket has no idle timeout. Shutdown on owner retirement wakes
        // even a partial header/payload read on Unix; Windows does not wake a
        // blocked receive on shutdown, so the reader also wakes every
        // `ASYNC_POLL` to check for stop. It runs no user callbacks.
        stream
            .set_read_timeout(Some(ASYNC_POLL))
            .map_err(|error| io_error(&error))?;
        let control = stream.try_clone().map_err(|error| io_error(&error))?;
        let failure = Arc::new(Mutex::new(None));
        let shared = Arc::clone(&failure);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let reader = thread::Builder::new()
            .name("capturefab-hislip-async".into())
            .spawn(move || {
                let result = async_loop(&mut stream, &stop);
                if let Err(error) = result {
                    let failed = error.downcast_ref::<Categorized>();
                    let kind = failed.map_or(ErrorKind::OperationFailed, |error| error.kind);
                    // Local shutdown wakes a partial socket read with EOF.
                    // Preserve protocol errors already decoded before stop, even
                    // if their publication races owner finalization.
                    if !(stop.load(Ordering::Acquire) && kind == ErrorKind::Disconnected)
                        && let Ok(mut saved) = shared.lock()
                    {
                        *saved = Some(Failure {
                            kind,
                            detail: error.to_string(),
                        });
                    }
                }
                // Wake a stalled synchronous read/write before returning.
                let _ = wake.shutdown(Shutdown::Both);
                let _ = stream.shutdown(Shutdown::Both);
            })
            .map_err(|error| io_error(&error))?;
        Ok(Self {
            control,
            failure,
            reader: Some(reader),
            stopped,
        })
    }
    fn check(&self) -> Result<()> {
        let saved = self
            .failure
            .lock()
            .map_err(|_| protocol("async state was poisoned"))?;
        if let Some(failure) = saved.as_ref() {
            return Err(failure.error());
        }
        Ok(())
    }
}
impl AsyncReader {
    fn stop(&mut self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let _ = self.control.shutdown(Shutdown::Both);
        // The only blocking operation in this reader is socket input. Shutdown
        // releases it even when the peer left an incomplete message. No callback
        // or retry can postpone owner finalization.
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| {
                Categorized::error(
                    ErrorKind::WorkerCrashed,
                    "HiSLIP asynchronous reader panicked",
                )
            })?;
        }
        self.check()
    }
}
impl Drop for AsyncReader {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(super) struct Client {
    config: Config,
    stream: Option<Box<dyn net::Transport>>,
    shutdown: Option<TcpStream>,
    asynchronous: Option<AsyncReader>,
    next_id: u32,
    request_id: Option<u32>,
    overlap: bool,
    next_response: u32,
    max_write: usize,
    pending: Vec<u8>,
    offset: usize,
    end: bool,
    reading: bool,
    rmt: bool,
    observed: Option<u64>,
}
impl Client {
    pub(super) fn new(config: &Config) -> Result<Self> {
        if config.address.port() == 0
            || config.address.ip().is_unspecified()
            || config.device.len() > 256
            || !config.device.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Usage("HiSLIP requires a specific numeric address with nonzero port and a 0–256 byte ASCII device name".into()).into());
        }
        Ok(Self {
            config: config.clone(),
            stream: None,
            shutdown: None,
            asynchronous: None,
            next_id: FIRST_ID,
            request_id: None,
            overlap: false,
            next_response: FIRST_ID,
            max_write: 0,
            pending: Vec::new(),
            offset: 0,
            end: false,
            reading: false,
            rmt: false,
            observed: None,
        })
    }
    pub(super) fn begin(&mut self, deadline: Instant) -> Result<()> {
        self.check()?;
        if self.stream.is_some() {
            return Ok(());
        }
        let socket = connect(self.config.address, deadline)?;
        self.shutdown = Some(socket.try_clone().map_err(|error| io_error(&error))?);
        self.stream = Some(net::plain(socket, deadline));
        // 1.0 with the same unregistered placeholder vendor used by PyVISA-py.
        let sub_address = self.config.device.clone();
        self.send(0, 0, 0x0100_7878, sub_address.as_bytes(), deadline)?;
        let initialized = self.message(deadline, 0)?;
        if initialized.kind != 1 || initialized.control > 1 || initialized.parameter >> 16 != 0x0100
        {
            return Err(protocol(
                "invalid InitializeResponse or unsupported negotiated protocol",
            ));
        }
        self.overlap = initialized.control == 1;
        let session = initialized.parameter & 0xffff;
        let socket = connect(self.config.address, deadline)?;
        let mut asynchronous = net::plain(
            socket.try_clone().map_err(|error| io_error(&error))?,
            deadline,
        );
        write_message(asynchronous.as_mut(), 17, 0, session, &[], deadline)?;
        let initialized = read_message(&mut asynchronous, 0)?;
        if initialized.kind != 18 || initialized.control != 0 {
            return Err(protocol("invalid AsyncInitializeResponse"));
        }
        write_message(
            asynchronous.as_mut(),
            15,
            0,
            0,
            &u64::try_from(MAX_MESSAGE)?.to_be_bytes(),
            deadline,
        )?;
        // Service requests can interleave with the max-message response.
        let mut notifications = 0;
        let maximum = loop {
            remaining(deadline)?;
            asynchronous.set_deadline(deadline);
            let message = read_message(&mut asynchronous, MAX_ASYNC)?;
            if message.kind == 20 {
                service_request(&message)?;
                notifications += 1;
                if notifications > 256 {
                    return Err(protocol(
                        "too many async notifications during initialization",
                    ));
                }
                continue;
            }
            break message;
        };
        if maximum.kind != 16 || maximum.control != 0 || maximum.parameter != 0 {
            return Err(protocol("invalid AsyncMaximumMessageSizeResponse"));
        }
        let maximum =
            u64::from_be_bytes(
                maximum.payload.as_slice().try_into().map_err(|_| {
                    protocol("maximum-message response requires eight payload bytes")
                })?,
            );
        let maximum = maximum
            .checked_sub(u64::try_from(HEADER)?)
            .filter(|size| *size != 0)
            .ok_or_else(|| protocol("peer maximum message leaves no data payload"))?;
        self.max_write = usize::try_from(maximum.min(u64::try_from(MAX_DATA)?))?;
        let wake = self
            .shutdown
            .as_ref()
            .ok_or_else(|| protocol("sync socket is closed"))?
            .try_clone()
            .map_err(|error| io_error(&error))?;
        self.asynchronous = Some(AsyncReader::start(socket, wake)?);
        self.next_id = FIRST_ID;
        self.next_response = FIRST_ID;
        self.request_id = None;
        self.rmt = false;
        remaining(deadline)?;
        self.check()
    }
    pub(super) fn check(&self) -> Result<()> {
        if let Some(asynchronous) = &self.asynchronous {
            asynchronous.check()?;
        }
        Ok(())
    }
    fn send(
        &mut self,
        kind: u8,
        control: u8,
        parameter: u32,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<()> {
        self.check()?;
        let result = write_message(
            self.stream
                .as_mut()
                .ok_or_else(|| protocol("sync socket is closed"))?
                .as_mut(),
            kind,
            control,
            parameter,
            payload,
            deadline,
        );
        self.check()?;
        result
    }
    fn message(&mut self, deadline: Instant, limit: usize) -> Result<Message> {
        self.check()?;
        remaining(deadline)?;
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| protocol("sync socket is closed"))?;
        stream.set_deadline(deadline);
        let result = read_message(stream, limit);
        self.check()?;
        let message = result?;
        self.observed = crate::host::monotonic_ns();
        Ok(message)
    }
    pub(super) fn send_query(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
        if bytes.is_empty()
            || self.reading
            || self.offset != self.pending.len()
            || self.max_write == 0
        {
            return Err(protocol(
                "query overlaps an unfinished response or uninitialized session",
            ));
        }
        let last = bytes.len().div_ceil(self.max_write) - 1;
        for (index, chunk) in bytes.chunks(self.max_write).enumerate() {
            self.send(
                if index == last { 7 } else { 6 },
                u8::from(self.rmt),
                self.next_id,
                chunk,
                deadline,
            )?;
            self.rmt = false;
            self.request_id = Some(self.next_id);
            self.next_id = self.next_id.wrapping_add(2);
        }
        self.pending.clear();
        self.offset = 0;
        self.end = false;
        self.reading = true;
        Ok(())
    }
    fn read_chunk(&mut self, deadline: Instant) -> Result<()> {
        let message = self.message(deadline, MAX_DATA)?;
        if !matches!(message.kind, 6 | 7) || message.control != 0 {
            return Err(protocol("unexpected synchronous message during query"));
        }
        if self.overlap {
            if message.parameter != self.next_response {
                return Err(protocol("overlap response message identifier differs"));
            }
            self.next_response = self.next_response.wrapping_add(2);
        } else if Some(message.parameter) != self.request_id
            && !(message.kind == 6 && message.parameter == UNKNOWN_ID)
        {
            // Retiring the source clears the complete transaction, including
            // bytes already passed to the SCPI parser/staged artifact writer.
            return Err(protocol("synchronized response message identifier differs"));
        }
        self.end = message.kind == 7;
        self.pending = message.payload;
        self.offset = 0;
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
            self.check()?;
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
                self.offset = end;
                return Ok(count);
            }
            if self.end {
                return Ok(0);
            }
            self.read_chunk(deadline)?;
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
            self.check()?;
            remaining(deadline)?;
            if self.offset != self.pending.len() {
                return Err(protocol("extra data after configured SCPI response"));
            }
            if self.end {
                break;
            }
            self.read_chunk(deadline)?;
        }
        self.reading = false;
        self.rmt = true;
        self.pending.clear();
        self.offset = 0;
        self.check()
    }
    pub(super) fn retire(&mut self) {
        if let Some(socket) = self.shutdown.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.stream = None;
        self.asynchronous = None;
        self.pending.clear();
        self.offset = 0;
        self.reading = false;
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        let result = self.asynchronous.as_mut().map_or(Ok(()), AsyncReader::stop);
        self.retire();
        result
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.retire();
    }
}

struct Message {
    kind: u8,
    control: u8,
    parameter: u32,
    payload: Vec<u8>,
}
fn header(input: [u8; HEADER], limit: usize) -> Result<(u8, u8, u32, usize)> {
    let [h, s, kind, control, a, b, c, d, e, f, g, i, j, k, l, m] = input;
    if [h, s] != *b"HS" {
        return Err(protocol("message prologue differs"));
    }
    let parameter = u32::from_be_bytes([a, b, c, d]);
    let length = u64::from_be_bytes([e, f, g, i, j, k, l, m]);
    if length > u64::try_from(limit)? {
        return Err(protocol(
            "message payload exceeds software/negotiated bound",
        ));
    }
    Ok((kind, control, parameter, usize::try_from(length)?))
}
fn read_message(stream: &mut impl io::Read, limit: usize) -> Result<Message> {
    let mut bytes = [0; HEADER];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| io_error(&error))?;
    let (kind, control, parameter, length) = header(bytes, limit)?;
    if matches!(kind, 2 | 3 | 13 | 14) {
        return Err(protocol(&format!(
            "peer error/interruption message {kind}, code {control}, parameter {parameter:#x}"
        )));
    }
    let mut payload = vec![0; length];
    stream
        .read_exact(&mut payload)
        .map_err(|error| io_error(&error))?;
    Ok(Message {
        kind,
        control,
        parameter,
        payload,
    })
}
fn write_message(
    stream: &mut dyn net::Transport,
    kind: u8,
    control: u8,
    parameter: u32,
    payload: &[u8],
    deadline: Instant,
) -> Result<()> {
    remaining(deadline)?;
    stream.set_deadline(deadline);
    let mut message = Vec::with_capacity(HEADER + payload.len());
    message.extend_from_slice(b"HS");
    message.extend_from_slice(&[kind, control]);
    message.extend_from_slice(&parameter.to_be_bytes());
    message.extend_from_slice(&u64::try_from(payload.len())?.to_be_bytes());
    message.extend_from_slice(payload);
    stream.write_all(&message).map_err(|error| io_error(&error))
}
fn service_request(message: &Message) -> Result<()> {
    if message.kind != 20 || message.parameter != 0 || !message.payload.is_empty() {
        return Err(protocol("malformed AsyncServiceRequest"));
    }
    Ok(())
}
fn async_loop(stream: &mut TcpStream, stopped: &AtomicBool) -> Result<()> {
    while !stopped.load(Ordering::Acquire) {
        // The only expected idle/query event is a zero-payload SRQ. Inspect
        // its complete header before waiting for any alleged payload, so a
        // terminal or malformed header can immediately wake a stalled query.
        let mut bytes = [0; HEADER];
        let mut filled = 0;
        while filled < HEADER {
            if stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            match stream.read(bytes.get_mut(filled..).unwrap_or_default()) {
                Ok(0) => {
                    return Err(io_error(&io::Error::from(io::ErrorKind::UnexpectedEof)));
                }
                Ok(count) => filled += count,
                // A wakeup to check for stop, not an idle timeout.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(io_error(&error)),
            }
        }
        let (kind, control, parameter, length) = header(bytes, MAX_ASYNC)?;
        if kind != 20 || parameter != 0 || length != 0 {
            return Err(protocol(&format!(
                "unexpected asynchronous header: message {kind}, code {control}, parameter {parameter:#x}, length {length}"
            )));
        }
        // SRQ is unacknowledged. Reading STB would clear RQS; a query-only
        // connection deliberately does not rearm or expose an SRQ event stream.
    }
    Ok(())
}

fn connect(address: SocketAddr, deadline: Instant) -> Result<TcpStream> {
    let socket = TcpStream::connect_timeout(&address, remaining(deadline)?)
        .map_err(|error| io_error(&error))?;
    socket.set_nodelay(true).map_err(|error| io_error(&error))?;
    Ok(socket)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "HiSLIP query deadline expired",
        ));
    }
    Ok(remaining)
}
fn protocol(detail: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("HiSLIP: {detail}"))
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("HiSLIP TCP I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Default)]
    struct Observed {
        bytes: Vec<u8>,
        deadlines: Vec<Instant>,
    }
    struct Memory {
        input: io::Cursor<Vec<u8>>,
        observed: Arc<Mutex<Observed>>,
    }
    impl io::Read for Memory {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.input.read(output)
        }
    }
    impl io::Write for Memory {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.observed
                .lock()
                .map_err(|_| io::Error::other("test observation state was poisoned"))?
                .bytes
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl net::Transport for Memory {
        fn set_deadline(&mut self, deadline: Instant) {
            self.observed.lock().unwrap().deadlines.push(deadline);
        }
    }
    fn memory(input: Vec<u8>) -> (Box<dyn net::Transport>, Arc<Mutex<Observed>>) {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let transport = Box::new(Memory {
            input: io::Cursor::new(input),
            observed: Arc::clone(&observed),
        });
        (transport, observed)
    }
    fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"HS".to_vec();
        bytes.extend_from_slice(&[kind, 0]);
        bytes.extend_from_slice(&id.to_be_bytes());
        bytes.extend_from_slice(&u64::try_from(payload.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
    fn memory_client(bytes: Vec<u8>, overlap: bool) -> Client {
        let config = serde_json::from_value(json!({"address":"127.0.0.1:4880"})).unwrap();
        let mut client = Client::new(&config).unwrap();
        client.stream = Some(memory(bytes).0);
        client.request_id = Some(0xffff_ff02);
        client.reading = true;
        client.overlap = overlap;
        client
    }
    #[test]
    fn config_has_bounded_ascii_subaddresses_and_strict_fields() {
        for device in [String::new(), "hislip0".to_owned(), "x".repeat(256)] {
            let config =
                serde_json::from_value(json!({"address":"127.0.0.1:4880","device":device}))
                    .unwrap();
            let client = Client::new(&config).unwrap();
            assert!(client.stream.is_none());
        }
        for value in [
            json!({"address":"127.0.0.1:0"}),
            json!({"address":"0.0.0.0:4880"}),
            json!({"address":"127.0.0.1:4880","device":"x".repeat(257)}),
            json!({"address":"127.0.0.1:4880","device":"hislip\n"}),
        ] {
            let config = serde_json::from_value(value).unwrap();
            let Err(error) = Client::new(&config) else {
                panic!("invalid config accepted")
            };
            assert!(error.to_string().contains("HiSLIP requires"));
        }
        let error = serde_json::from_value::<Config>(json!({"address":"127.0.0.1:4880","tls":{}}))
            .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
    #[test]
    fn literal_header_bounds_are_checked_before_payload_allocation() {
        let literal = [
            0x48, 0x53, 7, 0, 0xff, 0xff, 0xff, 2, 0, 0, 0, 0, 0, 0, 0, 3,
        ];
        assert_eq!(header(literal, 3).unwrap(), (7, 0, 0xffff_ff02, 3));
        let error = header(literal, 2).unwrap_err();
        assert!(error.to_string().contains("bound"));
        let mut wrong = literal;
        wrong[0] = b'X';
        assert!(
            header(wrong, 3)
                .unwrap_err()
                .to_string()
                .contains("prologue")
        );
        let mut overflow = literal;
        overflow.get_mut(8..).unwrap().fill(0xff);
        assert!(
            header(overflow, MAX_DATA)
                .unwrap_err()
                .to_string()
                .contains("bound")
        );
    }
    #[test]
    fn fragment_ids_wrap_rmt_is_once_and_every_write_keeps_one_deadline() {
        let mut client = memory_client(Vec::new(), false);
        client.reading = false;
        client.next_id = 0xffff_fffe;
        client.max_write = 2;
        client.rmt = true;
        let (stream, observed) = memory(frame(7, 2, b"12"));
        client.stream = Some(stream);
        let deadline = Instant::now() + Duration::from_secs(1);
        client.send_query(b"FETC?\n", deadline).unwrap();
        assert_eq!(client.next_id, 4);
        assert_eq!(client.request_id, Some(2));
        assert!(!client.rmt);
        let mut reply = [0; 2];
        assert_eq!(client.receive(&mut reply, deadline).unwrap(), 2);
        assert_eq!(reply, *b"12");
        client.complete(deadline).unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(observed.deadlines, [deadline; 4]);
        let mut encoded = io::Cursor::new(&observed.bytes);
        for (kind, control, id, payload) in [
            (6, 1, 0xffff_fffe, b"FE"),
            (6, 0, 0, b"TC"),
            (7, 0, 2, b"?\n"),
        ] {
            let message = read_message(&mut encoded, 2).unwrap();
            assert_eq!(
                (message.kind, message.control, message.parameter),
                (kind, control, id)
            );
            assert_eq!(message.payload, payload);
        }
        assert_eq!(
            usize::try_from(encoded.position()).unwrap(),
            observed.bytes.len()
        );
    }
    #[test]
    fn synchronous_wildcard_applies_only_to_data_and_overlap_ids_are_independent() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let bytes = [frame(6, UNKNOWN_ID, b"12"), frame(7, 0xffff_ff02, b"3")].concat();
        let mut client = memory_client(bytes, false);
        let mut output = [0; 8];
        assert_eq!(client.receive(&mut output, deadline).unwrap(), 2);
        assert_eq!(client.receive(&mut output, deadline).unwrap(), 1);
        client.complete(deadline).unwrap();
        assert!(client.rmt);
        for id in [UNKNOWN_ID, 0xffff_ff04] {
            let mut client = memory_client(frame(7, id, b"123"), false);
            let error = client.receive(&mut output, deadline).unwrap_err();
            assert!(error.to_string().contains("identifier"));
        }
        let bytes = [frame(6, 0xffff_fffe, b"1"), frame(7, 0, b"2")].concat();
        let mut client = memory_client(bytes, true);
        client.next_response = 0xffff_fffe;
        assert_eq!(client.receive(&mut output, deadline).unwrap(), 1);
        assert_eq!(client.receive(&mut output, deadline).unwrap(), 1);
        client.complete(deadline).unwrap();
        assert_eq!(client.next_response, 2);
    }
    #[test]
    fn finish_waits_for_reader_failure_publication_and_preserves_protocol_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let failure = Arc::new(Mutex::new(None));
        let saved = Arc::clone(&failure);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        // Model a decoded terminal header whose error publication was delayed
        // until owner finalization. No sleep or scheduling-order assumption.
        let reader = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                thread::yield_now();
            }
            *saved.lock().unwrap() = Some(Failure {
                kind: ErrorKind::OperationFailed,
                detail: "terminal header already decoded".into(),
            });
        });
        let mut client = memory_client(Vec::new(), false);
        client.asynchronous = Some(AsyncReader {
            control,
            failure,
            reader: Some(reader),
            stopped,
        });
        let error = client.finish().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("terminal header already decoded")
        );
        assert!(client.stream.is_none());
        assert!(client.asynchronous.is_none());
        drop(peer);
    }
}
