//! Immutable configured context and datagram groups; archive commits are not JSONL publication proof.
use super::{Config, Group, MAX_VALUE_JSON, ReadingRef, count_json};
use crate::{
    storage,
    types::{Categorized, ErrorKind, with_details},
};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

pub(super) const FILE_HEADER: usize = 80;
pub(super) const GROUP_HEADER: usize = 32;
pub(super) const RECORD_HEADER: usize = 32;
pub(super) const FOOTER: usize = 56;
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
        self.0
            .write_all(bytes)
            .map_err(|error| self.0.alarm().error().unwrap_or_else(|| error.into()))
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
pub(super) struct Journal {
    pub path: PathBuf,
    pub groups: u64,
    pub packets: u64,
    header: Vec<u8>,
    context: Option<Value>,
    summary: Value,
    max_bytes: u64,
    min_free: Option<u64>,
    sink: Option<Box<dyn Sink>>,
    committed: u64,
    digest: Sha256,
    last_group: Option<Value>,
    rollback_error: Option<String>,
    final_bytes: u64,
}
impl Journal {
    pub(super) fn new(
        path: PathBuf,
        config: &Config,
        context: super::Context,
        format: super::Format,
        origin: u64,
    ) -> Result<Self> {
        let super::Context { raw, supplied } = context;
        let packet_bytes = super::packet::expected_packet_bytes(format);
        let decoded_bound = super::packet::decoded_json_bound(format);
        let profile_id = match format.profile {
            super::Profile::Helios16pMsop => 1_u16,
            super::Profile::Helios16pDifop => 2,
            super::Profile::Rs16Msop => 3,
            super::Profile::Rs16Difop => 4,
        };
        let context_hash = Sha256::digest(&raw);
        let context = supplied.then(|| json!({"offset":FILE_HEADER,"length":raw.len(),"sha256":crate::types::hex(context_hash)}));
        let mut summary = json!({"profile":format.profile.name(),"profile_source":"configured","identity_status":"peer_filter_only",
            "context_status":if supplied {"configured_unverified"}else{"absent"},"difop_association":"not_performed"});
        if let Some(unit) = format.rs16_distance_unit_um {
            summary["rs16_distance_unit_um"] = json!(unit);
            summary["distance_unit_status"] = json!("configured_unverified");
            summary["distance_unit_origin"] = json!("configured");
        }
        let mut header = Vec::with_capacity(FILE_HEADER + raw.len());
        header.extend_from_slice(b"CFROBO1\0");
        header.extend_from_slice(&1_u16.to_le_bytes());
        header.extend_from_slice(&u16::try_from(FILE_HEADER)?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(raw.len())?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(packet_bytes)?.to_le_bytes());
        header.extend_from_slice(&profile_id.to_le_bytes());
        header.extend_from_slice(&u16::from(supplied).to_le_bytes());
        header.extend_from_slice(&config.source_ip.octets());
        header.extend_from_slice(&config.source_port.unwrap_or(0).to_le_bytes());
        header.extend_from_slice(&config.listen.port().to_le_bytes());
        header.extend_from_slice(&origin.to_le_bytes());
        header.extend_from_slice(&context_hash);
        header
            .extend_from_slice(&u32::from(format.rs16_distance_unit_um.unwrap_or(0)).to_le_bytes());
        header.extend_from_slice(&[0; 4]);
        ensure!(
            header.len() == FILE_HEADER,
            "invalid RoboSense archive header"
        );
        header.extend_from_slice(&raw);
        let minimum = header
            .len()
            .checked_add(GROUP_HEADER + RECORD_HEADER + packet_bytes + FOOTER)
            .context("RoboSense archive size overflow")?;
        ensure!(
            config.max_bytes >= u64::try_from(minimum)?,
            crate::types::Usage(format!(
                "RoboSense max_bytes must allow context and one packet ({minimum} bytes)"
            ))
        );
        let journal = Self {
            path,
            groups: 0,
            packets: 0,
            header,
            context,
            summary,
            max_bytes: config.max_bytes,
            min_free: config.min_free_bytes,
            sink: None,
            committed: 0,
            digest: Sha256::new(),
            last_group: None,
            rollback_error: None,
            final_bytes: 0,
        };
        let envelope = journal.descriptor(
            usize::from(super::MAX_PACKETS),
            super::MAX_RAW_GROUP - FOOTER,
            u64::MAX,
            u64::MAX,
        )?;
        let bound = count_json(&ReadingRef {
            signal: &config.signal,
            value: &envelope,
        })?
        .checked_add(decoded_bound)
        .and_then(|bytes| bytes.checked_add(512))
        .context("RoboSense native JSON bound overflow")?;
        ensure!(
            bound <= MAX_VALUE_JSON,
            crate::types::Usage(
                "RoboSense configured native packet and envelope exceed the JSON admission bound"
                    .into()
            )
        );
        Ok(journal)
    }
    pub(super) fn next_offset(&self) -> Result<u64> {
        self.committed
            .checked_add(if self.groups == 0 {
                u64::try_from(self.header.len())?
            } else {
                0
            })
            .context("RoboSense archive offset overflow")
    }
    pub(super) fn room_for(&self, raw_group_bytes: usize) -> bool {
        self.next_offset()
            .ok()
            .and_then(|offset| offset.checked_add(u64::try_from(raw_group_bytes + FOOTER).ok()?))
            .is_some_and(|end| end <= self.max_bytes)
    }
    pub(super) fn descriptor(
        &self,
        count: usize,
        raw_bytes: usize,
        first_received: u64,
        last_received: u64,
    ) -> Result<Value> {
        let mut value = json!({"kind":"robosense_udp_group","format_version":1,"path":self.path,
            "offset":self.next_offset()?,"length":raw_bytes + FOOTER,"sha256":"0".repeat(64),
            "group":self.groups.checked_add(1).context("RoboSense group index exhausted")?,
            "packet_count":count,"first_packet_index":self.packets.checked_add(1).context("RoboSense packet index exhausted")?,
            "last_packet_index":self.packets.checked_add(u64::try_from(count)?).context("RoboSense packet index exhausted")?,
            "first_received_monotonic_ns":first_received,"last_received_monotonic_ns":last_received,
            "sensor":self.summary,"datagrams":[]});
        if let Some(context) = &self.context {
            value["context"] = context.clone();
        }
        Ok(value)
    }

