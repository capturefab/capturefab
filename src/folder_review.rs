//! Folder review: rank a folder's images and sampled video frames, and queue
//! the interesting ones for a person to keep or discard, as auto capture does
//! for a live camera. A session is one DuckDB file holding the folder's
//! inventory, an embedding per item, every decision and the nearest-distance
//! bookkeeping ranking needs. The folder itself is never written: frames are
//! decoded again from their files to be shown, and `export` copies kept items
//! elsewhere through the storage ledger.
//!
//! Items are compared as embeddings from the bundled DINOv2 model (on the GPU
//! when one is usable) or as the native color/edge descriptor, exactly as
//! auto capture compares frames, so a capture session's examples can teach a
//! review that uses the same encoder.
mod rank;
mod scan;

pub use rank::Rank;

use crate::{
    autocapture::{self, Backend, Descriptor, Encoder, EncoderChoice},
    destination::Resolved,
    storage,
    types::{Categorized, ErrorKind, Frame},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use duckdb::{AccessMode, Config, Connection, OptionalExt, params};
use rank::{Candidate, Ranker};
use scan::Kind;
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: u32 = 1;
/// Items a session holds, so ranking stays in memory and a decision stays
/// one quick pass over every pending item.
pub const MAX_ITEMS: u64 = 20_000;
pub const MAX_VIDEO_SAMPLES: usize = 600;
pub const DEFAULT_VIDEO_INTERVAL_MS: u64 = 2_000;
pub const VIDEO_INTERVAL_MS: std::ops::RangeInclusive<u64> = 100..=3_600_000;
/// Examples taken from a capture session, newest first.
const MAX_EXAMPLES: usize = 1_024;
/// Recent scanned items a new one is compared with to find repeats, such as
/// consecutive samples of a still scene or a burst of near-identical photos.
const RECENT: usize = 64;
/// Embeddings for the item limit fit with room to spare.
const MEMORY_LIMIT: &str = "128MB";

/// How a new session ranks its items.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum RankChoice {
    /// `similar` when examples are imported, otherwise `novelty`
    #[default]
    Auto,
    /// Items least like everything reviewed first; discards suppress look-alikes
    Novelty,
    /// Items most like kept examples first, least like discarded ones
    Similar,
}

/// Settings of a new session.
#[derive(Debug, Clone)]
pub struct Options {
    pub folder: PathBuf,
    pub encoder: Option<EncoderChoice>,
    pub rank: RankChoice,
    /// A capture session whose kept and discarded captures teach the review.
    pub examples: Option<PathBuf>,
    pub sensitivity: f64,
    pub video_interval_ms: u64,
}

