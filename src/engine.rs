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
use crate::session::LogEntry;
use crate::session::{SessionCommand, WorkerSnapshot};
mod emulated;
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
    /// Whether settings it emulates for its camera are kept for the next
    /// worker (see `emulated`): in worker processes, not in unit tests,
    /// which share one runtime folder.
    keeps_settings: bool,
    /// The camera's counters when the recording in progress started.
    forward_baseline: Arc<Mutex<Option<Baseline>>>,
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
        let mut handle = Self::unstarted(output, crate::logins::Logins::system(), events, &session);
        handle.keeps_settings = true;
        handle.spawn()
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
        Self::unstarted(output, logins, events, session).spawn()
    }
    /// Start the camera thread.
    fn spawn(self) -> Self {
        let handle = self;
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
            keeps_settings: false,
            forward_baseline: Arc::default(),
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
    /// Write one feature as `set` does, for tests.
    #[cfg(test)]
    pub(crate) fn set_value(&self, feature: &str, value: &str) -> Result<Value> {
        self.request(SessionCommand::SetValues {
            values: vec![crate::controls::Assignment::parse(&format!(
                "{feature}={value}"
            ))?],
            dry_run: false,
        })
    }
    /// Read one feature as `get` does, for tests.
    #[cfg(test)]
    pub(crate) fn value(&self, feature: &str) -> Value {
        self.request(SessionCommand::GetValues {
            names: vec![crate::controls::Name::parse(feature).unwrap()],
        })
        .unwrap()["values"][feature]
            .clone()
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
/// Publish what the camera is: connected, streaming, its transport counts.
/// Never a walk of every feature, which on GigE is hundreds of round trips:
/// commands read what they need.
fn refresh(h: &WorkerHandle, camera: &Option<Camera>) {
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    s.connected = camera.as_ref().map(|c| c.info.clone());
    s.streaming = camera.as_ref().is_some_and(Camera::is_streaming);
    s.transport = camera.as_ref().and_then(Camera::stats);
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
    h.state.write().unwrap_or_else(|e| e.into_inner()).auto = Some(controller.status().clone());
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
/// Why a capture command takes one frame.
pub(crate) const ONE_FRAME: &str =
    "capture takes one frame; schedule takes more, saved by the encoders as they come";
/// How often a frame of a software trigger is asked for again when the
/// transport lost it or no frame came, before the wait for it fails.
const TRIGGER_RETRIES: u32 = 5;
/// How long a software-triggered frame is waited for before the transport's
/// counters are read again for its loss.
const LOSS_CHECK: Duration = Duration::from_millis(50);
/// Frames the transport lost on the way or received incomplete so far.
fn losses(c: &Camera) -> u64 {
    c.stats().map_or(0, |stats| {
        stats.lost_frames.saturating_add(stats.incomplete_frames)
    })
}
/// Ask for a frame and wait up to `wait` for it. The frame of a software
/// trigger the transport lost or received incomplete never comes: the
/// trigger is fired again as soon as the transport counts the loss, and
/// once when the wait runs out, counting the frames given up on in
/// `unanswered`.
fn requested_frame(
    h: &WorkerHandle,
    c: &mut Camera,
    auto: &mut Option<AutoController>,
    wait: Duration,
    unanswered: &mut u64,
) -> Result<Option<Frame>> {
    c.request_frame()?;
    if !c.software_triggered() {
        return next_frame(h, c, auto, deadline_after(wait));
    }
    let mut lost = losses(c);
    let mut deadline = deadline_after(wait);
    let (mut retries, mut waited_out) = (0, false);
    loop {
        let check = deadline.min(Instant::now() + LOSS_CHECK);
        if let Some(frame) = next_frame(h, c, auto, check)? {
            return Ok(Some(frame));
        }
        let now_lost = losses(c);
        let expired = Instant::now() >= deadline;
        let again = (now_lost > lost || expired)
            && retries < TRIGGER_RETRIES
            && !(expired && now_lost == lost && waited_out);
        if !again {
            if expired {
                return Ok(None);
            }
            continue;
        }
        retries += 1;
        if now_lost == lost {
            // Nothing says the frame was lost, nor that it ever came.
            waited_out = true;
            *unanswered += 1;
        }
        lost = now_lost;
        c.trigger_unanswered();
        c.request_frame()?;
        deadline = deadline_after(wait);
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

/// Note a line the worker logged: an error becomes its `last_error`, and
/// tests keep the line.
fn push_log(s: &mut WorkerSnapshot, level: &str, message: String) {
    if level == "error" {
        s.last_error = Some(message.clone());
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

/// The delivery counters a recording or forward's job reports as progress.
#[derive(Debug, Clone, Copy)]
struct ForwardingProgress {
    accepted: u64,
    written: u64,
    dropped: u64,
    late: u64,
    stalled: bool,
}
impl ForwardingProgress {
    fn of(stats: &crate::media::ForwardStats) -> Self {
        Self {
            accepted: stats.accepted,
            written: stats.written,
            dropped: stats.dropped,
            late: stats.late,
            stalled: stats.stalled,
        }
    }
    /// The job's progress event: these counters, and the file a local
    /// recording writes.
    fn event(&self, path: Option<String>) -> Value {
        let mut value = json!({
            "accepted": self.accepted,
            "written": self.written,
            "dropped": self.dropped,
            "late": self.late,
            "stalled": self.stalled,
        });
        if let Some(path) = path {
            value["path"] = json!(path);
        }
        value
    }
}
#[derive(Default)]
struct ForwardProgressSampler {
    id: Option<String>,
    at: Option<Instant>,
    /// The last progress told had a frame written.
    written: bool,
}
impl ForwardProgressSampler {
    /// Progress to tell, four times a second, and at once when the first
    /// frame is written: a recording leaves armed as its first frame is
    /// encoded, which `record --background` waits for.
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
        let due = self.id.as_deref() != Some(id)
            || self
                .at
                .is_none_or(|at| now.saturating_duration_since(at) >= FORWARD_PROGRESS_INTERVAL);
        if !due && self.written {
            return None;
        }
        let progress = ForwardingProgress::of(&stats());
        if !due && progress.written == 0 {
            return None;
        }
        self.id = Some(id.into());
        self.at = Some(now);
        self.written = progress.written > 0;
        Some(progress)
    }
}
/// Tell the job of the running recording or forward how it is going, as
/// often as `sampler` allows.
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
    if let (Some(progress), Some(job)) = (progress, job) {
        h.events.progress(job, progress.event(path));
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

/// End the forward that failed with `error`. A recording that reached its
/// maximum size ended as asked.
fn forward_failed(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    error: anyhow::Error,
) {
    if error.is::<crate::storage::MaxSize>() {
        let _ = end_forward(h, forwarder, auto, Ended::MaxSize, None, |_| {
            Some(("info", format!("Recording ended: {error:#}")))
        });
        return;
    }
    let message = format!("Forwarding stopped: {error:#}");
    let _ = end_forward(h, forwarder, auto, Ended::Failed, Some(&error), |_| {
        Some(("error", message))
    });
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
/// How a forward ended, for its log line: a level and a message.
type ForwardEnd = (&'static str, String);
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
    let (job, since_ms, until_ms) = {
        let state = h.state.read().unwrap_or_else(|e| e.into_inner());
        (
            state.forwarding_job,
            state.forwarding_since_ms,
            state.forwarding_until_ms,
        )
    };
    let mut summary = json!({"ended": ended.name()});
    let mut local = false;
    let stopped = match forwarder.take() {
        Some((mut f, _)) => {
            local = f.recording_path().is_some();
            let stopped = f.stop();
            let stats = f.stats();
            let baseline = h
                .forward_baseline
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let (_, lost, incomplete, resent) =
                baseline.map_or((0, None, None, None), |baseline| baseline.since(h));
            summary["written"] = json!(stats.written);
            // Frames the encoder could not take, and those the transport
            // lost on the way.
            summary["dropped"] = json!(stats.dropped.saturating_add(lost.unwrap_or(0)));
            if let Some(incomplete) = incomplete {
                summary["incomplete"] = json!(incomplete);
            }
            if let Some(resent) = resent {
                summary["resent"] = json!(resent);
            }
            summary["accepted"] = json!(stats.accepted);
            summary["encoder"] = json!(stats.encoder);
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
                        sha256: file.sha256,
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
    // recording, say) finds its file finished, saved and its job ended.
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    if let Some((level, message)) = report(&stopped) {
        drop(s);
        h.events.log(level, &message);
        s = h.state.write().unwrap_or_else(|e| e.into_inner());
        push_log(&mut s, level, message);
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
            Ok(_) => ("info", format!("{what} finished after {length}")),
            Err(e) => ("error", format!("{what} stopped after {length}: {e:#}")),
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
) -> Result<(PathBuf, crate::storage::Recording)> {
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
/// values of [`ACQUISITION_FEATURES`]. Read once per capture command, and for
/// scheduled shots at most once a second, with what auto exposure wrote since.
#[derive(Debug, Clone, Default)]
pub(super) struct Acquisition {
    vendor: String,
    model: String,
    pub(super) settings: serde_json::Map<String, Value>,
}

impl Acquisition {
    /// Take the values auto exposure wrote since this was read.
    fn overlay(&mut self, values: &BTreeMap<String, Value>) {
        for (name, value) in values {
            if ACQUISITION_FEATURES.contains(&name.as_str()) {
                self.settings.insert(name.clone(), value.clone());
            }
        }
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
        "wall_ms": wall_ms,
        "width": frame.width,
        "height": frame.height,
        "pixel_format": crate::frame::pixel_format_name(frame.pixel_format),
    });
    if let Some(bits) = crate::frame::sample_bits(frame.pixel_format) {
        meta["bits"] = json!(bits);
    }
    if let Some(device_ts_ns) = frame.device_ts_ns {
        meta["device_ts_ns"] = json!(device_ts_ns);
    }
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

/// Save `NAME.json` beside the raw file `path`: what reading its bytes back
/// needs, as they carry nothing but the samples. When it cannot be saved,
/// the raw file goes too: nothing reported it yet.
fn save_sidecar(
    path: &Path,
    frame: &Frame,
    wall_ms: u64,
    acquisition: &Acquisition,
    sync: crate::storage::SyncMode,
) -> Result<()> {
    let sidecar = crate::naming::sidecar(path);
    let document = crate::types::without_nulls(json!({
        "pixel_format": crate::inspect::pixel_format_label(frame.pixel_format),
        "pfnc_code": frame.pixel_format,
        "width": frame.width,
        "height": frame.height,
        "row_bytes": crate::frame::row_bytes(frame.width, frame.pixel_format),
        "bytes": frame.data.len(),
        "frame_id": (frame.id > 0).then_some(frame.id),
        "device_ts_ns": frame.device_ts_ns,
        "time": crate::scheduling::rfc3339(wall_ms),
        "settings": crate::engine::saved::settings(&Value::Object(acquisition.settings.clone())),
    }));
    let saved = serde_json::to_vec(&document)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| crate::storage::save(&bytes, &sidecar, sync, false));
    if let Err(error) = saved {
        let _ = std::fs::remove_file(path);
        return Err(error).with_context(|| format!("save {}", sidecar.display()));
    }
    Ok(())
}
/// A published image, as its `saved` line reports it.
fn image_file(
    path: &std::path::Path,
    stored: &crate::storage::Stored,
    frame: &Frame,
    format: &str,
    wall_ms: u64,
    job_id: Option<u64>,
    context: &crate::context::Context,
) -> crate::engine::saved::SavedFile {
    crate::engine::saved::SavedFile {
        wall_ms,
        path: absolute_path(path),
        bytes: stored.bytes,
        sha256: stored.sha256.clone(),
        context: context.clone(),
        item: crate::engine::saved::SavedItem::Capture {
            frame_id: frame.id,
            device_ts_ns: frame.device_ts_ns,
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
    }
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
    refresh(h, camera);
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
    refresh(h, camera);
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
    scheduled_stream: &mut JobStream,
    command: WorkerCommand,
) -> Result<Value> {
    let command = match command {
        WorkerCommand::OptimizeNetwork { bytes_per_second } => {
            return optimize_network(h, camera, forwarder, auto, jobs, bytes_per_second);
        }
        WorkerCommand::Session(command) => command,
    };
    match command {
        SessionCommand::Frame { .. } => {
            anyhow::bail!("frame requests are taken up by the camera thread's loop")
        }
        SessionCommand::Status => {
            refresh(h, camera);
            Ok(serde_json::to_value(h.snapshot())?)
        }
        SessionCommand::ReadSetup
        | SessionCommand::SetupDiff { .. }
        | SessionCommand::ApplySetup { .. } => {
            anyhow::bail!("saved setups are read and restored before plain commands")
        }
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
            max_file_bytes,
            on_exists,
            context,
            duration_ms,
            utc_offset_s,
            warmup_ms,
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
            let c = camera_mut(camera)?;
            let was_streaming = c.is_streaming();
            c.start()?;
            // A camera this started streams only while something needs its
            // frames: once the recording ends, the jobs' stream lets it go,
            // so a warm session does not go on acquiring.
            if !was_streaming {
                *scheduled_stream = JobStream {
                    started: true,
                    ..JobStream::default()
                };
            }
            // A camera that sets its own exposure records once it settled.
            let warmup = if was_streaming {
                None
            } else {
                warm_up(h, c, auto, warmup_ms)?
            };
            *forwarder = Some((f, fps));
            *h.forward_baseline.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Baseline::with(h, c.stats()));
            if let Some(a) = auto.as_mut() {
                a.set_forward_fps(Some(fps))?;
            }
            refresh(h, camera);
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
                s.forwarding_since_ms = Some(since_ms);
                s.forwarding_until_ms = until_ms;
                // `forwarding` keeps the requested output; this is the file it named.
                s.recording_path = f_path.clone();
            }
            let mut value =
                json!({"forwarding":shown,"forwarding_id":forwarding_id,"since_ms":since_ms});
            if let Some(warmup) = warmup {
                value["warmup_ms"] = json!(warmup.ms);
                value["settled"] = json!(warmup.settled);
            }
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
            // A selector that says what it opens never waits for discovery;
            // a serial, name or model is looked up.
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
                let (devices, warnings) = crate::camera::lookup(&selector, duration);
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
                                h.logins
                                    .index_path()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_default()
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
                    crate::logins::Remembered::Missing { user } => h.log(
                        "warn",
                        format!(
                            "the saved sign-in of {user} for {} is missing from the OS credential store",
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
            // The settings an earlier worker emulated for this camera, as a
            // real camera keeps its own.
            if h.keeps_settings
                && let Some(c) = camera.as_mut()
                && let Some(saved) = emulated::load(&c.info.id)
                && let Err(error) = c.restore_settings(&saved)
            {
                h.log(
                    "warn",
                    format!(
                        "{} opened with its default settings, not those set before: {error:#}",
                        crate::media::redact_url(&c.info.id)
                    ),
                );
            }
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
            }
            // A capture soon after takes the frame it opened with.
            scheduled_stream.rest_at = Some(Instant::now() + MEDIA_LINGER);
            refresh(h, camera);
            h.log(
                "info",
                format!("Connected to {}", crate::media::redact_url(&selector)),
            );
            Ok(json!({"connected":h.snapshot().connected}))
        }
        SessionCommand::Disconnect => {
            cancel_jobs(jobs, "cancelled");
            sync_jobs(h, jobs);
            *scheduled_stream = JobStream::default();
            let auto_result = release_auto(h, auto, camera, true);
            let forward_result = stop_forward(h, forwarder, auto, Ended::Stopped);
            let camera_result = if let Some(c) = camera.as_mut() {
                c.stop()
            } else {
                Ok(())
            };
            *camera = None;
            h.lock.lock().unwrap_or_else(|e| e.into_inner()).take();
            refresh(h, camera);
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
            refresh(h, camera);
            let result = result?;
            if let Some(changed) = &result.changed
                && !changed.is_empty()
            {
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
            refresh(h, camera);
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
            on_exists,
            manifest,
            sync,
            context,
            utc_offset_s,
            quality,
            fresh,
            warmup_ms,
            inspect,
            job,
            ..
        } => {
            validate_capture(&output, count, timeout_ms, &format)?;
            preflight_capture_output(&output, count, &format, on_exists)?;
            validate_schedule_outputs(&output, count, &format, on_exists, jobs)?;
            ensure!(
                output != "-",
                "scheduled capture requires a file or directory"
            );
            ensure!(
                jobs.iter().filter(|j| j.info.active()).count() < 32,
                "capture schedule queue is full (32 active jobs)"
            );
            crate::scheduling::validate_schedule(first_at_ms, interval_ms, count)?;
            let manifest = manifest
                .map(|path| crate::engine::saved::Manifest::open(Path::new(&path)).map(Arc::new))
                .transpose()?;
            let info = &camera_mut(camera)?.info;
            let label = crate::naming::camera_name(info);
            let source = Source {
                id: info.id.as_str().into(),
                kind: info.profile.kind,
            };
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
                after_change: false,
                warmup_ms: None,
                settled: None,
                ..Default::default()
            };
            jobs.push(Scheduled {
                info: info.clone(),
                plan: crate::naming::Plan::new(&output, count, &format),
                timeout_ms,
                wait: Duration::ZERO,
                fresh,
                warmup_ms,
                inspect,
                format,
                sync,
                manifest,
                source,
                folders: Default::default(),
                deadline: None,
                warm: None,
                camera: label,
                on_exists,
                next: None,
                shots: 0,
                free: Default::default(),
                stop_by: None,
                trigger: None,
                unanswered: 0,
                saving: 0,
                baseline: None,
                utc_offset_s,
                quality,
                reported: false,
            });
            trim_jobs(jobs);
            sync_jobs(h, jobs);
            h.log("info", format!("Capture job {} scheduled", info.id));
            Ok(json!({"job":info}))
        }
        SessionCommand::Auto {
            balance,
            job,
            region,
            max_exposure_us,
            ..
        } => {
            let entered = enter_auto(h, camera, auto, forwarder, jobs, balance, job)?;
            if let (Some(controller), Some(c)) = (auto.as_mut(), camera.as_mut()) {
                controller.set_limits(c, region, max_exposure_us)?;
            }
            Ok(entered)
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
            refresh(h, camera);
            released?;
            h.log("info", "Manual mode".into());
            Ok(json!({"auto":null}))
        }
        SessionCommand::Start => {
            *scheduled_stream = JobStream::default();
            camera_mut(camera)?.start()?;
            refresh(h, camera);
            h.log("info", "Acquisition started".into());
            Ok(json!({"streaming":true}))
        }
        SessionCommand::Arm {
            warmup_ms,
            bytes_per_second,
        } => {
            // Paced to its share of a link other cameras stream on too; a
            // camera that cannot be paced still streams, as it would alone.
            let paced = bytes_per_second
                .map(|budget| optimize_network(h, camera, forwarder, auto, jobs, budget));
            let c = camera_mut(camera)?;
            let was_streaming = c.is_streaming();
            c.start()?;
            // The jobs to come own a stream this started: it streams until
            // the first of them takes its first frame, then only while one
            // needs it.
            if !was_streaming || scheduled_stream.started {
                scheduled_stream.started = true;
                scheduled_stream.armed_until = Some(deadline_after(ARM_HOLD));
            }
            let warmup = if was_streaming {
                None
            } else {
                warm_up(h, c, auto, warmup_ms)?
            };
            refresh(h, camera);
            let mut value = json!({"streaming": true, "started": !was_streaming});
            if let Some(warmup) = warmup {
                value["warmup_ms"] = json!(warmup.ms);
                value["settled"] = json!(warmup.settled);
            }
            match paced {
                Some(Ok(tuning)) => value["network"] = tuning["tuning"].clone(),
                Some(Err(error)) => value["network_error"] = json!(format!("{error:#}")),
                None => {}
            }
            Ok(value)
        }
        SessionCommand::Stop => {
            cancel_jobs(jobs, "cancelled");
            sync_jobs(h, jobs);
            *scheduled_stream = JobStream::default();
            let forward_result = stop_forward(h, forwarder, auto, Ended::Stopped);
            let camera_result = camera_mut(camera)?.stop();
            refresh(h, camera);
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
            on_exists,
            sync,
            context,
            utc_offset_s,
            quality,
            fresh,
            warmup_ms,
            stdout,
        } => {
            // One frame, encoded here: a burst is a job of the acquisition
            // loop, whose encoders never hold up the camera.
            validate_capture(&output, count, timeout_ms, &format)?;
            ensure!(count == 1, "{ONE_FRAME}");
            if output != "-" {
                preflight_capture_output(&output, 1, &format, on_exists)?;
            }
            ensure!(!output.is_empty(), "provide an output path");
            let c = camera_mut(camera)?;
            let duration = frame_wait(c, timeout_ms);
            let was_streaming = c.is_streaming();
            // The camera's own counters, which only the acquisition loop
            // publishes.
            let baseline = Baseline::with(h, c.stats());
            c.start()?;
            let mut folders = crate::storage::Folders::default();
            let mut unanswered = 0;
            let mut result = (|| -> Result<Value> {
                let mut after_change = false;
                let warmup = if was_streaming {
                    None
                } else {
                    warm_up(h, c, auto, warmup_ms)?
                };
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
                let plan = crate::naming::Plan::new(&output, 1, &format);
                // Pick the name before requesting the frame: a feeder
                // consumes paper irreversibly, so a taken name in fail mode
                // is refused first.
                let input = crate::naming::NameInput {
                    camera: &label,
                    now: crate::naming::clock(utc_offset_s),
                    index: 0,
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
                // Frames exposed before the last settings change, and the
                // first `fresh` after it, are let pass.
                let mut skip = fresh;
                let frame = loop {
                    let frame = match requested_frame(h, c, auto, duration, &mut unanswered)? {
                        Some(frame) => frame,
                        None => return Err(frame_wait_error(c, duration)),
                    };
                    h.count_frame(&frame);
                    if c.frame_predates_change() {
                        after_change = true;
                    } else if skip > 0 {
                        skip -= 1;
                    } else {
                        break frame;
                    }
                };
                let status = auto.as_ref().map(|a| a.status().clone());
                let wall_ms = frame.wall_ms;
                let settings = acquisition(c);
                let mut metadata = serde_json::to_value(&frame)?;
                metadata["pixel_format_name"] =
                    json!(crate::inspect::pixel_format_label(frame.pixel_format));
                metadata["bytes"] = json!(frame.data.len());
                if !settings.settings.is_empty() {
                    metadata["settings"] = Value::Object(settings.settings.clone());
                }
                let mut files = Vec::new();
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
                        |path| crate::storage::save(&bytes, path, sync, false),
                    )
                    .with_context(|| format!("save {}", first.display()))?;
                    if format == "raw" {
                        save_sidecar(&path, &frame, wall_ms, &settings, sync)?;
                    }
                    folders.add(&path);
                    let file = path.to_string_lossy().into_owned();
                    let saved =
                        image_file(&path, &stored, &frame, &format, wall_ms, None, &context);
                    h.push_saved(None, saved);
                    metadata["file"] = json!(file);
                    metadata["file_bytes"] = json!(bytes.len());
                    files.push(file);
                }
                let mut value = json!({"files":files,"frame":metadata,"count":1});
                if !context.is_empty() {
                    value["context"] = json!(context);
                }
                if let Some(status) = status {
                    value["auto"] = json!(status);
                }
                if after_change {
                    value["after_change"] = json!(true);
                }
                if let Some(warmup) = warmup {
                    value["warmup_ms"] = json!(warmup.ms);
                    value["settled"] = json!(warmup.settled);
                }
                Ok(value)
            })();
            // What the transport counted while it ran, which also says why
            // a frame never came.
            let counted = baseline.counts(&Baseline::with(h, c.stats()), unanswered);
            let lingering = (!was_streaming && result.is_ok())
                .then(|| linger(c))
                .flatten();
            if let Some(linger) = lingering {
                scheduled_stream.started = true;
                scheduled_stream.linger_until = Some(Instant::now() + linger);
            } else if was_streaming {
                used(scheduled_stream, c);
            }
            let stopped = if !was_streaming && lingering.is_none() {
                c.stop()
            } else {
                Ok(())
            };
            refresh(h, camera);
            // The file's name reaches the disk before the reply.
            if sync.is_file()
                && let Err(error) = folders.sync()
                && result.is_ok()
            {
                result = Err(error);
            }
            let mut value =
                result.map_err(|error| crate::types::with_details(error, counted.clone()))?;
            if let (Some(value), Value::Object(counted)) = (value.as_object_mut(), counted) {
                value.extend(counted);
            }
            stopped?;
            h.log("info", "Captured 1 frame".into());
            Ok(value)
        }
    }
}

/// The camera a job's files come from, as its manifest lines name it.
#[derive(Clone, Default)]
struct Source {
    id: Arc<str>,
    /// What it images, which says what inspecting its frames measures.
    kind: crate::camera_profile::CameraKind,
}

struct Scheduled {
    info: crate::scheduling::CaptureJob,
    plan: crate::naming::Plan,
    /// The frame wait asked for; 0 for the camera's default.
    timeout_ms: u64,
    /// The frame wait of the frame being requested.
    wait: Duration,
    /// Frames still to let pass before the first shot.
    fresh: u32,
    /// The longest warm-up after the job starts the camera.
    warmup_ms: Option<u64>,
    /// Inspect its last frame.
    inspect: bool,
    format: String,
    sync: crate::storage::SyncMode,
    /// Where a line for each saved file goes.
    manifest: Option<Arc<crate::engine::saved::Manifest>>,
    source: Source,
    /// The folders its files were published in, synced when it ends.
    folders: crate::storage::Folders,
    deadline: Option<Instant>,
    warm: Option<Instant>,
    /// What {camera} and {serial} name, taken when the job was scheduled.
    camera: crate::naming::CameraName,
    on_exists: crate::naming::OnExists,
    /// The name picked for the frame being requested, with its clock time
    /// and number.
    next: Option<Picked>,
    /// Frame numbers handed out to shots so far, and those of shots that
    /// failed and are taken again.
    shots: u32,
    free: BTreeSet<u32>,
    /// Shots with the encoders.
    saving: u32,
    /// The camera's counters when the job first asked for a frame, which
    /// its own counts start from.
    baseline: Option<Baseline>,
    /// The caller's UTC offset, for {date} and {time}.
    utc_offset_s: Option<i32>,
    /// JPEG quality, when not the default.
    quality: Option<u8>,
    /// Its `job_end` line was sent.
    reported: bool,
    /// A stopped job's frames still with the encoders are saved until
    /// then, and given up after.
    stop_by: Option<Instant>,
    /// The software trigger of the frame being requested.
    trigger: Option<TriggerWait>,
    /// Software triggers no frame came for, which count as dropped.
    unanswered: u32,
}
/// A frame's file name, the clock time it was named at and its number.
type Picked = (PathBuf, chrono::DateTime<chrono::FixedOffset>, u32);
/// A software trigger whose frame a job waits for.
#[derive(Clone, Copy, Debug)]
struct TriggerWait {
    /// Frames the transport had lost or received incomplete when it fired.
    lost: u64,
    /// Times it was fired again for the same frame.
    retries: u32,
    /// It was fired again once because no frame came in time.
    waited_out: bool,
}
/// A camera's frame and transport counters at one moment.
#[derive(Clone, Default)]
struct Baseline {
    frames: u64,
    transport: Option<crate::types::TransportStats>,
}
impl Baseline {
    /// The counters as the acquisition loop last published them.
    fn now(h: &WorkerHandle) -> Self {
        let transport = h
            .state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .transport
            .clone();
        Self::with(h, transport)
    }
    /// The counters, with the transport's as `transport` says.
    fn with(h: &WorkerHandle, transport: Option<crate::types::TransportStats>) -> Self {
        Self {
            frames: h.counters.frames.load(Ordering::Relaxed),
            transport,
        }
    }
    /// Frames delivered since, frames the transport lost and that arrived
    /// incomplete, and packets resent; the last three where the transport
    /// counts them.
    fn since(&self, h: &WorkerHandle) -> (u64, Option<u64>, Option<u64>, Option<u64>) {
        self.until(&Self::now(h))
    }
    fn until(&self, now: &Self) -> (u64, Option<u64>, Option<u64>, Option<u64>) {
        let frames = now.frames.saturating_sub(self.frames);
        let Some(transport) = &now.transport else {
            return (frames, None, None, None);
        };
        let before = self.transport.clone().unwrap_or_default();
        (
            frames,
            Some(transport.lost_frames.saturating_sub(before.lost_frames)),
            Some(
                transport
                    .incomplete_frames
                    .saturating_sub(before.incomplete_frames),
            ),
            Some(
                transport
                    .resend_recovered
                    .saturating_sub(before.resend_recovered),
            ),
        )
    }
    /// What a result says of the frames from then until `now`: those
    /// delivered, those dropped (lost on the way, and the `unanswered`
    /// software triggers no frame came for), those incomplete and the
    /// packets resent.
    fn counts(&self, now: &Self, unanswered: u64) -> Value {
        let (frames, lost, incomplete, resent) = self.until(now);
        let mut value = json!({
            "frames": frames,
            "dropped": lost.unwrap_or(0).saturating_add(unanswered),
        });
        if let Some(incomplete) = incomplete {
            value["incomplete"] = json!(incomplete);
        }
        if let Some(resent) = resent {
            value["resent"] = json!(resent);
        }
        value
    }
}
impl Scheduled {
    /// Whether it still needs frames: shots not saved nor with the
    /// encoders.
    fn wants(&self) -> bool {
        self.info.active() && self.info.captured.saturating_add(self.saving) < self.info.count
    }
    /// Bring its frame counts up to date.
    fn tally(&mut self, h: &WorkerHandle) {
        let Some(baseline) = &self.baseline else {
            return;
        };
        let (frames, lost, incomplete, resent) = baseline.since(h);
        let info = &mut self.info;
        info.frames = frames;
        info.dropped = u64::from(info.skipped)
            .saturating_add(u64::from(self.unanswered))
            .saturating_add(lost.unwrap_or(0));
        info.incomplete = incomplete;
        info.resent = resent;
    }
    /// Pick the next frame's name, a free one, with the frame's number.
    fn allocate(&self, names: &Mutex<crate::naming::Namer>) -> Result<Picked> {
        let now = crate::naming::clock(self.utc_offset_s);
        let index = self.index();
        let path = names.lock().unwrap_or_else(|e| e.into_inner()).candidate(
            &self.plan,
            &crate::naming::NameInput {
                camera: &self.camera,
                now,
                index,
                context: &self.info.context,
            },
            self.on_exists,
        )?;
        Ok((path, now, index))
    }
    /// The number of the frame to shoot next: the first of a failed shot
    /// to take again, else the one after every number handed out.
    fn index(&self) -> u32 {
        self.free.first().copied().unwrap_or(self.shots)
    }
    /// Hand number `index` out to a shot.
    fn take(&mut self, index: u32) {
        if !self.free.remove(&index) {
            self.shots = self.shots.max(index.saturating_add(1));
        }
    }
    /// This job's file for its frame `index`, at the name picked for it.
    fn save(
        &self,
        path: PathBuf,
        now: chrono::DateTime<chrono::FixedOffset>,
        index: u32,
    ) -> saver::Save {
        saver::Save {
            job: self.info.id,
            path,
            now,
            index,
            camera: self.camera.clone(),
            plan: self.plan.clone(),
            on_exists: self.on_exists,
            format: self.format.clone(),
            quality: self.quality,
            sync: self.sync,
            manifest: self.manifest.clone(),
            source: self.source.clone(),
            context: self.info.context.clone(),
            inspect: self.inspect && index + 1 == self.info.count,
        }
    }
    /// Schedule the next shot one interval after every shot so far: saved,
    /// being saved, skipped or retried. A job that fell behind catches up.
    fn advance(&mut self) {
        let shots: u64 = [
            self.info.captured,
            self.info.skipped,
            self.info.retries,
            self.saving,
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
    // Counts stop with the job.
    for job in jobs.iter_mut().filter(|job| job.info.active()) {
        job.tally(h);
    }
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
        if job.info.active() || job.saving > 0 {
            h.events.progress(job.info.id, summary());
            continue;
        }
        // Its files' names, and its manifest, reach the disk before its end
        // is reported.
        if job.sync.is_file() {
            let manifest = job.manifest.as_ref().map(|manifest| manifest.sync());
            if let Err(error) = job.folders.sync().and(manifest.transpose()) {
                h.log("warn", format!("Capture job {}: {error:#}", job.info.id));
            }
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
/// The stream capture jobs started, which they stop once none needs it.
#[derive(Debug, Default)]
pub(super) struct JobStream {
    /// Capture jobs or a recording started it, or the cameras armed for
    /// capture jobs.
    pub(super) started: bool,
    /// Until when it streams for the first frame of jobs armed together,
    /// whether or not one is due yet.
    pub(super) armed_until: Option<Instant>,
    /// Until when it goes on streaming with nothing to do; see `linger`.
    pub(super) linger_until: Option<Instant>,
    /// When a camera that does not stream lets go of what it opened to
    /// connect: FFmpeg, which read a source's first frame, kept that long
    /// for a capture to take.
    pub(super) rest_at: Option<Instant>,
}
/// How long an armed camera streams for jobs that never come.
const ARM_HOLD: Duration = Duration::from_secs(30);
fn tick_jobs(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    jobs: &mut Vec<Scheduled>,
    stream: &mut JobStream,
    forwarding: bool,
    auto: bool,
) {
    let now = crate::scheduling::now_ms();
    let mut changed = false;
    for job in jobs.iter_mut().filter(|j| j.wants()) {
        if job.info.next_at_ms > now {
            continue;
        }
        if job.deadline.is_none() {
            changed = true;
            if job.baseline.is_none() {
                job.baseline = Some(Baseline::with(h, camera.as_ref().and_then(Camera::stats)));
            }
            let result = (|| -> Result<bool> {
                let c = camera_mut(camera)?;
                let stopped = !c.is_streaming();
                if stopped {
                    c.start()?;
                    stream.started = true;
                    // Capturefab's auto exposure never runs on a camera that
                    // sets its own, so the warm-up meters without it.
                    if let Some(warmup) = warm_up(h, c, &mut None, job.warmup_ms)? {
                        let warmup = warmup.and(
                            job.info
                                .warmup_ms
                                .zip(job.info.settled)
                                .map(|(ms, settled)| Warmup { ms, settled }),
                        );
                        job.info.warmup_ms = Some(warmup.ms);
                        job.info.settled = Some(warmup.settled);
                    }
                }
                // Read from the camera when it starts, not for every frame of
                // a burst.
                if stopped || job.wait.is_zero() {
                    job.wait = frame_wait(c, job.timeout_ms);
                }
                job.next = Some(job.allocate(&h.names)?);
                c.request_frame()?;
                job.trigger = c.software_triggered().then(|| TriggerWait {
                    lost: losses(c),
                    retries: 0,
                    waited_out: false,
                });
                // Auto exposure warms up a camera the job started, unless it
                // sends frames only when triggered.
                Ok(stopped && job.trigger.is_none())
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
            let timeout = job.wait;
            job.deadline = Some(deadline_after(timeout));
            job.warm = (auto && warm_up)
                .then(|| Instant::now() + (timeout / 2).min(Duration::from_secs(3)));
        }
        if let (Some(c), Some(deadline)) = (camera.as_mut(), job.deadline)
            && let Some(trigger) = job.trigger.as_mut()
        {
            // The frame of a software trigger the transport lost never
            // comes: fire it again, as `requested_frame` does.
            let lost = losses(c);
            let expired = Instant::now() >= deadline;
            if (lost > trigger.lost || expired)
                && trigger.retries < TRIGGER_RETRIES
                && !(expired && lost == trigger.lost && trigger.waited_out)
            {
                trigger.retries += 1;
                if lost == trigger.lost {
                    trigger.waited_out = true;
                    job.unanswered += 1;
                }
                trigger.lost = lost;
                c.trigger_unanswered();
                if let Err(error) = c.request_frame() {
                    h.log("warn", format!("Capture job {}: {error:#}", job.info.id));
                }
                job.deadline = Some(deadline_after(job.wait));
                changed = true;
                continue;
            }
        }
        if job.deadline.is_some_and(|d| Instant::now() >= d) {
            changed = true;
            let waited = job.wait;
            let error = match camera.as_mut() {
                Some(c) => frame_wait_error(c, waited),
                None => crate::types::frame_timeout("frame timeout"),
            };
            job.deadline = None;
            job.info.end("failed", Some(&error));
            h.log("error", format!("Capture job {}: {error:#}", job.info.id));
        }
    }
    // A shot with the saver holds the stream while its job still wants
    // frames: a burst would otherwise restart the camera, and warm it up,
    // as each shot is saved. A job with every frame taken lets it go at
    // once, so its end is seen as the encoders finish rather than at the
    // next frame.
    let now = Instant::now();
    if needs_stream(jobs) {
        stream.linger_until = None;
    } else if stream.started
        && !forwarding
        && stream.armed_until.is_none_or(|until| now >= until)
        && let Some(c) = camera.as_mut()
    {
        match (stream.linger_until, linger(c)) {
            // A camera that is slow to start lingers for the next command.
            (None, Some(linger)) => stream.linger_until = Some(now + linger),
            (Some(until), _) if now < until => {}
            _ => {
                let _ = c.stop();
                *stream = JobStream::default();
                changed = true;
            }
        }
    }
    // Nothing took a frame from what the camera opened to connect: no
    // FFmpeg goes on decoding, and a webcam's light goes off.
    if stream.rest_at.is_some_and(|at| now >= at) {
        stream.rest_at = None;
        if let Some(c) = camera.as_mut() {
            c.rest();
        }
    }
    if changed {
        jobs_changed(h, camera, jobs);
    }
}
/// Whether a job needs the camera to stream: it waits for a frame, or
/// still wants frames while one is saved.
fn needs_stream(jobs: &[Scheduled]) -> bool {
    jobs.iter()
        .any(|j| j.info.active() && (j.deadline.is_some() || (j.saving > 0 && j.wants())))
}
/// Stop a camera that streams only because it lingers, before a command
/// that writes its settings or checks what writing them would do: the
/// command finds it stopped, as a command of its own would, rather than its
/// geometry and frame rate locked by the stream.
fn end_linger(
    camera: &mut Option<Camera>,
    stream: &mut JobStream,
    jobs: &[Scheduled],
    forwarding: bool,
) {
    if stream.linger_until.is_none()
        || forwarding
        || stream
            .armed_until
            .is_some_and(|until| Instant::now() < until)
        || needs_stream(jobs)
    {
        return;
    }
    if let Some(c) = camera.as_mut() {
        let _ = c.stop();
    }
    *stream = JobStream::default();
}
/// How long a camera started for a capture or an inspection goes on
/// streaming once nothing needs its frames, for the next command of a
/// tight loop: live FFmpeg sources (webcams, streams), which take a long
/// time to start. Cameras that start at once stop at once, a file starts
/// from its beginning each time as it does for a command of its own, and
/// none streams idle for long: a webcam's light goes off.
pub(super) fn linger(c: &Camera) -> Option<Duration> {
    c.lingers().then_some(MEDIA_LINGER)
}
/// See `linger`.
const MEDIA_LINGER: Duration = Duration::from_secs(5);
/// Linger again from now when `stream` lingers: a command used it.
pub(super) fn used(stream: &mut JobStream, c: &Camera) {
    if stream.linger_until.is_some()
        && let Some(linger) = linger(c)
    {
        stream.linger_until = Some(Instant::now() + linger);
    }
}
/// Publish changed jobs.
fn jobs_changed(h: &WorkerHandle, camera: &mut Option<Camera>, jobs: &mut Vec<Scheduled>) {
    trim_jobs(jobs);
    sync_jobs(h, jobs);
    refresh(h, camera);
}
/// The files to save of the frame that came, for every job due now; the
/// saver encodes and writes them from its own copy. Without `room` the
/// encoders' queue is full: the frame is dropped, and due jobs take a later
/// one instead of holding up acquisition.
fn shoot(
    h: &WorkerHandle,
    c: &mut Camera,
    jobs: &mut [Scheduled],
    settled: bool,
    room: bool,
) -> Vec<saver::Save> {
    let now = crate::scheduling::now_ms();
    let mut saves = Vec::new();
    let mut changed = false;
    // A frame exposed before the camera's last settings change, or one the
    // job lets pass, is no shot: the job takes a later one.
    let stale = c.frame_predates_change();
    let mut again = false;
    for job in jobs.iter_mut().filter(|j| {
        j.info.active()
            && j.deadline.is_some()
            && j.info.next_at_ms <= now
            && (settled || j.warm.is_none_or(|w| Instant::now() >= w))
    }) {
        if stale || job.fresh > 0 {
            if stale {
                changed |= !job.info.after_change;
                job.info.after_change = true;
            } else {
                job.fresh -= 1;
            }
            again = true;
            continue;
        }
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
                        "Capture job {} is dropping frames: they come faster than they are saved",
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
            Ok((path, at, index)) => {
                saves.push(job.save(path, at, index));
                job.take(index);
                job.saving += 1;
                job.advance();
                job.info.status = if job.wants() { "pending" } else { "running" }.into();
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
    if again && let Err(error) = c.request_frame() {
        h.log("warn", format!("{error:#}"));
    }
    if changed {
        sync_jobs(h, jobs);
    }
    saves
}
/// Apply the shots the saver finished to their jobs; whether any changed.
fn saved(h: &WorkerHandle, jobs: &mut [Scheduled], saver: &saver::Saver) -> bool {
    let mut changed = false;
    loop {
        match saver.saved.try_recv() {
            Ok(saved) => {
                if let Some(job) = jobs.iter_mut().find(|job| job.info.id == saved.job) {
                    finish(h, job, saved.index, saved.result);
                    changed = true;
                }
            }
            Err(mpsc::TryRecvError::Empty) => return changed,
            // Its thread died, so no job can save another shot.
            Err(mpsc::TryRecvError::Disconnected) => {
                for job in jobs
                    .iter_mut()
                    .filter(|job| job.saving > 0 || job.info.active())
                {
                    finish(h, job, 0, Err(anyhow::anyhow!("capture saver stopped")));
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
            finish(h, job, done.index, done.result);
        }
    }
}
/// Record what became of a job's shot. A save that may work later is taken
/// again at the job's next interval; any other failure stops the job.
fn finish(h: &WorkerHandle, job: &mut Scheduled, index: u32, result: Result<saver::SavedShot>) {
    job.saving = job.saving.saturating_sub(1);
    let id = job.info.id;
    match result {
        Ok(shot) => {
            job.folders.add(Path::new(&shot.path));
            if job.info.active() && job.info.captured + 1 >= job.info.count {
                job.tally(h);
            }
            let info = &mut job.info;
            info.captured += 1;
            if info.files.len() < crate::scheduling::MAX_JOB_FILES {
                info.files.push(shot.path.clone());
            }
            info.last_file = Some(shot.path);
            info.frame = Some(shot.frame);
            if info.first_ms.is_none() {
                info.first_ms = Some(shot.wall_ms);
                info.first_id = Some(shot.id);
                info.first_ts_ns = shot.device_ts_ns;
                info.settings = shot.settings;
            }
            info.last_ms = Some(shot.wall_ms);
            info.last_id = Some(shot.id);
            info.last_ts_ns = shot.device_ts_ns;
            if shot.inspection.is_some() {
                info.inspect = shot.inspection;
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
        // A stopped job's frames the encoders had no time for end unsaved.
        Err(e) if !job.info.active() => {
            if crate::types::classify(&e).kind != crate::types::ErrorKind::Interrupted {
                h.log("error", format!("Capture job {id}: {e:#}"));
            }
        }
        Err(e) if transient(&e) => {
            // Taken again under its own number, not one still being saved.
            job.free.insert(index);
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
/// A failed save that may work later: the disk, such as a network share,
/// timed out.
fn transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
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
        Ok(trigger) if hardware(&trigger) => {
            let source = trigger.source.clone().unwrap_or_default();
            let what = match trigger.selector.as_deref() {
                Some(selector @ ("AcquisitionStart" | "FrameBurstStart")) => {
                    format!("no {selector} trigger")
                }
                _ => "no trigger".to_owned(),
            };
            crate::types::Categorized::new(
                crate::types::ErrorKind::TriggerTimeout,
                format!("{what} on {source} within {wait}; the camera sends a frame only when triggered"),
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
/// Whether `trigger` waits for a hardware line, not software or a clock.
fn hardware(trigger: &crate::controls::Trigger) -> bool {
    trigger.mode == "On"
        && trigger
            .source
            .as_deref()
            .is_some_and(|source| !matches!(source, "Software" | "Freerun" | "FixedRate"))
}
/// What reading a frame out after its exposure may take.
const READOUT: Duration = Duration::from_millis(100);
/// Note a settings change on a streaming camera, so frames exposed before
/// it are not taken for new ones (`Camera::settings_changed`).
/// `was_software` says whether it sent frames only when software triggered
/// before the change.
fn settings_changed(c: &mut Camera, was_software: bool) {
    if !c.is_streaming() {
        return;
    }
    let (exposure, fps) = crate::controls::frame_timing(c);
    let exposure = exposure.unwrap_or_default();
    let period = fps
        .and_then(|fps| Duration::try_from_secs_f64(1.0 / fps).ok())
        .unwrap_or_default();
    // A hardware-triggered camera may still deliver a frame exposed before
    // the change for its exposure and read-out. Once an acquisition trigger
    // let frames through, they run free.
    let hardware = c.has("TriggerMode")
        && crate::controls::trigger_state(c).is_ok_and(|trigger| {
            hardware(&trigger) && trigger.selector.as_deref() != Some("AcquisitionStart")
        });
    c.settings_changed(crate::camera::Change {
        hardware_hold: hardware.then(|| exposure.saturating_add(READOUT)),
        was_software,
        in_flight: exposure
            .saturating_add(period)
            .saturating_mul(2)
            .saturating_add(READOUT),
    });
}
/// The frame rate of `count` frames received over `elapsed`: by the
/// camera's clock from the first to the newest when it stamps them, else
/// by the host's.
fn frame_rate(count: u32, device_ts: Option<(u64, u64)>, elapsed: Duration) -> f64 {
    match device_ts {
        Some((first, last)) if count >= 2 && last > first => {
            f64::from(count - 1) / ((last - first) as f64 / 1e9)
        }
        _ => f64::from(count) / elapsed.as_secs_f64().max(f64::EPSILON),
    }
}
/// The shortest default wait for a frame.
const FRAME_WAIT: Duration = Duration::from_secs(5);
/// How long to wait for each frame: `timeout_ms` as asked, or for 0 the
/// camera's default: 5 s, or three exposures and frame periods when they
/// take longer.
fn frame_wait(c: &mut Camera, timeout_ms: u64) -> Duration {
    if timeout_ms > 0 {
        return Duration::from_millis(timeout_ms);
    }
    let (exposure, fps) = crate::controls::frame_timing(c);
    let period = fps
        .and_then(|fps| Duration::try_from_secs_f64(1.0 / fps).ok())
        .unwrap_or_default();
    exposure
        .unwrap_or_default()
        .saturating_add(period)
        .saturating_mul(3)
        .max(FRAME_WAIT)
}

/// How a camera that sets its own exposure was warmed up after it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Warmup {
    pub(crate) ms: u64,
    /// Whether its brightness held still before the limit.
    pub(crate) settled: bool,
}
impl Warmup {
    /// This warm-up and an earlier one of the same job: the first one's
    /// time, settled only when both settled.
    fn and(self, earlier: Option<Self>) -> Self {
        earlier.map_or(self, |earlier| Self {
            ms: earlier.ms,
            settled: earlier.settled && self.settled,
        })
    }
}
/// How much brightness may change over the last frames of a warm-up for
/// the camera to count as settled: 3 %.
const SETTLED_CHANGE: f32 = 0.03;
/// Frames whose brightness must hold still.
const SETTLED_FRAMES: usize = 3;
/// Whether the camera sets its own exposure, so its first frames after it
/// starts may be too dark or too bright: webcams and network streams read
/// through FFmpeg, and cameras whose own auto exposure runs continuously.
fn sets_own_exposure(c: &mut Camera) -> bool {
    matches!(crate::camera::kind(&c.info), "webcam" | "stream" | "onvif")
        || (c.has("ExposureAuto") && c.get("ExposureAuto").is_ok_and(|mode| mode == "Continuous"))
}
/// Let a camera that has just started, and sets its own exposure, settle:
/// drop frames until their brightness changes by less than 3 % over 3
/// frames, for at most `limit_ms` (2 s when unset; 0 does not wait). None
/// when there was nothing to wait for.
fn warm_up(
    h: &WorkerHandle,
    c: &mut Camera,
    auto: &mut Option<AutoController>,
    limit_ms: Option<u64>,
) -> Result<Option<Warmup>> {
    let limit = Duration::from_millis(limit_ms.unwrap_or(crate::session::DEFAULT_WARMUP_MS));
    // A triggered camera sends nothing to meter between triggers.
    if limit.is_zero()
        || c.software_triggered()
        || (c.has("TriggerMode")
            && crate::controls::trigger_state(c).is_ok_and(|trigger| hardware(&trigger)))
        || !sets_own_exposure(c)
    {
        return Ok(None);
    }
    let began = Instant::now();
    let deadline = deadline_after(limit);
    let mut means = VecDeque::with_capacity(SETTLED_FRAMES);
    let settled = loop {
        let Some(frame) = next_frame(h, c, auto, deadline)? else {
            break false;
        };
        h.count_frame(&frame);
        // A frame that cannot be measured cannot say it is unsettled.
        let Ok(exposure) = crate::frame::exposure(&frame) else {
            break true;
        };
        if means.len() == SETTLED_FRAMES {
            means.pop_front();
        }
        means.push_back(exposure.mean);
        if means.len() == SETTLED_FRAMES {
            let (low, high) = means.iter().fold((f32::MAX, 0f32), |(low, high), &mean| {
                (low.min(mean), high.max(mean))
            });
            // Near black, a few levels are a large share.
            if high - low <= SETTLED_CHANGE * high.max(0.1) {
                break true;
            }
        }
    };
    Ok(Some(Warmup {
        ms: u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX),
        settled,
    }))
}
/// The warning a warm-up that ran out of time leaves in a result.
pub(crate) fn unsettled_warning(warmup_ms: u64) -> String {
    format!(
        "the camera's exposure was still changing after {}; frames may be too dark or too bright (raise --warmup)",
        crate::scheduling::format_duration(warmup_ms)
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
    let saving = jobs
        .iter()
        .filter(|job| job.saving > 0)
        .filter_map(|job| job.stop_by)
        .map(|by| by.saturating_duration_since(now));
    jobs.iter()
        .filter(|job| job.wants())
        .map(|job| match job.deadline {
            Some(deadline) => deadline.saturating_duration_since(now),
            None => Duration::from_millis(job.info.next_at_ms.saturating_sub(now_ms)),
        })
        .chain(saving)
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
/// End the jobs asked to stop: a capture job once the frames it took are
/// saved (`STOP_SAVING`), a recording once its file is finished, continuous auto exposure holding
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
                // It takes no more frames; those it took are saved, unless
                // the encoders cannot finish them in time.
                scheduled.stop_by = Some(deadline_after(STOP_SAVING));
                h.log("info", format!("Capture job {job} stopped"));
                changed = true;
            }
        } else if forwarding == Some(job) {
            let _ = end_forward(h, forwarder, auto, Ended::Stopped, None, |stopped| {
                stopped
                    .as_ref()
                    .err()
                    .map(|e| ("error", format!("Recording stopped: {e:#}")))
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
/// How long the encoders may take to save the frames a stopped capture
/// job took, so its command, which allows two seconds, still reports them.
const STOP_SAVING: Duration = Duration::from_millis(1500);
/// Take back the shots no encoder started of the stopped jobs whose time to
/// save them ran out: they end unsaved, as interrupted.
fn give_up_saving(jobs: &mut [Scheduled], saver: &mut saver::Saver) {
    let now = Instant::now();
    for job in jobs.iter_mut() {
        if job.saving > 0 && job.stop_by.is_some_and(|by| now >= by) {
            job.stop_by = None;
            saver.discard(job.info.id);
        }
    }
}
/// Keep the settings this worker emulates for its camera for the next
/// worker to open it, when they changed since `kept`.
fn keep_settings(h: &WorkerHandle, camera: &mut Option<Camera>, kept: &mut Option<Value>) {
    if !h.keeps_settings {
        return;
    }
    let Some(c) = camera.as_mut() else {
        return;
    };
    let Some(now) = c.emulated_settings() else {
        return;
    };
    if kept.as_ref() == Some(&now) {
        return;
    }
    match emulated::save(&c.info.id, &now) {
        Ok(()) => *kept = Some(now),
        Err(error) => h.log("warn", format!("{error:#}")),
    }
}
fn worker(h: WorkerHandle) {
    let mut saver = saver::Saver::new(h.clone());
    let mut camera: Option<Camera> = None;
    let mut forwarder: Option<(Forwarder, f64)> = None;
    let mut auto: Option<AutoController> = None;
    let mut jobs: Vec<Scheduled> = Vec::new();
    let mut next_job = 1;
    let mut scheduled_stream = JobStream::default();
    let mut rate_epoch = Instant::now();
    let mut rate_count = 0u32;
    // The camera's clock at the first and the newest frame counted.
    let mut rate_ts: Option<(u64, u64)> = None;
    let mut consecutive_errors = 0u32;
    let mut reconnecting = false;
    let mut progress_sampler = ForwardProgressSampler::default();
    // The settings written into scheduled stills, read from the camera at
    // most once a second instead of once per shot: reading them costs a
    // register round trip per feature, which a fast schedule cannot spare.
    let mut settings = AcquisitionCache::default();
    // Whether Capturefab's auto exposure was still moving the camera at the
    // last frame.
    let mut auto_moving = false;
    let mut inspector = inspector::Inspector::default();
    let mut waiting: Vec<inspector::Waiting> = Vec::new();
    // The emulated settings last kept for the next worker.
    let mut kept: Option<Value> = None;
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
                    &mut scheduled_stream,
                );
                *h.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
                h.beat();
                continue;
            }
            if matches!(e.command.session(), Some(SessionCommand::Disconnect)) {
                settle_saves(&h, &mut saver, &mut jobs);
                keep_settings(&h, &mut camera, &mut kept);
            }
            let changes = e.command.changes_settings();
            if changes || matches!(e.command.session(), Some(SessionCommand::SetValues { .. })) {
                end_linger(
                    &mut camera,
                    &mut scheduled_stream,
                    &jobs,
                    forwarder.is_some(),
                );
            }
            // Whether the frames in flight before the change were sent
            // without a software trigger.
            let was_software = changes
                && camera
                    .as_mut()
                    .is_some_and(|c| c.is_streaming() && c.software_triggered());
            let connects = matches!(e.command.session(), Some(SessionCommand::Connect { .. }));
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
            // Even a change that failed and was put back wrote the camera.
            if changes && let Some(c) = camera.as_mut() {
                settings_changed(c, was_software);
                keep_settings(&h, &mut camera, &mut kept);
            } else if connects {
                kept = camera.as_mut().and_then(Camera::emulated_settings);
            }
            (e.reply)(result);
            h.beat();
            // Any command can connect, reconnect or change a feature.
            settings.invalidate();
            rate_epoch = Instant::now();
            rate_count = 0;
            rate_ts = None;
            consecutive_errors = 0;
        }
        apply_stops(&h, &mut camera, &mut forwarder, &mut auto, &mut jobs);
        give_up_saving(&mut jobs, &mut saver);
        inspector::expire(&h, &mut camera, &mut waiting, None);
        if saved(&h, &mut jobs, &saver) {
            jobs_changed(&h, &mut camera, &mut jobs);
        }
        // Inspections waiting for frames keep a lingering stream.
        if !waiting.is_empty()
            && let Some(c) = camera.as_ref()
        {
            used(&mut scheduled_stream, c);
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
        // A recording at its size, or on a disk below its floor, ends even
        // while no frame comes.
        if let Some(end) = forwarder.as_ref().and_then(|(f, _)| f.storage_end()) {
            forward_failed(&h, &mut forwarder, &mut auto, end);
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
                    let device_ts = frame.device_ts_ns;
                    if std::mem::take(&mut reconnecting) {
                        resumed(&h);
                        settings.invalidate();
                    }
                    if let Some(controller) = auto.as_mut() {
                        controller.observe(&frame, Instant::now());
                    }
                    let settled = auto.as_ref().is_none_or(AutoController::converged);
                    let room = saver.has_room(frame.data.len());
                    // Auto exposure moved the camera until now: read what it
                    // left once, for the shots to come.
                    if settled && std::mem::take(&mut auto_moving) {
                        settings.invalidate();
                    }
                    auto_moving |= !settled;
                    let saves = shoot(&h, c, &mut jobs, settled, room);
                    // The jobs armed together took their first frame: from
                    // now on the camera streams only while one needs it.
                    if !saves.is_empty() {
                        scheduled_stream.armed_until = None;
                    }
                    // The saver takes the frame itself when nothing else
                    // needs it, and a copy otherwise. Inspections share the
                    // frame; the forwarder then takes the sensor buffer,
                    // copied only if an inspection still holds it.
                    let forward = forwarder.is_some();
                    let (shot, frame) = if saves.is_empty() {
                        (None, Some(frame))
                    } else if forward || !waiting.is_empty() {
                        (Some(frame.clone()), Some(frame))
                    } else {
                        (Some(frame), None)
                    };
                    let frame = frame.and_then(|frame| {
                        inspector::serve(c, frame, forward, &mut waiting, &mut inspector, &h)
                    });
                    if let (Some((f, _)), Some(frame)) = (forwarder.as_mut(), frame)
                        && let Err(e) = f.push(frame)
                    {
                        forward_failed(&h, &mut forwarder, &mut auto, e);
                    }
                    if let Some(frame) = shot {
                        let mut acquisition = settings.get(c, Instant::now());
                        // While auto exposure moves the camera, the values it
                        // last wrote, rather than a read of every setting
                        // for each frame.
                        if let Some(controller) = auto.as_ref().filter(|_| !settled) {
                            acquisition.overlay(controller.values());
                        }
                        saver.offer(saver::Shot {
                            wall_ms: frame.wall_ms,
                            frame,
                            acquisition,
                            saves,
                        });
                    }
                    rate_count += 1;
                    if let Some(ts) = device_ts {
                        rate_ts = Some(rate_ts.map_or((ts, ts), |(first, _)| (first, ts)));
                    }
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
                        refresh(&h, &camera);
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
                            refresh(&h, &camera);
                        }
                    }
                },
            }
            if rate_epoch.elapsed() >= Duration::from_secs(1) {
                h.counters
                    .set_fps(frame_rate(rate_count, rate_ts, rate_epoch.elapsed()));
                rate_epoch = Instant::now();
                rate_count = 0;
                rate_ts = None;
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
    keep_settings(&h, &mut camera, &mut kept);
    if let Some(mut c) = camera
        && let Err(e) = c.stop()
    {
        h.log("warn", format!("Acquisition cleanup: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_recordings_first_written_frame_is_told_at_once() {
        let written = |written| crate::media::ForwardStats {
            accepted: 1,
            written,
            ..Default::default()
        };
        let mut sampler = ForwardProgressSampler::default();
        let now = Instant::now();
        assert_eq!(
            sampler
                .sample(Some("first"), now, || written(0))
                .unwrap()
                .written,
            0
        );
        let soon = now + Duration::from_millis(10);
        assert!(sampler.sample(Some("first"), soon, || written(0)).is_none());
        // Its first frame reached the file: told now, not at the next
        // quarter second.
        let told = sampler.sample(Some("first"), soon, || written(1)).unwrap();
        assert_eq!(told.written, 1);
        let later = soon + Duration::from_millis(10);
        assert!(
            sampler
                .sample(Some("first"), later, || written(2))
                .is_none()
        );
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
        // A replacement is told at once.
        assert!(
            sampler
                .sample(Some("replacement"), now + Duration::from_millis(251), stats)
                .is_some()
        );
        assert!(
            sampler
                .sample(None, now + Duration::from_secs(1), stats)
                .is_none()
        );
        assert_eq!(calls.get(), 3);
        assert_eq!(
            first.event(Some("/clips/a.mkv".into())),
            json!({"accepted":100,"written":91,"dropped":8,"late":7,"stalled":true,"path":"/clips/a.mkv"})
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
            on_exists: Default::default(),
            manifest: None,
            sync: Default::default(),
            context: Default::default(),
            utc_offset_s: None,
            quality: None,
            job: None,
            lease: false,
            fresh: 0,
            warmup_ms: None,
            inspect: false,
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
            max_file_bytes: Some(1 << 20),
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms: None,
            utc_offset_s: None,
            job: None,
            lease: false,
            warmup_ms: None,
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
            let _ = h.request(SessionCommand::Stop);
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
            max_file_bytes: Some(8 << 20),
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms,
            utc_offset_s: None,
            job: None,
            lease: false,
            warmup_ms: None,
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
                    &mut JobStream::default(),
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
        run(&mut camera, &mut forwarder, SessionCommand::Stop.into()).unwrap();
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
                    &mut JobStream::default(),
                    command,
                )
            };
        let connect = || SessionCommand::connect("sim:0".into(), 100);
        run(&mut camera, &mut forwarder, connect().into()).unwrap();
        let hour = Some(3_600_000);
        // A stop of its job, stop and disconnect each clear the end time.
        for (name, stop) in [
            ("stop-job", None),
            ("stop", Some(SessionCommand::Stop)),
            ("disconnect", Some(SessionCommand::Disconnect)),
        ] {
            let output = root.join(format!("{name}.mkv"));
            let mut forward = timed_forward(&output, hour);
            if let SessionCommand::Forward { job, .. } = &mut forward {
                *job = Some(1);
            }
            let reply = run(&mut camera, &mut forwarder, forward.into()).unwrap();
            let since = reply["since_ms"].as_u64().unwrap();
            assert_eq!(reply["until_ms"].as_u64(), Some(since + 3_600_000));
            let snapshot = h.snapshot();
            assert_eq!(
                (snapshot.forwarding_since_ms, snapshot.forwarding_until_ms),
                (Some(since), Some(since + 3_600_000)),
                "{name}"
            );
            match stop {
                Some(stop) => {
                    run(&mut camera, &mut forwarder, stop.into()).unwrap();
                }
                None => {
                    h.stop_job(1);
                    apply_stops(&h, &mut camera, &mut forwarder, &mut None, &mut Vec::new());
                }
            }
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
        run(&mut camera, &mut forwarder, SessionCommand::Stop.into()).unwrap();
        // At its end time the worker ends a timed forward as a stop does,
        // and says so in the step that clears it.
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
                    &mut JobStream::default(),
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
        run(&mut camera, &mut forwarder, SessionCommand::Stop.into()).unwrap();
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
        run(&mut camera, &mut forwarder, SessionCommand::Stop.into()).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn a_camera_a_recording_started_stops_when_the_recording_ends() {
        // Ended by its duration, or at its first frame where no FFmpeg
        // runs: either way nothing needs the camera's frames after it.
        let root = temp_dir("recording-stops-camera");
        let h = WorkerHandle::new();
        h.request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        h.request(timed_forward(&root.join("clip.mkv"), Some(300)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.snapshot().forwarding.is_some() || h.snapshot().streaming {
            assert!(
                Instant::now() < deadline,
                "the camera streams on after its recording"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // A camera that streamed before the recording goes on streaming.
        h.request(SessionCommand::Start).unwrap();
        h.request(timed_forward(&root.join("live.mkv"), Some(300)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.snapshot().forwarding.is_some() {
            assert!(Instant::now() < deadline, "the recording never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(h.snapshot().streaming);
        h.request(SessionCommand::Stop).unwrap();
        h.shutdown();
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
        // The camera it started stops with it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.snapshot().streaming {
            assert!(Instant::now() < deadline, "the camera streams on");
            std::thread::sleep(Duration::from_millis(5));
        }
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
        // Nothing is left to stop: stopping saves no second file.
        h.request(SessionCommand::Stop).unwrap();
        assert!(h.take_saved().is_empty());
        h.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn session_simulator_configuration_and_acquisition() {
        let h = WorkerHandle::new();
        h.request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        h.set_value("Width", "128").unwrap();
        assert_eq!(h.value("Width"), 128);
        assert!(h.set_value("Width", "1").is_err());
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
                on_exists: Default::default(),
                sync: Default::default(),
                context: context.clone(),
                utc_offset_s: None,
                quality: None,
                stdout: None,
                fresh: 0,
                warmup_ms: None,
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
        // Files are hashed only for a manifest, which a capture command has not.
        assert_eq!(saved.sha256, None);
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
            on_exists,
            sync: Default::default(),
            context: Default::default(),
            utc_offset_s: None,
            quality: None,
            stdout: None,
            fresh: 0,
            warmup_ms: None,
        }
    }
    #[test]
    fn session_clients_keep_folder_and_format_rules_for_image_names() {
        use crate::naming::OnExists;
        let h = WorkerHandle::new();
        connect(&h, "1000");
        let dir = temp_dir("guardrails");
        // The CLI refuses these names as typed (check_capture_name); the
        // worker keeps serving RPC clients as before: the format wins.
        let label = crate::naming::camera_label(&h.snapshot().connected.unwrap());
        let in_folder = |file: &Value, folder: &Path, seq: u32| {
            let file = Path::new(file.as_str().unwrap());
            let name = file.file_name().unwrap().to_str().unwrap();
            file.parent() == Some(folder)
                && name.starts_with(&format!("{label}-"))
                && name.ends_with(&format!("-{seq:04}.raw"))
        };
        // One frame: a sequence is a schedule.
        let error = h
            .request(capture_to(&dir.join("shot.raw"), 3, OnExists::Fail))
            .unwrap_err();
        assert!(error.to_string().contains("schedule"), "{error:#}");
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
                max_file_bytes: Some(1 << 20),
                on_exists: Default::default(),
                context: Default::default(),
                duration_ms: None,
                utc_offset_s: None,
                job: None,
                lease: false,
                warmup_ms: None,
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
        std::fs::create_dir(&frames).unwrap();
        for run in 0..3 {
            let v = h.request(capture_to(&frames, 1, OnExists::Fail)).unwrap();
            let name = &files(v)[0];
            let name = name
                .strip_prefix(&format!("frames{}{label}-", std::path::MAIN_SEPARATOR))
                .unwrap_or_else(|| panic!("{name}"));
            let (time, seq) = name.split_at("20261031-142233.123".len());
            assert!(
                time.bytes()
                    .all(|b| b.is_ascii_digit() || b == b'-' || b == b'.'),
                "{name}"
            );
            assert_eq!(seq, format!("-{:04}.raw", run + 1));
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
                on_exists: Default::default(),
                manifest: None,
                sync: Default::default(),
                context: crate::context::Context::from([("lot".into(), "7".into())]),
                utc_offset_s: None,
                quality: None,
                job: None,
                lease: false,
                fresh: 0,
                warmup_ms: None,
                inspect: false,
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
        // Each raw file has its sidecar.
        assert_eq!(
            names,
            [
                "s-0001.raw",
                "s-0001.raw.json",
                "s-0002.raw",
                "s-0002.raw.json",
                "s-0003.raw",
                "s-0003.raw.json"
            ]
        );
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
            sync: Default::default(),
            manifest: None,
            source: Default::default(),
            folders: Default::default(),
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            warm: None,
            camera: crate::naming::CameraName {
                label: "camera".into(),
                serial: "unknown".into(),
            },
            on_exists: Default::default(),
            next: None,
            shots: 0,
            free: Default::default(),
            stop_by: None,
            trigger: None,
            unanswered: 0,
            saving: 0,
            baseline: None,
            utc_offset_s: None,
            quality: None,
            reported: false,
            fresh: 0,
            wait: Duration::ZERO,
            warmup_ms: None,
            inspect: false,
        }
    }
    /// What the saver reports for a shot it wrote to `save.path`.
    fn written(shot: &saver::Shot, save: &saver::Save) -> saver::Written {
        let path = save.path.to_string_lossy().into_owned();
        saver::Written {
            file: crate::engine::saved::SavedFile {
                wall_ms: shot.wall_ms,
                path: path.clone(),
                bytes: 64,
                sha256: None,
                context: Default::default(),
                item: crate::engine::saved::SavedItem::Capture {
                    frame_id: shot.frame.id,
                    device_ts_ns: shot.frame.device_ts_ns,
                    width: 8,
                    height: 8,
                    pixel_format: crate::types::MONO8,
                    format: "raw".into(),
                    origin: "schedule".into(),
                    job_id: Some(save.job),
                },
            },
            line: None,
            shot: saver::SavedShot {
                path,
                frame: crate::scheduling::JobFrame {
                    width: 8,
                    height: 8,
                    pixel_format: crate::types::MONO8,
                },
                id: shot.frame.id,
                device_ts_ns: shot.frame.device_ts_ns,
                wall_ms: shot.wall_ms,
                settings: Default::default(),
                inspection: None,
            },
        }
    }
    #[test]
    fn frame_waits_default_to_five_seconds_or_three_slow_frames() {
        let mut c = sim_camera();
        assert_eq!(frame_wait(&mut c, 0), Duration::from_secs(5));
        assert_eq!(frame_wait(&mut c, 250), Duration::from_millis(250));
        c.set("AcquisitionFrameRate", "1").unwrap();
        c.set("ExposureTime", "1000000").unwrap();
        assert_eq!(frame_wait(&mut c, 0), Duration::from_secs(6));
    }
    fn sim_camera() -> Camera {
        Camera::open("sim:0", &[], Duration::from_secs(1)).unwrap()
    }
    fn small_frame(id: u64) -> Frame {
        Frame {
            id,
            width: 8,
            height: 8,
            pixel_format: crate::types::MONO8,
            device_ts_ns: Some(id),
            wall_ms: 0,
            data: vec![id as u8; 64],
        }
    }
    /// Offer `frame` to the jobs due, as the camera thread does.
    fn offer(
        h: &WorkerHandle,
        saver: &mut saver::Saver,
        jobs: &mut [Scheduled],
        frame: Frame,
    ) -> bool {
        let room = saver.has_room(frame.data.len());
        let saves = shoot(h, &mut sim_camera(), jobs, true, room);
        if saves.is_empty() {
            return false;
        }
        saver.offer(saver::Shot {
            wall_ms: frame.wall_ms,
            frame,
            acquisition: Acquisition::default(),
            saves,
        });
        true
    }
    /// Apply what the saver finished until no job waits for it.
    fn drain(h: &WorkerHandle, saver: &saver::Saver, jobs: &mut [Scheduled]) {
        let until = Instant::now() + Duration::from_secs(10);
        while jobs.iter().any(|job| job.saving > 0) && Instant::now() < until {
            saved(h, jobs, saver);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(saver.held(), 0);
    }
    #[test]
    fn slow_saves_hold_up_no_frames_and_a_full_queue_drops_frames() {
        fn slow(
            _: &WorkerHandle,
            shot: &saver::Shot,
            save: &saver::Save,
        ) -> Result<saver::Written> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(written(shot, save))
        }
        let h = WorkerHandle::new();
        // Room for two of the 64-byte frames.
        let mut saver = saver::Saver::with(h.clone(), slow, 128);
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
            let started = Instant::now();
            offer(&h, &mut saver, &mut jobs, small_frame(id));
            let took = started.elapsed();
            assert!(took < Duration::from_millis(50), "frame {id} took {took:?}");
        }
        // Two shots fill the queue; the third job's frame is dropped and it
        // takes another at its next interval.
        assert_eq!((jobs[0].saving, jobs[1].saving, jobs[2].saving), (1, 1, 0));
        assert_eq!(jobs[2].info.skipped, 1);
        assert_eq!(jobs[2].info.status, "pending");
        assert_eq!(jobs[2].info.next_at_ms, 60_000);
        assert!(jobs[2].deadline.is_none());
        drain(&h, &saver, &mut jobs);
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
    fn a_burst_is_encoded_in_parallel_and_reported_in_frame_order() {
        // The first frames take longest, so later ones finish first.
        fn uneven(
            _: &WorkerHandle,
            shot: &saver::Shot,
            save: &saver::Save,
        ) -> Result<saver::Written> {
            std::thread::sleep(Duration::from_millis(60 / shot.frame.id));
            Ok(written(shot, save))
        }
        let h = WorkerHandle::new();
        let mut saver = saver::Saver::with(h.clone(), uneven, saver::QUEUE_BYTES);
        let dir = temp_dir("ordered-burst");
        let mut burst = armed(1, &dir);
        burst.info.count = 6;
        burst.info.interval_ms = 0;
        let mut jobs = vec![burst];
        let started = Instant::now();
        for id in 1..=6u64 {
            jobs[0].deadline = Some(Instant::now() + Duration::from_secs(10));
            assert!(offer(&h, &mut saver, &mut jobs, small_frame(id)));
        }
        // Every frame was taken while the earlier ones were still encoding.
        assert!(started.elapsed() < Duration::from_millis(60));
        assert_eq!(jobs[0].saving, 6);
        drain(&h, &saver, &mut jobs);
        let info = &jobs[0].info;
        assert_eq!(info.status, "complete");
        assert_eq!(
            info.files
                .iter()
                .map(|file| file.rsplit('-').next().unwrap().to_owned())
                .collect::<Vec<_>>(),
            (1..=6).map(|n| format!("{n:06}.raw")).collect::<Vec<_>>()
        );
        assert_eq!((info.first_id, info.last_id), (Some(1), Some(6)));
        assert_eq!((info.first_ts_ns, info.last_ts_ns), (Some(1), Some(6)));
        let saved = h
            .take_saved()
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        assert_eq!(saved, info.files);
        drop(saver);
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_stopped_burst_saves_the_frames_it_took_while_it_has_time() {
        fn slow(
            _: &WorkerHandle,
            shot: &saver::Shot,
            save: &saver::Save,
        ) -> Result<saver::Written> {
            std::thread::sleep(Duration::from_millis(100));
            Ok(written(shot, save))
        }
        let encoders = crate::host::threads(usize::MAX);
        let stop = |count: usize| {
            let h = WorkerHandle::new();
            let mut saver = saver::Saver::with(h.clone(), slow, saver::QUEUE_BYTES);
            let dir = temp_dir("stopped-burst");
            let mut burst = armed(1, &dir);
            burst.info.count = count as u32;
            burst.info.interval_ms = 0;
            let mut jobs = vec![burst];
            for id in 1..=count as u64 {
                jobs[0].deadline = Some(Instant::now() + Duration::from_secs(10));
                assert!(offer(&h, &mut saver, &mut jobs, small_frame(id)));
            }
            // More frames wait than there are encoders.
            h.stops.lock().unwrap().insert(1);
            let stopped = Instant::now();
            apply_stops(&h, &mut None, &mut None, &mut None, &mut jobs);
            assert_eq!(jobs[0].info.status, "cancelled");
            while jobs[0].saving > 0 && stopped.elapsed() < Duration::from_secs(10) {
                saved(&h, &mut jobs, &saver);
                give_up_saving(&mut jobs, &mut saver);
                std::thread::sleep(Duration::from_millis(5));
            }
            let took = stopped.elapsed();
            let saved = jobs[0].info.captured as usize;
            assert_eq!(h.take_saved().len(), saved);
            assert_eq!(jobs[0].saving, 0);
            drop(saver);
            h.shutdown();
            std::fs::remove_dir_all(dir).unwrap();
            (saved, took)
        };
        // Every frame it took, when the encoders finish them in time.
        let few = encoders * 3;
        assert_eq!(stop(few).0, few);
        // Those they could not get to in time end unsaved.
        let many = encoders * 30;
        let (saved, took) = stop(many);
        assert!(saved > encoders && saved < many, "{saved} of {many}");
        assert!(took < STOP_SAVING + Duration::from_millis(500), "{took:?}");
    }
    #[test]
    fn a_disconnect_waits_for_the_shot_the_saver_is_writing() {
        fn slow(
            _: &WorkerHandle,
            shot: &saver::Shot,
            save: &saver::Save,
        ) -> Result<saver::Written> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(written(shot, save))
        }
        let h = WorkerHandle::new();
        let mut saver = saver::Saver::with(h.clone(), slow, saver::QUEUE_BYTES);
        let dir = temp_dir("disconnect-saves");
        let mut jobs = vec![armed(1, &dir)];
        assert!(offer(&h, &mut saver, &mut jobs, small_frame(1)));
        assert_eq!(jobs[0].saving, 1);
        settle_saves(&h, &mut saver, &mut jobs);
        assert_eq!(jobs[0].saving, 0);
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
        assert!(saver.has_room(64));
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
        let mut started = JobStream::default();
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(started.started && jobs[0].deadline.is_some() && jobs[0].warm.is_some());
        camera
            .as_mut()
            .unwrap()
            .next_frame(Duration::from_secs(1))
            .unwrap();
        assert!(!shoot(&h, camera.as_mut().unwrap(), &mut jobs, true, true).is_empty());
        assert!(jobs[0].saving == 1 && jobs[0].info.next_at_ms <= crate::scheduling::now_ms());
        // Due again while its shot is saved: the stream keeps running.
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(started.started && camera.as_ref().unwrap().is_streaming());
        assert_eq!(count(), (1, 0));
        // Saved: the next shot needs no start and no warm-up.
        finish(
            &h,
            &mut jobs[0],
            0,
            Ok(saver::SavedShot {
                path: "shot.raw".into(),
                frame: crate::scheduling::JobFrame {
                    width: 8,
                    height: 8,
                    pixel_format: crate::types::MONO8,
                },
                id: 1,
                device_ts_ns: None,
                wall_ms: 0,
                settings: Default::default(),
                inspection: None,
            }),
        );
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(jobs[0].deadline.is_some() && jobs[0].warm.is_none());
        assert_eq!(count(), (1, 0));
        // Nothing due or being saved: the schedule stops the stream it started.
        jobs[0].deadline = None;
        jobs[0].info.next_at_ms = u64::MAX;
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(!started.started && !camera.as_ref().unwrap().is_streaming());
        assert_eq!(count(), (1, 1));
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_job_with_every_frame_at_the_encoders_lets_the_stream_go() {
        let h = WorkerHandle::new();
        let mut camera = Some(Camera::open("sim:0", &[], Duration::from_secs(1)).unwrap());
        let dir = temp_dir("taken-stream");
        let mut job = armed(1, &dir);
        job.deadline = None;
        job.info.count = 1;
        let mut jobs = vec![job];
        let mut stream = JobStream::default();
        tick_jobs(&h, &mut camera, &mut jobs, &mut stream, false, false);
        assert!(stream.started && camera.as_ref().unwrap().is_streaming());
        camera
            .as_mut()
            .unwrap()
            .next_frame(Duration::from_secs(1))
            .unwrap();
        assert!(!shoot(&h, camera.as_mut().unwrap(), &mut jobs, true, true).is_empty());
        assert_eq!(jobs[0].saving, 1);
        // Its one frame is being saved: nothing needs the camera, which
        // stops now rather than at the next frame, and does not linger.
        tick_jobs(&h, &mut camera, &mut jobs, &mut stream, false, false);
        assert!(!stream.started && !camera.as_ref().unwrap().is_streaming());
        assert!(linger(camera.as_ref().unwrap()).is_none());
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
        let (frame, _, _, _) = inspector::acquire(
            &h,
            &mut camera,
            &mut None,
            1,
            Instant::now() + wait,
            wait,
            None,
            None,
        )
        .unwrap();
        assert_eq!(frame.width, 4);
        assert_eq!(triggers(), 1);
        let dir = temp_dir("software-trigger");
        let captured = run_command(
            &h,
            &mut camera,
            capture_to(&dir.join("shot.raw"), 1, crate::naming::OnExists::Fail),
        )
        .unwrap();
        assert_eq!(captured["files"].as_array().unwrap().len(), 1);
        assert_eq!(triggers(), 1);
        // A shot fires its trigger when due; auto exposure has no frames to
        // warm up on, so the triggered frame is taken.
        let mut job = armed(1, &dir);
        job.deadline = None;
        let mut jobs = vec![job];
        let mut started = JobStream::default();
        tick_jobs(&h, &mut camera, &mut jobs, &mut started, false, true);
        assert!(started.started && jobs[0].deadline.is_some() && jobs[0].warm.is_none());
        assert_eq!(triggers(), 1);
        camera
            .as_mut()
            .unwrap()
            .next_frame(Duration::from_secs(1))
            .unwrap();
        assert!(!shoot(&h, camera.as_mut().unwrap(), &mut jobs, false, true).is_empty());
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_slow_disk_retries_the_shot_and_other_failures_stop_the_job() {
        let h = WorkerHandle::new();
        let dir = temp_dir("retry");
        let mut job = armed(1, &dir);
        let shoot_once = |job: &mut Scheduled, result: Result<String>| {
            let index = job.index();
            job.take(index);
            job.saving += 1;
            job.advance();
            let result = result.map(|path| saver::SavedShot {
                path,
                frame: crate::scheduling::JobFrame {
                    width: 8,
                    height: 8,
                    pixel_format: crate::types::MONO8,
                },
                id: 1,
                device_ts_ns: None,
                wall_ms: 0,
                settings: Default::default(),
                inspection: None,
            });
            finish(&h, job, index, result);
        };
        let slow_disk = || {
            let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
            Err(anyhow::Error::from(timed_out).context("save shot.raw"))
        };
        shoot_once(&mut job, slow_disk());
        assert!(job.info.active() && job.saving == 0);
        assert_eq!((job.info.retries, job.info.captured), (1, 0));
        assert_eq!(job.info.status, "pending");
        assert!(job.info.error.as_deref().unwrap().contains("timed out"));
        // The same frame number, one interval later.
        assert_eq!(job.info.next_at_ms, 60_000);
        assert_eq!(job.index(), 0);
        shoot_once(&mut job, slow_disk());
        assert_eq!((job.info.retries, job.info.next_at_ms), (2, 120_000));
        // A later save clears the error.
        shoot_once(&mut job, Ok("shot.raw".into()));
        assert_eq!((job.info.captured, job.info.error.as_deref()), (1, None));
        assert_eq!(job.info.next_at_ms, 180_000);
        // A path that mentions timeouts does not make a refusal transient.
        let refused = anyhow::anyhow!("Permission denied").context("save /data/timeouts/a.raw");
        shoot_once(&mut job, Err(refused));
        assert_eq!(job.info.status, "failed");
        // A disk below its floor ends the job with its reason.
        let mut full = armed(2, &dir);
        let floor = crate::types::Categorized::error(
            crate::types::ErrorKind::StorageFull,
            "100 MiB free on the disk of /data/a.raw",
        );
        shoot_once(&mut full, Err(floor));
        assert_eq!(full.info.status, "failed");
        assert_eq!(full.info.error_tag.as_ref().unwrap().code, "storage_full");
        h.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_burst_takes_a_failed_frame_again_under_its_own_number() {
        let h = WorkerHandle::new();
        let dir = temp_dir("burst-retry");
        let mut job = armed(1, &dir);
        job.info.count = 3;
        // Three shots with the encoders, numbered 0, 1 and 2.
        for index in 0..3 {
            assert_eq!(job.index(), index);
            job.take(index);
            job.saving += 1;
        }
        assert!(!job.wants());
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        finish(
            &h,
            &mut job,
            0,
            Err(anyhow::Error::from(timed_out).context("save job1-000001.raw")),
        );
        // Frame 0 is shot again, never a number still being saved.
        assert!(job.wants());
        assert_eq!(job.index(), 0);
        let (path, _, index) = job.allocate(&h.names).unwrap();
        assert_eq!(index, 0);
        assert_eq!(path, dir.join("job1-000001.raw"));
        job.take(index);
        job.saving += 1;
        assert_eq!(job.index(), 3);
        assert!(!job.wants());
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
        // A job whose every frame is with the saver is woken by it.
        job.deadline = None;
        job.saving = 2;
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
            h.set_value(feature, value).unwrap();
        }
    }
    fn auto(h: &WorkerHandle, balance: Option<f64>) -> Result<Value> {
        h.request(SessionCommand::Auto {
            balance,
            continuous: false,
            job: None,
            max_exposure_us: None,
            region: None,
        })
    }
    fn set(h: &WorkerHandle, feature: &str, value: &str) {
        h.set_value(feature, value).unwrap();
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
            warmup_ms: None,
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
                warmup_ms: None,
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
            max_exposure_us: None,
            region: None,
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
                warmup_ms: None,
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
            device_ts_ns: Some(9),
            wall_ms: 0,
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
        let set = |feature: &str, value: &str| h.set_value(feature, value).unwrap();
        set("Width", "128");
        assert!(h.snapshot().auto.is_some());
        let manual = set("ExposureTime", "5000");
        assert_eq!(manual["changed"][0]["to"], 5000.0, "{manual}");
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
                on_exists: Default::default(),
                sync: Default::default(),
                context: Default::default(),
                utc_offset_s: None,
                quality: None,
                stdout: None,
                fresh: 0,
                warmup_ms: None,
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
            &mut JobStream::default(),
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

    /// The simulator as a live source: it goes on streaming a moment after
    /// a capture, and counts the times it let go of what it connected with.
    struct Lingering {
        simulator: crate::transport::simulator::Simulator,
        rests: Arc<AtomicU64>,
    }
    impl crate::types::RegisterIo for Lingering {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.simulator.read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.simulator.write_memory(address, data)
        }
    }
    impl crate::types::Backend for Lingering {
        fn xml(&mut self) -> Result<String> {
            self.simulator.xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.simulator.start(payload)
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            self.simulator.next_frame(timeout)
        }
        fn stop(&mut self) -> Result<()> {
            self.simulator.stop()
        }
        fn lingers(&self) -> bool {
            true
        }
        fn rest(&mut self) {
            self.rests.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn lingering(rests: &Arc<AtomicU64>) -> Option<Camera> {
        let backend = Lingering {
            simulator: Default::default(),
            rests: rests.clone(),
        };
        Some(Camera::from_backend(crate::transport::simulator::info(), Box::new(backend)).unwrap())
    }

    #[test]
    fn a_change_finds_a_lingering_camera_stopped() {
        let h = WorkerHandle::new();
        let mut camera = lingering(&Arc::default());
        let mut stream = JobStream::default();
        let wait = Duration::from_secs(1);
        inspector::acquire(
            &h,
            &mut camera,
            &mut None,
            1,
            Instant::now() + wait,
            wait,
            None,
            Some(&mut stream),
        )
        .unwrap();
        assert!(camera.as_ref().unwrap().is_streaming() && stream.linger_until.is_some());
        // A recording holds the stream.
        end_linger(&mut camera, &mut stream, &[], true);
        assert!(camera.as_ref().unwrap().is_streaming());
        end_linger(&mut camera, &mut stream, &[], false);
        assert!(!camera.as_ref().unwrap().is_streaming() && stream.linger_until.is_none());
        // Width, locked while the camera streams, changes as it would for
        // a command of its own.
        let values = vec![crate::controls::Assignment::parse("Width=320").unwrap()];
        let set = process(
            &h,
            &mut camera,
            &mut None,
            &mut None,
            &mut Vec::new(),
            &mut 1,
            &mut stream,
            SessionCommand::SetValues {
                values,
                dry_run: false,
            }
            .into(),
        )
        .unwrap();
        assert_eq!(set["changed"][0]["to"], 320, "{set}");
        h.shutdown();
    }
    #[test]
    fn a_camera_lets_go_of_what_it_connected_with_once_unused() {
        let h = WorkerHandle::new();
        let mut stream = JobStream::default();
        process(
            &h,
            &mut None,
            &mut None,
            &mut None,
            &mut Vec::new(),
            &mut 1,
            &mut stream,
            SessionCommand::connect("sim:0".into(), 1000).into(),
        )
        .unwrap();
        assert!(stream.rest_at.is_some_and(|at| at > Instant::now()));
        let rests = Arc::new(AtomicU64::new(0));
        let mut camera = lingering(&rests);
        tick_jobs(&h, &mut camera, &mut Vec::new(), &mut stream, false, false);
        assert_eq!(rests.load(Ordering::Relaxed), 0);
        stream.rest_at = Some(Instant::now());
        tick_jobs(&h, &mut camera, &mut Vec::new(), &mut stream, false, false);
        assert_eq!(rests.load(Ordering::Relaxed), 1);
        assert!(stream.rest_at.is_none());
        // A streaming camera keeps what it streams with.
        camera.as_mut().unwrap().start().unwrap();
        stream.rest_at = Some(Instant::now());
        tick_jobs(&h, &mut camera, &mut Vec::new(), &mut stream, false, false);
        assert_eq!(rests.load(Ordering::Relaxed), 1);
        h.shutdown();
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
            let error = inspector::acquire(
                &h,
                &mut camera,
                &mut None,
                1,
                Instant::now() + wait,
                wait,
                None,
                None,
            )
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
            warmup_ms: None,
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
        let (mut next_job, mut stream) = (1, JobStream::default());
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
                max_exposure_us: None,
                region: None,
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
            crate::dirs::ensure_private_dir(&session).unwrap();
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
        let index = logins.index_path().unwrap();
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
