//! Read-only Yokogawa FA-M3 higher-level PC Link over binary/ASCII TCP.
//! IM34M06H24-01E part A5 and IM34M6P41-01E sections 1.2, 2.1–2.2, 3.1–3.2.
//! <https://www.yokogawa.com/pdf/provide/E/GW/IM/0000014291/0/IM34M06H24-01E.pdf>
//! <https://web-material3.yokogawa.com/IM_34M6P41-01E.us.pdf>
use super::{
    Reading, Source, net,
    slmp::{DataType, SizeLimit},
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, BufRead as _, BufReader, Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    #[serde(default)]
    encoding: Encoding,
    #[serde(default = "cpu", deserialize_with = "crate::config_number::unsigned")]
    cpu: u8,
    signals: Vec<Signal>,
}
const fn cpu() -> u8 {
    1
}
const fn one() -> u16 {
    1
}
const fn scale() -> f64 {
    1.0
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    #[default]
    Binary,
    Ascii,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    device: Device,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    address: u32,
    #[serde(default)]
    data_type: DataType,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    elements: u16,
    #[serde(default)]
    word_order: Option<WordOrder>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    link_points: Option<u16>,
    #[serde(default = "scale")]
    scale: f64,
    #[serde(default)]
    offset: f64,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WordOrder {
    Little,
    Big,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Device {
    X,
    Y,
    I,
    E,
    M,
    Tu,
    Cu,
    L,
    D,
    B,
    R,
    V,
    Z,
    W,
    Ts,
    Tp,
    Ti,
    Cs,
    Cp,
    Ci,
}
impl Device {
    const fn code(self) -> u16 {
        match self {
            Self::X => 0x18,
            Self::Y => 0x19,
            Self::I => 9,
            Self::E => 5,
            Self::M => 0xd,
            Self::Tu => 0x14,
            Self::Cu => 3,
            Self::L => 0xc,
            Self::D => 4,
            Self::B => 2,
            Self::R => 0x12,
            Self::V => 0x16,
            Self::Z => 0x1a,
            Self::W => 0x17,
            Self::Ts => 0x20,
            Self::Tp => 0x21,
            Self::Ti => 0x25,
            Self::Cs => 0x30,
            Self::Cp => 0x31,
            Self::Ci => 0x35,
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::Y => "Y",
            Self::I => "I",
            Self::E => "E",
            Self::M => "M",
            Self::Tu => "TU",
            Self::Cu => "CU",
            Self::L => "L",
            Self::D => "D",
            Self::B => "B",
            Self::R => "R",
            Self::V => "V",
            Self::Z => "Z",
            Self::W => "W",
            Self::Ts => "TS",
            Self::Tp => "TP",
            Self::Ti => "TI",
            Self::Cs => "CS",
            Self::Cp => "CP",
            Self::Ci => "CI",
        }
    }
    const fn bit(self) -> bool {
        matches!(
            self,
            Self::X | Self::Y | Self::I | Self::E | Self::M | Self::Tu | Self::Cu | Self::L
        )
    }
    const fn io(self) -> bool {
        matches!(self, Self::X | Self::Y)
    }
    const fn link(self) -> bool {
        matches!(self, Self::L | Self::W)
    }
    fn advance(self, address: u32, points: u32) -> Result<u32> {
        if self.io() {
            let unit = address / 10_000;
            let slot = address / 100 % 100;
            let point = address % 100;
            if unit > 7
                || !(1..=16).contains(&slot)
                || !(1..=64).contains(&point)
                || (unit == 0 && slot == 1)
            {
                return Err(Usage("FA-M3 X/Y address requires unit 0–7, slot 1–16 and point 1–64; unit 0 starts at slot 2".into()).into());
            }
            let linear = unit * 1024 + (slot - 1) * 64 + point - 1;
            let end = linear
                .checked_add(points)
                .filter(|end| *end < 8192)
                .ok_or_else(|| Usage("FA-M3 X/Y range exceeds unit 7/slot 16/point 64".into()))?;
            Ok(end / 1024 * 10_000 + (end / 64 % 16 + 1) * 100 + end % 64 + 1)
        } else {
            address
                .checked_add(points)
                .ok_or_else(|| Usage("FA-M3 address range exceeds 32 bits".into()).into())
        }
    }
    fn ascii(self, address: u32) -> Result<String> {
        let prefix = self.name();
        let width = 6 - prefix.len();
        let label = format!("{prefix}{address:0width$}");
        if label.len() != 6 {
            return Err(Usage("FA-M3 ASCII requires a six-character device name".into()).into());
        }
        Ok(label)
    }
}
impl Signal {
    fn points(&self) -> u32 {
        u32::from(self.elements)
            * if self.device.bit() {
                u32::from(self.data_type.words().max(1))
                    * if matches!(self.data_type, DataType::Bool) {
                        1
                    } else {
                        16
                    }
            } else {
                u32::from(self.data_type.words())
            }
    }
    fn decode(&self, data: &[u8]) -> Result<Value> {
        if matches!(self.data_type, DataType::Bool) {
            if data.len() != usize::from(self.elements) || data.iter().any(|byte| *byte > 1) {
                return Err(protocol("bit reply requires exactly one 0 or 1 per point"));
            }
            if self.elements == 1 {
                return Ok(Value::Bool(data[0] != 0));
            }
            return Ok(Value::Array(
                data.iter().map(|byte| Value::Bool(*byte != 0)).collect(),
            ));
        }
        let width = usize::from(self.data_type.words()) * 2;
        if data.len() != usize::from(self.elements) * width {
            return Err(protocol("word reply differs from configured signal width"));
        }
        let mut ordered = Vec::with_capacity(data.len());
        for value in data.chunks_exact(width) {
            let (words, _) = value.as_chunks::<2>();
            for index in 0..words.len() {
                let word = words[if matches!(self.word_order, Some(WordOrder::Big)) {
                    words.len() - 1 - index
                } else {
                    index
                }];
                ordered.extend_from_slice(&[word[1], word[0]]);
            }
        }
        self.data_type
            .decode(self.elements, self.scale, self.offset, &ordered, protocol)
    }
}
fn validate(config: &Config) -> Result<()> {
    if config.address.port() == 0
        || !(1..=4).contains(&config.cpu)
        || !(1..=256).contains(&config.signals.len())
    {
        return Err(
            Usage("FA-M3 requires a nonzero TCP port, CPU 1–4 and 1–256 signals".into()).into(),
        );
    }
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "FA-M3 signal")?;
        if !names.insert(&signal.name)
            || signal.address == 0
            || !(1..=4096).contains(&signal.elements)
            || (matches!(signal.data_type, DataType::Bool) && !signal.device.bit())
            || (signal.data_type.words() > 1 && signal.word_order.is_none())
            || !signal.data_type.valid_scale(signal.scale, signal.offset)
        {
            return Err(Usage("FA-M3 requires unique signals, one-based addresses, 1–4096 elements, bool only on bit devices, explicit word_order for multiword values and compatible scaling".into()).into());
        }
        let last = signal.device.advance(signal.address, signal.points() - 1)?;
        // Shared and extended E/R have distinct fixed starts even when the
        // lower section is shorter in the project; do not infer hole skipping.
        let shared_end = match signal.device {
            Device::E => Some(2048),
            Device::R => Some(1024),
            _ => None,
        };
        if shared_end.is_some_and(|end| signal.address <= end && last > end) {
            return Err(Usage(
                "FA-M3 E/R arrays must stay within one configured shared or extended section"
                    .into(),
            )
            .into());
        }
        if signal.device.link() {
            let points = signal.link_points.ok_or_else(|| {
                Usage("FA-M3 L/W requires link_points from the configured bank".into())
            })?;
            if points == 0
                || points > 8192
                || !points.is_multiple_of(16)
                || signal.address / 10_000 > 7
                || signal.address % 10_000 == 0
                || last % 10_000 > u32::from(points)
                || signal.address / 10_000 != last / 10_000
            {
                return Err(Usage("FA-M3 L/W arrays must stay within one configured bank 0–7 with link_points 16–8192 in multiples of 16".into()).into());
            }
        } else if signal.link_points.is_some() {
            return Err(Usage("FA-M3 link_points is only valid for L/W devices".into()).into());
        }
        if matches!(config.encoding, Encoding::Ascii) {
            signal.device.ascii(signal.address)?;
            signal.device.ascii(last)?;
        }
    }
    Ok(())
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    validate(config)?;
    Ok(Box::new(Client {
        config: config.clone(),
        stream: None,
    }))
}
struct Client {
    config: Config,
    stream: Option<BufReader<Box<dyn net::Transport>>>,
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("FA-M3 timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.stream = None;
        }
        result
    }
}
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.stream.is_none() {
            let socket = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            socket.set_nodelay(true).map_err(|error| io_error(&error))?;
            self.stream = Some(BufReader::with_capacity(1024, net::plain(socket, deadline)));
        }
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        stream.get_mut().set_deadline(deadline);
        let mut readings = Vec::with_capacity(self.config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for signal in &self.config.signals {
            let value = read_signal(stream, &self.config, signal, deadline)?;
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
    stream: &mut BufReader<Box<dyn net::Transport>>,
    config: &Config,
    signal: &Signal,
    deadline: Instant,
) -> Result<Value> {
    let bits = matches!(signal.data_type, DataType::Bool);
    let total =
        u32::from(signal.elements) * u32::from(if bits { 1 } else { signal.data_type.words() });
    let mut data = Vec::with_capacity(usize::try_from(total)? * if bits { 1 } else { 2 });
    let mut done = 0;
    while done < total {
        remaining(deadline)?;
        let count = u16::try_from((total - done).min(if bits { 256 } else { 64 }))?;
        let address = signal.device.advance(
            signal.address,
            done * if signal.device.bit() && !bits { 16 } else { 1 },
        )?;
        let part = match config.encoding {
            Encoding::Binary => binary(stream, config.cpu, signal.device, address, bits, count)?,
            Encoding::Ascii => ascii(stream, config.cpu, signal.device, address, bits, count)?,
        };
        data.extend_from_slice(&part);
        done += u32::from(count);
    }
    remaining(deadline)?;
    signal.decode(&data)
}
fn binary(
    stream: &mut BufReader<Box<dyn net::Transport>>,
    cpu: u8,
    device: Device,
    address: u32,
    bits: bool,
    count: u16,
) -> Result<Vec<u8>> {
    let command = if bits { 1 } else { 0x11 };
    let mut request = vec![command, cpu, 0, 8];
    request.extend_from_slice(&device.code().to_be_bytes());
    request.extend_from_slice(&address.to_be_bytes());
    request.extend_from_slice(&count.to_be_bytes());
    stream
        .get_mut()
        .write_all(&request)
        .map_err(|error| io_error(&error))?;
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .map_err(|error| io_error(&error))?;
    if header[0] != (command | 0x80) {
        return Err(protocol("response subheader differs from read command"));
    }
    let expected = usize::from(count) * if bits { 1 } else { 2 };
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    if length != if header[1] == 0 { expected } else { 2 } {
        return Err(protocol("response parameter length differs from request"));
    }
    let mut response = vec![0; length];
    stream
        .read_exact(&mut response)
        .map_err(|error| io_error(&error))?;
    if !stream.buffer().is_empty() {
        return Err(protocol("unexpected bytes after binary reply"));
    }
    if header[1] != 0 {
        if response[1] != cpu {
            return Err(protocol("error response CPU differs from request"));
        }
        return Err(rejected(header[1], response[0], &response));
    }
    if bits && response.iter().any(|byte| *byte > 1) {
        return Err(protocol("bit reply byte must be 0 or 1"));
    }
    Ok(response)
}
fn ascii(
    stream: &mut BufReader<Box<dyn net::Transport>>,
    cpu: u8,
    device: Device,
    address: u32,
    bits: bool,
    count: u16,
) -> Result<Vec<u8>> {
    let command = if bits { "BRD" } else { "WRD" };
    let label = device.ascii(address)?;
    let request = if bits {
        format!("0{cpu}{command}{label},{count:03}\r\n")
    } else {
        format!("0{cpu}{command}{label},{count:02}\r\n")
    };
    stream
        .get_mut()
        .write_all(request.as_bytes())
        .map_err(|error| io_error(&error))?;
    let maximum = (usize::from(count) * if bits { 1 } else { 4 } + 6).max(13);
    let mut response = Vec::with_capacity(maximum);
    let length = (&mut *stream)
        .take(u64::try_from(maximum + 1)?)
        .read_until(b'\n', &mut response)
        .map_err(|error| io_error(&error))?;
    if length == 0 {
        return Err(Categorized::error(
            ErrorKind::Disconnected,
            "FA-M3 connection closed during ASCII reply",
        ));
    }
    if length > maximum || !response.ends_with(b"\r\n") {
        return Err(protocol(
            "ASCII reply exceeds requested bound or is missing CRLF",
        ));
    }
    if !stream.buffer().is_empty() {
        return Err(protocol("unexpected bytes after ASCII reply"));
    }
    response.truncate(response.len() - 2);
    if response.get(..2) != Some([b'1', b'0' + cpu].as_slice()) {
        return Err(protocol(
            "ASCII response CPU/subheader differs from request",
        ));
    }
    if response.get(2..4) == Some(b"ER") {
        if response.len() != 11 || response.get(8..) != Some(command.as_bytes()) {
            return Err(protocol(
                "malformed error response or echoed command differs",
            ));
        }
        let code = hex(response
            .get(4..6)
            .ok_or_else(|| protocol("missing error code"))?)?;
        let detail = if matches!(code, 3 | 4 | 5 | 8 | 0x52) {
            hex(response
                .get(6..8)
                .ok_or_else(|| protocol("missing error detail"))?)?
        } else {
            0
        };
        return Err(rejected(
            code,
            detail,
            response
                .get(6..)
                .ok_or_else(|| protocol("missing error parameters"))?,
        ));
    }
    if response.get(2..4) != Some(b"OK")
        || response.len() != usize::from(count) * if bits { 1 } else { 4 } + 4
    {
        return Err(protocol("ASCII status or data width differs from request"));
    }
    let payload = response
        .get(4..)
        .ok_or_else(|| protocol("missing ASCII data"))?;
    if bits {
        payload
            .iter()
            .map(|byte| match byte {
                b'0' => Ok(0),
                b'1' => Ok(1),
                _ => Err(protocol("ASCII bit must be 0 or 1")),
            })
            .collect()
    } else {
        let mut data = Vec::with_capacity(usize::from(count) * 2);
        for word in payload.as_chunks::<4>().0 {
            data.push(hex(&word[..2])?);
            data.push(hex(&word[2..])?);
        }
        Ok(data)
    }
}
fn hex(bytes: &[u8]) -> Result<u8> {
    if bytes.len() != 2 {
        return Err(protocol("hex byte requires two digits"));
    }
    bytes.iter().try_fold(0, |value, byte| {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'A'..=b'F' => byte - b'A' + 10,
            b'a'..=b'f' => byte - b'a' + 10,
            _ => return Err(protocol("invalid ASCII hex digit")),
        };
        Ok((value << 4) | digit)
    })
}
fn rejected(code: u8, detail: u8, raw: &[u8]) -> anyhow::Error {
    let mut details = json!({"yokogawa_exit_code":code,"yokogawa_error_parameters":raw});
    if matches!(code, 3 | 4 | 5 | 8 | 0x52) {
        details["yokogawa_detail_code"] = json!(detail);
    }
    Categorized::new(
        ErrorKind::OperationFailed,
        "Yokogawa FA-M3 PLC rejected read",
    )
    .details(details)
    .into()
}
fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("Yokogawa FA-M3: {message}"),
    )
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "FA-M3 poll deadline expired",
        ));
    }
    Ok(left)
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("FA-M3 I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};
    fn accept(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_nodelay(true).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected fresh FA-M3 connection");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }
    #[test]
    fn failed_reply_discards_socket_before_a_later_poll() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let mut first = accept(&listener);
            let mut request = [0; 12];
            first.read_exact(&mut request).unwrap();
            assert_eq!(request, [0x11, 1, 0, 8, 0, 4, 0, 0, 0, 1, 0, 1]);
            first.write_all(&[0x81, 0, 0, 2]).unwrap();
            let mut byte = [0];
            assert_eq!(first.read(&mut byte).unwrap(), 0);
            let mut second = accept(&listener);
            second.read_exact(&mut request).unwrap();
            assert_eq!(request, [0x11, 1, 0, 8, 0, 4, 0, 0, 0, 1, 0, 1]);
            second.write_all(&[0x91, 0, 0, 2, 0x12, 0x34]).unwrap();
        });
        let config: Config = serde_json::from_value(json!({
            "address": address.to_string(),
            "signals": [{"name":"value","device":"D","address":1}]
        }))
        .unwrap();
        let mut client = Client {
            config,
            stream: None,
        };
        assert!(
            client
                .read(Duration::from_secs(2))
                .unwrap_err()
                .to_string()
                .contains("subheader")
        );
        assert!(client.stream.is_none());
        let values = client.read(Duration::from_secs(2)).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].value, 0x1234);
        worker.join().unwrap();
    }
    #[test]
    fn manufacturer_io_address_gaps_and_typed_extent_are_preserved() {
        for (start, delta, end) in [
            (217, 16, 233),
            (249, 16, 301),
            (264, 1, 301),
            (1664, 1, 10_101),
            (1664, 256, 10_464),
            (71_663, 1, 71_664),
        ] {
            assert_eq!(Device::X.advance(start, delta).unwrap(), end);
        }
        Device::X.advance(71_664, 1).unwrap_err();
        Device::Y.advance(265, 0).unwrap_err();
        assert_eq!(Device::Tu.ascii(123).unwrap(), "TU0123");
        assert_eq!(Device::D.ascii(123).unwrap(), "D00123");
        Device::B.ascii(100_000).unwrap_err();
    }
}
