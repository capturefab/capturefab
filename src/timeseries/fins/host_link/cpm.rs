//! Explicit legacy CPM C-mode reads from Omron W353-E1-06 sections 4-2 through 4-5.
//!
//! Model/ranges are configured, not detected. All replies are complete independent
//! frames; no FINS aliases, continuation, timebase or BCD interpretation is inferred.

use super::super::{Area as FinsArea, ByteOrder, DataType, Signal as WordView, WordOrder};
use crate::{
    timeseries::{
        Reading, Source,
        serial::{self, SerialSignal},
    },
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Write as _, time::Instant};

const MAX_BATCH: usize = 1 << 20;
const FRAME_BYTES: usize = 131;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    CModeCpm,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Model {
    Cpm1,
    Cpm1a,
    Cpm2a,
    Cpm2c,
    Srm1,
    Srm1V2,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Area {
    IrSr,
    Lr,
    Hr,
    Dm,
    Ar,
    TcPv,
    TcStatus,
}

impl Area {
    const fn command(self) -> &'static str {
        match self {
            Self::IrSr => "RR",
            Self::Lr => "RL",
            Self::Hr => "RH",
            Self::Dm => "RD",
            Self::Ar => "RJ",
            Self::TcPv => "RC",
            Self::TcStatus => "RG",
        }
    }
    const fn flags(self) -> bool {
        matches!(self, Self::TcStatus)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(rename = "mode")]
    _mode: Mode,
    model: Model,
    device: String,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    unit: u8,
    #[serde(
        default = "super::baud",
        deserialize_with = "crate::config_number::unsigned"
    )]
    baud: u32,
    #[serde(default)]
    parity: serial::Parity,
    #[serde(
        default = "super::stop_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    stop_bits: u8,
    #[serde(
        default = "super::data_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    data_bits: u8,
    signals: Vec<SerialSignal<Signal>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    area: Area,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    address: u16,
    #[serde(default)]
    data_type: DataType,
    #[serde(
        default = "super::super::one",
        deserialize_with = "crate::config_number::unsigned"
    )]
    elements: u16,
    #[serde(default)]
    word_order: WordOrder,
    #[serde(default)]
    byte_order: ByteOrder,
    #[serde(default = "super::super::scale")]
    scale: f64,
    #[serde(default)]
    offset: f64,
}

impl Model {
    fn contains(self, area: Area, start: u16, count: u16) -> bool {
        let end = u32::from(start) + u32::from(count);
        let newer = matches!(self, Self::Cpm2a | Self::Cpm2c);
        let interval = |first: u16, last: u16| start >= first && end <= u32::from(last) + 1;
        count != 0
            && match area {
                Area::IrSr => interval(0, if newer { 49 } else { 19 }) || interval(200, 255),
                Area::Lr => interval(0, 15),
                Area::Hr => interval(0, 19),
                Area::Dm => {
                    interval(
                        0,
                        if matches!(self, Self::Cpm1 | Self::Cpm1a) {
                            1023
                        } else {
                            2047
                        },
                    ) || interval(6144, 6655)
                }
                Area::Ar => interval(0, if newer { 23 } else { 15 }),
                Area::TcPv | Area::TcStatus => interval(0, if newer { 255 } else { 127 }),
            }
    }
}

impl Config {
    fn validate(&self) -> Result<usize> {
        if self.device.is_empty()
            || self.device.contains('\0')
            || self.unit > 31
            || !(1..=256).contains(&self.signals.len())
        {
            return Err(Usage(
                "CPM Host Link requires a nonempty device, unit 0–31 and 1–256 signals".into(),
            )
            .into());
        }
        if !matches!(self.baud, 1200 | 2400 | 4800 | 9600 | 19_200)
            || !matches!(self.data_bits, 7 | 8)
            || !matches!(self.stop_bits, 1 | 2)
        {
            return Err(Usage("CPM Host Link supports baud 1200/2400/4800/9600/19200, 7/8 data bits and 1/2 stop bits".into()).into());
        }
        serial::validate(self.baud, self.stop_bits, self.data_bits)?;
        let mut names = BTreeSet::new();
        let mut minimum = 2_usize;
        for (index, entry) in self.signals.iter().enumerate() {
            let signal = &entry.signal;
            crate::timeseries::validate_name(&signal.name, "CPM Host Link signal")?;
            if !names.insert(&signal.name) || entry.unit(self.unit) > 31 {
                return Err(Usage(
                    "CPM Host Link names must be unique and units must be 0–31".into(),
                )
                .into());
            }
            signal.validate(self.model)?;
            let shell = serde_json::to_vec(&Reading {
                signal: signal.name.clone(),
                value: Value::Null,
                source_time: None,
            })?
            .len()
                - 4;
            let values = usize::from(signal.elements) * signal.minimum_scalar();
            let punctuation = if signal.elements == 1 {
                0
            } else {
                usize::from(signal.elements) + 1
            };
            minimum += shell + values + punctuation + usize::from(index != 0);
            if minimum > MAX_BATCH {
                return Err(
                    Usage("CPM Host Link minimum serialized poll exceeds 1 MiB".into()).into(),
                );
            }
        }
        Ok(minimum)
    }
}