impl Options {
    pub fn new(folder: impl Into<PathBuf>) -> Self {
        Self {
            folder: folder.into(),
            encoder: None,
            rank: RankChoice::Auto,
            examples: None,
            sensitivity: 0.5,
            video_interval_ms: DEFAULT_VIDEO_INTERVAL_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub path: String,
    pub folder: String,
    pub encoder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_fingerprint: Option<String>,
    pub rank: Rank,
    pub sensitivity: f64,
    pub threshold: f64,
    pub video_interval_ms: u64,
    pub files: u64,
    pub failed_files: u64,
    pub missing_files: u64,
    pub items: u64,
    pub pending: u64,
    /// Pending items at or above the threshold, which `next` offers.
    pub interesting: u64,
    pub kept: u64,
    pub discarded: u64,
    pub duplicates: u64,
    pub exported: u64,
    pub examples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub examples_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanned_ms: Option<u64>,
    /// Why the latest pending item was dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ScanSummary {
    /// Media files found in the folder.
    pub files: u64,
    pub added: u64,
    pub changed: u64,
    pub missing: u64,
    pub failed: u64,
    pub unchanged: u64,
    /// Media whose names are not UTF-8.
    pub skipped: u64,
    /// Items this scan added, duplicates included.
    pub items: u64,
    pub duplicates: u64,
    /// The session reached its item limit before every file was sampled.
    pub limited: bool,
    /// Stopped before the end; finished files are kept.
    pub stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accelerator: Option<String>,
    /// The first few failures, for the person to fix.
    pub errors: Vec<FileError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileError {
    pub path: String,
    pub error: String,
}

/// How far a scan is, after each file.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub path: String,
    pub items: u64,
}

/// A pending item offered for review.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub id: u64,
    /// Relative to the folder.
    pub path: String,
    pub kind: &'static str,
    /// Where in a video the frame is; `None` for a still.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub score: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExportSummary {
    pub output: String,
    pub exported: Vec<Exported>,
    pub skipped: Vec<FileError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Exported {
    pub id: u64,
    pub source: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<String>,
}

pub struct ReviewSession {
    db: Connection,
    path: PathBuf,
    folder: PathBuf,
    encoder: Encoder,
    rank: Rank,
    sensitivity: f64,
    video_interval_ms: u64,
    examples_from: Option<String>,
    ranker: Ranker,
    note: Option<String>,
}

impl ReviewSession {
    /// Create a session for `options.folder`. Nothing is scanned yet.
    pub fn create(path: &Path, options: &Options) -> Result<Self> {
        let path = absolute(path)?;
        validate_session_path(&path)?;
        validate_settings(options.sensitivity)?;
        ensure!(
            VIDEO_INTERVAL_MS.contains(&options.video_interval_ms),
            "video sampling interval must be between 100 and 3600000 ms"
        );
        ensure!(
            !path.exists(),
            "folder review session already exists; resume it instead"
        );
        let folder = absolute(&options.folder)?;
        crate::volumes::ensure_present(&folder)?;
        let folder = folder
            .canonicalize()
            .ok()
            .filter(|folder| folder.is_dir())
            .ok_or_else(|| {
                Categorized::error(
                    ErrorKind::Unavailable,
                    format!("folder {} does not exist", options.folder.display()),
                )
            })?;
        let folder_text = folder
            .to_str()
            .context("folder path must be valid UTF-8")?
            .to_owned();
        // Resolve the encoder and examples before creating the file, so a
        // refusal leaves nothing behind.
        let (encoder, examples, examples_from) = match &options.examples {
            Some(source) => {
                let source = absolute(source)?;
                let (encoder, examples) = import_examples(&source, options.encoder)?;
                (
                    encoder,
                    examples,
                    Some(source.to_string_lossy().into_owned()),
                )
            }
            None => (autocapture::new_encoder(options.encoder)?, Vec::new(), None),
        };
        let rank = match options.rank {
            RankChoice::Novelty => Rank::Novelty,
            RankChoice::Similar => Rank::Similar,
            RankChoice::Auto if examples.is_empty() => Rank::Novelty,
            RankChoice::Auto => Rank::Similar,
        };
        if let Some(parent) = path.parent() {
            crate::volumes::ensure_present(parent)?;
        }
        let created = (|| {
            let db = connect(&path, false)?;
            db.execute_batch(
                "CREATE TABLE review_session (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    schema_version UINTEGER NOT NULL, descriptor_version UINTEGER NOT NULL,
                    folder VARCHAR NOT NULL, created_ms UBIGINT NOT NULL,
                    rank VARCHAR NOT NULL CHECK (rank IN ('novelty', 'similar')),
                    sensitivity DOUBLE NOT NULL, video_interval_ms UBIGINT NOT NULL,
                    encoder VARCHAR NOT NULL, descriptor_dims UINTEGER NOT NULL,
                    model_fingerprint VARCHAR, examples_from VARCHAR, scanned_ms UBIGINT
                );
                CREATE TABLE files (
                    id UBIGINT PRIMARY KEY, path VARCHAR NOT NULL UNIQUE,
                    kind VARCHAR NOT NULL CHECK (kind IN ('image', 'video')),
                    bytes UBIGINT NOT NULL, modified_ns UBIGINT NOT NULL,
                    status VARCHAR NOT NULL CHECK (status IN ('ok', 'failed', 'missing')),
                    error VARCHAR, scanned_ms UBIGINT NOT NULL
                );
                CREATE TABLE items (
                    id UBIGINT PRIMARY KEY, file_id UBIGINT NOT NULL, offset_ms UBIGINT,
                    source_bytes UBIGINT NOT NULL, source_modified_ns UBIGINT NOT NULL,
                    width UINTEGER NOT NULL, height UINTEGER NOT NULL, descriptor BLOB NOT NULL,
                    label VARCHAR NOT NULL CHECK (label IN ('pending', 'kept', 'discarded', 'duplicate')),
                    duplicate_of UBIGINT, review_seq UBIGINT, reviewed_ms UBIGINT,
                    score DOUBLE, reason VARCHAR,
                    near_reviewed DOUBLE, near_kept DOUBLE, near_discarded DOUBLE,
                    exported_path VARCHAR
                );
                CREATE TABLE examples (id UINTEGER PRIMARY KEY, keep BOOLEAN NOT NULL, descriptor BLOB NOT NULL);
                CREATE TABLE rank_update (
                    id UBIGINT NOT NULL, near_reviewed DOUBLE, near_kept DOUBLE,
                    near_discarded DOUBLE, duplicate_of UBIGINT
                );",
            )?;
            db.execute(
                "INSERT INTO review_session VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
                params![
                    SCHEMA_VERSION,
                    encoder.version(),
                    folder_text,
                    epoch_ms(),
                    rank.name(),
                    options.sensitivity,
                    options.video_interval_ms,
                    encoder.name(),
                    encoder.dims() as u32,
                    encoder.fingerprint(),
                    examples_from,
                ],
            )?;
            for (id, (keep, feature)) in examples.iter().enumerate() {
                db.execute(
                    "INSERT INTO examples VALUES (?, ?, ?)",
                    params![id as u32, keep, autocapture::encode(feature)],
                )?;
            }
            db.execute_batch("CHECKPOINT")?;
            drop(db);
            Self::open(&path)
        })();
        if created.is_err() {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(wal(&path));
        }
        created
    }

    /// Reopen a session with its settings, decisions and ranking.
    pub fn open(path: &Path) -> Result<Self> {
        let path = absolute(path)?;
        validate_session_path(&path)?;
        if let Some(parent) = path.parent() {
            crate::volumes::ensure_present(parent)?;
        }
        ensure!(
            path.is_file(),
            "folder review session does not exist: {}",
            path.display()
        );
        let db = connect(&path, false)?;
        #[allow(clippy::type_complexity)]
        let (version, descriptor_version, folder, rank, sensitivity, video_interval_ms, name, dims, fingerprint, examples_from): (u32, u32, String, String, f64, u64, String, u32, Option<String>, Option<String>) = db
            .query_row(
                "SELECT schema_version, descriptor_version, folder, rank, sensitivity, video_interval_ms,
                        encoder, descriptor_dims, model_fingerprint, examples_from
                 FROM review_session WHERE singleton = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
            )
            .context("not a Capturefab folder review session or missing session metadata")?;
        ensure!(
            version == SCHEMA_VERSION,
            "unsupported folder review schema version {version}"
        );
        let encoder = autocapture::stored_encoder(&name, dims as usize, fingerprint.as_deref())
            .map_err(|error| {
                anyhow!(
                    error
                        .to_string()
                        .replace("capture session", "folder review session")
                )
            })?;
        ensure!(
            descriptor_version == encoder.version(),
            "unsupported folder review descriptor version {descriptor_version}"
        );
        validate_settings(sensitivity)?;
        ensure!(
            VIDEO_INTERVAL_MS.contains(&video_interval_ms),
            "invalid folder review video interval"
        );
        let mut session = Self {
            db,
            path,
            folder: PathBuf::from(folder),
            encoder,
            rank: Rank::parse(&rank)?,
            sensitivity,
            video_interval_ms,
            examples_from,
            ranker: Ranker::new(Vec::new()),
            note: None,
        };
        session.reload()?;
        Ok(session)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Load every pending item, as stored.
    fn reload(&mut self) -> Result<()> {
        let mut statement = self.db.prepare(
            "SELECT id, descriptor, near_reviewed, near_kept, near_discarded FROM items
             WHERE label = 'pending' ORDER BY id",
        )?;
        let rows = statement.query_map([], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
            ))
        })?;
        let mut candidates = Vec::new();
        for row in rows {
            let (id, bytes, near_reviewed, near_kept, near_discarded) = row?;
            candidates.push(Candidate {
                id,
                feature: self.encoder.decode(&bytes)?,
                near_reviewed: near_reviewed.filter(|d| d.is_finite()),
                near_kept: near_kept.filter(|d| d.is_finite()),
                near_discarded: near_discarded.filter(|d| d.is_finite()),
                dirty: false,
            });
        }
        self.ranker = Ranker::new(candidates);
        Ok(())
    }

    pub fn status(&self) -> Result<Status> {
        let (files, failed_files, missing_files): (u64, u64, u64) = self.db.query_row(
            "SELECT COUNT(*) FILTER (WHERE status <> 'missing'), COUNT(*) FILTER (WHERE status = 'failed'),
                    COUNT(*) FILTER (WHERE status = 'missing') FROM files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let (items, pending, kept, discarded, duplicates, exported): (u64, u64, u64, u64, u64, u64) =
            self.db.query_row(
                "SELECT COUNT(*), COUNT(*) FILTER (WHERE label = 'pending'), COUNT(*) FILTER (WHERE label = 'kept'),
                        COUNT(*) FILTER (WHERE label = 'discarded'), COUNT(*) FILTER (WHERE label = 'duplicate'),
                        COUNT(exported_path) FROM items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
        let examples: u64 = self
            .db
            .query_row("SELECT COUNT(*) FROM examples", [], |r| r.get(0))?;
        let scanned_ms: Option<u64> = self.db.query_row(
            "SELECT scanned_ms FROM review_session WHERE singleton = 1",
            [],
            |r| r.get(0),
        )?;
        Ok(Status {
            path: self.path.to_string_lossy().into_owned(),
            folder: self.folder.to_string_lossy().into_owned(),
            encoder: self.encoder.name().into(),
            model_fingerprint: self.encoder.fingerprint(),
            rank: self.rank,
            sensitivity: self.sensitivity,
            threshold: rank::threshold(self.sensitivity),
            video_interval_ms: self.video_interval_ms,
            files,
            failed_files,
            missing_files,
            items,
            pending,
            interesting: self
                .ranker
                .interesting(&self.encoder, self.rank, self.sensitivity)
                as u64,
            kept,
            discarded,
            duplicates,
            exported,
            examples,
            examples_from: self.examples_from.clone(),
            scanned_ms,
            note: self.note.clone(),
        })
    }

    pub fn configure(&mut self, rank: Option<Rank>, sensitivity: Option<f64>) -> Result<()> {
        let rank = rank.unwrap_or(self.rank);
        let sensitivity = sensitivity.unwrap_or(self.sensitivity);
        validate_settings(sensitivity)?;
        self.db.execute(
            "UPDATE review_session SET rank = ?, sensitivity = ? WHERE singleton = 1",
            params![rank.name(), sensitivity],
        )?;
        self.rank = rank;
        self.sensitivity = sensitivity;
        Ok(())
    }

    /// Bring the session up to date with the folder: sample new files and
    /// files that changed or failed before, and drop pending items of files
    /// that are gone. Each file is committed on its own, so a stopped or
    /// interrupted scan keeps what it finished and the next one goes on.
    pub fn scan(
        &mut self,
        stop: &AtomicBool,
        progress: &mut dyn FnMut(&Progress),
    ) -> Result<ScanSummary> {
        crate::volumes::ensure_present(&self.folder)?;
        if !self.folder.is_dir() {
            return Err(Categorized::error(
                ErrorKind::Unavailable,
                format!("folder {} is missing", self.folder.display()),
            ));
        }
        let walk = scan::walk(&self.folder)?;
        let mut summary = ScanSummary {
            files: walk.files.len() as u64,
            skipped: walk.skipped,
            ..ScanSummary::default()
        };
        let mut known: HashMap<String, (u64, u64, u64, String)> = HashMap::new();
        {
            let mut statement = self
                .db
                .prepare("SELECT path, id, bytes, modified_ns, status FROM files")?;
            let rows = statement.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
                ))
            })?;
            for row in rows {
                let (path, file) = row?;
                known.insert(path, file);
            }
        }
        let learned = Learned::load(&self.db, &self.encoder)?;
        let found: std::collections::HashSet<&str> =
            walk.files.iter().map(|f| f.path.as_str()).collect();
        for (path, (id, _, _, status)) in &known {
            if status != "missing" && !found.contains(path.as_str()) {
                let tx = self.db.transaction()?;
                tx.execute(
                    "UPDATE files SET status = 'missing', scanned_ms = ? WHERE id = ?",
                    params![epoch_ms(), id],
                )?;
                let dropped = drop_unreviewed(&tx, *id, &learned, &self.encoder)?;
                tx.commit()?;
                self.apply(dropped);
                summary.missing += 1;
            }
        }
        let mut recent: VecDeque<(u64, Descriptor)> = VecDeque::new();
        let mut items: u64 = self
            .db
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
        let mut next_item: u64 =
            self.db
                .query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM items", [], |r| {
                    r.get(0)
                })?;
        let mut next_file: u64 =
            self.db
                .query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM files", [], |r| {
                    r.get(0)
                })?;
        // Started with the first file to sample: an unchanged folder needs
        // no GPU helper.
        let mut backend: Option<Backend> = None;
        let total = walk.files.len();
        for (done, file) in walk.files.iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                summary.stopped = true;
                break;
            }
            let previous = known.get(&file.path);
            match previous {
                Some((_, bytes, modified, status))
                    if *bytes == file.bytes && *modified == file.modified_ns && status == "ok" =>
                {
                    summary.unchanged += 1;
                    continue;
                }
                _ => {}
            }
            let file_id = previous.map_or(next_file, |(id, ..)| *id);
            // Resampling replaces the file's unreviewed rows.
            let replaced: u64 = if previous.is_some() {
                self.db.query_row(
                    "SELECT COUNT(*) FROM items WHERE file_id = ? AND label IN ('pending', 'duplicate')",
                    params![file_id],
                    |r| r.get(0),
                )?
            } else {
                0
            };
            let kept_items = items - replaced.min(items);
            if kept_items >= MAX_ITEMS {
                summary.limited = true;
                break;
            }
            let backend = backend.get_or_insert_with(|| Backend::start(&self.encoder));
            let source = scan::source(&self.folder, &file.path)?;
            // Sample first, then write the file's rows in one transaction.
            let mut sampled: Vec<(Option<u64>, u32, u32, Descriptor)> = Vec::new();
            let room = (MAX_ITEMS - kept_items) as usize;
            let decoded = match file.kind {
                Kind::Image => scan::still(&source).and_then(|frame| {
                    let feature = backend.describe(&self.encoder, &frame)?;
                    sampled.push((None, frame.width, frame.height, feature));
                    Ok(())
                }),
                Kind::Video => scan::video(
                    &source,
                    self.video_interval_ms,
                    MAX_VIDEO_SAMPLES.min(room),
                    |offset, frame| {
                        if stop.load(Ordering::Relaxed) {
                            return Ok(false);
                        }
                        let feature = backend.describe(&self.encoder, &frame)?;
                        sampled.push((Some(offset), frame.width, frame.height, feature));
                        Ok(true)
                    },
                ),
            };
            if stop.load(Ordering::Relaxed) {
                // A partly sampled video is sampled again next time.
                summary.stopped = true;
                break;
            }
            let mut error = decoded.err().map(|error| format!("{error:#}"));
            if error.is_none() && file.kind == Kind::Video && sampled.len() >= room {
                // Cut short by the item limit: a later scan tries it again.
                summary.limited = true;
                error =
                    Some("the review reached its item limit before the end of this video".into());
            }
            let tx = self.db.transaction()?;
            let dropped = if previous.is_some() {
                tx.execute(
                    "UPDATE files SET kind = ?, bytes = ?, modified_ns = ?, status = ?, error = ?, scanned_ms = ? WHERE id = ?",
                    params![file.kind.name(), file.bytes, file.modified_ns, if error.is_some() { "failed" } else { "ok" }, error, epoch_ms(), file_id],
                )?;
                drop_unreviewed(&tx, file_id, &learned, &self.encoder)?
            } else {
                tx.execute(
                    "INSERT INTO files VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        file_id,
                        file.path,
                        file.kind.name(),
                        file.bytes,
                        file.modified_ns,
                        if error.is_some() { "failed" } else { "ok" },
                        error,
                        epoch_ms()
                    ],
                )?;
                Dropped::default()
            };
            let mut added = Vec::new();
            let duplicate = self.encoder.metric().duplicate;
            for (offset, width, height, feature) in sampled {
                let near = learned.nearest(&self.encoder, &feature);
                let repeat = recent
                    .iter()
                    .map(|(id, other)| (*id, self.encoder.distance(other, &feature)))
                    .chain(near.reviewed)
                    .filter(|(_, distance)| *distance < duplicate)
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(id, _)| id);
                let id = next_item;
                tx.execute(
                    "INSERT INTO items (id, file_id, offset_ms, source_bytes, source_modified_ns, width, height,
                                        descriptor, label, duplicate_of, near_reviewed, near_kept, near_discarded)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        id,
                        file_id,
                        offset,
                        file.bytes,
                        file.modified_ns,
                        width,
                        height,
                        autocapture::encode(&feature),
                        if repeat.is_some() { "duplicate" } else { "pending" },
                        repeat,
                        near.reviewed.map(|(_, d)| d),
                        near.kept,
                        near.discarded,
                    ],
                )?;
                next_item += 1;
                summary.items += 1;
                if repeat.is_some() {
                    summary.duplicates += 1;
                } else {
                    if recent.len() >= RECENT {
                        recent.pop_front();
                    }
                    recent.push_back((id, feature.clone()));
                    added.push(Candidate {
                        id,
                        near_reviewed: near.reviewed.map(|(_, d)| d),
                        near_kept: near.kept,
                        near_discarded: near.discarded,
                        feature,
                        dirty: false,
                    });
                }
            }
            tx.commit()?;
            items = self
                .db
                .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
            self.apply(dropped);
            self.ranker.add(added);
            if previous.is_none() {
                next_file += 1;
                summary.added += 1;
            } else {
                summary.changed += 1;
            }
            if let Some(error) = error {
                summary.failed += 1;
                if summary.errors.len() < 10 {
                    summary.errors.push(FileError {
                        path: file.path.clone(),
                        error,
                    });
                }
            }
            progress(&Progress {
                done: done + 1,
                total,
                path: file.path.clone(),
                items,
            });
        }
        summary.accelerator = backend.and_then(|backend| backend.accelerator());
        self.db.execute(
            "UPDATE review_session SET scanned_ms = ? WHERE singleton = 1",
            params![epoch_ms()],
        )?;
        self.db.execute_batch("CHECKPOINT")?;
        Ok(summary)
    }

    /// The most interesting pending item, or with `all` the best pending
    /// one even below the threshold. A pending item whose file is gone or
    /// changed is dropped with a note; scanning again samples it afresh.
    pub fn next(&mut self, all: bool) -> Result<Option<Item>> {
        loop {
            let Some((index, score, reason)) =
                self.ranker
                    .best(&self.encoder, self.rank, self.sensitivity, all)
            else {
                return Ok(None);
            };
            let id = self.ranker.candidates[index].id;
            let row = self.located(id)?;
            if let Err(lost) = scan::unchanged(&row.source, row.bytes, row.modified_ns) {
                self.forget(id, &row.path, lost)?;
                continue;
            }
            return Ok(Some(Item {
                id,
                path: row.path,
                kind: row.kind.name(),
                offset_ms: row.offset_ms,
                width: row.width,
                height: row.height,
                score,
                reason,
            }));
        }
    }

    /// Decode an item's frame from its file, which must be as it was scanned.
    pub fn frame(&self, id: u64) -> Result<Frame> {
        let row = self.located(id)?;
        scan::unchanged(&row.source, row.bytes, row.modified_ns)
            .map_err(|lost| anyhow!("{} {lost}; scan the folder again", row.path))?;
        match (row.kind, row.offset_ms) {
            (Kind::Video, Some(offset)) => scan::video_frame(&row.source, offset),
            _ => scan::still(&row.source),
        }
    }

    fn apply(&mut self, dropped: Dropped) {
        for id in dropped.removed {
            self.ranker.remove(id);
        }
        self.ranker.add(dropped.restored);
    }

    fn located(&self, id: u64) -> Result<Located> {
        let (path, kind, bytes, modified_ns, offset_ms, width, height): (
            String,
            String,
            u64,
            u64,
            Option<u64>,
            u32,
            u32,
        ) = self
            .db
            .query_row(
                "SELECT f.path, f.kind, i.source_bytes, i.source_modified_ns, i.offset_ms, i.width, i.height
                 FROM items i JOIN files f ON f.id = i.file_id WHERE i.id = ?",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?
            .with_context(|| format!("folder review has no item {id}"))?;
        Ok(Located {
            source: scan::source(&self.folder, &path)?,
            path,
            kind: Kind::parse(&kind)?,
            bytes,
            modified_ns,
            offset_ms,
            width,
            height,
        })
    }

    fn forget(&mut self, id: u64, path: &str, lost: &str) -> Result<()> {
        let learned = Learned::load(&self.db, &self.encoder)?;
        let tx = self.db.transaction()?;
        let restored = restore_duplicates(&tx, &[id], &learned, &self.encoder)?;
        tx.execute(
            "DELETE FROM items WHERE id = ? AND label = 'pending'",
            params![id],
        )?;
        tx.commit()?;
        self.apply(Dropped {
            removed: vec![id],
            restored,
        });
        self.note = Some(format!(
            "Dropped pending item {id}: {path} {lost}; scan the folder again to review it"
        ));
        Ok(())
    }

    /// Keep or discard a pending item. A pending-only transaction refuses a
    /// repeated or stale decision. The decision teaches ranking at once, and
    /// pending items within duplicate distance of it leave the queue.
    pub fn review(&mut self, id: u64, keep: bool) -> Result<()> {
        let Some(index) = self.ranker.candidates.iter().position(|c| c.id == id) else {
            bail!("item is no longer pending review");
        };
        let (score, reason) =
            self.ranker
                .score(&self.encoder, self.rank, &self.ranker.candidates[index]);
        let feature = self.ranker.candidates[index].feature.clone();
        let result = self.decide(id, keep, &feature, score, &reason);
        if result.is_err() {
            // Ranking may have learned a decision that was not saved.
            self.reload()?;
        }
        result
    }

    fn decide(
        &mut self,
        id: u64,
        keep: bool,
        feature: &Descriptor,
        score: f64,
        reason: &str,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let seq: u64 = tx.query_row(
            "SELECT COALESCE(MAX(review_seq), 0) + 1 FROM items",
            [],
            |r| r.get(0),
        )?;
        let changed = tx.execute(
            "UPDATE items SET label = ?, review_seq = ?, reviewed_ms = ?, score = ?, reason = ?
             WHERE id = ? AND label = 'pending'",
            params![
                if keep { "kept" } else { "discarded" },
                seq,
                epoch_ms(),
                score,
                reason,
                id
            ],
        )?;
        ensure!(changed == 1, "item is no longer pending review");
        self.ranker.remove(id);
        let duplicates = self.ranker.decided(&self.encoder, feature, keep);
        tx.execute_batch("DELETE FROM rank_update")?;
        {
            let mut appender = tx.appender("rank_update")?;
            for duplicate in &duplicates {
                appender.append_row(params![
                    duplicate,
                    None::<f64>,
                    None::<f64>,
                    None::<f64>,
                    id
                ])?;
            }
            for candidate in self.ranker.candidates.iter().filter(|c| c.dirty) {
                appender.append_row(params![
                    candidate.id,
                    candidate.near_reviewed,
                    candidate.near_kept,
                    candidate.near_discarded,
                    None::<u64>
                ])?;
            }
            appender.flush()?;
        }
        tx.execute_batch(
            "UPDATE items SET label = 'duplicate', duplicate_of = u.duplicate_of
             FROM rank_update u WHERE items.id = u.id AND u.duplicate_of IS NOT NULL AND items.label = 'pending';
             UPDATE items SET near_reviewed = u.near_reviewed, near_kept = u.near_kept, near_discarded = u.near_discarded
             FROM rank_update u WHERE items.id = u.id AND u.duplicate_of IS NULL AND items.label = 'pending';
             DELETE FROM rank_update;",
        )?;
        tx.commit()?;
        for candidate in &mut self.ranker.candidates {
            candidate.dirty = false;
        }
        Ok(())
    }

    /// Copy every kept item not yet exported into `target`: stills byte for
    /// byte, video frames as PNG. Files go through the storage ledger and
    /// never replace anything; a destination's bucket gets them queued for
    /// upload. Items whose source changed since the scan are skipped.
    pub fn export(&mut self, target: &Resolved) -> Result<ExportSummary> {
        let root = PathBuf::from(&target.output);
        let mut summary = ExportSummary {
            output: target.output.clone(),
            ..ExportSummary::default()
        };
        let mut rows = Vec::new();
        {
            let mut statement = self.db.prepare(
                "SELECT i.id FROM items i WHERE i.label = 'kept' AND i.exported_path IS NULL ORDER BY i.id",
            )?;
            for id in statement.query_map([], |r| r.get::<_, u64>(0))? {
                rows.push(id?);
            }
        }
        let policy = storage::configured_policy()?;
        for id in rows {
            let row = self.located(id)?;
            if let Err(lost) = scan::unchanged(&row.source, row.bytes, row.modified_ns) {
                summary.skipped.push(FileError {
                    path: row.path,
                    error: format!("{lost}; it was not exported"),
                });
                continue;
            }
            let (bytes, name) = match (row.kind, row.offset_ms) {
                (Kind::Video, Some(offset)) => {
                    let frame = scan::video_frame(&row.source, offset)?;
                    let name = match row.path.rsplit_once('.') {
                        Some((stem, _)) => format!("{stem}-{offset}ms.png"),
                        None => format!("{}-{offset}ms.png", row.path),
                    };
                    (crate::frame::encode(&frame, "png")?, name)
                }
                _ => {
                    let bytes = std::fs::read(&row.source)
                        .with_context(|| format!("read {}", row.source.display()))?;
                    (bytes, row.path.clone())
                }
            };
            let (path, stored) = save_new(&root, &name, &bytes, &policy, target)?;
            let upload = target
                .finished(&path, Some(&stored.sha256))?
                .map(|queued| queued.id);
            let text = path.to_string_lossy().into_owned();
            self.db.execute(
                "UPDATE items SET exported_path = ? WHERE id = ?",
                params![text, id],
            )?;
            summary.exported.push(Exported {
                id,
                source: row.path,
                path: text,
                bytes: stored.bytes,
                sha256: stored.sha256,
                upload,
            });
        }
        Ok(summary)
    }

    /// Checkpoint, so the database file is complete without its WAL.
    pub fn close(self) -> Result<()> {
        self.db.execute_batch("CHECKPOINT")?;
        Ok(())
    }
}

