//! Bounded SICK `CoLa A` / `CoLa B` single-layer `LMDscandata` reads.
//! `TiM4xx` 8026151/1ONO Appendix 12 and telegram listing 8014631/1PIK
//! §§4.3.6–4.3.7 define this read-only subset; device clocks stay boot-relative.
mod ascii;
mod binary;

use super::{Reading, Source, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const REQUEST: &[u8] = b"\x02sRN LMDscandata\x03";
const MAX_FRAME: usize = 65_536;
const MAX_POINTS: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    #[serde(default)]
    profile: Profile,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    max_frame_bytes: Option<usize>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    max_points: Option<usize>,
    #[serde(default = "signal")]
    signal: String,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
enum Profile {
    #[default]
    #[serde(rename = "cola_a")]
    Ascii,
    #[serde(rename = "cola_b_tim5xx")]
    Tim5xx,
    #[serde(rename = "cola_a_lms1xx")]
    AsciiLms1xx,
    #[serde(rename = "cola_a_lms5xx")]
    AsciiLms5xx,
    #[serde(rename = "cola_b_lms1xx")]
    Lms1xx,
    #[serde(rename = "cola_b_lms5xx")]
    Lms5xx,
}

impl Profile {
    const fn ascii_model(self) -> Option<ascii::Model> {
        match self {
            Self::AsciiLms1xx => Some(ascii::Model::Lms1xx),
            Self::AsciiLms5xx => Some(ascii::Model::Lms5xx),
            Self::Ascii | Self::Tim5xx | Self::Lms1xx | Self::Lms5xx => None,
        }
    }
    fn model(self) -> Option<binary::Model> {
        match self {
            Self::Ascii | Self::AsciiLms1xx | Self::AsciiLms5xx => None,
            Self::Tim5xx => Some(binary::Model::Tim5xx),
            Self::Lms1xx => Some(binary::Model::Lms1xx),
            Self::Lms5xx => Some(binary::Model::Lms5xx),
        }
    }
}

fn signal() -> String {
    "scan".into()
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    super::validate_name(&config.signal, "SICK CoLa signal")?;
    if config.address.port() == 0 || config.address.ip().is_unspecified() {
        return Err(
            Usage("SICK CoLa requires a numeric IP address and nonzero port".into()).into(),
        );
    }
    if matches!(config.profile, Profile::Ascii)
        && (config.max_frame_bytes.is_some() || config.max_points.is_some())
    {
        return Err(Usage(
            "SICK CoLa binary limits or larger ASCII limits require an explicit model profile"
                .into(),
        )
        .into());
    }
    let limits = binary::Limits::new(config.max_frame_bytes, config.max_points)?;
    Ok(Box::new(Client {
        address: config.address,
        signal: config.signal.clone(),
        profile: config.profile,
        limits,
        socket: None,
        closed: false,
    }))
}

struct Client {
    address: SocketAddr,
    signal: String,
    profile: Profile,
    limits: binary::Limits,
    socket: Option<Box<dyn net::Transport>>,
    closed: bool,
}

impl Client {
    fn exchange(&mut self, deadline: Instant) -> Result<Value> {
        if self.closed {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "SICK CoLa channel closed",
            ));
        }
        if self.socket.is_none() {
            let stream = TcpStream::connect_timeout(&self.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            stream.set_nodelay(true).map_err(|error| io_error(&error))?;
            self.socket = Some(net::plain(stream, deadline));
        }
        let socket = self.socket.as_deref_mut().expect("SICK CoLa connected");
        socket.set_deadline(deadline);
        let model = self.profile.model();
        socket
            .write_all(model.map_or(REQUEST, |_| binary::REQUEST))
            .map_err(|error| io_error(&error))?;
        if let Some(model) = model {
            binary::read_reply(|buffer| socket.read(buffer), deadline, model, self.limits)
        } else if let Some(model) = self.profile.ascii_model() {
            ascii::read_reply(|buffer| socket.read(buffer), deadline, model, self.limits)
        } else {
            read_reply(|buffer| socket.read(buffer), deadline)
        }
    }
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("SICK CoLa timeout is too large".into()))?;
        let value = match self.exchange(deadline) {
            Ok(value) => value,
            Err(error) => {
                self.socket = None;
                self.closed = true;
                return Err(error);
            }
        };
        Ok(vec![Reading {
            signal: self.signal.clone(),
            value,
            source_time: None,
        }])
    }
}

