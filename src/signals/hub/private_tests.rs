use super::*;
use crate::signals::{Quality, Value as SignalValue};
use std::io::{BufReader, Write};

#[derive(Default)]
struct Sink {
    rows: Mutex<Vec<(String, f64)>>,
    registrations: Mutex<Vec<String>>,
    events: Mutex<Vec<(String, String)>>,
    finishes: AtomicU64,
    fail_write: AtomicBool,
    fail_finish: AtomicBool,
    fail_event: AtomicBool,
    fail_register: AtomicBool,
}
impl SampleSink for Sink {
    fn dataset_slot_id(&self) -> Option<String> {
        Some("private-test-slot".into())
    }
    fn register(&self, source: &SignalInfo, _: &[TagInfo]) -> Result<()> {
        if self.fail_register.load(Ordering::SeqCst) {
            bail!("registration denied");
        }
        lock(&self.registrations).push(source.id.clone());
        Ok(())
    }
    fn write(&self, id: &str, batch: &[Sample]) -> Result<()> {
        if self.fail_write.load(Ordering::SeqCst) {
            bail!("private quota exhausted");
        }
        lock(&self.rows).extend(
            batch
                .iter()
                .map(|sample| (id.into(), sample.value.as_f64().unwrap())),
        );
        Ok(())
    }
    fn event(&self, kind: &str, id: &str, _: &str) -> Result<()> {
        if self.fail_event.load(Ordering::SeqCst) {
            bail!("event quota exhausted");
        }
        lock(&self.events).push((kind.into(), id.into()));
        Ok(())
    }
    fn finish(&self) -> Result<()> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        if self.fail_finish.load(Ordering::SeqCst) {
            bail!("checkpoint quota exhausted");
        }
        Ok(())
    }
    fn describe(&self) -> String {
        "private test".into()
    }
}

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(5));
    }
}
fn limits(samples: u64) -> CaptureLimits {
    CaptureLimits {
        duration_ms: 5000,
        max_samples: samples,
        max_bytes: 1 << 20,
    }
}
fn sample(value: f64) -> String {
    let line = Line::Samples {
        samples: vec![Sample {
            ts_ms: 1,
            source_ts_ms: None,
            tag: "value".into(),
            value: SignalValue::Number(value),
            quality: Quality::Good,
        }],
    };
    format!("{}\n", serde_json::to_string(&line).unwrap())
}
fn private_pipe(
    hub: &SignalHub,
    route: &PrivateRoute,
    uri: &str,
    count: u64,
) -> (Arc<Source>, std::io::PipeWriter) {
    let (reader, writer) = std::io::pipe().unwrap();
    (
        hub.capture_private_with_reader(route, uri, limits(count), BufReader::new(reader))
            .unwrap(),
        writer,
    )
}
fn shared_pipe(hub: &SignalHub, uri: &str) -> (Arc<Source>, std::io::PipeWriter) {
    let (reader, writer) = std::io::pipe().unwrap();
    (hub.start_with_reader(uri, BufReader::new(reader)), writer)
}
fn ready(writer: &mut impl Write) {
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&Line::Ready {
            tags: vec![TagInfo {
                tag: "value".into(),
                unit: None,
                kind: None,
            }]
        })
        .unwrap()
    )
    .unwrap();
}

