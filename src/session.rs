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
        Arc, Mutex, RwLock,
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
struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    ring: Arc<Mutex<SharedRing>>,
    cached: Option<Frame>,
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
            ring: Arc::new(Mutex::new(ring)),
            cached: None,
            snapshot: SessionSnapshot::default(),
        })
    }
    fn call(&mut self, command: SessionCommand) -> Result<Value> {
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
        let v: Value = serde_json::from_slice(&data)?;
        if let Some(snapshot) = v.get("snapshot") {
            match serde_json::from_value(snapshot.clone()) {
                Ok(snapshot) => self.snapshot = snapshot,
                Err(e) => eprintln!("capturefab: ignoring unreadable worker snapshot: {e}"),
            }
        }
        ensure!(
            v["ok"] == true,
            "{}",
            v["error"].as_str().unwrap_or("worker command failed")
        );
        Ok(v["result"].clone())
    }
    fn poll_frame(&mut self) {
        if let Ok(Some(frame)) = self
            .ring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_latest()
        {
            self.cached = Some(frame);
        }
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
    rings: RwLock<BTreeMap<String, Arc<Mutex<SharedRing>>>>,
    quitting: AtomicBool,
    pending: AtomicUsize,
    connecting: Mutex<()>,
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
                state: RwLock::new(SessionSnapshot::default()),
                workers: RwLock::new(BTreeMap::new()),
                rings: RwLock::new(BTreeMap::new()),
                quitting: AtomicBool::new(false),
                pending: AtomicUsize::new(0),
                connecting: Mutex::new(()),
            }),
        };
        let weak = Arc::downgrade(&h.inner);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(inner) = weak.upgrade() else { break };
                if inner.quitting.load(Ordering::Relaxed) {
                    break;
                }
                let h = Self { inner };
                for (_, w) in h.workers() {
                    if let Ok(mut p) = w.try_lock() {
                        let _ = p.call(SessionCommand::Status);
                    }
                }
                h.store_snapshot();
            }
        });
        h
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
                        dropped: s.dropped
                            + p.ring.lock().unwrap_or_else(|e| e.into_inner()).dropped() as u64,
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
    pub fn latest_frame(&self) -> Option<Frame> {
        let id = self.inner.state.read().ok()?.active_camera.clone()?;
        self.latest_frame_for(&id)
    }
    pub fn latest_frame_id(&self) -> Option<u64> {
        let id = self.inner.state.read().ok()?.active_camera.clone()?;
        self.latest_frame_id_for(&id)
    }
    pub fn latest_frame_for(&self, id: &str) -> Option<Frame> {
        let w = self.target(Some(id)).ok()?;
        let mut p = w.try_lock().ok()?;
        p.poll_frame();
        p.cached.clone()
    }
    pub fn latest_frame_id_for(&self, id: &str) -> Option<u64> {
        let w = self.target(Some(id)).ok()?;
        let mut p = w.try_lock().ok()?;
        p.poll_frame();
        p.cached.as_ref().map(|f| f.id)
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
        let old = self.inner.pending.fetch_add(1, Ordering::Relaxed);
        if old >= 32 {
            self.inner.pending.fetch_sub(1, Ordering::Relaxed);
            anyhow::bail!("camera command queue is full")
        }
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
        std::thread::spawn(move || {
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
            h.inner.pending.fetch_sub(1, Ordering::Relaxed);
            let _ = tx.send(result);
        });
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
                for (_, w) in self.workers() {
                    if let Ok(mut p) = w.try_lock() {
                        let _ = p.call(SessionCommand::Status);
                    }
                }
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
                self.inner
                    .state
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .devices = devices.clone();
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
                    .rings
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), p.ring.clone());
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
                    .rings
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
        if self.inner.quitting.swap(true, Ordering::Relaxed) {
            return;
        }
        for ring in self
            .inner
            .rings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            ring.lock().unwrap_or_else(|e| e.into_inner()).cancel();
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
            .rings
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
                Ok(serde_json::to_value(handle.snapshot())?)
            } else {
                handle.request(command)
            };
            let response = match result {
                Ok(v) => json!({"ok":true,"result":v,"snapshot":handle.snapshot()}),
                Err(e) => json!({"ok":false,"error":format!("{e:#}"),"snapshot":handle.snapshot()}),
            };
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok(())
    })();
    handle.shutdown();
    result
}
