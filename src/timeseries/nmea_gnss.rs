//! Passive NMEA GNSS GGA and RMC observations over TCP or native serial.
//! Manufacturer references: `NovAtel` OEM7 GPGGA/GPGGALONG/GPRMC and
//! u-blox F9 HPG1.32 UBX-22008968 §§2.1, 2.5, 2.7.5 and 2.7.17.
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

// High-precision manufacturer sentences can exceed the historical 82-byte limit.
const MAX_SENTENCE: usize = 512;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: Option<SocketAddr>,
    serial: Option<SerialConfig>,
    signals: Vec<Signal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SerialConfig {
    device: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
    #[serde(default = "parity")]
    parity: serial::Parity,
    #[serde(
        default = "data_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    data_bits: u8,
    #[serde(
        default = "stop_bits",
        deserialize_with = "crate::config_number::unsigned"
    )]
    stop_bits: u8,
}

const fn parity() -> serial::Parity {
    serial::Parity::None
}

const fn data_bits() -> u8 {
    8
}
const fn stop_bits() -> u8 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    sentence: Sentence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Sentence {
    Gga,
    Rmc,
}

impl Sentence {
    fn from_id(id: &str) -> Option<Self> {
        // A leading P denotes a proprietary sentence, not a standard talker.
        if id.len() != 5 || id.starts_with('P') || !id.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            return None;
        }
        match id.get(2..)? {
            "GGA" => Some(Self::Gga),
            "RMC" => Some(Self::Rmc),
            _ => None,
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.signals.is_empty() || config.signals.len() > 2 {
        return Err(Usage("NMEA GNSS requires one or two GGA/RMC signal mappings".into()).into());
    }
    let mut names = BTreeSet::new();
    let mut sentences = Vec::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "NMEA GNSS signal")?;
        if !names.insert(&signal.name) || sentences.contains(&signal.sentence) {
            return Err(
                Usage("NMEA GNSS signal names and sentence types must be unique".into()).into(),
            );
        }
        sentences.push(signal.sentence);
    }
    let connection = match (config.address, config.serial.as_ref()) {
        (Some(address), None) if address.port() != 0 && !address.ip().is_unspecified() => {
            Connection::Tcp {
                address,
                socket: None,
            }
        }
        (None, Some(config)) if !config.device.is_empty() => serial_connection(config)?,
        _ => {
            return Err(Usage(
                "NMEA GNSS requires one numeric TCP IP:port or one serial device".into(),
            )
            .into());
        }
    };
    Ok(Box::new(Stream {
        connection: Some(connection),
        signals: config.signals.clone(),
        lines: Lines::default(),
        observed: None,
        cutoff: None,
        seen: false,
        failed: false,
    }))
}

fn serial_connection(config: &SerialConfig) -> Result<Connection> {
    serial::validate(config.baud, config.stop_bits, config.data_bits)?;
    #[cfg(any(unix, windows))]
    {
        Ok(Connection::Serial(serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            config.data_bits,
        )?))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = config.parity;
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "NMEA GNSS serial requires a Unix terminal or Windows COM port",
        ))
    }
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
    fn receive(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<Option<usize>> {
        match self {
            Self::Tcp { address, socket } => {
                if socket.is_none() {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    let stream = TcpStream::connect_timeout(address, left)
                        .map_err(|error| io_error(&error))?;
                    *socket = Some(net::plain(stream, deadline));
                }
                let stream = socket.as_deref_mut().expect("NMEA TCP connected");
                stream.set_deadline(deadline);
                loop {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    match stream.read(buffer) {
                        Ok(count) => return Ok(Some(count)),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                            ) =>
                        {
                            return Ok(None);
                        }
                        Err(error) => return Err(io_error(&error)),
                    }
                }
            }
            #[cfg(any(unix, windows))]
            Self::Serial(port) => port
                .receive(buffer, deadline)
                .map_err(|error| io_error(&error)),
        }
    }
}

struct Stream {
    connection: Option<Connection>,
    signals: Vec<Signal>,
    lines: Lines,
    observed: Option<u64>,
    cutoff: Option<u64>,
    seen: bool,
    failed: bool,
}