struct Located {
    source: PathBuf,
    path: String,
    kind: Kind,
    bytes: u64,
    modified_ns: u64,
    offset_ms: Option<u64>,
    width: u32,
    height: u32,
}

/// What a scanned item is compared with: reviewed items and imported
/// examples.
struct Learned {
    reviewed: Vec<(u64, bool, Descriptor)>,
    examples: Vec<(bool, Descriptor)>,
}

struct Near {
    reviewed: Option<(u64, f64)>,
    kept: Option<f64>,
    discarded: Option<f64>,
}

impl Learned {
    fn load(db: &Connection, encoder: &Encoder) -> Result<Self> {
        let mut reviewed = Vec::new();
        let mut statement = db.prepare(
            "SELECT id, label = 'kept', descriptor FROM items WHERE label IN ('kept', 'discarded') ORDER BY id",
        )?;
        for row in statement.query_map([], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })? {
            let (id, keep, bytes) = row?;
            reviewed.push((id, keep, encoder.decode(&bytes)?));
        }
        let mut examples = Vec::new();
        let mut statement = db.prepare("SELECT keep, descriptor FROM examples ORDER BY id")?;
        for row in
            statement.query_map([], |r| Ok((r.get::<_, bool>(0)?, r.get::<_, Vec<u8>>(1)?)))?
        {
            let (keep, bytes) = row?;
            examples.push((keep, encoder.decode(&bytes)?));
        }
        Ok(Self { reviewed, examples })
    }

    fn nearest(&self, encoder: &Encoder, feature: &Descriptor) -> Near {
        let mut near = Near {
            reviewed: None,
            kept: None,
            discarded: None,
        };
        let lower = |slot: &mut Option<f64>, distance: f64| {
            if slot.is_none_or(|nearest| distance < nearest) {
                *slot = Some(distance);
            }
        };
        for (id, keep, other) in &self.reviewed {
            let distance = encoder.distance(other, feature);
            if near.reviewed.is_none_or(|(_, nearest)| distance < nearest) {
                near.reviewed = Some((*id, distance));
            }
            lower(
                if *keep {
                    &mut near.kept
                } else {
                    &mut near.discarded
                },
                distance,
            );
        }
        for (keep, other) in &self.examples {
            let distance = encoder.distance(other, feature);
            lower(
                if *keep {
                    &mut near.kept
                } else {
                    &mut near.discarded
                },
                distance,
            );
        }
        near
    }
}

