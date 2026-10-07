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
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::{
        Arc, Mutex, OnceLock, PoisonError, RwLock,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
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
    fn spawn() -> Result<Self> {
        let dir = crate::ipc::session_dir();
        crate::ipc::ensure_private_dir(&dir)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random ring name: {e}"))?;
        let name = random
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let path = dir.join(format!("worker-{name}.shm"));
        let capacity = std::env::var("CAPTUREFAB_FRAME_BYTES")
            .ok()
            .map(|s| s.parse::<usize>())
            .transpose()
            .context("CAPTUREFAB_FRAME_BYTES must be a number")?
            .unwrap_or(DEFAULT_CAPACITY);
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
                    let camera = CameraSnapshot {
                        info: info.clone(),
                        features: s.features.clone(),
                        streaming: s.streaming,
                        frames: s.frames,
                        dropped: s.dropped + locked(&p.feed).ring.dropped() as u64,
                        fps: s.fps,
                        last_error: s.last_error.clone(),
                        worker_pid: p.child.id(),
                        forwarding: s.forwarding.clone(),
                        jobs: s.jobs.clone(),
                        auto: s.auto.clone(),
                        transport: s.transport.clone(),
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
                        state.last_error = s.last_error.clone();
                        state.logs = s.logs.clone();
                    }
                    state.cameras.push(camera);
                }
            } else if let Some(c) = previous.cameras.iter().find(|c| c.info.id == id) {
                state.cameras.push(c.clone());
            }
        }
        if state.active_camera.is_none() {
            state.connected = None;
            state.features.clear();
            state.streaming = false;
            state.forwarding = None;
            state.auto = None;
            state.transport = None;
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
    fn submit_target(&self, target: Option<String>, command: SessionCommand) -> Result<Reply> {
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
        let h = self.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("capturefab-command".into())
            .spawn(move || {
                let _slot = slot;
                let result = h.process(target.as_deref(), command);
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
                let maximum = std::env::var("CAPTUREFAB_MAX_CAMERAS")
                    .ok()
                    .map(|s| s.parse::<usize>())
                    .transpose()
                    .context("CAPTUREFAB_MAX_CAMERAS must be a number")?
                    .unwrap_or(16);
                ensure!(
                    (1..=64).contains(&maximum),
                    "CAPTUREFAB_MAX_CAMERAS must be 1..64"
                );
                ensure!(
                    self.workers().len() < maximum,
                    "camera limit reached ({maximum}); disconnect a camera or increase CAPTUREFAB_MAX_CAMERAS"
                );
                let mut p = Process::spawn()?;
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
            let command: SessionCommand = serde_json::from_slice(&data)?;
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
