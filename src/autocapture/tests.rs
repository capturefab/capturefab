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
impl SessionFile {
    fn frames(&self) -> PathBuf {
        self.0.with_extension("frames")
    }
    fn frame_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<_> = std::fs::read_dir(self.frames())
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default();
        files.sort();
        files
    }
}
impl Drop for SessionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(format!("{}.wal", self.0.display()));
        let _ = std::fs::remove_dir_all(self.frames());
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
    let id = session.observe(frame).unwrap();
    session.flush();
    id
}
/// Observe, then wait for the writer, which the camera thread never does.
fn observe_at(session: &mut CaptureSession, frame: &Frame, at: Instant) -> Option<u64> {
    let id = session.observe_at(frame, at).unwrap();
    session.flush();
    id
}

#[test]
fn session_is_a_resumable_database_and_frames_folder_with_exact_sensor_frames() {
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
        "a cleanly closed database needs no WAL"
    );
    assert_eq!(
        file.frame_files()
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect::<Vec<_>>(),
        vec![raw.data.clone(), candidate.data.clone()],
        "each frame is one raw file beside the database"
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
    let stored: (Option<String>, u64) = session.sql(move |db| {
        db.query_row(
            "SELECT frame_path, data_bytes FROM captures WHERE id = ?",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    });
    assert_eq!(stored, (None, 0), "discard removes sensor pixels");
    assert!(!file.frames().join(format!("{id:08}.raw")).exists());
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
        observe_at(&mut session, &solid(120), t).is_none(),
        "manual example is suppressed as duplicate"
    );
    assert!(
        observe_at(&mut session, &solid(140), t + Duration::from_millis(100)).is_none(),
        "sampler caps at 4Hz"
    );
    assert!(observe_at(&mut session, &solid(140), t + Duration::from_millis(250)).is_some());
    session.cooldown_ms = 2000;
    assert!(
        observe_at(&mut session, &solid(100), t + Duration::from_millis(500)).is_none(),
        "capture cooldown applies independently of sampler"
    );
    assert!(observe_at(&mut session, &solid(100), t + Duration::from_millis(2250)).is_some());
    assert!(
        observe_at(&mut session, &solid(140), t + Duration::from_millis(4500)).is_none(),
        "recent candidate does not create a duplicate card"
    );
    assert!(
        observe_at(&mut session, &solid(140), t + Duration::from_secs(10)).is_some(),
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
        let (newer_data, newer_feature) =
            (newer.data.clone(), encode(&descriptor(&newer).unwrap()));
        let (old_data, old_feature) = (old.data.clone(), encode(&descriptor(&old).unwrap()));
        session.sql(move |db| {
            db.execute("INSERT INTO captures SELECT i + 100, 0, 'manual', 'manual', 1, 32, 32, ?, 0, ?, 1024, ?, 1.0, 'test', NULL, NULL, NULL FROM range(256) t(i)",params![MONO8,newer_data,newer_feature])?;
            db.execute("INSERT INTO captures VALUES (1, 0, 'automatic', 'pending', 1, 32, 32, ?, 0, ?, 1024, ?, 0.9, 'test', NULL, NULL, NULL)",params![MONO8,old_data,old_feature])
        });
        session.review(1, true).unwrap();
        assert_eq!(session.positive.len(), 256);
        assert!(
            session
                .positive
                .iter()
                .all(|feature| feature == &descriptor(&newer).unwrap())
        );
        session.close().unwrap();
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
    let feature = encode(&descriptor(&solid(140)).unwrap());
    session.sql(move |db| db.execute("INSERT INTO captures SELECT i + 100, 0, 'automatic', 'pending', 1, 32, 32, ?, 0, ?, 1024, ?, 0.9, 'test', NULL, NULL, NULL FROM range(200) t(i)",params![MONO8,solid(140).data,feature]));
    session.recount();
    assert_eq!(session.status().unwrap().state, "review_queue_full");
    assert!(observe(&mut session, &solid(140)).is_none());
    session.sql(|db| db.execute("DELETE FROM captures WHERE label = 'pending'", []));
    // Exercise the scalar accounting boundary without allocating 2 GiB.
    session.sql(|db| {
        db.execute(
            "UPDATE captures SET data_bytes = ?",
            params![MAX_STORED_BYTES],
        )
    });
    session.recount();
    assert_eq!(session.status().unwrap().state, "storage_full");
    assert!(observe(&mut session, &solid(140)).is_none());
    let full = session.manual(&solid(100)).unwrap_err();
    assert_eq!(
        crate::cli::error_code(&full),
        ("storage_full", 6),
        "{full:#}"
    );
    session.sql(|db| {
        db.execute(
            "UPDATE captures SET data_bytes = ?",
            params![MAX_STORED_BYTES - 100],
        )
    });
    session.recount();
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
        db.execute(
            "UPDATE capture_session SET schema_version = ?",
            params![SCHEMA_VERSION],
        )
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

/// Incompressible frame bytes, as a real sensor gives.
fn noise(seed: u64, width: u32, height: u32) -> Frame {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let data = (0..width as usize * height as usize)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    Frame {
        id: seed,
        width,
        height,
        pixel_format: MONO8,
        timestamp_ns: seed,
        data,
    }
}

/// Store `count` 4 MiB frames under a 32 MB DuckDB limit, then review two
/// suggestions. Schema 1 kept pixels in the database and failed fatally
/// after 16 MiB with this limit ("could not allocate block").
fn store_beyond_memory_limit(count: u64) {
    let file = SessionFile::new();
    let mut session = file.create();
    session.sql(|db| db.execute_batch("SET memory_limit = '32MB'"));
    for seed in 1..=count - 2 {
        session.manual(&noise(seed, 2048, 2048)).unwrap();
    }
    session.configure(true, 0.5, 250).unwrap();
    let start = Instant::now() + Duration::from_secs(31);
    let kept = observe_at(&mut session, &noise(count - 1, 2048, 2048), start)
        .expect("a frame like the examples is suggested");
    let later = start + Duration::from_secs(31);
    let discarded = observe_at(&mut session, &noise(count, 2048, 2048), later)
        .expect("a frame like the examples is suggested");
    let review = session.next_review().unwrap().unwrap();
    assert_eq!(review.id, kept);
    assert_eq!(review.frame.data, noise(count - 1, 2048, 2048).data);
    session.review(kept, true).unwrap();
    session.review(discarded, false).unwrap();
    session.close().unwrap();
    let stored: u64 = file
        .frame_files()
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .sum();
    assert_eq!(stored, (count - 1) * 4 * 1024 * 1024);
    let resumed = CaptureSession::open(&file.0, "test-camera", false).unwrap();
    let status = resumed.status().unwrap();
    assert_eq!(
        (
            status.manual_count,
            status.kept_count,
            status.discarded_count,
            status.pending_count
        ),
        (count - 2, 1, 1, 0)
    );
}

#[test]
fn sessions_hold_frames_far_beyond_the_database_memory_limit() {
    store_beyond_memory_limit(16);
}

#[test]
#[ignore = "writes 300 MiB of frames"]
fn sessions_hold_hundreds_of_megabytes_of_frames() {
    store_beyond_memory_limit(75);
}

#[test]
fn suggestions_never_wait_for_a_busy_writer() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    // A writer stalled by a slow card holds its current job until released.
    let (release, stalled) = std::sync::mpsc::channel::<()>();
    let (running, started) = std::sync::mpsc::channel::<()>();
    session
        .writer
        .post(move |_| {
            running.send(()).unwrap();
            let _ = stalled.recv();
            Ok(())
        })
        .unwrap();
    started.recv().unwrap();
    let t = Instant::now();
    let started = Instant::now();
    let queued = session.observe_at(&solid(140), t).unwrap();
    let first = started.elapsed();
    let started = Instant::now();
    let skipped = session
        .observe_at(&solid(100), t + Duration::from_secs(3))
        .unwrap();
    let second = started.elapsed();
    assert!(
        first < Duration::from_millis(5) && second < Duration::from_millis(5),
        "observe waited for the writer: {first:?}, {second:?}"
    );
    let status = session.status().unwrap();
    assert!(queued.is_some(), "one suggestion waits in the queue");
    assert!(skipped.is_none(), "a full writer skips the suggestion");
    assert_eq!(status.note.as_deref(), Some(BUSY_NOTE));
    assert_eq!(
        status.pending_count, 1,
        "a queued suggestion counts toward the limits"
    );
    release.send(()).unwrap();
    assert_eq!(session.next_review().unwrap().unwrap().id, queued.unwrap());
}

#[test]
fn keep_changes_only_the_label_and_discard_deletes_the_frame_file() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    let kept = observe(&mut session, &solid(140)).unwrap();
    let discarded = observe(&mut session, &solid(100)).unwrap();
    let kept_path = file.frames().join(format!("{kept:08}.raw"));
    let discarded_path = file.frames().join(format!("{discarded:08}.raw"));
    let before = std::fs::metadata(&kept_path).unwrap().modified().unwrap();
    session.review(kept, true).unwrap();
    session.review(discarded, false).unwrap();
    assert_eq!(std::fs::read(&kept_path).unwrap(), solid(140).data);
    assert_eq!(
        std::fs::metadata(&kept_path).unwrap().modified().unwrap(),
        before
    );
    assert!(!discarded_path.exists(), "discard deletes the frame file");
    let rows: Vec<(u64, String, Option<String>, u64)> = session.sql(|db| {
        db.prepare("SELECT id, label, frame_path, data_bytes FROM captures ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect()
    });
    assert_eq!(
        rows[1..],
        [
            (kept, "kept".into(), Some(format!("{kept:08}.raw")), 1024),
            (discarded, "discarded".into(), None, 0),
        ]
    );
    // The ledger no longer tracks the deleted frame.
    assert!(!crate::storage::remove(&discarded_path).unwrap());
}

#[test]
fn discard_leaves_a_frame_file_that_changed_after_saving() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    let id = observe(&mut session, &solid(140)).unwrap();
    let path = file.frames().join(format!("{id:08}.raw"));
    std::fs::write(&path, vec![7_u8; 1024]).unwrap();
    session.review(id, false).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), vec![7_u8; 1024]);
    let status = session.status().unwrap();
    assert_eq!((status.discarded_count, status.pending_count), (1, 0));
    assert!(status.note.unwrap().contains("left"));
}