impl Signal {
    fn words(&self) -> u16 {
        self.data_type.words().max(1)
    }
    const fn minimum_scalar(&self) -> usize {
        if self.area.flags() { 4 } else { 1 }
    }
    fn validate(&self, model: Model) -> Result<()> {
        let words = self.elements.checked_mul(self.words());
        if !(1..=4096).contains(&self.elements)
            || words.is_none_or(|count| !model.contains(self.area, self.address, count))
        {
            return Err(Usage(
                "CPM Host Link complete signal must fit one documented model/area interval".into(),
            )
            .into());
        }
        if self.area.flags() != matches!(self.data_type, DataType::Bool) {
            return Err(Usage(
                "CPM Host Link tc_status requires bool; word areas require numeric data_type"
                    .into(),
            )
            .into());
        }
        if !self.scale.is_finite()
            || !self.offset.is_finite()
            || (matches!(
                self.data_type,
                DataType::Bool | DataType::U64 | DataType::I64
            ) && (self.scale != 1.0 || self.offset != 0.0))
        {
            return Err(Usage("CPM Host Link scale/offset must be finite; bool and 64-bit integers require identity scaling".into()).into());
        }
        if self.area.flags()
            && (!matches!(self.word_order, WordOrder::Little)
                || !matches!(self.byte_order, ByteOrder::Big))
        {
            return Err(Usage(
                "CPM Host Link tc_status does not accept numeric byte/word order overrides".into(),
            )
            .into());
        }
        Ok(())
    }
    fn scalar(&self, bytes: &[u8]) -> Result<Value> {
        // This is only a private scalar codec view: its Area/address never enter
        // a FINS command or validation path. CPM planning owns every native span.
        WordView {
            name: String::new(),
            area: FinsArea::Dm,
            bank: None,
            address: 0,
            bit: None,
            data_type: self.data_type,
            elements: 1,
            word_order: self.word_order,
            byte_order: self.byte_order,
            scale: self.scale,
            offset: self.offset,
        }
        .scalar(bytes)
    }
}

struct Budget {
    bytes: usize,
}
impl Budget {
    fn charge(&mut self, value: &Value, minimum: usize) -> Result<()> {
        let actual = serde_json::to_vec(value)?.len();
        self.bytes = self.bytes.saturating_add(actual - minimum);
        if self.bytes > MAX_BATCH {
            return Err(protocol("serialized poll exceeds 1 MiB"));
        }
        Ok(())
    }
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("CPM Host Link: {message}"))
}
fn check(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "CPM Host Link original read deadline expired",
        ));
    }
    Ok(())
}
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, byte| sum ^ byte)
}
fn digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(protocol("invalid hexadecimal digit")),
    }
}
fn hex_byte([high, low]: [u8; 2]) -> Result<u8> {
    Ok((digit(high)? << 4) | digit(low)?)
}

fn request(unit: u8, area: Area, address: u16, count: u16) -> Result<[u8; 17]> {
    let mut text = format!("@{unit:02}{}{address:04}{count:04}", area.command());
    let fcs = checksum(text.as_bytes());
    write!(text, "{fcs:02X}*\r")?;
    text.into_bytes()
        .try_into()
        .map_err(|_| protocol("request extent differs from 17 bytes"))
}

