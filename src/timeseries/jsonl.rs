//! Receive-only JSON Lines sensor streams over TCP and native serial ports.
use super::{Reading, Source, serial::Parity};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::{
    io::{self, Read as _, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const MAX_LINE: usize = 1 << 20;
const MAX_BATCH: usize = 256;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TcpConfig {
    pub address: SocketAddr,
    #[serde(default)]
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SerialConfig {
    pub device: String,
    #[serde(
        default = "default_baud",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub baud: u32,
    #[serde(default = "default_parity")]
    pub parity: Parity,
    #[serde(
        default = "default_stop_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub stop_bits: u8,
    #[serde(
        default = "default_data_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub data_bits: u8,
    #[serde(default)]
    pub signal: Option<String>,
}
const fn default_baud() -> u32 {
    115_200
}
const fn default_parity() -> Parity {
    Parity::None
}
const fn default_stop_bits() -> u8 {
    1
}
const fn default_data_bits() -> u8 {
    8
}

pub(crate) fn tcp(config: &TcpConfig) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 {
        return Err(Usage("TCP JSONL address port must be nonzero".into()).into());
    }
    validate_signal(config.signal.as_deref())?;
    Ok(Box::new(Stream::new(
        Connection::Tcp {
            address: config.address,
            stream: None,
        },
        config.signal.clone(),
    )))
}

pub(crate) fn serial(config: &SerialConfig) -> Result<Box<dyn Source>> {
    if config.device.is_empty() {
        return Err(Usage("serial JSONL device must be nonempty".into()).into());
    }
    super::serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    validate_signal(config.signal.as_deref())?;
    #[cfg(any(unix, windows))]
    {
        let file = super::serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            config.data_bits,
        )?;
        Ok(Box::new(Stream::new(
            Connection::Serial(file),
            config.signal.clone(),
        )))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = config.parity;
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "serial JSONL requires a Unix terminal or Windows COM port",
        ))
    }
}

fn validate_signal(signal: Option<&str>) -> Result<()> {
    if let Some(signal) = signal {
        super::validate_name(signal, "JSONL signal")?;
    }
    Ok(())
}

enum Connection {
    Tcp {
        address: SocketAddr,
        stream: Option<TcpStream>,
    },
    #[cfg(any(unix, windows))]
    Serial(super::serial::Port),
}
impl Connection {
    /// None is an ordinary idle timeout. Every I/O retry uses the same deadline.
    fn receive(&mut self, bytes: &mut [u8], deadline: Instant) -> Result<Option<usize>> {
        match self {
            Self::Tcp { address, stream } => {
                if stream.is_none() {
                    let remaining = remaining(deadline)?;
                    let socket = TcpStream::connect_timeout(address, remaining)
                        .map_err(|error| io_error(&error))?;
                    socket.set_nodelay(true).map_err(|error| io_error(&error))?;
                    *stream = Some(socket);
                }
                let socket = stream.as_mut().expect("connected TCP sensor");
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    socket
                        .set_read_timeout(Some(left))
                        .map_err(|error| io_error(&error))?;
                    match socket.read(bytes) {
                        Ok(count) => return Ok(Some(count)),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) =>
                        {
                            return Ok(None);
                        }
                        Err(error) => return Err(io_error(&error)),
                    }
                }
            }
            #[cfg(any(unix, windows))]
            Self::Serial(file) => file
                .receive(bytes, deadline)
                .map_err(|error| io_error(&error)),
        }
    }
}

struct Stream {
    connection: Connection,
    signal: Option<String>,
    pending: Vec<u8>,
    scan: usize,
    eof: bool,
    seen: bool,
    observed: Option<u64>,
}
impl Stream {
    fn new(connection: Connection, signal: Option<String>) -> Self {
        Self {
            connection,
            signal,
            pending: Vec::new(),
            scan: 0,
            eof: false,
            seen: false,
            observed: None,
        }
    }

    fn reading(&self, line: &[u8]) -> Result<Reading> {
        let result = self.signal.as_ref().map_or_else(
            || serde_json::from_slice(line),
            |signal| {
                serde_json::from_slice(line).map(|value| Reading {
                    signal: signal.clone(),
                    value,
                    source_time: None,
                })
            },
        );
        // Do not echo sensor input: a malformed payload can contain secrets.
        result.map_err(|error| {
            Categorized::error(
                ErrorKind::OperationFailed,
                format!(
                    "sensor JSON line is invalid: {:?} at column {}",
                    error.classify(),
                    error.column()
                ),
            )
        })
    }
}

