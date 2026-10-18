//! Session-local, human-supervised automatic capture. A session is a DuckDB
//! database plus a `<name>.frames` folder beside it: the database holds
//! decisions, settings, descriptors and the bounded background model, and each
//! raw sensor frame is a file written through the storage ledger. Copy the
//! database and its folder together. No network service or downloaded weights
//! are needed.
use crate::{frame, storage, types::Frame};
use anyhow::{Context, Result, anyhow, bail, ensure};
use duckdb::{Config, Connection, OptionalExt, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: u32 = 2;
const DESCRIPTOR_VERSION: u32 = 1;
const FEATURES: usize = 84;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_PENDING: u64 = 200;
const MAX_STORED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_BACKGROUND: usize = 64;
const MAX_RECENT: usize = 32;
/// Frames live in files, so DuckDB only ever holds small rows.
const MEMORY_LIMIT: &str = "64MB";
/// Sessions from schema 1 keep raw pixels in the database, with its old budget.
const LEGACY_MEMORY_LIMIT: &str = "256MB";
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);
/// How long a storage refusal pauses suggestions before the next attempt.
const STORAGE_RETRY: Duration = Duration::from_secs(60);
/// How long suggestions wait after a frame failed to save for another
/// reason, such as a ledger busy with another process's bookkeeping.
const WRITE_RETRY: Duration = Duration::from_secs(5);
/// How long a note stays in the status.
const NOTE_LIFETIME: Duration = Duration::from_secs(60);
const BUSY_NOTE: &str = "Skipped a suggestion while the previous capture was still being saved";
const MODEL: &str = "Session exemplar kNN + background novelty (spatial color/edge descriptor v1)";
type Descriptor = Vec<f32>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub path: String,
    pub camera: String,
    pub enabled: bool,
    pub sensitivity: f64,
    pub cooldown_ms: u64,
    pub manual_count: u64,
    pub kept_count: u64,
    pub discarded_count: u64,
    pub pending_count: u64,
    pub blocked_storage: bool,
    pub state: String,
    pub model: String,
    /// Why the latest suggestion was skipped or a capture was dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewCapture {
    pub id: u64,
    pub frame: Frame,
    pub score: f64,
    pub reason: String,
}

pub struct CaptureSession {
    writer: Writer,
    shared: Arc<Mutex<Shared>>,
    path: PathBuf,
    camera: String,
    enabled: bool,
    sensitivity: f64,
    cooldown_ms: u64,
    next_id: u64,
    positive: Vec<Descriptor>,
    negative: Vec<Descriptor>,
    background: Vec<Descriptor>,
    recent: Vec<(Descriptor, u64)>,
    last_sample: Option<Instant>,
    last_capture: Option<Instant>,
    started_at: Instant,
    started_ms: u64,
    blocked_storage: bool,
    /// When storage last refused a suggested frame.
    refused: Option<Instant>,
    /// When a suggested frame last failed to save for another reason.
    unsaved: Option<Instant>,
}

/// State shared by the camera thread and the writer. Counts are reserved when
/// a capture is queued and released if its write fails, so the limits also
/// cover frames still being written, and status never queries DuckDB.
#[derive(Default)]
struct Shared {
    tally: Tally,
    /// Jobs waiting for the writer; it runs one more while one waits.
    queued: usize,
    /// A queued write's failure, reported by the next observe.
    failure: Option<anyhow::Error>,
    /// The latest note and when it was made; it fades after NOTE_LIFETIME.
    note: Option<(Instant, String)>,
}

impl Shared {
    fn note(&mut self, note: String) {
        self.note = Some((Instant::now(), note));
    }
}

/// Context for a frame file storage could not save. Storage can recover,
/// so a suggestion that fails this way is retried instead of pausing.
#[derive(Debug)]
struct FrameNotSaved;

impl std::fmt::Display for FrameNotSaved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("save capture frame file")
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    manual: u64,
    kept: u64,
    discarded: u64,
    pending: u64,
    bytes: u64,
}

impl Tally {
    fn count(&mut self, label: &str) -> &mut u64 {
        match label {
            "manual" => &mut self.manual,
            "kept" => &mut self.kept,
            "discarded" => &mut self.discarded,
            _ => &mut self.pending,
        }
    }
    fn add(&mut self, label: &str, bytes: u64) {
        *self.count(label) += 1;
        self.bytes = self.bytes.saturating_add(bytes);
    }
    fn remove(&mut self, label: &str, bytes: u64) {
        let count = self.count(label);
        *count = count.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(bytes);
    }
}

/// A capture handed to the writer; its counts are already reserved.
struct Row {
    id: u64,
    created_ms: u64,
    source: &'static str,
    label: &'static str,
    frame: Frame,
    feature: Descriptor,
    score: f64,
    reason: String,
}

