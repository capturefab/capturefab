//! The coordinator's signal hub: starts and stops source helpers, keeps the
//! latest values per tag and hands samples to the attached `SampleSink`.
//!
//! Each source has a reader thread for its helper's stdout. The shared sink
//! and each local private route have independent writer threads and ordered
//! queues. A shared sink swap follows its preceding batches; private routes
//! keep their original sink. Batches and registrations use `try_send`: a full
//! queue (or one holding `QUEUE_BYTES`) counts drops instead of blocking a
//! reader, and registrations wait in the reader until there is room.
use super::{
    CaptureCompletion, CaptureLimits, Line, MAX_SOURCES, MAX_TAGS, Mode, Sample, SampleSink,
    SignalInfo, SignalSpec, SourceState, SourceStatus, TagInfo, TagStatus, parse,
    process::{SignalProcess, read_line},
};
use crate::session::{LogEntry, SessionCommand};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::BufRead,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[cfg(test)]
mod private_tests;
mod route;
pub(crate) use route::PrivateRoute;

#[derive(Clone, Default)]
enum PersistenceRoute {
    #[default]
    Shared,
    Private(PrivateRoute),
}

/// Recent numeric points kept per tag for sparklines.
pub const RECENT: usize = 120;
/// Hub queue depth, in messages.
const QUEUE: usize = 1024;
/// Approximate memory the queued batches may hold before batches are
/// dropped (and counted), so a stalled sink cannot hold hundreds of MiB.
const QUEUE_BYTES: u64 = 64 << 20;
/// Largest batch handed to the sink at once.
const SINK_BATCH: usize = 5000;
/// How long the writer coalesces a source's samples.
const COALESCE: Duration = Duration::from_millis(500);
/// Helper restarts per source before it is marked failed.
const MAX_RESTARTS: u64 = 5;
/// A helper that ran this long before exiting starts a fresh restart count.
const STABLE_RUN: Duration = Duration::from_secs(600);
/// Log entries kept.
const MAX_LOGS: usize = 50;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

enum HubMsg {
    /// Samples and their approximate size in memory.
    Batch(Arc<Source>, Vec<Sample>, u64),
    Register(Arc<Source>, Vec<TagInfo>),
    Event(&'static str, Arc<Source>, String),
    Flush(mpsc::Sender<Result<()>>),
    FlushCapture(String, mpsc::Sender<Result<()>>),
    FinishPrivate(mpsc::Sender<Result<()>>),
    SetSink(Option<Arc<dyn SampleSink>>, mpsc::Sender<Result<()>>),
}

struct TagState {
    info: TagInfo,
    last: Option<Sample>,
    recent: VecDeque<(u64, f64)>,
}

struct Phase {
    state: SourceState,
    error: Option<String>,
    /// The first ready, state or exit has been seen.
    settled: bool,
}

#[derive(Clone, Default)]
struct CaptureProgress {
    closed: bool,
    reason: Option<CaptureCompletion>,
    completed_ms: Option<u64>,
    persistence_complete: bool,
    error: Option<String>,
}

#[cfg(test)]
struct ReaderGate {
    entered: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

struct Source {
    info: SignalInfo,
    auth: Option<super::auth::AuthBinding>,
    capture_id: String,
    limits: Option<CaptureLimits>,
    deadline: Option<Instant>,
    sink_generation: Option<u64>,
    dataset_slot: Option<String>,
    persistence: PersistenceRoute,
    capture: Mutex<CaptureProgress>,
    finishing: AtomicBool,
    spec: SignalSpec,
    started_ms: u64,
    process: Mutex<Option<SignalProcess>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    reader_done: AtomicBool,
    #[cfg(test)]
    reader_line_gate: Mutex<Option<ReaderGate>>,
    #[cfg(test)]
    reader_restart_gate: Mutex<Option<ReaderGate>>,
    phase: Mutex<Phase>,
    changed: Condvar,
    stop: AtomicBool,
    samples: AtomicU64,
    payload_bytes: AtomicU64,
    written: AtomicU64,
    written_bytes: AtomicU64,
    dropped: AtomicU64,
    helper_dropped: AtomicU64,
    restarts: AtomicU64,
    last_sample_ms: AtomicU64,
    helper_pid: AtomicU64,
    tags: Mutex<BTreeMap<String, TagState>>,
}

impl Source {
    fn queue(&self, hub: &Arc<HubInner>) -> SyncSender<HubMsg> {
        match &self.persistence {
            PersistenceRoute::Shared => hub.queue(),
            PersistenceRoute::Private(route) => route.queue(),
        }
    }
    fn has_sink(&self, hub: &HubInner) -> bool {
        match &self.persistence {
            PersistenceRoute::Shared => hub.has_sink.load(Ordering::SeqCst),
            PersistenceRoute::Private(route) => route.has_sink(),
        }
    }
    fn reserve(&self, hub: &HubInner, bytes: u64) -> bool {
        match &self.persistence {
            PersistenceRoute::Shared => reserve_queue(hub, bytes),
            PersistenceRoute::Private(route) => route.reserve(bytes),
        }
    }
    fn unreserve(&self, hub: &HubInner, bytes: u64) {
        match &self.persistence {
            PersistenceRoute::Shared => {
                hub.queued_bytes.fetch_sub(bytes, Ordering::SeqCst);
            }
            PersistenceRoute::Private(route) => route.unreserve(bytes),
        }
    }
    fn count_drops(&self, hub: Option<&HubInner>, count: u64) {
        self.dropped.fetch_add(count, Ordering::SeqCst);
        match &self.persistence {
            PersistenceRoute::Shared => {
                if let Some(hub) = hub {
                    hub.dropped_total.fetch_add(count, Ordering::SeqCst);
                }
            }
            PersistenceRoute::Private(route) => route.dropped(count),
        }
    }
    fn sink_error(&self, hub: Option<&HubInner>, message: String) {
        match &self.persistence {
            PersistenceRoute::Shared => {
                if let Some(hub) = hub {
                    *lock(&hub.sink_error) = Some(message);
                }
            }
            PersistenceRoute::Private(route) => route.error(message),
        }
    }
    fn flush(&self, hub: &Arc<HubInner>, timeout: Duration) -> Result<()> {
        let make = |ack| HubMsg::FlushCapture(self.capture_id.clone(), ack);
        match &self.persistence {
            PersistenceRoute::Shared => hub.request(make, timeout, "capture flush"),
            PersistenceRoute::Private(route) => route.request(make, timeout, "capture flush"),
        }
    }
    #[cfg(test)]
    fn wait_reader_gate(gate: &Mutex<Option<ReaderGate>>) {
        let gate = lock(gate).take();
        if let Some(gate) = gate {
            gate.entered.send(()).expect("reader gate observer");
            gate.resume
                .recv_timeout(Duration::from_secs(5))
                .expect("reader gate released within five seconds");
        }
    }
    fn redact(&self, text: &str) -> String {
        let password = parse(&self.spec.uri).ok().and_then(|uri| uri.password);
        let text = super::redact_text(text, &self.spec.uri, password.as_deref());
        lock(&self.process)
            .as_ref()
            .map_or_else(|| text.clone(), |process| process.redact(&text))
    }
    fn request_completion(&self, reason: CaptureCompletion, error: Option<String>) {
        let mut capture = lock(&self.capture);
        if capture.completed_ms.is_some() {
            return;
        }
        if !capture.closed {
            capture.closed = true;
            capture.reason = Some(reason);
        }
        if error.is_some() {
            capture.error = error;
        }
        self.stop.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }
    fn terminal_failure(&self, error: String) {
        if self.limits.is_some() {
            self.request_completion(CaptureCompletion::Failed, Some(error));
        } else {
            self.set_phase(SourceState::Failed, Some(error));
        }
    }
    fn set_phase(&self, state: SourceState, error: Option<String>) {
        let capture = lock(&self.capture);
        self.set_phase_with_capture(&capture, state, error);
    }
    fn set_phase_with_capture(
        &self,
        capture: &CaptureProgress,
        state: SourceState,
        error: Option<String>,
    ) {
        if capture.completed_ms.is_some()
            && !matches!(state, SourceState::Stopped | SourceState::Failed)
        {
            return;
        }
        let mut phase = lock(&self.phase);
        phase.state = state;
        if error.is_some() || state == SourceState::Running {
            phase.error = error;
        }
        phase.settled = true;
        self.changed.notify_all();
    }
    /// Metadata and restart bookkeeping share the same admission boundary as
    /// samples. Never hold it across helper I/O, credential lookup or sleep.
    fn admit_metadata(&self, apply: impl FnOnce(&mut CaptureProgress)) -> bool {
        let mut capture = lock(&self.capture);
        if capture.closed {
            return false;
        }
        apply(&mut capture);
        true
    }
    fn queue_event(
        self: &Arc<Self>,
        hub: &HubInner,
        queue: &SyncSender<HubMsg>,
        capture: &mut CaptureProgress,
        kind: &'static str,
        text: String,
    ) {
        if queue
            .try_send(HubMsg::Event(kind, self.clone(), text))
            .is_err()
            && matches!(self.persistence, PersistenceRoute::Private(_))
        {
            let message = "private signal event could not reach its persistence queue".to_string();
            self.sink_error(Some(hub), message.clone());
            capture.error = Some(message);
            capture.closed = true;
            capture.reason = Some(CaptureCompletion::Failed);
            self.stop.store(true, Ordering::SeqCst);
        }
    }
    fn tag_infos(&self) -> Vec<TagInfo> {
        lock(&self.tags).values().map(|t| t.info.clone()).collect()
    }
    fn status(&self, max_tags: usize, history: bool) -> SourceStatus {
        let capture = lock(&self.capture).clone();
        let phase = lock(&self.phase);
        let (state, error) = (phase.state, phase.error.clone());
        drop(phase);
        let tags = lock(&self.tags);
        let with_values: Vec<&TagState> = tags.values().filter(|t| t.last.is_some()).collect();
        let total = with_values.len();
        let list = with_values
            .into_iter()
            .take(max_tags)
            .filter_map(|t| {
                let last = t.last.as_ref()?;
                Some(TagStatus {
                    tag: t.info.tag.clone(),
                    unit: t.info.unit.clone(),
                    value: last.value.clone(),
                    quality: last.quality,
                    ts_ms: last.ts_ms,
                    source_ts_ms: last.source_ts_ms,
                    recent: history.then(|| t.recent.iter().copied().collect()),
                })
            })
            .collect();
        let pid = self.helper_pid.load(Ordering::Relaxed);
        let last = self.last_sample_ms.load(Ordering::Relaxed);
        SourceStatus {
            id: self.info.id.clone(),
            auth: self.auth.clone(),
            capture_id: self.capture_id.clone(),
            dataset_slot_id: self.dataset_slot.clone(),
            protocol: self.info.protocol,
            uri: self.info.uri.clone(),
            interval_ms: self.info.interval_ms,
            state,
            started_ms: self.started_ms,
            capture_limits: self.limits,
            deadline_ms: self
                .limits
                .map(|limits| self.started_ms.saturating_add(limits.duration_ms)),
            completed_ms: capture.completed_ms,
            completion: capture.completed_ms.and(capture.reason),
            payload_bytes: self.payload_bytes.load(Ordering::Relaxed),
            written_bytes: self.written_bytes.load(Ordering::Relaxed),
            persistence_complete: capture.persistence_complete,
            persistence_error: capture.error.clone(),
            last_sample_ms: (last > 0).then_some(last),
            samples: self.samples.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            helper_dropped: self.helper_dropped.load(Ordering::Relaxed),
            restarts: self.restarts.load(Ordering::Relaxed),
            error,
            helper_pid: (pid > 0 && !matches!(state, SourceState::Stopped | SourceState::Failed))
                .then_some(pid as u32),
            tags: list,
            tags_total: total as u64,
            tags_truncated: total > max_tags,
        }
    }
    /// Merge a batch into the latest values and recent points.
    fn absorb(&self, samples: &[Sample]) {
        let mut tags = lock(&self.tags);
        for sample in samples {
            if !tags.contains_key(&sample.tag) {
                if tags.len() >= MAX_TAGS {
                    continue;
                }
                tags.insert(
                    sample.tag.clone(),
                    TagState {
                        info: TagInfo {
                            tag: sample.tag.clone(),
                            unit: None,
                            kind: Some(sample.value.kind().into()),
                        },
                        last: None,
                        recent: VecDeque::new(),
                    },
                );
            }
            let Some(state) = tags.get_mut(&sample.tag) else {
                continue;
            };
            if let Some(x) = sample.value.as_f64() {
                if state.recent.len() >= RECENT {
                    state.recent.pop_front();
                }
                state.recent.push_back((sample.ts_ms, x));
            }
            state.last = Some(sample.clone());
        }
        if let Some(last) = samples.iter().map(|s| s.ts_ms).max() {
            self.last_sample_ms.fetch_max(last, Ordering::Relaxed);
        }
        self.samples
            .fetch_add(samples.len() as u64, Ordering::Relaxed);
    }
    fn merge_tags(&self, infos: &[TagInfo]) {
        let mut tags = lock(&self.tags);
        for info in infos {
            if let Some(state) = tags.get_mut(&info.tag) {
                state.info = info.clone();
            } else if tags.len() < MAX_TAGS {
                tags.insert(
                    info.tag.clone(),
                    TagState {
                        info: info.clone(),
                        last: None,
                        recent: VecDeque::new(),
                    },
                );
            }
        }
    }
}

struct HubInner {
    auth: super::auth::Session,
    sources: Mutex<BTreeMap<String, Arc<Source>>>,
    queue: OnceLock<SyncSender<HubMsg>>,
    has_sink: AtomicBool,
    sink_lifecycle: Mutex<()>,
    sink_generation: AtomicU64,
    current_sink: Mutex<Option<Arc<dyn SampleSink>>>,
    private_routes: Mutex<BTreeMap<u64, Arc<route::PrivateInner>>>,
    dropped_total: AtomicU64,
    /// Approximate bytes held by queued batches.
    queued_bytes: AtomicU64,
    sink_error: Mutex<Option<String>>,
    logs: Mutex<VecDeque<LogEntry>>,
    quitting: AtomicBool,
}

impl HubInner {
    fn log(&self, level: &str, message: String) {
        let mut logs = lock(&self.logs);
        if logs.len() >= MAX_LOGS {
            logs.pop_front();
        }
        logs.push_back(LogEntry::now_local(level, message));
    }
    /// The writer's queue, starting the writer thread on first use.
    fn queue(self: &Arc<Self>) -> SyncSender<HubMsg> {
        self.queue
            .get_or_init(|| {
                let (sender, receiver) = mpsc::sync_channel(QUEUE);
                let hub = Arc::downgrade(self);
                thread::Builder::new()
                    .name("capturefab-signal-writer".into())
                    .spawn(move || writer(hub, receiver, None))
                    .expect("start signal writer");
                sender
            })
            .clone()
    }
    /// Send a message that the writer acknowledges, and wait for the ack.
    fn request(
        self: &Arc<Self>,
        make: impl FnOnce(mpsc::Sender<Result<()>>) -> HubMsg,
        timeout: Duration,
        what: &str,
    ) -> Result<()> {
        request_queue(&self.queue(), make, timeout, what)
    }
}

fn send_queue(queue: &SyncSender<HubMsg>, mut message: HubMsg, deadline: Instant) -> Result<()> {
    loop {
        match queue.try_send(message) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(m)) if Instant::now() < deadline => {
                message = m;
                thread::sleep(Duration::from_millis(5));
            }
            Err(TrySendError::Full(_)) => bail!("signal sink queue timed out"),
            Err(TrySendError::Disconnected(_)) => bail!("signal writer has stopped"),
        }
    }
}
fn request_queue(
    queue: &SyncSender<HubMsg>,
    make: impl FnOnce(mpsc::Sender<Result<()>>) -> HubMsg,
    timeout: Duration,
    what: &str,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let (ack, done) = mpsc::channel();
    send_queue(queue, make(ack), deadline)?;
    match done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(_) => bail!("signal {what} timed out after {} ms", timeout.as_millis()),
    }
}

