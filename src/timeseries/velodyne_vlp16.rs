//! Explicit modern VLP-16/Puck LITE and HDL-32E raw UDP profiles.
//!
//! Velodyne VLP-16 manual Rev F §9.3.1–9.3.2 defines the 1206-byte
//! packet and product 0x22. HDL-32E 63-9113 Rev. M §§9.3.1–9.3.2
//! defines product 0x21 and paired dual blocks. Neither profile infers XYZ or UTC.
use super::{Reading, Source};
use crate::{
    storage,
    types::{Categorized, ErrorKind, Usage, with_details},
};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::{self, Write as _},
    net::{SocketAddr, SocketAddrV4, UdpSocket},
    path::PathBuf,
    time::{Duration, Instant},
};

const PACKET_BYTES: usize = 1206;
const FILE_HEADER: usize = 40;
const GROUP_HEADER: usize = 32;
const RECORD_HEADER: usize = 32;
const RECORD_BYTES: usize = RECORD_HEADER + PACKET_BYTES;
const GROUP_FOOTER: usize = 56;
const MIN_FILE_BYTES: usize = FILE_HEADER + GROUP_HEADER + RECORD_BYTES + GROUP_FOOTER;
const MAX_PACKETS: u16 = 128;
const MAX_FOREIGN_BYTES: usize = 64 * 1024;
const MAX_FOREIGN_PACKETS: usize = 128;
const GROUP_WINDOW: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub listen: SocketAddrV4,
    pub source: SocketAddrV4,
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
}
const fn default_packets() -> u16 {
    MAX_PACKETS
}
fn default_signal() -> String {
    "datagrams".into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Vlp16,
    Hdl32e,
}
impl Profile {
    const fn label(self) -> &'static str {
        match self {
            Self::Vlp16 => "VLP-16",
            Self::Hdl32e => "HDL-32E",
        }
    }
    const fn magic(self) -> &'static [u8; 8] {
        match self {
            Self::Vlp16 => b"CFVLP16\0",
            Self::Hdl32e => b"CFHDL32E",
        }
    }
    const fn product(self) -> u8 {
        match self {
            Self::Vlp16 => 0x22,
            Self::Hdl32e => 0x21,
        }
    }
    const fn key(self) -> &'static str {
        match self {
            Self::Vlp16 => "velodyne_vlp16",
            Self::Hdl32e => "velodyne_hdl32e",
        }
    }
    const fn group_kind(self) -> &'static str {
        match self {
            Self::Vlp16 => "velodyne_vlp16_group",
            Self::Hdl32e => "velodyne_hdl32e_group",
        }
    }
    const fn file_kind(self) -> &'static str {
        match self {
            Self::Vlp16 => "velodyne_vlp16_file",
            Self::Hdl32e => "velodyne_hdl32e_file",
        }
    }
    const fn product_error(self) -> &'static str {
        match self {
            Self::Vlp16 => {
                "VLP-16 profile requires modern product ID 0x22; no legacy model is inferred"
            }
            Self::Hdl32e => {
                "HDL-32E profile requires documented product ID 0x21; no legacy model is inferred"
            }
        }
    }
    fn message(self, tail: &str) -> String {
        format!("{} {tail}", self.label())
    }
    fn details(self, prefix: Value) -> Value {
        let mut details = json!({});
        details[self.key()] = prefix;
        details
    }
    fn annotate(self, value: &mut Value) {
        // Keep the already published VLP-16 schema unchanged.
        if self == Self::Hdl32e {
            value["model_profile"] = json!("hdl32e");
            value["product_id_raw"] = json!(self.product());
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    open_profile(config, Profile::Vlp16)
}
pub(crate) fn open_hdl32e(config: &Config) -> Result<Box<dyn Source>> {
    open_profile(config, Profile::Hdl32e)
}
fn open_profile(config: &Config, profile: Profile) -> Result<Box<dyn Source>> {
    super::validate_name(&config.signal, &profile.message("signal"))?;
    crate::naming::validate_output(&config.output)?;
    if config.listen.port() == 0
        || config.listen.ip().is_multicast()
        || config.listen.ip().is_broadcast()
        || config.source.port() == 0
        || config.source.ip().is_unspecified()
        || config.source.ip().is_multicast()
        || config.source.ip().is_broadcast()
        || !(1..=MAX_PACKETS).contains(&config.packets_per_group)
        || config.max_bytes < u64::try_from(MIN_FILE_BYTES)?
        || config.output == "-"
        || config.output.contains("://")
        || config.output.contains(['{', '}'])
    {
        return Err(Usage(profile.message("requires local IPv4 listen/source endpoints with nonzero ports, an exact unicast source, a literal output file, max_bytes >= 1366 and packets_per_group 1..128")).into());
    }
    let path = std::path::absolute(&config.output)?;
    if path.file_name().is_none() || path.is_dir() {
        return Err(Usage(profile.message("output must name a local file")).into());
    }
    // Capture is passive, including broadcast. Connecting a wildcard UDP
    // listener can select a local unicast address and miss broadcast packets.
    let socket = UdpSocket::bind(config.listen)
        .with_context(|| format!("bind {} UDP listener", profile.label()))?;
    let origin = crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(
            ErrorKind::Unsupported,
            profile.message("requires a host-wide monotonic receive clock"),
        )
    })?;
    Ok(Box::new(Stream {
        socket: Some(socket),
        source: config.source,
        signal: config.signal.clone(),
        packets_per_group: config.packets_per_group,
        journal: Journal::new(path, config, origin, profile),
        observed: None,
        cutoff: None,
        terminal: None,
    }))
}