#[test]
fn a_changed_pending_frame_leaves_the_review_queue_instead_of_blocking_it() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    let changed = observe(&mut session, &solid(140)).unwrap();
    let next = observe(&mut session, &solid(100)).unwrap();
    let path = file.frames().join(format!("{changed:08}.raw"));
    std::fs::write(&path, vec![7_u8; 1024]).unwrap();
    let review = session.next_review().unwrap().unwrap();
    assert_eq!(
        (review.id, review.frame.data),
        (next, solid(100).data),
        "a changed frame is never shown, and the next suggestion is"
    );
    let status = session.status().unwrap();
    assert_eq!(status.pending_count, 1);
    assert!(
        status
            .note
            .unwrap()
            .contains(&format!("capture {changed}: its frame file"))
    );
    assert_eq!(std::fs::read(&path).unwrap(), vec![7_u8; 1024]);
}

#[test]
fn a_pending_capture_whose_frame_was_deleted_leaves_the_review_queue() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    let gone = observe(&mut session, &solid(140)).unwrap();
    let next = observe(&mut session, &solid(100)).unwrap();
    // A missing folder, as when only the database was copied, drops nothing,
    // even after another frame is saved.
    let moved = file.0.with_extension("moved");
    std::fs::rename(file.frames(), &moved).unwrap();
    assert!(session.next_review().is_err());
    assert!(session.manual(&solid(60)).is_err());
    assert!(!file.frames().exists(), "a save never recreates the folder");
    assert!(session.next_review().is_err());
    std::fs::rename(&moved, file.frames()).unwrap();
    assert_eq!(session.status().unwrap().pending_count, 2);
    // Retention, or a user, deleted one frame.
    std::fs::remove_file(file.frames().join(format!("{gone:08}.raw"))).unwrap();
    assert_eq!(session.next_review().unwrap().unwrap().id, next);
    let status = session.status().unwrap();
    assert_eq!(status.pending_count, 1);
    assert!(status.note.unwrap().contains(&format!("capture {gone}")));
    assert!(session.review(gone, false).is_err());
}