/// The writer thread: owns the sink and coalesces batches per source.
struct Writer {
    hub: Weak<HubInner>,
    sink: Option<Arc<dyn SampleSink>>,
    pending: BTreeMap<String, (Arc<Source>, Vec<Sample>, Instant)>,
    last_error_log: Option<Instant>,
    private: Option<Weak<route::PrivateInner>>,
    private_finished: Option<Option<String>>,
}

impl Writer {
    fn write_capture(&mut self, capture_id: &str) {
        if let Some((source, samples, _)) = self.pending.remove(capture_id) {
            self.write(&source, &samples);
        }
    }
    fn finish_private(&mut self) -> Result<()> {
        if self.private.is_none() {
            bail!("not a private signal writer");
        }
        if self.private_finished.is_none() {
            self.write_due(true);
            let result = self.sink.take().map_or(Ok(()), |sink| sink.finish());
            self.private_finished = Some(result.err().map(|error| format!("{error:#}")));
        }
        match self.private_finished.as_ref().and_then(Option::as_ref) {
            Some(error) => bail!("{error}"),
            None => Ok(()),
        }
    }
    fn next_due(&self) -> Duration {
        self.pending
            .values()
            .map(|(_, _, since)| (*since + COALESCE).saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_secs(3600))
    }
    /// Write sources that are full or old enough, or all of them.
    fn write_due(&mut self, all: bool) {
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, (_, samples, since))| {
                all || samples.len() >= SINK_BATCH || since.elapsed() >= COALESCE
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if let Some((source, samples, _)) = self.pending.remove(&id) {
                self.write(&source, &samples);
            }
        }
    }
    fn write(&mut self, source: &Source, samples: &[Sample]) {
        let Some(sink) = self.sink.clone() else {
            if source.limits.is_some() {
                self.drop_samples(
                    source,
                    samples.len() as u64,
                    "signal capture sink was detached".into(),
                );
            }
            return;
        };
        if source.sink_generation.is_some_and(|generation| {
            self.hub
                .upgrade()
                .is_none_or(|hub| hub.sink_generation.load(Ordering::SeqCst) != generation)
        }) {
            self.drop_samples(
                source,
                samples.len() as u64,
                "signal capture sink was replaced".into(),
            );
            return;
        }
        for chunk in samples.chunks(SINK_BATCH) {
            let count = chunk.len() as u64;
            let result = match &source.dataset_slot {
                Some(slot) => sink.write_for_dataset_slot(slot, &source.info.id, chunk),
                None => sink.write(&source.info.id, chunk),
            };
            match result {
                Ok(()) => {
                    source.written.fetch_add(count, Ordering::Relaxed);
                    source
                        .written_bytes
                        .fetch_add(payload_bytes(chunk), Ordering::Relaxed);
                }
                Err(error) => {
                    let hub = self.hub.upgrade();
                    source.count_drops(hub.as_deref(), count);
                    let message = source.redact(&format!("{error:#}"));
                    if source.limits.is_some() {
                        source.request_completion(CaptureCompletion::Failed, Some(message.clone()));
                    }
                    source.sink_error(hub.as_deref(), message.clone());
                    if let Some(hub) = hub
                        && self
                            .last_error_log
                            .is_none_or(|t| t.elapsed() >= Duration::from_secs(10))
                    {
                        self.last_error_log = Some(Instant::now());
                        hub.log(
                            "error",
                            format!("Signal sink {} failed: {message}", sink.describe()),
                        );
                    }
                }
            }
        }
    }
    fn drop_samples(&self, source: &Source, count: u64, message: String) {
        let hub = self.hub.upgrade();
        source.count_drops(hub.as_deref(), count);
        source.sink_error(hub.as_deref(), message.clone());
        source.request_completion(CaptureCompletion::Failed, Some(message));
    }
    fn set_sink(&mut self, sink: Option<Arc<dyn SampleSink>>) -> Result<()> {
        if self.private.is_some() {
            bail!("a private signal route cannot replace its sink");
        }
        self.write_due(true);
        let finished = match self.sink.take() {
            Some(old) => old.finish(),
            None => Ok(()),
        };
        self.sink = sink;
        if let Some(hub) = self.hub.upgrade() {
            let generation = {
                let mut current = lock(&hub.current_sink);
                let generation = hub.sink_generation.fetch_add(1, Ordering::SeqCst) + 1;
                *current = self.sink.clone();
                generation
            };
            hub.has_sink.store(self.sink.is_some(), Ordering::SeqCst);
            for source in lock(&hub.sources).values() {
                if !matches!(source.persistence, PersistenceRoute::Shared) {
                    continue;
                }
                if source
                    .sink_generation
                    .is_some_and(|bound| bound != generation)
                    && lock(&source.capture).completed_ms.is_none()
                {
                    source.request_completion(
                        CaptureCompletion::Failed,
                        Some("signal capture sink was replaced or detached".into()),
                    );
                }
            }
            if let Some(sink) = &self.sink {
                *lock(&hub.sink_error) = None;
                let sources: Vec<Arc<Source>> = lock(&hub.sources).values().cloned().collect();
                for source in sources {
                    if !matches!(source.persistence, PersistenceRoute::Shared) {
                        continue;
                    }
                    if source
                        .sink_generation
                        .is_some_and(|bound| bound != generation)
                    {
                        continue;
                    }
                    if let Err(error) = sink.register(&source.info, &source.tag_infos()) {
                        hub.log(
                            "error",
                            format!("Signal sink registration failed: {error:#}"),
                        );
                    }
                }
            }
        }
        finished
    }
}

