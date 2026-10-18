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
        db.execute(
            "UPDATE capture_session SET schema_version = ?",
            params![SCHEMA_VERSION],
        )
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

/// The tables Capturefab wrote before encoder choice and frame files.
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
    CREATE TABLE background (id UINTEGER PRIMARY KEY, descriptor BLOB NOT NULL);";
/// Schema 2 added the encoder; frames stayed in the database.
const SCHEMA_2: &str = "ALTER TABLE capture_session ADD COLUMN encoder VARCHAR DEFAULT 'native';
    ALTER TABLE capture_session ADD COLUMN descriptor_dims UINTEGER DEFAULT 84;
    ALTER TABLE capture_session ADD COLUMN model_fingerprint VARCHAR;
    UPDATE capture_session SET schema_version = 2;";
/// A session written by an earlier version, with `frames` stored in the database.
fn legacy_session(file: &SessionFile, schema: &[&str], frames: &[(u64, &str, &str, Frame)]) {
    let db = Connection::open(&file.0).unwrap();
    db.execute_batch(SCHEMA_1).unwrap();
    db.execute_batch(
        "INSERT INTO capture_session VALUES (1, 1, 1, 'test-camera', 0, true, 0.25, 500);",
    )
    .unwrap();
    for sql in schema {
        db.execute_batch(sql).unwrap();
    }
    for (id, source, label, frame) in frames {
        db.execute(
            "INSERT INTO captures VALUES (?, 0, ?, ?, 42, 32, 32, ?, 7, ?, ?, ?, 0.9, 'old', NULL)",
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
fn schema_version(file: &SessionFile) -> u32 {
    Connection::open(&file.0)
        .unwrap()
        .query_row("SELECT schema_version FROM capture_session", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn version_1_sessions_upgrade_in_place_as_native() {
    let file = SessionFile::new();
    legacy_session(&file, &[], &[(1, "manual", "manual", solid(120))]);
    let session = file.resume().unwrap();
    let status = session.status().unwrap();
    assert_eq!(status.encoder, "native");
    assert_eq!((status.manual_count, status.cooldown_ms), (1, 500));
    assert_eq!(session.positive, vec![descriptor(&solid(120)).unwrap()]);
    drop(session);
    assert_eq!(schema_version(&file), SCHEMA_VERSION);
}

#[test]
fn sessions_with_pixels_in_the_database_still_review() {
    for schema in [&[][..], &[SCHEMA_2][..]] {
        let file = SessionFile::new();
        legacy_session(
            &file,
            schema,
            &[
                (1, "manual", "manual", solid(120)),
                (2, "automatic", "pending", solid(140)),
                (3, "automatic", "pending", solid(100)),
            ],
        );
        std::fs::create_dir(file.frames()).unwrap();
        assert!(
            file.resume().is_err(),
            "an older session never adopts another session's folder"
        );
        assert_eq!(schema_version(&file), 1 + schema.len() as u32);
        std::fs::remove_dir(file.frames()).unwrap();
        let mut session = file.resume().unwrap();
        let status = session.status().unwrap();
        assert_eq!((status.manual_count, status.pending_count), (1, 2));
        assert_eq!(status.encoder, "native");
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
        assert_eq!(schema_version(&file), SCHEMA_VERSION);
        let resumed = file.resume().unwrap();
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
    assert!(!file.frames().exists());
    let session = CaptureSession::open(&file.0, "test-camera", true, None).unwrap();
    assert_eq!(session.status().unwrap().encoder, "native");
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
/// suggestions. Sessions that kept pixels in the database failed fatally
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
    let resumed = file.resume().unwrap();
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
    // A writer stalled by a slow card holds its current task until released.
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
    lock(&session.shared).failure = Some(anyhow!(crate::storage::Busy).context(FrameNotSaved));
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

/// Run a test in a child with its own session directory, so it can change
/// the global storage policy without affecting other tests.
#[cfg(unix)]
fn isolated(name: &str) -> bool {
    if std::env::var_os("CAPTUREFAB_AUTOCAPTURE_STORAGE_TEST").is_some() {
        return true;
    }
    let dir = std::env::temp_dir().join(format!(
        "capturefab-autocapture-{}-{}",
        std::process::id(),
        epoch_ms()
    ));
    crate::ipc::ensure_private_dir(&dir).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("autocapture::tests::{name}"),
            "--nocapture",
        ])
        .env("CAPTUREFAB_SESSION_DIR", &dir)
        .env("CAPTUREFAB_AUTOCAPTURE_STORAGE_TEST", "1")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[cfg(unix)]
#[test]
fn frames_keep_to_the_configured_storage_budget_not_the_default_one() {
    if !isolated("frames_keep_to_the_configured_storage_budget_not_the_default_one") {
        return;
    }
    // The user raised the budget to twice the default and lets the oldest
    // captures go once it is full.
    let default = crate::storage::StoragePolicy::default();
    let configured = crate::storage::StoragePolicy {
        max_bytes: default.max_bytes * 2,
        on_full: "delete-oldest".into(),
        ..default.clone()
    };
    crate::storage::configure_policy(&configured).unwrap();
    let captures = crate::ipc::session_dir().join("captures");
    std::fs::create_dir(&captures).unwrap();
    let oldest = captures.join("oldest.png");
    crate::storage::save_bytes(&[1; 8192], &oldest, &configured).unwrap();
    // Stored captures fill all but 4 KiB of the default budget: a sparse
    // file the ledger counts at its length once it reconciles.
    let large = captures.join("large.raw");
    crate::storage::save_bytes(&[2], &large, &configured).unwrap();
    File::options()
        .write(true)
        .open(&large)
        .unwrap()
        .set_len(default.max_bytes - 4096)
        .unwrap();
    crate::storage::quota_status().unwrap();
    let file = SessionFile::new();
    let mut session = file.create();
    session.manual(&solid(120)).unwrap();
    assert!(
        oldest.exists(),
        "a frame within the configured budget removes no capture"
    );
    assert_eq!(file.frame_files().len(), 1);
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
    assert!(CaptureSession::open(&file.0, "test-camera", true, NATIVE).is_err());
    assert!(!file.0.exists());
    std::fs::remove_dir(file.frames()).unwrap();
    // The folder is claimed with the session, before any frame is saved.
    let _session = file.create();
    let other = file.0.with_extension("db");
    assert!(CaptureSession::open(&other, "test-camera", true, NATIVE).is_err());
    assert!(!other.exists());
    let unclaimed = SessionFile::new();
    assert!(
        CaptureSession::open(&unclaimed.frames(), "test-camera", true, NATIVE).is_err(),
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
    let missing = file
        .resume()
        .err()
        .expect("a session whose frames are elsewhere does not open");
    assert!(format!("{missing:#}").contains("is missing"), "{missing:#}");
    assert!(!file.frames().exists());
    std::fs::rename(&moved, file.frames()).unwrap();
    let session = file.resume().unwrap();
    assert_eq!(session.status().unwrap().pending_count, 2);
    assert!(session.next_review().unwrap().is_some());
    drop(session);
    // A session that never saved a frame claims a new folder.
    let empty = SessionFile::new();
    empty.create().close().unwrap();
    std::fs::remove_dir(empty.frames()).unwrap();
    empty.resume().unwrap();
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
    let resumed = file.resume().unwrap();
    assert_eq!(resumed.status().unwrap().manual_count, 1);
}

#[test]
fn cpu_embeddings_rest_to_stay_within_their_share_of_a_core() {
    let took = Duration::from_millis(100);
    // Four busy threads for 100 ms is 400 ms of processor time: at half a
    // core the cycle lasts 800 ms, at a fifth 2 s.
    let cpu = took * 4;
    assert_eq!(rest_after(cpu, took, CPU_SHARE), Duration::from_millis(700));
    assert_eq!(
        rest_after(cpu, took, EFFICIENT_CPU_SHARE),
        Duration::from_millis(1900)
    );
    assert_eq!(rest_after(Duration::ZERO, took, CPU_SHARE), Duration::ZERO);
    // The worker's measured processor time counts, up to every thread of
    // the forward pass; without a measurement all of them are assumed busy.
    let most = took * cpu_threads();
    assert_eq!(Backend::Cpu.cpu_time(took, Some(took)), took);
    assert_eq!(Backend::Cpu.cpu_time(took, Some(most * 2)), most);
    assert_eq!(Backend::Cpu.cpu_time(took, None), most);
    assert!((1..=4).contains(&cpu_threads()));
}

#[cfg(unix)]
#[test]
fn process_cpu_time_counts_work_on_any_thread() {
    let before = process_cpu().unwrap();
    std::thread::spawn(|| {
        let started = Instant::now();
        let mut state = 1_u64;
        while started.elapsed() < Duration::from_millis(100) {
            state = std::hint::black_box(state.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
    })
    .join()
    .unwrap();
    assert!(process_cpu().unwrap() >= before + Duration::from_millis(20));
}

#[test]
fn slow_sampling_still_reports_its_rate_until_it_stops() {
    let mut throughput = Throughput::default();
    let now = Instant::now();
    throughput.record(now - Duration::from_secs(20));
    throughput.record(now - Duration::from_secs(10));
    throughput.record(now);
    assert_eq!(throughput.finished.len(), 2, "the latest two are kept");
    assert_eq!(throughput.rate(), Some(0.1));
    throughput.finished = [now - Duration::from_secs(40), now - Duration::from_secs(30)].into();
    assert_eq!(throughput.rate(), None, "no sample for three spacings");
    let mut fast = Throughput::default();
    for frame in 0..=30 {
        fast.record(now - Duration::from_millis(1000 - frame * 1000 / 30));
    }
    assert!(fast.rate().is_some_and(|rate| (rate - 30.0).abs() < 0.5));
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
        let id = session
            .decide_late(frame, feature.unwrap(), at, finished)
            .unwrap();
        session.flush();
        id
    }
    /// The grid alone decides when to sample; resting has its own test.
    fn rested(session: &mut CaptureSession) {
        session.embedder.as_mut().unwrap().rested = None;
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
        rested(&mut session);
        session.observe_at(&color([220, 40, 40]), now).unwrap();
        session.configure(false, 0.5, 250).unwrap();
        session.configure(true, 0.5, 250).unwrap();
        assert_eq!(settle(&mut session, now + Duration::from_millis(100)), None);
        assert_eq!(session.status().unwrap().pending_count, 1);
        // The same frame sampled after re-arming is captured.
        session.last_sample = None;
        rested(&mut session);
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
        rested(&mut session);
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
        rested(&mut session);
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
    fn cpu_sampling_rests_after_each_embedding() {
        let file = SessionFile::new();
        let mut session = create(&file, Some(EncoderChoice::Dinov2));
        session.manual(&color([220, 40, 40])).unwrap();
        ready(&mut session);
        let t = Instant::now();
        session.observe_at(&color([0, 0, 0]), t).unwrap();
        let (_, _, finished, _) = session.embedder.as_mut().unwrap().wait().unwrap();
        let rested = session.embedder.as_ref().unwrap().rested.unwrap();
        assert!(rested >= finished, "unit tests embed on the CPU");
        // As after a 200 ms embedding: no sample until the CPU has rested.
        let until = finished + Duration::from_secs(1);
        session.embedder.as_mut().unwrap().rested = Some(until);
        session
            .observe_at(&color([0, 0, 0]), until - Duration::from_millis(10))
            .unwrap();
        assert!(!session.embedder.as_ref().unwrap().busy, "still resting");
        session.observe_at(&color([0, 0, 0]), until).unwrap();
        assert!(session.embedder.as_ref().unwrap().busy);
        assert_eq!(
            session.last_sample,
            Some(until),
            "a rested sampler restarts its grid"
        );
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

    #[test]
    fn a_schema_2_session_from_another_model_is_refused_before_any_upgrade() {
        let file = SessionFile::new();
        let model = dinov2::bundled().unwrap().unwrap();
        legacy_session(&file, &[SCHEMA_2], &[]);
        Connection::open(&file.0)
            .unwrap()
            .execute(
                "UPDATE capture_session SET encoder = 'dinov2', descriptor_dims = ?, model_fingerprint = 'abc'",
                params![model.dims() as u32],
            )
            .unwrap();
        let error = file.resume().err().unwrap().to_string();
        assert!(error.contains("learned with DINOv2 model abc"), "{error}");
        assert_eq!(
            schema_version(&file),
            2,
            "the version that made it still opens it"
        );
        assert!(!file.frames().exists());
        Connection::open(&file.0)
            .unwrap()
            .execute(
                "UPDATE capture_session SET model_fingerprint = ?",
                params![model.fingerprint()],
            )
            .unwrap();
        let session = file.resume().unwrap();
        assert_eq!(session.status().unwrap().encoder, "dinov2");
        drop(session);
        assert_eq!(schema_version(&file), SCHEMA_VERSION);
    }
}