#[test]
fn storage_refusals_pause_suggestions_and_other_write_failures_stop_them() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    lock(&session.shared).failure = Some(anyhow!("Storage full: test budget"));
    assert!(observe(&mut session, &solid(140)).is_none());
    let status = session.status().unwrap();
    assert_eq!(status.state, "storage_full");
    assert!(status.blocked_storage);
    assert_eq!(status.note.as_deref(), Some("Storage full: test budget"));
    session.last_sample = None;
    assert!(
        observe_at(&mut session, &solid(140), Instant::now() + STORAGE_RETRY).is_some(),
        "storage is tried again later"
    );
    assert_eq!(session.status().unwrap().note, None);
    lock(&session.shared).failure = Some(anyhow!("disk failed"));
    session.last_sample = None;
    assert!(session.observe(&solid(100)).is_err());
}

#[test]
fn a_frame_that_cannot_be_saved_now_is_retried_without_pausing() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    // Another process holds the ledger, as while a long recording finishes.
    lock(&session.shared).failure = Some(
        anyhow!("storage manager is busy; retry after other capture processes finish bookkeeping")
            .context(FrameNotSaved),
    );
    let t = Instant::now();
    assert!(observe_at(&mut session, &solid(140), t).is_none());
    let status = session.status().unwrap();
    assert_eq!(
        (status.state.as_str(), status.blocked_storage),
        ("watching", false)
    );
    assert!(status.note.unwrap().contains("storage manager is busy"));
    assert!(
        observe_at(&mut session, &solid(140), t + WRITE_RETRY).is_some(),
        "the next attempt comes soon"
    );
    // A frames folder moved away fails each save the same way.
    let moved = file.0.with_extension("moved");
    std::fs::rename(file.frames(), &moved).unwrap();
    assert!(observe_at(&mut session, &solid(100), t + WRITE_RETRY * 2).is_some());
    assert_eq!(
        session.status().unwrap().pending_count,
        1,
        "a failed save releases its count"
    );
    assert!(
        session
            .observe_at(&solid(60), t + WRITE_RETRY * 3)
            .unwrap()
            .is_none()
    );
    let status = session.status().unwrap();
    assert!(status.enabled);
    assert!(status.note.unwrap().contains("is missing"));
    std::fs::rename(&moved, file.frames()).unwrap();
}

