//! The coordinator's signal hub: starts and stops source helpers, keeps the
//! latest values per tag and hands samples to the attached `SampleSink`.
//!
//! Each source has a reader thread for its helper's stdout. One writer
//! thread owns the sink and receives batches, registrations, flushes and
//! sink swaps through a single ordered queue, so a swap always follows the
//! batches queued before it. Batches and registrations use `try_send`: a full
//! queue (or one holding `QUEUE_BYTES`) counts drops instead of blocking a
//! reader, and registrations wait in the reader until there is room.
use super::{
    Line, MAX_SOURCES, MAX_TAGS, Mode, Sample, SampleSink, SignalInfo, SignalSpec, SourceState,
    SourceStatus, TagInfo, TagStatus, parse,
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
    Register(SignalInfo, Vec<TagInfo>),
    Event(&'static str, String, String),
    Flush(mpsc::Sender<Result<()>>),
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

struct Source {
    info: SignalInfo,
    spec: SignalSpec,
    started_ms: u64,
    process: Mutex<Option<SignalProcess>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    phase: Mutex<Phase>,
    changed: Condvar,
    stop: AtomicBool,
    samples: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    helper_dropped: AtomicU64,
    restarts: AtomicU64,
    last_sample_ms: AtomicU64,
    helper_pid: AtomicU64,
    tags: Mutex<BTreeMap<String, TagState>>,
}

impl Source {
    fn set_phase(&self, state: SourceState, error: Option<String>) {
        let mut phase = lock(&self.phase);
        phase.state = state;
        if error.is_some() || state == SourceState::Running {
            phase.error = error;
        }
        phase.settled = true;
        self.changed.notify_all();
    }
    fn tag_infos(&self) -> Vec<TagInfo> {
        lock(&self.tags).values().map(|t| t.info.clone()).collect()
    }
    fn status(&self, max_tags: usize, history: bool) -> SourceStatus {
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
            protocol: self.info.protocol,
            uri: self.info.uri.clone(),
            interval_ms: self.info.interval_ms,
            state,
            started_ms: self.started_ms,
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
    sources: Mutex<BTreeMap<String, Arc<Source>>>,
    queue: OnceLock<SyncSender<HubMsg>>,
    has_sink: AtomicBool,
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
                    .spawn(move || writer(hub, receiver))
                    .expect("start signal writer");
                sender
            })
            .clone()
    }
    /// Send a control message without blocking forever on a full queue.
    fn send(self: &Arc<Self>, message: HubMsg, deadline: Instant) -> Result<()> {
        let queue = self.queue();
        let mut message = message;
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
    /// Send a message that the writer acknowledges, and wait for the ack.
    fn request(
        self: &Arc<Self>,
        make: impl FnOnce(mpsc::Sender<Result<()>>) -> HubMsg,
        timeout: Duration,
        what: &str,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let (ack, done) = mpsc::channel();
        self.send(make(ack), deadline)?;
        match done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => bail!("signal {what} timed out after {} ms", timeout.as_millis()),
        }
    }
}

/// The writer thread: owns the sink and coalesces batches per source.
struct Writer {
    hub: Weak<HubInner>,
    sink: Option<Arc<dyn SampleSink>>,
    pending: BTreeMap<String, (Arc<Source>, Vec<Sample>, Instant)>,
    last_error_log: Option<Instant>,
}

