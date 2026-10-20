use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
enum Step {
    Frame(u32, u32),
    Skip,
    Fail(Failure),
    /// Wait until the session is cancelled.
    Block,
}

type Script = Arc<dyn Fn(usize, usize) -> Step + Send + Sync>;
type OpenScript = Arc<dyn Fn(usize, &Cx) -> Result<(), Failure> + Send + Sync>;

struct Fake {
    stream: Stream,
    opens: Arc<AtomicUsize>,
    open: OpenScript,
    step: Script,
    delay: Duration,
}

struct FakeSession {
    open: usize,
    count: usize,
    step: Script,
    delay: Duration,
    continuous: bool,
}

impl SourceFactory for Fake {
    fn stream(&self) -> Stream {
        self.stream
    }
    fn open(&self, cx: &Cx) -> Result<Box<dyn FrameSession>, Failure> {
        let open = self.opens.fetch_add(1, Ordering::SeqCst);
        (self.open)(open, cx)?;
        Ok(Box::new(FakeSession {
            open,
            count: 0,
            step: self.step.clone(),
            delay: self.delay,
            continuous: self.stream != Stream::Snapshot,
        }))
    }
    fn fps(&self) -> f64 {
        2.0
    }
    fn first_frame(&self) -> Duration {
        Duration::ZERO
    }
}

impl FrameSession for FakeSession {
    fn continuous(&self) -> bool {
        self.continuous
    }
    fn next(&mut self, cx: &Cx) -> Result<Next, Failure> {
        if !self.continuous {
            cx.wait_until(Instant::now() + self.delay);
            cx.begin_snapshot();
        } else {
            std::thread::sleep(self.delay);
        }
        let step = (self.step)(self.open, self.count);
        self.count += 1;
        match step {
            Step::Frame(width, height) => Ok(Next::Frame(Frame {
                id: 0,
                width,
                height,
                pixel_format: crate::types::RGB8,
                timestamp_ns: 0,
                data: vec![(self.open * 16 + self.count) as u8; (width * height * 3) as usize],
            })),
            Step::Skip => Ok(Next::Skip("test skip")),
            Step::Fail(failure) => Err(failure),
            Step::Block => loop {
                if cx.cancelled() {
                    return Err(Failure::transient(ErrorKind::OperationFailed, "cancelled"));
                }
                std::thread::sleep(Duration::from_millis(5));
            },
        }
    }
}

fn fake(
    stream: Stream,
    delay: Duration,
    open: impl Fn(usize) -> Result<(), Failure> + Send + Sync + 'static,
    step: impl Fn(usize, usize) -> Step + Send + Sync + 'static,
) -> (Arc<dyn SourceFactory>, Arc<AtomicUsize>) {
    let opens = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(Fake {
            stream,
            opens: opens.clone(),
            open: Arc::new(move |count, _| open(count)),
            step: Arc::new(step),
            delay,
        }),
        opens,
    )
}

fn meta() -> Meta {
    Meta {
        id: "hub:test/camera.x".into(),
        product: "Home Assistant",
        latch: None,
    }
}

fn frames(backend: &mut HubBackend, count: usize) -> Vec<Frame> {
    (0..count)
        .map(|_| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match backend.next_frame(Duration::from_millis(50)) {
                    Ok(frame) => return frame,
                    Err(error) if error.is::<FrameWait>() && Instant::now() < deadline => {}
                    Err(error) => panic!("{error:#}"),
                }
            }
        })
        .collect()
}

#[test]
fn frames_flow_newest_first_after_start() {
    let (factory, opens) = fake(
        Stream::Mjpeg,
        Duration::from_millis(2),
        |_| Ok(()),
        |_, _| Step::Frame(4, 2),
    );
    let mut backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    assert!(
        backend.next_frame(Duration::from_millis(10)).is_err(),
        "not started"
    );
    backend.start(0).unwrap();
    let got = frames(&mut backend, 3);
    assert!(got.windows(2).all(|pair| pair[0].id < pair[1].id));
    assert!(
        got.iter()
            .all(|frame| frame.width == 4 && frame.height == 2)
    );
    // A slow reader skips frames instead of falling behind.
    std::thread::sleep(Duration::from_millis(50));
    let later = frames(&mut backend, 1);
    assert!(later[0].id > got[2].id + 2);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    let error = backend.next_frame(Duration::from_millis(0)).err();
    assert!(error.is_none_or(|e| crate::cli::error_code(&e) == ("timeout", 4)));
}

