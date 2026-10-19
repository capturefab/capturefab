//! The frame pump: one thread per connected hub camera owns every network
//! read, image decode and reconnect, so the camera worker's engine thread
//! only ever waits on a condition variable and keeps handling commands while
//! a hub is slow or away.
//!
//! Transient failures (refused connections, timeouts, a stream that ended)
//! reconnect with exponential backoff and are invisible to the engine except
//! as "no frame yet". Permanent failures (a refused secret or certificate, a
//! missing camera) end the pump; every later read returns the failure, so the
//! engine stops acquisition and reports it. A refused secret is also recorded
//! in the hub's saved record so discovery stops sending it.
use super::{Stream, http::Cancel, http::HttpError, registry::Store};
use crate::types::{Backend, Categorized, ErrorKind, Frame, FrameWait, RegisterIo, TransportStats};
use anyhow::Result;
use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Tests run the same schedule a hundred times faster.
const SCALE: u64 = if cfg!(test) { 10 } else { 1000 };
/// Backoff bounds between reconnect attempts.
const BACKOFF_MIN: Duration = Duration::from_millis(SCALE);
const BACKOFF_MAX: Duration = Duration::from_millis(30 * SCALE);
const RETRY_AFTER_MAX: Duration = Duration::from_millis(300 * SCALE);
/// A source that streamed this long without failing starts its backoff over.
const HEALTHY: Duration = Duration::from_millis(30 * SCALE);
/// After connecting, a stream is kept reading this long waiting for
/// acquisition to start (the GUI starts right after connecting).
const PARK: Duration = Duration::from_millis(10 * SCALE);
/// A frame older than this when acquisition starts is not delivered.
const STALE: Duration = Duration::from_millis(SCALE);
/// How long dropping a backend waits for its pump to finish.
const JOIN: Duration = Duration::from_secs(1);

/// Why a source produced no frame.
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    pub kind: ErrorKind,
    /// Retrying cannot help.
    pub permanent: bool,
    /// The HTTP status when the server refused the secret (401 or 403).
    pub auth_status: Option<u16>,
    /// When the server asked to wait before retrying.
    pub retry_after: Option<Duration>,
    /// Reconnect at once without counting an attempt (an idle stream that a
    /// server closed, as Home Assistant does for unchanged images).
    pub quiet: bool,
    /// Whether `stream=auto` may try the next stream after this at connect.
    pub fallback: bool,
    pub message: String,
}

impl Failure {
    pub(crate) fn transient(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            permanent: false,
            auth_status: None,
            retry_after: None,
            quiet: false,
            fallback: true,
            message: message.into(),
        }
    }
    pub(crate) fn permanent(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            permanent: true,
            ..Self::transient(kind, message)
        }
    }
    /// The server refused the secret: never retried automatically, and no
    /// other stream is tried with it.
    pub(crate) fn rejected(status: u16, message: impl Into<String>) -> Self {
        Self {
            auth_status: Some(status),
            fallback: false,
            ..Self::permanent(ErrorKind::OperationFailed, message)
        }
    }
    pub(crate) fn into_error(self) -> anyhow::Error {
        Categorized::error(self.kind, self.message)
    }
}

impl From<HttpError> for Failure {
    fn from(error: HttpError) -> Self {
        let mut failure = if error.permanent() {
            Self::permanent(error.kind(), error.message())
        } else {
            Self::transient(error.kind(), error.message())
        };
        // A certificate problem is the same on every stream of the hub.
        failure.fallback = !error.permanent();
        failure
    }
}