impl Writer {
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
            return;
        };
        for chunk in samples.chunks(SINK_BATCH) {
            let count = chunk.len() as u64;
            match sink.write(&source.info.id, chunk) {
                Ok(()) => {
                    source.written.fetch_add(count, Ordering::Relaxed);
                }
                Err(error) => {
                    source.dropped.fetch_add(count, Ordering::Relaxed);
                    let Some(hub) = self.hub.upgrade() else {
                        continue;
                    };
                    hub.dropped_total.fetch_add(count, Ordering::Relaxed);
                    let message = super::redact_text(&format!("{error:#}"), &source.spec.uri, None);
                    *lock(&hub.sink_error) = Some(message.clone());
                    if self
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
    fn set_sink(&mut self, sink: Option<Arc<dyn SampleSink>>) -> Result<()> {
        self.write_due(true);
        let finished = match self.sink.take() {
            Some(old) => old.finish(),
            None => Ok(()),
        };
        self.sink = sink;
        if let Some(hub) = self.hub.upgrade() {
            hub.has_sink.store(self.sink.is_some(), Ordering::SeqCst);
            if let Some(sink) = &self.sink {
                *lock(&hub.sink_error) = None;
                let sources: Vec<Arc<Source>> = lock(&hub.sources).values().cloned().collect();
                for source in sources {
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

fn writer(hub: Weak<HubInner>, receiver: mpsc::Receiver<HubMsg>) {
    let mut writer = Writer {
        hub,
        sink: None,
        pending: BTreeMap::new(),
        last_error_log: None,
    };
    loop {
        match receiver.recv_timeout(writer.next_due()) {
            Ok(HubMsg::Batch(source, samples, bytes)) => {
                if let Some(hub) = writer.hub.upgrade() {
                    hub.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
                }
                if writer.sink.is_some() {
                    writer
                        .pending
                        .entry(source.info.id.clone())
                        .or_insert_with(|| (source, Vec::new(), Instant::now()))
                        .1
                        .extend(samples);
                }
            }
            Ok(HubMsg::Register(info, tags)) => {
                if let Some(sink) = &writer.sink
                    && let Err(error) = sink.register(&info, &tags)
                    && let Some(hub) = writer.hub.upgrade()
                {
                    hub.log(
                        "error",
                        format!("Signal sink registration failed: {error:#}"),
                    );
                }
            }
            Ok(HubMsg::Event(kind, id, text)) => {
                if let Some(sink) = &writer.sink {
                    let _ = sink.event(kind, &id, &text);
                }
            }
            Ok(HubMsg::Flush(ack)) => {
                writer.write_due(true);
                let _ = ack.send(Ok(()));
            }
            Ok(HubMsg::SetSink(sink, ack)) => {
                let result = writer.set_sink(sink);
                let _ = ack.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = writer.set_sink(None);
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

/// Queue tags waiting to be registered without blocking the reader. They
/// stay pending while the queue is full.
fn register(queue: &SyncSender<HubMsg>, source: &Source, pending: &mut Vec<TagInfo>) -> bool {
    if pending.is_empty() {
        return true;
    }
    match queue.try_send(HubMsg::Register(
        source.info.clone(),
        std::mem::take(pending),
    )) {
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
    let queue = hub.queue();
    let mut unregistered: Vec<TagInfo> = Vec::new();
    let mut recent_restarts = 0u64;
    loop {
        let mut error: Option<String> = None;
        let started = Instant::now();
        loop {
            match read_line(&mut reader) {
                Ok(Some(Line::Samples { samples })) => {
                    if samples.is_empty() {
                        continue;
                    }
                    source.absorb(&samples);
                    if !hub.has_sink.load(Ordering::Relaxed) {
                        continue;
                    }
                    let count = samples.len() as u64;
                    let bytes = batch_bytes(&samples);
                    // New tags are registered before their samples, and a
                    // full queue drops batches rather than blocking.
                    let sent = register(&queue, &source, &mut unregistered)
                        && hub.queued_bytes.load(Ordering::Relaxed) + bytes <= QUEUE_BYTES
                        && queue
                            .try_send(HubMsg::Batch(source.clone(), samples, bytes))
                            .is_ok();
                    if sent {
                        hub.queued_bytes.fetch_add(bytes, Ordering::Relaxed);
                    } else {
                        source.dropped.fetch_add(count, Ordering::Relaxed);
                        hub.dropped_total.fetch_add(count, Ordering::Relaxed);
                    }
                }
                Ok(Some(Line::Ready { tags })) => {
                    source.merge_tags(&tags);
                    if lock(&source.phase).state != SourceState::Running {
                        source.set_phase(SourceState::Running, None);
                    }
                    unregistered.extend(tags);
                    register(&queue, &source, &mut unregistered);
                }
                Ok(Some(Line::State { state, error })) => {
                    if state == "connected" {
                        source.set_phase(SourceState::Running, None);
                    } else {
                        let text = error.unwrap_or_else(|| "connection lost".into());
                        hub.log(
                            "warn",
                            format!("Signal source {} is reconnecting: {text}", source.info.uri),
                        );
                        let _ = queue.try_send(HubMsg::Event(
                            "signal_gap",
                            source.info.id.clone(),
                            text.clone(),
                        ));
                        source.set_phase(SourceState::Reconnecting, Some(text));
                    }
                }
                Ok(Some(Line::Dropped { count })) => {
                    source.helper_dropped.fetch_add(count, Ordering::Relaxed);
                }
                Ok(Some(Line::Error { message })) => {
                    error = Some(message);
                    break;
                }
                Ok(Some(Line::Browse { .. })) => {}
                Ok(Some(Line::Done { .. })) | Ok(None) => break,
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        let process = lock(&source.process).take();
        if let Some(mut process) = process {
            process.wait_or_kill(Instant::now() + Duration::from_secs(2));
        }
        let stopping = || source.stop.load(Ordering::SeqCst) || hub.quitting.load(Ordering::SeqCst);
        if stopping() {
            source.set_phase(SourceState::Stopped, None);
            return;
        }
        let error = error.unwrap_or_else(|| "signal helper exited".into());
        if !lock(&source.phase).settled {
            source.set_phase(SourceState::Failed, Some(error));
            return;
        }
        let _ = queue.try_send(HubMsg::Event(
            "signal_error",
            source.info.id.clone(),
            error.clone(),
        ));
        source.restarts.fetch_add(1, Ordering::Relaxed);
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
            source.set_phase(SourceState::Failed, Some(error));
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
                source.set_phase(SourceState::Stopped, None);
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        match SignalProcess::spawn(&source.spec) {
            Ok((process, stdout)) => {
                source
                    .helper_pid
                    .store(process.pid() as u64, Ordering::Relaxed);
                let mut slot = lock(&source.process);
                *slot = Some(process);
                if stopping()
                    && let Some(process) = slot.as_mut()
                {
                    process.request_stop();
                }
                drop(slot);
                reader = Box::new(stdout);
            }
            Err(error) => {
                let error = format!("{error:#}");
                hub.log(
                    "error",
                    format!("Signal source {} failed: {error}", source.info.uri),
                );
                source.set_phase(SourceState::Failed, Some(error));
                return;
            }
        }
    }
}

pub struct SignalHub {
    inner: Arc<HubInner>,
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
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HubInner {
                sources: Mutex::default(),
                queue: OnceLock::new(),
                has_sink: AtomicBool::new(false),
                dropped_total: AtomicU64::new(0),
                queued_bytes: AtomicU64::new(0),
                sink_error: Mutex::default(),
                logs: Mutex::default(),
                quitting: AtomicBool::new(false),
            }),
        }
    }

    /// Register a new source under the source limit.
    fn insert(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        timeout_ms: u64,
    ) -> Result<Arc<Source>> {
        let parsed = parse(uri)?;
        super::require_compiled(parsed.protocol)?;
        super::validate_interval(interval_ms)?;
        super::validate_tags(&tags)?;
        if self.inner.quitting.load(Ordering::SeqCst) {
            bail!("signal logging is shutting down");
        }
        let id = super::source_id(uri);
        let source = Arc::new(Source {
            info: SignalInfo {
                id: id.clone(),
                protocol: parsed.protocol,
                uri: crate::media::redact_url(uri),
                interval_ms,
            },
            spec: SignalSpec {
                uri: uri.to_string(),
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
            phase: Mutex::new(Phase {
                state: SourceState::Connecting,
                error: None,
                settled: false,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            samples: AtomicU64::new(0),
            written: AtomicU64::new(0),
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
        let handle = thread::Builder::new()
            .name(name)
            .spawn(move || read_source(hub, follow, reader))?;
        *lock(&source.reader) = Some(handle);
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
        let source = self.insert(uri, interval_ms, tags, timeout_ms)?;
        let (process, stdout) = match SignalProcess::spawn(&source.spec) {
            Ok(started) => started,
            Err(error) => {
                self.remove(&source);
                return Err(error);
            }
        };
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
        while !phase.settled {
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
        let deadline = Instant::now() + wait;
        for source in &targets {
            source.stop.store(true, Ordering::SeqCst);
            if let Some(process) = lock(&source.process).as_mut() {
                process.request_stop();
            }
        }
        for source in &targets {
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
        if !targets.is_empty() && self.has_sink() {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1));
            let _ = self.flush(remaining);
        }
        Ok(targets
            .iter()
            .map(|source| {
                self.remove(source);
                {
                    let mut phase = lock(&source.phase);
                    phase.state = SourceState::Stopped;
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
            source.stop.store(true, Ordering::SeqCst);
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
        let source = self.insert(uri, 1000, Vec::new(), 1000).unwrap();
        self.follow(&source, Box::new(reader)).unwrap();
        source
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
        })),
        SessionCommand::SignalRead { uri, timeout_ms } => super::process::read(&uri, timeout_ms),
        SessionCommand::SignalBrowse {
            uri,
            duration_ms,
            limit,
        } => super::process::browse(&uri, duration_ms, limit),
        SessionCommand::SignalStart {
            uri,
            interval_ms,
            tags,
        } => Ok(serde_json::to_value(hub.start(
            &uri,
            interval_ms,
            tags,
            super::default_timeout_ms(),
        )?)?),
        SessionCommand::SignalStop { id } => {
            Ok(json!({"stopped": hub.stop(&id, Duration::from_secs(3))?}))
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
        assert!(hub.status_brief()[0].tags[0].recent.is_none());
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
