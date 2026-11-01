//! A single camera owner in a camera worker: one thread runs the camera's
//! commands, captures, recordings and scheduled jobs. It never waits on its
//! coordinator: replies and events go to an outbox, requests wait in a queue
//! a cancel can take them back from, and a heartbeat says it still runs.
use crate::{
    auto::AutoController,
    camera::Camera,
    media::{ForwardConfig, Forwarder, StreamEnd},
    session::protocol::{JobStatus, Output, WorkerCommand},
    types::Frame,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[cfg(test)]
use crate::session::{FeatureValue, LogEntry};
use crate::session::{ForwardingProgress, SessionCommand, WorkerSnapshot};
pub(crate) mod events;
mod history;
pub(crate) mod inspector;
mod lock;
mod network_defaults;
#[cfg(test)]
mod network_tests;
pub(crate) mod saved;
mod saver;
use events::Events;
use history::process;
#[cfg(test)]
type Reply = Receiver<Result<Value>>;
/// Where a request's answer goes.
type Answer = Box<dyn FnOnce(Result<Value>) + Send>;
struct Envelope {
    /// The coordinator's request number; none for requests made in this
    /// process.
    id: Option<u64>,
    command: WorkerCommand,
    reply: Answer,
}
/// Requests the camera thread has yet to take.
pub(crate) const QUEUE: usize = 32;
/// Requests waiting for the camera thread. A cancel takes one back.
#[derive(Default)]
struct Queue {
    waiting: Mutex<Waiting>,
    ready: Condvar,
}
#[derive(Default)]
struct Waiting {
    requests: VecDeque<Envelope>,
    /// Look at jobs and the quit flag before the wait ends.
    woken: bool,
}
impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Waiting> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Queue `envelope`, or hand it back when the queue is full.
    fn push(&self, envelope: Envelope) -> std::result::Result<(), Box<Envelope>> {
        let mut waiting = self.lock();
        if waiting.requests.len() >= QUEUE {
            return Err(Box::new(envelope));
        }
        waiting.requests.push_back(envelope);
        drop(waiting);
        self.ready.notify_one();
        Ok(())
    }
    /// The next request, waiting up to `wait` for one; None when the wait
    /// ended or the camera thread was woken.
    fn take(&self, wait: Duration) -> Option<Envelope> {
        let mut waiting = self.lock();
        if waiting.requests.is_empty() && !waiting.woken && !wait.is_zero() {
            waiting = self
                .ready
                .wait_timeout_while(waiting, wait, |w| w.requests.is_empty() && !w.woken)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        waiting.woken = false;
        waiting.requests.pop_front()
    }
    /// Take request `id` back while it still waits.
    fn remove(&self, id: u64) -> Option<Envelope> {
        let mut waiting = self.lock();
        let position = waiting
            .requests
            .iter()
            .position(|envelope| envelope.id == Some(id))?;
        waiting.requests.remove(position)
    }
    fn drain(&self) -> Vec<Envelope> {
        self.lock().requests.drain(..).collect()
    }
    fn wake(&self) {
        self.lock().woken = true;
        self.ready.notify_one();
    }
}
/// The request the camera thread is running, and its cancel flag.
struct Running {
    id: Option<u64>,
    cancel: Arc<AtomicBool>,
}
/// Milliseconds on a clock that never goes back, for the heartbeat.
fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
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

#[derive(Clone)]
pub struct WorkerHandle {
    queue: Arc<Queue>,
    state: Arc<RwLock<WorkerSnapshot>>,
    counters: Arc<Counters>,
    quitting: Arc<AtomicBool>,
    running: Arc<Mutex<Option<Running>>>,
    /// Cancel flags of requests answered later, such as frame requests
    /// waiting for a stream's frames.
    deferred: Arc<Mutex<BTreeMap<u64, Arc<AtomicBool>>>>,
    /// Jobs asked to stop, until the camera thread ends them.
    stops: Arc<Mutex<BTreeSet<u64>>>,
    /// When the camera thread last made progress, on `clock_ms`.
    beat: Arc<AtomicU64>,
    events: Events,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    /// Where a direct capture to stdout writes its image: a pipe the
    /// command that started this worker reads.
    output: Arc<Mutex<Option<std::fs::File>>>,
    /// The local recording in progress, reported when it stops.
    recording: Arc<Mutex<Option<crate::scheduling::RecordingMeta>>>,
    /// Private timing state, bound to the exact worker-issued forward ID.
    forward_clock: Arc<Mutex<Option<ForwardClock>>>,
    /// The job of Capturefab's continuous auto exposure, while it runs.
    auto_job: Arc<Mutex<Option<u64>>>,
    /// Output numbers handed out, so concurrent captures never share a name.
    names: Arc<Mutex<crate::naming::Namer>>,
    /// Saved camera sign-ins, read when a camera opens.
    logins: crate::logins::Logins,
    /// The session this worker serves, which its camera lock names.
    session: Arc<str>,
    /// The lock of the camera it holds open.
    lock: Arc<Mutex<Option<lock::CameraLock>>>,
}
impl Default for WorkerHandle {
    fn default() -> Self {
        Self::new()
    }
}
impl WorkerHandle {
    pub fn new() -> Self {
        Self::create(
            None,
            crate::logins::Logins::system(),
            Events::detached(),
            crate::session::DIRECT,
        )
    }
    /// The camera thread of a worker process for session `session`, sending
    /// its replies and events to `events`, and an image captured to stdout
    /// to `output`.
    pub(crate) fn serving(events: Events, session: String, output: Option<std::fs::File>) -> Self {
        Self::create(output, crate::logins::Logins::system(), events, &session)
    }
    #[cfg(all(test, feature = "keychain"))]
    fn with_logins(logins: crate::logins::Logins) -> Self {
        Self::create(None, logins, Events::detached(), crate::session::DIRECT)
    }
    fn create(
        output: Option<std::fs::File>,
        logins: crate::logins::Logins,
        events: Events,
        session: &str,
    ) -> Self {
        let handle = Self::unstarted(output, logins, events, session);
        let worker_handle = handle.clone();
        let thread = std::thread::Builder::new()
            .name("capturefab-camera".into())
            .spawn(move || worker(worker_handle))
            .expect("spawn camera worker");
        *handle.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread);
        handle
    }
    /// A handle without a camera thread, for tests that run commands
    /// directly.
    #[cfg(test)]
    fn idle() -> Self {
        Self::unstarted(
            None,
            crate::logins::Logins::system(),
            Events::detached(),
            crate::session::DIRECT,
        )
    }
    fn unstarted(
        output: Option<std::fs::File>,
        logins: crate::logins::Logins,
        events: Events,
        session: &str,
    ) -> Self {
        Self {
            queue: Arc::default(),
            state: Arc::new(RwLock::new(WorkerSnapshot::default())),
            counters: Arc::default(),
            quitting: Arc::new(AtomicBool::new(false)),
            running: Arc::default(),
            deferred: Arc::default(),
            stops: Arc::default(),
            beat: Arc::new(AtomicU64::new(clock_ms())),
            events,
            thread: Arc::new(Mutex::new(None)),
            output: Arc::new(Mutex::new(output)),
            recording: Arc::default(),
            forward_clock: Arc::default(),
            auto_job: Arc::default(),
            names: Arc::default(),
            logins,
            session: session.into(),
            lock: Arc::default(),
        }
    }
    pub fn snapshot(&self) -> WorkerSnapshot {
        let mut s = self.state.read().unwrap_or_else(|e| e.into_inner()).clone();
        s.frames = self.counters.frames.load(Ordering::Relaxed);
        s.received_bytes = self.counters.received_bytes.load(Ordering::Relaxed);
        s.dropped = self.counters.dropped.load(Ordering::Relaxed);
        s.fps = f64::from_bits(self.counters.fps_bits.load(Ordering::Relaxed));
        s
    }
    /// Report a published file: as a `saved` line of its job, or of no job
    /// for a capture command's.
    pub(crate) fn push_saved(&self, job: Option<u64>, file: crate::engine::saved::SavedFile) {
        self.events.emit(Output::Saved { job, file });
    }
    /// The saved-file lines waiting in a detached outbox, for tests.
    #[cfg(test)]
    pub(crate) fn take_saved(&self) -> Vec<crate::engine::saved::SavedFile> {
        self.events
            .drain()
            .into_iter()
            .filter_map(|line| match line {
                Output::Saved { file, .. } => Some(file),
                _ => None,
            })
            .collect()
    }
    /// The lines waiting in a detached outbox, for tests.
    #[cfg(test)]
    pub(crate) fn take_events(&self) -> Vec<Output> {
        self.events.drain()
    }
    /// Queue `command` and the receiver its answer comes to, as a worker's
    /// stdin reader does, for tests.
    #[cfg(test)]
    pub fn submit(&self, command: impl Into<WorkerCommand>) -> Result<Reply> {
        let (reply, rx) = mpsc::channel();
        self.queue_request(
            None,
            command.into(),
            Box::new(move |result| {
                let _ = reply.send(result);
            }),
        )
        .map_err(|(error, _)| error)?;
        Ok(rx)
    }
    #[cfg(test)]
    pub fn request(&self, command: impl Into<WorkerCommand>) -> Result<Value> {
        self.submit(command)?
            .recv()
            .context("camera worker stopped")?
    }
    /// Queue `command` as request `id`, answered through `reply`. A request
    /// that is refused is handed back with why.
    pub(crate) fn queue_request(
        &self,
        id: Option<u64>,
        command: WorkerCommand,
        reply: Answer,
    ) -> std::result::Result<(), (anyhow::Error, Answer)> {
        if let Err(error) = command.validate() {
            return Err((error, reply));
        }
        if self.quitting.load(Ordering::Acquire) {
            return Err((crate::session::stopping(), reply));
        }
        self.queue
            .push(Envelope { id, command, reply })
            .map_err(|envelope| {
                (
                    crate::types::Categorized::new(
                        crate::types::ErrorKind::CameraBusy,
                        format!("camera busy: {QUEUE} requests are queued"),
                    )
                    .hint("wait for the camera's earlier commands to finish")
                    .into(),
                    envelope.reply,
                )
            })
    }
    /// Cancel request `id`: a queued one is answered `interrupted` at once,
    /// a running one stops at its next frame wait. Others are ignored.
    pub(crate) fn cancel(&self, id: u64) {
        if let Some(envelope) = self.queue.remove(id) {
            (envelope.reply)(Err(interrupted("cancelled before it started")));
            return;
        }
        if let Some(running) = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|running| running.id == Some(id))
        {
            running.cancel.store(true, Ordering::Release);
        }
        if let Some(cancel) = self
            .deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            cancel.store(true, Ordering::Release);
            self.wake();
        }
    }
    /// Stop job `job` at the camera thread's next turn; its `job_end`
    /// follows. Unknown and finished jobs are ignored.
    pub(crate) fn stop_job(&self, job: u64) {
        let mut stops = self.stops.lock().unwrap_or_else(|e| e.into_inner());
        stops.insert(job);
        // A stop for a job still being scheduled waits for it; old ones go.
        while stops.len() > 256 {
            stops.pop_first();
        }
        drop(stops);
        self.wake();
    }
    /// How long ago the camera thread last made progress.
    pub(crate) fn heartbeat_age(&self) -> Duration {
        Duration::from_millis(clock_ms().saturating_sub(self.beat.load(Ordering::Acquire)))
    }
    fn beat(&self) {
        self.beat.store(clock_ms(), Ordering::Release);
    }
    /// Stop taking requests, answer the queued ones `interrupted`, end the
    /// camera's jobs and recording, and wait for the camera thread.
    pub fn shutdown(&self) {
        self.quitting.store(true, Ordering::Release);
        for envelope in self.queue.drain() {
            (envelope.reply)(Err(crate::session::stopping()));
        }
        self.wake();
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
    /// Whether the camera thread has ended.
    pub(crate) fn finished(&self) -> bool {
        self.thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
    /// Have the camera thread look at its jobs and the quit flag now rather
    /// than when its wait ends.
    fn wake(&self) {
        self.queue.wake();
    }
    /// Whether the request in progress was cancelled, or the worker is
    /// shutting down: what ends frame waits.
    fn cancelled(&self) -> bool {
        self.quitting.load(Ordering::Acquire)
            || self
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|running| running.cancel.load(Ordering::Acquire))
    }
    fn log(&self, level: &str, message: String) {
        self.events.log(level, &message);
        push_log(
            &mut self.state.write().unwrap_or_else(|e| e.into_inner()),
            level,
            message,
        );
    }
    /// Count a frame the camera delivered.
    fn count_frame(&self, frame: &Frame) {
        self.counters.frames.fetch_add(1, Ordering::Relaxed);
        self.counters
            .received_bytes
            .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
    }
    /// Write an image captured to stdout: to `file`, which a session client
    /// named, or else to the pipe of the command that started this worker,
    /// which is closed after it.
    fn write_stdout(&self, file: Option<&str>, image: &[u8]) -> Result<()> {
        use std::io::Write as _;
        let Some(file) = file else {
            let mut output = self
                .output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .context("this camera worker has no stdout for an image; save it to a file")?;
            output.write_all(image)?;
            return output.flush().map_err(Into::into);
        };
        let path = crate::session::stdout_file(file)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut written = options
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        written.write_all(image)?;
        Ok(())
    }
}
/// A request or job cut short.
fn interrupted(message: &str) -> anyhow::Error {
    crate::types::Categorized::error(crate::types::ErrorKind::Interrupted, message)
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
    end_auto_job(h, JobStatus::Done);
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
            return categorized.kind.code().1 == 4;
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
        h.beat();
        // A worker shutting down ends a wait as its session does, not as a
        // signal to its caller would.
        if h.quitting.load(Ordering::Acquire) {
            return Err(crate::session::stopping());
        }
        if h.cancelled() {
            return Err(interrupted("capture interrupted"));
        }
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

/// Let auto exposure settle on a camera that has just started, metering
/// its frames, for at most half of `timeout` and 3 s. False when it had not
/// settled by then.
fn settle_auto(
    h: &WorkerHandle,
    c: &mut Camera,
    auto: &mut Option<AutoController>,
    timeout: Duration,
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
        h.count_frame(&frame);
    }
    Ok(true)
}