impl Stream {
    fn next(&mut self) -> Result<Option<Reading>> {
        while let Some(line) = self.lines.take()? {
            let (raw, fields) = checked_frame(&line)?;
            self.lines.synchronized = true;
            let Some(sentence) = Sentence::from_id(fields[0]) else {
                continue;
            };
            let Some(signal) = self
                .signals
                .iter()
                .find(|signal| signal.sentence == sentence)
            else {
                continue;
            };
            let value = decode(raw, &fields, sentence)?;
            self.seen = true;
            return Ok(Some(Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            }));
        }
        Ok(None)
    }

    fn receive(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        loop {
            if let Some(reading) = self.next()? {
                return Ok(vec![reading]);
            }
            if Instant::now() >= deadline {
                return Ok(Vec::new());
            }
            let connection = self.connection.as_mut().ok_or_else(|| {
                Categorized::error(ErrorKind::Disconnected, "NMEA GNSS channel closed")
            })?;
            let mut buffer = [0; MAX_SENTENCE];
            match connection.receive(&mut buffer, deadline)? {
                Some(0) => {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "NMEA GNSS stream closed",
                    ));
                }
                Some(count) => {
                    let observed = crate::host::monotonic_ns();
                    if let (Some(observed), Some(cutoff)) = (observed, self.cutoff)
                        && observed > cutoff
                    {
                        return Ok(Vec::new());
                    }
                    self.observed = observed;
                    self.lines.pending.extend_from_slice(&buffer[..count]);
                }
                None => return Ok(Vec::new()),
            }
        }
    }
}

impl Source for Stream {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("NMEA GNSS timeout is too large".into()))?;
        let result = self.receive(deadline);
        if result.is_err() {
            self.failed = true;
            self.connection = None;
        }
        result
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
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.cutoff = end_ns;
    }
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        self.connection = None;
        Ok(())
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        if self.failed {
            return Ok(None);
        }
        // At a requested cutoff a trailing partial sentence is not an observation.
        // Complete buffered sentences still retain their original receive time.
        self.next()
    }
}

#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    synchronized: bool,
}

impl Lines {
    fn take(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.synchronized {
            let Some(start) = self.pending.iter().position(|&byte| byte == b'$') else {
                self.pending.clear();
                return Ok(None);
            };
            self.pending.drain(..start);
        }
        let Some(end) = self.pending.iter().position(|&byte| byte == b'\n') else {
            if self.pending.len() >= MAX_SENTENCE {
                return Err(malformed("sentence exceeds 512-byte limit"));
            }
            return Ok(None);
        };
        if end + 1 > MAX_SENTENCE {
            return Err(malformed("sentence exceeds 512-byte limit"));
        }
        let line = self.pending.drain(..=end).collect();
        Ok(Some(line))
    }
}

fn checked_frame(line: &[u8]) -> Result<(&str, Vec<&str>)> {
    let raw = line
        .strip_suffix(b"\r\n")
        .ok_or_else(|| malformed("sentence must end with CRLF"))?;
    if raw.len() < 5 || raw.first() != Some(&b'$') || raw[raw.len() - 3] != b'*' {
        return Err(malformed(
            "missing sentence delimiters or two-digit checksum",
        ));
    }
    let body = &raw[1..raw.len() - 3];
    if !body
        .iter()
        .all(|byte| (0x20..=0x7e).contains(byte) && !matches!(byte, b'$' | b'*'))
    {
        return Err(malformed("invalid sentence characters"));
    }
    if !raw[raw.len() - 2..].iter().all(u8::is_ascii_hexdigit) {
        return Err(malformed("invalid checksum digits"));
    }
    let checksum = std::str::from_utf8(&raw[raw.len() - 2..])
        .ok()
        .and_then(|text| u8::from_str_radix(text, 16).ok())
        .ok_or_else(|| malformed("invalid checksum digits"))?;
    if body.iter().fold(0, |sum, &byte| sum ^ byte) != checksum {
        return Err(malformed("sentence checksum mismatch"));
    }
    let raw = std::str::from_utf8(raw).map_err(|_| malformed("sentence is not ASCII"))?;
    let fields = part(raw, 1, raw.len() - 3)?.split(',').collect();
    Ok((raw, fields))
}

