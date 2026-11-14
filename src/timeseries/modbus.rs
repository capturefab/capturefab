//! Read-only Modbus polling. Addresses are zero-based PDU addresses, never
//! the printed 40001/30001 register numbers. Multi-register words default
//! to network order; byte swapping accommodates device-specific register layouts.
//! Read quantities and bit packing follow Modbus Application Protocol `V1.1b3`,
//! sections 6.1–6.4. RTU frame bounds and CRC use the Serial Line Guide `V1.02`,
//! section 2.5.1.
//! <https://www.modbus.org/file/secure/modbusprotocolspecification.pdf>
//! <https://www.modbus.org/file/secure/modbusoverserial.pdf>

pub(super) mod ascii;

use super::{Reading, Source, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const MAX_BITS: u16 = 2_000;
const MAX_REGISTERS: u16 = 125;
const MAX_BATCH: usize = 1 << 20;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TcpConfig {
    pub address: String,
    #[serde(default = "default_unit")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub unit: u8,
    pub signals: Vec<SerialSignal<Signal>>,
    #[serde(default)]
    pub tls: Option<net::TlsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RtuConfig {
    pub device: String,
    #[serde(default = "default_baud")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub baud: u32,
    #[serde(default)]
    pub parity: Parity,
    #[serde(default = "default_stop_bits")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub stop_bits: u8,
    #[serde(default = "default_unit")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub unit: u8,
    pub signals: Vec<SerialSignal<Signal>>,
}

const fn default_unit() -> u8 {
    1
}
const fn default_baud() -> u32 {
    19_200
}
const fn default_stop_bits() -> u8 {
    1
}
const fn one() -> u16 {
    1
}
const fn default_scale() -> f64 {
    1.0
}

pub(crate) use super::serial::Parity;
use super::serial::SerialSignal;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub address: u16,
    #[serde(default)]
    pub table: Table,
    #[serde(default)]
    pub data_type: DataType,
    /// Consecutive typed values in one read. One value is a scalar.
    #[serde(default = "one", deserialize_with = "crate::config_number::unsigned")]
    pub elements: u16,
    #[serde(default)]
    pub word_order: WordOrder,
    #[serde(default)]
    pub byte_order: WordOrder,
    #[serde(default = "default_scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Table {
    #[default]
    Holding,
    Input,
    Coil,
    Discrete,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataType {
    #[default]
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WordOrder {
    #[default]
    Big,
    Little,
}

impl Signal {
    const fn function(&self) -> u8 {
        match self.table {
            Table::Coil => 1,
            Table::Discrete => 2,
            Table::Holding => 3,
            Table::Input => 4,
        }
    }

    const fn bit_table(&self) -> bool {
        matches!(self.table, Table::Coil | Table::Discrete)
    }

    const fn element_quantity(&self) -> u16 {
        match self.data_type {
            DataType::U64 | DataType::I64 | DataType::F64 => 4,
            DataType::U32 | DataType::I32 | DataType::F32 => 2,
            DataType::U16 | DataType::I16 | DataType::Bool => 1,
        }
    }

    fn quantity(&self) -> Result<u16> {
        let (maximum, units) = if self.bit_table() {
            (MAX_BITS, "bits")
        } else {
            (MAX_REGISTERS, "registers")
        };
        self.element_quantity()
            .checked_mul(self.elements)
            .filter(|count| (1..=maximum).contains(count))
            .ok_or_else(|| {
                Usage(format!(
                    "Modbus signal {} requires 1–{maximum} {units} per read",
                    self.name
                ))
                .into()
            })
    }

    fn request(&self) -> Result<[u8; 5]> {
        let address = self.address.to_be_bytes();
        let quantity = self.quantity()?.to_be_bytes();
        Ok([
            self.function(),
            address[0],
            address[1],
            quantity[0],
            quantity[1],
        ])
    }

    fn decode(&self, response: &[u8]) -> Result<Reading> {
        if response.first() == Some(&(self.function() | 0x80)) {
            if response.len() != 2 {
                return Err(protocol("invalid exception response length"));
            }
            return Err(Categorized::new(
                ErrorKind::OperationFailed,
                format!("Modbus exception {} reading {}", response[1], self.name),
            )
            .details(json!({"signal": self.name, "exception": response[1]}))
            .into());
        }
        let quantity = usize::from(self.quantity()?);
        let bytes = if self.bit_table() {
            quantity.div_ceil(8)
        } else {
            quantity * 2
        };
        if response.len() != bytes + 2
            || response.first() != Some(&self.function())
            || response.get(1).copied().map(usize::from) != Some(bytes)
        {
            return Err(protocol("unexpected function or response byte count"));
        }
        let data = &response[2..];
        let value = if self.bit_table() {
            self.decode_bits(data)?
        } else if self.elements == 1 {
            self.decode_registers(data)?
        } else {
            let width = usize::from(self.element_quantity()) * 2;
            Value::Array(
                data.chunks_exact(width)
                    .map(|element| self.decode_registers(element))
                    .collect::<Result<Vec<_>>>()?,
            )
        };
        Ok(Reading {
            signal: self.name.clone(),
            value,
            source_time: None,
        })
    }

    fn decode_bits(&self, data: &[u8]) -> Result<Value> {
        // The starting address is bit zero of the first byte. High unused
        // bits in the final byte must be zero, including in scalar replies.
        let tail = self.elements % 8;
        if tail != 0 && data.last().is_some_and(|&byte| byte >> tail != 0) {
            return Err(protocol("nonzero padding bits in bit response"));
        }
        if self.elements == 1 {
            let [byte] = data else {
                return Err(protocol("invalid single-bit response width"));
            };
            return Ok(Value::Bool(byte & 1 != 0));
        }
        Ok(Value::Array(
            data.iter()
                .flat_map(|&byte| (0..8).map(move |bit| Value::Bool(byte & (1 << bit) != 0)))
                .take(usize::from(self.elements))
                .collect(),
        ))
    }

    fn decode_registers(&self, data: &[u8]) -> Result<Value> {
        match self.data_type {
            DataType::Bool => Err(protocol("registers require a numeric data type")),
            DataType::U16 => self.number(f64::from(u16::from_be_bytes(self.ordered(data)?))),
            DataType::I16 => self.number(f64::from(i16::from_be_bytes(self.ordered(data)?))),
            DataType::U32 => self.number(f64::from(u32::from_be_bytes(self.ordered(data)?))),
            DataType::I32 => self.number(f64::from(i32::from_be_bytes(self.ordered(data)?))),
            // Do not send full-width counters through f64, which rounds values
            // above 2^53. Validation excludes scaling for these exact types.
            DataType::U64 => Ok(json!(u64::from_be_bytes(self.ordered(data)?))),
            DataType::I64 => Ok(json!(i64::from_be_bytes(self.ordered(data)?))),
            DataType::F32 => self.number(f64::from(f32::from_be_bytes(self.ordered(data)?))),
            DataType::F64 => self.number(f64::from_be_bytes(self.ordered(data)?)),
        }
    }

    fn ordered<const N: usize>(&self, data: &[u8]) -> Result<[u8; N]> {
        let mut ordered: [u8; N] = data
            .try_into()
            .map_err(|_| protocol("invalid sample width for configured data type"))?;
        if matches!(self.word_order, WordOrder::Little) {
            for (output, input) in ordered
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(data.as_chunks::<2>().0.iter().rev())
            {
                output.copy_from_slice(input);
            }
        }
        if matches!(self.byte_order, WordOrder::Little) {
            for word in ordered.as_chunks_mut::<2>().0 {
                word.swap(0, 1);
            }
        }
        Ok(ordered)
    }

    fn number(&self, value: f64) -> Result<Value> {
        let scaled = value * self.scale + self.offset;
        if !scaled.is_finite() {
            return Err(protocol("non-finite Modbus sample or scaled result"));
        }
        Ok(json!(scaled))
    }
}

fn validate<'a, I>(signals: I) -> Result<()>
where
    I: ExactSizeIterator<Item = &'a Signal>,
{
    if !(1..=256).contains(&signals.len()) {
        return Err(Usage("Modbus needs 1..256 signals".into()).into());
    }
    let mut names = std::collections::BTreeSet::new();
    for signal in signals {
        if signal.name.trim().is_empty()
            || signal.name.len() > 256
            || signal.name.chars().any(char::is_control)
            || !names.insert(&signal.name)
        {
            return Err(Usage(
                "Modbus signal names must be unique, nonempty text (up to 256 bytes)".into(),
            )
            .into());
        }
        let bits = signal.bit_table();
        if bits != matches!(signal.data_type, DataType::Bool) {
            return Err(Usage("Modbus coil/discrete signals require data_type bool; registers require a numeric data_type".into()).into());
        }
        if signal.address.checked_add(signal.quantity()? - 1).is_none() {
            return Err(Usage(format!(
                "Modbus signal {} exceeds address 65535",
                signal.name
            ))
            .into());
        }
        let exact = bits || matches!(signal.data_type, DataType::U64 | DataType::I64);
        if !signal.scale.is_finite()
            || !signal.offset.is_finite()
            || (exact && (signal.scale != 1.0 || signal.offset != 0.0))
        {
            return Err(Usage(
                "Modbus scale/offset must be finite and cannot scale bool or 64-bit integer signals".into(),
            )
            .into());
        }
    }
    Ok(())
}

// Count the actual serialized array shape before retaining each reading or
// requesting the next signal. Neither transport publishes a partial poll.
struct BatchSize(usize);
impl BatchSize {
    fn charge(&mut self, reading: &Reading, separator: bool) -> Result<()> {
        let too_large = || protocol("serialized poll exceeds 1 MiB batch limit");
        if separator {
            self.write_all(b",").map_err(|_| too_large())?;
        }
        serde_json::to_writer(self, reading).map_err(|_| too_large())
    }
}
impl Write for BatchSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_BATCH {
            return Err(io::Error::other("Modbus batch exceeds size limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("Modbus: {message}"))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "Modbus poll deadline expired",
        ));
    }
    Ok(remaining)
}

fn io_error(error: &io::Error) -> anyhow::Error {
    #[cfg(feature = "mqtt-tls")]
    if matches!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<rustls::Error>()),
        Some(
            rustls::Error::InvalidCertificate(_)
                | rustls::Error::NoCertificatesPresented
                | rustls::Error::AlertReceived(
                    rustls::AlertDescription::BadCertificate
                        | rustls::AlertDescription::NoCertificate
                        | rustls::AlertDescription::CertificateRequired
                        | rustls::AlertDescription::CertificateRevoked
                        | rustls::AlertDescription::CertificateExpired
                        | rustls::AlertDescription::CertificateUnknown
                        | rustls::AlertDescription::UnsupportedCertificate
                        | rustls::AlertDescription::UnknownCA
                        | rustls::AlertDescription::AccessDenied,
                ),
        ),
    ) {
        return Categorized::error(ErrorKind::AuthRejected, "Modbus TLS authentication failed");
    }
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Unavailable,
    };
    Categorized::error(kind, format!("Modbus I/O: {error}"))
}