impl CaptureSession {
    /// Create a new session or reopen one without changing its settings or labels.
    /// A session is bound to one camera identity to avoid mixing unrelated scenes.
    pub fn open(path: &Path, camera: &str, create: bool) -> Result<Self> {
        ensure!(
            !camera.trim().is_empty(),
            "capture session requires a camera identity"
        );
        ensure!(camera.len() <= 1024, "camera identity is too long");
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        if let Some(parent) = path.parent() {
            crate::volumes::ensure_present(parent)?;
        }
        let frames = path.with_extension("frames");
        ensure!(
            frames != path,
            "capture session file name must not end in .frames"
        );
        if create {
            ensure!(
                !path.exists(),
                "capture session already exists; resume it instead"
            );
            // Claim the folder before the database, so sessions whose names
            // differ only in extension never share one.
            claim(&frames)?;
        } else {
            ensure!(
                path.is_file(),
                "capture session does not exist: {}",
                path.display()
            );
        }
        let session = Self::load(path, &frames, camera, create);
        // A failed `new` leaves no folder behind to block the next attempt.
        if create && session.is_err() {
            let _ = std::fs::remove_dir(&frames);
        }
        session
    }

    fn load(path: PathBuf, frames: &Path, camera: &str, create: bool) -> Result<Self> {
        let config = Config::default()
            .max_memory(MEMORY_LIMIT)?
            .threads(1)?
            .enable_autoload_extension(false)?;
        let db =
            Connection::open_with_flags(&path, config).context("open capture session DuckDB")?;
        // Never ask DuckDB to load extensions, read external data, or spill a
        // second session data file beside the database.
        db.execute_batch("SET temp_directory = ''; SET enable_external_access = false;")?;
        if create {
            // `data` holds pixels only in sessions from schema 1; newer rows
            // name a file in the frames folder and its SHA-256.
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
                    reason VARCHAR NOT NULL, reviewed_ms UBIGINT, frame_path VARCHAR, frame_sha256 VARCHAR
                );
                CREATE TABLE background (id UINTEGER PRIMARY KEY, descriptor BLOB NOT NULL);"
            )?;
            db.execute(
                "INSERT INTO capture_session VALUES (1, ?, ?, ?, ?, false, 0.5, 2000)",
                params![SCHEMA_VERSION, DESCRIPTOR_VERSION, camera, epoch_ms()],
            )?;
            db.execute_batch("CHECKPOINT")?;
        }
        let (version, descriptor_version, stored_camera, enabled, sensitivity, cooldown_ms): (u32, u32, String, bool, f64, u64) = db.query_row(
            "SELECT schema_version, descriptor_version, camera, enabled, sensitivity, cooldown_ms FROM capture_session WHERE singleton = 1", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
            .context("not a CaptureFab capture session or missing session metadata")?;
        ensure!(
            version == 1 || version == SCHEMA_VERSION,
            "unsupported capture session schema version {version}"
        );
        ensure!(
            descriptor_version == DESCRIPTOR_VERSION,
            "unsupported capture session descriptor version {descriptor_version}"
        );
        ensure!(
            stored_camera == camera,
            "capture session belongs to camera {stored_camera}, not {camera}"
        );
        validate_settings(sensitivity, cooldown_ms)?;
        let (positive, negative) = examples(&db)?;
        let background = load_descriptors(
            &db,
            "SELECT descriptor FROM background ORDER BY id LIMIT 64",
        )?;
        let mut recent = Vec::new();
        {
            let mut statement = db.prepare("SELECT descriptor, created_ms FROM captures WHERE label <> 'discarded' ORDER BY id DESC LIMIT 32")?;
            let rows =
                statement.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u64>(1)?)))?;
            for row in rows {
                let (bytes, created_ms) = row?;
                recent.push((decode(&bytes)?, created_ms));
            }
            recent.reverse();
        }
        let (last_id, last_ms): (Option<u64>, Option<u64>) =
            db.query_row("SELECT MAX(id), MAX(created_ms) FROM captures", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let legacy = version == 1
            || db.query_row(
                "SELECT COUNT(*) FROM captures WHERE frame_path IS NULL AND data_bytes > 0",
                [],
                |r| r.get::<_, u64>(0),
            )? > 0;
        if legacy {
            db.execute_batch(&format!("SET memory_limit = '{LEGACY_MEMORY_LIMIT}'"))?;
        }
        if version == 1 {
            // Schema 1 sessions had no folder, so one by this name is another session's.
            claim(frames)?;
            upgrade(&db).inspect_err(|_| {
                let _ = std::fs::remove_dir(frames);
            })?;
        } else if !frames.is_dir() {
            // Saving a frame would recreate the folder, and the next review
            // would then take every saved frame for deleted.
            let files: u64 = db.query_row(
                "SELECT COUNT(*) FROM captures WHERE frame_path IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                files == 0,
                "capture session frames folder {} is missing; copy it beside the database, or create an empty folder there to drop the frames it held",
                frames.display()
            );
            claim(frames)?;
        }
        let shared = Arc::new(Mutex::new(Shared {
            tally: counts(&db)?,
            ..Shared::default()
        }));
        let writer = Writer::spawn(Store {
            db,
            frames: frames.to_owned(),
            dirty_since: None,
            shared: shared.clone(),
        })?;
        let now = Instant::now();
        let started_ms = epoch_ms();
        let last_capture = last_ms
            .and_then(|ms| now.checked_sub(Duration::from_millis(epoch_ms().saturating_sub(ms))));
        Ok(Self {
            writer,
            shared,
            path,
            camera: stored_camera,
            enabled,
            sensitivity,
            cooldown_ms,
            next_id: last_id
                .unwrap_or(0)
                .checked_add(1)
                .context("capture session identifier overflow")?,
            positive,
            negative,
            background,
            recent,
            last_sample: None,
            last_capture,
            started_at: now,
            started_ms,
            blocked_storage: false,
            refused: None,
            unsaved: None,
        })
    }

    pub fn configure(&mut self, enabled: bool, sensitivity: f64, cooldown_ms: u64) -> Result<()> {
        validate_settings(sensitivity, cooldown_ms)?;
        self.writer.call(move |store| {
            store.db.execute("UPDATE capture_session SET enabled = ?, sensitivity = ?, cooldown_ms = ? WHERE singleton = 1", params![enabled, sensitivity, cooldown_ms])?;
            store.wrote();
            Ok(())
        })?;
        self.enabled = enabled;
        self.sensitivity = sensitivity;
        self.cooldown_ms = cooldown_ms;
        // Changing settings is also how a user retries after freeing space.
        self.retry_storage();
        lock(&self.shared).note = None;
        Ok(())
    }

    /// Let the next suggestion try storage again. Every queued write has
    /// landed by now, so a failure one left behind is stale.
    fn retry_storage(&mut self) {
        lock(&self.shared).failure = None;
        self.refused = None;
        self.unsaved = None;
        self.blocked_storage = false;
    }

    pub fn status(&self) -> Result<SessionStatus> {
        let shared = lock(&self.shared);
        let tally = shared.tally;
        let blocked_storage = self.blocked_storage || tally.bytes >= MAX_STORED_BYTES;
        let state = if !self.enabled {
            "paused"
        } else if tally.pending >= MAX_PENDING {
            "review_queue_full"
        } else if blocked_storage {
            "storage_full"
        } else if self.positive.is_empty() {
            "learning"
        } else {
            "watching"
        };
        Ok(SessionStatus {
            path: self.path.to_string_lossy().into_owned(),
            camera: self.camera.clone(),
            enabled: self.enabled,
            sensitivity: self.sensitivity,
            cooldown_ms: self.cooldown_ms,
            manual_count: tally.manual,
            kept_count: tally.kept,
            discarded_count: tally.discarded,
            pending_count: tally.pending,
            blocked_storage,
            state: state.into(),
            model: MODEL.into(),
            note: shared
                .note
                .as_ref()
                .filter(|(at, _)| at.elapsed() < NOTE_LIFETIME)
                .map(|(_, note)| note.clone()),
        })
    }

    /// Evaluate at most four small previews per second. Automatic suggestions do
    /// not become training examples until the user reviews them. Scoring stays in
    /// memory; a suggestion is written by the session's writer, so acquisition
    /// never waits for the disk.
    pub fn observe(&mut self, frame: &Frame) -> Result<Option<u64>> {
        self.observe_at(frame, Instant::now())
    }

    fn observe_at(&mut self, frame: &Frame, now: Instant) -> Result<Option<u64>> {
        if !self.enabled
            || self
                .last_sample
                .is_some_and(|last| now.saturating_duration_since(last) < SAMPLE_INTERVAL)
        {
            return Ok(None);
        }
        self.last_sample = Some(now);
        let tally = self.settle(now)?;
        self.refused = self
            .refused
            .filter(|at| now.saturating_duration_since(*at) < STORAGE_RETRY);
        self.unsaved = self
            .unsaved
            .filter(|at| now.saturating_duration_since(*at) < WRITE_RETRY);
        self.blocked_storage = self.refused.is_some()
            || tally.bytes.saturating_add(frame.data.len() as u64) > MAX_STORED_BYTES;
        if tally.pending >= MAX_PENDING || self.blocked_storage || self.unsaved.is_some() {
            return Ok(None);
        }
        let feature = descriptor(frame)?;
        let (score, reason) = self.score(&feature);
        let now_ms = self.started_ms.saturating_add(
            now.saturating_duration_since(self.started_at)
                .as_millis()
                .min(u64::MAX as u128) as u64,
        );
        let duplicate_window_ms = self.cooldown_ms.saturating_mul(3).clamp(1000, 30_000);
        self.recent
            .retain(|(_, created_ms)| now_ms.saturating_sub(*created_ms) < duplicate_window_ms);
        let cooling = self.last_capture.is_some_and(|last| {
            now.saturating_duration_since(last) < Duration::from_millis(self.cooldown_ms)
        });
        let duplicate = self
            .recent
            .iter()
            .any(|(previous, _)| distance(previous, &feature) < 0.018);
        // Even the strict endpoint admits an exact positive with no observed
        // background (minimum exact-match score is 0.90).
        let threshold = 0.90 - 0.35 * self.sensitivity;
        if !self.positive.is_empty() && !cooling && !duplicate && score >= threshold {
            // Copy the frame only when the writer can take it.
            if !self.writer.ready() {
                lock(&self.shared).note(BUSY_NOTE.into());
                return Ok(None);
            }
            let row = self.row(
                frame,
                feature.clone(),
                "automatic",
                "pending",
                score,
                reason,
            )?;
            let id = row.id;
            self.writer.post(move |store| store.insert(row))?;
            lock(&self.shared).note = None;
            self.last_capture = Some(now);
            push_bounded(&mut self.recent, (feature, now_ms), MAX_RECENT);
            return Ok(Some(id));
        }
        // Do not absorb a likely target into normal background. Persist only
        // distinct prototypes, keeping both work and on-disk model size bounded.
        if self.background.len() < MAX_BACKGROUND && (self.positive.is_empty() || score < threshold)
        {
            let distinct = self
                .background
                .iter()
                .all(|previous| distance(previous, &feature) >= 0.035);
            // A busy writer skips the prototype; a later sample offers it again.
            if distinct && self.writer.ready() {
                let (id, bytes) = (self.background.len() as u32, encode(&feature));
                self.writer
                    .post(move |store| store.add_background(id, bytes))?;
                self.background.push(feature);
            }
        }
        Ok(None)
    }

    /// Store the exact user-selected sensor frame and immediately teach the
    /// session that this is an interesting example, even while auto mode is off.
    pub fn manual(&mut self, frame: &Frame) -> Result<u64> {
        let feature = descriptor(frame)?;
        let row = self.row(
            frame,
            feature.clone(),
            "manual",
            "manual",
            1.0,
            "User-initiated capture: positive training example".into(),
        )?;
        let id = row.id;
        let (positive, negative) = self.writer.call(move |store| {
            store.insert(row)?;
            examples(&store.db)
        })?;
        self.positive = positive;
        self.negative = negative;
        push_bounded(&mut self.recent, (feature, epoch_ms()), MAX_RECENT);
        self.last_capture = Some(Instant::now());
        Ok(id)
    }

    /// Keeping changes only the label. Discarding deletes the frame file
    /// through the storage ledger and keeps the descriptor and negative
    /// decision for future learning.
    pub fn review(&mut self, id: u64, keep: bool) -> Result<()> {
        let (positive, negative) = self.writer.call(move |store| {
            store.review(id, keep)?;
            examples(&store.db)
        })?;
        self.positive = positive;
        self.negative = negative;
        if !keep {
            self.retry_storage();
        }
        Ok(())
    }

    pub fn next_review(&self) -> Result<Option<ReviewCapture>> {
        self.writer.call(|store| store.load_review(None))
    }

    pub fn review_capture(&self, id: u64) -> Result<Option<ReviewCapture>> {
        self.writer.call(move |store| store.load_review(Some(id)))
    }

    /// Finish queued writes and checkpoint, so the database file is complete
    /// without its WAL. The session closes even if this fails: DuckDB replays
    /// the WAL when the session is resumed.
    pub fn close(mut self) -> Result<()> {
        self.writer.close()
    }

    /// Apply the outcome of queued writes. A frame storage refused or could
    /// not save holds suggestions back for a while; any other failure is
    /// returned so the caller can pause.
    fn settle(&mut self, now: Instant) -> Result<Tally> {
        let mut shared = lock(&self.shared);
        if let Some(error) = shared.failure.take() {
            if crate::cli::error_code(&error).0 == "storage_full" {
                self.refused = Some(now);
            } else if error.downcast_ref::<FrameNotSaved>().is_some() {
                self.unsaved = Some(now);
            } else {
                return Err(error);
            }
            shared.note(format!("{error:#}"));
        }
        Ok(shared.tally)
    }

    /// Assign the next identifier and reserve the capture's counts.
    fn row(
        &mut self,
        frame: &Frame,
        feature: Descriptor,
        source: &'static str,
        label: &'static str,
        score: f64,
        reason: String,
    ) -> Result<Row> {
        let bytes = frame.data.len() as u64;
        let next_id = self
            .next_id
            .checked_add(1)
            .context("capture session identifier overflow")?;
        {
            let mut shared = lock(&self.shared);
            self.blocked_storage = shared.tally.bytes.saturating_add(bytes) > MAX_STORED_BYTES;
            ensure!(
                !self.blocked_storage,
                "Storage full: capture session reached its 2 GiB pixel storage limit; start another session or discard pending captures"
            );
            shared.tally.add(label, bytes);
        }
        let id = self.next_id;
        self.next_id = next_id;
        Ok(Row {
            id,
            created_ms: epoch_ms(),
            source,
            label,
            frame: frame.clone(),
            feature,
            score,
            reason,
        })
    }

    fn score(&self, feature: &Descriptor) -> (f64, String) {
        let Some(positive_distance) = nearest(&self.positive, feature) else {
            return (0.0, "Waiting for a user-initiated capture".into());
        };
        let affinity = similarity(positive_distance);
        let preference = match nearest(&self.negative, feature) {
            Some(negative_distance) => {
                // Distance-ratio preference makes an exact rejection dominate
                // nearby positive examples without suppressing an exact human
                // positive just because a rejected variant was nearby.
                let total = positive_distance + negative_distance;
                let ratio = if total <= f64::EPSILON {
                    0.5
                } else {
                    negative_distance / total
                };
                affinity * ratio
            }
            None => affinity,
        };
        let novelty = nearest(&self.background, feature)
            .map(|d| (d / 0.30).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        // Human preference dominates. Novelty can promote an unusual variant
        // of something the user showed, never an unrelated frame on its own.
        let score = (preference * (0.9 + 0.1 * novelty)).clamp(0.0, 1.0);
        (
            score,
            format!(
                "{:.0}% match to your captures; {:.0}% background novelty{}",
                affinity * 100.0,
                novelty * 100.0,
                if self.negative.is_empty() {
                    ""
                } else {
                    "; adjusted by discarded examples"
                }
            ),
        )
    }
}

