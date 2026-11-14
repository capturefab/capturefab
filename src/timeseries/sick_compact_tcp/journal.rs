//! One synced record per complete telegram; part delivery is distinct from archive commitment.
use super::{Config, Context, FAILURE_RESERVE, MAX_DIAGNOSTIC, MAX_FRAME};
use crate::{
    storage,
    types::{Categorized, ErrorKind, Usage, with_details},
};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
    time::Instant,
};
pub(super) const HEADER: usize = 80;
pub(super) const RECORD: usize = 32;
pub(super) const FOOTER: usize = 56;
trait Sink: Send {
    fn append(&mut self, raw: &[u8]) -> Result<()>;
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
    path: PathBuf,
    directory: PathBuf,
    owned: bool,
    header: Vec<u8>,
    total_limit: u64,
    archive_limit: u64,
    min_free: Option<u64>,
    pub context: Option<Value>,
    pub sensor: Value,
    pub telegrams: u64,
    parts_admitted: u64,
    committed: u64,
    digest: Sha256,
    sink: Option<Box<dyn Sink>>,
    final_bytes: u64,
    last: Option<Value>,
    context_only: bool,
    diagnostic_bytes: u64,
    evidence: Option<Value>,
    evidence_error: Option<Value>,
    rollback_error: Option<String>,
}
impl Journal {
    pub(super) fn new(
        directory: PathBuf,
        config: &Config,
        context: Context,
        origin: u64,
    ) -> Result<Self> {
        let Context { raw, supplied } = context;
        let hash = Sha256::digest(&raw);
        let mut header = Vec::with_capacity(HEADER + raw.len());
        header.extend_from_slice(b"CFSCTP01");
        header.extend_from_slice(&1_u16.to_le_bytes());
        header.extend_from_slice(&u16::try_from(HEADER)?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(raw.len())?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(MAX_FRAME)?.to_le_bytes());
        header.extend_from_slice(&config.profile.id().to_le_bytes());
        header.extend_from_slice(&u16::from(supplied).to_le_bytes());
        header.extend_from_slice(&config.address.ip().octets());
        header.extend_from_slice(&config.address.port().to_le_bytes());
        header.extend_from_slice(&[0; 2]);
        header.extend_from_slice(&origin.to_le_bytes());
        header.extend_from_slice(&hash);
        header.extend_from_slice(&config.stream.id().to_le_bytes());
        header.extend_from_slice(
            &u16::try_from(config.stream.minimum(config.profile))?.to_le_bytes(),
        );
        header.extend_from_slice(&[0; 4]);
        ensure!(header.len() == HEADER, "invalid Compact TCP archive header");
        header.extend_from_slice(&raw);
        let minimum = header.len() + RECORD + FOOTER + config.stream.minimum(config.profile);
        ensure!(config.max_bytes>=u64::try_from(minimum)?+FAILURE_RESERVE,Usage("Compact TCP max_bytes does not include configured context and rejected-stream reserve".into()));
        let context = supplied
            .then(|| json!({"offset":HEADER,"length":raw.len(),"sha256":crate::types::hex(hash)}));
        let sensor = json!({"profile":config.profile.name(),"profile_source":"configured","stream":config.stream.name(),"identity_status":"configured_endpoint_only","context_status":if supplied{"configured_unverified"}else{"absent"}});
        Ok(Self {
            path: directory.join("telegrams.sickcompacttcp"),
            directory,
            owned: false,
            header,
            total_limit: config.max_bytes,
            archive_limit: config.max_bytes - FAILURE_RESERVE,
            min_free: config.min_free_bytes,
            context,
            sensor,
            telegrams: 0,
            parts_admitted: 0,
            committed: 0,
            digest: Sha256::new(),
            sink: None,
            final_bytes: 0,
            last: None,
            context_only: false,
            diagnostic_bytes: 0,
            evidence: None,
            evidence_error: None,
            rollback_error: None,
        })
    }
    fn record_header(&self, raw: &[u8], received: u64, offset: u64) -> Result<Vec<u8>> {
        let mut result = Vec::with_capacity(RECORD);
        result.extend_from_slice(b"TGM1");
        result.extend_from_slice(&u32::try_from(raw.len())?.to_le_bytes());
        result.extend_from_slice(
            &self
                .telegrams
                .checked_add(1)
                .context("Compact TCP index exhausted")?
                .to_le_bytes(),
        );
        result.extend_from_slice(&received.to_le_bytes());
        result.extend_from_slice(&offset.to_le_bytes());
        Ok(result)
    }
    fn footer(&self, header: &[u8], raw: &[u8]) -> Result<Vec<u8>> {
        let mut hash = Sha256::new();
        hash.update(header);
        hash.update(raw);
        let mut result = Vec::with_capacity(FOOTER);
        result.extend_from_slice(b"CFSCTC1\0");
        result.extend_from_slice(
            &self
                .telegrams
                .checked_add(1)
                .context("Compact TCP index exhausted")?
                .to_le_bytes(),
        );
        result.extend_from_slice(&u64::try_from(RECORD + raw.len())?.to_le_bytes());
        result.extend_from_slice(&hash.finalize());
        Ok(result)
    }
    pub(super) fn descriptor(&self, raw: &[u8], received: u64, offset: u64) -> Result<Value> {
        ensure!(raw.len() <= MAX_FRAME, "Compact TCP descriptor raw bound");
        let header = self.record_header(raw, received, offset)?;
        let footer = self.footer(&header, raw)?;
        let mut hash = Sha256::new();
        hash.update(&header);
        hash.update(raw);
        hash.update(&footer);
        let start = self
            .committed
            .checked_add(if self.telegrams == 0 {
                u64::try_from(self.header.len())?
            } else {
                0
            })
            .context("Compact TCP archive offset exhausted")?;
        Ok(
            json!({"path":self.path,"telegram_index":self.telegrams.checked_add(1).context("Compact TCP index exhausted")?,"offset":start,"length":RECORD+raw.len()+FOOTER,
            "sha256":crate::types::hex(hash.finalize()),"raw_offset":start+u64::try_from(RECORD)?,"raw_length":raw.len()}),
        )
    }
    pub(super) fn commit(
        &mut self,
        raw: &[u8],
        received: u64,
        offset: u64,
        parts: usize,
        deadline: Option<Instant>,
    ) -> Result<()> {
        if let Some(deadline) = deadline {
            super::before(deadline)?;
        }
        let header = self.record_header(raw, received, offset)?;
        let footer = self.footer(&header, raw)?;
        let descriptor = self.descriptor(raw, received, offset)?;
        let first = self.telegrams == 0;
        let adding =
            u64::try_from(RECORD + raw.len() + FOOTER + if first { self.header.len() } else { 0 })?;
        ensure!(
            adding <= self.archive_limit.saturating_sub(self.committed),
            Categorized::error(
                ErrorKind::StorageFull,
                "Compact TCP archive reached max_bytes"
            )
        );
        let parts_after = self
            .parts_admitted
            .checked_add(u64::try_from(parts)?)
            .context("Compact TCP part inventory exhausted")?;
        let index = self
            .telegrams
            .checked_add(1)
            .context("Compact TCP index exhausted")?;
        room(&self.path, adding, self.min_free)?;
        if let Some(deadline) = deadline {
            super::before(deadline)?;
        }
        self.ensure_directory()?;
        if let Some(deadline) = deadline {
            super::before(deadline)?;
        }
        if self.sink.is_none() {
            self.sink = Some(Box::new(Disk(storage::Recording::create(
                &self.path,
                Some(self.archive_limit),
                self.min_free,
            )?)));
        }
        let sink = self
            .sink
            .as_deref_mut()
            .context("Compact TCP archive sink unavailable")?;
        let written = (|| {
            if let Some(deadline) = deadline {
                super::before(deadline)?;
            }
            if first {
                sink.append(&self.header)?;
            }
            if let Some(deadline) = deadline {
                super::before(deadline)?;
            }
            sink.append(&header)?;
            sink.append(raw)?;
            sink.append(&footer)?;
            if let Some(deadline) = deadline {
                super::before(deadline)?;
            }
            sink.sync(first)
        })();
        if let Err(error) = written {
            if let Err(rollback) = sink.rollback(self.committed) {
                self.rollback_error = Some(format!("{rollback:#}"));
            }
            return Err(with_details(
                error.context("Compact TCP telegram commit failed; prior synced prefix retained"),
                json!({"sick_compact_tcp":self.prefix()}),
            ));
        }
        if first {
            self.digest.update(&self.header);
        }
        self.digest.update(&header);
        self.digest.update(raw);
        self.digest.update(&footer);
        self.committed += adding;
        self.telegrams = index;
        self.parts_admitted = parts_after;
        self.last = Some(descriptor);
        Ok(())
    }
    pub(super) fn prefix(&self) -> Value {
        let bytes = self
            .sink
            .as_ref()
            .map_or(self.final_bytes, |sink| sink.bytes());
        let mut value = json!({"kind":"sick_compact_tcp_file","format_version":1,"path":self.path,"output_directory":self.directory,"max_bytes":self.total_limit,
            "failure_reserved_bytes":FAILURE_RESERVE,"diagnostic_bytes":self.diagnostic_bytes,"telegrams":self.telegrams,"parts_admitted":self.parts_admitted,
            "committed_bytes":self.committed,"sha256_committed_prefix":crate::types::hex(self.digest.clone().finalize()),"file_bytes":bytes,"uncommitted_tail_bytes":bytes.saturating_sub(self.committed),
            "archive_committed_prefix_only":true,"jsonl_publication_not_proven":true,"part_delivery_crash_gap_possible":true});
        if (self.telegrams > 0 || self.context_only)
            && let Some(context) = &self.context
        {
            value["context"] = context.clone();
        }
        if let Some(last) = &self.last {
            value["last_telegram"] = last.clone();
        }
        if self.context_only && self.telegrams == 0 {
            value["context_evidence_archive"] = json!({"path":self.path,"length":self.header.len(),"sha256":crate::types::hex(Sha256::digest(&self.header)),"valid_telegram_count":0});
        }
        if let Some(evidence) = &self.evidence {
            value["rejected_stream_prefix"] = evidence.clone();
        }
        if let Some(error) = &self.evidence_error {
            value["evidence_write_error"] = error.clone();
        }
        if let Some(error) = &self.rollback_error {
            value["rollback_error"] = json!(error);
        }
        value
    }
    fn ensure_directory(&mut self) -> Result<()> {
        if self.owned {
            return Ok(());
        }
        if let Some(parent) = self.directory.parent() {
            crate::volumes::ensure_present(parent)?;
            let missing:Vec<_>=parent.ancestors().take_while(|p|matches!(std::fs::symlink_metadata(p),Err(error) if error.kind()==std::io::ErrorKind::NotFound)).map(Path::to_path_buf).collect();
            std::fs::create_dir_all(parent).context("create Compact TCP output parent")?;
            for directory in missing.iter().rev() {
                storage::Folders::of(directory).sync()?;
            }
        }
        std::fs::create_dir(&self.directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::Error::from(storage::OutputExists(self.directory.clone()))
            } else {
                anyhow::Error::from(error).context("create exclusive Compact TCP output_directory")
            }
        })?;
        self.owned = true;
        let mut folders = storage::Folders::of(&self.path);
        folders.add(&self.directory);
        folders.sync()
    }
    pub(super) fn reject(
        &mut self,
        raw: &[u8],
        received: Option<u64>,
        offset: u64,
        framed: bool,
        cause: &anyhow::Error,
    ) -> Result<()> {
        let result = self.reject_inner(raw, received, offset, framed, cause);
        if let Err(error) = &result {
            let message: String = format!("{error:#}").chars().take(2048).collect();
            self.evidence_error =
                Some(json!({"code":crate::types::classify(error).kind.code().0,"message":message}));
        }
        result
    }
    fn reject_inner(
        &mut self,
        raw: &[u8],
        received: Option<u64>,
        offset: u64,
        framed: bool,
        cause: &anyhow::Error,
    ) -> Result<()> {
        ensure!(
            raw.len() <= MAX_FRAME && self.evidence.is_none(),
            "Compact TCP rejected-prefix bound/state"
        );
        let raw_path = self.directory.join("rejected-stream.raw");
        let sidecar = self.directory.join("rejected-stream.json");
        let hash = crate::types::hex(Sha256::digest(raw));
        let scope = if framed {
            "framed_telegram"
        } else {
            "stream_prefix"
        };
        let message: String = format!("{cause:#}").chars().take(2048).collect();
        let mut metadata = json!({"kind":"sick_compact_tcp_rejected_stream_prefix","format_version":1,"source_endpoint":self.sensor_endpoint(),"stream_offset":offset,"raw_length":raw.len(),"sha256":hash,"scope":scope,
            "checksum_status":"not_accepted","sensor":self.sensor,"cause":{"code":crate::types::classify(cause).kind.code().0,"message":message}});
        if let Some(received) = received {
            metadata["last_read_monotonic_ns"] = json!(received);
        }
        if self.telegrams == 0 {
            self.ensure_directory()?;
            if self.sink.is_none() {
                self.sink = Some(Box::new(Disk(storage::Recording::create(
                    &self.path,
                    Some(self.archive_limit),
                    self.min_free,
                )?)));
            }
            room(&self.path, u64::try_from(self.header.len())?, self.min_free)?;
            let sink = self
                .sink
                .as_deref_mut()
                .context("Compact TCP context evidence archive unavailable")?;
            ensure!(
                sink.bytes() == 0,
                "Compact TCP uncommitted tail prevents claiming a context-only archive"
            );
            sink.append(&self.header)?;
            sink.sync(true)?;
            self.context_only = true;
            metadata["context_evidence_archive"] = json!({"file":"telegrams.sickcompacttcp","length":self.header.len(),"sha256":crate::types::hex(Sha256::digest(&self.header)),"valid_telegram_count":0});
        }
        if let Some(context) = &self.context {
            let mut context = context.clone();
            context["file"] = json!("telegrams.sickcompacttcp");
            metadata["context"] = context;
        }
        let bytes = serde_json::to_vec(&metadata)?;
        ensure!(
            bytes.len() <= MAX_DIAGNOSTIC,
            "Compact TCP rejection sidecar exceeds reserved capacity"
        );
        let adding = u64::try_from(raw.len() + bytes.len())?;
        let physical = self
            .sink
            .as_ref()
            .map_or(self.final_bytes, |sink| sink.bytes());
        ensure!(
            physical
                .checked_add(self.diagnostic_bytes)
                .and_then(|n| n.checked_add(adding))
                .is_some_and(|n| n <= self.total_limit),
            Categorized::error(
                ErrorKind::StorageFull,
                "Compact TCP rejected-prefix evidence exceeds max_bytes"
            )
        );
        room(&raw_path, adding, self.min_free)?;
        self.ensure_directory()?;
        storage::save_one(raw, &raw_path, self.min_free)?;
        self.diagnostic_bytes += u64::try_from(raw.len())?;
        self.evidence = Some(
            json!({"raw":{"path":raw_path,"length":raw.len(),"sha256":hash},"scope":scope,"complete":false}),
        );
        storage::save_one(&bytes, &sidecar, self.min_free)?;
        self.diagnostic_bytes += u64::try_from(bytes.len())?;
        if let Some(evidence) = &mut self.evidence {
            evidence["metadata"] = json!({"path":sidecar,"length":bytes.len(),"sha256":crate::types::hex(Sha256::digest(&bytes))});
            evidence["complete"] = json!(true);
        }
        Ok(())
    }
    fn sensor_endpoint(&self) -> String {
        let ip = std::net::Ipv4Addr::new(
            self.header[24],
            self.header[25],
            self.header[26],
            self.header[27],
        );
        let port = u16::from_le_bytes([self.header[28], self.header[29]]);
        format!("{ip}:{port}")
    }
    fn remove_empty(&mut self) -> Result<()> {
        if self.owned && self.telegrams == 0 && !self.context_only && self.diagnostic_bytes == 0 {
            match std::fs::remove_dir(&self.directory) {
                Ok(()) => self.owned = false,
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => (),
                Err(error) => return Err(error).context("remove empty Compact TCP directory"),
            }
        }
        Ok(())
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        if let Some(sink) = self.sink.take() {
            self.final_bytes = sink.bytes();
            let keep = self.telegrams > 0 || self.context_only || self.rollback_error.is_some();
            sink.close(keep)?;
            if !keep {
                self.final_bytes = 0;
            }
        }
        self.remove_empty()
    }
}
impl Drop for Journal {
    fn drop(&mut self) {
        if let Some(sink) = self.sink.take() {
            drop(
                sink.close(
                    self.telegrams > 0 || self.context_only || self.rollback_error.is_some(),
                ),
            );
        }
        drop(self.remove_empty());
    }
}
fn room(path: &Path, adding: u64, minimum: Option<u64>) -> Result<()> {
    let Some(space) = storage::space(path) else {
        return Ok(());
    };
    let floor = storage::floor(space.total)?.max(minimum.unwrap_or(0));
    ensure!(floor==0 || space.free.saturating_sub(adding)>=floor,Categorized::new(ErrorKind::StorageFull,"Compact TCP write would cross its free-space floor").details(json!({"path":path,"free_bytes":space.free,"min_free_bytes":floor,"adding_bytes":adding})));
    Ok(())
}
#[cfg(test)]
mod tests;