#[derive(Debug, Clone, Copy)]
struct Packet {
    timestamp: u32,
    mode: u8,
    model: u8,
}
impl Packet {
    fn parse(raw: &[u8], profile: Profile) -> Result<Self> {
        let raw: &[u8; PACKET_BYTES] = raw
            .try_into()
            .map_err(|_| incomplete(profile.message("datagram must contain exactly 1206 bytes")))?;
        let blocks = raw
            .get(..1200)
            .with_context(|| format!("missing {} data blocks", profile.label()))?;
        let mut previous = None;
        let mode = raw[1204];
        for (index, block) in blocks.as_chunks::<100>().0.iter().enumerate() {
            ensure!(
                block[..2] == [0xff, 0xee],
                incomplete(profile.message("block has an invalid FFEE flag"))
            );
            let azimuth = u16::from_le_bytes([block[2], block[3]]);
            ensure!(
                azimuth < 36_000,
                incomplete(profile.message("azimuth exceeds 35999 hundredths of a degree"))
            );
            if mode == 0x39 && index % 2 == 1 {
                ensure!(
                    previous == Some(azimuth),
                    incomplete(profile.message("dual-return block pair has different azimuths"))
                );
            }
            previous = Some(azimuth);
        }
        let timestamp = u32::from_le_bytes(
            raw.get(1200..1204)
                .and_then(|value| value.try_into().ok())
                .with_context(|| format!("missing {} timestamp", profile.label()))?,
        );
        ensure!(
            timestamp < 3_600_000_000,
            incomplete(profile.message("raw timestamp exceeds its documented hourly range"))
        );
        ensure!(
            matches!(mode, 0x37..=0x39),
            Categorized::new(
                ErrorKind::Unsupported,
                format!("unsupported {} return mode", profile.label())
            )
            .details(json!({"return_mode_raw":mode}))
        );
        let model = raw[1205];
        ensure!(
            model == profile.product(),
            Categorized::new(ErrorKind::Unsupported, profile.product_error())
                .details(json!({"product_id_raw":model}))
        );
        Ok(Self {
            timestamp,
            mode,
            model,
        })
    }
}

struct Group {
    raw: Vec<u8>,
    count: u32,
    first_index: u64,
    first_received: u64,
    last_received: u64,
    first_timestamp: u32,
    last_timestamp: u32,
    modes: u8,
}
impl Group {
    fn new(capacity: u16) -> Self {
        let mut raw =
            Vec::with_capacity(GROUP_HEADER + usize::from(capacity) * RECORD_BYTES + GROUP_FOOTER);
        raw.resize(GROUP_HEADER, 0);
        Self {
            raw,
            count: 0,
            first_index: 0,
            first_received: 0,
            last_received: 0,
            first_timestamp: 0,
            last_timestamp: 0,
            modes: 0,
        }
    }
    fn push(&mut self, raw: &[u8], packet: Packet, index: u64, received: u64) {
        if self.count == 0 {
            self.first_index = index;
            self.first_received = received;
            self.first_timestamp = packet.timestamp;
        }
        self.raw.extend_from_slice(b"PKT1");
        self.raw.extend_from_slice(&1206u32.to_le_bytes());
        self.raw.extend_from_slice(&index.to_le_bytes());
        self.raw.extend_from_slice(&received.to_le_bytes());
        self.raw.extend_from_slice(&packet.timestamp.to_le_bytes());
        self.raw
            .extend_from_slice(&[packet.mode, packet.model, 0, 0]);
        self.raw.extend_from_slice(raw);
        self.count += 1;
        self.last_received = received;
        self.last_timestamp = packet.timestamp;
        self.modes |= 1 << (packet.mode - 0x37);
    }
    fn seal(&mut self, sequence: u64, profile: Profile) -> Result<()> {
        let mut header = Vec::with_capacity(GROUP_HEADER);
        header.extend_from_slice(b"CFVGRP1\0");
        header.extend_from_slice(&sequence.to_le_bytes());
        header.extend_from_slice(&self.count.to_le_bytes());
        header.extend_from_slice(&(self.count * 1238).to_le_bytes());
        header.extend_from_slice(&self.first_index.to_le_bytes());
        self.raw
            .get_mut(..GROUP_HEADER)
            .with_context(|| format!("missing {} group header", profile.label()))?
            .copy_from_slice(&header);
        let prefix = u64::try_from(self.raw.len())?;
        let digest = Sha256::digest(&self.raw);
        self.raw.extend_from_slice(b"CFVCMT1\0");
        self.raw.extend_from_slice(&sequence.to_le_bytes());
        self.raw.extend_from_slice(&prefix.to_le_bytes());
        self.raw.extend_from_slice(&digest);
        Ok(())
    }
}

