//! Analysis bundles: DuckDB writes each table into the private staging
//! folder, then every file is copied into a new export folder through the
//! storage ledger, with `manifest.json` and `README.md` beside them.
use super::{
    ExportFormat, Reader, TABLES,
    db::{file_label, now_ms, sql_text},
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

/// Room reserved in the storage budget for manifest.json and README.md.
const METADATA_RESERVE: u64 = 64 * 1024;
/// The "Summary for AI tools" section stays under this many bytes.
pub const SUMMARY_BYTES: usize = 8 * 1024;
/// `captures_wide` is written only for 1..=256 distinct tags.
pub const WIDE_TAGS: usize = 256;
const EXPORT_TIMEOUT: Duration = Duration::from_secs(570);

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub output: PathBuf,
    pub formats: Vec<ExportFormat>,
    pub tables: Vec<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub storage: StoragePolicy,
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
        "captures" | "events" | "captures_with_signals" | "captures_wide" => Some("wall_ms"),
        "recordings" => Some("start_ms"),
        "samples" => Some("ts_ms"),
        _ => None,
    }
}

fn describe_table(table: &str) -> &'static str {
    match table {
        "captures" => {
            "One row per saved image, with camera identity, frame metadata, file path, SHA-256 and capture context."
        }
        "recordings" => "One row per finished local video recording.",
        "signals" => "One row per sensor or PLC tag: source, redacted URI, unit and kind.",
        "samples" => "Every logged signal sample. ts_ms is the host clock when the value was read.",
        "events" => "Notes, dataset open/close, data gaps and signal errors.",
        "captures_with_signals" => {
            "Each capture joined with the latest sample of every tag at or before the capture time (ASOF join). sample_age_ms is how old that sample was."
        }
        "captures_wide" => {
            "One row per capture with one column per tag (latest numeric value at or before capture time)."
        }
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
        "ts_ms" | "sample_ms" => "UTC Unix milliseconds, host clock, when the value was read",
        "source_ts_ms" => "The device's own timestamp, when the protocol reports one",
        "sample_age_ms" => "wall_ms minus sample_ms",
        "camera_id" => "Camera identifier",
        "path" => "Absolute path of the saved file on the capturing computer",
        "file_name" => "File name of the saved file",
        "sha256" => "SHA-256 of the file as written",
        "bytes" => "File size in bytes",
        "object_key" => {
            "Remote object key when the file was queued for upload; the file may then live only on the remote"
        }
        "context" => "Capture context (line, station, part, lot, note) as JSON text",
        "frame_id" => "Camera frame counter",
        "timestamp_ns" => "Camera or transport timestamp in nanoseconds (device clock)",
        "value_double" => "Numeric value; booleans are 0 or 1",
        "value_text" => "Text value; booleans are 'true' or 'false'",
        "quality" => "good, uncertain or bad",
        "unit" => "Engineering unit, when known",
        "tag" => "Signal tag name",
        "source_id" => "Signal source identifier (signal:<hex>)",
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
        let _ = self.conn.execute_batch("ROLLBACK");
        let staged = match staged {
            Err(_) if fired => {
                bail!("dataset export timed out; narrow --since/--until or --tables")
            }
            other => other?,
        };
        let metadata = metadata.unwrap_or_else(|| Ok(Metadata::default()))?;
        self.preflight(&staged, &request.storage)?;
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
                    for rest in &staged {
                        let _ = fs::remove_file(&rest.staged);
                    }
                    let written: Vec<_> = files.iter().map(|f| f["path"].clone()).collect();
                    return Err(e.context(format!(
                        "export stopped; files already written stay in {}: {}",
                        output.display(),
                        serde_json::to_string(&written).unwrap_or_default()
                    )));
                }
            }
        }
        let mut tables = BTreeMap::new();
        for item in &staged {
            tables.insert(item.table.clone(), item.rows);
        }
        let manifest = self.manifest(request, &formats, &files, &metadata);
        let readme = self.readme(&manifest, &metadata);
        for (name, text) in [
            ("manifest.json", serde_json::to_string_pretty(&manifest)?),
            ("README.md", readme),
        ] {
            let (bytes, sha256) = write_new(&output.join(name), text.as_bytes(), &request.storage)?;
            files.push(json!({"path": name, "bytes": bytes, "sha256": sha256}));
        }
        Ok(json!({
            "output": output,
            "files": files,
            "tables": tables,
            "formats": formats,
        }))
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

    /// The SELECT for `captures_wide`, or `None` without 1..=256 tags.
    fn wide_select(&self, request: &ExportRequest) -> Result<Option<String>> {
        let pairs: Vec<(String, String)> = {
            let mut statement = self
                .conn
                .prepare("SELECT DISTINCT source_id, tag FROM samples ORDER BY tag, source_id")?;
            statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<duckdb::Result<_>>()?
        };
        if pairs.is_empty() || pairs.len() > WIDE_TAGS {
            return Ok(None);
        }
        let mut seen = BTreeMap::<&str, usize>::new();
        for (_, tag) in &pairs {
            *seen.entry(tag.as_str()).or_default() += 1;
        }
        let repeated = seen.values().any(|count| *count > 1);
        let key = if repeated {
            "source_id || ':' || tag"
        } else {
            "tag"
        };
        let keys: Vec<String> = pairs
            .iter()
            .map(|(source, tag)| {
                let key = if repeated {
                    format!("{source}:{tag}")
                } else {
                    tag.clone()
                };
                format!("'{}'", sql_text(&key))
            })
            .collect();
        Ok(Some(format!(
            "SELECT * FROM (PIVOT (SELECT capture_id, wall_ms, camera_id, path, value_double, {key} AS key
               FROM captures_with_signals{}) ON key IN ({}) USING first(value_double)
               GROUP BY capture_id, wall_ms, camera_id, path) ORDER BY capture_id",
            self.filter("captures_with_signals", request),
            keys.join(", ")
        )))
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
        let dir = self.staging.sql();
        for table in tables {
            let select = if table == "captures_wide" {
                match self.wide_select(request)? {
                    Some(select) => select,
                    None => continue,
                }
            } else {
                let order = match table {
                    "signals" => " ORDER BY source_id, tag",
                    "samples" => " ORDER BY ts_ms",
                    "captures_with_signals" => " ORDER BY capture_id, source_id, tag",
                    _ => " ORDER BY 1",
                };
                format!(
                    "SELECT * FROM {table}{}{order}",
                    self.filter(table, request)
                )
            };
            for format in formats {
                let file = format!("{table}.{}", format.extension());
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
        Ok(staged)
    }

    /// Fail before writing anything when the storage budget cannot hold the
    /// export; retention never evicts captures to make room for one.
    fn preflight(&self, staged: &[Staged], policy: &StoragePolicy) -> Result<()> {
        let needed: u64 = staged.iter().map(|s| s.bytes).sum::<u64>() + METADATA_RESERVE;
        let count = staged.len() as u64 + 2;
        let status = crate::storage::quota_status()?;
        let configured: StoragePolicy =
            serde_json::from_value(status["policy"].clone()).unwrap_or_default();
        let max_bytes = policy.max_bytes.min(configured.max_bytes);
        let max_files = u64::from(policy.max_files.min(configured.max_files));
        let allocated = status["allocated_bytes"].as_u64().unwrap_or(0);
        let files = status["files"].as_u64().unwrap_or(0);
        ensure!(
            allocated.saturating_add(needed) <= max_bytes && files + count <= max_files,
            "Storage full: export needs {needed} bytes and {count} files, but the storage budget has {} bytes and {} files left; raise --max-storage or remove old captures",
            max_bytes.saturating_sub(allocated),
            max_files.saturating_sub(files)
        );
        Ok(())
    }
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
    signals: Vec<Value>,
    notes: Vec<(u64, String)>,
    counts: BTreeMap<String, u64>,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
}

