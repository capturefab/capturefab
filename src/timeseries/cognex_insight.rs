//! Read-only Cognex In-Sight Native Mode `GV` over Telnet.
//! <https://docs.cognex.com/is_651/web/EN/ise/Content/Communications_Reference/GetValue_Spreadsheet.htm>
//! <https://docs.cognex.com/isvs_500/web/EN/InSight_Sheet/Content/Topics/IndustrialCommunications/GetValue.htm>

use super::{Reading, Source, net, validate_name};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Number, Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};
use zeroize::{Zeroize as _, Zeroizing};

const CRLF: &[u8] = b"\r\n";
const MAX_LOGIN: usize = 4096;
const MAX_CONTROL: usize = 1024;
const MAX_VALUE: usize = 16 * 1024;
const MAX_BATCH: usize = 1 << 20;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    username_env: String,
    password_env: String,
    signals: Vec<Signal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    cell: String,
    #[serde(default)]
    format: Format,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Format {
    #[default]
    Text,
    Number,
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.address.port() == 0 || !(1..=256).contains(&config.signals.len()) {
        return Err(
            Usage("Cognex In-Sight needs a nonzero TCP port and 1–256 GV signals".into()).into(),
        );
    }
    for env in [&config.username_env, &config.password_env] {
        if env.is_empty() || env.len() > 256 || env.contains(['=', '\0']) {
            return Err(Usage("invalid Cognex credential environment variable name".into()).into());
        }
    }
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        validate_name(&signal.name, "Cognex signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("Cognex signal names must be unique".into()).into());
        }
        if !valid_cell(&signal.cell) {
            return Err(Usage(
                "Cognex GV cell needs a legacy address A000–Z399 or a published ASCII name".into(),
            )
            .into());
        }
    }
    Ok(Box::new(Capture {
        config: config.clone(),
        link: None,
    }))
}