pub(crate) fn tcp(config: &TcpConfig) -> Result<Box<dyn Source>> {
    validate(config.signals.iter().map(|entry| &entry.signal))?;
    let address = config.address.parse::<SocketAddr>().map_err(|_| {
        Usage(
            "Modbus TCP address must be a numeric IP:port, e.g. 192.168.1.20:502 or [::1]:502"
                .into(),
        )
    })?;
    Ok(Box::new(Tcp {
        address,
        unit: config.unit,
        signals: config.signals.clone(),
        transport: net::prepare(config.tls.as_ref())?,
        stream: None,
        transaction: 0,
    }))
}

struct Tcp {
    address: SocketAddr,
    unit: u8,
    signals: Vec<SerialSignal<Signal>>,
    transport: net::Prepared,
    stream: Option<Box<dyn net::Transport>>,
    transaction: u16,
}

impl Tcp {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.stream.is_none() {
            let stream = TcpStream::connect_timeout(&self.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            stream.set_nodelay(true).map_err(|error| io_error(&error))?;
            self.stream = Some(self.transport.connect(stream, deadline)?);
        }
        let stream = self
            .stream
            .as_deref_mut()
            .ok_or_else(|| protocol("connection missing"))?;
        stream.set_deadline(deadline);
        let mut readings = Vec::with_capacity(self.signals.len());
        let mut size = BatchSize(2);
        for entry in &self.signals {
            let signal = &entry.signal;
            let unit = entry.unit(self.unit);
            self.transaction = self.transaction.wrapping_add(1);
            let transaction = self.transaction.to_be_bytes();
            let pdu = signal.request()?;
            let mut request = [0_u8; 12];
            request[..7].copy_from_slice(&[transaction[0], transaction[1], 0, 0, 0, 6, unit]);
            request[7..].copy_from_slice(&pdu);
            tcp_write(stream, &request)?;
            let mut header = [0_u8; 7];
            tcp_read(stream, &mut header)?;
            let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
            if header[..2] != transaction || header[2..4] != [0, 0] || header[6] != unit {
                return Err(protocol(
                    "unexpected transaction, protocol or unit in TCP response",
                ));
            }
            if !(3..=254).contains(&length) {
                return Err(protocol("invalid TCP response length"));
            }
            let mut response = vec![0_u8; length - 1];
            tcp_read(stream, &mut response)?;
            let reading = signal.decode(&response)?;
            size.charge(&reading, !readings.is_empty())?;
            remaining(deadline)?;
            readings.push(reading);
        }
        Ok(readings)
    }
}