#[test]
fn frames_of_another_size_are_dropped_until_acquisition_restarts() {
    let larger = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch = larger.clone();
    let (factory, _) = fake(
        Stream::Mjpeg,
        Duration::from_millis(2),
        |_| Ok(()),
        move |_, _| {
            if switch.load(Ordering::SeqCst) {
                Step::Frame(8, 4)
            } else {
                Step::Frame(4, 2)
            }
        },
    );
    let mut backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    backend.start(0).unwrap();
    assert_eq!(frames(&mut backend, 2)[1].width, 4);
    larger.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(50));
    // At most one frame from before the switch is still waiting; nothing of
    // the new size gets through.
    let mut delivered = 0;
    while let Ok(frame) = backend.next_frame(Duration::from_millis(30)) {
        assert_eq!(frame.width, 4);
        delivered += 1;
        assert!(delivered < 3);
    }
    let notes = backend.stats().unwrap().notes.join("; ");
    assert!(notes.contains("frames of another size dropped"), "{notes}");
    backend.stop().unwrap();
    backend.start(0).unwrap();
    assert_eq!(frames(&mut backend, 1)[0].width, 8);
}

#[test]
fn transient_failures_reconnect_and_permanent_ones_stop() {
    let (factory, opens) = fake(
        Stream::Mjpeg,
        Duration::from_millis(1),
        |open| match open {
            1 => Err(Failure::transient(
                ErrorKind::Unavailable,
                "connection refused",
            )),
            _ => Ok(()),
        },
        |open, count| match (open, count) {
            (0, 0..=2) => Step::Frame(2, 2),
            (0, _) => Step::Fail(Failure::transient(
                ErrorKind::OperationFailed,
                "stream ended",
            )),
            (2, 0..=4) => Step::Frame(2, 2),
            _ => Step::Fail(Failure::rejected(
                401,
                "Home Assistant hub test rejected the access token (HTTP 401)",
            )),
        },
    );
    let mut backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    backend.start(0).unwrap();
    frames(&mut backend, 4);
    assert!(
        opens.load(Ordering::SeqCst) >= 3,
        "reopened after the stream ended"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let error = loop {
        match backend.next_frame(Duration::from_millis(20)) {
            Err(error) if !error.is::<FrameWait>() => break error,
            _ => assert!(Instant::now() < deadline, "no permanent failure"),
        }
    };
    assert!(format!("{error}").contains("rejected the access token"));
    assert_eq!(crate::cli::error_code(&error), ("operation_failed", 1));
    // Every later read reports the same failure, so the engine stops.
    for _ in 0..3 {
        let again = backend.next_frame(Duration::from_millis(1)).unwrap_err();
        assert_eq!(format!("{again}"), format!("{error}"));
    }
    assert!(backend.start(0).is_err());
    let notes = backend.stats().unwrap().notes.join("; ");
    assert!(
        notes.contains("stopped:") && notes.contains("reconnects: 2"),
        "{notes}"
    );
}

#[test]
fn connecting_falls_back_but_never_past_a_refused_secret() {
    let (broken, _) = fake(
        Stream::Rtsp,
        Duration::ZERO,
        |_| {
            Err(Failure::transient(
                ErrorKind::Unavailable,
                "restream unreachable",
            ))
        },
        |_, _| Step::Skip,
    );
    let (snapshot, _) = fake(
        Stream::Snapshot,
        Duration::ZERO,
        |_| Ok(()),
        |_, _| Step::Frame(2, 2),
    );
    let backend = HubBackend::connect(
        vec![broken.clone(), snapshot.clone()],
        Duration::from_secs(2),
        meta(),
    )
    .unwrap();
    assert_eq!(backend.stream, Stream::Snapshot);
    let (refused, _) = fake(
        Stream::Mjpeg,
        Duration::ZERO,
        |_| {
            Err(Failure::rejected(
                401,
                "rejected the access token (HTTP 401)",
            ))
        },
        |_, _| Step::Skip,
    );
    let error = HubBackend::connect(vec![refused, snapshot], Duration::from_secs(2), meta())
        .err()
        .expect("refused");
    assert!(format!("{error}").contains("HTTP 401"));
    let error = HubBackend::connect(vec![broken], Duration::from_secs(2), meta())
        .err()
        .expect("unreachable");
    assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
}

#[test]
fn a_pump_thread_spawn_failure_is_reported_without_exposing_os_error_text() {
    let (factory, opens) = fake(
        Stream::Mjpeg,
        Duration::ZERO,
        |_| Ok(()),
        |_, _| Step::Frame(2, 2),
    );
    let identity = meta();
    let (session, cx, frame) =
        first(factory.as_ref(), Duration::from_secs(2), &identity.id).unwrap();
    let error = HubBackend::start_pump_with(factory, session, cx, frame, identity, |_work| {
        Err(std::io::Error::other("CANARY operating system error"))
    })
    .err()
    .expect("the missing pump must fail connection");
    assert_eq!(format!("{error}"), "cannot start the hub camera frame pump");
    assert_eq!(crate::cli::error_code(&error), ("operation_failed", 1));
    assert_eq!(opens.load(Ordering::SeqCst), 1);
}

#[test]
fn a_silent_source_times_out_at_connect() {
    let (factory, _) = fake(
        Stream::Mjpeg,
        Duration::ZERO,
        |_| Ok(()),
        |_, _| Step::Block,
    );
    let start = Instant::now();
    let error = HubBackend::connect(vec![factory], Duration::from_millis(200), meta())
        .err()
        .expect("silent");
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(
        format!("{error}")
            .contains("timed out waiting for the first frame from hub:test/camera.x after 200 ms")
    );
    assert_eq!(crate::cli::error_code(&error), ("timeout", 4));
}

#[test]
fn stopping_and_dropping_do_not_wait_for_a_blocked_read() {
    let (factory, _) = fake(
        Stream::Mjpeg,
        Duration::ZERO,
        |_| Ok(()),
        |_, count| {
            if count == 0 {
                Step::Frame(2, 2)
            } else {
                Step::Block
            }
        },
    );
    let mut backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    backend.start(0).unwrap();
    frames(&mut backend, 1);
    let start = Instant::now();
    backend.stop().unwrap();
    backend.start(0).unwrap();
    drop(backend);
    assert!(start.elapsed() < Duration::from_millis(1500));
}

#[test]
fn cancelling_an_open_during_stop_does_not_permanently_fail_the_pump() {
    let (entered, opening) = std::sync::mpsc::channel();
    let factory = Arc::new(Fake {
        stream: Stream::Mjpeg,
        opens: Arc::new(AtomicUsize::new(0)),
        open: Arc::new(move |open, cx| {
            if open == 1 {
                entered.send(()).unwrap();
                while !cx.cancelled() {
                    cx.wait_until(Instant::now() + Duration::from_secs(1));
                }
                return Err(Failure::permanent(ErrorKind::OperationFailed, "cancelled"));
            }
            Ok(())
        }),
        step: Arc::new(|open, count| {
            if open == 0 && count > 0 {
                Step::Fail(Failure::transient(ErrorKind::Unavailable, "stream ended"))
            } else {
                Step::Frame(2, 2)
            }
        }),
        delay: Duration::from_millis(2),
    });
    let mut backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    backend.start(0).unwrap();
    opening.recv_timeout(Duration::from_secs(2)).unwrap();
    backend.stop().unwrap();
    backend.start(0).unwrap();
    frames(&mut backend, 2);
    assert!(backend.shared.lock().failure.is_none());
}

#[test]
fn a_cancelled_session_cannot_publish_a_frame_after_stop() {
    let (factory, _) = fake(
        Stream::Mjpeg,
        Duration::ZERO,
        |_| Ok(()),
        |_, _| Step::Frame(2, 2),
    );
    let mut backend =
        HubBackend::connect(vec![factory.clone()], Duration::from_secs(2), meta()).unwrap();
    let cancel = backend.shared.lock().current.clone().unwrap();
    backend.stop().unwrap();
    // Model a successful read finishing concurrently with stop, before it
    // manages to acquire the publication lock.
    Pump {
        shared: backend.shared.clone(),
        factory,
        meta: meta(),
        epoch: Instant::now(),
    }
    .publish(
        Frame {
            id: 0,
            width: 2,
            height: 2,
            pixel_format: crate::types::RGB8,
            timestamp_ns: 0,
            data: vec![0; 12],
        },
        &Cx {
            cancel,
            wake: backend.shared.wake.clone(),
            snapshot_generation: AtomicU64::new(0),
        },
    );
    assert!(backend.shared.lock().latest.is_none());
}

#[test]
fn a_new_authentication_refusal_cannot_be_cleared_by_connect_success() {
    use crate::hubs::registry::{Hub, Kind, SecretSource};

    let dir = std::env::temp_dir().join(format!(
        "capturefab-pump-latch-{}-{}",
        std::process::id(),
        getrandom::u64().unwrap()
    ));
    let store = Store::at(&dir);
    let hub = Hub {
        name: "test".into(),
        kind: Kind::HomeAssistant,
        url: "http://127.0.0.1:8123".into(),
        user: None,
        secret: SecretSource::Environment,
        secret_env: Some("CAPTUREFAB_HUB_TEST".into()),
        certificate: None,
        tls_server_name: None,
        rtsp_port: None,
        rejected: None,
        revision: None,
    };
    store.save_with(&hub, |_| Ok(()), |_| Ok(())).unwrap();
    let hub = store.get("test").unwrap();
    store.mark_rejected("test", &hub.digest(), 401).unwrap();
    let observed = store.clone();
    let (factory, _) = fake(
        Stream::Mjpeg,
        Duration::ZERO,
        |_| Ok(()),
        move |_, count| {
            if count == 0 {
                Step::Frame(2, 2)
            } else {
                assert!(
                    observed.get("test").unwrap().rejected.is_none(),
                    "the successful first frame clears the old latch before pumping"
                );
                Step::Fail(Failure::rejected(403, "the token was revoked"))
            }
        },
    );
    let mut identity = meta();
    identity.latch = Some((store.clone(), hub.name.clone(), hub.digest()));
    let mut backend = HubBackend::connect(vec![factory], Duration::from_secs(2), identity).unwrap();
    backend.start(0).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !backend.shared.lock().finished {
        assert!(Instant::now() < deadline, "pump did not record the refusal");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(store.get("test").unwrap().rejected.unwrap().status, 403);
    drop(backend);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_parked_stream_is_adopted_or_closed() {
    let (factory, opens) = fake(
        Stream::Mjpeg,
        Duration::from_millis(2),
        |_| Ok(()),
        |_, _| Step::Frame(2, 2),
    );
    let mut backend =
        HubBackend::connect(vec![factory.clone()], Duration::from_secs(2), meta()).unwrap();
    backend.start(0).unwrap();
    frames(&mut backend, 2);
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "the connect-time stream was reused"
    );
    let mut idle = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    std::thread::sleep(PARK + Duration::from_millis(100));
    // A loaded host may not have run the pump thread yet; it drops the parked
    // stream when it does.
    let deadline = Instant::now() + Duration::from_secs(5);
    while idle.shared.lock().current.is_some() {
        assert!(
            Instant::now() < deadline,
            "the parked stream was not closed"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let before = opens.load(Ordering::SeqCst);
    idle.start(0).unwrap();
    frames(&mut idle, 1);
    assert_eq!(
        opens.load(Ordering::SeqCst),
        before + 1,
        "the parked stream was closed"
    );
}

#[test]
fn a_parked_stream_is_closed_without_entering_a_blocking_next_read() {
    struct Held {
        reads: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Held {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl FrameSession for Held {
        fn next(&mut self, _cx: &Cx) -> Result<Next, Failure> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(Next::Frame(Frame {
                id: 0,
                width: 2,
                height: 2,
                pixel_format: crate::types::RGB8,
                timestamp_ns: 0,
                data: vec![0; 12],
            }))
        }
    }
    struct HoldingFactory {
        reads: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }
    impl SourceFactory for HoldingFactory {
        fn stream(&self) -> Stream {
            Stream::Mjpeg
        }
        fn open(&self, _cx: &Cx) -> Result<Box<dyn FrameSession>, Failure> {
            Ok(Box::new(Held {
                reads: self.reads.clone(),
                drops: self.drops.clone(),
            }))
        }
    }
    let reads = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(HoldingFactory {
        reads: reads.clone(),
        drops: drops.clone(),
    });
    let backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while drops.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the parked session was not closed"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "only the connect-time frame"
    );
    drop(backend);
}

#[test]
fn snapshot_captures_wait_for_an_image_taken_after_they_asked() {
    let (factory, _) = fake(
        Stream::Snapshot,
        Duration::from_millis(30),
        |_| Ok(()),
        |_, _| Step::Frame(2, 2),
    );
    let mut backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    backend.start(0).unwrap();
    let first = frames(&mut backend, 1)[0].id;
    backend.request_frame().unwrap();
    let next = frames(&mut backend, 1)[0].id;
    assert!(next > first);
}

#[test]
fn a_capture_during_a_snapshot_get_waits_for_a_new_get_without_the_poll_delay() {
    use crate::hubs::{
        fake_http::{FakeHttp, Reply},
        http::{self, Auth, Endpoint, Limits, Request, Response},
        image::tests::jpeg_of,
        registry::parse_url,
        sources::{Access, MediaRequest, SnapshotSource},
        tls,
    };

    struct TestAccess(Endpoint);
    impl Access for TestAccess {
        fn media(&self, request: &MediaRequest, cx: &Cx) -> Result<Response, Failure> {
            http::send(
                &self.0,
                &Request::get(
                    request.route,
                    request.target.clone(),
                    request.accept,
                    request.limits,
                ),
                &Auth::None,
                Some(&cx.cancel),
            )
            .map_err(Failure::from)
        }
    }

    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let release = Arc::new(std::sync::Barrier::new(2));
    let wait_for_capture = release.clone();
    let (entered, in_flight) = std::sync::mpsc::channel();
    let fake = FakeHttp::start(move |_| {
        let request = counted.fetch_add(1, Ordering::SeqCst);
        if request == 1 {
            entered.send(()).unwrap();
            wait_for_capture.wait();
        }
        // The connect-time, in-flight and subsequent images are distinct.
        Reply::image("image/jpeg", jpeg_of(4, 4, [20, 100, 200][request.min(2)]))
    });
    let source = Arc::new(SnapshotSource {
        access: Arc::new(TestAccess(
            Endpoint::new(
                "Test hub test",
                parse_url(&fake.url()).unwrap(),
                &tls::Trust::Platform,
            )
            .unwrap(),
        )),
        request: MediaRequest {
            route: "the snapshot",
            target: "/snapshot".into(),
            accept: "image/*",
            limits: Limits::request(Duration::from_secs(5), 1 << 20),
        },
        label: "camera test on hub test".into(),
        fps: 0.01, // A regular next poll would wait 100 seconds.
        offline_header: None,
    });
    let mut backend = HubBackend::connect(vec![source], Duration::from_secs(2), meta()).unwrap();
    backend.start(0).unwrap();
    backend.request_frame().unwrap();
    in_flight.recv_timeout(Duration::from_secs(2)).unwrap();
    // This request starts after the server received the second GET. Its
    // response must be discarded even though it completes after this call.
    backend.request_frame().unwrap();
    release.wait();
    let frame = backend.next_frame(Duration::from_secs(2)).unwrap();
    assert!(frame.data.iter().all(|pixel| (195..=205).contains(pixel)));
    assert!(requests.load(Ordering::SeqCst) >= 3);
}

#[test]
fn hub_cameras_are_read_only_genicam_devices() {
    let (factory, _) = fake(
        Stream::Mjpeg,
        Duration::from_millis(2),
        |_| Ok(()),
        |_, _| Step::Frame(6, 4),
    );
    let backend = HubBackend::connect(vec![factory], Duration::from_secs(2), meta()).unwrap();
    let mut camera = crate::camera::Camera::from_backend(
        crate::types::CameraInfo {
            id: "hub:test/camera.x".into(),
            transport: crate::types::Transport::Media,
            vendor: "Home Assistant".into(),
            model: "Test".into(),
            serial: "test/camera.x".into(),
            address: Some("hub:test/camera.x".into()),
            profile: Default::default(),
        },
        Box::new(backend),
    )
    .unwrap();
    assert_eq!(camera.get("Width").unwrap(), 6);
    assert_eq!(camera.get("Height").unwrap(), 4);
    let error = camera.set("Width", "8").unwrap_err();
    assert!(format!("{error:#}").contains("read-only"), "{error:#}");
    camera.start().unwrap();
    let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
    assert_eq!((frame.width, frame.height), (6, 4));
    camera.stop().unwrap();
}
