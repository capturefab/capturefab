//! Analysis bundles: DuckDB writes each table into the private staging
//! folder, then every file is copied into a new export folder through the
//! storage ledger, with `manifest.json` and `README.md` beside them. With
//! `media`, the cataloged images (and recordings) are copied in as well.
pub(super) mod bounded;
mod media;
use super::{
    ExportFormat, Reader, TABLES,
    db::{file_label, now_ms},
    query::with_timeout,
};
use crate::storage::StoragePolicy;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

/// The advice a storage-full export gives when only its media copies do not
/// fit. The desktop app replaces it with its own checkbox's name.
pub(crate) const EXPORT_WITHOUT_MEDIA: &str = "export without them (--media none)";

/// Room reserved in the storage budget for manifest.json and README.md.
const METADATA_RESERVE: u64 = 64 * 1024;
/// The "Summary for AI tools" section stays under this many bytes.
pub const SUMMARY_BYTES: usize = 8 * 1024;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
const EXPORT_TIMEOUT: Duration = Duration::from_secs(570);

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub output: PathBuf,
    pub formats: Vec<ExportFormat>,
    pub tables: Vec<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub storage: StoragePolicy,
    /// Copy the saved files into `images/` and `videos/`.
    pub media: super::MediaExport,
    /// With `media`, one folder per value of this capture tag.
    pub group_by: Option<String>,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Parquet => "parquet",
            Self::Json => "json",
            Self::Jsonl => "jsonl",
        }
    }
    fn options(self) -> &'static str {
        match self {
            Self::Csv => "FORMAT csv, HEADER",
            Self::Parquet => "FORMAT parquet, COMPRESSION zstd",
            Self::Json => "FORMAT json, ARRAY true",
            Self::Jsonl => "FORMAT json",
        }
    }
    pub fn supported(self) -> bool {
        self == Self::Csv || cfg!(feature = "parquet")
    }
    /// Parquet and CSV when the build has Parquet, otherwise CSV; duplicates removed.
    pub fn or_default(formats: Vec<Self>) -> Vec<Self> {
        let mut out = Vec::new();
        for format in formats {
            if !out.contains(&format) {
                out.push(format);
            }
        }
        if out.is_empty() {
            if cfg!(feature = "parquet") {
                out.push(Self::Parquet);
            }
            out.push(Self::Csv);
        }
        out
    }
}

/// The time column each table is filtered on.
fn time_column(table: &str) -> Option<&'static str> {
    match table {
        "captures" | "events" => Some("wall_ms"),
        "recordings" => Some("start_ms"),
        _ => None,
    }
}

fn describe_table(table: &str) -> &'static str {
    match table {
        "captures" => {
            "One row per saved image, with camera identity, frame metadata, file path, SHA-256 and capture context."
        }
        "recordings" => "One row per finished local video recording.",
        "events" => "Notes, dataset open/close and data gaps.",
        _ => "",
    }
}

fn describe_column(name: &str) -> &'static str {
    match name {
        "id" | "capture_id" => "Row identifier within this dataset",
        "wall_ms" => {
            "UTC Unix milliseconds, host clock, when the frame was received or the event happened"
        }
        "start_ms" | "end_ms" => "Recording start or end, UTC Unix milliseconds",
        "camera_id" => "Camera identifier",
        "path" => "Absolute path of the saved file on the capturing computer",
        "file_name" => "File name of the saved file",
        "sha256" => "SHA-256 of the file as written",
        "bytes" => "File size in bytes",
        "object_key" => "Legacy remote object key from a catalogue created by an older version",
        "context" => "Capture context (line, station, part, lot, note) as JSON text",
        "frame_id" => "Camera frame counter",
        "timestamp_ns" => "Camera or transport timestamp in nanoseconds (device clock)",
        "vendor" | "model" | "serial" => "Camera vendor, model or serial number, as reported",
        "transport" => "How the camera was reached, such as gige, media or simulator",
        "source" => "Camera address or stream URL, with credentials removed",
        "width" | "height" => "Frame size in pixels",
        "pixel_format" | "pixel_format_code" => "Camera pixel format, by name or GenICam code",
        "format" => "Saved file format (png, jpeg, raw, ...)",
        "origin" => "capture (on request) or schedule (a scheduled job)",
        "job_id" => "Scheduled capture job, for origin = schedule",
        "codec" | "encoder" | "fps" => "Video codec, FFmpeg encoder and frame rate",
        "destination" => "Saved destination name, when one was used",
        "upload_id" | "remote_url" | "uploaded_ms" => {
            "Legacy transfer metadata retained from older catalogues; not used for new saves"
        }
        "kind" => "Event kind",
        "first_ms" | "last_ms" => "First and last capture, UTC Unix milliseconds",
        "text" => "Event text, such as a note",
        _ => "",
    }
}

