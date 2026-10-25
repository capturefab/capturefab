//! A single camera owner shared by the UI and authenticated local RPC clients.
use crate::{
    auto::AutoController,
    camera::Camera,
    media::{ForwardConfig, Forwarder, StreamEnd},
    shared_memory::Published,
    types::Frame,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

use crate::session::{FeatureValue, ForwardingProgress, LogEntry, SessionCommand, SessionSnapshot};
mod history;
mod network_defaults;
#[cfg(test)]
mod network_tests;
mod saver;
use history::process;
type Reply = Receiver<Result<Value>>;
struct Envelope {
    command: SessionCommand,
    reply: mpsc::Sender<Result<Value>>,
}
/// Per-frame statistics, updated lock-free on the acquisition hot path and
/// merged into snapshots on read.
#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    received_bytes: AtomicU64,
    dropped: AtomicU64,
    fps_bits: AtomicU64,
}
impl Counters {
    fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.received_bytes.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.set_fps(0.0);
    }
    fn set_fps(&self, fps: f64) {
        self.fps_bits.store(fps.to_bits(), Ordering::Relaxed);
    }
}

/// A timed forward keeps elapsed time already credited by the wall clock.
/// Forward jumps (including sleep) count; a later rollback cannot take that
/// time back or prolong the recording beyond its remaining duration.
#[derive(Clone, Debug)]
struct ForwardClock {
    forwarding_id: String,
    anchor_ms: u64,
    anchor: Instant,
}
impl ForwardClock {
    fn now_ms(&mut self, wall_ms: u64, now: Instant) -> u64 {
        let elapsed_ms = u64::try_from(now.saturating_duration_since(self.anchor).as_millis())
            .unwrap_or(u64::MAX);
        let floor = self.anchor_ms.saturating_add(elapsed_ms);
        if wall_ms > floor {
            self.anchor_ms = wall_ms;
            self.anchor = now;
            wall_ms
        } else {
            floor
        }
    }
}

/// A stale caller's rejected stop did not fail the camera or its current
/// recording. Keep the refusal distinct from encoder stop/commit failures.
#[derive(Debug)]
struct ForwardChanged;
impl std::fmt::Display for ForwardChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the recording changed before it could be stopped; the current recording was preserved",
        )
    }
}
impl std::error::Error for ForwardChanged {}

