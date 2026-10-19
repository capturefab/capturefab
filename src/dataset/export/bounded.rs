//! Exports for agent tasks use a capped writer before anything reaches disk.
//! DuckDB's COPY writer has no hard byte limit, so it cannot be used here.
use super::*;
use crate::dataset::query::to_json;
use sha2::{Digest, Sha256};
use std::{
    io::{Read as _, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

struct Cleanup(Vec<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

struct Budget {
    used: u64,
    max: u64,
    deadline: Instant,
}
impl Budget {
    fn check(&self, reader: &Reader) -> Result<()> {
        self.check_flag(&reader.cancelled)
    }
    fn check_flag(&self, cancelled: &AtomicBool) -> Result<()> {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "dataset operation cancelled because its open instance was closed"
        );
        ensure!(
            Instant::now() < self.deadline,
            "dataset export timed out; narrow the selected tables or time range"
        );
        Ok(())
    }
}

/// All bytes, including headers, separators, metadata and checksum-bearing
/// manifest text, pass this writer. A rejected write reaches no disk file.
struct CappedWriter<'a> {
    file: fs::File,
    budget: &'a mut Budget,
    cancelled: Arc<AtomicBool>,
    staging_dir: PathBuf,
    bytes: u64,
    hash: Sha256,
    space_check_at: u64,
}
impl Write for CappedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.budget
            .check_flag(&self.cancelled)
            .map_err(std::io::Error::other)?;
        let count = bytes.len() as u64;
        if self.budget.used.saturating_add(count) > self.budget.max {
            return Err(std::io::Error::other(format!(
                "Storage full: dataset export exceeds its {}-byte task allowance; narrow the selected tables or time range",
                self.budget.max
            )));
        }
        if self.budget.used >= self.space_check_at {
            if let Ok(stats) = fs2::statvfs(&self.staging_dir)
                && stats.total_space() > 0
                && stats.available_space() < crate::storage::FREE_SPACE_FLOOR.saturating_add(count)
            {
                return Err(std::io::Error::other(
                    "Storage full: dataset export staging must leave at least 512 MiB free",
                ));
            }
            self.space_check_at = self.budget.used.saturating_add(64 * 1024);
        }
        let written = self.file.write(bytes)?;
        self.budget.used += written as u64;
        self.bytes += written as u64;
        self.hash.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

struct File {
    name: String,
    staged: PathBuf,
    description: Value,
    bytes: u64,
}

impl Reader {
    /// Export within a hard total byte bound and a finite deadline. Existing
    /// folders are refused; failures before publication leave no output folder.
    pub fn export_bounded(
        &self,
        request: &ExportRequest,
        max_bytes: u64,
        timeout_ms: u64,
    ) -> Result<Value> {
        self.export_bounded_in(request, max_bytes, timeout_ms, None)
    }

    /// The approved root was canonicalized during tool review. Directory
    /// anchors are captured before staging and retained through final writes.
    pub fn export_bounded_in(
        &self,
        request: &ExportRequest,
        max_bytes: u64,
        timeout_ms: u64,
        approved_root: Option<&Path>,
    ) -> Result<Value> {
        self.check_cancelled()?;
        ensure!(max_bytes > 0, "dataset export byte limit must be positive");
        ensure!(
            (1..=MAX_TIMEOUT_MS).contains(&timeout_ms),
            "dataset export timeout must be 1..600000 milliseconds"
        );
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        request.storage.validate()?;
        let formats = if request.formats.is_empty() {
            vec![ExportFormat::Csv]
        } else {
            ExportFormat::or_default(request.formats.clone())
        };
        for format in &formats {
            ensure!(
                format.supported(),
                "{} export unsupported in this build",
                format.extension()
            );
        }
        ensure!(formats.len() <= 4, "export accepts at most 4 formats");
        for table in &request.tables {
            ensure!(
                TABLES.contains(&table.as_str()),
                "unknown dataset table {}",
                table.escape_debug()
            );
        }
        if let (Some(since), Some(until)) = (request.since_ms, request.until_ms) {
            ensure!(since <= until, "export start must not be after its end");
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
        let publication = crate::storage::OutputDirectoryPlan::prepare(output, approved_root)?;
        let status = crate::storage::quota_status()?;
        let configured: StoragePolicy =
            serde_json::from_value(status["policy"].clone()).unwrap_or_default();
        let allocated = status["allocated_bytes"].as_u64().unwrap_or(0);
        let available = request
            .storage
            .max_bytes
            .min(configured.max_bytes)
            .saturating_sub(allocated);
        let mut budget = Budget {
            used: 0,
            max: max_bytes.min(available),
            deadline,
        };
        let mut cleanup = Cleanup(Vec::new());
        budget.check(self)?;
        self.conn
            .execute_batch("BEGIN TRANSACTION")
            .map_err(|e| super::super::db::explain(e, &self.path, "export"))?;
        let (staged, fired) = with_timeout(
            &self.conn,
            deadline.saturating_duration_since(Instant::now()),
            || self.stage_bounded(request, &formats, &mut budget, &mut cleanup),
        );
        let _ = self.conn.execute_batch("ROLLBACK");
        self.check_cancelled()?;
        if fired {
            bail!("dataset export timed out; narrow the selected tables or time range");
        }
        let (staged, tables) = staged?;
        budget.check(self)?;
        // Apply global count and byte quotas before creating the final folder.
        let needed = budget.used;
        let count = staged.len() as u64;
        let files = status["files"].as_u64().unwrap_or(0);
        ensure!(
            files.saturating_add(count)
                <= u64::from(request.storage.max_files.min(configured.max_files)),
            "Storage full: dataset export needs {count} more files"
        );
        ensure!(
            needed <= available,
            "Storage full: dataset export exceeds the remaining storage budget"
        );
        let directory = publication.publish()?;
        let mut files = Vec::new();
        for file in &staged {
            let result = self.copy_bounded(file, &directory, &request.storage, &budget);
            match result {
                Ok(()) => {
                    files.push(file.description.clone());
                    let _ = fs::remove_file(&file.staged);
                    // Commit can finish after a deadline or close. Retain its
                    // receipt while refusing a late success acknowledgement.
                    if let Err(error) = budget.check(self) {
                        let written: Vec<_> = files.iter().map(|f| f["path"].clone()).collect();
                        return Err(error.context(format!(
                            "export stopped; files already saved in {}: {}",
                            output.display(),
                            serde_json::to_string(&written).unwrap_or_default()
                        )));
                    }
                }
                Err(error) => {
                    let written: Vec<_> = files.iter().map(|f| f["path"].clone()).collect();
                    return Err(error.context(format!(
                        "export stopped; files already written stay in {}: {}",
                        output.display(),
                        serde_json::to_string(&written).unwrap_or_default()
                    )));
                }
            }
        }
        budget
            .check(self)
            .and_then(|()| directory.verify())
            .and_then(|()| budget.check(self))
            .with_context(|| {
                let written: Vec<_> = files.iter().map(|f| f["path"].clone()).collect();
                format!(
                    "export stopped; files already saved in {}: {}",
                    output.display(),
                    serde_json::to_string(&written).unwrap_or_default()
                )
            })?;
        Ok(
            json!({"output": output, "files": files, "tables": tables, "formats": formats,
            "bytes": needed, "max_bytes": max_bytes, "slot_id": self.status.slot_id, "dataset_id": self.status.dataset_id}),
        )
    }

    fn capped_file<'a>(
        &'a self,
        name: &str,
        budget: &'a mut Budget,
        cleanup: &mut Cleanup,
    ) -> Result<(PathBuf, CappedWriter<'a>)> {
        budget.check(self)?;
        let path = self
            .staging
            .dir
            .join(format!("{}-{name}", super::super::db::random_name()?));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        cleanup.0.push(path.clone());
        Ok((
            path,
            CappedWriter {
                file,
                budget,
                cancelled: self.cancelled.clone(),
                staging_dir: self.staging.dir.clone(),
                bytes: 0,
                hash: Sha256::new(),
                space_check_at: 0,
            },
        ))
    }

    fn stage_bounded(
        &self,
        request: &ExportRequest,
        formats: &[ExportFormat],
        budget: &mut Budget,
        cleanup: &mut Cleanup,
    ) -> Result<(Vec<File>, BTreeMap<String, u64>)> {
        let mut staged = Vec::new();
        let mut tables = BTreeMap::new();
        for table in TABLES {
            if !request.tables.is_empty() && !request.tables.iter().any(|t| t == table) {
                continue;
            }
            budget.check(self)?;
            let Some(select) = self.export_select(table, request)? else {
                continue;
            };
            for format in formats {
                let name = format!("{table}.{}", format.extension());
                let (path, mut writer) = self.capped_file(&name, budget, cleanup)?;
                let deadline = writer.budget.deadline;
                let rows =
                    self.export_source()
                        .stream_rows(&select, *format, &mut writer, deadline)?;
                writer.flush()?;
                let bytes = writer.bytes;
                let sha256 = format!("{:x}", writer.hash.finalize());
                staged.push(File { name: name.clone(), staged: path, bytes,
                    description: json!({"path": name, "table": table, "format": format, "rows": rows, "bytes": bytes, "sha256": sha256}) });
                tables.insert(table.to_owned(), rows);
            }
        }
        budget.check(self)?;
        let metadata = self.metadata(request)?;
        budget.check(self)?;
        let descriptions: Vec<_> = staged.iter().map(|f| f.description.clone()).collect();
        let manifest = self.manifest(request, formats, &descriptions, &metadata);
        let readme = self.readme(&manifest, &metadata);
        for (name, text) in [
            ("manifest.json", serde_json::to_string_pretty(&manifest)?),
            ("README.md", readme),
        ] {
            let (path, mut writer) = self.capped_file(name, budget, cleanup)?;
            writer.write_all(text.as_bytes())?;
            writer.flush()?;
            let bytes = writer.bytes;
            let sha256 = format!("{:x}", writer.hash.finalize());
            staged.push(File {
                name: name.into(),
                staged: path,
                bytes,
                description: json!({"path": name, "bytes": bytes, "sha256": sha256}),
            });
        }
        Ok((staged, tables))
    }

    fn copy_bounded(
        &self,
        file: &File,
        directory: &Arc<crate::storage::OutputDirectory>,
        policy: &StoragePolicy,
        budget: &Budget,
    ) -> Result<()> {
        budget.check(self)?;
        let mut writer =
            crate::storage::create_writer_in(directory, &file.name, policy, file.bytes.max(1))?;
        let result = (|| -> Result<()> {
            let mut source = fs::File::open(&file.staged)?;
            let mut buffer = [0; 64 * 1024];
            loop {
                budget.check(self)?;
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                writer.write_all(&buffer[..count])?;
            }
            ensure!(
                writer.bytes_written() == file.bytes
                    && writer.sha256() == file.description["sha256"].as_str().unwrap_or_default(),
                "staged dataset export changed before publication"
            );
            Ok(())
        })();
        if let Err(error) = result {
            let _ = writer.abort();
            return Err(error);
        }
        writer.finish()?;
        Ok(())
    }
}

fn csv_cell(writer: &mut (impl Write + Send), value: Option<&str>) -> Result<()> {
    let Some(text) = value else { return Ok(()) };
    let quote = text.is_empty() || text.contains([',', '"', '\n', '\r']);
    if quote {
        writer.write_all(b"\"")?;
    }
    for (i, part) in text.split('"').enumerate() {
        if i > 0 {
            writer.write_all(b"\"\"")?;
        }
        writer.write_all(part.as_bytes())?;
    }
    if quote {
        writer.write_all(b"\"")?;
    }
    Ok(())
}

impl super::ExportSource<'_> {
    pub(in crate::dataset) fn stream_rows(
        &self,
        select: &str,
        format: ExportFormat,
        writer: &mut (impl Write + Send),
        deadline: Instant,
    ) -> Result<u64> {
        if format == ExportFormat::Parquet {
            return self.stream_parquet(select, writer, deadline);
        }
        let mut statement = self
            .conn
            .prepare(select)
            .map_err(|e| super::super::db::explain(e, self.path, "export"))?;
        let mut rows = statement
            .query([])
            .map_err(|e| super::super::db::explain(e, self.path, "export"))?;
        let columns: Vec<String> = rows
            .as_ref()
            .map(|s| s.column_names().iter().map(|s| (*s).to_owned()).collect())
            .unwrap_or_default();
        if format == ExportFormat::Csv {
            for (i, name) in columns.iter().enumerate() {
                if i > 0 {
                    writer.write_all(b",")?;
                }
                csv_cell(writer, Some(name))?;
            }
            writer.write_all(b"\n")?;
        } else if format == ExportFormat::Json {
            writer.write_all(b"[")?;
        }
        let mut count = 0;
        while let Some(row) = rows
            .next()
            .map_err(|e| super::super::db::explain(e, self.path, "export"))?
        {
            self.check_cancelled()?;
            ensure!(
                Instant::now() < deadline,
                "dataset export timed out; narrow the selected tables or time range"
            );
            if format == ExportFormat::Csv {
                for i in 0..columns.len() {
                    if i > 0 {
                        writer.write_all(b",")?;
                    }
                    let value = to_json(row.get::<_, duckdb::types::Value>(i)?);
                    match value {
                        Value::Null => csv_cell(writer, None)?,
                        Value::String(text) => csv_cell(writer, Some(&text))?,
                        other => csv_cell(writer, Some(&other.to_string()))?,
                    }
                }
                writer.write_all(b"\n")?;
            } else {
                if format == ExportFormat::Json && count > 0 {
                    writer.write_all(b",")?;
                }
                writer.write_all(b"{")?;
                for (i, name) in columns.iter().enumerate() {
                    if i > 0 {
                        writer.write_all(b",")?;
                    }
                    serde_json::to_writer(&mut *writer, name)?;
                    writer.write_all(b":")?;
                    serde_json::to_writer(
                        &mut *writer,
                        &to_json(row.get::<_, duckdb::types::Value>(i)?),
                    )?;
                }
                writer.write_all(b"}")?;
                if format == ExportFormat::Jsonl {
                    writer.write_all(b"\n")?;
                }
            }
            count += 1;
        }
        if format == ExportFormat::Json {
            writer.write_all(b"]\n")?;
        }
        Ok(count)
    }

    #[cfg(feature = "parquet")]
    fn stream_parquet(
        &self,
        select: &str,
        writer: &mut (impl Write + Send),
        deadline: Instant,
    ) -> Result<u64> {
        use duckdb::arrow::record_batch::RecordBatch;
        use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};
        let mut statement = self
            .conn
            .prepare(select)
            .map_err(|e| super::super::db::explain(e, self.path, "export"))?;
        // Use the fallible Statement::step API instead of the Arrow iterator,
        // whose next() panics when a deadline or dataset close interrupts it.
        let schema = statement.stream_arrow([])?.get_schema();
        let properties = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_max_row_group_bytes(Some(1024 * 1024))
            .set_max_row_group_row_count(Some(2048))
            .build();
        let mut parquet = ArrowWriter::try_new(&mut *writer, schema, Some(properties))?;
        let mut count = 0;
        loop {
            self.check_cancelled()?;
            ensure!(
                Instant::now() < deadline,
                "dataset export timed out; narrow the selected tables or time range"
            );
            let Some(array) = statement
                .step()
                .map_err(|e| super::super::db::explain(e, self.path, "export"))?
            else {
                break;
            };
            let batch = RecordBatch::from(&array);
            count += batch.num_rows() as u64;
            parquet.write(&batch)?;
        }
        parquet.finish()?;
        Ok(count)
    }

    #[cfg(not(feature = "parquet"))]
    fn stream_parquet(
        &self,
        _select: &str,
        _writer: &mut (impl Write + Send),
        _deadline: Instant,
    ) -> Result<u64> {
        bail!("parquet export unsupported in this build")
    }
}
