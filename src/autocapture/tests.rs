use super::*;
use crate::types::{MONO8, RGB8};
use std::sync::atomic::{AtomicU64, Ordering};

struct SessionFile(PathBuf);
impl SessionFile {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "capturefab-auto-{}-{}-{}.duckdb",
            std::process::id(),
            epoch_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn create(&self) -> CaptureSession {
        CaptureSession::open(&self.0, "test-camera", true, NATIVE).unwrap()
    }
    fn resume(&self) -> Result<CaptureSession> {
        CaptureSession::open(&self.0, "test-camera", false, None)
    }
}
impl Drop for SessionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(format!("{}.wal", self.0.display()));
    }
}
/// Pinned so these tests keep the native descriptor's calibration.
const NATIVE: Option<EncoderChoice> = Some(EncoderChoice::Native);
fn solid(value: u8) -> Frame {
    Frame {
        id: 42,
        width: 32,
        height: 32,
        pixel_format: MONO8,
        timestamp_ns: u64::MAX,
        data: vec![value; 32 * 32],
    }
}
fn color(rgb: [u8; 3]) -> Frame {
    Frame {
        id: 7,
        width: 32,
        height: 32,
        pixel_format: RGB8,
        timestamp_ns: 9,
        data: rgb.repeat(32 * 32),
    }
}
fn ready(session: &mut CaptureSession) {
    session.configure(true, 0.5, 250).unwrap();
    session.last_capture = None;
    session.last_sample = None;
}
fn observe(session: &mut CaptureSession, frame: &Frame) -> Option<u64> {
    session.last_sample = None;
    session.last_capture = None;
    session.observe(frame).unwrap()
}

#[test]
fn session_is_a_single_resumable_duckdb_with_exact_sensor_frames() {
    let file = SessionFile::new();
    let raw = solid(120);
    let candidate = solid(140);
    let pending_id;
    {
        let mut session = file.create();
        assert_eq!(session.status().unwrap().state, "paused");
        session.manual(&raw).unwrap();
        ready(&mut session);
        pending_id = observe(&mut session, &candidate).expect("nearby variant is proposed");
        assert_eq!(
            session.positive.len(),
            1,
            "pending is never a positive training label"
        );
        assert_eq!(session.status().unwrap().pending_count, 1);
        let review = session.next_review().unwrap().unwrap();
        assert_eq!(review.id, pending_id);
        assert_eq!(review.frame.data, candidate.data);
        assert_eq!(review.frame.timestamp_ns, u64::MAX);
        assert_eq!(review.frame.pixel_format, MONO8);
    }
    assert!(file.0.is_file());
    assert!(
        !Path::new(&format!("{}.wal", file.0.display())).exists(),
        "clean session consists of one file"
    );
    let mut resumed = file.resume().unwrap();
    assert!(resumed.status().unwrap().enabled);
    assert_eq!(resumed.status().unwrap().cooldown_ms, 250);
    assert_eq!(
        resumed
            .review_capture(pending_id)
            .unwrap()
            .unwrap()
            .frame
            .data,
        candidate.data
    );
    resumed.review(pending_id, true).unwrap();
    assert!(resumed.next_review().unwrap().is_none());
    assert_eq!(resumed.status().unwrap().kept_count, 1);
    assert_eq!(resumed.positive.len(), 2);
    drop(resumed);
    let resumed = file.resume().unwrap();
    assert_eq!(resumed.status().unwrap().manual_count, 1);
    assert_eq!(resumed.status().unwrap().kept_count, 1);
    assert_eq!(resumed.positive.len(), 2);
}