#[derive(Clone)]
pub struct WorkerHandle {
    /// Commands for the camera thread; `None` only wakes it.
    sender: SyncSender<Option<Envelope>>,
    state: Arc<RwLock<SessionSnapshot>>,
    counters: Arc<Counters>,
    frame: Arc<RwLock<Option<Frame>>>,
    quitting: Arc<AtomicBool>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    ring: Option<Arc<Mutex<crate::shared_memory::SharedRing>>>,
    /// The shared-ring problem last logged, so one that every frame of a
    /// stream hits is logged once.
    ring_note: Arc<Mutex<Option<String>>>,
    history: Arc<Mutex<history::Journal>>,
    /// Files saved since the coordinator last collected them.
    saved: Arc<Mutex<crate::dataset::SavedLog>>,
    /// The local recording in progress, reported when it stops.
    recording: Arc<Mutex<Option<crate::scheduling::RecordingMeta>>>,
    /// Private timing state, bound to the exact worker-issued forward ID.
    forward_clock: Arc<Mutex<Option<ForwardClock>>>,
    /// Output numbers handed out, so concurrent captures never share a name.
    names: Arc<Mutex<crate::naming::Namer>>,
    /// Saved camera sign-ins, read when a camera opens.
    logins: crate::logins::Logins,
}
impl Default for WorkerHandle {
    fn default() -> Self {
        Self::new()
    }
}
impl WorkerHandle {
    pub fn new() -> Self {
        Self::create(None, crate::logins::Logins::system())
    }
    pub fn with_ring(ring: Arc<Mutex<crate::shared_memory::SharedRing>>) -> Self {
        Self::create(Some(ring), crate::logins::Logins::system())
    }
    #[cfg(all(test, feature = "keychain"))]
    fn with_logins(logins: crate::logins::Logins) -> Self {
        Self::create(None, logins)
    }
    fn create(
        ring: Option<Arc<Mutex<crate::shared_memory::SharedRing>>>,
        logins: crate::logins::Logins,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(32);
        let handle = Self {
            sender,
            state: Arc::new(RwLock::new(SessionSnapshot::default())),
            counters: Arc::default(),
            frame: Arc::new(RwLock::new(None)),
            quitting: Arc::new(AtomicBool::new(false)),
            thread: Arc::new(Mutex::new(None)),
            ring,
            ring_note: Arc::default(),
            history: Arc::default(),
            saved: Arc::default(),
            recording: Arc::default(),
            forward_clock: Arc::default(),
            names: Arc::default(),
            logins,
        };
        let worker_handle = handle.clone();
        let thread = std::thread::Builder::new()
            .name("capturefab-camera".into())
            .spawn(move || worker(worker_handle, receiver))
            .expect("spawn camera worker");
        *handle.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread);
        handle
    }
    pub fn snapshot(&self) -> SessionSnapshot {
        let mut s = self.state.read().unwrap_or_else(|e| e.into_inner()).clone();
        s.frames = self.counters.frames.load(Ordering::Relaxed);
        s.received_bytes = self.counters.received_bytes.load(Ordering::Relaxed);
        s.dropped = self.counters.dropped.load(Ordering::Relaxed);
        s.fps = f64::from_bits(self.counters.fps_bits.load(Ordering::Relaxed));
        s
    }
    /// Report a finished file to the coordinator's dataset; stamps its `seq`.
    pub(crate) fn push_saved(&self, file: crate::dataset::SavedFile) {
        self.saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(file);
    }
    /// Drain up to `max` saved files, with the camera that saved them.
    pub(crate) fn take_saved(&self, max: usize) -> crate::dataset::SavedBatch {
        let (items, lost) = self
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take(max);
        if items.is_empty() && lost == 0 {
            return crate::dataset::SavedBatch::default();
        }
        let camera = self
            .state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .connected
            .as_ref()
            .map(crate::dataset::SavedCamera::from);
        crate::dataset::SavedBatch {
            camera,
            items,
            lost,
        }
    }
    #[cfg(test)]
    pub fn latest_frame(&self) -> Option<Frame> {
        self.frame.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn submit(&self, command: SessionCommand) -> Result<Reply> {
        command.validate()?;
        ensure!(
            !self.quitting.load(Ordering::Relaxed),
            "session is shutting down"
        );
        let (reply, rx) = mpsc::channel();
        self.sender
            .try_send(Some(Envelope { command, reply }))
            .context("camera command queue is full or closed")?;
        Ok(rx)
    }
    pub fn request(&self, command: SessionCommand) -> Result<Value> {
        self.submit(command)?
            .recv()
            .context("camera worker stopped")?
    }
    pub fn shutdown(&self) {
        self.quitting.store(true, Ordering::Relaxed);
        self.wake();
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
    /// Have the camera thread look at its jobs and the quit flag now rather
    /// than when its wait ends. A full queue wakes it anyway.
    fn wake(&self) {
        let _ = self.sender.try_send(None);
    }
    fn cancelled(&self) -> bool {
        self.quitting.load(Ordering::Relaxed)
            || self
                .ring
                .as_ref()
                .is_some_and(|r| r.lock().unwrap_or_else(|e| e.into_inner()).cancelled())
    }
    fn log(&self, level: &str, message: String) {
        push_log(
            &mut self.state.write().unwrap_or_else(|e| e.into_inner()),
            level,
            message,
        );
    }
    /// Publish the latest frame: offer it to the shared ring, which holds a
    /// frame it skipped for a reader that comes later, or without a ring
    /// retain it here. With `forward` the frame is handed back, so the
    /// forwarder takes its buffer.
    fn record(&self, frame: Frame, forward: bool) -> Option<Frame> {
        self.publish(frame, false, forward)
    }
    /// Publish a frame a command captured. It always reaches the ring, so a
    /// reader such as `capture -o -` gets this frame, not a keep-alive.
    fn record_capture(&self, frame: Frame) {
        self.publish(frame, true, false);
    }
    fn publish(&self, frame: Frame, force: bool, forward: bool) -> Option<Frame> {
        let bytes = frame.data.len();
        let count = || {
            self.counters.frames.fetch_add(1, Ordering::Relaxed);
            self.counters
                .received_bytes
                .fetch_add(bytes as u64, Ordering::Relaxed);
        };
        let Some(ring) = &self.ring else {
            count();
            let kept = forward.then(|| frame.clone());
            *self.frame.write().unwrap_or_else(|e| e.into_inner()) = Some(frame);
            return kept;
        };
        let mut ring = ring.lock().unwrap_or_else(|e| e.into_inner());
        let published = ring.write(&frame, force);
        let frame = match (&published, forward) {
            // The forwarder takes the buffer, so the ring holds a copy.
            (Ok(Published::Skipped), true) => {
                ring.hold(frame.clone());
                Some(frame)
            }
            (Ok(Published::Skipped), false) => {
                ring.hold(frame);
                None
            }
            _ => forward.then_some(frame),
        };
        let capacity = ring.capacity();
        drop(ring);
        self.note_ring(published, bytes, capacity);
        count();
        frame
    }
    /// Publish a frame the ring held back once a reader polls or asks for
    /// it, or a keep-alive is due, so a stream that goes quiet still ends on
    /// its newest frame.
    fn flush_ring(&self) {
        let Some(ring) = &self.ring else {
            return;
        };
        let mut ring = ring.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bytes) = ring.held_bytes() else {
            return;
        };
        let published = ring.flush();
        let capacity = ring.capacity();
        drop(ring);
        self.note_ring(published, bytes, capacity);
    }
    /// Whether the ring holds back a frame it has yet to publish.
    fn holding(&self) -> bool {
        self.ring.as_ref().is_some_and(|ring| {
            ring.lock()
                .unwrap_or_else(|e| e.into_inner())
                .held_bytes()
                .is_some()
        })
    }
    /// Log a frame the ring could not take as it was, once while that keeps
    /// happening rather than for every frame.
    fn note_ring(&self, published: Result<Published>, bytes: usize, capacity: usize) {
        use crate::analytics::human_bytes;
        let (level, message) = match published {
            Ok(Published::Frame) => {
                *self.ring_note.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return;
            }
            Ok(Published::Skipped) => return,
            Ok(Published::Preview) => (
                "warn",
                format!(
                    "Frames of {} are larger than the {} preview buffer, so previews show them downscaled; saved files keep full resolution. Set CAPTUREFAB_FRAME_BYTES for full-resolution previews.",
                    human_bytes(bytes as u64),
                    human_bytes(capacity as u64)
                ),
            ),
            Err(e) => ("error", format!("Shared frame buffer: {e:#}")),
        };
        let mut note = self.ring_note.lock().unwrap_or_else(|e| e.into_inner());
        if note.as_deref() != Some(&message) {
            *note = Some(message.clone());
            drop(note);
            self.log(level, message);
        }
    }
}
fn timeout(ms: u64) -> Result<Duration> {
    ensure!(
        (1..=60_000).contains(&ms),
        "timeout must be 1..60000 milliseconds"
    );
    Ok(Duration::from_millis(ms))
}
fn refresh(h: &WorkerHandle, camera: &mut Option<Camera>, features: bool) {
    let list = if features {
        camera.as_mut().map(|c| c.features())
    } else {
        None
    };
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    s.connected = camera.as_ref().map(|c| c.info.clone());
    s.streaming = camera.as_ref().is_some_and(Camera::is_streaming);
    s.transport = camera.as_ref().and_then(Camera::stats);
    if let Some(list) = list {
        s.features = list;
    } else if camera.is_none() {
        s.features.clear();
    }
}
/// Stopping unlocks what the camera locked while it streamed, the image
/// geometry and often more: read again whether each feature last seen
/// read-only is writable, without walking every feature.
fn unlock_features(h: &WorkerHandle, camera: &mut Option<Camera>) {
    let Some(c) = camera.as_mut() else {
        return;
    };
    let names = h
        .state
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .features
        .iter()
        .filter(|feature| !feature.writable)
        .map(|feature| feature.name.clone())
        .collect::<Vec<_>>();
    let writable = names
        .into_iter()
        .map(|name| {
            let writable = c.is_writable(&name);
            (name, writable)
        })
        .collect::<Vec<_>>();
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    for (name, writable) in writable {
        if let Some(feature) = s.features.iter_mut().find(|f| f.name == name) {
            feature.writable = writable;
        }
    }
}
fn release_auto(
    h: &WorkerHandle,
    auto: &mut Option<AutoController>,
    camera: &mut Option<Camera>,
    revert: bool,
) -> Result<()> {
    let result = match (auto.take(), camera.as_mut()) {
        (Some(controller), Some(c)) => controller.release(c, revert),
        _ => Ok(()),
    };
    if let Err(e) = &result {
        h.log("warn", format!("Auto mode cleanup: {e:#}"));
    }
    h.state.write().unwrap_or_else(|e| e.into_inner()).auto = None;
    result
}
fn publish_auto(h: &WorkerHandle, controller: &AutoController) {
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    s.auto = Some(controller.status().clone());
    for f in s
        .features
        .iter_mut()
        .filter(|f| controller.manages(&f.name))
    {
        if let Some(v) = controller.values().get(&f.name) {
            f.value = Some(v.clone());
        }
    }
}
fn camera_mut(camera: &mut Option<Camera>) -> Result<&mut Camera> {
    camera
        .as_mut()
        .context("no camera connected; select --camera or use connect in a session")
}
/// The whole error of a `Grab` that received no frame before its deadline.
/// The coordinator explains it; every other failure keeps its own cause.
pub(crate) const GRAB_TIMEOUT: &str = "frame timeout";
/// Whether a failed frame read only means no frame was ready yet. A stream
/// that ended is never a timeout, even when FFmpeg's reason says "timed out":
/// its decoder fails every later read at once, so treating it as a wait would
/// spin the worker forever.
fn timed_out(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if cause.is::<crate::types::FrameWait>() {
            return true;
        }
        if cause.is::<crate::media::StreamEnded>() {
            return false;
        }
        if let Some(categorized) = cause.downcast_ref::<crate::types::Categorized>() {
            return categorized.kind == crate::types::ErrorKind::Timeout;
        }
    }
    let message = format!("{e:#}").to_ascii_lowercase();
    message.contains("timeout") || message.contains("timed out")
}
/// A media stream came back after a reconnect: say so, and retire the
/// reconnecting error.
fn resumed(h: &WorkerHandle) {
    h.log("info", "Stream resumed".into());
    forget_lost(h);
}
/// Retire the reconnecting error unless a newer error replaced it.
fn forget_lost(h: &WorkerHandle) {
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    if s.last_error
        .as_deref()
        .is_some_and(|error| error.starts_with("Stream lost"))
    {
        s.last_error = None;
    }
}
fn next_frame(
    h: &WorkerHandle,
    c: &mut Camera,
    auto: &mut Option<AutoController>,
    deadline: Instant,
) -> Result<Option<Frame>> {
    loop {
        ensure!(!h.cancelled(), "capture interrupted");
        if let Some(a) = auto.as_mut()
            && a.tick(c, Instant::now())
        {
            publish_auto(h, a);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        match c.next_frame(remaining.min(Duration::from_millis(100))) {
            Ok(frame) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                if let Some(a) = auto.as_mut() {
                    a.observe(&frame, Instant::now());
                }
                return Ok(Some(frame));
            }
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Let auto exposure settle on a camera that has just started, publishing
/// the frames it meters, for at most half of `timeout` and 3 s. False when
/// it had not settled by then. With `force` they reach the ring even
/// without a reader.
fn settle_auto(
    h: &WorkerHandle,
    c: &mut Camera,
    auto: &mut Option<AutoController>,
    timeout: Duration,
    force: bool,
) -> Result<bool> {
    // Refresh the controller's streaming state before testing convergence:
    // a previously stable acquisition must settle again after a restart.
    if let Some(a) = auto.as_mut() {
        a.tick(c, Instant::now());
        publish_auto(h, a);
    }
    let until = Instant::now() + (timeout / 2).min(Duration::from_secs(3));
    while auto.as_ref().is_some_and(|a| !a.converged()) {
        let Some(frame) = next_frame(h, c, auto, until)? else {
            return Ok(false);
        };
        h.publish(frame, force, false);
    }
    Ok(true)
}

/// Write `values` in order, each as Set would: later values may depend on
/// earlier ones (a selector, then the value it selects). `checkpoint` runs
/// just before each write, with the bank that write changes selected, so
/// camera history can record the value it replaces.
fn set_many(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    auto: &mut Option<AutoController>,
    values: &[FeatureValue],
    mut checkpoint: impl FnMut(&mut Camera, &str) -> Result<()>,
) -> Result<Value> {
    let mut switched = false;
    let written = (|| -> Result<Vec<Value>> {
        let mut written = Vec::with_capacity(values.len());
        for FeatureValue { feature, value } in values {
            camera_mut(camera)?.validate_set(feature, value)?;
            if auto.as_ref().is_some_and(|a| a.manages(feature)) {
                release_auto(h, auto, camera, false)?;
                switched = true;
                h.log(
                    "info",
                    format!("Switched to manual mode because {feature} was set"),
                );
            } else if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            let c = camera_mut(camera)?;
            checkpoint(c, feature)?;
            c.set(feature, value)?;
            written.push(json!({"name":feature,"value":c.get(feature)?}));
        }
        Ok(written)
    })();
    refresh(h, camera, true);
    let written = written?;
    h.log(
        "info",
        format!(
            "Set {}",
            values
                .iter()
                .map(|v| format!("{} = {}", v.feature, v.value))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    let mut reply = json!({"values":written});
    if switched {
        reply["auto"] = Value::Null;
    }
    Ok(reply)
}

/// Add a line to the worker's log; an error also becomes its `last_error`.
fn push_log(s: &mut SessionSnapshot, level: &str, message: String) {
    if level == "error" {
        s.last_error = Some(message.clone());
    }
    s.logs.push(LogEntry::now_utc(level, message));
    if s.logs.len() > 200 {
        s.logs.remove(0);
    }
}

const FORWARD_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

fn compact_forwarding(id: String, stats: &crate::media::ForwardStats) -> ForwardingProgress {
    ForwardingProgress {
        id,
        accepted: stats.accepted,
        written: stats.written,
        dropped: stats.dropped,
        late: stats.late,
        stalled: stats.stalled,
    }
}
#[derive(Default)]
struct ForwardProgressSampler {
    id: Option<String>,
    at: Option<Instant>,
}
impl ForwardProgressSampler {
    fn sample(
        &mut self,
        id: Option<&str>,
        now: Instant,
        stats: impl FnOnce() -> crate::media::ForwardStats,
    ) -> Option<ForwardingProgress> {
        let Some(id) = id else {
            self.id = None;
            self.at = None;
            return None;
        };
        if self.id.as_deref() == Some(id)
            && self
                .at
                .is_some_and(|at| now.saturating_duration_since(at) < FORWARD_PROGRESS_INTERVAL)
        {
            return None;
        }
        self.id = Some(id.into());
        self.at = Some(now);
        Some(compact_forwarding(id.into(), &stats()))
    }
}
fn sync_forwarding_progress(
    h: &WorkerHandle,
    forwarder: &Option<(Forwarder, f64)>,
    sampler: &mut ForwardProgressSampler,
    now: Instant,
) {
    let id = h
        .state
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .forwarding_id
        .clone();
    let id = forwarder.as_ref().and(id.as_deref());
    let progress = sampler.sample(id, now, || {
        forwarder.as_ref().expect("active forwarder").0.stats()
    });
    if let Some(progress) = progress {
        let mut state = h.state.write().unwrap_or_else(|error| error.into_inner());
        if state.forwarding_id.as_deref() == Some(&progress.id) {
            state.forwarding_progress = Some(progress);
        }
    }
}

/// Stop forwarding; the stopped forwarder's final counters, if one ran.
fn stop_forward(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
) -> Result<Option<crate::media::ForwardStats>> {
    end_forward(h, forwarder, auto, |_| None)
}
/// Stop the forward, and log the line `report` makes of how it ended (a
/// level and message) in the same step that clears the forwarding state: a
/// status never shows the forward gone without the error it ended with, nor
/// still running with that error.
fn end_forward(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    report: impl FnOnce(&Result<Option<crate::media::ForwardStats>>) -> Option<(&'static str, String)>,
) -> Result<Option<crate::media::ForwardStats>> {
    if let Some(a) = auto.as_mut() {
        a.set_forward_fps(None)?;
    }
    let meta = h.recording.lock().unwrap_or_else(|e| e.into_inner()).take();
    let id = h
        .state
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .forwarding_id
        .clone();
    let mut final_progress = None;
    let stopped = match forwarder.take() {
        Some((mut f, _)) => {
            let stopped = f.stop();
            let stats = f.stats();
            final_progress = id.map(|id| compact_forwarding(id, &stats));
            // A finished file is reported even when stopping reported an error.
            if let Some(file) = stats.recording.clone() {
                let context = meta.map(|m| m.context).unwrap_or_default();
                h.push_saved(crate::dataset::SavedFile {
                    seq: 0,
                    wall_ms: file.started_ms,
                    path: absolute_path(std::path::Path::new(&file.path)),
                    bytes: file.bytes,
                    sha256: Some(file.sha256),
                    destination: file.destination,

                    context,
                    item: crate::dataset::SavedItem::Recording {
                        started_ms: file.started_ms,
                        ended_ms: file.ended_ms,
                        codec: file.codec,
                        encoder: file.encoder,
                        fps: file.fps,
                    },
                });
            }
            stopped.map(|()| Some(stats))
        }
        None => Ok(None),
    };
    *h.forward_clock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // Last: whoever sees the forward gone (a client waiting out a timed
    // recording, say) finds its file finished and reported.
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    if let Some((level, message)) = report(&stopped) {
        push_log(&mut s, level, message);
    }
    if let Some(progress) = final_progress {
        s.forwarding_progress = Some(progress);
    }
    s.forwarding = None;
    s.forwarding_id = None;
    s.recording_path = None;
    s.forwarding_since_ms = None;
    s.forwarding_until_ms = None;
    stopped
}
/// The requested length of a timed recording or forward whose end time has
/// come at `now_ms` (UTC Unix milliseconds), or `None` while it should run on.
fn forward_due(state: &SessionSnapshot, now_ms: u64) -> Option<u64> {
    let until = state.forwarding_until_ms.filter(|until| now_ms >= *until)?;
    Some(until.saturating_sub(state.forwarding_since_ms.unwrap_or(until)))
}
/// Stop a timed recording or forward once its end time has come, the way
/// stop-forward does, so a recording is committed and cataloged for
/// local storage whether or not the client that started it is still there.
fn finish_timed_forward(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    now_ms: u64,
) {
    finish_timed_forward_at(h, forwarder, auto, now_ms, Instant::now());
}
fn finish_timed_forward_at(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    wall_ms: u64,
    now: Instant,
) {
    let due = {
        let state = h.state.read().unwrap_or_else(|e| e.into_inner());
        let mut clock = h.forward_clock.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = clock
            .as_mut()
            .filter(|clock| state.forwarding_id.as_deref() == Some(clock.forwarding_id.as_str()))
            .map_or(wall_ms, |clock| clock.now_ms(wall_ms, now));
        forward_due(&state, now_ms)
    };
    let (Some(duration_ms), Some((f, _))) = (due, forwarder.as_ref()) else {
        return;
    };
    let what = if f.recording_path().is_some() {
        "Recording"
    } else {
        "Forwarding"
    };
    let length = crate::scheduling::format_duration(duration_ms);
    let _ = end_forward(h, forwarder, auto, |stopped| {
        Some(match stopped {
            Ok(_) => ("info", format!("{what} finished after {length}")),
            Err(e) => ("error", format!("{what} stopped after {length}: {e:#}")),
        })
    });
}
/// Pick a free name for a recording template under its destination and
/// create the file, moving to the next free name when another writer (another
/// camera's worker) took it first.
fn name_recording(
    h: &WorkerHandle,
    config: &ForwardConfig,
    camera: &str,
    on_exists: crate::naming::OnExists,
) -> Result<(
    PathBuf,
    crate::destination::Resolved,
    crate::storage::StorageWriter,
)> {
    let resolved = crate::destination::resolve(config.destination.as_deref(), &config.output)?;
    let local = crate::media::local_file_path(&resolved.output)?;
    let local = local
        .to_str()
        .context("recording path must be valid UTF-8")?
        .to_owned();
    let plan = if crate::naming::has_placeholder(&local) {
        crate::naming::Plan::Template(local)
    } else {
        crate::naming::Plan::File(local.into())
    };
    let input = crate::naming::NameInput {
        camera,
        now: chrono::Local::now(),
        index: 0,
    };
    let first = h
        .names
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .candidate(&plan, &input, on_exists)?;
    let (path, writer) =
        crate::naming::save_named(&h.names, &plan, &input, on_exists, first, |path| {
            resolved.contain(path)?;
            check_parents(path)?;
            crate::storage::create_writer(path, &config.storage, config.max_file_bytes)
        })?;
    Ok((path, resolved, writer))
}
fn absolute_path(path: &std::path::Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}
/// Report a locally saved image to the coordinator's dataset.
#[allow(clippy::too_many_arguments)]
fn report_image(
    h: &WorkerHandle,
    resolved: &crate::destination::Resolved,
    path: &std::path::Path,
    stored: crate::storage::Stored,
    frame: &Frame,
    format: &str,
    wall_ms: u64,
    job_id: Option<u64>,
    context: &crate::context::Context,
) {
    h.push_saved(crate::dataset::SavedFile {
        seq: 0,
        wall_ms,
        path: absolute_path(path),
        bytes: stored.bytes,
        sha256: Some(stored.sha256),
        destination: resolved.destination.clone(),

        context: context.clone(),
        item: crate::dataset::SavedItem::Capture {
            frame_id: frame.id,
            timestamp_ns: frame.timestamp_ns,
            width: frame.width,
            height: frame.height,
            pixel_format: frame.pixel_format,
            format: format.into(),
            origin: if job_id.is_some() {
                "schedule"
            } else {
                "capture"
            }
            .into(),
            job_id,
        },
    });
}

#[allow(clippy::too_many_arguments)]
fn process_plain(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &mut Vec<Scheduled>,
    next_job: &mut u64,
    scheduled_stream: &mut bool,
    command: SessionCommand,
) -> Result<Value> {
    match command {
        SessionCommand::Grab { timeout_ms } => {
            let duration = timeout(timeout_ms)?;
            let deadline = Instant::now() + duration;
            let c = camera_mut(camera)?;
            ensure!(
                !c.is_scanner(),
                "a document scanner cannot be inspected without scanning a page"
            );
            let was_streaming = c.is_streaming();
            c.start()?;
            // Capture without a file, name or log line: the frame only
            // reaches the shared ring.
            let result = (|| -> Result<Value> {
                let settled = was_streaming
                    || auto.is_none()
                    || settle_auto(
                        h,
                        c,
                        auto,
                        deadline.saturating_duration_since(Instant::now()),
                        true,
                    )?;
                ensure!(Instant::now() < deadline, GRAB_TIMEOUT);
                c.request_frame()?;
                let frame = next_frame(h, c, auto, deadline)?.context(GRAB_TIMEOUT)?;
                let mut value = json!({"frame":frame});
                if !settled {
                    value["auto_settled"] = json!(false);
                }
                h.record_capture(frame);
                Ok(value)
            })();
            let stopped = if !was_streaming { c.stop() } else { Ok(()) };
            refresh(h, camera, false);
            let value = result?;
            stopped?;
            Ok(value)
        }
        SessionCommand::Frame { .. } => anyhow::bail!("frames belong to the coordinator"),
        SessionCommand::Status => {
            refresh(h, camera, false);
            Ok(serde_json::to_value(h.snapshot())?)
        }
        SessionCommand::Undo | SessionCommand::Redo | SessionCommand::ReplayCameraChange { .. } => {
            anyhow::bail!("history commands must be handled by the camera journal")
        }
        SessionCommand::Select { .. } => anyhow::bail!("selection belongs to the coordinator"),
        SessionCommand::Activity { .. } => anyhow::bail!("activity belongs to the coordinator"),
        SessionCommand::DatasetOpen { .. }
        | SessionCommand::DatasetClose
        | SessionCommand::DatasetStatus
        | SessionCommand::DatasetNote { .. }
        | SessionCommand::DatasetQuery { .. }
        | SessionCommand::DatasetExport { .. }
        | SessionCommand::DatasetOpenIf { .. }
        | SessionCommand::DatasetCloseIf { .. }
        | SessionCommand::DatasetNoteIf { .. }
        | SessionCommand::DatasetQueryIf { .. }
        | SessionCommand::DatasetModelQueryIf { .. }
        | SessionCommand::DatasetExportIf { .. } => {
            anyhow::bail!("datasets belong to the coordinator")
        }
        SessionCommand::NetworkScan { .. } => {
            anyhow::bail!("network scan belongs to the coordinator")
        }
        SessionCommand::OptimizeNetwork { bytes_per_second } => {
            ensure!(
                forwarder.is_none() && !jobs.iter().any(|job| job.info.active()),
                "Stop recording, forwarding and scheduled captures, then scan again to apply network defaults"
            );
            let c = camera_mut(camera)?;
            ensure!(
                c.info.transport == crate::types::Transport::GigE,
                "network optimization requires a GigE camera"
            );
            let streaming = c.is_streaming();
            let stopped = if streaming { c.stop() } else { Ok(()) };
            let tuned = stopped.and_then(|_| network_defaults::apply(c, bytes_per_second));
            if let Some(controller) = auto.as_mut() {
                controller.invalidate();
            }
            // Resume even when a probe or camera setting failed. Never leave a
            // previously running preview stopped as a side effect of scanning.
            let resumed = if streaming { c.start() } else { Ok(()) };
            refresh(h, camera, true);
            let tuning = match (tuned, resumed) {
                (Ok(tuning), Ok(())) => tuning,
                (Err(error), Ok(())) => return Err(error),
                (Ok(_), Err(error)) => {
                    return Err(error).context("resuming stream after network scan");
                }
                (Err(tuning), Err(resume)) => anyhow::bail!(
                    "Network defaults failed: {tuning:#}; stream could not resume: {resume:#}"
                ),
            };
            h.log("info", "GigE network defaults applied".into());
            Ok(json!({"tuning":tuning}))
        }
        SessionCommand::Forward {
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
            duration_ms,
        } => {
            ensure!(
                !camera_mut(camera)?.is_scanner(),
                "scanner forwarding is unsupported; use capture for document scans"
            );
            ensure!(
                forwarder.is_none(),
                "forwarding is already active; stop-forward first"
            );
            let mut identity = [0u8; 16];
            getrandom::fill(&mut identity)
                .map_err(|_| anyhow::anyhow!("could not allocate a recording identity"))?;
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let forwarding_id: String = identity
                .iter()
                .flat_map(|byte| {
                    [
                        char::from(HEX[usize::from(byte >> 4)]),
                        char::from(HEX[usize::from(byte & 15)]),
                    ]
                })
                .collect();
            let f_destination = destination.clone();
            let mut config = ForwardConfig {
                output: output.clone(),
                codec,
                encoder,
                fps,
                bitrate,
                storage,
                max_file_bytes,
                destination,
            };
            let named = crate::media::is_recording(&output)
                && (crate::naming::has_placeholder(&output) || !on_exists.is_fail());
            let f = if named {
                config.validate()?;
                let label = crate::naming::camera_label(&camera_mut(camera)?.info);
                let (path, resolved, reserved) = name_recording(h, &config, &label, on_exists)?;
                config.output = path.to_string_lossy().into_owned();
                config.destination = None;
                Forwarder::with_resolved(
                    config,
                    resolved.destination.is_some().then_some(resolved),
                    Some(reserved),
                )?
            } else {
                Forwarder::new(config)?
            };
            let f_path = f.recording_path().map(|p| absolute_path(&p));
            camera_mut(camera)?.start()?;
            *forwarder = Some((f, fps));
            if let Some(a) = auto.as_mut() {
                a.set_forward_fps(Some(fps))?;
            }
            refresh(h, camera, false);
            let since_ms = crate::scheduling::now_ms();
            // The worker loop stops a timed forward at this wall-clock time.
            let until_ms = duration_ms.map(|duration| since_ms.saturating_add(duration));
            *h.forward_clock.lock().unwrap_or_else(|e| e.into_inner()) =
                duration_ms.map(|_| ForwardClock {
                    forwarding_id: forwarding_id.clone(),
                    anchor_ms: since_ms,
                    anchor: Instant::now(),
                });
            let shown = crate::media::redact_url(&output);
            {
                let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
                s.forwarding = Some(shown.clone());
                s.forwarding_id = Some(forwarding_id.clone());
                s.forwarding_progress = Some(ForwardingProgress {
                    id: forwarding_id.clone(),
                    ..ForwardingProgress::default()
                });
                s.forwarding_since_ms = Some(since_ms);
                s.forwarding_until_ms = until_ms;
                // `forwarding` keeps the requested output; this is the file it named.
                s.recording_path = f_path.clone();
            }
            let mut value =
                json!({"forwarding":shown,"forwarding_id":forwarding_id,"since_ms":since_ms});
            if let Some(until_ms) = until_ms {
                value["until_ms"] = json!(until_ms);
            }
            if let Some(path) = f_path {
                value["path"] = json!(path);
                *h.recording.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(crate::scheduling::RecordingMeta {
                        path,
                        since_ms,
                        destination: f_destination,
                        context,
                    });
            }
            Ok(value)
        }
        SessionCommand::StopForward => {
            let stats = stop_forward(h, forwarder, auto)?;
            Ok(json!({"forwarding":null,"stats":stats}))
        }
        SessionCommand::StopForwardIf { forwarding_id } => {
            ensure!(
                h.snapshot().forwarding_id.as_deref() == Some(forwarding_id.as_str()),
                ForwardChanged
            );
            stop_forward(h, forwarder, auto)?;
            Ok(json!({"forwarding":null}))
        }
        SessionCommand::Discover {
            timeout_ms,
            simulated,
        } => {
            let (devices, warnings) = crate::camera::discover(timeout(timeout_ms)?, simulated)?;
            h.state.write().unwrap_or_else(|e| e.into_inner()).devices = devices.clone();
            for w in &warnings {
                h.log("warn", w.clone());
            }
            h.log("info", format!("Discovered {} camera(s)", devices.len()));
            Ok(json!({"devices":devices,"warnings":warnings}))
        }
        SessionCommand::Connect {
            camera: selector,
            timeout_ms,
            login,
            saved_login,
        } => {
            ensure!(
                camera.is_none(),
                "a camera is already connected; disconnect first"
            );
            let duration = timeout(timeout_ms)?;
            let mut known = h.snapshot().devices;
            if !known
                .iter()
                .any(|d| d.id == selector || d.serial == selector)
                && !selector.starts_with("sim:")
                && !crate::scanner::is_source(&selector)
                && !crate::media::is_source(&selector)
                && !crate::onvif::is_source(&selector)
                && !crate::hubs::is_source(&selector)
                && selector
                    .trim_start_matches("gige:")
                    .parse::<std::net::Ipv4Addr>()
                    .is_err()
            {
                let (devices, warnings) = crate::camera::discover(duration, false)?;
                known = devices;
                for w in warnings {
                    h.log("warn", w);
                }
                h.state.write().unwrap_or_else(|e| e.into_inner()).devices = known.clone();
            }
            // The address a sign-in is saved under: the selector itself, or
            // the ONVIF address of a discovered camera named by its serial.
            let key = crate::logins::key(&selector).ok().or_else(|| {
                known
                    .iter()
                    .find(|d| {
                        crate::onvif::is_source(&d.id) && (d.id == selector || d.serial == selector)
                    })
                    .and_then(|d| crate::logins::key(&d.id).ok())
            });
            let mut login = login;
            let mut remembered = None;
            if login.is_none()
                && saved_login
                && let Some(key) = &key
            {
                // An unreadable sign-in list must not break every camera:
                // go on without a saved sign-in, as when there is none. A
                // camera that needs one still answers auth_required.
                let listed = match h.logins.entry(&key.address) {
                    Ok(entry) => entry.is_some(),
                    Err(error) => {
                        h.log(
                            "warn",
                            format!(
                                "ignoring saved camera sign-ins; fix or remove {}: {error:#}",
                                h.logins.index_path().display()
                            ),
                        );
                        false
                    }
                };
                match if listed {
                    h.logins.remembered(&key.address)?
                } else {
                    crate::logins::Remembered::None
                } {
                    crate::logins::Remembered::Login(saved) => {
                        login = Some(saved);
                        remembered = Some(key.address.clone());
                    }
                    crate::logins::Remembered::Missing { .. } => h.log(
                        "warn",
                        format!(
                            "the saved sign-in for {} is missing from the OS credential store",
                            crate::logins::short(&key.address)
                        ),
                    ),
                    crate::logins::Remembered::None => {}
                }
            }
            *camera = Some(Camera::open_with_login(
                &selector,
                &known,
                duration,
                login.as_ref(),
            )?);
            drop(login);
            if let Some(address) = remembered {
                h.logins.touch(&address);
            }
            *h.frame.write().unwrap_or_else(|e| e.into_inner()) = None;
            {
                h.counters.reset();
                h.state
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_error = None;
                h.state
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .forwarding_progress = None;
            }
            refresh(h, camera, true);
            h.log(
                "info",
                format!("Connected to {}", crate::media::redact_url(&selector)),
            );
            Ok(json!({"connected":h.snapshot().connected}))
        }
        SessionCommand::Disconnect => {
            cancel_jobs(jobs);
            sync_jobs(h, jobs);
            *scheduled_stream = false;
            let auto_result = release_auto(h, auto, camera, true);
            let forward_result = stop_forward(h, forwarder, auto);
            let camera_result = if let Some(c) = camera.as_mut() {
                c.stop()
            } else {
                Ok(())
            };
            *camera = None;
            *h.frame.write().unwrap_or_else(|e| e.into_inner()) = None;
            refresh(h, camera, false);
            auto_result?;
            forward_result?;
            camera_result?;
            h.log("info", "Disconnected".into());
            Ok(json!({"connected":null}))
        }
        SessionCommand::Get { feature } => {
            Ok(json!({"name":feature,"value":camera_mut(camera)?.get(&feature)?}))
        }
        SessionCommand::Set { feature, value } => {
            camera_mut(camera)?.validate_set(&feature, &value)?;
            let switched = auto.as_ref().is_some_and(|a| a.manages(&feature));
            if switched {
                release_auto(h, auto, camera, false)?;
                h.log(
                    "info",
                    format!("Switched to manual mode because {feature} was set"),
                );
            } else if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            camera_mut(camera)?.set(&feature, &value)?;
            let result = camera_mut(camera)?.get(&feature)?;
            refresh(h, camera, true);
            h.log("info", format!("Set {feature} = {value}"));
            let mut reply = json!({"name":feature,"value":result});
            if switched {
                reply["auto"] = Value::Null;
            }
            Ok(reply)
        }
        // Camera history checkpoints each write; see `set_many`.
        SessionCommand::SetMany { values } => set_many(h, camera, auto, &values, |_, _| Ok(())),
        SessionCommand::Execute { feature } => {
            if auto.is_some() && matches!(feature.as_str(), "UserSetLoad" | "UserSetDefault") {
                release_auto(h, auto, camera, false)?;
                h.log(
                    "info",
                    format!("Switched to manual mode because {feature} was executed"),
                );
            } else if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            camera_mut(camera)?.execute(&feature)?;
            refresh(h, camera, true);
            Ok(json!({"executed":feature}))
        }
        SessionCommand::Features => {
            refresh(h, camera, true);
            Ok(json!({"features":h.snapshot().features}))
        }
        SessionCommand::Xml => Ok(json!({"xml":camera_mut(camera)?.xml()})),
        SessionCommand::ReadMemory { address, length } => {
            ensure!(
                (1..=65536).contains(&length),
                "memory length must be 1..65536"
            );
            Ok(json!({"address":address,"data":camera_mut(camera)?.read_memory(address,length)?}))
        }
        SessionCommand::WriteMemory { address, data } => {
            ensure!(
                !data.is_empty() && data.len() <= 65536,
                "memory length must be 1..65536"
            );
            if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            camera_mut(camera)?.write_memory(address, &data)?;
            Ok(json!({"address":address,"written":data.len()}))
        }
        SessionCommand::Jobs => {
            Ok(json!({"jobs":jobs.iter().map(|j| &j.info).collect::<Vec<_>>()}))
        }
        SessionCommand::CancelJob { id } => {
            let job = jobs
                .iter_mut()
                .find(|j| j.info.id == id)
                .context("capture job not found")?;
            ensure!(job.info.active(), "capture job is already finished");
            job.info.status = "cancelled".into();
            sync_jobs(h, jobs);
            Ok(json!({"cancelled":id}))
        }
        SessionCommand::Schedule {
            output,
            count,
            timeout_ms,
            format,
            first_at_ms,
            interval_ms,
            storage,
            destination,
            on_exists,
            context,
        } => {
            validate_capture(&output, count, timeout_ms, &format)?;
            let resolved = crate::destination::resolve(destination.as_deref(), &output)?;
            preflight_capture_output(&resolved.output, count, &format, on_exists)?;
            validate_schedule_outputs(&resolved.output, count, &format, on_exists, jobs)?;
            storage.validate()?;
            ensure!(
                output != "-",
                "scheduled capture requires a file or directory"
            );
            ensure!(
                jobs.iter().filter(|j| j.info.active()).count() < 32,
                "capture schedule queue is full (32 active jobs)"
            );
            crate::scheduling::validate_schedule(first_at_ms, interval_ms, count)?;
            let label = crate::naming::camera_label(&camera_mut(camera)?.info);
            let first_at_ms = first_at_ms.max(crate::scheduling::now_ms());
            let info = crate::scheduling::CaptureJob {
                id: *next_job,
                status: "pending".into(),
                output: resolved.output.clone(),
                destination: resolved.destination.clone(),
                count,
                captured: 0,
                skipped: 0,
                retries: 0,
                first_at_ms,
                interval_ms,
                next_at_ms: first_at_ms,
                last_file: None,
                error: None,
                context,
            };
            *next_job = next_job.checked_add(1).context("job ID exhausted")?;
            jobs.push(Scheduled {
                info: info.clone(),
                plan: crate::naming::Plan::new(&resolved.output, count, &format),
                resolved,
                timeout_ms,
                format,
                storage,
                deadline: None,
                warm: None,
                camera: label,
                on_exists,
                next: None,
                saving: false,
            });
            trim_jobs(jobs);
            sync_jobs(h, jobs);
            h.log("info", format!("Capture job {} scheduled", info.id));
            Ok(json!({"job":info}))
        }
        SessionCommand::Auto { balance } => {
            let c = camera_mut(camera)?;
            ensure!(
                !c.is_scanner(),
                "scanner auto exposure is unsupported; configure scan settings with set"
            );
            let explicit = match auto.as_mut() {
                Some(controller) => {
                    if let Some(balance) = balance {
                        controller.set_balance(c, balance)?;
                    }
                    balance.is_some()
                }
                None => {
                    let balance = balance.unwrap_or(crate::auto::DEFAULT_BALANCE);
                    let mut controller = AutoController::enter(c, balance)?;
                    controller.set_forward_fps(forwarder.as_ref().map(|(_, fps)| *fps))?;
                    *auto = Some(controller);
                    true
                }
            };
            let controller = auto.as_mut().context("auto controller")?;
            if explicit && forwarder.is_none() && !jobs.iter().any(|j| j.info.active()) {
                controller.choose_video_mode(c);
            }
            controller.tick(c, Instant::now());
            let status = controller.status().clone();
            refresh(h, camera, true);
            publish_auto(h, controller);
            h.log("info", format!("Auto mode, balance {:.2}", status.balance));
            Ok(json!({"auto":status}))
        }
        SessionCommand::Manual { revert } => {
            let c = camera_mut(camera)?;
            let released = match auto.take() {
                Some(controller) => controller.release(c, revert),
                None if revert => Ok(()),
                None => crate::auto::hold(c),
            };
            h.state.write().unwrap_or_else(|e| e.into_inner()).auto = None;
            refresh(h, camera, true);
            released?;
            h.log("info", "Manual mode".into());
            Ok(json!({"auto":null}))
        }
        SessionCommand::Start => {
            *scheduled_stream = false;
            camera_mut(camera)?.start()?;
            refresh(h, camera, false);
            h.log("info", "Acquisition started".into());
            Ok(json!({"streaming":true}))
        }
        SessionCommand::Stop => {
            cancel_jobs(jobs);
            sync_jobs(h, jobs);
            *scheduled_stream = false;
            let forward_result = stop_forward(h, forwarder, auto);
            let camera_result = camera_mut(camera)?.stop();
            refresh(h, camera, false);
            unlock_features(h, camera);
            forward_result?;
            camera_result?;
            h.log("info", "Acquisition stopped".into());
            Ok(json!({"streaming":false}))
        }
        SessionCommand::Capture {
            output,
            count,
            timeout_ms,
            format,
            storage,
            destination,
            on_exists,
            context,
        } => {
            ensure!(
                output != "-" || destination.is_none(),
                "stdout output cannot use a destination"
            );
            let resolved = if output == "-" {
                crate::destination::resolve(None, &output)?
            } else {
                crate::destination::resolve(destination.as_deref(), &output)?
            };
            validate_capture(&output, count, timeout_ms, &format)?;
            storage.validate()?;
            if output != "-" {
                preflight_capture_output(&resolved.output, count, &format, on_exists)?;
            }
            let duration = timeout(timeout_ms)?;
            ensure!((1..=100_000).contains(&count), "count must be 1..100000");
            ensure!(
                ["png", "jpeg", "raw", "ppm", "pgm"].contains(&format.as_str()),
                "format must be png, jpeg, raw, pgm or ppm"
            );
            ensure!(!output.is_empty(), "provide an output path");
            ensure!(
                output != "-" || count == 1,
                "stdout supports one frame only"
            );
            let c = camera_mut(camera)?;
            ensure!(
                !c.is_scanner()
                    || !jobs
                        .iter()
                        .any(|job| job.info.active() && job.deadline.is_some()),
                "scanner is busy with a scheduled scan; wait for it to finish or cancel the capture job first"
            );
            let was_streaming = c.is_streaming();
            c.start()?;
            let result = (|| -> Result<Value> {
                let mut files = Vec::new();
                let mut frames = Vec::new();
                let mut status = None;
                if !was_streaming && auto.is_some() && !settle_auto(h, c, auto, duration, false)? {
                    h.log(
                        "warn",
                        "Auto exposure had not settled; capturing anyway".into(),
                    );
                }
                let label = crate::naming::camera_label(&c.info);
                let plan = crate::naming::Plan::new(&resolved.output, count, &format);
                for index in 0..count {
                    // Pick the name before requesting the frame: a feeder
                    // consumes paper irreversibly, so a taken name in fail
                    // mode is refused first.
                    let input = crate::naming::NameInput {
                        camera: &label,
                        now: chrono::Local::now(),
                        index,
                    };
                    let first = if output == "-" {
                        None
                    } else {
                        let path = h
                            .names
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .candidate(&plan, &input, on_exists)?;
                        resolved.contain(&path)?;
                        Some(path)
                    };
                    c.request_frame()?;
                    let frame = next_frame(h, c, auto, Instant::now() + duration)?
                        .context("frame timeout")?;
                    if index == 0 {
                        status = auto.as_ref().map(|a| a.status().clone());
                    }
                    let wall_ms = crate::scheduling::now_ms();
                    let mut metadata = serde_json::to_value(&frame)?;
                    metadata["bytes"] = json!(frame.data.len());
                    metadata["wall_ms"] = json!(wall_ms);
                    if let Some(first) = first {
                        let bytes = crate::frame::encode(&frame, &format)?;
                        let (path, stored) = crate::naming::save_named(
                            &h.names,
                            &plan,
                            &input,
                            on_exists,
                            first.clone(),
                            |path| {
                                resolved.contain(path)?;
                                crate::storage::save_bytes(&bytes, path, &storage)
                            },
                        )
                        .with_context(|| format!("save {}", first.display()))?;
                        let file = path.to_string_lossy().into_owned();
                        report_image(
                            h, &resolved, &path, stored, &frame, &format, wall_ms, None, &context,
                        );
                        metadata["file"] = json!(file);
                        files.push(file);
                    }
                    frames.push(metadata);
                    h.record_capture(frame);
                }
                let mut value = json!({"files":files,"frames":frames,"count":count});
                if !context.is_empty() {
                    value["context"] = json!(context);
                }
                if let Some(name) = &resolved.destination {
                    value["destination"] = json!(name);
                }
                if let Some(status) = status {
                    value["auto"] = json!(status);
                }
                Ok(value)
            })();
            // A scanner's pending helper must be cancelled on a failed scan,
            // even if the caller had already armed acquisition with start.
            let scanner = c.is_scanner();
            let stopped = if !was_streaming || scanner {
                c.stop()
            } else {
                Ok(())
            };
            refresh(h, camera, scanner);
            let value = result?;
            stopped?;
            h.log("info", format!("Captured {count} frame(s)"));
            Ok(value)
        }
    }
}

struct Scheduled {
    info: crate::scheduling::CaptureJob,
    resolved: crate::destination::Resolved,
    plan: crate::naming::Plan,
    timeout_ms: u64,
    format: String,
    storage: crate::storage::StoragePolicy,
    deadline: Option<Instant>,
    warm: Option<Instant>,
    /// The camera label for {camera}, taken when the job was scheduled.
    camera: String,
    on_exists: crate::naming::OnExists,
    /// The name picked for the frame being requested, with its clock time.
    next: Option<(PathBuf, chrono::DateTime<chrono::Local>)>,
    /// A shot is with the saver; the next one waits for its outcome.
    saving: bool,
}
impl Scheduled {
    /// Pick the next frame's name: a free one, inside the destination.
    fn allocate(
        &self,
        names: &Mutex<crate::naming::Namer>,
    ) -> Result<(PathBuf, chrono::DateTime<chrono::Local>)> {
        let now = chrono::Local::now();
        let path = names.lock().unwrap_or_else(|e| e.into_inner()).candidate(
            &self.plan,
            &self.input(now),
            self.on_exists,
        )?;
        self.resolved.contain(&path)?;
        Ok((path, now))
    }
    fn input(&self, now: chrono::DateTime<chrono::Local>) -> crate::naming::NameInput<'_> {
        crate::naming::NameInput {
            camera: &self.camera,
            now,
            index: self.info.captured,
        }
    }
    /// This job's file for the frame being shot, at the name picked for it.
    fn save(&self, path: PathBuf, now: chrono::DateTime<chrono::Local>) -> saver::Save {
        saver::Save {
            job: self.info.id,
            path,
            now,
            index: self.info.captured,
            camera: self.camera.clone(),
            plan: self.plan.clone(),
            on_exists: self.on_exists,
            format: self.format.clone(),
            resolved: self.resolved.clone(),
            storage: self.storage.clone(),
            context: self.info.context.clone(),
        }
    }
    /// Schedule the next shot one interval after every shot so far: saved,
    /// being saved, skipped or retried. A job that fell behind catches up.
    fn advance(&mut self) {
        let shots: u64 = [
            self.info.captured,
            self.info.skipped,
            self.info.retries,
            u32::from(self.saving),
        ]
        .into_iter()
        .map(u64::from)
        .sum();
        self.info.next_at_ms = self
            .info
            .first_at_ms
            .saturating_add(self.info.interval_ms.saturating_mul(shots));
    }
}
fn validate_capture(output: &str, count: u32, timeout_ms: u64, format: &str) -> Result<()> {
    timeout(timeout_ms)?;
    ensure!((1..=100_000).contains(&count), "count must be 1..100000");
    ensure!(
        ["png", "jpeg", "raw", "ppm", "pgm"].contains(&format),
        "format must be png, jpeg, raw, pgm or ppm"
    );
    validate_capture_output(output)?;
    Ok(())
}
pub fn validate_capture_output(output: &str) -> Result<()> {
    crate::destination::validate_output(None, output)?;
    ensure!(
        !output.contains("://") && !output.starts_with("file:"),
        "capture output must be a file or directory; use forward for a network stream"
    );
    crate::naming::validate_template(output)
}
/// Reject collisions and unusable parent paths before settings are changed or
/// acquisition starts. The storage writer still creates each file atomically.
pub fn preflight_capture_output(
    output: &str,
    count: u32,
    format: &str,
    on_exists: crate::naming::OnExists,
) -> Result<()> {
    validate_capture_output(output)?;
    ensure!((1..=100_000).contains(&count), "count must be 1..100000");
    let plan = crate::naming::Plan::new(output, count, format);
    if !plan.is_static() {
        // Names are picked per frame; only the folders before the first
        // placeholder are known now.
        let fixed = match output.find('{') {
            Some(i) => &output[..i],
            None => output,
        };
        let probe = match &plan {
            crate::naming::Plan::Directory { .. } => Path::new(output).join("frame"),
            _ if fixed.ends_with(std::path::is_separator) => Path::new(fixed).join("x"),
            _ => PathBuf::from(fixed),
        };
        return check_parents(&probe);
    }
    for index in 0..count {
        let path = output_path(output, count, index, format);
        if on_exists.is_fail() {
            crate::naming::check_free(&path)?;
        }
        check_parents(&path)?;
    }
    Ok(())
}
fn check_parents(path: &Path) -> Result<()> {
    let mut ancestor = path.parent();
    while let Some(parent) = ancestor.filter(|parent| !parent.as_os_str().is_empty()) {
        match std::fs::metadata(parent) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir(),
                    "capture output parent is not a directory: {}",
                    parent.display()
                );
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = parent.parent()
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect capture output parent {}", parent.display())
                });
            }
        }
    }
    Ok(())
}
fn planned_path(path: &std::path::Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    let mut resolved = loop {
        match std::fs::canonicalize(existing) {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .context("capture output has no file name")?,
                );
                existing = existing
                    .parent()
                    .context("capture output has no existing ancestor")?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolve capture output {}", path.display()));
            }
        }
    };
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}
fn validate_schedule_outputs(
    output: &str,
    count: u32,
    format: &str,
    on_exists: crate::naming::OnExists,
    jobs: &[Scheduled],
) -> Result<()> {
    use std::{
        collections::{HashMap, hash_map::DefaultHasher},
        hash::{Hash, Hasher},
    };
    // Names picked per frame never collide: the namer skips taken ones.
    let dynamic = |plan: &crate::naming::Plan, on_exists| {
        !plan.is_static() || crate::naming::renumbers(plan, on_exists)
    };
    if dynamic(&crate::naming::Plan::new(output, count, format), on_exists) {
        return Ok(());
    }
    let active = jobs
        .iter()
        .filter(|job| job.info.active() && !dynamic(&job.plan, job.on_exists))
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Ok(());
    }
    // Store indexes, not potentially 16 KiB paths. Compare actual paths on a
    // hash match so collisions cannot reject a distinct requested output.
    let hash = |path: &std::path::Path| {
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        hasher.finish()
    };
    let mut requested = HashMap::<u64, Vec<u32>>::new();
    for index in 0..count {
        let path = planned_path(&output_path(output, count, index, format))?;
        requested.entry(hash(&path)).or_default().push(index);
    }
    for job in active {
        for index in job.info.captured..job.info.count {
            let existing = planned_path(&output_path(
                &job.info.output,
                job.info.count,
                index,
                &job.format,
            ))?;
            if let Some(candidates) = requested.get(&hash(&existing)) {
                for &candidate in candidates {
                    let path = planned_path(&output_path(output, count, candidate, format))?;
                    ensure!(
                        path != existing,
                        "capture output is already scheduled by active job {}: {}; choose a different path or cancel that job",
                        job.info.id,
                        existing.display()
                    );
                }
            }
        }
    }
    Ok(())
}
fn cancel_jobs(jobs: &mut [Scheduled]) {
    for j in jobs.iter_mut().filter(|j| j.info.active()) {
        j.info.status = "cancelled".into();
    }
}
fn trim_jobs(jobs: &mut Vec<Scheduled>) {
    while jobs.len() > 132 {
        if let Some(i) = jobs.iter().position(|j| !j.info.active()) {
            jobs.remove(i);
        } else {
            break;
        }
    }
}
fn sync_jobs(h: &WorkerHandle, jobs: &[Scheduled]) {
    h.state.write().unwrap_or_else(|e| e.into_inner()).jobs =
        jobs.iter().map(|j| j.info.clone()).collect();
}
fn tick_jobs(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    jobs: &mut Vec<Scheduled>,
    started: &mut bool,
    forwarding: bool,
    auto: bool,
) {
    let now = crate::scheduling::now_ms();
    let mut changed = false;
    for job in jobs.iter_mut().filter(|j| j.info.active() && !j.saving) {
        if job.info.next_at_ms > now {
            continue;
        }
        if job.deadline.is_none() {
            changed = true;
            let result = (|| -> Result<bool> {
                let c = camera_mut(camera)?;
                let stopped = !c.is_streaming();
                if stopped {
                    c.start()?;
                    *started = true;
                }
                if c.is_scanner() {
                    *started = true;
                }
                job.next = Some(job.allocate(&h.names)?);
                c.request_frame()?;
                Ok(stopped)
            })();
            let stopped = match result {
                Ok(stopped) => stopped,
                Err(e) => {
                    job.info.status = "failed".into();
                    job.info.error = Some(format!("{e:#}"));
                    h.log(
                        "error",
                        format!("Capture job {} failed: {e:#}", job.info.id),
                    );
                    continue;
                }
            };
            job.info.status = "running".into();
            let timeout = Duration::from_millis(job.timeout_ms);
            job.deadline = Some(Instant::now() + timeout);
            job.warm = (auto && stopped)
                .then(|| Instant::now() + (timeout / 2).min(Duration::from_secs(3)));
        }
        if job.deadline.is_some_and(|d| Instant::now() >= d) {
            changed = true;
            job.info.status = "failed".into();
            job.info.error = Some("frame timeout".into());
            h.log(
                "error",
                format!("Capture job {}: frame timeout", job.info.id),
            );
        }
    }
    // A shot with the saver holds the stream too: a job due again once it is
    // saved would otherwise restart the camera, and warm it up, every shot.
    if *started
        && !forwarding
        && !jobs
            .iter()
            .any(|j| j.info.active() && (j.deadline.is_some() || j.saving))
    {
        if let Some(c) = camera.as_mut() {
            let _ = c.stop();
        }
        *started = false;
        changed = true;
    }
    if changed {
        jobs_changed(h, camera, jobs);
    }
}
/// Publish changed jobs. Scanners also read their features again, since a
/// scan changes them.
fn jobs_changed(h: &WorkerHandle, camera: &mut Option<Camera>, jobs: &mut Vec<Scheduled>) {
    trim_jobs(jobs);
    sync_jobs(h, jobs);
    let scanner = camera.as_ref().is_some_and(Camera::is_scanner);
    refresh(h, camera, scanner);
}
/// Shoot the frame for every job due now: the saver encodes and writes its
/// own copy. Without `room` the saver is full, and due jobs skip to their next
/// interval instead of holding up acquisition.
fn shoot(
    h: &WorkerHandle,
    frame: &Frame,
    jobs: &mut [Scheduled],
    settled: bool,
    room: bool,
) -> Option<saver::Shot> {
    let now = crate::scheduling::now_ms();
    let mut saves = Vec::new();
    let mut changed = false;
    for job in jobs.iter_mut().filter(|j| {
        j.info.active()
            && j.deadline.is_some()
            && j.info.next_at_ms <= now
            && (settled || j.warm.is_none_or(|w| Instant::now() >= w))
    }) {
        changed = true;
        job.deadline = None;
        job.warm = None;
        let picked = job.next.take();
        if !room {
            job.info.skipped += 1;
            job.info.status = "pending".into();
            job.advance();
            if job.info.skipped == 1 {
                h.log(
                    "warn",
                    format!(
                        "Capture job {} is skipping shots while earlier ones are still being saved",
                        job.info.id
                    ),
                );
            }
            continue;
        }
        let named = match picked {
            Some(picked) => Ok(picked),
            None => job.allocate(&h.names),
        };
        match named {
            Ok((path, at)) => {
                saves.push(job.save(path, at));
                job.saving = true;
                job.advance();
                job.info.status = if job.info.captured + 1 < job.info.count {
                    "pending"
                } else {
                    "running"
                }
                .into();
            }
            Err(e) => {
                job.info.status = "failed".into();
                job.info.error = Some(format!("{e:#}"));
                h.log(
                    "error",
                    format!("Capture job {} stopped: {e:#}", job.info.id),
                );
            }
        }
    }
    if changed {
        sync_jobs(h, jobs);
    }
    (!saves.is_empty()).then(|| saver::Shot {
        frame: frame.clone(),
        wall_ms: now,
        saves,
    })
}
/// Apply the shots the saver finished to their jobs; whether any changed.
fn saved(h: &WorkerHandle, jobs: &mut [Scheduled], saver: &saver::Saver) -> bool {
    let mut changed = false;
    loop {
        match saver.saved.try_recv() {
            Ok(saved) => {
                if let Some(job) = jobs.iter_mut().find(|job| job.info.id == saved.job) {
                    finish(h, job, saved.result);
                    changed = true;
                }
            }
            Err(mpsc::TryRecvError::Empty) => return changed,
            // Its thread died, so no job can save another shot.
            Err(mpsc::TryRecvError::Disconnected) => {
                for job in jobs
                    .iter_mut()
                    .filter(|job| job.saving || job.info.active())
                {
                    finish(h, job, Err(anyhow::anyhow!("capture saver stopped")));
                    changed = true;
                }
                return changed;
            }
        }
    }
}
/// Wait for the shots with the saver and apply them, as before a disconnect:
/// their files then ride its reply to the coordinator, which catalogs them.
fn settle_saves(h: &WorkerHandle, saver: &mut saver::Saver, jobs: &mut [Scheduled]) {
    let finished = std::mem::replace(saver, saver::Saver::new(h.clone())).join();
    for done in finished.try_iter() {
        if let Some(job) = jobs.iter_mut().find(|job| job.info.id == done.job) {
            finish(h, job, done.result);
        }
    }
}
/// Record what became of a job's shot. A save that may work later is taken
/// again at the job's next interval; any other failure stops the job.
fn finish(h: &WorkerHandle, job: &mut Scheduled, result: Result<String>) {
    job.saving = false;
    let id = job.info.id;
    match result {
        Ok(path) => {
            job.info.captured += 1;
            job.info.last_file = Some(path);
            if job.info.active() {
                job.info.error = None;
                if job.info.captured >= job.info.count {
                    job.info.status = "complete".into();
                    h.log(
                        "info",
                        format!("Capture job {id} complete: {} frame(s)", job.info.captured),
                    );
                }
            }
        }
        Err(e) if !job.info.active() => h.log("error", format!("Capture job {id}: {e:#}")),
        Err(e) if transient(&e) => {
            job.info.retries += 1;
            job.info.status = "pending".into();
            job.info.error = Some(format!("{e:#}"));
            h.log(
                "warn",
                format!("Capture job {id} retries at its next interval: {e:#}"),
            );
        }
        Err(e) => {
            job.info.status = "failed".into();
            job.info.error = Some(format!("{e:#}"));
            h.log("error", format!("Capture job {id} stopped: {e:#}"));
        }
    }
}
/// A failed save that may work later: the storage ledger stayed locked, or
/// the disk, such as a network share, timed out.
fn transient(error: &anyhow::Error) -> bool {
    crate::storage::is_busy(error)
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
        })
}

