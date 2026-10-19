//! Local persistence capabilities. Nothing in this module is a wire command.
use super::*;
use std::sync::TryLockError;

static NEXT_ROUTE: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct State {
    closing: bool,
    closed: bool,
    finish_error: Option<String>,
    starting: usize,
    sources: Vec<Weak<Source>>,
}

pub(super) struct PrivateInner {
    owner: Weak<HubInner>,
    id: u64,
    deadline: Option<Instant>,
    slot: Option<String>,
    sink: Mutex<Option<Arc<dyn SampleSink>>>,
    queue: SyncSender<HubMsg>,
    pub(super) queued_bytes: AtomicU64,
    dropped: AtomicU64,
    error: Mutex<Option<String>>,
    state: Mutex<State>,
    close_lock: Mutex<()>,
}

/// A coordinator-local capability for one immutable sink and writer queue.
/// Public task/actor/source IDs cannot construct or select this object.
#[derive(Clone)]
pub(crate) struct PrivateRoute {
    pub(super) inner: Arc<PrivateInner>,
}

pub(super) struct Starting(Arc<PrivateInner>);
impl Drop for Starting {
    fn drop(&mut self) {
        lock(&self.0.state).starting -= 1;
    }
}

impl PrivateRoute {
    pub(super) fn create(
        hub: &Arc<HubInner>,
        sink: Arc<dyn SampleSink>,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            bail!("private signal persistence deadline expired");
        }
        let mut routes = lock(&hub.private_routes);
        if routes.len() >= MAX_SOURCES {
            bail!("private signal persistence route limit reached");
        }
        let (queue, receiver) = mpsc::sync_channel(QUEUE);
        let inner = Arc::new(PrivateInner {
            owner: Arc::downgrade(hub),
            id: NEXT_ROUTE.fetch_add(1, Ordering::SeqCst) | (1 << 63),
            deadline,
            slot: sink.dataset_slot_id(),
            sink: Mutex::new(Some(sink.clone())),
            queue,
            queued_bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            error: Mutex::default(),
            state: Mutex::default(),
            close_lock: Mutex::default(),
        });
        let owner = Arc::downgrade(hub);
        let route = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("capturefab-private-signal-writer".into())
            .spawn(move || writer(owner, receiver, Some((route, sink))))?;
        routes.insert(inner.id, inner.clone());
        Ok(Self { inner })
    }

    pub(super) fn start(&self, hub: &Arc<HubInner>) -> Result<Starting> {
        if !self.inner.owner.ptr_eq(&Arc::downgrade(hub)) {
            bail!("private signal persistence route belongs to another coordinator");
        }
        let mut state = lock(&self.inner.state);
        if state.closing || state.closed {
            bail!("private signal persistence route is closing or closed");
        }
        self.require_deadline(Instant::now())?;
        if lock(&self.inner.error).is_some() {
            bail!("private signal persistence route has failed; inspect its storage status");
        }
        state.starting += 1;
        Ok(Starting(self.inner.clone()))
    }

    pub(super) fn track(&self, source: &Arc<Source>) -> Result<()> {
        let mut state = lock(&self.inner.state);
        if state.closing || state.closed {
            bail!("private signal persistence route closed before capture started");
        }
        self.require_deadline(source.deadline.expect("private captures are finite"))?;
        state.sources.retain(|source| source.strong_count() > 0);
        if state.sources.len() >= MAX_SOURCES {
            bail!("private signal capture instance limit reached; finish earlier readers first");
        }
        state.sources.push(Arc::downgrade(source));
        Ok(())
    }

    fn require_deadline(&self, capture_end: Instant) -> Result<()> {
        if self
            .inner
            .deadline
            .is_some_and(|deadline| capture_end > deadline || Instant::now() >= deadline)
        {
            bail!("private signal capture exceeds the remaining task deadline");
        }
        Ok(())
    }

    pub(super) fn queue(&self) -> SyncSender<HubMsg> {
        self.inner.queue.clone()
    }
    pub(super) fn slot(&self) -> Option<String> {
        self.inner.slot.clone()
    }
    pub(super) fn has_sink(&self) -> bool {
        lock(&self.inner.sink).is_some()
    }
    pub(super) fn reserve(&self, bytes: u64) -> bool {
        reserve_bytes(&self.inner.queued_bytes, bytes)
    }
    pub(super) fn unreserve(&self, bytes: u64) {
        self.inner.queued_bytes.fetch_sub(bytes, Ordering::SeqCst);
    }
    pub(super) fn dropped(&self, count: u64) {
        self.inner.dropped.fetch_add(count, Ordering::SeqCst);
    }
    pub(super) fn error(&self, message: String) {
        *lock(&self.inner.error) = Some(message);
    }
    pub(super) fn request(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<()>>) -> HubMsg,
        timeout: Duration,
        what: &str,
    ) -> Result<()> {
        request_queue(&self.inner.queue, make, timeout, what)
    }

    pub(crate) fn status(&self) -> super::super::SignalSinkStatus {
        super::super::SignalSinkStatus {
            attached: self.has_sink(),
            generation: self.inner.id,
            dataset_slot_id: self.inner.slot.clone(),
            error: lock(&self.inner.error).clone(),
            dropped_total: self.inner.dropped.load(Ordering::SeqCst),
        }
    }

    /// True only after startup and reader fences and an acknowledged writer
    /// finish. A failed finish is safe to reclaim; a timeout is not.
    pub(crate) fn is_closed(&self) -> bool {
        lock(&self.inner.state).closed
    }

    /// Close admission for every source before spending time draining any one
    /// writer. No helper I/O, reader join or sink operation happens here.
    pub(crate) fn begin_close(&self) {
        let sources = {
            let mut state = lock(&self.inner.state);
            state.closing = true;
            state
                .sources
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for source in sources {
            source.request_completion(CaptureCompletion::Stopped, None);
        }
    }

    /// A successful close proves that initial starts and readers ended and
    /// accepted writes reached this sink's finish barrier. Uncertainty retains
    /// the sink and its lease for explicit review or retry.
    pub(crate) fn close(&self, wait: Duration) -> Result<()> {
        let deadline = Instant::now() + wait;
        self.begin_close();
        let result = (|| {
            let _close = loop {
                match self.inner.close_lock.try_lock() {
                    Ok(guard) => break guard,
                    Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                    Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(TryLockError::WouldBlock) => bail!("private signal route close timed out"),
                }
            };
            if self.is_closed() {
                return match lock(&self.inner.state).finish_error.clone() {
                    Some(error) => Err(anyhow!(error)),
                    None => Ok(()),
                };
            }
            while lock(&self.inner.state).starting != 0 {
                if Instant::now() >= deadline {
                    bail!("private signal capture startup is still stopping");
                }
                thread::sleep(Duration::from_millis(5));
            }
            let sources = lock(&self.inner.state)
                .sources
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            if let Some(hub) = self.inner.owner.upgrade() {
                for source in &sources {
                    if lock(&source.capture).completed_ms.is_none() {
                        finish_capture(
                            &hub,
                            source,
                            deadline.saturating_duration_since(Instant::now()),
                        );
                    }
                }
            }
            for source in &sources {
                while !source.reader_done.load(Ordering::Acquire) {
                    if Instant::now() >= deadline {
                        bail!("private signal capture reader is still stopping");
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                if let Some(reader) = lock(&source.reader).take() {
                    let _ = reader.join();
                }
            }
            let (ack, done) = mpsc::channel();
            send_queue(&self.inner.queue, HubMsg::FinishPrivate(ack), deadline)?;
            // A received error proves the writer drained and released its
            // sink too. Failure to receive does not establish that boundary.
            let finished = done
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| anyhow!("private signal persistence finish timed out"))?;
            let finish_error = finished.as_ref().err().map(|error| format!("{error:#}"));
            if let Some(error) = &finish_error {
                self.error(error.clone());
            }
            lock(&self.inner.sink).take();
            if let Some(hub) = self.inner.owner.upgrade() {
                lock(&hub.private_routes).remove(&self.inner.id);
            }
            {
                let mut state = lock(&self.inner.state);
                state.finish_error = finish_error;
                state.closed = true;
            }
            finished
        })();
        if let Err(error) = &result {
            self.error(format!("{error:#}"));
        }
        result
    }
}

impl PrivateInner {
    pub(super) fn receive_bytes(&self, bytes: u64) {
        self.queued_bytes.fetch_sub(bytes, Ordering::SeqCst);
    }
}