impl Source for Tcp {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let result = self.poll(Instant::now() + timeout);
        // Discard a timed-out or invalid stream; its late reply must never be
        // mistaken for the next request's reply.
        if result.is_err() {
            self.stream = None;
        }
        result
    }
}

fn tcp_read(stream: &mut dyn net::Transport, bytes: &mut [u8]) -> Result<()> {
    stream.read_exact(bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            protocol("TCP peer closed the connection")
        } else {
            io_error(&error)
        }
    })
}

fn tcp_write(stream: &mut dyn net::Transport, bytes: &[u8]) -> Result<()> {
    stream.write_all(bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::WriteZero {
            protocol("TCP peer accepted no request bytes")
        } else {
            io_error(&error)
        }
    })?;
    stream.flush().map_err(|error| io_error(&error))
}

fn validate_serial_units(default: u8, signals: &[SerialSignal<Signal>], mode: &str) -> Result<()> {
    let valid = |unit| (1..=247).contains(&unit);
    if !valid(default) {
        return Err(Usage(format!(
            "Modbus {mode} unit must be 1..247 (broadcasts cannot be read)"
        ))
        .into());
    }
    for entry in signals {
        if !valid(entry.unit(default)) {
            return Err(Usage(format!(
                "Modbus {mode} signal {} unit must be 1..247 (broadcasts cannot be read)",
                entry.signal.name
            ))
            .into());
        }
    }
    Ok(())
}

