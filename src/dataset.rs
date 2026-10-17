//! Dataset catalog: one `.duckdb` file per dataset, owned by the session
//! coordinator (never a camera worker), recording every saved capture and
//! recording, signal samples and notes, with exports for later analysis.
pub mod coordinator;

use crate::{context::Context, types::CameraInfo};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
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
/// shared with the signal hub (as its sample sink) and the GUI.
#[derive(Debug, Default)]
pub struct Slot {}

impl Slot {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn open(
        &self,
        _path: &Path,
        _mode: OpenMode,
        _max_bytes: Option<u64>,
    ) -> Result<DatasetStatus> {
        bail!("datasets are not implemented yet")
    }
    pub fn close(&self) -> Result<Option<DatasetStatus>> {
        Ok(None)
    }
    /// Close for shutdown; errors are logged, never returned.
    pub fn close_quiet(&self) {}
    /// Catalog files a worker saved. Never fails; problems are counted.
    pub fn record_saved(&self, _batch: SavedBatch, _gaps: u64) {}
    /// Where the GUI's automatic dataset is created on the first saved file.
    pub fn set_auto_path(&self, _path: Option<PathBuf>) {}
    pub fn status_cached(&self) -> Option<DatasetStatus> {
        None
    }
    /// The most recently saved files, newest last.
    pub fn recent(&self) -> Vec<SavedFile> {
        Vec::new()
    }
}

impl crate::signals::SampleSink for Slot {
    fn register(
        &self,
        _source: &crate::signals::SignalInfo,
        _tags: &[crate::signals::TagInfo],
    ) -> Result<()> {
        Ok(())
    }
    fn write(&self, _source_id: &str, _batch: &[crate::signals::Sample]) -> Result<()> {
        Ok(())
    }
    fn finish(&self) -> Result<()> {
        Ok(())
    }
    fn describe(&self) -> String {
        "dataset".into()
    }
}

/// Check the shape of a dataset command's fields.
pub fn validate_command(_command: &crate::session::SessionCommand) -> Result<()> {
    Ok(())
}

/// `schema` output: `contract["dataset"]`.
pub fn schema_contract() -> Value {
    json!({"schema_version": SCHEMA_VERSION})
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
mod tests {
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