pub(crate) enum Next {
    Frame(Frame),
    /// Nothing usable this time (a duplicate or damaged image); the stream
    /// goes on.
    Skip(&'static str),
}

/// Wakes a snapshot source early when a capture asks for a fresh image.
#[derive(Default)]
pub(crate) struct Wake {
    pending: Mutex<bool>,
    changed: Condvar,
    /// Increased under the publication lock whenever capture asks for a
    /// fresh snapshot, independently of the poller's current sequence.
    request_generation: AtomicU64,
}

impl Wake {
    fn wake(&self) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

/// What a session gets from the pump.
pub(crate) struct Cx {
    pub cancel: Cancel,
    wake: Arc<Wake>,
    snapshot_generation: AtomicU64,
}

impl Cx {
    pub(crate) fn new() -> Self {
        Self {
            cancel: Cancel::default(),
            wake: Arc::default(),
            snapshot_generation: AtomicU64::new(0),
        }
    }
    fn with_wake(wake: Arc<Wake>) -> Self {
        Self {
            cancel: Cancel::default(),
            wake,
            snapshot_generation: AtomicU64::new(0),
        }
    }
    /// Sleep until `at`, a wake-up, or cancellation; true when woken early.
    pub(crate) fn wait_until(&self, at: Instant) -> bool {
        let mut pending = self
            .wake
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if std::mem::take(&mut *pending) {
                return true;
            }
            if self.cancel.is_cancelled() {
                return true;
            }
            let left = at.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            pending = self
                .wake
                .changed
                .wait_timeout(pending, left.min(Duration::from_millis(100)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
    /// Called after the polling wait, immediately before a snapshot GET.
    /// A capture while that GET is in flight must wait for its successor.
    pub(crate) fn begin_snapshot(&self) {
        self.snapshot_generation.store(
            self.wake.request_generation.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
    }
}

/// One open stream or polling loop.
pub(crate) trait FrameSession: Send {
    fn next(&mut self, cx: &Cx) -> Result<Next, Failure>;
    /// Whether frames arrive on their own. Snapshot sources fetch only when
    /// acquisition runs or a capture asks.
    fn continuous(&self) -> bool {
        true
    }
}

/// Opens sessions of one kind of stream, resolving the camera again each
/// time (a hub may hand out a different stream after a restart).
pub(crate) trait SourceFactory: Send + Sync {
    fn stream(&self) -> Stream;
    fn open(&self, cx: &Cx) -> Result<Box<dyn FrameSession>, Failure>;
    /// The rate a polling source asks for images, shown as the camera's
    /// frame rate; 0 for streams.
    fn fps(&self) -> f64 {
        0.0
    }
    /// The least time to wait for the first frame when connecting: Home
    /// Assistant may take 10 s for one image, and a restream dials its
    /// camera on demand.
    fn first_frame(&self) -> Duration {
        Duration::from_millis(12 * SCALE)
    }
}

/// Identity and messages for a hub camera.
#[derive(Clone)]
pub(crate) struct Meta {
    /// The camera's ID, `hub:NAME/CAMERA`.
    pub id: String,
    /// The product name, for "change them in Home Assistant".
    pub product: &'static str,
    /// Where a refused secret is recorded: the store, hub name and the
    /// digest of the record the secret came from.
    pub latch: Option<(Store, String, [u8; 32])>,
}

#[derive(Default)]
struct State {
    seq: u64,
    latest: Option<(u64, Instant, Frame)>,
    failure: Option<(ErrorKind, String)>,
    running: bool,
    stop: bool,
    /// Retain a continuous stream until then while not running.
    park_until: Option<Instant>,
    /// Frames of another size than the first after starting are dropped.
    size: Option<(u32, u32)>,
    /// A capture asked for a frame newer than this sequence number.
    demand: Option<u64>,
    /// Cancels the session being read, so stop does not wait for the
    /// network.
    current: Option<Cancel>,
    note: String,
    resized: u64,
    skipped: u64,
    last_skip: Option<&'static str>,
    reconnects: u64,
    finished: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    wake: Arc<Wake>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn jitter(delay: Duration) -> Duration {
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    // 0.8 .. 1.2
    delay.mul_f64(0.8 + byte[0] as f64 / 255.0 * 0.4)
}

struct Pump {
    shared: Arc<Shared>,
    factory: Arc<dyn SourceFactory>,
    meta: Meta,
    epoch: Instant,
}

impl Pump {
    /// The pump thread: read frames while there is demand, reconnect with
    /// backoff, stop on a permanent failure.
    fn run(self, mut session: Option<(Box<dyn FrameSession>, Cx)>) {
        let mut attempts: u32 = 0;
        let mut next_attempt = Instant::now();
        let mut healthy_since = session.as_ref().map(|_| Instant::now());
        loop {
            let (running, demand, parked) = {
                let state = self.shared.lock();
                if state.stop {
                    break;
                }
                let parked = state.park_until.is_some_and(|until| Instant::now() < until);
                (state.running, state.demand.is_some(), parked)
            };
            let continuous = session
                .as_ref()
                .map_or(self.factory.stream() != Stream::Snapshot, |(session, _)| {
                    session.continuous()
                });
            if !running && parked && continuous {
                // Keep the established session ready for start, but do not
                // enter a potentially 120-second MJPEG read while parked:
                // that would prevent closing it when PARK expires.
                self.wait_parked();
                continue;
            }
            // Without acquisition, a stream is kept only while parked after
            // connecting, and a snapshot source polls only when asked.
            if !running && !(parked && continuous) && !(demand && !continuous) {
                session = None;
                self.idle();
                continue;
            }
            let Some((current, cx)) = session.as_mut() else {
                if Instant::now() < next_attempt {
                    self.sleep_until(next_attempt);
                    continue;
                }
                let cx = Cx::with_wake(self.shared.wake.clone());
                {
                    let mut state = self.shared.lock();
                    // Stop may have won the race since the loop checked
                    // demand. Register cancellation while holding the same
                    // lock stop uses, before starting any network work.
                    let parked = state.park_until.is_some_and(|until| Instant::now() < until);
                    if state.stop
                        || (!state.running
                            && !(parked && continuous)
                            && !(state.demand.is_some() && !continuous))
                    {
                        continue;
                    }
                    state.current = Some(cx.cancel.clone());
                }
                match self.factory.open(&cx) {
                    Ok(opened) => {
                        session = Some((opened, cx));
                        healthy_since = Some(Instant::now());
                        let mut state = self.shared.lock();
                        state.note = format!("streaming ({})", self.factory.stream().name());
                    }
                    Err(failure) => {
                        if cx.cancelled() {
                            continue;
                        }
                        if !self.failed(failure, &mut attempts, &mut next_attempt) {
                            break;
                        }
                    }
                }
                continue;
            };
            match current.next(cx) {
                Ok(Next::Frame(frame)) => {
                    if cx.cancelled() {
                        session = None;
                        continue;
                    }
                    if healthy_since.is_some_and(|since| since.elapsed() >= HEALTHY) {
                        attempts = 0;
                    }
                    self.publish(frame, cx);
                }
                Ok(Next::Skip(reason)) => {
                    let mut state = self.shared.lock();
                    state.skipped += 1;
                    state.last_skip = Some(reason);
                }
                Err(failure) => {
                    let cancelled = cx.cancelled();
                    session = None;
                    healthy_since = None;
                    if cancelled {
                        continue;
                    }
                    if !self.failed(failure, &mut attempts, &mut next_attempt) {
                        break;
                    }
                }
            }
        }
        let mut state = self.shared.lock();
        state.finished = true;
        state.current = None;
        self.shared.changed.notify_all();
    }

    /// Record a failure; false when the pump must end.
    fn failed(&self, failure: Failure, attempts: &mut u32, next_attempt: &mut Instant) -> bool {
        if failure.permanent {
            if let (Some(status), Some((store, name, digest))) =
                (failure.auth_status, &self.meta.latch)
            {
                let _ = store.mark_rejected(name, digest, status);
            }
            let mut state = self.shared.lock();
            state.note = format!("stopped: {}", failure.message);
            state.failure = Some((failure.kind, failure.message));
            self.shared.changed.notify_all();
            return false;
        }
        let mut state = self.shared.lock();
        state.reconnects += 1;
        if failure.quiet {
            *next_attempt = Instant::now();
            state.note = format!("reconnecting: {}", failure.message);
            return true;
        }
        *attempts = attempts.saturating_add(1);
        let backoff = BACKOFF_MIN
            .saturating_mul(1u32 << (*attempts - 1).min(5))
            .min(BACKOFF_MAX);
        let delay = failure
            .retry_after
            .map(|wait| wait.min(RETRY_AFTER_MAX))
            .unwrap_or_else(|| jitter(backoff));
        *next_attempt = Instant::now() + delay;
        state.note = format!(
            "reconnecting (attempt {attempts}, next in {} s): {}",
            delay.as_secs().max(1),
            failure.message
        );
        true
    }

    fn publish(&self, mut frame: Frame, cx: &Cx) {
        let mut state = self.shared.lock();
        // stop cancels under this lock. Recheck here so a frame finishing
        // concurrently cannot repopulate the cleared slot after stop/start.
        if cx.cancelled() {
            return;
        }
        if self.factory.stream() == Stream::Snapshot
            && cx.snapshot_generation.load(Ordering::SeqCst)
                != self.shared.wake.request_generation.load(Ordering::SeqCst)
        {
            // This GET began before the latest capture request. Keep its
            // demand pending and bypass the next polling delay.
            self.shared.wake.wake();
            return;
        }
        if state.running {
            let size = (frame.width, frame.height);
            match state.size {
                None => state.size = Some(size),
                Some(locked) if locked != size => {
                    state.resized += 1;
                    return;
                }
                Some(_) => {}
            }
        }
        state.seq += 1;
        frame.id = state.seq;
        frame.timestamp_ns = self.epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let seq = state.seq;
        state.latest = Some((seq, Instant::now(), frame));
        self.shared.changed.notify_all();
    }

    fn idle(&self) {
        let mut state = self.shared.lock();
        state.current = None;
        if !state.stop && !state.running && state.demand.is_none() {
            let _ = self
                .shared
                .changed
                .wait_timeout(state, Duration::from_millis(500))
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn wait_parked(&self) {
        let state = self.shared.lock();
        if !state.stop
            && !state.running
            && let Some(until) = state.park_until
        {
            let left = until.saturating_duration_since(Instant::now());
            let _ = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn sleep_until(&self, at: Instant) {
        let state = self.shared.lock();
        if state.stop {
            return;
        }
        let left = at.saturating_duration_since(Instant::now());
        let _ = self
            .shared
            .changed
            .wait_timeout(state, left.min(Duration::from_millis(500)))
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// A hub camera as the engine sees it: read-only RGB8 frames.
pub(crate) struct HubBackend {
    meta: Meta,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    stream: Stream,
    fps: f64,
    width: u32,
    height: u32,
    running: bool,
    delivered: u64,
}

impl HubBackend {
    /// Try each source in order until one delivers a frame within
    /// `first_frame`, or the source's own floor if that is longer. An authentication or certificate failure ends the
    /// search: the next source would send the same secret.
    pub(crate) fn connect(
        candidates: Vec<Arc<dyn SourceFactory>>,
        first_frame: Duration,
        meta: Meta,
    ) -> Result<Self> {
        let mut last = None;
        for factory in candidates {
            let limit = first_frame.max(factory.first_frame());
            match first(factory.as_ref(), limit, &meta.id) {
                Ok((session, cx, frame)) => {
                    // Clear the old refusal before the pump can observe and
                    // record a new one. Clearing it after spawning the pump
                    // could erase an immediate 401 from the next frame.
                    if let Some((store, name, digest)) = &meta.latch {
                        let _ = store.clear_rejected(name, digest);
                    }
                    // A thread failure is infrastructure failure, not a
                    // reason to try another stream with the same camera.
                    return Self::start_pump(factory, session, cx, frame, meta);
                }
                Err(failure) => {
                    if let (Some(status), Some((store, name, digest))) =
                        (failure.auth_status, &meta.latch)
                    {
                        let _ = store.mark_rejected(name, digest, status);
                    }
                    if !failure.fallback {
                        return Err(failure.into_error());
                    }
                    last = Some(failure);
                }
            }
        }
        Err(last
            .map(Failure::into_error)
            .unwrap_or_else(|| anyhow::anyhow!("hub camera {} offers no stream", meta.id)))
    }

    fn start_pump(
        factory: Arc<dyn SourceFactory>,
        session: Box<dyn FrameSession>,
        cx: Cx,
        frame: Frame,
        meta: Meta,
    ) -> Result<Self> {
        Self::start_pump_with(factory, session, cx, frame, meta, |work| {
            std::thread::Builder::new()
                .name("capturefab-hub-pump".into())
                .spawn(work)
        })
    }

    fn start_pump_with(
        factory: Arc<dyn SourceFactory>,
        session: Box<dyn FrameSession>,
        cx: Cx,
        mut frame: Frame,
        meta: Meta,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>,
    ) -> Result<Self> {
        let wake = cx.wake.clone();
        let (width, height) = (frame.width, frame.height);
        let epoch = Instant::now();
        frame.id = 1;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                seq: 1,
                latest: Some((1, Instant::now(), frame)),
                park_until: Some(Instant::now() + PARK),
                current: Some(cx.cancel.clone()),
                note: format!("connected ({})", factory.stream().name()),
                ..State::default()
            }),
            changed: Condvar::new(),
            wake,
        });
        let pump = Pump {
            shared: shared.clone(),
            factory: factory.clone(),
            meta: meta.clone(),
            epoch,
        };
        let thread = spawn(Box::new(move || pump.run(Some((session, cx))))).map_err(|_| {
            Categorized::error(
                ErrorKind::OperationFailed,
                "cannot start the hub camera frame pump",
            )
        })?;
        Ok(Self {
            meta,
            shared,
            thread: Some(thread),
            stream: factory.stream(),
            fps: factory.fps(),
            width,
            height,
            running: false,
            delivered: 0,
        })
    }

    fn shutdown(&mut self) {
        {
            let mut state = self.shared.lock();
            state.stop = true;
            if let Some(cancel) = state.current.take() {
                cancel.cancel();
            }
        }
        self.shared.changed.notify_all();
        self.shared.wake.wake();
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + JOIN;
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
            // Otherwise the pump is still leaving a network call; it has its
            // own state and finishes on its own.
        }
    }
}

/// Read until the first frame, cancelling the session at `limit`.
fn first(
    factory: &dyn SourceFactory,
    limit: Duration,
    id: &str,
) -> Result<(Box<dyn FrameSession>, Cx, Frame), Failure> {
    let cx = Cx::new();
    let armed = Arc::new(Mutex::new(true));
    let expired = Arc::new(Mutex::new(false));
    {
        let (armed, expired, cancel) = (armed.clone(), expired.clone(), cx.cancel.clone());
        let deadline = Instant::now() + limit;
        std::thread::spawn(move || {
            while Instant::now() < deadline {
                std::thread::sleep(
                    Duration::from_millis(20)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
                if !*armed.lock().unwrap_or_else(PoisonError::into_inner) {
                    return;
                }
            }
            let armed = armed.lock().unwrap_or_else(PoisonError::into_inner);
            if *armed {
                *expired.lock().unwrap_or_else(PoisonError::into_inner) = true;
                cancel.cancel();
            }
        });
    }
    let timeout = || {
        Failure::transient(
            ErrorKind::Timeout,
            format!(
                "timed out waiting for the first frame from {id} after {} ms; raise --timeout-ms or try stream=snapshot",
                limit.as_millis()
            ),
        )
    };
    let result = (|| {
        let mut session = factory.open(&cx)?;
        loop {
            match session.next(&cx)? {
                Next::Frame(frame) => return Ok(Some((session, frame))),
                Next::Skip(_) if cx.cancelled() => return Ok(None),
                Next::Skip(_) => {}
            }
        }
    })();
    let mut still_armed = armed.lock().unwrap_or_else(PoisonError::into_inner);
    *still_armed = false;
    drop(still_armed);
    let expired = *expired.lock().unwrap_or_else(PoisonError::into_inner);
    match result {
        Ok(Some((session, frame))) if !expired => Ok((session, cx, frame)),
        Ok(_) => Err(timeout()),
        Err(_) if expired => Err(timeout()),
        Err(failure) => Err(failure),
    }
}

impl RegisterIo for HubBackend {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        crate::media::read_only_registers(
            self.width,
            self.height,
            self.running,
            self.fps,
            address,
            length,
        )
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        // AcquisitionStart and AcquisitionStop.
        if address == 16 && data.len() == 4 {
            return Ok(());
        }
        anyhow::bail!(
            "hub camera settings are read-only; change them in {}",
            self.meta.product
        )
    }
}

impl Backend for HubBackend {
    fn xml(&mut self) -> Result<String> {
        Ok(crate::media::read_only_xml())
    }
    fn start(&mut self, _payload_size: usize) -> Result<()> {
        let mut state = self.shared.lock();
        if let Some((kind, message)) = &state.failure {
            return Err(Categorized::error(*kind, message.clone()));
        }
        state.running = true;
        state.park_until = None;
        state.size = None;
        if let Some((seq, at, _)) = &state.latest
            && at.elapsed() > STALE
        {
            self.delivered = *seq;
        }
        drop(state);
        self.shared.changed.notify_all();
        self.running = true;
        Ok(())
    }
    fn request_frame(&mut self) -> Result<()> {
        if self.stream == Stream::Snapshot {
            let mut state = self.shared.lock();
            self.shared
                .wake
                .request_generation
                .fetch_add(1, Ordering::SeqCst);
            let seq = state.seq;
            state.demand = Some(state.demand.map_or(seq, |pending| pending.max(seq)));
            drop(state);
            self.shared.changed.notify_all();
            self.shared.wake.wake();
        }
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        anyhow::ensure!(self.running, "hub acquisition has not started");
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        loop {
            if let Some((kind, message)) = &state.failure {
                return Err(Categorized::error(*kind, message.clone()));
            }
            let fresh = state.latest.as_ref().is_some_and(|(seq, _, _)| {
                *seq > self.delivered && state.demand.is_none_or(|demand| *seq > demand)
            });
            if fresh && let Some((seq, _, frame)) = state.latest.take() {
                self.delivered = seq;
                if state.demand.is_some_and(|demand| seq > demand) {
                    state.demand = None;
                }
                self.width = frame.width;
                self.height = frame.height;
                return Ok(frame);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(FrameWait.into());
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        let mut state = self.shared.lock();
        state.running = false;
        state.park_until = None;
        state.demand = None;
        state.latest = None;
        if let Some(cancel) = state.current.take() {
            cancel.cancel();
        }
        drop(state);
        self.shared.changed.notify_all();
        self.shared.wake.wake();
        Ok(())
    }
    fn stats(&self) -> Option<TransportStats> {
        let state = self.shared.lock();
        let mut notes = vec![
            format!("mode: {}", self.stream.name()),
            format!("state: {}", state.note),
        ];
        if state.reconnects > 0 {
            notes.push(format!("reconnects: {}", state.reconnects));
        }
        if state.skipped > 0 {
            notes.push(format!(
                "images skipped: {} ({})",
                state.skipped,
                state.last_skip.unwrap_or("unusable")
            ));
        }
        if state.resized > 0 {
            notes.push(format!(
                "frames of another size dropped: {} (stop and start acquisition to adopt the new size)",
                state.resized
            ));
        }
        Some(TransportStats {
            notes,
            ..Default::default()
        })
    }
}

impl Drop for HubBackend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests;