fn writer(
    hub: Weak<HubInner>,
    receiver: mpsc::Receiver<HubMsg>,
    private: Option<(Weak<route::PrivateInner>, Arc<dyn SampleSink>)>,
) {
    let (private, sink) = match private {
        Some((route, sink)) => (Some(route), Some(sink)),
        None => (None, None),
    };
    let mut writer = Writer {
        hub,
        sink,
        pending: BTreeMap::new(),
        last_error_log: None,
        private,
        private_finished: None,
    };
    loop {
        match receiver.recv_timeout(writer.next_due()) {
            Ok(HubMsg::Batch(source, samples, bytes)) => {
                if let Some(route) = writer.private.as_ref().and_then(Weak::upgrade) {
                    route.receive_bytes(bytes);
                } else if let Some(hub) = writer.hub.upgrade() {
                    hub.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
                }
                if writer.sink.is_some() || source.limits.is_some() {
                    writer
                        .pending
                        .entry(source.capture_id.clone())
                        .or_insert_with(|| (source, Vec::new(), Instant::now()))
                        .1
                        .extend(samples);
                }
            }
            Ok(HubMsg::Register(source, tags)) => {
                let matches_sink = source.sink_generation.is_none_or(|bound| {
                    writer
                        .hub
                        .upgrade()
                        .is_some_and(|hub| hub.sink_generation.load(Ordering::SeqCst) == bound)
                });
                if let Some(sink) = &writer.sink
                    && matches_sink
                    && let Err(error) = match &source.dataset_slot {
                        Some(slot) => sink.register_for_dataset_slot(slot, &source.info, &tags),
                        None => sink.register(&source.info, &tags),
                    }
                {
                    let hub = writer.hub.upgrade();
                    if source.limits.is_some() {
                        let message = source.redact(&format!("{error:#}"));
                        source.request_completion(CaptureCompletion::Failed, Some(message.clone()));
                        source.sink_error(hub.as_deref(), message);
                    }
                    if let Some(hub) = hub {
                        hub.log(
                            "error",
                            format!(
                                "Signal sink registration failed: {}",
                                source.redact(&format!("{error:#}"))
                            ),
                        );
                    }
                }
            }
            Ok(HubMsg::Event(kind, source, text)) => {
                if let Some(sink) = &writer.sink
                    && let Err(error) = sink.event(kind, &source.info.id, &text)
                    && writer.private.is_some()
                {
                    let message = source.redact(&format!("{error:#}"));
                    source.request_completion(CaptureCompletion::Failed, Some(message.clone()));
                    source.sink_error(writer.hub.upgrade().as_deref(), message);
                }
            }
            Ok(HubMsg::Flush(ack)) => {
                writer.write_due(true);
                let _ = ack.send(Ok(()));
            }
            Ok(HubMsg::FlushCapture(id, ack)) => {
                writer.write_capture(&id);
                let _ = ack.send(Ok(()));
            }
            Ok(HubMsg::FinishPrivate(ack)) => {
                let result = writer.finish_private();
                let _ = ack.send(result);
            }
            Ok(HubMsg::SetSink(sink, ack)) => {
                let result = writer.set_sink(sink);
                let _ = ack.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if writer.private.is_some() {
                    let _ = writer.finish_private();
                } else {
                    let _ = writer.set_sink(None);
                }
                return;
            }
        }
        writer.write_due(false);
    }
}

/// Approximate memory a batch holds while queued.
fn batch_bytes(samples: &[Sample]) -> u64 {
    samples
        .iter()
        .map(|s| {
            let text = match &s.value {
                super::Value::Text(text) => text.len(),
                _ => 0,
            };
            (std::mem::size_of::<Sample>() + s.tag.len() + text) as u64
        })
        .sum()
}

fn payload_bytes(samples: &[Sample]) -> u64 {
    samples
        .iter()
        .map(|sample| {
            serde_json::to_vec(sample)
                .expect("serializable signal sample")
                .len() as u64
        })
        .sum()
}

fn reserve_queue(hub: &HubInner, bytes: u64) -> bool {
    reserve_bytes(&hub.queued_bytes, bytes)
}
fn reserve_bytes(queued: &AtomicU64, bytes: u64) -> bool {
    let mut held = queued.load(Ordering::Relaxed);
    loop {
        let Some(next) = held.checked_add(bytes).filter(|next| *next <= QUEUE_BYTES) else {
            return false;
        };
        match queued.compare_exchange_weak(held, next, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => held = actual,
        }
    }
}

/// Serialize admission and queueing with capture completion. A final flush
/// therefore always follows every accepted batch, including a partial batch.
fn ingest(
    hub: &Arc<HubInner>,
    source: &Arc<Source>,
    queue: &SyncSender<HubMsg>,
    unregistered: &mut Vec<TagInfo>,
    mut samples: Vec<Sample>,
) {
    let mut capture = lock(&source.capture);
    if capture.closed {
        return;
    }
    let mut bytes = 0;
    let mut reason = None;
    if let Some(limits) = source.limits {
        if source
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            samples.clear();
            reason = Some(CaptureCompletion::Duration);
        } else {
            let left_samples = limits
                .max_samples
                .saturating_sub(source.samples.load(Ordering::Relaxed));
            let left_bytes = limits
                .max_bytes
                .saturating_sub(source.payload_bytes.load(Ordering::Relaxed));
            let mut accepted = 0;
            for sample in &samples {
                if accepted as u64 >= left_samples {
                    reason = Some(CaptureCompletion::MaxSamples);
                    break;
                }
                let size = payload_bytes(std::slice::from_ref(sample));
                if bytes + size > left_bytes {
                    reason = Some(CaptureCompletion::MaxBytes);
                    break;
                }
                bytes += size;
                accepted += 1;
            }
            samples.truncate(accepted);
            if reason.is_none() && accepted as u64 == left_samples {
                reason = Some(CaptureCompletion::MaxSamples);
            } else if reason.is_none() && bytes == left_bytes {
                reason = Some(CaptureCompletion::MaxBytes);
            }
        }
    } else {
        bytes = payload_bytes(&samples);
    }
    if !samples.is_empty() {
        source.absorb(&samples);
        source.payload_bytes.fetch_add(bytes, Ordering::Relaxed);
        if source.has_sink(hub) {
            let count = samples.len() as u64;
            let queue_bytes = batch_bytes(&samples);
            // Reserve before sending: the writer may receive immediately.
            let reserved = source.reserve(hub, queue_bytes);
            let sent = reserved
                && register(queue, source, unregistered)
                && queue
                    .try_send(HubMsg::Batch(source.clone(), samples, queue_bytes))
                    .is_ok();
            if !sent {
                if reserved {
                    source.unreserve(hub, queue_bytes);
                }
                source.count_drops(Some(hub), count);
                if source.limits.is_some() {
                    let message =
                        "signal capture samples dropped because the sink queue is full".to_string();
                    source.sink_error(Some(hub), message.clone());
                    capture.error = Some(message);
                    reason = Some(CaptureCompletion::Failed);
                }
            }
        } else if source.limits.is_some() {
            let count = samples.len() as u64;
            source.count_drops(Some(hub), count);
            capture.error = Some("signal capture sink was detached".into());
            reason = Some(CaptureCompletion::Failed);
        }
    }
    if let Some(reason) = reason {
        capture.closed = true;
        capture.reason = Some(reason);
        source.stop.store(true, Ordering::SeqCst);
    }
}

/// Finite acquisition is owned by the coordinator, independently of the
/// client and reader. A blocked helper cannot keep admitting samples.
fn supervise_capture(hub: Arc<HubInner>, source: Arc<Source>) {
    loop {
        if lock(&source.capture).completed_ms.is_some() {
            return;
        }
        if source
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            source.request_completion(CaptureCompletion::Duration, None);
        }
        if hub.quitting.load(Ordering::SeqCst) {
            source.request_completion(CaptureCompletion::Stopped, None);
        }
        if lock(&source.capture).closed {
            finish_capture(&hub, &source, Duration::from_secs(3));
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn finish_capture(hub: &Arc<HubInner>, source: &Arc<Source>, wait: Duration) {
    if source.finishing.swap(true, Ordering::SeqCst) {
        return;
    }
    let deadline = Instant::now() + wait;
    if let Some(mut process) = lock(&source.process).take() {
        process.request_stop();
        process.wait_or_kill(deadline.min(Instant::now() + Duration::from_secs(2)));
    }
    let result = source.flush(
        hub,
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1)),
    );
    let mut capture = lock(&source.capture);
    if let Err(error) = result {
        capture.error = Some(format!("{error:#}"));
    }
    capture.persistence_complete = capture.error.is_none()
        && source.written.load(Ordering::Relaxed) == source.samples.load(Ordering::Relaxed)
        && source.dropped.load(Ordering::Relaxed) == 0
        && source.helper_dropped.load(Ordering::Relaxed) == 0;
    if !capture.persistence_complete && capture.error.is_none() {
        capture.error = Some("signal capture did not persist every acquired sample".into());
    }
    capture.completed_ms = Some(crate::scheduling::now_ms());
    let failed = capture.reason == Some(CaptureCompletion::Failed) || capture.error.is_some();
    let error = capture.error.clone();
    drop(capture);
    source.set_phase(
        if failed {
            SourceState::Failed
        } else {
            SourceState::Stopped
        },
        error,
    );
}

/// Queue tags waiting to be registered without blocking the reader. They
/// stay pending while the queue is full.
fn register(queue: &SyncSender<HubMsg>, source: &Arc<Source>, pending: &mut Vec<TagInfo>) -> bool {
    if pending.is_empty() {
        return true;
    }
    match queue.try_send(HubMsg::Register(source.clone(), std::mem::take(pending))) {
        Ok(()) => true,
        Err(TrySendError::Full(HubMsg::Register(_, tags))) => {
            *pending = tags;
            false
        }
        Err(_) => true,
    }
}

