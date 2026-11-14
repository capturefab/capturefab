//! Read-only result access to Keyence IV2/IV3/IV4 nonprocedural TCP servers.
//! Field Network manuals 869GB and H06MX chapter 6 and the IV4 user manual
//! pages 9-157 and 9-161 define the `VI` and `RT` exchanges.
//! <https://www.keyence.com/support/user/vision/iv3/communication/>

use super::{Reading, Source, net, validate_name};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead as _, BufReader, Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const MAX_IDENTITY: usize = 128;
// IV3's detailed result has at most 65 records of 32 bytes plus delimiters.
const MAX_RESULT: usize = 3 * 1024;
// 12 envelope bytes including CR, then at most 65 14-byte standard records.
const MAX_IV4_STANDARD_RESULT: usize = 12 + 65 * 14;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    #[serde(default = "default_signal")]
    signal: String,
    #[serde(default)]
    format: Format,
}

fn default_signal() -> String {
    "inspection".into()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Format {
    #[default]
    Standard,
    Detailed,
    StandardMaster,
    DetailedMaster,
}

impl Format {
    const fn detailed(self) -> bool {
        matches!(self, Self::Detailed | Self::DetailedMaster)
    }

    const fn master(self) -> bool {
        matches!(self, Self::StandardMaster | Self::DetailedMaster)
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 {
        return Err(Usage("Keyence IV address port must be nonzero".into()).into());
    }
    validate_name(&config.signal, "Keyence IV signal")?;
    Ok(Box::new(Sensor {
        config: config.clone(),
        session: None,
    }))
}

struct Sensor {
    config: Config,
    session: Option<Session>,
}

impl Source for Sensor {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now() + timeout;
        let mut session = self
            .session
            .take()
            .map_or_else(|| Session::connect(self.config.address, deadline), Ok)?;
        session.wire.get_mut().set_deadline(deadline);
        if self.config.format.master() && matches!(session.family, Family::Iv2) {
            return Err(protocol("master-number result format requires IV3 or IV4"));
        }
        if matches!(session.family, Family::Iv4) && self.config.format.detailed() {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "Keyence IV4 detailed results are unsupported. Configure a standard result format",
            ));
        }
        let limit = match session.family {
            Family::Iv2 | Family::Iv3 => MAX_RESULT,
            Family::Iv4 => MAX_IV4_STANDARD_RESULT,
        };
        let reply = exchange(&mut session.wire, b"RT\r", limit)?;
        let value = session.result(&reply, self.config.format)?;
        remaining(deadline)?;
        self.session = Some(session);
        Ok(vec![Reading {
            signal: self.config.signal.clone(),
            value,
            source_time: None,
        }])
    }

    fn end_timeout_is_idle(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy)]
enum Family {
    Iv2,
    Iv3,
    Iv4,
}

impl Family {
    const fn last_tool(self) -> u8 {
        match self {
            Self::Iv2 => 16,
            Self::Iv3 | Self::Iv4 => 64,
        }
    }
}

struct Session {
    wire: BufReader<Box<dyn net::Transport>>,
    family: Family,
    model: String,
    firmware: String,
}

impl Session {
    fn connect(address: SocketAddr, deadline: Instant) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        stream.set_nodelay(true).map_err(|error| io_error(&error))?;
        let mut wire = BufReader::new(net::plain(stream, deadline));
        // On a different product an identical command could change state.
        // Identify this TCP peer before sending its result command.
        let reply = exchange(&mut wire, b"VI\r", MAX_IDENTITY)?;
        let (family, model, firmware) = identity(&reply)?;
        Ok(Self {
            wire,
            family,
            model: model.into(),
            firmware: firmware.into(),
        })
    }

    fn result(&self, reply: &str, format: Format) -> Result<Value> {
        let mut value = json!({
            "model":self.model,
            "firmware":self.firmware,
            "format":format,
            "raw_reply":reply,
        });
        let line = payload(reply)?;
        if line == "ER,RT,03" {
            // The same response covers absent prior judgment and active
            // automatic transmission. The device does not distinguish them.
            value["state"] = json!("unavailable");
            value["command_error"] = json!(3);
        } else {
            if let Some(code) = line.strip_prefix("ER,RT,") {
                decimal(code, 2, 99)?;
                return Err(protocol(&format!("RT rejected with command error {code}")));
            }
            value["state"] = json!("available");
            value["result"] = serde_json::to_value(parse_result(line, self.family, format)?)?;
        }
        Ok(value)
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "Keyence IV deadline expired",
        ));
    }
    Ok(left)
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("Keyence IV: {message}"))
}

fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::ConnectionRefused => ErrorKind::Unavailable,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("Keyence IV TCP: {error}"))
}