/// The journal can exercise failed sync/rollback without unreliable disk faults.
trait Sink: Send {
    fn append(&mut self, bytes: &[u8]) -> Result<()>;
    fn sync(&mut self, first: bool) -> Result<()>;
    fn rollback(&mut self, length: u64) -> Result<()>;
    fn bytes(&self) -> u64;
    fn close(self: Box<Self>, keep: bool) -> Result<()>;
}
struct Disk(storage::Recording);
impl Sink for Disk {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.write_all(bytes).map_err(|error| {
            self.0
                .alarm()
                .error()
                .unwrap_or_else(|| anyhow::Error::from(error))
        })?;
        Ok(())
    }
    fn sync(&mut self, first: bool) -> Result<()> {
        if let Some(error) = self.0.alarm().error() {
            return Err(error);
        }
        self.0.amend(|file, _| file.sync_data())?;
        if first {
            storage::Folders::of(self.0.path()).sync()?;
        }
        Ok(())
    }
    fn rollback(&mut self, length: u64) -> Result<()> {
        self.0.amend(|file, _| {
            file.set_len(length)?;
            file.sync_data()
        })
    }
    fn bytes(&self) -> u64 {
        self.0.bytes_written()
    }
    fn close(self: Box<Self>, keep: bool) -> Result<()> {
        if keep {
            self.0.finish().map(|_| ())
        } else {
            self.0.abort()
        }
    }
}