/// Follow one source's helper output; restart the helper after it exits
/// unexpectedly, up to `MAX_RESTARTS` times in a row.
fn read_source(hub: Arc<HubInner>, source: Arc<Source>, mut reader: Box<dyn BufRead + Send>) {
    let queue = source.queue(&hub);
    let mut unregistered: Vec<TagInfo> = Vec::new();
    let mut recent_restarts = 0u64;
    loop {
        let mut error: Option<String> = None;
        let started = Instant::now();
        loop {
            let mut line = read_line(&mut reader);
            if let Ok(Some(line)) = &mut line
                && let Some(process) = lock(&source.process).as_ref()
            {
                process.scrub_line(line);
            }
            if source.stop.load(Ordering::SeqCst) {
                break;
            }
            #[cfg(test)]
            Source::wait_reader_gate(&source.reader_line_gate);
            match line {
                Ok(Some(Line::Samples { samples })) => {
                    if samples.is_empty() {
                        continue;
                    }
                    ingest(&hub, &source, &queue, &mut unregistered, samples);
                    if source.stop.load(Ordering::SeqCst) {
                        break;
                    }
                }
                Ok(Some(Line::Ready { tags })) => {
                    if !source.admit_metadata(|capture| {
                        source.merge_tags(&tags);
                        if lock(&source.phase).state != SourceState::Running {
                            source.set_phase_with_capture(capture, SourceState::Running, None);
                        }
                        unregistered.extend(tags);
                        register(&queue, &source, &mut unregistered);
                    }) {
                        break;
                    }
                }
                Ok(Some(Line::State { state, error })) => {
                    if state == "connected" {
                        if !source.admit_metadata(|capture| {
                            source.set_phase_with_capture(capture, SourceState::Running, None);
                        }) {
                            break;
                        }
                    } else {
                        let text =
                            source.redact(&error.unwrap_or_else(|| "connection lost".into()));
                        if !source.admit_metadata(|capture| {
                            hub.log(
                                "warn",
                                format!(
                                    "Signal source {} is reconnecting: {text}",
                                    source.info.uri
                                ),
                            );
                            source.queue_event(&hub, &queue, capture, "signal_gap", text.clone());
                            source.set_phase_with_capture(
                                capture,
                                SourceState::Reconnecting,
                                Some(text),
                            );
                        }) {
                            break;
                        }
                    }
                }
                Ok(Some(Line::Dropped { count })) => {
                    if !source.admit_metadata(|_| {
                        source.helper_dropped.fetch_add(count, Ordering::Relaxed);
                    }) {
                        break;
                    }
                }
                Ok(Some(Line::Error { message })) => {
                    error = Some(source.redact(&message));
                    break;
                }
                Ok(Some(Line::Browse { .. })) => {}
                Ok(Some(Line::Done { .. })) | Ok(None) => break,
                Err(e) => {
                    error = Some(source.redact(&format!("{e:#}")));
                    break;
                }
            }
        }
        // The supervisor owns stopping a bounded helper once admission is
        // closed, so completion cannot race a reader waiting on that helper.
        if source.limits.is_some() && source.stop.load(Ordering::SeqCst) {
            return;
        }
        let process = lock(&source.process).take();
        if let Some(mut process) = process {
            process.wait_or_kill(Instant::now() + Duration::from_secs(2));
        }
        let stopping = || source.stop.load(Ordering::SeqCst) || hub.quitting.load(Ordering::SeqCst);
        if stopping() {
            if source.limits.is_none() {
                source.set_phase(SourceState::Stopped, None);
            }
            return;
        }
        let error = error.unwrap_or_else(|| "signal helper exited".into());
        if !lock(&source.phase).settled {
            source.terminal_failure(error);
            return;
        }
        #[cfg(test)]
        Source::wait_reader_gate(&source.reader_restart_gate);
        if !source.admit_metadata(|capture| {
            source.queue_event(&hub, &queue, capture, "signal_error", error.clone());
            source.restarts.fetch_add(1, Ordering::Relaxed);
        }) {
            return;
        }
        // Rare exits weeks apart must not add up to a permanent failure.
        if started.elapsed() >= STABLE_RUN {
            recent_restarts = 0;
        }
        recent_restarts += 1;
        let restarts = recent_restarts;
        if restarts > MAX_RESTARTS {
            hub.log(
                "error",
                format!("Signal source {} failed: {error}", source.info.uri),
            );
            source.terminal_failure(error);
            return;
        }
        hub.log(
            "error",
            format!(
                "Signal source {} exited ({error}); restarting",
                source.info.uri
            ),
        );
        source.set_phase(SourceState::Reconnecting, Some(error));
        let until = Instant::now() + Duration::from_secs(1 << (restarts - 1));
        while Instant::now() < until {
            if stopping() {
                if source.limits.is_none() {
                    source.set_phase(SourceState::Stopped, None);
                }
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        match SignalProcess::spawn_bound(
            &source.spec,
            &hub.auth,
            source.auth.as_ref(),
            source.deadline.or_else(|| {
                Some(Instant::now() + Duration::from_millis(source.spec.timeout_ms + 5000))
            }),
        ) {
            Ok((mut process, stdout)) => {
                source
                    .helper_pid
                    .store(process.pid() as u64, Ordering::Relaxed);
                let mut slot = lock(&source.process);
                if stopping() {
                    process.request_stop();
                    process.wait_or_kill(Instant::now() + Duration::from_secs(2));
                    return;
                }
                *slot = Some(process);
                drop(slot);
                reader = Box::new(stdout);
            }
            Err(error) => {
                if stopping() {
                    return;
                }
                let error = format!("{error:#}");
                hub.log(
                    "error",
                    format!("Signal source {} failed: {error}", source.info.uri),
                );
                source.terminal_failure(error);
                return;
            }
        }
    }
}

pub struct SignalHub {
    inner: Arc<HubInner>,
}

struct StartConfig {
    interval_ms: u64,
    tags: Vec<String>,
    timeout_ms: u64,
    limits: Option<CaptureLimits>,
    auth: Option<super::auth::AuthBinding>,
    persistence: PersistenceRoute,
}

pub struct DatasetCaptureTarget<'a> {
    pub slot_id: &'a str,
    pub sink_generation: Option<u64>,
    pub auth: Option<&'a super::auth::AuthBinding>,
}

impl Default for SignalHub {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SignalHub {
    fn drop(&mut self) {
        self.stop_all_nowait();
    }
}

impl SignalHub {
    #[cfg(test)]
    pub(crate) fn private_route(&self, sink: Arc<dyn SampleSink>) -> Result<PrivateRoute> {
        route::PrivateRoute::create(&self.inner, sink, None)
    }

    pub(crate) fn private_route_until(
        &self,
        sink: Arc<dyn SampleSink>,
        deadline: Instant,
    ) -> Result<PrivateRoute> {
        route::PrivateRoute::create(&self.inner, sink, Some(deadline))
    }

    /// Start only through a coordinator-local sink capability. Wire commands
    /// cannot name this route, and an active source cannot be adopted by it.
    pub(crate) fn capture_to_private(
        &self,
        route: &PrivateRoute,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        limits: CaptureLimits,
        auth: Option<&super::auth::AuthBinding>,
    ) -> Result<SourceStatus> {
        let _starting = route.start(&self.inner)?;
        self.start_config(
            uri,
            StartConfig {
                interval_ms,
                tags,
                timeout_ms: super::default_timeout_ms(),
                limits: Some(limits),
                auth: auth.cloned(),
                persistence: PersistenceRoute::Private(route.clone()),
            },
        )
    }

    pub fn new() -> Self {
        Self::with_auth_store(super::auth::Store::session())
    }
    pub fn with_auth_store(store: super::auth::Store) -> Self {
        Self {
            inner: Arc::new(HubInner {
                auth: super::auth::Session::new(store),
                sources: Mutex::default(),
                queue: OnceLock::new(),
                has_sink: AtomicBool::new(false),
                sink_lifecycle: Mutex::default(),
                sink_generation: AtomicU64::new(0),
                current_sink: Mutex::default(),
                private_routes: Mutex::default(),
                dropped_total: AtomicU64::new(0),
                queued_bytes: AtomicU64::new(0),
                sink_error: Mutex::default(),
                logs: Mutex::default(),
                quitting: AtomicBool::new(false),
            }),
        }
    }

    /// Register a new source under the source limit.
    #[cfg(test)]
    fn insert(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        timeout_ms: u64,
        limits: Option<CaptureLimits>,
    ) -> Result<Arc<Source>> {
        self.insert_config(
            uri,
            StartConfig {
                interval_ms,
                tags,
                timeout_ms,
                limits,
                auth: None,
                persistence: PersistenceRoute::Shared,
            },
        )
    }
    fn insert_config(&self, uri: &str, config: StartConfig) -> Result<Arc<Source>> {
        let StartConfig {
            interval_ms,
            tags,
            timeout_ms,
            limits,
            auth,
            persistence,
        } = config;
        if let Some(binding) = &auth {
            super::auth::validate_endpoint(uri)?;
            self.inner.auth.validate_cached(binding, uri)?;
        }
        let parsed = parse(uri)?;
        super::require_compiled(parsed.protocol)?;
        super::validate_interval(interval_ms)?;
        super::validate_tags(&tags)?;
        if let Some(limits) = limits {
            limits.validate()?;
            let ready = match &persistence {
                PersistenceRoute::Shared => self.has_sink(),
                PersistenceRoute::Private(route) => route.has_sink(),
            };
            if !ready {
                bail!("bounded signal capture requires an open dataset or writable signal sink");
            }
        }
        if self.inner.quitting.load(Ordering::SeqCst) {
            bail!("signal logging is shutting down");
        }
        let id = super::source_id(uri);
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow!("signal capture identity: {error}"))?;
        let capture_id = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let source = Arc::new(Source {
            info: SignalInfo {
                id: id.clone(),
                protocol: parsed.protocol,
                uri: crate::media::redact_url(uri),
                interval_ms,
            },
            auth,
            capture_id,
            limits,
            deadline: limits
                .map(|limits| Instant::now() + Duration::from_millis(limits.duration_ms)),
            sink_generation: limits.and_then(|_| {
                matches!(persistence, PersistenceRoute::Shared)
                    .then(|| self.inner.sink_generation.load(Ordering::SeqCst))
            }),
            dataset_slot: match &persistence {
                PersistenceRoute::Shared => limits.and_then(|_| {
                    lock(&self.inner.current_sink)
                        .as_ref()
                        .and_then(|sink| sink.dataset_slot_id())
                }),
                PersistenceRoute::Private(route) => route.slot(),
            },
            persistence,
            capture: Mutex::default(),
            finishing: AtomicBool::new(false),
            spec: SignalSpec {
                uri: uri.to_string(),
                credentials: None,
                mode: Mode::Log,
                interval_ms,
                tags,
                timeout_ms,
                pki_dir: (parsed.protocol == super::Protocol::Opcua)
                    .then(super::process::pki_dir)
                    .flatten(),
            },
            started_ms: crate::scheduling::now_ms(),
            process: Mutex::default(),
            reader: Mutex::default(),
            reader_done: AtomicBool::new(true),
            #[cfg(test)]
            reader_line_gate: Mutex::default(),
            #[cfg(test)]
            reader_restart_gate: Mutex::default(),
            phase: Mutex::new(Phase {
                state: SourceState::Connecting,
                error: None,
                settled: false,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            samples: AtomicU64::new(0),
            payload_bytes: AtomicU64::new(0),
            written: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            helper_dropped: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
            last_sample_ms: AtomicU64::new(0),
            helper_pid: AtomicU64::new(0),
            tags: Mutex::default(),
        });
        let mut sources = lock(&self.inner.sources);
        if let Some(existing) = sources.get(&id) {
            let state = lock(&existing.phase).state;
            if !matches!(state, SourceState::Failed | SourceState::Stopped) {
                bail!("{id} is already logging; stop it first");
            }
            sources.remove(&id);
        }
        if sources.len() >= MAX_SOURCES {
            bail!("signal source limit reached ({MAX_SOURCES}); stop one first");
        }
        if let PersistenceRoute::Private(route) = &source.persistence {
            route.track(&source)?;
            // MQTT can connect or report a gap before discovering any tags.
            // Register the source itself before any helper output can refer
            // to it; the immutable route preserves this queue ordering.
            if route
                .queue()
                .try_send(HubMsg::Register(source.clone(), Vec::new()))
                .is_err()
            {
                let message = "private signal source registration queue is unavailable".to_string();
                route.error(message.clone());
                bail!("{message}");
            }
        }
        sources.insert(id, source.clone());
        Ok(source)
    }

    fn remove(&self, source: &Arc<Source>) {
        let mut sources = lock(&self.inner.sources);
        if sources
            .get(&source.info.id)
            .is_some_and(|s| Arc::ptr_eq(s, source))
        {
            sources.remove(&source.info.id);
        }
    }

    fn follow(&self, source: &Arc<Source>, reader: Box<dyn BufRead + Send>) -> Result<()> {
        let hub = self.inner.clone();
        let follow = source.clone();
        let name = format!(
            "capturefab-signal-{}",
            source.info.id.get(7..15).unwrap_or("source")
        );
        source.reader_done.store(false, Ordering::Release);
        struct Done(Arc<Source>);
        impl Drop for Done {
            fn drop(&mut self) {
                self.0.reader_done.store(true, Ordering::Release);
            }
        }
        let handle = thread::Builder::new().name(name).spawn(move || {
            let _done = Done(follow.clone());
            read_source(hub, follow, reader);
        });
        let handle = match handle {
            Ok(handle) => handle,
            Err(error) => {
                source.reader_done.store(true, Ordering::Release);
                return Err(error.into());
            }
        };
        *lock(&source.reader) = Some(handle);
        if source.limits.is_some() {
            let hub = self.inner.clone();
            let capture = source.clone();
            thread::Builder::new()
                .name("capturefab-signal-limit".into())
                .spawn(move || supervise_capture(hub, capture))?;
        }
        Ok(())
    }

    /// Start logging a source. Waits up to `timeout_ms` for its helper to
    /// connect; a helper that fails before connecting fails the start.
    pub fn start(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        timeout_ms: u64,
    ) -> Result<SourceStatus> {
        self.start_inner(uri, interval_ms, tags, timeout_ms, None)
    }
    pub fn start_bound(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        timeout_ms: u64,
        auth: Option<super::auth::AuthBinding>,
    ) -> Result<SourceStatus> {
        self.start_config(
            uri,
            StartConfig {
                interval_ms,
                tags,
                timeout_ms,
                limits: None,
                auth,
                persistence: PersistenceRoute::Shared,
            },
        )
    }
    pub fn bind(&self, profile: &super::auth::ProfileRef) -> Result<super::auth::BindingReceipt> {
        self.inner.auth.bind(profile)
    }
    pub fn cached_bindings(&self) -> Vec<super::auth::BindingReceipt> {
        self.inner.auth.cached_bindings()
    }

    pub fn capture(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        timeout_ms: u64,
        limits: CaptureLimits,
    ) -> Result<SourceStatus> {
        let _lifecycle = lock(&self.inner.sink_lifecycle);
        self.start_inner(uri, interval_ms, tags, timeout_ms, Some(limits))
    }

    pub fn capture_for_dataset(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        limits: CaptureLimits,
        slot_id: &str,
        sink_generation: Option<u64>,
    ) -> Result<SourceStatus> {
        self.capture_for_dataset_bound(
            uri,
            interval_ms,
            tags,
            limits,
            DatasetCaptureTarget {
                slot_id,
                sink_generation,
                auth: None,
            },
        )
    }
    pub fn capture_for_dataset_bound(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        limits: CaptureLimits,
        target: DatasetCaptureTarget<'_>,
    ) -> Result<SourceStatus> {
        let _lifecycle = lock(&self.inner.sink_lifecycle);
        if target
            .sink_generation
            .is_some_and(|expected| self.inner.sink_generation.load(Ordering::SeqCst) != expected)
        {
            bail!("signal sink instance changed; review the attached sink before capturing");
        }
        self.require_dataset_sink(target.slot_id)?;
        self.start_config(
            uri,
            StartConfig {
                interval_ms,
                tags,
                timeout_ms: super::default_timeout_ms(),
                limits: Some(limits),
                auth: target.auth.cloned(),
                persistence: PersistenceRoute::Shared,
            },
        )
    }

    fn require_dataset_sink(&self, slot_id: &str) -> Result<()> {
        if lock(&self.inner.current_sink)
            .as_ref()
            .and_then(|sink| sink.dataset_slot_id())
            .as_deref()
            != Some(slot_id)
        {
            bail!(
                "signal sink does not write to the approved dataset instance; inspect the attached sink before capturing"
            );
        }
        Ok(())
    }

    fn start_inner(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        timeout_ms: u64,
        limits: Option<CaptureLimits>,
    ) -> Result<SourceStatus> {
        self.start_config(
            uri,
            StartConfig {
                interval_ms,
                tags,
                timeout_ms,
                limits,
                auth: None,
                persistence: PersistenceRoute::Shared,
            },
        )
    }
    fn start_config(&self, uri: &str, config: StartConfig) -> Result<SourceStatus> {
        let timeout_ms = config.timeout_ms;
        let source = self.insert_config(uri, config)?;
        if source.stop.load(Ordering::SeqCst)
            || source
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.remove(&source);
            bail!("signal capture closed before its helper started");
        }
        let (mut process, stdout) = match SignalProcess::spawn_bound(
            &source.spec,
            &self.inner.auth,
            source.auth.as_ref(),
            source
                .deadline
                .or_else(|| Some(Instant::now() + Duration::from_millis(timeout_ms + 5000))),
        ) {
            Ok(started) => started,
            Err(error) => {
                self.remove(&source);
                return Err(error);
            }
        };
        if source.stop.load(Ordering::SeqCst)
            || source
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            process.request_stop();
            process.wait_or_kill(Instant::now() + Duration::from_secs(2));
            self.remove(&source);
            bail!("signal capture closed while its helper started");
        }
        source
            .helper_pid
            .store(process.pid() as u64, Ordering::Relaxed);
        *lock(&source.process) = Some(process);
        if let Err(error) = self.follow(&source, Box::new(stdout)) {
            self.remove(&source);
            return Err(error);
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms + 5000);
        let mut phase = lock(&source.phase);
        while !phase.settled && !source.stop.load(Ordering::SeqCst) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            phase = source
                .changed
                .wait_timeout(phase, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let failed = (phase.state == SourceState::Failed).then(|| {
            phase
                .error
                .clone()
                .unwrap_or_else(|| "signal helper exited".into())
        });
        drop(phase);
        if let Some(error) = failed {
            self.remove(&source);
            if let Some(handle) = lock(&source.reader).take() {
                let _ = handle.join();
            }
            return Err(anyhow!(error));
        }
        Ok(source.status(256, false))
    }

    /// Stop a source by ID, or every source with "all". Helpers are asked
    /// to stop together, then waited for until one shared deadline; their
    /// last samples are flushed to the sink before this returns.
    pub fn stop(&self, id: &str, wait: Duration) -> Result<Vec<SourceStatus>> {
        let targets: Vec<Arc<Source>> = {
            let sources = lock(&self.inner.sources);
            if id == "all" {
                sources.values().cloned().collect()
            } else {
                vec![
                    sources
                        .get(id)
                        .cloned()
                        .ok_or_else(|| anyhow!("signal source {id} not found"))?,
                ]
            }
        };
        self.stop_targets(targets, wait)
    }

    pub fn stop_if(&self, id: &str, capture_id: &str, wait: Duration) -> Result<Vec<SourceStatus>> {
        super::validate_id(id)?;
        super::validate_capture_id(capture_id)?;
        if id == "all" {
            bail!("conditional signal stop requires one source ID");
        }
        let source = lock(&self.inner.sources)
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("signal source {id} not found"))?;
        if source.capture_id != capture_id {
            bail!("signal capture instance changed; refusing to stop replacement {id}");
        }
        self.stop_targets(vec![source], wait)
    }

    fn stop_targets(&self, targets: Vec<Arc<Source>>, wait: Duration) -> Result<Vec<SourceStatus>> {
        let deadline = Instant::now() + wait;
        for source in &targets {
            // Close every source's admission before dropping the helper and
            // its credential scrubber. A timed-out reader may still hold a
            // buffered line, but cannot publish it after this stop receipt.
            source.request_completion(CaptureCompletion::Stopped, None);
            if source.limits.is_some() {
                finish_capture(
                    &self.inner,
                    source,
                    deadline.saturating_duration_since(Instant::now()),
                );
                continue;
            }
            if let Some(process) = lock(&source.process).as_mut() {
                process.request_stop();
            }
        }
        for source in &targets {
            if source.limits.is_some() {
                continue;
            }
            let process = lock(&source.process).take();
            if let Some(mut process) = process {
                process.wait_or_kill(deadline);
            }
        }
        for source in &targets {
            let handle = lock(&source.reader).take();
            if let Some(handle) = handle {
                while !handle.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                if handle.is_finished() {
                    let _ = handle.join();
                }
            }
        }
        if targets.iter().any(|source| source.limits.is_none()) && self.has_sink() {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1));
            let _ = self.flush(remaining);
        }
        for source in &targets {
            if source.limits.is_none() {
                continue;
            }
            while lock(&source.capture).completed_ms.is_none() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    bail!("signal capture is stopping; final flush is still pending");
                }
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
        }
        Ok(targets
            .iter()
            .map(|source| {
                self.remove(source);
                {
                    let mut phase = lock(&source.phase);
                    if source.limits.is_none() {
                        phase.state = SourceState::Stopped;
                    }
                }
                source.status(256, false)
            })
            .collect())
    }
    /// Stop everything for shutdown; nothing restarts afterwards.
    pub fn stop_all(&self, wait: Duration) {
        self.inner.quitting.store(true, Ordering::SeqCst);
        let _ = self.stop("all", wait);
    }
    /// Ask every helper to stop without waiting; used from `Drop`.
    pub fn stop_all_nowait(&self) {
        self.inner.quitting.store(true, Ordering::SeqCst);
        for source in lock(&self.inner.sources).values() {
            source.request_completion(CaptureCompletion::Stopped, None);
            if let Some(process) = lock(&source.process).as_mut() {
                process.request_stop();
            }
        }
    }
    /// Up to 256 tags per source; recent points only with `history`.
    pub fn status(&self, history: bool) -> Vec<SourceStatus> {
        self.sources()
            .iter()
            .map(|s| s.status(256, history))
            .collect()
    }
    /// Up to 32 tags per source and no history, for session snapshots.
    pub fn status_brief(&self) -> Vec<SourceStatus> {
        self.sources().iter().map(|s| s.status(32, false)).collect()
    }
    fn sources(&self) -> Vec<Arc<Source>> {
        lock(&self.inner.sources).values().cloned().collect()
    }
    pub fn has_sources(&self) -> bool {
        !lock(&self.inner.sources).is_empty()
    }
    /// Attach or detach the sink after the batches already queued. The old
    /// sink is finished; its error, if any, is returned.
    pub fn set_sink(&self, sink: Option<Arc<dyn SampleSink>>, timeout: Duration) -> Result<()> {
        self.set_sink_if_generation(sink, None, timeout)
    }

    /// Compare the reviewed sink generation and change it under the same
    /// lifecycle lock, so a shared sink replacement is never detached by a
    /// stale dataset close.
    pub fn set_sink_if_generation(
        &self,
        sink: Option<Arc<dyn SampleSink>>,
        expected_generation: Option<u64>,
        timeout: Duration,
    ) -> Result<()> {
        let _lifecycle = lock(&self.inner.sink_lifecycle);
        if expected_generation
            .is_some_and(|expected| self.inner.sink_generation.load(Ordering::SeqCst) != expected)
        {
            bail!(
                "signal sink instance changed; review the attached sink before closing the dataset"
            );
        }
        if sink.is_none() && self.inner.queue.get().is_none() {
            return Ok(());
        }
        self.inner
            .request(|ack| HubMsg::SetSink(sink, ack), timeout, "sink change")
    }
    pub fn has_sink(&self) -> bool {
        self.inner.has_sink.load(Ordering::SeqCst)
    }
    pub fn sink_error(&self) -> Option<String> {
        lock(&self.inner.sink_error).clone()
    }
    /// Inspect the actual attached sink's current local identity. Keeping an
    /// Arc here, rather than a token cached when attaching, handles lazy opens.
    pub fn sink_status(&self) -> super::SignalSinkStatus {
        let (sink, generation) = {
            let current = lock(&self.inner.current_sink);
            (
                current.clone(),
                self.inner.sink_generation.load(Ordering::SeqCst),
            )
        };
        super::SignalSinkStatus {
            attached: sink.is_some(),
            generation,
            dataset_slot_id: sink.and_then(|sink| sink.dataset_slot_id()),
            error: self.sink_error(),
            dropped_total: self.dropped_total(),
        }
    }
    pub fn dropped_total(&self) -> u64 {
        self.inner.dropped_total.load(Ordering::Relaxed)
    }
    /// Write every queued sample to the sink.
    pub fn flush(&self, timeout: Duration) -> Result<()> {
        if self.inner.queue.get().is_none() {
            return Ok(());
        }
        self.inner.request(HubMsg::Flush, timeout, "flush")
    }
    /// Latest sample per `source_id/tag`.
    pub fn latest_values(&self) -> BTreeMap<String, Sample> {
        let mut values = BTreeMap::new();
        for source in self.sources() {
            for (tag, state) in lock(&source.tags).iter() {
                if let Some(last) = &state.last {
                    values.insert(format!("{}/{tag}", source.info.id), last.clone());
                }
            }
        }
        values
    }
    pub fn logs(&self) -> Vec<LogEntry> {
        lock(&self.inner.logs).iter().cloned().collect()
    }

    /// A source fed by `reader` instead of a helper process.
    #[cfg(test)]
    fn start_with_reader(&self, uri: &str, reader: impl BufRead + Send + 'static) -> Arc<Source> {
        let source = self.insert(uri, 1000, Vec::new(), 1000, None).unwrap();
        self.follow(&source, Box::new(reader)).unwrap();
        source
    }

    #[cfg(test)]
    fn capture_with_reader(
        &self,
        uri: &str,
        limits: CaptureLimits,
        reader: impl BufRead + Send + 'static,
    ) -> Arc<Source> {
        let _lifecycle = lock(&self.inner.sink_lifecycle);
        let source = self
            .insert(uri, 100, Vec::new(), 1000, Some(limits))
            .unwrap();
        self.follow(&source, Box::new(reader)).unwrap();
        source
    }

    #[cfg(all(test, unix))]
    pub(crate) fn capture_to_private_with_reader(
        &self,
        route: &PrivateRoute,
        uri: &str,
        limits: CaptureLimits,
        reader: Box<dyn BufRead + Send>,
    ) -> Result<SourceStatus> {
        self.capture_private_with_reader(route, uri, limits, reader)
            .map(|source| source.status(256, false))
    }

    #[cfg(test)]
    fn capture_private_with_reader(
        &self,
        route: &PrivateRoute,
        uri: &str,
        limits: CaptureLimits,
        reader: impl BufRead + Send + 'static,
    ) -> Result<Arc<Source>> {
        let _starting = route.start(&self.inner)?;
        let source = self.insert_config(
            uri,
            StartConfig {
                interval_ms: 100,
                tags: Vec::new(),
                timeout_ms: 1000,
                limits: Some(limits),
                auth: None,
                persistence: PersistenceRoute::Private(route.clone()),
            },
        )?;
        self.follow(&source, Box::new(reader))?;
        Ok(source)
    }
}