fn fail_scanner_jobs(h: &WorkerHandle, jobs: &mut [Scheduled], error: &anyhow::Error) {
    let error = format!("{error:#}");
    for job in jobs
        .iter_mut()
        .filter(|job| job.info.active() && job.deadline.is_some())
    {
        job.info.status = "failed".into();
        job.info.error = Some(error.clone());
        job.deadline = None;
        job.warm = None;
        h.log(
            "error",
            format!("Capture job {} failed: {error}", job.info.id),
        );
    }
    sync_jobs(h, jobs);
}

fn output_path(output: &str, count: u32, index: u32, format: &str) -> PathBuf {
    if output.contains("{frame}") {
        PathBuf::from(output.replace("{frame}", &format!("{:06}", index + 1)))
    } else if count > 1 || PathBuf::from(output).is_dir() {
        PathBuf::from(output).join(format!("frame-{:06}.{format}", index + 1))
    } else {
        PathBuf::from(output)
    }
}

/// The longest the camera thread waits while it receives no frames.
/// Commands, saved shots and shutdown wake it sooner.
const IDLE: Duration = Duration::from_secs(1);
/// How often a frame the ring holds back is looked at while no frames arrive.
const TICK: Duration = Duration::from_millis(100);
/// How long the camera thread may wait for a command while it receives no
/// frames: until the next job is due, a requested frame times out or a timed
/// forward ends, even if its media source has already stopped.
fn idle_wait(h: &WorkerHandle, jobs: &[Scheduled]) -> Duration {
    let mut wait = if h.holding() { TICK } else { IDLE };
    let now = Instant::now();
    let now_ms = crate::scheduling::now_ms();
    {
        let state = h.state.read().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = state.forwarding_until_ms {
            let mut clock = h.forward_clock.lock().unwrap_or_else(|e| e.into_inner());
            let forward_ms = clock
                .as_mut()
                .filter(|clock| {
                    state.forwarding_id.as_deref() == Some(clock.forwarding_id.as_str())
                })
                .map_or(now_ms, |clock| clock.now_ms(now_ms, now));
            wait = wait.min(Duration::from_millis(until.saturating_sub(forward_ms)));
        }
    }
    jobs.iter()
        .filter(|job| job.info.active() && !job.saving)
        .map(|job| match job.deadline {
            Some(deadline) => deadline.saturating_duration_since(now),
            None => Duration::from_millis(job.info.next_at_ms.saturating_sub(now_ms)),
        })
        .fold(wait, Duration::min)
}
/// Publish the stream state and transport counters when they change, which
/// for GigE is with every frame.
fn sync_stream(h: &WorkerHandle, camera: &Option<Camera>) {
    let streaming = camera.as_ref().is_some_and(Camera::is_streaming);
    let transport = camera.as_ref().and_then(Camera::stats);
    let current = {
        let s = h.state.read().unwrap_or_else(|e| e.into_inner());
        s.streaming == streaming && s.transport == transport
    };
    if !current {
        let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
        s.streaming = streaming;
        s.transport = transport;
    }
}
fn worker(h: WorkerHandle, rx: Receiver<Option<Envelope>>) {
    let mut saver = saver::Saver::new(h.clone());
    let mut camera: Option<Camera> = None;
    let mut forwarder: Option<(Forwarder, f64)> = None;
    let mut auto: Option<AutoController> = None;
    let mut jobs: Vec<Scheduled> = Vec::new();
    let mut next_job = 1;
    let mut scheduled_stream = false;
    let mut rate_epoch = Instant::now();
    let mut rate_count = 0;
    let mut consecutive_errors = 0u32;
    let mut reconnecting = false;
    let mut progress_sampler = ForwardProgressSampler::default();
    while !h.cancelled() {
        let receiving = camera.as_ref().is_some_and(|c| {
            c.is_streaming()
                && (!c.is_scanner() || jobs.iter().any(|j| j.info.active() && j.deadline.is_some()))
        });
        let envelope = if receiving {
            rx.try_recv().ok()
        } else {
            rx.recv_timeout(idle_wait(&h, &jobs)).ok()
        };
        if let Some(e) = envelope.flatten() {
            if matches!(e.command, SessionCommand::Disconnect) {
                settle_saves(&h, &mut saver, &mut jobs);
            }
            // An inspection's failure is its caller's to report: a slow or
            // triggered camera must not fill the log or set `last_error`.
            let inspection = matches!(e.command, SessionCommand::Grab { .. });
            let result = process(
                &h,
                &mut camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next_job,
                &mut scheduled_stream,
                e.command,
            );
            if let Err(err) = &result
                && !inspection
                && !err.is::<ForwardChanged>()
            {
                h.log("error", format!("{err:#}"));
            }
            let _ = e.reply.send(result);
            rate_epoch = Instant::now();
            rate_count = 0;
            consecutive_errors = 0;
        }
        if saved(&h, &mut jobs, &saver) {
            jobs_changed(&h, &mut camera, &mut jobs);
        }
        tick_jobs(
            &h,
            &mut camera,
            &mut jobs,
            &mut scheduled_stream,
            forwarder.is_some(),
            auto.is_some(),
        );
        if forwarder.is_some() {
            finish_timed_forward(&h, &mut forwarder, &mut auto, crate::scheduling::now_ms());
        }
        if let (Some(controller), Some(c)) = (auto.as_mut(), camera.as_mut())
            && controller.tick(c, Instant::now())
        {
            publish_auto(&h, controller);
        }
        h.flush_ring();
        sync_stream(&h, &camera);
        sync_forwarding_progress(&h, &forwarder, &mut progress_sampler, Instant::now());
        let scheduled_scan = jobs.iter().any(|j| j.info.active() && j.deadline.is_some());
        if let Some(c) = camera
            .as_mut()
            .filter(|c| c.is_streaming() && (!c.is_scanner() || scheduled_scan))
        {
            let asked = Instant::now();
            match c.next_frame(Duration::from_millis(100)) {
                Ok(frame) => {
                    if std::mem::take(&mut reconnecting) {
                        resumed(&h);
                    }
                    if let Some(controller) = auto.as_mut() {
                        controller.observe(&frame, Instant::now());
                    }
                    let settled = auto.as_ref().is_none_or(AutoController::converged);
                    // A scanned page is never skipped: it waits for room.
                    let room = c.is_scanner() || saver.has_room();
                    let shot = shoot(&h, &frame, &mut jobs, settled, room);
                    // Publish first: the forwarder then takes the sensor
                    // buffer itself instead of a copy of it, and the saver
                    // works on its own copy.
                    let forward = forwarder.is_some();
                    if let (Some((f, _)), Some(frame)) =
                        (forwarder.as_mut(), h.record(frame, forward))
                        && let Err(e) = f.push(frame)
                    {
                        let message = format!("Forwarding stopped: {e:#}");
                        let _ = end_forward(&h, &mut forwarder, &mut auto, |_| {
                            Some(("error", message))
                        });
                    }
                    if let Some(shot) = shot {
                        saver.offer(shot);
                    }
                    rate_count += 1;
                    consecutive_errors = 0;
                }
                Err(e) => match crate::media::stream_end(&e) {
                    Some(StreamEnd::Lost) => {
                        reconnecting = true;
                        h.log("error", format!("Stream lost; {e:#}"));
                    }
                    Some(end) => {
                        // The file ended, or its server refused every
                        // reconnect: stop instead of counting errors.
                        let _ = c.stop();
                        reconnecting = false;
                        if end == StreamEnd::Finished {
                            h.log("info", "Video file finished".into());
                        } else {
                            h.log("error", format!("Acquisition stopped: {e:#}"));
                        }
                        refresh(&h, &mut camera, false);
                    }
                    None if timed_out(&e) => {
                        // A source that fails fast must not turn this loop
                        // into a spin.
                        if asked.elapsed() < Duration::from_millis(10) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    None => {
                        consecutive_errors += 1;
                        h.counters.dropped.fetch_add(1, Ordering::Relaxed);
                        if c.is_scanner() {
                            // Scanner errors are terminal for the requested
                            // page: the backend has already discarded its
                            // helper, so another poll cannot produce it.
                            fail_scanner_jobs(&h, &mut jobs, &e);
                            let _ = c.stop();
                            scheduled_stream = false;
                            refresh(&h, &mut camera, true);
                            continue;
                        }
                        if consecutive_errors == 1 {
                            h.log("warn", format!("{e:#}"));
                        }
                        if consecutive_errors >= 10 {
                            let _ = c.stop();
                            h.log(
                                "error",
                                "Acquisition stopped after repeated transport errors".into(),
                            );
                            refresh(&h, &mut camera, false);
                        }
                    }
                },
            }
            if rate_epoch.elapsed() >= Duration::from_secs(1) {
                h.counters
                    .set_fps(rate_count as f64 / rate_epoch.elapsed().as_secs_f64());
                rate_epoch = Instant::now();
                rate_count = 0;
            }
        } else if std::mem::take(&mut reconnecting) {
            // Stopped or disconnected while the stream was being reopened.
            forget_lost(&h);
        }
    }
    drop(saver);
    let _ = stop_forward(&h, &mut forwarder, &mut auto);
    let _ = release_auto(&h, &mut auto, &mut camera, true);
    if let Some(mut c) = camera
        && let Err(e) = c.stop()
    {
        h.log("warn", format!("Acquisition cleanup: {e:#}"));
    }
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forwarding_progress_is_sampled_four_times_a_second_without_payload_details() {
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        let stats = || {
            calls.set(calls.get() + 1);
            crate::media::ForwardStats {
                accepted: 100,
                written: 91,
                dropped: 8,
                late: 7,
                stalled: true,
                encoder: "private-encoder-detail".into(),
                error: Some("private-error-detail".into()),
                ..Default::default()
            }
        };
        let mut sampler = ForwardProgressSampler::default();
        let now = Instant::now();
        let first = sampler.sample(Some("first"), now, stats).unwrap();
        assert_eq!(
            (
                first.accepted,
                first.written,
                first.dropped,
                first.late,
                first.stalled
            ),
            (100, 91, 8, 7, true)
        );
        for millis in [1, 50, 100, 249] {
            assert!(
                sampler
                    .sample(Some("first"), now + Duration::from_millis(millis), stats)
                    .is_none()
            );
        }
        assert_eq!(calls.get(), 1);
        assert!(
            sampler
                .sample(Some("first"), now + Duration::from_millis(250), stats)
                .is_some()
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(
            sampler
                .sample(Some("replacement"), now + Duration::from_millis(251), stats)
                .unwrap()
                .id,
            "replacement"
        );
        assert!(
            sampler
                .sample(None, now + Duration::from_secs(1), stats)
                .is_none()
        );
        assert_eq!(calls.get(), 3);
        let encoded = serde_json::to_string(&first).unwrap();
        assert!(
            !encoded.contains("encoder")
                && !encoded.contains("error")
                && !encoded.contains("recording")
        );
    }

    #[test]
    fn ended_streams_count_as_errors_even_when_they_mention_timeouts() {
        use crate::types::{Categorized, ErrorKind, FrameWait};
        let ended: anyhow::Error = crate::media::StreamEnded(
            "media stream ended: Error during demuxing: Operation timed out".into(),
        )
        .into();
        assert!(!timed_out(&ended));
        assert!(!timed_out(&ended.context("read rtsp://camera/live")));
        assert!(timed_out(&FrameWait.into()));
        assert!(timed_out(
            &anyhow::Error::from(FrameWait).context("hub:home/camera.timeout")
        ));
        assert!(timed_out(&Categorized::error(
            ErrorKind::Timeout,
            "no answer"
        )));
        assert!(!timed_out(&Categorized::error(
            ErrorKind::Unavailable,
            "camera camera.timeout on hub home is offline (HTTP 503)"
        )));
        // Backends that predate the typed errors keep their meaning.
        assert!(timed_out(&anyhow::anyhow!("frame timeout")));
        assert!(timed_out(&anyhow::anyhow!(
            "media frame capture timed out: x"
        )));
        assert!(!timed_out(&anyhow::anyhow!("truncated GVSP frame")));
    }
    #[test]
    fn active_jobs_reject_overlapping_output_files() {
        let handle = WorkerHandle::new();
        handle
            .request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "capturefab-job-conflict-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        let schedule = |output: String, count: u32, format: &str| SessionCommand::Schedule {
            output,
            count,
            timeout_ms: 100,
            format: format.into(),
            first_at_ms: crate::scheduling::now_ms() + 60_000,
            interval_ms: 1000,
            storage: Default::default(),
            destination: None,
            on_exists: Default::default(),
            context: Default::default(),
        };
        let frames = root.join("f-{frame}.raw").to_string_lossy().into_owned();
        handle.request(schedule(frames.clone(), 3, "raw")).unwrap();
        for (output, count, format) in [
            (frames.clone(), 2, "raw"),
            (
                root.join("f-000002.raw").to_string_lossy().into_owned(),
                1,
                "png",
            ),
        ] {
            let error = handle.request(schedule(output, count, format)).unwrap_err();
            assert!(error.to_string().contains("already scheduled"), "{error:#}");
        }
        // Directories and {seq} templates pick a free name per frame, so
        // jobs can share them with each other and with static names.
        let directory = root.to_string_lossy().into_owned();
        let seq = root.join("s-{seq}.raw").to_string_lossy().into_owned();
        for output in [directory.clone(), directory, seq.clone(), seq] {
            handle.request(schedule(output, 2, "raw")).unwrap();
        }
        assert_eq!(
            handle.request(SessionCommand::Jobs).unwrap()["jobs"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert!(!root.exists());
        handle.shutdown();
    }
    #[test]
    fn recordings_reserve_their_seq_name_before_the_first_frame() {
        // Two cameras' workers have separate namers; the name must be taken
        // when it is picked, or both would record to rec-0001.
        let root = std::env::temp_dir().join(format!(
            "capturefab-rec-reserve-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("rec-{seq}.mkv").to_string_lossy().into_owned();
        let forward = || SessionCommand::Forward {
            output: output.clone(),
            codec: "h264".into(),
            encoder: "auto".into(),
            fps: 5.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: 1 << 20,
            destination: None,
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms: None,
        };
        let mut paths = Vec::new();
        let handles: Vec<_> = (0..2).map(|_| WorkerHandle::new()).collect();
        for h in &handles {
            h.request(SessionCommand::connect("sim:0".into(), 100))
                .unwrap();
            let reply = h.request(forward()).unwrap();
            paths.push(reply["path"].as_str().unwrap().to_owned());
        }
        assert_ne!(paths[0], paths[1]);
        assert!(paths[0].ends_with("rec-0001.mkv"), "{paths:?}");
        assert!(paths[1].ends_with("rec-0002.mkv"), "{paths:?}");
        for h in handles {
            let _ = h.request(SessionCommand::StopForward);
            h.shutdown();
        }
        let _ = std::fs::remove_dir_all(root);
    }
    fn timed_forward(output: &Path, duration_ms: Option<u64>) -> SessionCommand {
        SessionCommand::Forward {
            output: output.to_string_lossy().into_owned(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 10.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: 8 << 20,
            destination: None,
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms,
        }
    }
    #[test]
    fn recording_clock_keeps_elapsed_time_across_wall_clock_rollbacks() {
        let epoch = Instant::now();
        let mut clock = ForwardClock {
            forwarding_id: "11111111111111111111111111111111".into(),
            anchor_ms: 1_000,
            anchor: epoch,
        };
        assert_eq!(clock.now_ms(500, epoch + Duration::from_secs(1)), 2_000);
        // A forward wall jump counts time spent asleep even on a platform
        // whose Instant does not advance during system sleep.
        assert_eq!(clock.now_ms(20_000, epoch + Duration::from_secs(1)), 20_000);
        assert_eq!(clock.now_ms(0, epoch + Duration::from_secs(2)), 21_000);
        assert_eq!(clock.now_ms(0, epoch + Duration::from_secs(3)), 22_000);
        assert_eq!(
            clock.now_ms(u64::MAX, epoch + Duration::from_secs(4)),
            u64::MAX
        );
        assert_eq!(clock.now_ms(0, epoch + Duration::from_secs(5)), u64::MAX);
    }

    #[test]
    fn timed_forward_ends_after_credited_sleep_even_if_wall_time_rolls_back() {
        // Commands run without an acquisition loop: no frame reaches the
        // encoder, so neither FFmpeg nor a network connection is involved.
        let h = WorkerHandle::new();
        let mut camera = None;
        let mut forwarder = None;
        let mut auto = None;
        let mut run =
            |camera: &mut Option<Camera>, forwarder: &mut Option<(Forwarder, f64)>, command| {
                process(
                    &h,
                    camera,
                    forwarder,
                    &mut auto,
                    &mut Vec::new(),
                    &mut 1,
                    &mut false,
                    command,
                )
            };
        run(
            &mut camera,
            &mut forwarder,
            SessionCommand::connect("sim:0".into(), 100),
        )
        .unwrap();
        let target = Path::new("udp://127.0.0.1:9");
        let reply = run(
            &mut camera,
            &mut forwarder,
            timed_forward(target, Some(2_000)),
        )
        .unwrap();
        let since = reply["since_ms"].as_u64().unwrap();
        let clock = h.forward_clock.lock().unwrap().clone().unwrap();
        assert_eq!(reply["forwarding_id"], clock.forwarding_id);
        finish_timed_forward_at(&h, &mut forwarder, &mut None, since + 1_000, clock.anchor);
        assert!(forwarder.is_some());
        finish_timed_forward_at(
            &h,
            &mut forwarder,
            &mut None,
            since - 1_000,
            clock.anchor + Duration::from_millis(999),
        );
        assert!(
            forwarder.is_some(),
            "not before its remaining elapsed duration"
        );
        finish_timed_forward_at(
            &h,
            &mut forwarder,
            &mut None,
            since - 1_000,
            clock.anchor + Duration::from_secs(1),
        );
        assert!(
            forwarder.is_none(),
            "a wall rollback cannot extend the credited duration"
        );
        assert!(h.forward_clock.lock().unwrap().is_none());
        assert!(h.snapshot().forwarding_until_ms.is_none());
        assert_eq!(
            h.snapshot().logs.last().unwrap().message,
            "Forwarding finished after 2 s"
        );
        run(&mut camera, &mut forwarder, timed_forward(target, None)).unwrap();
        assert!(
            h.forward_clock.lock().unwrap().is_none(),
            "untimed forwarding has no deadline clock"
        );
        finish_timed_forward_at(
            &h,
            &mut forwarder,
            &mut None,
            u64::MAX,
            clock.anchor + Duration::from_secs(10),
        );
        assert!(forwarder.is_some());
        run(&mut camera, &mut forwarder, SessionCommand::StopForward).unwrap();
        h.shutdown();
    }

    #[test]
    fn stale_conditional_stop_does_not_report_a_camera_or_recording_failure() {
        let h = WorkerHandle::new();
        let current = "11111111111111111111111111111111";
        // A cached active report is enough to exercise the worker's actual
        // reply/error path; no camera or encoder is opened by this fixture.
        {
            let mut state = h.state.write().unwrap();
            state.forwarding = Some("clip.mkv".into());
            state.forwarding_id = Some(current.into());
            state.last_error = Some("an earlier unrelated error".into());
        }
        let before = h.snapshot();
        let error = h
            .request(SessionCommand::StopForwardIf {
                forwarding_id: "22222222222222222222222222222222".into(),
            })
            .unwrap_err();
        assert!(error.is::<ForwardChanged>());
        let after = h.snapshot();
        assert_eq!(after.forwarding_id.as_deref(), Some(current));
        assert_eq!(after.last_error, before.last_error);
        assert_eq!(after.logs.len(), before.logs.len());
        // Ordinary camera errors still reach last_error; the exemption is
        // specific to a stale identity, not every conditional stop failure.
        let error = h
            .request(SessionCommand::Set {
                feature: "Gain".into(),
                value: "1".into(),
            })
            .unwrap_err();
        assert!(!error.is::<ForwardChanged>());
        let after = h.snapshot();
        assert_eq!(after.logs.last().unwrap().level, "error");
        assert_ne!(after.last_error, before.last_error);
        h.shutdown();
    }

    #[test]
    fn conditional_stop_preserves_a_replacement_with_the_same_start_time_and_output() {
        // No acquisition loop runs here, so the encoder never starts and
        // no FFmpeg, socket connection or saved recording is needed.
        let h = WorkerHandle::new();
        let mut camera = None;
        let mut forwarder = None;
        let mut auto = None;
        let mut run =
            |camera: &mut Option<Camera>, forwarder: &mut Option<(Forwarder, f64)>, command| {
                process(
                    &h,
                    camera,
                    forwarder,
                    &mut auto,
                    &mut Vec::new(),
                    &mut 1,
                    &mut false,
                    command,
                )
            };
        run(
            &mut camera,
            &mut forwarder,
            SessionCommand::connect("sim:0".into(), 100),
        )
        .unwrap();
        let target = Path::new("udp://127.0.0.1:9");
        let first = run(
            &mut camera,
            &mut forwarder,
            timed_forward(target, Some(60_000)),
        )
        .unwrap();
        let first_id = first["forwarding_id"].as_str().unwrap().to_owned();
        assert!(crate::session::valid_forwarding_id(&first_id));
        assert_eq!(
            h.snapshot().forwarding_progress.as_ref().unwrap().id,
            first_id
        );
        assert_eq!(
            h.forward_clock
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .forwarding_id,
            first_id
        );
        run(
            &mut camera,
            &mut forwarder,
            SessionCommand::StopForwardIf {
                forwarding_id: first_id.clone(),
            },
        )
        .unwrap();
        assert!(h.snapshot().forwarding_id.is_none());
        assert_eq!(
            h.snapshot().forwarding_progress.as_ref().unwrap().id,
            first_id
        );
        assert!(h.forward_clock.lock().unwrap().is_none());
        let next = run(
            &mut camera,
            &mut forwarder,
            timed_forward(target, Some(120_000)),
        )
        .unwrap();
        let next_id = next["forwarding_id"].as_str().unwrap().to_owned();
        assert_ne!(first_id, next_id);
        assert_eq!(
            h.snapshot().forwarding_progress.as_ref().unwrap().id,
            next_id
        );
        let replacement_clock = h.forward_clock.lock().unwrap().clone().unwrap();
        assert_eq!(replacement_clock.forwarding_id, next_id);
        assert_eq!(
            replacement_clock.anchor_ms,
            next["since_ms"].as_u64().unwrap()
        );
        {
            let mut state = h.state.write().unwrap();
            state.forwarding_since_ms = first["since_ms"].as_u64();
        }
        let before = h.snapshot();
        let error = run(
            &mut camera,
            &mut forwarder,
            SessionCommand::StopForwardIf {
                forwarding_id: first_id,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("current recording was preserved")
        );
        let after = h.snapshot();
        assert!(forwarder.is_some());
        assert_eq!(after.forwarding, before.forwarding);
        assert_eq!(after.forwarding_id, before.forwarding_id);
        assert_eq!(after.forwarding_progress, before.forwarding_progress);
        assert_eq!(after.forwarding_until_ms, before.forwarding_until_ms);
        let after_clock = h.forward_clock.lock().unwrap().clone().unwrap();
        assert_eq!(after_clock.forwarding_id, replacement_clock.forwarding_id);
        assert_eq!(after_clock.anchor, replacement_clock.anchor);
        assert_eq!(after_clock.anchor_ms, replacement_clock.anchor_ms);
        run(
            &mut camera,
            &mut forwarder,
            SessionCommand::StopForwardIf {
                forwarding_id: next_id.clone(),
            },
        )
        .unwrap();
        let stopped = h.snapshot();
        assert!(forwarder.is_none());
        assert!(stopped.forwarding_id.is_none());
        assert!(stopped.forwarding_until_ms.is_none());
        assert_eq!(stopped.forwarding_progress.as_ref().unwrap().id, next_id);
        assert!(h.forward_clock.lock().unwrap().is_none());
        h.shutdown();
    }
    #[test]
    fn timed_forwards_keep_their_end_time_until_stopped() {
        // The deadline falls due at its end time, never before.
        let state = |since, until| SessionSnapshot {
            forwarding: Some("clip.mkv".into()),
            forwarding_since_ms: since,
            forwarding_until_ms: until,
            ..Default::default()
        };
        assert_eq!(forward_due(&state(Some(1000), None), u64::MAX), None);
        assert_eq!(forward_due(&state(Some(1000), Some(4600)), 4599), None);
        assert_eq!(
            forward_due(&state(Some(1000), Some(4600)), 4600),
            Some(3600)
        );
        assert_eq!(
            forward_due(&state(Some(1000), Some(4600)), 9000),
            Some(3600)
        );
        assert_eq!(forward_due(&state(None, Some(4600)), 4600), Some(0));
        // Commands run here without the worker loop, so no frame reaches the
        // encoder: FFmpeg, which starts with the first frame, is not needed,
        // and only these commands or the deadline end the forward.
        let root = temp_dir("timed-forward");
        let h = WorkerHandle::new();
        let mut camera = None;
        let mut forwarder = None;
        let mut auto = None;
        let mut run =
            |camera: &mut Option<Camera>, forwarder: &mut Option<(Forwarder, f64)>, command| {
                process(
                    &h,
                    camera,
                    forwarder,
                    &mut auto,
                    &mut Vec::new(),
                    &mut 1,
                    &mut false,
                    command,
                )
            };
        let connect = || SessionCommand::connect("sim:0".into(), 100);
        run(&mut camera, &mut forwarder, connect()).unwrap();
        let hour = Some(3_600_000);
        // Stop-forward, stop and disconnect each clear the end time.
        for (name, stop) in [
            ("stop-forward", SessionCommand::StopForward),
            ("stop", SessionCommand::Stop),
            ("disconnect", SessionCommand::Disconnect),
        ] {
            let output = root.join(format!("{name}.mkv"));
            let reply = run(&mut camera, &mut forwarder, timed_forward(&output, hour)).unwrap();
            let since = reply["since_ms"].as_u64().unwrap();
            assert_eq!(reply["until_ms"].as_u64(), Some(since + 3_600_000));
            let snapshot = h.snapshot();
            assert_eq!(
                (snapshot.forwarding_since_ms, snapshot.forwarding_until_ms),
                (Some(since), Some(since + 3_600_000)),
                "{name}"
            );
            run(&mut camera, &mut forwarder, stop).unwrap();
            let snapshot = h.snapshot();
            assert!(forwarder.is_none(), "{name}");
            assert_eq!(
                (
                    snapshot.forwarding,
                    snapshot.forwarding_since_ms,
                    snapshot.forwarding_until_ms
                ),
                (None, None, None),
                "{name}"
            );
            assert_eq!(snapshot.last_error, None, "{name}");
            if camera.is_none() {
                run(&mut camera, &mut forwarder, connect()).unwrap();
            }
        }
        let reply = run(
            &mut camera,
            &mut forwarder,
            timed_forward(&root.join("untimed.mkv"), None),
        )
        .unwrap();
        assert!(reply.get("until_ms").is_none(), "{reply}");
        assert_eq!(h.snapshot().forwarding_until_ms, None);
        finish_timed_forward(&h, &mut forwarder, &mut None, u64::MAX);
        assert!(forwarder.is_some(), "a forward without a duration runs on");
        run(&mut camera, &mut forwarder, SessionCommand::StopForward).unwrap();
        // At its end time the worker ends a timed forward as stop-forward
        // does, and says so in the step that clears it.
        let reply = run(
            &mut camera,
            &mut forwarder,
            timed_forward(&root.join("due.mkv"), Some(90_000)),
        )
        .unwrap();
        let until = reply["until_ms"].as_u64().unwrap();
        finish_timed_forward(&h, &mut forwarder, &mut None, until - 1);
        assert!(forwarder.is_some(), "not before its end time");
        finish_timed_forward(&h, &mut forwarder, &mut None, until);
        assert!(forwarder.is_none());
        let snapshot = h.snapshot();
        assert_eq!(
            (snapshot.forwarding, snapshot.forwarding_until_ms),
            (None, None)
        );
        assert_eq!(snapshot.last_error, None);
        assert_eq!(
            snapshot.logs.last().map(|entry| entry.message.as_str()),
            Some("Recording finished after 1 min 30 s")
        );
        h.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn status_names_the_file_a_recording_template_resolved_to() {
        // No worker loop runs, so no frame reaches the encoder and FFmpeg
        // is not needed; naming happens before it would start.
        let root = temp_dir("recording-path");
        let h = WorkerHandle::new();
        let mut camera = None;
        let mut forwarder = None;
        let mut auto = None;
        let mut run =
            |camera: &mut Option<Camera>, forwarder: &mut Option<(Forwarder, f64)>, command| {
                process(
                    &h,
                    camera,
                    forwarder,
                    &mut auto,
                    &mut Vec::new(),
                    &mut 1,
                    &mut false,
                    command,
                )
            };
        run(
            &mut camera,
            &mut forwarder,
            SessionCommand::connect("sim:0".into(), 100),
        )
        .unwrap();
        let template = root.join("{camera}-{seq}.mkv");
        let reply = run(&mut camera, &mut forwarder, timed_forward(&template, None)).unwrap();
        let path = reply["path"].as_str().unwrap();
        assert!(!path.contains('{'), "{path}");
        let snapshot = h.snapshot();
        assert_eq!(snapshot.recording_path.as_deref(), Some(path));
        // `forwarding` still says what was asked for.
        assert_eq!(
            snapshot.forwarding.as_deref(),
            Some(template.to_string_lossy().as_ref())
        );
        run(&mut camera, &mut forwarder, SessionCommand::StopForward).unwrap();
        assert_eq!(h.snapshot().recording_path, None);
        // A network forward names no file.
        let target = Path::new("udp://127.0.0.1:9");
        run(&mut camera, &mut forwarder, timed_forward(target, None)).unwrap();
        assert_eq!(h.snapshot().recording_path, None);
        run(&mut camera, &mut forwarder, SessionCommand::StopForward).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    #[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
    fn a_timed_recording_ends_itself_and_commits_its_file() {
        let root = temp_dir("timed-recording");
        let output = root.join("clip.mkv");
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let reply = h.request(timed_forward(&output, Some(1000))).unwrap();
        let until = reply["until_ms"].as_u64().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let ended = loop {
            let snapshot = h.snapshot();
            if snapshot.forwarding.is_none() {
                break snapshot;
            }
            assert!(Instant::now() < deadline, "the recording never ended");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(crate::scheduling::now_ms() >= until);
        // Once the forward is gone, its file is finished and reported.
        let batch = h.take_saved(10);
        assert_eq!(batch.items.len(), 1);
        assert!(matches!(
            batch.items[0].item,
            crate::dataset::SavedItem::Recording { .. }
        ));
        assert_eq!(
            std::fs::metadata(&output).unwrap().len(),
            batch.items[0].bytes
        );
        // The first status that shows it gone already says how it ended.
        assert_eq!(
            (ended.forwarding_since_ms, ended.forwarding_until_ms),
            (None, None)
        );
        assert!(
            ended
                .logs
                .iter()
                .any(|entry| entry.message == "Recording finished after 1 s"),
            "{:?}",
            ended.logs
        );
        assert_eq!(ended.last_error, None);
        // Nothing is left to stop.
        assert_eq!(
            h.request(SessionCommand::StopForward).unwrap()["forwarding"],
            Value::Null
        );
        h.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn session_simulator_configuration_and_acquisition() {
        let h = WorkerHandle::new();
        h.request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        h.request(SessionCommand::Set {
            feature: "Width".into(),
            value: "128".into(),
        })
        .unwrap();
        assert_eq!(
            h.request(SessionCommand::Get {
                feature: "Width".into()
            })
            .unwrap()["value"],
            128
        );
        assert!(
            h.request(SessionCommand::Set {
                feature: "Width".into(),
                value: "1".into()
            })
            .is_err()
        );
        let v = h
            .request(SessionCommand::Capture {
                output: "-".into(),
                count: 1,
                timeout_ms: 1000,
                format: "raw".into(),
                storage: Default::default(),
                destination: None,
                on_exists: Default::default(),
                context: Default::default(),
            })
            .unwrap();
        assert_eq!(v["frames"][0]["width"], 128);
        assert!(!h.snapshot().streaming);
        assert!(h.latest_frame().is_some());
        h.request(SessionCommand::Disconnect).unwrap();
        assert!(h.snapshot().connected.is_none());
        h.shutdown();
    }
    #[test]
    fn captures_reach_an_unwatched_ring_and_oversize_frames_log_once() {
        let writer = crate::shared_memory::SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        let h = WorkerHandle::with_ring(Arc::new(Mutex::new(writer)));
        let size = |side: &str| {
            for feature in ["Width", "Height"] {
                h.request(SessionCommand::Set {
                    feature: feature.into(),
                    value: side.into(),
                })
                .unwrap();
            }
        };
        h.request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        size("64");
        // Nobody reads while it streams, so only a keep-alive is published,
        // and without force the capture would be skipped as well.
        h.request(SessionCommand::Start).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        h.request(SessionCommand::Stop).unwrap();
        let v = h
            .request(SessionCommand::Capture {
                output: "-".into(),
                count: 1,
                timeout_ms: 1000,
                format: "raw".into(),
                storage: Default::default(),
                destination: None,
                on_exists: Default::default(),
                context: Default::default(),
            })
            .unwrap();
        let captured = reader.take_latest().unwrap().unwrap();
        assert_eq!(Some(captured.id), v["frames"][0]["id"].as_u64());
        assert_eq!(captured.data.len(), 4096);
        // A watched stream of frames larger than the ring previews every
        // frame downscaled and says so once.
        size("128");
        h.request(SessionCommand::Start).unwrap();
        let until = Instant::now() + Duration::from_millis(400);
        let mut previews = 0;
        while Instant::now() < until {
            if reader.take_latest().unwrap().is_some() {
                assert!(reader.preview_only());
                previews += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        h.request(SessionCommand::Stop).unwrap();
        assert!(previews > 1, "{previews} previews");
        let snapshot = h.snapshot();
        let notes = snapshot
            .logs
            .iter()
            .filter(|entry| entry.message.contains("preview buffer"))
            .count();
        assert_eq!(notes, 1, "{:?}", snapshot.logs);
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        h.shutdown();
    }
    #[test]
    fn a_stream_nobody_watched_still_ends_on_its_newest_frame() {
        let writer = crate::shared_memory::SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        let h = WorkerHandle::with_ring(Arc::new(Mutex::new(writer)));
        h.request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        for feature in ["Width", "Height"] {
            h.request(SessionCommand::Set {
                feature: feature.into(),
                value: "64".into(),
            })
            .unwrap();
        }
        h.request(SessionCommand::Start).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        h.request(SessionCommand::Stop).unwrap();
        // Only the first frame was copied. A viewer that turns up after the
        // stop still gets the last one, although no frame follows it.
        let until = Instant::now() + Duration::from_secs(1);
        let mut newest = 0;
        while !reader.fresh() && Instant::now() < until {
            if let Some(frame) = reader.take_latest().unwrap() {
                newest = frame.id;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(reader.fresh(), "still showing frame {newest}");
        assert!(newest > 1, "{newest}");
        h.shutdown();
    }
    #[test]
    fn saved_captures_are_reported_with_context() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let root = std::env::temp_dir().join(format!(
            "capturefab-saved-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        let context = crate::context::Context::from([("line".into(), "L1".into())]);
        let v = h
            .request(SessionCommand::Capture {
                output: root.join("shot.raw").to_string_lossy().into_owned(),
                count: 1,
                timeout_ms: 1000,
                format: "raw".into(),
                storage: Default::default(),
                destination: None,
                on_exists: Default::default(),
                context: context.clone(),
            })
            .unwrap();
        assert_eq!(v["context"]["line"], "L1");
        assert_eq!(v["frames"][0]["file"], v["files"][0]);
        let wall_ms = v["frames"][0]["wall_ms"].as_u64().unwrap();
        let batch = h.take_saved(10);
        assert_eq!(batch.items.len(), 1);
        let saved = &batch.items[0];
        assert_eq!((saved.seq, saved.wall_ms), (1, wall_ms));
        assert_eq!(saved.context, context);
        assert_eq!(saved.bytes, 128 * 128);
        assert!(saved.sha256.as_ref().is_some_and(|s| s.len() == 64));
        assert!(std::path::Path::new(&saved.path).is_absolute());
        assert!(matches!(
            saved.item,
            crate::dataset::SavedItem::Capture { ref origin, job_id: None, .. } if origin == "capture"
        ));
        assert_eq!(batch.camera.unwrap().id, "sim:0");
        assert!(h.take_saved(10).is_empty());
        h.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn capture_to(output: &Path, count: u32, on_exists: crate::naming::OnExists) -> SessionCommand {
        SessionCommand::Capture {
            output: output.to_string_lossy().into_owned(),
            count,
            timeout_ms: 1000,
            format: "raw".into(),
            storage: Default::default(),
            destination: None,
            on_exists,
            context: Default::default(),
        }
    }
    #[test]
    fn repeated_captures_pick_free_names() {
        use crate::naming::OnExists;
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let dir = temp_dir("names");
        let label = crate::naming::camera_label(&h.snapshot().connected.unwrap());
        let files = |v: Value| -> Vec<String> {
            v["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    Path::new(f.as_str().unwrap())
                        .strip_prefix(&dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        };
        let seq = dir.join("{camera}-{seq}.raw");
        for n in 1..=2 {
            let v = h.request(capture_to(&seq, 1, OnExists::Fail)).unwrap();
            assert_eq!(files(v), [format!("{label}-{n:04}.raw")]);
        }
        let file = dir.join("shot.raw");
        h.request(capture_to(&file, 1, OnExists::Fail)).unwrap();
        let error = h.request(capture_to(&file, 1, OnExists::Fail)).unwrap_err();
        assert!(
            error.to_string().contains("output already exists"),
            "{error:#}"
        );
        let v = h.request(capture_to(&file, 1, OnExists::Number)).unwrap();
        assert_eq!(files(v), ["shot-0001.raw"]);
        // A directory continues its numbering on every run.
        let frames = dir.join("frames");
        for run in 0..2 {
            let v = h.request(capture_to(&frames, 2, OnExists::Fail)).unwrap();
            assert_eq!(
                files(v),
                [1, 2].map(|i| {
                    Path::new("frames")
                        .join(format!("frame-{:06}.raw", run * 2 + i))
                        .to_string_lossy()
                        .into_owned()
                })
            );
        }
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn scheduled_seq_jobs_save_distinct_files() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let dir = temp_dir("scheduled-seq");
        let job = h
            .request(SessionCommand::Schedule {
                output: dir.join("s-{seq}.raw").to_string_lossy().into_owned(),
                count: 3,
                timeout_ms: 1000,
                format: "raw".into(),
                first_at_ms: 0,
                interval_ms: 10,
                storage: Default::default(),
                destination: None,
                on_exists: Default::default(),
                context: crate::context::Context::from([("lot".into(), "7".into())]),
            })
            .unwrap();
        assert_eq!(job["job"]["context"]["lot"], "7");
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.snapshot().jobs.iter().any(|j| j.active()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(h.snapshot().jobs[0].status, "complete");
        let mut names = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["s-0001.raw", "s-0002.raw", "s-0003.raw"]);
        let batch = h.take_saved(10);
        assert_eq!(batch.items.len(), 3);
        assert!(batch.items.iter().all(|f| f.context["lot"] == "7"
            && matches!(
                f.item,
                crate::dataset::SavedItem::Capture {
                    job_id: Some(1),
                    ..
                }
            )));
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// A job armed for the next frame, saving two raw files into `dir`.
    fn armed(id: u64, dir: &Path) -> Scheduled {
        let output = dir
            .join(format!("job{id}-{{frame}}.raw"))
            .to_string_lossy()
            .into_owned();
        Scheduled {
            info: crate::scheduling::CaptureJob {
                id,
                status: "running".into(),
                output: output.clone(),
                destination: None,
                count: 2,
                captured: 0,
                skipped: 0,
                retries: 0,
                first_at_ms: 0,
                interval_ms: 60_000,
                next_at_ms: 0,
                last_file: None,
                error: None,
                context: Default::default(),
            },
            resolved: crate::destination::resolve(None, &output).unwrap(),
            plan: crate::naming::Plan::new(&output, 2, "raw"),
            timeout_ms: 1000,
            format: "raw".into(),
            storage: Default::default(),
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            warm: None,
            camera: "camera".into(),
            on_exists: Default::default(),
            next: None,
            saving: false,
        }
    }
    fn small_frame(id: u64) -> Frame {
        Frame {
            id,
            width: 8,
            height: 8,
            pixel_format: crate::types::MONO8,
            timestamp_ns: id,
            data: vec![id as u8; 64],
        }
    }
    #[test]
    fn slow_saves_hold_up_no_frames_and_the_saver_holds_two() {
        fn slow(_: &WorkerHandle, _: &saver::Shot, save: &saver::Save) -> Result<String> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(save.path.to_string_lossy().into_owned())
        }
        let writer = crate::shared_memory::SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        let h = WorkerHandle::with_ring(Arc::new(Mutex::new(writer)));
        let mut saver = saver::Saver::with(h.clone(), slow);
        let dir = temp_dir("slow-saves");
        let mut jobs = (1..=3).map(|id| armed(id, &dir)).collect::<Vec<_>>();
        for job in &mut jobs[1..] {
            job.deadline = None;
        }
        // A watching reader gets every frame.
        assert!(reader.take_latest().unwrap().is_none());
        for id in 1..=3u64 {
            // Each frame finds one more job due.
            let job = &mut jobs[id as usize - 1];
            job.deadline
                .get_or_insert(Instant::now() + Duration::from_secs(10));
            let frame = small_frame(id);
            let started = Instant::now();
            let shot = shoot(&h, &frame, &mut jobs, true, saver.has_room());
            h.record(frame, false);
            if let Some(shot) = shot {
                saver.offer(shot);
            }
            let took = started.elapsed();
            assert!(took < Duration::from_millis(5), "frame {id} took {took:?}");
            assert_eq!(reader.take_latest().unwrap().map(|f| f.id), Some(id));
        }
        // Two shots fill the saver; the third job skips to its next interval.
        assert!(jobs[0].saving && jobs[1].saving && !jobs[2].saving);
        assert_eq!(jobs[2].info.skipped, 1);
        assert_eq!(jobs[2].info.status, "pending");
        assert_eq!(jobs[2].info.next_at_ms, 60_000);
        assert!(jobs[2].deadline.is_none());
        let until = Instant::now() + Duration::from_secs(5);
        while jobs.iter().any(|job| job.saving) && Instant::now() < until {
            saved(&h, &mut jobs, &saver);
            std::thread::sleep(Duration::from_millis(10));
        }
        for job in &jobs[..2] {
            assert_eq!(
                (job.info.captured, job.info.status.as_str()),
                (1, "pending")
            );
            assert!(
                job.info
                    .last_file
                    .as_ref()
                    .unwrap()
                    .ends_with("-000001.raw")
            );
            assert_eq!(job.info.next_at_ms, 60_000);
        }
        drop(saver);
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_disconnect_waits_for_the_shot_the_saver_is_writing() {
        fn slow(_: &WorkerHandle, _: &saver::Shot, save: &saver::Save) -> Result<String> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(save.path.to_string_lossy().into_owned())
        }
        let h = WorkerHandle::new();
        let mut saver = saver::Saver::with(h.clone(), slow);
        let dir = temp_dir("disconnect-saves");
        let mut jobs = vec![armed(1, &dir)];
        let shot = shoot(&h, &small_frame(1), &mut jobs, true, saver.has_room()).unwrap();
        saver.offer(shot);
        assert!(jobs[0].saving);
        settle_saves(&h, &mut saver, &mut jobs);
        assert!(!jobs[0].saving);
        assert_eq!(jobs[0].info.captured, 1);
        assert!(
            jobs[0]
                .info
                .last_file
                .as_ref()
                .unwrap()
                .ends_with("-000001.raw")
        );
        // The saver takes shots again afterwards.
        assert!(saver.has_room());
        drop(saver);
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// A simulator that counts how often its stream starts and stops.
    struct Counted {
        simulator: crate::transport::simulator::Simulator,
        starts: Arc<AtomicU64>,
        stops: Arc<AtomicU64>,
    }
    impl crate::types::RegisterIo for Counted {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.simulator.read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.simulator.write_memory(address, data)
        }
    }
    impl crate::types::Backend for Counted {
        fn xml(&mut self) -> Result<String> {
            self.simulator.xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            self.simulator.start(payload)
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            self.simulator.next_frame(timeout)
        }
        fn stop(&mut self) -> Result<()> {
            self.stops.fetch_add(1, Ordering::Relaxed);
            self.simulator.stop()
        }
    }
    #[test]
    fn a_shot_being_saved_keeps_the_scheduled_stream_running() {
        let h = WorkerHandle::new();
        let (starts, stops) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Counted {
                    simulator: Default::default(),
                    starts: starts.clone(),
                    stops: stops.clone(),
                }),
            )
            .unwrap(),
        );
        let count = || {
            (
                starts.load(Ordering::Relaxed),
                stops.load(Ordering::Relaxed),
            )
        };
        let dir = temp_dir("saving-stream");
        // Due every 10 ms, which is sooner than a save.
        let mut job = armed(1, &dir);
        job.deadline = None;
        job.info.interval_ms = 10;
        let mut jobs = vec![job];
        let mut started = false;
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(started && jobs[0].deadline.is_some() && jobs[0].warm.is_some());
        let frame = camera
            .as_mut()
            .unwrap()
            .next_frame(Duration::from_secs(1))
            .unwrap();
        assert!(shoot(&h, &frame, &mut jobs, true, true).is_some());
        assert!(jobs[0].saving && jobs[0].info.next_at_ms <= crate::scheduling::now_ms());
        // Due again while its shot is saved: the stream keeps running.
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(started && camera.as_ref().unwrap().is_streaming());
        assert_eq!(count(), (1, 0));
        // Saved: the next shot needs no start and no warm-up.
        finish(&h, &mut jobs[0], Ok("shot.raw".into()));
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(jobs[0].deadline.is_some() && jobs[0].warm.is_none());
        assert_eq!(count(), (1, 0));
        // Nothing due or being saved: the schedule stops the stream it started.
        jobs[0].deadline = None;
        jobs[0].info.next_at_ms = u64::MAX;
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(!started && !camera.as_ref().unwrap().is_streaming());
        assert_eq!(count(), (1, 1));
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_busy_ledger_retries_the_shot_and_other_failures_stop_the_job() {
        let h = WorkerHandle::new();
        let dir = temp_dir("retry");
        let mut job = armed(1, &dir);
        let shoot_once = |job: &mut Scheduled, result: Result<String>| {
            job.saving = true;
            job.advance();
            finish(&h, job, result);
        };
        let busy = anyhow::Error::from(crate::storage::Busy).context("save shot.raw");
        shoot_once(&mut job, Err(busy));
        assert!(job.info.active() && !job.saving);
        assert_eq!((job.info.retries, job.info.captured), (1, 0));
        assert_eq!(job.info.status, "pending");
        assert!(
            job.info
                .error
                .as_deref()
                .unwrap()
                .contains("storage manager is busy")
        );
        // The same frame number, one interval later.
        assert_eq!(job.info.next_at_ms, 60_000);
        assert_eq!(job.input(chrono::Local::now()).index, 0);
        let slow_disk = std::io::Error::from(std::io::ErrorKind::TimedOut);
        shoot_once(
            &mut job,
            Err(anyhow::Error::from(slow_disk).context("save shot.raw")),
        );
        assert_eq!((job.info.retries, job.info.next_at_ms), (2, 120_000));
        // A later save clears the error.
        shoot_once(&mut job, Ok("shot.raw".into()));
        assert_eq!((job.info.captured, job.info.error.as_deref()), (1, None));
        assert_eq!(job.info.next_at_ms, 180_000);
        // A path that mentions timeouts does not make a refusal transient.
        let refused = anyhow::anyhow!("Permission denied").context("save /data/timeouts/a.raw");
        shoot_once(&mut job, Err(refused));
        assert_eq!(job.info.status, "failed");
        let mut full = armed(2, &dir);
        shoot_once(
            &mut full,
            Err(anyhow::anyhow!("Storage full: 10 GiB budget reached")),
        );
        assert_eq!(full.info.status, "failed");
        assert!(
            full.info
                .error
                .as_deref()
                .unwrap()
                .starts_with("Storage full")
        );
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn an_idle_worker_waits_for_its_next_job_or_a_second() {
        let writer = crate::shared_memory::SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let h = WorkerHandle::with_ring(Arc::new(Mutex::new(writer)));
        assert_eq!(idle_wait(&h, &[]), IDLE);
        let dir = temp_dir("idle");
        let mut job = armed(1, &dir);
        job.deadline = None;
        job.info.next_at_ms = crate::scheduling::now_ms() + 300;
        let wait = idle_wait(&h, std::slice::from_ref(&job));
        assert!(wait > Duration::from_millis(200) && wait <= Duration::from_millis(300));
        // A requested frame times out sooner.
        job.deadline = Some(Instant::now() + Duration::from_millis(50));
        assert!(idle_wait(&h, std::slice::from_ref(&job)) <= Duration::from_millis(50));
        // A job waiting for the saver is woken by it.
        job.deadline = None;
        job.saving = true;
        job.info.next_at_ms = 0;
        assert_eq!(idle_wait(&h, std::slice::from_ref(&job)), IDLE);
        // A frame the ring holds back keeps the short tick, so it is
        // published once a reader turns up.
        h.record(small_frame(1), false);
        h.record(small_frame(2), false);
        assert_eq!(idle_wait(&h, &[]), TICK);
        // A timed forward must still finish on time after its source ends.
        let until = crate::scheduling::now_ms() + 50;
        let identity = "11111111111111111111111111111111";
        {
            let mut state = h.state.write().unwrap();
            state.forwarding_id = Some(identity.into());
            state.forwarding_until_ms = Some(until);
        }
        assert!(idle_wait(&h, &[]) <= Duration::from_millis(50));
        // Time already credited by a wall-clock jump cannot be slept again.
        *h.forward_clock.lock().unwrap() = Some(ForwardClock {
            forwarding_id: identity.into(),
            anchor_ms: until,
            anchor: Instant::now(),
        });
        assert_eq!(idle_wait(&h, &[]), Duration::ZERO);
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn shutdown_wakes_an_idle_worker_at_once() {
        let fastest = (0..3)
            .map(|_| {
                let h = WorkerHandle::new();
                h.request(SessionCommand::connect("sim:0".into(), 100))
                    .unwrap();
                let started = Instant::now();
                h.shutdown();
                started.elapsed()
            })
            .min()
            .unwrap();
        assert!(fastest < Duration::from_millis(30), "{fastest:?}");
    }
    #[test]
    fn numbered_scanner_capture_uses_one_page() {
        let h = WorkerHandle::new();
        let (mut camera, requests, _) = scanner(Duration::ZERO);
        let dir = temp_dir("scanner-number");
        let path = dir.join("document.raw");
        std::fs::write(&path, b"existing document").unwrap();
        let v = scanner_command(
            &h,
            &mut camera,
            capture_to(&path, 1, crate::naming::OnExists::Number),
        )
        .unwrap();
        assert_eq!(
            v["files"][0].as_str().unwrap(),
            dir.join("document-0001.raw").to_str().unwrap()
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing document");
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn capture_paths() {
        assert_eq!(
            output_path("frames", 2, 0, "png"),
            PathBuf::from("frames/frame-000001.png")
        );
        assert_eq!(
            output_path("shot-{frame}.png", 5, 3, "png"),
            PathBuf::from("shot-000004.png")
        );
    }

    fn connect(h: &WorkerHandle, exposure: &str) {
        h.request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        for (feature, value) in [
            ("Width", "128"),
            ("Height", "128"),
            ("ExposureTime", exposure),
        ] {
            h.request(SessionCommand::Set {
                feature: feature.into(),
                value: value.into(),
            })
            .unwrap();
        }
    }
    fn auto(h: &WorkerHandle, balance: Option<f64>) -> Result<Value> {
        h.request(SessionCommand::Auto { balance })
    }
    fn set(h: &WorkerHandle, feature: &str, value: &str) {
        h.request(SessionCommand::Set {
            feature: feature.into(),
            value: value.into(),
        })
        .unwrap();
    }
    fn grab(h: &WorkerHandle) -> Value {
        h.request(SessionCommand::Grab { timeout_ms: 2000 })
            .unwrap()
    }
    /// What an inspection of the camera's newest frame would measure.
    fn measured(h: &WorkerHandle) -> crate::inspect::Measurements {
        let frame = h.latest_frame().unwrap();
        let kind = h.snapshot().connected.unwrap().profile.kind;
        crate::inspect::measure(&frame, kind, [0, 0, frame.width, frame.height])
    }
    #[test]
    fn grab_publishes_one_frame_and_leaves_a_stopped_camera_stopped() {
        let dir = temp_dir("grab");
        let ring = crate::shared_memory::SharedRing::create(&dir, 1024 * 1024).unwrap();
        let mut reader = ring.reader().unwrap();
        let backing_files = std::fs::read_dir(&dir).unwrap().count();
        let h = WorkerHandle::with_ring(Arc::new(Mutex::new(ring)));
        connect(&h, "1000");
        let history = h.snapshot().history.entries.len();
        assert!(!h.snapshot().streaming);
        let grabbed = grab(&h);
        assert_eq!(grabbed["frame"]["width"], 128);
        assert!(grabbed.get("auto_settled").is_none(), "{grabbed}");
        assert!(grabbed.get("files").is_none() && grabbed.get("camera_change").is_none());
        let snapshot = h.snapshot();
        assert!(!snapshot.streaming);
        assert_eq!(snapshot.frames, 1);
        assert_eq!(snapshot.history.entries.len(), history);
        assert!(
            !snapshot
                .logs
                .iter()
                .any(|entry| entry.message.starts_with("Captured"))
        );
        // Exactly one frame reached the ring, and nothing else was written.
        assert_eq!(reader.sequence(), 1);
        let frame = reader.take_latest().unwrap().unwrap();
        assert_eq!(grabbed["frame"]["id"], frame.id);
        assert_eq!(
            (frame.width, frame.height, frame.data.len()),
            (128, 128, 128 * 128)
        );
        assert!(h.take_saved(10).is_empty());
        // An inspection creates no files beyond any ring backing file.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), backing_files);
        // A streaming camera keeps streaming.
        h.request(SessionCommand::Start).unwrap();
        grab(&h);
        assert!(h.snapshot().streaming);
        h.shutdown();
        drop(reader);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn grab_settles_auto_exposure_on_a_stopped_camera_first() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        auto(&h, None).unwrap();
        let grabbed = grab(&h);
        // Frames metered while settling are published too.
        assert!(h.snapshot().frames >= 1);
        assert!(!h.snapshot().streaming);
        if grabbed.get("auto_settled").is_some() {
            assert_eq!(grabbed["auto_settled"], false);
        } else {
            assert!(h.snapshot().auto.unwrap().state != "converging");
        }
        h.shutdown();
    }
    #[test]
    fn failed_grabs_leave_the_log_and_last_error_alone() {
        let h = WorkerHandle::new();
        let logs = h.snapshot().logs.len();
        // Without a camera, a grab fails like a missed frame would.
        assert!(h.request(SessionCommand::Grab { timeout_ms: 10 }).is_err());
        let snapshot = h.snapshot();
        assert_eq!(snapshot.logs.len(), logs);
        assert!(snapshot.last_error.is_none());
        // Other failures are still the worker's to log.
        assert!(
            h.request(SessionCommand::Get {
                feature: "Width".into()
            })
            .is_err()
        );
        let snapshot = h.snapshot();
        assert_eq!(snapshot.logs.len(), logs + 1);
        assert!(snapshot.last_error.unwrap().contains("no camera connected"));
        h.shutdown();
    }
    #[test]
    fn exposure_measurements_follow_exposure_time_on_the_simulator() {
        let h = WorkerHandle::new();
        connect(&h, "2500");
        let mut means = Vec::new();
        for exposure in ["2500", "10000", "40000"] {
            set(&h, "ExposureTime", exposure);
            grab(&h);
            means.push(measured(&h).exposure.unwrap());
        }
        assert!(means[0].mean < means[1].mean && means[1].mean < means[2].mean);
        assert!((0.3..0.6).contains(&means[1].mean), "{:?}", means[1]);
        assert!(means[2].highlights > 0.3, "{:?}", means[2]);
        assert!(means[0].mean < 0.25, "{:?}", means[0]);
        h.shutdown();
    }
    #[test]
    fn microscope_focus_scores_peak_at_the_specimen() {
        let h = WorkerHandle::new();
        h.request(SessionCommand::connect("sim:microscope".into(), 100))
            .unwrap();
        set(&h, "Width", "256");
        set(&h, "Height", "192");
        let tenengrad = |position: &str| {
            set(&h, "FocusPosition", position);
            grab(&h);
            measured(&h).focus.unwrap().tenengrad
        };
        let sharp = tenengrad("50");
        let (near, far) = (tenengrad("20"), tenengrad("80"));
        assert!(sharp > near && sharp > far, "{sharp} {near} {far}");
        h.shutdown();
    }

    #[test]
    fn software_auto_exposure_converges_inside_the_balance_caps() {
        for (balance, exposure, fps) in [(1.0, "2500", 120.0f64), (0.0, "40000", 30.0)] {
            let h = WorkerHandle::new();
            connect(&h, exposure);
            h.request(SessionCommand::Start).unwrap();
            assert_eq!(
                auto(&h, Some(balance)).unwrap()["auto"]["strategy"],
                "software"
            );
            let deadline = Instant::now() + Duration::from_secs(20);
            let status = loop {
                let status = h.snapshot().auto.unwrap();
                if status.state == "stable" || Instant::now() > deadline {
                    break status;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(status.state, "stable");
            let cap = 1e6 / fps - (0.02e6 / fps).max(100.0);
            assert!((status.exposure_limits_us.unwrap()[1] - cap).abs() < 1e-6);
            assert!(status.exposure_us.unwrap() <= cap + 1e-6);
            assert!((status.brightness.unwrap() / 0.45).log2().abs() < 0.2);
            assert_eq!(status.gain_db.unwrap() > 0.5, balance == 1.0);
            h.shutdown();
        }
    }

    #[test]
    fn auto_mode_commands_follow_the_contract() {
        let h = WorkerHandle::new();
        assert!(auto(&h, None).is_err());
        connect(&h, "10000");
        assert!(auto(&h, Some(2.0)).is_err());
        assert!(h.snapshot().auto.is_none());
        let entered = auto(&h, Some(0.25)).unwrap();
        assert_eq!(entered["auto"]["balance"], 0.25);
        assert!(
            entered["auto"]["managed"]
                .as_array()
                .unwrap()
                .contains(&json!("ExposureTime"))
        );
        assert_eq!(auto(&h, None).unwrap()["auto"]["balance"], 0.25);
        assert!(auto(&h, Some(f64::NAN)).is_err());
        assert_eq!(h.snapshot().auto.unwrap().balance, 0.25);
        let set = |feature: &str, value: &str| {
            h.request(SessionCommand::Set {
                feature: feature.into(),
                value: value.into(),
            })
            .unwrap()
        };
        assert!(set("Width", "128").get("auto").is_none());
        assert!(h.snapshot().auto.is_some());
        let manual = set("ExposureTime", "5000");
        assert!(manual.get("auto").is_some_and(Value::is_null));
        assert_eq!(manual["value"], 5000.0);
        assert!(h.snapshot().auto.is_none());
        assert_eq!(
            h.request(SessionCommand::Manual { revert: false }).unwrap(),
            json!({"auto": null})
        );
        h.shutdown();
    }

    #[test]
    fn capture_warms_up_auto_exposure_and_reports_auto_status() {
        let h = WorkerHandle::new();
        connect(&h, "2500");
        auto(&h, Some(0.5)).unwrap();
        let v = h
            .request(SessionCommand::Capture {
                output: "-".into(),
                count: 1,
                timeout_ms: 6000,
                format: "raw".into(),
                storage: Default::default(),
                destination: None,
                on_exists: Default::default(),
                context: Default::default(),
            })
            .unwrap();
        assert_eq!(v["frames"][0]["width"], 128);
        assert_eq!(v["auto"]["strategy"], "software");
        assert!(v["auto"]["brightness"].as_f64().unwrap() > 0.2);
        assert!(!h.snapshot().streaming);
        h.shutdown();
    }

    struct Shared(Arc<Mutex<crate::transport::simulator::Simulator>>);
    impl crate::types::RegisterIo for Shared {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.0.lock().unwrap().read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.0.lock().unwrap().write_memory(address, data)
        }
    }
    impl crate::types::Backend for Shared {
        fn xml(&mut self) -> Result<String> {
            self.0.lock().unwrap().xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.0.lock().unwrap().start(payload)
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            self.0.lock().unwrap().next_frame(timeout)
        }
        fn stop(&mut self) -> Result<()> {
            self.0.lock().unwrap().stop()
        }
    }

    struct DemandScanner {
        simulator: crate::transport::simulator::Simulator,
        requests: Arc<AtomicU64>,
        stops: Arc<AtomicU64>,
        ready: Option<Instant>,
        latency: Duration,
    }
    impl crate::types::RegisterIo for DemandScanner {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.simulator.read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.simulator.write_memory(address, data)
        }
    }
    impl crate::types::Backend for DemandScanner {
        fn xml(&mut self) -> Result<String> {
            self.simulator.xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.simulator.start(payload)
        }
        fn request_frame(&mut self) -> Result<()> {
            if self.ready.is_none() {
                self.requests.fetch_add(1, Ordering::Relaxed);
                self.ready = Some(Instant::now() + self.latency);
            }
            Ok(())
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            let Some(ready) = self.ready else {
                anyhow::bail!("frame timeout; no scan requested");
            };
            let wait = ready.saturating_duration_since(Instant::now());
            if wait > timeout {
                std::thread::sleep(timeout);
                anyhow::bail!("frame timeout");
            }
            std::thread::sleep(wait);
            let frame = self.simulator.next_frame(timeout)?;
            self.ready = None;
            Ok(frame)
        }
        fn stop(&mut self) -> Result<()> {
            self.stops.fetch_add(1, Ordering::Relaxed);
            self.ready = None;
            self.simulator.stop()
        }
    }
    fn scanner(latency: Duration) -> (Option<Camera>, Arc<AtomicU64>, Arc<AtomicU64>) {
        let requests = Arc::new(AtomicU64::new(0));
        let stops = Arc::new(AtomicU64::new(0));
        let mut info = crate::transport::simulator::info();
        info.transport = crate::types::Transport::Scanner;
        (
            Some(
                Camera::from_backend(
                    info,
                    Box::new(DemandScanner {
                        simulator: Default::default(),
                        requests: requests.clone(),
                        stops: stops.clone(),
                        ready: None,
                        latency,
                    }),
                )
                .unwrap(),
            ),
            requests,
            stops,
        )
    }
    fn scanner_command(
        h: &WorkerHandle,
        camera: &mut Option<Camera>,
        command: SessionCommand,
    ) -> Result<Value> {
        process(
            h,
            camera,
            &mut None,
            &mut None,
            &mut Vec::new(),
            &mut 1,
            &mut false,
            command,
        )
    }

    struct SlowInspection {
        simulator: crate::transport::simulator::Simulator,
        start_delay: Duration,
        frame_delay: Duration,
        reads: Arc<AtomicU64>,
    }
    impl crate::types::RegisterIo for SlowInspection {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.simulator.read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.simulator.write_memory(address, data)
        }
    }
    impl crate::types::Backend for SlowInspection {
        fn xml(&mut self) -> Result<String> {
            self.simulator.xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.simulator.start(payload)?;
            std::thread::sleep(self.start_delay);
            Ok(())
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(self.frame_delay);
            // Deliberately violate the backend deadline: the caller must
            // reject a late successful frame too, not only timeout errors.
            self.simulator.next_frame(Duration::from_secs(1))
        }
        fn stop(&mut self) -> Result<()> {
            self.simulator.stop()
        }
    }

    #[test]
    fn inspection_uses_one_deadline_including_startup_and_rejects_late_frames() {
        for (start_delay, frame_delay, expected_reads) in [(40, 0, 0), (0, 40, 1)] {
            let reads = Arc::new(AtomicU64::new(0));
            let mut camera = Some(
                Camera::from_backend(
                    crate::transport::simulator::info(),
                    Box::new(SlowInspection {
                        simulator: Default::default(),
                        start_delay: Duration::from_millis(start_delay),
                        frame_delay: Duration::from_millis(frame_delay),
                        reads: reads.clone(),
                    }),
                )
                .unwrap(),
            );
            let h = WorkerHandle::new();
            let error = scanner_command(&h, &mut camera, SessionCommand::Grab { timeout_ms: 10 })
                .unwrap_err();
            assert_eq!(error.to_string(), GRAB_TIMEOUT);
            assert_eq!(reads.load(Ordering::Relaxed), expected_reads);
            assert!(
                h.frame.read().unwrap().is_none(),
                "A late frame must never enter the preview feed."
            );
            assert!(!camera.as_ref().unwrap().is_streaming());
            h.shutdown();
        }
    }
    fn scanner_capture(timeout_ms: u64) -> SessionCommand {
        SessionCommand::Capture {
            output: "-".into(),
            count: 1,
            timeout_ms,
            format: "raw".into(),
            storage: Default::default(),
            destination: None,
            on_exists: Default::default(),
            context: Default::default(),
        }
    }

    #[test]
    fn grab_refuses_scanners_and_frames_belong_to_the_coordinator() {
        let h = WorkerHandle::new();
        let (mut camera, requests, _) = scanner(Duration::ZERO);
        let error = scanner_command(&h, &mut camera, SessionCommand::Grab { timeout_ms: 1000 })
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "a document scanner cannot be inspected without scanning a page"
        );
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert!(!camera.as_ref().unwrap().is_streaming());
        let frame = SessionCommand::Frame {
            fresh: 1,
            timeout_ms: 1000,
            max_dimension: 0,
            format: Default::default(),
            region: None,
        };
        let error = scanner_command(&h, &mut camera, frame).unwrap_err();
        assert_eq!(error.to_string(), "frames belong to the coordinator");
        h.shutdown();
    }

    #[test]
    fn scanner_capture_requests_one_page_across_timeout_polls() {
        let h = WorkerHandle::new();
        let (mut camera, requests, stops) = scanner(Duration::from_millis(220));
        camera.as_mut().unwrap().start().unwrap();
        assert!(
            camera
                .as_mut()
                .unwrap()
                .next_frame(Duration::from_millis(1))
                .is_err()
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            0,
            "start only arms the scanner"
        );
        let result = scanner_command(&h, &mut camera, scanner_capture(1000)).unwrap();
        assert_eq!(result["count"], 1);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(stops.load(Ordering::Relaxed), 1);
        assert!(!camera.as_ref().unwrap().is_streaming());
        assert_eq!(h.snapshot().frames, 1);
        h.shutdown();
    }

    #[test]
    fn scanner_capture_timeout_cancels_an_already_armed_scan() {
        let h = WorkerHandle::new();
        let (mut camera, requests, stops) = scanner(Duration::from_secs(1));
        camera.as_mut().unwrap().start().unwrap();
        let error = scanner_command(&h, &mut camera, scanner_capture(1)).unwrap_err();
        assert!(error.to_string().contains("timeout"));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(stops.load(Ordering::Relaxed), 1);
        assert!(!camera.as_ref().unwrap().is_streaming());
        assert_eq!(h.snapshot().frames, 0);
        h.shutdown();
    }

    #[test]
    fn scanner_acquisition_has_no_replayable_checkpoint_or_redo_page() {
        let h = WorkerHandle::new();
        let (mut camera, requests, stops) = scanner(Duration::ZERO);
        let started = scanner_command(&h, &mut camera, SessionCommand::Start).unwrap();
        assert!(started.get("camera_change").is_none());
        assert!(
            started["history_unavailable"]
                .as_str()
                .unwrap()
                .contains("document")
        );
        let stopped = scanner_command(&h, &mut camera, SessionCommand::Stop).unwrap();
        assert!(stopped.get("camera_change").is_none());
        assert!(scanner_command(&h, &mut camera, SessionCommand::Undo).is_err());
        assert!(scanner_command(&h, &mut camera, SessionCommand::Redo).is_err());
        // Scanner settings remain ordinary reversible configuration edits.
        scanner_command(
            &h,
            &mut camera,
            SessionCommand::Set {
                feature: "Width".into(),
                value: "128".into(),
            },
        )
        .unwrap();
        scanner_command(&h, &mut camera, SessionCommand::Undo).unwrap();
        assert_eq!(camera.as_mut().unwrap().get("Width").unwrap(), 640);
        scanner_command(&h, &mut camera, SessionCommand::Redo).unwrap();
        assert_eq!(camera.as_mut().unwrap().get("Width").unwrap(), 128);
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(stops.load(Ordering::Relaxed), 1);
        h.shutdown();
    }

    #[test]
    fn existing_scanner_output_fails_without_consuming_a_page() {
        let h = WorkerHandle::new();
        let (mut camera, requests, _) = scanner(Duration::ZERO);
        let dir = std::env::temp_dir().join(format!(
            "capturefab-scanner-output-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("document.png");
        std::fs::write(&path, b"existing document").unwrap();
        let mut command = scanner_capture(1000);
        if let SessionCommand::Capture { output, .. } = &mut command {
            *output = path.to_string_lossy().into_owned();
        }
        let error = scanner_command(&h, &mut camera, command.clone()).unwrap_err();
        assert!(error.to_string().contains("output already exists"));
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(h.snapshot().frames, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing document");
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(dir.join("missing-document.png"), &path).unwrap();
            let error = scanner_command(&h, &mut camera, command).unwrap_err();
            assert!(error.to_string().contains("output already exists"));
            assert_eq!(requests.load(Ordering::Relaxed), 0);
            assert!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
        h.shutdown();
    }

    #[test]
    fn scheduled_scanner_requests_once_and_cancels_on_deadline() {
        let h = WorkerHandle::new();
        let (mut camera, requests, stops) = scanner(Duration::from_secs(1));
        let now = crate::scheduling::now_ms();
        let mut jobs = vec![Scheduled {
            plan: crate::naming::Plan::File("scan.png".into()),
            camera: "camera".into(),
            on_exists: Default::default(),
            next: None,
            saving: false,
            info: crate::scheduling::CaptureJob {
                id: 1,
                status: "pending".into(),
                output: "scan.png".into(),
                destination: None,
                count: 1,
                captured: 0,
                skipped: 0,
                retries: 0,
                first_at_ms: now,
                interval_ms: 0,
                next_at_ms: now,
                last_file: None,
                error: None,
                context: Default::default(),
            },
            resolved: crate::destination::resolve(None, "scan.png").unwrap(),
            timeout_ms: 1000,
            format: "png".into(),
            storage: Default::default(),
            deadline: None,
            warm: None,
        }];
        let mut started = false;
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, false);
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, false);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(jobs[0].info.status, "running");
        jobs[0].deadline = Some(Instant::now() - Duration::from_millis(1));
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, false);
        assert_eq!(jobs[0].info.status, "failed");
        assert_eq!(stops.load(Ordering::Relaxed), 1);
        assert!(!camera.as_ref().unwrap().is_streaming());
        h.shutdown();
    }

    #[test]
    fn explicit_capture_does_not_consume_a_scheduled_scan() {
        let h = WorkerHandle::new();
        let (mut camera, requests, stops) = scanner(Duration::from_millis(220));
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next_job, mut started) = (1, false);
        process(
            &h,
            &mut camera,
            &mut forwarder,
            &mut auto,
            &mut jobs,
            &mut next_job,
            &mut started,
            SessionCommand::Schedule {
                output: "scan.png".into(),
                count: 1,
                timeout_ms: 1000,
                format: "png".into(),
                first_at_ms: crate::scheduling::now_ms(),
                interval_ms: 0,
                storage: Default::default(),
                destination: None,
                on_exists: Default::default(),
                context: Default::default(),
            },
        )
        .unwrap();
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, false);
        let deadline = jobs[0].deadline;
        let error = process(
            &h,
            &mut camera,
            &mut forwarder,
            &mut auto,
            &mut jobs,
            &mut next_job,
            &mut started,
            scanner_capture(1000),
        )
        .unwrap_err();
        assert!(error.to_string().contains("busy with a scheduled scan"));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(stops.load(Ordering::Relaxed), 0);
        assert!(camera.as_ref().unwrap().is_streaming());
        assert_eq!(jobs[0].info.status, "running");
        assert_eq!(jobs[0].deadline, deadline);
        assert_eq!(jobs[0].info.captured, 0);
        assert_eq!(h.snapshot().frames, 0);
        // The scheduled page is still pending; rejecting the direct capture
        // must neither consume it nor ask the scanner for a replacement page.
        let page = camera
            .as_mut()
            .unwrap()
            .next_frame(Duration::from_secs(1))
            .unwrap();
        assert_eq!(page.id, 1);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        h.shutdown();
    }

    #[test]
    fn scheduled_scanner_failure_preserves_diagnostics_and_future_jobs() {
        let h = WorkerHandle::new();
        let (mut camera, requests, _) = scanner(Duration::from_secs(1));
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next_job, mut started) = (1, false);
        let now = crate::scheduling::now_ms();
        for (index, first_at_ms) in [now, now, now + 60_000].into_iter().enumerate() {
            process(
                &h,
                &mut camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next_job,
                &mut started,
                SessionCommand::Schedule {
                    output: format!("scan-{index}.png"),
                    count: 2,
                    timeout_ms: 1000,
                    format: "png".into(),
                    first_at_ms,
                    interval_ms: 60_000,
                    storage: Default::default(),
                    destination: None,
                    on_exists: Default::default(),
                    context: Default::default(),
                },
            )
            .unwrap();
        }
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, false);
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "due jobs share one scan"
        );
        assert_eq!(jobs[0].info.status, "running");
        assert_eq!(jobs[1].info.status, "running");
        assert_eq!(jobs[2].info.status, "pending");
        // A driver rejects the requested page before its deadline. Keep the
        // real failure in every waiting job instead of later timing out.
        let error = anyhow::anyhow!("unsupported scanner source: document feeder unavailable")
            .context("scanner acquisition failed");
        fail_scanner_jobs(&h, &mut jobs, &error);
        for job in &jobs[..2] {
            assert_eq!(job.info.status, "failed");
            assert_eq!(
                job.info.error.as_deref(),
                Some(
                    "scanner acquisition failed: unsupported scanner source: document feeder unavailable"
                )
            );
            assert!(job.deadline.is_none());
            assert_eq!(job.info.captured, 0);
        }
        assert_eq!(jobs[2].info.status, "pending");
        assert!(jobs[2].info.error.is_none());
        assert!(jobs[2].deadline.is_none());
        assert_eq!(h.snapshot().jobs[0].error, jobs[0].info.error);
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, false);
        assert!(!camera.as_ref().unwrap().is_streaming());
        h.shutdown();
    }

    #[test]
    fn disconnect_reverts_auto_mode_changes() {
        use crate::types::{Backend, RegisterIo};
        let sim = Arc::new(Mutex::new(crate::transport::simulator::Simulator::default()));
        let read = |address: u64| {
            let bytes = sim.lock().unwrap().read_memory(address, 8).unwrap();
            f64::from_le_bytes(bytes.try_into().unwrap())
        };
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Shared(sim.clone())),
            )
            .unwrap(),
        );
        camera
            .as_mut()
            .unwrap()
            .set("ExposureTime", "20000")
            .unwrap();
        let h = WorkerHandle::new();
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next_job, mut stream) = (1, false);
        let mut run = |camera: &mut Option<Camera>, command| {
            process(
                &h,
                camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next_job,
                &mut stream,
                command,
            )
        };
        run(&mut camera, SessionCommand::Auto { balance: Some(1.0) }).unwrap();
        assert!((read(0x110) - (1e6 / 120.0 - 1e6 / 120.0 * 0.02)).abs() < 1e-6);
        assert!(read(0x118) > 7.0);
        assert_eq!(read(0x120), 120.0);
        run(&mut camera, SessionCommand::Disconnect).unwrap();
        assert!(camera.is_none());
        assert_eq!(
            [read(0x110), read(0x118), read(0x120)],
            [20_000.0, 0.0, 30.0]
        );
        assert!(sim.lock().unwrap().xml().is_ok());
        h.shutdown();
    }

    #[test]
    #[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
    fn a_video_file_plays_in_real_time_and_ends_with_one_notice() {
        let ffmpeg = std::env::var_os("CAPTUREFAB_FFMPEG").unwrap();
        let dir = std::env::temp_dir().join(format!(
            "capturefab-file-end-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mkv");
        let made = std::process::Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .args(["testsrc=size=64x48:rate=10", "-t", "3", "-c:v", "ffv1"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let h = WorkerHandle::new();
        h.request(SessionCommand::connect(clip.to_string_lossy().into(), 3000))
            .unwrap();
        let started = Instant::now();
        h.request(SessionCommand::Start).unwrap();
        while h.snapshot().streaming {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(20));
        }
        // Thirty frames at 10 fps, after FFmpeg's initial half-second burst,
        // not as fast as they decode.
        assert!(started.elapsed() >= Duration::from_secs(2));
        let snapshot = h.snapshot();
        let said = |text: &str| {
            snapshot
                .logs
                .iter()
                .filter(|log| log.message.contains(text))
                .count()
        };
        assert_eq!(said("Video file finished"), 1);
        assert_eq!(said("repeated transport errors"), 0);
        assert_eq!(snapshot.last_error, None);
        assert!(snapshot.frames >= 29, "{}", snapshot.frames);
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A worker whose network stream drops, returns, drops again and is
    /// stopped while reconnecting, with a stand-in FFmpeg in a child process.
    #[test]
    #[cfg(unix)]
    fn a_lost_stream_resumes_and_stopping_ends_its_reconnecting() {
        const TEST: &str = "a_lost_stream_resumes_and_stopping_ends_its_reconnecting";
        if std::env::var_os("CAPTUREFAB_RECONNECT_TEST").is_none() {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!(
                "capturefab-engine-reconnect-{}-{}",
                std::process::id(),
                crate::scheduling::now_ms()
            ));
            let session = dir.join("session");
            crate::ipc::ensure_private_dir(&session).unwrap();
            // Two launches that end after half a second, then a steady one.
            let ffmpeg = dir.join("ffmpeg");
            let script = r#"case "$*" in *rtsp*) ;; *) exit 0 ;; esac
n=$(($(cat "$dir/launches" 2>/dev/null || echo 0) + 1)); echo $n > "$dir/launches"
frame() { printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; }
case $n in
1|2) for i in 1 2 3 4 5 6 7 8 9 10; do frame; sleep 0.05; done ;;
*) while frame; do sleep 0.05; done ;;
esac"#;
            std::fs::write(
                &ffmpeg,
                format!("#!/bin/sh\ndir='{}'\n{script}\n", dir.display()),
            )
            .unwrap();
            std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o700)).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &format!("engine::tests::{TEST}"), "--nocapture"])
                .env("CAPTUREFAB_RECONNECT_TEST", "1")
                .env("CAPTUREFAB_FFMPEG", &ffmpeg)
                .env("CAPTUREFAB_SESSION_DIR", &session)
                .output()
                .unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let h = WorkerHandle::new();
        let said = |text: &str| {
            h.snapshot()
                .logs
                .iter()
                .filter(|log| log.message.contains(text))
                .count()
        };
        let wait = |what: &str, done: &dyn Fn() -> bool| {
            let started = Instant::now();
            while !done() {
                assert!(started.elapsed() < Duration::from_secs(10), "{what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        h.request(SessionCommand::connect(
            "rtsp://user:secret@127.0.0.1:9/live".into(),
            2000,
        ))
        .unwrap();
        h.request(SessionCommand::Start).unwrap();
        wait("first loss", &|| said("Stream lost") == 1);
        let error = h.snapshot().last_error.unwrap();
        assert!(
            error.starts_with(
                "Stream lost; reconnecting to rtsp://<redacted>@127.0.0.1:9/live (attempt 1): timed out"
            ),
            "{error}"
        );
        assert!(h.snapshot().streaming);
        wait("resume", &|| said("Stream resumed") == 1);
        assert_eq!(h.snapshot().last_error, None);
        // It drops again before settling: reported again, backing off longer.
        wait("second loss", &|| said("Stream lost") == 2);
        let error = h.snapshot().last_error.unwrap();
        assert!(error.contains("(attempt 2): timed out"), "{error}");
        // Stopping meanwhile ends the outage without a resume.
        h.request(SessionCommand::Stop).unwrap();
        wait("outage ended", &|| h.snapshot().last_error.is_none());
        h.request(SessionCommand::Start).unwrap();
        let frames = h.snapshot().frames;
        wait("frames", &|| h.snapshot().frames > frames + 5);
        assert_eq!(said("Stream resumed"), 1);
        assert_eq!(said("Stream lost"), 2);
        assert_eq!(said("repeated transport errors"), 0);
        assert!(
            h.snapshot()
                .logs
                .iter()
                .all(|log| !log.message.contains("secret"))
        );
        h.shutdown();
    }

    #[cfg(feature = "keychain")]
    #[test]
    fn saved_sign_ins_are_read_only_when_allowed_and_never_fall_back() {
        use crate::logins::{CameraLogin, Logins, MemoryStore};
        use std::sync::atomic::Ordering;
        let dir = std::env::temp_dir().join(format!(
            "capturefab-engine-logins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Arc::new(MemoryStore::default());
        let logins = Logins::with(dir, store.clone());
        let index = logins.index_path();
        let address = "rtsp://127.0.0.1:9/live";
        logins
            .save(
                address,
                &CameraLogin::new("admin", "engine-test-pw").unwrap(),
            )
            .unwrap();
        store.reads.store(0, Ordering::SeqCst);
        let h = WorkerHandle::with_logins(logins);
        let connect = |saved_login| SessionCommand::Connect {
            camera: "rtsp://127.0.0.1:9/live".into(),
            timeout_ms: 1000,
            login: None,
            saved_login,
        };
        // An assistant connect never reads the store.
        assert!(h.request(connect(false)).is_err());
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
        // A saved sign-in is looked up when none was sent.
        assert!(h.request(connect(true)).is_err());
        assert_eq!(store.reads.load(Ordering::SeqCst), 1);
        // A locked store fails the connect rather than trying anonymously.
        store.fail.store(true, Ordering::SeqCst);
        let error = h.request(connect(true)).unwrap_err();
        assert!(
            format!("{error:#}").contains("unlock it and try again"),
            "{error:#}"
        );
        assert!(!format!("{error:#}").contains("engine-test-pw"));
        // A corrupt sign-in list is skipped with a warning, not an error
        // that breaks every camera.
        store.fail.store(false, Ordering::SeqCst);
        std::fs::write(index, b"not json").unwrap();
        let reads = store.reads.load(Ordering::SeqCst);
        let error = h.request(connect(true)).unwrap_err();
        assert!(!format!("{error:#}").contains("sign-in list"), "{error:#}");
        assert_eq!(store.reads.load(Ordering::SeqCst), reads);
        assert!(
            h.snapshot()
                .logs
                .iter()
                .any(|log| log.message.contains("camera-logins.json")),
            "a warning names the file"
        );
        h.shutdown();
    }
}