struct Staged {
    table: String,
    format: ExportFormat,
    staged: PathBuf,
    rows: u64,
    bytes: u64,
}

impl Reader {
    /// Write the requested tables in each format into a new folder.
    pub fn export(&self, request: &ExportRequest) -> Result<Value> {
        let formats = ExportFormat::or_default(request.formats.clone());
        for format in &formats {
            ensure!(
                format.supported(),
                "{} export unsupported in this build",
                format.extension()
            );
        }
        let output = &request.output;
        ensure!(output.is_absolute(), "export folder must be absolute");
        ensure!(
            !output.exists(),
            "export folder {} already exists; choose a new folder",
            output.display()
        );
        if let Some(parent) = output.parent() {
            crate::volumes::ensure_present(parent)?;
        }
        request.storage.validate()?;
        // One snapshot for every file.
        self.conn
            .execute_batch("BEGIN TRANSACTION")
            .map_err(|e| super::db::explain(e, &self.path, "export"))?;
        let (staged, fired) =
            with_timeout(&self.conn, EXPORT_TIMEOUT, || self.stage(request, &formats));
        let metadata = staged.as_ref().ok().map(|_| self.metadata(request));
        let media_rows = staged
            .as_ref()
            .ok()
            .map(|_| self.export_source().media_rows(request));
        let _ = self.conn.execute_batch("ROLLBACK");
        let staged = match staged {
            Err(_) if fired => {
                bail!("dataset export timed out; narrow --since/--until or --tables")
            }
            other => other?,
        };
        // Remove this export's staged files however it ends.
        struct Cleanup<'a>(&'a [Staged]);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                for item in self.0 {
                    let _ = fs::remove_file(&item.staged);
                }
            }
        }
        let _cleanup = Cleanup(&staged);
        let metadata = metadata.unwrap_or_else(|| Ok(Metadata::default()))?;
        let media_rows = media_rows.unwrap_or_else(|| Ok(Vec::new()))?;
        let extensions: Vec<&'static str> = formats.iter().map(|f| f.extension()).collect();
        let mut media = (!request.media.is_none())
            .then(|| media::Plan::new(media_rows, request.group_by.as_deref(), &extensions));
        let extra = media.as_ref().map_or((0, 0), media::Plan::needs);
        self.preflight(&staged, extra, &request.storage)?;
        fs::create_dir_all(output.parent().context("export folder needs a parent")?)?;
        fs::create_dir(output).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => anyhow!(
                "export folder {} already exists; choose a new folder",
                output.display()
            ),
            _ => anyhow!("cannot create export folder {}: {e}", output.display()),
        })?;
        let mut files = Vec::new();
        for item in &staged {
            let name = format!("{}.{}", item.table, item.format.extension());
            let result = copy_into_ledger(&item.staged, &output.join(&name), &request.storage);
            let _ = fs::remove_file(&item.staged);
            match result {
                Ok((bytes, sha256)) => files.push(json!({
                    "path": name, "table": item.table, "format": item.format,
                    "rows": item.rows, "bytes": bytes, "sha256": sha256,
                })),
                Err(e) => {
                    let written: Vec<_> = files.iter().map(|f| f["path"].clone()).collect();
                    return Err(e.context(format!(
                        "export stopped; files already written stay in {}: {}",
                        output.display(),
                        serde_json::to_string(&written).unwrap_or_default()
                    )));
                }
            }
        }
        let stopped = |e: anyhow::Error, files: &[Value]| {
            let written: Vec<_> = files.iter().map(|f| f["path"].clone()).collect();
            e.context(format!(
                "export stopped; files already written stay in {}: {}",
                output.display(),
                serde_json::to_string(&written).unwrap_or_default()
            ))
        };
        let media = match media.as_mut() {
            Some(plan) => {
                let copied = plan
                    .copy(output, &request.storage)
                    .map_err(|e| stopped(e, &files))?;
                let mut index = Vec::new();
                for format in plan.index_formats() {
                    let written = self
                        .media_index(output, format, &copied.rows, &request.storage)
                        .map_err(|e| stopped(e, &files))?;
                    index.push(written.clone());
                    files.push(written);
                }
                let mut summary = plan.summary(request, &copied);
                summary["index"] = json!(index);
                Some(summary)
            }
            None => None,
        };
        let mut tables = BTreeMap::new();
        for item in &staged {
            tables.insert(item.table.clone(), item.rows);
        }
        let mut manifest = self.manifest(request, &formats, &files, &metadata);
        if let Some(media) = &media {
            manifest["media"] = media.clone();
        }
        let readme = self.readme(&manifest, &metadata);
        for (name, text) in [
            ("manifest.json", serde_json::to_string_pretty(&manifest)?),
            ("README.md", readme),
        ] {
            let (bytes, sha256) = write_new(&output.join(name), text.as_bytes(), &request.storage)?;
            files.push(json!({"path": name, "bytes": bytes, "sha256": sha256}));
        }
        let mut result = json!({
            "output": output,
            "files": files,
            "tables": tables,
            "formats": formats,
        });
        if let Some(mut media) = media {
            if let Some(media) = media.as_object_mut() {
                media.remove("index");
            }
            result["media"] = media;
        }
        Ok(result)
    }

    /// Write `media.<format>` for the copied files; returns its file entry.
    fn media_index(
        &self,
        output: &Path,
        format: &str,
        rows: &[Value],
        policy: &StoragePolicy,
    ) -> Result<Value> {
        let name = format!("media.{format}");
        let (bytes, sha256) = match format {
            "json" | "jsonl" => write_new(
                &output.join(&name),
                media::json_text(rows, format == "jsonl")?.as_bytes(),
                policy,
            )?,
            "parquet" => {
                // DuckDB turns a staged CSV into Parquet in the staging folder.
                let prefix = super::db::random_name()?;
                let csv = self.staging.dir.join(format!("{prefix}-media.csv"));
                let parquet = self.staging.dir.join(format!("{prefix}-media.parquet"));
                let result = (|| {
                    fs::write(&csv, media::csv(rows))?;
                    let dir = self.staging.sql();
                    self.conn
                        .execute(
                            &format!(
                                "COPY ({}) TO '{dir}/{prefix}-media.parquet' ({})",
                                media::parquet_select(&format!("{dir}/{prefix}-media.csv")),
                                ExportFormat::Parquet.options()
                            ),
                            [],
                        )
                        .map_err(|e| super::db::explain(e, &self.path, "export"))?;
                    copy_into_ledger(&parquet, &output.join(&name), policy)
                })();
                let _ = fs::remove_file(&csv);
                let _ = fs::remove_file(&parquet);
                result?
            }
            _ => write_new(&output.join(&name), media::csv(rows).as_bytes(), policy)?,
        };
        Ok(json!({
            "path": name, "table": "media", "format": format,
            "rows": rows.len(), "bytes": bytes, "sha256": sha256,
        }))
    }

    fn export_source(&self) -> ExportSource<'_> {
        ExportSource {
            conn: &self.conn,
            path: &self.path,
            status: &self.status,
            cancelled: &self.cancelled,
        }
    }
    fn export_select(&self, table: &str, request: &ExportRequest) -> Result<Option<String>> {
        self.export_source().export_select(table, request)
    }
    fn metadata(&self, request: &ExportRequest) -> Result<Metadata> {
        self.export_source().metadata(request)
    }
    fn manifest(
        &self,
        request: &ExportRequest,
        formats: &[ExportFormat],
        files: &[Value],
        meta: &Metadata,
    ) -> Value {
        self.export_source().manifest(request, formats, files, meta)
    }
    fn readme(&self, manifest: &Value, meta: &Metadata) -> String {
        self.export_source().readme(manifest, meta)
    }

    fn stage(&self, request: &ExportRequest, formats: &[ExportFormat]) -> Result<Vec<Staged>> {
        let tables: Vec<&str> = if request.tables.is_empty() {
            TABLES.to_vec()
        } else {
            TABLES
                .iter()
                .copied()
                .filter(|t| request.tables.iter().any(|r| r == t))
                .collect()
        };
        let mut staged = Vec::new();
        let result = self.stage_into(request, formats, tables, &mut staged);
        if result.is_err() {
            for item in &staged {
                let _ = fs::remove_file(&item.staged);
            }
        }
        result.map(|()| staged)
    }

    fn stage_into(
        &self,
        request: &ExportRequest,
        formats: &[ExportFormat],
        tables: Vec<&str>,
        staged: &mut Vec<Staged>,
    ) -> Result<()> {
        let dir = self.staging.sql();
        // Exports of one dataset share its staging folder and may overlap,
        // so each export's files carry their own prefix.
        let prefix = super::db::random_name()?;
        for table in tables {
            let Some(select) = self.export_select(table, request)? else {
                continue;
            };
            for format in formats {
                let file = format!("{prefix}-{table}.{}", format.extension());
                let target = format!("{dir}/{file}");
                let rows = self
                    .conn
                    .execute(
                        &format!("COPY ({select}) TO '{target}' ({})", format.options()),
                        [],
                    )
                    .map_err(|e| super::db::explain(e, &self.path, "export"))?;
                let staged_path = self.staging.dir.join(&file);
                let bytes = fs::metadata(&staged_path)
                    .with_context(|| format!("staged export {file} is missing"))?
                    .len();
                staged.push(Staged {
                    table: table.into(),
                    format: *format,
                    staged: staged_path,
                    rows: rows as u64,
                    bytes,
                });
            }
        }
        Ok(())
    }

    /// Fail before writing anything when the storage budget cannot hold the
    /// export; retention never evicts captures to make room for one.
    /// `extra` is the bytes and files the copied media and its index need.
    fn preflight(
        &self,
        staged: &[Staged],
        extra: (u64, u64),
        policy: &StoragePolicy,
    ) -> Result<()> {
        let data: u64 = staged.iter().map(|s| s.bytes).sum::<u64>() + METADATA_RESERVE;
        let data_files = staged.len() as u64 + 2;
        let status = crate::storage::quota_status()?;
        let configured: StoragePolicy =
            serde_json::from_value(status["policy"].clone()).unwrap_or_default();
        let max_bytes = policy.max_bytes.min(configured.max_bytes);
        let max_files = u64::from(policy.max_files.min(configured.max_files));
        let allocated = status["allocated_bytes"].as_u64().unwrap_or(0);
        let files = status["files"].as_u64().unwrap_or(0);
        match over_budget(
            (
                max_bytes.saturating_sub(allocated),
                max_files.saturating_sub(files),
            ),
            (data, data_files),
            extra,
        ) {
            Some(message) => Err(anyhow!(message)),
            None => Ok(()),
        }
    }
}

