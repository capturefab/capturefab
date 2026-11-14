//! Immutable configured context and datagram groups; archive commits are not JSONL publication proof.
use super::{
    Config, FAILURE_RESERVE, Group, MAX_EVIDENCE_JSON, MAX_PACKET, MAX_VALUE_JSON, ReadingRef,
    count_json,
};
use crate::{
    storage,
    types::{Categorized, ErrorKind, with_details},
};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::Write as _,
    net::SocketAddrV4,
    path::{Path, PathBuf},
    time::Instant,
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
    directory: PathBuf,
    owned_directory: bool,
    context_only: bool,
    total_limit: u64,
    diagnostic_bytes: u64,
    evidence: Option<Value>,
    evidence_error: Option<Value>,
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
        directory: PathBuf,
        config: &Config,
        context: super::Context,
        origin: u64,
    ) -> Result<Self> {
        let super::Context { raw, supplied } = context;
        let path = directory.join("datagrams.sickcompact");
        let packet_bytes = MAX_PACKET;
        let minimum_packet_bytes = config.stream.minimum_packet();
        let profile_id = config.profile.id();
        let stream_id = config.stream.id();
        let context_hash = Sha256::digest(&raw);
        let context = supplied.then(|| json!({"offset":FILE_HEADER,"length":raw.len(),"sha256":crate::types::hex(context_hash)}));
        let summary = json!({"profile":config.profile.name(),"profile_source":"configured","stream":config.stream.name(),
            "identity_status":"peer_filter_only","context_status":if supplied {"configured_unverified"}else{"absent"}});
        let mut header = Vec::with_capacity(FILE_HEADER + raw.len());
        header.extend_from_slice(b"CFSCMP01");
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
        header.extend_from_slice(&stream_id.to_le_bytes());
        header.extend_from_slice(&u16::try_from(minimum_packet_bytes)?.to_le_bytes());
        header.extend_from_slice(&[0; 4]);
        ensure!(
            header.len() == FILE_HEADER,
            "invalid SICK Compact archive header"
        );
        header.extend_from_slice(&raw);
        let minimum = header
            .len()
            .checked_add(GROUP_HEADER + RECORD_HEADER + minimum_packet_bytes + FOOTER)
            .context("SICK Compact archive size overflow")?;
        ensure!(
            config.max_bytes >= u64::try_from(minimum)? + FAILURE_RESERVE,
            crate::types::Usage(format!(
                "SICK Compact max_bytes must allow context, one packet and failure reserve ({} bytes)",
                u64::try_from(minimum)? + FAILURE_RESERVE
            ))
        );
        let journal = Self {
            path,
            directory,
            owned_directory: false,
            context_only: false,
            total_limit: config.max_bytes,
            diagnostic_bytes: 0,
            evidence: None,
            evidence_error: None,
            groups: 0,
            packets: 0,
            header,
            context,
            summary,
            max_bytes: config.max_bytes - FAILURE_RESERVE,
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
        let bound = count_json(
            &ReadingRef {
                signal: &config.signal,
                value: &envelope,
            },
            None,
        )?;
        ensure!(
            bound
                .checked_add(512)
                .is_some_and(|bytes| bytes < MAX_VALUE_JSON),
            crate::types::Usage(
                "SICK Compact configured envelope exceeds the JSON admission bound".into()
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
            .context("SICK Compact archive offset overflow")
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
        let mut value = json!({"kind":"sick_compact_udp_group","format_version":1,"path":self.path,
            "offset":self.next_offset()?,"length":raw_bytes + FOOTER,"sha256":"0".repeat(64),
            "group":self.groups.checked_add(1).context("SICK Compact group index exhausted")?,
            "packet_count":count,"first_packet_index":self.packets.checked_add(1).context("SICK Compact packet index exhausted")?,
            "last_packet_index":self.packets.checked_add(u64::try_from(count)?).context("SICK Compact packet index exhausted")?,
            "first_received_monotonic_ns":first_received,"last_received_monotonic_ns":last_received,
            "sensor":self.summary,"datagrams":[]});
        if let Some(context) = &self.context {
            value["context"] = context.clone();
        }
        Ok(value)
    }

    pub(super) fn commit(
        &mut self,
        mut group: Group,
        signal: &str,
        deadline: Option<Instant>,
    ) -> Result<Value> {
        ensure!(
            !group.values.is_empty(),
            "cannot commit empty SICK Compact group"
        );
        let sequence = self
            .groups
            .checked_add(1)
            .context("SICK Compact group index exhausted")?;
        let count = group.values.len();
        let mut header = Vec::with_capacity(GROUP_HEADER);
        header.extend_from_slice(b"CFSCGR1\0");
        header.extend_from_slice(&sequence.to_le_bytes());
        header.extend_from_slice(&u32::try_from(count)?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(group.raw.len() - GROUP_HEADER)?.to_le_bytes());
        header.extend_from_slice(
            &self
                .packets
                .checked_add(1)
                .context("SICK Compact packet index exhausted")?
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
        group.raw.extend_from_slice(b"CFSCCM1\0");
        group.raw.extend_from_slice(&sequence.to_le_bytes());
        group.raw.extend_from_slice(&prefix.to_le_bytes());
        group.raw.extend_from_slice(&digest);
        value["sha256"] = json!(crate::types::hex(Sha256::digest(&group.raw)));
        let mut descriptor = value.clone();
        if let Some(fields) = descriptor.as_object_mut() {
            fields.remove("datagrams");
        }
        value["datagrams"] = Value::Array(group.values);
        let counted = match count_json(
            &ReadingRef {
                signal,
                value: &value,
            },
            deadline,
        ) {
            Ok(bytes) => bytes,
            Err(error) if crate::types::classify(&error).kind == ErrorKind::Timeout => {
                // These datagrams already passed admission. Complete only their bounded cleanup prefix;
                // the caller reports Timeout and releases this reading through drain.
                count_json(
                    &ReadingRef {
                        signal,
                        value: &value,
                    },
                    None,
                )?
            }
            Err(error) => return Err(error),
        };
        ensure!(
            counted <= MAX_VALUE_JSON,
            "SICK Compact native Reading exceeds its bounded JSON admission"
        );
        let first = self.groups == 0;
        let adding = u64::try_from(group.raw.len() + if first { self.header.len() } else { 0 })?;
        ensure!(
            adding <= self.max_bytes.saturating_sub(self.committed),
            Categorized::error(
                ErrorKind::StorageFull,
                "SICK Compact archive reached max_bytes"
            )
        );
        room(&self.path, adding, self.min_free)?;
        self.ensure_directory()?;
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
            .context("SICK Compact archive unavailable")?;
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
                error.context("commit SICK Compact group; prior archive prefix retained"),
                json!({"sick_compact_udp":self.prefix()}),
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
            .context("SICK Compact packet index exhausted")?;
        self.last_group = Some(descriptor);
        Ok(value)
    }
    pub(super) fn prefix(&self) -> Value {
        let physical = self
            .sink
            .as_ref()
            .map_or(self.final_bytes, |sink| sink.bytes());
        let mut value = json!({"kind":"sick_compact_udp_file","format_version":1,"path":self.path,"output_directory":self.directory,
            "diagnostic_bytes":self.diagnostic_bytes,"max_bytes":self.total_limit,"failure_reserved_bytes":FAILURE_RESERVE,
            "committed_bytes":self.committed,"groups":self.groups,"packets":self.packets,
            "sha256_committed_prefix":crate::types::hex(self.digest.clone().finalize()),
            "archive_committed_prefix_only":true,"jsonl_publication_not_proven":true,
            "file_bytes":physical,"uncommitted_tail_bytes":physical.saturating_sub(self.committed)});
        if (self.groups > 0 || self.context_only)
            && let Some(context) = &self.context
        {
            value["context"] = context.clone();
        }
        if let Some(last) = &self.last_group {
            value["last_group"] = last.clone();
        }
        if self.context_only && self.groups == 0 {
            value["context_evidence_archive"] = json!({"path":self.path,"length":self.header.len(),
                "sha256":crate::types::hex(Sha256::digest(&self.header)),"valid_group_count":0});
        }
        if let Some(evidence) = &self.evidence {
            value["rejected_datagram"] = evidence.clone();
        }
        if let Some(error) = &self.evidence_error {
            value["evidence_write_error"] = json!(error);
        }
        if let Some(error) = &self.rollback_error {
            value["rollback_error"] = json!(error);
        }
        value
    }
    fn ensure_directory(&mut self) -> Result<()> {
        if self.owned_directory {
            return Ok(());
        }
        if let Some(parent) = self.directory.parent() {
            crate::volumes::ensure_present(parent)?;
            let missing: Vec<_> = parent.ancestors().take_while(|path| {
                matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            }).map(Path::to_path_buf).collect();
            std::fs::create_dir_all(parent).context("create SICK Compact output parent")?;
            for directory in missing.iter().rev() {
                storage::Folders::of(directory).sync()?;
            }
        }
        std::fs::create_dir(&self.directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::Error::from(storage::OutputExists(self.directory.clone()))
            } else {
                anyhow::Error::from(error).context("create exclusive SICK Compact output_directory")
            }
        })?;
        self.owned_directory = true;
        let mut folders = storage::Folders::of(&self.path);
        folders.add(&self.directory);
        folders.sync()?;
        Ok(())
    }
    pub(super) fn reject(
        &mut self,
        raw: &[u8],
        received: u64,
        source: SocketAddrV4,
        cause: &anyhow::Error,
    ) -> Result<()> {
        let result = self.reject_inner(raw, received, source, cause);
        if let Err(error) = &result {
            let mut message = format!("{error:#}");
            if message.len() > 1024 {
                let mut end = 1024;
                while !message.is_char_boundary(end) {
                    end -= 1;
                }
                message.truncate(end);
            }
            self.evidence_error =
                Some(json!({"code":crate::types::classify(error).kind.code().0,"message":message}));
        }
        result
    }
    fn reject_inner(
        &mut self,
        raw: &[u8],
        received: u64,
        source: SocketAddrV4,
        cause: &anyhow::Error,
    ) -> Result<()> {
        ensure!(
            raw.len() <= MAX_PACKET,
            "SICK Compact rejected UDP extent exceeds IPv4 payload maximum"
        );
        ensure!(
            self.evidence.is_none(),
            "SICK Compact already retained a rejected datagram"
        );
        let raw_path = self.directory.join("rejected-datagram.raw");
        let metadata_path = self.directory.join("rejected-datagram.json");
        let hash = crate::types::hex(Sha256::digest(raw));
        let classified = crate::types::classify(cause);
        let message: String = format!("{cause:#}").chars().take(2048).collect();
        let mut metadata = json!({"kind":"sick_compact_rejected_datagram","format_version":1,
            "source":source,"received_monotonic_ns":received,"raw_length":raw.len(),"sha256":hash,
            "sensor":self.summary,"cause":{"code":classified.kind.code().0,"message":message}});
        if let Some(context) = &self.context {
            metadata["context"] = context.clone();
        }
        if self.groups == 0 {
            self.ensure_directory()?;
            if self.sink.is_none() {
                self.sink = Some(Box::new(Disk(storage::Recording::create(
                    &self.path,
                    Some(self.max_bytes),
                    self.min_free,
                )?)));
            }
            room(&self.path, u64::try_from(self.header.len())?, self.min_free)?;
            let sink = self
                .sink
                .as_deref_mut()
                .context("SICK Compact context evidence archive unavailable")?;
            ensure!(
                sink.bytes() == 0,
                "SICK Compact uncommitted archive tail prevents context-only evidence"
            );
            sink.append(&self.header)?;
            sink.sync(true)?;
            self.context_only = true;
            metadata["context_evidence_archive"] = json!({"file":"datagrams.sickcompact","length":self.header.len(),
                "sha256":crate::types::hex(Sha256::digest(&self.header)),"valid_group_count":0});
        }
        if let Some(context) = metadata.get_mut("context") {
            context["file"] = json!("datagrams.sickcompact");
        }
        let bytes = serde_json::to_vec(&metadata)?;
        ensure!(
            bytes.len() <= MAX_EVIDENCE_JSON,
            "SICK Compact rejection metadata exceeds reserved extent"
        );
        let adding = u64::try_from(raw.len() + bytes.len())?;
        let physical = self
            .sink
            .as_ref()
            .map_or(self.final_bytes, |sink| sink.bytes());
        ensure!(
            physical
                .checked_add(self.diagnostic_bytes)
                .and_then(|size| size.checked_add(adding))
                .is_some_and(|end| end <= self.total_limit),
            Categorized::error(
                ErrorKind::StorageFull,
                "SICK Compact evidence exceeds max_bytes"
            )
        );
        room(&raw_path, adding, self.min_free)?;
        self.ensure_directory()?;
        storage::save_one(raw, &raw_path, self.min_free)?;
        self.diagnostic_bytes += u64::try_from(raw.len())?;
        self.evidence = Some(
            json!({"raw":{"path":raw_path,"length":raw.len(),"sha256":hash},"complete":false}),
        );
        storage::save_one(&bytes, &metadata_path, self.min_free)?;
        self.diagnostic_bytes += u64::try_from(bytes.len())?;
        if let Some(evidence) = &mut self.evidence {
            evidence["metadata"] = json!({"path":metadata_path,"length":bytes.len(),"sha256":crate::types::hex(Sha256::digest(&bytes))});
            evidence["complete"] = json!(true);
        }
        Ok(())
    }
    fn remove_empty_directory(&mut self) -> Result<()> {
        if self.owned_directory
            && self.groups == 0
            && !self.context_only
            && self.diagnostic_bytes == 0
        {
            match std::fs::remove_dir(&self.directory) {
                Ok(()) => self.owned_directory = false,
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => (),
                Err(error) => {
                    return Err(error).context("remove empty owned SICK Compact output_directory");
                }
            }
        }
        Ok(())
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        if let Some(sink) = self.sink.take() {
            self.final_bytes = sink.bytes();
            let keep = self.groups > 0 || self.context_only;
            sink.close(keep)?;
            if !keep {
                self.final_bytes = 0;
            }
        }
        self.remove_empty_directory()
    }
}
impl Drop for Journal {
    fn drop(&mut self) {
        if self.groups == 0
            && !self.context_only
            && let Some(sink) = self.sink.take()
        {
            drop(sink.close(false));
        }
        drop(self.remove_empty_directory());
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
            "SICK Compact group would cross the free-space floor"
        )
        .details(
            json!({"path":path,"free_bytes":space.free,"min_free_bytes":floor,"group_bytes":adding})
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Memory {
        raw: Vec<u8>,
        syncs: usize,
        fail_sync: Option<usize>,
        fail_rollback: bool,
        rollbacks: usize,
    }
    struct MemorySink(Arc<Mutex<Memory>>);
    impl Sink for MemorySink {
        fn append(&mut self, raw: &[u8]) -> Result<()> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("test memory sink lock poisoned"))?
                .raw
                .extend_from_slice(raw);
            Ok(())
        }
        fn sync(&mut self, _first: bool) -> Result<()> {
            let mut state = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("test memory sink lock poisoned"))?;
            state.syncs += 1;
            ensure!(
                state.fail_sync != Some(state.syncs),
                "injected sync failure"
            );
            Ok(())
        }
        fn rollback(&mut self, length: u64) -> Result<()> {
            let mut state = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("test memory sink lock poisoned"))?;
            state.rollbacks += 1;
            ensure!(!state.fail_rollback, "injected rollback failure");
            state.raw.truncate(usize::try_from(length)?);
            Ok(())
        }
        fn bytes(&self) -> u64 {
            u64::try_from(self.0.lock().unwrap().raw.len()).unwrap()
        }
        fn close(self: Box<Self>, keep: bool) -> Result<()> {
            if !keep {
                self.0
                    .lock()
                    .map_err(|_| anyhow::anyhow!("test memory sink lock poisoned"))?
                    .raw
                    .clear();
            }
            Ok(())
        }
    }
    fn journal(directory: PathBuf) -> (Journal, Arc<Mutex<Memory>>) {
        let config:Config=serde_json::from_value(json!({"listen":"127.0.0.1:56301","source_ip":"127.0.0.1","profile":"multiscan100","stream":"points","output_directory":"test-output","max_bytes":10_000_000,"min_free_bytes":0})).unwrap();
        let mut journal = Journal::new(
            directory,
            &config,
            super::super::Context::read(None).unwrap(),
            0,
        )
        .unwrap();
        let memory = Arc::new(Mutex::new(Memory::default()));
        journal.sink = Some(Box::new(MemorySink(Arc::clone(&memory))));
        (journal, memory)
    }
    fn group(journal: &Journal, received: u64) -> Group {
        let mut group = Group::new();
        let packet = super::super::Received {
            raw: vec![7; 36],
            value: json!({"kind":"fixture"}),
            received,
            source: "127.0.0.1:7502".parse().unwrap(),
        };
        assert!(
            group
                .offer(packet, journal, "datagrams", 32, None)
                .unwrap()
                .is_none()
        );
        group
    }
    #[test]
    fn failed_sync_rolls_back_only_new_group_and_cannot_advance_commit() {
        let folder = tempfile::tempdir().unwrap();
        let (mut journal, memory) = journal(folder.path().join("output"));
        journal
            .commit(group(&journal, 1), "datagrams", None)
            .unwrap();
        let before = journal.prefix();
        let raw = memory.lock().unwrap().raw.clone();
        memory.lock().unwrap().fail_sync = Some(2);
        let error = journal
            .commit(group(&journal, 2), "datagrams", None)
            .unwrap_err();
        assert!(error.to_string().contains("commit SICK Compact group"));
        assert_eq!(
            journal.prefix()["committed_bytes"],
            before["committed_bytes"]
        );
        assert_eq!(
            journal.prefix()["sha256_committed_prefix"],
            before["sha256_committed_prefix"]
        );
        assert_eq!(journal.groups, 1);
        assert_eq!(journal.packets, 1);
        assert_eq!(memory.lock().unwrap().rollbacks, 1);
        assert_eq!(memory.lock().unwrap().raw, raw);
        assert_eq!(journal.prefix()["uncommitted_tail_bytes"], 0);
        assert_eq!(journal.prefix()["jsonl_publication_not_proven"], true);
    }
    #[test]
    fn failed_rollback_reports_tail_without_claiming_it_as_committed() {
        let folder = tempfile::tempdir().unwrap();
        let (mut journal, memory) = journal(folder.path().join("output"));
        journal
            .commit(group(&journal, 1), "datagrams", None)
            .unwrap();
        let before = journal.prefix();
        {
            let mut state = memory.lock().unwrap();
            state.fail_sync = Some(2);
            state.fail_rollback = true;
        }
        let _error = journal
            .commit(group(&journal, 2), "datagrams", None)
            .unwrap_err();
        let prefix = journal.prefix();
        assert_eq!(prefix["committed_bytes"], before["committed_bytes"]);
        assert_eq!(prefix["groups"], 1);
        assert!(prefix["uncommitted_tail_bytes"].as_u64().unwrap() > 0);
        assert!(
            prefix["rollback_error"]
                .as_str()
                .unwrap()
                .contains("injected rollback failure")
        );
    }
}