struct Journal {
    profile: Profile,
    path: PathBuf,
    header: Vec<u8>,
    max_bytes: u64,
    min_free: Option<u64>,
    sink: Option<Box<dyn Sink>>,
    committed: u64,
    groups: u64,
    packets: u64,
    digest: Sha256,
    last_group: Option<Value>,
    rollback_error: Option<String>,
    final_bytes: u64,
}
impl Journal {
    fn new(path: PathBuf, config: &Config, origin: u64, profile: Profile) -> Self {
        let mut header = Vec::with_capacity(FILE_HEADER);
        header.extend_from_slice(profile.magic());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&40u16.to_le_bytes());
        header.extend_from_slice(&1206u32.to_le_bytes());
        header.extend_from_slice(&config.source.ip().octets());
        header.extend_from_slice(&config.source.port().to_le_bytes());
        header.extend_from_slice(&config.listen.port().to_le_bytes());
        header.extend_from_slice(&origin.to_le_bytes());
        header.extend_from_slice(&[0; 8]);
        Self {
            profile,
            path,
            header,
            max_bytes: config.max_bytes,
            min_free: config.min_free_bytes,
            sink: None,
            committed: 0,
            groups: 0,
            packets: 0,
            digest: Sha256::new(),
            last_group: None,
            rollback_error: None,
            final_bytes: 0,
        }
    }
    fn check_limit(&self, adding: u64) -> Result<()> {
        let profile = self.profile;
        ensure!(
            adding <= self.max_bytes.saturating_sub(self.committed),
            Categorized::new(
                ErrorKind::Incomplete,
                profile.message("archive reached max_bytes")
            )
            .details(json!({"max_bytes":self.max_bytes,"next_group_bytes":adding}))
        );
        Ok(())
    }
    fn packet_capacity(&self, requested: u16) -> Result<u16> {
        let overhead = u64::try_from(
            GROUP_HEADER + GROUP_FOOTER + if self.groups == 0 { FILE_HEADER } else { 0 },
        )?;
        self.check_limit(overhead + u64::try_from(RECORD_BYTES)?)?;
        let available = (self.max_bytes - self.committed - overhead) / u64::try_from(RECORD_BYTES)?;
        Ok(u16::try_from(available.min(u64::from(requested)))?)
    }
    fn commit(&mut self, group: &mut Group) -> Result<Value> {
        let profile = self.profile;
        let sequence = self
            .groups
            .checked_add(1)
            .with_context(|| profile.message("group index exhausted"))?;
        let packets = self
            .packets
            .checked_add(u64::from(group.count))
            .with_context(|| profile.message("host packet index exhausted"))?;
        group.seal(sequence, profile)?;
        let first = self.groups == 0;
        let header_bytes = if first { self.header.len() } else { 0 };
        let adding = u64::try_from(header_bytes + group.raw.len())?;
        self.check_limit(adding)?;
        room(&self.path, adding, self.min_free, profile)?;
        let offset = self.committed + u64::try_from(header_bytes)?;
        let mut value = json!({
            "kind":profile.group_kind(), "path":self.path,
            "format_version":1,"offset":offset,"length":group.raw.len(),
            "sha256":crate::types::hex(Sha256::digest(&group.raw)),
            "group":sequence,"packets":group.count,
            "first_packet_index":group.first_index,"last_packet_index":packets,
            "first_received_monotonic_ns":group.first_received,
            "last_received_monotonic_ns":group.last_received,
            "first_sensor_timestamp_us_raw":group.first_timestamp,
            "last_sensor_timestamp_us_raw":group.last_timestamp,
            "return_modes_raw":(0..3u8).filter(|bit| group.modes & (1u8 << *bit) != 0)
                .map(|bit| 0x37 + bit).collect::<Vec<_>>(),
            "product_id_raw":profile.product(),"packet_bytes":PACKET_BYTES,
            "record_header_bytes":RECORD_HEADER
        });
        profile.annotate(&mut value);
        if self.sink.is_none() {
            self.sink = Some(Box::new(Disk(storage::Recording::create(
                &self.path,
                Some(self.max_bytes),
                self.min_free,
            )?)));
        }
        let sink = self
            .sink
            .as_deref_mut()
            .with_context(|| profile.message("archive is unavailable"))?;
        let written = (|| {
            if first {
                sink.append(&self.header)?;
            }
            sink.append(&group.raw)?;
            sink.sync(first)
        })();
        if let Err(error) = written {
            if let Err(rollback) = sink.rollback(self.committed) {
                self.rollback_error = Some(format!("{rollback:#}"));
            }
            return Err(with_details(
                error.context(format!(
                    "commit {} group; prior published prefix retained",
                    profile.label()
                )),
                profile.details(self.prefix()),
            ));
        }
        if first {
            self.digest.update(&self.header);
        }
        self.digest.update(&group.raw);
        self.committed += adding;
        self.groups = sequence;
        self.packets = packets;
        self.last_group = Some(value.clone());
        Ok(value)
    }
    fn prefix(&self) -> Value {
        let physical = self
            .sink
            .as_ref()
            .map_or(self.final_bytes, |sink| sink.bytes());
        let mut value = json!({
            "kind":self.profile.file_kind(), "path":self.path,"format_version":1,
            "committed_bytes":self.committed,"groups":self.groups,"packets":self.packets,
            "sha256_committed_prefix":crate::types::hex(self.digest.clone().finalize()),
            "published_prefix_only":true,"file_bytes":physical,
            "unpublished_tail_bytes":physical.saturating_sub(self.committed)
        });
        self.profile.annotate(&mut value);
        if let Some(last) = &self.last_group {
            value["last_group"] = last.clone();
        }
        if let Some(error) = &self.rollback_error {
            value["rollback_error"] = error.clone().into();
        }
        value
    }
    fn finish(&mut self) -> Result<()> {
        let Some(sink) = self.sink.take() else {
            return Ok(());
        };
        self.final_bytes = sink.bytes();
        let keep = self.groups != 0;
        let result = sink.close(keep);
        if !keep && result.is_ok() {
            self.final_bytes = 0;
        }
        result
    }
}
impl Drop for Journal {
    fn drop(&mut self) {
        if self.groups == 0
            && let Some(sink) = self.sink.take()
        {
            drop(sink.close(false));
        }
    }
}

fn room(path: &std::path::Path, adding: u64, minimum: Option<u64>, profile: Profile) -> Result<()> {
    let Some(space) = storage::space(path) else {
        return Ok(());
    };
    let floor = storage::floor(space.total)?.max(minimum.unwrap_or(0));
    ensure!(
        floor == 0 || space.free.saturating_sub(adding) >= floor,
        Categorized::new(
            ErrorKind::StorageFull,
            profile.message("group would cross the free-space floor")
        )
        .details(
            json!({"path":path,"free_bytes":space.free,"min_free_bytes":floor,"group_bytes":adding})
        )
    );
    Ok(())
}
fn incomplete(message: impl Into<String>) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}