fn payload<'a>(frame: &'a [u8], request: &[u8; 17], count: u16, flags: bool) -> Result<&'a [u8]> {
    if frame.len() > FRAME_BYTES {
        return Err(protocol("response exceeds complete-frame bound"));
    }
    let body = frame
        .strip_suffix(b"*\r")
        .ok_or_else(|| protocol("expected complete *CR frame"))?;
    let (body, &fcs) = body
        .split_last_chunk::<2>()
        .ok_or_else(|| protocol("missing response FCS"))?;
    if hex_byte(fcs)? != checksum(body) {
        return Err(protocol("response FCS mismatch"));
    }
    let rest = body
        .strip_prefix(&request[..5])
        .ok_or_else(|| protocol("response unit/header differs"))?;
    let (&status, data) = rest
        .split_first_chunk::<2>()
        .ok_or_else(|| protocol("missing end code"))?;
    let status = hex_byte(status)?;
    if status != 0 {
        if !data.is_empty() {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "CPM Host Link nonzero end code has unexpected data",
            )
            .details(json!({"host_link_end_code":status}))
            .into());
        }
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            "CPM Host Link target rejected request",
        )
        .details(json!({"host_link_end_code":status}))
        .into());
    }
    if data.len() != usize::from(count) * if flags { 1 } else { 4 } {
        return Err(protocol("response native count differs"));
    }
    Ok(data)
}

pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
    let minimum = config.validate()?;
    #[cfg(any(unix, windows))]
    {
        Ok(Box::new(wire::Client::new(config.clone(), minimum)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = minimum;
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "CPM Host Link requires a native serial port",
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{
        Budget, Categorized, Config, ErrorKind, FRAME_BYTES, Instant, Reading, Result, Signal,
        Source, Usage, Value, check, hex_byte, json, payload, protocol, request, serial,
    };
    use std::{io, time::Duration};

    pub(super) struct Client {
        config: Config,
        minimum: usize,
        port: Option<serial::Port>,
        retired: bool,
    }
    impl Client {
        pub(super) const fn new(config: Config, minimum: usize) -> Self {
            Self {
                config,
                minimum,
                port: None,
                retired: false,
            }
        }
        fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
            check(deadline)?;
            if self.port.is_none() {
                let c = &self.config;
                let port = serial::open(&c.device, c.baud, c.parity, c.stop_bits, c.data_bits)?;
                check(deadline)?;
                port.discard_input().map_err(|error| io_error(&error))?;
                check(deadline)?;
                self.port = Some(port);
            }
            let port = self
                .port
                .as_mut()
                .ok_or_else(|| protocol("missing serial owner"))?;
            let mut budget = Budget {
                bytes: self.minimum,
            };
            let mut readings = Vec::with_capacity(self.config.signals.len());
            for entry in &self.config.signals {
                check(deadline)?;
                let value = read_signal(
                    port,
                    &entry.signal,
                    entry.unit(self.config.unit),
                    &mut budget,
                    deadline,
                )?;
                readings.push(Reading {
                    signal: entry.signal.name.clone(),
                    value,
                    source_time: None,
                });
                check(deadline)?;
            }
            check(deadline)?;
            Ok(readings)
        }
    }
    impl Source for Client {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            if self.retired {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "CPM Host Link owner is permanently retired",
                ));
            }
            let result = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| {
                    anyhow::Error::from(Usage("CPM Host Link timeout is too large".into()))
                })
                .and_then(|deadline| self.poll(deadline));
            if result.is_err() {
                self.port = None;
                self.retired = true;
            }
            result
        }
    }
    fn io_error(error: &io::Error) -> anyhow::Error {
        let kind = match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Disconnected,
        };
        Categorized::error(kind, format!("CPM Host Link I/O: {error}"))
    }
    fn receive(port: &mut serial::Port, deadline: Instant) -> Result<Vec<u8>> {
        let mut buffer = [0; FRAME_BYTES + 1];
        let mut length = 0;
        while length < buffer.len() {
            check(deadline)?;
            let count = port
                .receive(&mut buffer[length..], deadline)
                .map_err(|error| io_error(&error))?
                .ok_or_else(|| {
                    Categorized::error(ErrorKind::Timeout, "CPM Host Link response timed out")
                })?;
            check(deadline)?;
            if count == 0 {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "CPM Host Link serial device disconnected",
                ));
            }
            let end = length + count;
            if let Some(at) = buffer[length..end].iter().position(|&byte| byte == b'\r') {
                if at + 1 != count || end > FRAME_BYTES {
                    return Err(protocol(
                        "unexpected bytes after terminator or frame overflow",
                    ));
                }
                return Ok(buffer[..end].to_vec());
            }
            length = end;
        }
        Err(protocol("response exceeds complete-frame bound"))
    }
    fn read_signal(
        port: &mut serial::Port,
        signal: &Signal,
        unit: u8,
        budget: &mut Budget,
        deadline: Instant,
    ) -> Result<Value> {
        let mut values = Vec::with_capacity(usize::from(signal.elements));
        let flags = signal.area.flags();
        let maximum = if flags { 120 } else { 30 / signal.words() };
        let mut offset = 0;
        while offset < signal.elements {
            check(deadline)?;
            let elements = maximum.min(signal.elements - offset);
            let count = elements * signal.words();
            let address = signal
                .address
                .checked_add(offset * signal.words())
                .ok_or_else(|| protocol("native address overflow"))?;
            let command = request(unit, signal.area, address, count)?;
            check(deadline)?;
            port.write_all(&command, deadline)
                .map_err(|error| io_error(&error))?;
            check(deadline)?;
            let frame = receive(port, deadline)?;
            let payload = payload(&frame, &command, count, flags)?;
            if flags {
                for &byte in payload {
                    let value = match byte {
                        b'0' => json!(false),
                        b'1' => json!(true),
                        _ => return Err(protocol("RG flag is not ASCII 0 or 1")),
                    };
                    check(deadline)?;
                    budget.charge(&value, signal.minimum_scalar())?;
                    values.push(value);
                }
            } else {
                let bytes = payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .copied()
                    .map(hex_byte)
                    .collect::<Result<Vec<_>>>()?;
                for bytes in bytes.chunks_exact(usize::from(signal.words()) * 2) {
                    let value = signal.scalar(bytes)?;
                    check(deadline)?;
                    budget.charge(&value, signal.minimum_scalar())?;
                    values.push(value);
                }
            }
            check(deadline)?;
            offset += elements;
        }
        let value = if signal.elements == 1 {
            values.pop().ok_or_else(|| protocol("scalar missing"))?
        } else {
            Value::Array(values)
        };
        check(deadline)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::classify;
    use std::time::Duration;

    fn vectors() -> Value {
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/omron/cpm-literals.json"
        ))
        .unwrap()
    }
    fn source_config(signals: &Value) -> Config {
        serde_json::from_value(
            json!({"mode":"c_mode_cpm","model":"cpm2a","device":"must-not-open","signals":signals}),
        )
        .unwrap()
    }
    fn signal(value: Value) -> Signal {
        serde_json::from_value(value).unwrap()
    }
    fn area(command: &str) -> Area {
        match command {
            "RR" => Area::IrSr,
            "RL" => Area::Lr,
            "RH" => Area::Hr,
            "RD" => Area::Dm,
            "RJ" => Area::Ar,
            "RC" => Area::TcPv,
            "RG" => Area::TcStatus,
            _ => panic!("unexpected literal command"),
        }
    }

    #[test]
    fn complete_frames_match_independent_bytes_native_values_and_flag_extents() {
        for case in vectors()["cases"].as_array().unwrap() {
            let command = case["command"].as_str().unwrap();
            let request = request(
                u8::try_from(case["station_decimal"].as_u64().unwrap()).unwrap(),
                area(command),
                u16::try_from(case["start_decimal"].as_u64().unwrap()).unwrap(),
                u16::try_from(case["native_count_decimal"].as_u64().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(
                request.as_slice(),
                case["request_ascii"].as_str().unwrap().as_bytes()
            );
            let frame = case["response_ascii"].as_str().unwrap().as_bytes();
            let count = u16::try_from(case["native_count_decimal"].as_u64().unwrap()).unwrap();
            let decoded = payload(frame, &request, count, command == "RG");
            if case.get("host_link_end_code").is_some() {
                let error = classify(&decoded.unwrap_err());
                assert_eq!(error.kind, ErrorKind::OperationFailed);
                assert_eq!(error.details.unwrap()["host_link_end_code"], 20);
            } else if let Some(expected) = case.get("native_words_u16") {
                let words = decoded
                    .unwrap()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|bytes| {
                        u16::from_be_bytes([
                            hex_byte([bytes[0], bytes[1]]).unwrap(),
                            hex_byte([bytes[2], bytes[3]]).unwrap(),
                        ])
                    })
                    .collect::<Vec<_>>();
                assert_eq!(json!(words), *expected);
            } else {
                let flags = decoded
                    .unwrap()
                    .iter()
                    .map(|byte| match byte {
                        b'0' => false,
                        b'1' => true,
                        _ => panic!("invalid literal flag"),
                    })
                    .collect::<Vec<_>>();
                assert_eq!(json!(flags), case["native_completion_flags"]);
            }
        }
        for schedule in vectors()["multi_command_cases"].as_array().unwrap() {
            for (span, expected) in schedule["request_starts_counts"]
                .as_array()
                .unwrap()
                .iter()
                .zip(schedule["requests_ascii"].as_array().unwrap())
            {
                let command = if schedule["area"] == "tc_status" {
                    Area::TcStatus
                } else {
                    Area::IrSr
                };
                let actual = request(
                    0,
                    command,
                    u16::try_from(span[0].as_u64().unwrap()).unwrap(),
                    u16::try_from(span[1].as_u64().unwrap()).unwrap(),
                )
                .unwrap();
                assert_eq!(actual.as_slice(), expected.as_str().unwrap().as_bytes());
            }
        }
    }

    #[test]
    fn model_intervals_reject_holes_end_overflow_and_unproved_area_aliases() {
        for model in [
            Model::Cpm1,
            Model::Cpm1a,
            Model::Cpm2a,
            Model::Cpm2c,
            Model::Srm1,
            Model::Srm1V2,
        ] {
            let newer = matches!(model, Model::Cpm2a | Model::Cpm2c);
            assert!(model.contains(Area::IrSr, 200, 56));
            assert!(!model.contains(Area::IrSr, 199, 2));
            assert!(!model.contains(Area::IrSr, 19, 182));
            assert!(model.contains(Area::TcPv, 0, if newer { 256 } else { 128 }));
            assert!(!model.contains(Area::TcStatus, 127, if newer { 130 } else { 2 }));
            assert!(model.contains(Area::Dm, 6144, 512));
            assert!(!model.contains(Area::Dm, 6143, 2));
            assert!(!model.contains(Area::Dm, 6655, 2));
            assert!(model.contains(
                Area::Dm,
                if matches!(model, Model::Cpm1 | Model::Cpm1a) {
                    1023
                } else {
                    2047
                },
                1
            ));
            assert!(!model.contains(Area::Lr, 15, 2));
            assert!(!model.contains(Area::Hr, 19, 2));
            assert_eq!(model.contains(Area::Ar, 22, 2), newer);
            assert!(!model.contains(Area::IrSr, 0, 0));
        }
        for area in ["wr", "cio", "em", "sr", "timer", "counter"] {
            let error =
                serde_json::from_value::<Signal>(json!({"name":"invalid","area":area,"address":0}))
                    .unwrap_err();
            assert!(error.is_data());
        }
    }

    #[test]
    fn configured_numeric_views_reuse_existing_exact_scalar_and_order_contract() {
        for (kind, bytes, expected) in [
            ("u16", vec![0xff, 0xff], json!(u16::MAX)),
            ("i16", vec![0x80, 0], json!(i16::MIN)),
            ("u32", vec![0xff; 4], json!(u32::MAX)),
            ("i32", vec![0, 0, 0x80, 0], json!(i32::MIN)),
            ("u64", vec![0xff; 8], json!(u64::MAX)),
            ("i64", vec![0, 0, 0, 0, 0, 0, 0x80, 0], json!(i64::MIN)),
            ("f32", vec![0, 0, 0x3f, 0x80], json!(1.0)),
            ("f64", vec![0, 0, 0, 0, 0, 0, 0x3f, 0xf0], json!(1.0)),
        ] {
            let source = signal(json!({"name":"word","area":"dm","address":0,"data_type":kind}));
            source.validate(Model::Cpm2a).unwrap();
            assert_eq!(source.scalar(&bytes).unwrap(), expected);
        }
        let source = signal(
            json!({"name":"word","area":"dm","address":0,"data_type":"u32","word_order":"big","byte_order":"little"}),
        );
        assert_eq!(
            source.scalar(&[0x34, 0x12, 0x78, 0x56]).unwrap(),
            json!(0x1234_5678)
        );
        let source = signal(json!({"name":"word","area":"dm","address":0,"data_type":"f32"}));
        assert_eq!(
            classify(&source.scalar(&[0, 0, 0x7f, 0xc0]).unwrap_err()).kind,
            ErrorKind::OperationFailed
        );
    }

    #[test]
    fn exact_json_charge_includes_names_array_punctuation_and_actual_scalars() {
        let config =
            source_config(&json!([{"name":"quote\"name","area":"dm","address":0,"elements":3}]));
        let mut budget = Budget {
            bytes: config.validate().unwrap(),
        };
        for value in [json!(0), json!(65_535), json!(1)] {
            budget.charge(&value, 1).unwrap();
        }
        let reading = Reading {
            signal: "quote\"name".into(),
            value: json!([0, 65_535, 1]),
            source_time: None,
        };
        assert_eq!(budget.bytes, serde_json::to_vec(&[reading]).unwrap().len());
        let mut budget = Budget {
            bytes: MAX_BATCH - 1,
        };
        assert_eq!(
            classify(&budget.charge(&json!(65_535), 1).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        let signals = (0..256)
            .map(|n| json!({"name":format!("signal{n}"),"area":"dm","address":0,"elements":2048}))
            .collect::<Vec<_>>();
        assert_eq!(
            classify(&source_config(&json!(signals)).validate().unwrap_err()).kind,
            ErrorKind::Usage
        );
    }

    #[test]
    fn config_admission_and_complete_frame_correlation_fail_before_any_value() {
        for patch in [
            json!({"baud":115_200}),
            json!({"baud":691_200}),
            json!({"unit":32}),
            json!({"data_bits":6}),
            json!({"stop_bits":3}),
            json!({"device":"bad\u{0000}path"}),
        ] {
            let mut value = json!({"mode":"c_mode_cpm","model":"cpm2a","device":"must-not-open","signals":[{"name":"value","area":"hr","address":0}]});
            for (key, patch) in patch.as_object().unwrap() {
                value[key].clone_from(patch);
            }
            let parsed: Config = serde_json::from_value(value).unwrap();
            assert_eq!(
                classify(&parsed.validate().unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
        let request = request(0, Area::IrSr, 0, 2).unwrap();
        let vectors = vectors();
        let frame = vectors["cases"][0]["response_ascii"]
            .as_str()
            .unwrap()
            .as_bytes();
        let mut corrupt = frame.to_vec();
        corrupt[7] ^= 1;
        let coalesced = [frame, b"x"].concat();
        for bad in [
            corrupt.as_slice(),
            coalesced.as_slice(),
            &frame[..frame.len() - 1],
            b"@31RL000000FFFF5C*\r",
        ] {
            assert_eq!(
                classify(&payload(bad, &request, 2, false).unwrap_err()).kind,
                ErrorKind::Incomplete
            );
        }
        assert_eq!(
            classify(&payload(frame, &request, 1, false).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        let error = classify(
            &payload(
                b"@00RG14060*\r",
                &self::request(0, Area::TcStatus, 0, 5).unwrap(),
                5,
                true,
            )
            .unwrap_err(),
        );
        assert_eq!(error.kind, ErrorKind::Incomplete);
        assert_eq!(error.details.unwrap()["host_link_end_code"], 20);
        assert_eq!(
            classify(
                &check(Instant::now().checked_sub(Duration::from_secs(1)).unwrap()).unwrap_err()
            )
            .kind,
            ErrorKind::Timeout
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn zero_budget_retires_lazy_owner_without_open_or_later_retry() {
        let config = source_config(&json!([{"name":"value","area":"hr","address":0}]));
        let mut source = open(&config).unwrap();
        assert_eq!(
            classify(&source.read(Duration::ZERO).unwrap_err()).kind,
            ErrorKind::Timeout
        );
        assert_eq!(
            classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
            ErrorKind::Disconnected
        );
    }
}