type Job = Box<dyn FnOnce(&mut Store) + Send>;

/// The 'capturefab-autocapture' thread. It owns the DuckDB connection and runs
/// every statement and frame write in order, off the camera thread.
struct Writer {
    jobs: Option<SyncSender<Job>>,
    thread: Option<JoinHandle<Result<()>>>,
    shared: Arc<Mutex<Shared>>,
}

impl Writer {
    fn spawn(store: Store) -> Result<Self> {
        let shared = store.shared.clone();
        let (jobs, queue) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("capturefab-autocapture".into())
            .spawn(move || store.run(queue))
            .context("start capture session writer")?;
        Ok(Self {
            jobs: Some(jobs),
            thread: Some(thread),
            shared,
        })
    }

    fn ready(&self) -> bool {
        lock(&self.shared).queued == 0
    }

    /// Queue a job without waiting for it; its failure is reported by the
    /// next observe. Only the camera thread queues jobs, so after `ready`
    /// there is room.
    fn post(&self, job: impl FnOnce(&mut Store) -> Result<()> + Send + 'static) -> Result<()> {
        let jobs = self.jobs.as_ref().ok_or_else(stopped)?;
        lock(&self.shared).queued += 1;
        let job = Box::new(move |store: &mut Store| {
            if let Err(error) = job(store) {
                lock(&store.shared).failure.get_or_insert(error);
            }
        });
        jobs.try_send(job).map_err(|error| {
            lock(&self.shared).queued -= 1;
            match error {
                TrySendError::Full(_) => anyhow!("capture session writer is busy"),
                TrySendError::Disconnected(_) => stopped(),
            }
        })
    }

