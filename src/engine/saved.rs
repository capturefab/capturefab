//! Files a camera worker saved: captures and finished recordings, with the
//! camera that produced them, in a bounded log each reply drains.
use crate::{context::Context, types::CameraInfo};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Saved-file entries a worker keeps until its replies drain them.
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
}