impl Reader {
    fn metadata(&self, request: &ExportRequest) -> Result<Metadata> {
        let conn = &self.conn;
        let mut meta = Metadata::default();
        for table in TABLES.iter().filter(|t| **t != "captures_wide") {
            let mut statement = conn.prepare(&format!("DESCRIBE {table}"))?;
            let columns = statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<duckdb::Result<Vec<_>>>()?;
            meta.columns.insert(table.to_string(), columns);
            if *table != "captures_with_signals" {
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
        let sample_filter = self.filter("samples", request).replace("WHERE", "AND");
        let mut statement = conn.prepare(&format!(
            "SELECT g.source_id, g.uri, g.tag, g.unit, g.kind, count(s.ts_ms), min(s.value_double),
                    max(s.value_double), avg(s.value_double), min(s.ts_ms), max(s.ts_ms)
             FROM signals g LEFT JOIN samples s ON s.source_id = g.source_id AND s.tag = g.tag{sample_filter}
             GROUP BY ALL ORDER BY g.source_id, g.tag"
        ))?;
        meta.signals = statement
            .query_map([], |r| {
                Ok(json!({
                    "source_id": r.get::<_, String>(0)?,
                    "uri": r.get::<_, String>(1)?,
                    "tag": r.get::<_, String>(2)?,
                    "unit": r.get::<_, Option<String>>(3)?,
                    "kind": r.get::<_, Option<String>>(4)?,
                    "samples": r.get::<_, u64>(5)?,
                    "min": r.get::<_, Option<f64>>(6)?,
                    "max": r.get::<_, Option<f64>>(7)?,
                    "mean": r.get::<_, Option<f64>>(8)?,
                    "first_ms": r.get::<_, Option<u64>>(9)?,
                    "last_ms": r.get::<_, Option<u64>>(10)?,
                }))
            })?
            .collect::<duckdb::Result<_>>()?;
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
                 SELECT start_ms FROM recordings{} UNION ALL SELECT ts_ms FROM samples{})",
                self.filter("captures", request),
                self.filter("recordings", request),
                self.filter("samples", request)
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
        let units: BTreeMap<String, String> = meta
            .signals
            .iter()
            .filter_map(|s| {
                Some((
                    s["tag"].as_str()?.to_owned(),
                    s["unit"].as_str()?.to_owned(),
                ))
            })
            .collect();
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
            let columns: Vec<Value> = match meta.columns.get(table) {
                Some(columns) => columns
                    .iter()
                    .map(|(name, kind)| {
                        let mut column = json!({"name": name, "type": kind, "description": describe_column(name)});
                        if name == "value_double" && table == "samples" {
                            column["unit"] = json!("see signals.unit for the tag");
                        }
                        column
                    })
                    .collect(),
                None => {
                    let mut columns = vec![
                        json!({"name": "capture_id", "type": "UBIGINT", "description": describe_column("capture_id")}),
                        json!({"name": "wall_ms", "type": "UBIGINT", "description": describe_column("wall_ms")}),
                        json!({"name": "camera_id", "type": "VARCHAR", "description": describe_column("camera_id")}),
                        json!({"name": "path", "type": "VARCHAR", "description": describe_column("path")}),
                    ];
                    for signal in &meta.signals {
                        if let Some(tag) = signal["tag"].as_str() {
                            let mut column = json!({"name": tag, "type": "DOUBLE", "description": "Latest value at or before wall_ms"});
                            if let Some(unit) = units.get(tag) {
                                column["unit"] = json!(unit);
                            }
                            columns.push(column);
                        }
                    }
                    columns
                }
            };
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
            "name": self.status.name,
            "source_file": file_label(&self.path),
            "exported_ms": now_ms(),
            "time_base": "UTC Unix milliseconds, host clock",
            "filters": {"since_ms": request.since_ms, "until_ms": request.until_ms, "tables": request.tables},
            "formats": formats,
            "counts": meta.counts,
            "first_ms": meta.first_ms,
            "last_ms": meta.last_ms,
            "tables": tables,
            "cameras": meta.cameras,
            "signals": meta.signals,
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
        out.push_str(
            "Images, recordings and sensor or PLC signals captured with Capturefab, exported for analysis.\n\
             The files below describe what was saved and when; the images and videos themselves stay at the\n\
             paths listed in `captures.path` and `recordings.path`.\n\n",
        );
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
        let parquet = manifest["formats"]
            .as_array()
            .is_some_and(|f| f.iter().any(|f| f == "parquet"));
        let ext = if parquet { "parquet" } else { "csv" };
        out.push_str(&format!(
            "\n## Loading\n\npandas:\n\n```python\nimport pandas as pd\ncaptures = pd.read_{}('captures.{ext}')\nwide = pd.read_{}('captures_with_signals.{ext}')\n```\n\n\
             DuckDB:\n\n```sql\nSELECT * FROM 'captures.{ext}';\nSELECT tag, avg(value_double) FROM 'samples.{ext}' GROUP BY tag;\n```\n\n\
             polars:\n\n```python\nimport polars as pl\nsamples = pl.read_{}('samples.{ext}')\n```\n\n",
            if parquet { "parquet" } else { "csv" },
            if parquet { "parquet" } else { "csv" },
            if parquet { "parquet" } else { "csv" },
        ));
        out.push_str(
            "## Notes on paths\n\nMedia paths are absolute paths on the capturing computer. A non-null `object_key` means the \
             file was queued for upload to that remote destination and may live only there.\n\n",
        );
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
        for signal in &meta.signals {
            let number = |key: &str| {
                signal[key]
                    .as_f64()
                    .map(|v| format!("{v:.4}"))
                    .unwrap_or_else(|| "n/a".into())
            };
            lines.push(format!(
                "- tag {} [{}] from {}: {} samples, min {}, max {}, mean {}",
                signal["tag"].as_str().unwrap_or(""),
                signal["unit"].as_str().unwrap_or("no unit"),
                signal["uri"].as_str().unwrap_or(""),
                signal["samples"],
                number("min"),
                number("max"),
                number("mean")
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