#[test]
fn manual_preference_novelty_and_rejection_drive_scoring() {
    let file = SessionFile::new();
    let mut session = file.create();
    ready(&mut session);
    assert!(observe(&mut session, &color([20, 20, 20])).is_none());
    assert_eq!(session.background.len(), 1);
    assert_eq!(session.status().unwrap().state, "learning");
    session.manual(&color([220, 40, 40])).unwrap();
    let target = descriptor(&color([200, 45, 40])).unwrap();
    let unrelated = descriptor(&color([20, 200, 200])).unwrap();
    assert!(session.score(&target).0 > session.score(&unrelated).0 + 0.5);
    let with_novelty = session.score(&target).0;
    let background = std::mem::take(&mut session.background);
    assert!(with_novelty > session.score(&target).0);
    session.background = background;
    let id = observe(&mut session, &color([200, 45, 40])).expect("similar target auto captured");
    let before = session.score(&target).0;
    session.review(id, false).unwrap();
    assert!(session.score(&target).0 < before / 2.0 + 0.01);
    let original = descriptor(&color([220, 40, 40])).unwrap();
    assert!(
        session.score(&original).0 >= 0.90,
        "a nearby rejected variant cannot suppress the exact manual example"
    );
    assert!(
        session
            .observe_at(
                &color([220, 40, 40]),
                Instant::now() + Duration::from_secs(31)
            )
            .unwrap()
            .is_some()
    );
    let stored: Option<Vec<u8>> = session
        .db
        .query_row("SELECT data FROM captures WHERE id = ?", params![id], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(stored.is_none(), "discard removes sensor pixels");
    assert!(
        session.review(id, true).is_err(),
        "a stale review cannot change a final decision"
    );
    drop(session);
    let resumed = file.resume().unwrap();
    assert_eq!(resumed.negative.len(), 1);
    assert_eq!(resumed.background.len(), 1);
    assert_eq!(resumed.status().unwrap().discarded_count, 1);
    assert!(resumed.score(&target).0 < before / 2.0 + 0.01);
}

#[test]
fn sampling_cooldown_and_near_duplicates_bound_work_and_capture_volume() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    let t = Instant::now();
    assert!(
        session.observe_at(&solid(120), t).unwrap().is_none(),
        "manual example is suppressed as duplicate"
    );
    assert!(
        session
            .observe_at(&solid(140), t + Duration::from_millis(100))
            .unwrap()
            .is_none(),
        "sampler caps at 4Hz"
    );
    assert!(
        session
            .observe_at(&solid(140), t + Duration::from_millis(250))
            .unwrap()
            .is_some()
    );
    session.cooldown_ms = 2000;
    assert!(
        session
            .observe_at(&solid(100), t + Duration::from_millis(500))
            .unwrap()
            .is_none(),
        "capture cooldown applies independently of sampler"
    );
    assert!(
        session
            .observe_at(&solid(100), t + Duration::from_millis(2250))
            .unwrap()
            .is_some()
    );
    assert!(
        session
            .observe_at(&solid(140), t + Duration::from_millis(4500))
            .unwrap()
            .is_none(),
        "recent candidate does not create a duplicate card"
    );
    assert!(
        session
            .observe_at(&solid(140), t + Duration::from_secs(10))
            .unwrap()
            .is_some(),
        "duplicate suppression expires so a later recurrence remains capturable"
    );
}

#[test]
fn strict_sensitivity_still_admits_exact_human_examples_without_background() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    session.configure(true, 0.0, 250).unwrap();
    assert!(session.background.is_empty());
    assert!(
        session
            .observe_at(&solid(120), Instant::now() + Duration::from_secs(31))
            .unwrap()
            .is_some()
    );
}

#[test]
fn out_of_order_reviews_use_the_same_bounded_example_bank_after_resume() {
    let file = SessionFile::new();
    let old = solid(220);
    let newer = solid(100);
    {
        let mut session = file.create();
        session.db.execute("INSERT INTO captures SELECT i + 100, 0, 'manual', 'manual', 1, 32, 32, ?, 0, ?, 1024, ?, 1.0, 'test', NULL FROM range(256) t(i)",params![MONO8,&newer.data,encode(&descriptor(&newer).unwrap())]).unwrap();
        session.db.execute("INSERT INTO captures VALUES (1, 0, 'automatic', 'pending', 1, 32, 32, ?, 0, ?, 1024, ?, 0.9, 'test', NULL)",params![MONO8,&old.data,encode(&descriptor(&old).unwrap())]).unwrap();
        session.review(1, true).unwrap();
        assert_eq!(session.positive.len(), 256);
        assert!(
            session
                .positive
                .iter()
                .all(|feature| feature == &descriptor(&newer).unwrap())
        );
        session.checkpoint().unwrap();
    }
    let resumed = file.resume().unwrap();
    assert_eq!(resumed.positive.len(), 256);
    assert!(
        resumed
            .positive
            .iter()
            .all(|feature| feature == &descriptor(&newer).unwrap())
    );
}

