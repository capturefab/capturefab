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
        CaptureSession::open(&self.0, "test-camera", true).unwrap()
    }
}
impl Drop for SessionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(format!("{}.wal", self.0.display()));
    }
}
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
    let mut resumed = CaptureSession::open(&file.0, "test-camera", false).unwrap();
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
    let resumed = CaptureSession::open(&file.0, "test-camera", false).unwrap();
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
    let resumed = CaptureSession::open(&file.0, "test-camera", false).unwrap();
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
    let resumed = CaptureSession::open(&file.0, "test-camera", false).unwrap();
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
    assert!(CaptureSession::open(&file.0, "test-camera", false).is_err());
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
    assert!(CaptureSession::open(&file.0, "other-camera", false).is_err());
    assert!(CaptureSession::open(&file.0, "test-camera", true).is_err());
    {
        let db = Connection::open(&file.0).unwrap();
        db.execute("UPDATE capture_session SET schema_version = 99", [])
            .unwrap();
    }
    assert!(CaptureSession::open(&file.0, "test-camera", false).is_err());
    {
        let db = Connection::open(&file.0).unwrap();
        db.execute("UPDATE capture_session SET schema_version = 1", [])
            .unwrap();
        db.execute("UPDATE captures SET descriptor = ?", params![vec![0_u8; 3]])
            .unwrap();
    }
    assert!(CaptureSession::open(&file.0, "test-camera", false).is_err());
    assert!(decode(&vec![0; FEATURES * 4 - 1]).is_err());
    let mut nonfinite = vec![0_u8; FEATURES * 4];
    nonfinite[..4].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(decode(&nonfinite).is_err());
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
        assert_eq!(decode(&encode(&feature)).unwrap(), feature);
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