    pub(super) fn commit(&mut self, mut group: Group, signal: &str) -> Result<Value> {
        ensure!(
            !group.values.is_empty(),
            "cannot commit empty RoboSense group"
        );
        let sequence = self
            .groups
            .checked_add(1)
            .context("RoboSense group index exhausted")?;
        let count = group.values.len();
        let mut header = Vec::with_capacity(GROUP_HEADER);
        header.extend_from_slice(b"CFRGRP1\0");
        header.extend_from_slice(&sequence.to_le_bytes());
        header.extend_from_slice(&u32::try_from(count)?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(group.raw.len() - GROUP_HEADER)?.to_le_bytes());
        header.extend_from_slice(
            &self
                .packets
                .checked_add(1)
                .context("RoboSense packet index exhausted")?
                .to_le_bytes(),
        );
        group.raw[..GROUP_HEADER].copy_from_slice(&header);
        let prefix = u64::try_from(group.raw.len())?;
        let digest = Sha256::digest(&group.raw);
        let mut value = self.descriptor(
            count,
            group.raw.len(),
            group.first_received,
            group.last_received,
        )?;
        group.raw.extend_from_slice(b"CFRCMT1\0");
        group.raw.extend_from_slice(&sequence.to_le_bytes());
        group.raw.extend_from_slice(&prefix.to_le_bytes());
        group.raw.extend_from_slice(&digest);
        value["sha256"] = json!(crate::types::hex(Sha256::digest(&group.raw)));
        let mut descriptor = value.clone();
        if let Some(fields) = descriptor.as_object_mut() {
            fields.remove("datagrams");
        }
        value["datagrams"] = Value::Array(group.values);
        ensure!(
            count_json(&ReadingRef {
                signal,
                value: &value
            })? <= MAX_VALUE_JSON,
            "RoboSense native Reading exceeds its bounded JSON admission"
        );
        let first = self.groups == 0;
        let adding = u64::try_from(group.raw.len() + if first { self.header.len() } else { 0 })?;
        ensure!(
            adding <= self.max_bytes.saturating_sub(self.committed),
            Categorized::error(
                ErrorKind::StorageFull,
                "RoboSense archive reached max_bytes"
            )
        );
        room(&self.path, adding, self.min_free)?;
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
            .context("RoboSense archive unavailable")?;
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
                error.context("commit RoboSense group; prior archive prefix retained"),
                json!({"robosense_udp":self.prefix()}),
            ));
        }
        if first {
            self.digest.update(&self.header);
        }
        self.digest.update(&group.raw);
        self.committed += adding;
        self.groups = sequence;
        self.packets = self
            .packets
            .checked_add(u64::try_from(count)?)
            .context("RoboSense packet index exhausted")?;
        self.last_group = Some(descriptor);
        Ok(value)
    }
    pub(super) fn prefix(&self) -> Value {
        let physical = self
            .sink
            .as_ref()
            .map_or(self.final_bytes, |sink| sink.bytes());
        let mut value = json!({"kind":"robosense_udp_file","format_version":1,"path":self.path,
            "committed_bytes":self.committed,"groups":self.groups,"packets":self.packets,
            "sha256_committed_prefix":crate::types::hex(self.digest.clone().finalize()),
            "archive_committed_prefix_only":true,"jsonl_publication_not_proven":true,
            "file_bytes":physical,"uncommitted_tail_bytes":physical.saturating_sub(self.committed)});
        if self.groups > 0
            && let Some(context) = &self.context
        {
            value["context"] = context.clone();
        }
        if let Some(last) = &self.last_group {
            value["last_group"] = last.clone();
        }
        if let Some(error) = &self.rollback_error {
            value["rollback_error"] = json!(error);
        }
        value
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        let Some(sink) = self.sink.take() else {
            return Ok(());
        };
        self.final_bytes = sink.bytes();
        let keep = self.groups > 0;
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
fn room(path: &Path, adding: u64, minimum: Option<u64>) -> Result<()> {
    let Some(space) = storage::space(path) else {
        return Ok(());
    };
    let floor = storage::floor(space.total)?.max(minimum.unwrap_or(0));
    ensure!(
        floor == 0 || space.free.saturating_sub(adding) >= floor,
        Categorized::new(
            ErrorKind::StorageFull,
            "RoboSense group would cross the free-space floor"
        )
        .details(
            json!({"path":path,"free_bytes":space.free,"min_free_bytes":floor,"group_bytes":adding})
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{Received, Stream};
    use super::*;
    use crate::timeseries::Source as _;
    use std::{
        collections::VecDeque,
        net::{Ipv4Addr, SocketAddrV4},
        sync::{Arc, Mutex},
        time::Duration,
    };
    #[derive(Default)]
    struct Memory {
        raw: Vec<u8>,
        syncs: usize,
        fail_sync: Option<usize>,
        fail_append: bool,
        fail_rollback: bool,
        rollbacks: usize,
        kept: Option<bool>,
    }
    struct MemorySink(Arc<Mutex<Memory>>);
    impl Sink for MemorySink {
        fn append(&mut self, raw: &[u8]) -> Result<()> {
            let mut memory = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("test journal lock poisoned"))?;
            if memory.fail_append {
                memory.raw.extend_from_slice(&raw[..raw.len() / 2]);
                return Err(std::io::Error::other("injected append failure").into());
            }
            memory.raw.extend_from_slice(raw);
            Ok(())
        }
        fn sync(&mut self, _first: bool) -> Result<()> {
            let mut memory = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("test journal lock poisoned"))?;
            memory.syncs += 1;
            ensure!(
                memory.fail_sync != Some(memory.syncs),
                "injected sync failure"
            );
            Ok(())
        }
        fn rollback(&mut self, length: u64) -> Result<()> {
            let mut memory = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("test journal lock poisoned"))?;
            memory.rollbacks += 1;
            ensure!(!memory.fail_rollback, "injected truncate failure");
            memory.raw.truncate(usize::try_from(length)?);
            Ok(())
        }
        fn bytes(&self) -> u64 {
            let length = self.0.lock().map_or_else(
                |error| panic!("test journal lock poisoned: {error}"),
                |memory| memory.raw.len(),
            );
            u64::try_from(length).unwrap_or_else(|error| panic!("test length exceeds u64: {error}"))
        }
        fn close(self: Box<Self>, keep: bool) -> Result<()> {
            let mut memory = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("test journal lock poisoned"))?;
            memory.kept = Some(keep);
            if !keep {
                memory.raw.clear();
            }
            Ok(())
        }
    }
    fn config() -> Config {
        serde_json::from_value(json!({"listen":"127.0.0.1:7502","source_ip":"127.0.0.1","profile":"helios16p_msop","output":"raw.robosense","max_bytes":10_000_000})).unwrap()
    }
    fn journal(path: PathBuf) -> (Journal, Arc<Mutex<Memory>>) {
        let settings = config();
        let mut journal = Journal::new(
            path,
            &settings,
            super::super::Context {
                raw: b"{\"original\":true}".to_vec(),
                supplied: true,
            },
            format(),
            0,
        )
        .unwrap();
        let memory = Arc::new(Mutex::new(Memory::default()));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        (journal, memory)
    }
    fn packet(received: u64, padding: usize) -> Received {
        Received {
            raw: vec![u8::try_from(received).unwrap(); 1248],
            value: json!({"kind":"fixture","padding":"x".repeat(padding)}),
            elements: 1,
            received,
            source: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7502),
        }
    }
    fn group(journal: &Journal, received: u64, padding: usize) -> Group {
        let mut group = Group::new();
        assert!(
            group
                .offer(packet(received, padding), journal, "datagrams", 32)
                .unwrap()
                .is_none()
        );
        group
    }
    fn format() -> super::super::Format {
        super::super::Format {
            profile: super::super::Profile::Helios16pMsop,
            rs16_distance_unit_um: None,
        }
    }
    fn stream(journal: Journal) -> Stream {
        Stream {
            socket: None,
            source_ip: Ipv4Addr::LOCALHOST,
            source_port: None,
            signal: "datagrams".into(),
            packets_per_group: 32,
            format: format(),
            journal,
            pending: None,
            observed: None,
            cutoff: Some(2),
            terminal: VecDeque::new(),
            failed: false,
            finished: false,
            foreign_packets: 0,
            uncommitted_packets: 0,
        }
    }
    #[test]
    fn original_context_presence_and_configured_unit_are_preserved_as_separate_header_facts() {
        let folder = tempfile::tempdir().unwrap();
        for (profile, unit, id) in [
            (super::super::Profile::Helios16pMsop, None, 1_u16),
            (super::super::Profile::Helios16pDifop, None, 2),
            (super::super::Profile::Rs16Msop, Some(5_000_u16), 3),
            (super::super::Profile::Rs16Msop, Some(10_000), 3),
            (super::super::Profile::Rs16Difop, None, 4),
        ] {
            let settings = config();
            let journal = Journal::new(
                folder.path().join("raw.robosense"),
                &settings,
                super::super::Context {
                    raw: Vec::new(),
                    supplied: true,
                },
                super::super::Format {
                    profile,
                    rs16_distance_unit_um: unit,
                },
                0,
            )
            .unwrap();
            assert_eq!(&journal.header[..8], b"CFROBO1\0");
            assert_eq!(
                u16::from_le_bytes(journal.header[20..22].try_into().unwrap()),
                id
            );
            assert_eq!(
                u16::from_le_bytes(journal.header[22..24].try_into().unwrap()),
                1
            );
            assert_eq!(
                u32::from_le_bytes(journal.header[72..76].try_into().unwrap()),
                u32::from(unit.unwrap_or(0))
            );
            assert_eq!(&journal.header[76..80], &[0; 4]);
            let descriptor = journal
                .descriptor(1, GROUP_HEADER + RECORD_HEADER + 1248, 1, 1)
                .unwrap();
            assert_eq!(descriptor["context"]["length"], 0);
            assert_eq!(descriptor["sensor"]["difop_association"], "not_performed");
            if let Some(unit) = unit {
                assert_eq!(descriptor["sensor"]["rs16_distance_unit_um"], unit);
                assert_eq!(
                    descriptor["sensor"]["distance_unit_status"],
                    "configured_unverified"
                );
                assert_eq!(descriptor["sensor"]["distance_unit_origin"], "configured");
            } else {
                assert!(descriptor["sensor"].get("rs16_distance_unit_um").is_none());
            }
        }
    }
    #[test]
    fn thirty_two_msop_packets_fit_the_full_tuple_budget_without_mutating_overflow() {
        let folder = tempfile::tempdir().unwrap();
        let (mut journal, memory) = journal(folder.path().join("raw.robosense"));
        let mut group = Group::new();
        for index in 1..=32 {
            let mut accepted = packet(index, 0);
            accepted.elements = 384;
            assert!(
                group
                    .offer(accepted, &journal, "datagrams", 32)
                    .unwrap()
                    .is_none()
            );
        }
        let before = group.raw.clone();
        let mut extra = packet(33, 0);
        extra.elements = 384;
        assert!(
            group
                .offer(extra, &journal, "datagrams", 32)
                .unwrap()
                .is_some()
        );
        assert_eq!(group.values.len(), 32);
        assert_eq!(group.elements, 12_288);
        assert_eq!(group.raw, before);
        let value = journal.commit(group, "datagrams").unwrap();
        assert_eq!(value["packet_count"], 32);
        assert_eq!(value["datagrams"][31]["host_packet_index"], 32);
        assert_eq!(memory.lock().unwrap().syncs, 1);
    }
    #[test]
    fn failed_sync_and_rollback_cannot_advance_archive_commit_or_claim_jsonl_publication() {
        let folder = tempfile::tempdir().unwrap();
        let (mut journal, memory) = journal(folder.path().join("raw.robosense"));
        journal.commit(group(&journal, 1, 0), "datagrams").unwrap();
        let before = journal.prefix();
        let bytes = memory.lock().unwrap().raw.clone();
        {
            let mut memory = memory.lock().unwrap();
            memory.fail_sync = Some(2);
            memory.fail_rollback = true;
        }
        let error = journal
            .commit(group(&journal, 2, 0), "datagrams")
            .unwrap_err();
        assert!(error.to_string().contains("commit RoboSense group"));
        let after = journal.prefix();
        assert_eq!(after["committed_bytes"], before["committed_bytes"]);
        assert_eq!(
            after["sha256_committed_prefix"],
            before["sha256_committed_prefix"]
        );
        assert_eq!(after["groups"], 1);
        assert_eq!(after["packets"], 1);
        assert_eq!(after["jsonl_publication_not_proven"], true);
        assert!(after["uncommitted_tail_bytes"].as_u64().unwrap() > 0);
        assert!(
            after["rollback_error"]
                .as_str()
                .unwrap()
                .contains("truncate")
        );
        let memory = memory.lock().unwrap();
        assert_eq!(&memory.raw[..bytes.len()], &bytes);
        let tail = &memory.raw[bytes.len()..];
        assert_eq!(
            &tail[tail.len() - FOOTER..tail.len() - FOOTER + 8],
            b"CFRCMT1\0"
        );
    }
    #[test]
    fn json_overflow_packet_is_owned_then_committed_and_drained_after_cutoff_with_original_clock() {
        let folder = tempfile::tempdir().unwrap();
        let (journal, memory) = journal(folder.path().join("raw.robosense"));
        let mut source = stream(journal);
        let mut group = group(&source.journal, 1, 600_000);
        source.pending = group
            .offer(packet(2, 600_000), &source.journal, &source.signal, 32)
            .unwrap();
        assert!(source.pending.is_some());
        let readings = source.finish_group(group).unwrap();
        assert_eq!(readings.len(), 1);
        source.finish(Duration::ZERO).unwrap();
        let pending = source.drain().unwrap().unwrap().value;
        assert_eq!(pending["packet_count"], 1);
        assert_eq!(pending["datagrams"][0]["host_packet_index"], 2);
        assert_eq!(pending["datagrams"][0]["received_monotonic_ns"], 2);
        assert_eq!(source.observed_ns(), Some(2));
        let terminal = source.drain().unwrap().unwrap().value;
        assert_eq!(terminal["packets"], 2);
        assert_eq!(terminal["storage_finalized"], true);
        assert!(source.drain().unwrap().is_none());
        assert_eq!(memory.lock().unwrap().syncs, 2);
        let packet_offset =
            usize::try_from(pending["datagrams"][0]["raw_offset"].as_u64().unwrap()).unwrap();
        assert_eq!(
            &memory.lock().unwrap().raw[packet_offset..packet_offset + 1248],
            &[2; 1248]
        );
    }
    #[test]
    fn failed_first_append_removes_only_owned_uncommitted_archive_and_has_no_context_ref() {
        let folder = tempfile::tempdir().unwrap();
        let (mut journal, memory) = journal(folder.path().join("raw.robosense"));
        memory.lock().unwrap().fail_append = true;
        journal
            .commit(group(&journal, 1, 0), "datagrams")
            .unwrap_err();
        journal.finish().unwrap();
        let prefix = journal.prefix();
        assert_eq!(prefix["committed_bytes"], 0);
        assert!(prefix.get("context").is_none());
        assert_eq!(memory.lock().unwrap().kept, Some(false));
        assert_eq!(memory.lock().unwrap().raw, [] as [u8; 0]);
    }
    #[test]
    fn exact_max_bytes_stops_before_another_write_and_failed_source_never_retries_pending() {
        let folder = tempfile::tempdir().unwrap();
        let (mut journal, memory) = journal(folder.path().join("raw.robosense"));
        let first = group(&journal, 1, 0);
        journal.max_bytes = u64::try_from(journal.header.len() + first.raw.len() + FOOTER).unwrap();
        journal.commit(first, "datagrams").unwrap();
        let before = memory.lock().unwrap().raw.clone();
        let mut next = Group::new();
        assert!(
            next.offer(packet(2, 0), &journal, "datagrams", 32)
                .unwrap_err()
                .to_string()
                .contains("max_bytes")
        );
        let mut source = stream(journal);
        source.failed = true;
        source.pending = Some(packet(2, 0));
        source.finish(Duration::ZERO).unwrap();
        assert_eq!(memory.lock().unwrap().raw, before);
        assert_eq!(memory.lock().unwrap().syncs, 1);
        let terminal = source.drain().unwrap().unwrap().value;
        assert_eq!(terminal["pending_accepted_packets"], 1);
        assert_eq!(terminal["committed_bytes"], before.len());
    }

    #[test]
    fn pending_packet_that_reaches_storage_limit_stays_accounted_on_read_and_finish() {
        for during_read in [false, true] {
            let folder = tempfile::tempdir().unwrap();
            let (mut journal, memory) = journal(folder.path().join("raw.robosense"));
            journal.commit(group(&journal, 1, 0), "datagrams").unwrap();
            journal.max_bytes = journal.committed;
            let before = memory.lock().unwrap().raw.clone();
            let mut source = stream(journal);
            source.pending = Some(packet(2, 0));
            let error = if during_read {
                let error = source.read(Duration::ZERO).unwrap_err();
                source.finish(Duration::ZERO).unwrap();
                error
            } else {
                source.finish(Duration::ZERO).unwrap_err()
            };
            let classified = crate::types::classify(&error);
            assert_eq!(classified.kind, ErrorKind::StorageFull);
            let details = classified.details.unwrap();
            assert_eq!(details["robosense_udp"]["uncommitted_packets"], 1);
            assert_eq!(details["robosense_udp"]["packets"], 1);
            let terminal = source.drain().unwrap().unwrap().value;
            assert_eq!(terminal["uncommitted_packets"], 1);
            assert_eq!(terminal["committed_bytes"], before.len());
            assert_eq!(memory.lock().unwrap().raw, before);
            assert_eq!(memory.lock().unwrap().syncs, 1);
            assert!(source.drain().unwrap().is_none());
        }
    }
}