/// Write `values` in order, each as Set would: later values may depend on
/// earlier ones (a selector, then the value it selects). `checkpoint` runs
/// just before each write, with the bank that write changes selected, so
/// the rollback checkpoint can record the value it replaces.
#[cfg(test)]
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

/// Note a line the worker logged: an error becomes its `last_error`, and
/// tests keep the line.
fn push_log(s: &mut WorkerSnapshot, level: &str, message: String) {
    if level == "error" {
        s.last_error = Some(message.clone());
        s.last_error_tag = None;
    }
    #[cfg(test)]
    {
        s.logs.push(LogEntry::now(level, message));
        s.logged += 1;
        if s.logs.len() > 200 {
            s.logs.remove(0);
        }
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
        recording: None,
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
    let (id, job, path) = {
        let state = h.state.read().unwrap_or_else(|error| error.into_inner());
        (
            state.forwarding_id.clone(),
            state.forwarding_job,
            state.recording_path.clone(),
        )
    };
    let id = forwarder.as_ref().and(id.as_deref());
    let progress = sampler.sample(id, now, || {
        forwarder.as_ref().expect("active forwarder").0.stats()
    });
    if let Some(progress) = progress {
        if let Some(job) = job {
            let mut value = json!({
                "accepted": progress.accepted,
                "written": progress.written,
                "dropped": progress.dropped,
                "late": progress.late,
                "stalled": progress.stalled,
            });
            if let Some(path) = path {
                value["path"] = json!(path);
            }
            h.events.progress(job, value);
        }
        let mut state = h.state.write().unwrap_or_else(|error| error.into_inner());
        if state.forwarding_id.as_deref() == Some(&progress.id) {
            state.forwarding_progress = Some(progress);
        }
    }
}

/// How a recording or forward ended, as its job reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// Its timer.
    Duration,
    /// The file reached the size asked for.
    MaxSize,
    /// A stop: of its job, the camera or the recording.
    Stopped,
    Failed,
    /// The worker shut down.
    Interrupted,
}
impl Ended {
    fn name(self) -> &'static str {
        match self {
            Self::Duration => "duration",
            Self::MaxSize => "max_size",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
    fn status(self) -> JobStatus {
        match self {
            Self::Failed => JobStatus::Failed,
            Self::Interrupted => JobStatus::Interrupted,
            Self::Duration | Self::MaxSize | Self::Stopped => JobStatus::Done,
        }
    }
}

/// Stop forwarding; the stopped forwarder's final counters, if one ran.
fn stop_forward(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    ended: Ended,
) -> Result<Option<crate::media::ForwardStats>> {
    end_forward(h, forwarder, auto, ended, None, |_| None)
}
/// How a forward ended, for its log line: a level, a message and, for an
/// error, its reason.
type ForwardEnd = (&'static str, String, Option<crate::types::ErrorTag>);
/// Stop the forward, and log the line `report` makes of how it ended (a
/// level and message) in the same step that clears the forwarding state: a
/// status never shows the forward gone without the error it ended with, nor
/// still running with that error. Its job ends with `ended`, failed by
/// `cause` or by a stop that failed, after the `saved` line of its file.
fn end_forward(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    ended: Ended,
    cause: Option<&anyhow::Error>,
    report: impl FnOnce(&Result<Option<crate::media::ForwardStats>>) -> Option<ForwardEnd>,
) -> Result<Option<crate::media::ForwardStats>> {
    if let Some(a) = auto.as_mut()
        && let Err(error) = a.set_forward_fps(None)
    {
        h.log("warn", format!("Auto mode after the recording: {error:#}"));
    }
    let meta = h.recording.lock().unwrap_or_else(|e| e.into_inner()).take();
    let (id, job, since_ms, until_ms) = {
        let state = h.state.read().unwrap_or_else(|e| e.into_inner());
        (
            state.forwarding_id.clone(),
            state.forwarding_job,
            state.forwarding_since_ms,
            state.forwarding_until_ms,
        )
    };
    let mut final_progress = None;
    let mut summary = json!({"ended": ended.name()});
    let mut local = false;
    let stopped = match forwarder.take() {
        Some((mut f, _)) => {
            local = f.recording_path().is_some();
            let stopped = f.stop();
            let stats = f.stats();
            summary["written"] = json!(stats.written);
            summary["dropped"] = json!(stats.dropped);
            summary["accepted"] = json!(stats.accepted);
            summary["encoder"] = json!(stats.encoder);
            final_progress = id.map(|id| ForwardingProgress {
                recording: stats
                    .recording
                    .as_ref()
                    .map(|file| crate::session::FinishedRecording {
                        path: absolute_path(std::path::Path::new(&file.path)),
                        bytes: file.bytes,
                        started_ms: file.started_ms,
                        ended_ms: file.ended_ms,
                        encoder: file.encoder.clone(),
                    }),
                ..compact_forwarding(id, &stats)
            });
            // A finished file is reported even when stopping reported an error.
            if let Some(file) = stats.recording.clone() {
                let path = absolute_path(std::path::Path::new(&file.path));
                summary["recording"] = json!({
                    "path": path,
                    "bytes": file.bytes,
                    "started_ms": file.started_ms,
                    "ended_ms": file.ended_ms,
                    "encoder": file.encoder,
                });
                let context = meta.map(|m| m.context).unwrap_or_default();
                h.push_saved(
                    job,
                    crate::engine::saved::SavedFile {
                        wall_ms: file.started_ms,
                        path,
                        bytes: file.bytes,
                        sha256: Some(file.sha256),
                        context,
                        item: crate::engine::saved::SavedItem::Recording {
                            started_ms: file.started_ms,
                            ended_ms: file.ended_ms,
                            codec: file.codec,
                            encoder: file.encoder,
                            fps: file.fps,
                        },
                    },
                );
            }
            stopped.map(|()| Some(stats))
        }
        None => Ok(None),
    };
    *h.forward_clock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Some(job) = job {
        summary["since_ms"] = json!(since_ms);
        if let Some(until_ms) = until_ms {
            summary["until_ms"] = json!(until_ms);
        }
        summary["ended_ms"] = json!(crate::scheduling::now_ms());
        summary["finalized"] =
            json!(stopped.is_ok() && (!local || summary.get("recording").is_some()));
        let (status, error) = match (cause, &stopped) {
            (Some(cause), _) => (ended.status(), Some(cause)),
            (None, Err(error)) => (JobStatus::Failed, Some(error)),
            (None, Ok(_)) => (ended.status(), None),
        };
        h.events.emit(Output::JobEnd {
            job,
            status,
            error: error
                .filter(|_| status == JobStatus::Failed)
                .map(crate::session::protocol::wire_error),
            summary,
        });
    }
    // Last: whoever sees the forward gone (a client waiting out a timed
    // recording, say) finds its file finished and reported.
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    if let Some((level, message, tag)) = report(&stopped) {
        drop(s);
        h.events.log(level, &message);
        s = h.state.write().unwrap_or_else(|e| e.into_inner());
        push_log(&mut s, level, message);
        if level == "error" {
            s.last_error_tag = tag;
        }
    }
    if let Some(progress) = final_progress {
        s.forwarding_progress = Some(progress);
    }
    s.forwarding = None;
    s.forwarding_id = None;
    s.forwarding_job = None;
    s.recording_path = None;
    s.forwarding_since_ms = None;
    s.forwarding_until_ms = None;
    stopped
}
/// The requested length of a timed recording or forward whose end time has
/// come at `now_ms` (UTC Unix milliseconds), or `None` while it should run on.
fn forward_due(state: &WorkerSnapshot, now_ms: u64) -> Option<u64> {
    let until = state.forwarding_until_ms.filter(|until| now_ms >= *until)?;
    Some(until.saturating_sub(state.forwarding_since_ms.unwrap_or(until)))
}
/// Stop a timed recording or forward once its end time has come, so its
/// file is finished and its job ends whether or not the client that started
/// it is still there.
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
    let _ = end_forward(h, forwarder, auto, Ended::Duration, None, |stopped| {
        Some(match stopped {
            Ok(_) => ("info", format!("{what} finished after {length}"), None),
            Err(e) => (
                "error",
                format!("{what} stopped after {length}: {e:#}"),
                crate::types::error_tag(e),
            ),
        })
    });
}
/// Pick a free name for a recording template and create the file, moving to the next free name when another writer (another
/// camera's worker) took it first.
fn name_recording(
    h: &WorkerHandle,
    config: &ForwardConfig,
    camera: &crate::naming::CameraName,
    on_exists: crate::naming::OnExists,
    context: &crate::context::Context,
    utc_offset_s: Option<i32>,
) -> Result<(PathBuf, crate::storage::StorageWriter)> {
    crate::naming::validate_output(&config.output)?;
    let local = crate::media::local_file_path(&config.output)?;
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
        now: crate::naming::clock(utc_offset_s),
        index: 0,
        context,
    };
    let first = h
        .names
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .candidate(&plan, &input, on_exists)?;
    let (path, writer) =
        crate::naming::save_named(&h.names, &plan, &input, on_exists, first, |path| {
            check_parents(path)?;
            config.writer(path)
        })?;
    Ok((path, writer))
}
fn absolute_path(path: &std::path::Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}
/// Acquisition features recorded with each saved still when the camera has
/// them. The list is fixed, short and holds no addresses or names, so the
/// snapshot stays small and never carries credentials.
pub(crate) const ACQUISITION_FEATURES: &[&str] = &[
    "ExposureTime",
    "ExposureTimeAbs",
    "ExposureAuto",
    "Gain",
    "GainRaw",
    "GainAuto",
    "BlackLevel",
    "Gamma",
    "BalanceWhiteAuto",
    "AcquisitionFrameRate",
    "AcquisitionFrameRateAbs",
    "TriggerMode",
    "TriggerSource",
    "BinningHorizontal",
    "BinningVertical",
    "OffsetX",
    "OffsetY",
    "ReverseX",
    "ReverseY",
];

