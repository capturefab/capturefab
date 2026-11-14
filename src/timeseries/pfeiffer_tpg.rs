//! Read-only TPG 361/362/366 mnemonic queries on a dedicated command interface.
//! Pfeiffer BG 5510 BEN/D (2018-08) and BG 5511 BEN/A (2022-07) describe
//! command CR -> ACK CRLF -> ENQ -> data CRLF. This is not PV telegram framing.
use super::{Reading, Source, net, serial};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Number, Value, json};
use std::{
    collections::BTreeSet,
    io,
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const MAX_REPLY: usize = 256;
const MAX_STARTUP: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    model: Model,
    address: Option<SocketAddr>,
    serial: Option<SerialConfig>,
    signals: Vec<Signal>,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Model {
    Tpg361,
    Tpg362,
    Tpg366,
}
impl Model {
    const fn identity(self) -> &'static str {
        match self {
            Self::Tpg361 => "TPG361",
            Self::Tpg362 => "TPG362",
            Self::Tpg366 => "TPG366",
        }
    }
    const fn channels(self) -> u8 {
        match self {
            Self::Tpg361 => 1,
            Self::Tpg362 => 2,
            Self::Tpg366 => 6,
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SerialConfig {
    device: String,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
}
const fn baud() -> u32 {
    9600
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    channel: u8,
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.signals.is_empty() || config.signals.len() > usize::from(config.model.channels()) {
        return Err(
            Usage("TPG requires 1–6 unique channels within the selected model".into()).into(),
        );
    }
    let mut names = BTreeSet::new();
    let mut channels = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "TPG signal")?;
        if signal.channel == 0
            || signal.channel > config.model.channels()
            || !names.insert(&signal.name)
            || !channels.insert(signal.channel)
        {
            return Err(Usage(
                "TPG signal names/channels must be unique and within the selected model".into(),
            )
            .into());
        }
    }
    let connection = match (config.address, config.serial.as_ref()) {
        (Some(address), None) if address.port() != 0 && !address.ip().is_unspecified() => {
            Connection::Tcp {
                address,
                socket: None,
            }
        }
        (None, Some(config)) => Connection::serial(config)?,
        _ => {
            return Err(Usage(
                "TPG requires exactly one numeric TCP endpoint or USB serial device".into(),
            )
            .into());
        }
    };
    Ok(Box::new(Client {
        connection: Some(connection),
        config: config.clone(),
        identity: None,
        startup: true,
        startup_bytes: 0,
    }))
}

enum Connection {
    Tcp {
        address: SocketAddr,
        socket: Option<Box<dyn net::Transport>>,
    },
    #[cfg(any(unix, windows))]
    Serial(serial::Port),
}
impl Connection {
    fn serial(config: &SerialConfig) -> Result<Self> {
        if config.device.is_empty()
            || !matches!(config.baud, 9600 | 19_200 | 38_400 | 57_600 | 115_200)
        {
            return Err(Usage(
                "TPG USB serial requires a device and a documented 9600–115200 baud rate".into(),
            )
            .into());
        }
        #[cfg(any(unix, windows))]
        {
            Ok(Self::Serial(serial::open(
                &config.device,
                config.baud,
                serial::Parity::None,
                1,
                8,
            )?))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(Categorized::error(
                ErrorKind::Unsupported,
                "TPG serial requires Unix terminals or Windows COM ports",
            ))
        }
    }
    fn connect(&mut self, deadline: Instant) -> Result<()> {
        if let Self::Tcp { address, socket } = self
            && socket.is_none()
        {
            let stream = TcpStream::connect_timeout(address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            stream.set_nodelay(true).map_err(|error| io_error(&error))?;
            *socket = Some(net::plain(stream, deadline));
        }
        remaining(deadline)?;
        Ok(())
    }
    fn write(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        match self {
            Self::Tcp { socket, .. } => {
                let socket = socket.as_deref_mut().ok_or_else(closed)?;
                socket.set_deadline(deadline);
                socket.write_all(bytes).map_err(|error| io_error(&error))?;
            }
            #[cfg(any(unix, windows))]
            Self::Serial(port) => port
                .write_all(bytes, deadline)
                .map_err(|error| io_error(&error))?,
        }
        remaining(deadline)?;
        Ok(())
    }
    fn byte(&mut self, deadline: Instant) -> Result<u8> {
        loop {
            remaining(deadline)?;
            let mut byte = [0];
            let result = match self {
                Self::Tcp { socket, .. } => {
                    let socket = socket.as_deref_mut().ok_or_else(closed)?;
                    socket.set_deadline(deadline);
                    socket.read(&mut byte)
                }
                #[cfg(any(unix, windows))]
                Self::Serial(port) => port.read_exact(&mut byte, deadline).map(|()| 1),
            };
            match result {
                Ok(0) => {
                    remaining(deadline)?;
                    return Err(closed());
                }
                Ok(_) => {
                    remaining(deadline)?;
                    return Ok(byte[0]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) => return Err(io_error(&error)),
            }
        }
    }
    fn query(
        &mut self,
        command: [u8; 3],
        startup: &mut bool,
        discarded: &mut usize,
        deadline: Instant,
    ) -> Result<String> {
        self.write(&command, deadline)?;
        // CR alone is valid on all scoped interfaces. LF can collide on RS-485.
        self.write(b"\r", deadline)?;
        loop {
            match self.byte(deadline)? {
                first @ (0x06 | 0x15) => {
                    if self.byte(deadline)? != b'\r' || self.byte(deadline)? != b'\n' {
                        return Err(malformed("acknowledgement must end with CRLF"));
                    }
                    *startup = false;
                    if first == 0x15 {
                        return Err(Categorized::error(
                            ErrorKind::OperationFailed,
                            "TPG rejected the read-only mnemonic",
                        ));
                    }
                    break;
                }
                byte if *startup
                    && (byte.is_ascii_graphic() || matches!(byte, b' ' | b'\r' | b'\n')) =>
                {
                    *discarded += 1;
                    if *discarded > MAX_STARTUP {
                        return Err(malformed("startup output exceeds the 4096-byte bound"));
                    }
                }
                _ => return Err(malformed("unexpected byte before acknowledgement")),
            }
        }
        self.write(&[0x05], deadline)?;
        let mut bytes = Vec::new();
        loop {
            let byte = self.byte(deadline)?;
            if byte == b'\r' {
                if self.byte(deadline)? != b'\n' {
                    return Err(malformed("data must end with CRLF"));
                }
                remaining(deadline)?;
                return String::from_utf8(bytes).map_err(|_| malformed("data is not ASCII"));
            }
            if !byte.is_ascii_graphic() && byte != b' ' {
                return Err(malformed("unexpected control or non-ASCII data byte"));
            }
            if bytes.len() == MAX_REPLY {
                return Err(malformed("reply exceeds the 256-byte bound"));
            }
            bytes.push(byte);
        }
    }
}

struct Client {
    connection: Option<Connection>,
    config: Config,
    identity: Option<Value>,
    startup: bool,
    startup_bytes: usize,
}
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let connection = self.connection.as_mut().ok_or_else(closed)?;
        connection.connect(deadline)?;
        if self.identity.is_none() {
            let raw = connection.query(
                *b"AYT",
                &mut self.startup,
                &mut self.startup_bytes,
                deadline,
            )?;
            self.identity = Some(identity(self.config.model, &raw)?);
        }
        let before = connection.query(
            *b"UNI",
            &mut self.startup,
            &mut self.startup_bytes,
            deadline,
        )?;
        let reported_unit = unit(&before)?;
        let mut readings = Vec::with_capacity(self.config.signals.len());
        for signal in &self.config.signals {
            let request = [b'P', b'R', b'0' + signal.channel];
            let raw = connection.query(
                request,
                &mut self.startup,
                &mut self.startup_bytes,
                deadline,
            )?;
            let mut value = measurement(signal.channel, &raw, reported_unit)?;
            value["device"] = self.identity.as_ref().ok_or_else(closed)?.clone();
            value["startup_bytes_discarded"] = json!(self.startup_bytes);
            readings.push(Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            });
        }
        let after = connection.query(
            *b"UNI",
            &mut self.startup,
            &mut self.startup_bytes,
            deadline,
        )?;
        unit(&after)?;
        if before != after {
            return Err(malformed(
                "reported unit changed during the poll; no sibling was published",
            ));
        }
        remaining(deadline)?;
        Ok(readings)
    }
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("TPG timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.connection = None;
        }
        result
    }
}