    /// Run a job after those already queued and wait for its result.
    fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let jobs = self.jobs.as_ref().ok_or_else(stopped)?;
        let (reply, result) = mpsc::channel();
        lock(&self.shared).queued += 1;
        if jobs
            .send(Box::new(move |store: &mut Store| {
                let _ = reply.send(job(store));
            }))
            .is_err()
        {
            lock(&self.shared).queued -= 1;
            return Err(stopped());
        }
        result.recv().map_err(|_| stopped())?
    }

    fn close(&mut self) -> Result<()> {
        self.jobs.take();
        match self.thread.take() {
            Some(thread) => thread.join().map_err(|_| stopped())?,
            None => Ok(()),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn stopped() -> anyhow::Error {
    anyhow!("capture session writer stopped")
}

/// The writer's side of a session: its database connection and frames folder.
struct Store {
    db: Connection,
    frames: PathBuf,
    dirty_since: Option<Instant>,
    shared: Arc<Mutex<Shared>>,
}

impl Store {
    /// Run jobs until the session closes. The WAL makes each write durable;
    /// checkpoints fold it into the database at most once a minute, and on close.
    fn run(mut self, queue: mpsc::Receiver<Job>) -> Result<()> {
        loop {
            let job = match self.dirty_since {
                None => queue.recv().ok(),
                Some(since) => {
                    match queue.recv_timeout(CHECKPOINT_INTERVAL.saturating_sub(since.elapsed())) {
                        Ok(job) => Some(job),
                        Err(RecvTimeoutError::Timeout) => {
                            if self.checkpoint().is_err() {
                                self.dirty_since = Some(Instant::now());
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => None,
                    }
                }
            };
            let Some(job) = job else {
                return self.checkpoint();
            };
            lock(&self.shared).queued -= 1;
            job(&mut self);
        }
    }

    fn wrote(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.db
            .execute_batch("CHECKPOINT")
            .context("checkpoint capture session database")?;
        self.dirty_since = None;
        Ok(())
    }

    /// Save the frame file, then its row, so a row never names a partial file.
    /// A failure releases the counts reserved for the capture.
    fn insert(&mut self, row: Row) -> Result<()> {
        let (label, bytes) = (row.label, row.frame.data.len() as u64);
        let result = self.save(&row);
        if result.is_err() {
            lock(&self.shared).tally.remove(label, bytes);
        }
        result
    }

    fn save(&mut self, row: &Row) -> Result<()> {
        let (name, sha256) = self
            .write_frame(row.id, &row.frame.data)
            .context(FrameNotSaved)?;
        let inserted = self.db.execute(
            "INSERT INTO captures (id, created_ms, source, label, frame_id, width, height, pixel_format,
                timestamp_ns, data_bytes, descriptor, score, reason, frame_path, frame_sha256)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                row.id,
                row.created_ms,
                row.source,
                row.label,
                row.frame.id,
                row.frame.width,
                row.frame.height,
                row.frame.pixel_format,
                row.frame.timestamp_ns,
                row.frame.data.len() as u64,
                encode(&row.feature),
                row.score,
                row.reason,
                name,
                sha256
            ],
        );
        self.wrote();
        if let Err(error) = inserted {
            let _ = storage::remove(&self.frames.join(&name));
            return Err(error.into());
        }
        Ok(())
    }

    fn add_background(&mut self, id: u32, descriptor: Vec<u8>) -> Result<()> {
        self.db.execute(
            "INSERT INTO background VALUES (?, ?)",
            params![id, descriptor],
        )?;
        self.wrote();
        Ok(())
    }

    /// Write a raw frame through the storage ledger, which applies the global
    /// budget, volume checks and never-overwrite. A name left by an interrupted
    /// session is skipped, not replaced.
    fn write_frame(&self, id: u64, data: &[u8]) -> Result<(String, String)> {
        self.check_folder()?;
        for attempt in 0..8 {
            let name = match attempt {
                0 => format!("{id:08}.raw"),
                n => format!("{id:08}-{n}.raw"),
            };
            let mut writer = match storage::create_writer(
                &self.frames.join(&name),
                &storage::StoragePolicy::default(),
                data.len() as u64,
            ) {
                Ok(writer) => writer,
                Err(error) if error.downcast_ref::<storage::OutputExists>().is_some() => continue,
                Err(error) => return Err(error),
            };
            writer.write_all(data)?;
            let sha256 = writer.sha256();
            writer.finish()?;
            return Ok((name, sha256));
        }
        bail!(
            "no free frame file name for capture {id} in {}",
            self.frames.display()
        )
    }

    /// The folder must already exist. Writing never recreates a folder that
    /// was moved away, so a review never takes its frames for deleted.
    fn check_folder(&self) -> Result<()> {
        crate::volumes::ensure_present(&self.frames)?;
        ensure!(
            self.frames.is_dir(),
            "capture session frames folder {} is missing; keep it beside the database",
            self.frames.display()
        );
        Ok(())
    }

    /// A pending-only transaction prevents a double key press or stale review
    /// card from relabeling a capture.
    fn review(&mut self, id: u64, keep: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        let row: Option<(Vec<u8>, u64, Option<String>)> = tx
            .query_row(
                "SELECT descriptor, data_bytes, frame_path FROM captures WHERE id = ? AND label = 'pending'",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (descriptor, bytes, frame_path) = row.context("capture is no longer pending review")?;
        decode(&descriptor)?;
        let file = frame_path
            .map(|name| frame_file(&self.frames, &name))
            .transpose()?;
        let now = epoch_ms();
        let changed = if keep {
            tx.execute(
                "UPDATE captures SET label = 'kept', reviewed_ms = ? WHERE id = ? AND label = 'pending'",
                params![now, id],
            )?
        } else {
            // Release the pixels: schema 1 rows keep them in `data`, newer
            // rows in the file deleted below.
            tx.execute(
                "UPDATE captures SET label = 'discarded', reviewed_ms = ?, data = NULL, data_bytes = 0, frame_path = NULL, frame_sha256 = NULL WHERE id = ? AND label = 'pending'",
                params![now, id],
            )?
        };
        ensure!(changed == 1, "capture is no longer pending review");
        tx.commit()?;
        self.wrote();
        {
            let mut shared = lock(&self.shared);
            shared.tally.remove("pending", if keep { 0 } else { bytes });
            shared.tally.add(if keep { "kept" } else { "discarded" }, 0);
        }
        // Delete only once the decision is saved: a failed delete leaves a
        // file the ledger still tracks, never a lost decision.
        if let (false, Some(path)) = (keep, file) {
            self.delete_frame(id, &path);
        }
        Ok(())
    }

    /// Delete a discarded capture's frame through the ledger. A file that
    /// changed after saving, or cannot be deleted now, stays with a note.
    fn delete_frame(&self, id: u64, path: &Path) {
        let left = match storage::remove(path) {
            Ok(removed) => (!removed && path.exists())
                .then(|| "it changed after saving or was saved on another computer".into()),
            Err(error) => Some(format!("{error:#}")),
        };
        if let Some(reason) = left {
            lock(&self.shared).note(format!(
                "Discarded capture {id} but left {} in place: {reason}",
                path.display()
            ));
        }
    }

    fn load_review(&mut self, id: Option<u64>) -> Result<Option<ReviewCapture>> {
        loop {
            let row = self.db.query_row(
                "SELECT id, frame_id, width, height, pixel_format, timestamp_ns, data, score, reason, data_bytes, frame_path, frame_sha256
                 FROM captures WHERE label = 'pending' AND (?::UBIGINT IS NULL OR id = ?::UBIGINT) ORDER BY id LIMIT 1",
                params![id, id],
                |r| {
                    let capture = ReviewCapture { id: r.get(0)?, frame: Frame { id:r.get(1)?,width:r.get(2)?,height:r.get(3)?,pixel_format:r.get(4)?,timestamp_ns:r.get(5)?,data:r.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default() },score:r.get(7)?,reason:r.get(8)? };
                    Ok((capture, r.get::<_, u64>(9)?, r.get::<_, Option<String>>(10)?, r.get::<_, Option<String>>(11)?))
                },
            ).optional()?;
            let Some((mut capture, bytes, frame_path, sha256)) = row else {
                return Ok(None);
            };
            if let Some(name) = frame_path {
                let path = frame_file(&self.frames, &name)?;
                match self.read_frame(&path, bytes, sha256.as_deref())? {
                    Ok(data) => capture.frame.data = data,
                    Err(lost) => {
                        self.forget(capture.id, bytes, &path, lost)?;
                        continue;
                    }
                }
            }
            // Validate metadata before callers allocate a UI image for a file
            // which may have been modified outside CaptureFab.
            ensure!(
                capture.frame.data.len() <= MAX_FRAME_BYTES,
                "stored frame exceeds capture size limit"
            );
            descriptor(&capture.frame).context("corrupt capture frame in session")?;
            ensure!(capture.score.is_finite(), "invalid stored capture score");
            return Ok(Some(capture));
        }
    }

    /// A pending frame's bytes, or why they are lost: storage retention or a
    /// user deleted the file, or it no longer holds what was saved. A missing
    /// folder or volume is an error, so it never empties the review queue.
    fn read_frame(
        &self,
        path: &Path,
        bytes: u64,
        sha256: Option<&str>,
    ) -> Result<Result<Vec<u8>, &'static str>> {
        ensure!(
            bytes <= MAX_FRAME_BYTES as u64,
            "stored frame exceeds capture size limit"
        );
        self.check_folder()?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Err("was deleted")),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            }
        };
        let mut data = Vec::with_capacity(bytes as usize);
        file.take(bytes + 1).read_to_end(&mut data)?;
        if data.len() as u64 != bytes
            || sha256 != Some(format!("{:x}", Sha256::digest(&data)).as_str())
        {
            return Ok(Err("changed after saving and was left in place"));
        }
        Ok(Ok(data))
    }

    /// Drop a pending capture whose frame is lost, so it cannot block the
    /// review queue. Pending captures never train the model, so nothing
    /// learned is lost.
    fn forget(&mut self, id: u64, bytes: u64, path: &Path, lost: &str) -> Result<()> {
        let removed = self.db.execute(
            "DELETE FROM captures WHERE id = ? AND label = 'pending'",
            params![id],
        )?;
        self.wrote();
        ensure!(removed == 1, "capture is no longer pending review");
        let mut shared = lock(&self.shared);
        shared.tally.remove("pending", bytes);
        shared.note(format!(
            "Removed pending capture {id}: its frame file {} {lost}",
            path.display()
        ));
        Ok(())
    }
}

