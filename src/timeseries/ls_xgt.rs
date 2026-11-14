//! Read-only LS ELECTRIC XGT/XGB `FEnet` dedicated TCP (0054/0055).
//! XGT `FEnet` v2.30, sections 8.1–8.2, and XBL-EMTA v2.1, section 5.2.
//! <https://sol.ls-electric.com/uploads/document/17743368081670/User_s%20Manual_XBL-EMTA_ENG_V2.1_20260324.pdf>
use super::{Reading, Source, net, slmp};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const COMPANY: &[u8; 10] = b"LSIS-XGT\0\0";
const MAX_READ: u16 = 1400;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    #[serde(default)]
    cpu: Cpu,
    #[serde(
        default = "max_read_bytes",
        deserialize_with = "crate::config_number::unsigned"
    )]
    max_read_bytes: u16,
    #[serde(default)]
    require_header_checksum: bool,
    signals: Vec<Signal>,
}
const fn max_read_bytes() -> u16 {
    256
}
const fn one() -> u16 {
    1
}
const fn scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Cpu {
    #[default]
    Xgk,
    Xgi,
    Xgr,
    XgbMk,
    XgbIec,
}
impl Cpu {
    const fn code(self) -> u8 {
        match self {
            Self::Xgk => 0xa0,
            Self::Xgi => 0xa4,
            Self::Xgr => 0xa8,
            Self::XgbMk => 0xb0,
            Self::XgbIec => 0xb4,
        }
    }
    const fn iec(self) -> bool {
        matches!(self, Self::Xgi | Self::Xgr | Self::XgbIec)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    variable: String,
    #[serde(default)]
    data_type: DataType,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    elements: u16,
    #[serde(default = "scale")]
    scale: f64,
    #[serde(default)]
    offset: f64,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DataType {
    Bool,
    U8,
    I8,
    #[default]
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
}
impl DataType {
    const fn bytes(self) -> u16 {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }
    const fn shared(self) -> Option<slmp::DataType> {
        Some(match self {
            Self::Bool | Self::U8 | Self::I8 => return None,
            Self::U16 => slmp::DataType::U16,
            Self::I16 => slmp::DataType::I16,
            Self::U32 => slmp::DataType::U32,
            Self::I32 => slmp::DataType::I32,
            Self::U64 => slmp::DataType::U64,
            Self::I64 => slmp::DataType::I64,
            Self::F32 => slmp::DataType::F32,
            Self::F64 => slmp::DataType::F64,
        })
    }
    fn valid_scale(self, scale: f64, offset: f64) -> bool {
        self.shared().map_or_else(
            || {
                scale.is_finite()
                    && offset.is_finite()
                    && (!matches!(self, Self::Bool) || (scale == 1.0 && offset == 0.0))
            },
            |shared| shared.valid_scale(scale, offset),
        )
    }
    fn decode(self, signal: &Signal, data: &[u8]) -> Result<Value> {
        if data.len() != usize::from(signal.elements) * usize::from(self.bytes()) {
            return Err(protocol("read width differs from configured signal"));
        }
        if let Some(shared) = self.shared() {
            return shared.decode(signal.elements, signal.scale, signal.offset, data, protocol);
        }
        let values = data
            .iter()
            .map(|byte| {
                let value = match self {
                    // The manual defines only the low bit as effective in BIT data.
                    Self::Bool => return Ok(Value::Bool(byte & 1 != 0)),
                    Self::I8 => json!(i8::from_ne_bytes([*byte])),
                    _ => json!(*byte),
                };
                if signal.scale == 1.0 && signal.offset == 0.0 {
                    return Ok(value);
                }
                let scaled = value
                    .as_f64()
                    .ok_or_else(|| protocol("invalid byte value"))?
                    * signal.scale
                    + signal.offset;
                if !scaled.is_finite() {
                    return Err(protocol("non-finite scaled value"));
                }
                Ok(json!(scaled))
            })
            .collect::<Result<Vec<_>>>()?;
        if signal.elements == 1 {
            values
                .into_iter()
                .next()
                .ok_or_else(|| protocol("missing scalar"))
        } else {
            Ok(Value::Array(values))
        }
    }
}

/// Addresses are transmitted literally. No ladder-address to bit-index conversion.
#[derive(Debug)]
struct Variable<'a> {
    kind: u8,
    tail: &'a str,
    prefix: &'a str,
}
fn variable(text: &str, cpu: Cpu) -> Result<Variable<'_>> {
    let bytes = text.as_bytes();
    let (area, kind) = match bytes {
        [b'%', area, kind, ..] if (4..=16).contains(&bytes.len()) => {
            (area.to_ascii_uppercase(), kind.to_ascii_uppercase())
        }
        _ => {
            return Err(Usage(
                "LS XGT variable requires a 4–16 byte direct address beginning with %".into(),
            )
            .into());
        }
    };
    let supported_area = b"PMLKFTCDNRS".contains(&area) || (cpu.iec() && b"IQURW".contains(&area));
    if !matches!(kind, b'X' | b'B' | b'W' | b'D' | b'L')
        || !supported_area
        || (area == b'D' && !matches!(kind, b'B' | b'W'))
    {
        return Err(Usage(
            "LS XGT variable area/type is outside the configured CPU profile (D supports B/W)"
                .into(),
        )
        .into());
    }
    let tail = text
        .get(3..)
        .ok_or_else(|| Usage("invalid LS XGT address".into()))?;
    let parts = tail.split('.').collect::<Vec<_>>();
    if parts
        .iter()
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        || if matches!(area, b'I' | b'Q') {
            parts.len() != 3
        } else if area == b'U' {
            parts.len() != if kind == b'X' { 3 } else { 2 }
        } else {
            parts.len() != 1
        }
    {
        return Err(Usage("LS XGT direct address needs decimal components; I/Q use base.slot.point and U uses module.channel[.bit]".into()).into());
    }
    Ok(Variable {
        kind,
        tail,
        prefix: text
            .get(..3)
            .ok_or_else(|| Usage("invalid LS XGT prefix".into()))?,
    })
}