impl Source for Stream {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("JSONL timeout is too large".into()))?;
        let mut readings = Vec::new();
        let mut batch_bytes = 2; // Serialized array brackets.
        loop {
            while let Some(relative) = self.pending[self.scan..]
                .iter()
                .position(|&byte| byte == b'\n')
            {
                let newline = self.scan + relative;
                if newline > MAX_LINE {
                    return Err(too_long());
                }
                let separator = usize::from(!readings.is_empty());
                if !readings.is_empty() && batch_bytes + separator + newline > MAX_LINE {
                    return Ok(readings);
                }
                let reading = match self.reading(&self.pending[..newline]) {
                    Ok(reading) => reading,
                    Err(_) if !readings.is_empty() => return Ok(readings),
                    Err(error) => return Err(error),
                };
                let mut encoded = SizeLimit::default();
                if serde_json::to_writer(&mut encoded, &reading).is_err()
                    || batch_bytes + separator + encoded.bytes > MAX_LINE
                {
                    if !readings.is_empty() {
                        return Ok(readings);
                    }
                    return Err(Categorized::error(
                        ErrorKind::OperationFailed,
                        "sensor JSON reading exceeds 1 MiB serialized batch limit",
                    ));
                }
                batch_bytes += separator + encoded.bytes;
                self.pending.drain(..=newline);
                self.scan = 0;
                self.seen = true;
                readings.push(reading);
                if readings.len() == MAX_BATCH {
                    return Ok(readings);
                }
            }
            self.scan = self.pending.len();
            if self.pending.len() > MAX_LINE {
                return Err(too_long());
            }
            if !readings.is_empty() {
                return Ok(readings);
            }
            if self.eof {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    if self.pending.is_empty() {
                        "sensor JSONL connection closed"
                    } else {
                        "sensor JSONL connection ended with an incomplete line"
                    },
                ));
            }
            if Instant::now() >= deadline {
                return Ok(readings);
            }
            let mut bytes = [0; 8192];
            match self.connection.receive(&mut bytes, deadline)? {
                Some(0) => self.eof = true,
                Some(count) => {
                    self.observed = crate::host::monotonic_ns();
                    self.pending.extend_from_slice(&bytes[..count]);
                }
                None => return Ok(readings),
            }
        }
    }

    fn ready(&self) -> bool {
        self.seen
    }

    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }

    fn buffered_timestamps(&self) -> bool {
        true
    }

    fn drain(&mut self) -> Result<Option<Reading>> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let Some(newline) = self.pending.iter().position(|&byte| byte == b'\n') else {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "sensor JSONL acquisition ended with an incomplete line",
            ));
        };
        if newline > MAX_LINE {
            return Err(too_long());
        }
        let reading = self.reading(&self.pending[..newline])?;
        let mut encoded = SizeLimit::default();
        if serde_json::to_writer(&mut encoded, &reading).is_err() {
            return Err(Categorized::error(
                ErrorKind::OperationFailed,
                "sensor JSON reading exceeds 1 MiB serialized batch limit",
            ));
        }
        self.pending.drain(..=newline);
        self.scan = 0;
        self.seen = true;
        Ok(Some(reading))
    }
}