/// Why an export of `data` (bytes, files) plus its media copies `extra`
/// does not fit in the `room` the storage budget has left, or `None`.
pub(super) fn over_budget(room: (u64, u64), data: (u64, u64), extra: (u64, u64)) -> Option<String> {
    let fits = |bytes: u64, count: u64| bytes <= room.0 && count <= room.1;
    if fits(data.0.saturating_add(extra.0), data.1 + extra.1) {
        return None;
    }
    let left = format!(
        "the storage budget has {} and {} files left",
        crate::tools::format_bytes(room.0),
        room.1
    );
    // Name the copies when they alone tip the export over the budget,
    // since leaving them out is the cheapest way to make it fit.
    Some(if extra != (0, 0) && fits(data.0, data.1) {
        format!(
            "Storage full: copying the images needs {} and {} files, but {left}; {EXPORT_WITHOUT_MEDIA}, raise --max-space or --max-files, or remove old captures",
            crate::tools::format_bytes(extra.0),
            extra.1
        )
    } else {
        format!(
            "Storage full: export needs {} and {} files, but {left}; raise --max-space or --max-files, or remove old captures",
            crate::tools::format_bytes(data.0.saturating_add(extra.0)),
            data.1 + extra.1
        )
    })
}

/// Copy a staged file into a new ledger-tracked file; returns size and SHA-256.
fn copy_into_ledger(from: &Path, to: &Path, policy: &StoragePolicy) -> Result<(u64, String)> {
    let len = fs::metadata(from)?.len();
    let mut writer = crate::storage::create_writer(to, policy, len.max(1))?;
    let mut source = fs::File::open(from)?;
    if let Err(e) = std::io::copy(&mut source, &mut writer) {
        let _ = writer.abort();
        return Err(anyhow!(e).context(format!("write {}", to.display())));
    }
    let result = (writer.bytes_written(), writer.sha256());
    writer.finish()?;
    Ok(result)
}