fn validate(config: &Config) -> Result<()> {
    if config.address.port() == 0
        || !(1..=MAX_READ).contains(&config.max_read_bytes)
        || !(1..=256).contains(&config.signals.len())
    {
        return Err(Usage(
            "LS XGT requires a nonzero TCP port, max_read_bytes 1–1400 and 1–256 signals".into(),
        )
        .into());
    }
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "LS XGT signal")?;
        let address = variable(&signal.variable, config.cpu)?;
        let width = signal.data_type.bytes();
        let valid_type = match address.kind {
            b'X' => matches!(signal.data_type, DataType::Bool) && signal.elements == 1,
            b'B' => !matches!(signal.data_type, DataType::Bool),
            b'W' => width == 2 && signal.elements == 1,
            b'D' => width == 4 && signal.elements == 1,
            b'L' => width == 8 && signal.elements == 1,
            _ => false,
        };
        if !names.insert(&signal.name)
            || !(1..=4096).contains(&signal.elements)
            || !valid_type
            || !signal.data_type.valid_scale(signal.scale, signal.offset)
            || width > config.max_read_bytes
        {
            return Err(Usage("LS XGT needs unique signals, 1–4096 elements, matching native widths, scalar bool X variables, byte variables for arrays, and compatible scaling".into()).into());
        }
        if address.kind == b'B' && (signal.elements > 1 || width > 1) {
            if address.tail.contains('.') {
                return Err(Usage("LS XGT continuous reads require a flat decimal byte address; dotted I/O variables remain scalar".into()).into());
            }
            let start = address
                .tail
                .parse::<u64>()
                .map_err(|_| Usage("LS XGT byte address exceeds u64".into()))?;
            let last = start
                .checked_add(u64::from(signal.elements) * u64::from(width) - 1)
                .ok_or_else(|| Usage("LS XGT byte range overflows".into()))?;
            if format!("{}{last}", address.prefix).len() > 16 {
                return Err(Usage(
                    "LS XGT byte range exceeds the 16-byte direct-variable field".into(),
                )
                .into());
            }
        }
    }
    Ok(())
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    validate(config)?;
    Ok(Box::new(Client {
        config: config.clone(),
        connection: None,
    }))
}
struct Client {
    config: Config,
    connection: Option<Connection>,
}
struct Connection {
    stream: Box<dyn net::Transport>,
    next_invoke: u32,
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("LS XGT timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.connection = None;
        }
        result
    }
}
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = slmp::SizeLimit { bytes: 2 };
        for signal in &self.config.signals {
            let value = read_signal(&mut self.connection, &self.config, signal, deadline)?;
            let reading = Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            };
            if !readings.is_empty() {
                size.write_all(b",")
                    .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            }
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            readings.push(reading);
        }
        remaining(deadline)?;
        Ok(readings)
    }
}
fn read_signal(
    connection: &mut Option<Connection>,
    config: &Config,
    signal: &Signal,
    deadline: Instant,
) -> Result<Value> {
    let address = variable(&signal.variable, config.cpu)?;
    let width = signal.data_type.bytes();
    let continuous = address.kind == b'B' && (signal.elements > 1 || width > 1);
    let wire_type = if continuous {
        0x14
    } else {
        match address.kind {
            b'X' => 0,
            b'B' => 1,
            b'W' => 2,
            b'D' => 3,
            b'L' => 4,
            _ => return Err(protocol("unsupported variable type")),
        }
    };
    let mut bytes = Vec::with_capacity(usize::from(signal.elements) * usize::from(width));
    let mut done = 0;
    while done < signal.elements {
        remaining(deadline)?;
        let elements = if continuous {
            (signal.elements - done).min(config.max_read_bytes / width)
        } else {
            1
        };
        let count = elements * width;
        let label = if done == 0 {
            signal.variable.clone()
        } else {
            let start = address.tail.parse::<u64>()?;
            format!(
                "{}{}",
                address.prefix,
                start + u64::from(done) * u64::from(width)
            )
        };
        let mut body = Vec::with_capacity(28);
        body.extend_from_slice(&[0x54, 0, wire_type, 0, 0, 0, 1, 0]);
        body.extend_from_slice(&u16::try_from(label.len())?.to_le_bytes());
        body.extend_from_slice(label.as_bytes());
        if continuous {
            body.extend_from_slice(&count.to_le_bytes());
        }
        bytes.extend(exchange(
            connection, config, &body, wire_type, count, deadline,
        )?);
        done += elements;
    }
    signal.data_type.decode(signal, &bytes)
}

