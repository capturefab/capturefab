//! Multi-camera coordinator: one isolated process per camera, shared-memory pixels.
use crate::{
    genicam::FeatureInfo,
    shared_memory::{DEFAULT_CAPACITY, SharedRing},
    types::{CameraInfo, Frame},
};
use anyhow::{Context, Result, ensure};
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
    },
    Jobs,
    CancelJob {
        id: u64,
    },
    Status,
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
    },
    StopForward,
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
            Self::Discover { timeout_ms, .. } => validate_timeout(*timeout_ms)?,
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
            }
            | Self::Schedule {
                output,
                count,
                timeout_ms,
                format,
                storage,
                destination,
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
            } => {
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
            _ => {}
        }
        Ok(())
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
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
    pub dropped: u64,
    pub fps: f64,
    pub last_error: Option<String>,
    pub worker_pid: u32,
    #[serde(default)]
    pub forwarding: Option<String>,
    #[serde(default)]
    pub jobs: Vec<crate::scheduling::CaptureJob>,
    #[serde(default)]
    pub auto: Option<crate::auto::AutoStatus>,
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
    pub connected: Option<CameraInfo>,
    pub features: Vec<FeatureInfo>,
    pub streaming: bool,
    pub frames: u64,
    pub dropped: u64,
    pub fps: f64,
    pub last_error: Option<String>,
    pub logs: Vec<LogEntry>,
    #[serde(default)]
    pub cameras: Vec<CameraSnapshot>,
    #[serde(default)]
    pub active_camera: Option<String>,
    #[serde(default)]
    pub forwarding: Option<String>,
    #[serde(default)]
    pub jobs: Vec<crate::scheduling::CaptureJob>,
    #[serde(default)]
    pub auto: Option<crate::auto::AutoStatus>,
    #[serde(default)]
    pub transport: Option<crate::types::TransportStats>,
    #[serde(default)]
    pub history: CameraHistoryStatus,
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
}
struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    feed: Arc<Mutex<FrameFeed>>,
    snapshot: SessionSnapshot,
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
        let reply: ParentReply = serde_json::from_slice(&data)?;
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
    state: RwLock<SessionSnapshot>,
    workers: RwLock<BTreeMap<String, Arc<Mutex<Process>>>>,
    feeds: RwLock<BTreeMap<String, Arc<Mutex<FrameFeed>>>>,
    quitting: AtomicBool,
    pending: AtomicUsize,
    connecting: Mutex<()>,
    command_order: Mutex<BTreeMap<CommandTarget, Weak<CommandOrder>>>,
    poller: OnceLock<std::thread::Thread>,
    discovery_logs: Mutex<Vec<LogEntry>>,
}
impl Drop for Coordinator {
    fn drop(&mut self) {
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
                state: RwLock::new(SessionSnapshot::default()),
                workers: RwLock::new(BTreeMap::new()),
                feeds: RwLock::new(BTreeMap::new()),
                quitting: AtomicBool::new(false),
                pending: AtomicUsize::new(0),
                connecting: Mutex::new(()),
                command_order: Mutex::new(BTreeMap::new()),
                poller: OnceLock::new(),
                discovery_logs: Mutex::new(Vec::new()),
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
        for (_, w) in self.workers() {
            if let Ok(mut p) = w.try_lock() {
                let _ = p.exchange(SessionCommand::Status);
            }
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
                    let ring_dropped = u64::from(locked(&p.feed).ring.dropped());
                    let camera = CameraSnapshot {
                        info: info.clone(),
                        features: s.features.clone(),
                        streaming: s.streaming,
                        frames: s.frames,
                        dropped: s.dropped + ring_dropped,
                        fps: s.fps,
                        last_error: s.last_error.clone(),
                        worker_pid: p.child.id(),
                        forwarding: s.forwarding.clone(),
                        jobs: s.jobs.clone(),
                        auto: s.auto.clone(),
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
                    if state.active_camera.as_deref() == Some(&id) {
                        state.connected = s.connected.clone();
                        state.features = s.features.clone();
                        state.streaming = s.streaming;
                        state.frames = camera.frames;
                        state.dropped = camera.dropped;
                        state.fps = s.fps;
                        state.forwarding = s.forwarding.clone();
                        state.jobs = s.jobs.clone();
                        state.auto = s.auto.clone();
                        state.transport = s.transport.clone();
                        state.history = s.history.clone();
                        state.last_error = s.last_error.clone();
                        state.logs = s.logs.clone();
                    }
                    state.cameras.push(camera);
                }
            } else if let Some(c) = previous.cameras.iter().find(|c| c.info.id == id) {
                state.cameras.push(CameraSnapshot {
                    stale: true,
                    ..c.clone()
                });
            }
        }
        if state.active_camera.is_none() {
            state.connected = None;
            state.features.clear();
            state.streaming = false;
            state.forwarding = None;
            state.auto = None;
            state.transport = None;
            state.history = CameraHistoryStatus::default();
        }
        // Active worker snapshots replace their own logs. Keep discovery
        // diagnostics independently so connecting a camera cannot erase them.
        for entry in locked(&self.inner.discovery_logs).iter() {
            if !state.logs.iter().any(|existing| {
                existing.time == entry.time
                    && existing.level == entry.level
                    && existing.message == entry.message
            }) {
                state.logs.push(entry.clone());
            }
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
        self.submit_target(None, c)
    }
    pub fn submit_to(&self, camera: &str, c: SessionCommand) -> Result<Reply> {
        self.submit_target(Some(camera.to_owned()), c)
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
        let target = match command {
            SessionCommand::Discover { .. } => CommandTarget::Discovery,
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
    fn submit_target(&self, target: Option<String>, command: SessionCommand) -> Result<Reply> {
        command.validate()?;
        if let Some(camera) = &target {
            validate_selector(camera)?;
        }
        ensure!(
            !self.inner.quitting.load(Ordering::Relaxed),
            "session is shutting down"
        );
        let slot = PendingSlot::acquire(&self.inner)?;
        let target = target.or_else(|| {
            if matches!(
                &command,
                SessionCommand::Connect { .. }
                    | SessionCommand::Discover { .. }
                    | SessionCommand::Select { .. }
                    | SessionCommand::Status
            ) {
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
                let result = if h.inner.quitting.load(Ordering::Acquire) {
                    Err(anyhow::anyhow!("session is shutting down"))
                } else {
                    h.process(target.as_deref(), command)
                };
                if let Err(e) = &result {
                    let mut s = h.inner.state.write().unwrap_or_else(|e| e.into_inner());
                    s.last_error = Some(format!("{e:#}"));
                    s.logs.push(LogEntry {
                        time: "".into(),
                        level: "error".into(),
                        message: format!("{e:#}"),
                    });
                    if s.logs.len() > 200 {
                        s.logs.remove(0);
                    }
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
    fn process(&self, target: Option<&str>, command: SessionCommand) -> Result<Value> {
        match command {
            SessionCommand::Status if target.is_some() => {
                let w = self.target(target)?;
                w.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .call(SessionCommand::Status)
            }
            SessionCommand::Status => {
                self.refresh_workers();
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
                        logs.push(LogEntry {
                            time: chrono::Local::now().format("%H:%M:%S").to_string(),
                            level: "warn".into(),
                            message: warning.clone(),
                        });
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
                let mut p = w.lock().unwrap_or_else(|e| e.into_inner());
                let id = p
                    .snapshot
                    .connected
                    .as_ref()
                    .context("worker has no camera")?
                    .id
                    .clone();
                let result = p.call(SessionCommand::Disconnect)?;
                drop(p);
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
                let result = w.lock().unwrap_or_else(|e| e.into_inner()).call(command);
                self.store_snapshot();
                result
            }
        }
    }
    fn store_snapshot(&self) {
        let snapshot = self.snapshot();
        *self.inner.state.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
    }
    pub fn shutdown(&self) {
        if self.inner.quitting.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(poller) = self.inner.poller.get() {
            poller.unpark();
        }
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
    fn strict_json_commands_reject_ignored_or_duplicate_fields() {
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
        let addressed_status = handle
            .command_turn(Some("serial-a"), &SessionCommand::Status)
            .unwrap();
        assert!(Arc::ptr_eq(&set.order, &addressed_status.order));
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