/// Schema 2 keeps frames as files beside the database. Rows written by
/// schema 1 keep their pixels in `data` and review exactly as before.
fn upgrade(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN;
         ALTER TABLE captures ADD COLUMN IF NOT EXISTS frame_path VARCHAR;
         ALTER TABLE captures ADD COLUMN IF NOT EXISTS frame_sha256 VARCHAR;
         UPDATE capture_session SET schema_version = 2 WHERE singleton = 1;
         COMMIT;
         CHECKPOINT;",
    )
    .context("upgrade capture session to frame files")
}

/// Create a session's frames folder. It must not exist yet, which reserves
/// the name, so a session never mixes another's frames into its own.
fn claim(frames: &Path) -> Result<()> {
    match std::fs::create_dir(frames) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => bail!(
            "capture session frames folder already exists: {}; choose another session name",
            frames.display()
        ),
        Err(error) => Err(error)
            .with_context(|| format!("create capture session frames folder {}", frames.display())),
    }
}

/// A stored frame name resolved inside the frames folder; a modified
/// database cannot point outside it.
fn frame_file(frames: &Path, name: &str) -> Result<PathBuf> {
    let mut components = Path::new(name).components();
    ensure!(
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
        "invalid frame file name in capture session"
    );
    Ok(frames.join(name))
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn counts(db: &Connection) -> Result<Tally> {
    Ok(db.query_row(
        "SELECT COUNT(*) FILTER (WHERE label = 'manual'), COUNT(*) FILTER (WHERE label = 'kept'),
                COUNT(*) FILTER (WHERE label = 'discarded'), COUNT(*) FILTER (WHERE label = 'pending'),
                COALESCE(SUM(data_bytes), 0)::UBIGINT FROM captures", [],
        |r| Ok(Tally { manual: r.get(0)?, kept: r.get(1)?, discarded: r.get(2)?, pending: r.get(3)?, bytes: r.get(4)? }))?)
}

fn examples(db: &Connection) -> Result<(Vec<Descriptor>, Vec<Descriptor>)> {
    let mut positive = load_descriptors(
        db,
        "SELECT descriptor FROM captures WHERE label IN ('manual', 'kept') ORDER BY id DESC LIMIT 256",
    )?;
    let mut negative = load_descriptors(
        db,
        "SELECT descriptor FROM captures WHERE label = 'discarded' ORDER BY id DESC LIMIT 256",
    )?;
    positive.reverse();
    negative.reverse();
    Ok((positive, negative))
}

fn validate_settings(sensitivity: f64, cooldown_ms: u64) -> Result<()> {
    ensure!(
        sensitivity.is_finite() && (0.0..=1.0).contains(&sensitivity),
        "auto capture sensitivity must be between 0 and 1"
    );
    ensure!(
        (250..=3_600_000).contains(&cooldown_ms),
        "auto capture cooldown must be between 250 and 3600000 ms"
    );
    Ok(())
}
fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn push_bounded<T>(items: &mut Vec<T>, feature: T, limit: usize) {
    if items.len() >= limit {
        items.remove(0);
    }
    items.push(feature);
}
fn encode(feature: &Descriptor) -> Vec<u8> {
    feature
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}
fn decode(bytes: &[u8]) -> Result<Descriptor> {
    ensure!(
        bytes.len() == FEATURES * 4,
        "corrupt session descriptor length"
    );
    let feature: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    ensure!(
        feature
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
        "corrupt session descriptor values"
    );
    Ok(feature)
}
fn load_descriptors(db: &Connection, query: &str) -> Result<Vec<Descriptor>> {
    let mut statement = db.prepare(query)?;
    let rows = statement.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
    rows.map(|row| decode(&row?)).collect()
}
fn nearest(examples: &[Descriptor], feature: &Descriptor) -> Option<f64> {
    // An exemplar nearest-neighbor model can learn immediately from a single
    // manual capture; unlike a fitted classifier it needs no balanced batch.
    examples
        .iter()
        .map(|example| distance(example, feature))
        .reduce(f64::min)
}
fn distance(a: &Descriptor, b: &Descriptor) -> f64 {
    (a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(x - y).powi(2))
        .sum::<f64>()
        / FEATURES as f64)
        .sqrt()
}
fn similarity(distance: f64) -> f64 {
    (-distance.powi(2) / (2.0 * 0.18_f64.powi(2))).exp()
}