/// What dropping a file's unreviewed items changed in the queue.
#[derive(Default)]
struct Dropped {
    removed: Vec<u64>,
    /// Duplicates in other files of a dropped item, pending again.
    restored: Vec<Candidate>,
}

/// Delete a file's pending and duplicate items, keeping decisions. An item
/// elsewhere that only repeated one of them is pending again.
fn drop_unreviewed(
    tx: &Connection,
    file_id: u64,
    learned: &Learned,
    encoder: &Encoder,
) -> Result<Dropped> {
    let mut dropped = Dropped::default();
    let mut statement =
        tx.prepare("SELECT id FROM items WHERE file_id = ? AND label = 'pending'")?;
    dropped.removed = statement
        .query_map(params![file_id], |r| r.get::<_, u64>(0))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    let mut statement =
        tx.prepare("SELECT id FROM items WHERE file_id = ? AND label = 'duplicate'")?;
    let repeats = statement
        .query_map(params![file_id], |r| r.get::<_, u64>(0))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    // Repeats in this file go with it, so only other files' are restored.
    dropped.restored = restore_duplicates(tx, &dropped.removed, learned, encoder)?
        .into_iter()
        .filter(|candidate| !repeats.contains(&candidate.id))
        .collect();
    tx.execute(
        "DELETE FROM items WHERE file_id = ? AND label IN ('pending', 'duplicate')",
        params![file_id],
    )?;
    Ok(dropped)
}