/// Coordinator arm for `signals`, `signal_read`, `signal_browse`,
/// `signal_start` and `signal_stop`.
pub fn run(hub: &SignalHub, command: SessionCommand) -> Result<Value> {
    match command {
        SessionCommand::Signals => Ok(json!({
            "sources": hub.status(false),
            "persisting": hub.has_sink(),
            "dropped": hub.dropped_total(),
            "sink_error": hub.sink_error(),
            "sink": hub.sink_status(),
            "bindings": hub.cached_bindings(),
        })),
        SessionCommand::SignalBind { profile } => Ok(serde_json::to_value(hub.bind(&profile)?)?),
        SessionCommand::SignalRead {
            uri,
            timeout_ms,
            auth,
        } => super::process::read_bound(&uri, timeout_ms, &hub.inner.auth, auth.as_ref()),
        SessionCommand::SignalBrowse {
            uri,
            duration_ms,
            limit,
            auth,
        } => super::process::browse_bound(&uri, duration_ms, limit, &hub.inner.auth, auth.as_ref()),
        SessionCommand::SignalStart {
            uri,
            interval_ms,
            tags,
            auth,
        } => Ok(serde_json::to_value(hub.start_bound(
            &uri,
            interval_ms,
            tags,
            super::default_timeout_ms(),
            auth,
        )?)?),
        SessionCommand::SignalStop { id } => {
            Ok(json!({"stopped": hub.stop(&id, Duration::from_secs(3))?}))
        }
        SessionCommand::SignalStopIf { id, capture_id } => {
            Ok(json!({"stopped": hub.stop_if(&id, &capture_id, Duration::from_secs(3))?}))
        }
        _ => bail!("not a signal command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{Quality, Value as V};
    use std::io::Write;

    #[derive(Default)]
    struct MemorySink {
        written: Mutex<Vec<(String, Sample)>>,
        registered: Mutex<Vec<String>>,
        events: Mutex<Vec<String>>,
        finished: AtomicBool,
        fail: AtomicBool,
    }
    impl SampleSink for MemorySink {
        fn register(&self, source: &SignalInfo, _tags: &[TagInfo]) -> Result<()> {
            lock(&self.registered).push(source.id.clone());
            Ok(())
        }
        fn write(&self, source_id: &str, batch: &[Sample]) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                bail!("Storage full: test");
            }
            lock(&self.written).extend(batch.iter().map(|s| (source_id.to_string(), s.clone())));
            Ok(())
        }
        fn finish(&self) -> Result<()> {
            self.finished.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn event(&self, kind: &str, _: &str, _: &str) -> Result<()> {
            lock(&self.events).push(kind.into());
            Ok(())
        }
        fn describe(&self) -> String {
            "memory".into()
        }
    }

    fn samples_line(from: u64, count: u64) -> String {
        let samples: Vec<Sample> = (from..from + count)
            .map(|i| Sample {
                ts_ms: i + 1,
                source_ts_ms: None,
                tag: format!("t{}", i % 3),
                value: V::Number(i as f64),
                quality: Quality::Good,
            })
            .collect();
        format!(
            "{}\n",
            serde_json::to_string(&Line::Samples { samples }).unwrap()
        )
    }

    fn wait(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "condition not reached");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn limits(duration_ms: u64, max_samples: u64, max_bytes: u64) -> CaptureLimits {
        CaptureLimits {
            duration_ms,
            max_samples,
            max_bytes,
        }
    }

    fn bounded_pipe(
        hub: &SignalHub,
        uri: &str,
        limits: CaptureLimits,
    ) -> (Arc<Source>, std::io::PipeWriter) {
        let (reader, writer) = std::io::pipe().unwrap();
        (
            hub.capture_with_reader(uri, limits, std::io::BufReader::new(reader)),
            writer,
        )
    }

    #[test]
    fn bounded_reader_metadata_cannot_change_after_terminal_capture_status() {
        for line in [
            Line::Dropped { count: 7 },
            Line::Ready {
                tags: vec![TagInfo {
                    tag: "late-tag".into(),
                    unit: None,
                    kind: None,
                }],
            },
            Line::State {
                state: "disconnected".into(),
                error: Some("late gap".into()),
            },
            Line::Error {
                message: "late error".into(),
            },
        ] {
            let hub = SignalHub::new();
            let sink = Arc::new(MemorySink::default());
            hub.set_sink(Some(sink.clone()), Duration::from_secs(1))
                .unwrap();
            let (source, mut writer) =
                bounded_pipe(&hub, "sim-signal://metadata-race", limits(500, 100, 10000));
            writer.write_all(samples_line(0, 1).as_bytes()).unwrap();
            wait(|| source.samples.load(Ordering::SeqCst) == 1);
            let (entered, observed) = mpsc::channel();
            let (resume, paused) = mpsc::channel();
            *lock(&source.reader_line_gate) = Some(ReaderGate {
                entered,
                resume: paused,
            });
            writeln!(writer, "{}", serde_json::to_string(&line).unwrap()).unwrap();
            observed.recv_timeout(Duration::from_secs(1)).unwrap();
            // The buffered reader passed its early stop check. A coordinator
            // deadline still completes without waiting for that stalled reader.
            wait(|| {
                let status = source.status(32, false);
                status.completed_ms.is_some() && status.state == SourceState::Stopped
            });
            let terminal = source.status(32, false);
            assert_eq!(terminal.completion, Some(CaptureCompletion::Duration));
            assert!(terminal.persistence_complete, "{terminal:?}");
            assert_eq!(terminal.written, 1);
            assert_eq!(terminal.helper_dropped, 0);
            assert!(!lock(&source.reader).as_ref().unwrap().is_finished());
            resume.send(()).unwrap();
            drop(writer);
            wait(|| lock(&source.reader).as_ref().unwrap().is_finished());
            hub.flush(Duration::from_secs(1)).unwrap();
            let after = source.status(32, false);
            assert_eq!(
                after, terminal,
                "late line {line:?} changed terminal status"
            );
            assert!(!lock(&source.tags).contains_key("late-tag"));
            assert!(lock(&sink.registered).is_empty());
            assert!(lock(&sink.events).is_empty());
            assert_eq!(lock(&sink.written).len(), 1);
        }
    }

    #[test]
    fn bounded_restart_and_late_failure_cannot_rewrite_terminal_capture_status() {
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink.clone()), Duration::from_secs(1))
            .unwrap();
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://late-restart", limits(500, 100, 10000));
        writer
            .write_all(b"{\"t\":\"ready\",\"tags\":[]}\n")
            .unwrap();
        writer.write_all(samples_line(0, 1).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::SeqCst) == 1);
        let (entered, observed) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        *lock(&source.reader_restart_gate) = Some(ReaderGate {
            entered,
            resume: paused,
        });
        writer
            .write_all(b"{\"t\":\"error\",\"message\":\"helper exited\"}\n")
            .unwrap();
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        wait(|| {
            let status = source.status(32, false);
            status.completed_ms.is_some() && status.state == SourceState::Stopped
        });
        let terminal = source.status(32, false);
        assert!(terminal.persistence_complete, "{terminal:?}");
        assert_eq!(terminal.restarts, 0);
        assert!(!lock(&source.reader).as_ref().unwrap().is_finished());
        resume.send(()).unwrap();
        wait(|| lock(&source.reader).as_ref().unwrap().is_finished());
        hub.flush(Duration::from_secs(1)).unwrap();
        assert_eq!(source.status(32, false), terminal);
        assert!(lock(&sink.events).is_empty());
        // Credential/helper startup failures can return after the supervisor
        // deadline too. A completed result must remain the reviewed result.
        source.terminal_failure("late helper startup failure".into());
        assert_eq!(source.status(32, false), terminal);
        drop(writer);
    }

    #[test]
    fn bounded_helper_drops_admitted_before_close_report_incomplete_persistence() {
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink), Duration::from_secs(1)).unwrap();
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://admitted-drops", limits(500, 100, 10000));
        writer.write_all(samples_line(0, 1).as_bytes()).unwrap();
        writer
            .write_all(b"{\"t\":\"dropped\",\"count\":7}\n")
            .unwrap();
        wait(|| source.helper_dropped.load(Ordering::SeqCst) == 7);
        wait(|| {
            let status = source.status(32, false);
            status.completed_ms.is_some() && status.state == SourceState::Failed
        });
        let terminal = source.status(32, false);
        assert_eq!(terminal.state, SourceState::Failed);
        assert_eq!(terminal.samples, 1);
        assert_eq!(terminal.written, 1);
        assert_eq!(terminal.helper_dropped, 7);
        assert!(!terminal.persistence_complete);
        assert!(
            terminal
                .persistence_error
                .as_ref()
                .unwrap()
                .contains("did not persist every acquired sample")
        );
        drop(writer);
    }

    #[test]
    fn unbounded_stop_fences_buffered_values_after_a_reader_join_timeout() {
        struct NotifyDrop<R> {
            reader: R,
            done: mpsc::Sender<()>,
        }
        impl<R: BufRead> std::io::Read for NotifyDrop<R> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                self.reader.read(out)
            }
        }
        impl<R: BufRead> BufRead for NotifyDrop<R> {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                self.reader.fill_buf()
            }
            fn consume(&mut self, count: usize) {
                self.reader.consume(count);
            }
        }
        impl<R> Drop for NotifyDrop<R> {
            fn drop(&mut self) {
                let _ = self.done.send(());
            }
        }
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink.clone()), Duration::from_secs(1))
            .unwrap();
        let (reader, mut writer) = std::io::pipe().unwrap();
        let (done, finished) = mpsc::channel();
        let source = hub.start_with_reader(
            "sim-signal://unbounded-stop-race",
            NotifyDrop {
                reader: std::io::BufReader::new(reader),
                done,
            },
        );
        writer.write_all(samples_line(0, 1).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::SeqCst) == 1);
        hub.flush(Duration::from_secs(1)).unwrap();
        let (entered, observed) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        *lock(&source.reader_line_gate) = Some(ReaderGate {
            entered,
            resume: paused,
        });
        let canary = "CREDENTIAL_ECHO_AFTER_STOP_CANARY";
        let late = Line::Samples {
            samples: vec![Sample::now(
                "password-echo",
                V::Text(canary.into()),
                Quality::Good,
            )],
        };
        writeln!(writer, "{}", serde_json::to_string(&late).unwrap()).unwrap();
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        // Stop must return after its bounded reader wait. A protected helper's
        // process/credential scrubber may already be gone at this same point.
        let stopped = hub
            .stop_if(
                &source.info.id,
                &source.capture_id,
                Duration::from_millis(1),
            )
            .unwrap();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].state, SourceState::Stopped);
        assert_eq!(stopped[0].samples, 1);
        assert_eq!(stopped[0].written, 1);
        assert!(lock(&source.process).is_none());
        assert!(
            finished.try_recv().is_err(),
            "the stalled reader has not returned yet"
        );
        resume.send(()).unwrap();
        drop(writer);
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        hub.flush(Duration::from_secs(1)).unwrap();
        let after = source.status(32, false);
        assert_eq!(
            after, stopped[0],
            "late buffered data changed the stop receipt"
        );
        assert_eq!(lock(&sink.written).len(), 1);
        assert!(!serde_json::to_string(&after).unwrap().contains(canary));
        assert!(
            !serde_json::to_string(&*lock(&sink.written))
                .unwrap()
                .contains(canary)
        );
    }

    #[test]
    fn bounded_capture_requires_a_sink_and_does_not_adopt_an_active_source() {
        let hub = SignalHub::new();
        assert!(
            hub.insert(
                "sim-signal://bounded",
                100,
                vec![],
                1000,
                Some(limits(1000, 10, 10000))
            )
            .err()
            .unwrap()
            .to_string()
            .contains("requires an open dataset")
        );
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink), Duration::from_secs(2)).unwrap();
        assert!(
            hub.capture_for_dataset(
                "sim-signal://foreign-sink",
                100,
                vec![],
                limits(1000, 10, 10000),
                "0123456789abcdef0123456789abcdef",
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("does not write to the approved dataset")
        );
        assert!(hub.status(false).is_empty());
        let (_source, writer) = bounded_pipe(&hub, "sim-signal://bounded", limits(1000, 10, 10000));
        let error = hub
            .insert(
                "sim-signal://bounded",
                100,
                vec![],
                1000,
                Some(limits(1000, 10, 10000)),
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("already logging"));
        drop(writer);
        hub.stop("all", Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn bounded_capture_trims_a_batch_to_sample_limit_and_flushes() {
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink.clone()), Duration::from_secs(2))
            .unwrap();
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://sample-limit", limits(5000, 3, 10000));
        writer.write_all(samples_line(0, 20).as_bytes()).unwrap();
        wait(|| source.status(32, false).completed_ms.is_some());
        let status = source.status(32, false);
        assert_eq!(status.completion, Some(CaptureCompletion::MaxSamples));
        assert_eq!(status.state, SourceState::Stopped);
        assert_eq!(status.samples, 3);
        assert_eq!(status.written, 3);
        assert_eq!(status.payload_bytes, status.written_bytes);
        assert!(status.persistence_complete);
        assert!(status.persistence_error.is_none());
        assert_eq!(lock(&sink.written).len(), 3);
        assert_eq!(hub.inner.queued_bytes.load(Ordering::SeqCst), 0);
        assert_eq!(hub.status(false)[0].capture_id, source.capture_id);
        drop(writer);
    }

    #[test]
    fn bounded_capture_byte_limit_accepts_only_a_fitting_prefix() {
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink.clone()), Duration::from_secs(2))
            .unwrap();
        let line = samples_line(0, 4);
        let Line::Samples { samples } = serde_json::from_str::<Line>(&line).unwrap() else {
            panic!()
        };
        let first_two = payload_bytes(&samples[..2]);
        let (source, mut writer) = bounded_pipe(
            &hub,
            "sim-signal://byte-limit",
            limits(5000, 100, first_two + 1),
        );
        writer.write_all(line.as_bytes()).unwrap();
        wait(|| source.status(32, false).completed_ms.is_some());
        let status = source.status(32, false);
        assert_eq!(status.completion, Some(CaptureCompletion::MaxBytes));
        assert_eq!(status.samples, 2);
        assert_eq!(status.payload_bytes, first_two);
        assert_eq!(status.written_bytes, first_two);
        assert!(status.persistence_complete);
        assert_eq!(lock(&sink.written).len(), 2);
        drop(writer);
    }

    #[test]
    fn deadline_finishes_and_flushes_when_the_client_and_helper_are_idle() {
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink.clone()), Duration::from_secs(2))
            .unwrap();
        let (source, mut writer) = bounded_pipe(&hub, "sim-signal://idle", limits(100, 100, 10000));
        writer.write_all(samples_line(0, 2).as_bytes()).unwrap();
        wait(|| source.status(32, false).completed_ms.is_some());
        let status = source.status(32, false);
        assert_eq!(status.completion, Some(CaptureCompletion::Duration));
        assert_eq!(status.samples, 2);
        assert_eq!(status.written, 2);
        assert!(status.persistence_complete);
        // The pipe and its blocked reader remain alive after the caller has
        // left. Late helper samples cannot extend or change the capture.
        writer.write_all(samples_line(2, 2).as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(30));
        assert_eq!(source.status(32, false), status);
        assert_eq!(lock(&sink.written).len(), 2);
        drop(writer);
    }

    #[test]
    fn conditional_stop_refuses_replacement_but_survives_a_helper_restart() {
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        hub.set_sink(Some(sink), Duration::from_secs(2)).unwrap();
        let (first, mut writer) =
            bounded_pipe(&hub, "sim-signal://replacement", limits(5000, 1, 10000));
        writer.write_all(samples_line(0, 1).as_bytes()).unwrap();
        wait(|| first.status(32, false).completed_ms.is_some());
        drop(writer);
        let (second, mut writer) =
            bounded_pipe(&hub, "sim-signal://replacement", limits(5000, 100, 10000));
        assert_ne!(first.capture_id, second.capture_id);
        assert!(
            hub.stop_if(&first.info.id, &first.capture_id, Duration::from_secs(2))
                .unwrap_err()
                .to_string()
                .contains("instance changed")
        );
        assert!(!second.stop.load(Ordering::SeqCst));
        writer
            .write_all(b"{\"t\":\"ready\",\"tags\":[]}\n")
            .unwrap();
        wait(|| second.status(32, false).state == SourceState::Running);
        drop(writer);
        wait(|| second.restarts.load(Ordering::Relaxed) > 0);
        assert_eq!(hub.status(false)[0].capture_id, second.capture_id);
        let stopped = hub
            .stop_if(&second.info.id, &second.capture_id, Duration::from_secs(2))
            .unwrap();
        assert_eq!(stopped[0].capture_id, second.capture_id);
        assert_eq!(stopped[0].completion, Some(CaptureCompletion::Stopped));
        assert!(hub.status(false).is_empty());
    }

    #[test]
    fn bounded_capture_records_sink_failure_without_claiming_persistence() {
        let hub = SignalHub::new();
        let sink = Arc::new(MemorySink::default());
        sink.fail.store(true, Ordering::SeqCst);
        hub.set_sink(Some(sink), Duration::from_secs(2)).unwrap();
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://storage-failure", limits(5000, 3, 10000));
        writer.write_all(samples_line(0, 10).as_bytes()).unwrap();
        wait(|| source.status(32, false).completed_ms.is_some());
        let status = source.status(32, false);
        assert_eq!(status.state, SourceState::Failed);
        assert_eq!(status.samples, 3);
        assert_eq!(status.written, 0);
        assert_eq!(status.written_bytes, 0);
        assert_eq!(status.dropped, 3);
        assert!(!status.persistence_complete);
        assert!(status.persistence_error.unwrap().contains("Storage full"));
        let sink_status = hub.sink_status();
        assert!(sink_status.attached);
        assert_eq!(sink_status.dropped_total, 3);
        assert!(sink_status.error.unwrap().contains("Storage full"));
        drop(writer);
    }

    #[test]
    fn bounded_capture_never_writes_into_a_replacement_sink() {
        let hub = SignalHub::new();
        let first = Arc::new(MemorySink::default());
        let second = Arc::new(MemorySink::default());
        hub.set_sink(Some(first.clone()), Duration::from_secs(2))
            .unwrap();
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://sink-replaced", limits(5000, 100, 10000));
        writer.write_all(samples_line(0, 2).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::Relaxed) == 2);
        hub.set_sink(Some(second.clone()), Duration::from_secs(2))
            .unwrap();
        wait(|| source.status(32, false).completed_ms.is_some());
        writer.write_all(samples_line(2, 2).as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(30));
        let status = source.status(32, false);
        assert_eq!(status.state, SourceState::Failed);
        assert!(!status.persistence_complete);
        assert!(
            status
                .persistence_error
                .unwrap()
                .contains("replaced or detached")
        );
        assert_eq!(lock(&first.written).len(), 2);
        assert!(lock(&second.written).is_empty());
        drop(writer);
    }

    #[test]
    fn lazy_dataset_identity_is_read_from_the_actual_sink_and_pinned() {
        #[derive(Default)]
        struct DatasetSink {
            slot: Mutex<Option<String>>,
            written: AtomicU64,
        }
        impl SampleSink for DatasetSink {
            fn dataset_slot_id(&self) -> Option<String> {
                lock(&self.slot).clone()
            }
            fn register(&self, _: &SignalInfo, _: &[TagInfo]) -> Result<()> {
                Ok(())
            }
            fn write(&self, _: &str, batch: &[Sample]) -> Result<()> {
                self.written.fetch_add(batch.len() as u64, Ordering::SeqCst);
                Ok(())
            }
            fn finish(&self) -> Result<()> {
                Ok(())
            }
            fn describe(&self) -> String {
                "dataset".into()
            }
        }
        let hub = SignalHub::new();
        let sink = Arc::new(DatasetSink::default());
        hub.set_sink(Some(sink.clone()), Duration::from_secs(2))
            .unwrap();
        let first = "0123456789abcdef0123456789abcdef";
        assert!(hub.require_dataset_sink(first).is_err());
        // GUI datasets open lazily after their Slot sink has been attached.
        *lock(&sink.slot) = Some(first.into());
        hub.require_dataset_sink(first).unwrap();
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://lazy-slot", limits(1000, 1, 10000));
        assert_eq!(source.dataset_slot.as_deref(), Some(first));
        *lock(&sink.slot) = Some("fedcba9876543210fedcba9876543210".into());
        assert!(hub.require_dataset_sink(first).is_err());
        writer.write_all(samples_line(0, 1).as_bytes()).unwrap();
        wait(|| source.status(32, false).completed_ms.is_some());
        let status = source.status(32, false);
        assert_eq!(status.state, SourceState::Failed);
        assert_eq!(status.written, 0);
        assert_eq!(sink.written.load(Ordering::SeqCst), 0);
        assert!(
            status
                .persistence_error
                .unwrap()
                .contains("dataset instance changed")
        );
        drop(writer);
    }

    #[test]
    fn sink_readiness_tracks_lazy_dataset_open_reopen_foreign_sink_and_detach() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-sink-status-{}-{}",
            std::process::id(),
            getrandom::u64().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let hub = SignalHub::new();
        assert_eq!(hub.sink_status(), super::super::SignalSinkStatus::default());
        let slot = Arc::new(crate::dataset::Slot::new());
        hub.set_sink(Some(slot.clone()), Duration::from_secs(2))
            .unwrap();
        assert!(hub.sink_status().attached);
        assert_eq!(hub.sink_status().dataset_slot_id, None);

        let path = root.join("sink.duckdb");
        let first = slot
            .open(&path, crate::dataset::OpenMode::Create, None)
            .unwrap();
        assert_eq!(hub.sink_status().dataset_slot_id, first.slot_id);
        let first_generation = hub.sink_status().generation;
        hub.set_sink(Some(slot.clone()), Duration::from_secs(2))
            .unwrap();
        assert_eq!(hub.sink_status().dataset_slot_id, first.slot_id);
        assert!(hub.sink_status().generation > first_generation);
        let error = hub
            .capture_for_dataset(
                "sim-signal://stale-sink",
                100,
                vec![],
                limits(1000, 10, 10000),
                first.slot_id.as_deref().unwrap(),
                Some(first_generation),
            )
            .unwrap_err();
        assert!(error.to_string().contains("sink instance changed"));
        assert!(hub.status(false).is_empty());
        let current = hub.sink_status();
        let error = crate::dataset::coordinator::run(
            &slot,
            &hub,
            SessionCommand::DatasetCloseIf {
                slot_id: first.slot_id.clone().unwrap(),
                sink_generation: Some(first_generation),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("sink instance changed"));
        assert_eq!(hub.sink_status(), current);
        assert_eq!(slot.status_cached().unwrap().slot_id, first.slot_id);
        slot.close().unwrap();
        assert!(hub.sink_status().attached);
        assert_eq!(hub.sink_status().dataset_slot_id, None);
        let second = slot
            .open(&path, crate::dataset::OpenMode::Open, None)
            .unwrap();
        assert_ne!(second.slot_id, first.slot_id);
        assert_eq!(hub.sink_status().dataset_slot_id, second.slot_id);

        hub.set_sink(
            Some(Arc::new(MemorySink::default())),
            Duration::from_secs(2),
        )
        .unwrap();
        let foreign = hub.sink_status();
        assert!(foreign.attached);
        assert_eq!(foreign.dataset_slot_id, None);
        assert_eq!(foreign.error, None);
        assert_eq!(foreign.dropped_total, 0);
        hub.set_sink(None, Duration::from_secs(2)).unwrap();
        let detached = hub.sink_status();
        assert!(!detached.attached);
        assert_eq!(detached.dataset_slot_id, None);
        assert!(detached.generation > foreign.generation);
        assert_eq!(detached.error, None);
        assert_eq!(detached.dropped_total, 0);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_capture_does_not_report_completion_ahead_of_a_stalled_flush() {
        struct GatedSink {
            gate: Mutex<()>,
            entered: AtomicBool,
            written: AtomicU64,
        }
        impl SampleSink for GatedSink {
            fn register(&self, _: &SignalInfo, _: &[TagInfo]) -> Result<()> {
                Ok(())
            }
            fn write(&self, _: &str, batch: &[Sample]) -> Result<()> {
                self.entered.store(true, Ordering::SeqCst);
                drop(lock(&self.gate));
                self.written.fetch_add(batch.len() as u64, Ordering::SeqCst);
                Ok(())
            }
            fn finish(&self) -> Result<()> {
                Ok(())
            }
            fn describe(&self) -> String {
                "gated".into()
            }
        }
        let hub = SignalHub::new();
        let sink = Arc::new(GatedSink {
            gate: Mutex::new(()),
            entered: AtomicBool::new(false),
            written: AtomicU64::new(0),
        });
        hub.set_sink(Some(sink.clone()), Duration::from_secs(2))
            .unwrap();
        let gate = lock(&sink.gate);
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://stalled", limits(100, 100, 10000));
        writer.write_all(samples_line(0, 2).as_bytes()).unwrap();
        wait(|| sink.entered.load(Ordering::SeqCst));
        let status = source.status(32, false);
        assert_eq!(status.completed_ms, None);
        assert_eq!(status.completion, None);
        assert!(!status.persistence_complete);
        assert_eq!(status.written, 0);
        drop(gate);
        wait(|| source.status(32, false).completed_ms.is_some());
        let status = source.status(32, false);
        assert_eq!(status.completion, Some(CaptureCompletion::Duration));
        assert!(status.persistence_complete);
        assert_eq!(status.written, 2);
        assert_eq!(sink.written.load(Ordering::SeqCst), 2);
        drop(writer);

        // A sink can stall longer than the bounded final flush. Report a
        // failed, incomplete result; a late write cannot turn it into success.
        let gate = lock(&sink.gate);
        sink.entered.store(false, Ordering::SeqCst);
        let (source, mut writer) =
            bounded_pipe(&hub, "sim-signal://flush-timeout", limits(100, 100, 10000));
        writer.write_all(samples_line(0, 2).as_bytes()).unwrap();
        wait(|| sink.entered.load(Ordering::SeqCst));
        wait(|| source.status(32, false).completed_ms.is_some());
        let status = source.status(32, false);
        assert_eq!(status.state, SourceState::Failed);
        assert!(!status.persistence_complete);
        assert_eq!(status.samples, 2);
        assert_eq!(status.written, 0);
        assert!(status.persistence_error.unwrap().contains("timed out"));
        drop(gate);
        wait(|| source.status(32, false).written == 2);
        assert!(!source.status(32, false).persistence_complete);
        drop(writer);
    }

    #[test]
    fn without_sink_keeps_latest_only() {
        let hub = SignalHub::new();
        let (reader, mut writer) = std::io::pipe().unwrap();
        let source = hub.start_with_reader("sim-signal://a", std::io::BufReader::new(reader));
        writer
            .write_all(b"{\"t\":\"ready\",\"tags\":[{\"tag\":\"t0\",\"unit\":\"degC\"}]}\n")
            .unwrap();
        writer.write_all(samples_line(0, 600).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::Relaxed) == 600);
        assert!(!hub.has_sink());
        let status = hub.status(true);
        assert_eq!(status[0].state, SourceState::Running);
        assert_eq!(status[0].written, 0);
        assert_eq!(status[0].dropped, 0);
        assert_eq!(status[0].tags.len(), 3);
        assert_eq!(status[0].tags[0].unit.as_deref(), Some("degC"));
        assert_eq!(status[0].tags[0].recent.as_ref().unwrap().len(), RECENT);
        let mut without_history = status;
        for tag in &mut without_history[0].tags {
            tag.recent = None;
        }
        // A hidden GUI still receives all latest values and counters. Only
        // historical point vectors are omitted from its cached status.
        assert_eq!(hub.status(false), without_history);
        assert_eq!(hub.status_brief(), without_history);
        assert_eq!(hub.latest_values().len(), 3);
        drop(writer);
        let stopped = hub.stop("all", Duration::from_secs(2)).unwrap();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].state, SourceState::Stopped);
        assert!(hub.status(false).is_empty());
        assert!(hub.stop("all", Duration::from_secs(1)).unwrap().is_empty());
        let error = hub
            .stop("signal:0000000000000000", Duration::from_secs(1))
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
    }

    #[test]
    fn sink_swap_follows_queued_batches() {
        let hub = SignalHub::new();
        let first = Arc::new(MemorySink::default());
        hub.set_sink(Some(first.clone()), Duration::from_secs(5))
            .unwrap();
        assert!(hub.has_sink());
        let (reader, mut writer) = std::io::pipe().unwrap();
        let source = hub.start_with_reader("sim-signal://b", std::io::BufReader::new(reader));
        writer.write_all(samples_line(0, 50).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::Relaxed) == 50);
        let second = Arc::new(MemorySink::default());
        hub.set_sink(Some(second.clone()), Duration::from_secs(5))
            .unwrap();
        assert_eq!(lock(&first.written).len(), 50);
        assert!(first.finished.load(Ordering::SeqCst));
        assert_eq!(
            lock(&second.registered).as_slice(),
            std::slice::from_ref(&source.info.id)
        );
        writer.write_all(samples_line(50, 10).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::Relaxed) == 60);
        hub.flush(Duration::from_secs(5)).unwrap();
        assert_eq!(lock(&second.written).len(), 10);
        assert_eq!(source.written.load(Ordering::Relaxed), 60);
        second.fail.store(true, Ordering::SeqCst);
        writer.write_all(samples_line(60, 5).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::Relaxed) == 65);
        hub.flush(Duration::from_secs(5)).unwrap();
        assert_eq!(hub.dropped_total(), 5);
        assert!(hub.sink_error().unwrap().contains("Storage full"));
        drop(writer);
        hub.stop("all", Duration::from_secs(2)).unwrap();
        hub.set_sink(None, Duration::from_secs(5)).unwrap();
        assert!(!hub.has_sink());
        assert!(second.finished.load(Ordering::SeqCst));
    }

    #[test]
    fn full_queue_counts_drops_without_blocking() {
        let hub = SignalHub::new();
        // A sink that blocks the writer until released.
        struct Slow(Mutex<()>);
        impl SampleSink for Slow {
            fn register(&self, _: &SignalInfo, _: &[TagInfo]) -> Result<()> {
                Ok(())
            }
            fn write(&self, _: &str, _: &[Sample]) -> Result<()> {
                drop(lock(&self.0));
                Ok(())
            }
            fn finish(&self) -> Result<()> {
                Ok(())
            }
            fn describe(&self) -> String {
                "slow".into()
            }
        }
        let slow = Arc::new(Slow(Mutex::new(())));
        hub.set_sink(Some(slow.clone()), Duration::from_secs(5))
            .unwrap();
        let gate = lock(&slow.0);
        let (reader, mut writer) = std::io::pipe().unwrap();
        let source = hub.start_with_reader("sim-signal://c", std::io::BufReader::new(reader));
        // The writer blocks on its first coalesced write; later batches
        // fill the queue and then count as drops.
        writer.write_all(samples_line(0, 1).as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::Relaxed) == 1);
        thread::sleep(COALESCE + Duration::from_millis(300));
        let line = samples_line(1, 1);
        let total = QUEUE + 50;
        let feed = thread::spawn(move || {
            for _ in 0..total {
                writer.write_all(line.as_bytes()).unwrap();
            }
            writer
        });
        wait(|| source.samples.load(Ordering::Relaxed) == total as u64 + 1);
        assert!(source.dropped.load(Ordering::Relaxed) > 0);
        assert_eq!(hub.dropped_total(), source.dropped.load(Ordering::Relaxed));
        drop(gate);
        drop(feed.join().unwrap());
        hub.stop("all", Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn caps_tags_and_rejects_duplicates() {
        let hub = SignalHub::new();
        let (reader, mut writer) = std::io::pipe().unwrap();
        let source = hub.start_with_reader("sim-signal://d", std::io::BufReader::new(reader));
        let samples: Vec<Sample> = (0..MAX_TAGS + 10)
            .map(|i| Sample::number(&format!("tag{i:05}"), i as f64))
            .collect();
        let line = format!(
            "{}\n",
            serde_json::to_string(&Line::Samples { samples }).unwrap()
        );
        writer.write_all(line.as_bytes()).unwrap();
        wait(|| source.samples.load(Ordering::Relaxed) > 0);
        let status = hub.status(false);
        assert_eq!(status[0].tags_total, MAX_TAGS as u64);
        assert_eq!(status[0].tags.len(), 256);
        assert!(status[0].tags_truncated);
        assert_eq!(hub.status_brief()[0].tags.len(), 32);
        let error = hub
            .start("sim-signal://d", 1000, Vec::new(), 1000)
            .unwrap_err();
        assert!(error.to_string().contains("is already logging"));
        drop(writer);
        hub.stop(&source.info.id, Duration::from_secs(2)).unwrap();
    }
}