/// What a camera was doing when a still was taken: its make and the current
/// values of [`ACQUISITION_FEATURES`]. Read once per capture command (and per
/// frame while auto exposure adjusts the camera) and once per scheduled shot.
#[derive(Debug, Clone, Default)]
pub(super) struct Acquisition {
    vendor: String,
    model: String,
    pub(super) settings: serde_json::Map<String, Value>,
}

impl Acquisition {
    /// Whether the camera's own auto exposure, gain or white balance was on,
    /// so its values may differ on the next frame.
    fn firmware_auto(&self) -> bool {
        ["ExposureAuto", "GainAuto", "BalanceWhiteAuto"]
            .iter()
            .any(|name| {
                self.settings
                    .get(*name)
                    .and_then(Value::as_str)
                    .is_some_and(|mode| !mode.eq_ignore_ascii_case("off"))
            })
    }
}

fn acquisition(c: &mut Camera) -> Acquisition {
    let mut settings = serde_json::Map::new();
    for &name in ACQUISITION_FEATURES {
        if !c.has(name) {
            continue;
        }
        // Unreadable features (locked, unavailable now) are left out.
        let value = match c.get(name) {
            Ok(Value::String(text)) => Value::String(bounded(&text, 64)),
            Ok(value @ (Value::Number(_) | Value::Bool(_))) => value,
            _ => continue,
        };
        settings.insert(name.into(), value);
    }
    Acquisition {
        vendor: bounded(&c.info.vendor, 128),
        model: bounded(&c.info.model, 128),
        settings,
    }
}

/// The worker's last [`acquisition`] read, reused for scheduled shots until
/// a command or auto exposure may have changed the camera, or for at most
/// [`AcquisitionCache::MAX_AGE`] so camera-side auto features stay current.
#[derive(Default)]
struct AcquisitionCache {
    read: Option<(Instant, Acquisition)>,
}

impl AcquisitionCache {
    const MAX_AGE: Duration = Duration::from_secs(1);

    fn invalidate(&mut self) {
        self.read = None;
    }

    fn get(&mut self, c: &mut Camera, now: Instant) -> Acquisition {
        match &self.read {
            Some((at, cached)) if now.saturating_duration_since(*at) < Self::MAX_AGE => {
                cached.clone()
            }
            _ => {
                let fresh = acquisition(c);
                self.read = Some((now, fresh.clone()));
                fresh
            }
        }
    }
}

