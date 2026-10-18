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

use crate::session::{LogEntry, SessionCommand, SessionSnapshot};
mod history;
mod network_defaults;
#[cfg(test)]
mod network_tests;
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
#[derive(Clone)]
pub struct WorkerHandle {
    sender: SyncSender<Envelope>,
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
    capture_session: Arc<Mutex<Option<crate::autocapture::CaptureSession>>>,
    /// Files saved since the coordinator last collected them.
    saved: Arc<Mutex<crate::dataset::SavedLog>>,
    /// The local recording in progress, reported when it stops.
    recording: Arc<Mutex<Option<crate::scheduling::RecordingMeta>>>,
    /// Output numbers handed out, so concurrent captures never share a name.
    names: Arc<Mutex<crate::naming::Namer>>,
}
impl Default for WorkerHandle {
    fn default() -> Self {
        Self::new()
    }
}
impl WorkerHandle {
    pub fn new() -> Self {
        Self::create(None)
    }
    pub fn with_ring(ring: Arc<Mutex<crate::shared_memory::SharedRing>>) -> Self {
        Self::create(Some(ring))
    }
    fn create(ring: Option<Arc<Mutex<crate::shared_memory::SharedRing>>>) -> Self {
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
            capture_session: Arc::default(),
            saved: Arc::default(),
            recording: Arc::default(),
            names: Arc::default(),
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
            .try_send(Envelope { command, reply })
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
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
    fn cancelled(&self) -> bool {
        self.quitting.load(Ordering::Relaxed)
            || self
                .ring
                .as_ref()
                .is_some_and(|r| r.lock().unwrap_or_else(|e| e.into_inner()).cancelled())
    }
    fn log(&self, level: &str, message: String) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        if level == "error" {
            s.last_error = Some(message.clone());
        }
        s.logs.push(LogEntry::now_utc(level, message));
        if s.logs.len() > 200 {
            s.logs.remove(0);
        }
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
        self.observe_capture(&frame);
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
    fn publish_capture(&self, session: &crate::autocapture::CaptureSession) -> Result<()> {
        let status = session.status()?;
        self.state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .capture_session = Some(status);
        Ok(())
    }
    fn teach_capture(&self, frame: &Frame) -> Result<Option<u64>> {
        let mut guard = self
            .capture_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(session) = guard.as_mut() else {
            return Ok(None);
        };
        let id = session.manual(frame)?;
        self.publish_capture(session)?;
        Ok(Some(id))
    }
    fn observe_capture(&self, frame: &Frame) {
        self.drive_capture(|session| session.observe(frame));
    }
    /// Finish background embeddings while no frames arrive (slow, triggered
    /// or stopped cameras).
    fn poll_capture(&self) {
        self.drive_capture(crate::autocapture::CaptureSession::poll);
    }
    fn drive_capture(
        &self,
        step: impl FnOnce(&mut crate::autocapture::CaptureSession) -> Result<Option<u64>>,
    ) {
        let mut guard = self
            .capture_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(session) = guard.as_mut() else {
            return;
        };
        match step(session) {
            Ok(Some(_)) => {
                if let Err(error) = self.publish_capture(session) {
                    self.log("error", format!("Auto capture status: {error:#}"));
                }
            }
            Ok(None) => {}
            Err(error) => {
                // Preserve the database and all labels; acquisition continues.
                if let Ok(status) = session.status() {
                    let _ = session.configure(false, status.sensitivity, status.cooldown_ms);
                }
                let _ = self.publish_capture(session);
                self.log("error", format!("Auto capture paused: {error:#}"));
            }
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
fn timed_out(e: &anyhow::Error) -> bool {
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

/// Stop forwarding; the stopped forwarder's final counters, if one ran.
fn stop_forward(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
) -> Result<Option<crate::media::ForwardStats>> {
    {
        let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
        s.forwarding = None;
        s.forwarding_since_ms = None;
    }
    if let Some(a) = auto.as_mut() {
        a.set_forward_fps(None)?;
    }
    let meta = h.recording.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some((mut f, _)) = forwarder.take() else {
        return Ok(None);
    };
    let stopped = f.stop();
    let stats = f.stats();
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
            object_key: file.object_key,
            remote_url: file.remote_url,
            upload_id: file.upload_id,
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
/// Queue a saved image for upload and report it to the coordinator's
/// dataset. Returns what was queued, for the capture reply.
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
) -> Option<crate::destination::Queued> {
    let queued = resolved.finished(path).unwrap_or_else(|e| {
        h.log(
            "error",
            format!("{} saved but not queued for upload: {e:#}", path.display()),
        );
        None
    });
    h.push_saved(crate::dataset::SavedFile {
        seq: 0,
        wall_ms,
        path: absolute_path(path),
        bytes: stored.bytes,
        sha256: Some(stored.sha256),
        destination: resolved.destination.clone(),
        object_key: queued.as_ref().map(|q| q.key.clone()),
        remote_url: queued.as_ref().map(|q| q.url.clone()),
        upload_id: queued.as_ref().map(|q| q.id.clone()),
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
    queued
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
        SessionCommand::CaptureSessionOpen {
            path,
            create,
            encoder,
        } => {
            let c = camera_mut(camera)?;
            ensure!(
                !c.is_scanner(),
                "auto capture needs a streaming camera, not a document scanner"
            );
            let mut guard = h.capture_session.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                guard.is_none(),
                "close the current capture session before opening another"
            );
            let session = crate::autocapture::CaptureSession::open(
                std::path::Path::new(&path),
                &c.info.id,
                create,
                encoder,
            )?;
            if session.status()?.enabled {
                c.start()?;
            }
            h.publish_capture(&session)?;
            *guard = Some(session);
            refresh(h, camera, false);
            Ok(json!({"capture_session":h.snapshot().capture_session}))
        }
        SessionCommand::CaptureSessionClose => {
            let mut guard = h.capture_session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(session) = guard.as_ref() {
                session.checkpoint()?;
            }
            guard.take();
            h.state
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .capture_session = None;
            Ok(json!({"capture_session":null}))
        }
        SessionCommand::AutoCapture {
            enabled,
            sensitivity,
            cooldown_ms,
        } => {
            let c = camera_mut(camera)?;
            let mut guard = h.capture_session.lock().unwrap_or_else(|e| e.into_inner());
            let session = guard
                .as_mut()
                .context("create or resume a capture session first")?;
            // Start before arming; failed acquisition must not silently arm a session.
            if enabled {
                c.start()?;
            }
            session.configure(enabled, sensitivity, cooldown_ms)?;
            h.publish_capture(session)?;
            refresh(h, camera, false);
            Ok(json!({"capture_session":h.snapshot().capture_session}))
        }
        SessionCommand::CaptureExample { timeout_ms } => {
            ensure!(
                h.capture_session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some(),
                "create or resume a capture session first"
            );
            let c = camera_mut(camera)?;
            let was_streaming = c.is_streaming();
            c.start()?;
            let result = (|| -> Result<Value> {
                c.request_frame()?;
                let frame = next_frame(h, c, auto, Instant::now() + timeout(timeout_ms)?)?
                    .context("frame timeout")?;
                let id = h.teach_capture(&frame)?.context("capture session closed")?;
                h.record_capture(frame);
                Ok(json!({"id":id,"capture_session":h.snapshot().capture_session}))
            })();
            let stopped = if !was_streaming { c.stop() } else { Ok(()) };
            refresh(h, camera, false);
            let value = result?;
            stopped?;
            Ok(value)
        }
        SessionCommand::CaptureReview { id } => {
            use base64::Engine;
            let guard = h.capture_session.lock().unwrap_or_else(|e| e.into_inner());
            let session = guard
                .as_ref()
                .context("create or resume a capture session first")?;
            let capture = match id {
                Some(id) => session.review_capture(id)?,
                None => session.next_review()?,
            };
            let capture = capture.map(|capture| -> Result<Value> {
                let (width, height, rgba) = crate::frame::preview_rgba(&capture.frame, 480, 360)?;
                Ok(json!({"id":capture.id,"score":capture.score,"reason":capture.reason,
                    "width":width,"height":height,"rgba":base64::engine::general_purpose::STANDARD.encode(rgba)}))
            }).transpose()?;
            Ok(json!({"capture":capture,"capture_session":session.status()?}))
        }
        SessionCommand::ReviewCapture { id, keep } => {
            let mut guard = h.capture_session.lock().unwrap_or_else(|e| e.into_inner());
            let session = guard
                .as_mut()
                .context("create or resume a capture session first")?;
            session.review(id, keep)?;
            h.publish_capture(session)?;
            Ok(json!({"id":id,"keep":keep,"capture_session":h.snapshot().capture_session}))
        }
        SessionCommand::Status => {
            refresh(h, camera, false);
            if let Some(session) = h
                .capture_session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                h.publish_capture(session)?;
            }
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
        | SessionCommand::DatasetExport { .. } => {
            anyhow::bail!("datasets belong to the coordinator")
        }
        SessionCommand::Signals
        | SessionCommand::SignalRead { .. }
        | SessionCommand::SignalBrowse { .. }
        | SessionCommand::SignalStart { .. }
        | SessionCommand::SignalStop { .. } => anyhow::bail!("signals belong to the coordinator"),
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
        } => {
            ensure!(
                !camera_mut(camera)?.is_scanner(),
                "scanner forwarding is unsupported; use capture for document scans"
            );
            ensure!(
                forwarder.is_none(),
                "forwarding is already active; stop-forward first"
            );
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
            let shown = crate::media::redact_url(&output);
            {
                let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
                s.forwarding = Some(shown.clone());
                s.forwarding_since_ms = Some(since_ms);
            }
            let mut value = json!({"forwarding":shown,"since_ms":since_ms});
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
            *camera = Some(Camera::open(&selector, &known, duration)?);
            *h.frame.write().unwrap_or_else(|e| e.into_inner()) = None;
            {
                h.counters.reset();
                h.state
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_error = None;
            }
            refresh(h, camera, true);
            h.log(
                "info",
                format!("Connected to {}", crate::media::redact_url(&selector)),
            );
            Ok(json!({"connected":h.snapshot().connected}))
        }
        SessionCommand::Disconnect => {
            {
                let mut guard = h.capture_session.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(session) = guard.as_ref() {
                    session.checkpoint()?;
                }
                guard.take();
            }
            h.state
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .capture_session = None;
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
                let mut queued = Vec::new();
                if !was_streaming && auto.is_some() {
                    // Refresh the controller's streaming state before testing convergence:
                    // a previously stable acquisition must settle again after a restart.
                    if let Some(a) = auto.as_mut() {
                        a.tick(c, Instant::now());
                        publish_auto(h, a);
                    }
                    let until = Instant::now() + (duration / 2).min(Duration::from_secs(3));
                    while auto.as_ref().is_some_and(|a| !a.converged()) {
                        let Some(frame) = next_frame(h, c, auto, until)? else {
                            h.log(
                                "warn",
                                "Auto exposure had not settled; capturing anyway".into(),
                            );
                            break;
                        };
                        h.record(frame, false);
                    }
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
                        if let Some(q) = report_image(
                            h, &resolved, &path, stored, &frame, &format, wall_ms, None, &context,
                        ) {
                            queued.push(
                                json!({"file":file,"upload_id":q.id,"key":q.key,"url":q.url}),
                            );
                        }
                        metadata["file"] = json!(file);
                        files.push(file);
                    }
                    frames.push(metadata);
                    if let Err(error) = h.teach_capture(&frame) {
                        h.log(
                            "error",
                            format!("Capture saved but session learning failed: {error:#}"),
                        );
                    }
                    h.record_capture(frame);
                }
                let mut value = json!({"files":files,"frames":frames,"count":count});
                if !context.is_empty() {
                    value["context"] = json!(context);
                }
                if let Some(name) = &resolved.destination {
                    value["destination"] = json!(name);
                    if resolved.upload.is_some() {
                        value["queued_uploads"] = json!(queued.len());
                        value["queued"] = json!(queued);
                    }
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
    for job in jobs.iter_mut().filter(|j| j.info.active()) {
        if job.info.next_at_ms > now {
            continue;
        }
        if job.deadline.is_none() {
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
            job.info.status = "failed".into();
            job.info.error = Some("frame timeout".into());
            h.log(
                "error",
                format!("Capture job {}: frame timeout", job.info.id),
            );
        }
    }
    if *started && !forwarding && !jobs.iter().any(|j| j.info.active() && j.deadline.is_some()) {
        if let Some(c) = camera.as_mut() {
            let _ = c.stop();
        }
        *started = false;
    }
    trim_jobs(jobs);
    sync_jobs(h, jobs);
    let scanner = camera.as_ref().is_some_and(Camera::is_scanner);
    refresh(h, camera, scanner);
}
fn capture_jobs(h: &WorkerHandle, frame: &Frame, jobs: &mut [Scheduled], settled: bool) {
    let now = crate::scheduling::now_ms();
    for job in jobs.iter_mut().filter(|j| {
        j.info.active()
            && j.deadline.is_some()
            && j.info.next_at_ms <= now
            && (settled || j.warm.is_none_or(|w| Instant::now() >= w))
    }) {
        let wall_ms = crate::scheduling::now_ms();
        let saved = (|| {
            let (first, now) = match job.next.take() {
                Some(next) => next,
                None => job.allocate(&h.names)?,
            };
            let bytes = crate::frame::encode(frame, &job.format)?;
            crate::naming::save_named(
                &h.names,
                &job.plan,
                &job.input(now),
                job.on_exists,
                first.clone(),
                |path| {
                    job.resolved.contain(path)?;
                    crate::storage::save_bytes(&bytes, path, &job.storage)
                },
            )
            .with_context(|| format!("save {}", first.display()))
        })();
        match saved {
            Ok((path, stored)) => {
                report_image(
                    h,
                    &job.resolved,
                    &path,
                    stored,
                    frame,
                    &job.format,
                    wall_ms,
                    Some(job.info.id),
                    &job.info.context,
                );
                job.info.captured += 1;
                job.info.last_file = Some(path.to_string_lossy().into_owned());
                job.deadline = None;
                job.warm = None;
                if job.info.captured >= job.info.count {
                    job.info.status = "complete".into();
                    h.log(
                        "info",
                        format!(
                            "Capture job {} complete: {} frame(s)",
                            job.info.id, job.info.captured
                        ),
                    );
                } else {
                    job.info.status = "pending".into();
                    job.info.next_at_ms = job.info.first_at_ms.saturating_add(
                        job.info
                            .interval_ms
                            .saturating_mul(job.info.captured as u64),
                    );
                }
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
    sync_jobs(h, jobs);
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

fn worker(h: WorkerHandle, rx: Receiver<Envelope>) {
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
    while !h.cancelled() {
        let receiving = camera.as_ref().is_some_and(|c| {
            c.is_streaming()
                && (!c.is_scanner() || jobs.iter().any(|j| j.info.active() && j.deadline.is_some()))
        });
        let envelope = if receiving {
            rx.try_recv().ok()
        } else {
            rx.recv_timeout(Duration::from_millis(100)).ok()
        };
        if let Some(e) = envelope {
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
            if let Err(err) = &result {
                h.log("error", format!("{err:#}"));
            }
            let _ = e.reply.send(result);
            rate_epoch = Instant::now();
            rate_count = 0;
            consecutive_errors = 0;
        }
        tick_jobs(
            &h,
            &mut camera,
            &mut jobs,
            &mut scheduled_stream,
            forwarder.is_some(),
            auto.is_some(),
        );
        if let (Some(controller), Some(c)) = (auto.as_mut(), camera.as_mut())
            && controller.tick(c, Instant::now())
        {
            publish_auto(&h, controller);
        }
        h.poll_capture();
        h.flush_ring();
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
                    capture_jobs(&h, &frame, &mut jobs, settled);
                    // Publish first: the forwarder then takes the sensor
                    // buffer itself instead of a copy of it.
                    let forward = forwarder.is_some();
                    if let (Some((f, _)), Some(frame)) =
                        (forwarder.as_mut(), h.record(frame, forward))
                        && let Err(e) = f.push(frame)
                    {
                        h.log("error", format!("Forwarding stopped: {e:#}"));
                        let _ = stop_forward(&h, &mut forwarder, &mut auto);
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
    if let Some((mut f, _)) = forwarder {
        let _ = f.stop();
    }
    let _ = release_auto(&h, &mut auto, &mut camera, true);
    // Close/checkpoint the session even when callers retain WorkerHandle clones.
    h.capture_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(mut c) = camera
        && let Err(e) = c.stop()
    {
        h.log("warn", format!("Acquisition cleanup: {e:#}"));
    }
}

#[cfg(test)]
mod autocapture_tests;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_jobs_reject_overlapping_output_files() {
        let handle = WorkerHandle::new();
        handle
            .request(SessionCommand::Connect {
                camera: "sim:0".into(),
                timeout_ms: 100,
            })
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
        };
        let mut paths = Vec::new();
        let handles: Vec<_> = (0..2).map(|_| WorkerHandle::new()).collect();
        for h in &handles {
            h.request(SessionCommand::Connect {
                camera: "sim:0".into(),
                timeout_ms: 100,
            })
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
    #[test]
    fn session_simulator_configuration_and_acquisition() {
        let h = WorkerHandle::new();
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 100,
        })
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
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 100,
        })
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
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 100,
        })
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
                [1, 2].map(|i| format!("frames/frame-{:06}.raw", run * 2 + i))
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
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 100,
        })
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
            info: crate::scheduling::CaptureJob {
                id: 1,
                status: "pending".into(),
                output: "scan.png".into(),
                destination: None,
                count: 1,
                captured: 0,
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
        h.request(SessionCommand::Connect {
            camera: clip.to_string_lossy().into(),
            timeout_ms: 3000,
        })
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
        h.request(SessionCommand::Connect {
            camera: "rtsp://user:secret@127.0.0.1:9/live".into(),
            timeout_ms: 2000,
        })
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
}
