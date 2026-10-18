//! Multi-camera coordinator: one isolated process per camera, shared-memory pixels.
mod activity;
mod log;
use crate::{
    genicam::FeatureInfo,
    shared_memory::{DEFAULT_CAPACITY, RingCounters, SharedRing},
    types::{CameraInfo, Frame},
};
pub use activity::{
    ActivityJob, ActivityOrigin, ActivityState, ActivityView, ActorKind, SessionActivity,
    SessionActor,
};
use anyhow::{Context, Result, ensure};
pub use log::{LogEntry, LogTimestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock, PoisonError, RwLock, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionCommand {
    Discover {
        timeout_ms: u64,
        simulated: bool,
    },
    /// Inspect all GigE paths and optimize the connected cameras together.
    NetworkScan {
        timeout_ms: u64,
    },
    /// Per-camera worker operation used by the network scan coordinator.
    OptimizeNetwork {
        bytes_per_second: u64,
    },
    Connect {
        camera: String,
        timeout_ms: u64,
    },
    Select {
        camera: String,
    },
    Disconnect,
    Get {
        feature: String,
    },
    Set {
        feature: String,
        value: String,
    },
    Execute {
        feature: String,
    },
    Start,
    Stop,
    /// Revert the last successful reversible change on the addressed camera.
    Undo,
    /// Reapply the last reverted change on the addressed camera.
    Redo,
    /// Replay a specific acknowledged checkpoint, used by the GUI's shared
    /// timeline. Tokens are tied to a worker lifetime and cannot target a
    /// replacement connection accidentally.
    ReplayCameraChange {
        token: String,
        undo: bool,
    },
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        balance: Option<f64>,
    },
    Manual {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        revert: bool,
    },
    /// Create or resume a single-file, camera-bound capture database.
    CaptureSessionOpen {
        path: String,
        create: bool,
        /// New sessions only: the image encoder (default auto).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoder: Option<crate::autocapture::EncoderChoice>,
    },
    CaptureSessionClose,
    /// Capture preference learning is independent of automatic exposure.
    AutoCapture {
        enabled: bool,
        sensitivity: f64,
        cooldown_ms: u64,
    },
    /// Save one positive training capture directly into the active database.
    CaptureExample {
        timeout_ms: u64,
    },
    /// Fetch the next pending candidate, or a specific pending candidate.
    CaptureReview {
        id: Option<u64>,
    },
    ReviewCapture {
        id: u64,
        keep: bool,
    },
    Capture {
        output: String,
        count: u32,
        timeout_ms: u64,
        format: String,
        #[serde(default)]
        storage: crate::storage::StoragePolicy,
        /// A saved destination (folder or S3 bucket); `output` is then a name inside it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
    },
    Schedule {
        output: String,
        count: u32,
        timeout_ms: u64,
        format: String,
        first_at_ms: u64,
        interval_ms: u64,
        #[serde(default)]
        storage: crate::storage::StoragePolicy,
        /// A saved destination (folder or S3 bucket); `output` is then a name inside it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
    },
    Jobs,
    CancelJob {
        id: u64,
    },
    Status,
    /// Observe session-wide participant activity, optionally waiting for a change.
    Activity {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<u64>,
        #[serde(default)]
        timeout_ms: u64,
    },
    Features,
    Xml,
    ReadMemory {
        address: u64,
        length: usize,
    },
    WriteMemory {
        address: u64,
        data: Vec<u8>,
    },
    Forward {
        output: String,
        codec: String,
        encoder: String,
        fps: f64,
        bitrate: String,
        #[serde(default)]
        storage: crate::storage::StoragePolicy,
        #[serde(default = "crate::media::default_recording_cap")]
        max_file_bytes: u64,
        /// A saved destination (folder or S3 bucket); `output` is then a name inside it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
    },
    StopForward,
    /// Open or create the session's dataset catalog (one .duckdb file).
    DatasetOpen {
        path: String,
        #[serde(default)]
        mode: crate::dataset::OpenMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
    },
    DatasetClose,
    DatasetStatus,
    /// Add a timestamped note to the open dataset.
    DatasetNote {
        text: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        camera: Option<String>,
    },
    /// One read-only SELECT on the open dataset.
    DatasetQuery {
        sql: String,
        #[serde(default = "crate::dataset::default_query_limit")]
        limit: u32,
        #[serde(default)]
        timeout_ms: u64,
    },
    /// Write data files, manifest.json and README.md into a new folder.
    DatasetExport {
        output: String,
        #[serde(default)]
        formats: Vec<crate::dataset::ExportFormat>,
        #[serde(default)]
        tables: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until_ms: Option<u64>,
        #[serde(default)]
        storage: crate::storage::StoragePolicy,
    },
    /// One-shot read of a signal source's current values.
    SignalRead {
        uri: String,
        #[serde(default = "crate::signals::default_timeout_ms")]
        timeout_ms: u64,
    },
    /// List OPC UA nodes, MQTT topics or MTConnect data items for a bounded time.
    SignalBrowse {
        uri: String,
        #[serde(default = "crate::signals::default_browse_ms")]
        duration_ms: u64,
        #[serde(default = "crate::signals::default_browse_limit")]
        limit: u32,
    },
    /// Start logging a signal source in this session.
    SignalStart {
        uri: String,
        #[serde(default = "crate::signals::default_interval_ms")]
        interval_ms: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
    },
    /// Stop a signal source by `signal:<16 hex>` ID, or `all`.
    SignalStop {
        id: String,
    },
    /// Signal source status.
    Signals,
}
/// Deserialize JSON control messages without accepting ignored fields on
/// Serde's internally tagged unit variants or duplicate object fields.
pub fn deserialize_command<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<SessionCommand, D::Error> {
    let StrictValue(value) = StrictValue::deserialize(deserializer)?;
    if let Value::Object(fields) = &value
        && fields.get("op").and_then(Value::as_str).is_some_and(|op| {
            matches!(
                op,
                "disconnect"
                    | "start"
                    | "stop"
                    | "undo"
                    | "redo"
                    | "jobs"
                    | "status"
                    | "features"
                    | "xml"
                    | "stop_forward"
                    | "capture_session_close"
                    | "dataset_close"
                    | "dataset_status"
                    | "signals"
            )
        })
        && let Some(field) = fields.keys().find(|key| key.as_str() != "op")
    {
        return Err(serde::de::Error::custom(format!(
            "unknown field `{field}`; this operation only accepts `op`"
        )));
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictValue;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                value: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(value)))
            }
            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(value.into())))
            }
            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(value.into())))
            }
            fn visit_f64<E: serde::de::Error>(
                self,
                value: f64,
            ) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| StrictValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("JSON numbers must be finite"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(value.into())))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                value: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(value)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut values: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut result = Vec::new();
                while let Some(StrictValue(value)) = values.next_element()? {
                    result.push(value);
                }
                Ok(StrictValue(Value::Array(result)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut values: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut result = serde_json::Map::new();
                while let Some(key) = values.next_key::<String>()? {
                    if result.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                    }
                    let StrictValue(value) = values.next_value()?;
                    result.insert(key, value);
                }
                Ok(StrictValue(Value::Object(result)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
impl SessionCommand {
    pub fn from_json_slice(data: &[u8]) -> Result<Self> {
        let mut deserializer = serde_json::Deserializer::from_slice(data);
        let command = deserialize_command(&mut deserializer)?;
        deserializer.end()?;
        Ok(command)
    }
    /// Validate supplied values before queueing a command or changing a camera.
    /// Live feature bounds and destination availability are checked again by
    /// their owners when the command executes.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CaptureSessionOpen {
                path,
                create,
                encoder,
            } => {
                ensure!(
                    !path.trim().is_empty()
                        && !path.chars().any(char::is_control)
                        && path != ":memory:",
                    "capture session requires a database file path"
                );
                ensure!(
                    *create || encoder.is_none(),
                    "the encoder is chosen when a capture session is created; resume uses the session's own"
                );
                ensure!(
                    std::path::Path::new(path)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("duckdb")),
                    "capture session file must have a .duckdb extension"
                );
            }
            Self::AutoCapture {
                sensitivity,
                cooldown_ms,
                ..
            } => {
                ensure!(
                    sensitivity.is_finite() && (0.0..=1.0).contains(sensitivity),
                    "sensitivity must be a finite number from 0 to 1"
                );
                ensure!(
                    (250..=3_600_000).contains(cooldown_ms),
                    "cooldown must be 250..3600000 milliseconds"
                );
            }
            Self::CaptureExample { timeout_ms } => validate_timeout(*timeout_ms)?,
            Self::CaptureReview { id: Some(id) } | Self::ReviewCapture { id, .. } => {
                ensure!(
                    *id > 0 && *id <= i64::MAX as u64,
                    "capture ID must be 1..9223372036854775807"
                );
            }
            Self::Activity { timeout_ms, .. } => {
                ensure!(
                    *timeout_ms <= 25_000,
                    "activity timeout must be 0..25000 milliseconds"
                );
            }
            Self::Discover { timeout_ms, .. } | Self::NetworkScan { timeout_ms } => {
                validate_timeout(*timeout_ms)?;
            }
            Self::OptimizeNetwork { bytes_per_second } => {
                ensure!(
                    (1..=125_000_000_000).contains(bytes_per_second),
                    "network budget must be 1..125000000000 bytes per second"
                );
            }
            Self::Connect { camera, timeout_ms } => {
                validate_selector(camera)?;
                validate_timeout(*timeout_ms)?;
                maximum_cameras()?;
                frame_capacity(false)?;
            }
            Self::Select { camera } => validate_selector(camera)?,
            Self::Get { feature } | Self::Execute { feature } | Self::Set { feature, .. } => {
                ensure!(
                    !feature.is_empty()
                        && !feature
                            .chars()
                            .any(|c| c.is_whitespace() || c.is_control() || c == '='),
                    "feature name must be nonempty and contain no whitespace, control characters or '='"
                );
            }
            Self::ReplayCameraChange { token, .. } => {
                ensure!(
                    !token.is_empty() && !token.chars().any(char::is_control),
                    "history token must be nonempty and contain no control characters"
                );
            }
            Self::Auto {
                balance: Some(balance),
            } => {
                ensure!(
                    balance.is_finite() && (0.0..=1.0).contains(balance),
                    "auto balance must be a finite number from 0 (quality) to 1 (frame rate)"
                );
            }
            Self::Capture {
                output,
                count,
                timeout_ms,
                format,
                storage,
                destination,
                on_exists,
                context,
            }
            | Self::Schedule {
                output,
                count,
                timeout_ms,
                format,
                storage,
                destination,
                on_exists,
                context,
                ..
            } => {
                validate_timeout(*timeout_ms)?;
                ensure!((1..=100_000).contains(count), "count must be 1..100000");
                ensure!(
                    ["png", "jpeg", "raw", "pgm", "ppm"].contains(&format.as_str()),
                    "format must be png, jpeg, raw, pgm or ppm"
                );
                storage.validate()?;
                crate::destination::validate_output(destination.as_deref(), output)?;
                crate::engine::validate_capture_output(output)?;
                crate::naming::validate_request(output, *count, *on_exists, context)?;
                if matches!(self, Self::Schedule { .. }) {
                    ensure!(
                        output != "-",
                        "scheduled capture requires a file or directory"
                    );
                } else {
                    ensure!(
                        output != "-" || (*count == 1 && destination.is_none()),
                        "stdout output needs one frame and cannot use a destination"
                    );
                }
                if let Self::Schedule {
                    first_at_ms,
                    interval_ms,
                    ..
                } = self
                {
                    crate::scheduling::validate_schedule(*first_at_ms, *interval_ms, *count)?;
                }
            }
            Self::CancelJob { id } => ensure!(*id > 0, "capture job ID must be positive"),
            Self::ReadMemory { address, length } => validate_memory(*address, *length)?,
            Self::WriteMemory { address, data } => validate_memory(*address, data.len())?,
            Self::Forward {
                output,
                codec,
                encoder,
                fps,
                bitrate,
                storage,
                max_file_bytes,
                destination,
                on_exists,
                context,
            } => {
                crate::naming::validate_request(output, 1, *on_exists, context)?;
                crate::media::ForwardConfig {
                    output: output.clone(),
                    codec: codec.clone(),
                    encoder: encoder.clone(),
                    fps: *fps,
                    bitrate: bitrate.clone(),
                    storage: storage.clone(),
                    max_file_bytes: *max_file_bytes,
                    destination: destination.clone(),
                }
                .validate()?;
            }
            Self::DatasetOpen { .. }
            | Self::DatasetClose
            | Self::DatasetStatus
            | Self::DatasetNote { .. }
            | Self::DatasetQuery { .. }
            | Self::DatasetExport { .. } => crate::dataset::validate_command(self)?,
            Self::SignalRead { .. }
            | Self::SignalBrowse { .. }
            | Self::SignalStart { .. }
            | Self::SignalStop { .. }
            | Self::Signals => crate::signals::validate_command(self)?,
            _ => {}
        }
        Ok(())
    }
    /// Commands the coordinator answers itself, without a camera worker.
    pub fn is_dataset(&self) -> bool {
        matches!(
            self,
            Self::DatasetOpen { .. }
                | Self::DatasetClose
                | Self::DatasetStatus
                | Self::DatasetNote { .. }
                | Self::DatasetQuery { .. }
                | Self::DatasetExport { .. }
        )
    }
    pub fn is_signal(&self) -> bool {
        matches!(
            self,
            Self::SignalRead { .. }
                | Self::SignalBrowse { .. }
                | Self::SignalStart { .. }
                | Self::SignalStop { .. }
                | Self::Signals
        )
    }
}
pub fn validate_timeout(timeout_ms: u64) -> Result<()> {
    ensure!(
        (1..=60_000).contains(&timeout_ms),
        "timeout must be 1..60000 milliseconds"
    );
    Ok(())
}
pub fn validate_selector(camera: &str) -> Result<()> {
    crate::camera::validate_selector_shape(camera)
}
fn validate_memory(address: u64, length: usize) -> Result<()> {
    ensure!(
        (1..=65_536).contains(&length),
        "memory length must be 1..65536"
    );
    ensure!(
        address.checked_add((length - 1) as u64).is_some(),
        "memory address range overflows"
    );
    Ok(())
}
fn env_usize(name: &str, default: usize) -> Result<usize> {
    match std::env::var_os(name) {
        None => Ok(default),
        Some(value) => value
            .to_str()
            .with_context(|| format!("{name} must be a positive integer"))?
            .parse::<usize>()
            .with_context(|| format!("{name} must be a positive integer")),
    }
}
fn maximum_cameras() -> Result<usize> {
    let maximum = env_usize("CAPTUREFAB_MAX_CAMERAS", 16)?;
    ensure!(
        (1..=64).contains(&maximum),
        "CAPTUREFAB_MAX_CAMERAS must be 1..64"
    );
    Ok(maximum)
}
fn frame_capacity(scanner: bool) -> Result<usize> {
    let capacity = env_usize(
        "CAPTUREFAB_FRAME_BYTES",
        if scanner {
            128 * 1024 * 1024
        } else {
            DEFAULT_CAPACITY
        },
    )?;
    ensure!(
        (4096..=256 * 1024 * 1024).contains(&capacity) && capacity.is_multiple_of(64),
        "CAPTUREFAB_FRAME_BYTES must be 4096..268435456, aligned to 64 bytes"
    );
    Ok(capacity)
}
/// Lines the session log keeps.
const LOG_LINES: usize = 200;
/// Session log sources besides camera workers, which count up from `WORKERS`.
const COORDINATOR: u64 = 0;
const SIGNALS: u64 = 1;
const WORKERS: u64 = 2;
/// The session's newest log lines from camera workers, the coordinator and
/// signal sources, in time order. Each source's lines are taken once and in
/// order, so trimming never brings a line back or reshuffles the rest.
#[derive(Default)]
struct SessionLog {
    lines: BTreeMap<(i64, u64, u64), LogEntry>,
    /// The next line number to take from each source.
    taken: BTreeMap<u64, u64>,
    coordinator: u64,
    /// The signal hub's lines last seen, to tell its new ones.
    signals: Vec<LogEntry>,
    revision: u64,
}
impl SessionLog {
    fn add(&mut self, source: u64, seq: u64, entry: LogEntry) {
        // Lines without a timestamp come from old producers; place them on arrival.
        let time = entry.timestamp.map_or_else(
            || chrono::Utc::now().timestamp_millis(),
            LogTimestamp::unix_ms,
        );
        // One event reported by two producers is kept once.
        if self
            .lines
            .range((time, 0, 0)..=(time, u64::MAX, u64::MAX))
            .any(|((_, from, _), line)| *from != source && line.same_event(&entry))
        {
            return;
        }
        let key = (time, source, seq);
        self.lines.insert(key, entry);
        if self.lines.len() > LOG_LINES && self.lines.pop_first().is_some_and(|(old, _)| old == key)
        {
            return;
        }
        self.revision += 1;
    }
    fn note(&mut self, entry: LogEntry) {
        self.coordinator += 1;
        self.add(COORDINATOR, self.coordinator, entry);
    }
    /// A worker's lines this log has not taken yet.
    fn take(&mut self, worker: &WorkerLog) {
        let next = self.taken.entry(worker.source).or_default();
        let from = std::mem::replace(next, worker.next);
        let start = worker.tagged.partition_point(|(seq, _)| *seq < from);
        for (seq, line) in worker.tagged.range(start..) {
            self.add(worker.source, *seq, line.clone());
        }
    }
    fn forget(&mut self, source: u64) {
        self.taken.remove(&source);
    }
    fn take_signals(&mut self, now: Vec<LogEntry>) {
        let kept = kept_lines(&self.signals, &now);
        for line in &now[kept..] {
            self.coordinator += 1;
            self.add(SIGNALS, self.coordinator, line.clone());
        }
        self.signals = now;
    }
    fn lines(&self) -> Vec<LogEntry> {
        self.lines.values().cloned().collect()
    }
}
/// How many of the lines `sent` ended with still start `now`. A log keeps
/// its newest lines, so new ones push old ones off the front.
fn kept_lines(sent: &[LogEntry], now: &[LogEntry]) -> usize {
    (0..=sent.len().min(now.len()))
        .rev()
        .find(|&n| now[..n] == sent[sent.len() - n..])
        .unwrap_or(0)
}
/// A worker's log lines as the session log shows them: tagged with the
/// camera, redacted and numbered once, when they arrive.
struct WorkerLog {
    /// Tells this worker's lines apart in the session log.
    source: u64,
    camera: Option<String>,
    /// The number the next line gets.
    next: u64,
    /// The worker's own lines, to follow its window of newest lines.
    raw: Vec<LogEntry>,
    tagged: VecDeque<(u64, LogEntry)>,
}
impl WorkerLog {
    fn new() -> Self {
        static SOURCES: AtomicU64 = AtomicU64::new(WORKERS);
        Self {
            source: SOURCES.fetch_add(1, Ordering::Relaxed),
            camera: None,
            next: 0,
            raw: Vec::new(),
            tagged: VecDeque::new(),
        }
    }
    /// Follow the worker's window: it kept `kept` of the lines it last sent
    /// and added `lines` after them.
    fn update(&mut self, kept: usize, lines: Vec<LogEntry>) {
        let dropped = self.raw.len() - kept;
        self.raw.drain(..dropped);
        self.tagged.drain(..dropped);
        for line in lines {
            let tagged = match &self.camera {
                Some(camera) => camera_log(&line, camera),
                None => LogEntry {
                    message: activity::redact(&line.message),
                    ..line.clone()
                },
            };
            self.tagged.push_back((self.next, tagged));
            self.next += 1;
            self.raw.push(line);
        }
    }
    /// The time of the newest error line.
    fn newest_error(&self) -> Option<String> {
        self.tagged
            .iter()
            .rev()
            .find(|(_, entry)| entry.level == "error")
            .map(|(_, entry)| entry.time.clone())
    }
}
/// Frames a worker lost on the way, never frames skipped for a preview.
fn frames_lost(worker: &SessionSnapshot) -> u64 {
    let transport = worker
        .transport
        .as_ref()
        .map_or(0, |t| t.incomplete_frames + t.lost_frames);
    worker.dropped.saturating_add(transport)
}
fn camera_log(entry: &LogEntry, camera: &str) -> LogEntry {
    LogEntry {
        message: activity::redact(&format!("[{camera}] {}", entry.message)),
        ..entry.clone()
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraHistoryStatus {
    pub undo: Option<String>,
    pub redo: Option<String>,
    #[serde(default)]
    pub entries: Vec<CameraHistoryEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraHistoryEntry {
    pub token: String,
    pub label: String,
    pub applied: bool,
    pub created_at_ms: u64,
    #[serde(default)]
    pub last_changed_at_us: u64,
    pub feature: Option<String>,
    pub kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraSnapshot {
    pub info: CameraInfo,
    /// Shared with the coordinator's copy, so a snapshot never copies them.
    pub features: Arc<[FeatureInfo]>,
    pub streaming: bool,
    pub frames: u64,
    /// Payload bytes acquired by this worker, before preview downsampling or encoding.
    #[serde(default)]
    pub received_bytes: u64,
    pub dropped: u64,
    pub fps: f64,
    pub last_error: Option<String>,
    pub worker_pid: u32,
    #[serde(default)]
    pub forwarding: Option<String>,
    /// When the current recording or forward started, UTC Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_since_ms: Option<u64>,
    #[serde(default)]
    pub jobs: Vec<crate::scheduling::CaptureJob>,
    #[serde(default)]
    pub auto: Option<crate::auto::AutoStatus>,
    #[serde(default)]
    pub capture_session: Option<crate::autocapture::SessionStatus>,
    #[serde(default)]
    pub transport: Option<crate::types::TransportStats>,
    #[serde(default)]
    pub history: CameraHistoryStatus,
    /// Frames lost on the way: the worker's receive errors plus the
    /// transport's incomplete and missing frames.
    #[serde(default)]
    pub lost: u64,
    /// Preview frames the shared ring replaced while a viewer was reading,
    /// because a newer one came first; `dropped` includes them. Captures and
    /// recordings still receive those frames.
    #[serde(default)]
    pub preview_skipped: u64,
    /// The worker was busy, so this entry repeats its last report. Kept for
    /// this process's own views; not serialized.
    #[serde(skip)]
    pub stale: bool,
    /// The time of the worker's newest error line, the one `last_error`
    /// holds, so a repeat of the same error can be told from the one before.
    /// Kept for this process's own views; not serialized.
    #[serde(skip)]
    pub error_logged: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub devices: Vec<CameraInfo>,
    #[serde(default)]
    pub network_scan: Option<crate::network::NetworkScan>,
    pub connected: Option<CameraInfo>,
    pub features: Vec<FeatureInfo>,
    pub streaming: bool,
    pub frames: u64,
    #[serde(default)]
    pub received_bytes: u64,
    pub dropped: u64,
    /// The selected camera's `lost` and `preview_skipped`.
    #[serde(default)]
    pub lost: u64,
    #[serde(default)]
    pub preview_skipped: u64,
    pub fps: f64,
    pub last_error: Option<String>,
    pub logs: Vec<LogEntry>,
    /// Session-wide command history, including queued and running client work.
    /// Status polling is omitted; completed entries are bounded to 200.
    #[serde(default)]
    pub activity: Vec<SessionActivity>,
    #[serde(default)]
    pub activity_revision: u64,
    #[serde(default)]
    pub cameras: Vec<CameraSnapshot>,
    #[serde(default)]
    pub active_camera: Option<String>,
    #[serde(default)]
    pub forwarding: Option<String>,
    /// When the current recording or forward started, UTC Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_since_ms: Option<u64>,
    #[serde(default)]
    pub jobs: Vec<crate::scheduling::CaptureJob>,
    #[serde(default)]
    pub auto: Option<crate::auto::AutoStatus>,
    #[serde(default)]
    pub capture_session: Option<crate::autocapture::SessionStatus>,
    #[serde(default)]
    pub transport: Option<crate::types::TransportStats>,
    #[serde(default)]
    pub history: CameraHistoryStatus,
    /// The session's open dataset catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<crate::dataset::DatasetStatus>,
    /// Signal sources logging in this session (at most 32 tags each).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<crate::signals::SourceStatus>,
}
impl SessionSnapshot {
    /// Project one participant's local view without changing shared selection.
    pub fn select_view(&mut self, camera: Option<&str>) {
        self.active_camera = camera.map(str::to_owned);
        project_selected(self);
    }
}
/// Always project the selected view from that camera's own cached report.
/// A busy selected worker must never inherit a different camera's settings.
fn project_selected(state: &mut SessionSnapshot) {
    if let Some(camera) = state
        .cameras
        .iter()
        .find(|camera| state.active_camera.as_deref() == Some(&camera.info.id))
    {
        state.connected = Some(camera.info.clone());
        state.features = camera.features.to_vec();
        state.streaming = camera.streaming;
        state.frames = camera.frames;
        state.received_bytes = camera.received_bytes;
        state.dropped = camera.dropped;
        state.lost = camera.lost;
        state.preview_skipped = camera.preview_skipped;
        state.fps = camera.fps;
        state.forwarding = camera.forwarding.clone();
        state.forwarding_since_ms = camera.forwarding_since_ms;
        state.jobs = camera.jobs.clone();
        state.auto = camera.auto.clone();
        state.capture_session = camera.capture_session.clone();
        state.transport = camera.transport.clone();
        state.history = camera.history.clone();
        state.last_error = camera.last_error.clone();
    } else {
        state.connected = None;
        state.features.clear();
        state.streaming = false;
        state.frames = 0;
        state.received_bytes = 0;
        state.dropped = 0;
        state.lost = 0;
        state.preview_skipped = 0;
        state.fps = 0.0;
        state.forwarding = None;
        state.forwarding_since_ms = None;
        state.jobs.clear();
        state.auto = None;
        state.capture_session = None;
        state.transport = None;
        state.history = CameraHistoryStatus::default();
    }
}
fn merge_session_metadata(snapshot: &mut SessionSnapshot, current: &SessionSnapshot) {
    // Polling does worker I/O outside the state lock. Preserve session edits
    // that completed while that poll was in flight.
    snapshot.active_camera = current.active_camera.clone();
    snapshot.devices = current.devices.clone();
    snapshot.network_scan = current.network_scan.clone();
    project_selected(snapshot);
}
/// A camera-scoped status keeps that camera's feature/capture fields while
/// including the same session-wide context as an unaddressed status request.
fn addressed_status(
    mut camera_state: Value,
    snapshot: &SessionSnapshot,
    target_camera: &str,
) -> Result<Value> {
    let fields = camera_state
        .as_object_mut()
        .context("worker status is not an object")?;
    fields.insert("target_camera".into(), json!(target_camera));
    fields.insert("active_camera".into(), json!(snapshot.active_camera));
    fields.insert("cameras".into(), serde_json::to_value(&snapshot.cameras)?);
    fields.insert("devices".into(), serde_json::to_value(&snapshot.devices)?);
    fields.insert(
        "network_scan".into(),
        serde_json::to_value(&snapshot.network_scan)?,
    );
    fields.insert("activity".into(), serde_json::to_value(&snapshot.activity)?);
    fields.insert(
        "activity_revision".into(),
        json!(snapshot.activity_revision),
    );
    fields.insert("logs".into(), serde_json::to_value(&snapshot.logs)?);
    fields.insert("dataset".into(), serde_json::to_value(&snapshot.dataset)?);
    if !snapshot.signals.is_empty() {
        fields.insert("signals".into(), serde_json::to_value(&snapshot.signals)?);
    }
    Ok(camera_state)
}
type Reply = Receiver<Result<Value>>;
/// A camera's frames: the newest-frame cache behind its own lock, and the
/// ring's counters beside it, which sequence polls read without that lock.
struct Feed {
    counters: RingCounters,
    frames: Mutex<FrameFeed>,
    /// The worker process writing the ring; frame ids start over in a new one.
    worker: u32,
}
impl Feed {
    fn new(ring: SharedRing, worker: u32) -> Self {
        Self {
            counters: ring.counters(),
            frames: Mutex::new(FrameFeed { ring, latest: None }),
            worker,
        }
    }
}
/// Newest-frame cache fed by a worker's shared ring. It has its own lock so
/// previews never wait behind a slow command on the worker's stdio pipe.
/// Lock order: a `Process` lock may be held while taking a feed lock, never
/// the reverse.
struct FrameFeed {
    ring: SharedRing,
    latest: Option<Arc<Frame>>,
}
impl FrameFeed {
    /// The newest frame. With `watch` this counts as the ring's reader
    /// polling, which keeps the worker publishing every frame.
    fn poll(&mut self, watch: bool) -> Option<Arc<Frame>> {
        // Overwrite the cached frame in place when no consumer still holds it,
        // so steady-state streaming reuses one pixel allocation.
        let updated = match self.latest.as_mut().and_then(Arc::get_mut) {
            Some(frame) => self.ring.take_into(frame, watch),
            None => {
                let mut frame = Frame::default();
                let updated = self.ring.take_into(&mut frame, watch);
                if matches!(updated, Ok(true)) {
                    self.latest = Some(Arc::new(frame));
                }
                updated
            }
        };
        if let Err(e) = updated {
            eprintln!("capturefab: ignoring unreadable shared frame: {e:#}");
        }
        self.latest.clone()
    }
}
fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
#[derive(Serialize)]
struct WorkerReply<'a> {
    ok: bool,
    /// Omitted for Status, whose result is the snapshot itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    snapshot: &'a SessionSnapshot,
    #[serde(skip_serializing_if = "Kept::is_nothing")]
    kept: Kept,
    /// Files saved since the previous reply, for the coordinator's dataset.
    #[serde(skip_serializing_if = "crate::dataset::SavedBatch::is_empty")]
    saved: crate::dataset::SavedBatch,
}
/// A worker reply as the coordinator reads it. `S` is the snapshot, or
/// `IgnoredAny` to keep the rest of a reply whose snapshot is unreadable.
#[derive(Deserialize)]
struct ParentReply<S = SessionSnapshot> {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    snapshot: Option<S>,
    #[serde(default)]
    kept: Kept,
    #[serde(default)]
    saved: crate::dataset::SavedBatch,
}
/// What a reply leaves out because the previous reply on the same pipe
/// already carried it: features, camera history, and the log lines the
/// previous reply ended with that still start the worker's log. The reply's
/// `logs` then holds only the lines after those.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Kept {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    features: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    history: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    logs: usize,
}
impl Kept {
    fn is_nothing(&self) -> bool {
        *self == Self::default()
    }
}
fn is_zero(value: &usize) -> bool {
    *value == 0
}
/// What a worker last sent. Only its coordinator reads the pipe, so a reply
/// can leave out what has not changed since.
#[derive(Default)]
struct Sent {
    features: Vec<FeatureInfo>,
    history: CameraHistoryStatus,
    logs: Vec<LogEntry>,
}
impl Sent {
    /// Take out of `snapshot` what the coordinator already holds.
    fn strip(&mut self, snapshot: &mut SessionSnapshot) -> Kept {
        let kept = Kept {
            features: snapshot.features == self.features,
            history: snapshot.history == self.history,
            logs: kept_lines(&self.logs, &snapshot.logs),
        };
        if kept.features {
            snapshot.features = Vec::new();
        } else {
            self.features = snapshot.features.clone();
        }
        if kept.history {
            snapshot.history = CameraHistoryStatus::default();
        } else {
            self.history = snapshot.history.clone();
        }
        let added = snapshot.logs[kept.logs..].to_vec();
        self.logs = std::mem::replace(&mut snapshot.logs, added);
        kept
    }
}
/// A status request only a coordinator sends its own worker. It carries an
/// efficiency setting that changed since the worker started, for FFmpeg
/// decoders it launches later; `full` asks for every field again.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    efficiency: Option<crate::efficiency::Mode>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    full: bool,
}
/// What the coordinator knows of a worker, merged from its replies.
/// Features and log lines are kept apart, ready for snapshots.
struct Report {
    snapshot: SessionSnapshot,
    features: Arc<[FeatureInfo]>,
    log: WorkerLog,
    /// A reply could not be read, so ask for every field again.
    resync: bool,
}
impl Report {
    fn new() -> Self {
        Self {
            snapshot: SessionSnapshot::default(),
            features: Arc::default(),
            log: WorkerLog::new(),
            resync: false,
        }
    }
    /// Merge a reply's snapshot: what it left out stays as it was. After a
    /// `full` request the worker resends everything, including log lines
    /// this side already has.
    fn apply(&mut self, mut next: SessionSnapshot, mut kept: Kept, full: bool) {
        if full {
            self.features = Arc::default();
            self.snapshot.history = CameraHistoryStatus::default();
            kept.logs = kept_lines(&self.log.raw, &next.logs);
            next.logs.drain(..kept.logs);
        }
        if !kept.features {
            self.features = std::mem::take(&mut next.features).into();
        }
        if kept.history {
            next.history = std::mem::take(&mut self.snapshot.history);
        }
        if kept.logs > self.log.raw.len() {
            self.resync = true;
            kept.logs = self.log.raw.len();
        }
        if let Some(info) = &next.connected {
            self.log.camera = Some(info.id.clone());
        }
        self.log.update(kept.logs, std::mem::take(&mut next.logs));
        self.snapshot = next;
    }
    fn camera(&self, worker_pid: u32, preview_skipped: u64) -> Option<CameraSnapshot> {
        let s = &self.snapshot;
        Some(CameraSnapshot {
            info: s.connected.clone()?,
            features: self.features.clone(),
            streaming: s.streaming,
            frames: s.frames,
            received_bytes: s.received_bytes,
            dropped: s.dropped + preview_skipped,
            fps: s.fps,
            last_error: s.last_error.clone(),
            worker_pid,
            forwarding: s.forwarding.clone(),
            forwarding_since_ms: s.forwarding_since_ms,
            jobs: s.jobs.clone(),
            auto: s.auto.clone(),
            capture_session: s.capture_session.clone(),
            transport: s.transport.clone(),
            history: s.history.clone(),
            lost: frames_lost(s),
            preview_skipped,
            stale: false,
            error_logged: self.log.newest_error(),
        })
    }
}
/// Saved files a worker reported more than this many of are counted as lost.
const SAVED_CAP: usize = 100_000;
struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    feed: Arc<Feed>,
    report: Report,
    /// The last reply as received, and whether it carried only light fields,
    /// so an identical one is neither parsed nor counted as a change.
    reply: Vec<u8>,
    quiet: bool,
    /// Bumped whenever a reply changes what this worker reports.
    revision: Arc<AtomicU64>,
    /// The efficiency setting the worker was last given.
    efficiency: crate::efficiency::Mode,
    /// Saved files reported by the worker, not yet given to the dataset.
    saved: Vec<crate::dataset::SavedFile>,
    saved_camera: Option<crate::dataset::SavedCamera>,
    saved_seq: u64,
    saved_gaps: u64,
}
// Linux can briefly refuse exec after a newly written helper was closed,
// including while another process is completing a concurrent fork/exec. No
// child was started on ETXTBSY, so retry only that error for a bounded time.
pub(crate) fn spawn_helper(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match command.spawn() {
            Err(error)
                if cfg!(target_os = "linux")
                    && error.raw_os_error() == Some(26)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

/// A camera worker inherits this process's decoding and efficiency settings,
/// including a `--efficiency` flag or desktop choice it cannot see itself.
fn worker_command(
    executable: std::path::PathBuf,
    efficiency: crate::efficiency::Mode,
) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command
        .env("CAPTUREFAB_HWACCEL", crate::media::hwaccel())
        .env(crate::efficiency::ENV, efficiency.name())
        .arg("__worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

impl Process {
    fn spawn(scanner: bool, revision: Arc<AtomicU64>) -> Result<Self> {
        let capacity = frame_capacity(scanner)?;
        let dir = crate::ipc::runtime_dir();
        crate::ipc::ensure_private_dir(&dir)?;
        // Ring files are left behind only when a process dies; clear them
        // before this coordinator's first camera.
        static SWEEP: std::sync::Once = std::sync::Once::new();
        SWEEP.call_once(|| crate::shared_memory::sweep(&dir));
        let ring = SharedRing::create(&dir, capacity)?;
        let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE")
            .map(std::path::PathBuf::from)
            .unwrap_or(std::env::current_exe()?);
        let efficiency = crate::efficiency::for_workers();
        let mut command = worker_command(executable, efficiency);
        ring.pass_to(&mut command);
        let child = command.spawn().context("launch camera capture process")?;
        Self::attach(child, ring, revision, efficiency)
    }
    /// Take over a worker started with `ring` and `efficiency`.
    fn attach(
        mut child: Child,
        mut ring: SharedRing,
        revision: Arc<AtomicU64>,
        efficiency: crate::efficiency::Mode,
    ) -> Result<Self> {
        ring.passed();
        let input = child.stdin.take().context("worker stdin")?;
        let output = BufReader::new(child.stdout.take().context("worker stdout")?);
        let feed = Arc::new(Feed::new(ring, child.id()));
        Ok(Self {
            child,
            input,
            output,
            feed,
            report: Report::new(),
            reply: Vec::new(),
            quiet: false,
            revision,
            efficiency,
            saved: Vec::new(),
            saved_camera: None,
            saved_seq: 0,
            saved_gaps: 0,
        })
    }
    fn call(&mut self, command: SessionCommand) -> Result<Value> {
        Ok(self.exchange(command)?.unwrap_or_default())
    }
    /// Send one command and apply the reply's snapshot.
    fn exchange(&mut self, command: SessionCommand) -> Result<Option<Value>> {
        let status = matches!(command, SessionCommand::Status);
        self.round_trip(&command, status, false)
    }
    /// Ask for the worker's status, giving it the coordinator's efficiency
    /// setting when that changed.
    fn refresh(&mut self, efficiency: crate::efficiency::Mode) -> Result<()> {
        let request = Refresh {
            efficiency: (efficiency != self.efficiency).then_some(efficiency),
            full: self.report.resync,
        };
        if request == Refresh::default() {
            self.exchange(SessionCommand::Status)?;
        } else {
            self.report.resync = false;
            self.round_trip(&json!({ "refresh": request }), true, request.full)?;
            self.efficiency = efficiency;
        }
        Ok(())
    }
    /// One request and its reply. `status` requests have no result, so an
    /// idle worker answers them with the same light reply again and again.
    fn round_trip(
        &mut self,
        request: &impl Serialize,
        status: bool,
        full: bool,
    ) -> Result<Option<Value>> {
        ensure!(
            self.child.try_wait()?.is_none(),
            "camera worker exited; disconnect and reconnect"
        );
        serde_json::to_writer(&mut self.input, request)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let mut data = Vec::with_capacity(self.reply.len());
        use std::io::Read;
        self.output
            .by_ref()
            .take(16 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut data)?;
        ensure!(
            data.len() <= 16 * 1024 * 1024 && data.last() == Some(&b'\n'),
            "camera worker exited or sent an invalid response"
        );
        if status && !full && self.quiet && data == self.reply {
            return Ok(None);
        }
        let mut reply = match serde_json::from_slice::<ParentReply>(&data) {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("capturefab: ignoring unreadable worker snapshot: {e}");
                self.report.resync = true;
                let reply: ParentReply<serde::de::IgnoredAny> = serde_json::from_slice(&data)?;
                ParentReply {
                    ok: reply.ok,
                    result: reply.result,
                    error: reply.error,
                    snapshot: None,
                    kept: Kept::default(),
                    saved: reply.saved,
                }
            }
        };
        // Before the ok check: a failed command may already have saved files.
        self.keep_saved(std::mem::take(&mut reply.saved));
        if let Some(snapshot) = reply.snapshot {
            self.quiet = reply.kept.features && reply.kept.history && snapshot.logs.is_empty();
            self.report.apply(snapshot, reply.kept, full);
            self.revision.fetch_add(1, Ordering::Relaxed);
        } else {
            self.quiet = false;
        }
        self.reply = data;
        ensure!(
            reply.ok,
            "{}",
            reply.error.as_deref().unwrap_or("worker command failed")
        );
        Ok(reply.result)
    }
    /// This worker's camera as session snapshots show it.
    fn camera(&self) -> Option<CameraSnapshot> {
        let preview_skipped = u64::from(self.feed.counters.preview_skipped());
        self.report.camera(self.child.id(), preview_skipped)
    }
    fn keep_saved(&mut self, batch: crate::dataset::SavedBatch) {
        if let Some(camera) = batch.camera {
            self.saved_camera = Some(camera);
        }
        // Entries the worker dropped are the oldest, so the jump to the next
        // item's sequence number counts them; `batch.lost` would count twice.
        for item in batch.items {
            // Sequence numbers increase by one; a jump means lost entries.
            self.saved_gaps += item.seq.saturating_sub(self.saved_seq + 1);
            self.saved_seq = self.saved_seq.max(item.seq);
            if self.saved.len() >= SAVED_CAP {
                self.saved_gaps += 1;
            } else {
                self.saved.push(item);
            }
        }
    }
    /// Ask the worker until its saved-file log is empty; each reply drains a
    /// bounded number of entries.
    fn drain_saved(&mut self) {
        for _ in 0..64 {
            let before = self.saved_seq;
            if self.exchange(SessionCommand::Status).is_err() || self.saved_seq == before {
                break;
            }
        }
    }
    /// Saved files and missed-entry count collected since the last call.
    fn take_saved(&mut self) -> (crate::dataset::SavedBatch, u64) {
        let batch = crate::dataset::SavedBatch {
            camera: self
                .saved_camera
                .clone()
                .or_else(|| self.report.snapshot.connected.as_ref().map(Into::into)),
            items: std::mem::take(&mut self.saved),
            lost: 0,
        };
        (batch, std::mem::take(&mut self.saved_gaps))
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.input.write_all(b"{\"shutdown\":true}\n");
        let _ = self.input.flush();
        // An idle worker exits within a few milliseconds of being told to.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut wait = Duration::from_millis(1);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Coordinator {
    analytics: crate::analytics::Analytics,
    started: Instant,
    state: RwLock<SessionSnapshot>,
    workers: RwLock<BTreeMap<String, Arc<Mutex<Process>>>>,
    feeds: RwLock<BTreeMap<String, Arc<Feed>>>,
    quitting: AtomicBool,
    /// Set when `shutdown` has finished; later callers wait for it.
    shut_down: (Mutex<bool>, Condvar),
    pending: AtomicUsize,
    connecting: Mutex<()>,
    command_order: Mutex<BTreeMap<CommandTarget, Weak<CommandOrder>>>,
    poller: OnceLock<std::thread::Thread>,
    /// How long the poller waits now: `PARKED`, `SLOW` or `QUICK`.
    pace: AtomicU8,
    /// When a view, browser or activity watcher last looked, in
    /// milliseconds since `started` plus one; zero for never.
    watched: AtomicU64,
    /// Bumped by worker reports and session edits; see `revision`.
    revision: Arc<AtomicU64>,
    log: Mutex<SessionLog>,
    activity: Arc<activity::Activity>,
    dataset: Arc<crate::dataset::Slot>,
    signals: crate::signals::hub::SignalHub,
}
impl Drop for Coordinator {
    fn drop(&mut self) {
        self.signals.stop_all_nowait();
        self.dataset.close_quiet();
        // The poller holds only a Weak; wake it so it exits now, not next tick.
        if let Some(poller) = self.poller.get() {
            poller.unpark();
        }
    }
}
/// Holds one of the bounded command slots; released even if the command panics.
struct PendingSlot(Arc<Coordinator>);
impl PendingSlot {
    fn acquire(inner: &Arc<Coordinator>) -> Result<Self> {
        // Count first; dropping the guard undoes the increment when full.
        let previous = inner.pending.fetch_add(1, Ordering::AcqRel);
        let slot = Self(inner.clone());
        ensure!(previous < MAX_PENDING, "camera command queue is full");
        Ok(slot)
    }
}
impl Drop for PendingSlot {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CommandTarget {
    Camera(String),
    Discovery,
    /// Signal sources start and stop in order.
    Signals,
}
#[derive(Default)]
struct CommandOrder {
    state: Mutex<CommandOrderState>,
    ready: Condvar,
}
#[derive(Default)]
struct CommandOrderState {
    next: u64,
    pending: VecDeque<u64>,
}
/// Reserved before spawning so thread scheduling cannot reorder commands.
/// Dropping an unstarted task removes its ticket without releasing a later
/// command while an earlier command is still running.
struct CommandTurn {
    order: Arc<CommandOrder>,
    ticket: u64,
}
impl CommandOrder {
    fn reserve(self: &Arc<Self>) -> CommandTurn {
        let mut state = locked(&self.state);
        let ticket = state.next;
        state.next = state.next.wrapping_add(1);
        state.pending.push_back(ticket);
        CommandTurn {
            order: self.clone(),
            ticket,
        }
    }
}
impl CommandTurn {
    fn wait(&self) {
        let mut state = locked(&self.order.state);
        while state.pending.front() != Some(&self.ticket) {
            state = self
                .order
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}
impl Drop for CommandTurn {
    fn drop(&mut self) {
        let mut state = locked(&self.order.state);
        if let Some(position) = state
            .pending
            .iter()
            .position(|ticket| *ticket == self.ticket)
        {
            state.pending.remove(position);
        }
        self.order.ready.notify_all();
    }
}
const MAX_PENDING: usize = 32;
/// Status poll intervals; see `poll_pace`.
const STATUS_POLL: Duration = Duration::from_millis(250);
const STATUS_POLL_IDLE: Duration = Duration::from_secs(1);
/// A view that looked this recently still counts as watching.
const WATCHING: Duration = Duration::from_secs(3);
const PARKED: u8 = 0;
const SLOW: u8 = 1;
const QUICK: u8 = 2;
/// The status poller polls workers briefly while a camera streams or
/// someone watches, longer otherwise, and waits to be woken while no camera
/// is connected. Efficiency mode halves the rate.
fn poll_pace(workers: bool, quick: bool, efficient: bool) -> (u8, Option<Duration>) {
    let scale = if efficient { 2 } else { 1 };
    match (workers, quick) {
        (false, _) => (PARKED, None),
        (true, true) => (QUICK, Some(STATUS_POLL * scale)),
        (true, false) => (SLOW, Some(STATUS_POLL_IDLE * scale)),
    }
}
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Coordinator>,
}
impl Default for SessionHandle {
    fn default() -> Self {
        Self::new()
    }
}
impl SessionHandle {
    pub fn new() -> Self {
        let h = Self {
            inner: Arc::new(Coordinator {
                analytics: crate::analytics::Analytics::new(),
                started: Instant::now(),
                state: RwLock::new(SessionSnapshot::default()),
                workers: RwLock::new(BTreeMap::new()),
                feeds: RwLock::new(BTreeMap::new()),
                quitting: AtomicBool::new(false),
                shut_down: (Mutex::new(false), Condvar::new()),
                pending: AtomicUsize::new(0),
                connecting: Mutex::new(()),
                command_order: Mutex::new(BTreeMap::new()),
                poller: OnceLock::new(),
                pace: AtomicU8::new(QUICK),
                watched: AtomicU64::new(0),
                revision: Arc::default(),
                log: Mutex::default(),
                activity: Arc::new(activity::Activity::default()),
                dataset: Arc::new(crate::dataset::Slot::new()),
                signals: crate::signals::hub::SignalHub::new(),
            }),
        };
        let weak = Arc::downgrade(&h.inner);
        let poller = std::thread::Builder::new()
            .name("capturefab-status".into())
            .spawn(move || {
                let mut wait = Some(STATUS_POLL);
                loop {
                    // Parked rather than slept so shutdown, drop and new
                    // work wake it at once.
                    match wait {
                        Some(wait) => std::thread::park_timeout(wait),
                        None => std::thread::park(),
                    }
                    let Some(inner) = weak.upgrade() else { break };
                    if inner.quitting.load(Ordering::Acquire) {
                        break;
                    }
                    let h = Self { inner };
                    h.refresh_workers();
                    h.store_snapshot();
                    wait = h.next_poll();
                }
            });
        match poller {
            Ok(thread) => {
                let _ = h.inner.poller.set(thread.thread().clone());
            }
            Err(e) => eprintln!("capturefab: status refresh unavailable: {e}"),
        }
        h
    }
    /// How long the status poller waits before its next round.
    fn next_poll(&self) -> Option<Duration> {
        let workers = !self.workers().is_empty();
        let quick = self.watching()
            || self
                .inner
                .state
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .cameras
                .iter()
                .any(|camera| camera.streaming);
        let (pace, wait) = poll_pace(workers, quick, crate::efficiency::efficient());
        self.inner.pace.store(pace, Ordering::Relaxed);
        wait
    }
    /// Wake the status poller if it waits at `pace`.
    fn hurry(&self, pace: u8) {
        if self.inner.pace.load(Ordering::Relaxed) == pace {
            self.wake();
        }
    }
    /// Wake the status poller whatever its pace. A wake that comes before it
    /// parks is kept, so one that just saw no workers still polls a new one.
    fn wake(&self) {
        if let Some(poller) = self.inner.poller.get() {
            poller.unpark();
        }
    }
    fn elapsed_ms(&self) -> u64 {
        self.inner.started.elapsed().as_millis() as u64 + 1
    }
    /// A view, browser or activity watcher looked: keep workers polled often.
    fn watch(&self) {
        self.inner
            .watched
            .store(self.elapsed_ms(), Ordering::Relaxed);
        self.hurry(SLOW);
    }
    fn watching(&self) -> bool {
        let watched = self.inner.watched.load(Ordering::Relaxed);
        watched != 0 && self.elapsed_ms() - watched < WATCHING.as_millis() as u64
    }
    /// Changes whenever a snapshot could: a camera worker reports
    /// something new, the session log grows, selection, devices or activity
    /// change, or a signal source runs. Views compare it with the revision
    /// they last drew to skip unchanged work. Asking counts as watching.
    pub fn revision(&self) -> u64 {
        self.watch();
        // Signal values change all the time.
        if self.inner.signals.has_sources() {
            self.inner.revision.fetch_add(1, Ordering::Relaxed);
        }
        self.inner
            .revision
            .load(Ordering::Relaxed)
            .wrapping_add(locked(&self.inner.log).revision)
            .wrapping_add(self.inner.activity.revision())
    }
    /// The stored state, for an edit outside a status poll.
    fn edit_state(&self) -> std::sync::RwLockWriteGuard<'_, SessionSnapshot> {
        self.inner.revision.fetch_add(1, Ordering::Relaxed);
        self.inner
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
    /// Refresh every idle worker's snapshot; busy workers report on their reply.
    fn refresh_workers(&self) {
        let workers = self.workers();
        if workers.is_empty() {
            return;
        }
        let efficiency = crate::efficiency::for_workers();
        for (id, w) in workers {
            let Ok(mut p) = w.try_lock() else { continue };
            if p.refresh(efficiency).is_err() && p.child.try_wait().ok().flatten().is_some() {
                self.inner.activity.cancel_jobs(
                    Some(&id),
                    Some(p.child.id()),
                    "camera worker exited before schedule completed",
                );
            }
            let (batch, gaps) = p.take_saved();
            // Never hold a worker lock across DuckDB.
            drop(p);
            self.inner.dataset.record_saved(batch, gaps);
        }
    }
    /// Give a connected worker's frames and status to the session.
    fn add_worker(&self, id: String, p: Process) {
        self.inner
            .feeds
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), p.feed.clone());
        self.inner
            .workers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::new(Mutex::new(p)));
        // The poller may have just seen no workers, at whatever pace.
        self.wake();
    }
    fn workers(&self) -> Vec<(String, Arc<Mutex<Process>>)> {
        self.inner
            .workers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    /// The snapshot the status poller last stored, for loops that check on
    /// work now and then. Unlike `snapshot`, reading it is not watching.
    pub fn stored(&self) -> SessionSnapshot {
        if self.inner.poller.get().is_none() {
            // No poller started, so poll here or the loop would never see
            // its work finish.
            self.refresh_workers();
            self.store_snapshot();
        }
        self.inner
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    /// The session as views show it. Asking counts as watching, so camera
    /// workers are polled often while views keep asking.
    pub fn snapshot(&self) -> SessionSnapshot {
        self.watch();
        self.assemble().0
    }
    /// Build a snapshot from every worker's latest report, with the session
    /// log revision it copied.
    fn assemble(&self) -> (SessionSnapshot, u64) {
        // Only these fields are the session's own; the rest is rebuilt below.
        let mut state = {
            let stored = self
                .inner
                .state
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            SessionSnapshot {
                devices: stored.devices.clone(),
                network_scan: stored.network_scan.clone(),
                active_camera: stored.active_camera.clone(),
                last_error: stored.last_error.clone(),
                ..Default::default()
            }
        };
        for (id, w) in self.workers() {
            if let Ok(p) = w.try_lock() {
                // Keep diagnostics from every camera, including those that
                // are not selected and cameras later disconnected.
                locked(&self.inner.log).take(&p.report.log);
                state.cameras.extend(p.camera());
            } else if let Some(c) = self
                .inner
                .state
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .cameras
                .iter()
                .find(|c| c.info.id == id)
            {
                state.cameras.push(CameraSnapshot {
                    stale: true,
                    ..c.clone()
                });
            }
        }
        self.inner.activity.observe_jobs(&state.cameras);
        let activity = self.inner.activity.snapshot();
        state.activity = activity.activity;
        state.activity_revision = activity.revision;
        state.dataset = self.inner.dataset.status_cached();
        state.signals = self.inner.signals.status_brief();
        project_selected(&mut state);
        let mut log = locked(&self.inner.log);
        log.take_signals(self.inner.signals.logs());
        state.logs = log.lines();
        (state, log.revision)
    }
    fn target(&self, target: Option<&str>) -> Result<Arc<Mutex<Process>>> {
        let id = target
            .map(str::to_owned)
            .or_else(|| {
                self.inner
                    .state
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .active_camera
                    .clone()
            })
            .context("no camera connected; use connect")?;
        let id = self.camera_id(&id);
        let workers = self.inner.workers.read().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = workers.get(&id) {
            return Ok(w.clone());
        }
        for w in workers.values() {
            if let Ok(p) = w.try_lock()
                && p.report
                    .snapshot
                    .connected
                    .as_ref()
                    .is_some_and(|c| c.serial == id || c.address.as_deref() == Some(&id))
            {
                return Ok(w.clone());
            }
        }
        anyhow::bail!("camera '{id}' not connected in this session")
    }
    /// The selected camera's id.
    pub fn active_camera(&self) -> Option<String> {
        self.inner
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .active_camera
            .clone()
    }
    pub fn latest_frame(&self) -> Option<Arc<Frame>> {
        self.latest_frame_for(&self.active_camera()?)
    }
    /// Newest frame from a camera's shared ring. Never waits for the worker's
    /// command pipe, so previews keep updating while commands run. Polling
    /// keeps the worker publishing; after a pause of over two seconds the
    /// first poll may return a keep-alive frame up to a second old (see
    /// `shared_memory`).
    pub fn latest_frame_for(&self, id: &str) -> Option<Arc<Frame>> {
        Some(self.latest_frame_tagged(id)?.1)
    }
    /// `latest_frame_for` with a tag naming the frame: the worker's pid above
    /// the ring sequence it was published under. Unlike the camera's frame
    /// ids, which start over with a worker and repeat for scanned pages, it
    /// changes with every frame.
    pub fn latest_frame_tagged(&self, id: &str) -> Option<(u64, Arc<Frame>)> {
        let feed = self.feed(id)?;
        let mut frames = locked(&feed.frames);
        let frame = frames.poll(true)?;
        let sequence = frames.ring.taken_sequence().unwrap_or(0);
        Some(((u64::from(feed.worker) << 32) | u64::from(sequence), frame))
    }
    /// When the frame a camera's feed last returned is a downscaled preview
    /// of a frame larger than its ring, the width, height and pixel format
    /// of the frame the camera delivered.
    pub fn preview_of(&self, id: &str) -> Option<(u32, u32, u32)> {
        let feed = self.feed(id)?;
        locked(&feed.frames).ring.preview_of()
    }
    /// The newest frame a camera has acquired, for occasional readers that
    /// need current pixels. An unwatched ring holds only keep-alive frames,
    /// so this asks for a frame, without keeping the ring watched, and waits
    /// up to `wait` for one offered since it was called.
    pub fn fresh_frame_for(&self, id: &str, wait: Duration) -> Option<Arc<Frame>> {
        let feed = self.feed(id)?;
        let deadline = Instant::now() + wait;
        let since = feed.counters.sequence();
        loop {
            let (frame, fresh) = {
                let mut frames = locked(&feed.frames);
                let frame = frames.poll(false);
                (frame, frames.ring.fresh() || frames.ring.taken_since(since))
            };
            if fresh || Instant::now() >= deadline {
                return frame;
            }
            feed.counters.ask();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// The frame a capture just published from the selected camera, for
    /// binary stdout. A downscaled preview of a frame larger than the ring
    /// is refused rather than written in its place.
    pub fn captured_frame(&self) -> Result<Arc<Frame>> {
        let id = self.active_camera().context("capture returned no frame")?;
        let feed = self.feed(&id).context("capture returned no frame")?;
        let mut frames = locked(&feed.frames);
        let frame = frames.poll(false).context("capture returned no frame")?;
        ensure!(
            !frames.ring.preview_only(),
            "the frame is larger than the {} shared frame buffer; save it to a file, or raise CAPTUREFAB_FRAME_BYTES",
            crate::analytics::human_bytes(frames.ring.capacity() as u64)
        );
        Ok(frame)
    }
    /// Changes whenever any camera offers a frame, or publishes one it held
    /// back. Reads each shared ring's sequence counter without its lock or
    /// pixels, so it is cheap to poll.
    pub fn frame_sequence(&self) -> u64 {
        self.inner
            .feeds
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|feed| u64::from(feed.counters.sequence()))
            .fold(0, u64::wrapping_add)
    }
    /// `frame_sequence` for only these cameras, so a view can wait on the
    /// cameras it shows.
    pub fn frame_sequence_of(&self, ids: &[&str]) -> u64 {
        let feeds = self
            .inner
            .feeds
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        ids.iter()
            .filter_map(|id| feeds.get(*id))
            .map(|feed| u64::from(feed.counters.sequence()))
            .fold(0, u64::wrapping_add)
    }
    fn feed(&self, id: &str) -> Option<Arc<Feed>> {
        let feeds = self
            .inner
            .feeds
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(feed) = feeds.get(id) {
            return Some(feed.clone());
        }
        drop(feeds);
        // Serial or address selectors resolve through the worker table.
        let w = self.target(Some(id)).ok()?;
        let p = w.try_lock().ok()?;
        Some(p.feed.clone())
    }
    pub fn submit(&self, c: SessionCommand) -> Result<Reply> {
        self.submit_with_origin(c, ActivityOrigin::Client)
    }
    pub fn submit_to(&self, camera: &str, c: SessionCommand) -> Result<Reply> {
        self.submit_to_with_origin(camera, c, ActivityOrigin::Client)
    }
    pub fn submit_with_origin(&self, c: SessionCommand, origin: ActivityOrigin) -> Result<Reply> {
        self.submit_as(c, origin, SessionActor::process(origin))
    }
    pub fn submit_to_with_origin(
        &self,
        camera: &str,
        c: SessionCommand,
        origin: ActivityOrigin,
    ) -> Result<Reply> {
        self.submit_to_as(camera, c, origin, SessionActor::process(origin))
    }
    pub fn submit_as(
        &self,
        c: SessionCommand,
        origin: ActivityOrigin,
        actor: SessionActor,
    ) -> Result<Reply> {
        self.submit_target(None, c, origin, actor)
    }
    pub fn submit_to_as(
        &self,
        camera: &str,
        c: SessionCommand,
        origin: ActivityOrigin,
        actor: SessionActor,
    ) -> Result<Reply> {
        self.submit_target(Some(camera.to_owned()), c, origin, actor)
    }
    /// Watching activity counts as watching the session, since job
    /// progress comes from worker polls.
    pub fn activity(&self) -> ActivityView {
        self.watch();
        self.inner.activity.snapshot()
    }
    pub fn activity_revision(&self) -> u64 {
        self.inner.activity.revision()
    }
    pub fn wait_for_activity(&self, after: u64, timeout: Duration) -> u64 {
        self.watch();
        self.inner.activity.wait(after, timeout)
    }
    /// Resolve aliases from cached identities, without waiting for a busy
    /// worker's command pipe. All names for one camera share an ordering gate.
    fn camera_id(&self, selector: &str) -> String {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let identities = || {
            state
                .cameras
                .iter()
                .map(|camera| &camera.info)
                .chain(state.connected.iter())
                .chain(state.devices.iter())
        };
        if identities().any(|info| info.id == selector) {
            return selector.to_owned();
        }
        identities()
            .find(|info| info.serial == selector || info.address.as_deref() == Some(selector))
            .map_or_else(|| selector.to_owned(), |info| info.id.clone())
    }
    fn command_turn(&self, target: Option<&str>, command: &SessionCommand) -> Option<CommandTurn> {
        if matches!(
            command,
            SessionCommand::Status
                | SessionCommand::Activity { .. }
                | SessionCommand::Signals
                | SessionCommand::SignalRead { .. }
                | SessionCommand::SignalBrowse { .. }
        ) || command.is_dataset()
        {
            return None;
        }
        let target = match command {
            SessionCommand::SignalStart { .. } | SessionCommand::SignalStop { .. } => {
                CommandTarget::Signals
            }
            SessionCommand::Discover { .. } | SessionCommand::NetworkScan { .. } => {
                CommandTarget::Discovery
            }
            SessionCommand::Connect { camera, .. } | SessionCommand::Select { camera } => {
                CommandTarget::Camera(self.camera_id(camera))
            }
            _ => CommandTarget::Camera(self.camera_id(target?)),
        };
        let mut orders = locked(&self.inner.command_order);
        // Idle gates have no state to retain. At most the bounded pending
        // commands keep gates alive, including across disconnect/reconnect.
        orders.retain(|_, order| order.strong_count() > 0);
        let order = orders
            .get(&target)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let order = Arc::new(CommandOrder::default());
                orders.insert(target, Arc::downgrade(&order));
                order
            });
        Some(order.reserve())
    }
    fn submit_target(
        &self,
        target: Option<String>,
        command: SessionCommand,
        origin: ActivityOrigin,
        actor: SessionActor,
    ) -> Result<Reply> {
        command.validate()?;
        actor.validate()?;
        if let Some(camera) = &target {
            validate_selector(camera)?;
        }
        ensure!(
            !self.inner.quitting.load(Ordering::Relaxed),
            "session is shutting down"
        );
        // Observers remain responsive even when every action slot is occupied.
        // Observations never wait for a command gate or a busy worker's pipe.
        if matches!(
            command,
            SessionCommand::Status | SessionCommand::Activity { .. } | SessionCommand::Signals
        ) {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(self.process(target.as_deref(), command, origin, actor));
            return Ok(rx);
        }
        let slot = PendingSlot::acquire(&self.inner)?;
        let target = target.or_else(|| {
            if matches!(
                &command,
                SessionCommand::Connect { .. }
                    | SessionCommand::Discover { .. }
                    | SessionCommand::NetworkScan { .. }
                    | SessionCommand::Select { .. }
                    | SessionCommand::Status
            ) || command.is_dataset()
                || command.is_signal()
            {
                None
            } else {
                self.inner
                    .state
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .active_camera
                    .clone()
            }
        });
        let target = target.map(|camera| self.camera_id(&camera));
        let turn = self.command_turn(target.as_deref(), &command);
        let mut activity =
            self.inner
                .activity
                .begin_as(origin, actor.clone(), target.as_deref(), &command);
        let h = self.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("capturefab-command".into())
            .spawn(move || {
                let _slot = slot;
                let _turn = turn;
                if let Some(turn) = &_turn {
                    turn.wait();
                }
                if let Some(activity) = &activity {
                    activity.running();
                }
                let result = if h.inner.quitting.load(Ordering::Acquire) {
                    Err(anyhow::anyhow!("session is shutting down"))
                } else {
                    h.process(target.as_deref(), command, origin, actor)
                };
                if let Err(e) = &result {
                    let message = activity::redact(&format!("{e:#}"));
                    h.edit_state().last_error = Some(message.clone());
                    locked(&h.inner.log).note(LogEntry::now_local_millis("error", message));
                }
                if let Some(activity) = &mut activity {
                    activity.finish(&result);
                }
                if h.inner.quitting.load(Ordering::Acquire) {
                    h.inner.activity.cancel_jobs(
                        None,
                        None,
                        "session stopped before schedule completed",
                    );
                }
                let _ = tx.send(result);
            })
            .context("start camera command thread")?;
        Ok(rx)
    }
    pub fn request(&self, c: SessionCommand) -> Result<Value> {
        self.submit(c)?.recv().context("coordinator stopped")?
    }
    pub fn request_to(&self, camera: &str, c: SessionCommand) -> Result<Value> {
        self.submit_to(camera, c)?
            .recv()
            .context("coordinator stopped")?
    }
    pub fn request_with_origin(&self, c: SessionCommand, origin: ActivityOrigin) -> Result<Value> {
        self.submit_with_origin(c, origin)?
            .recv()
            .context("coordinator stopped")?
    }
    pub fn request_to_with_origin(
        &self,
        camera: &str,
        c: SessionCommand,
        origin: ActivityOrigin,
    ) -> Result<Value> {
        self.submit_to_with_origin(camera, c, origin)?
            .recv()
            .context("coordinator stopped")?
    }
    pub fn request_as(
        &self,
        c: SessionCommand,
        origin: ActivityOrigin,
        actor: SessionActor,
    ) -> Result<Value> {
        self.submit_as(c, origin, actor)?
            .recv()
            .context("coordinator stopped")?
    }
    pub fn request_to_as(
        &self,
        camera: &str,
        c: SessionCommand,
        origin: ActivityOrigin,
        actor: SessionActor,
    ) -> Result<Value> {
        self.submit_to_as(camera, c, origin, actor)?
            .recv()
            .context("coordinator stopped")?
    }
    fn scan_network(
        &self,
        timeout: Duration,
        origin: ActivityOrigin,
        actor: &SessionActor,
    ) -> Result<Value> {
        self.refresh_workers();
        let before = self.assemble().0;
        let active: Vec<_> = before
            .cameras
            .iter()
            .map(|camera| camera.info.clone())
            .collect();
        let mut devices: Vec<_> = before
            .devices
            .iter()
            .filter(|camera| camera.transport != crate::types::Transport::GigE)
            .cloned()
            .collect();
        let mut warnings = Vec::new();
        match crate::transport::gige::discover(timeout) {
            Ok(discovered) => devices.extend(discovered),
            Err(error) => {
                warnings.push(format!("GigE discovery: {error:#}"));
                devices.extend(
                    before
                        .devices
                        .iter()
                        .filter(|camera| camera.transport == crate::types::Transport::GigE)
                        .cloned(),
                );
            }
        }
        for device in &mut devices {
            device.profile = before
                .devices
                .iter()
                .chain(active.iter())
                .find(|previous| previous.id == device.id)
                .map_or_else(
                    || crate::camera_profile::detect(device, &[]),
                    |previous| previous.profile.clone(),
                );
        }
        let mut report = crate::network::scan(&devices, &active, timeout)?;
        report.warnings.extend(warnings);
        // One budget per physical receive interface, including connected cameras
        // that are idle now but may be started together later.
        let plans: Vec<_> = report
            .cameras
            .iter()
            .enumerate()
            .filter(|(_, camera)| camera.active && camera.reachable)
            .map(|(index, camera)| {
                let sharers = report
                    .cameras
                    .iter()
                    .filter(|other| other.active && other.interface == camera.interface)
                    .count();
                (
                    index,
                    camera.camera.id.clone(),
                    crate::network::budget_bytes_per_second(camera, &report.interfaces, sharers),
                )
            })
            .collect();
        for batch in plans.chunks(4) {
            let results = std::thread::scope(|scope| {
                let before = &before;
                let threads: Vec<_> = batch
                    .iter()
                    .map(|(index, camera, budget)| {
                        (
                            *index,
                            scope.spawn(move || {
                                let current = self.assemble().0;
                                let original = before.cameras.iter().find(|item| item.info.id == *camera);
                                let unchanged = current.cameras.iter()
                                    .find(|item| item.info.id == *camera).zip(original)
                                    .is_some_and(|(now, then)| {
                                        now.worker_pid == then.worker_pid
                                            && now.info.address == then.info.address
                                    });
                                ensure!(unchanged,
                                    "Camera connection changed during the scan; scan again to apply current defaults"
                                );
                                self.request_to_as(
                                    camera,
                                    SessionCommand::OptimizeNetwork {
                                        bytes_per_second: *budget,
                                    },
                                    origin,
                                    actor.clone(),
                                )
                            }),
                        )
                    })
                    .collect();
                threads
                    .into_iter()
                    .map(|(index, thread)| {
                        (
                            index,
                            thread.join().unwrap_or_else(|_| {
                                Err(anyhow::anyhow!("network tuning worker stopped"))
                            }),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            for (index, result) in results {
                let camera = &mut report.cameras[index];
                match result.and_then(|value| {
                    serde_json::from_value::<crate::network::NetworkTuning>(value["tuning"].clone())
                        .context("reading camera network tuning")
                }) {
                    Ok(tuning) => {
                        camera.tuning = Some(tuning);
                    }
                    Err(error) => {
                        camera.tuning = Some(crate::network::NetworkTuning {
                            warnings: vec![format!("Defaults were not applied: {error:#}")],
                            ..Default::default()
                        })
                    }
                }
            }
        }
        self.refresh_workers();
        let after = self.assemble().0;
        if after.cameras.len() != before.cameras.len()
            || after.cameras.iter().any(|camera| {
                !before.cameras.iter().any(|old| {
                    old.info.id == camera.info.id
                        && old.worker_pid == camera.worker_pid
                        && old.info.address == camera.info.address
                })
            })
        {
            report.warnings.push("Camera connections changed during the scan. Scan again to refresh shared bandwidth budgets and reachability.".into());
        }
        for camera in &mut report.cameras {
            camera.active = after.cameras.iter().any(|active| {
                active.info.id == camera.camera.id && active.info.address == camera.camera.address
            });
            if let Some(current) = after
                .cameras
                .iter()
                .find(|active| active.info.id == camera.camera.id)
                && let Some(previous) = before
                    .cameras
                    .iter()
                    .find(|active| active.info.id == camera.camera.id)
                && !current.stale
                && !previous.stale
                && current.worker_pid == previous.worker_pid
                && current.info.address == previous.info.address
                && let (Some(now), Some(then)) = (&current.transport, &previous.transport)
            {
                let losses = now.lost_frames.saturating_sub(then.lost_frames)
                    + now.incomplete_frames.saturating_sub(then.incomplete_frames);
                let resends = now.resend_requested.saturating_sub(then.resend_requested);
                if losses > 0 || resends > 0 {
                    camera.health = crate::network::NetworkHealth::Unstable;
                    camera.summary = format!(
                        "Stream needs attention · {losses} frames lost, {resends} packet retries during scan"
                    );
                    camera.details.push("Check the cable and switch, use a dedicated Ethernet port, or reduce the camera data rate; scan again to retest.".into());
                } else if current.streaming
                    && current.frames > previous.frames
                    && now.packets_received > then.packets_received
                    && camera.health == crate::network::NetworkHealth::Unverified
                    && camera
                        .tuning
                        .as_ref()
                        .is_some_and(|tuning| tuning.warnings.is_empty())
                    && camera.interface.as_ref().is_some_and(|name| {
                        report.interfaces.iter().any(|interface| {
                            interface.name == *name
                                && camera.host_address == Some(interface.address)
                                && interface.kind == crate::network::InterfaceKind::Wired
                                && interface.speed_mbps.is_some_and(|speed| speed >= 1000)
                        })
                    })
                {
                    camera.health = crate::network::NetworkHealth::Ready;
                    camera.summary = "Frames received without loss during scan".into();
                }
            }
        }
        {
            let mut state = self.edit_state();
            state.devices = devices;
            state.network_scan = Some(report.clone());
        }
        Ok(json!({"network_scan":report}))
    }
    fn process(
        &self,
        target: Option<&str>,
        command: SessionCommand,
        origin: ActivityOrigin,
        actor: SessionActor,
    ) -> Result<Value> {
        match command {
            SessionCommand::Activity { after, timeout_ms } => {
                if let Some(after) = after {
                    self.wait_for_activity(after, Duration::from_millis(timeout_ms));
                }
                Ok(serde_json::to_value(self.activity())?)
            }
            SessionCommand::NetworkScan { timeout_ms } => {
                self.scan_network(Duration::from_millis(timeout_ms), origin, &actor)
            }
            command if command.is_dataset() => {
                crate::dataset::coordinator::run(&self.inner.dataset, &self.inner.signals, command)
            }
            command if command.is_signal() => {
                crate::signals::hub::run(&self.inner.signals, command)
            }
            // Both read the snapshot this request stores, without building another.
            SessionCommand::Status if target.is_some() => {
                self.target(target)?;
                let camera = self.camera_id(target.unwrap());
                self.store_snapshot();
                let shared = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
                let mut view = shared.clone();
                view.select_view(Some(&camera));
                addressed_status(serde_json::to_value(view)?, &shared, &camera)
            }
            SessionCommand::Status => {
                self.store_snapshot();
                let state = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
                Ok(serde_json::to_value(&*state)?)
            }
            SessionCommand::Discover {
                timeout_ms,
                simulated,
            } => {
                ensure!(
                    (1..=60000).contains(&timeout_ms),
                    "timeout must be 1..60000 milliseconds"
                );
                let (devices, warnings) =
                    crate::camera::discover(Duration::from_millis(timeout_ms), simulated)?;
                self.edit_state().devices = devices.clone();
                let mut log = locked(&self.inner.log);
                for warning in &warnings {
                    log.note(LogEntry::now_local("warn", warning.clone()));
                }
                drop(log);
                Ok(json!({"devices":devices,"warnings":warnings}))
            }
            SessionCommand::Connect { camera, timeout_ms } => {
                let _connect = self
                    .inner
                    .connecting
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Ok(w) = self.target(Some(&camera)) {
                    let p = w.lock().unwrap_or_else(|e| e.into_inner());
                    let info = p
                        .report
                        .snapshot
                        .connected
                        .clone()
                        .context("worker has no camera")?;
                    self.edit_state().active_camera = Some(info.id.clone());
                    return Ok(json!({"connected":info,"worker_pid":p.child.id()}));
                }
                crate::camera::validate_selector(&camera)?;
                let maximum = maximum_cameras()?;
                ensure!(
                    self.workers().len() < maximum,
                    "camera limit reached ({maximum}); disconnect a camera or increase CAPTUREFAB_MAX_CAMERAS"
                );
                let scanner = crate::scanner::is_source(&camera)
                    || self
                        .inner
                        .state
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .devices
                        .iter()
                        .any(|device| {
                            device.transport == crate::types::Transport::Scanner
                                && (device.id == camera || device.serial == camera)
                        });
                let mut p = Process::spawn(scanner, self.inner.revision.clone())?;
                let result = p.call(SessionCommand::Connect { camera, timeout_ms })?;
                let info = p
                    .report
                    .snapshot
                    .connected
                    .clone()
                    .context("worker connected without identity")?;
                let id = info.id.clone();
                let pid = p.child.id();
                self.add_worker(id.clone(), p);
                self.edit_state().active_camera = Some(id);
                self.store_snapshot();
                Ok(json!({"connected":result["connected"],"worker_pid":pid}))
            }
            SessionCommand::Select { camera } => {
                let w = self.target(Some(&camera))?;
                let p = w.lock().unwrap_or_else(|e| e.into_inner());
                let id = p
                    .report
                    .snapshot
                    .connected
                    .as_ref()
                    .context("worker has no camera")?
                    .id
                    .clone();
                drop(p);
                self.edit_state().active_camera = Some(id);
                self.store_snapshot();
                Ok(json!({"active_camera":self.active_camera()}))
            }
            SessionCommand::Disconnect => {
                let w = self.target(target)?;
                let p = w.lock().unwrap_or_else(|e| e.into_inner());
                let id = p
                    .report
                    .snapshot
                    .connected
                    .as_ref()
                    .context("worker has no camera")?
                    .id
                    .clone();
                drop(p);
                let mut final_snapshot = self.assemble().0;
                let mut p = w.lock().unwrap_or_else(|e| e.into_inner());
                let result = p.call(SessionCommand::Disconnect);
                // The worker exits once dropped; collect what it still holds,
                // keeping the disconnect reply's final counters.
                let disconnected = p.report.snapshot.clone();
                p.drain_saved();
                p.report.snapshot = disconnected;
                let (batch, gaps) = p.take_saved();
                let result = match result {
                    Ok(result) => result,
                    Err(e) => {
                        drop(p);
                        self.inner.dataset.record_saved(batch, gaps);
                        return Err(e);
                    }
                };
                self.inner.activity.cancel_jobs(
                    Some(&id),
                    Some(p.child.id()),
                    "camera disconnected before schedule completed",
                );
                // The disconnect reply carries the final counters but clears
                // the camera identity. Preserve that identity so even a stream
                // shorter than a status poll contributes all its activity.
                if let Some(camera) = final_snapshot.cameras.iter_mut().find(|c| c.info.id == id) {
                    camera.frames = p.report.snapshot.frames;
                    camera.received_bytes = p.report.snapshot.received_bytes;
                    camera.preview_skipped = u64::from(p.feed.counters.preview_skipped());
                    camera.dropped = p
                        .report
                        .snapshot
                        .dropped
                        .saturating_add(camera.preview_skipped);
                    camera.lost = frames_lost(&p.report.snapshot);
                    camera.streaming = false;
                    camera.fps = 0.0;
                    camera.stale = false;
                }
                // Its last lines, such as why it stopped, stay in the session log.
                let mut log = locked(&self.inner.log);
                log.take(&p.report.log);
                log.forget(p.report.log.source);
                drop(log);
                drop(p);
                self.inner.dataset.record_saved(batch, gaps);
                self.inner.analytics.observe_final(&final_snapshot);
                self.inner
                    .feeds
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                self.inner
                    .workers
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                let next = self
                    .inner
                    .workers
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys()
                    .next()
                    .cloned();
                let mut s = self.edit_state();
                if s.active_camera.as_ref() == Some(&id) {
                    s.active_camera = next;
                }
                drop(s);
                drop(w);
                self.store_snapshot();
                Ok(result)
            }
            command => {
                let w = self.target(target)?;
                let schedule = matches!(command, SessionCommand::Schedule { .. });
                let (result, (batch, gaps)) = {
                    let mut worker = w.lock().unwrap_or_else(|e| e.into_inner());
                    let mut result = worker.call(command);
                    if schedule
                        && let Ok(value) = &mut result
                        && let Some(fields) = value.as_object_mut()
                    {
                        fields.insert("worker_pid".into(), json!(worker.child.id()));
                    }
                    (result, worker.take_saved())
                };
                self.inner.dataset.record_saved(batch, gaps);
                self.store_snapshot();
                result
            }
        }
    }
    fn store_snapshot(&self) {
        let (snapshot, revision) = self.assemble();
        self.inner.analytics.observe(&snapshot);
        if snapshot.cameras.iter().any(|camera| camera.streaming) {
            self.hurry(SLOW);
        }
        self.commit(snapshot, revision);
    }
    /// Store a snapshot built from the session log at `revision`.
    fn commit(&self, mut snapshot: SessionSnapshot, revision: u64) {
        let mut state = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
        merge_session_metadata(&mut snapshot, &state);
        // Lines logged while this snapshot was built: copy them too.
        let log = locked(&self.inner.log);
        if log.revision != revision {
            snapshot.logs = log.lines();
        }
        drop(log);
        // Busy workers change nothing themselves but show as stale.
        if snapshot.cameras.len() != state.cameras.len()
            || (snapshot.cameras.iter().zip(&state.cameras))
                .any(|(now, then)| now.info.id != then.info.id || now.stale != then.stale)
        {
            self.inner.revision.fetch_add(1, Ordering::Relaxed);
        }
        *state = snapshot;
    }
    pub fn analytics(&self) -> &crate::analytics::Analytics {
        &self.inner.analytics
    }
    /// The session's dataset catalog slot.
    pub fn dataset(&self) -> &Arc<crate::dataset::Slot> {
        &self.inner.dataset
    }
    /// The session's signal sources.
    pub fn signals(&self) -> &crate::signals::hub::SignalHub {
        &self.inner.signals
    }
    /// Before workers are dropped: stop recordings cleanly and catalog the
    /// files every worker still holds.
    fn finalize_workers(&self) {
        for (id, w) in self.workers() {
            let deadline = Instant::now() + Duration::from_secs(5);
            let guard = loop {
                match w.try_lock() {
                    Ok(p) => break Some(p),
                    Err(std::sync::TryLockError::Poisoned(e)) => break Some(e.into_inner()),
                    Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(std::sync::TryLockError::WouldBlock) => break None,
                }
            };
            let Some(mut p) = guard else {
                eprintln!(
                    "capturefab: camera {id} stayed busy; its last saved files may not be cataloged"
                );
                continue;
            };
            if p.report.snapshot.forwarding.is_some()
                && let Err(e) = p.call(SessionCommand::StopForward)
            {
                eprintln!("capturefab: stop recording on {id}: {e:#}");
            }
            p.drain_saved();
            let (batch, gaps) = p.take_saved();
            drop(p);
            self.inner.dataset.record_saved(batch, gaps);
        }
    }
    pub fn uptime_seconds(&self) -> f64 {
        self.inner.started.elapsed().as_secs_f64()
    }
    pub fn is_running(&self) -> bool {
        !self.is_shutting_down() && self.inner.poller.get().is_some()
    }
    pub fn shutdown(&self) {
        if self.inner.quitting.swap(true, Ordering::AcqRel) {
            // Another thread (a Ctrl-C handler) is finishing recordings and
            // the dataset; returning now would let the process exit first.
            let (done, finished) = &self.inner.shut_down;
            let _ = finished
                .wait_timeout_while(locked(done), Duration::from_secs(30), |done| !*done)
                .unwrap_or_else(PoisonError::into_inner);
            return;
        }
        self.inner.activity.stop();
        self.inner
            .activity
            .cancel_jobs(None, None, "session stopped before schedule completed");
        self.wake();
        // Signals first, so their final samples reach the dataset before it closes.
        let signals = &self.inner.signals;
        signals.stop_all(Duration::from_secs(3));
        let flushed = signals
            .flush(Duration::from_secs(10))
            .and_then(|()| signals.set_sink(None, Duration::from_secs(10)));
        if let Err(e) = flushed {
            eprintln!("capturefab: signal samples not flushed: {e:#}");
        }
        self.finalize_workers();
        self.inner.dataset.close_quiet();
        for feed in self
            .inner
            .feeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            feed.counters.cancel();
        }
        let workers = std::mem::take(
            &mut *self
                .inner
                .workers
                .write()
                .unwrap_or_else(|e| e.into_inner()),
        );
        drop(workers);
        self.inner
            .feeds
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let (done, finished) = &self.inner.shut_down;
        *locked(done) = true;
        finished.notify_all();
    }
    pub fn is_shutting_down(&self) -> bool {
        self.inner.quitting.load(Ordering::Relaxed)
    }
}

/// A line a coordinator writes to its worker: a session command, or one of
/// the requests only a coordinator sends.
#[derive(Debug)]
enum WorkerRequest {
    Shutdown,
    Refresh(Refresh),
    Command(Box<SessionCommand>),
}
impl WorkerRequest {
    fn read(data: &[u8]) -> Result<Self> {
        let control = serde_json::from_slice::<Value>(data).ok();
        if control.as_ref().is_some_and(|v| v["shutdown"] == true) {
            return Ok(Self::Shutdown);
        }
        match control.as_ref().and_then(|v| v.get("refresh")) {
            Some(refresh) => Ok(Self::Refresh(Refresh::deserialize(refresh)?)),
            None => Ok(Self::Command(Box::new(SessionCommand::from_json_slice(
                data,
            )?))),
        }
    }
}

/// Hidden mode: stdio controls, separate publisher, mmap frame transfer.
/// `ring` is what `SharedRing::pass_to` gave the worker.
pub fn run_worker(ring: &std::ffi::OsStr) -> Result<()> {
    let ring = Arc::new(Mutex::new(SharedRing::open(ring)?));
    let handle = crate::engine::WorkerHandle::with_ring(ring);
    let result = (|| -> Result<()> {
        let input = std::io::stdin();
        let mut output = std::io::stdout().lock();
        let mut reader = input.lock();
        let mut sent = Sent::default();
        loop {
            let mut data = Vec::new();
            use std::io::Read;
            reader
                .by_ref()
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut data)?;
            if data.is_empty() {
                break;
            }
            ensure!(
                data.len() <= 1024 * 1024 && data.last() == Some(&b'\n'),
                "worker command exceeds 1 MiB"
            );
            let result = match WorkerRequest::read(&data)? {
                WorkerRequest::Shutdown => break,
                WorkerRequest::Refresh(refresh) => {
                    if let Some(mode) = refresh.efficiency {
                        // The coordinator's choice outranks the one this
                        // worker started with.
                        crate::efficiency::configure(Some(mode));
                    }
                    if refresh.full {
                        sent = Sent::default();
                    }
                    Ok(None)
                }
                WorkerRequest::Command(command) => match *command {
                    SessionCommand::Status => Ok(None),
                    command => handle.request(command).map(Some),
                },
            };
            // One snapshot clone per reply, serialized straight to the pipe; the
            // clone keeps the state lock out of the blocking write.
            let mut snapshot = handle.snapshot();
            let kept = sent.strip(&mut snapshot);
            let (ok, result, error) = match result {
                Ok(result) => (true, result, None),
                Err(e) => (false, None, Some(format!("{e:#}"))),
            };
            serde_json::to_writer(
                &mut output,
                &WorkerReply {
                    ok,
                    result,
                    error,
                    snapshot: &snapshot,
                    kept,
                    saved: handle.take_saved(2048),
                },
            )?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok(())
    })();
    handle.shutdown();
    result
}

#[cfg(test)]
impl SessionHandle {
    /// A session whose selected camera, "cam", is fed by a ring without a
    /// worker, and that ring's writer, for tests of frame consumers.
    pub(crate) fn fed(capacity: usize) -> (Self, SharedRing) {
        let writer = SharedRing::create(&std::env::temp_dir(), capacity).unwrap();
        let handle = Self::new();
        handle.inner.feeds.write().unwrap().insert(
            "cam".into(),
            Arc::new(Feed::new(writer.reader().unwrap(), 0)),
        );
        handle.inner.state.write().unwrap().active_camera = Some("cam".into());
        (handle, writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::Published;
    #[test]
    fn workers_are_told_this_process_efficiency_setting() {
        let command = worker_command("capturefab".into(), crate::efficiency::for_workers());
        let env = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
        };
        // Resolved here: efficient whenever this process is, otherwise the
        // setting, which the worker then resolves on the same host.
        assert_eq!(
            env(crate::efficiency::ENV).as_deref(),
            Some(crate::efficiency::for_workers().name())
        );
        assert_eq!(
            env("CAPTUREFAB_HWACCEL").as_deref(),
            Some(crate::media::hwaccel().as_str())
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("__worker")]
        );
    }
    #[test]
    fn parent_replies_carry_saved_files_even_when_failed() {
        let old: ParentReply = serde_json::from_str(r#"{"ok":true,"snapshot":null}"#).unwrap();
        assert!(old.saved.is_empty());
        let failed: ParentReply = serde_json::from_str(
            r#"{"ok":false,"error":"frame 3 timed out","saved":{"camera":{"id":"sim:0","transport":"simulator","vendor":"","model":"","serial":""},
               "items":[{"seq":1,"wall_ms":5,"path":"/a.png","bytes":1,"kind":"capture","frame_id":1,"timestamp_ns":2,
               "width":3,"height":4,"pixel_format":5,"format":"png","origin":"capture"}],"lost":2}}"#,
        )
        .unwrap();
        assert!(!failed.ok);
        assert_eq!((failed.saved.items.len(), failed.saved.lost), (1, 2));
    }
    #[test]
    fn plant_floor_commands_keep_old_wire_shapes_and_strict_unit_ops() {
        let old = r#"{"op":"capture","output":"a.png","count":1,"timeout_ms":100,"format":"png"}"#;
        let command = SessionCommand::from_json_slice(old.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_value(&command).unwrap()["on_exists"],
            Value::Null
        );
        assert!(
            serde_json::to_string(&command)
                .unwrap()
                .find("context")
                .is_none()
        );
        for op in ["dataset_close", "dataset_status", "signals"] {
            let extra = format!(r#"{{"op":"{op}","x":1}}"#);
            assert!(
                SessionCommand::from_json_slice(extra.as_bytes()).is_err(),
                "{op}"
            );
            let bare = format!(r#"{{"op":"{op}"}}"#);
            SessionCommand::from_json_slice(bare.as_bytes()).unwrap();
        }
        let start = br#"{"op":"signal_start","uri":"sim-signal://line1"}"#;
        let SessionCommand::SignalStart {
            interval_ms, tags, ..
        } = SessionCommand::from_json_slice(start).unwrap()
        else {
            panic!("not a signal start")
        };
        assert_eq!((interval_ms, tags.len()), (1000, 0));
        let snapshot = serde_json::to_value(SessionSnapshot::default()).unwrap();
        assert!(snapshot.get("dataset").is_none() && snapshot.get("signals").is_none());
    }
    fn cached_camera(id: &str, frames: u64) -> CameraSnapshot {
        CameraSnapshot {
            info: crate::transport::simulator::info_named(id),
            features: Arc::default(),
            streaming: true,
            frames,
            received_bytes: frames * 100,
            dropped: 0,
            fps: 30.0,
            last_error: None,
            worker_pid: 1,
            forwarding: Some(format!("{id}.mp4")),
            jobs: Vec::new(),
            auto: None,
            capture_session: None,
            transport: None,
            history: CameraHistoryStatus::default(),
            lost: 0,
            preview_skipped: 0,
            stale: true,
            error_logged: None,
            forwarding_since_ms: None,
        }
    }
    #[test]
    fn busy_selected_camera_uses_its_own_cached_state_and_disconnect_clears_view() {
        let mut snapshot = SessionSnapshot {
            active_camera: Some("sim:1".into()),
            connected: Some(crate::transport::simulator::info_named("sim:0")),
            frames: 10,
            cameras: vec![cached_camera("sim:0", 10), cached_camera("sim:1", 20)],
            ..Default::default()
        };
        project_selected(&mut snapshot);
        assert_eq!(snapshot.connected.as_ref().unwrap().id, "sim:1");
        assert_eq!(snapshot.frames, 20);
        assert_eq!(snapshot.forwarding.as_deref(), Some("sim:1.mp4"));
        snapshot.active_camera = None;
        project_selected(&mut snapshot);
        assert!(
            snapshot.connected.is_none() && !snapshot.streaming && snapshot.forwarding.is_none()
        );
        assert_eq!(
            (snapshot.frames, snapshot.received_bytes, snapshot.fps),
            (0, 0, 0.0)
        );
    }
    #[test]
    fn snapshot_commits_preserve_selection_and_discovery_completed_during_a_poll() {
        let mut in_flight = SessionSnapshot {
            active_camera: Some("sim:0".into()),
            cameras: vec![cached_camera("sim:0", 10), cached_camera("sim:1", 20)],
            ..Default::default()
        };
        let current = SessionSnapshot {
            active_camera: Some("sim:1".into()),
            devices: vec![crate::transport::simulator::info_named("sim:2")],
            ..Default::default()
        };
        merge_session_metadata(&mut in_flight, &current);
        assert_eq!(in_flight.active_camera.as_deref(), Some("sim:1"));
        assert_eq!(in_flight.connected.as_ref().unwrap().id, "sim:1");
        assert_eq!(in_flight.devices[0].id, "sim:2");
    }
    #[test]
    fn lines_logged_while_a_poll_builds_its_snapshot_are_stored_with_it() {
        let handle = SessionHandle::new();
        let (in_flight, revision) = handle.assemble();
        locked(&handle.inner.log).note(LogEntry::now_local_millis("error", "client failure"));
        handle.commit(in_flight, revision);
        let stored = handle.inner.state.read().unwrap().logs.clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].message, "client failure");
        handle.shutdown();
    }
    #[test]
    fn log_timestamps_survive_redaction_snapshot_merge_and_same_clock_events() {
        let first = LogEntry {
            timestamp: LogTimestamp::new(1_792_017_000_123, 0),
            ..LogEntry::legacy("12:30:00", "error", "rtsp://user:secret@example.test/live")
        };
        let tagged = camera_log(&first, "camera-a");
        assert_eq!(tagged.timestamp, first.timestamp);
        assert!(tagged.message.starts_with("[camera-a]"));
        assert!(!tagged.message.contains("secret"));
        let later = LogEntry {
            timestamp: LogTimestamp::new(first.timestamp.unwrap().unix_ms() + 86_400_000, 0),
            ..tagged.clone()
        };
        // The later line arrives first; the session log orders by time.
        let mut log = SessionLog::default();
        log.note(later.clone());
        log.note(tagged.clone());
        // The same event reported by a worker as well is kept once.
        log.add(WORKERS, 0, tagged.clone());
        let lines = log.lines();
        assert_eq!(
            lines.len(),
            2,
            "same display clocks on different dates remain distinct"
        );
        assert_eq!(lines[0].timestamp, tagged.timestamp);
        assert_eq!(lines[1].timestamp, later.timestamp);
        log.note(tagged.clone());
        assert_eq!(
            log.lines().len(),
            3,
            "a source repeating itself logged twice"
        );
    }
    #[test]
    fn addressed_status_preserves_camera_fields_and_includes_actual_session_selection() {
        let activity = Arc::new(activity::Activity::default());
        let mut command = activity
            .begin(
                ActivityOrigin::Client,
                Some("sim:1"),
                &SessionCommand::Start,
            )
            .unwrap();
        command.finish(&Ok(json!({})));
        let context = SessionSnapshot {
            active_camera: Some("sim:0".into()),
            cameras: vec![cached_camera("sim:0", 10), cached_camera("sim:1", 20)],
            devices: vec![crate::transport::simulator::info_named("sim:2")],
            activity: activity.snapshot().activity,
            ..Default::default()
        };
        let result = addressed_status(
            json!({
                "connected": {"id": "sim:1"}, "features": [{"name": "Width", "value": 80}],
                "streaming": true, "forwarding": "sim:1.mp4", "frames": 20,
                "jobs": [{"id": 1}], "history": {"undo": "Start stream"}
            }),
            &context,
            "sim:1",
        )
        .unwrap();
        assert_eq!(result["target_camera"], "sim:1");
        assert_eq!(
            result["active_camera"], "sim:0",
            "status never changes visible selection"
        );
        assert_eq!(result["connected"]["id"], "sim:1");
        assert_eq!(result["forwarding"], "sim:1.mp4");
        assert_eq!(result["features"][0]["value"], 80);
        assert_eq!(result["jobs"][0]["id"], 1);
        assert_eq!(result["history"]["undo"], "Start stream");
        assert_eq!(result["cameras"].as_array().unwrap().len(), 2);
        assert_eq!(result["devices"][0]["id"], "sim:2");
        assert_eq!(result["activity"][0]["origin"], "client");
    }
    #[test]
    fn strict_json_commands_reject_ignored_or_duplicate_fields() {
        let activity = SessionCommand::from_json_slice(br#"{"op":"activity"}"#).unwrap();
        assert!(matches!(
            activity,
            SessionCommand::Activity {
                after: None,
                timeout_ms: 0
            }
        ));
        assert!(SessionCommand::from_json_slice(br#"{"op":"activity","unknown":1}"#).is_err());
        for op in [
            "disconnect",
            "start",
            "stop",
            "undo",
            "redo",
            "jobs",
            "status",
            "features",
            "xml",
            "stop_forward",
        ] {
            let valid = format!(r#"{{"op":"{op}"}}"#);
            assert!(SessionCommand::from_json_slice(valid.as_bytes()).is_ok());
            let extra = format!(r#"{{"op":"{op}","unknown":1}}"#);
            assert!(
                SessionCommand::from_json_slice(extra.as_bytes())
                    .unwrap_err()
                    .to_string()
                    .contains("unknown field `unknown`")
            );
        }
        for (command, field) in [
            (r#"{"op":"status","op":"start"}"#, "op"),
            (
                r#"{"op":"set","feature":"Width","feature":"Height","value":"8"}"#,
                "feature",
            ),
            (
                r#"{"op":"capture","output":"frame.png","count":1,"timeout_ms":1000,"format":"png","storage":{"max_files":1,"max_files":2}}"#,
                "max_files",
            ),
        ] {
            assert!(
                SessionCommand::from_json_slice(command.as_bytes())
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("duplicate field `{field}`"))
            );
        }
        assert!(SessionCommand::from_json_slice(br#"{"op":"status"} {}"#).is_err());
    }
    #[test]
    fn invalid_command_values_are_rejected_before_queueing() {
        let handle = SessionHandle::new();
        for command in [
            SessionCommand::NetworkScan { timeout_ms: 0 },
            SessionCommand::Activity {
                after: Some(0),
                timeout_ms: 25_001,
            },
            SessionCommand::OptimizeNetwork {
                bytes_per_second: 0,
            },
            SessionCommand::OptimizeNetwork {
                bytes_per_second: u64::MAX,
            },
            SessionCommand::Connect {
                camera: "sim:0".into(),
                timeout_ms: 0,
            },
            SessionCommand::Select { camera: " ".into() },
            SessionCommand::Auto {
                balance: Some(f64::NAN),
            },
            SessionCommand::Auto {
                balance: Some(1.01),
            },
            SessionCommand::Get {
                feature: "Width\n".into(),
            },
            SessionCommand::ReadMemory {
                address: u64::MAX,
                length: 2,
            },
            SessionCommand::WriteMemory {
                address: 0,
                data: vec![],
            },
            SessionCommand::CancelJob { id: 0 },
        ] {
            assert!(handle.submit(command).is_err());
        }
        let snapshot = handle.snapshot();
        assert!(snapshot.cameras.is_empty());
        assert!(snapshot.logs.is_empty());
        assert!(snapshot.last_error.is_none());
        handle.shutdown();
    }

    fn width_command() -> SessionCommand {
        SessionCommand::Set {
            feature: "Width".into(),
            value: "128".into(),
        }
    }

    #[test]
    fn camera_command_turns_follow_submission_order_with_independent_targets() {
        let handle = SessionHandle::new();
        let first = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let second = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let third = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let other = handle
            .command_turn(Some("camera-b"), &width_command())
            .unwrap();
        first.wait();
        let (events, received) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let spawn =
            |turn: CommandTurn, number, events: mpsc::Sender<_>, started: mpsc::Sender<_>| {
                std::thread::spawn(move || {
                    started.send(number).unwrap();
                    turn.wait();
                    events.send(number).unwrap();
                })
            };
        // Start later submissions first to exercise the ordering gate rather
        // than relying on the scheduler to happen to start threads in order.
        let third_thread = spawn(third, 3, events.clone(), started.clone());
        assert_eq!(running.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        let second_thread = spawn(second, 2, events.clone(), started.clone());
        assert_eq!(running.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        let other_thread = spawn(other, 4, events, started);
        assert_eq!(running.recv_timeout(Duration::from_secs(2)).unwrap(), 4);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 4);
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(first);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        for thread in [third_thread, second_thread, other_thread] {
            thread.join().unwrap();
        }
    }

    #[test]
    fn canceled_or_panicking_command_turns_do_not_deadlock_or_bypass_earlier_commands() {
        let order = Arc::new(CommandOrder::default());
        let first = order.reserve();
        let canceled = order.reserve();
        let last = order.reserve();
        first.wait();
        // Builder::spawn drops its closure on failure. This exercises that
        // exact ownership path without depending on OS thread exhaustion.
        let unstarted_task = move || {
            canceled.wait();
        };
        drop(unstarted_task);
        assert_eq!(locked(&order.state).pending.len(), 2);
        let (started, running) = mpsc::channel();
        let (completed, received) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            last.wait();
            completed.send(()).unwrap();
        });
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        // A running task unwinding also releases the head reservation.
        assert!(
            std::panic::catch_unwind(move || {
                let _first = first;
                panic!("simulated command failure");
            })
            .is_err()
        );
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
        assert!(locked(&order.state).pending.is_empty());
        order.reserve().wait();
    }

    #[test]
    fn submission_threads_obey_the_target_gate_and_allow_other_targets_to_finish() {
        let handle = SessionHandle::new();
        let running = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        running.wait();
        // Disconnected-camera errors are enough to exercise the real
        // submission thread without launching a subprocess in a unit test.
        let blocked = handle.submit_to("camera-a", width_command()).unwrap();
        assert_eq!(handle.snapshot().activity[0].state, ActivityState::Queued);
        let independent = handle.submit_to("camera-b", width_command()).unwrap();
        assert!(
            independent
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(matches!(
            blocked.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(running);
        assert!(
            blocked
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.activity.len(), 2);
        assert!(
            snapshot
                .activity
                .iter()
                .all(|entry| entry.state == ActivityState::Failed)
        );
        assert!(snapshot.logs.iter().all(|entry| !entry.time.is_empty()));
        handle.request(SessionCommand::Status).unwrap();
        assert_eq!(
            handle.snapshot().activity.len(),
            2,
            "status polling creates no history entries"
        );
    }

    #[test]
    fn status_and_activity_remain_visible_when_action_queue_is_full() {
        let handle = SessionHandle::new();
        let running = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        running.wait();
        let replies: Vec<_> = (0..MAX_PENDING)
            .map(|index| {
                handle
                    .submit_to_as(
                        "camera-a",
                        width_command(),
                        ActivityOrigin::Client,
                        SessionActor::new(
                            format!("agent:{index}"),
                            format!("Agent {index}"),
                            ActorKind::Agent,
                        )
                        .unwrap(),
                    )
                    .unwrap()
            })
            .collect();
        assert!(
            handle
                .submit_to("camera-a", width_command())
                .unwrap_err()
                .to_string()
                .contains("queue is full")
        );
        let revision = handle.activity_revision();
        let started = Instant::now();
        let status = handle.request(SessionCommand::Status).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(status["activity"].as_array().unwrap().len(), MAX_PENDING);
        assert_eq!(status["activity_revision"], revision);
        let activity = handle
            .request_to(
                "camera-a",
                SessionCommand::Activity {
                    after: Some(revision + 99),
                    timeout_ms: 25_000,
                },
            )
            .unwrap();
        assert_eq!(activity["revision"], revision);
        assert_eq!(activity["activity"].as_array().unwrap().len(), MAX_PENDING);
        assert!(
            status["activity"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["state"] == "queued" && entry["actor"]["kind"] == "agent")
        );
        drop(running);
        for reply in replies {
            assert!(reply.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        }
        handle.shutdown();
    }

    #[test]
    fn session_activity_observers_wake_on_actions_and_shutdown_without_camera_access() {
        let handle = SessionHandle::new();
        let running = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        running.wait();
        let (started, ready) = mpsc::channel();
        let observers: Vec<_> = (0..2)
            .map(|_| {
                let h = handle.clone();
                let started = started.clone();
                std::thread::spawn(move || {
                    started.send(()).unwrap();
                    h.request_to(
                        "unconnected-camera",
                        SessionCommand::Activity {
                            after: Some(0),
                            timeout_ms: 25_000,
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        for _ in 0..2 {
            ready.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        let action = handle.submit_to("camera-a", width_command()).unwrap();
        for observer in observers {
            let view = observer.join().unwrap();
            assert_eq!(view["revision"], 1);
            assert_eq!(view["activity"][0]["state"], "queued");
        }
        assert_eq!(
            handle.snapshot().activity.len(),
            1,
            "observing never creates history"
        );
        drop(running);
        assert!(
            action
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        let revision = handle.activity_revision();
        let (returned, result) = mpsc::channel();
        let h = handle.clone();
        let waiting = std::thread::spawn(move || {
            let view = h.request(SessionCommand::Activity {
                after: Some(revision),
                timeout_ms: 25_000,
            });
            returned.send(view).unwrap();
        });
        handle.shutdown();
        match result.recv_timeout(Duration::from_secs(1)).unwrap() {
            Ok(view) => assert_eq!(view["revision"], revision),
            Err(error) => assert!(format!("{error:#}").contains("session is shutting down")),
        }
        waiting.join().unwrap();
    }

    #[test]
    fn camera_aliases_and_lifecycle_commands_share_a_gate_but_discovery_is_separate() {
        let handle = SessionHandle::new();
        handle
            .inner
            .state
            .write()
            .unwrap()
            .devices
            .push(CameraInfo {
                id: "camera-a".into(),
                transport: crate::types::Transport::GigE,
                vendor: "Test".into(),
                model: "Test".into(),
                serial: "serial-a".into(),
                address: Some("192.0.2.1".into()),
                profile: Default::default(),
            });
        let set = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let alias = handle
            .command_turn(Some("serial-a"), &width_command())
            .unwrap();
        let disconnect = handle
            .command_turn(Some("192.0.2.1"), &SessionCommand::Disconnect)
            .unwrap();
        let connect = handle
            .command_turn(
                None,
                &SessionCommand::Connect {
                    camera: "serial-a".into(),
                    timeout_ms: 1000,
                },
            )
            .unwrap();
        let select = handle
            .command_turn(
                None,
                &SessionCommand::Select {
                    camera: "camera-a".into(),
                },
            )
            .unwrap();
        for turn in [&alias, &disconnect, &connect, &select] {
            assert!(Arc::ptr_eq(&set.order, &turn.order));
        }
        let discover = SessionCommand::Discover {
            timeout_ms: 1000,
            simulated: true,
        };
        let first_discovery = handle.command_turn(None, &discover).unwrap();
        let second_discovery = handle.command_turn(None, &discover).unwrap();
        assert!(Arc::ptr_eq(&first_discovery.order, &second_discovery.order));
        assert!(!Arc::ptr_eq(&set.order, &first_discovery.order));
        assert!(handle.command_turn(None, &SessionCommand::Status).is_none());
        assert!(
            handle
                .command_turn(Some("serial-a"), &SessionCommand::Status)
                .is_none()
        );
    }

    fn ring_frame(id: u64) -> Frame {
        Frame {
            id,
            width: 4,
            height: 4,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![id as u8; 16],
        }
    }
    #[test]
    fn frame_feed_reuses_unshared_frames_and_never_mutates_shared_ones() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut feed = FrameFeed {
            ring: writer.reader().unwrap(),
            latest: None,
        };
        let frame = ring_frame;
        assert!(feed.poll(true).is_none());
        writer.write(&frame(1), false).unwrap();
        let held = feed.poll(true).unwrap();
        // A consumer still holds frame 1, so frame 2 must get its own buffer.
        writer.write(&frame(2), false).unwrap();
        let second = feed.poll(true).unwrap();
        assert_eq!((held.id, held.data.as_slice()), (1, &[1; 16][..]));
        assert_eq!((second.id, second.data.as_slice()), (2, &[2; 16][..]));
        assert_ne!(held.data.as_ptr(), second.data.as_ptr());
        let buffer = second.data.as_ptr();
        drop((held, second));
        // Once released, the cached allocation is overwritten in place.
        writer.write(&frame(3), false).unwrap();
        let third = feed.poll(true).unwrap();
        assert_eq!((third.id, third.data.as_ptr()), (3, buffer));
        drop(third);
        // Without a newer frame the cached one is returned unchanged.
        assert_eq!(feed.poll(true).unwrap().id, 3);
    }
    #[test]
    fn sequences_never_wait_for_a_frame_copy() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        writer.write(&ring_frame(1), false).unwrap();
        writer.write(&ring_frame(2), false).unwrap();
        let feed = handle.feed("cam").unwrap();
        // A frame copy holds this lock; sequence polls must not need it.
        let copying = locked(&feed.frames);
        assert_eq!(handle.frame_sequence(), 2);
        assert_eq!(handle.frame_sequence_of(&["cam", "absent"]), 2);
        assert_eq!(handle.frame_sequence_of(&["absent"]), 0);
        drop(copying);
        handle.shutdown();
    }
    #[test]
    fn fresh_frames_wait_for_an_idle_ring_to_publish() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        // Nobody watched: frame 1 was the keep-alive, 2 and 3 were skipped.
        for id in 1..=3 {
            writer.write(&ring_frame(id), false).unwrap();
        }
        let acquisition = std::thread::spawn(move || {
            for id in 4..=40 {
                std::thread::sleep(Duration::from_millis(5));
                writer.write(&ring_frame(id), false).unwrap();
            }
            writer
        });
        let fresh = handle
            .fresh_frame_for("cam", Duration::from_secs(5))
            .unwrap();
        assert!(fresh.id >= 4, "got keep-alive frame {}", fresh.id);
        // The sample asked for one frame; it did not watch the ring, so the
        // frames after it were neither copied nor counted as skipped.
        let mut writer = acquisition.join().unwrap();
        assert_eq!(writer.preview_skipped(), 0);
        assert_eq!(
            writer.write(&ring_frame(41), false).unwrap(),
            Published::Skipped
        );
        handle.shutdown();
    }
    #[test]
    fn fresh_frames_from_a_quiet_ring_are_its_held_frame() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        // Frames 2 and 3 were skipped, then no frame came: a triggered
        // camera between triggers. The worker held frame 3.
        for id in 1..=3 {
            if writer.write(&ring_frame(id), false).unwrap() == Published::Skipped {
                writer.hold(ring_frame(id));
            }
        }
        // The worker's loop flushes while it waits for frames.
        let worker = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            while writer.flush().unwrap() == Published::Skipped && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            writer
        });
        let fresh = handle
            .fresh_frame_for("cam", Duration::from_secs(5))
            .unwrap();
        assert_eq!(fresh.id, 3);
        let writer = worker.join().unwrap();
        assert_eq!(writer.preview_skipped(), 0);
        handle.shutdown();
    }
    #[test]
    fn stdout_captures_refuse_downscaled_previews() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        writer.write(&ring_frame(1), true).unwrap();
        assert_eq!(handle.captured_frame().unwrap().id, 1);
        let large = Frame {
            id: 2,
            width: 80,
            height: 80,
            data: vec![9; 6400],
            ..ring_frame(2)
        };
        writer.write(&large, true).unwrap();
        let error = handle.captured_frame().unwrap_err().to_string();
        assert!(
            error.contains("larger than the 4.0 KiB shared frame buffer"),
            "{error}"
        );
        // Views can still say what the camera delivers.
        assert_eq!(
            handle.preview_of("cam"),
            Some((80, 80, crate::types::MONO8))
        );
        handle.shutdown();
    }

    fn feature(name: &str, value: i64) -> FeatureInfo {
        FeatureInfo {
            name: name.into(),
            display_name: name.into(),
            kind: "Integer".into(),
            value: Some(json!(value)),
            writable: true,
            description: format!("The {name} of the image, in pixels."),
            unit: None,
            min: Some(16.0),
            max: Some(4096.0),
            inc: Some(16.0),
            choices: Vec::new(),
            error: None,
        }
    }
    fn timed(unix_ms: i64, message: &str) -> LogEntry {
        LogEntry {
            timestamp: LogTimestamp::new(unix_ms, 0),
            ..LogEntry::legacy("12:00:00", "info", message)
        }
    }
    /// Send `worker` on a pipe as `run_worker` does and merge the reply as
    /// the coordinator does; the reply as sent.
    fn relay(sent: &mut Sent, report: &mut Report, worker: &SessionSnapshot, full: bool) -> Value {
        if full {
            *sent = Sent::default();
        }
        let mut reply = worker.clone();
        let kept = sent.strip(&mut reply);
        let line = serde_json::to_vec(&WorkerReply {
            ok: true,
            result: None,
            error: None,
            snapshot: &reply,
            kept,
            saved: Default::default(),
        })
        .unwrap();
        let parsed: ParentReply = serde_json::from_slice(&line).unwrap();
        report.apply(parsed.snapshot.unwrap(), parsed.kept, full);
        serde_json::from_slice(&line).unwrap()
    }
    /// The worker snapshot the coordinator holds, put back together.
    fn merged(report: &Report) -> Value {
        let mut snapshot = report.snapshot.clone();
        snapshot.features = report.features.to_vec();
        snapshot.logs = report.log.raw.clone();
        serde_json::to_value(snapshot).unwrap()
    }
    #[test]
    fn status_deltas_rebuild_the_worker_snapshot_exactly() {
        let (mut sent, mut report) = (Sent::default(), Report::new());
        let mut worker = SessionSnapshot {
            connected: Some(crate::transport::simulator::info_named("sim:0")),
            features: vec![feature("Width", 640), feature("Height", 480)],
            frames: 10,
            logs: (0..3).map(|i| timed(i, &format!("line {i}"))).collect(),
            history: CameraHistoryStatus {
                undo: Some("Set Width".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let first = relay(&mut sent, &mut report, &worker, false);
        assert_eq!(first["snapshot"]["features"].as_array().unwrap().len(), 2);
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());

        // Only counters changed: the reply carries no features, history or lines.
        worker.frames = 20;
        let idle = relay(&mut sent, &mut report, &worker, false);
        assert_eq!(idle["snapshot"]["features"], json!([]));
        assert_eq!(idle["snapshot"]["logs"], json!([]));
        assert_eq!(idle["snapshot"]["history"]["undo"], Value::Null);
        assert_eq!(
            idle["kept"],
            json!({"features":true,"history":true,"logs":3})
        );
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());

        worker.features[0] = feature("Width", 320);
        worker.history.redo = Some("Set Height".into());
        relay(&mut sent, &mut report, &worker, false);
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());

        // The worker keeps its newest 200 lines; only new ones are sent.
        for i in 3..250 {
            worker.logs.push(timed(i, &format!("line {i}")));
            if worker.logs.len() > 200 {
                worker.logs.remove(0);
            }
            if i % 60 == 0 {
                let reply = relay(&mut sent, &mut report, &worker, false);
                assert!(reply["snapshot"]["logs"].as_array().unwrap().len() <= 60);
                assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());
            }
        }
        relay(&mut sent, &mut report, &worker, false);
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());
        assert_eq!(report.log.next, 250, "every line is numbered once");

        // A reply the coordinator could not read: it asks for everything,
        // and lines it already had keep their numbers.
        worker.features.push(feature("OffsetX", 0));
        worker.logs.push(timed(250, "missed"));
        worker.logs.remove(0);
        let mut lost = Sent::default();
        std::mem::swap(&mut lost, &mut sent);
        relay(&mut lost, &mut Report::new(), &worker, false);
        worker.logs.push(timed(251, "after"));
        worker.logs.remove(0);
        relay(&mut sent, &mut report, &worker, true);
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());
        assert_eq!(report.log.next, 252);
        assert!(!report.resync);

        // A worker that clears its features and log is followed too.
        worker.features.clear();
        worker.logs.clear();
        relay(&mut sent, &mut report, &worker, false);
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());
    }
    #[test]
    fn deltas_carry_skipped_shots_capture_notes_and_writability_read_on_stop() {
        // A job's skipped and retried shots and an auto capture note change
        // without anything else changing, and Stop re-reads writable flags
        // without walking the features: each reaches the coordinator.
        let (mut sent, mut report) = (Sent::default(), Report::new());
        let job = serde_json::from_value(json!({
            "id": 1, "status": "running", "output": "s-{seq}.png", "count": 10,
            "captured": 3, "first_at_ms": 0, "interval_ms": 1000, "next_at_ms": 3000,
            "last_file": null, "error": null
        }))
        .unwrap();
        let capture = serde_json::from_value(json!({
            "path": "line.duckdb", "camera": "sim:0", "enabled": true, "sensitivity": 0.5,
            "cooldown_ms": 250, "manual_count": 0, "kept_count": 0, "discarded_count": 0,
            "pending_count": 0, "blocked_storage": false, "state": "watching", "model": "native"
        }))
        .unwrap();
        let mut worker = SessionSnapshot {
            features: vec![feature("Width", 640)],
            jobs: vec![job],
            capture_session: Some(capture),
            ..Default::default()
        };
        worker.features[0].writable = false;
        relay(&mut sent, &mut report, &worker, false);
        worker.jobs[0].skipped = 2;
        worker.jobs[0].retries = 1;
        worker.jobs[0].next_at_ms = 6000;
        worker.capture_session.as_mut().unwrap().note = Some("Skipped a suggestion".into());
        let reply = relay(&mut sent, &mut report, &worker, false);
        assert_eq!(reply["snapshot"]["features"], json!([]), "unchanged");
        assert_eq!(reply["snapshot"]["jobs"][0]["skipped"], 2);
        assert_eq!(reply["snapshot"]["jobs"][0]["retries"], 1);
        assert_eq!(
            reply["snapshot"]["capture_session"]["note"],
            "Skipped a suggestion"
        );
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());
        worker.features[0].writable = true;
        let reply = relay(&mut sent, &mut report, &worker, false);
        assert_eq!(reply["snapshot"]["features"][0]["writable"], true);
        assert_eq!(merged(&report), serde_json::to_value(&worker).unwrap());
    }
    #[test]
    fn sim_camera_reports_survive_status_deltas() {
        let worker = crate::engine::WorkerHandle::new();
        let (mut sent, mut report, mut whole) = (Sent::default(), Report::new(), Report::new());
        let mut step = |command: Option<SessionCommand>| {
            if let Some(command) = command {
                let _ = worker.request(command);
            }
            let snapshot = worker.snapshot();
            let reply = relay(&mut sent, &mut report, &snapshot, false);
            // What every reply carried before deltas.
            whole.apply(snapshot.clone(), Kept::default(), false);
            assert_eq!(merged(&report), serde_json::to_value(&snapshot).unwrap());
            assert_eq!(
                serde_json::to_value(report.camera(7, 3)).unwrap(),
                serde_json::to_value(whole.camera(7, 3)).unwrap()
            );
            reply
        };
        let connected = step(Some(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 1000,
        }));
        assert!(
            !connected["snapshot"]["features"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let idle = step(None);
        assert_eq!(idle["snapshot"]["features"], json!([]), "{idle}");
        let idle = serde_json::to_vec(&idle).unwrap().len();
        let full = serde_json::to_vec(&connected).unwrap().len();
        assert!(
            idle * 2 < full,
            "an idle reply is {idle} bytes, a full one {full}"
        );
        step(Some(SessionCommand::Set {
            feature: "Width".into(),
            value: "320".into(),
        }));
        step(Some(SessionCommand::Set {
            feature: "NoSuchFeature".into(),
            value: "1".into(),
        }));
        step(Some(SessionCommand::Start));
        std::thread::sleep(Duration::from_millis(200));
        step(None);
        step(Some(SessionCommand::Stop));
        step(Some(SessionCommand::Undo));
        let camera = serde_json::to_value(report.camera(7, 3)).unwrap();
        let features: Vec<FeatureInfo> =
            serde_json::from_value(camera["features"].clone()).unwrap();
        assert_eq!(camera["features"], serde_json::to_value(&features).unwrap());
        worker.shutdown();
    }
    #[test]
    fn worker_requests_are_commands_refreshes_or_shutdown() {
        let request = |line: Value| WorkerRequest::read(line.to_string().as_bytes());
        assert!(matches!(
            request(json!({"shutdown":true})).unwrap(),
            WorkerRequest::Shutdown
        ));
        assert!(matches!(
            request(json!({"op":"status"})).unwrap(),
            WorkerRequest::Command(command) if matches!(*command, SessionCommand::Status)
        ));
        let refresh = Refresh {
            efficiency: Some(crate::efficiency::Mode::Efficient),
            full: true,
        };
        let WorkerRequest::Refresh(read) = request(json!({ "refresh": refresh })).unwrap() else {
            panic!("not a refresh")
        };
        assert_eq!(read, refresh);
        assert_eq!(
            serde_json::to_value(Refresh::default()).unwrap(),
            json!({}),
            "a plain refresh is just a status request"
        );
        assert!(request(json!({"refresh":{"full":true,"other":1}})).is_err());
        assert!(request(json!({"op":"status","full":true})).is_err());
    }
    #[test]
    fn the_session_log_keeps_the_newest_lines_in_time_order_without_reshuffling() {
        let camera = |id: &str, offset: i64| {
            let mut log = WorkerLog::new();
            log.camera = Some(id.into());
            log.update(
                0,
                (0..200)
                    .map(|i| timed(1_792_017_000_000 + i * 10 + offset, &format!("{id} {i}")))
                    .collect(),
            );
            log
        };
        let (mut a, b) = (camera("a", 0), camera("b", 5));
        let mut log = SessionLog::default();
        log.take(&a);
        log.take(&b);
        let first = log.lines();
        log.take(&a);
        log.take(&b);
        assert_eq!(log.lines(), first, "a second merge changes nothing");
        assert_eq!(first.len(), LOG_LINES);
        let times: Vec<_> = first
            .iter()
            .map(|line| line.timestamp.unwrap().unix_ms())
            .collect();
        assert!(times.is_sorted());
        assert_eq!(
            times[0],
            1_792_017_000_000 + 100 * 10,
            "the newest 200 of both"
        );
        assert!(first[0].message.starts_with("[a] a 100"));
        // A new line pushes out the oldest, which never comes back.
        a.update(200, vec![timed(1_792_017_002_000, "a new")]);
        log.take(&a);
        log.take(&b);
        let next = log.lines();
        assert_eq!(next.len(), LOG_LINES);
        assert_eq!(next[..LOG_LINES - 1], first[1..]);
        assert_eq!(next[LOG_LINES - 1].message, "[a] a new");
    }
    #[test]
    fn worker_log_lines_are_tagged_and_redacted_once() {
        let mut log = WorkerLog::new();
        log.camera = Some("cam".into());
        log.update(
            0,
            vec![timed(1, "rtsp://user:secret@example.test/live failed")],
        );
        log.update(
            1,
            vec![LogEntry {
                level: "error".into(),
                ..timed(2, "second")
            }],
        );
        assert_eq!(log.tagged.len(), 2);
        assert!(log.tagged[0].1.message.starts_with("[cam] "));
        assert!(!log.tagged[0].1.message.contains("secret"));
        assert_eq!(log.newest_error().as_deref(), Some("12:00:00"));
        log.update(1, Vec::new());
        assert_eq!(
            (log.raw.len(), log.tagged.len(), log.tagged[0].0),
            (1, 1, 1)
        );
    }
    #[test]
    fn workers_are_polled_often_only_while_watched_or_streaming() {
        assert_eq!(poll_pace(false, true, false), (PARKED, None));
        assert_eq!(poll_pace(true, true, false), (QUICK, Some(STATUS_POLL)));
        assert_eq!(
            poll_pace(true, false, false),
            (SLOW, Some(STATUS_POLL_IDLE))
        );
        assert_eq!(
            poll_pace(true, false, true),
            (SLOW, Some(Duration::from_secs(2)))
        );
        assert_eq!(
            poll_pace(true, true, true),
            (QUICK, Some(Duration::from_millis(500)))
        );
        let handle = SessionHandle::new();
        assert!(!handle.watching(), "nobody has looked yet");
        handle.request(SessionCommand::Status).unwrap();
        assert!(!handle.watching(), "a status request is not a view");
        handle.snapshot();
        assert!(handle.watching());
        assert_eq!(handle.next_poll(), None, "no worker: wait to be woken");
        assert_eq!(handle.inner.pace.load(Ordering::Relaxed), PARKED);
        handle.shutdown();
    }
    const TEST_WORKER: &str = "CAPTUREFAB_TEST_WORKER";
    /// This test binary as a camera worker, for `test_worker`.
    #[test]
    fn serve_as_test_worker() {
        if std::env::var_os(TEST_WORKER).is_none() {
            return;
        }
        println!("test worker ready");
        // `SharedRing::pass_to` adds the ring last.
        run_worker(&std::env::args_os().last().unwrap()).unwrap();
        println!("test worker efficient {}", crate::efficiency::efficient());
    }
    /// A camera worker run by this test binary, started with `efficiency`.
    #[cfg(unix)]
    fn test_worker(efficiency: crate::efficiency::Mode) -> Process {
        let ring = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut command = worker_command(std::env::current_exe().unwrap(), efficiency);
        command
            .args([
                "--exact",
                "session::tests::serve_as_test_worker",
                "--nocapture",
            ])
            .env(TEST_WORKER, "1");
        ring.pass_to(&mut command);
        let child = command.spawn().unwrap();
        let mut worker = Process::attach(child, ring, Arc::default(), efficiency).unwrap();
        // Past what the test harness prints first.
        let mut line = String::new();
        while !line.ends_with("test worker ready\n") {
            line.clear();
            let read = worker.output.read_line(&mut line).unwrap();
            assert_ne!(read, 0, "test worker did not start");
        }
        worker
    }
    #[test]
    #[cfg(unix)]
    fn a_new_worker_wakes_the_poller_whatever_pace_it_last_set() {
        let handle = SessionHandle::new();
        let reaches = |pace| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while handle.inner.pace.load(Ordering::Relaxed) != pace {
                if Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            true
        };
        assert!(reaches(PARKED), "no worker: the poller parks");
        // As if it had just seen no workers, but not yet said so.
        handle.inner.pace.store(QUICK, Ordering::Relaxed);
        let worker = test_worker(crate::efficiency::for_workers());
        handle.add_worker("cam".into(), worker);
        assert!(reaches(SLOW), "the poller polls the new worker");
        handle.shutdown();
    }
    #[test]
    #[cfg(unix)]
    fn running_workers_follow_the_coordinator_efficiency() {
        use crate::efficiency::Mode;
        use std::io::Read;
        // Started the other way, so only a refresh can bring it in line.
        let efficient = crate::efficiency::efficient();
        let started = if efficient {
            Mode::Smooth
        } else {
            Mode::Efficient
        };
        let handle = SessionHandle::new();
        handle.add_worker("cam".into(), test_worker(started));
        handle.refresh_workers();
        let worker = handle.inner.workers.write().unwrap().remove("cam").unwrap();
        let mut worker = worker.lock().unwrap();
        assert_eq!(worker.efficiency, crate::efficiency::for_workers());
        worker.input.write_all(b"{\"shutdown\":true}\n").unwrap();
        let mut rest = String::new();
        worker.output.read_to_string(&mut rest).unwrap();
        assert!(
            rest.contains(&format!("test worker efficient {efficient}\n")),
            "{rest}"
        );
        drop(worker);
        handle.shutdown();
    }
    #[test]
    fn revision_moves_only_when_the_session_changes() {
        let handle = SessionHandle::new();
        let start = handle.revision();
        handle.store_snapshot();
        handle.store_snapshot();
        assert_eq!(handle.revision(), start, "an idle poll changes nothing");
        let failed = handle.submit_to("camera-a", width_command()).unwrap();
        assert!(
            failed
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        let after = handle.revision();
        assert_ne!(after, start, "a failed command logs and records activity");
        handle.store_snapshot();
        assert_eq!(handle.revision(), after);
        handle.edit_state().active_camera = None;
        assert_ne!(handle.revision(), after);
        handle.shutdown();
    }
}