#[test]
fn changing_settings_forgets_failures_and_notes_from_earlier_writes() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    // A queued write fails while the user pauses auto capture.
    session
        .writer
        .post(|_| bail!("Storage full: old refusal"))
        .unwrap();
    session.configure(false, 0.5, 250).unwrap();
    ready(&mut session);
    assert!(
        observe(&mut session, &solid(140)).is_some(),
        "turning auto capture on tries storage again"
    );
    assert_eq!(session.status().unwrap().state, "watching");
    lock(&session.shared).failure = Some(anyhow!("Storage full: test budget"));
    assert!(observe(&mut session, &solid(100)).is_none());
    session.configure(false, 0.5, 250).unwrap();
    assert_eq!(session.status().unwrap().note, None);
    // Other notes fade.
    lock(&session.shared).note = Some((
        Instant::now().checked_sub(NOTE_LIFETIME).unwrap(),
        BUSY_NOTE.into(),
    ));
    assert_eq!(session.status().unwrap().note, None);
}

#[test]
fn a_discard_that_cannot_be_saved_keeps_the_frame() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    ready(&mut session);
    let id = observe(&mut session, &solid(140)).unwrap();
    let path = file.frames().join(format!("{id:08}.raw"));
    // An uncommitted change to the row makes the label update conflict.
    let blocker = session.sql(move |db| {
        let other = db.try_clone()?;
        other.execute_batch(&format!(
            "BEGIN; UPDATE captures SET label = 'kept' WHERE id = {id};"
        ))?;
        Ok(other)
    });
    assert!(session.review(id, false).is_err());
    assert!(path.exists(), "the frame stays until the decision is saved");
    drop(blocker);
    assert_eq!(session.next_review().unwrap().unwrap().id, id);
    session.review(id, false).unwrap();
    assert!(!path.exists());
}

#[test]
fn a_new_session_never_reuses_a_frames_folder() {
    let file = SessionFile::new();
    std::fs::create_dir(file.frames()).unwrap();
    assert!(CaptureSession::open(&file.0, "test-camera", true).is_err());
    assert!(!file.0.exists());
    std::fs::remove_dir(file.frames()).unwrap();
    // The folder is claimed with the session, before any frame is saved.
    let _session = file.create();
    let other = file.0.with_extension("db");
    assert!(CaptureSession::open(&other, "test-camera", true).is_err());
    assert!(!other.exists());
    let unclaimed = SessionFile::new();
    assert!(
        CaptureSession::open(&unclaimed.frames(), "test-camera", true).is_err(),
        "a session cannot be its own frames folder"
    );
    assert!(!unclaimed.frames().exists());
}