#[test]
fn full_review_queue_and_pixel_budget_apply_backpressure() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    let feature = descriptor(&solid(140)).unwrap();
    session.db.execute("INSERT INTO captures SELECT i + 100, 0, 'automatic', 'pending', 1, 32, 32, ?, 0, ?, 1024, ?, 0.9, 'test', NULL FROM range(200) t(i)",params![MONO8,solid(140).data,encode(&feature)]).unwrap();
    assert_eq!(session.status().unwrap().state, "review_queue_full");
    assert!(observe(&mut session, &solid(140)).is_none());
    session
        .db
        .execute("DELETE FROM captures WHERE label = 'pending'", [])
        .unwrap();
    // Exercise the scalar accounting boundary without allocating 2 GiB.
    session
        .db
        .execute(
            "UPDATE captures SET data_bytes = ?",
            params![MAX_STORED_BYTES],
        )
        .unwrap();
    assert_eq!(session.status().unwrap().state, "storage_full");
    assert!(observe(&mut session, &solid(140)).is_none());
    assert!(session.manual(&solid(100)).is_err());
    session
        .db
        .execute(
            "UPDATE captures SET data_bytes = ?",
            params![MAX_STORED_BYTES - 100],
        )
        .unwrap();
    assert!(observe(&mut session, &solid(140)).is_none());
    assert_eq!(
        session.status().unwrap().state,
        "storage_full",
        "a partially full session reports blocked next-frame storage"
    );
    assert!(session.status().unwrap().blocked_storage);
    let malformed = Frame {
        width: u32::MAX,
        height: u32::MAX,
        ..solid(0)
    };
    assert!(descriptor(&malformed).is_err());
}

#[test]
fn malformed_session_settings_identity_versions_and_descriptors_are_rejected() {
    let file = SessionFile::new();
    assert!(file.resume().is_err());
    {
        let mut session = file.create();
        assert!(session.configure(true, f64::NAN, 250).is_err());
        assert!(session.configure(true, 1.01, 250).is_err());
        assert!(session.configure(true, 0.5, 0).is_err());
        assert!(
            session
                .manual(&Frame {
                    data: vec![],
                    ..solid(1)
                })
                .is_err()
        );
        session.manual(&solid(1)).unwrap();
        assert_eq!(session.status().unwrap().manual_count, 1);
    }
    assert!(CaptureSession::open(&file.0, "other-camera", false, None).is_err());
    assert!(CaptureSession::open(&file.0, "test-camera", true, NATIVE).is_err());
    {
        let db = Connection::open(&file.0).unwrap();
        db.execute("UPDATE capture_session SET schema_version = 99", [])
            .unwrap();
    }
    assert!(file.resume().is_err());
    {
        let db = Connection::open(&file.0).unwrap();
        db.execute("UPDATE capture_session SET schema_version = 2", [])
            .unwrap();
        db.execute("UPDATE captures SET descriptor = ?", params![vec![0_u8; 3]])
            .unwrap();
    }
    assert!(file.resume().is_err());
    assert!(Encoder::Native.decode(&vec![0; FEATURES * 4 - 1]).is_err());
    let mut nonfinite = vec![0_u8; FEATURES * 4];
    nonfinite[..4].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(Encoder::Native.decode(&nonfinite).is_err());
}

#[test]
fn descriptor_handles_supported_raw_formats_and_is_bounded() {
    for pixel_format in [
        MONO8,
        RGB8,
        0x0218_0015,
        0x0110_0003,
        0x0110_0005,
        0x0110_0007,
        0x0108_0008,
        0x0108_0009,
        0x0108_000a,
        0x0108_000b,
    ] {
        let bytes = match pixel_format {
            RGB8 | 0x0218_0015 => 3,
            0x0110_0003 | 0x0110_0005 | 0x0110_0007 => 2,
            _ => 1,
        };
        let frame = Frame {
            width: 641,
            height: 479,
            pixel_format,
            data: vec![100; 641 * 479 * bytes],
            ..Frame::default()
        };
        let feature = descriptor(&frame).unwrap();
        assert_eq!(feature.len(), FEATURES);
        assert!(
            feature
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        );
        assert_eq!(Encoder::Native.decode(&encode(&feature)).unwrap(), feature);
    }
    assert!(
        descriptor(&Frame {
            width: 1,
            height: 1,
            pixel_format: 0,
            data: vec![0],
            ..Frame::default()
        })
        .is_err()
    );
}