/// A counting writer prevents number normalization from allocating an oversized
/// serialized duplicate merely to discover it exceeds the batch budget.
#[derive(Default)]
struct SizeLimit {
    bytes: usize,
}
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_LINE - 2 {
            return Err(io::Error::other("sensor reading exceeds batch budget"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "sensor JSONL connection deadline expired",
        ));
    }
    Ok(left)
}
fn too_long() -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, "sensor JSON line exceeds 1 MiB")
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::NotConnected
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof => ErrorKind::Disconnected,
        _ => ErrorKind::Unavailable,
    };
    Categorized::error(kind, format!("sensor JSONL I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{net::TcpListener, thread};

    fn buffered(signal: Option<&str>, bytes: &[u8]) -> Stream {
        let mut stream = Stream::new(
            Connection::Tcp {
                address: "127.0.0.1:1".parse().unwrap(),
                stream: None,
            },
            signal.map(str::to_owned),
        );
        stream.pending.extend_from_slice(bytes);
        stream
    }

    #[test]
    fn reading_and_scalar_modes_preserve_precision_and_source_timestamps() {
        let literal = "[18446744073709551617,0.123456789012345678901234567890123456789]";
        let mut stream = buffered(None, format!("{{\"signal\":\"counter\",\"value\":{literal},\"source_time\":\"2026-11-10T14:00:00.123Z\"}}\r\n").as_bytes());
        assert!(!stream.ready());
        let batch = stream.read(Duration::from_secs(1)).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].value.to_string(), literal);
        assert_eq!(
            batch[0].source_time.as_deref(),
            Some("2026-11-10T14:00:00.123Z")
        );
        assert!(stream.ready());
        stream.finish(Duration::ZERO).unwrap();
        let mut scalar = buffered(Some("pressure"), b"12.345678901234567890123456789\n");
        let batch = scalar.read(Duration::ZERO).unwrap();
        assert_eq!(batch[0].signal, "pressure");
        assert_eq!(batch[0].value.to_string(), "12.345678901234567890123456789");
        assert!(batch[0].source_time.is_none());
    }

    #[test]
    fn malformed_lines_do_not_echo_payloads_and_unknown_fields_are_rejected() {
        for bytes in [
            b"{\"signal\":\"x\",\"value\":credential-secret}\n".as_slice(),
            b"{\"signal\":\"x\",\"value\":1,\"extra\":\"credential-secret\"}\n",
            b"\n",
            b"\xff\n",
        ] {
            let mut stream = buffered(None, bytes);
            let error = stream.read(Duration::ZERO).err().unwrap();
            assert!(!error.to_string().contains("credential-secret"));
        }
    }

    #[test]
    fn batches_bound_count_and_serialized_bytes_without_losing_the_next_line() {
        let mut many = buffered(Some("x"), "1\n".repeat(MAX_BATCH + 1).as_bytes());
        assert_eq!(many.read(Duration::ZERO).unwrap().len(), MAX_BATCH);
        assert_eq!(many.read(Duration::ZERO).unwrap().len(), 1);
        let payload = "x".repeat(600_000);
        let first = serde_json::to_string(&json!({"signal":"a","value":payload})).unwrap();
        let second = serde_json::to_string(&json!({"signal":"b","value":payload})).unwrap();
        let mut large = buffered(None, format!("{first}\n{second}\n").as_bytes());
        assert_eq!(large.read(Duration::ZERO).unwrap()[0].signal, "a");
        assert_eq!(large.read(Duration::ZERO).unwrap()[0].signal, "b");
        let mut too_large = buffered(Some("x"), &vec![b'1'; MAX_LINE + 1]);
        too_large.read(Duration::ZERO).unwrap_err();
    }

    #[test]
    fn cutoff_drains_complete_buffered_lines_before_reporting_a_partial_tail() {
        for partial in [false, true] {
            let mut bytes = "1\n".repeat(MAX_BATCH + 7);
            if partial {
                bytes.push_str("12.");
            }
            let mut stream = buffered(Some("x"), bytes.as_bytes());
            let observed = crate::host::monotonic_ns();
            stream.observed = observed;
            assert_eq!(stream.read(Duration::ZERO).unwrap().len(), MAX_BATCH);
            stream.finish(Duration::ZERO).unwrap();
            for _ in 0..7 {
                assert_eq!(stream.drain().unwrap().unwrap().value, 1);
                assert_eq!(stream.observed_ns(), observed);
            }
            if partial {
                assert_eq!(
                    crate::types::classify(&stream.drain().unwrap_err()).kind,
                    ErrorKind::Incomplete
                );
            } else {
                assert!(stream.drain().unwrap().is_none());
            }
        }
        let mut malformed = buffered(Some("x"), b"1\ninvalid\n");
        assert_eq!(malformed.read(Duration::ZERO).unwrap()[0].value, 1);
        malformed.read(Duration::ZERO).unwrap_err();
    }

    #[test]
    fn partial_tcp_lines_survive_idle_timeouts_and_cutoff_is_explicit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (send, resume) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .write_all(b"{\"signal\":\"count\",\"value\":")
                .unwrap();
            resume.recv_timeout(Duration::from_secs(3)).unwrap();
            socket.write_all(b"42}\n").unwrap();
            resume.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let mut stream = tcp(&TcpConfig {
            address,
            signal: None,
        })
        .unwrap();
        assert!(stream.read(Duration::from_millis(30)).unwrap().is_empty());
        assert!(!stream.ready());
        assert_eq!(
            crate::types::classify(&stream.drain().unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        send.send(()).unwrap();
        let batch = stream.read(Duration::from_secs(1)).unwrap();
        assert_eq!(batch[0].value, 42);
        assert!(stream.ready());
        stream.finish(Duration::ZERO).unwrap();
        send.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(
            crate::types::classify(&stream.read(Duration::from_secs(1)).unwrap_err()).kind,
            ErrorKind::Disconnected
        );
    }

    #[test]
    fn quiet_or_trickling_source_does_not_extend_one_read_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (send, stop) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            while stop.try_recv().is_err() {
                if socket.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let mut stream = tcp(&TcpConfig {
            address,
            signal: None,
        })
        .unwrap();
        let start = Instant::now();
        assert!(stream.read(Duration::from_millis(40)).unwrap().is_empty());
        assert!(start.elapsed() < Duration::from_millis(300));
        assert!(!stream.ready());
        send.send(()).unwrap();
        server.join().unwrap();
    }
}