fn valid_cell(cell: &str) -> bool {
    if cell.len() > 127 {
        return false;
    }
    let Some((&first, rest)) = cell.as_bytes().split_first() else {
        return false;
    };
    if first.is_ascii_alphabetic() && !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) {
        // A cell location always has three row digits; the first is 0–3.
        return matches!(rest, [b'0'..=b'3', _, _]);
    }
    // Published cells and legacy EasyBuilder tags such as
    // Job.Robot.FormatString. Restrict syntax before concatenating GV.
    (first.is_ascii_alphabetic() || first == b'_')
        && cell.split('.').all(|part| {
            part.as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

struct Capture {
    config: Config,
    link: Option<Telnet>,
}

impl Source for Capture {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now() + timeout;
        let mut link = self
            .link
            .take()
            .map_or_else(|| Telnet::connect(&self.config, deadline), Ok)?;
        link.wire.set_deadline(deadline);
        let mut result = Vec::with_capacity(self.config.signals.len());
        let mut size = 2;
        for signal in &self.config.signals {
            let reading = link.read_value(signal)?;
            size += usize::from(!result.is_empty()) + serde_json::to_vec(&reading)?.len();
            if size > MAX_BATCH {
                return Err(protocol("serialized poll exceeds 1 MiB"));
            }
            remaining(deadline)?;
            result.push(reading);
        }
        link.control_left = MAX_CONTROL;
        self.link = Some(link);
        Ok(result)
    }

    fn end_timeout_is_idle(&self) -> bool {
        false
    }
}

fn credential(name: &str, minimum: usize) -> Result<Zeroizing<String>> {
    let value = Zeroizing::new(std::env::var(name).map_err(|_| {
        Categorized::error(
            ErrorKind::AuthRequired,
            "Cognex credential environment variable is missing or is not UTF-8",
        )
    })?);
    if !(minimum..=1024).contains(&value.len())
        || !value.bytes().all(|byte| (b' '..=b'~').contains(&byte))
    {
        return Err(Usage(
            "Cognex credential must be bounded printable ASCII without control characters".into(),
        )
        .into());
    }
    crate::scrub::register(&value);
    Ok(value)
}

struct Telnet {
    wire: Box<dyn net::Transport>,
    incoming: Zeroizing<[u8; 512]>,
    buffered: std::ops::Range<usize>,
    control_left: usize,
}

impl Telnet {
    fn connect(config: &Config, deadline: Instant) -> Result<Self> {
        let username = credential(&config.username_env, 1)?;
        let password = credential(&config.password_env, 0)?;
        let socket = TcpStream::connect_timeout(&config.address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        socket.set_nodelay(true).map_err(|error| io_error(&error))?;
        let mut link = Self {
            wire: net::plain(socket, deadline),
            incoming: Zeroizing::new([0; 512]),
            buffered: 0..0,
            control_left: MAX_CONTROL,
        };
        link.login(&username, &password)?;
        Ok(link)
    }

    fn login(&mut self, username: &str, password: &str) -> Result<()> {
        let banner = self.prompt(b"User:", false)?;
        validate_banner(&banner)?;
        self.send_credential(username)?;
        let echo = self.prompt(b"Password:", true)?;
        if !echo.trim_ascii().is_empty() && echo.trim_ascii() != username.as_bytes().trim_ascii() {
            return Err(auth_rejected());
        }
        self.send_credential(password)?;
        self.confirm_login()
    }

    fn send_credential(&mut self, credential: &str) -> Result<()> {
        let mut request = Zeroizing::new(Vec::with_capacity(credential.len() + CRLF.len()));
        request.extend_from_slice(credential.as_bytes());
        request.extend_from_slice(CRLF);
        self.write(&request)
    }

    fn prompt(&mut self, expected: &[u8], after_username: bool) -> Result<Zeroizing<Vec<u8>>> {
        let mut bytes = Zeroizing::new(Vec::new());
        for _ in 0..MAX_LOGIN {
            bytes.push(self.data_byte()?);
            if bytes.ends_with(expected) {
                let end = bytes.len() - expected.len();
                bytes.truncate(end);
                return Ok(bytes);
            }
            if after_username && bytes.ends_with(b"User:") {
                return Err(auth_rejected());
            }
        }
        Err(protocol("login prompt exceeds its byte limit"))
    }

    fn confirm_login(&mut self) -> Result<()> {
        let mut line = Zeroizing::new(Vec::new());
        for _ in 0..MAX_LOGIN {
            let byte = self.data_byte()?;
            line.push(byte);
            if line.ends_with(b"User:") || line.ends_with(b"Password:") {
                return Err(auth_rejected());
            }
            if byte == b'\n' {
                let reply = line
                    .strip_suffix(CRLF)
                    .ok_or_else(auth_rejected)?
                    .trim_ascii();
                if reply == b"User Logged In" {
                    return Ok(());
                }
                if !reply.is_empty() {
                    return Err(auth_rejected());
                }
                line.zeroize();
            }
        }
        Err(auth_rejected())
    }

    fn read_value(&mut self, signal: &Signal) -> Result<Reading> {
        self.check_buffered()?;
        self.write(format!("GV{}\r\n", signal.cell).as_bytes())?;
        let status = self.line(12)?;
        if status.as_slice() != b"1" {
            let code = str::from_utf8(&status)
                .ok()
                .and_then(|status| status.parse::<i32>().ok())
                .filter(|code| code.to_string().as_bytes() == status.as_slice());
            return Err(code.map_or_else(
                || protocol("invalid GV status line"),
                |code| {
                    Categorized::new(
                        ErrorKind::OperationFailed,
                        "Cognex In-Sight GV was rejected",
                    )
                    .details(json!({"native_status":code}))
                    .into()
                },
            ));
        }
        let raw = self.line(MAX_VALUE)?;
        self.check_buffered()?;
        Ok(Reading {
            signal: signal.name.clone(),
            value: value(signal, &raw)?,
            source_time: None,
        })
    }

    fn check_buffered(&self) -> Result<()> {
        // A buffered second application reply cannot belong to a command
        // we have not sent. Telnet controls can arrive between commands.
        if self
            .buffered
            .clone()
            .next()
            .and_then(|index| self.incoming.get(index))
            .is_some_and(|&byte| byte != IAC)
        {
            return Err(protocol("unexpected application data at GV boundary"));
        }
        Ok(())
    }

    fn line(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        // Bound both terminated and unterminated lines, allowing a CR at
        // exactly limit bytes before consuming its required LF.
        for _ in 0..=limit {
            let byte = self.data_byte()?;
            if byte == b'\r' {
                if self.data_byte()? != b'\n' {
                    return Err(protocol("GV lines require CRLF"));
                }
                return Ok(bytes);
            }
            if byte == b'\n' {
                return Err(protocol("GV lines require CRLF"));
            }
            bytes.push(byte);
        }
        Err(protocol("GV line exceeds its byte limit"))
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.wire.write_all(bytes).map_err(|error| io_error(&error))
    }

    fn raw_byte(&mut self) -> Result<u8> {
        while self.buffered.is_empty() {
            match io::Read::read(&mut self.wire, &mut *self.incoming) {
                Ok(0) => {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "Cognex In-Sight closed the connection before the reply completed",
                    ));
                }
                Ok(count) => self.buffered = 0..count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(&error)),
            }
        }
        self.buffered
            .next()
            .and_then(|index| self.incoming.get_mut(index))
            .map(std::mem::take)
            .ok_or_else(|| protocol("internal Telnet receive buffer exhausted"))
    }

    fn control(&mut self) -> Result<()> {
        self.control_left = self
            .control_left
            .checked_sub(1)
            .ok_or_else(|| protocol("Telnet control data exceeds its byte limit"))?;
        Ok(())
    }

    fn control_byte(&mut self) -> Result<u8> {
        self.control()?;
        self.raw_byte()
    }

    fn data_byte(&mut self) -> Result<u8> {
        loop {
            let byte = self.raw_byte()?;
            if byte != IAC {
                return Ok(byte);
            }
            self.control()?;
            match self.control_byte()? {
                IAC => return Ok(IAC),
                WILL => {
                    let option = self.control_byte()?;
                    self.write(&[IAC, DONT, option])?;
                }
                DO => {
                    let option = self.control_byte()?;
                    self.write(&[IAC, WONT, option])?;
                }
                WONT | DONT => {
                    self.control_byte()?;
                }
                SB => self.skip_subnegotiation()?,
                NOP | GA => {}
                _ => return Err(protocol("unsupported or malformed Telnet control")),
            }
        }
    }

    fn skip_subnegotiation(&mut self) -> Result<()> {
        self.control_byte()?; // Option code.
        loop {
            if self.control_byte()? == IAC {
                match self.control_byte()? {
                    SE => return Ok(()),
                    IAC => {}
                    _ => return Err(protocol("malformed Telnet subnegotiation")),
                }
            }
        }
    }
}

