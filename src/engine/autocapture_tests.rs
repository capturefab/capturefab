use super::*;

struct Fixture {
    worker: WorkerHandle,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        // Parallel tests can read the same clock value; the counter keeps
        // each fixture's directory distinct.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "capturefab-auto-worker-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let worker = WorkerHandle::new();
        worker
            .request(SessionCommand::Connect {
                camera: "sim:0".into(),
                timeout_ms: 1000,
            })
            .unwrap();
        Self { worker, root }
    }
    fn open(&self, create: bool) -> Value {
        self.worker
            .request(SessionCommand::CaptureSessionOpen {
                path: self
                    .root
                    .join("session.duckdb")
                    .to_string_lossy()
                    .into_owned(),
                create,
                // Pinned to the native descriptor, whose calibration these tests assume.
                encoder: create.then_some(crate::autocapture::EncoderChoice::Native),
            })
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.worker.shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn exact_manual_sensor_frame_survives_close_and_resume() {
    let f = Fixture::new();
    assert_eq!(f.open(true)["capture_session"]["manual_count"], 0);
    let learned = f
        .worker
        .request(SessionCommand::CaptureExample { timeout_ms: 1000 })
        .unwrap();
    assert_eq!(learned["capture_session"]["manual_count"], 1);
    let frame = f.worker.latest_frame().unwrap();
    f.worker
        .request(SessionCommand::CaptureSessionClose)
        .unwrap();
    let db = duckdb::Connection::open(f.root.join("session.duckdb")).unwrap();
    let (pixels, width, stamp): (Vec<u8>, u32, u64) = db
        .query_row(
            "SELECT data,width,timestamp_ns FROM captures WHERE source = 'manual'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (pixels, width, stamp),
        (frame.data, frame.width, frame.timestamp_ns)
    );
    drop(db);
    assert_eq!(f.open(false)["capture_session"]["manual_count"], 1);
    assert!(
        f.worker
            .request(SessionCommand::CaptureReview { id: None })
            .unwrap()["capture"]
            .is_null()
    );
}

#[test]
fn failed_file_capture_does_not_teach_and_successful_file_capture_does() {
    let f = Fixture::new();
    f.open(true);
    let path = f.root.join("frame.raw").to_string_lossy().into_owned();
    let command = || SessionCommand::Capture {
        output: path.clone(),
        count: 1,
        timeout_ms: 1000,
        format: "raw".into(),
        storage: Default::default(),
        destination: None,
        on_exists: Default::default(),
        context: Default::default(),
    };
    f.worker.request(command()).unwrap();
    assert_eq!(f.worker.snapshot().capture_session.unwrap().manual_count, 1);
    assert!(f.worker.request(command()).is_err());
    assert_eq!(f.worker.snapshot().capture_session.unwrap().manual_count, 1);
}

#[test]
fn enabling_auto_capture_starts_stream_and_resuming_restores_armed_settings() {
    let f = Fixture::new();
    assert!(
        f.worker
            .request(SessionCommand::AutoCapture {
                enabled: true,
                sensitivity: 0.7,
                cooldown_ms: 500
            })
            .is_err()
    );
    f.open(true);
    let status = f
        .worker
        .request(SessionCommand::AutoCapture {
            enabled: true,
            sensitivity: 0.7,
            cooldown_ms: 500,
        })
        .unwrap();
    assert_eq!(status["capture_session"]["state"], "learning");
    assert!(f.worker.snapshot().streaming);
    assert!(
        f.worker.snapshot().auto.is_none(),
        "capture preference is independent of exposure auto"
    );
    f.worker
        .request(SessionCommand::CaptureSessionClose)
        .unwrap();
    f.worker.request(SessionCommand::Stop).unwrap();
    let resumed = f.open(false);
    assert_eq!(resumed["capture_session"]["sensitivity"], 0.7);
    assert_eq!(resumed["capture_session"]["cooldown_ms"], 500);
    assert!(f.worker.snapshot().streaming);
    f.worker.request(SessionCommand::Disconnect).unwrap();
    assert!(f.worker.snapshot().capture_session.is_none());
}

#[test]
fn invalid_auto_capture_inputs_are_rejected_before_mutation() {
    for command in [
        SessionCommand::AutoCapture {
            enabled: true,
            sensitivity: f64::NAN,
            cooldown_ms: 2000,
        },
        SessionCommand::AutoCapture {
            enabled: true,
            sensitivity: 0.5,
            cooldown_ms: 0,
        },
        SessionCommand::CaptureSessionOpen {
            path: ":memory:".into(),
            create: true,
            encoder: None,
        },
        SessionCommand::CaptureSessionOpen {
            path: "resume.duckdb".into(),
            create: false,
            encoder: Some(crate::autocapture::EncoderChoice::Dinov2),
        },
        SessionCommand::ReviewCapture { id: 0, keep: true },
    ] {
        assert!(command.validate().is_err());
    }
    assert!(
        SessionCommand::from_json_slice(br#"{"op":"capture_session_close","extra":true}"#).is_err()
    );
}