#[test]
fn version_1_sessions_upgrade_in_place_as_native() {
    let file = SessionFile::new();
    let feature = descriptor(&solid(120)).unwrap();
    {
        // The schema Capturefab wrote before encoder choice existed.
        let db = Connection::open(&file.0).unwrap();
        db.execute_batch(
            "CREATE TABLE capture_session (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                schema_version UINTEGER NOT NULL, descriptor_version UINTEGER NOT NULL,
                camera VARCHAR NOT NULL, created_ms UBIGINT NOT NULL,
                enabled BOOLEAN NOT NULL, sensitivity DOUBLE NOT NULL, cooldown_ms UBIGINT NOT NULL
            );
            CREATE TABLE captures (
                id UBIGINT PRIMARY KEY, created_ms UBIGINT NOT NULL,
                source VARCHAR NOT NULL CHECK (source IN ('manual', 'automatic')),
                label VARCHAR NOT NULL CHECK (label IN ('manual', 'pending', 'kept', 'discarded')),
                frame_id UBIGINT NOT NULL, width UINTEGER NOT NULL, height UINTEGER NOT NULL,
                pixel_format UINTEGER NOT NULL, timestamp_ns UBIGINT NOT NULL,
                data BLOB, data_bytes UBIGINT NOT NULL, descriptor BLOB NOT NULL, score DOUBLE NOT NULL,
                reason VARCHAR NOT NULL, reviewed_ms UBIGINT
            );
            CREATE TABLE background (id UINTEGER PRIMARY KEY, descriptor BLOB NOT NULL);
            INSERT INTO capture_session VALUES (1, 1, 1, 'test-camera', 0, true, 0.25, 500);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO captures VALUES (1, 0, 'manual', 'manual', 1, 32, 32, ?, 0, ?, 1024, ?, 1.0, 'test', NULL)",
            params![MONO8, solid(120).data, encode(&feature)],
        )
        .unwrap();
    }
    let session = file.resume().unwrap();
    let status = session.status().unwrap();
    assert_eq!(status.encoder, "native");
    assert_eq!((status.manual_count, status.cooldown_ms), (1, 500));
    assert_eq!(session.positive, vec![feature]);
    drop(session);
    let version: u32 = Connection::open(&file.0)
        .unwrap()
        .query_row("SELECT schema_version FROM capture_session", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn the_encoder_is_chosen_only_when_a_session_is_created() {
    let file = SessionFile::new();
    file.create();
    let error = CaptureSession::open(&file.0, "test-camera", false, NATIVE)
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("fixed when a capture session is created"),
        "{error}"
    );
    let resumed = file.resume().unwrap();
    assert_eq!(resumed.status().unwrap().encoder, "native");
    assert!(resumed.status().unwrap().accelerator.is_none());
}

#[cfg(not(feature = "dinov2"))]
#[test]
fn builds_without_dinov2_use_the_native_descriptor() {
    let file = SessionFile::new();
    let error = CaptureSession::open(&file.0, "test-camera", true, Some(EncoderChoice::Dinov2))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("not supported"), "{error}");
    assert!(!file.0.exists(), "a failed encoder leaves no session file");
    let session = CaptureSession::open(&file.0, "test-camera", true, None).unwrap();
    assert_eq!(session.status().unwrap().encoder, "native");
}

/// Under `cfg(test)` the bundled model is the tiny committed fixture.
#[cfg(feature = "dinov2")]
mod dinov2_sessions {
    use super::*;

    fn create(file: &SessionFile, choice: Option<EncoderChoice>) -> CaptureSession {
        CaptureSession::open(&file.0, "test-camera", true, choice).unwrap()
    }
    /// Wait for the background embedding, then decide on it as if it had
    /// finished at `finished`.
    fn settle(session: &mut CaptureSession, finished: Instant) -> Option<u64> {
        let (frame, at, _, feature) = session.embedder.as_mut().unwrap().wait().unwrap();
        session
            .decide_late(&frame, feature.unwrap(), at, finished)
            .unwrap()
    }

    #[test]
    fn auto_uses_the_bundled_model_and_reports_where_it_runs() {
        let file = SessionFile::new();
        let mut session = create(&file, None);
        session.manual(&color([220, 40, 40])).unwrap();
        let status = session.status().unwrap();
        assert_eq!(status.encoder, "dinov2");
        assert!(status.model.contains("DINOv2"), "{}", status.model);
        assert_eq!(
            status.accelerator.as_deref(),
            Some("CPU"),
            "unit tests never start the GPU helper"
        );
    }

