//! Dataset catalog: one `.duckdb` file per dataset, owned by the session
//! coordinator (never a camera worker), recording every saved capture and
//! recording, signal samples and notes, with exports for later analysis.
pub mod coordinator;
mod db;
pub mod export;
mod query;
#[cfg(test)]
mod tests;

pub use db::Dataset;
pub use query::Reader;

use crate::{context::Context, types::CameraInfo};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::Duration,
};

pub const SCHEMA_VERSION: u32 = 1;
/// Saved-file entries a worker keeps until the coordinator collects them.
pub const SAVED_LOG_CAP: usize = 100_000;

/// The camera that produced a batch of saved files. Its source address is
/// redacted before it leaves the worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedCamera {
    pub id: String,
    pub transport: String,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}
impl From<&CameraInfo> for SavedCamera {
    fn from(info: &CameraInfo) -> Self {
        Self {
            id: info.id.clone(),
            transport: serde_json::to_value(info.transport)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            vendor: info.vendor.clone(),
            model: info.model.clone(),
            serial: info.serial.clone(),
            source: info.address.as_deref().map(crate::media::redact_url),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedItem {
    Capture {
        frame_id: u64,
        timestamp_ns: u64,
        width: u32,
        height: u32,
        pixel_format: u32,
        format: String,
        /// capture or schedule.
        origin: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<u64>,
    },
    Recording {
        started_ms: u64,
        ended_ms: u64,
        codec: String,
        encoder: String,
        fps: f64,
    },
}

/// One finished file, reported by a camera worker to the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedFile {
    /// Per-worker sequence number, stamped by `push_saved`; gaps mean losses.
    #[serde(default)]
    pub seq: u64,
    /// UTC Unix milliseconds, taken when the frame was received.
    pub wall_ms: u64,
    pub path: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Context::is_empty")]
    pub context: Context,
    #[serde(flatten)]
    pub item: SavedItem,
}

/// Saved files drained from a worker in one reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SavedBatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<SavedCamera>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<SavedFile>,
    /// Entries dropped because the worker's log was full.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lost: u64,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
impl SavedBatch {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.lost == 0
    }
}

/// A worker's bounded log of saved files; overflow drops the oldest entries
/// and counts them as lost.
#[derive(Debug, Default)]
pub struct SavedLog {
    next_seq: u64,
    items: VecDeque<SavedFile>,
    lost: u64,
}
impl SavedLog {
    pub fn push(&mut self, mut file: SavedFile) {
        self.next_seq += 1;
        file.seq = self.next_seq;
        if self.items.len() >= SAVED_LOG_CAP {
            self.items.pop_front();
            self.lost += 1;
        }
        self.items.push_back(file);
    }
    /// Remove up to `max` of the oldest entries and the lost count.
    pub fn take(&mut self, max: usize) -> (Vec<SavedFile>, u64) {
        let count = max.min(self.items.len());
        (
            self.items.drain(..count).collect(),
            std::mem::take(&mut self.lost),
        )
    }
}

/// A finished local recording, reported by the forwarder when it stops.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordedFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub codec: String,
    pub encoder: String,
    pub fps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DatasetStatus {
    pub path: String,
    pub dataset_id: String,
    pub name: String,
    pub created_ms: u64,
    pub captures: u64,
    pub recordings: u64,
    pub signals: u64,
    pub samples: u64,
    pub events: u64,
    /// The database file plus its write-ahead log.
    pub bytes: u64,
    pub max_bytes: u64,
    #[serde(default)]
    pub first_ms: Option<u64>,
    #[serde(default)]
    pub last_ms: Option<u64>,
    #[serde(default)]
    pub last_saved: Option<String>,
    #[serde(default)]
    pub missed_files: u64,
    #[serde(default)]
    pub dropped_samples: u64,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenMode {
    /// Open an existing dataset or create a new one.
    #[default]
    Auto,
    /// Create a new file; refuse an existing one.
    Create,
    /// Open an existing dataset; refuse a missing file.
    Open,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Parquet,
    Json,
    Jsonl,
}

pub fn default_query_limit() -> u32 {
    1000
}

/// The coordinator's dataset slot: at most one open dataset per session,
/// shared with the signal hub (as its sample sink) and the GUI. The writer
/// lock is held only for inserts, open and close; queries and exports run on
/// cloned connections outside it.
#[derive(Default)]
pub struct Slot {
    writer: Mutex<Option<Dataset>>,
    cached: RwLock<Option<DatasetStatus>>,
    readers: Arc<query::Readers>,
    recent: Mutex<VecDeque<SavedFile>>,
    auto_path: Mutex<Option<PathBuf>>,
}

/// Saved files the slot remembers for "Reveal last save".
const RECENT: usize = 32;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Slot {
    pub fn new() -> Self {
        Self::default()
    }
    /// Open or create a dataset. Opening the file that is already open
    /// returns its status; a different file must be closed first.
    pub fn open(
        &self,
        path: &Path,
        mode: OpenMode,
        max_bytes: Option<u64>,
    ) -> Result<DatasetStatus> {
        let mut writer = lock(&self.writer);
        if let Some(open) = writer.as_ref() {
            let target = db::canonical_target(path, false).ok();
            if target.as_deref() == Some(open.path.as_path()) {
                ensure!(
                    mode != OpenMode::Create,
                    "dataset {} already exists; use --existing or choose a new name",
                    db::file_label(&open.path)
                );
                return Ok(open.status.clone());
            }
            bail!(
                "dataset {} is already open; close it first",
                db::file_label(&open.path)
            );
        }
        let dataset = Dataset::open(path, mode, max_bytes)?;
        let status = dataset.status.clone();
        *writer = Some(dataset);
        self.publish(Some(status.clone()));
        Ok(status)
    }
    /// Interrupt queries and exports, checkpoint and close.
    pub fn close(&self) -> Result<Option<DatasetStatus>> {
        if !self.readers.interrupt_and_wait(Duration::from_secs(5)) {
            eprintln!("capturefab: dataset readers still running after 5 s; closing anyway");
        }
        let mut writer = lock(&self.writer);
        let Some(mut dataset) = writer.take() else {
            return Ok(None);
        };
        let label = db::file_label(&dataset.path);
        let event = dataset.record_event("dataset_closed", None, None, &label, &Context::new());
        let checkpoint = dataset.checkpoint();
        dataset.status.bytes = dataset.size();
        let status = dataset.status.clone();
        drop(dataset);
        drop(writer);
        self.publish(None);
        event.and(checkpoint).map(|()| Some(status))
    }
    /// Close for shutdown; errors are logged, never returned.
    pub fn close_quiet(&self) {
        if let Err(e) = self.close() {
            eprintln!("capturefab: dataset close: {e:#}");
        }
    }
    fn publish(&self, status: Option<DatasetStatus>) {
        *self.cached.write().unwrap_or_else(PoisonError::into_inner) = status;
    }
    /// Run `f` on the open dataset (opening the automatic one when `auto`)
    /// and refresh the cached status, even when `f` fails.
    fn with_open<T>(
        &self,
        auto: bool,
        f: impl FnOnce(&mut Dataset) -> Result<T>,
    ) -> Option<Result<T>> {
        let mut writer = lock(&self.writer);
        if writer.is_none() && auto {
            let path = lock(&self.auto_path).clone()?;
            match Dataset::open(&path, OpenMode::Auto, None) {
                Ok(dataset) => *writer = Some(dataset),
                Err(e) => {
                    // Stop retrying on every poll; the GUI shows nothing open.
                    eprintln!("capturefab: automatic dataset {}: {e:#}", path.display());
                    *lock(&self.auto_path) = None;
                    return None;
                }
            }
        }
        let dataset = writer.as_mut()?;
        let result = f(dataset);
        if let Err(e) = &result {
            dataset.status.last_error = Some(format!("{e:#}"));
        }
        let status = dataset.status.clone();
        drop(writer);
        self.publish(Some(status));
        Some(result)
    }
    /// Catalog files a worker saved. Never fails; problems are counted.
    pub fn record_saved(&self, batch: SavedBatch, gaps: u64) {
        if batch.items.is_empty() && gaps == 0 {
            return;
        }
        {
            let mut recent = lock(&self.recent);
            for item in batch.items.iter().rev().take(RECENT).rev() {
                if recent.len() >= RECENT {
                    recent.pop_front();
                }
                recent.push_back(item.clone());
            }
        }
        let items = batch.items.len() as u64;
        let missed = gaps.saturating_add(batch.lost);
        let result = self.with_open(!batch.items.is_empty(), |dataset| {
            let mut errors = Vec::new();
            if missed > 0 {
                dataset.status.missed_files += missed;
                let text = format!("{missed} saved files were not cataloged");
                if let Err(e) = dataset.record_event(
                    "data_gap",
                    batch.camera.as_ref().map(|c| c.id.as_str()),
                    None,
                    &text,
                    &Context::new(),
                ) {
                    errors.push(e);
                }
            }
            if let Err(e) = dataset.record_saved(batch.camera.as_ref(), &batch.items) {
                dataset.status.missed_files += items;
                errors.push(e);
            }
            errors.into_iter().next().map_or(Ok(()), Err)
        });
        if let Some(Err(e)) = result {
            eprintln!("capturefab: dataset: {e:#}");
        }
    }
    /// Where the GUI's automatic dataset is created on the first saved file.
    pub fn set_auto_path(&self, path: Option<PathBuf>) {
        *lock(&self.auto_path) = path;
    }
    pub fn auto_path(&self) -> Option<PathBuf> {
        lock(&self.auto_path).clone()
    }
    pub fn status_cached(&self) -> Option<DatasetStatus> {
        self.cached
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    /// The most recently saved files, newest last.
    pub fn recent(&self) -> Vec<SavedFile> {
        lock(&self.recent).iter().cloned().collect()
    }
    /// Add a note or another event; `None` when no dataset is open.
    pub fn record_event(
        &self,
        kind: &str,
        camera_id: Option<&str>,
        source_id: Option<&str>,
        text: &str,
        context: &Context,
    ) -> Result<Option<u64>> {
        self.with_open(false, |d| {
            d.record_event(kind, camera_id, source_id, text, context)
        })
        .transpose()
    }
    /// A cloned connection for one query or export.
    pub fn reader(&self) -> Result<Reader> {
        let writer = lock(&self.writer);
        let dataset = writer.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no dataset is open; run `capturefab dataset open FILE.duckdb`")
        })?;
        dataset.reader_with(Some(&self.readers))
    }
    /// Checkpoint the open dataset, if any.
    pub fn checkpoint(&self) -> Result<()> {
        self.with_open(false, |d| d.checkpoint()).unwrap_or(Ok(()))
    }
}

impl crate::signals::SampleSink for Slot {
    fn register(
        &self,
        source: &crate::signals::SignalInfo,
        tags: &[crate::signals::TagInfo],
    ) -> Result<()> {
        // Signals open the GUI's automatic dataset just as captures do.
        self.with_open(true, |d| d.register(source, tags))
            .unwrap_or(Ok(()))
    }
    fn write(&self, source_id: &str, batch: &[crate::signals::Sample]) -> Result<()> {
        self.with_open(!batch.is_empty(), |d| {
            let written = d.append_samples(source_id, batch);
            if written.is_err() {
                d.status.dropped_samples += batch.len() as u64;
            }
            written.map(drop)
        })
        .unwrap_or(Ok(()))
    }
    fn event(&self, kind: &str, source_id: &str, text: &str) -> Result<()> {
        self.record_event(kind, None, Some(source_id), text, &Context::new())
            .map(drop)
    }
    fn finish(&self) -> Result<()> {
        self.checkpoint()
    }
    fn describe(&self) -> String {
        self.status_cached()
            .map(|s| format!("dataset {}", db::file_label(Path::new(&s.path))))
            .unwrap_or_else(|| "dataset".into())
    }
}

/// Tables an export can write, in order.
pub const TABLES: [&str; 7] = [
    "captures",
    "recordings",
    "signals",
    "samples",
    "events",
    "captures_with_signals",
    "captures_wide",
];
pub const MAX_NOTE_BYTES: usize = 4096;

/// A request the caller must change (exit code 2), raised while it runs.
#[derive(Debug)]
pub struct Usage(pub String);
impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Usage {}

fn check_path(path: &str, what: &str) -> Result<()> {
    if path.is_empty() || path.chars().any(char::is_control) {
        bail!("{what} must be nonempty without control characters");
    }
    Ok(())
}

/// Check the shape of a dataset command's fields.
pub fn validate_command(command: &crate::session::SessionCommand) -> Result<()> {
    use crate::session::SessionCommand as C;
    match command {
        C::DatasetOpen {
            path, max_bytes, ..
        } => {
            check_path(path, "dataset path")?;
            if !Path::new(path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("duckdb"))
            {
                bail!("dataset files use the .duckdb extension");
            }
            ensure!(
                max_bytes.is_none_or(|b| b >= 1024 * 1024),
                "dataset size limit must be at least 1 MiB"
            );
        }
        C::DatasetNote {
            text,
            context,
            camera,
        } => {
            ensure!(
                !text.is_empty()
                    && text.len() <= MAX_NOTE_BYTES
                    && !text
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t'),
                "dataset note must be 1..4096 bytes without control characters"
            );
            crate::context::validate(context)?;
            if let Some(camera) = camera {
                check_path(camera, "note camera")?;
            }
        }
        C::DatasetQuery {
            sql,
            limit,
            timeout_ms,
        } => {
            ensure!(
                !sql.trim().is_empty() && sql.len() <= query::MAX_SQL_BYTES,
                "dataset query must be 1 byte to 64 KiB of SQL"
            );
            ensure!(
                (1..=query::MAX_LIMIT).contains(limit),
                "dataset query limit must be 1..10000"
            );
            ensure!(
                *timeout_ms <= query::MAX_TIMEOUT_MS,
                "dataset query timeout must be at most 300000 milliseconds"
            );
        }
        C::DatasetExport {
            output,
            formats,
            tables,
            since_ms,
            until_ms,
            storage,
        } => {
            check_path(output, "export folder")?;
            ensure!(formats.len() <= 4, "export accepts at most 4 formats");
            for table in tables {
                if !TABLES.contains(&table.as_str()) {
                    bail!(
                        "unknown dataset table {}; choose from {}",
                        table.escape_debug(),
                        TABLES.join(", ")
                    );
                }
            }
            if let (Some(since), Some(until)) = (since_ms, until_ms) {
                ensure!(since <= until, "--since must not be after --until");
            }
            storage.validate()?;
        }
        _ => {}
    }
    Ok(())
}

/// `schema` output: `contract["dataset"]`.
pub fn schema_contract() -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "file": "one .duckdb file per dataset, owned by one session; existing files are never overwritten",
        "time_base": "UTC Unix milliseconds, host clock",
        "tables": {
            "captures": "one row per saved image: wall_ms, camera, frame, format, bytes, path, sha256, destination, object_key, context (JSON text)",
            "recordings": "one row per finished local recording: start_ms, end_ms, camera, path, bytes, sha256, codec",
            "signals": "one row per signal tag: source_id, protocol, uri (redacted), tag, unit, kind",
            "samples": "ts_ms, source_ts_ms, source_id, tag, value_double, value_text, quality",
            "events": "notes, dataset_opened/closed, data_gap, signal_gap, signal_error",
            "captures_with_signals": "view: each capture with the latest sample of every tag at or before its wall_ms (ASOF join)",
            "captures_wide": "export only: one row per capture, one column per tag (1..256 tags)",
        },
        "formats": if cfg!(feature = "parquet") { json!(["csv", "parquet", "json", "jsonl"]) } else { json!(["csv"]) },
        "default_formats": if cfg!(feature = "parquet") { json!(["parquet", "csv"]) } else { json!(["csv"]) },
        "export": "a new folder with data files, manifest.json (schema, units, sha256) and README.md (loading snippets and a summary for AI tools); an existing folder is refused",
        "query": "one SELECT/WITH statement; read-only; external files blocked; rolled back",
        "limits": {
            "query_limit": "1..10000 rows (default 1000)",
            "query_timeout_ms": "0..300000 (0 means 30000)",
            "query_result_bytes": query::MAX_RESULT_BYTES,
            "sql_bytes": query::MAX_SQL_BYTES,
            "note_bytes": MAX_NOTE_BYTES,
            "max_bytes_default": db::DEFAULT_MAX_BYTES,
            "context": {"entries": crate::context::MAX_ENTRIES, "key": "^[a-z][a-z0-9_]{0,31}$", "value_bytes": crate::context::MAX_VALUE},
        },
        "exit_codes": {
            "2": "bad input: wrong extension, unknown table, more than one statement",
            "3": "the dataset is locked by another process; reach it with --session NAME",
            "4": "query timed out",
            "5": "format unsupported in this build",
            "6": "Storage full: dataset size limit, free-space floor or storage budget",
        },
    })
}

