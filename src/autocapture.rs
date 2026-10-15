//! Session-local, human-supervised automatic capture. The database is the session:
//! raw sensor frames, decisions, settings and the bounded background model all
//! live in one DuckDB file. No network service or downloaded weights are needed.
use crate::{frame, types::Frame};
use anyhow::{Context, Result, ensure};
use duckdb::{Config, Connection, OptionalExt, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: u32 = 1;
const DESCRIPTOR_VERSION: u32 = 1;
const FEATURES: usize = 84;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_PENDING: u64 = 200;
const MAX_STORED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_BACKGROUND: usize = 64;
const MAX_RECENT: usize = 32;
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
}

#[derive(Debug, Clone)]
pub struct ReviewCapture {
    pub id: u64,
    pub frame: Frame,
    pub score: f64,
    pub reason: String,
}

pub struct CaptureSession {
    db: Connection,
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
        if create {
            ensure!(
                !path.exists(),
                "capture session already exists; resume it instead"
            );
        } else {
            ensure!(
                path.is_file(),
                "capture session does not exist: {}",
                path.display()
            );
        }
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let config = Config::default()
            .max_memory("256MB")?
            .threads(1)?
            .enable_autoload_extension(false)?;
        let db =
            Connection::open_with_flags(&path, config).context("open capture session DuckDB")?;
        // Frames are BLOBs; never ask DuckDB to load extensions, read external
        // data, or spill a second session data file beside the database.
        db.execute_batch("SET temp_directory = ''; SET enable_external_access = false;")?;
        if create {
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
            version == SCHEMA_VERSION,
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
        let mut positive = load_descriptors(
            &db,
            "SELECT descriptor FROM captures WHERE label IN ('manual', 'kept') ORDER BY id DESC LIMIT 256",
        )?;
        let mut negative = load_descriptors(
            &db,
            "SELECT descriptor FROM captures WHERE label = 'discarded' ORDER BY id DESC LIMIT 256",
        )?;
        let background = load_descriptors(
            &db,
            "SELECT descriptor FROM background ORDER BY id LIMIT 64",
        )?;
        positive.reverse();
        negative.reverse();
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
        let now = Instant::now();
        let started_ms = epoch_ms();
        let last_capture = last_ms
            .and_then(|ms| now.checked_sub(Duration::from_millis(epoch_ms().saturating_sub(ms))));
        Ok(Self {
            db,
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
        })
    }

    pub fn configure(&mut self, enabled: bool, sensitivity: f64, cooldown_ms: u64) -> Result<()> {
        validate_settings(sensitivity, cooldown_ms)?;
        self.db.execute("UPDATE capture_session SET enabled = ?, sensitivity = ?, cooldown_ms = ? WHERE singleton = 1", params![enabled, sensitivity, cooldown_ms])?;
        self.enabled = enabled;
        self.sensitivity = sensitivity;
        self.cooldown_ms = cooldown_ms;
        self.checkpoint()?;
        Ok(())
    }