    #[test]
    fn embeds_off_thread_and_captures_after_the_result_arrives() {
        let file = SessionFile::new();
        let mut session = create(&file, Some(EncoderChoice::Dinov2));
        session.manual(&color([220, 40, 40])).unwrap();
        ready(&mut session);
        let t = Instant::now() + Duration::from_secs(31);
        assert_eq!(
            session.observe_at(&color([220, 40, 40]), t).unwrap(),
            None,
            "the frame is only submitted on this call"
        );
        assert!(
            session.embedder.as_ref().unwrap().busy,
            "one frame is in flight; sampling waits for it"
        );
        let id = settle(&mut session, t + Duration::from_millis(40))
            .expect("the exact manual target is suggested once embedded");
        let review = session.review_capture(id).unwrap().unwrap();
        assert_eq!(
            review.frame.data,
            color([220, 40, 40]).data,
            "the sampled frame is stored"
        );
        // A result that arrives after the scene moved on is dropped.
        session.last_capture = None;
        session.recent.clear();
        session.last_sample = None;
        session
            .observe_at(&color([220, 40, 40]), t + Duration::from_secs(40))
            .unwrap();
        assert_eq!(settle(&mut session, t + Duration::from_secs(43)), None);
        assert_eq!(session.status().unwrap().pending_count, 1);
        // Pausing discards the frame in flight, even when re-armed at once.
        session.last_capture = None;
        session.recent.clear();
        session.last_sample = None;
        let now = Instant::now();
        session.observe_at(&color([220, 40, 40]), now).unwrap();
        session.configure(false, 0.5, 250).unwrap();
        session.configure(true, 0.5, 250).unwrap();
        assert_eq!(settle(&mut session, now + Duration::from_millis(100)), None);
        assert_eq!(session.status().unwrap().pending_count, 1);
        // The same frame sampled after re-arming is captured.
        session.last_sample = None;
        let later = Instant::now();
        session.observe_at(&color([220, 40, 40]), later).unwrap();
        assert!(settle(&mut session, later + Duration::from_millis(100)).is_some());
    }

    #[test]
    fn a_single_triggered_frame_is_captured_without_a_later_frame() {
        let file = SessionFile::new();
        let mut session = create(&file, Some(EncoderChoice::Dinov2));
        session.manual(&color([220, 40, 40])).unwrap();
        ready(&mut session);
        session.recent.clear();
        assert_eq!(session.observe(&color([220, 40, 40])).unwrap(), None);
        // No further frames arrive; the worker loop polls, even seconds later.
        let started = Instant::now();
        let id = loop {
            if let Some(id) = session.poll().unwrap() {
                break id;
            }
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(session.review_capture(id).unwrap().is_some());
    }

    #[test]
    fn samples_follow_the_camera_at_up_to_30_frames_per_second() {
        let file = SessionFile::new();
        let mut session = create(&file, Some(EncoderChoice::Dinov2));
        session.manual(&color([220, 40, 40])).unwrap();
        ready(&mut session);
        let t = Instant::now();
        session.observe_at(&color([0, 0, 0]), t).unwrap();
        session.embedder.as_mut().unwrap().wait();
        assert!(
            !session.embedder.as_ref().unwrap().busy
                && session.last_sample.is_some_and(|last| last == t),
        );
        session
            .observe_at(&color([0, 0, 0]), t + Duration::from_millis(20))
            .unwrap();
        assert!(
            !session.embedder.as_ref().unwrap().busy,
            "a 60 fps frame is skipped"
        );
        // A 30 fps camera's frame arriving a few milliseconds early still counts.
        session
            .observe_at(&color([0, 0, 0]), t + Duration::from_millis(30))
            .unwrap();
        assert!(
            session.embedder.as_ref().unwrap().busy,
            "the next 30 fps slot"
        );
        session.embedder.as_mut().unwrap().wait();
        // ...and the grid, not the early arrival, sets the following slot.
        session
            .observe_at(&color([0, 0, 0]), t + Duration::from_millis(48))
            .unwrap();
        assert!(!session.embedder.as_ref().unwrap().busy);
        session
            .observe_at(&color([0, 0, 0]), t + Duration::from_millis(60))
            .unwrap();
        assert!(session.embedder.as_ref().unwrap().busy);
    }

    #[test]
    fn resume_requires_the_model_the_session_learned_with() {
        let file = SessionFile::new();
        {
            let mut session = create(&file, None);
            session.manual(&color([220, 40, 40])).unwrap();
        }
        let resumed = file.resume().unwrap();
        assert_eq!(resumed.status().unwrap().encoder, "dinov2");
        assert_eq!(resumed.positive.len(), 1);
        drop(resumed);
        // A session from a build that bundled another model is refused
        // rather than mixing embedding spaces.
        Connection::open(&file.0)
            .unwrap()
            .execute("UPDATE capture_session SET model_fingerprint = 'abc'", [])
            .unwrap();
        let error = file.resume().err().unwrap().to_string();
        assert!(error.contains("learned with DINOv2 model abc"), "{error}");
    }
}
