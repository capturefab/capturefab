//! Read-only MELSEC-F Computer Link serial formats 1 and 4.
//! Mitsubishi `JY997D16901R` §§6–8 defines BR/WR/QR, ACK and sum check.
//! <https://dl.mitsubishielectric.com/dl/fa/document/manual/plc_fx/jy997d16901/jy997d16901r.pdf>
use super::{
    Source,
    serial::{self, Parity, SerialSignal},
    slmp::DataType,
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    device: String,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
    #[serde(default)]
    parity: Parity,
    #[serde(
        default = "stop_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    stop_bits: u8,
    #[serde(
        default = "data_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    data_bits: u8,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    unit: u8,
    #[serde(default)]
    format: Format,
    #[serde(default = "sum_check")]
    sum_check: bool,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    message_wait_ms: u8,
    #[serde(
        default = "inter_frame_ms",
        deserialize_with = "crate::config_number::unsigned"
    )]
    inter_frame_ms: u16,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    ack_delay_ms: Option<u16>,
    #[serde(
        default = "max_words",
        deserialize_with = "crate::config_number::unsigned"
    )]
    max_words: u16,
    #[serde(
        default = "max_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    max_bits: u16,
    signals: Vec<SerialSignal<Signal>>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
enum Format {
    #[default]
    #[serde(rename = "format_1")]
    One,
    #[serde(rename = "format_4")]
    Four,
}

const fn baud() -> u32 {
    9600
}
const fn stop_bits() -> u8 {
    1
}
const fn data_bits() -> u8 {
    7
}
const fn sum_check() -> bool {
    true
}
const fn inter_frame_ms() -> u16 {
    1
}
const fn max_words() -> u16 {
    64
}
const fn max_bits() -> u16 {
    256
}
const fn one() -> u16 {
    1
}
const fn scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    device: Device,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    address: u32,
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    elements: u16,
    #[serde(default)]
    data_type: DataType,
    #[serde(default = "scale")]
    scale: f64,
    #[serde(default)]
    offset: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Device {
    D,
    R,
    Tn,
    X,
    Y,
    M,
    S,
}

impl Device {
    const fn bits(self) -> bool {
        matches!(self, Self::X | Self::Y | Self::M | Self::S)
    }

    fn range_valid(self, start: u32, end: u32) -> bool {
        match self {
            Self::D => end <= 8511 && (end <= 7999 || start >= 8000),
            Self::R => end <= 32_767,
            Self::Tn => end <= 511,
            Self::X | Self::Y => end <= 255,
            Self::M => end <= 8511 && (end <= 7679 || start >= 8000),
            Self::S => end <= 4095,
        }
    }
}