fn exchange(
    wire: &mut BufReader<Box<dyn net::Transport>>,
    command: &[u8],
    limit: usize,
) -> Result<String> {
    wire.get_mut()
        .write_all(command)
        .map_err(|error| io_error(&error))?;
    let mut reply = Vec::new();
    (&mut *wire)
        .take(u64::try_from(limit + 1)?)
        .read_until(b'\r', &mut reply)
        .map_err(|error| io_error(&error))?;
    if reply.len() > limit {
        return Err(protocol("response exceeds the byte limit"));
    }
    if !reply.ends_with(b"\r") {
        return Err(Categorized::error(
            ErrorKind::Disconnected,
            "Keyence IV TCP closed before CR",
        ));
    }
    if !wire.buffer().is_empty() {
        return Err(protocol("unexpected buffered data after the response"));
    }
    String::from_utf8(reply).map_err(|_| protocol("response is not ASCII"))
}

fn payload(reply: &str) -> Result<&str> {
    let line = reply
        .strip_suffix('\r')
        .ok_or_else(|| protocol("response has no CR delimiter"))?;
    if !line.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err(protocol("response contains non-printable ASCII"));
    }
    Ok(line)
}

fn identity(reply: &str) -> Result<(Family, &str, &str)> {
    let line = payload(reply)?;
    let (model, firmware) = line
        .strip_prefix("VI,")
        .and_then(|fields| fields.split_once(','))
        .ok_or_else(|| protocol("expected VI model and firmware response"))?;
    let (family, head) = if let Some(head) = model.strip_prefix("IV2-") {
        (Family::Iv2, head)
    } else if let Some(head) = model.strip_prefix("IV3-") {
        (Family::Iv3, head)
    } else if let Some(head) = model.strip_prefix("IV4-") {
        // VI returns the sensor-head model. The reviewed IV4 manual covers
        // these smart cameras and compact heads.
        if !matches!(
            head.strip_prefix('G').unwrap_or(head),
            "400CA" | "400MA" | "500CA" | "500MA" | "600CA" | "600MA"
        ) {
            return Err(protocol(
                "VI identified an unsupported IV4 sensor-head model",
            ));
        }
        (Family::Iv4, head)
    } else {
        return Err(protocol(
            "VI did not identify an IV2, IV3 or supported IV4 sensor",
        ));
    };
    if head.is_empty()
        || !head
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || firmware.is_empty()
        || !firmware
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(protocol("invalid VI model or firmware field"));
    }
    Ok((family, model, firmware))
}

#[derive(Serialize)]
struct ResultData<'a> {
    number: u16,
    number_raw: &'a str,
    status: &'a str,
    tools: Vec<Tool<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    through_counts: Vec<ThroughCount<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<ResultDetails>,
}

#[derive(Serialize)]
struct ResultDetails {
    logic: [bool; 4],
    processing_time_ms: u32,
    program: u8,
}

#[derive(Serialize)]
struct Tool<'a> {
    number: u8,
    status: &'a str,
    measurement_raw: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    measurement_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details_raw: Option<&'a str>,
}

#[derive(Serialize)]
struct ThroughCount<'a> {
    number: u8,
    count: u32,
    count_raw: &'a str,
    daily_total: u32,
    daily_total_raw: &'a str,
}

fn parse_result(line: &str, family: Family, format: Format) -> Result<ResultData<'_>> {
    let mut tail = line
        .strip_prefix("RT")
        .ok_or_else(|| protocol("expected RT result response"))?;
    let number_raw = field(&mut tail, 5)?;
    let number = u16::try_from(decimal(number_raw, 5, 32767)?)?;
    let status = field(&mut tail, 2)?;
    let mut sorting = false;
    if format.master() {
        if !matches!(status, "99" | "--") && !product_type(status) {
            return Err(protocol("invalid master-number result status"));
        }
    } else if !matches!(status, "OK" | "NG") {
        sorting = product_type(status);
        if matches!(family, Family::Iv2) || (!sorting && status != "--") {
            return Err(protocol("invalid overall result status"));
        }
    }
    let details = format
        .detailed()
        .then(|| parse_details(&mut tail))
        .transpose()?;
    // A standard judgment has a two-byte tool status followed by a comma.
    // Count records have seven decimal bytes at that position. Overall "--"
    // alone is ambiguous and remains an ordinary unavailable judgment.
    let through_counts = if matches!(family, Family::Iv4)
        && matches!(format, Format::Standard)
        && status == "--"
        && tail
            .get(4..12)
            .and_then(|field| field.strip_suffix(','))
            .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
    {
        parse_through_counts(&mut tail)?
    } else {
        Vec::new()
    };
    let mut tools = Vec::new();
    let mut seen = 0_u128;
    while !tail.is_empty() {
        let record = field(&mut tail, if format.detailed() { 32 } else { 13 })?;
        let tool = parse_tool(record, family, format)?;
        let bit = 1_u128 << tool.number;
        if seen & bit != 0 {
            return Err(protocol("duplicate tool number in RT result"));
        }
        seen |= bit;
        sorting |= product_type(tool.status);
        tools.push(tool);
    }
    if sorting && tools.iter().any(|tool| tool.number > 8) {
        return Err(protocol("sorting results support only tools 00–08"));
    }
    Ok(ResultData {
        number,
        number_raw,
        status,
        tools,
        through_counts,
        details,
    })
}