/// Make the duplicates of `originals` pending again, ranked against what is
/// learned, before the originals are deleted.
fn restore_duplicates(
    tx: &Connection,
    originals: &[u64],
    learned: &Learned,
    encoder: &Encoder,
) -> Result<Vec<Candidate>> {
    let mut restored = Vec::new();
    let mut statement = tx.prepare(
        "SELECT id, descriptor FROM items WHERE label = 'duplicate' AND duplicate_of = ? ORDER BY id",
    )?;
    for original in originals {
        let orphans = statement
            .query_map(params![original], |r| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<duckdb::Result<Vec<_>>>()?;
        for (id, bytes) in orphans {
            let feature = encoder.decode(&bytes)?;
            let near = learned.nearest(encoder, &feature);
            let near_reviewed = near.reviewed.map(|(_, d)| d);
            tx.execute(
                "UPDATE items SET label = 'pending', duplicate_of = NULL, near_reviewed = ?, near_kept = ?, near_discarded = ? WHERE id = ?",
                params![near_reviewed, near.kept, near.discarded, id],
            )?;
            restored.push(Candidate {
                id,
                feature,
                near_reviewed,
                near_kept: near.kept,
                near_discarded: near.discarded,
                dirty: false,
            });
        }
    }
    Ok(restored)
}

/// Save `bytes` as `name` below `root`, or as the first free `-N` variant.
fn save_new(
    root: &Path,
    name: &str,
    bytes: &[u8],
    policy: &storage::StoragePolicy,
    target: &Resolved,
) -> Result<(PathBuf, storage::Stored)> {
    let relative = scan::source(root, name)?;
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !extension.contains('/') => (stem, Some(extension)),
        _ => (name, None),
    };
    for attempt in 0..100 {
        let path = match attempt {
            0 => relative.clone(),
            n => scan::source(
                root,
                &match extension {
                    Some(extension) => format!("{stem}-{n}.{extension}"),
                    None => format!("{stem}-{n}"),
                },
            )?,
        };
        target.contain(&path)?;
        match storage::save_bytes(bytes, &path, policy) {
            Ok(stored) => return Ok((path, stored)),
            Err(error) if error.downcast_ref::<storage::OutputExists>().is_some() => continue,
            Err(error) => return Err(error),
        }
    }
    bail!("no free file name for {name} in {}", root.display())
}

