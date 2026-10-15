//! Bounded, process-local capture analytics. No camera frames or files are stored.
use crate::session::SessionSnapshot;
use anyhow::{Context, Result, anyhow};
use duckdb::{Config, Connection, params};
use plotters::{coord::Shift, prelude::*};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use unicode_segmentation::UnicodeSegmentation;

#[cfg(feature = "ai")]
mod measurements;
#[cfg(feature = "ai")]
pub use measurements::{MAX_MEASUREMENT_SAMPLES, MeasurementSample, render_measurement_svg};

pub const RETENTION_SECONDS: u64 = 24 * 60 * 60;
pub const SAMPLE_INTERVAL_SECONDS: u64 = 1;
pub const MAX_SAMPLES: usize = 250_000;
const MAX_CAMERAS: usize = 256;
const MAX_POINTS: u64 = 1_440;
const MAX_LABEL_BYTES: usize = 512;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AnalyticsTotals {
    pub frames: u64,
    pub dropped: u64,
    pub bytes: u64,
}
impl AnalyticsTotals {
    fn add(&mut self, other: &Self) {
        self.frames = self.frames.saturating_add(other.frames);
        self.dropped = self.dropped.saturating_add(other.dropped);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
    fn difference(&self, previous: &Self) -> Self {
        Self {
            frames: counter_delta(self.frames, previous.frames),
            dropped: counter_delta(self.dropped, previous.dropped),
            bytes: counter_delta(self.bytes, previous.bytes),
        }
    }
}
fn counter_delta(now: u64, previous: u64) -> u64 {
    if now >= previous { now - previous } else { now }
}

#[derive(Debug, Clone, Serialize)]
pub struct CameraAnalytics {
    pub id: String,
    pub name: String,
    pub frames: u64,
    pub dropped: u64,
    pub bytes: u64,
    pub fps: f64,
    pub bytes_per_second: f64,
    pub drop_percent: f64,
    pub streaming: bool,
    pub stale: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsPoint {
    pub timestamp_ms: u64,
    /// Counts observed during this bucket, rather than lifetime totals.
    pub frames: u64,
    pub dropped: u64,
    pub bytes: u64,
    pub fps: f64,
    pub bytes_per_second: f64,
    pub drop_percent: f64,
}
#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsSnapshot {
    pub retention_seconds: u64,
    pub sample_interval_seconds: u64,
    pub started_at_ms: u64,
    pub collected_at_ms: u64,
    pub samples: usize,
    /// True if the hard row limit shortened the available history.
    pub truncated: bool,
    pub totals: AnalyticsTotals,
    pub cameras: Vec<CameraAnalytics>,
    pub points: Vec<AnalyticsPoint>,
}
#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsHealth {
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub retention_seconds: u64,
    pub sample_interval_seconds: u64,
    pub max_samples: usize,
}

struct CameraState {
    report: CameraAnalytics,
    db_key: u64,
    worker_pid: u32,
    previous: AnalyticsTotals,
    last_seen_ms: u64,
    present: bool,
    initialized: bool,
    pending: AnalyticsTotals,
    rate_pending: AnalyticsTotals,
}
struct Store {
    db: Option<Connection>,
    error: Option<String>,
    started_at_ms: u64,
    started_at: Instant,
    collected_at_ms: u64,
    last_sample: Option<Instant>,
    last_sample_ms: Option<u64>,
    next_sequence: u64,
    next_camera_key: u64,
    truncated: bool,
    totals: AnalyticsTotals,
    pending: AnalyticsTotals,
    rate_pending: AnalyticsTotals,
    cameras: BTreeMap<String, CameraState>,
}