fn read_reply(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    deadline: Instant,
) -> Result<Value> {
    let mut frame = Vec::with_capacity(4096);
    let mut buffer = [0; 4096];
    while frame.len() < MAX_FRAME {
        remaining(deadline)?;
        let count = match read(&mut buffer) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "SICK CoLa peer closed during scan",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        if frame.len() + count > MAX_FRAME {
            return Err(malformed("scan exceeds 64 KiB limit"));
        }
        let previous_length = frame.len();
        frame.extend_from_slice(&buffer[..count]);
        if frame.first() != Some(&2) {
            return Err(malformed("scan must start with one STX"));
        }
        if let Some(end) = buffer[..count].iter().position(|&byte| byte == 3) {
            let end = previous_length + end;
            if end + 1 != frame.len() {
                return Err(malformed("extra bytes after scan ETX"));
            }
            let payload = &frame[1..end];
            if !payload.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
                return Err(malformed("scan payload must be printable CoLa A ASCII"));
            }
            let payload =
                std::str::from_utf8(payload).map_err(|_| malformed("scan payload is not ASCII"))?;
            let value = decode(payload)?;
            remaining(deadline)?;
            return Ok(value);
        }
    }
    Err(malformed("scan exceeds 64 KiB limit"))
}

#[derive(Serialize)]
struct Channel<T> {
    scale_bits: u32,
    offset_bits: u32,
    start_angle_raw: i32,
    angle_step_raw: u16,
    values: Vec<T>,
}

fn decode(payload: &str) -> Result<Value> {
    let mut fields = Fields { payload, offset: 0 };
    fields.expect("sRA")?;
    fields.expect("LMDscandata")?;
    let version: u16 = fields.number()?;
    if version != 1 {
        return Err(unsupported("only scan version 1 is qualified"));
    }
    let device_number: u16 = fields.number()?;
    let serial_number: u32 = fields.number()?;
    let device_status: [u8; 2] = [fields.number()?, fields.number()?];
    let telegram_counter: u16 = fields.number()?;
    let scan_counter: u16 = fields.number()?;
    let startup_us: u32 = fields.number()?;
    let transmit_us: u32 = fields.number()?;
    let digital_inputs_raw: [u8; 2] = [fields.number()?, fields.number()?];
    let digital_outputs_raw: [u8; 2] = [fields.number()?, fields.number()?];
    fields.zero("layer angle")?;
    let scan_frequency_raw: u32 = fields.number()?;
    let measurement_frequency_raw: u32 = fields.number()?;
    fields.zero("encoder data")?;
    if fields.number::<u16>()? != 1 {
        return Err(unsupported(
            "scan requires exactly one 16-bit DIST1 channel",
        ));
    }
    let distance: Channel<u16> = channel(&mut fields, "DIST1")?;
    let rssi = match fields.number::<u16>()? {
        0 => None,
        1 => {
            let rssi: Channel<u8> = channel(&mut fields, "RSSI1")?;
            if rssi.values.len() != distance.values.len()
                || rssi.start_angle_raw != distance.start_angle_raw
                || rssi.angle_step_raw != distance.angle_step_raw
            {
                return Err(malformed("DIST1 and RSSI1 geometry must match"));
            }
            Some(rssi)
        }
        _ => return Err(unsupported("scan permits at most one 8-bit RSSI1 channel")),
    };
    fields.zero("position data")?;
    let name = match fields.number::<u16>()? {
        0 => None,
        1 => Some(fields.name()?),
        _ => return Err(malformed("invalid device-name flag")),
    };
    for extension in ["comment", "calendar time", "event data"] {
        fields.zero(extension)?;
    }
    if fields.offset != payload.len() {
        return Err(malformed("extra scan fields"));
    }
    let mut value = json!({
        "version":version,"device_number":device_number,"serial_number":serial_number,
        "device_status":device_status,"telegram_counter":telegram_counter,"scan_counter":scan_counter,
        "startup_us":startup_us,"transmit_us":transmit_us,
        "digital_inputs_raw":digital_inputs_raw,"digital_outputs_raw":digital_outputs_raw,
        "scan_frequency_raw":scan_frequency_raw,"measurement_frequency_raw":measurement_frequency_raw,
        "distance":distance
    });
    if let Some(rssi) = rssi {
        value["rssi"] = serde_json::to_value(rssi)?;
    }
    if let Some(name) = name {
        value["device_name"] = json!(name);
    }
    Ok(value)
}