impl Signal {
    fn points_per_element(&self) -> u16 {
        self.data_type.words().max(1)
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.device.is_empty()
        || config.unit > 15
        || !(1..=256).contains(&config.signals.len())
        || !matches!(config.data_bits, 7 | 8)
        || config.baud > 38_400
        || config.message_wait_ms > 150
        || !config.message_wait_ms.is_multiple_of(10)
        || !(1..=1000).contains(&config.inter_frame_ms)
        || !(1..=64).contains(&config.max_words)
        || !(1..=256).contains(&config.max_bits)
    {
        return Err(Usage(
            "FX Computer Link requires a device, unit 0–15, 1–256 signals, 7/8 data bits, baud up to 38400, message_wait_ms 0–150 in steps of 10, inter_frame_ms 1–1000, max_words 1–64 and max_bits 1–256".into(),
        ).into());
    }
    if config
        .ack_delay_ms
        .is_some_and(|delay| !(1..=30_000).contains(&delay))
    {
        return Err(Usage("FX Computer Link ack_delay_ms must be 1–30000".into()).into());
    }
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    let mut names = BTreeSet::new();
    for entry in &config.signals {
        let signal = &entry.signal;
        super::validate_name(&signal.name, "FX Computer Link signal")?;
        if !names.insert(&signal.name) || entry.unit(config.unit) > 15 {
            return Err(Usage(
                "FX Computer Link signals require unique names and units 0–15".into(),
            )
            .into());
        }
        if signal.device.bits() != matches!(signal.data_type, DataType::Bool)
            || (matches!(signal.device, Device::Tn) && signal.data_type.words() != 1)
            || signal.points_per_element() > config.max_words
            || !signal.data_type.valid_scale(signal.scale, signal.offset)
        {
            return Err(Usage(
                "FX Computer Link requires bool for X/Y/M/S, a 16-bit integer for TN, or a numeric type for D/R with compatible limits and scaling".into(),
            ).into());
        }
        let extent = u32::from(signal.elements) * u32::from(signal.points_per_element());
        if !(1..=4096).contains(&signal.elements)
            || signal
                .address
                .checked_add(extent.saturating_sub(1))
                .is_none_or(|end| !signal.device.range_valid(signal.address, end))
        {
            return Err(Usage(
                "FX Computer Link signal exceeds the documented device range or 4096 elements"
                    .into(),
            )
            .into());
        }
    }
    #[cfg(any(unix, windows))]
    {
        wire::open(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config.format, config.sum_check);
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "FX Computer Link requires a Unix terminal or Windows COM port",
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{Categorized, Config, Device, ErrorKind, Format, Parity, Signal, Source, Usage};
    use crate::timeseries::{
        Reading,
        serial::{self, Port},
        slmp::SizeLimit,
    };
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::{
        io::{self, Write as _},
        thread,
        time::{Duration, Instant},
    };

    const MAX_DATA: usize = 256;
    const ETX: u8 = 3;

    pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
        let port = serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            config.data_bits,
        )?;
        port.discard_input().map_err(|error| serial_error(&error))?;
        Ok(Box::new(Client {
            config: config.clone(),
            port: Some(port),
        }))
    }

    struct Client {
        config: Config,
        port: Option<Port>,
    }

