//! Passive qualified Compact TCP telegrams, immutable raw commits and admitted native parts.
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage, with_details};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, Read as _},
    net::{SocketAddr, SocketAddrV4, TcpStream},
    path::PathBuf,
    time::{Duration, Instant},
};
mod context;
mod journal;
mod packet;
use context::Context;
use journal::Journal;
const MAX_JSON: usize = 1024 * 1024;
const MAX_READING_JSON: usize = MAX_JSON - 16 * 1024;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_PARTS: usize = 64;
const MAX_TOTAL_NATIVE: usize = 16 * 1024 * 1024;
const MAX_DIAGNOSTIC: usize = 16 * 1024;
const FAILURE_RESERVE: u64 = 4 * 1024 * 1024 + 16 * 1024;
const CLEANUP_RENDER: Duration = Duration::from_secs(30);
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Profile {
    Lrs4000,
    Multiscan200,
}
impl Profile {
    const fn name(self) -> &'static str {
        match self {
            Self::Lrs4000 => "lrs4000",
            Self::Multiscan200 => "multiscan200",
        }
    }
    const fn id(self) -> u16 {
        match self {
            Self::Lrs4000 => 1,
            Self::Multiscan200 => 2,
        }
    }
}
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stream {
    Points,
    Imu,
    Ambient,
}
impl Stream {
    const fn name(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Imu => "imu",
            Self::Ambient => "ambient",
        }
    }
    const fn id(self) -> u16 {
        match self {
            Self::Points => 1,
            Self::Imu => 2,
            Self::Ambient => 3,
        }
    }
    const fn minimum(self, profile: Profile) -> usize {
        match (profile, self) {
            (Profile::Lrs4000, _) => 36,
            (_, Self::Points) => 168,
            (_, Self::Imu) => 88,
            (_, Self::Ambient) => 86,
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddrV4,
    profile: Profile,
    stream: Stream,
    pub output_directory: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub max_bytes: u64,
    #[serde(default)]
    pub context: Option<PathBuf>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub min_free_bytes: Option<u64>,
    #[serde(default = "default_signal")]
    pub signal: String,
}
fn default_signal() -> String {
    "telegrams".into()
}
fn incomplete(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
fn before(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        Categorized::error(
            ErrorKind::Timeout,
            "Compact TCP original operation deadline expired"
        )
    );
    Ok(())
}
impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        super::validate_name(&self.signal, "Compact TCP signal")?;
        crate::naming::validate_output(&self.output_directory)?;
        ensure!(!self.address.ip().is_unspecified() && !self.address.ip().is_multicast() && !self.address.ip().is_broadcast() && self.address.port()!=0
            && self.output_directory!="-" && !self.output_directory.contains("://") && !self.output_directory.contains(['{','}']),
            Usage("Compact TCP requires an explicit unicast IPv4 endpoint and literal new output_directory".into()));
        ensure!(
            self.profile != Profile::Lrs4000 || self.stream == Stream::Points,
            Categorized::error(
                ErrorKind::Unsupported,
                "LRS4000 profile qualifies only the points stream"
            )
        );
        let minimum =
            journal::HEADER + journal::RECORD + journal::FOOTER + self.stream.minimum(self.profile);
        ensure!(self.max_bytes>=u64::try_from(minimum)?+FAILURE_RESERVE,Usage("Compact TCP max_bytes must include one telegram and reserved rejected-stream evidence".into()));
        Ok(())
    }
    fn format(&self) -> packet::Format {
        packet::Format {
            profile: self.profile,
            stream: self.stream,
        }
    }
}
pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    let context = Context::read(config.context.as_deref())?;
    let directory = std::path::absolute(&config.output_directory)?;
    ensure!(
        directory.file_name().is_some(),
        Usage("Compact TCP output_directory must name a new directory".into())
    );
    crate::naming::check_free(&directory)?;
    let origin =
        crate::host::monotonic_ns().context("Compact TCP requires a host monotonic clock")?;
    Ok(Box::new(Receiver {
        socket: None,
        address: config.address,
        format: config.format(),
        signal: config.signal.clone(),
        journal: Journal::new(directory, config, context, origin)?,
        pending: None,
        failed: None,
        finished: false,
        observed: None,
        cutoff: None,
        stream_bytes: 0,
        part_readings_released: 0,
        terminal: None,
    }))
}
struct Count {
    bytes: usize,
    deadline: Option<Instant>,
}
impl io::Write for Count {
    fn write(&mut self, raw: &[u8]) -> io::Result<usize> {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Compact TCP JSON deadline",
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(raw.len())
            .filter(|n| *n <= MAX_JSON)
            .ok_or_else(|| io::Error::other("Compact TCP JSON capacity"))?;
        Ok(raw.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn count_json(value: &impl Serialize, deadline: Option<Instant>) -> Result<usize> {
    let mut writer = Count { bytes: 0, deadline };
    let result = serde_json::to_writer(&mut writer, value);
    if let Some(deadline) = deadline {
        before(deadline)?;
    }
    result.map_err(|_| incomplete("Compact TCP Reading exceeds bounded JSON capacity"))?;
    Ok(writer.bytes)
}
#[derive(Serialize)]
struct ReadingRef<'a> {
    signal: &'a str,
    value: &'a Value,
}
#[derive(Clone)]
struct Failure {
    message: String,
    tag: Option<crate::types::ErrorTag>,
}
impl Failure {
    fn capture(error: &anyhow::Error) -> Self {
        Self {
            message: format!("{error:#}"),
            tag: crate::types::error_tag(error),
        }
    }
    fn error(&self) -> anyhow::Error {
        crate::types::error_from_tag(self.tag.clone(), self.message.clone())
    }
}
struct Pending {
    raw: Vec<u8>,
    decoded: packet::Decoded,
    archive: Value,
    received: u64,
    stream_offset: u64,
    next: usize,
}
struct Receiver {
    socket: Option<TcpStream>,
    address: SocketAddrV4,
    format: packet::Format,
    signal: String,
    journal: Journal,
    pending: Option<Pending>,
    failed: Option<Failure>,
    finished: bool,
    observed: Option<u64>,
    cutoff: Option<u64>,
    stream_bytes: u64,
    part_readings_released: u64,
    terminal: Option<Reading>,
}
impl Receiver {
    fn connect(&mut self, deadline: Instant) -> Result<()> {
        before(deadline)?;
        if self.socket.is_none() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| {
                    Categorized::error(ErrorKind::Timeout, "Compact TCP connect deadline expired")
                })?;
            let socket = TcpStream::connect_timeout(&SocketAddr::V4(self.address), remaining)
                .with_context(|| format!("connect Compact TCP {}", self.address))?;
            before(deadline)?;
            socket
                .set_nonblocking(false)
                .context("set Compact TCP blocking receive")?;
            ensure!(
                socket.peer_addr()? == SocketAddr::V4(self.address),
                incomplete("Compact TCP endpoint changed during connect")
            );
            self.socket = Some(socket);
        }
        Ok(())
    }
    fn wrapper(
        &self,
        archive: &Value,
        received: u64,
        stream_offset: u64,
        part: usize,
        parts: usize,
        native: Value,
    ) -> Value {
        let mut value = json!({"kind":"sick_compact_tcp_part","format_version":1,"telegram_index":archive["telegram_index"],"part_index":part,"part_count":parts,
            "received_monotonic_ns":received,"source_endpoint":self.address,"stream_offset":stream_offset,"archive":archive,"sensor":self.journal.sensor});
        value["native"] = native;
        if let Some(context) = &self.journal.context {
            value["context"] = context.clone();
        }
        value
    }
    fn admission(
        &self,
        decoded: &packet::Decoded,
        archive: &Value,
        received: u64,
        offset: u64,
        deadline: Instant,
    ) -> Result<()> {
        ensure!(
            (1..=MAX_PARTS).contains(&decoded.parts) && decoded.json_bytes.len() == decoded.parts,
            incomplete("Compact TCP decoder part inventory exceeds fixed capacity")
        );
        let mut native_total = 0_usize;
        for (index, bytes) in decoded.json_bytes.iter().enumerate() {
            before(deadline)?;
            native_total = native_total
                .checked_add(*bytes)
                .filter(|n| *n <= MAX_TOTAL_NATIVE)
                .ok_or_else(|| {
                    incomplete("Compact TCP total native materialization exceeds capacity")
                })?;
            let value = self.wrapper(archive, received, offset, index, decoded.parts, Value::Null);
            let count = count_json(
                &ReadingRef {
                    signal: &self.signal,
                    value: &value,
                },
                Some(deadline),
            )?
            .checked_sub(4)
            .and_then(|n| n.checked_add(*bytes));
            ensure!(
                count.is_some_and(|n| n <= MAX_READING_JSON),
                incomplete("Compact TCP native part and owner envelope exceed Reading capacity")
            );
        }
        before(deadline)
    }
    fn release(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        before(deadline)?;
        let pending = self
            .pending
            .as_ref()
            .context("Compact TCP pending telegram unavailable")?;
        let native = packet::materialize(&pending.decoded, &pending.raw, pending.next, deadline)?;
        before(deadline)?;
        let value = self.wrapper(
            &pending.archive,
            pending.received,
            pending.stream_offset,
            pending.next,
            pending.decoded.parts,
            native,
        );
        ensure!(
            count_json(
                &ReadingRef {
                    signal: &self.signal,
                    value: &value
                },
                Some(deadline)
            )? <= MAX_READING_JSON,
            incomplete("Compact TCP native part exceeded its exact admitted bound")
        );
        before(deadline)?;
        let pending = self
            .pending
            .as_mut()
            .context("Compact TCP pending telegram unavailable")?;
        let released = self
            .part_readings_released
            .checked_add(1)
            .context("Compact TCP part counter exhausted")?;
        self.observed = Some(pending.received);
        pending.next += 1;
        self.part_readings_released = released;
        if pending.next == pending.decoded.parts {
            self.pending = None;
        }
        Ok(vec![Reading {
            signal: self.signal.clone(),
            value,
            source_time: None,
        }])
    }
    fn committed(&mut self, pending: Pending, deadline: Instant) -> Result<Vec<Reading>> {
        self.pending = Some(pending);
        before(deadline)?;
        self.release(deadline)
    }
    fn acquire(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        ensure!(!self.finished, incomplete("Compact TCP owner is retired"));
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("Compact TCP timeout too large")?;
        before(deadline)?;
        if self.pending.is_some() {
            return self.release(deadline);
        }
        self.connect(deadline)?;
        let offset = self.stream_bytes;
        let mut raw = Vec::new();
        let mut scratch = vec![0_u8; 64 * 1024];
        let mut last_received = None;
        let mut framed = false;
        let prior = self.journal.telegrams;
        let result = (|| -> Result<bool> {
            loop {
                if Instant::now() >= deadline && raw.is_empty() {
                    return Ok(false);
                }
                before(deadline)?;
                let need = packet::frame_need(&raw, self.format, deadline)?;
                ensure!(
                    need.next <= MAX_FRAME && need.next >= raw.len(),
                    incomplete("Compact TCP framing exceeds fixed raw extent")
                );
                if need.complete && need.next == raw.len() {
                    framed = true;
                    break;
                }
                ensure!(
                    need.next > raw.len(),
                    incomplete("Compact TCP framing did not advance")
                );
                let old = raw.len();
                before(deadline)?;
                let socket = self
                    .socket
                    .as_mut()
                    .context("Compact TCP epoch was retired")?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    if raw.is_empty() {
                        return Ok(false);
                    }
                    return Err(Categorized::error(
                        ErrorKind::Timeout,
                        "Compact TCP partial frame exceeded the original deadline",
                    ));
                }
                socket.set_read_timeout(Some(remaining))?;
                raw.try_reserve_exact(need.next - old)
                    .context("reserve bounded Compact TCP frame")?;
                let count = (need.next - old).min(scratch.len());
                let read = socket.read(&mut scratch[..count]);
                match read {
                    Ok(0) => {
                        raw.truncate(old);
                        return Err(Categorized::error(
                            if raw.is_empty() {
                                ErrorKind::Disconnected
                            } else {
                                ErrorKind::Incomplete
                            },
                            "Compact TCP peer closed at an incomplete stream boundary",
                        ));
                    }
                    Ok(length) => {
                        raw.extend_from_slice(&scratch[..length]);
                        framed = need.complete && raw.len() == need.next;
                        self.stream_bytes = self
                            .stream_bytes
                            .checked_add(u64::try_from(length)?)
                            .context("Compact TCP stream offset exhausted")?;
                        last_received = Some(crate::host::monotonic_ns().ok_or_else(|| {
                            incomplete("Compact TCP complete-read clock unavailable")
                        })?);
                    }
                    Err(error) => {
                        raw.truncate(old);
                        match error.kind() {
                            io::ErrorKind::Interrupted => (),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                                if raw.is_empty() =>
                            {
                                return Ok(false);
                            }
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                                return Err(Categorized::error(
                                    ErrorKind::Timeout,
                                    "Compact TCP partial frame exceeded the original deadline",
                                ));
                            }
                            _ => {
                                return Err(anyhow::Error::from(error)
                                    .context("receive Compact TCP stream"));
                            }
                        }
                    }
                }
            }
            let received =
                last_received.context("Compact TCP frame has no original complete-read clock")?;
            ensure!(
                self.cutoff.is_none_or(|end| received <= end),
                Categorized::error(
                    ErrorKind::Timeout,
                    "Compact TCP frame completed beyond acquisition cutoff"
                )
            );
            before(deadline)?;
            let decoded = packet::validate(&raw, self.format, deadline)?;
            before(deadline)?;
            let archive = self.journal.descriptor(&raw, received, offset)?;
            self.admission(&decoded, &archive, received, offset, deadline)?;
            before(deadline)?;
            self.journal
                .commit(&raw, received, offset, decoded.parts, Some(deadline))?;
            self.pending = Some(Pending {
                raw: std::mem::take(&mut raw),
                decoded,
                archive,
                received,
                stream_offset: offset,
                next: 0,
            });
            Ok(true)
        })();
        match result {
            Ok(false) => Ok(Vec::new()),
            Ok(true) => {
                let pending = self
                    .pending
                    .take()
                    .context("Compact TCP committed plan unavailable")?;
                self.committed(pending, deadline)
            }
            Err(mut primary) => {
                if self.journal.telegrams == prior
                    && let Err(evidence) =
                        self.journal
                            .reject(&raw, last_received, offset, framed, &primary)
                {
                    primary = primary.context(format!(
                        "Compact TCP stream evidence was not completely persisted: {evidence:#}"
                    ));
                }
                Err(primary)
            }
        }
    }
    fn prefix(&self) -> Value {
        let mut value = self.journal.prefix();
        value["stream_bytes_consumed"] = json!(self.stream_bytes);
        value["part_readings_released"] = json!(self.part_readings_released);
        value["pending_parts"] = json!(
            self.pending
                .as_ref()
                .map_or(0, |p| p.decoded.parts - p.next)
        );
        value
    }
    fn remember(&mut self, error: anyhow::Error) -> anyhow::Error {
        self.socket = None;
        let error = with_details(error, json!({"sick_compact_tcp":self.prefix()}));
        if self.failed.is_none() {
            self.failed = Some(Failure::capture(&error));
        }
        error
    }
    fn drain_until(&mut self, deadline: Instant) -> Result<Option<Reading>> {
        let result = (|| -> Result<Option<Reading>> {
            if self.pending.is_some() {
                let mut readings = self.release(deadline)?;
                return Ok(readings.pop());
            }
            if let Some(mut reading) = self.terminal.take() {
                reading.value["part_readings_released"] = json!(self.part_readings_released);
                reading.value["pending_parts"] = json!(0);
                return Ok(Some(reading));
            }
            Ok(None)
        })();
        result.map_err(|error| self.remember(error))
    }
}
impl Source for Receiver {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if let Some(error) = &self.failed {
            return Err(error.error());
        }
        self.acquire(timeout).map_err(|error| self.remember(error))
    }
    fn ready(&self) -> bool {
        self.journal.telegrams > 0
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn buffered_timestamps(&self) -> bool {
        true
    }
    fn set_acquisition_end(&mut self, end: Option<u64>) {
        self.cutoff = end;
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.socket = None;
        let began = Instant::now();
        let closed = self.journal.finish();
        if self.journal.telegrams > 0 {
            let mut value = self.prefix();
            value["storage_finalized"] = json!(closed.is_ok());
            self.terminal = Some(Reading {
                signal: self.signal.clone(),
                value,
                source_time: None,
            });
        }
        let result = closed.and_then(|()| {
            ensure!(
                began.elapsed() <= timeout,
                Categorized::error(
                    ErrorKind::Timeout,
                    "Compact TCP close completed after cleanup deadline; stored prefix retained"
                )
            );
            Ok(())
        });
        result.map_err(|error| self.remember(error))
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        let deadline = Instant::now()
            .checked_add(CLEANUP_RENDER)
            .context("Compact TCP cleanup deadline overflow")?;
        self.drain_until(deadline)
    }
}
#[cfg(test)]
mod tests;