fn decode(raw: &str, fields: &[&str], sentence: Sentence) -> Result<Value> {
    let id = fields
        .first()
        .ok_or_else(|| malformed("missing sentence ID"))?;
    let mut value = json!({"raw":raw,"talker":part(id,0,2)?,"sentence":part(id,2,id.len())?,"checksum":part(raw,raw.len().saturating_sub(2),raw.len())?});
    match sentence {
        Sentence::Gga => {
            let fields: &[&str; 15] = fields
                .try_into()
                .map_err(|_| malformed("GGA requires fourteen data fields"))?;
            time(&mut value, fields[1])?;
            coordinates(&mut value, fields[2], fields[3], fields[4], fields[5])?;
            if fields[6].len() != 1 {
                return Err(malformed("invalid GGA quality indicator"));
            }
            value["quality"] = json!(unsigned(fields[6])?);
            if !fields[7].is_empty() {
                value["satellites"] = json!(unsigned(fields[7])?);
            }
            for (key, index, signed) in [
                ("hdop", 8, false),
                ("altitude", 9, true),
                ("geoid_separation", 11, true),
                ("differential_age_s", 13, false),
            ] {
                numeric(&mut value, key, fields[index], signed)?;
            }
            text(&mut value, "altitude_unit", fields[10]);
            text(&mut value, "geoid_unit", fields[12]);
            text(&mut value, "station_id", fields[14]);
        }
        Sentence::Rmc => {
            let (fields, flags) = fields
                .split_at_checked(12)
                .filter(|(_, flags)| flags.len() <= 2)
                .ok_or_else(|| malformed("RMC requires eleven to thirteen data fields"))?;
            let fields: &[&str; 12] = fields
                .try_into()
                .map_err(|_| malformed("RMC requires eleven to thirteen data fields"))?;
            time(&mut value, fields[1])?;
            if !matches!(fields[2], "A" | "V") {
                return Err(malformed("invalid RMC validity status"));
            }
            value["status"] = json!(fields[2]);
            coordinates(&mut value, fields[3], fields[4], fields[5], fields[6])?;
            numeric(&mut value, "speed_knots", fields[7], false)?;
            numeric(&mut value, "course_true_deg", fields[8], false)?;
            if !fields[9].is_empty() {
                if fields[9].len() != 6 || !fields[9].bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(malformed("invalid RMC date"));
                }
                value["date_ddmmyy"] = json!(fields[9]);
            }
            numeric(&mut value, "magnetic_variation_deg", fields[10], false)?;
            if !matches!(fields[11], "" | "E" | "W") {
                return Err(malformed("invalid magnetic variation direction"));
            }
            text(&mut value, "magnetic_variation_direction", fields[11]);
            for (key, flag) in ["mode", "navigation_status"].into_iter().zip(flags) {
                if !flag.is_empty()
                    && (flag.len() != 1 || !flag.bytes().all(|byte| byte.is_ascii_uppercase()))
                {
                    return Err(malformed("invalid RMC mode/navigation status"));
                }
                text(&mut value, key, flag);
            }
        }
    }
    Ok(value)
}

fn coordinates(
    value: &mut Value,
    latitude: &str,
    ns: &str,
    longitude: &str,
    ew: &str,
) -> Result<()> {
    for (key, field, direction, degrees, limit, directions) in [
        ("latitude_dm", latitude, ns, 2, 90, ["N", "S"]),
        ("longitude_dm", longitude, ew, 3, 180, ["E", "W"]),
    ] {
        if field.is_empty() && direction.is_empty() {
            continue;
        }
        if !directions.contains(&direction) {
            return Err(malformed("missing or invalid coordinate hemisphere"));
        }
        let integer = field.split('.').next().unwrap_or_default();
        if integer.len() != degrees + 2 || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed("invalid degrees/minutes coordinate"));
        }
        decimal(field, false)?;
        let angle = unsigned(part(integer, 0, degrees)?)?;
        let minutes = unsigned(part(integer, degrees, integer.len())?)?;
        if angle > limit
            || minutes >= 60
            || (angle == limit
                && part(field, degrees, field.len())?
                    .bytes()
                    .any(|byte| byte != b'0' && byte != b'.'))
        {
            return Err(malformed("coordinate exceeds angular bounds"));
        }
        value[key] = json!(field);
        value[if degrees == 2 {
            "latitude_hemisphere"
        } else {
            "longitude_hemisphere"
        }] = json!(direction);
    }
    Ok(())
}

fn time(value: &mut Value, field: &str) -> Result<()> {
    if field.is_empty() {
        return Ok(());
    }
    let integer = field.split('.').next().unwrap_or_default();
    if integer.len() != 6 || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed("invalid UTC time of day"));
    }
    decimal(field, false)?;
    if unsigned(part(integer, 0, 2)?)? > 23
        || unsigned(part(integer, 2, 4)?)? > 59
        || unsigned(part(integer, 4, integer.len())?)? > 60
    {
        return Err(malformed("UTC time exceeds clock bounds"));
    }
    value["utc_time"] = json!(field);
    Ok(())
}

fn part(text: &str, start: usize, end: usize) -> Result<&str> {
    text.get(start..end)
        .ok_or_else(|| malformed("invalid sentence string bounds"))
}