/// Observation never waits for chart rendering or a database query. The next
/// snapshot catches up counters after a busy analytics lock is released.
pub struct Analytics {
    store: Mutex<Store>,
}
impl Default for Analytics {
    fn default() -> Self {
        Self::new()
    }
}
impl Analytics {
    pub fn new() -> Self {
        let now = now_ms();
        let opened = open_database();
        let (db, error) = match opened {
            Ok(db) => (Some(db), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        Self {
            store: Mutex::new(Store {
                db,
                error,
                started_at_ms: now,
                started_at: Instant::now(),
                collected_at_ms: now,
                last_sample: None,
                last_sample_ms: None,
                next_sequence: 0,
                next_camera_key: 1,
                truncated: false,
                totals: AnalyticsTotals::default(),
                pending: AnalyticsTotals::default(),
                rate_pending: AnalyticsTotals::default(),
                cameras: BTreeMap::new(),
            }),
        }
    }

    pub fn observe(&self, snapshot: &SessionSnapshot) {
        let Ok(mut store) = self.store.try_lock() else {
            return;
        };
        Self::observe_locked(&mut store, snapshot);
    }

    /// Preserve the last counter deltas before a worker is removed. Used only
    /// during command completion, away from frame acquisition and UI polling.
    pub fn observe_final(&self, snapshot: &SessionSnapshot) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        Self::observe_locked(&mut store, snapshot);
    }

    fn observe_locked(store: &mut Store, snapshot: &SessionSnapshot) {
        let now = store.now();
        store.observe_counters(snapshot, now);
        let instant = Instant::now();
        if store
            .last_sample
            .is_some_and(|last| instant.duration_since(last).as_secs() < SAMPLE_INTERVAL_SECONDS)
        {
            return;
        }
        match store.sample(now) {
            Ok(()) => {
                store.last_sample = Some(instant);
                store.error = None;
            }
            Err(error) => {
                if store.db.is_some() || store.error.is_none() {
                    store.error = Some(format!("{error:#}"));
                }
            }
        }
    }

    pub fn snapshot(&self) -> Result<AnalyticsSnapshot> {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let now = store.now();
        let result = store.report(now);
        if let Err(error) = &result
            && (store.db.is_some() || store.error.is_none())
        {
            store.error = Some(format!("{error:#}"));
        }
        result
    }

    pub fn health(&self) -> AnalyticsHealth {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let now = store.now();
        if let Err(error) = store.prune(now)
            && (store.db.is_some() || store.error.is_none())
        {
            store.error = Some(format!("{error:#}"));
        }
        AnalyticsHealth {
            ready: store.db.is_some() && store.error.is_none(),
            error: store.error.clone(),
            retention_seconds: RETENTION_SECONDS,
            sample_interval_seconds: SAMPLE_INTERVAL_SECONDS,
            max_samples: MAX_SAMPLES,
        }
    }

    pub fn svg(&self) -> Result<String> {
        render_svg(&self.snapshot()?)
    }

    pub fn metrics(&self, session: &SessionSnapshot) -> Result<String> {
        let report = self.snapshot()?;
        let mut output = String::with_capacity(4096);
        metric(
            &mut output,
            "capturefab_analytics_ready",
            "Whether the in-memory analytics database is ready.",
            "gauge",
            if self.health().ready { "1" } else { "0" },
        );
        metric(
            &mut output,
            "capturefab_analytics_samples",
            "Retained analytics rows, limited by time and row count.",
            "gauge",
            &report.samples.to_string(),
        );
        metric(
            &mut output,
            "capturefab_analytics_retention_seconds",
            "Maximum retained analytics history in seconds.",
            "gauge",
            &RETENTION_SECONDS.to_string(),
        );
        metric(
            &mut output,
            "capturefab_analytics_history_truncated",
            "Whether the row cap shortened the retained history.",
            "gauge",
            if report.truncated { "1" } else { "0" },
        );
        metric(
            &mut output,
            "capturefab_session_frames_total",
            "Frames acquired in this process session across worker reconnects.",
            "counter",
            &report.totals.frames.to_string(),
        );
        metric(
            &mut output,
            "capturefab_session_dropped_frames_total",
            "Missed frames including transport losses and preview ring overwrites.",
            "counter",
            &report.totals.dropped.to_string(),
        );
        metric(
            &mut output,
            "capturefab_session_received_bytes_total",
            "Frame payload bytes acquired in this process session.",
            "counter",
            &report.totals.bytes.to_string(),
        );
        metric(
            &mut output,
            "capturefab_session_connected_cameras",
            "Currently connected camera workers.",
            "gauge",
            &session.cameras.len().to_string(),
        );
        metric(
            &mut output,
            "capturefab_session_streaming",
            "Whether any camera is streaming.",
            "gauge",
            if session.cameras.iter().any(|c| c.streaming) {
                "1"
            } else {
                "0"
            },
        );
        for (name, help, kind) in [
            (
                "capturefab_camera_frames_total",
                "Frames acquired by this camera in the process session.",
                "counter",
            ),
            (
                "capturefab_camera_dropped_frames_total",
                "Missed frames including transport losses and preview ring overwrites.",
                "counter",
            ),
            (
                "capturefab_camera_received_bytes_total",
                "Frame payload bytes acquired by this camera in the process session.",
                "counter",
            ),
            (
                "capturefab_camera_fps",
                "Latest observed acquisition rate in frames per second.",
                "gauge",
            ),
            (
                "capturefab_camera_received_bytes_per_second",
                "Latest observed payload throughput in bytes per second.",
                "gauge",
            ),
            (
                "capturefab_camera_streaming",
                "Whether this camera is streaming.",
                "gauge",
            ),
            (
                "capturefab_camera_stale",
                "Whether the last worker status was stale.",
                "gauge",
            ),
        ] {
            let _ = writeln!(output, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for camera in &report.cameras {
                let value = match name {
                    "capturefab_camera_frames_total" => camera.frames.to_string(),
                    "capturefab_camera_dropped_frames_total" => camera.dropped.to_string(),
                    "capturefab_camera_received_bytes_total" => camera.bytes.to_string(),
                    "capturefab_camera_fps" => finite(camera.fps).to_string(),
                    "capturefab_camera_received_bytes_per_second" => {
                        finite(camera.bytes_per_second).to_string()
                    }
                    "capturefab_camera_streaming" => u8::from(camera.streaming).to_string(),
                    _ => u8::from(camera.stale).to_string(),
                };
                let _ = writeln!(
                    output,
                    "{name}{{camera_id=\"{}\",name=\"{}\"}} {value}",
                    prometheus_label(&camera.id),
                    prometheus_label(&camera.name)
                );
            }
        }
        Ok(output)
    }
}

fn open_database() -> Result<Connection> {
    // No spill directory, no external files/extensions, and one engine thread.
    let config = Config::default().max_memory("128MB")?.threads(1)?;
    let db =
        Connection::open_in_memory_with_flags(config).context("open in-memory analytics DuckDB")?;
    // DuckDB's C configuration API rejects empty string values, so disable
    // spilling through SQL before creating or collecting any data.
    db.execute_batch(
        "SET temp_directory = ''; SET enable_external_access = false; CREATE TABLE samples (
        sequence UBIGINT NOT NULL, timestamp_ms UBIGINT NOT NULL,
        camera_id UBIGINT NOT NULL, frames UBIGINT NOT NULL,
        dropped UBIGINT NOT NULL, bytes UBIGINT NOT NULL,
        fps DOUBLE NOT NULL, bytes_per_second DOUBLE NOT NULL,
        rate_frames UBIGINT NOT NULL, rate_dropped UBIGINT NOT NULL
    )",
    )?;
    #[cfg(feature = "ai")]
    measurements::initialize_database(&db)?;
    Ok(db)
}

impl Store {
    fn now(&self) -> u64 {
        // Wall-clock changes must not extend the retention window or corrupt
        // rate intervals; the session's epoch is anchored to monotonic time.
        self.started_at_ms
            .saturating_add(self.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }
    fn observe_counters(&mut self, snapshot: &SessionSnapshot, now: u64) {
        self.collected_at_ms = now;
        let incoming: BTreeSet<_> = snapshot
            .cameras
            .iter()
            .map(|c| camera_key(&c.info.id))
            .collect();
        for state in self.cameras.values_mut() {
            state.present = false;
            state.report.streaming = false;
        }
        for camera in &snapshot.cameras {
            let id = camera_key(&camera.info.id);
            if !self.cameras.contains_key(&id) && self.cameras.len() >= MAX_CAMERAS {
                if let Some(oldest) = self
                    .cameras
                    .iter()
                    .filter(|(id, _)| !incoming.contains(*id))
                    .min_by_key(|(_, c)| c.last_seen_ms)
                    .map(|(id, _)| id.clone())
                {
                    self.cameras.remove(&oldest);
                } else {
                    continue;
                }
            }
            let counters = AnalyticsTotals {
                frames: camera.frames,
                dropped: camera.dropped,
                bytes: camera.received_bytes,
            };
            let db_key = self.next_camera_key;
            if !self.cameras.contains_key(&id) {
                self.next_camera_key = self.next_camera_key.saturating_add(1);
            }
            let state = self
                .cameras
                .entry(id.clone())
                .or_insert_with(|| CameraState {
                    report: CameraAnalytics {
                        id,
                        name: bounded_label(
                            format!("{} {}", camera.info.vendor, camera.info.model).trim(),
                        ),
                        frames: 0,
                        dropped: 0,
                        bytes: 0,
                        fps: 0.0,
                        bytes_per_second: 0.0,
                        drop_percent: 0.0,
                        streaming: false,
                        stale: false,
                    },
                    db_key,
                    worker_pid: camera.worker_pid,
                    previous: AnalyticsTotals::default(),
                    last_seen_ms: now,
                    present: false,
                    initialized: false,
                    pending: AnalyticsTotals::default(),
                    rate_pending: AnalyticsTotals::default(),
                });
            state.present = true;
            state.report.streaming = camera.streaming;
            state.report.stale = camera.stale;
            if camera.stale {
                state.report.fps = 0.0;
                state.report.bytes_per_second = 0.0;
                continue;
            }
            let same_generation = state.worker_pid == camera.worker_pid;
            let reset = !same_generation
                || counters.frames < state.previous.frames
                || counters.dropped < state.previous.dropped
                || counters.bytes < state.previous.bytes;
            let delta = if same_generation {
                counters.difference(&state.previous)
            } else {
                counters.clone()
            };
            if !reset && state.initialized {
                state.rate_pending.add(&delta);
                self.rate_pending.add(&delta);
            }
            state.pending.add(&delta);
            self.pending.add(&delta);
            self.totals.add(&delta);
            state.report.frames = state.report.frames.saturating_add(delta.frames);
            state.report.dropped = state.report.dropped.saturating_add(delta.dropped);
            state.report.bytes = state.report.bytes.saturating_add(delta.bytes);
            state.report.drop_percent = drop_percent(state.report.frames, state.report.dropped);
            state.report.fps = if camera.streaming {
                finite(camera.fps)
            } else {
                0.0
            };
            state.worker_pid = camera.worker_pid;
            state.previous = counters;
            state.initialized = true;
            state.last_seen_ms = now;
        }
        for state in self
            .cameras
            .values_mut()
            .filter(|c| !c.present || !c.report.streaming)
        {
            state.report.fps = 0.0;
            state.report.bytes_per_second = 0.0;
        }
    }

    fn sample(&mut self, now: u64) -> Result<()> {
        self.prune(now)?;
        let interval = self
            .last_sample_ms
            .map(|last| now.saturating_sub(last).max(1) as f64 / 1000.0)
            .unwrap_or(1.0);
        let db = self
            .db
            .as_mut()
            .context("analytics database is unavailable")?;
        let transaction = db.transaction()?;
        {
            let mut insert = transaction
                .prepare_cached("INSERT INTO samples VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
            insert.execute(params![
                self.next_sequence,
                now,
                0_u64,
                self.pending.frames,
                self.pending.dropped,
                self.pending.bytes,
                self.rate_pending.frames as f64 / interval,
                self.rate_pending.bytes as f64 / interval,
                self.rate_pending.frames,
                self.rate_pending.dropped
            ])?;
            self.next_sequence = self.next_sequence.saturating_add(1);
            for camera in self
                .cameras
                .values()
                .filter(|c| c.present || c.pending != AnalyticsTotals::default())
            {
                insert.execute(params![
                    self.next_sequence,
                    now,
                    camera.db_key,
                    camera.pending.frames,
                    camera.pending.dropped,
                    camera.pending.bytes,
                    camera.rate_pending.frames as f64 / interval,
                    camera.rate_pending.bytes as f64 / interval,
                    camera.rate_pending.frames,
                    camera.rate_pending.dropped
                ])?;
                self.next_sequence = self.next_sequence.saturating_add(1);
            }
        }
        transaction.commit()?;
        self.last_sample_ms = Some(now);
        self.pending = AnalyticsTotals::default();
        self.rate_pending = AnalyticsTotals::default();
        for camera in self.cameras.values_mut() {
            if camera.present && camera.report.streaming && !camera.report.stale {
                camera.report.bytes_per_second = camera.rate_pending.bytes as f64 / interval;
            }
            camera.pending = AnalyticsTotals::default();
            camera.rate_pending = AnalyticsTotals::default();
        }
        self.prune(now)
    }

    fn prune(&mut self, now: u64) -> Result<()> {
        let cutoff = now.saturating_sub(RETENTION_SECONDS * 1000);
        self.cameras
            .retain(|_, c| c.present || c.last_seen_ms >= cutoff);
        let db = self
            .db
            .as_ref()
            .context("analytics database is unavailable")?;
        db.execute(
            "DELETE FROM samples WHERE timestamp_ms < ?",
            params![cutoff],
        )?;
        let rows: u64 = db.query_row("SELECT count(*) FROM samples", [], |row| row.get(0))?;
        if rows > MAX_SAMPLES as u64 {
            let removed = db.execute("DELETE FROM samples WHERE sequence IN (SELECT sequence FROM samples ORDER BY sequence LIMIT ?)", params![rows - MAX_SAMPLES as u64])?;
            self.truncated |= removed > 0;
        }
        #[cfg(feature = "ai")]
        measurements::prune_measurements(db, now)?;
        Ok(())
    }

    fn report(&mut self, now: u64) -> Result<AnalyticsSnapshot> {
        self.prune(now)?;
        let db = self
            .db
            .as_ref()
            .context("analytics database is unavailable")?;
        let samples: u64 = db.query_row("SELECT count(*) FROM samples", [], |row| row.get(0))?;
        let bounds: (Option<u64>, Option<u64>) = db.query_row(
            "SELECT min(timestamp_ms), max(timestamp_ms) FROM samples WHERE camera_id = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let first = bounds.0.unwrap_or(now);
        let bucket_ms = match bounds {
            (Some(first), Some(last)) => (last.saturating_sub(first) / MAX_POINTS + 1).max(1000),
            _ => 1000,
        };
        let mut statement = db.prepare("SELECT CAST(max(timestamp_ms) AS UBIGINT), CAST(sum(frames) AS UBIGINT),
            CAST(sum(dropped) AS UBIGINT), CAST(sum(bytes) AS UBIGINT), avg(fps), avg(bytes_per_second),
            CAST(sum(rate_frames) AS UBIGINT), CAST(sum(rate_dropped) AS UBIGINT)
            FROM samples WHERE camera_id = 0 GROUP BY (timestamp_ms - ?) // ? ORDER BY 1")?;
        let points = statement
            .query_map(params![first, bucket_ms], |row| {
                let frames: u64 = row.get(6)?;
                let dropped: u64 = row.get(7)?;
                Ok(AnalyticsPoint {
                    timestamp_ms: row.get(0)?,
                    frames: row.get(1)?,
                    dropped: row.get(2)?,
                    bytes: row.get(3)?,
                    fps: finite(row.get(4)?),
                    bytes_per_second: finite(row.get(5)?),
                    drop_percent: drop_percent(frames, dropped),
                })
            })?
            .collect::<duckdb::Result<Vec<_>>>()?;
        Ok(AnalyticsSnapshot {
            retention_seconds: RETENTION_SECONDS,
            sample_interval_seconds: SAMPLE_INTERVAL_SECONDS,
            started_at_ms: self.started_at_ms,
            collected_at_ms: self.collected_at_ms,
            samples: samples as usize,
            truncated: self.truncated,
            totals: self.totals.clone(),
            cameras: self.cameras.values().map(|c| c.report.clone()).collect(),
            points,
        })
    }
}

fn finite(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    }
}
fn drop_percent(frames: u64, dropped: u64) -> f64 {
    let denominator = frames as f64 + dropped as f64;
    if denominator > 0.0 {
        dropped as f64 * 100.0 / denominator
    } else {
        0.0
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn bounded_label(value: &str) -> String {
    let mut end = value.len().min(MAX_LABEL_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
fn camera_key(value: &str) -> String {
    if value.len() <= MAX_LABEL_BYTES {
        return value.to_owned();
    }
    // Bound attacker-controlled labels without merging identities with the
    // same long prefix (which would repeatedly count reconnect baselines).
    let digest = Sha256::digest(value.as_bytes());
    let suffix = format!("{digest:x}");
    let mut end = MAX_LABEL_BYTES - suffix.len() - 1;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}#{suffix}", &value[..end])
}
fn prometheus_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(['\n', '\r'], "\\n")
        .replace('"', "\\\"")
}
fn metric(output: &mut String, name: &str, help: &str, kind: &str, value: &str) {
    let _ = writeln!(
        output,
        "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
    );
}

const BACKGROUND: RGBColor = RGBColor(28, 32, 32);
const TEXT: RGBColor = RGBColor(228, 236, 231);
const MUTED: RGBColor = RGBColor(156, 169, 163);
const ACCENT: RGBColor = RGBColor(182, 239, 112);
const BLUE: RGBColor = RGBColor(111, 199, 239);
const CORAL: RGBColor = RGBColor(239, 145, 120);

/// Chart colors are independent of any desktop toolkit or theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvgColors {
    pub background: [u8; 3],
    pub text: [u8; 3],
    pub secondary: [u8; 3],
    pub axis: [u8; 3],
    pub frame_rate: [u8; 3],
    pub throughput: [u8; 3],
    pub missed_frames: [u8; 3],
    pub increased_contrast: bool,
}
impl Default for SvgColors {
    fn default() -> Self {
        Self {
            background: [BACKGROUND.0, BACKGROUND.1, BACKGROUND.2],
            text: [TEXT.0, TEXT.1, TEXT.2],
            secondary: [MUTED.0, MUTED.1, MUTED.2],
            axis: [MUTED.0, MUTED.1, MUTED.2],
            frame_rate: [ACCENT.0, ACCENT.1, ACCENT.2],
            throughput: [BLUE.0, BLUE.1, BLUE.2],
            missed_frames: [CORAL.0, CORAL.1, CORAL.2],
            increased_contrast: false,
        }
    }
}

/// The default remains the HTTP/CLI overview. Interface charts use their
/// actual logical width, so the caller can scale text and controls together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvgOptions {
    width: u32,
    interface: bool,
    colors: SvgColors,
}
impl Default for SvgOptions {
    fn default() -> Self {
        Self {
            width: 1200,
            interface: false,
            colors: SvgColors::default(),
        }
    }
}
impl SvgOptions {
    pub fn interface(logical_width: u32, colors: SvgColors) -> Self {
        Self {
            width: logical_width.clamp(180, 2048),
            interface: true,
            colors,
        }
    }

    pub fn size(self) -> (u32, u32) {
        if self.interface {
            (self.width, if self.columns() == 1 { 1040 } else { 520 })
        } else {
            (1200, 700)
        }
    }

    fn columns(self) -> usize {
        if self.interface && self.width < 840 {
            1
        } else {
            2
        }
    }

    // plotters-svg 0.3.7 divides the requested font size by 1.24. Keep
    // emitted interface text at 12/16 logical points, not 9.68/12.90.
    fn label_size(self) -> f64 {
        if self.interface { 12.0 * 1.24 } else { 12.0 }
    }
    fn caption_size(self) -> f64 {
        if self.interface { 16.0 * 1.24 } else { 19.0 }
    }

    fn font_family(self) -> &'static str {
        if self.interface {
            // fontdb 0.23 uses fontconfig's first generic preference even
            // when that face is absent. Keep the user's generic choice first,
            // then offer installed desktop families to SVG text resolution.
            "sans-serif, DejaVu Sans, Liberation Sans, Noto Sans, Ubuntu, Cantarell, Fira Sans, Roboto, Arial"
        } else {
            "sans-serif"
        }
    }
}

fn rgb([r, g, b]: [u8; 3]) -> RGBColor {
    RGBColor(r, g, b)
}

pub fn render_svg(report: &AnalyticsSnapshot) -> Result<String> {
    render_svg_with_options(report, SvgOptions::default())
}

pub fn render_svg_with_options(report: &AnalyticsSnapshot, options: SvgOptions) -> Result<String> {
    let mut svg = String::with_capacity(50_000);
    {
        let root = SVGBackend::with_string(&mut svg, options.size()).into_drawing_area();
        root.fill(&rgb(options.colors.background))
            .map_err(plot_error)?;
        if options.interface {
            let columns = options.columns();
            let panels = root.split_evenly((4 / columns, columns));
            draw_panels(&panels, report, options)?;
        } else {
            root.draw(&Text::new(
                "Your capture, at a glance",
                (32, 39),
                ("sans-serif", 26).into_font().color(&TEXT),
            ))
            .map_err(plot_error)?;
            let summary = format!(
                "{} frames   ·   {} missed   ·   {} captured   ·   session totals",
                compact(report.totals.frames),
                compact(report.totals.dropped),
                human_bytes(report.totals.bytes)
            );
            root.draw(&Text::new(
                summary,
                (32, 69),
                ("sans-serif", 16).into_font().color(&MUTED),
            ))
            .map_err(plot_error)?;
            if report.totals.frames == 0 && report.totals.bytes == 0 {
                root.draw(&Circle::new((600, 275), 58, ACCENT.mix(0.08).filled()))
                    .map_err(plot_error)?;
                root.draw(&Circle::new((600, 275), 24, ACCENT.mix(0.25).filled()))
                    .map_err(plot_error)?;
                root.draw(&Text::new(
                    "Ready for your first frame",
                    (417, 390),
                    ("sans-serif", 28).into_font().color(&TEXT),
                ))
                .map_err(plot_error)?;
                root.draw(&Text::new(
                    "Start a camera to see frame rate, throughput and missed-frame trends.",
                    (306, 428),
                    ("sans-serif", 17).into_font().color(&MUTED),
                ))
                .map_err(plot_error)?;
                root.draw(&Text::new(
                    "Analytics lives in memory for this capture session.",
                    (380, 459),
                    ("sans-serif", 16).into_font().color(&MUTED),
                ))
                .map_err(plot_error)?;
            } else {
                let panels = root.margin(99, 33, 22, 22).split_evenly((2, 2));
                draw_panels(&panels, report, options)?;
            }
            root.draw(&Text::new(if report.truncated { "History shortened by the memory row limit. Missed frames include transport losses and preview overwrites." } else { "Missed frames include transport losses and preview overwrites. Rates are sampled every second." }, (32, 679), ("sans-serif", 12).into_font().color(&MUTED))).map_err(plot_error)?;
        }
        root.present().map_err(plot_error)?;
    }
    // Plotters produces the marks; metadata makes the returned vector graphic
    // scale with its container and readable to assistive technology.
    svg = svg.replacen(
        "<svg ",
        "<svg role=\"img\" aria-labelledby=\"analytics-title analytics-description\" ",
        1,
    );
    if let Some(position) = svg.find('>') {
        svg.insert_str(position + 1, "<title id=\"analytics-title\">Current capture session analytics</title><desc id=\"analytics-description\">Frame rate, payload throughput, missed-frame trends and camera data volume. History is retained in memory for at most twenty-four hours.</desc>");
    }
    Ok(svg)
}

fn draw_panels(
    panels: &[DrawingArea<SVGBackend<'_>, Shift>],
    report: &AnalyticsSnapshot,
    options: SvgOptions,
) -> Result<()> {
    draw_time_chart(
        &panels[0],
        report,
        "Frame rate",
        "frames / s",
        rgb(options.colors.frame_rate),
        |p| p.fps,
        options,
    )?;
    draw_time_chart(
        &panels[1],
        report,
        "Throughput",
        "MiB / s",
        rgb(options.colors.throughput),
        |p| p.bytes_per_second / 1_048_576.0,
        options,
    )?;
    draw_time_chart(
        &panels[2],
        report,
        "Missed frame rate",
        if options.interface {
            "% missed"
        } else {
            "% of observed frames"
        },
        rgb(options.colors.missed_frames),
        |p| p.drop_percent,
        options,
    )?;
    draw_camera_chart(&panels[3], report, options)
}

/// Numeric GUI plots use the same measurements and drawing as exports. User
/// camera identities are rendered by native GUI text, outside the SVG shaper.
#[cfg(feature = "gui")]
pub(crate) fn render_interface_time_plots(
    report: &AnalyticsSnapshot,
    width: u32,
    colors: SvgColors,
) -> Result<[String; 3]> {
    let options = SvgOptions::interface(width, colors);
    let mut plots = [String::new(), String::new(), String::new()];
    for (index, output) in plots.iter_mut().enumerate() {
        let root = SVGBackend::with_string(output, (options.width, 260)).into_drawing_area();
        root.fill(&rgb(colors.background)).map_err(plot_error)?;
        let (caption, unit, color) = match index {
            0 => ("Frame rate", "frames / s", colors.frame_rate),
            1 => ("Throughput", "MiB / s", colors.throughput),
            _ => ("Missed frame rate", "% missed", colors.missed_frames),
        };
        draw_time_chart(
            &root,
            report,
            caption,
            unit,
            rgb(color),
            |point| match index {
                0 => point.fps,
                1 => point.bytes_per_second / 1_048_576.0,
                _ => point.drop_percent,
            },
            options,
        )?;
        root.present().map_err(plot_error)?;
    }
    Ok(plots)
}

fn plot_error<E: std::fmt::Debug>(error: E) -> anyhow::Error {
    anyhow!("render analytics SVG: {error:?}")
}

fn draw_time_chart<F: Fn(&AnalyticsPoint) -> f64>(
    area: &DrawingArea<SVGBackend<'_>, Shift>,
    report: &AnalyticsSnapshot,
    caption: &str,
    unit: &str,
    color: RGBColor,
    value: F,
    options: SvgOptions,
) -> Result<()> {
    let first = report
        .points
        .first()
        .map_or(report.started_at_ms, |p| p.timestamp_ms);
    let last = report
        .points
        .last()
        .map_or(first + 1000, |p| p.timestamp_ms)
        .max(first + 1000);
    let span = (last - first) as f64 / 1000.0;
    let maximum = report
        .points
        .iter()
        .map(&value)
        .fold(0.0_f64, f64::max)
        .max(1.0)
        * 1.18;
    let mut chart = ChartBuilder::on(area)
        .margin(if options.interface { 12 } else { 18 })
        .caption(
            caption,
            (options.font_family(), options.caption_size())
                .into_font()
                .color(&rgb(options.colors.text)),
        )
        .x_label_area_size(if options.interface { 42 } else { 46 })
        .y_label_area_size(if options.interface { 60 } else { 64 })
        .build_cartesian_2d(0.0..span, 0.0..maximum)
        .map_err(plot_error)?;
    let mut mesh = chart.configure_mesh();
    mesh.disable_x_mesh()
        .light_line_style(
            rgb(options.colors.axis).mix(if options.colors.increased_contrast {
                0.3
            } else {
                0.12
            }),
        )
        .bold_line_style(
            rgb(options.colors.axis).mix(if options.colors.increased_contrast {
                0.5
            } else {
                0.2
            }),
        )
        .axis_style(rgb(options.colors.axis).mix(if options.interface { 1.0 } else { 0.4 }))
        .label_style(
            (options.font_family(), options.label_size())
                .into_font()
                .color(&rgb(options.colors.secondary)),
        )
        .x_labels(if options.interface && area.dim_in_pixel().0 < 272 {
            2
        } else if options.interface && area.dim_in_pixel().0 < 430 {
            3
        } else {
            5
        })
        .y_labels(4)
        .x_label_formatter(&|seconds| duration_label(*seconds))
        .x_desc(if options.interface {
            "Elapsed time"
        } else {
            "Time in retained window"
        })
        .y_desc(unit)
        .axis_desc_style(
            (options.font_family(), options.label_size())
                .into_font()
                .color(&rgb(options.colors.secondary)),
        );
    if options.interface {
        mesh.y_label_formatter(&interface_axis_label);
    }
    mesh.draw().map_err(plot_error)?;
    let points: Vec<_> = report
        .points
        .iter()
        .map(|p| {
            (
                (p.timestamp_ms.saturating_sub(first)) as f64 / 1000.0,
                value(p),
            )
        })
        .collect();
    chart
        .draw_series(AreaSeries::new(points.clone(), 0.0, color.mix(0.12)))
        .map_err(plot_error)?;
    chart
        .draw_series(LineSeries::new(
            points.clone(),
            color.stroke_width(if options.colors.increased_contrast {
                3
            } else {
                2
            }),
        ))
        .map_err(plot_error)?;
    if let Some(last) = points.last() {
        chart
            .draw_series(std::iter::once(Circle::new(*last, 3, color.filled())))
            .map_err(plot_error)?;
    }
    Ok(())
}

fn draw_interface_cameras(
    area: &DrawingArea<SVGBackend<'_>, Shift>,
    cameras: &[&CameraAnalytics],
    options: SvgOptions,
) -> Result<()> {
    use plotters::style::text_anchor::{HPos, Pos, VPos};
    let (width, height) = area.dim_in_pixel();
    let inset = 14;
    let available = width.saturating_sub(inset * 2);
    area.draw(&Text::new(
        "Camera data volume",
        (width as i32 / 2, 17),
        (options.font_family(), options.caption_size())
            .into_font()
            .color(&rgb(options.colors.text))
            .pos(Pos::new(HPos::Center, VPos::Top)),
    ))
    .map_err(plot_error)?;
    let maximum = cameras
        .iter()
        .map(|camera| camera.bytes)
        .max()
        .unwrap_or(0)
        .max(1);
    for (index, camera) in cameras.iter().enumerate() {
        let name = if camera.name.is_empty() {
            &camera.id
        } else {
            &camera.name
        };
        // SVG's font estimator counts UTF-8 bytes rather than shaped glyphs.
        // Use a conservative em budget and elide names; the caller retains
        // the full camera identity in its accompanying textual report.
        let name = interface_camera_name(name, available / 12);
        let y = 40 + index as i32 * 42;
        area.draw(&Text::new(
            name,
            (inset as i32, y),
            (options.font_family(), options.label_size())
                .into_font()
                .color(&rgb(options.colors.secondary)),
        ))
        .map_err(plot_error)?;
        // Values get a separate line, including u64::MAX-sized byte totals.
        area.draw(&Text::new(
            human_bytes(camera.bytes),
            (width as i32 - inset as i32, y + 15),
            (options.font_family(), options.label_size())
                .into_font()
                .color(&rgb(options.colors.text))
                .pos(Pos::new(HPos::Right, VPos::Top)),
        ))
        .map_err(plot_error)?;
        let top = y + 30;
        area.draw(&Rectangle::new(
            [(inset as i32, top), ((width - inset) as i32, top + 8)],
            rgb(options.colors.axis).mix(0.12).filled(),
        ))
        .map_err(plot_error)?;
        let extent = (camera.bytes as f64 / maximum as f64 * available as f64).round() as i32;
        let color = if camera.streaming {
            options.colors.throughput
        } else {
            options.colors.secondary
        };
        area.draw(&Rectangle::new(
            [(inset as i32, top), (inset as i32 + extent, top + 8)],
            rgb(color).filled(),
        ))
        .map_err(plot_error)?;
    }
    if cameras.is_empty() {
        area.draw(&Text::new(
            "No retained cameras",
            (width as i32 / 2, height as i32 / 2),
            (options.font_family(), options.label_size())
                .into_font()
                .color(&rgb(options.colors.secondary))
                .pos(Pos::new(HPos::Center, VPos::Center)),
        ))
        .map_err(plot_error)?;
    }
    Ok(())
}

fn interface_camera_name(name: &str, budget: u32) -> String {
    let budget = budget.max(1) as usize;
    if name.chars().count() <= budget {
        return name.to_owned();
    }
    let mut shortened = String::new();
    let mut used = 0;
    for grapheme in name.graphemes(true) {
        let next = used + grapheme.chars().count();
        if next > budget.saturating_sub(1) {
            break;
        }
        shortened.push_str(grapheme);
        used = next;
    }
    shortened.push('…');
    shortened
}

fn interface_axis_label(value: &f64) -> String {
    if *value >= 1e12 {
        format!("{value:.0e}")
    } else if *value >= 1e9 {
        format!("{:.0}G", value / 1e9)
    } else if *value >= 1e6 {
        format!("{:.0}M", value / 1e6)
    } else if *value >= 1e3 {
        format!("{:.0}k", value / 1e3)
    } else if *value >= 10.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn draw_camera_chart(
    area: &DrawingArea<SVGBackend<'_>, Shift>,
    report: &AnalyticsSnapshot,
    options: SvgOptions,
) -> Result<()> {
    let cameras = chart_cameras(report);
    if options.interface {
        return draw_interface_cameras(area, &cameras, options);
    }
    let maximum = cameras
        .iter()
        .map(|c| c.bytes as f64 / 1_048_576.0)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let (width, height) = area.dim_in_pixel();
    area.draw(&Text::new(
        "Camera data volume",
        ((width as i32 - 170) / 2, 32),
        ("sans-serif", 19).into_font().color(&TEXT),
    ))
    .map_err(plot_error)?;
    let bar_width = (width as i32 - 275).max(20);
    for (index, camera) in cameras.iter().enumerate() {
        let name: String = camera.name.chars().take(17).collect();
        let id: String = camera
            .id
            .chars()
            .rev()
            .take(6)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let label = if name.is_empty() {
            id
        } else {
            format!("{name} · {id}")
        };
        let y = 68 + index as i32 * 34;
        area.draw(&Text::new(
            label,
            (18, y + 16),
            ("sans-serif", 12).into_font().color(&MUTED),
        ))
        .map_err(plot_error)?;
        area.draw(&Rectangle::new(
            [(201, y), (201 + bar_width, y + 22)],
            MUTED.mix(0.08).filled(),
        ))
        .map_err(plot_error)?;
        let extent = (camera.bytes as f64 / 1_048_576.0 / maximum * bar_width as f64) as i32;
        area.draw(&Rectangle::new(
            [(201, y), (201 + extent, y + 22)],
            BLUE.mix(if camera.streaming { 0.9 } else { 0.45 }).filled(),
        ))
        .map_err(plot_error)?;
        area.draw(&Text::new(
            human_bytes(camera.bytes),
            (210 + bar_width, y + 16),
            ("sans-serif", 11).into_font().color(&TEXT),
        ))
        .map_err(plot_error)?;
    }
    area.draw(&Text::new(
        "Session payload by camera · brightest bars are streaming",
        (18, height as i32 - 14),
        ("sans-serif", 12).into_font().color(&MUTED),
    ))
    .map_err(plot_error)?;
    if cameras.is_empty() {
        area.draw(&Text::new(
            "No retained cameras",
            (175, 133),
            ("sans-serif", 15).into_font().color(&MUTED),
        ))
        .map_err(plot_error)?;
    }
    Ok(())
}
fn duration_label(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}k", value as f64 / 1000.0)
    } else {
        value.to_string()
    }
}
pub(crate) fn chart_cameras(report: &AnalyticsSnapshot) -> Vec<&CameraAnalytics> {
    let mut cameras: Vec<_> = report.cameras.iter().collect();
    cameras.sort_by_key(|camera| std::cmp::Reverse(camera.bytes));
    cameras.truncate(5);
    cameras
}

pub(crate) fn human_bytes(value: u64) -> String {
    if value >= 1_073_741_824 {
        format!("{:.2} GiB", value as f64 / 1_073_741_824.0)
    } else if value >= 1_048_576 {
        format!("{:.1} MiB", value as f64 / 1_048_576.0)
    } else {
        format!("{:.1} KiB", value as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod render_tests;
