//! Passive explicitly qualified Hesai datagrams with native fields and durable raw ranges.
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
use packet::{Format, Profile};
const MAX_JSON: usize = 1024 * 1024;
// The adapter cannot see source id or the recorder's Sample timing wrapper.
// Shared bounded names and all scalar wrapper fields fit well within this reserve.
const MAX_VALUE_JSON: usize = MAX_JSON - 16 * 1024;
const MAX_RAW_GROUP: usize = 4 * 1024 * 1024;
const MAX_ELEMENTS: usize = 8192;
const MAX_PACKETS: u16 = 32;
const MAX_FOREIGN_BYTES: usize = 64 * 1024;
const MAX_FOREIGN_PACKETS: usize = 128;
const GROUP_WINDOW: Duration = Duration::from_millis(100);
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
    pub profile: String,
    #[serde(default)]
    pub context: Option<PathBuf>,
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
    super::validate_name(&config.signal, "Hesai signal")?;
    crate::naming::validate_output(&config.output)?;
    ensure!(config.listen.port() != 0 && !config.listen.ip().is_multicast() && !config.listen.ip().is_broadcast()
        && unicast(config.source_ip) && config.source_port != Some(0)
        && (1..=MAX_PACKETS).contains(&config.packets_per_group)
        && config.output != "-" && !config.output.contains("://") && !config.output.contains(['{','}']),
        Usage("Hesai requires explicit IPv4 listen/source, literal local output, max_bytes and packets_per_group 1..32".into()));
    if let Some(join) = &config.multicast {
        ensure!(join.group.is_multicast() && unicast(join.interface)
            && config.listen.ip().is_unspecified(),
            Usage("Hesai multicast requires listen 0.0.0.0:port, an explicit multicast group and unicast interface".into()));
    }
    let format = Format {
        profile: match config.profile.as_str() {
            "pandar_xt32_v6_1" => Profile::PandarXt32V6_1,
            "pandar_xt32_gps" => Profile::PandarXt32Gps,
            _ => {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "Hesai profile is unsupported; select pandar_xt32_v6_1 or pandar_xt32_gps explicitly",
                ));
            }
        },
    };
    let context = Context::read(config.context.as_deref())?;
    let path = std::path::absolute(&config.output)?;
    ensure!(
        path.file_name().is_some() && !path.is_dir(),
        Usage("Hesai output must name a local file".into())
    );
    let origin =
        crate::host::monotonic_ns().context("Hesai requires a host monotonic receive clock")?;
    let journal = Journal::new(path, config, context, format, origin)?;
    let socket = UdpSocket::bind(config.listen).context("bind Hesai UDP listener")?;
    if let Some(join) = &config.multicast {
        socket
            .join_multicast_v4(&join.group, &join.interface)
            .context("join explicit Hesai multicast group/interface")?;
    }
    Ok(Box::new(Stream {
        socket: Some(socket),
        source_ip: config.source_ip,
        source_port: config.source_port,
        signal: config.signal.clone(),
        packets_per_group: config.packets_per_group,
        format,
        journal,
        pending: None,
        observed: None,
        cutoff: None,
        terminal: VecDeque::new(),
        failed: false,
        finished: false,
        foreign_packets: 0,
        last_sequence: None,
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
            .ok_or_else(|| io::Error::other("Hesai native JSON exceeds 1 MiB"))?;
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
            "Hesai native JSON exceeds its bounded admission: {error}"
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
            .context("Hesai group extent overflow")?;
        let elements = self
            .elements
            .checked_add(packet.elements)
            .context("Hesai decoded count overflow")?;
        let index = journal
            .packets
            .checked_add(u64::try_from(count)?)
            .context("Hesai packet index exhausted")?;
        let raw_offset = journal
            .next_offset()?
            .checked_add(u64::try_from(self.raw.len() + RECORD_HEADER)?)
            .context("Hesai raw offset overflow")?;
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
        .context("Hesai JSON bound overflow")?;
        if self.values.is_empty() && !journal.room_for(raw_bytes) {
            return Err(Categorized::error(
                ErrorKind::StorageFull,
                "Hesai archive reached max_bytes",
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
                incomplete("one Hesai packet exceeds group, JSON, or max_bytes admission")
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
    journal: Journal,
    pending: Option<Received>,
    observed: Option<u64>,
    cutoff: Option<u64>,
    terminal: VecDeque<Reading>,
    failed: bool,
    finished: bool,
    foreign_packets: u64,
    last_sequence: Option<u32>,
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
    fn acquire(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        ensure!(
            !self.failed && !self.finished,
            incomplete("Hesai source was retired")
        );
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("Hesai timeout is too large")?;
        let mut group = Group::new();
        if let Some(packet) = self.pending.take() {
            self.uncommitted_packets = 1;
            // Already accepted packets are committed without another socket read.
            ensure!(
                group
                    .offer(packet, &self.journal, &self.signal, self.packets_per_group)?
                    .is_none(),
                "pending Hesai packet does not fit an empty group"
            );
            return self.finish_group(group);
        }
        let mut group_end = deadline;
        let mut foreign = (0_usize, 0_usize);
        let mut raw = vec![0; 65_536].into_boxed_slice();
        loop {
            let remaining = group_end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let socket = self.socket.as_ref().context("Hesai source was retired")?;
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
                Err(error) => return Err(error).context("receive Hesai datagram"),
            };
            let received =
                crate::host::monotonic_ns().context("Hesai receive clock unavailable")?;
            if self.cutoff.is_some_and(|cutoff| received > cutoff) {
                break;
            }
            let SocketAddr::V4(source) = source else {
                return Err(incomplete("Hesai listener returned a non-IPv4 source"));
            };
            if source.ip() != &self.source_ip
                || self.source_port.is_some_and(|port| port != source.port())
            {
                foreign.0 += 1;
                foreign.1 += length;
                self.foreign_packets = self
                    .foreign_packets
                    .checked_add(1)
                    .context("Hesai foreign count exhausted")?;
                ensure!(
                    foreign.0 <= MAX_FOREIGN_PACKETS && foreign.1 <= MAX_FOREIGN_BYTES,
                    incomplete("Hesai foreign-source traffic exceeded bounded allowance")
                );
                continue;
            }
            let bytes = &raw[..length];
            let mut decoded = packet::decode(bytes, self.format)?;
            self.observe_sequence(&mut decoded.value)?;
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
    fn observe_sequence(&mut self, value: &mut Value) -> Result<()> {
        if self.format.profile == Profile::PandarXt32Gps {
            return Ok(());
        }
        let current = value["sequence_raw"]
            .as_u64()
            .and_then(|raw| u32::try_from(raw).ok())
            .context("Hesai decoder omitted its sequence word")?;
        value["sequence_observation"] = self.last_sequence.map_or_else(
            || json!({"relation":"first_observed","loss_count_proven":false}),
            |previous| {
                let delta = current.wrapping_sub(previous);
                json!({"previous_raw":previous,"delta_modulo_u32":delta,
                    "relation":match delta {0 => "equal",1 => "next_modulo",2..=0x7fff_ffff => "forward_or_gap",_ => "backward_or_large_forward"},
                    "loss_count_proven":false})
            },
        );
        self.last_sequence = Some(current);
        Ok(())
    }
    fn prefix(&self) -> Value {
        let mut prefix = self.journal.prefix();
        prefix["foreign_packets"] = json!(self.foreign_packets);
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
                        "cannot remove unpublished Hesai archive: {cleanup:#}"
                    )),
                }
            } else {
                error
            };
            with_details(error, json!({"hesai_udp":self.prefix()}))
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
                    ensure!(pending.is_none(), "pending Hesai packet does not fit");
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
                Err(error.context(format!("Hesai archive close also failed: {close:#}")))
            }
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        };
        result.map_err(|error| with_details(error, json!({"hesai_udp":self.prefix()})))
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        Ok(self.terminal.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stream(folder: &std::path::Path) -> Stream {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let config: Config = serde_json::from_value(json!({
            "listen":socket.local_addr().unwrap(),"source_ip":"127.0.0.1",
            "profile":"pandar_xt32_v6_1","output":"raw.hesai","max_bytes":10_000_000
        }))
        .unwrap();
        let format = Format {
            profile: Profile::PandarXt32V6_1,
        };
        let journal = Journal::new(
            folder.join("raw.hesai"),
            &config,
            Context {
                raw: Vec::new(),
                supplied: false,
            },
            format,
            0,
        )
        .unwrap();
        Stream {
            socket: Some(socket),
            source_ip: Ipv4Addr::LOCALHOST,
            source_port: None,
            signal: "datagrams".into(),
            packets_per_group: 32,
            format,
            journal,
            pending: None,
            observed: None,
            cutoff: None,
            terminal: VecDeque::new(),
            failed: false,
            finished: false,
            foreign_packets: 0,
            last_sequence: None,
            uncommitted_packets: 0,
        }
    }
    #[test]
    fn sequence_observations_are_modulo_between_received_packets_and_never_prove_loss() {
        let folder = tempfile::tempdir().unwrap();
        let mut source = stream(folder.path());
        for (sequence, relation, delta) in [
            (u32::MAX, "first_observed", None),
            (0, "next_modulo", Some(1)),
            (0, "equal", Some(0)),
            (2, "forward_or_gap", Some(2)),
            (1, "backward_or_large_forward", Some(u32::MAX)),
        ] {
            let mut value = json!({"sequence_raw":sequence});
            source.observe_sequence(&mut value).unwrap();
            assert_eq!(value["sequence_observation"]["relation"], relation);
            assert_eq!(value["sequence_observation"]["loss_count_proven"], false);
            assert_eq!(
                value["sequence_observation"]["delta_modulo_u32"].as_u64(),
                delta.map(u64::from)
            );
        }
        source.format.profile = Profile::PandarXt32Gps;
        let mut gps = json!({"profile":"pandar_xt32_gps"});
        source.observe_sequence(&mut gps).unwrap();
        assert!(gps.get("sequence_observation").is_none());
        assert_eq!(source.last_sequence, Some(1));
    }
    #[test]
    fn expired_read_does_not_receive_and_after_cutoff_datagrams_are_not_accepted() {
        let folder = tempfile::tempdir().unwrap();
        let mut source = stream(folder.path());
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket2::SockRef::from(&sender)
            .set_send_buffer_size(128 * 1024)
            .unwrap();
        let target = source.socket.as_ref().unwrap().local_addr().unwrap();
        sender
            .send_to(b"queued-before-zero-budget", target)
            .unwrap();
        let mut raw = [0; 64];
        {
            let socket = source.socket.as_ref().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            assert_eq!(socket.peek_from(&mut raw).unwrap().0, 25);
        }
        assert_eq!(source.read(Duration::ZERO).unwrap().len(), 0);
        let socket = source.socket.as_ref().unwrap();
        socket.set_nonblocking(true).unwrap();
        assert_eq!(socket.recv_from(&mut raw).unwrap().0, 25);
        assert_eq!(
            raw.first_chunk::<25>().unwrap(),
            b"queued-before-zero-budget"
        );
        socket.set_nonblocking(false).unwrap();
        source.set_acquisition_end(Some(0));
        // Cutoff is checked before any decoder interpretation of this body.
        sender.send_to(&[0; 1080], target).unwrap();
        assert_eq!(source.read(Duration::from_millis(50)).unwrap().len(), 0);
        assert_eq!(source.last_sequence, None);
        assert_eq!(source.uncommitted_packets, 0);
        assert_eq!(source.journal.groups, 0);
        source.finish(Duration::ZERO).unwrap();
        assert!(source.drain().unwrap().is_none());
        assert!(!folder.path().join("raw.hesai").exists());
    }
}
