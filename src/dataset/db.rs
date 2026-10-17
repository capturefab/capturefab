//! The single read-write connection to one dataset file: schema, sandboxed
//! settings, inserts and the incrementally maintained status.
use super::{DatasetStatus, SCHEMA_VERSION, SavedCamera, SavedFile, SavedItem};
use crate::{
    context::Context,
    signals::{Sample, SignalInfo, TagInfo, Value as SignalValue},
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use duckdb::{AccessMode, Config, Connection, OptionalExt, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime},
};

/// Default cap on the dataset file plus its write-ahead log.
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Inserts stop while the disk holding the dataset has less free space.
pub const FREE_SPACE_FLOOR: u64 = 512 * 1024 * 1024;
/// Rows accepted by one `append_samples` call.
pub const MAX_SAMPLES_PER_CALL: usize = 10_000;

const DDL: &str = "
CREATE TABLE dataset_info (singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  schema_version UINTEGER NOT NULL, dataset_id VARCHAR NOT NULL, name VARCHAR NOT NULL,
  created_ms UBIGINT NOT NULL, generator VARCHAR NOT NULL,
  time_base VARCHAR NOT NULL DEFAULT 'UTC Unix milliseconds, host clock', max_bytes UBIGINT NOT NULL);
CREATE SEQUENCE capture_ids; CREATE SEQUENCE recording_ids; CREATE SEQUENCE event_ids;
CREATE TABLE captures (id UBIGINT PRIMARY KEY DEFAULT nextval('capture_ids'), wall_ms UBIGINT NOT NULL,
  camera_id VARCHAR NOT NULL, vendor VARCHAR, model VARCHAR, serial VARCHAR, transport VARCHAR, source VARCHAR,
  frame_id UBIGINT, timestamp_ns UBIGINT, width UINTEGER, height UINTEGER, pixel_format VARCHAR,
  pixel_format_code UINTEGER, format VARCHAR NOT NULL, bytes UBIGINT NOT NULL, path VARCHAR NOT NULL,
  file_name VARCHAR NOT NULL, sha256 VARCHAR, origin VARCHAR NOT NULL, job_id UBIGINT, destination VARCHAR,
  object_key VARCHAR, upload_id VARCHAR, remote_url VARCHAR, uploaded_ms UBIGINT, context VARCHAR,
  UNIQUE (path, wall_ms));
CREATE TABLE recordings (id UBIGINT PRIMARY KEY DEFAULT nextval('recording_ids'), start_ms UBIGINT NOT NULL,
  end_ms UBIGINT NOT NULL, camera_id VARCHAR NOT NULL, vendor VARCHAR, model VARCHAR, serial VARCHAR,
  transport VARCHAR, source VARCHAR, path VARCHAR NOT NULL, file_name VARCHAR NOT NULL, bytes UBIGINT NOT NULL,
  sha256 VARCHAR, codec VARCHAR, encoder VARCHAR, fps DOUBLE, destination VARCHAR, object_key VARCHAR,
  upload_id VARCHAR, remote_url VARCHAR, uploaded_ms UBIGINT, context VARCHAR, UNIQUE (path, start_ms));
CREATE TABLE signals (source_id VARCHAR NOT NULL, protocol VARCHAR, uri VARCHAR NOT NULL, tag VARCHAR NOT NULL,
  unit VARCHAR, kind VARCHAR, description VARCHAR, first_ms UBIGINT, last_ms UBIGINT,
  PRIMARY KEY (source_id, tag));
CREATE TABLE samples (ts_ms UBIGINT NOT NULL, source_ts_ms UBIGINT, source_id VARCHAR NOT NULL,
  tag VARCHAR NOT NULL, value_double DOUBLE, value_text VARCHAR, quality VARCHAR);
CREATE TABLE events (id UBIGINT PRIMARY KEY DEFAULT nextval('event_ids'), wall_ms UBIGINT NOT NULL,
  kind VARCHAR NOT NULL, camera_id VARCHAR, source_id VARCHAR, text VARCHAR NOT NULL, context VARCHAR);