    impl Source for Client {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("FX Computer Link timeout is too large".into()))?;
            let port = self
                .port
                .as_mut()
                .ok_or_else(|| protocol("serial link closed"))?;
            let result = poll(port, &self.config, deadline);
            if result.is_err() {
                self.port = None;
            }
            result
        }
    }

    fn poll(port: &mut Port, config: &Config, deadline: Instant) -> Result<Vec<Reading>> {
        let mut readings = Vec::with_capacity(config.signals.len());
        let mut size = SizeLimit { bytes: 2 };
        for entry in &config.signals {
            let signal = &entry.signal;
            let value = read_signal(port, config, entry.unit(config.unit), signal, deadline)?;
            let reading = Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            };
            serde_json::to_writer(&mut size, &reading)
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            size.write_all(b",")
                .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
            readings.push(reading);
        }
        check_deadline(deadline)?;
        Ok(readings)
    }

    fn read_signal(
        port: &mut Port,
        config: &Config,
        unit: u8,
        signal: &Signal,
        deadline: Instant,
    ) -> Result<Value> {
        let bits = signal.device.bits();
        let points_per_element = signal.points_per_element();
        let maximum = if bits {
            config.max_bits
        } else {
            config.max_words / points_per_element
        };
        let mut output =
            Vec::with_capacity(usize::from(signal.elements) * usize::from(points_per_element) * 2);
        let station = format!("{unit:02X}FF");
        let gap = Duration::from_millis(u64::from(config.inter_frame_ms));
        let mut done = 0;
        let mut data = [0; MAX_DATA];
        while done < signal.elements {
            let elements = (signal.elements - done).min(maximum);
            let points = elements * points_per_element;
            let address = signal.address + u32::from(done) * u32::from(points_per_element);
            let request = request(config, &station, signal.device, address, points)?;
            port.write_all(&request, deadline)
                .map_err(|error| serial_error(&error))?;
            let expected = usize::from(points) * if bits { 1 } else { 4 };
            let payload = &mut data[..expected];
            response(port, config, station.as_bytes(), payload, deadline)?;
            if bits {
                if !payload.iter().all(|byte| matches!(byte, b'0' | b'1')) {
                    return Err(protocol("BR payload must contain one ASCII 0 or 1 per bit"));
                }
                output.extend_from_slice(payload);
            } else {
                for word in payload.as_chunks::<4>().0 {
                    let mut value = 0_u16;
                    for &byte in word {
                        value = (value << 4) | u16::from(hex_digit(byte)?);
                    }
                    output.extend(value.to_le_bytes());
                }
            }
            acknowledge(port, config, station.as_bytes(), gap, deadline)?;
            done += elements;
        }
        if bits {
            if signal.elements == 1 {
                Ok(Value::Bool(output[0] == b'1'))
            } else {
                Ok(Value::Array(
                    output
                        .into_iter()
                        .map(|bit| Value::Bool(bit == b'1'))
                        .collect(),
                ))
            }
        } else {
            signal.data_type.decode(
                signal.elements,
                signal.scale,
                signal.offset,
                &output,
                protocol,
            )
        }
    }

    fn request(
        config: &Config,
        station: &str,
        device: Device,
        address: u32,
        points: u16,
    ) -> Result<Vec<u8>> {
        let (command, head) = match device {
            Device::D => ("WR", format!("D{address:04}")),
            Device::R => ("QR", format!("R{address:06}")),
            Device::Tn => ("WR", format!("TN{address:03}")),
            Device::X => ("BR", format!("X{address:04o}")),
            Device::Y => ("BR", format!("Y{address:04o}")),
            Device::M => ("BR", format!("M{address:04}")),
            Device::S => ("BR", format!("S{address:04}")),
        };
        let count = if points == 256 {
            0
        } else {
            u8::try_from(points)?
        };
        let wait = config.message_wait_ms / 10;
        let mut bytes = format!("\x05{station}{command}{wait:X}{head}{count:02X}").into_bytes();
        if config.sum_check {
            bytes.extend(hex_code(sum(&bytes[1..])));
        }
        bytes.extend(line_end(config.format));
        Ok(bytes)
    }

    fn response(
        port: &mut Port,
        config: &Config,
        station: &[u8],
        data: &mut [u8],
        deadline: Instant,
    ) -> Result<()> {
        let mut prefix = [0; 5];
        receive(port, &mut prefix, deadline)?;
        if &prefix[1..] != station {
            return Err(protocol(
                "reply station or PLC number differs from the request",
            ));
        }
        let end = line_end(config.format);
        if prefix[0] == 0x15 {
            let mut error = [0; 4];
            receive(port, &mut error[..2 + end.len()], deadline)?;
            if &error[2..2 + end.len()] != end {
                return Err(protocol("invalid NAK terminator"));
            }
            let status = (hex_digit(error[0])? << 4) | hex_digit(error[1])?;
            require_quiet(port, config.inter_frame_ms, deadline)?;
            return Err(Categorized::new(
                ErrorKind::OperationFailed,
                "FX Computer Link PLC rejected read",
            )
            .details(json!({"fx_error_code":status}))
            .into());
        }
        if prefix[0] != 2 {
            return Err(protocol("expected STX read reply or NAK"));
        }
        receive(port, data, deadline)?;
        let mut suffix = [0; 5];
        let checksum_width = if config.sum_check { 2 } else { 0 };
        let tail_start = 1 + checksum_width;
        receive(port, &mut suffix[..tail_start + end.len()], deadline)?;
        if suffix[0] != ETX || &suffix[tail_start..tail_start + end.len()] != end {
            return Err(protocol("invalid read reply length or terminator"));
        }
        if config.sum_check {
            let received = (hex_digit(suffix[1])? << 4) | hex_digit(suffix[2])?;
            let expected = sum(station).wrapping_add(sum(data)).wrapping_add(ETX);
            if received != expected {
                return Err(protocol("reply sum check differs from received bytes"));
            }
        }
        let ack_delay = config.ack_delay_ms.unwrap_or(config.inter_frame_ms);
        require_quiet(port, ack_delay, deadline)
    }

    fn require_quiet(port: &mut Port, delay_ms: u16, deadline: Instant) -> Result<()> {
        // Wait for the configured turnaround and reject bytes beyond the framed reply.
        let ready = Instant::now() + Duration::from_millis(u64::from(delay_ms));
        if ready >= deadline {
            return Err(timeout());
        }
        match port
            .receive(&mut [0], ready)
            .map_err(|error| serial_error(&error))?
        {
            Some(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "FX Computer Link serial device closed",
                ));
            }
            Some(_) => return Err(protocol("unexpected bytes after read reply")),
            None => {}
        }
        wait(ready, deadline)
    }

    fn acknowledge(
        port: &mut Port,
        config: &Config,
        station: &[u8],
        gap: Duration,
        deadline: Instant,
    ) -> Result<()> {
        let mut ack = vec![6];
        ack.extend_from_slice(station);
        ack.extend(line_end(config.format));
        port.write_all(&ack, deadline)
            .map_err(|error| serial_error(&error))?;
        let bits = u64::from(
            1 + config.data_bits
                + config.stop_bits
                + u8::from(!matches!(config.parity, Parity::None)),
        );
        let baud = u64::from(config.baud);
        let wire =
            Duration::from_nanos((bits * u64::try_from(ack.len())? * 1_000_000_000).div_ceil(baud));
        wait(Instant::now() + wire, deadline)?;
        let mut extra = Duration::ZERO;
        while !port.output_empty().map_err(|error| serial_error(&error))? {
            extra = Duration::from_nanos((bits * 1_000_000_000).div_ceil(baud));
            wait(Instant::now() + Duration::from_millis(1), deadline)?;
        }
        // A queue that drained late can still have its final character on the wire.
        wait(Instant::now() + gap + extra, deadline)
    }

    const fn line_end(format: Format) -> &'static [u8] {
        match format {
            Format::One => b"",
            Format::Four => b"\r\n",
        }
    }

    fn sum(bytes: &[u8]) -> u8 {
        bytes.iter().copied().fold(0, u8::wrapping_add)
    }

    fn hex_code(byte: u8) -> [u8; 2] {
        const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
        [
            DIGITS[usize::from(byte >> 4)],
            DIGITS[usize::from(byte & 15)],
        ]
    }

    fn hex_digit(byte: u8) -> Result<u8> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(protocol("reply contains invalid hexadecimal")),
        }
    }

    fn receive(port: &mut Port, bytes: &mut [u8], deadline: Instant) -> Result<()> {
        port.read_exact(bytes, deadline)
            .map_err(|error| serial_error(&error))
    }

    fn wait(ready: Instant, deadline: Instant) -> Result<()> {
        if ready >= deadline {
            return Err(timeout());
        }
        thread::sleep(ready.saturating_duration_since(Instant::now()));
        check_deadline(deadline)
    }

    fn check_deadline(deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        Ok(())
    }

    fn serial_error(error: &io::Error) -> anyhow::Error {
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => timeout(),
            io::ErrorKind::UnexpectedEof => Categorized::error(
                ErrorKind::Disconnected,
                "FX Computer Link serial device closed",
            ),
            _ => protocol(&format!("serial I/O: {error}")),
        }
    }

    fn timeout() -> anyhow::Error {
        Categorized::error(ErrorKind::Timeout, "FX Computer Link poll deadline expired")
    }

    fn protocol(message: &str) -> anyhow::Error {
        Categorized::error(
            ErrorKind::Incomplete,
            format!("FX Computer Link: {message}"),
        )
    }
}