// RFC 854 / 855. Remain in the NVT mode and refuse every requested option.
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const GA: u8 = 249;
const NOP: u8 = 241;
const SE: u8 = 240;

fn validate_banner(banner: &[u8]) -> Result<()> {
    let model = banner
        .trim_ascii_start()
        .strip_suffix(CRLF)
        .and_then(|banner| banner.strip_prefix(b"Welcome to In-Sight"))
        .ok_or_else(|| protocol("expected Cognex In-Sight welcome and user prompt"))?;
    let model = model.strip_prefix(b"(tm)").unwrap_or(model);
    if model
        .first()
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        return Err(protocol("expected Cognex In-Sight welcome and user prompt"));
    }
    let model = model
        .trim_ascii_start()
        .split(u8::is_ascii_whitespace)
        .next()
        .unwrap_or_default();
    if model.starts_with(b"2000")
        || model
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"IS2000"))
    {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "In-Sight 2000 GV requires Serial Native",
        ));
    }
    Ok(())
}

fn value(signal: &Signal, raw: &[u8]) -> Result<Value> {
    let mut value = json!({"cell":signal.cell,"command_status":1});
    if raw.is_empty() {
        value["state"] = json!("empty");
        value["text"] = json!("");
        return Ok(value);
    }
    let text = str::from_utf8(raw)
        .ok()
        .filter(|text| !text.chars().any(char::is_control));
    match (signal.format, text) {
        (Format::Text, Some(text)) => {
            value["state"] = json!("available");
            value["text"] = json!(text);
        }
        (Format::Text, None) => {
            value["state"] = json!("available");
            value["raw"] = json!({"encoding":"base64","data":STANDARD.encode(raw)});
        }
        (Format::Number, Some(text @ ("#" | "#ERR"))) => {
            value["state"] = json!("device_marker");
            value["text"] = json!(text);
        }
        (Format::Number, Some(text)) => {
            let number = text
                .trim_ascii()
                .parse::<Number>()
                .map_err(|_| protocol("GV value does not contain an exact JSON number"))?;
            value["state"] = json!("available");
            value["text"] = json!(text);
            value["number"] = json!(number);
        }
        (Format::Number, None) => return Err(protocol("GV numeric value is not text")),
    }
    Ok(value)
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "Cognex In-Sight deadline expired",
        ));
    }
    Ok(left)
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("Cognex In-Sight: {message}"),
    )
}

fn auth_rejected() -> anyhow::Error {
    Categorized::error(
        ErrorKind::AuthRejected,
        "Cognex In-Sight did not accept the login",
    )
}

fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::ConnectionRefused => ErrorKind::Unavailable,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("Cognex In-Sight TCP: {error}"))
}