CREATE VIEW captures_with_signals AS
  SELECT c.id AS capture_id, c.wall_ms, c.camera_id, c.path, c.object_key, c.context, t.source_id, t.tag, g.unit,
         s.ts_ms AS sample_ms, c.wall_ms - s.ts_ms AS sample_age_ms, s.value_double, s.value_text, s.quality
  FROM captures c CROSS JOIN (SELECT DISTINCT source_id, tag FROM samples) t
  ASOF LEFT JOIN samples s ON s.source_id = t.source_id AND s.tag = t.tag AND c.wall_ms >= s.ts_ms
  LEFT JOIN signals g ON g.source_id = t.source_id AND g.tag = t.tag;
";

pub(crate) fn now_ms() -> u64 {
    crate::scheduling::now_ms()
}

pub(crate) fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Paths of datasets open in this process. DuckDB's file lock is per process,
/// so a second open here would not fail on its own and could corrupt the file.
static OPEN_PATHS: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

pub(crate) struct PathClaim(PathBuf);
impl PathClaim {
    fn new(path: &Path) -> Result<Self> {
        let mut open = OPEN_PATHS.lock().unwrap_or_else(PoisonError::into_inner);
        if !open.insert(path.to_owned()) {
            bail!("{}", locked_message(path));
        }
        Ok(Self(path.to_owned()))
    }
}
impl Drop for PathClaim {
    fn drop(&mut self) {
        OPEN_PATHS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

fn locked_message(path: &Path) -> String {
    format!(
        "dataset {} is locked by another process; reach it with --session NAME",
        file_label(path)
    )
}

/// A private scratch folder: DuckDB may write only here (exports, spill).
#[derive(Debug)]
pub(crate) struct Staging {
    pub dir: PathBuf,
}
impl Staging {
    fn create() -> Result<Self> {
        let root = crate::ipc::session_dir().join("dataset-staging");
        crate::ipc::ensure_private_dir(&root)?;
        remove_stale(&root);
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow!("dataset staging name: {e}"))?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let dir = root.join(name);
        crate::ipc::ensure_private_dir(&dir)?;
        fs::create_dir(dir.join("spill")).context("create dataset spill folder")?;
        // allowed_directories compares paths lexically, so use the real path.
        let dir = dir.canonicalize()?;
        Ok(Self { dir })
    }
    /// The folder as a quoted SQL string body, with forward slashes.
    pub fn sql(&self) -> String {
        sql_text(&self.dir.to_string_lossy().replace('\\', "/"))
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
/// Staging folders left by crashed processes, after a day.
fn remove_stale(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let cutoff = SystemTime::now() - Duration::from_secs(24 * 3600);
    for entry in entries.flatten() {
        if entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified < cutoff)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Escape text for a single-quoted SQL literal.
pub(crate) fn sql_text(text: &str) -> String {
    text.replace('\'', "''")
}

fn config(read_only: bool) -> Result<Config> {
    let config = Config::default()
        .max_memory("256MB")?
        .threads(2)?
        .enable_autoload_extension(false)?
        .with("autoinstall_known_extensions", "false")?;
    Ok(if read_only {
        config.access_mode(AccessMode::ReadOnly)?
    } else {
        config
    })
}

/// Confine DuckDB to the staging folder, then lock the settings. The order
/// matters: these settings cannot change once external access is off.
fn configure(conn: &Connection, staging: &Staging) -> Result<()> {
    let dir = staging.sql();
    conn.execute_batch(&format!(
        "SET temp_directory = '{dir}/spill';
         SET max_temp_directory_size = '1GiB';
         SET allowed_directories = ['{dir}/'];
         SET enable_external_access = false;
         SET lock_configuration = true;"
    ))
    .context("configure dataset sandbox")
}

/// Turn DuckDB errors into messages whose wording gives the right exit code.
pub(crate) fn explain(error: duckdb::Error, path: &Path, op: &str) -> anyhow::Error {
    let text = error.to_string();
    if text.contains("Could not set lock") || text.contains("Conflicting lock") {
        anyhow!("{}", locked_message(path))
    } else if text.contains("Out of Memory") {
        anyhow!("dataset {op} ran out of memory; narrow --since/--until or --tables")
    } else {
        anyhow!("dataset {op} failed for {}: {text}", file_label(path))
    }
}

fn not_a_dataset(path: &Path) -> anyhow::Error {
    anyhow!("{} is not a Capturefab dataset", file_label(path))
}

/// Check the file name and resolve the parent directory to a real path.
pub(crate) fn canonical_target(path: &Path, create_parent: bool) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "dataset path must be absolute");
    ensure!(
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("duckdb")),
        "dataset files use the .duckdb extension"
    );
    let name = path.file_name().context("dataset path needs a file name")?;
    let parent = path.parent().context("dataset path needs a folder")?;
    crate::volumes::ensure_present(parent)?;
    if create_parent && !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create dataset folder {}", parent.display()))?;
    }
    let parent = parent
        .canonicalize()
        .with_context(|| format!("dataset folder {} is not available", parent.display()))?;
    Ok(parent.join(name))
}