fn write_new(to: &Path, bytes: &[u8], policy: &StoragePolicy) -> Result<(u64, String)> {
    let mut writer = crate::storage::create_writer(to, policy, (bytes.len() as u64).max(1))?;
    if let Err(e) = writer.write_all(bytes) {
        let _ = writer.abort();
        return Err(anyhow!(e).context(format!("write {}", to.display())));
    }
    let result = (writer.bytes_written(), writer.sha256());
    writer.finish()?;
    Ok(result)
}

/// Facts gathered inside the export's snapshot for the manifest and README.
#[derive(Default)]
struct Metadata {
    columns: BTreeMap<String, Vec<(String, String)>>,
    cameras: Vec<Value>,
    notes: Vec<(u64, String)>,
    /// Capture context key -> value -> captures.
    contexts: BTreeMap<String, BTreeMap<String, u64>>,
    counts: BTreeMap<String, u64>,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
}

/// Borrowed only for the duration of a checked export. The connection is never
/// cloned or exposed by this adapter, including for private task engines.
pub(super) struct ExportSource<'a> {
    pub conn: &'a duckdb::Connection,
    pub path: &'a Path,
    pub status: &'a super::DatasetStatus,
    pub cancelled: &'a std::sync::atomic::AtomicBool,
}
impl ExportSource<'_> {
    pub(super) fn check_cancelled(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(std::sync::atomic::Ordering::Acquire),
            "dataset export cancelled"
        );
        Ok(())
    }
    pub(super) fn metadata_text(
        &self,
        request: &ExportRequest,
        formats: &[ExportFormat],
        files: &[Value],
    ) -> Result<Vec<(&'static str, String)>> {
        let metadata = self.metadata(request)?;
        let manifest = self.manifest(request, formats, files, &metadata);
        let readme = self.readme(&manifest, &metadata);
        Ok(vec![
            ("manifest.json", serde_json::to_string_pretty(&manifest)?),
            ("README.md", readme),
        ])
    }
    fn filter(&self, table: &str, request: &ExportRequest) -> String {
        let Some(column) = time_column(table) else {
            return String::new();
        };
        let mut clauses = Vec::new();
        if let Some(since) = request.since_ms {
            clauses.push(format!("{column} >= {since}"));
        }
        if let Some(until) = request.until_ms {
            clauses.push(format!("{column} <= {until}"));
        }
        if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        }
    }

    pub(super) fn export_select(
        &self,
        table: &str,
        request: &ExportRequest,
    ) -> Result<Option<String>> {
        let order = " ORDER BY 1";
        Ok(Some(format!(
            "SELECT * FROM {table}{}{order}",
            self.filter(table, request)
        )))
    }

    fn metadata(&self, request: &ExportRequest) -> Result<Metadata> {
        let conn = &self.conn;
        let mut meta = Metadata::default();
        for table in &TABLES {
            let mut statement = conn.prepare(&format!("DESCRIBE {table}"))?;
            let columns = statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<duckdb::Result<Vec<_>>>()?;
            meta.columns.insert(table.to_string(), columns);
            {
                let count: u64 = conn.query_row(
                    &format!(
                        "SELECT count(*) FROM {table}{}",
                        self.filter(table, request)
                    ),
                    [],
                    |r| r.get(0),
                )?;
                meta.counts.insert(table.to_string(), count);
            }
        }
        let mut statement = conn.prepare(&format!(
            "SELECT camera_id, any_value(vendor), any_value(model), any_value(serial), any_value(transport),
                    count(*), min(wall_ms), max(wall_ms)
             FROM captures{} GROUP BY camera_id ORDER BY camera_id",
            self.filter("captures", request)
        ))?;
        meta.cameras = statement
            .query_map([], |r| {
                Ok(json!({
                    "camera_id": r.get::<_, String>(0)?,
                    "vendor": r.get::<_, Option<String>>(1)?,
                    "model": r.get::<_, Option<String>>(2)?,
                    "serial": r.get::<_, Option<String>>(3)?,
                    "transport": r.get::<_, Option<String>>(4)?,
                    "captures": r.get::<_, u64>(5)?,
                    "first_ms": r.get::<_, Option<u64>>(6)?,
                    "last_ms": r.get::<_, Option<u64>>(7)?,
                }))
            })?
            .collect::<duckdb::Result<_>>()?;
        let mut statement = conn.prepare(&format!(
            "SELECT context, count(*) FROM captures{} GROUP BY context ORDER BY 2 DESC LIMIT 1000",
            self.filter("captures", request)
        ))?;
        let contexts: Vec<(Option<String>, u64)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<duckdb::Result<_>>()?;
        for (context, count) in contexts {
            let Some(Value::Object(fields)) = context.and_then(|c| serde_json::from_str(&c).ok())
            else {
                continue;
            };
            for (key, value) in fields {
                let value = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned);
                *meta
                    .contexts
                    .entry(key)
                    .or_default()
                    .entry(value)
                    .or_default() += count;
            }
        }
        let mut statement = conn.prepare(&format!(
            "SELECT wall_ms, text FROM (SELECT wall_ms, text FROM events{} {} kind = 'note'
               ORDER BY wall_ms DESC LIMIT 20) ORDER BY wall_ms",
            self.filter("events", request),
            if request.since_ms.is_some() || request.until_ms.is_some() {
                "AND"
            } else {
                "WHERE"
            }
        ))?;
        meta.notes = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<duckdb::Result<_>>()?;
        let (first, last): (Option<u64>, Option<u64>) = conn.query_row(
            &format!(
                "SELECT min(t), max(t) FROM (SELECT wall_ms AS t FROM captures{} UNION ALL
                 SELECT start_ms FROM recordings{})",
                self.filter("captures", request),
                self.filter("recordings", request)
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        meta.first_ms = first;
        meta.last_ms = last;
        Ok(meta)
    }

    fn manifest(
        &self,
        request: &ExportRequest,
        formats: &[ExportFormat],
        files: &[Value],
        meta: &Metadata,
    ) -> Value {
        let mut tables = Vec::new();
        for table in TABLES {
            let table_files: Vec<Value> = files
                .iter()
                .filter(|f| f["table"] == table)
                .map(|f| json!({"format": f["format"], "path": f["path"], "bytes": f["bytes"], "sha256": f["sha256"]}))
                .collect();
            if table_files.is_empty() {
                continue;
            }
            let rows = files
                .iter()
                .find(|f| f["table"] == table)
                .map(|f| f["rows"].clone())
                .unwrap_or(Value::Null);
            let columns: Vec<Value> = meta.columns.get(table).into_iter().flatten()
                .map(|(name, kind)| json!({"name":name,"type":kind,"description":describe_column(name)}))
                .collect();
            tables.push(json!({
                "name": table,
                "rows": rows,
                "description": describe_table(table),
                "time_column": time_column(table),
                "columns": columns,
                "files": table_files,
            }));
        }
        json!({
            "format_version": 1,
            "generator": format!("capturefab {}", env!("CARGO_PKG_VERSION")),
            "dataset_id": self.status.dataset_id,
            "slot_id": self.status.slot_id,
            "name": self.status.name,
            "source_file": file_label(self.path),
            "exported_ms": now_ms(),
            "time_base": "UTC Unix milliseconds, host clock",
            "filters": {"since_ms": request.since_ms, "until_ms": request.until_ms, "tables": request.tables},
            "formats": formats,
            "counts": meta.counts,
            "first_ms": meta.first_ms,
            "last_ms": meta.last_ms,
            "tables": tables,
            "cameras": meta.cameras,
        })
    }

    fn readme(&self, manifest: &Value, meta: &Metadata) -> String {
        let mut out = String::new();
        let name = manifest["name"].as_str().unwrap_or("dataset");
        let iso = |ms: Option<u64>| {
            ms.and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_else(|| "none".into())
        };
        out.push_str(&format!("# {name}: Capturefab dataset export\n\n"));
        let media = manifest.get("media").filter(|m| m.is_object());
        out.push_str(match media {
            Some(_) => {
                "Images and recordings captured with Capturefab, exported for analysis.\n\
                 The files below describe what was saved and when, and the saved files themselves are copied\n\
                 into `images/` (and `videos/`) in this folder.\n\n"
            }
            None => {
                "Images and recordings captured with Capturefab, exported for analysis.\n\
                 The files below describe what was saved and when; the images and videos themselves stay at the\n\
                 paths listed in `captures.path` and `recordings.path`.\n\n"
            }
        });
        out.push_str("## Time base\n\nEvery `*_ms` column is UTC Unix milliseconds from the capturing computer's clock. \
            `timestamp_ns` and `source_ts_ms` come from device clocks and may differ.\n\n");
        out.push_str("## Files\n\n| File | Table | Rows | Bytes |\n|---|---|---|---|\n");
        for table in manifest["tables"].as_array().into_iter().flatten() {
            for file in table["files"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "| `{}` | {} | {} | {} |\n",
                    file["path"].as_str().unwrap_or(""),
                    table["name"].as_str().unwrap_or(""),
                    table["rows"],
                    file["bytes"]
                ));
            }
        }
        out.push_str("\n`manifest.json` lists every column with its type and description, and the SHA-256 of every file.\n\n");
        out.push_str("## Tables\n\n");
        for table in manifest["tables"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "- **{}**: {}\n",
                table["name"].as_str().unwrap_or(""),
                table["description"].as_str().unwrap_or("")
            ));
        }
        let mut pandas = String::from("import pandas as pd\n");
        let mut polars = String::from("import polars as pl\n");
        let mut sql = String::new();
        for table in manifest["tables"].as_array().into_iter().flatten() {
            // Examples must follow the actual selected tables and formats.
            // A notes-only or JSON-only bundle has no captures.csv to load.
            let file = ["parquet", "csv", "json", "jsonl"]
                .iter()
                .find_map(|format| {
                    table["files"]
                        .as_array()?
                        .iter()
                        .find(|f| f["format"] == *format)
                });
            let Some(file) = file else { continue };
            let name = table["name"].as_str().unwrap_or("data");
            let path = file["path"].as_str().unwrap_or("");
            let format = file["format"].as_str().unwrap_or("csv");
            let variable = name;
            let (pandas_reader, polars_reader, duck_reader, args) = match format {
                "parquet" => ("read_parquet", "read_parquet", "read_parquet", ""),
                "json" => ("read_json", "read_json", "read_json_auto", ""),
                "jsonl" => ("read_json", "read_ndjson", "read_json_auto", ", lines=True"),
                _ => ("read_csv", "read_csv", "read_csv_auto", ""),
            };
            pandas.push_str(&format!(
                "{variable} = pd.{pandas_reader}('{path}'{args})\n"
            ));
            polars.push_str(&format!("{variable} = pl.{polars_reader}('{path}')\n"));
            sql.push_str(&format!(
                "SELECT * FROM {duck_reader}('{path}') LIMIT 10;\n"
            ));
        }
        if media.is_some() {
            pandas.push_str("media = pd.read_csv('media.csv')\n");
            if manifest["tables"]
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t["name"] == "captures"))
            {
                pandas.push_str(
                    "images = captures.merge(media[media.table == 'captures'], on='id', suffixes=('', '_export'))\n",
                );
            }
            polars.push_str("media = pl.read_csv('media.csv')\n");
            sql.push_str("SELECT * FROM read_csv_auto('media.csv') LIMIT 10;\n");
        }
        out.push_str(&format!("\n## Loading\n\npandas:\n\n```python\n{pandas}```\n\nDuckDB:\n\n```sql\n{sql}```\n\npolars:\n\n```python\n{polars}```\n\n"));
        match media {
            Some(media) => {
                let videos = media["mode"] == "all";
                let layout = match media["group_by"].as_str() {
                    Some(key) => format!(
                        "`images/<{key}>/<file>`{}, one folder per value of the `{key}` tag; files without it are in `images/{}/`",
                        if videos {
                            format!(" and `videos/<{key}>/<file>`")
                        } else {
                            String::new()
                        },
                        media::UNLABELED
                    ),
                    None if videos => "`images/<file>` and `videos/<file>`".into(),
                    None => "`images/<file>`".into(),
                };
                out.push_str(&format!(
                    "## Images and videos\n\n{} files ({} bytes) are copied into this folder as {layout}. \
                     `media.csv` lists each copy's `export_path` (relative, with forward slashes) and SHA-256, \
                     and joins to `captures` or `recordings` on `table` and `id`. The folder can be moved or zipped \
                     as a whole.\n\n",
                    media["copied"], media["bytes"]
                ));
                if media["skipped"].as_u64().is_some_and(|n| n > 0) {
                    out.push_str(&format!(
                        "{} cataloged files were not copied because they were missing or had changed since capture; \
                         `manifest.json` lists them under `media.skipped_files`.\n\n",
                        media["skipped"]
                    ));
                }
                out.push_str(
                    "## Notes on paths\n\n`captures.path` and `recordings.path` still hold the absolute paths on the capturing computer; use `media.csv` for the copies here.\n\n",
                );
            }
            None => out.push_str(
                "## Notes on paths\n\nMedia paths are absolute paths on the capturing computer. Files are saved on the local or selected mounted drive. \
                 Re-run with `--media images` to copy the files into this folder.\n\n",
            ),
        }
        out.push_str(&self.summary(meta, &iso));
        out
    }

    /// The "Summary for AI tools" section, at most `SUMMARY_BYTES`.
    fn summary(&self, meta: &Metadata, iso: &dyn Fn(Option<u64>) -> String) -> String {
        let mut text = String::from("## Summary for AI tools\n\n");
        text.push_str(&format!(
            "Time range: {} to {}.\nCounts: {}.\n\n",
            iso(meta.first_ms),
            iso(meta.last_ms),
            meta.counts
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let mut lines = Vec::new();
        for camera in &meta.cameras {
            lines.push(format!(
                "- camera {} ({} {}): {} captures",
                camera["camera_id"].as_str().unwrap_or(""),
                camera["vendor"].as_str().unwrap_or(""),
                camera["model"].as_str().unwrap_or(""),
                camera["captures"]
            ));
        }
        for (key, values) in &meta.contexts {
            let mut values: Vec<(&String, &u64)> = values.iter().collect();
            values.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let shown: Vec<String> = values
                .iter()
                .take(10)
                .map(|(value, count)| {
                    let value: String = value.chars().take(60).collect();
                    format!("{} ({count})", value.replace(['\n', '\r'], " "))
                })
                .collect();
            let more = values.len().saturating_sub(shown.len());
            lines.push(format!(
                "- context {key}: {}{}",
                shown.join(", "),
                if more > 0 {
                    format!(", and {more} more")
                } else {
                    String::new()
                }
            ));
        }
        for (ms, note) in &meta.notes {
            let mut note: String = note.chars().take(200).collect();
            note = note.replace(['\n', '\r'], " ").replace('"', "'");
            lines.push(format!("- note at {}: \"{note}\"", iso(Some(*ms))));
        }
        let footer = "\n(Further lines omitted to keep this summary short; see manifest.json.)\n";
        for line in lines {
            if text.len() + line.len() + 1 + footer.len() > SUMMARY_BYTES {
                text.push_str(footer);
                return text;
            }
            text.push_str(&line);
            text.push('\n');
        }
        text
    }
}