fn identity(model: Model, raw: &str) -> Result<Value> {
    let fields = raw.split(',').collect::<Vec<_>>();
    let [name, part, serial, firmware, hardware] = fields.as_slice() else {
        return Err(malformed("AYT must return five identification fields"));
    };
    if fields.iter().any(|field| field.is_empty()) {
        return Err(malformed("AYT identification fields cannot be empty"));
    }
    if *name != model.identity() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "TPG identity does not match the explicit model",
        ));
    }
    Ok(
        json!({"model":name,"part_number":part,"serial_number":serial,
        "firmware_raw":firmware,"hardware_raw":hardware,"raw":raw}),
    )
}
fn unit(raw: &str) -> Result<(u8, &'static str)> {
    match raw {
        "0" => Ok((0, "mbar/bar")),
        "1" => Ok((1, "Torr")),
        "2" => Ok((2, "Pascal")),
        "3" => Ok((3, "Micron")),
        "4" => Ok((4, "hPascal")),
        "5" => Ok((5, "Volt")),
        _ => Err(malformed("UNI returned an unknown unit code")),
    }
}
fn decimal(raw: &str) -> Result<Number> {
    let unsigned = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    let Some((mantissa, exponent)) = unsigned.split_once('E') else {
        return Err(malformed(
            "measurement must use the documented exponential format",
        ));
    };
    let Some((integer, fraction)) = mantissa.split_once('.') else {
        return Err(malformed("measurement mantissa is incomplete"));
    };
    let Some(exponent) = exponent.strip_prefix(['+', '-']) else {
        return Err(malformed("measurement exponent must have a sign"));
    };
    // The PRn table prints two exponent digits; its no-sensor literal uses one.
    if integer.len() != 1
        || fraction.len() != 4
        || !(1..=2).contains(&exponent.len())
        || !integer
            .bytes()
            .chain(fraction.bytes())
            .chain(exponent.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(malformed("measurement has invalid decimal field widths"));
    }
    raw.strip_prefix('+')
        .unwrap_or(raw)
        .parse::<Number>()
        .map_err(|_| malformed("measurement decimal is invalid"))
}
fn measurement(channel: u8, raw: &str, unit: (u8, &'static str)) -> Result<Value> {
    let Some((status, reading)) = raw.split_once(',') else {
        return Err(malformed("PRn requires status and measurement fields"));
    };
    let (code, status) = match status {
        "0" => (0, "ok"),
        "1" => (1, "underrange"),
        "2" => (2, "overrange"),
        "3" => (3, "sensor_error"),
        "4" => (4, "sensor_off"),
        "5" => (5, "no_sensor"),
        "6" => (6, "identification_error"),
        _ => return Err(malformed("PRn returned an unknown measurement status")),
    };
    let number = decimal(reading)?;
    let mut value = json!({"kind":"pfeiffer_tpg_measurement","channel":channel,
        "status_code":code,"status":status,"reading_raw":reading,"raw":raw,
        "reported_unit_code":unit.0,"reported_unit":unit.1});
    if code == 0 {
        value["value"] = Value::Number(number);
    }
    Ok(value)
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("TPG: {message}"))
}
fn closed() -> anyhow::Error {
    Categorized::error(ErrorKind::Disconnected, "TPG command channel closed")
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "TPG poll deadline expired",
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
    Categorized::error(kind, format!("TPG I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_no_sensor_literal_preserves_raw_without_a_valid_numeric_measurement() {
        // BG 5510 BEN/D and BG 5511 BEN/A §1.5.4 print this exact placeholder.
        let value = measurement(1, "5,2.0000E-2", (4, "hPascal")).unwrap();
        assert_eq!(value["status"], "no_sensor");
        assert_eq!(value["status_code"], 5);
        assert_eq!(value["reading_raw"], "2.0000E-2");
        assert_eq!(value["raw"], "5,2.0000E-2");
        assert_eq!(value["reported_unit_code"], 4);
        assert_eq!(value.get("value"), None);
        for (code, status) in [
            (1, "underrange"),
            (2, "overrange"),
            (3, "sensor_error"),
            (4, "sensor_off"),
            (6, "identification_error"),
        ] {
            let value = measurement(2, &format!("{code},-1.0000E+00"), (5, "Volt")).unwrap();
            assert_eq!(value["status"], status);
            assert_eq!(value.get("value"), None);
            assert_eq!(value["reading_raw"], "-1.0000E+00");
        }
    }

    #[test]
    fn all_reported_units_and_exact_signed_exponents_are_retained_without_conversion() {
        for (raw, label) in [
            ("0", "mbar/bar"),
            ("1", "Torr"),
            ("2", "Pascal"),
            ("3", "Micron"),
            ("4", "hPascal"),
            ("5", "Volt"),
        ] {
            let value = measurement(6, "0,+9.9999E+99", unit(raw).unwrap()).unwrap();
            assert_eq!(value["reported_unit"], label);
            assert_eq!(value["reading_raw"], "+9.9999E+99");
            assert_eq!(
                value["value"],
                json!("9.9999E+99".parse::<Number>().unwrap())
            );
        }
        for raw in ["0,-0.0000E-00", "0,1.2345E-06", "0,-1.2345E+06"] {
            let value = measurement(1, raw, (0, "mbar/bar")).unwrap();
            assert_eq!(value["raw"], raw);
            assert!(value["value"].is_number());
        }
        for raw in ["6", "00", " 0", "0,1", ""] {
            unit(raw).unwrap_err();
        }
        for raw in [
            "7,1.0000E+00",
            "00,1.0000E+00",
            "0,NaN",
            "0,1.000E+00",
            "0,1.00000E+00",
            "0,11.0000E+00",
            "0,1.0000E00",
            "0,1.0000E+000",
            "0,1.0000e+00",
            "0,1.0000E+00,0",
            "0, 1.0000E+00",
            "0,+-1.0000E+00",
        ] {
            measurement(1, raw, (0, "mbar/bar")).unwrap_err();
        }
    }

    #[test]
    fn model_identity_and_channels_are_explicit_preconnection_boundaries() {
        let identity = identity(Model::Tpg362, "TPG362,PTG28290,44990000,010100,010100").unwrap();
        assert_eq!(identity["firmware_raw"], "010100");
        for raw in [
            "TPG366,PTG28770,44990000,010100,010100",
            "TPG362,a,b,c",
            "TPG362,a,b,c,d,e",
            "TPG362,,b,c,d",
            "tpg362,a,b,c,d",
        ] {
            super::identity(Model::Tpg362, raw).unwrap_err();
        }
        for model in [Model::Tpg361, Model::Tpg362, Model::Tpg366] {
            let mut config = Config {
                model,
                address: Some("127.0.0.1:8000".parse().unwrap()),
                serial: None,
                signals: vec![Signal {
                    name: "gauge".into(),
                    channel: model.channels(),
                }],
            };
            drop(open(&config).unwrap());
            config.signals.first_mut().unwrap().channel = model.channels() + 1;
            let error = open(&config).map(drop).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
            config.signals.first_mut().unwrap().channel = 0;
            let error = open(&config).map(drop).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        }
    }
}