/// Bounded, CPU-native appearance descriptor: 4x4 spatial RGB means, smooth
/// channel histograms, edge directions, contrast, brightness and aspect ratio.
/// Features retain absolute color/brightness since those can be the anomaly.
fn descriptor(frame: &Frame) -> Result<Descriptor> {
    ensure!(
        frame.data.len() <= MAX_FRAME_BYTES,
        "frame exceeds 256 MiB session capture limit"
    );
    let (w, h, rgba) = frame::preview_rgba(frame, 64, 64)?;
    ensure!(
        w > 0 && h > 0 && w <= 64 && h <= 64,
        "invalid descriptor preview dimensions"
    );
    let (w, h) = (w as usize, h as usize);
    let mut feature = vec![0.0_f32; FEATURES];
    let mut cells = [0_u32; 16];
    let mut luma = vec![0.0_f32; w * h];
    let (mut sum, mut squares) = (0.0, 0.0);
    for (i, pixel) in rgba.as_chunks::<4>().0.iter().enumerate() {
        let cell = ((i / w) * 4 / h) * 4 + (i % w) * 4 / w;
        cells[cell] += 1;
        for (channel, &byte) in pixel[..3].iter().enumerate() {
            let value = f32::from(byte) / 255.0;
            feature[cell * 3 + channel] += value;
            let position = value * 7.0;
            let lower = position.floor() as usize;
            let fraction = position - lower as f32;
            feature[48 + channel * 8 + lower] += 1.0 - fraction;
            if lower < 7 {
                feature[48 + channel * 8 + lower + 1] += fraction;
            }
        }
        let y = (0.2126 * f32::from(pixel[0])
            + 0.7152 * f32::from(pixel[1])
            + 0.0722 * f32::from(pixel[2]))
            / 255.0;
        luma[i] = y;
        sum += y;
        squares += y * y;
    }
    for (cell, &count) in cells.iter().enumerate() {
        if count > 0 {
            for channel in 0..3 {
                feature[cell * 3 + channel] /= count as f32;
            }
        }
    }
    let n = (w * h) as f32;
    for value in &mut feature[48..72] {
        // Histograms complement spatial means. Full-strength bins would make
        // a small global exposure change outweigh a strong spatial match.
        *value *= 0.4 / n;
    }
    let mut gradient_sum = 0.0;
    for y in 0..h.saturating_sub(1) {
        for x in 0..w.saturating_sub(1) {
            let i = y * w + x;
            let gx = luma[i + 1] - luma[i];
            let gy = luma[i + w] - luma[i];
            let magnitude = (gx * gx + gy * gy).sqrt();
            let direction = ((gy.atan2(gx) + std::f32::consts::PI) / (2.0 * std::f32::consts::PI)
                * 8.0) as usize
                % 8;
            feature[72 + direction] += magnitude;
            gradient_sum += magnitude;
        }
    }
    if gradient_sum > 0.0 {
        for value in &mut feature[72..80] {
            *value /= gradient_sum;
        }
    }
    let mean = sum / n;
    feature[80] = mean.clamp(0.0, 1.0);
    feature[81] = ((squares / n - mean * mean).max(0.0).sqrt() * 2.0).clamp(0.0, 1.0);
    feature[82] = (gradient_sum / n).clamp(0.0, 1.0);
    feature[83] = (frame.width as f64 / (f64::from(frame.width) + f64::from(frame.height))) as f32;
    Ok(feature)
}

#[cfg(test)]
impl CaptureSession {
    /// Run SQL on the writer's connection after every queued write lands.
    fn sql<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Connection) -> duckdb::Result<T> + Send + 'static,
    ) -> T {
        self.writer
            .call(move |store| Ok(query(&store.db)?))
            .unwrap()
    }
    /// Wait until every queued write has landed.
    fn flush(&self) {
        self.writer.call(|_| Ok(())).unwrap();
    }
    /// Recount after tests edit rows behind the session's back.
    fn recount(&self) {
        let tally = self.writer.call(|store| counts(&store.db)).unwrap();
        lock(&self.shared).tally = tally;
    }
    /// Hold a write transaction open on another connection, which makes
    /// DuckDB refuse to checkpoint until it is dropped.
    pub(crate) fn block_checkpoints(&self) -> Connection {
        self.sql(|db| {
            let other = db.try_clone()?;
            other.execute_batch(
                "BEGIN; UPDATE capture_session SET cooldown_ms = cooldown_ms WHERE singleton = 1;",
            )?;
            Ok(other)
        })
    }
}

#[cfg(test)]
mod tests;