fn parse_through_counts<'a>(tail: &mut &'a str) -> Result<Vec<ThroughCount<'a>>> {
    let mut counts = Vec::new();
    let mut seen = 0_u16;
    while !tail.is_empty() {
        let number = u8::try_from(decimal(field(tail, 2)?, 2, 8)?)?;
        if number == 0 {
            return Err(protocol("IV4 through-count tools are numbered 01–08"));
        }
        let bit = 1_u16 << number;
        if seen & bit != 0 {
            return Err(protocol(
                "duplicate tool number in IV4 through-count result",
            ));
        }
        seen |= bit;
        let count_raw = field(tail, 7)?;
        let count = decimal(count_raw, 7, 9_999_999)?;
        let daily_total_raw = field(tail, 7)?;
        let daily_total = decimal(daily_total_raw, 7, 9_999_999)?;
        counts.push(ThroughCount {
            number,
            count,
            count_raw,
            daily_total,
            daily_total_raw,
        });
    }
    Ok(counts)
}

fn parse_details(tail: &mut &str) -> Result<ResultDetails> {
    let logic = field(tail, 4)?;
    let [a, b, c, d] = logic.as_bytes() else {
        return Err(protocol("invalid logic field"));
    };
    if !logic.bytes().all(|byte| matches!(byte, b'0' | b'1')) {
        return Err(protocol("logic results require four binary digits"));
    }
    Ok(ResultDetails {
        logic: [*a == b'1', *b == b'1', *c == b'1', *d == b'1'],
        processing_time_ms: decimal(field(tail, 5)?, 5, 99999)?,
        program: u8::try_from(decimal(field(tail, 3)?, 3, 127)?)?,
    })
}

fn parse_tool(record: &str, family: Family, format: Format) -> Result<Tool<'_>> {
    let (number, mut tail) = record
        .split_at_checked(2)
        .ok_or_else(|| protocol("truncated tool result"))?;
    let number = u8::try_from(decimal(number, 2, u32::from(family.last_tool()))?)?;
    let status = field(&mut tail, 2)?;
    if !matches!(status, "OK" | "NG" | "--")
        && (matches!(family, Family::Iv2) || format.master() || !product_type(status))
    {
        return Err(protocol("invalid tool status"));
    }
    let measurement_raw = field(&mut tail, 7)?;
    let code = decimal(measurement_raw, 7, 9_999_999)?;
    if status == "--" && code != 0 {
        return Err(protocol("unavailable tool has a nonzero measurement code"));
    }
    // Type-dependent fields share the same 19-byte suffix. OCR fields may
    // contain literal commas. Preserve it byte-for-byte, including padding.
    let details_raw = format.detailed().then_some(tail);
    if details_raw.is_some_and(|tail| !valid_tool_suffix(tail)) {
        return Err(protocol("malformed detailed tool extension"));
    }
    Ok(Tool {
        number,
        status,
        measurement_raw,
        measurement_code: (status != "--").then_some(code),
        details_raw,
    })
}

fn valid_tool_suffix(suffix: &str) -> bool {
    if suffix.bytes().all(|byte| byte == b' ') {
        return true;
    }
    let Some(rest) = suffix.strip_prefix(',') else {
        return false;
    };
    // OCR can use all 16 bytes, including commas. Both OCR and color
    // variants end in at least two pad spaces and can overlap in syntax.
    if rest.ends_with("  ") {
        return true;
    }
    // The pitch extension has two seven-digit values and a two-digit
    // count. Only attempt this after separating the 32-byte tool record.
    let mut fields = rest.split(',');
    [7, 7, 2].into_iter().all(|width| {
        fields.next().is_some_and(|part| {
            part.len() == width && part.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) && fields.next().is_none()
}

fn product_type(field: &str) -> bool {
    matches!(field.as_bytes(), [b'0', b'0'..=b'7'])
}

fn field<'a>(tail: &mut &'a str, width: usize) -> Result<&'a str> {
    let (value, rest) = tail
        .strip_prefix(',')
        .and_then(|tail| tail.split_at_checked(width))
        .ok_or_else(|| protocol("malformed fixed-width RT field"))?;
    *tail = rest;
    Ok(value)
}

fn decimal(field: &str, width: usize, maximum: u32) -> Result<u32> {
    if field.len() != width || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(protocol("invalid fixed-width decimal field"));
    }
    let value = field
        .parse::<u32>()
        .map_err(|_| protocol("invalid decimal field"))?;
    if value > maximum {
        return Err(protocol("decimal field exceeds the device range"));
    }
    Ok(value)
}
