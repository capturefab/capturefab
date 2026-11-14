//! Passive Ouster OS-series datagrams with exact metadata, native fields and committed raw ranges.
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

mod journal;
mod metadata;
mod packet;
use journal::{FOOTER, GROUP_HEADER, Journal, RECORD_HEADER};
use metadata::Metadata;
use packet::{Format, Integrity, Profile};
const MAX_JSON: usize = 1024 * 1024;
// The adapter cannot see source id or the recorder's Sample timing wrapper.
// Shared bounded names and all scalar wrapper fields fit well within this reserve.
const MAX_VALUE_JSON: usize = MAX_JSON - 16 * 1024;
const MAX_RAW_GROUP: usize = 4 * 1024 * 1024;
const MAX_ELEMENTS: usize = 16_384;
const MAX_PACKETS: u16 = 32;
const MAX_FOREIGN_BYTES: usize = 64 * 1024;
const MAX_FOREIGN_PACKETS: usize = 128;
const MAX_OTHER_BYTES: usize = 64 * 1024 * 1024;
const MAX_OTHER_PACKETS: usize = 4096;
const GROUP_WINDOW: Duration = Duration::from_millis(100);
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StreamKind {
    Lidar,
    Imu,
}
impl StreamKind {
    const fn id(self) -> u16 {
        match self {
            Self::Lidar => 1,
            Self::Imu => 2,
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Multicast {
    pub group: Ipv4Addr,
    pub interface: Ipv4Addr,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub listen: SocketAddrV4,
    pub source_ip: Ipv4Addr,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub source_port: Option<u16>,
    pub stream: StreamKind,
    pub metadata: PathBuf,
    pub output: String,
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
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub header_type: Option<String>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub pixels_per_column: Option<usize>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub columns_per_packet: Option<usize>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub columns_per_frame: Option<usize>,
    #[serde(default)]
    pub multicast: Option<Multicast>,
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
fn unicast(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast()
}
pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    super::validate_name(&config.signal, "Ouster signal")?;
    crate::naming::validate_output(&config.output)?;
    ensure!(config.listen.port() != 0 && !config.listen.ip().is_multicast() && !config.listen.ip().is_broadcast()
        && unicast(config.source_ip) && config.source_port != Some(0)
        && (1..=MAX_PACKETS).contains(&config.packets_per_group)
        && config.output != "-" && !config.output.contains("://") && !config.output.contains(['{','}']),
        Usage("Ouster requires explicit IPv4 listen/source, literal local output, max_bytes and packets_per_group 1..32".into()));
    if let Some(join) = &config.multicast {
        ensure!(join.group.is_multicast() && unicast(join.interface)
            && config.listen.ip().is_unspecified(),
            Usage("Ouster multicast requires listen 0.0.0.0:port, an explicit multicast group and unicast interface".into()));
    }
    let metadata = Metadata::read(&config.metadata, config)?;
    let path = std::path::absolute(&config.output)?;
    ensure!(
        path.file_name().is_some() && !path.is_dir(),
        Usage("Ouster output must name a local file".into())
    );
    let origin =
        crate::host::monotonic_ns().context("Ouster requires a host monotonic receive clock")?;
    let Metadata {
        raw,
        format,
        summary,
        other,
        other_standard,
    } = metadata;
    let journal = Journal::new(path, config, &raw, summary, &format, origin)?;
    let socket = UdpSocket::bind(config.listen).context("bind Ouster UDP listener")?;
    if let Some(join) = &config.multicast {
        socket
            .join_multicast_v4(&join.group, &join.interface)
            .context("join explicit Ouster multicast group/interface")?;
    }
    Ok(Box::new(Stream {
        socket: Some(socket),
        source_ip: config.source_ip,
        source_port: config.source_port,
        signal: config.signal.clone(),
        packets_per_group: config.packets_per_group,
        format,
        other,
        other_standard,
        journal,
        pending: None,
        observed: None,
        cutoff: None,
        terminal: VecDeque::new(),
        failed: false,
        finished: false,
        foreign_packets: 0,
        other_packets: 0,
        other_bytes: 0,
        uncommitted_packets: 0,
    }))
}
struct Count(usize);
impl io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_JSON)
            .ok_or_else(|| io::Error::other("Ouster native JSON exceeds 1 MiB"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn count_json(value: &impl Serialize) -> Result<usize> {
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_err(|error| {
        incomplete(&format!(
            "Ouster native JSON exceeds its bounded admission: {error}"
        ))
    })?;
    Ok(count.0)
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
    elements: usize,
    received: u64,
    source: SocketAddrV4,
}
struct Group {
    raw: Vec<u8>,
    values: Vec<Value>,
    json_bytes: usize,
    elements: usize,
    first_received: u64,
    last_received: u64,
}
impl Group {
    fn new() -> Self {
        Self {
            raw: vec![0; GROUP_HEADER],
            values: Vec::new(),
            json_bytes: 0,
            elements: 0,
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
    ) -> Result<Option<Received>> {
        let count = self.values.len() + 1;
        let raw_bytes = self
            .raw
            .len()
            .checked_add(RECORD_HEADER + packet.raw.len())
            .context("Ouster group extent overflow")?;
        let elements = self
            .elements
            .checked_add(packet.elements)
            .context("Ouster decoded count overflow")?;
        let index = journal
            .packets
            .checked_add(u64::try_from(count)?)
            .context("Ouster packet index exhausted")?;
        let raw_offset = journal
            .next_offset()?
            .checked_add(u64::try_from(self.raw.len() + RECORD_HEADER)?)
            .context("Ouster raw offset overflow")?;
        packet.value["host_packet_index"] = json!(index);
        packet.value["received_monotonic_ns"] = json!(packet.received);
        packet.value["source"] = json!(packet.source);
        packet.value["raw_offset"] = json!(raw_offset);
        packet.value["raw_length"] = json!(packet.raw.len());
        let bytes = count_json(&packet.value)?;
        let first_received = if self.values.is_empty() {
            packet.received
        } else {
            self.first_received
        };
        let skeleton = journal.descriptor(count, raw_bytes, first_received, packet.received)?;
        let total = count_json(&ReadingRef {
            signal,
            value: &skeleton,
        })?
        .checked_add(self.json_bytes)
        .and_then(|total| total.checked_add(bytes))
        .and_then(|total| total.checked_add(count - 1))
        .context("Ouster JSON bound overflow")?;
        if self.values.is_empty() && !journal.room_for(raw_bytes) {
            return Err(Categorized::error(
                ErrorKind::StorageFull,
                "Ouster archive reached max_bytes",
            ));
        }
        let fits = count <= usize::from(limit)
            && raw_bytes + FOOTER <= MAX_RAW_GROUP
            && elements <= MAX_ELEMENTS
            && total <= MAX_VALUE_JSON
            && journal.room_for(raw_bytes);
        if !fits {
            ensure!(
                !self.values.is_empty(),
                incomplete("one Ouster packet exceeds group, JSON, or max_bytes admission")
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
        self.elements = elements;
        self.first_received = first_received;
        self.last_received = packet.received;
        Ok(None)
    }
}
struct Stream {
    socket: Option<UdpSocket>,
    source_ip: Ipv4Addr,
    source_port: Option<u16>,
    signal: String,
    packets_per_group: u16,
    format: Format,
    other: Option<Format>,
    other_standard: Option<(u64, u32, u16, Integrity)>,
    journal: Journal,
    pending: Option<Received>,
    observed: Option<u64>,
    cutoff: Option<u64>,
    terminal: VecDeque<Reading>,
    failed: bool,
    finished: bool,
    foreign_packets: u64,
    other_packets: u64,
    other_bytes: u64,
    uncommitted_packets: usize,
}
impl Stream {
    fn finish_group(&mut self, group: Group) -> Result<Vec<Reading>> {
        if group.values.is_empty() {
            return Ok(Vec::new());
        }
        self.observed = Some(group.last_received);
        self.uncommitted_packets = group.values.len();
        let value = self.journal.commit(group, &self.signal)?;
        self.uncommitted_packets = 0;
        Ok(vec![Reading {
            signal: self.signal.clone(),
            value,
            source_time: None,
        }])
    }
    fn is_other(&self, raw: &[u8]) -> Result<bool> {
        let selected_modern = !matches!(
            self.format.profile,
            Profile::LegacyLidar | Profile::LegacyImu
        );
        let kind = raw
            .first_chunk::<2>()
            .map(|bytes| u16::from_le_bytes(*bytes));
        if selected_modern && kind == Some(if self.format.profile.is_imu() { 2 } else { 1 }) {
            return Ok(false);
        }
        if !selected_modern && raw.len() == packet::expected_packet_bytes(&self.format)? {
            return Ok(false);
        }
        if let Some((serial, init, other_types, integrity)) = self.other_standard
            && let Some(kind) = kind
            && matches!(kind, 1..=3)
            && other_types & (1_u16 << kind) != 0
        {
            packet::validate_standard_packet(raw, serial, init, kind, integrity)?;
            return Ok(true);
        }
        if let Some(other) = &self.other
            && raw.len() == packet::expected_packet_bytes(other)?
        {
            // LEGACY has no type/CRC. Exact declared framing cannot distinguish
            // a genuine 48B IMU packet from a deliberately shortened lidar body.
            packet::decode(raw, other)?;
            return Ok(true);
        }
        Ok(false)
    }
    fn acquire(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        ensure!(
            !self.failed && !self.finished,
            incomplete("Ouster source was retired")
        );
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("Ouster timeout is too large")?;
        let mut group = Group::new();
        if let Some(packet) = self.pending.take() {
            self.uncommitted_packets = 1;
            // Already accepted packets are committed without another socket read.
            ensure!(
                group
                    .offer(packet, &self.journal, &self.signal, self.packets_per_group)?
                    .is_none(),
                "pending Ouster packet does not fit an empty group"
            );
            return self.finish_group(group);
        }
        let mut group_end = deadline;
        let mut foreign = (0_usize, 0_usize);
        let mut other = (0_usize, 0_usize);
        let mut raw = vec![0; 65_536].into_boxed_slice();
        loop {
            let remaining = group_end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let socket = self.socket.as_ref().context("Ouster source was retired")?;
            socket.set_read_timeout(Some(remaining))?;
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
                Err(error) => return Err(error).context("receive Ouster datagram"),
            };
            let received =
                crate::host::monotonic_ns().context("Ouster receive clock unavailable")?;
            if self.cutoff.is_some_and(|cutoff| received > cutoff) {
                break;
            }
            let SocketAddr::V4(source) = source else {
                return Err(incomplete("Ouster listener returned a non-IPv4 source"));
            };
            if source.ip() != &self.source_ip
                || self.source_port.is_some_and(|port| port != source.port())
            {
                foreign.0 += 1;
                foreign.1 += length;
                self.foreign_packets = self
                    .foreign_packets
                    .checked_add(1)
                    .context("Ouster foreign count exhausted")?;
                ensure!(
                    foreign.0 <= MAX_FOREIGN_PACKETS && foreign.1 <= MAX_FOREIGN_BYTES,
                    incomplete("Ouster foreign-source traffic exceeded bounded allowance")
                );
                continue;
            }
            let bytes = &raw[..length];
            if self.is_other(bytes)? {
                other.0 += 1;
                other.1 += length;
                self.other_packets = self
                    .other_packets
                    .checked_add(1)
                    .context("Ouster filtered count exhausted")?;
                self.other_bytes = self
                    .other_bytes
                    .checked_add(u64::try_from(length)?)
                    .context("Ouster filtered bytes exhausted")?;
                ensure!(
                    other.0 <= MAX_OTHER_PACKETS && other.1 <= MAX_OTHER_BYTES,
                    incomplete("Ouster unselected-stream traffic exceeded bounded allowance")
                );
                continue;
            }
            let decoded = packet::decode(bytes, &self.format)?;
            let packet = Received {
                raw: bytes.to_vec(),
                value: decoded.value,
                elements: decoded.elements,
                received,
                source,
            };
            let was_empty = group.values.is_empty();
            self.uncommitted_packets = group.values.len() + 1;
            self.pending =
                group.offer(packet, &self.journal, &self.signal, self.packets_per_group)?;
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
        self.finish_group(group)
    }
    fn prefix(&self) -> Value {
        let mut prefix = self.journal.prefix();
        prefix["foreign_packets"] = json!(self.foreign_packets);
        prefix["filtered_other_packets"] = json!(self.other_packets);
        prefix["filtered_other_bytes"] = json!(self.other_bytes);
        prefix["uncommitted_packets"] = json!(self.uncommitted_packets);
        prefix["pending_accepted_packets"] = json!(usize::from(self.pending.is_some()));
        prefix
    }
}
impl Source for Stream {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        self.acquire(timeout).map_err(|error| {
            self.socket = None;
            self.failed = true;
            let error = if self.journal.groups == 0 {
                match self.journal.finish() {
                    Ok(()) => error,
                    Err(cleanup) => error.context(format!(
                        "cannot remove unpublished Ouster archive: {cleanup:#}"
                    )),
                }
            } else {
                error
            };
            with_details(error, json!({"ouster_udp":self.prefix()}))
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
        let saved = if !self.failed
            && let Some(packet) = self.pending.take()
        {
            self.uncommitted_packets = 1;
            let mut group = Group::new();
            group
                .offer(packet, &self.journal, &self.signal, self.packets_per_group)
                .and_then(|pending| {
                    ensure!(pending.is_none(), "pending Ouster packet does not fit");
                    self.finish_group(group)
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
                Err(error.context(format!("Ouster archive close also failed: {close:#}")))
            }
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        };
        result.map_err(|error| with_details(error, json!({"ouster_udp":self.prefix()})))
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        Ok(self.terminal.pop_front())
    }
}