pub struct Dataset {
    pub(crate) conn: Connection,
    pub(crate) path: PathBuf,
    pub(crate) staging: Arc<Staging>,
    pub(crate) status: DatasetStatus,
    pub(crate) read_only: bool,
    since_size_check: usize,
    /// Last registered description per signal source, for tags first seen in samples.
    sources: BTreeMap<String, (String, String)>,
    _claim: PathClaim,
}

impl Dataset {
    pub fn open(path: &Path, mode: super::OpenMode, max_bytes: Option<u64>) -> Result<Self> {
        use super::OpenMode;
        let path = canonical_target(path, mode != OpenMode::Open)?;
        let claim = PathClaim::new(&path)?;
        let exists = path
            .try_exists()
            .with_context(|| format!("dataset file {} cannot be inspected", path.display()))?;
        let label = file_label(&path);
        let create = match (mode, exists) {
            (OpenMode::Create, true) => {
                bail!("dataset {label} already exists; use --existing or choose a new name")
            }
            (OpenMode::Open, false) => bail!("dataset file {label} does not exist"),
            (_, true) => {
                // Validate read-only first: a read-write open of a foreign file
                // could replay its log or initialize it.
                ensure!(fs::metadata(&path)?.len() > 0, "{}", not_a_dataset(&path));
                let conn = Connection::open_with_flags(&path, config(true)?)
                    .map_err(|e| explain(e, &path, "open"))?;
                read_info(&conn).map_err(|_| not_a_dataset(&path))?;
                drop(conn);
                false
            }
            (_, false) => {
                // Claim the name atomically; DuckDB then creates the file.
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::AlreadyExists => anyhow!(
                            "dataset {label} already exists; use --existing or choose a new name"
                        ),
                        _ => anyhow!("cannot create dataset {}: {e}", path.display()),
                    })?;
                fs::remove_file(&path)?;
                true
            }
        };
        let staging = Arc::new(Staging::create()?);
        let conn = Connection::open_with_flags(&path, config(false)?)
            .map_err(|e| explain(e, &path, "open"))?;
        configure(&conn, &staging)?;
        if create {
            let created = (|| -> Result<()> {
                let mut id = [0u8; 16];
                getrandom::fill(&mut id).map_err(|e| anyhow!("dataset id: {e}"))?;
                let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                conn.execute_batch(&format!("BEGIN TRANSACTION; {DDL}"))?;
                conn.execute(
                    "INSERT INTO dataset_info (singleton, schema_version, dataset_id, name, created_ms, generator, max_bytes)
                     VALUES (1, ?, ?, ?, ?, ?, ?)",
                    params![
                        SCHEMA_VERSION,
                        id,
                        name,
                        now_ms(),
                        format!("capturefab {}", env!("CARGO_PKG_VERSION")),
                        max_bytes.unwrap_or(DEFAULT_MAX_BYTES)
                    ],
                )?;
                conn.execute_batch("COMMIT; CHECKPOINT;")?;
                Ok(())
            })();
            if let Err(e) = created {
                drop(conn);
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(wal_path(&path));
                return Err(e.context(format!("create dataset {label}")));
            }
        } else if let Some(max_bytes) = max_bytes {
            conn.execute(
                "UPDATE dataset_info SET max_bytes = ? WHERE singleton = 1",
                params![max_bytes],
            )
            .map_err(|e| explain(e, &path, "open"))?;
        }
        let mut dataset = Self::from_connection(conn, path, staging, false, claim)?;
        dataset.record_event("dataset_opened", None, None, &label, &Context::new())?;
        Ok(dataset)
    }

    /// Open a dataset no session holds, for queries and exports.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let path = canonical_target(path, false)?;
        let claim = PathClaim::new(&path)?;
        ensure!(
            path.is_file(),
            "dataset file {} does not exist",
            file_label(&path)
        );
        ensure!(fs::metadata(&path)?.len() > 0, "{}", not_a_dataset(&path));
        let staging = Arc::new(Staging::create()?);
        let conn = Connection::open_with_flags(&path, config(true)?)
            .map_err(|e| explain(e, &path, "open"))?;
        configure(&conn, &staging)?;
        Self::from_connection(conn, path, staging, true, claim)
    }

    fn from_connection(
        conn: Connection,
        path: PathBuf,
        staging: Arc<Staging>,
        read_only: bool,
        claim: PathClaim,
    ) -> Result<Self> {
        let info = read_info(&conn).map_err(|_| not_a_dataset(&path))?;
        let mut dataset = Self {
            conn,
            status: DatasetStatus {
                path: path.display().to_string(),
                dataset_id: info.0,
                name: info.1,
                created_ms: info.2,
                max_bytes: info.3,
                ..Default::default()
            },
            path,
            staging,
            read_only,
            since_size_check: 0,
            sources: BTreeMap::new(),
            _claim: claim,
        };
        dataset.load_counts()?;
        Ok(dataset)
    }

    fn load_counts(&mut self) -> Result<()> {
        let conn = &self.conn;
        let count = |table: &str| -> Result<u64> {
            Ok(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                    r.get::<_, u64>(0)
                })?,
            )
        };
        self.status.captures = count("captures")?;
        self.status.recordings = count("recordings")?;
        self.status.signals = count("signals")?;
        self.status.samples = count("samples")?;
        self.status.events = count("events")?;
        let (first, last): (Option<u64>, Option<u64>) = conn.query_row(
            "SELECT min(t), max(t) FROM (SELECT wall_ms AS t FROM captures UNION ALL
             SELECT start_ms FROM recordings UNION ALL SELECT end_ms FROM recordings UNION ALL
             SELECT min(ts_ms) FROM samples UNION ALL SELECT max(ts_ms) FROM samples)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        self.status.first_ms = first;
        self.status.last_ms = last;
        self.status.last_saved = conn
            .query_row(
                "SELECT path FROM (SELECT path, wall_ms AS t FROM captures UNION ALL
                 SELECT path, end_ms FROM recordings) ORDER BY t DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        self.status.bytes = self.size();
        Ok(())
    }

    pub fn status(&self) -> &DatasetStatus {
        &self.status
    }

    fn touch(&mut self, ms: u64) {
        self.status.first_ms = Some(self.status.first_ms.map_or(ms, |f| f.min(ms)));
        self.status.last_ms = Some(self.status.last_ms.map_or(ms, |l| l.max(ms)));
    }

    /// The database file plus its write-ahead log.
    pub fn size(&self) -> u64 {
        let len = |p: &Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        len(&self.path) + len(&wal_path(&self.path))
    }

    /// Refuse to grow past the cap or to fill the disk.
    fn check_room(&mut self) -> Result<()> {
        self.status.bytes = self.size();
        let label = file_label(&self.path);
        ensure!(
            self.status.bytes < self.status.max_bytes,
            "Storage full: dataset {label} reached its {} byte limit; open a new dataset or raise --max-bytes",
            self.status.max_bytes
        );
        if let Some(parent) = self.path.parent()
            && let Ok(free) = fs2::available_space(parent)
        {
            ensure!(
                free >= FREE_SPACE_FLOOR,
                "Storage full: dataset {label} needs at least 512 MiB free on its disk; free space or move the dataset"
            );
        }
        Ok(())
    }

    /// Catalog finished files in one transaction. Rows already present (same
    /// path and time) are skipped. Returns the number of new rows.
    pub fn record_saved(
        &mut self,
        camera: Option<&SavedCamera>,
        items: &[SavedFile],
    ) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        self.check_room()?;
        let unknown = SavedCamera {
            id: "unknown".into(),
            transport: String::new(),
            vendor: String::new(),
            model: String::new(),
            serial: String::new(),
            source: None,
        };
        let camera = camera.unwrap_or(&unknown);
        let path = self.path.clone();
        let conn = &self.conn;
        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| explain(e, &path, "insert"))?;
        let inserted = (|| -> Result<(usize, usize, Option<(String, u64)>)> {
            let (mut captures, mut recordings, mut last) = (0, 0, None);
            for item in items {
                let name = file_label(Path::new(&item.path));
                let context = context_json(&item.context);
                let changed = match &item.item {
                    SavedItem::Capture {
                        frame_id,
                        timestamp_ns,
                        width,
                        height,
                        pixel_format,
                        format,
                        origin,
                        job_id,
                    } => {
                        let n = conn.execute(
                            "INSERT INTO captures (wall_ms, camera_id, vendor, model, serial, transport, source,
                               frame_id, timestamp_ns, width, height, pixel_format, pixel_format_code, format, bytes,
                               path, file_name, sha256, origin, job_id, destination, object_key, upload_id, remote_url, context)
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                             ON CONFLICT DO NOTHING",
                            params![
                                item.wall_ms, camera.id, camera.vendor, camera.model, camera.serial,
                                camera.transport, camera.source, frame_id, timestamp_ns, width, height,
                                crate::frame::pixel_format_name(*pixel_format), pixel_format, format,
                                item.bytes, item.path, name, item.sha256, origin, job_id, item.destination,
                                item.object_key, item.upload_id, item.remote_url, context
                            ],
                        )?;
                        captures += n;
                        n
                    }
                    SavedItem::Recording {
                        started_ms,
                        ended_ms,
                        codec,
                        encoder,
                        fps,
                    } => {
                        let n = conn.execute(
                            "INSERT INTO recordings (start_ms, end_ms, camera_id, vendor, model, serial, transport,
                               source, path, file_name, bytes, sha256, codec, encoder, fps, destination, object_key,
                               upload_id, remote_url, context)
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                             ON CONFLICT DO NOTHING",
                            params![
                                started_ms, ended_ms, camera.id, camera.vendor, camera.model, camera.serial,
                                camera.transport, camera.source, item.path, name, item.bytes, item.sha256,
                                codec, encoder, fps, item.destination, item.object_key, item.upload_id,
                                item.remote_url, context
                            ],
                        )?;
                        recordings += n;
                        n
                    }
                };
                if changed > 0 {
                    last = Some((item.path.clone(), item.wall_ms));
                }
            }
            Ok((captures, recordings, last))
        })();
        match inserted {
            Ok((captures, recordings, last)) => {
                conn.execute_batch("COMMIT")
                    .map_err(|e| explain(e, &path, "insert"))?;
                self.status.captures += captures as u64;
                self.status.recordings += recordings as u64;
                if let Some((path, ms)) = last {
                    self.status.last_saved = Some(path);
                    self.touch(ms);
                }
                for item in items {
                    if let SavedItem::Recording { started_ms, .. } = item.item {
                        self.touch(started_ms);
                    }
                }
                self.status.bytes = self.size();
                Ok(captures + recordings)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e.context(format!("catalog saved files in {}", file_label(&path))))
            }
        }
    }

    /// Insert or update a source's tags.
    pub fn register(&mut self, source: &SignalInfo, tags: &[TagInfo]) -> Result<()> {
        let protocol = serde_json::to_value(source.protocol)?
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.sources
            .insert(source.id.clone(), (source.uri.clone(), protocol.clone()));
        let path = self.path.clone();
        let conn = &self.conn;
        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| explain(e, &path, "insert"))?;
        let result = (|| -> Result<()> {
            for tag in tags {
                conn.execute(
                    "INSERT INTO signals (source_id, protocol, uri, tag, unit, kind) VALUES (?, ?, ?, ?, ?, ?)
                     ON CONFLICT (source_id, tag) DO UPDATE SET protocol = excluded.protocol, uri = excluded.uri,
                       unit = coalesce(excluded.unit, signals.unit), kind = coalesce(excluded.kind, signals.kind)",
                    params![source.id, protocol, source.uri, tag.tag, tag.unit, tag.kind],
                )?;
            }
            Ok(())
        })();
        self.finish_txn(result)?;
        self.status.signals = self
            .conn
            .query_row("SELECT count(*) FROM signals", [], |r| r.get(0))?;
        Ok(())
    }

    fn finish_txn(&self, result: Result<()>) -> Result<()> {
        match result {
            Ok(()) => self
                .conn
                .execute_batch("COMMIT")
                .map_err(|e| explain(e, &self.path, "insert")),
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Append one source's samples. Tags not registered yet are added.
    pub fn append_samples(&mut self, source_id: &str, batch: &[Sample]) -> Result<usize> {
        ensure!(
            batch.len() <= MAX_SAMPLES_PER_CALL,
            "at most {MAX_SAMPLES_PER_CALL} samples per write"
        );
        if batch.is_empty() {
            return Ok(0);
        }
        self.since_size_check += batch.len();
        if self.since_size_check >= 1000 || self.status.samples == 0 {
            self.since_size_check = 0;
            self.check_room()?;
        }
        let (uri, protocol) = self
            .sources
            .get(source_id)
            .cloned()
            .unwrap_or_else(|| (source_id.to_owned(), String::new()));
        let mut range: BTreeMap<&str, (u64, u64, &'static str)> = BTreeMap::new();
        for sample in batch {
            let kind = match sample.value {
                SignalValue::Bool(_) => "bool",
                SignalValue::Number(_) => "number",
                SignalValue::Text(_) => "text",
            };
            let entry =
                range
                    .entry(sample.tag.as_str())
                    .or_insert((sample.ts_ms, sample.ts_ms, kind));
            entry.0 = entry.0.min(sample.ts_ms);
            entry.1 = entry.1.max(sample.ts_ms);
        }
        let path = self.path.clone();
        let conn = &self.conn;
        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| explain(e, &path, "insert"))?;
        let result = (|| -> Result<()> {
            {
                let mut appender = conn.appender("samples")?;
                for sample in batch {
                    let (double, text) = match &sample.value {
                        SignalValue::Bool(b) => (
                            Some(if *b { 1.0 } else { 0.0 }),
                            Some(if *b { "true" } else { "false" }.to_owned()),
                        ),
                        SignalValue::Number(n) => (Some(*n), None),
                        SignalValue::Text(t) => (None, Some(t.clone())),
                    };
                    let quality = serde_json::to_value(sample.quality)?
                        .as_str()
                        .map(str::to_owned);
                    appender.append_row(params![
                        sample.ts_ms,
                        sample.source_ts_ms,
                        source_id,
                        sample.tag,
                        double,
                        text,
                        quality
                    ])?;
                }
                appender.flush()?;
            }
            for (tag, (first, last, kind)) in &range {
                conn.execute(
                    "INSERT INTO signals (source_id, protocol, uri, tag, kind, first_ms, last_ms)
                     VALUES (?, nullif(?, ''), ?, ?, ?, ?, ?)
                     ON CONFLICT (source_id, tag) DO UPDATE SET
                       kind = coalesce(signals.kind, excluded.kind),
                       first_ms = least(coalesce(signals.first_ms, excluded.first_ms), excluded.first_ms),
                       last_ms = greatest(coalesce(signals.last_ms, excluded.last_ms), excluded.last_ms)",
                    params![source_id, protocol, uri, tag, kind, first, last],
                )?;
            }
            Ok(())
        })();
        self.finish_txn(result)?;
        self.status.samples += batch.len() as u64;
        if let (Some(first), Some(last)) = (
            range.values().map(|r| r.0).min(),
            range.values().map(|r| r.1).max(),
        ) {
            self.touch(first);
            self.touch(last);
        }
        self.status.signals = self
            .conn
            .query_row("SELECT count(*) FROM signals", [], |r| r.get(0))?;
        Ok(batch.len())
    }

    pub fn record_event(
        &mut self,
        kind: &str,
        camera_id: Option<&str>,
        source_id: Option<&str>,
        text: &str,
        context: &Context,
    ) -> Result<u64> {
        let wall_ms = now_ms();
        let id: u64 = self
            .conn
            .query_row(
                "INSERT INTO events (wall_ms, kind, camera_id, source_id, text, context)
                 VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
                params![
                    wall_ms,
                    kind,
                    camera_id,
                    source_id,
                    text,
                    context_json(context)
                ],
                |r| r.get(0),
            )
            .map_err(|e| explain(e, &self.path, "insert"))?;
        self.status.events += 1;
        Ok(id)
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .execute_batch("CHECKPOINT")
            .map_err(|e| explain(e, &self.path, "checkpoint"))
    }
}

fn read_info(conn: &Connection) -> Result<(String, String, u64, u64)> {
    let (version, id, name, created, max): (u32, String, String, u64, u64) = conn.query_row(
        "SELECT schema_version, dataset_id, name, created_ms, max_bytes FROM dataset_info WHERE singleton = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    ensure!(
        version <= SCHEMA_VERSION,
        "dataset schema version {version} is newer than this Capturefab"
    );
    Ok((id, name, created, max))
}

fn context_json(context: &Context) -> Option<String> {
    (!context.is_empty()).then(|| serde_json::to_string(context).unwrap_or_default())
}

pub(crate) fn wal_path(path: &Path) -> PathBuf {
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    PathBuf::from(wal)
}