/// Read a capture session's kept and discarded examples, and the encoder
/// they were learned with. An explicit encoder must be that one: descriptors
/// from another model live in a different space.
fn import_examples(
    source: &Path,
    choice: Option<EncoderChoice>,
) -> Result<(Encoder, Vec<(bool, Descriptor)>)> {
    ensure!(
        source.is_file(),
        "capture session does not exist: {}",
        source.display()
    );
    let db = connect(source, true)?;
    let version: u32 = db
        .query_row(
            "SELECT schema_version FROM capture_session WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .context("examples must come from a Capturefab capture session")?;
    let (name, dims, fingerprint): (String, u32, Option<String>) = if version == 1 {
        ("native".into(), 84, None)
    } else {
        db.query_row(
            "SELECT encoder, descriptor_dims, model_fingerprint FROM capture_session WHERE singleton = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
    };
    let encoder = autocapture::stored_encoder(&name, dims as usize, fingerprint.as_deref())?;
    if let Some(choice) = choice.filter(|choice| *choice != EncoderChoice::Auto) {
        let chosen = autocapture::new_encoder(Some(choice))?;
        ensure!(
            chosen.name() == encoder.name() && chosen.fingerprint() == encoder.fingerprint(),
            "the capture session's examples use the {} encoder; create the review with --encoder {} or without --encoder",
            encoder.name(),
            encoder.name()
        );
    }
    let mut statement = db.prepare(&format!(
        "SELECT label <> 'discarded', descriptor FROM captures
         WHERE label IN ('manual', 'kept', 'discarded') ORDER BY id DESC LIMIT {MAX_EXAMPLES}"
    ))?;
    let mut examples = Vec::new();
    for row in statement.query_map([], |r| Ok((r.get::<_, bool>(0)?, r.get::<_, Vec<u8>>(1)?)))? {
        let (keep, bytes) = row?;
        examples.push((keep, encoder.decode(&bytes)?));
    }
    examples.reverse();
    ensure!(
        !examples.is_empty(),
        "the capture session has no captures, kept or discarded examples to learn from"
    );
    Ok((encoder, examples))
}

fn connect(path: &Path, read_only: bool) -> Result<Connection> {
    let mut config = Config::default()
        .max_memory(MEMORY_LIMIT)?
        .threads(1)?
        .enable_autoload_extension(false)?;
    if read_only {
        config = config.access_mode(AccessMode::ReadOnly)?;
    }
    let db = Connection::open_with_flags(path, config).map_err(|error| {
        if error.to_string().to_ascii_lowercase().contains("lock") {
            Categorized::error(
                ErrorKind::Unavailable,
                format!(
                    "{} is open in another Capturefab process; close it there first",
                    path.display()
                ),
            )
        } else {
            anyhow!(error).context(format!("open {}", path.display()))
        }
    })?;
    // Never load extensions, read external data, or spill beside the file.
    db.execute_batch("SET temp_directory = ''; SET enable_external_access = false;")?;
    Ok(db)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

fn validate_session_path(path: &Path) -> Result<()> {
    ensure!(
        path.extension().and_then(|e| e.to_str()) == Some("duckdb"),
        "folder review session file must end in .duckdb"
    );
    Ok(())
}

pub fn validate_settings(sensitivity: f64) -> Result<()> {
    ensure!(
        sensitivity.is_finite() && (0.0..=1.0).contains(&sensitivity),
        "folder review sensitivity must be between 0 and 1"
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

/// The tests decode their PNG fixtures in process, without FFmpeg.
#[cfg(all(test, feature = "hubs"))]
mod tests;