#[test]
fn private_route_keeps_exact_rows_and_metadata_across_shared_sink_swaps() {
    let hub = SignalHub::new();
    let shared = Arc::new(Sink::default());
    let replacement = Arc::new(Sink::default());
    let private = Arc::new(Sink::default());
    hub.set_sink(Some(shared.clone()), Duration::from_secs(1))
        .unwrap();
    let route = hub.private_route(private.clone()).unwrap();
    let (human, mut human_writer) = shared_pipe(&hub, "sim-signal://route-human");
    let (owned, mut owned_writer) = private_pipe(&hub, &route, "sim-signal://route-owned", 2);
    ready(&mut human_writer);
    ready(&mut owned_writer);
    owned_writer.write_all(sample(10.0).as_bytes()).unwrap();
    human_writer.write_all(sample(1.0).as_bytes()).unwrap();
    wait(|| owned.samples.load(Ordering::SeqCst) == 1 && human.samples.load(Ordering::SeqCst) == 1);
    let gap = Line::State {
        state: "disconnected".into(),
        error: Some("test gap".into()),
    };
    writeln!(owned_writer, "{}", serde_json::to_string(&gap).unwrap()).unwrap();
    wait(|| lock(&private.events).len() == 1);
    hub.set_sink(Some(replacement.clone()), Duration::from_secs(1))
        .unwrap();
    assert!(!owned.stop.load(Ordering::SeqCst));
    assert!(!lock(&owned.capture).closed);
    human_writer.write_all(sample(2.0).as_bytes()).unwrap();
    owned_writer.write_all(sample(20.0).as_bytes()).unwrap();
    wait(|| {
        owned.status(0, false).completed_ms.is_some() && human.samples.load(Ordering::SeqCst) == 2
    });
    hub.flush(Duration::from_secs(1)).unwrap();
    assert!(owned.status(0, false).persistence_complete);
    assert_eq!(
        *lock(&private.rows),
        vec![(owned.info.id.clone(), 10.0), (owned.info.id.clone(), 20.0)]
    );
    assert_eq!(*lock(&shared.rows), vec![(human.info.id.clone(), 1.0)]);
    assert_eq!(*lock(&replacement.rows), vec![(human.info.id.clone(), 2.0)]);
    assert_eq!(
        *lock(&private.registrations),
        vec![owned.info.id.clone(), owned.info.id.clone()]
    );
    assert!(
        lock(&shared.registrations)
            .iter()
            .all(|id| id == &human.info.id)
    );
    assert!(
        lock(&replacement.registrations)
            .iter()
            .all(|id| id == &human.info.id)
    );
    assert_eq!(
        *lock(&private.events),
        vec![("signal_gap".into(), owned.info.id.clone())]
    );
    assert!(lock(&shared.events).is_empty() && lock(&replacement.events).is_empty());
    assert_eq!(owned.dataset_slot.as_deref(), Some("private-test-slot"));
    drop(owned_writer);
    route.close(Duration::from_secs(1)).unwrap();
    assert!(route.is_closed());
    assert_eq!(private.finishes.load(Ordering::SeqCst), 1);
    drop(human_writer);
    hub.stop("all", Duration::from_secs(1)).unwrap();
}