struct Stream {
    socket: Option<UdpSocket>,
    source: SocketAddrV4,
    signal: String,
    packets_per_group: u16,
    journal: Journal,
    observed: Option<u64>,
    cutoff: Option<u64>,
    terminal: Option<Reading>,
}
impl Stream {
    fn acquire(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let profile = self.journal.profile;
        let packets_per_group = self.journal.packet_capacity(self.packets_per_group)?;
        let socket = self
            .socket
            .as_ref()
            .with_context(|| profile.message("source was retired"))?;
        let deadline = Instant::now() + timeout;
        let mut group_end = deadline;
        let mut group = Group::new(packets_per_group);
        let mut buffer = [0; PACKET_BYTES + 1];
        let mut foreign_bytes = 0usize;
        let mut foreign_packets = 0usize;
        loop {
            let remaining = group_end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(remaining))?;
            let (length, source) = match socket.recv_from(&mut buffer) {
                Ok(packet) => packet,
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
                    return Err(error)
                        .with_context(|| format!("receive {} datagram", profile.label()));
                }
            };
            let received = crate::host::monotonic_ns()
                .with_context(|| profile.message("receive clock unavailable"))?;
            if self.cutoff.is_some_and(|cutoff| received > cutoff) {
                break;
            }
            if source != SocketAddr::V4(self.source) {
                foreign_packets += 1;
                foreign_bytes += length;
                ensure!(
                    foreign_packets <= MAX_FOREIGN_PACKETS && foreign_bytes <= MAX_FOREIGN_BYTES,
                    incomplete(
                        profile
                            .message("foreign-source traffic exceeded the bounded read allowance")
                    )
                );
                continue;
            }
            let raw = buffer
                .get(..length)
                .with_context(|| format!("invalid {} datagram extent", profile.label()))?;
            // A timer is a batching boundary, not permission to discard an
            // in-cutoff datagram already received from the configured peer.
            if self.push_packet(
                &mut group,
                raw,
                received,
                packets_per_group,
                Instant::now() >= group_end,
            )? {
                break;
            }
            if group.count == 1 {
                group_end = deadline.min(Instant::now() + GROUP_WINDOW);
            }
        }
        if group.count == 0 {
            return Ok(Vec::new());
        }
        let value = self.journal.commit(&mut group)?;
        self.observed = Some(group.last_received);
        Ok(vec![Reading {
            signal: self.signal.clone(),
            value,
            source_time: None,
        }])
    }
    fn push_packet(
        &self,
        group: &mut Group,
        raw: &[u8],
        received: u64,
        limit: u16,
        window_elapsed: bool,
    ) -> Result<bool> {
        let profile = self.journal.profile;
        let packet = Packet::parse(raw, profile)?;
        let index = self
            .journal
            .packets
            .checked_add(u64::from(group.count) + 1)
            .with_context(|| profile.message("host packet index exhausted"))?;
        group.push(raw, packet, index, received);
        Ok(group.count == u32::from(limit) || window_elapsed)
    }
}
impl Source for Stream {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        self.acquire(timeout).map_err(|error| {
            let profile = self.journal.profile;
            self.socket = None;
            // Preserve cleanup failures in the acquisition error itself:
            // the coordinator retains the original error after finish().
            let error = if self.journal.groups == 0 {
                match self.journal.finish() {
                    Ok(()) => error,
                    Err(cleanup) => error.context(format!(
                        "cannot remove unpublished {} archive: {cleanup:#}",
                        profile.label()
                    )),
                }
            } else {
                error
            };
            with_details(error, profile.details(self.journal.prefix()))
        })
    }
    fn ready(&self) -> bool {
        self.journal.groups != 0
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
        self.socket = None;
        let result = self.journal.finish();
        let mut value = self.journal.prefix();
        value["storage_finalized"] = result.is_ok().into();
        if self.journal.groups > 0 {
            self.terminal = Some(Reading {
                signal: self.signal.clone(),
                value: value.clone(),
                source_time: None,
            });
        }
        result.map_err(|error| with_details(error, self.journal.profile.details(value)))
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        Ok(self.terminal.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn config() -> Config {
        serde_json::from_value(json!({
            "listen":"0.0.0.0:2368","source":"192.168.1.201:2368",
            "output":"capture.vlp16","max_bytes":1_000_000
        }))
        .unwrap()
    }

    /// Independently constructed from Rev F §9.3.2, not edited capture bytes.
    fn packet(mode: u8, timestamp: u32) -> Vec<u8> {
        packet_profile(mode, timestamp, Profile::Vlp16)
    }
    fn packet_profile(mode: u8, timestamp: u32, profile: Profile) -> Vec<u8> {
        let mut raw = Vec::with_capacity(PACKET_BYTES);
        for block in 0..12u16 {
            raw.extend_from_slice(&[0xff, 0xee]);
            let azimuth = if mode == 0x39 { block / 2 } else { block };
            raw.extend_from_slice(&azimuth.to_le_bytes());
            for channel in 0..32u16 {
                raw.extend_from_slice(&(block * 32 + channel).to_le_bytes());
                raw.push(u8::try_from(channel).unwrap());
            }
        }
        raw.extend_from_slice(&timestamp.to_le_bytes());
        raw.extend_from_slice(&[mode, profile.product()]);
        raw
    }

    #[test]
    fn exact_limits_and_manual_factory_bytes_remain_explicit() {
        let text = r#"{"listen":"0.0.0.0:2368","source":"192.168.1.201:2368","output":"x.vlp16","max_bytes":1e6,"packets_per_group":1.28e2,"min_free_bytes":0.0}"#;
        let parsed: Config = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.max_bytes, 1_000_000);
        assert_eq!(parsed.packets_per_group, 128);
        assert_eq!(parsed.min_free_bytes, Some(0));
        for (name, value) in [
            ("max_bytes", "1.00000000000000000001"),
            ("max_bytes", "18446744073709551616"),
            ("max_bytes", "\"1000000\""),
            ("packets_per_group", "65536"),
            ("min_free_bytes", "-1"),
        ] {
            let mut value_config = json!({
                "listen":"0.0.0.0:2368","source":"192.168.1.201:2368",
                "output":"x.vlp16","max_bytes":1_000_000
            });
            value_config[name] = serde_json::from_str(value).unwrap();
            serde_json::from_value::<Config>(value_config).unwrap_err();
        }
        for mode in [0x37, 0x38, 0x39] {
            let raw = packet(mode, 3_599_999_999);
            let decoded = Packet::parse(&raw, Profile::Vlp16).unwrap();
            assert_eq!(decoded.timestamp, 3_599_999_999);
            assert_eq!((decoded.mode, decoded.model), (mode, 0x22));
        }
        let mut invalid = packet(0x37, 0);
        assert_eq!(invalid.len(), PACKET_BYTES);
        invalid[1205] = 0x21;
        assert_eq!(
            crate::types::classify(&Packet::parse(&invalid, Profile::Vlp16).unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        invalid[1205] = 0x22;
        invalid[1204] = 0x36;
        assert!(
            Packet::parse(&invalid, Profile::Vlp16)
                .unwrap_err()
                .to_string()
                .contains("return mode")
        );
    }

    #[derive(Default)]
    struct Memory {
        bytes: Vec<u8>,
        syncs: u32,
        fail_sync_at: Option<u32>,
        fail_rollback: bool,
        rollbacks: u32,
        kept: Option<bool>,
    }
    struct MemorySink(Arc<Mutex<Memory>>);
    impl Sink for MemorySink {
        fn append(&mut self, bytes: &[u8]) -> Result<()> {
            self.0
                .lock()
                .map_err(|_| incomplete("mock archive lock poisoned"))?
                .bytes
                .extend_from_slice(bytes);
            Ok(())
        }
        fn sync(&mut self, _first: bool) -> Result<()> {
            let mut memory = self
                .0
                .lock()
                .map_err(|_| incomplete("mock archive lock poisoned"))?;
            memory.syncs += 1;
            ensure!(
                memory.fail_sync_at != Some(memory.syncs),
                "injected sync failure"
            );
            Ok(())
        }
        fn rollback(&mut self, length: u64) -> Result<()> {
            let mut memory = self
                .0
                .lock()
                .map_err(|_| incomplete("mock archive lock poisoned"))?;
            memory.rollbacks += 1;
            ensure!(!memory.fail_rollback, "injected truncate failure");
            memory.bytes.truncate(usize::try_from(length)?);
            Ok(())
        }
        fn bytes(&self) -> u64 {
            u64::try_from(self.0.lock().unwrap().bytes.len()).unwrap()
        }
        fn close(self: Box<Self>, keep: bool) -> Result<()> {
            let mut memory = self
                .0
                .lock()
                .map_err(|_| incomplete("mock archive lock poisoned"))?;
            memory.kept = Some(keep);
            if !keep {
                memory.bytes.clear();
            }
            Ok(())
        }
    }

    fn group(timestamp: u32, index: u64, received: u64) -> Group {
        group_profile(Profile::Vlp16, timestamp, index, received)
    }
    fn group_profile(profile: Profile, timestamp: u32, index: u64, received: u64) -> Group {
        let raw = packet_profile(0x37, timestamp, profile);
        let mut group = Group::new(1);
        group.push(&raw, Packet::parse(&raw, profile).unwrap(), index, received);
        group
    }

    fn stream(journal: Journal) -> Stream {
        Stream {
            socket: None,
            source: config().source,
            signal: default_signal(),
            packets_per_group: 128,
            journal,
            observed: Some(u64::MAX - 1),
            cutoff: None,
            terminal: None,
        }
    }

    #[test]
    fn elapsed_group_window_flushes_the_received_packet_without_dropping_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = stream(Journal::new(
            directory.path().join("raw.vlp16"),
            &config(),
            0,
            Profile::Vlp16,
        ));
        source.cutoff = Some(300);
        let mut group = group(7, 1, 100);
        let next = packet(0x38, 8);
        // The next recv completed just after the internal batching timer.
        // Its receive clock is still inside acquisition, so it belongs to
        // this exact committed group before the caller ends its read.
        assert!(
            source
                .push_packet(&mut group, &next, 200, 128, true)
                .unwrap()
        );
        assert_eq!(group.count, 2);
        assert_eq!(group.last_received, 200);
        assert_eq!(
            &group.raw[GROUP_HEADER + RECORD_BYTES + RECORD_HEADER..],
            next
        );
    }

    #[test]
    fn complete_unadvertised_footer_cannot_advance_failed_sync_high_water_mark() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::new(
            directory.path().join("raw.vlp16"),
            &config(),
            0,
            Profile::Vlp16,
        );
        let memory = Arc::new(Mutex::new(Memory::default()));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        let first = journal.commit(&mut group(7, 1, u64::MAX - 1)).unwrap();
        let committed = memory.lock().unwrap().bytes.clone();
        let prefix = journal.prefix();
        assert_eq!(prefix["committed_bytes"], committed.len());
        assert_eq!(
            prefix["sha256_committed_prefix"],
            crate::types::hex(Sha256::digest(&committed))
        );
        {
            let mut memory = memory.lock().unwrap();
            memory.fail_sync_at = Some(2);
            memory.fail_rollback = true;
        }
        let error = journal.commit(&mut group(8, 2, u64::MAX)).unwrap_err();
        let details = crate::types::classify(&error).details.unwrap();
        let failed = &details["velodyne_vlp16"];
        assert_eq!(failed["committed_bytes"], prefix["committed_bytes"]);
        assert_eq!(
            failed["sha256_committed_prefix"],
            prefix["sha256_committed_prefix"]
        );
        assert_eq!(failed["last_group"], first);
        assert_eq!(
            (failed["groups"].as_u64(), failed["packets"].as_u64()),
            (Some(1), Some(1))
        );
        assert!(
            failed["rollback_error"]
                .as_str()
                .unwrap()
                .contains("truncate")
        );
        let raw = memory.lock().unwrap().bytes.clone();
        assert_eq!(&raw[..committed.len()], committed.as_slice());
        let tail = &raw[committed.len()..];
        assert_eq!(
            &tail[tail.len() - GROUP_FOOTER..tail.len() - GROUP_FOOTER + 8],
            b"CFVCMT1\0"
        );
        // The tail is structurally complete, but was never committed/published.
        let expected: [u8; 32] = Sha256::digest(&tail[..tail.len() - GROUP_FOOTER]).into();
        assert_eq!(&tail[tail.len() - 32..], expected.as_slice());
        assert_eq!(failed["unpublished_tail_bytes"], tail.len());
        let mut source = stream(journal);
        source.finish(Duration::from_secs(1)).unwrap();
        let terminal = source.drain().unwrap().unwrap().value;
        assert_eq!(terminal["committed_bytes"], prefix["committed_bytes"]);
        assert_eq!(
            terminal["sha256_committed_prefix"],
            prefix["sha256_committed_prefix"]
        );
        assert_eq!(terminal["last_group"], first);
        assert_eq!(terminal["unpublished_tail_bytes"], tail.len());
        assert_eq!(terminal["storage_finalized"], true);
        assert!(source.drain().unwrap().is_none());
        assert_eq!(memory.lock().unwrap().kept, Some(true));
    }

    #[test]
    fn rollback_preserves_first_range_and_exact_byte_ceiling_stops_before_io() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config();
        settings.max_bytes = u64::try_from(MIN_FILE_BYTES).unwrap();
        let mut journal = Journal::new(
            directory.path().join("raw.vlp16"),
            &settings,
            42,
            Profile::Vlp16,
        );
        let memory = Arc::new(Mutex::new(Memory::default()));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        assert_eq!(journal.packet_capacity(128).unwrap(), 1);
        let descriptor = journal.commit(&mut group(0, 1, u64::MAX)).unwrap();
        assert_eq!(descriptor["last_received_monotonic_ns"], u64::MAX);
        assert_eq!(journal.committed, settings.max_bytes);
        assert!(
            journal
                .packet_capacity(128)
                .unwrap_err()
                .to_string()
                .contains("max_bytes")
        );
        assert_eq!(memory.lock().unwrap().syncs, 1);
        journal.max_bytes = 1_000_000;
        memory.lock().unwrap().fail_sync_at = Some(2);
        journal.commit(&mut group(1, 2, 0)).unwrap_err();
        assert_eq!(journal.prefix()["unpublished_tail_bytes"], 0);
        assert_eq!(journal.prefix()["last_group"], descriptor);
        assert_eq!(memory.lock().unwrap().rollbacks, 1);
    }

    #[test]
    fn failed_first_group_never_leaves_an_owned_unpublished_file_after_finish() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::new(
            directory.path().join("raw.vlp16"),
            &config(),
            0,
            Profile::Vlp16,
        );
        let memory = Arc::new(Mutex::new(Memory {
            fail_sync_at: Some(1),
            fail_rollback: true,
            ..Memory::default()
        }));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        let error = journal.commit(&mut group(0, 1, 1)).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).details.unwrap()["velodyne_vlp16"]["committed_bytes"],
            0
        );
        let mut source = stream(journal);
        source.finish(Duration::from_secs(1)).unwrap();
        assert!(source.drain().unwrap().is_none());
        let memory = memory.lock().unwrap();
        assert_eq!(memory.kept, Some(false));
        assert_eq!(memory.bytes, Vec::<u8>::new());
        assert_eq!(source.journal.prefix()["file_bytes"], 0);
    }

    #[test]
    fn free_space_floor_refuses_next_group_before_changing_the_committed_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::new(
            directory.path().join("raw.vlp16"),
            &config(),
            0,
            Profile::Vlp16,
        );
        let memory = Arc::new(Mutex::new(Memory::default()));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        journal.commit(&mut group(0, 1, 1)).unwrap();
        let before = journal.prefix();
        let bytes = memory.lock().unwrap().bytes.clone();
        journal.min_free = Some(u64::MAX);
        let error = journal.commit(&mut group(1, 2, 2)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::StorageFull);
        assert_eq!(journal.prefix(), before);
        assert_eq!(memory.lock().unwrap().bytes, bytes);
        assert_eq!(memory.lock().unwrap().syncs, 1);
    }
    #[test]
    fn explicit_profiles_preserve_the_exact_published_vlp_header_and_schema() {
        const HEADER: [u8; 40] = [
            b'C', b'F', b'V', b'L', b'P', b'1', b'6', 0, 1, 0, 40, 0, 0xb6, 4, 0, 0, 192, 168, 1,
            201, 0x40, 9, 0x40, 9, 42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("raw.vlp16");
        let mut journal = Journal::new(path.clone(), &config(), 42, Profile::Vlp16);
        assert_eq!(journal.header, HEADER);
        let memory = Arc::new(Mutex::new(Memory::default()));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        let value = journal.commit(&mut group(7, 1, 100)).unwrap();
        let bytes = memory.lock().unwrap().bytes.clone();
        assert_eq!(bytes.len(), 1366);
        let expected = json!({"kind":"velodyne_vlp16_group","path":path,
            "format_version":1,"offset":40,"length":1326,
            "sha256":crate::types::hex(Sha256::digest(&bytes[40..])),
            "group":1,"packets":1,"first_packet_index":1,"last_packet_index":1,
            "first_received_monotonic_ns":100,"last_received_monotonic_ns":100,
            "first_sensor_timestamp_us_raw":7,"last_sensor_timestamp_us_raw":7,
            "return_modes_raw":[55],"product_id_raw":34,"packet_bytes":1206,
            "record_header_bytes":32});
        assert_eq!(value, expected);
        assert_eq!(
            journal.prefix(),
            json!({"kind":"velodyne_vlp16_file","path":path,
            "format_version":1,"committed_bytes":1366,"groups":1,"packets":1,
            "sha256_committed_prefix":crate::types::hex(Sha256::digest(&bytes)),
            "published_prefix_only":true,"file_bytes":1366,"unpublished_tail_bytes":0,
            "last_group":expected})
        );
        let hdl = Journal::new(path, &config(), 42, Profile::Hdl32e);
        assert_eq!(&hdl.header[..8], b"CFHDL32E");
        assert_eq!(&hdl.header[8..], &HEADER[8..]);
    }

    #[test]
    fn hdl_sync_and_rollback_failure_keep_its_exact_model_and_old_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::new(
            directory.path().join("raw.hdl32e"),
            &config(),
            42,
            Profile::Hdl32e,
        );
        let memory = Arc::new(Mutex::new(Memory::default()));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        let first = journal
            .commit(&mut group_profile(Profile::Hdl32e, 7, 1, u64::MAX - 1))
            .unwrap();
        let prefix = journal.prefix();
        {
            let mut memory = memory.lock().unwrap();
            memory.fail_sync_at = Some(2);
            memory.fail_rollback = true;
        }
        let error = journal
            .commit(&mut group_profile(Profile::Hdl32e, 8, 2, u64::MAX))
            .unwrap_err();
        let details = crate::types::classify(&error).details.unwrap();
        assert!(details.get("velodyne_vlp16").is_none());
        let failed = &details["velodyne_hdl32e"];
        assert_eq!(failed["committed_bytes"], prefix["committed_bytes"]);
        assert_eq!(
            failed["sha256_committed_prefix"],
            prefix["sha256_committed_prefix"]
        );
        assert_eq!(failed["last_group"], first);
        assert_eq!(failed["model_profile"], "hdl32e");
        assert_eq!(first["product_id_raw"], 0x21);
        assert_eq!(first["kind"], "velodyne_hdl32e_group");
        assert!(failed["unpublished_tail_bytes"].as_u64().unwrap() > 0);
        let mut source = stream(journal);
        source.finish(Duration::from_secs(1)).unwrap();
        let terminal = source.drain().unwrap().unwrap().value;
        assert_eq!(terminal["kind"], "velodyne_hdl32e_file");
        assert_eq!(terminal["model_profile"], "hdl32e");
        assert_eq!(terminal["committed_bytes"], prefix["committed_bytes"]);
        assert_eq!(
            terminal["sha256_committed_prefix"],
            prefix["sha256_committed_prefix"]
        );
        assert_eq!(terminal["last_group"], first);
    }
}
