//! Multi-camera coordinator: one isolated process per camera, shared-memory pixels.
mod activity;
mod log;
use crate::{
    genicam::FeatureInfo,
    shared_memory::{DEFAULT_CAPACITY, SharedRing},
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
            Self::CaptureSessionOpen { path, .. } => {
                ensure!(
                    !path.trim().is_empty()
                        && !path.chars().any(char::is_control)
                        && path != ":memory:",
                    "capture session requires a database file path"
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
fn append_log(logs: &mut Vec<LogEntry>, entry: LogEntry) {
    if !logs.iter().any(|existing| existing.same_event(&entry)) {
        logs.push(entry);
    }
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
    pub features: Vec<FeatureInfo>,
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
    /// Frames the shared ring replaced before this process read them, which
    /// `dropped` includes. Kept for this process's own views; not serialized.
    #[serde(skip)]
    pub ring_dropped: u64,
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
        state.features = camera.features.clone();
        state.streaming = camera.streaming;
        state.frames = camera.frames;
        state.received_bytes = camera.received_bytes;
        state.dropped = camera.dropped;
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
    let newer_logs = std::mem::replace(&mut snapshot.logs, current.logs.clone());
    for entry in newer_logs {
        append_log(&mut snapshot.logs, entry);
    }
    let excess = snapshot.logs.len().saturating_sub(200);
    snapshot.logs.drain(..excess);
    project_selected(snapshot);
}
/// A camera-scoped status keeps that camera's feature/capture fields while
/// including the same session-wide context as an unaddressed status request.
fn addressed_status(
    mut camera_state: Value,
    snapshot: SessionSnapshot,
    target_camera: &str,
) -> Result<Value> {
    let fields = camera_state
        .as_object_mut()
        .context("worker status is not an object")?;
    fields.insert("target_camera".into(), json!(target_camera));
    fields.insert("active_camera".into(), json!(snapshot.active_camera));
    fields.insert("cameras".into(), serde_json::to_value(snapshot.cameras)?);
    fields.insert("devices".into(), serde_json::to_value(snapshot.devices)?);
    fields.insert(
        "network_scan".into(),
        serde_json::to_value(snapshot.network_scan)?,
    );
    fields.insert("activity".into(), serde_json::to_value(snapshot.activity)?);
    fields.insert(
        "activity_revision".into(),
        json!(snapshot.activity_revision),
    );
    fields.insert("logs".into(), serde_json::to_value(snapshot.logs)?);
    fields.insert("dataset".into(), serde_json::to_value(snapshot.dataset)?);
    if !snapshot.signals.is_empty() {
        fields.insert("signals".into(), serde_json::to_value(snapshot.signals)?);
    }
    Ok(camera_state)
}
type Reply = Receiver<Result<Value>>;
/// Newest-frame cache fed by a worker's shared ring. It has its own lock so
/// previews never wait behind a slow command on the worker's stdio pipe.
/// Lock order: a `Process` lock may be held while taking a feed lock, never
/// the reverse.
struct FrameFeed {
    ring: SharedRing,
    latest: Option<Arc<Frame>>,
}
impl FrameFeed {
    fn poll(&mut self) -> Option<Arc<Frame>> {
        // Overwrite the cached frame in place when no consumer still holds it,
        // so steady-state streaming reuses one pixel allocation.
        let updated = match self.latest.as_mut().and_then(Arc::get_mut) {
            Some(frame) => self.ring.take_latest_into(frame),
            None => {
                let mut frame = Frame::default();
                let updated = self.ring.take_latest_into(&mut frame);
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
    /// Files saved since the previous reply, for the coordinator's dataset.
    #[serde(skip_serializing_if = "crate::dataset::SavedBatch::is_empty")]
    saved: crate::dataset::SavedBatch,
}
#[derive(Deserialize)]
struct ParentReply {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    snapshot: Option<Value>,
    #[serde(default)]
    saved: crate::dataset::SavedBatch,
}
/// Saved files a worker reported more than this many of are counted as lost.
const SAVED_CAP: usize = 100_000;
struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    feed: Arc<Mutex<FrameFeed>>,
    snapshot: SessionSnapshot,
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

impl Process {
    fn spawn(scanner: bool) -> Result<Self> {
        let capacity = frame_capacity(scanner)?;
        let dir = crate::ipc::session_dir();
        crate::ipc::ensure_private_dir(&dir)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random ring name: {e}"))?;
        let name = random
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let path = dir.join(format!("worker-{name}.shm"));
        let ring = SharedRing::create(&path, capacity)?;
        let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE")
            .map(std::path::PathBuf::from)
            .unwrap_or(std::env::current_exe()?);
        let mut child = std::process::Command::new(executable)
            .env("CAPTUREFAB_HWACCEL", crate::media::hwaccel())
            .arg("__worker")
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("launch camera capture process")?;
        let input = child.stdin.take().context("worker stdin")?;
        let output = BufReader::new(child.stdout.take().context("worker stdout")?);
        Ok(Self {
            child,
            input,
            output,
            feed: Arc::new(Mutex::new(FrameFeed { ring, latest: None })),
            snapshot: SessionSnapshot::default(),
            saved: Vec::new(),
            saved_camera: None,
            saved_seq: 0,
            saved_gaps: 0,
        })
    }
    fn call(&mut self, command: SessionCommand) -> Result<Value> {
        let status = matches!(command, SessionCommand::Status);
        match self.exchange(command)? {
            None if status => Ok(serde_json::to_value(&self.snapshot)?),
            result => Ok(result.unwrap_or_default()),
        }
    }
    /// Send one command and apply the reply's snapshot. Status replies carry no
    /// separate result, so a background refresh never serializes it twice.
    fn exchange(&mut self, command: SessionCommand) -> Result<Option<Value>> {
        ensure!(
            self.child.try_wait()?.is_none(),
            "camera worker exited; disconnect and reconnect"
        );
        serde_json::to_writer(&mut self.input, &command)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let mut data = Vec::new();
        use std::io::Read;
        self.output
            .by_ref()
            .take(16 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut data)?;
        ensure!(
            data.len() <= 16 * 1024 * 1024 && data.last() == Some(&b'\n'),
            "camera worker exited or sent an invalid response"
        );
        let mut reply: ParentReply = serde_json::from_slice(&data)?;
        // Before the ok check: a failed command may already have saved files.
        self.keep_saved(std::mem::take(&mut reply.saved));
        if let Some(snapshot) = reply.snapshot {
            match serde_json::from_value(snapshot) {
                Ok(snapshot) => self.snapshot = snapshot,
                Err(e) => eprintln!("capturefab: ignoring unreadable worker snapshot: {e}"),
            }
        }
        ensure!(
            reply.ok,
            "{}",
            reply.error.as_deref().unwrap_or("worker command failed")
        );
        Ok(reply.result)
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
                .or_else(|| self.snapshot.connected.as_ref().map(Into::into)),
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
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
    feeds: RwLock<BTreeMap<String, Arc<Mutex<FrameFeed>>>>,
    quitting: AtomicBool,
    /// Set when `shutdown` has finished; later callers wait for it.
    shut_down: (Mutex<bool>, Condvar),
    pending: AtomicUsize,
    connecting: Mutex<()>,
    command_order: Mutex<BTreeMap<CommandTarget, Weak<CommandOrder>>>,
    poller: OnceLock<std::thread::Thread>,
    discovery_logs: Mutex<Vec<LogEntry>>,
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
const STATUS_POLL: Duration = Duration::from_millis(250);
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
                discovery_logs: Mutex::new(Vec::new()),
                activity: Arc::new(activity::Activity::default()),
                dataset: Arc::new(crate::dataset::Slot::new()),
                signals: crate::signals::hub::SignalHub::new(),
            }),
        };
        let weak = Arc::downgrade(&h.inner);
        let poller = std::thread::Builder::new()
            .name("capturefab-status".into())
            .spawn(move || {
                loop {
                    // Parked rather than slept so shutdown and drop wake it at once.
                    std::thread::park_timeout(STATUS_POLL);
                    let Some(inner) = weak.upgrade() else { break };
                    if inner.quitting.load(Ordering::Acquire) {
                        break;
                    }
                    let h = Self { inner };
                    h.refresh_workers();
                    h.store_snapshot();
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
    /// Refresh every idle worker's snapshot; busy workers report on their reply.
    fn refresh_workers(&self) {
        for (id, w) in self.workers() {
            let Ok(mut p) = w.try_lock() else { continue };
            if p.exchange(SessionCommand::Status).is_err()
                && p.child.try_wait().ok().flatten().is_some()
            {
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
    fn workers(&self) -> Vec<(String, Arc<Mutex<Process>>)> {
        self.inner
            .workers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    pub fn snapshot(&self) -> SessionSnapshot {
        let previous = self
            .inner
            .state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut state = previous.clone();
        state.cameras.clear();
        for (id, w) in self.workers() {
            if let Ok(p) = w.try_lock() {
                let s = &p.snapshot;
                if let Some(info) = &s.connected {
                    // Preserve diagnostics from every camera, including those
                    // that are not selected and cameras later disconnected.
                    for entry in &s.logs {
                        append_log(&mut state.logs, camera_log(entry, &info.id));
                    }
                    let ring_dropped = u64::from(locked(&p.feed).ring.dropped());
                    let camera = CameraSnapshot {
                        info: info.clone(),
                        features: s.features.clone(),
                        streaming: s.streaming,
                        frames: s.frames,
                        received_bytes: s.received_bytes,
                        dropped: s.dropped + ring_dropped,
                        fps: s.fps,
                        last_error: s.last_error.clone(),
                        worker_pid: p.child.id(),
                        forwarding: s.forwarding.clone(),
                        forwarding_since_ms: s.forwarding_since_ms,
                        jobs: s.jobs.clone(),
                        auto: s.auto.clone(),
                        capture_session: s.capture_session.clone(),
                        transport: s.transport.clone(),
                        history: s.history.clone(),
                        ring_dropped,
                        stale: false,
                        error_logged: s
                            .logs
                            .iter()
                            .rev()
                            .find(|entry| entry.level == "error")
                            .map(|entry| entry.time.clone()),
                    };
                    state.cameras.push(camera);
                }
            } else if let Some(c) = previous.cameras.iter().find(|c| c.info.id == id) {
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
        // Coordinator diagnostics survive worker snapshot refreshes as well.
        for entry in locked(&self.inner.discovery_logs).iter() {
            append_log(&mut state.logs, entry.clone());
        }
        for entry in self.inner.signals.logs() {
            append_log(&mut state.logs, entry);
        }
        let excess = state.logs.len().saturating_sub(200);
        state.logs.drain(..excess);
        state
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
                && p.snapshot
                    .connected
                    .as_ref()
                    .is_some_and(|c| c.serial == id || c.address.as_deref() == Some(&id))
            {
                return Ok(w.clone());
            }
        }
        anyhow::bail!("camera '{id}' not connected in this session")
    }
    pub fn latest_frame(&self) -> Option<Arc<Frame>> {
        let id = self.inner.state.read().ok()?.active_camera.clone()?;
        self.latest_frame_for(&id)
    }
    /// Newest frame from a camera's shared ring. Never waits for the worker's
    /// command pipe, so previews keep updating while commands run.
    pub fn latest_frame_for(&self, id: &str) -> Option<Arc<Frame>> {
        let feed = self.feed(id)?;
        locked(&feed).poll()
    }
    /// Changes whenever any camera publishes a frame. Reads each shared ring's
    /// sequence counter without copying pixels, so it is cheap to poll.
    pub fn frame_sequence(&self) -> u64 {
        self.inner
            .feeds
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|feed| u64::from(locked(feed).ring.sequence()))
            .fold(0, u64::wrapping_add)
    }
    fn feed(&self, id: &str) -> Option<Arc<Mutex<FrameFeed>>> {
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
    pub fn activity(&self) -> ActivityView {
        self.inner.activity.snapshot()
    }
    pub fn activity_revision(&self) -> u64 {
        self.inner.activity.revision()
    }
    pub fn wait_for_activity(&self, after: u64, timeout: Duration) -> u64 {
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
                    let mut s = h.inner.state.write().unwrap_or_else(|e| e.into_inner());
                    let message = activity::redact(&format!("{e:#}"));
                    s.last_error = Some(message.clone());
                    drop(s);
                    let mut logs = locked(&h.inner.discovery_logs);
                    logs.push(LogEntry::now_local_millis("error", message));
                    if logs.len() > 200 {
                        logs.remove(0);
                    }
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
        let before = self.snapshot();
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
                                let current = self.snapshot();
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
        let after = self.snapshot();
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
            let mut state = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
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
            SessionCommand::Status if target.is_some() => {
                self.target(target)?;
                let camera = self.camera_id(target.unwrap());
                self.store_snapshot();
                let shared = self.snapshot();
                let mut view = shared.clone();
                view.select_view(Some(&camera));
                addressed_status(serde_json::to_value(view)?, shared, &camera)
            }
            SessionCommand::Status => {
                self.store_snapshot();
                Ok(serde_json::to_value(self.snapshot())?)
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
                {
                    let mut state = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
                    state.devices = devices.clone();
                    let mut logs = locked(&self.inner.discovery_logs);
                    for warning in &warnings {
                        logs.push(LogEntry::now_local("warn", warning.clone()));
                    }
                    let excess = logs.len().saturating_sub(200);
                    logs.drain(..excess);
                }
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
                        .snapshot
                        .connected
                        .clone()
                        .context("worker has no camera")?;
                    self.inner
                        .state
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .active_camera = Some(info.id.clone());
                    return Ok(json!({"connected":info,"worker_pid":p.child.id()}));
                }
                crate::camera::validate_selector(&camera)?;
                let maximum = maximum_cameras()?;
                ensure!(
                    self.workers().len() < maximum,
                    "camera limit reached ({maximum}); disconnect a camera or increase CAPTUREFAB_MAX_CAMERAS"
                );
                let scanner = crate::scanner::is_source(&camera)
                    || self.snapshot().devices.iter().any(|device| {
                        device.transport == crate::types::Transport::Scanner
                            && (device.id == camera || device.serial == camera)
                    });
                let mut p = Process::spawn(scanner)?;
                let result = p.call(SessionCommand::Connect { camera, timeout_ms })?;
                let info = p
                    .snapshot
                    .connected
                    .clone()
                    .context("worker connected without identity")?;
                let id = info.id.clone();
                let pid = p.child.id();
                self.inner
                    .feeds
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), p.feed.clone());
                self.inner
                    .workers
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), Arc::new(Mutex::new(p)));
                self.inner
                    .state
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .active_camera = Some(id);
                self.store_snapshot();
                Ok(json!({"connected":result["connected"],"worker_pid":pid}))
            }
            SessionCommand::Select { camera } => {
                let w = self.target(Some(&camera))?;
                let p = w.lock().unwrap_or_else(|e| e.into_inner());
                let id = p
                    .snapshot
                    .connected
                    .as_ref()
                    .context("worker has no camera")?
                    .id
                    .clone();
                drop(p);
                self.inner
                    .state
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .active_camera = Some(id);
                self.store_snapshot();
                Ok(json!({"active_camera":self.snapshot().active_camera}))
            }
            SessionCommand::Disconnect => {
                let w = self.target(target)?;
                let p = w.lock().unwrap_or_else(|e| e.into_inner());
                let id = p
                    .snapshot
                    .connected
                    .as_ref()
                    .context("worker has no camera")?
                    .id
                    .clone();
                drop(p);
                let mut final_snapshot = self.snapshot();
                let mut p = w.lock().unwrap_or_else(|e| e.into_inner());
                let result = p.call(SessionCommand::Disconnect);
                // The worker exits once dropped; collect what it still holds,
                // keeping the disconnect reply's final counters.
                let disconnected = p.snapshot.clone();
                p.drain_saved();
                p.snapshot = disconnected;
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
                    camera.frames = p.snapshot.frames;
                    camera.received_bytes = p.snapshot.received_bytes;
                    camera.ring_dropped = u64::from(locked(&p.feed).ring.dropped());
                    camera.dropped = p.snapshot.dropped.saturating_add(camera.ring_dropped);
                    camera.streaming = false;
                    camera.fps = 0.0;
                    camera.stale = false;
                }
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
                let mut s = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
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
        let mut snapshot = self.snapshot();
        self.inner.analytics.observe(&snapshot);
        let mut state = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
        merge_session_metadata(&mut snapshot, &state);
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
            if p.snapshot.forwarding.is_some()
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
        if let Some(poller) = self.inner.poller.get() {
            poller.unpark();
        }
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
            locked(feed).ring.cancel();
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

/// Hidden mode: stdio controls, separate publisher, mmap frame transfer.
pub fn run_worker(path: &std::path::Path) -> Result<()> {
    let ring = Arc::new(Mutex::new(SharedRing::open(path)?));
    let handle = crate::engine::WorkerHandle::with_ring(ring);
    let result = (|| -> Result<()> {
        let input = std::io::stdin();
        let mut output = std::io::stdout().lock();
        let mut reader = input.lock();
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
            if serde_json::from_slice::<Value>(&data)
                .ok()
                .is_some_and(|v| v["shutdown"] == true)
            {
                break;
            }
            let command = SessionCommand::from_json_slice(&data)?;
            let result = if matches!(command, SessionCommand::Status) {
                Ok(None)
            } else {
                handle.request(command).map(Some)
            };
            // One snapshot clone per reply, serialized straight to the pipe; the
            // clone keeps the state lock out of the blocking write.
            let snapshot = handle.snapshot();
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
mod tests {
    use super::*;
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
            features: Vec::new(),
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
            ring_dropped: 0,
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
            logs: vec![LogEntry {
                timestamp: None,
                time: "12:00:01".into(),
                level: "error".into(),
                message: "client failure".into(),
            }],
            ..Default::default()
        };
        merge_session_metadata(&mut in_flight, &current);
        assert_eq!(in_flight.active_camera.as_deref(), Some("sim:1"));
        assert_eq!(in_flight.connected.as_ref().unwrap().id, "sim:1");
        assert_eq!(in_flight.devices[0].id, "sim:2");
        assert_eq!(in_flight.logs[0].message, "client failure");
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
        let mut current = SessionSnapshot::default();
        append_log(&mut current.logs, tagged.clone());
        append_log(&mut current.logs, tagged.clone());
        append_log(&mut current.logs, later.clone());
        assert_eq!(
            current.logs.len(),
            2,
            "same display clocks on different dates remain distinct"
        );
        let mut in_flight = SessionSnapshot {
            logs: vec![tagged],
            ..Default::default()
        };
        merge_session_metadata(&mut in_flight, &current);
        assert_eq!(in_flight.logs.len(), 2);
        assert_eq!(in_flight.logs[1].timestamp, later.timestamp);
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
            context,
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

    #[test]
    fn frame_feed_reuses_unshared_frames_and_never_mutates_shared_ones() {
        let path = std::env::temp_dir().join(format!(
            "capturefab-feed-test-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut writer = SharedRing::create(&path, 4096).unwrap();
        let mut feed = FrameFeed {
            ring: SharedRing::open(&path).unwrap(),
            latest: None,
        };
        let frame = |id: u64| Frame {
            id,
            width: 4,
            height: 4,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![id as u8; 16],
        };
        assert!(feed.poll().is_none());
        writer.write(&frame(1)).unwrap();
        let held = feed.poll().unwrap();
        // A consumer still holds frame 1, so frame 2 must get its own buffer.
        writer.write(&frame(2)).unwrap();
        let second = feed.poll().unwrap();
        assert_eq!((held.id, held.data.as_slice()), (1, &[1; 16][..]));
        assert_eq!((second.id, second.data.as_slice()), (2, &[2; 16][..]));
        assert_ne!(held.data.as_ptr(), second.data.as_ptr());
        let buffer = second.data.as_ptr();
        drop((held, second));
        // Once released, the cached allocation is overwritten in place.
        writer.write(&frame(3)).unwrap();
        let third = feed.poll().unwrap();
        assert_eq!((third.id, third.data.as_ptr()), (3, buffer));
        drop(third);
        // Without a newer frame the cached one is returned unchanged.
        assert_eq!(feed.poll().unwrap().id, 3);
    }
}