fn bounded(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// The provenance JSON written into a saved PNG or JPEG: frame fields, the
/// camera's make, its acquisition settings and the capture context. Never a
/// serial, address or source URL, which can identify a site or carry
/// credentials.
fn still_meta(
    frame: &Frame,
    wall_ms: u64,
    acquisition: &Acquisition,
    context: &crate::context::Context,
) -> Value {
    let mut meta = json!({
        "capturefab": env!("CARGO_PKG_VERSION"),
        "frame_id": frame.id,
        "timestamp_ns": frame.timestamp_ns,
        "wall_ms": wall_ms,
        "width": frame.width,
        "height": frame.height,
        "pixel_format": crate::frame::pixel_format_name(frame.pixel_format),
    });
    if !acquisition.vendor.is_empty() || !acquisition.model.is_empty() {
        meta["camera"] = json!({"vendor": acquisition.vendor, "model": acquisition.model});
    }
    if !acquisition.settings.is_empty() {
        meta["settings"] = Value::Object(acquisition.settings.clone());
    }
    if !context.is_empty() {
        meta["context"] = json!(context);
    }
    meta
}

/// A still as `frame::encode_with_meta` encodes it, but a JPEG at `quality`
/// when one other than the default is asked for.
pub(crate) fn encode_still(
    frame: &Frame,
    format: &str,
    quality: Option<u8>,
    meta: Option<&Value>,
) -> Result<Vec<u8>> {
    match quality {
        Some(quality) if format == "jpeg" && i32::from(quality) != crate::jpeg::QUALITY => {
            let bytes = crate::jpeg::software_quality(
                &crate::jpeg::Raster::new(frame)?,
                i32::from(quality),
            )?;
            let Some(meta) = meta else {
                return Ok(bytes);
            };
            let json = serde_json::to_string(meta)?;
            ensure!(
                json.len() <= crate::frame::MAX_META,
                "capture metadata exceeds {} bytes",
                crate::frame::MAX_META
            );
            crate::jpeg::insert_com(&bytes, json.as_bytes())
        }
        _ => crate::frame::encode_with_meta(frame, format, meta),
    }
}

/// Record a locally saved image in the saved-file log.
#[allow(clippy::too_many_arguments)]
fn report_image(
    h: &WorkerHandle,
    path: &std::path::Path,
    stored: crate::storage::Stored,
    frame: &Frame,
    format: &str,
    wall_ms: u64,
    job_id: Option<u64>,
    context: &crate::context::Context,
) {
    h.push_saved(
        job_id,
        crate::engine::saved::SavedFile {
            wall_ms,
            path: absolute_path(path),
            bytes: stored.bytes,
            sha256: Some(stored.sha256),
            context: context.clone(),
            item: crate::engine::saved::SavedItem::Capture {
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
        },
    );
}

/// Enter auto mode, or change its balance: the `auto` command, which a
/// restored setup with an auto policy also runs.
fn enter_auto(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    auto: &mut Option<AutoController>,
    forwarder: &Option<(Forwarder, f64)>,
    jobs: &[Scheduled],
    balance: Option<f64>,
    job: Option<u64>,
) -> Result<Value> {
    let c = camera_mut(camera)?;
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
    if let Some(job) = job {
        // A new continuous run replaces the one before.
        end_auto_job(h, JobStatus::Done);
        *h.auto_job.lock().unwrap_or_else(|e| e.into_inner()) = Some(job);
        h.events.progress(job, json!({"auto": status}));
    }
    Ok(json!({"auto":status}))
}
/// End the job of Capturefab's continuous auto exposure, if one runs.
fn end_auto_job(h: &WorkerHandle, status: JobStatus) {
    let job = h.auto_job.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(job) = job {
        h.events.emit(Output::JobEnd {
            job,
            status,
            error: None,
            summary: json!({}),
        });
    }
}
/// A network scan's tuning of this camera.
fn optimize_network(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &[Scheduled],
    bytes_per_second: u64,
) -> Result<Value> {
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
#[allow(clippy::too_many_arguments)]
fn process_plain(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &mut Vec<Scheduled>,
    next_job: &mut u64,
    scheduled_stream: &mut bool,
    command: WorkerCommand,
) -> Result<Value> {
    let command = match command {
        WorkerCommand::OptimizeNetwork { bytes_per_second } => {
            return optimize_network(h, camera, forwarder, auto, jobs, bytes_per_second);
        }
        #[cfg(test)]
        WorkerCommand::Get { feature } => {
            return Ok(json!({"name":feature,"value":camera_mut(camera)?.get(&feature)?}));
        }
        #[cfg(test)]
        WorkerCommand::Set { feature, value } => {
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
            return Ok(reply);
        }
        // The rollback checkpoint records each write; see `set_many`.
        #[cfg(test)]
        WorkerCommand::SetMany { values } => {
            return set_many(h, camera, auto, &values, |_, _| Ok(()));
        }
        #[cfg(test)]
        WorkerCommand::Execute { feature } => {
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
            return Ok(json!({"executed":feature}));
        }
        #[cfg(test)]
        WorkerCommand::Features => {
            refresh(h, camera, true);
            return Ok(json!({"features":h.snapshot().features}));
        }
        #[cfg(test)]
        WorkerCommand::StopForward => {
            let stats = stop_forward(h, forwarder, auto, Ended::Stopped)?;
            return Ok(json!({"forwarding":null,"stats":stats}));
        }
        WorkerCommand::Session(command) => command,
    };
    match command {
        SessionCommand::Frame { .. } => {
            anyhow::bail!("frame requests are taken up by the camera thread's loop")
        }
        SessionCommand::Status => {
            refresh(h, camera, false);
            Ok(serde_json::to_value(h.snapshot())?)
        }
        SessionCommand::ReadSetup
        | SessionCommand::SetupDiff { .. }
        | SessionCommand::ApplySetup { .. } => {
            anyhow::bail!("saved setups are read and restored before plain commands")
        }
        SessionCommand::Discover { .. } => anyhow::bail!("discovery belongs to the coordinator"),
        SessionCommand::NetworkScan { .. } => {
            anyhow::bail!("network scan belongs to the coordinator")
        }
        SessionCommand::Wait { .. } | SessionCommand::StopJob { .. } => {
            anyhow::bail!("jobs belong to the coordinator")
        }
        SessionCommand::Forward {
            output,
            codec,
            encoder,
            fps,
            bitrate,
            storage,
            max_file_bytes,
            on_exists,
            context,
            duration_ms,
            utc_offset_s,
            job,
            ..
        } => {
            ensure!(
                forwarder.is_none(),
                "the camera is already recording; capturefab jobs stop ends that recording"
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
            let mut config = ForwardConfig {
                output: output.clone(),
                codec,
                encoder,
                fps,
                bitrate,
                storage,
                max_file_bytes,
            };
            let named = crate::media::is_recording(&output)
                && (crate::naming::has_placeholder(&output) || !on_exists.is_fail());
            let f = if named {
                config.validate()?;
                let label = crate::naming::camera_name(&camera_mut(camera)?.info);
                let (path, reserved) =
                    name_recording(h, &config, &label, on_exists, &context, utc_offset_s)?;
                config.output = path.to_string_lossy().into_owned();
                Forwarder::with_resolved(config, Some(reserved))?
            } else {
                // Refuse a taken file now, not after this reply looks like
                // success; the writer still refuses it atomically.
                preflight_recording_output(&output, on_exists)?;
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
                s.forwarding_job = job;
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
                        context,
                    });
            }
            Ok(value)
        }
        SessionCommand::Connect {
            camera: selector,
            timeout_ms,
            login,
            saved_login,
        } => {
            if camera.is_some() {
                return Err(crate::types::Categorized::error(
                    crate::types::ErrorKind::CameraBusy,
                    "a camera is already connected; disconnect first",
                ));
            }
            let duration = timeout(timeout_ms)?;
            // Each worker opens one camera, so it has discovered nothing yet.
            let mut known = Vec::new();
            if !selector.starts_with("sim:")
                && !crate::media::is_source(&selector)
                && !crate::onvif::is_source(&selector)
                && !crate::camera::needs_no_discovery(&selector)
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
            }
            if !crate::camera::is_explicit_selector(&selector)
                && let crate::camera::Resolution::Loose { info, matched_on } =
                    crate::camera::resolve_selector(&selector, &known)
            {
                h.log(
                    "info",
                    format!(
                        "Using {} for '{}' (matched its {matched_on})",
                        crate::media::redact_url(&info.id),
                        crate::media::redact_url(&selector)
                    ),
                );
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
            // A device camera is locked to this worker before it opens, so
            // a second owner learns who holds it.
            let (opened, lock) = if crate::onvif::is_source(&selector) {
                let opened = Camera::open_with_login(&selector, &known, duration, login.as_ref())?;
                (opened, None)
            } else {
                let info = Camera::identify(&selector, &known, duration)?;
                let lock = lock::acquire(&info, &h.session)?;
                let opened = lock::open_released(
                    &info,
                    duration,
                    || Camera::open_identified(&selector, info.clone(), duration, login.as_ref()),
                    || h.beat(),
                )?;
                (opened, lock)
            };
            *camera = Some(opened);
            *h.lock.lock().unwrap_or_else(|e| e.into_inner()) = lock;
            drop(login);
            if let Some(address) = remembered {
                h.logins.touch(&address);
            }
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
            cancel_jobs(jobs, "cancelled");
            sync_jobs(h, jobs);
            *scheduled_stream = false;
            let auto_result = release_auto(h, auto, camera, true);
            let forward_result = stop_forward(h, forwarder, auto, Ended::Stopped);
            let camera_result = if let Some(c) = camera.as_mut() {
                c.stop()
            } else {
                Ok(())
            };
            *camera = None;
            h.lock.lock().unwrap_or_else(|e| e.into_inner()).take();
            refresh(h, camera, false);
            auto_result?;
            forward_result?;
            camera_result?;
            h.log("info", "Disconnected".into());
            Ok(json!({"connected":null}))
        }
        SessionCommand::GetValues { names } => {
            let c = camera_mut(camera)?;
            let values = crate::controls::get(c, &names)?;
            Ok(json!({"camera": crate::media::redact_url(&c.info.id), "values": values}))
        }
        SessionCommand::SetValues { values, dry_run } => {
            if !dry_run {
                // Writing a value Capturefab's auto mode manages ends it, as
                // set always has; any other write changes what it measures.
                // Decided by the features the change writes, so a control
                // (auto_exposure is ExposureAuto) counts like its feature,
                // and a camera auto never runs beside this one. Planning
                // also refuses invalid values, which leave auto mode as it
                // was.
                let managed = match auto.as_ref() {
                    Some(a) => {
                        let written = crate::controls::written(camera_mut(camera)?, &values)?;
                        written.iter().any(|feature| {
                            a.manages(feature)
                                || matches!(
                                    feature.as_str(),
                                    "ExposureAuto" | "GainAuto" | "BalanceWhiteAuto"
                                )
                        }) || values
                            .iter()
                            .any(|value| matches!(value.name.as_str(), "exposure" | "gain" | "fps"))
                    }
                    None => false,
                };
                if managed {
                    crate::controls::set(camera_mut(camera)?, &values, true)?;
                    release_auto(h, auto, camera, false)?;
                    h.log(
                        "info",
                        "Switched to manual mode because set changed its values".into(),
                    );
                } else if let Some(a) = auto.as_mut() {
                    a.invalidate();
                }
            }
            let c = camera_mut(camera)?;
            let result = crate::controls::set(c, &values, dry_run);
            refresh(h, camera, false);
            let result = result?;
            if let Some(changed) = &result.changed
                && !changed.is_empty()
            {
                let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
                for change in changed {
                    if let Some(feature) = s.features.iter_mut().find(|f| f.name == change.name) {
                        feature.value = Some(change.to.clone());
                    }
                }
                drop(s);
                h.log(
                    "info",
                    format!(
                        "Set {}",
                        changed
                            .iter()
                            .map(|change| format!(
                                "{} = {}",
                                change.name,
                                crate::controls::text(&change.to)
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            Ok(serde_json::to_value(result)?)
        }
        SessionCommand::Exec { name } => {
            if auto.is_some() && matches!(name.name.as_str(), "UserSetLoad" | "UserSetDefault") {
                release_auto(h, auto, camera, false)?;
                h.log(
                    "info",
                    format!("Switched to manual mode because {} was executed", name.name),
                );
            } else if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            let c = camera_mut(camera)?;
            let executed = crate::controls::exec(c, &name);
            let id = crate::media::redact_url(&c.info.id);
            refresh(h, camera, false);
            Ok(json!({"camera": id, "executed": executed?}))
        }
        SessionCommand::Info => {
            let c = camera_mut(camera)?;
            Ok(serde_json::to_value(crate::controls::info(c))?)
        }
        SessionCommand::FeatureList { filter } => {
            let c = camera_mut(camera)?;
            Ok(serde_json::to_value(crate::controls::features(
                c, &filter,
            )?)?)
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
        SessionCommand::Schedule {
            output,
            count,
            timeout_ms,
            format,
            first_at_ms,
            interval_ms,
            storage,
            on_exists,
            context,
            utc_offset_s,
            quality,
            job,
            ..
        } => {
            validate_capture(&output, count, timeout_ms, &format)?;
            preflight_capture_output(&output, count, &format, on_exists)?;
            validate_schedule_outputs(&output, count, &format, on_exists, jobs)?;
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
            let label = crate::naming::camera_name(&camera_mut(camera)?.info);
            let now_ms = crate::scheduling::now_ms();
            let first_at_ms = first_at_ms.max(now_ms);
            // The coordinator numbers jobs across a session's cameras.
            let id = match job {
                Some(id) => {
                    ensure!(
                        !jobs.iter().any(|j| j.info.id == id),
                        "capture job {id} already exists"
                    );
                    id
                }
                None => {
                    let id = *next_job;
                    *next_job = next_job.checked_add(1).context("job ID exhausted")?;
                    id
                }
            };
            let info = crate::scheduling::CaptureJob {
                id,
                status: "pending".into(),
                output: output.clone(),
                count,
                captured: 0,
                skipped: 0,
                retries: 0,
                first_at_ms,
                interval_ms,
                next_at_ms: first_at_ms,
                last_file: None,
                error: None,
                error_tag: None,
                context,
                created_ms: now_ms,
                ended_ms: None,
                files: Vec::new(),
                frame: None,
                first_ms: None,
                settings: serde_json::Map::new(),
            };
            jobs.push(Scheduled {
                info: info.clone(),
                plan: crate::naming::Plan::new(&output, count, &format),
                timeout_ms,
                format,
                storage,
                deadline: None,
                warm: None,
                camera: label,
                on_exists,
                next: None,
                saving: false,
                utc_offset_s,
                quality,
                reported: false,
            });
            trim_jobs(jobs);
            sync_jobs(h, jobs);
            h.log("info", format!("Capture job {} scheduled", info.id));
            Ok(json!({"job":info}))
        }
        SessionCommand::Auto { balance, job, .. } => {
            enter_auto(h, camera, auto, forwarder, jobs, balance, job)
        }
        SessionCommand::Manual { revert } => {
            let c = camera_mut(camera)?;
            let released = match auto.take() {
                Some(controller) => controller.release(c, revert),
                None if revert => Ok(()),
                None => crate::auto::hold(c),
            };
            h.state.write().unwrap_or_else(|e| e.into_inner()).auto = None;
            // Continuous auto exposure ended with auto mode.
            end_auto_job(h, JobStatus::Done);
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
            cancel_jobs(jobs, "cancelled");
            sync_jobs(h, jobs);
            *scheduled_stream = false;
            let forward_result = stop_forward(h, forwarder, auto, Ended::Stopped);
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
            on_exists,
            context,
            utc_offset_s,
            quality,
            stdout,
        } => {
            validate_capture(&output, count, timeout_ms, &format)?;
            storage.validate()?;
            if output != "-" {
                preflight_capture_output(&output, count, &format, on_exists)?;
            }
            let duration = Duration::from_millis(timeout_ms);
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
            let was_streaming = c.is_streaming();
            c.start()?;
            let result = (|| -> Result<Value> {
                let mut files = Vec::new();
                // The first frame's metadata: a reply lists files, never
                // every frame, so a long burst's reply stays small.
                let mut first_frame = Value::Null;
                let mut status = None;
                if !was_streaming
                    && auto.is_some()
                    && (c.software_triggered() || !settle_auto(h, c, auto, duration)?)
                {
                    h.log(
                        "warn",
                        "Auto exposure had not settled; capturing anyway".into(),
                    );
                }
                let label = crate::naming::camera_name(&c.info);
                let plan = crate::naming::Plan::new(&output, count, &format);
                let mut settings = acquisition(c);
                for index in 0..count {
                    let mut step = || -> Result<()> {
                        // Pick the name before requesting the frame: a feeder
                        // consumes paper irreversibly, so a taken name in fail
                        // mode is refused first.
                        let input = crate::naming::NameInput {
                            camera: &label,
                            now: crate::naming::clock(utc_offset_s),
                            index,
                            context: &context,
                        };
                        let first = if output == "-" {
                            None
                        } else {
                            let path = h
                                .names
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .candidate(&plan, &input, on_exists)?;
                            Some(path)
                        };
                        c.request_frame()?;
                        let frame = match next_frame(h, c, auto, deadline_after(duration))? {
                            Some(frame) => frame,
                            None => return Err(frame_wait_error(c, duration)),
                        };
                        if index == 0 {
                            status = auto.as_ref().map(|a| a.status().clone());
                        }
                        let wall_ms = crate::scheduling::now_ms();
                        // Auto exposure changes the camera between frames until
                        // it settles, and the camera's own auto features may
                        // change it on any frame.
                        if index > 0
                            && (auto.as_ref().is_some_and(|a| !a.converged())
                                || settings.firmware_auto())
                        {
                            settings = acquisition(c);
                        }
                        let mut metadata = serde_json::to_value(&frame)?;
                        metadata["pixel_format_name"] =
                            json!(crate::inspect::pixel_format_label(frame.pixel_format));
                        metadata["bytes"] = json!(frame.data.len());
                        metadata["wall_ms"] = json!(wall_ms);
                        if !settings.settings.is_empty() {
                            metadata["settings"] = Value::Object(settings.settings.clone());
                        }
                        if output == "-" {
                            let image = encode_still(&frame, &format, quality, None)?;
                            h.write_stdout(stdout.as_deref(), &image)?;
                            metadata["file_bytes"] = json!(image.len());
                        }
                        if let Some(first) = first {
                            let meta = still_meta(&frame, wall_ms, &settings, &context);
                            let bytes = encode_still(&frame, &format, quality, Some(&meta))?;
                            let (path, stored) = crate::naming::save_named(
                                &h.names,
                                &plan,
                                &input,
                                on_exists,
                                first.clone(),
                                |path| crate::storage::save_bytes(&bytes, path, &storage),
                            )
                            .with_context(|| format!("save {}", first.display()))?;
                            let file = path.to_string_lossy().into_owned();
                            report_image(
                                h, &path, stored, &frame, &format, wall_ms, None, &context,
                            );
                            metadata["file"] = json!(file);
                            metadata["file_bytes"] = json!(bytes.len());
                            files.push(file);
                        }
                        if index == 0 {
                            first_frame = metadata;
                        }
                        h.count_frame(&frame);
                        Ok(())
                    };
                    // The frames already saved stay; say which, keeping the
                    // failure's category (storage_full) in the chain.
                    if let Err(error) = step() {
                        return Err(if files.is_empty() {
                            error
                        } else {
                            let listed =
                                &files[..files.len().min(crate::scheduling::MAX_JOB_FILES)];
                            crate::types::with_details(
                                error.context(saved_before(&files, count)),
                                json!({ "files": listed }),
                            )
                        });
                    }
                }
                let listed = files.len().min(crate::scheduling::MAX_JOB_FILES);
                let mut value = json!({"files":files[..listed],"frame":first_frame,"count":count});
                if files.len() > listed {
                    value["truncated"] = json!(files.len() - listed);
                }
                if !context.is_empty() {
                    value["context"] = json!(context);
                }
                if let Some(status) = status {
                    value["auto"] = json!(status);
                }
                Ok(value)
            })();
            let stopped = if !was_streaming { c.stop() } else { Ok(()) };
            refresh(h, camera, false);
            let value = result?;
            stopped?;
            h.log("info", format!("Captured {count} frame(s)"));
            Ok(value)
        }
    }
}

struct Scheduled {
    info: crate::scheduling::CaptureJob,
    plan: crate::naming::Plan,
    timeout_ms: u64,
    format: String,
    storage: crate::storage::StoragePolicy,
    deadline: Option<Instant>,
    warm: Option<Instant>,
    /// What {camera} and {serial} name, taken when the job was scheduled.
    camera: crate::naming::CameraName,
    on_exists: crate::naming::OnExists,
    /// The name picked for the frame being requested, with its clock time.
    next: Option<(PathBuf, chrono::DateTime<chrono::FixedOffset>)>,
    /// A shot is with the saver; the next one waits for its outcome.
    saving: bool,
    /// The caller's UTC offset, for {date} and {time}.
    utc_offset_s: Option<i32>,
    /// JPEG quality, when not the default.
    quality: Option<u8>,
    /// Its `job_end` line was sent.
    reported: bool,
}
impl Scheduled {
    /// Pick the next frame's name: a free one.
    fn allocate(
        &self,
        names: &Mutex<crate::naming::Namer>,
    ) -> Result<(PathBuf, chrono::DateTime<chrono::FixedOffset>)> {
        let now = crate::naming::clock(self.utc_offset_s);
        let path = names.lock().unwrap_or_else(|e| e.into_inner()).candidate(
            &self.plan,
            &self.input(now),
            self.on_exists,
        )?;
        Ok((path, now))
    }
    fn input(&self, now: chrono::DateTime<chrono::FixedOffset>) -> crate::naming::NameInput<'_> {
        crate::naming::NameInput {
            camera: &self.camera,
            now,
            index: self.info.captured,
            context: &self.info.context,
        }
    }
    /// This job's file for the frame being shot, at the name picked for it.
    fn save(&self, path: PathBuf, now: chrono::DateTime<chrono::FixedOffset>) -> saver::Save {
        saver::Save {
            job: self.info.id,
            path,
            now,
            index: self.info.captured,
            camera: self.camera.clone(),
            plan: self.plan.clone(),
            on_exists: self.on_exists,
            format: self.format.clone(),
            quality: self.quality,
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
    crate::session::validate_frame_wait(timeout_ms)?;
    ensure!((1..=100_000).contains(&count), "count must be 1..100000");
    ensure!(
        ["png", "jpeg", "raw", "ppm", "pgm"].contains(&format),
        "format must be png, jpeg, raw, pgm or ppm"
    );
    validate_capture_output(output)?;
    Ok(())
}
pub fn validate_capture_output(output: &str) -> Result<()> {
    crate::naming::validate_output(output)?;
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
/// Refuse a recording to a fixed local file name that is taken, or whose
/// parent is not a folder, before an encoder starts. Streams, templates and
/// --on-exists number pick or need no free name here.
pub fn preflight_recording_output(output: &str, on_exists: crate::naming::OnExists) -> Result<()> {
    if !crate::media::is_recording(output)
        || !on_exists.is_fail()
        || crate::naming::has_placeholder(output)
    {
        return Ok(());
    }
    // A malformed output is reported by the forwarder with its own wording.
    if crate::naming::validate_output(output).is_err() {
        return Ok(());
    }
    let Ok(path) = crate::media::local_file_path(output) else {
        return Ok(());
    };
    crate::naming::check_free(&path)?;
    check_parents(&path)
}
/// A name that cannot mean what it says is a usage error: several frames
/// sent to one image name, or an image extension naming another format.
/// The CLI checks the name as typed; RPC clients keep the folder and format
/// rules they always had.
pub fn check_capture_name(output: &str, count: u32, format: &str) -> Result<()> {
    if output == "-" {
        return Ok(());
    }
    if let Some(message) = crate::naming::sequence_needs_template(output, count) {
        return Err(crate::types::Usage(message).into());
    }
    let directory = matches!(
        crate::naming::Plan::new(output, count, format),
        crate::naming::Plan::Directory { .. }
    );
    if !directory && let Some(message) = crate::naming::format_mismatch(output, format) {
        return Err(crate::types::Usage(message).into());
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
/// End every active job as `status`: cancelled by a command, interrupted
/// by shutdown.
fn cancel_jobs(jobs: &mut [Scheduled], status: &str) {
    for j in jobs.iter_mut().filter(|j| j.info.active()) {
        j.info.end(status, None);
        j.deadline = None;
    }
}
/// Finished jobs whose file lists are kept; older ones keep their counts
/// and last file, so every status stays small.
const LISTED_FINISHED_JOBS: usize = 8;
fn trim_jobs(jobs: &mut Vec<Scheduled>) {
    while jobs.len() > 132 {
        if let Some(i) = jobs.iter().position(|j| j.reported) {
            jobs.remove(i);
        } else {
            break;
        }
    }
    let finished = jobs.iter().filter(|j| !j.info.active()).count();
    for job in jobs
        .iter_mut()
        .filter(|j| !j.info.active())
        .take(finished.saturating_sub(LISTED_FINISHED_JOBS))
    {
        job.info.files = Vec::new();
    }
}
/// Publish the jobs: a progress line for each one still running, and one
/// `job_end` for each that ended, once its last shot is saved.
fn sync_jobs(h: &WorkerHandle, jobs: &mut [Scheduled]) {
    h.state.write().unwrap_or_else(|e| e.into_inner()).jobs =
        jobs.iter().map(|j| j.info.clone()).collect();
    for job in jobs.iter_mut().filter(|job| !job.reported) {
        // A job's files are reported by its `saved` lines alone.
        let summary = || {
            serde_json::to_value(crate::scheduling::CaptureJob {
                files: Vec::new(),
                ..job.info.clone()
            })
            .unwrap_or_default()
        };
        if job.info.active() || job.saving {
            h.events.progress(job.info.id, summary());
            continue;
        }
        let status = match job.info.status.as_str() {
            "complete" => JobStatus::Done,
            "cancelled" => JobStatus::Cancelled,
            "interrupted" => JobStatus::Interrupted,
            _ => JobStatus::Failed,
        };
        let error = job.info.error.as_ref().map(|message| {
            let mut error = json!({ "message": message });
            if let (Some(error), Some(Ok(Value::Object(tag)))) = (
                error.as_object_mut(),
                job.info.error_tag.as_ref().map(serde_json::to_value),
            ) {
                error.extend(tag);
            }
            error
        });
        h.events.emit(Output::JobEnd {
            job: job.info.id,
            status,
            error: error.filter(|_| status == JobStatus::Failed),
            summary: summary(),
        });
        job.reported = true;
    }
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
                job.next = Some(job.allocate(&h.names)?);
                c.request_frame()?;
                // Auto exposure warms up a camera the job started, unless it
                // sends frames only when triggered.
                Ok(stopped && !c.software_triggered())
            })();
            let warm_up = match result {
                Ok(warm_up) => warm_up,
                Err(e) => {
                    job.info.end("failed", Some(&e));
                    h.log(
                        "error",
                        format!("Capture job {} failed: {e:#}", job.info.id),
                    );
                    continue;
                }
            };
            job.info.status = "running".into();
            let timeout = Duration::from_millis(job.timeout_ms);
            job.deadline = Some(deadline_after(timeout));
            job.warm = (auto && warm_up)
                .then(|| Instant::now() + (timeout / 2).min(Duration::from_secs(3)));
        }
        if job.deadline.is_some_and(|d| Instant::now() >= d) {
            changed = true;
            let waited = Duration::from_millis(job.timeout_ms);
            let error = match camera.as_mut() {
                Some(c) => frame_wait_error(c, waited),
                None => crate::types::frame_timeout("frame timeout"),
            };
            job.deadline = None;
            job.info.end("failed", Some(&error));
            h.log("error", format!("Capture job {}: {error:#}", job.info.id));
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
/// Publish changed jobs.
fn jobs_changed(h: &WorkerHandle, camera: &mut Option<Camera>, jobs: &mut Vec<Scheduled>) {
    trim_jobs(jobs);
    sync_jobs(h, jobs);
    refresh(h, camera, false);
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
                job.info.end("failed", Some(&e));
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
        acquisition: Acquisition::default(),
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
fn finish(h: &WorkerHandle, job: &mut Scheduled, result: Result<saver::SavedShot>) {
    job.saving = false;
    let id = job.info.id;
    match result {
        Ok(shot) => {
            let info = &mut job.info;
            info.captured += 1;
            if info.files.len() < crate::scheduling::MAX_JOB_FILES {
                info.files.push(shot.path.clone());
            }
            info.last_file = Some(shot.path);
            info.frame = Some(shot.frame);
            if info.first_ms.is_none() {
                info.first_ms = Some(shot.wall_ms);
                info.settings = shot.settings;
            }
            if info.active() {
                info.error = None;
                info.error_tag = None;
                if info.captured >= info.count {
                    info.status = "complete".into();
                    info.ended_ms = Some(crate::scheduling::now_ms());
                    h.log(
                        "info",
                        format!("Capture job {id} complete: {} frame(s)", info.captured),
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
            job.info.end("failed", Some(&e));
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

/// `wait` from now, or as far ahead as this platform's clock can say.
fn deadline_after(wait: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(wait)
        .or_else(|| now.checked_add(Duration::from_secs(86_400 * 365)))
        .unwrap_or(now)
}
/// Why no frame came within `waited`: a trigger that never fired when the
/// camera waits for one from a line, with its configuration, else a plain
/// frame timeout. Read only once the wait has expired.
pub(crate) fn frame_wait_error(c: &mut Camera, waited: Duration) -> anyhow::Error {
    let wait =
        crate::scheduling::format_duration(u64::try_from(waited.as_millis()).unwrap_or(u64::MAX));
    match crate::controls::trigger_state(c) {
        Ok(trigger)
            if trigger.mode == "On"
                && trigger.source.as_deref().is_some_and(|source| {
                    !matches!(source, "Software" | "Freerun" | "FixedRate")
                }) =>
        {
            let source = trigger.source.clone().unwrap_or_default();
            crate::types::Categorized::new(
                crate::types::ErrorKind::TriggerTimeout,
                format!("no trigger on {source} within {wait}; the camera sends a frame only when triggered"),
            )
            .hint(format!(
                "send a trigger on {source}, wait longer with --timeout (none waits until stopped), or set trigger=off"
            ))
            .details(json!({"trigger": trigger}))
            .into()
        }
        _ => crate::types::frame_timeout(format!("no frame within {wait}")),
    }
}
/// What a burst that failed part way had already saved:
/// `saved 2 of 3 frames to DIR (a.png, b.png) before this failed`.
pub fn saved_before(files: &[String], count: u32) -> String {
    let paths = files.iter().map(Path::new).collect::<Vec<_>>();
    let folder = paths
        .first()
        .and_then(|first| first.parent())
        .map(|parent| parent.display().to_string())
        .unwrap_or_default();
    let name = |path: &&Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut names = paths
        .iter()
        .take(3)
        .map(name)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > 3 {
        names.push_str(&format!(" and {} more", paths.len() - 3));
    }
    format!(
        "saved {} of {count} frames to {folder} ({names}) before this failed",
        files.len()
    )
}
/// Frame `index`'s name under an output whose names are known in advance.
/// A folder's names depend on the camera and the time, so they never are:
/// its frames are in the folder.
fn output_path(output: &str, count: u32, index: u32, format: &str) -> PathBuf {
    match crate::naming::Plan::new(output, count, format) {
        crate::naming::Plan::Template(template) => {
            PathBuf::from(template.replace("{frame}", &format!("{:06}", index + 1)))
        }
        crate::naming::Plan::Directory { dir, .. } => dir,
        crate::naming::Plan::File(path) => path,
        crate::naming::Plan::Stdout => PathBuf::from("-"),
    }
}

/// The longest the camera thread waits while it receives no frames.
/// Commands, saved shots and shutdown wake it sooner.
const IDLE: Duration = Duration::from_secs(1);
/// How long the camera thread may wait for a command while it receives no
/// frames: until the next job is due, a requested frame times out or a timed
/// forward ends, even if its media source has already stopped.
fn idle_wait(h: &WorkerHandle, jobs: &[Scheduled]) -> Duration {
    let mut wait = IDLE;
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
/// End the jobs asked to stop: a capture job after the shot being saved, a
/// recording once its file is finished, continuous auto exposure holding
/// its values. A stop for a job not scheduled yet waits for it.
fn apply_stops(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &mut Vec<Scheduled>,
) {
    let stops = std::mem::take(&mut *h.stops.lock().unwrap_or_else(|e| e.into_inner()));
    if stops.is_empty() {
        return;
    }
    let forwarding = h
        .state
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .forwarding_job;
    let auto_job = *h.auto_job.lock().unwrap_or_else(|e| e.into_inner());
    let mut waiting = BTreeSet::new();
    let mut changed = false;
    for job in stops {
        if let Some(scheduled) = jobs.iter_mut().find(|j| j.info.id == job) {
            if scheduled.info.active() {
                scheduled.info.end("cancelled", None);
                scheduled.deadline = None;
                h.log("info", format!("Capture job {job} stopped"));
                changed = true;
            }
        } else if forwarding == Some(job) {
            let _ = end_forward(h, forwarder, auto, Ended::Stopped, None, |stopped| {
                stopped.as_ref().err().map(|e| {
                    (
                        "error",
                        format!("Recording stopped: {e:#}"),
                        crate::types::error_tag(e),
                    )
                })
            });
        } else if auto_job == Some(job) {
            end_auto_job(h, JobStatus::Cancelled);
            let _ = release_auto(h, auto, camera, false);
            h.log(
                "info",
                "Manual mode: continuous auto exposure stopped".into(),
            );
        } else {
            waiting.insert(job);
        }
    }
    if changed {
        jobs_changed(h, camera, jobs);
    }
    h.stops
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(waiting);
}
fn worker(h: WorkerHandle) {
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
    // The settings written into scheduled stills, read from the camera at
    // most once a second instead of once per shot: reading them costs a
    // register round trip per feature, which a fast schedule cannot spare.
    let mut settings = AcquisitionCache::default();
    let mut inspector = inspector::Inspector::default();
    let mut waiting: Vec<inspector::Waiting> = Vec::new();
    while !h.quitting.load(Ordering::Acquire) {
        h.beat();
        let receiving = camera.as_ref().is_some_and(Camera::is_streaming);
        let wait = if receiving {
            Duration::ZERO
        } else {
            idle_wait(&h, &jobs)
        };
        if let Some(e) = h.queue.take(wait) {
            h.beat();
            if let Some(id) = e.id {
                h.events.emit(Output::Started {
                    id,
                    budget_ms: e.command.budget_ms(),
                });
            }
            *h.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running {
                id: e.id,
                cancel: Arc::default(),
            });
            if let Some(request) = inspector::Request::of(&e.command) {
                inspector::begin(
                    &h,
                    &mut camera,
                    &mut auto,
                    e,
                    request,
                    &mut inspector,
                    &mut waiting,
                );
                *h.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
                h.beat();
                continue;
            }
            if matches!(e.command.session(), Some(SessionCommand::Disconnect)) {
                settle_saves(&h, &mut saver, &mut jobs);
            }
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
            *h.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
            if let Err(err) = &result {
                h.log("error", format!("{err:#}"));
            }
            (e.reply)(result);
            h.beat();
            // Any command can connect, reconnect or change a feature.
            settings.invalidate();
            rate_epoch = Instant::now();
            rate_count = 0;
            consecutive_errors = 0;
        }
        apply_stops(&h, &mut camera, &mut forwarder, &mut auto, &mut jobs);
        inspector::expire(&h, &mut camera, &mut waiting, None);
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
        sync_stream(&h, &camera);
        sync_forwarding_progress(&h, &forwarder, &mut progress_sampler, Instant::now());
        if let Some(c) = camera.as_mut().filter(|c| c.is_streaming()) {
            let asked = Instant::now();
            match c.next_frame(Duration::from_millis(100)) {
                Ok(frame) => {
                    h.count_frame(&frame);
                    if std::mem::take(&mut reconnecting) {
                        resumed(&h);
                        settings.invalidate();
                    }
                    if let Some(controller) = auto.as_mut() {
                        controller.observe(&frame, Instant::now());
                    }
                    let settled = auto.as_ref().is_none_or(AutoController::converged);
                    let room = saver.has_room();
                    if !settled {
                        // Auto exposure is still moving the camera.
                        settings.invalidate();
                    }
                    let shot =
                        shoot(&h, &frame, &mut jobs, settled, room).map(|shot| saver::Shot {
                            acquisition: settings.get(c, Instant::now()),
                            ..shot
                        });
                    // Inspections share the frame; the forwarder then takes
                    // the sensor buffer itself, copied only if an inspection
                    // still holds it, and the saver works on its own copy.
                    let forward = forwarder.is_some();
                    let frame =
                        inspector::serve(c, frame, forward, &mut waiting, &mut inspector, &h);
                    if let (Some((f, _)), Some(frame)) = (forwarder.as_mut(), frame)
                        && let Err(e) = f.push(frame)
                    {
                        let message = format!("Forwarding stopped: {e:#}");
                        let tag = crate::types::error_tag(&e);
                        // A recording that reached the size asked for ended
                        // as asked.
                        let ended = if message.contains("maximum file size") {
                            Ended::MaxSize
                        } else {
                            Ended::Failed
                        };
                        let _ = end_forward(&h, &mut forwarder, &mut auto, ended, Some(&e), |_| {
                            Some(("error", message, tag))
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
    // Shutting down: the shots with the saver are finished and reported,
    // every job ends as interrupted, and the recording is finalized.
    inspector::expire(
        &h,
        &mut camera,
        &mut waiting,
        Some(&crate::session::stopping),
    );
    settle_saves(&h, &mut saver, &mut jobs);
    drop(saver);
    cancel_jobs(&mut jobs, "interrupted");
    sync_jobs(&h, &mut jobs);
    let _ = stop_forward(&h, &mut forwarder, &mut auto, Ended::Interrupted);
    end_auto_job(&h, JobStatus::Interrupted);
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
    fn camera_auto_features_refresh_settings_every_frame() {
        let mut settings = Acquisition::default();
        settings
            .settings
            .insert("ExposureAuto".into(), json!("Off"));
        assert!(!settings.firmware_auto());
        settings
            .settings
            .insert("GainAuto".into(), json!("Continuous"));
        assert!(settings.firmware_auto());
    }
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
            &anyhow::Error::from(FrameWait).context("rtsp://camera.timeout/live")
        ));
        assert!(timed_out(&Categorized::error(
            ErrorKind::Timeout,
            "no answer"
        )));
        assert!(!timed_out(&Categorized::error(
            ErrorKind::Unavailable,
            "camera camera.timeout is offline"
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
            on_exists: Default::default(),
            context: Default::default(),
            utc_offset_s: None,
            quality: None,
            job: None,
            lease: false,
        };
        let frames = root.join("f-{frame}.raw").to_string_lossy().into_owned();
        handle.request(schedule(frames.clone(), 3, "raw")).unwrap();
        for (output, count, format) in [
            (frames.clone(), 2, "raw"),
            (
                root.join("f-000002.raw").to_string_lossy().into_owned(),
                1,
                "raw",
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
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms: None,
            utc_offset_s: None,
            job: None,
            lease: false,
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
            let _ = h.request(WorkerCommand::StopForward);
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
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms,
            utc_offset_s: None,
            job: None,
            lease: false,
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
            SessionCommand::connect("sim:0".into(), 100).into(),
        )
        .unwrap();
        let target = Path::new("udp://127.0.0.1:9");
        let reply = run(
            &mut camera,
            &mut forwarder,
            timed_forward(target, Some(2_000)).into(),
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
        run(
            &mut camera,
            &mut forwarder,
            timed_forward(target, None).into(),
        )
        .unwrap();
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
        run(&mut camera, &mut forwarder, WorkerCommand::StopForward).unwrap();
        h.shutdown();
    }

    #[test]
    fn timed_forwards_keep_their_end_time_until_stopped() {
        // The deadline falls due at its end time, never before.
        let state = |since, until| WorkerSnapshot {
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
        run(&mut camera, &mut forwarder, connect().into()).unwrap();
        let hour = Some(3_600_000);
        // Stop-forward, stop and disconnect each clear the end time.
        for (name, stop) in [
            ("stop-forward", WorkerCommand::StopForward),
            ("stop", SessionCommand::Stop.into()),
            ("disconnect", SessionCommand::Disconnect.into()),
        ] {
            let output = root.join(format!("{name}.mkv"));
            let reply = run(
                &mut camera,
                &mut forwarder,
                timed_forward(&output, hour).into(),
            )
            .unwrap();
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
                run(&mut camera, &mut forwarder, connect().into()).unwrap();
            }
        }
        let reply = run(
            &mut camera,
            &mut forwarder,
            timed_forward(&root.join("untimed.mkv"), None).into(),
        )
        .unwrap();
        assert!(reply.get("until_ms").is_none(), "{reply}");
        assert_eq!(h.snapshot().forwarding_until_ms, None);
        finish_timed_forward(&h, &mut forwarder, &mut None, u64::MAX);
        assert!(forwarder.is_some(), "a forward without a duration runs on");
        run(&mut camera, &mut forwarder, WorkerCommand::StopForward).unwrap();
        // At its end time the worker ends a timed forward as stop-forward
        // does, and says so in the step that clears it.
        let reply = run(
            &mut camera,
            &mut forwarder,
            timed_forward(&root.join("due.mkv"), Some(90_000)).into(),
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
            SessionCommand::connect("sim:0".into(), 100).into(),
        )
        .unwrap();
        let template = root.join("{camera}-{seq}.mkv");
        let reply = run(
            &mut camera,
            &mut forwarder,
            timed_forward(&template, None).into(),
        )
        .unwrap();
        let path = reply["path"].as_str().unwrap();
        assert!(!path.contains('{'), "{path}");
        let snapshot = h.snapshot();
        assert_eq!(snapshot.recording_path.as_deref(), Some(path));
        // `forwarding` still says what was asked for.
        assert_eq!(
            snapshot.forwarding.as_deref(),
            Some(template.to_string_lossy().as_ref())
        );
        run(&mut camera, &mut forwarder, WorkerCommand::StopForward).unwrap();
        assert_eq!(h.snapshot().recording_path, None);
        // A network forward names no file.
        let target = Path::new("udp://127.0.0.1:9");
        run(
            &mut camera,
            &mut forwarder,
            timed_forward(target, None).into(),
        )
        .unwrap();
        assert_eq!(h.snapshot().recording_path, None);
        run(&mut camera, &mut forwarder, WorkerCommand::StopForward).unwrap();
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
        let saved = h.take_saved();
        assert_eq!(saved.len(), 1);
        assert!(matches!(
            saved[0].item,
            crate::engine::saved::SavedItem::Recording { .. }
        ));
        assert_eq!(std::fs::metadata(&output).unwrap().len(), saved[0].bytes);
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
            h.request(WorkerCommand::StopForward).unwrap()["forwarding"],
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
        h.request(WorkerCommand::Set {
            feature: "Width".into(),
            value: "128".into(),
        })
        .unwrap();
        assert_eq!(
            h.request(WorkerCommand::Get {
                feature: "Width".into()
            })
            .unwrap()["value"],
            128
        );
        assert!(
            h.request(WorkerCommand::Set {
                feature: "Width".into(),
                value: "1".into()
            })
            .is_err()
        );
        let dir = temp_dir("simulator-acquisition");
        let v = h
            .request(capture_to(&dir.join("one.raw"), 1, Default::default()))
            .unwrap();
        assert_eq!(v["frame"]["width"], 128);
        assert!(!h.snapshot().streaming);
        assert_eq!(h.snapshot().frames, 1);
        let _ = std::fs::remove_dir_all(dir);
        h.request(SessionCommand::Disconnect).unwrap();
        assert!(h.snapshot().connected.is_none());
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
                on_exists: Default::default(),
                context: context.clone(),
                utc_offset_s: None,
                quality: None,
                stdout: None,
            })
            .unwrap();
        assert_eq!(v["context"]["line"], "L1");
        assert_eq!(v["frame"]["file"], v["files"][0]);
        assert_eq!(v["frame"]["pixel_format_name"], "Mono8");
        assert_eq!(
            v["frame"]["file_bytes"].as_u64().unwrap(),
            std::fs::metadata(v["files"][0].as_str().unwrap())
                .unwrap()
                .len()
        );
        let wall_ms = v["frame"]["wall_ms"].as_u64().unwrap();
        let batch = h.take_saved();
        assert_eq!(batch.len(), 1);
        let saved = &batch[0];
        assert_eq!(saved.wall_ms, wall_ms);
        assert_eq!(saved.context, context);
        assert_eq!(saved.bytes, 128 * 128);
        assert!(saved.sha256.as_ref().is_some_and(|s| s.len() == 64));
        assert!(std::path::Path::new(&saved.path).is_absolute());
        assert!(matches!(
            saved.item,
            crate::engine::saved::SavedItem::Capture { ref origin, job_id: None, .. } if origin == "capture"
        ));
        assert!(h.take_saved().is_empty());
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
            on_exists,
            context: Default::default(),
            utc_offset_s: None,
            quality: None,
            stdout: None,
        }
    }
    #[test]
    fn session_clients_keep_folder_and_format_rules_for_image_names() {
        use crate::naming::OnExists;
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let dir = temp_dir("guardrails");
        // The CLI refuses these names as typed (check_capture_name); the
        // worker keeps serving RPC clients as before: a sequence fills a
        // folder, and the format wins.
        let label = crate::naming::camera_label(&h.snapshot().connected.unwrap());
        let in_folder = |file: &Value, folder: &Path, seq: u32| {
            let file = Path::new(file.as_str().unwrap());
            let name = file.file_name().unwrap().to_str().unwrap();
            file.parent() == Some(folder)
                && name.starts_with(&format!("{label}-"))
                && name.ends_with(&format!("-{seq:04}.raw"))
        };
        let v = h
            .request(capture_to(&dir.join("shot.raw"), 3, OnExists::Fail))
            .unwrap();
        assert!(in_folder(&v["files"][2], &dir.join("shot.raw"), 3), "{v}");
        let mut mismatched = capture_to(&dir.join("y.jpg"), 1, OnExists::Fail);
        if let SessionCommand::Capture { format, .. } = &mut mismatched {
            *format = "png".into();
        }
        let v = h.request(mismatched).unwrap();
        assert_eq!(
            v["files"][0].as_str().unwrap(),
            dir.join("y.jpg").to_str().unwrap()
        );
        assert!(check_capture_name(dir.join("y.jpg").to_str().unwrap(), 1, "png").is_err());
        // A trailing separator names a folder; a bare name gets an extension.
        let folder = format!(
            "{}{}",
            dir.join("newdir").display(),
            std::path::MAIN_SEPARATOR
        );
        let v = h
            .request(capture_to(Path::new(&folder), 1, OnExists::Fail))
            .unwrap();
        assert!(in_folder(&v["files"][0], &dir.join("newdir"), 1), "{v}");
        let v = h
            .request(capture_to(&dir.join("noext"), 1, OnExists::Fail))
            .unwrap();
        assert_eq!(
            v["files"][0].as_str().unwrap(),
            dir.join("noext.raw").to_str().unwrap()
        );
        h.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn a_taken_recording_file_is_refused_before_forwarding_starts() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let dir = temp_dir("taken-recording");
        let path = dir.join("clip.mkv");
        std::fs::write(&path, b"keep").unwrap();
        let error = h
            .request(SessionCommand::Forward {
                output: path.to_string_lossy().into_owned(),
                codec: "h264".into(),
                encoder: "auto".into(),
                fps: 30.0,
                bitrate: "4M".into(),
                storage: Default::default(),
                max_file_bytes: 1 << 20,
                on_exists: Default::default(),
                context: Default::default(),
                duration_ms: None,
                utc_offset_s: None,
                job: None,
                lease: false,
            })
            .unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::storage::OutputExists>())
        );
        assert!(h.snapshot().forwarding.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        h.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn partial_bursts_name_what_was_saved() {
        let files = (1..=5)
            .map(|i| format!("out/frame-00000{i}.png"))
            .collect::<Vec<_>>();
        assert_eq!(
            saved_before(&files[..2], 3),
            "saved 2 of 3 frames to out (frame-000001.png, frame-000002.png) before this failed"
        );
        assert!(
            saved_before(&files, 9).ends_with("frame-000003.png and 2 more) before this failed")
        );
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
        // A directory names frames by camera and time, and continues its
        // numbering on every run.
        let frames = dir.join("frames");
        for run in 0..2 {
            let v = h.request(capture_to(&frames, 2, OnExists::Fail)).unwrap();
            for (i, name) in files(v).iter().enumerate() {
                let name = name
                    .strip_prefix(&format!("frames{}{label}-", std::path::MAIN_SEPARATOR))
                    .unwrap_or_else(|| panic!("{name}"));
                let (time, seq) = name.split_at("20261031-142233.123".len());
                assert!(
                    time.bytes()
                        .all(|b| b.is_ascii_digit() || b == b'-' || b == b'.'),
                    "{name}"
                );
                assert_eq!(seq, format!("-{:04}.raw", run * 2 + i + 1));
            }
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
                on_exists: Default::default(),
                context: crate::context::Context::from([("lot".into(), "7".into())]),
                utc_offset_s: None,
                quality: None,
                job: None,
                lease: false,
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
        let batch = h.take_saved();
        assert_eq!(batch.len(), 3);
        assert!(batch.iter().all(|f| f.context["lot"] == "7"
            && matches!(
                f.item,
                crate::engine::saved::SavedItem::Capture {
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
                count: 2,
                captured: 0,
                skipped: 0,
                retries: 0,
                first_at_ms: 0,
                interval_ms: 60_000,
                next_at_ms: 0,
                last_file: None,
                ..Default::default()
            },
            plan: crate::naming::Plan::new(&output, 2, "raw"),
            timeout_ms: 1000,
            format: "raw".into(),
            storage: Default::default(),
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            warm: None,
            camera: crate::naming::CameraName {
                label: "camera".into(),
                serial: "unknown".into(),
            },
            on_exists: Default::default(),
            next: None,
            saving: false,
            utc_offset_s: None,
            quality: None,
            reported: false,
        }
    }
    /// What the saver reports for a shot it wrote to `save.path`.
    fn written(save: &saver::Save) -> saver::SavedShot {
        saver::SavedShot {
            path: save.path.to_string_lossy().into_owned(),
            frame: crate::scheduling::JobFrame {
                width: 8,
                height: 8,
                pixel_format: crate::types::MONO8,
            },
            wall_ms: 0,
            settings: Default::default(),
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
        fn slow(_: &WorkerHandle, _: &saver::Shot, save: &saver::Save) -> Result<saver::SavedShot> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(written(save))
        }
        let h = WorkerHandle::new();
        let mut saver = saver::Saver::with(h.clone(), slow);
        let dir = temp_dir("slow-saves");
        let mut jobs = (1..=3).map(|id| armed(id, &dir)).collect::<Vec<_>>();
        for job in &mut jobs[1..] {
            job.deadline = None;
        }
        for id in 1..=3u64 {
            // Each frame finds one more job due.
            let job = &mut jobs[id as usize - 1];
            job.deadline
                .get_or_insert(Instant::now() + Duration::from_secs(10));
            let frame = small_frame(id);
            let started = Instant::now();
            let shot = shoot(&h, &frame, &mut jobs, true, saver.has_room());
            if let Some(shot) = shot {
                saver.offer(shot);
            }
            let took = started.elapsed();
            assert!(took < Duration::from_millis(5), "frame {id} took {took:?}");
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
        fn slow(_: &WorkerHandle, _: &saver::Shot, save: &saver::Save) -> Result<saver::SavedShot> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(written(save))
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
        finish(
            &h,
            &mut jobs[0],
            Ok(saver::SavedShot {
                path: "shot.raw".into(),
                frame: crate::scheduling::JobFrame {
                    width: 8,
                    height: 8,
                    pixel_format: crate::types::MONO8,
                },
                wall_ms: 0,
                settings: Default::default(),
            }),
        );
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
    fn captures_grabs_and_schedules_trigger_a_software_triggered_camera() {
        use crate::camera::tests::{
            Device, FRAME_TRIGGER_MODE, OPEN_ACQUISITION, TRIGGER_NODES, device_camera, events,
            taken,
        };
        let device =
            Device::new(&format!("{OPEN_ACQUISITION}{TRIGGER_NODES}")).with(FRAME_TRIGGER_MODE, 1);
        let log = events(&device);
        let mut camera = Some(device_camera(device));
        let h = WorkerHandle::new();
        let triggers = || {
            taken(&log)
                .iter()
                .filter(|event| *event == "TriggerSoftware FrameStart")
                .count()
        };
        let wait = Duration::from_secs(1);
        let (frame, _, _) =
            inspector::acquire(&h, &mut camera, &mut None, 1, Instant::now() + wait, wait).unwrap();
        assert_eq!(frame.width, 4);
        assert_eq!(triggers(), 1);
        let dir = temp_dir("software-trigger");
        let captured = run_command(
            &h,
            &mut camera,
            capture_to(&dir.join("shots"), 2, crate::naming::OnExists::Fail),
        )
        .unwrap();
        assert_eq!(captured["files"].as_array().unwrap().len(), 2);
        assert_eq!(triggers(), 2);
        // A shot fires its trigger when due; auto exposure has no frames to
        // warm up on, so the triggered frame is taken.
        let mut job = armed(1, &dir);
        job.deadline = None;
        let mut jobs = vec![job];
        let mut started = false;
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(started && jobs[0].deadline.is_some() && jobs[0].warm.is_none());
        assert_eq!(triggers(), 1);
        let frame = camera
            .as_mut()
            .unwrap()
            .next_frame(Duration::from_secs(1))
            .unwrap();
        assert!(shoot(&h, &frame, &mut jobs, false, true).is_some());
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
            let result = result.map(|path| saver::SavedShot {
                path,
                frame: crate::scheduling::JobFrame {
                    width: 8,
                    height: 8,
                    pixel_format: crate::types::MONO8,
                },
                wall_ms: 0,
                settings: Default::default(),
            });
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
        assert_eq!(job.input(crate::naming::clock(None)).index, 0);
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
        let h = WorkerHandle::new();
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
    fn capture_paths() {
        assert_eq!(output_path("frames", 2, 0, "png"), PathBuf::from("frames"));
        assert_eq!(
            output_path("shot-{frame}.png", 5, 3, "png"),
            PathBuf::from("shot-000004.png")
        );
        assert_eq!(
            output_path("newdir/", 1, 0, "png"),
            PathBuf::from("newdir/")
        );
        assert_eq!(
            output_path("noext", 1, 0, "jpeg"),
            PathBuf::from("noext.jpg")
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
            h.request(WorkerCommand::Set {
                feature: feature.into(),
                value: value.into(),
            })
            .unwrap();
        }
    }
    fn auto(h: &WorkerHandle, balance: Option<f64>) -> Result<Value> {
        h.request(SessionCommand::Auto {
            balance,
            continuous: false,
            job: None,
        })
    }
    fn set(h: &WorkerHandle, feature: &str, value: &str) {
        h.request(WorkerCommand::Set {
            feature: feature.into(),
            value: value.into(),
        })
        .unwrap();
    }
    /// Inspect the camera's next frame, measurements only.
    fn inspect(h: &WorkerHandle, fresh: u32) -> Result<Value> {
        h.request(SessionCommand::Frame {
            fresh,
            timeout_ms: 2000,
            max_dimension: 0,
            format: Default::default(),
            region: None,
            preview: None,
        })
    }
    #[test]
    fn an_inspection_acquires_a_frame_and_leaves_a_stopped_camera_stopped() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let logged = h.snapshot().logged;
        assert!(!h.snapshot().streaming);
        let inspected = inspect(&h, 1).unwrap();
        assert_eq!(inspected["frame"]["width"], 128);
        assert_eq!(inspected["frame"]["source"], "grab");
        assert!(inspected.get("settled").is_none(), "{inspected}");
        assert!(inspected.get("image").is_none(), "measurements only");
        let snapshot = h.snapshot();
        assert!(!snapshot.streaming);
        assert_eq!(snapshot.frames, 1);
        assert_eq!(snapshot.logged, logged, "an inspection logs nothing");
        assert!(h.take_saved().is_empty());
        // A stopped camera lets frames pass too: the third one is measured.
        assert_eq!(inspect(&h, 3).unwrap()["frame"]["waited_frames"], 3);
        // A streaming camera's next frames serve it, and it keeps streaming.
        h.request(SessionCommand::Start).unwrap();
        let live = inspect(&h, 2).unwrap();
        assert_eq!(live["frame"]["source"], "live");
        assert_eq!(live["frame"]["waited_frames"], 2);
        assert!(live["frame"]["age_ms"].as_u64().unwrap() < 1000);
        assert!(h.snapshot().streaming);
        h.shutdown();
    }
    #[test]
    fn an_inspection_settles_auto_exposure_on_a_stopped_camera_first() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        auto(&h, None).unwrap();
        let inspected = inspect(&h, 1).unwrap();
        // Frames metered while settling are counted too.
        assert!(h.snapshot().frames >= 1);
        assert!(!h.snapshot().streaming);
        if inspected.get("settled").is_some() {
            assert_eq!(inspected["settled"], false);
        } else {
            assert!(h.snapshot().auto.unwrap().state != "converging");
        }
        h.shutdown();
    }
    #[test]
    fn a_cancelled_or_late_inspection_of_a_stream_ends_without_a_frame() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        // A hardware trigger that never fires: the stream sends nothing.
        set(&h, "TriggerMode", "On");
        set(&h, "TriggerSource", "Line0");
        h.request(SessionCommand::Start).unwrap();
        let late = inspect(&h, 1).unwrap_err();
        assert_eq!(crate::cli::error_code(&late).0, "trigger_timeout");
        let (reply, answer) = mpsc::channel();
        h.queue_request(
            Some(7),
            SessionCommand::Frame {
                fresh: 1,
                timeout_ms: 10_000,
                max_dimension: 0,
                format: Default::default(),
                region: None,
                preview: None,
            }
            .into(),
            Box::new(move |result| {
                let _ = reply.send(result);
            }),
        )
        .map_err(|(error, _)| error)
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let asked = Instant::now();
        h.cancel(7);
        let error = answer
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(asked.elapsed() < Duration::from_secs(2));
        assert_eq!(crate::cli::error_code(&error).0, "interrupted");
        h.shutdown();
    }
    #[test]
    fn manual_mode_ends_continuous_auto_exposure() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        h.request(SessionCommand::Auto {
            balance: None,
            continuous: true,
            job: Some(7),
        })
        .unwrap();
        let ended = |h: &WorkerHandle| {
            h.take_events()
                .into_iter()
                .filter_map(|line| match line {
                    Output::JobEnd { job, status, .. } => Some((job, status)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(ended(&h).is_empty());
        h.request(SessionCommand::Manual { revert: false }).unwrap();
        assert_eq!(ended(&h), [(7, JobStatus::Done)]);
        // Nothing is left to end at shutdown.
        h.shutdown();
        assert!(ended(&h).is_empty());
    }
    #[test]
    fn a_shutdown_ends_a_frame_wait_as_the_session_stopping() {
        let h = WorkerHandle::new();
        connect(&h, "1000");
        // A hardware trigger that never fires, on a stopped camera.
        set(&h, "TriggerMode", "On");
        set(&h, "TriggerSource", "Line0");
        let reply = h
            .submit(SessionCommand::Frame {
                fresh: 1,
                timeout_ms: 10_000,
                max_dimension: 0,
                format: Default::default(),
                region: None,
                preview: None,
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let asked = Instant::now();
        let stopper = {
            let h = h.clone();
            std::thread::spawn(move || h.shutdown())
        };
        let error = reply
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(asked.elapsed() < Duration::from_secs(3));
        assert!(crate::session::is_stopping(&error), "{error:#}");
        assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
        stopper.join().unwrap();
    }
    #[test]
    fn failed_inspections_leave_the_log_and_last_error_alone() {
        let h = WorkerHandle::new();
        let errors = |h: &WorkerHandle| {
            h.take_events()
                .into_iter()
                .filter_map(|line| match line {
                    Output::Log { level, message } if level == "error" => Some(message),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        errors(&h);
        // Without a camera, an inspection fails like a missed frame would.
        assert!(inspect(&h, 1).is_err());
        assert!(errors(&h).is_empty());
        assert!(h.snapshot().last_error.is_none());
        // Other failures are still the worker's to log, each time.
        let get = || {
            h.request(SessionCommand::GetValues {
                names: vec![crate::controls::Name::parse("Width").unwrap()],
            })
        };
        assert!(get().is_err());
        let logged = errors(&h);
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].contains("no camera connected"), "{logged:?}");
        assert!(
            h.snapshot()
                .last_error
                .unwrap()
                .contains("no camera connected")
        );
        assert!(get().is_err());
        assert_eq!(errors(&h).len(), 1);
        h.shutdown();
    }
    #[test]
    fn saved_stills_record_settings_and_tags_but_no_serial_or_address() {
        let mut c = Camera::open("sim:0", &[], Duration::from_secs(5)).unwrap();
        c.set("ExposureTime", "2500").unwrap();
        let acquired = acquisition(&mut c);
        assert_eq!(acquired.settings["ExposureTime"], json!(2500.0));
        assert!(acquired.settings.contains_key("Gain"));
        let frame = Frame {
            id: 7,
            width: 2,
            height: 1,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 9,
            data: vec![0, 0],
        };
        let context = crate::context::Context::from([("part".into(), "B-7".into())]);
        let meta = still_meta(&frame, 11, &acquired, &context);
        assert_eq!(
            (&meta["frame_id"], &meta["wall_ms"], &meta["pixel_format"]),
            (&json!(7), &json!(11), &json!("Mono8"))
        );
        assert_eq!(meta["context"]["part"], "B-7");
        assert_eq!(meta["camera"]["model"], json!(c.info.model));
        assert_eq!(
            meta["camera"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["model", "vendor"]
        );
        for key in ["serial", "address", "source", "id"] {
            assert!(meta.get(key).is_none(), "{key}");
        }
        // Tagless stills leave the context out.
        let plain = still_meta(&frame, 11, &Acquisition::default(), &Default::default());
        assert!(plain.get("context").is_none() && plain.get("camera").is_none());
    }
    #[test]
    fn exposure_measurements_follow_exposure_time_on_the_simulator() {
        let h = WorkerHandle::new();
        connect(&h, "2500");
        let mut means = Vec::new();
        for exposure in ["2500", "10000", "40000"] {
            set(&h, "ExposureTime", exposure);
            let exposure = inspect(&h, 1).unwrap()["exposure"].clone();
            means.push((
                exposure["mean"].as_f64().unwrap(),
                exposure["highlights"].as_f64().unwrap(),
            ));
        }
        assert!(means[0].0 < means[1].0 && means[1].0 < means[2].0);
        assert!((0.3..0.6).contains(&means[1].0), "{:?}", means[1]);
        assert!(means[2].1 > 0.3, "{:?}", means[2]);
        assert!(means[0].0 < 0.25, "{:?}", means[0]);
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
            inspect(&h, 1).unwrap()["focus"]["tenengrad"]
                .as_f64()
                .unwrap()
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
            h.request(WorkerCommand::Set {
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
        // Turning on the camera's own auto, by control or by feature, ends
        // auto mode too.
        for value in ["auto_exposure=once", "ExposureAuto=Once"] {
            set("ExposureAuto", "Off");
            auto(&h, Some(0.5)).unwrap();
            let values = vec![crate::controls::Assignment::parse(value).unwrap()];
            h.request(SessionCommand::SetValues {
                values,
                dry_run: false,
            })
            .unwrap();
            assert!(h.snapshot().auto.is_none(), "{value}");
        }
        h.shutdown();
    }

    #[test]
    fn capture_warms_up_auto_exposure_and_reports_auto_status() {
        let h = WorkerHandle::new();
        connect(&h, "2500");
        auto(&h, Some(0.5)).unwrap();
        let dir = temp_dir("auto-capture");
        let v = h
            .request(SessionCommand::Capture {
                output: dir.join("warm.raw").to_string_lossy().into_owned(),
                count: 1,
                timeout_ms: 6000,
                format: "raw".into(),
                storage: Default::default(),
                on_exists: Default::default(),
                context: Default::default(),
                utc_offset_s: None,
                quality: None,
                stdout: None,
            })
            .unwrap();
        assert_eq!(v["frame"]["width"], 128);
        assert_eq!(v["auto"]["strategy"], "software");
        assert!(v["auto"]["brightness"].as_f64().unwrap() > 0.2);
        assert!(!h.snapshot().streaming);
        h.shutdown();
        let _ = std::fs::remove_dir_all(dir);
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

    fn run_command(
        h: &WorkerHandle,
        camera: &mut Option<Camera>,
        command: impl Into<WorkerCommand>,
    ) -> Result<Value> {
        process(
            h,
            camera,
            &mut None,
            &mut None,
            &mut Vec::new(),
            &mut 1,
            &mut false,
            command.into(),
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
            let wait = Duration::from_millis(10);
            let error =
                inspector::acquire(&h, &mut camera, &mut None, 1, Instant::now() + wait, wait)
                    .unwrap_err();
            assert_eq!(crate::cli::error_code(&error).0, "frame_timeout");
            assert_eq!(reads.load(Ordering::Relaxed), expected_reads);
            assert_eq!(h.snapshot().frames, 0, "a late frame is never measured");
            assert!(!camera.as_ref().unwrap().is_streaming());
            h.shutdown();
        }
    }
    #[test]
    fn frame_requests_are_the_camera_loops_alone() {
        let h = WorkerHandle::new();
        let mut camera = Some(Camera::open("sim:0", &[], Duration::from_secs(1)).unwrap());
        let frame = SessionCommand::Frame {
            fresh: 1,
            timeout_ms: 1000,
            max_dimension: 0,
            format: Default::default(),
            region: None,
            preview: None,
        };
        let error = run_command(&h, &mut camera, frame).unwrap_err();
        assert_eq!(
            error.to_string(),
            "frame requests are taken up by the camera thread's loop"
        );
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
        run(
            &mut camera,
            SessionCommand::Auto {
                balance: Some(1.0),
                continuous: false,
                job: None,
            }
            .into(),
        )
        .unwrap();
        assert!((read(0x110) - (1e6 / 120.0 - 1e6 / 120.0 * 0.02)).abs() < 1e-6);
        assert!(read(0x118) > 7.0);
        assert_eq!(read(0x120), 120.0);
        run(&mut camera, SessionCommand::Disconnect.into()).unwrap();
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
        // A connect that refuses saved sign-ins never reads the store.
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