pub(crate) fn rtu(config: &RtuConfig) -> Result<Box<dyn Source>> {
    validate(config.signals.iter().map(|entry| &entry.signal))?;
    validate_serial_units(config.unit, &config.signals, "RTU")?;
    if config.device.is_empty() || !matches!(config.stop_bits, 1 | 2) {
        return Err(Usage("Modbus RTU needs device and stop_bits 1 or 2".into()).into());
    }
    #[cfg(any(unix, windows))]
    {
        serial::open(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "Modbus RTU ({} baud, {:?} parity) requires a Unix terminal or Windows COM port",
                config.baud, config.parity
            ),
        ))
    }
}

#[cfg(any(unix, windows, test))]
fn crc(bytes: &[u8]) -> u16 {
    let mut crc = 0xffff_u16;
    for &byte in bytes {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xa001 } else { 0 };
        }
    }
    crc
}

#[cfg(any(unix, windows))]
mod serial {
    use super::{
        BatchSize, Parity, RtuConfig, SerialSignal, Signal, crc, io_error, protocol, remaining,
    };
    use crate::timeseries::serial::Port;
    use crate::timeseries::{Reading, Source};
    use crate::types::{Categorized, ErrorKind};
    use anyhow::Result;
    use std::time::{Duration, Instant};

