//! Passive explicitly qualified `SICK Compact` datagrams and independently committed raw groups.
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage, with_details};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    path::PathBuf,
    time::{Duration, Instant},
};

mod context;
mod journal;
mod packet;
use context::Context;
use journal::{FOOTER, GROUP_HEADER, Journal, RECORD_HEADER};

const MAX_JSON: usize = 1024 * 1024;
// The adapter cannot see source id or the recorder's Sample timing wrapper.
// Shared bounded names and all scalar wrapper fields fit well within this reserve.
const MAX_VALUE_JSON: usize = MAX_JSON - 16 * 1024;
const MAX_RAW_GROUP: usize = 4 * 1024 * 1024;
const MAX_PACKETS: u16 = 32;
const MAX_FOREIGN_BYTES: usize = 64 * 1024;
const MAX_FOREIGN_PACKETS: usize = 128;
const GROUP_WINDOW: Duration = Duration::from_millis(100);
const MAX_PACKET: usize = 65_507;
const MAX_EVIDENCE_JSON: usize = 16 * 1024;
const FAILURE_RESERVE: u64 = 81_891;
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stream {
    Points,
    Imu,
    Encoder,
    Mixed,
}
impl Stream {
    const fn name(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Imu => "imu",
            Self::Encoder => "encoder",
            Self::Mixed => "mixed",
        }
    }
    const fn id(self) -> u16 {
        match self {
            Self::Points => 1,
            Self::Imu => 2,
            Self::Encoder => 3,
            Self::Mixed => 4,
        }
    }
    const fn minimum_packet(self) -> usize {
        match self {
            Self::Points | Self::Mixed => 36,
            Self::Imu => 64,
            Self::Encoder => 88,
        }
    }
}
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Profile {
    Multiscan100,
    Picoscan100,
    Picoscan150,
}
impl Profile {
    const fn name(self) -> &'static str {
        match self {
            Self::Multiscan100 => "multiscan100",
            Self::Picoscan100 => "picoscan100",
            Self::Picoscan150 => "picoscan150",
        }
    }
    const fn id(self) -> u16 {
        match self {
            Self::Multiscan100 => 1,
            Self::Picoscan100 => 2,
            Self::Picoscan150 => 3,
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub listen: SocketAddrV4,
    pub source_ip: Ipv4Addr,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub source_port: Option<u16>,
    profile: Profile,
    stream: Stream,
    #[serde(default)]
    pub context: Option<PathBuf>,
    pub output_directory: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub max_bytes: u64,
    #[serde(
        default = "default_packets",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub packets_per_group: u16,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub min_free_bytes: Option<u64>,
    #[serde(default = "default_signal")]
    pub signal: String,
}
const fn default_packets() -> u16 {
    MAX_PACKETS
}
fn default_signal() -> String {
    "datagrams".into()
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
// This bounded marker classification does not validate ignored CRC or native layout.
fn other_stream(raw: &[u8], profile: Profile, stream: Stream) -> bool {
    let Some([2, 2, 2, 2, kind, 0, 0, 0]) = raw.first_chunk::<8>().copied() else {
        return false;
    };
    let known = matches!(kind, 1 | 2) || (profile == Profile::Picoscan150 && kind == 4);
    let selected = match stream {
        Stream::Points => kind == 1,
        Stream::Imu => kind == 2,
        Stream::Encoder => kind == 4,
        Stream::Mixed => matches!(kind, 1 | 2 | 4),
    };
    known && !selected
}
fn unicast(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast()
}
impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !matches!(self.stream, Stream::Encoder | Stream::Mixed)
                || self.profile == Profile::Picoscan150,
            Categorized::error(
                ErrorKind::Unsupported,
                "SICK Compact encoder and mixed streams require explicit picoscan150 profile"
            )
        );
        super::validate_name(&self.signal, "SICK Compact signal")?;
        crate::naming::validate_output(&self.output_directory)?;
        ensure!(self.listen.port() != 0 && !self.listen.ip().is_multicast() && !self.listen.ip().is_broadcast()
            && unicast(self.source_ip) && self.source_port != Some(0)
            && (1..=MAX_PACKETS).contains(&self.packets_per_group)
            && self.output_directory != "-" && !self.output_directory.contains("://") && !self.output_directory.contains(['{','}']),
            Usage("SICK Compact requires explicit IPv4 listen/source, literal local output_directory and packets_per_group 1..32".into()));
        let minimum = journal::FILE_HEADER
            + GROUP_HEADER
            + RECORD_HEADER
            + self.stream.minimum_packet()
            + FOOTER;
        ensure!(
            self.max_bytes >= u64::try_from(minimum)? + FAILURE_RESERVE,
            Usage(
                "SICK Compact max_bytes must allow an archive packet and reserved failure evidence"
                    .into()
            )
        );
        Ok(())
    }
}
pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    let context = Context::read(config.context.as_deref())?;
    let directory = std::path::absolute(&config.output_directory)?;
    ensure!(
        directory.file_name().is_some(),
        Usage("SICK Compact output_directory must name a new local directory".into())
    );
    crate::naming::check_free(&directory)?;
    let origin = crate::host::monotonic_ns()
        .context("SICK Compact requires a host monotonic receive clock")?;
    let journal = Journal::new(directory, config, context, origin)?;
    let socket = UdpSocket::bind(config.listen).context("bind SICK Compact UDP listener")?;
    Ok(Box::new(Receiver {
        socket: Some(socket),
        source_ip: config.source_ip,
        source_port: config.source_port,
        signal: config.signal.clone(),
        packets_per_group: config.packets_per_group,
        stream: config.stream,
        profile: config.profile,
        journal,
        pending: None,
        drain_pending_after_failure: false,
        observed: None,
        cutoff: None,
        terminal: VecDeque::new(),
        failed: None,
        finished: false,
        foreign_packets: 0,
        other_stream_packets: 0,
        uncommitted_packets: 0,
    }))
}
fn before(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        Categorized::error(
            ErrorKind::Timeout,
            "SICK Compact original read deadline expired"
        )
    );
    Ok(())
}
struct Count {
    bytes: usize,
    deadline: Option<Instant>,
}
impl io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SICK Compact JSON admission deadline expired",
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_JSON)
            .ok_or_else(|| io::Error::other("SICK Compact native JSON exceeds 1 MiB"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn count_json(value: &impl Serialize, deadline: Option<Instant>) -> Result<usize> {
    let mut count = Count { bytes: 0, deadline };
    serde_json::to_writer(&mut count, value).map_err(|error| {
        if deadline.is_some_and(|until| Instant::now() >= until) {
            Categorized::error(
                ErrorKind::Timeout,
                "SICK Compact JSON admission deadline expired",
            )
        } else {
            Categorized::error(
                ErrorKind::Incomplete,
                format!("SICK Compact native JSON exceeds its bounded admission: {error}"),
            )
        }
    })?;
    Ok(count.bytes)
}

#[derive(Serialize)]
struct ReadingRef<'a> {
    signal: &'a str,
    value: &'a Value,
}
#[derive(Debug)]
struct Received {
    raw: Vec<u8>,
    value: Value,
    received: u64,
    source: SocketAddrV4,
}
struct Group {
    raw: Vec<u8>,
    values: Vec<Value>,
    json_bytes: usize,
    first_received: u64,
    last_received: u64,
}
impl Group {
    fn new() -> Self {
        Self {
            raw: vec![0; GROUP_HEADER],
            values: Vec::new(),
            json_bytes: 0,
            first_received: 0,
            last_received: 0,
        }
    }
    fn offer(
        &mut self,
        mut packet: Received,
        journal: &Journal,
        signal: &str,
        limit: u16,
        deadline: Option<Instant>,
    ) -> Result<Option<Received>> {
        let count = self.values.len() + 1;
        let raw_bytes = self
            .raw
            .len()
            .checked_add(RECORD_HEADER + packet.raw.len())
            .context("SICK Compact group extent overflow")?;
        let index = journal
            .packets
            .checked_add(u64::try_from(count)?)
            .context("SICK Compact packet index exhausted")?;
        let raw_offset = journal
            .next_offset()?
            .checked_add(u64::try_from(self.raw.len() + RECORD_HEADER)?)
            .context("SICK Compact raw offset overflow")?;
        packet.value["host_packet_index"] = json!(index);
        packet.value["received_monotonic_ns"] = json!(packet.received);
        packet.value["source"] = json!(packet.source);
        packet.value["raw_offset"] = json!(raw_offset);
        packet.value["raw_length"] = json!(packet.raw.len());
        let bytes = count_json(&packet.value, deadline)?;
        let first_received = if self.values.is_empty() {
            packet.received
        } else {
            self.first_received
        };
        let skeleton = journal.descriptor(count, raw_bytes, first_received, packet.received)?;
        let total = count_json(
            &ReadingRef {
                signal,
                value: &skeleton,
            },
            deadline,
        )?
        .checked_add(self.json_bytes)
        .and_then(|total| total.checked_add(bytes))
        .and_then(|total| total.checked_add(count - 1))
        .context("SICK Compact JSON bound overflow")?;
        if self.values.is_empty() && !journal.room_for(raw_bytes) {
            return Err(Categorized::error(
                ErrorKind::StorageFull,
                "SICK Compact archive reached max_bytes",
            ));
        }
        let fits = count <= usize::from(limit)
            && raw_bytes + FOOTER <= MAX_RAW_GROUP
            && total <= MAX_VALUE_JSON
            && journal.room_for(raw_bytes);
        if !fits {
            ensure!(
                !self.values.is_empty(),
                incomplete("one SICK Compact packet exceeds group, JSON, or max_bytes admission")
            );
            return Ok(Some(packet));
        }
        self.raw.extend_from_slice(b"PKT1");
        self.raw
            .extend_from_slice(&u32::try_from(packet.raw.len())?.to_le_bytes());
        self.raw.extend_from_slice(&index.to_le_bytes());
        self.raw.extend_from_slice(&packet.received.to_le_bytes());
        self.raw.extend_from_slice(&packet.source.ip().octets());
        self.raw
            .extend_from_slice(&packet.source.port().to_le_bytes());
        self.raw.extend_from_slice(&[0; 2]);
        self.raw.extend_from_slice(&packet.raw);
        self.values.push(packet.value);
        self.json_bytes += bytes;
        self.first_received = first_received;
        self.last_received = packet.received;
        Ok(None)
    }
}
struct Receiver {
    socket: Option<UdpSocket>,
    source_ip: Ipv4Addr,
    source_port: Option<u16>,
    signal: String,
    packets_per_group: u16,
    stream: Stream,
    profile: Profile,
    journal: Journal,
    pending: Option<Received>,
    drain_pending_after_failure: bool,
    observed: Option<u64>,
    cutoff: Option<u64>,
    terminal: VecDeque<Reading>,
    failed: Option<Failure>,
    finished: bool,
    foreign_packets: u64,
    other_stream_packets: u64,
    uncommitted_packets: usize,
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
impl Receiver {
    fn finish_group(&mut self, group: Group, deadline: Option<Instant>) -> Result<Vec<Reading>> {
        if group.values.is_empty() {
            return Ok(Vec::new());
        }
        self.observed = Some(group.last_received);
        self.uncommitted_packets = group.values.len();
        let value = self.journal.commit(group, &self.signal, deadline)?;
        self.uncommitted_packets = 0;
        let reading = Reading {
            signal: self.signal.clone(),
            value,
            source_time: None,
        };
        if deadline.is_some_and(|until| Instant::now() >= until) {
            self.drain_pending_after_failure = self.pending.is_some();
            self.terminal.push_back(reading);
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "SICK Compact commit completed after the original read deadline; committed prefix retained for drain",
            ));
        }
        Ok(vec![reading])
    }
    fn retain_group(&mut self, mut error: anyhow::Error, group: Group) -> anyhow::Error {
        match self.finish_group(group, None) {
            Ok(readings) => self.terminal.extend(readings),
            Err(commit) => {
                error = error.context(format!("accepted prefix commit also failed: {commit:#}"));
            }
        }
        error
    }
    fn rejected(
        &mut self,
        error: anyhow::Error,
        raw: &[u8],
        received: u64,
        source: SocketAddrV4,
        group: Group,
    ) -> anyhow::Error {
        // Previously accepted datagrams remain a valid cleanup prefix. No new receive occurs.
        let mut primary = error;
        match self.finish_group(group, None) {
            Ok(readings) => self.terminal.extend(readings),
            Err(commit) => {
                primary =
                    primary.context(format!("accepted prefix commit also failed: {commit:#}"));
            }
        }
        if let Err(evidence) = self.journal.reject(raw, received, source, &primary) {
            primary = primary.context(format!(
                "SICK Compact rejected-datagram evidence was not completely persisted: {evidence:#}"
            ));
        }
        primary
    }
    fn acquire(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if let Some(error) = &self.failed {
            return Err(error.error());
        }
        ensure!(
            !self.finished,
            incomplete("SICK Compact source was retired")
        );
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("SICK Compact timeout is too large")?;
        let mut group = Group::new();
        if let Some(packet) = self.pending.take() {
            self.uncommitted_packets = 1;
            ensure!(
                group
                    .offer(
                        packet,
                        &self.journal,
                        &self.signal,
                        self.packets_per_group,
                        None
                    )?
                    .is_none(),
                "pending SICK Compact packet does not fit an empty group"
            );
            return self.finish_group(group, None);
        }
        before(deadline)?;
        let mut group_end = deadline;
        let mut foreign = (0_usize, 0_usize);
        let mut raw = vec![0; 65_536].into_boxed_slice();
        loop {
            let remaining = group_end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let socket = self
                .socket
                .as_ref()
                .context("SICK Compact source was retired")?;
            if let Err(error) = socket.set_read_timeout(Some(remaining)) {
                return Err(self.retain_group(
                    anyhow::Error::from(error).context("set SICK Compact receive timeout"),
                    group,
                ));
            }
            let (length, source) = match socket.recv_from(&mut raw) {
                Ok(received) => received,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    break;
                }
                Err(error) => {
                    return Err(self.retain_group(
                        anyhow::Error::from(error).context("receive SICK Compact datagram"),
                        group,
                    ));
                }
            };
            let received = match crate::host::monotonic_ns()
                .context("SICK Compact receive clock unavailable")
            {
                Ok(received) => received,
                Err(error) => return Err(self.retain_group(error, group)),
            };
            if self.cutoff.is_some_and(|cutoff| received > cutoff) {
                break;
            }
            let SocketAddr::V4(source) = source else {
                return Err(self.retain_group(
                    incomplete("SICK Compact listener returned a non-IPv4 source"),
                    group,
                ));
            };
            let wrong_peer = source.ip() != &self.source_ip
                || self.source_port.is_some_and(|port| port != source.port());
            if wrong_peer || other_stream(&raw[..length], self.profile, self.stream) {
                foreign.0 += 1;
                foreign.1 += length;
                let counter = if wrong_peer {
                    &mut self.foreign_packets
                } else {
                    &mut self.other_stream_packets
                };
                let Some(next) = counter.checked_add(1) else {
                    return Err(self
                        .retain_group(incomplete("SICK Compact ignored count exhausted"), group));
                };
                *counter = next;
                if foreign.0 > MAX_FOREIGN_PACKETS || foreign.1 > MAX_FOREIGN_BYTES {
                    return Err(self.retain_group(
                        incomplete(
                            "SICK Compact foreign-source traffic exceeded bounded allowance",
                        ),
                        group,
                    ));
                }
                continue;
            }
            let bytes = &raw[..length];
            let value = match packet::decode(bytes, self.stream, deadline) {
                Ok(value) => value,
                Err(error) => return Err(self.rejected(error, bytes, received, source, group)),
            };
            let packet = Received {
                raw: bytes.to_vec(),
                value,
                received,
                source,
            };
            let was_empty = group.values.is_empty();
            self.uncommitted_packets = group.values.len() + 1;
            self.pending = match group.offer(
                packet,
                &self.journal,
                &self.signal,
                self.packets_per_group,
                Some(deadline),
            ) {
                Ok(pending) => pending,
                Err(error) => return Err(self.retain_group(error, group)),
            };
            self.uncommitted_packets = group.values.len();
            if self.pending.is_some()
                || group.values.len() == usize::from(self.packets_per_group)
                || Instant::now() >= group_end
            {
                break;
            }
            if was_empty {
                group_end = deadline.min(Instant::now() + GROUP_WINDOW);
            }
        }
        // A receive timeout is a healthy idle read. Accepted data keeps its actual receive clock.
        self.finish_group(group, Some(deadline))
    }
    fn prefix(&self) -> Value {
        let mut prefix = self.journal.prefix();
        prefix["foreign_packets"] = json!(self.foreign_packets);
        prefix["other_stream_packets"] = json!(self.other_stream_packets);
        prefix["uncommitted_packets"] = json!(self.uncommitted_packets);
        prefix["pending_accepted_packets"] = json!(usize::from(self.pending.is_some()));
        prefix
    }
}
impl Source for Receiver {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if let Some(error) = &self.failed {
            return Err(error.error());
        }
        self.acquire(timeout).map_err(|error| {
            self.socket = None;
            let error = with_details(error, json!({"sick_compact_udp":self.prefix()}));
            self.failed = Some(Failure::capture(&error));
            error
        })
    }
    fn ready(&self) -> bool {
        self.journal.groups > 0
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
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.socket = None;
        let saved = if (self.failed.is_none() || self.drain_pending_after_failure)
            && let Some(packet) = self.pending.take()
        {
            self.drain_pending_after_failure = false;
            self.uncommitted_packets = 1;
            let mut group = Group::new();
            group
                .offer(
                    packet,
                    &self.journal,
                    &self.signal,
                    self.packets_per_group,
                    None,
                )
                .and_then(|pending| {
                    ensure!(
                        pending.is_none(),
                        "pending SICK Compact packet does not fit"
                    );
                    self.finish_group(group, None)
                })
        } else {
            Ok(Vec::new())
        };
        let closed = self.journal.finish();
        let primary = match saved {
            Ok(readings) => {
                self.terminal.extend(readings);
                Ok(())
            }
            Err(error) => Err(error),
        };
        if self.journal.groups > 0 {
            let mut value = self.prefix();
            value["storage_finalized"] = json!(closed.is_ok());
            self.terminal.push_back(Reading {
                signal: self.signal.clone(),
                value,
                source_time: None,
            });
        }
        let result = match (primary, closed) {
            (Err(error), Err(close)) => {
                Err(error.context(format!("SICK Compact archive close also failed: {close:#}")))
            }
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        };
        result.map_err(|error| {
            let error = with_details(error, json!({"sick_compact_udp":self.prefix()}));
            if self.failed.is_none() {
                self.failed = Some(Failure::capture(&error));
            }
            error
        })
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        Ok(self.terminal.pop_front())
    }
}
#[cfg(test)]
mod tests;