#[test]
fn resuming_without_the_frames_folder_fails_instead_of_dropping_captures() {
    let file = SessionFile::new();
    {
        let mut session = file.create();
        session.manual(&solid(120)).unwrap();
        ready(&mut session);
        observe(&mut session, &solid(140)).unwrap();
        observe(&mut session, &solid(100)).unwrap();
    }
    // As when only the database was copied.
    let moved = file.0.with_extension("moved");
    std::fs::rename(file.frames(), &moved).unwrap();
    let missing = CaptureSession::open(&file.0, "test-camera", false)
        .err()
        .expect("a session whose frames are elsewhere does not open");
    assert!(format!("{missing:#}").contains("is missing"), "{missing:#}");
    assert!(!file.frames().exists());
    std::fs::rename(&moved, file.frames()).unwrap();
    let session = CaptureSession::open(&file.0, "test-camera", false).unwrap();
    assert_eq!(session.status().unwrap().pending_count, 2);
    assert!(session.next_review().unwrap().is_some());
    drop(session);
    // A session that never saved a frame claims a new folder.
    let empty = SessionFile::new();
    empty.create().close().unwrap();
    std::fs::remove_dir(empty.frames()).unwrap();
    CaptureSession::open(&empty.0, "test-camera", false).unwrap();
    assert!(empty.frames().is_dir());
}

#[test]
fn close_finishes_even_when_duckdb_cannot_checkpoint() {
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    let blocker = session.block_checkpoints();
    assert!(session.close().is_err());
    drop(blocker);
    // The WAL keeps every committed write.
    let resumed = CaptureSession::open(&file.0, "test-camera", false).unwrap();
    assert_eq!(resumed.status().unwrap().manual_count, 1);
}

const SCHEMA_1: &str = "CREATE TABLE capture_session (
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
    INSERT INTO capture_session VALUES (1, 1, 1, 'test-camera', 0, true, 0.5, 250);";

#[test]
fn schema_1_sessions_with_pixels_in_the_database_still_review() {
    let file = SessionFile::new();
    let frames = [
        (1, "manual", "manual", solid(120)),
        (2, "automatic", "pending", solid(140)),
        (3, "automatic", "pending", solid(100)),
    ];
    {
        let db = Connection::open(&file.0).unwrap();
        db.execute_batch(SCHEMA_1).unwrap();
        for (id, source, label, frame) in &frames {
            db.execute(
                "INSERT INTO captures VALUES (?, 0, ?, ?, 42, 32, 32, ?, 7, ?, ?, ?, 0.9, 'v1', NULL)",
                params![
                    id,
                    source,
                    label,
                    MONO8,
                    &frame.data,
                    frame.data.len() as u64,
                    encode(&descriptor(frame).unwrap())
                ],
            )
            .unwrap();
        }
    }
    std::fs::create_dir(file.frames()).unwrap();
    assert!(
        CaptureSession::open(&file.0, "test-camera", false).is_err(),
        "a schema 1 session never adopts another session's folder"
    );
    std::fs::remove_dir(file.frames()).unwrap();
    let mut session = CaptureSession::open(&file.0, "test-camera", false).unwrap();
    let status = session.status().unwrap();
    assert_eq!((status.manual_count, status.pending_count), (1, 2));
    assert!(status.enabled);
    let review = session.next_review().unwrap().unwrap();
    assert_eq!((review.id, review.frame.data), (2, solid(140).data));
    assert_eq!(
        session.review_capture(3).unwrap().unwrap().frame.data,
        solid(100).data
    );
    session.review(2, true).unwrap();
    session.review(3, false).unwrap();
    let rows: Vec<(u64, Option<Vec<u8>>, u64)> = session.sql(|db| {
        db.prepare("SELECT id, data, data_bytes FROM captures ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    });
    assert_eq!(
        rows,
        [
            (1, Some(solid(120).data), 1024),
            (2, Some(solid(140).data), 1024),
            (3, None, 0),
        ],
        "keep leaves stored pixels in place; discard releases them"
    );
    let id = session.manual(&solid(60)).unwrap();
    assert_eq!(id, 4);
    assert_eq!(file.frame_files(), [file.frames().join("00000004.raw")]);
    drop(session);
    let version: u32 = Connection::open(&file.0)
        .unwrap()
        .query_row("SELECT schema_version FROM capture_session", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let resumed = CaptureSession::open(&file.0, "test-camera", false).unwrap();
    let status = resumed.status().unwrap();
    assert_eq!(
        (
            status.manual_count,
            status.kept_count,
            status.discarded_count
        ),
        (2, 1, 1)
    );
    assert_eq!(resumed.positive.len(), 3);
}