fn channel<T: TryFrom<u32>>(fields: &mut Fields<'_>, name: &str) -> Result<Channel<T>> {
    if fields.token()? != name {
        return Err(unsupported("unexpected scan channel name"));
    }
    let scale_bits = fields.number()?;
    let offset_bits = fields.number()?;
    let start_angle_raw = i32::from_be_bytes(fields.number::<u32>()?.to_be_bytes());
    let angle_step_raw = fields.number()?;
    let count = usize::from(fields.number::<u16>()?);
    if count > MAX_POINTS {
        return Err(malformed("channel exceeds 4096-point software limit"));
    }
    let values = std::iter::repeat_with(|| fields.number())
        .take(count)
        .collect::<Result<Vec<T>>>()?;
    Ok(Channel {
        scale_bits,
        offset_bits,
        start_angle_raw,
        angle_step_raw,
        values,
    })
}

struct Fields<'a> {
    payload: &'a str,
    offset: usize,
}

impl<'a> Fields<'a> {
    fn token(&mut self) -> Result<&'a str> {
        let tail = self
            .payload
            .get(self.offset..)
            .ok_or_else(|| malformed("invalid scan field boundary"))?;
        let end = tail.find(' ').unwrap_or(tail.len());
        let token = tail
            .get(..end)
            .ok_or_else(|| malformed("invalid scan field boundary"))?;
        if token.is_empty() {
            return Err(malformed("missing or empty scan field"));
        }
        self.offset += end + usize::from(end < tail.len());
        Ok(token)
    }

    fn number<T: TryFrom<u32>>(&mut self) -> Result<T> {
        let token = self.token()?;
        if token.len() > 8 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(malformed("invalid hexadecimal scan field"));
        }
        u32::from_str_radix(token, 16)
            .ok()
            .and_then(|value| T::try_from(value).ok())
            .ok_or_else(|| malformed("scan field exceeds its wire type"))
    }

    fn expect(&mut self, expected: &str) -> Result<()> {
        if self.token()? != expected {
            return Err(malformed("expected sRA LMDscandata single-scan response"));
        }
        Ok(())
    }

    fn zero(&mut self, extension: &str) -> Result<()> {
        if self.number::<u16>()? != 0 {
            return Err(unsupported(&format!("scan {extension} is not qualified")));
        }
        Ok(())
    }

    fn name(&mut self) -> Result<&'a str> {
        let length = usize::from(self.number::<u16>()?);
        if length > 15 {
            return Err(malformed("device name exceeds 15 characters"));
        }
        let end = self.offset + length;
        let name = self
            .payload
            .get(self.offset..end)
            .ok_or_else(|| malformed("truncated device name"))?;
        if !name.bytes().all(|byte| (0x20..=0x7a).contains(&byte))
            || self.payload.as_bytes().get(end) != Some(&b' ')
        {
            return Err(malformed("invalid length-prefixed device name"));
        }
        self.offset = end + 1;
        Ok(name)
    }
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("SICK CoLa: {message}"))
}