fn unsigned(field: &str) -> Result<u16> {
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed("invalid unsigned decimal field"));
    }
    field
        .parse()
        .map_err(|_| malformed("unsigned decimal field exceeds u16"))
}

fn decimal(field: &str, signed: bool) -> Result<Number> {
    let (negative, digits) = if signed {
        field.strip_prefix('-').map_or_else(
            || (false, field.strip_prefix('+').unwrap_or(field)),
            |digits| (true, digits),
        )
    } else {
        (false, field)
    };
    let (integer, fraction) = digits
        .split_once('.')
        .map_or((digits, None), |(a, b)| (a, Some(b)));
    if integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || fraction
            .is_some_and(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(malformed("invalid decimal field"));
    }
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let mut normalized = if negative {
        format!("-{integer}")
    } else {
        integer.into()
    };
    if let Some(fraction) = fraction {
        normalized.push('.');
        normalized.push_str(fraction);
    }
    normalized
        .parse()
        .map_err(|_| malformed("invalid decimal number"))
}

fn numeric(value: &mut Value, key: &str, field: &str, signed: bool) -> Result<()> {
    if !field.is_empty() {
        value[key] = Value::Number(decimal(field, signed)?);
    }
    Ok(())
}

fn text(value: &mut Value, key: &str, field: &str) {
    if !field.is_empty() {
        value[key] = json!(field);
    }
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("NMEA GNSS: {message}"))
}

fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("NMEA GNSS I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent literal from u-blox UBX-22008968 R01 §2.5.3.
    const UBLOX: &[u8] =
        b"$GNRMC,014230.00,A,4722.80340,N,00831.68218,E,0.000,,120477,,,A,V*14\r\n";

    fn wire(body: &str) -> Vec<u8> {
        let checksum = body.bytes().fold(0, |sum, byte| sum ^ byte);
        format!("${body}*{checksum:02X}\r\n").into_bytes()
    }

    fn reading(bytes: &[u8], sentence: Sentence) -> Value {
        let (raw, fields) = checked_frame(bytes).unwrap();
        decode(raw, &fields, sentence).unwrap()
    }

    fn stream(bytes: &[u8]) -> Stream {
        Stream {
            connection: None,
            signals: vec![
                Signal {
                    name: "fix".into(),
                    sentence: Sentence::Gga,
                },
                Signal {
                    name: "motion".into(),
                    sentence: Sentence::Rmc,
                },
            ],
            lines: Lines {
                pending: bytes.into(),
                synchronized: false,
            },
            observed: Some(1),
            cutoff: None,
            seen: false,
            failed: false,
        }
    }

    #[test]
    fn modern_ublox_literal_retains_exact_coordinates_time_flags_and_checksum() {
        let value = reading(UBLOX, Sentence::Rmc);
        assert_eq!(value["raw"], std::str::from_utf8(UBLOX).unwrap().trim_end());
        assert_eq!(value["talker"], "GN");
        assert_eq!(value["sentence"], "RMC");
        assert_eq!(value["checksum"], "14");
        assert_eq!(value["utc_time"], "014230.00");
        assert_eq!(value["latitude_dm"], "4722.80340");
        assert_eq!(value["longitude_dm"], "00831.68218");
        assert_eq!(value["longitude_hemisphere"], "E");
        assert_eq!(value["date_ddmmyy"], "120477");
        assert_eq!(value["mode"], "A");
        // Navigation status V is a separate field, not the A validity status.
        assert_eq!(value["status"], "A");
        assert_eq!(value["navigation_status"], "V");
        assert!(value.get("magnetic_variation_deg").is_none());
    }

    #[test]
    fn invalid_and_void_observations_do_not_invent_missing_fields() {
        let gga = wire(
            &[
                "GNGGA", "", "", "", "", "", "0", "00", "", "", "", "", "", "", "",
            ]
            .join(","),
        );
        let rmc = wire(
            &[
                "GNRMC", "", "V", "", "", "", "", "", "", "", "", "", "N", "V",
            ]
            .join(","),
        );
        for (frame, kind, key, expected) in [
            (&gga, Sentence::Gga, "quality", json!(0)),
            (&rmc, Sentence::Rmc, "status", json!("V")),
        ] {
            let value = reading(frame, kind);
            assert_eq!(value[key], expected);
            for absent in [
                "utc_time",
                "date_ddmmyy",
                "latitude_dm",
                "latitude_hemisphere",
                "longitude_dm",
                "longitude_hemisphere",
                "altitude",
            ] {
                assert!(value.get(absent).is_none(), "{value}");
            }
        }
    }

    #[test]
    fn high_precision_south_west_and_signed_decimals_remain_exact() {
        let frame = wire(
            "GNGGA,235960.000000,4717.12345678901234567890,S,00833.00000000000000000001,W,5,140,000.0100,-000.0000,M,+001.0200,M,09,TSTR",
        );
        let value = reading(&frame, Sentence::Gga);
        assert_eq!(value["latitude_dm"], "4717.12345678901234567890");
        assert_eq!(value["latitude_hemisphere"], "S");
        assert_eq!(value["longitude_dm"], "00833.00000000000000000001");
        assert_eq!(value["longitude_hemisphere"], "W");
        assert_eq!(value["satellites"], 140);
        assert_eq!(value["utc_time"], "235960.000000");
        assert_eq!(value["hdop"].to_string(), "0.0100");
        assert_eq!(value["altitude"].to_string(), "-0.0000");
        assert_eq!(value["geoid_separation"].to_string(), "1.0200");
        assert_eq!(value["station_id"], "TSTR");
    }

    #[test]
    fn invalid_coordinate_pairing_clock_and_numeric_grammar_fail() {
        let body = "GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,";
        for (before, after) in [
            ("4807.038", "9060.0"),
            ("01131.000", "18000.001"),
            (",N,", ",,"),
            ("123519", "126000"),
            ("0.9", "NaN"),
            ("0.9", "1e3"),
            ("08", "65536"),
        ] {
            let frame = wire(&body.replace(before, after));
            let (raw, fields) = checked_frame(&frame).unwrap();
            let error = decode(raw, &fields, Sentence::Gga).unwrap_err();
            assert!(error.to_string().contains("NMEA GNSS"), "{error:#}");
        }
        for field in ["", "1.", ".1", "1.2.3", "-1", "+1", "inf"] {
            let error = decimal(field, false).unwrap_err();
            assert!(error.to_string().contains("decimal"));
        }
    }

    #[test]
    fn every_complete_sentence_requires_a_real_checksum_and_crlf() {
        // §2.7.17 adds ,V to an older example but retains its old checksum.
        // Exact XOR for this published body is 2D; it must not bypass verification.
        let inconsistent =
            b"$GPRMC,083559.00,A,4717.11437,N,00833.91522,E,0.004,77.52,091202,,,A,V*57\r\n";
        assert!(
            checked_frame(inconsistent)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
        let text = std::str::from_utf8(UBLOX).unwrap();
        for line in [
            text.replace("*14", "*15"),
            text.replace("*14", "*+4"),
            text.replace("*14", "*XYZ"),
            text.replace("\r\n", "\n"),
            text.replace("014230", "014\x0230"),
        ] {
            let error = checked_frame(line.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("NMEA GNSS"));
        }
    }

    #[test]
    fn startup_fragment_and_unselected_sentences_do_not_merge_epochs() {
        let ignored = wire("GPTXT,01,01,02,receiver ready");
        let mut first =
            stream(&[&b"old incomplete sentence\r\n"[..], &ignored, &UBLOX[..20]].concat());
        assert!(first.next().unwrap().is_none());
        assert_eq!(first.lines.pending, &UBLOX[..20]);
        first.lines.pending.extend_from_slice(&UBLOX[20..]);
        first.lines.pending.extend_from_slice(UBLOX);
        assert_eq!(first.next().unwrap().unwrap().signal, "motion");
        assert_eq!(first.next().unwrap().unwrap().signal, "motion");
        assert!(first.next().unwrap().is_none());
        assert!(first.ready());
        assert_eq!(first.observed_ns(), Some(1));
        assert!(first.buffered_timestamps());
        assert!(Sentence::from_id("PTGGA").is_none());
    }

    #[test]
    fn bounded_lines_and_failure_drain_do_not_publish_partial_sentences() {
        let mut oversized = stream(&[b'$'; MAX_SENTENCE]);
        let error = oversized.next().unwrap_err();
        assert!(error.to_string().contains("512-byte"));
        let mut cutoff = stream(&[UBLOX, &UBLOX[..10]].concat());
        assert_eq!(cutoff.drain().unwrap().unwrap().signal, "motion");
        assert!(cutoff.drain().unwrap().is_none());
        let mut failed = stream(UBLOX);
        failed.failed = true;
        assert!(failed.drain().unwrap().is_none());
        let empty = stream(&[]).read(Duration::ZERO).unwrap();
        assert!(empty.is_empty());
    }
}