fn exchange(
    connection: &mut Option<Connection>,
    config: &Config,
    body: &[u8],
    wire_type: u8,
    count: u16,
    deadline: Instant,
) -> Result<Vec<u8>> {
    if connection
        .as_ref()
        .is_some_and(|connection| connection.next_invoke > u32::from(u16::MAX))
    {
        // An Invoke ID is never reused on a TCP connection, even after 65536 requests.
        *connection = None;
    }
    if connection.is_none() {
        let socket = TcpStream::connect_timeout(&config.address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        socket.set_nodelay(true).map_err(|error| io_error(&error))?;
        *connection = Some(Connection {
            stream: net::plain(socket, deadline),
            next_invoke: 0,
        });
    }
    let connection = connection
        .as_mut()
        .ok_or_else(|| protocol("connection missing"))?;
    connection.stream.set_deadline(deadline);
    let invoke = u16::try_from(connection.next_invoke)?;
    connection.next_invoke += 1;
    let mut header = [0_u8; 20];
    header[..10].copy_from_slice(COMPANY);
    header[12] = config.cpu.code();
    header[13] = 0x33;
    header[14..16].copy_from_slice(&invoke.to_le_bytes());
    header[16..18].copy_from_slice(&u16::try_from(body.len())?.to_le_bytes());
    header[19] = checksum(&header[..19]);
    connection
        .stream
        .write_all(&header)
        .and_then(|()| connection.stream.write_all(body))
        .map_err(|error| io_error(&error))?;
    connection
        .stream
        .read_exact(&mut header)
        .map_err(|error| io_error(&error))?;
    if header[..10] != COMPANY[..]
        || header[13] != 0x11
        || u16::from_le_bytes([header[14], header[15]]) != invoke
    {
        return Err(protocol(
            "response company, direction or Invoke ID differs from request",
        ));
    }
    if (config.require_header_checksum || header[19] != 0) && header[19] != checksum(&header[..19])
    {
        return Err(protocol("invalid response header checksum"));
    }
    let length = usize::from(u16::from_le_bytes([header[16], header[17]]));
    if !(9..=usize::from(count) + 12).contains(&length) {
        return Err(protocol(
            "response application length exceeds the requested bound",
        ));
    }
    let mut response = vec![0; length];
    connection
        .stream
        .read_exact(&mut response)
        .map_err(|error| io_error(&error))?;
    remaining(deadline)?;
    if response[..4] != [0x55, 0, wire_type, 0] {
        return Err(protocol(
            "response command or data type differs from request",
        ));
    }
    // Bytes 4..6 are explicitly reserved/don't-care, including 0801 in the vendor example.
    let status = u16::from_le_bytes([response[6], response[7]]);
    if status != 0 {
        let code = match response.get(8..) {
            Some([low]) => u16::from(*low),
            Some([low, high]) => u16::from_le_bytes([*low, *high]),
            _ => return Err(protocol("invalid error response width")),
        };
        return Err(
            Categorized::new(ErrorKind::OperationFailed, "LS XGT PLC rejected read")
                .details(json!({"ls_xgt_status": status, "ls_xgt_error_code": code}))
                .into(),
        );
    }
    if response.len() != usize::from(count) + 12
        || response.get(8..10) != Some(&[1, 0])
        || response.get(10..12) != Some(count.to_le_bytes().as_slice())
    {
        return Err(protocol(
            "response block count or data count differs from request",
        ));
    }
    Ok(response.split_off(12))
}
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("LS XGT: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "LS XGT poll deadline expired",
        ));
    }
    Ok(remaining)
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("LS XGT I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read as _, net::TcpListener, thread};

    fn accept(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_nodelay(true).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected new LS XGT connection");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn config(address: SocketAddr) -> Config {
        serde_json::from_value(json!({"address":address.to_string(),"max_read_bytes":8,
            "signals":[{"name":"wide","variable":"%MB0","data_type":"u64","elements":2}]}))
        .unwrap()
    }
    fn serve(stream: &mut TcpStream, invoke: u16, label: &[u8], value: u64) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = [0; 20];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(&header[14..16], &invoke.to_le_bytes());
        let mut body = vec![0; usize::from(u16::from_le_bytes([header[16], header[17]]))];
        stream.read_exact(&mut body).unwrap();
        assert_eq!(body.len(), 16);
        assert_eq!(&body[..10], &[0x54, 0, 0x14, 0, 0, 0, 1, 0, 4, 0]);
        assert_eq!(&body[10..14], label);
        assert_eq!(&body[14..], &[8, 0]);
        header[13] = 0x11;
        header[16..18].copy_from_slice(&20_u16.to_le_bytes());
        header[19] = checksum(&header[..19]);
        body = vec![0x55, 0, 0x14, 0, 0, 0, 0, 0, 1, 0, 8, 0];
        body.extend_from_slice(&value.to_le_bytes());
        stream.write_all(&header).unwrap();
        stream.write_all(&body).unwrap();
    }

    #[test]
    fn exhausted_invoke_space_opens_a_fresh_socket_before_reuse() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let mut first = accept(&listener);
            serve(&mut first, u16::MAX, b"%MB0", u64::MAX);
            let mut byte = [0];
            assert_eq!(
                first.read(&mut byte).unwrap(),
                0,
                "old socket closes before Invoke ID reuse"
            );
            let mut second = accept(&listener);
            serve(&mut second, 0, b"%MB8", (1_u64 << 53) + 1);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let socket = TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
        let mut client = Client {
            config: config(address),
            connection: Some(Connection {
                stream: net::plain(socket, deadline),
                next_invoke: u32::from(u16::MAX),
            }),
        };
        let readings = client.read(Duration::from_secs(2)).unwrap();
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value, json!([u64::MAX, (1_u64 << 53) + 1]));
        worker.join().unwrap();
    }

    #[test]
    fn failed_poll_discards_the_socket_and_cannot_reuse_a_late_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let mut first = accept(&listener);
            first
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 36];
            first.read_exact(&mut request).unwrap();
            // Correct header except for the foreign Invoke ID; no body is required to reject it.
            request[13] = 0x11;
            request[14] = 1;
            request[16..18].copy_from_slice(&20_u16.to_le_bytes());
            request[19] = checksum(&request[..19]);
            first.write_all(&request[..20]).unwrap();
            let mut byte = [0];
            assert_eq!(first.read(&mut byte).unwrap(), 0);
            let mut second = accept(&listener);
            serve(&mut second, 0, b"%MB0", 11);
            serve(&mut second, 1, b"%MB8", 22);
        });
        let mut client = Client {
            config: config(address),
            connection: None,
        };
        assert!(
            client
                .read(Duration::from_secs(2))
                .unwrap_err()
                .to_string()
                .contains("Invoke ID")
        );
        assert!(client.connection.is_none());
        let readings = client.read(Duration::from_secs(2)).unwrap();
        assert_eq!(readings[0].value, json!([11, 22]));
        worker.join().unwrap();
    }

    #[test]
    fn literal_vendor_continuous_header_and_cpu_specific_address_grammar() {
        let header = b"LSIS-XGT\0\0\x02\x08\xa0\x11\x00\x01\x0e\x00\x01\x26";
        assert_eq!(checksum(&header[..19]), 0x26);
        assert_eq!(u16::from_le_bytes([header[14], header[15]]), 256);
        for cpu in [Cpu::Xgk, Cpu::XgbMk] {
            variable("%MX2764", cpu).unwrap();
            variable("%IW0.0.0", cpu).unwrap_err();
        }
        for cpu in [Cpu::Xgi, Cpu::Xgr, Cpu::XgbIec] {
            for label in [
                "%IW0.0.0",
                "%mw000",
                "%QX0.0.15",
                "%UW00.00",
                "%UX00.00.0",
                "%RW0",
                "%WW0",
            ] {
                variable(label, cpu).unwrap_or_else(|error| panic!("{cpu:?}: {label}: {error}"));
            }
            for label in [
                "%IW0.0", "%UW00", "%UX00.00", "%MX1.A", "%MW١", "%MW1\r", "%DD0",
            ] {
                variable(label, cpu).unwrap_err();
            }
        }
    }
}