/// `schema` output: example RPC commands, each parsed and validated by tests.
pub fn rpc_examples() -> Vec<Value> {
    vec![
        json!({"op":"dataset_open","path":"/data/plant.duckdb"}),
        json!({"op":"dataset_close"}),
        json!({"op":"dataset_status"}),
        json!({"op":"dataset_note","text":"shift B start","context":{"line":"L1"}}),
        json!({"op":"dataset_query","sql":"SELECT count(*) FROM captures"}),
        json!({"op":"dataset_export","output":"/data/export-1","formats":["csv"]}),
    ]
}

#[cfg(test)]
mod seam_tests {
    use super::*;

    fn capture(path: &str) -> SavedFile {
        SavedFile {
            seq: 0,
            wall_ms: 1,
            path: path.into(),
            bytes: 2,
            sha256: None,
            destination: None,
            object_key: None,
            remote_url: None,
            upload_id: None,
            context: Context::new(),
            item: SavedItem::Capture {
                frame_id: 3,
                timestamp_ns: 4,
                width: 5,
                height: 6,
                pixel_format: 7,
                format: "png".into(),
                origin: "capture".into(),
                job_id: None,
            },
        }
    }

    #[test]
    fn saved_files_round_trip_and_log_counts_overflow() {
        let file = capture("a.png");
        let text = serde_json::to_string(&file).unwrap();
        assert!(text.contains("\"kind\":\"capture\""), "{text}");
        assert_eq!(serde_json::from_str::<SavedFile>(&text).unwrap(), file);
        assert!(SavedBatch::default().is_empty());
        assert_eq!(serde_json::to_string(&SavedBatch::default()).unwrap(), "{}");
        let mut log = SavedLog::default();
        for _ in 0..SAVED_LOG_CAP + 2 {
            log.push(capture("b.png"));
        }
        let (items, lost) = log.take(3);
        assert_eq!((items.len(), lost, items[0].seq), (3, 2, 3));
        assert_eq!(log.take(usize::MAX).1, 0);
    }
}