fn unsupported(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unsupported, format!("SICK CoLa: {message}"))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "SICK CoLa poll deadline expired",
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
    Categorized::error(kind, format!("SICK CoLa I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    // A specification-derived scan, including all reserved distance codes and
    // deliberately non-default scale/offset bits. No coordinate conversion occurs.
    const SCAN: &str = "sRA LMDscandata 1 FFFF FFFFFFFF FF FF FFFF FFFF FFFFFFFF 0 FF FF FF FF 0 FFFFFFFF FFFFFFFF 0 1 DIST1 40000000 BF800000 FFFFFFFF 1 5 0 1 2 F FFFF 0 0 0 0 0 0";

    fn reply(frame: &[u8]) -> Result<Value> {
        let mut input = io::Cursor::new(frame);
        read_reply(
            |buffer| input.read(&mut buffer[..1]),
            Instant::now() + Duration::from_secs(1),
        )
    }

    fn replace_field(index: usize, field: &str) -> String {
        let mut fields = SCAN.split(' ').collect::<Vec<_>>();
        fields[index] = field;
        fields.join(" ")
    }

    #[test]
    fn binary_profile_limits_accept_exact_integral_config_and_preserve_ascii_default() {
        let config: Config = serde_json::from_str(r#"{"address":"127.0.0.1:2112","profile":"cola_b_lms5xx","max_frame_bytes":1.28e5,"max_points":8192.0}"#).unwrap();
        open(&config).unwrap();
        let config: Config = serde_json::from_str(r#"{"address":"127.0.0.1:2111"}"#).unwrap();
        assert!(matches!(config.profile, Profile::Ascii));
        open(&config).unwrap();
        let config: Config =
            serde_json::from_str(r#"{"address":"127.0.0.1:2111","max_points":8192}"#).unwrap();
        assert!(
            open(&config)
                .err()
                .unwrap()
                .to_string()
                .contains("binary limits")
        );
        serde_json::from_str::<Config>(
            r#"{"address":"127.0.0.1:2112","profile":"cola_b_lms5xx","max_points":1.1}"#,
        )
        .unwrap_err();
    }

    #[test]
    fn raw_scan_fields_retain_widths_signed_angles_and_reserved_distances() {
        let value = reply(format!("\x02{SCAN}\x03").as_bytes()).unwrap();
        assert_eq!(value["serial_number"], u32::MAX);
        assert_eq!(value["device_status"], json!([255, 255]));
        assert_eq!(value["startup_us"], u32::MAX);
        assert_eq!(value["transmit_us"], 0);
        assert_eq!(value["distance"]["scale_bits"], 0x4000_0000_u32);
        assert_eq!(value["distance"]["offset_bits"], 0xbf80_0000_u32);
        assert_eq!(value["distance"]["start_angle_raw"], -1);
        assert_eq!(value["distance"]["angle_step_raw"], 1);
        assert_eq!(value["distance"]["values"], json!([0, 1, 2, 15, 65_535]));
        assert!(value.get("rssi").is_none());
        assert!(value.get("device_name").is_none());
    }

    #[test]
    fn tim24x_documented_geometry_retains_all_241_raw_points() {
        // Specification-derived, not a captured scanner response: TiM24x has
        // -120..120 degree range and a one-degree step (listing pp. 104–105).
        let samples = (0..241)
            .map(|value| format!("{value:X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let payload = format!(
            "sRA LMDscandata 1 1 1 0 0 1 1 1 2 0 0 2 0 0 5AA 0 0 1 DIST1 3F800000 0 FFEDB080 2710 F1 {samples} 0 0 0 0 0 0"
        );
        let value = decode(&payload).unwrap();
        assert_eq!(value["distance"]["start_angle_raw"], -1_200_000);
        assert_eq!(value["distance"]["angle_step_raw"], 10_000);
        assert_eq!(value["distance"]["values"].as_array().unwrap().len(), 241);
        assert_eq!(value["distance"]["values"][240], 240);
        assert_eq!(value["digital_outputs_raw"], json!([2, 0]));
    }

    #[test]
    fn declared_widths_and_counts_are_checked_before_publication() {
        for (index, field) in [
            (3, "10000"),
            (4, "100000000"),
            (5, "100"),
            (7, "10000"),
            (9, "-1"),
            (12, "100"),
            (23, "100000000"),
            (24, "10000"),
            (25, "1001"),
            (26, "10000"),
        ] {
            let error = decode(&replace_field(index, field)).unwrap_err();
            assert!(error.to_string().contains("SICK CoLa"), "{error:#}");
        }
        for count in ["4", "6"] {
            let error = decode(&replace_field(25, count)).unwrap_err();
            assert!(error.to_string().contains("SICK CoLa"), "{error:#}");
        }
    }

    #[test]
    fn unknown_versions_layers_encoders_channels_and_tail_extensions_fail() {
        for (index, field) in [
            (2, "2"),
            (15, "1"),
            (18, "1"),
            (19, "2"),
            (20, "DIST2"),
            (31, "2"),
            (32, "1"),
            (34, "1"),
            (35, "1"),
            (36, "1"),
        ] {
            let error = decode(&replace_field(index, field)).unwrap_err();
            assert!(error.to_string().contains("SICK CoLa"), "{error:#}");
        }
    }

    #[test]
    fn length_prefixed_name_preserves_embedded_and_trailing_spaces() {
        let named = SCAN.strip_suffix("0 0 0 0 0 0").unwrap();
        let value = decode(&format!("{named}0 0 1 5 A B   0 0 0")).unwrap();
        assert_eq!(value["device_name"], "A B  ");
        for tail in ["0 0 1 10 abc 0 0 0", "0 0 1 5 abc 0 0 0", "0 0 1 1 ~ 0 0 0"] {
            let error = decode(&format!("{named}{tail}")).unwrap_err();
            assert!(error.to_string().contains("SICK CoLa"), "{error:#}");
        }
    }

    #[test]
    fn optional_rssi_requires_matching_geometry_count_and_u8_samples() {
        let prefix = SCAN.strip_suffix("0 0 0 0 0 0").unwrap();
        let rssi = "1 RSSI1 3F800000 0 FFFFFFFF 1 5 0 FF 0 0 0 0 0 0 0 0";
        let value = decode(&format!("{prefix}{rssi}")).unwrap();
        assert_eq!(value["rssi"]["values"], json!([0, 255, 0, 0, 0]));
        for (before, after) in [
            ("RSSI1", "RSSI2"),
            ("FFFFFFF", "FFFFFFE"),
            (" 1 5 ", " 2 5 "),
            (" 1 5 ", " 1 4 "),
            (" FF ", " 100 "),
        ] {
            let error = decode(&format!("{prefix}{}", rssi.replace(before, after))).unwrap_err();
            assert!(error.to_string().contains("SICK CoLa"), "{error:#}");
        }
    }

    #[test]
    fn framing_rejects_binary_unsolicited_truncation_extra_and_oversize() {
        for frame in [
            format!("{SCAN}\x03"),
            format!("\x02{SCAN}"),
            format!("\x02{}\x03", SCAN.replacen("sRA", "sSN", 1)),
            format!("\x02{}\x03", SCAN.replacen("LMDscandata", "DeviceIdent", 1)),
            format!("\x02{SCAN}\x02\x03"),
            format!("\x02{SCAN}\n\x03"),
        ] {
            let error = reply(frame.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("SICK CoLa"), "{error:#}");
        }
        let mut frame = vec![b'0'; MAX_FRAME];
        frame[0] = 2;
        let error = reply(&frame).unwrap_err();
        assert!(error.to_string().contains("64 KiB"));
        let mut input = io::Cursor::new(format!("\x02{SCAN}\x03extra"));
        let error = read_reply(
            |buffer| input.read(buffer),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("extra bytes"));
    }

    #[test]
    fn expired_deadline_prevents_io() {
        let error = read_reply(|_| panic!("I/O after deadline"), Instant::now()).unwrap_err();
        assert!(error.to_string().contains("deadline"));
    }
}