    pub(super) fn open(config: &RtuConfig) -> Result<Box<dyn Source>> {
        let file = crate::timeseries::serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            8,
        )?;
        // Above 19200 baud, the serial-line specification recommends a fixed
        // 1.75ms inter-frame gap; slower links wait 3.5 characters.
        let bits =
            1 + 8 + u32::from(!matches!(config.parity, Parity::None)) + u32::from(config.stop_bits);
        let gap = if config.baud > 19_200 {
            Duration::from_micros(1750)
        } else {
            Duration::from_secs_f64(3.5 * f64::from(bits) / f64::from(config.baud))
        };
        file.discard_input().map_err(|error| io_error(&error))?;
        Ok(Box::new(Rtu {
            file,
            unit: config.unit,
            signals: config.signals.clone(),
            gap,
            last_activity: Instant::now(),
            idle_before_request: false,
        }))
    }

    struct Rtu {
        file: Port,
        unit: u8,
        signals: Vec<SerialSignal<Signal>>,
        gap: Duration,
        last_activity: Instant,
        idle_before_request: bool,
    }

    impl Source for Rtu {
        fn end_timeout_is_idle(&self) -> bool {
            self.idle_before_request
        }
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            self.idle_before_request = true;
            let result = self.poll(Instant::now() + timeout);
            self.last_activity = Instant::now();
            if result.is_err() {
                // Drop any partial response so a later poll begins with a
                // clean frame boundary. Preserve the original error.
                drop(self.file.discard_input());
            }
            result
        }
    }

    impl Rtu {
        fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
            let mut readings = Vec::with_capacity(self.signals.len());
            let mut size = BatchSize(2);
            for entry in &self.signals {
                let signal = &entry.signal;
                let unit = entry.unit(self.unit);
                let gap = self.gap.saturating_sub(self.last_activity.elapsed());
                wait_for_gap(gap, deadline)?;
                let mut request = Vec::with_capacity(8);
                request.push(unit);
                request.extend_from_slice(&signal.request()?);
                request.extend_from_slice(&crc(&request).to_le_bytes());
                // Only the initial quiet gap is idle. Once any request in this
                // poll starts, even a later sibling's gap belongs to that poll.
                self.idle_before_request = false;
                self.file
                    .write_all(&request, deadline)
                    .map_err(|error| io_error(&error))?;
                let mut header = [0_u8; 3];
                self.file
                    .read_exact(&mut header, deadline)
                    .map_err(|error| io_error(&error))?;
                if header[0] != unit {
                    return Err(protocol("unexpected unit in RTU response"));
                }
                let exception = header[1] == signal.function() | 0x80;
                let following = if exception {
                    2
                } else {
                    usize::from(header[2]) + 2
                };
                if header[1] != signal.function() && !exception {
                    return Err(protocol("unexpected function in RTU response"));
                }
                if following > 252 {
                    return Err(protocol("oversized RTU response"));
                }
                let mut response = Vec::from(header);
                response.resize(3 + following, 0);
                self.file
                    .read_exact(&mut response[3..], deadline)
                    .map_err(|error| io_error(&error))?;
                self.last_activity = Instant::now();
                let crc_start = response.len() - 2;
                if crc(&response[..crc_start])
                    != u16::from_le_bytes([response[crc_start], response[crc_start + 1]])
                {
                    return Err(protocol("RTU response CRC mismatch"));
                }
                let reading = signal.decode(&response[1..crc_start])?;
                size.charge(&reading, !readings.is_empty())?;
                remaining(deadline)?;
                readings.push(reading);
            }
            Ok(readings)
        }
    }

    fn wait_for_gap(gap: Duration, deadline: Instant) -> Result<()> {
        let left = remaining(deadline)?;
        // A recording can clip the final poll at its acquisition cutoff.
        // Report the timeout at that deadline without issuing another request.
        std::thread::sleep(gap.min(left));
        if gap >= left {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "Modbus RTU deadline before inter-frame gap",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::{Duration, ErrorKind, Instant, wait_for_gap};

        #[test]
        fn rtu_quiet_gap_timeout_is_reported_at_the_poll_deadline() {
            let deadline = Instant::now() + Duration::from_millis(30);
            let error = wait_for_gap(Duration::from_secs(1), deadline).unwrap_err();
            assert!(Instant::now() >= deadline);
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{Parity, RtuConfig};
    use super::{SerialSignal, Signal, TcpConfig, crc, rtu, tcp, validate};
    use crate::types::Categorized;
    use serde_json::{Value, json};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::Duration;

    fn signal(value: Value) -> Signal {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn decode_signed_scaled_swapped_float_and_coil() {
        let signed =
            signal(json!({"name":"temperature", "address":0, "data_type":"i16", "scale":0.1}));
        assert_eq!(
            signed.decode(&[3, 2, 0xff, 0x9c]).unwrap().value,
            json!(-10.0)
        );
        let swapped = signal(
            json!({"name":"pressure", "address":8, "data_type":"f32", "word_order":"little"}),
        );
        assert_eq!(swapped.request().unwrap(), [3, 0, 8, 0, 2]);
        assert_eq!(
            swapped.decode(&[3, 4, 0, 0, 0x3f, 0xc0]).unwrap().value,
            json!(1.5)
        );
        let coil =
            signal(json!({"name":"running", "address":0, "table":"coil", "data_type":"bool"}));
        assert_eq!(coil.decode(&[1, 1, 1]).unwrap().value, json!(true));
        coil.decode(&[1, 1, 3]).unwrap_err();
        swapped.decode(&[3, 4, 0, 0, 0x7f, 0x80]).unwrap_err();
    }

    #[test]
    fn decodes_all_register_and_byte_orders_without_rounding_wide_integers() {
        // Four distinct register values expose both register reversal and byte
        // swapping, including layouts used for 64-bit counters and meter values.
        let layouts = [
            (
                "big",
                "big",
                [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
            ),
            (
                "little",
                "big",
                [0xcd, 0xef, 0x89, 0xab, 0x45, 0x67, 0x01, 0x23],
            ),
            (
                "big",
                "little",
                [0x23, 0x01, 0x67, 0x45, 0xab, 0x89, 0xef, 0xcd],
            ),
            (
                "little",
                "little",
                [0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01],
            ),
        ];
        for (word_order, byte_order, data) in layouts {
            let unsigned = signal(json!({
                "name":"counter", "address":65532, "data_type":"u64",
                "word_order":word_order, "byte_order":byte_order
            }));
            validate(std::iter::once(&unsigned)).unwrap();
            assert_eq!(unsigned.request().unwrap(), [3, 0xff, 0xfc, 0, 4]);
            let mut response = vec![3, 8];
            response.extend_from_slice(&data);
            assert_eq!(
                unsigned.decode(&response).unwrap().value,
                json!(0x0123_4567_89ab_cdef_u64)
            );
            // The same four layouts work for two-register values too.
            let short = signal(json!({
                "name":"counter", "address":0, "data_type":"u32",
                "word_order":word_order, "byte_order":byte_order
            }));
            let short_data = if word_order == "big" {
                &data[..4]
            } else {
                &data[4..]
            };
            let mut response = vec![3, 4];
            response.extend_from_slice(short_data);
            assert_eq!(short.decode(&response).unwrap().value, json!(19_088_743.0));
        }
        for (kind, bytes, expected) in [
            ("u64", u64::MAX.to_be_bytes(), json!(u64::MAX)),
            ("i64", (i64::MIN + 1).to_be_bytes(), json!(i64::MIN + 1)),
            ("i64", i64::MAX.to_be_bytes(), json!(i64::MAX)),
            ("f64", (-7.25_f64).to_be_bytes(), json!(-7.25)),
        ] {
            let wide = signal(json!({"name":"wide", "address":0, "data_type":kind}));
            let mut response = vec![3, 8];
            response.extend_from_slice(&bytes);
            assert_eq!(wide.decode(&response).unwrap().value, expected);
            wide.decode(&response[..8]).unwrap_err();
        }
        let swapped = signal(json!({
            "name":"signed", "address":0, "data_type":"i16", "byte_order":"little"
        }));
        assert_eq!(
            swapped.decode(&[3, 2, 0x9c, 0xff]).unwrap().value,
            json!(-100.0)
        );
    }

    #[test]
    fn wide_numeric_types_validate_bounds_scaling_and_finite_values() {
        for kind in ["u64", "i64", "f64"] {
            let overflow = signal(json!({"name":"bad", "address":65533, "data_type":kind}));
            validate(std::iter::once(&overflow)).unwrap_err();
        }
        for kind in ["u64", "i64"] {
            for options in [(0.1, 0.0), (1.0, 1.0)] {
                let scaled = signal(json!({
                    "name":"bad", "address":0, "data_type":kind,
                    "scale":options.0, "offset":options.1
                }));
                validate(std::iter::once(&scaled)).unwrap_err();
            }
        }
        let scaled = signal(json!({
            "name":"energy", "address":0, "data_type":"f64", "scale":0.5, "offset":2.0
        }));
        validate(std::iter::once(&scaled)).unwrap();
        for (value, expected) in [
            (12.5_f64, Some(json!(8.25))),
            (f64::NAN, None),
            (f64::INFINITY, None),
        ] {
            let mut response = vec![3, 8];
            response.extend_from_slice(&value.to_be_bytes());
            let decoded = scaled.decode(&response);
            if let Some(expected) = expected {
                assert_eq!(decoded.unwrap().value, expected);
            } else {
                decoded.unwrap_err();
            }
        }
    }

    #[test]
    fn validate_address_and_signal_semantics() {
        let overflowing = signal(json!({"name":"bad", "address":65535, "data_type":"u32"}));
        assert!(validate(std::iter::once(&overflowing)).is_err());
        let wrong_type = signal(json!({"name":"bad", "address":0, "table":"coil"}));
        assert!(validate(std::iter::once(&wrong_type)).is_err());
        let valid = signal(json!({"name":"ok", "address":65535}));
        assert!(validate([valid.clone(), valid].iter()).is_err());
        let config = json!({"address":"127.0.0.1:502", "unit":255, "signals":[{"name":"value", "address":0}]});
        // Native TCP devices can use unit 255 or 0; the serial-only range
        // restriction must not be imposed on their unit identifier.
        drop(tcp(&serde_json::from_value(config).unwrap()).unwrap());
        let invalid_rtu = serde_json::from_value(json!({
            "device":"unused", "unit":0, "signals":[{"name":"value", "address":0}]
        }))
        .unwrap();
        assert_eq!(
            rtu(&invalid_rtu)
                .err()
                .unwrap()
                .downcast_ref::<crate::types::Usage>()
                .unwrap()
                .0,
            "Modbus RTU unit must be 1..247 (broadcasts cannot be read)"
        );
    }

    #[test]
    fn tcp_fixture_validates_requests_and_fragmented_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 12];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request, [0, 1, 0, 0, 0, 6, 7, 4, 0, 15, 0, 1]);
            stream.write_all(&[0, 1, 0, 0, 0]).unwrap();
            stream.write_all(&[5, 7, 4, 2, 0x12, 0x34]).unwrap();
        });
        let mut source = tcp(&TcpConfig {
            address,
            unit: 7,
            signals: vec![SerialSignal {
                unit: None,
                signal: signal(json!({"name":"counter", "address":15, "table":"input"})),
            }],
            tls: None,
        })
        .unwrap();
        let readings = source.read(Duration::from_secs(1)).unwrap();
        assert_eq!(readings[0].value, json!(4660.0));
        server.join().unwrap();
    }

    #[test]
    fn tcp_rejects_wrong_transaction_and_discards_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            for transaction in [99_u8, 2] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 12];
                stream.read_exact(&mut request).unwrap();
                stream
                    .write_all(&[0, transaction, 0, 0, 0, 5, 1, 3, 2, 0, 42])
                    .unwrap();
            }
        });
        let mut source = tcp(&TcpConfig {
            address,
            unit: 1,
            signals: vec![SerialSignal {
                unit: None,
                signal: signal(json!({"name":"value", "address":0})),
            }],
            tls: None,
        })
        .unwrap();
        source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            source.read(Duration::from_secs(1)).unwrap()[0].value,
            json!(42.0)
        );
        server.join().unwrap();
    }

    #[test]
    fn tcp_uses_one_deadline_for_a_trickling_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 12];
            stream.read_exact(&mut request).unwrap();
            for byte in [0, 1, 0, 0, 0, 5, 1, 3, 2, 0, 42] {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let mut source = tcp(&TcpConfig {
            address,
            unit: 1,
            signals: vec![SerialSignal {
                unit: None,
                signal: signal(json!({"name":"value", "address":0})),
            }],
            tls: None,
        })
        .unwrap();
        let error = source.read(Duration::from_millis(60)).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Timeout
        );
        server.join().unwrap();
    }

    #[test]
    fn exceptions_and_crc() {
        let value = signal(json!({"name":"value", "address":0}));
        let error = value.decode(&[0x83, 2]).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<Categorized>()
                .unwrap()
                .details
                .as_ref()
                .unwrap()["exception"],
            2
        );
        value.decode(&[3, 4, 0, 1]).unwrap_err();
        assert_eq!(crc(&[1, 3, 0, 0, 0, 10]), 0xcdc5);
    }

    #[cfg(unix)]
    #[test]
    fn rtu_reads_a_serial_frame_and_rejects_corrupt_crc() {
        use std::ffi::CStr;
        use std::os::fd::FromRawFd as _;

        let mut master = -1;
        let mut slave = -1;
        let mut name: [libc::c_char; 256] = [0; 256];
        // SAFETY: both fd outputs and the name buffer have sufficient space;
        // null termios/winsize request the platform's defaults.
        assert_eq!(
            // SAFETY: output allocations and optional null settings are valid.
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty returned two new descriptors owned by this test.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        // SAFETY: this is the other new descriptor from openpty.
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        // SAFETY: successful openpty wrote a NUL-terminated device path.
        let device = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        let mut source = rtu(&RtuConfig {
            device,
            baud: 19_200,
            parity: Parity::None,
            stop_bits: 2,
            unit: 3,
            signals: vec![SerialSignal {
                unit: None,
                signal: signal(json!({"name":"value", "address":0})),
            }],
        })
        .unwrap();
        let server = std::thread::spawn(move || {
            for valid in [true, false] {
                let mut request = [0_u8; 8];
                master.read_exact(&mut request).unwrap();
                assert_eq!(&request[..6], &[3, 3, 0, 0, 0, 1]);
                assert_eq!(
                    crc(&request[..6]),
                    u16::from_le_bytes([request[6], request[7]])
                );
                let mut response = vec![3, 3, 2, 0, 21];
                let checksum = if valid { crc(&response) } else { 0 };
                response.extend_from_slice(&checksum.to_le_bytes());
                master.write_all(&response).unwrap();
            }
            // Keep both PTY descriptors alive until both responses are read.
            (master, slave)
        });
        assert_eq!(
            source.read(Duration::from_secs(1)).unwrap()[0].value,
            json!(21.0)
        );
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("CRC"));
        drop(server.join().unwrap());
    }
}