    pub fn status(&self) -> Result<SessionStatus> {
        let (manual_count, kept_count, discarded_count, pending_count, bytes) = self.counts()?;
        let state = if !self.enabled {
            "paused"
        } else if pending_count >= MAX_PENDING {
            "review_queue_full"
        } else if self.blocked_storage || bytes >= MAX_STORED_BYTES {
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
            manual_count,
            kept_count,
            discarded_count,
            pending_count,
            blocked_storage: self.blocked_storage || bytes >= MAX_STORED_BYTES,
            state: state.into(),
            model: MODEL.into(),
        })
    }

    /// Evaluate at most four small previews per second. Automatic suggestions do
    /// not become training examples until the user reviews them.
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
        let (_, _, _, pending, bytes) = self.counts()?;
        self.blocked_storage = bytes.saturating_add(frame.data.len() as u64) > MAX_STORED_BYTES;
        if pending >= MAX_PENDING || self.blocked_storage {
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
            let id = self.insert(frame, &feature, "automatic", "pending", score, &reason)?;
            self.last_capture = Some(now);
            push_bounded(&mut self.recent, (feature, now_ms), MAX_RECENT);
            self.checkpoint()?;
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
            if distinct {
                self.db.execute(
                    "INSERT INTO background VALUES (?, ?)",
                    params![self.background.len() as u32, encode(&feature)],
                )?;
                self.background.push(feature);
            }
        }
        Ok(None)
    }

    /// Store the exact user-selected sensor frame and immediately teach the
    /// session that this is an interesting example, even while auto mode is off.
    pub fn manual(&mut self, frame: &Frame) -> Result<u64> {
        let feature = descriptor(frame)?;
        let id = self.insert(
            frame,
            &feature,
            "manual",
            "manual",
            1.0,
            "User-initiated capture: positive training example",
        )?;
        self.reload_examples()?;
        push_bounded(&mut self.recent, (feature, epoch_ms()), MAX_RECENT);
        self.last_capture = Some(Instant::now());
        self.checkpoint()?;
        Ok(id)
    }

    /// A pending-only transaction prevents a double key press or stale review
    /// card from relabeling a capture. Discarding releases raw pixels while
    /// keeping the descriptor and negative decision for future learning.
    pub fn review(&mut self, id: u64, keep: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        let bytes: Option<Vec<u8>> = tx
            .query_row(
                "SELECT descriptor FROM captures WHERE id = ? AND label = 'pending'",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        decode(&bytes.context("capture is no longer pending review")?)?;
        let changed = tx.execute("UPDATE captures SET label = ?, reviewed_ms = ?, data = CASE WHEN ? THEN data ELSE NULL END, data_bytes = CASE WHEN ? THEN data_bytes ELSE 0 END WHERE id = ? AND label = 'pending'",
            params![if keep { "kept" } else { "discarded" }, epoch_ms(), keep, keep, id])?;
        ensure!(changed == 1, "capture is no longer pending review");
        tx.commit()?;
        self.reload_examples()?;
        if !keep {
            self.blocked_storage = false;
        }
        self.checkpoint()?;
        Ok(())
    }

    pub fn next_review(&self) -> Result<Option<ReviewCapture>> {
        self.load_review(None)
    }

    pub fn review_capture(&self, id: u64) -> Result<Option<ReviewCapture>> {
        self.load_review(Some(id))
    }

    /// Flush DuckDB's recovery log into the session file before transferring it
    /// or explicitly closing it. Callers can retain the live session on error.
    pub fn checkpoint(&self) -> Result<()> {
        self.db
            .execute_batch("CHECKPOINT")
            .context("checkpoint capture session into its portable DuckDB file")
    }

    fn reload_examples(&mut self) -> Result<()> {
        let mut positive = load_descriptors(
            &self.db,
            "SELECT descriptor FROM captures WHERE label IN ('manual', 'kept') ORDER BY id DESC LIMIT 256",
        )?;
        let mut negative = load_descriptors(
            &self.db,
            "SELECT descriptor FROM captures WHERE label = 'discarded' ORDER BY id DESC LIMIT 256",
        )?;
        positive.reverse();
        negative.reverse();
        self.positive = positive;
        self.negative = negative;
        Ok(())
    }

    fn load_review(&self, id: Option<u64>) -> Result<Option<ReviewCapture>> {
        let result = self.db.query_row(
            "SELECT id, frame_id, width, height, pixel_format, timestamp_ns, data, score, reason FROM captures
             WHERE label = 'pending' AND (?::UBIGINT IS NULL OR id = ?::UBIGINT) ORDER BY id LIMIT 1",
            params![id, id], |r| Ok(ReviewCapture { id: r.get(0)?, frame: Frame { id:r.get(1)?,width:r.get(2)?,height:r.get(3)?,pixel_format:r.get(4)?,timestamp_ns:r.get(5)?,data:r.get(6)? },score:r.get(7)?,reason:r.get(8)? })).optional()?;
        if let Some(capture) = &result {
            // Validate metadata before callers allocate a UI image for a file
            // which may have been modified outside CaptureFab.
            ensure!(
                capture.frame.data.len() <= MAX_FRAME_BYTES,
                "stored frame exceeds capture size limit"
            );
            descriptor(&capture.frame).context("corrupt capture frame in session")?;
            ensure!(capture.score.is_finite(), "invalid stored capture score");
        }
        Ok(result)
    }

    fn counts(&self) -> Result<(u64, u64, u64, u64, u64)> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FILTER (WHERE label = 'manual'), COUNT(*) FILTER (WHERE label = 'kept'),
                    COUNT(*) FILTER (WHERE label = 'discarded'), COUNT(*) FILTER (WHERE label = 'pending'),
                    COALESCE(SUM(data_bytes), 0)::UBIGINT FROM captures", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?)
    }

    fn insert(
        &mut self,
        frame: &Frame,
        feature: &Descriptor,
        source: &str,
        label: &str,
        score: f64,
        reason: &str,
    ) -> Result<u64> {
        ensure!(
            frame.data.len() <= MAX_FRAME_BYTES,
            "frame exceeds 256 MiB session capture limit"
        );
        let (_, _, _, _, bytes) = self.counts()?;
        self.blocked_storage = bytes.saturating_add(frame.data.len() as u64) > MAX_STORED_BYTES;
        ensure!(
            !self.blocked_storage,
            "capture session reached its 2 GiB pixel storage limit; start another session or discard pending captures"
        );
        let next_id = self
            .next_id
            .checked_add(1)
            .context("capture session identifier overflow")?;
        self.db.execute(
            "INSERT INTO captures VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
            params![
                self.next_id,
                epoch_ms(),
                source,
                label,
                frame.id,
                frame.width,
                frame.height,
                frame.pixel_format,
                frame.timestamp_ns,
                &frame.data,
                frame.data.len() as u64,
                encode(feature),
                score,
                reason
            ],
        )?;
        let id = self.next_id;
        self.next_id = next_id;
        Ok(id)
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

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // A cleanly closed session is one portable file, including all labels
        // and raw pixels. DuckDB's WAL protects interrupted writes while open.
        let _ = self.checkpoint();
    }
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
mod tests;