struct BlockedSink {
    inner: Sink,
    entered: mpsc::Sender<()>,
    resume: Mutex<Option<mpsc::Receiver<()>>>,
}
impl SampleSink for BlockedSink {
    fn register(&self, source: &SignalInfo, tags: &[TagInfo]) -> Result<()> {
        self.inner.register(source, tags)
    }
    fn write(&self, id: &str, batch: &[Sample]) -> Result<()> {
        let pause = lock(&self.resume).take();
        if let Some(pause) = pause {
            self.entered.send(()).unwrap();
            pause.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        self.inner.write(id, batch)
    }
    fn finish(&self) -> Result<()> {
        self.inner.finish()
    }
    fn describe(&self) -> String {
        "blocked private test".into()
    }
}

#[test]
fn private_stalled_writer_does_not_block_shared_and_uncertain_close_retains_sink() {
    let hub = SignalHub::new();
    let shared = Arc::new(Sink::default());
    hub.set_sink(Some(shared.clone()), Duration::from_secs(1))
        .unwrap();
    let (entered, observed) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    let private = Arc::new(BlockedSink {
        inner: Sink::default(),
        entered,
        resume: Mutex::new(Some(paused)),
    });
    let route = hub.private_route(private.clone()).unwrap();
    let (owned, mut owned_writer) = private_pipe(&hub, &route, "sim-signal://blocked-owned", 1);
    owned_writer.write_all(sample(10.0).as_bytes()).unwrap();
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    let (human, mut human_writer) = shared_pipe(&hub, "sim-signal://unblocked-human");
    human_writer.write_all(sample(1.0).as_bytes()).unwrap();
    wait(|| human.samples.load(Ordering::SeqCst) == 1);
    hub.flush(Duration::from_millis(250)).unwrap();
    assert_eq!(*lock(&shared.rows), vec![(human.info.id.clone(), 1.0)]);
    assert!(route.close(Duration::from_millis(20)).is_err());
    assert!(!route.is_closed());
    assert!(route.status().attached);
    assert_eq!(private.inner.finishes.load(Ordering::SeqCst), 0);
    assert!(lock(&hub.inner.private_routes).contains_key(&route.status().generation));
    assert_eq!(hub.dropped_total(), 0);
    assert!(hub.sink_error().is_none());
    resume.send(()).unwrap();
    drop(owned_writer);
    wait(|| owned.reader_done.load(Ordering::Acquire));
    route.close(Duration::from_secs(1)).unwrap();
    assert!(route.is_closed());
    assert!(!route.status().attached);
    assert_eq!(private.inner.finishes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *lock(&private.inner.rows),
        vec![(owned.info.id.clone(), 10.0)]
    );
    drop(human_writer);
    hub.stop("all", Duration::from_secs(1)).unwrap();
}

#[test]
fn private_failures_are_local_and_finish_errors_still_prove_closed() {
    for kind in ["write", "register", "event", "finish"] {
        let hub = SignalHub::new();
        let shared = Arc::new(Sink::default());
        hub.set_sink(Some(shared), Duration::from_secs(1)).unwrap();
        let private = Arc::new(Sink::default());
        match kind {
            "write" => private.fail_write.store(true, Ordering::SeqCst),
            "register" => private.fail_register.store(true, Ordering::SeqCst),
            "event" => private.fail_event.store(true, Ordering::SeqCst),
            "finish" => private.fail_finish.store(true, Ordering::SeqCst),
            _ => unreachable!(),
        }
        let route = hub.private_route(private.clone()).unwrap();
        let (source, mut writer) = private_pipe(&hub, &route, "sim-signal://failed-owned", 1);
        if kind == "event" {
            let gap = Line::State {
                state: "disconnected".into(),
                error: None,
            };
            writeln!(writer, "{}", serde_json::to_string(&gap).unwrap()).unwrap();
        } else if kind != "register" {
            writer.write_all(sample(1.0).as_bytes()).unwrap();
        }
        wait(|| source.status(0, false).completed_ms.is_some());
        drop(writer);
        let status = source.status(0, false);
        if kind != "finish" {
            assert!(
                !status.persistence_complete && status.persistence_error.is_some(),
                "{kind}: {status:?}"
            );
        }
        assert_eq!(hub.dropped_total(), 0);
        assert!(hub.sink_error().is_none());
        assert_eq!(
            route.status().dropped_total,
            if kind == "write" { 1 } else { 0 }
        );
        let result = route.close(Duration::from_secs(1));
        assert_eq!(result.is_err(), kind == "finish");
        assert!(route.is_closed());
        assert!(!route.status().attached);
        assert!(route.status().error.is_some());
        assert_eq!(private.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(
            route.close(Duration::from_millis(1)).is_err(),
            kind == "finish"
        );
        assert_eq!(private.finishes.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn private_queue_overload_counts_only_its_own_samples_and_releases_reservations() {
    let hub = SignalHub::new();
    let shared = Arc::new(Sink::default());
    hub.set_sink(Some(shared), Duration::from_secs(1)).unwrap();
    let (entered, observed) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    let private = Arc::new(BlockedSink {
        inner: Sink::default(),
        entered,
        resume: Mutex::new(Some(paused)),
    });
    let route = hub.private_route(private.clone()).unwrap();
    let (source, mut writer) = private_pipe(&hub, &route, "sim-signal://overloaded-private", 5000);
    writer.write_all(sample(1.0).as_bytes()).unwrap();
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    // The private writer is stalled inside its first write. More than its
    // bounded queue depth must close this capture rather than block a reader.
    let overflow = sample(2.0).repeat(QUEUE + 10);
    let _ = writer.write_all(overflow.as_bytes());
    wait(|| source.dropped.load(Ordering::SeqCst) > 0);
    assert_eq!(
        source.dropped.load(Ordering::SeqCst),
        route.status().dropped_total
    );
    assert!(route.status().error.is_some());
    assert_eq!(hub.dropped_total(), 0);
    assert!(hub.sink_error().is_none());
    assert_eq!(hub.inner.queued_bytes.load(Ordering::SeqCst), 0);
    resume.send(()).unwrap();
    drop(writer);
    wait(|| source.reader_done.load(Ordering::Acquire));
    route.close(Duration::from_secs(1)).unwrap();
    assert!(!source.status(0, false).persistence_complete);
    assert_eq!(route.inner.queued_bytes.load(Ordering::SeqCst), 0);
    assert_eq!(
        private.inner.rows.lock().unwrap().len() as u64 + source.dropped.load(Ordering::SeqCst),
        source.samples.load(Ordering::SeqCst)
    );
}

#[test]
fn private_close_requires_startup_and_reader_fences_and_rejects_late_metadata() {
    let hub = SignalHub::new();
    let sink = Arc::new(Sink::default());
    let route = hub.private_route(sink.clone()).unwrap();
    let starting = route.start(&hub.inner).unwrap();
    assert!(route.close(Duration::from_millis(10)).is_err());
    assert!(!route.is_closed());
    assert!(route.status().attached);
    drop(starting);
    route.close(Duration::from_secs(1)).unwrap();
    assert!(route.is_closed());

    let route = hub.private_route(sink.clone()).unwrap();
    let (source, mut writer) = private_pipe(&hub, &route, "sim-signal://reader-owned", 2);
    let (entered, observed) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    *lock(&source.reader_line_gate) = Some(ReaderGate {
        entered,
        resume: paused,
    });
    writer.write_all(sample(99.0).as_bytes()).unwrap();
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    route.begin_close();
    assert!(source.stop.load(Ordering::SeqCst));
    assert!(route.close(Duration::from_millis(10)).is_err());
    assert!(!route.is_closed());
    assert_eq!(sink.finishes.load(Ordering::SeqCst), 1);
    resume.send(()).unwrap();
    drop(writer);
    wait(|| source.reader_done.load(Ordering::Acquire));
    route.close(Duration::from_secs(1)).unwrap();
    assert!(route.is_closed());
    assert!(lock(&sink.rows).is_empty());
    assert_eq!(source.samples.load(Ordering::SeqCst), 0);
    assert_eq!(sink.finishes.load(Ordering::SeqCst), 2);
}

#[test]
fn private_restart_events_and_replacement_keep_their_original_routes() {
    let hub = SignalHub::new();
    let other_hub = SignalHub::new();
    let shared = Arc::new(Sink::default());
    hub.set_sink(Some(shared.clone()), Duration::from_secs(1))
        .unwrap();
    let first = Arc::new(Sink::default());
    let route = hub.private_route(first.clone()).unwrap();
    assert!(route.start(&other_hub.inner).is_err());
    let (source, mut writer) = private_pipe(&hub, &route, "sim-signal://restart-owned", 10);
    let capture_id = source.capture_id.clone();
    ready(&mut writer);
    writer.write_all(sample(1.0).as_bytes()).unwrap();
    drop(writer);
    wait(|| source.restarts.load(Ordering::SeqCst) == 1);
    wait(|| lock(&first.events).len() == 1);
    assert_eq!(source.capture_id, capture_id);
    assert!(
        hub.capture_to_private(&route, &source.spec.uri, 100, vec![], limits(1), None)
            .unwrap_err()
            .to_string()
            .contains("already logging")
    );
    hub.set_sink(None, Duration::from_secs(1)).unwrap();
    assert!(!source.stop.load(Ordering::SeqCst));
    route.close(Duration::from_secs(1)).unwrap();
    let second = Arc::new(Sink::default());
    let replacement_route = hub.private_route(second.clone()).unwrap();
    let (replacement, mut writer) = private_pipe(&hub, &replacement_route, &source.spec.uri, 1);
    assert_ne!(replacement.capture_id, capture_id);
    assert!(
        hub.stop_if(&replacement.info.id, &capture_id, Duration::from_millis(10))
            .is_err()
    );
    writer.write_all(sample(2.0).as_bytes()).unwrap();
    wait(|| replacement.status(0, false).completed_ms.is_some());
    drop(writer);
    replacement_route.close(Duration::from_secs(1)).unwrap();
    assert_eq!(*lock(&first.rows), vec![(source.info.id.clone(), 1.0)]);
    assert_eq!(*lock(&second.rows), vec![(source.info.id.clone(), 2.0)]);
    assert_eq!(
        *lock(&first.events),
        vec![("signal_error".into(), source.info.id.clone())]
    );
    assert!(lock(&shared.rows).is_empty() && lock(&shared.events).is_empty());
}

#[test]
fn private_task_deadline_refuses_oversized_and_expired_capture_before_contact() {
    let hub = SignalHub::new();
    let sink = Arc::new(Sink::default());
    let route = hub
        .private_route_until(sink.clone(), Instant::now() + Duration::from_millis(50))
        .unwrap();
    let error = hub
        .capture_to_private(
            &route,
            "sim-signal://too-long-private",
            100,
            vec![],
            limits(1),
            None,
        )
        .unwrap_err();
    assert!(error.to_string().contains("remaining task deadline"));
    assert!(!hub.has_sources());
    thread::sleep(Duration::from_millis(60));
    assert!(route.start(&hub.inner).is_err());
    assert!(
        hub.private_route_until(sink.clone(), Instant::now() - Duration::from_millis(1))
            .is_err()
    );
    assert!(lock(&sink.rows).is_empty());
    assert!(lock(&sink.registrations).is_empty());
    route.close(Duration::from_secs(1)).unwrap();
}

#[test]
fn private_writer_records_quota_failure_after_the_coordinator_is_gone() {
    let hub = SignalHub::new();
    let (entered, observed) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    let sink = Arc::new(BlockedSink {
        inner: Sink::default(),
        entered,
        resume: Mutex::new(Some(paused)),
    });
    sink.inner.fail_write.store(true, Ordering::SeqCst);
    let route = hub.private_route(sink.clone()).unwrap();
    let starting = route.start(&hub.inner).unwrap();
    let source = hub
        .insert_config(
            "sim-signal://owner-gone",
            StartConfig {
                interval_ms: 100,
                tags: Vec::new(),
                timeout_ms: 1000,
                limits: Some(limits(1)),
                auth: None,
                persistence: PersistenceRoute::Private(route.clone()),
            },
        )
        .unwrap();
    let Line::Samples { samples } = serde_json::from_str::<Line>(&sample(9.0)).unwrap() else {
        unreachable!();
    };
    // Exercise the real admission and writer queue without a reader/supervisor
    // holding the coordinator alive. The sink owns the in-flight write.
    ingest(
        &hub.inner,
        &source,
        &route.queue(),
        &mut Vec::new(),
        samples,
    );
    drop(starting);
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    let owner = Arc::downgrade(&hub.inner);
    drop(hub);
    assert!(owner.upgrade().is_none());
    resume.send(()).unwrap();
    wait(|| source.dropped.load(Ordering::SeqCst) == 1);
    assert_eq!(source.written.load(Ordering::SeqCst), 0);
    assert_eq!(route.status().dropped_total, 1);
    assert!(
        route
            .status()
            .error
            .unwrap()
            .contains("private quota exhausted")
    );
    route.close(Duration::from_secs(1)).unwrap();
    assert!(route.is_closed());
    assert_eq!(sink.inner.finishes.load(Ordering::SeqCst), 1);
}
