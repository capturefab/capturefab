//! Fixed-table exports from a live private engine. Rust staging has its own
//! durable global namespace reservation; it never writes around the engine's
//! native DB/WAL/spill extent cap.
use super::{TaskDataset, TaskMode};
use crate::{
    dataset::{
        ExportFormat, TABLES,
        export::{ExportRequest, ExportSource, MAX_TIMEOUT_MS},
    },
    storage::{OutputDirectory, RetainedNamespaceLease},
};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Local recovery metadata, kept typed so a provider-facing host can remove
/// its exact private location without losing the operator's inventory receipt.
/// During failed reservation preparation only the parent is known locally;
/// the underlying storage error retains any allocated namespace id/path.
#[derive(Debug)]
pub(crate) struct RetainedExportStage {
    namespace_id: Option<String>,
    path: PathBuf,
}
impl RetainedExportStage {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn namespace_id(&self) -> Option<&str> {
        self.namespace_id.as_deref()
    }
}
impl std::fmt::Display for RetainedExportStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(id) = self.namespace_id() {
            write!(
                f,
                "task export staging reservation {id} may remain charged at {}; inspect storage status",
                self.path().display()
            )
        } else {
            write!(
                f,
                "task export staging below {} may remain charged; inspect storage status",
                self.path().display()
            )
        }
    }
}
impl std::error::Error for RetainedExportStage {}

struct Budget<'a> {
    used: u64,
    max: u64,
    deadline: Instant,
    cancel: &'a AtomicBool,
}
impl Budget<'_> {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.load(Ordering::Acquire),
            "task dataset export cancelled"
        );
        ensure!(
            Instant::now() < self.deadline,
            "task dataset export timed out"
        );
        Ok(())
    }
}

fn file_identity(file: &fs::File) -> Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.nlink() == 1,
            "staging file is not an exclusively owned regular file"
        );
        Ok((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    anyhow::bail!("private task export staging is unsupported on this platform")
}

/// File::Drop cannot report close uncertainty. Consume each fd exactly once;
/// a failed close is never retried and permanently prevents stage refund.
struct CheckedFile<'a> {
    file: Option<fs::File>,
    uncertain: &'a AtomicBool,
}
impl CheckedFile<'_> {
    fn close(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            self.close_with(|fd| {
                // SAFETY: this is the consumed owned descriptor; never retry.
                if unsafe { libc::close(fd) } < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            })
        }
        #[cfg(not(unix))]
        {
            if self.file.take().is_none() {
                return Ok(());
            }
            self.uncertain.store(true, Ordering::Release);
            anyhow::bail!("checked staging close is unsupported on this platform")
        }
    }
    #[cfg(unix)]
    fn close_with(&mut self, close: impl FnOnce(i32) -> io::Result<()>) -> Result<()> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        use std::os::fd::IntoRawFd;
        let fd = file.into_raw_fd();
        if let Err(error) = close(fd) {
            self.uncertain.store(true, Ordering::Release);
            return Err(error).context("staging file close is uncertain");
        }
        Ok(())
    }
    fn get(&self) -> &fs::File {
        self.file.as_ref().expect("live staging file")
    }
    fn get_mut(&mut self) -> &mut fs::File {
        self.file.as_mut().expect("live staging file")
    }
}
impl Drop for CheckedFile<'_> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct CappedWriter<'a, 'b> {
    file: CheckedFile<'a>,
    budget: &'a mut Budget<'b>,
    directory: &'a Path,
    bytes: u64,
    hash: Sha256,
    check_space_at: u64,
}
impl CappedWriter<'_, '_> {
    fn finish(mut self) -> Result<(u64, String)> {
        self.file.get_mut().flush()?;
        self.file.get().sync_all()?;
        self.file.close()?;
        Ok((self.bytes, format!("{:x}", self.hash.finalize())))
    }
}
impl Write for CappedWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.budget.check().map_err(io::Error::other)?;
        ensure_write(self.budget.used, bytes.len() as u64, self.budget.max)?;
        if self.budget.used >= self.check_space_at {
            if let Ok(space) = fs2::statvfs(self.directory)
                && space.total_space() > 0
                && space.available_space()
                    < crate::storage::FREE_SPACE_FLOOR.saturating_add(bytes.len() as u64)
            {
                return Err(io::Error::other(
                    "Storage full: task export staging must leave at least 512 MiB free",
                ));
            }
            self.check_space_at = self.budget.used.saturating_add(64 * 1024);
        }
        let written = self.file.get_mut().write(bytes)?;
        self.budget.used += written as u64;
        self.bytes += written as u64;
        self.hash.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.get_mut().flush()
    }
}
fn ensure_write(used: u64, count: u64, max: u64) -> io::Result<()> {
    if used.checked_add(count).is_none_or(|total| total > max) {
        return Err(io::Error::other(format!(
            "Storage full: dataset export exceeds its {max}-byte task allowance; narrow the selected tables or time range"
        )));
    }
    Ok(())
}

struct StagedFile {
    name: String,
    identity: (u64, u64),
    bytes: u64,
    description: Value,
    removed: bool,
}
struct Stage {
    lease: Arc<RetainedNamespaceLease>,
    anchor: Arc<OutputDirectory>,
    files: Vec<StagedFile>,
    uncertain: AtomicBool,
    cleanup_failed: bool,
    removed: bool,
}
impl Stage {
    fn reserve(parent: &Path, request: &ExportRequest, max: u64) -> Result<Self> {
        let lease = crate::storage::reserve_task_namespace(parent, &request.storage, max)
            .with_context(|| RetainedExportStage {
                namespace_id: None,
                path: parent.to_owned(),
            })?;
        let anchor = lease
            .claim_for_staging()
            .with_context(|| RetainedExportStage {
                namespace_id: Some(lease.id().into()),
                path: lease.anchor().path().to_owned(),
            })?;
        Ok(Self {
            lease,
            anchor,
            files: Vec::new(),
            uncertain: AtomicBool::new(false),
            cleanup_failed: false,
            removed: false,
        })
    }
    fn writer<'a, 'b>(
        &'a mut self,
        name: &str,
        budget: &'a mut Budget<'b>,
    ) -> Result<CappedWriter<'a, 'b>> {
        budget.check()?;
        self.anchor.verify()?;
        let file = self.anchor.create_new(OsStr::new(name))?;
        let file = CheckedFile {
            file: Some(file),
            uncertain: &self.uncertain,
        };
        let identity = match file_identity(file.get()) {
            Ok(identity) => identity,
            Err(error) => {
                // Without the created file's identity, even a definite close
                // cannot authorize deleting its later named replacement.
                self.uncertain.store(true, Ordering::Release);
                return Err(error);
            }
        };
        self.files.push(StagedFile {
            name: name.into(),
            identity,
            bytes: 0,
            description: Value::Null,
            removed: false,
        });
        Ok(CappedWriter {
            file,
            budget,
            directory: self.anchor.path(),
            bytes: 0,
            hash: Sha256::new(),
            check_space_at: 0,
        })
    }
    fn describe(&mut self, name: &str, bytes: u64, mut description: Value, sha256: String) {
        let file = self
            .files
            .iter_mut()
            .find(|file| file.name == name)
            .expect("created staging file");
        description["path"] = json!(name);
        description["bytes"] = json!(bytes);
        description["sha256"] = json!(sha256);
        file.bytes = bytes;
        file.description = description;
    }
    fn cleanup(&mut self) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        ensure!(
            !self.uncertain.load(Ordering::Acquire),
            "staging file close or identity is uncertain; reservation retained"
        );
        ensure!(
            !self.cleanup_failed,
            "staging cleanup was uncertain; reservation retained"
        );
        let result = (|| {
            for file in &mut self.files {
                if !file.removed {
                    let mut original = CheckedFile {
                        file: Some(self.anchor.open_file(OsStr::new(&file.name))?),
                        uncertain: &self.uncertain,
                    };
                    let removed = self.anchor.remove_file(
                        OsStr::new(&file.name),
                        file.identity,
                        original.get(),
                    );
                    let closed = original.close();
                    removed.and(closed)?;
                    file.removed = true;
                }
            }
            self.anchor.remove_empty()?;
            let proof = self.anchor.removed_receipt()?;
            self.lease.confirm_removed(&proof)?;
            self.removed = true;
            Ok(())
        })();
        self.cleanup_failed = result.is_err();
        result
    }
    fn retained_context(&self) -> RetainedExportStage {
        RetainedExportStage {
            namespace_id: Some(self.lease.id().into()),
            path: self.anchor.path().to_owned(),
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if !self.removed
            && !self.cleanup_failed
            && !self.uncertain.load(Ordering::Acquire)
            && let Err(error) = self.cleanup()
        {
            crate::eprintln!("capturefab: {}: {error:#}", self.retained_context());
        }
    }
}

impl TaskDataset {
    /// Export fixed canonical tables while borrowing the live private engine.
    /// Caller holds its owning task sink mutex; no Reader/Connection escapes.
    pub fn export_bounded(
        &mut self,
        request: &ExportRequest,
        max_bytes: u64,
        timeout_ms: u64,
        approved_root: Option<&Path>,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(timeout_ms))
            .context("invalid export deadline")?;
        ensure!(
            self.mode == TaskMode::Fresh && !self.close_uncertain,
            "task export requires a live, nonquarantined fresh catalog"
        );
        self.connection()?;
        ensure!(max_bytes > 0, "dataset export byte limit must be positive");
        ensure!(
            (1..=MAX_TIMEOUT_MS).contains(&timeout_ms),
            "dataset export timeout must be 1..600000 milliseconds"
        );
        request.storage.validate()?;
        let formats = if request.formats.is_empty() {
            vec![ExportFormat::Csv]
        } else {
            ExportFormat::or_default(request.formats.clone())
        };
        ensure!(formats.len() <= 4, "export accepts at most 4 formats");
        for format in &formats {
            ensure!(
                format.supported(),
                "{} export unsupported in this build",
                format.extension()
            );
        }
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
        ensure!(
            request.output.is_absolute(),
            "export folder must be absolute"
        );
        ensure!(
            !request.output.exists(),
            "export folder already exists; choose a new folder"
        );
        crate::storage::ensure_not_task_namespace(&request.output)?;
        if let Some(parent) = request.output.parent() {
            crate::volumes::ensure_present(parent)?;
        }
        let publication =
            crate::storage::OutputDirectoryPlan::prepare(&request.output, approved_root)?;
        let mut budget = Budget {
            used: 0,
            max: max_bytes,
            deadline,
            cancel,
        };
        budget.check()?;
        let status = self.catalog_status()?.dataset;
        let database_path = self.database_path();
        let parent = self
            .path()
            .parent()
            .context("task namespace has no parent")?;
        let mut stage = Stage::reserve(parent, request, max_bytes)?;
        let mut published = Vec::<Value>::new();
        let mut transaction_attempted = false;
        let conn = self.connection()?;
        let source = ExportSource {
            conn,
            path: &database_path,
            status: &status,
            cancelled: cancel,
        };
        let staged = crate::dataset::snapshot::with_watchdog(cancel, None, conn, deadline, || {
            budget.check()?;
            // BEGIN may itself be interrupted after opening the transaction.
            // Cleanup belongs outside the joined watcher in either case.
            transaction_attempted = true;
            conn.execute_batch("BEGIN TRANSACTION READ ONLY")?;
            stage_tables(&source, request, &formats, &mut stage, &mut budget)
        });
        let rollback_error = if transaction_attempted {
            rollback_read_transaction(conn).err()
        } else {
            None
        };
        let staged = if let Some(error) = rollback_error {
            self.mode = TaskMode::Failed;
            if let Some(catalog) = &mut self.catalog {
                catalog.record_error(&error);
            }
            Err(error.context("task export could not close its read transaction"))
        } else {
            staged
        };
        // All rows and metadata have been staged from one source snapshot.
        // Publication performs no SQL, so its existing absolute budget checks
        // provide cancellation without an idle engine interrupt racing cleanup.
        let result = staged.and_then(|tables| {
            budget.check()?;
            let quota = crate::storage::quota_status()?;
            let configured: crate::storage::StoragePolicy =
                serde_json::from_value(quota["policy"].clone())?;
            ensure!(
                quota["allocated_bytes"]
                    .as_u64()
                    .unwrap_or(u64::MAX)
                    .checked_add(budget.used)
                    .is_some_and(
                        |bytes| bytes <= request.storage.max_bytes.min(configured.max_bytes)
                    ),
                "Storage full: published export and its reserved staging do not fit the global storage budget"
            );
            ensure!(
                quota["files"]
                    .as_u64()
                    .unwrap_or(u64::MAX)
                    .checked_add(stage.files.len() as u64)
                    .is_some_and(|count| count
                        <= u64::from(request.storage.max_files.min(configured.max_files))),
                "Storage full: dataset export needs more tracked files"
            );
            let directory = publication.publish()?;
            for file in &stage.files {
                copy_file(&stage, file, &directory, request, &budget)?;
                published.push(file.description.clone());
                budget.check()?;
            }
            directory.verify()?;
            budget.check()?;
            Ok(
                json!({"output": request.output, "files": published, "tables": tables, "formats": formats,
                "bytes": budget.used, "max_bytes": max_bytes, "slot_id": status.slot_id, "dataset_id": status.dataset_id}),
            )
        });
        let cleanup = stage.cleanup();
        let result = match (result, cleanup) {
            (Ok(value), Ok(())) => budget.check().map(|()| value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(cleanup)) => Err(cleanup.context(stage.retained_context())),
            (Err(error), Err(cleanup)) => Err(error
                .context(format!("staging cleanup failed: {cleanup:#}"))
                .context(stage.retained_context())),
        };
        result.with_context(|| {
            format!(
                "export stopped; files already saved in {}: {}",
                request.output.display(),
                serde_json::to_string(
                    &published
                        .iter()
                        .map(|file| &file["path"])
                        .collect::<Vec<_>>()
                )
                .unwrap_or_default()
            )
        })
    }
}

fn rollback_read_transaction(conn: &duckdb::Connection) -> Result<()> {
    match conn.execute_batch("ROLLBACK") {
        Ok(()) => Ok(()),
        // The private source cannot have a caller-owned transaction. An
        // interrupted BEGIN may leave none, which is already a closed state.
        Err(duckdb::Error::DuckDBFailure(_, Some(message)))
            if message
                == "TransactionContext Error: cannot rollback - no transaction is active" =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn stage_tables(
    source: &ExportSource<'_>,
    request: &ExportRequest,
    formats: &[ExportFormat],
    stage: &mut Stage,
    budget: &mut Budget<'_>,
) -> Result<std::collections::BTreeMap<String, u64>> {
    let mut tables = std::collections::BTreeMap::new();
    for table in TABLES {
        if !request.tables.is_empty() && !request.tables.iter().any(|selected| selected == table) {
            continue;
        }
        budget.check()?;
        let Some(select) = source.export_select(table, request)? else {
            continue;
        };
        for format in formats {
            let name = format!("{table}.{}", format.extension());
            let deadline = budget.deadline;
            let mut writer = stage.writer(&name, budget)?;
            let rows = source.stream_rows(&select, *format, &mut writer, deadline)?;
            let (bytes, hash) = writer.finish()?;
            stage.describe(
                &name,
                bytes,
                json!({"table": table, "format": format, "rows": rows}),
                hash,
            );
            tables.insert(table.to_string(), rows);
            #[cfg(all(test, unix))]
            tests::after_staged_file(source.conn)?;
        }
    }
    budget.check()?;
    let descriptions = stage
        .files
        .iter()
        .map(|file| file.description.clone())
        .collect::<Vec<_>>();
    for (name, text) in source.metadata_text(request, formats, &descriptions)? {
        budget.check()?;
        let mut writer = stage.writer(name, budget)?;
        writer.write_all(text.as_bytes())?;
        let (bytes, hash) = writer.finish()?;
        stage.describe(name, bytes, json!({}), hash);
    }
    Ok(tables)
}

fn copy_file(
    stage: &Stage,
    file: &StagedFile,
    directory: &Arc<OutputDirectory>,
    request: &ExportRequest,
    budget: &Budget<'_>,
) -> Result<()> {
    budget.check()?;
    let mut source = CheckedFile {
        file: Some(stage.anchor.open_file(OsStr::new(&file.name))?),
        uncertain: &stage.uncertain,
    };
    ensure!(
        file_identity(source.get())? == file.identity
            && source.get().metadata()?.len() == file.bytes,
        "staged dataset export changed before publication"
    );
    let mut writer = crate::storage::create_writer_in(
        directory,
        &file.name,
        &request.storage,
        file.bytes.max(1),
    )?;
    let result = (|| {
        let mut buffer = [0; 64 * 1024];
        loop {
            budget.check()?;
            let count = source.get_mut().read(&mut buffer)?;
            if count == 0 {
                break;
            }
            ensure!(
                writer
                    .bytes_written()
                    .checked_add(count as u64)
                    .is_some_and(|total| total <= file.bytes),
                "staged dataset export grew before publication"
            );
            writer.write_all(&buffer[..count])?;
        }
        ensure!(
            writer.bytes_written() == file.bytes
                && writer.sha256() == file.description["sha256"].as_str().unwrap_or_default(),
            "staged dataset export changed before publication"
        );
        Ok(())
    })();
    let closed = source.close();
    if let Err(error) = result.and(closed) {
        return match writer.abort() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "partial export output {} may remain; ledger cleanup failed: {cleanup:#}",
                directory.path().join(&file.name).display()
            ))),
        };
    }
    writer.finish()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::dataset::task::TaskExtentBudget;
    use std::{path::PathBuf, sync::atomic::AtomicUsize};

    type StagedHook = Box<dyn FnOnce(&duckdb::Connection) -> Result<()>>;
    thread_local! {
        static STAGED_HOOK: std::cell::RefCell<Option<StagedHook>> = const { std::cell::RefCell::new(None) };
    }
    struct StagedHookGuard;
    impl Drop for StagedHookGuard {
        fn drop(&mut self) {
            STAGED_HOOK.with(|hook| {
                hook.borrow_mut().take();
            });
        }
    }
    fn install_staged_hook(hook: StagedHook) -> StagedHookGuard {
        assert!(
            STAGED_HOOK
                .with(|slot| slot.borrow_mut().replace(hook))
                .is_none()
        );
        StagedHookGuard
    }
    pub(super) fn after_staged_file(conn: &duckdb::Connection) -> Result<()> {
        let hook = STAGED_HOOK.with(|slot| slot.borrow_mut().take());
        hook.map_or(Ok(()), |hook| hook(conn))
    }

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "capturefab-task-export-{}",
                crate::dataset::db::random_name().unwrap()
            ));
            crate::ipc::ensure_private_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn request(&self) -> ExportRequest {
            ExportRequest {
                output: self.0.join("bundle"),
                formats: vec![ExportFormat::Csv],
                tables: vec!["events".into()],
                since_ms: None,
                until_ms: None,
                storage: crate::storage::StoragePolicy::default(),
                media: Default::default(),
                group_by: None,
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn retained(stage: &Stage) -> bool {
        crate::storage::quota_status().unwrap()["retained_namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["id"] == stage.lease.id() && entry["allocated_bytes"] == stage.lease.cap()
            })
    }
    fn stages(parent: &Path) -> usize {
        crate::storage::quota_status().unwrap()["retained_namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path).parent() == Some(parent))
            })
            .count()
    }
    fn budget(cancel: &AtomicBool, max: u64) -> Budget<'_> {
        Budget {
            used: 0,
            max,
            deadline: Instant::now() + Duration::from_secs(10),
            cancel,
        }
    }
    fn fresh(temp: &Temp) -> (TaskDataset, Arc<RetainedNamespaceLease>) {
        let policy = crate::storage::StoragePolicy::default();
        let lease =
            crate::storage::reserve_task_namespace(&temp.0, &policy, 16 * 1024 * 1024).unwrap();
        let mut dataset = TaskDataset::create_reserved(
            lease.clone(),
            TaskExtentBudget::new(lease.cap()).unwrap(),
        )
        .unwrap();
        dataset.initialize_catalog("private export").unwrap();
        dataset
            .record_event(
                "note",
                Some("sim:0"),
                None,
                "two,\"quoted\"\nrows",
                &crate::context::Context::new(),
            )
            .unwrap();
        dataset
            .record_event(
                "note",
                Some("sim:0"),
                None,
                "ready",
                &crate::context::Context::new(),
            )
            .unwrap();
        (dataset, lease)
    }
    fn remove(dataset: &mut TaskDataset, lease: &RetainedNamespaceLease) {
        lease
            .confirm_removed(&dataset.remove_reserved().unwrap())
            .unwrap();
    }

    #[test]
    fn private_export_reuses_formats_and_manifest_and_refunds_only_its_separate_stage() {
        let temp = Temp::new();
        let (mut dataset, lease) = fresh(&temp);
        let identity = dataset.catalog_identity().unwrap().clone();
        let mut request = temp.request();
        request
            .formats
            .extend([ExportFormat::Json, ExportFormat::Jsonl]);
        if cfg!(feature = "parquet") {
            request.formats.push(ExportFormat::Parquet);
        }
        request.tables = vec!["events".into()];
        let value = dataset
            .export_bounded(
                &request,
                128 * 1024,
                10_000,
                Some(&temp.0),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(value["dataset_id"], identity.dataset_id);
        assert_eq!(value["slot_id"], identity.slot_id);
        assert_eq!(value["tables"]["events"], 2);
        let mut total = 0;
        for file in value["files"].as_array().unwrap() {
            let bytes = fs::read(request.output.join(file["path"].as_str().unwrap())).unwrap();
            assert_eq!(file["bytes"], bytes.len() as u64);
            assert_eq!(file["sha256"], format!("{:x}", Sha256::digest(&bytes)));
            total += bytes.len() as u64;
        }
        assert_eq!(value["bytes"], total);
        assert!(total <= 128 * 1024);
        let csv = fs::read_to_string(request.output.join("events.csv")).unwrap();
        assert!(csv.contains("\"two,\"\"quoted\"\"\nrows\""));
        let jsonl = fs::read_to_string(request.output.join("events.jsonl")).unwrap();
        let jsonl_rows = jsonl
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(jsonl_rows.len(), 2);
        assert_eq!(jsonl_rows[0]["text"], "two,\"quoted\"\nrows");
        assert_eq!(jsonl_rows[1]["text"], "ready");
        let manifest: Value =
            serde_json::from_slice(&fs::read(request.output.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["dataset_id"], identity.dataset_id);
        assert_eq!(manifest["counts"]["events"], 2);
        assert_eq!(stages(&temp.0), 1, "only native task reservation remains");
        assert_eq!(dataset.usage().unwrap().limit, lease.cap());
        assert!(dataset.usage().unwrap().peak <= lease.cap());
        assert_eq!(
            dataset
                .connection()
                .unwrap()
                .query_row("SELECT count(*) FROM events", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            2
        );
        remove(&mut dataset, &lease);
        assert_eq!(stages(&temp.0), 0);
    }

    #[test]
    fn failed_or_cancelled_exports_leave_no_stage_refund_without_cleanup_and_no_final_folder() {
        let temp = Temp::new();
        let (mut dataset, lease) = fresh(&temp);
        let request = temp.request();
        let error = dataset
            .export_bounded(&request, 64, 10_000, Some(&temp.0), &AtomicBool::new(false))
            .unwrap_err();
        assert!(format!("{error:#}").contains("64-byte"), "{error:#}");
        assert!(!request.output.exists());
        assert_eq!(stages(&temp.0), 1);
        assert!(
            dataset
                .export_bounded(
                    &request,
                    4096,
                    10_000,
                    Some(&temp.0),
                    &AtomicBool::new(true)
                )
                .is_err()
        );
        assert_eq!(stages(&temp.0), 1);
        assert!(!request.output.exists());
        dataset.close().unwrap();
        assert!(
            dataset
                .export_bounded(
                    &request,
                    4096,
                    10_000,
                    Some(&temp.0),
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert_eq!(stages(&temp.0), 1, "closed engine does not reserve staging");
        remove(&mut dataset, &lease);
    }

    #[test]
    fn interrupted_export_rolls_back_after_join_and_leaves_the_catalog_writable_and_stage_refunded()
    {
        for cancel_during_read in [true, false] {
            let temp = Temp::new();
            let (mut dataset, lease) = fresh(&temp);
            // Also verify the exact bundled-engine no-active acknowledgement
            // accepted when BEGIN itself was interrupted before opening a txn.
            rollback_read_transaction(dataset.connection().unwrap()).unwrap();
            let request = temp.request();
            let cancel = Arc::new(AtomicBool::new(false));
            let entered = Arc::new(AtomicBool::new(false));
            let interrupted = Arc::new(AtomicBool::new(false));
            let charged_during_read = Arc::new(AtomicBool::new(false));
            let _hook = install_staged_hook(Box::new({
                let cancel = cancel.clone();
                let entered = entered.clone();
                let interrupted = interrupted.clone();
                let charged_during_read = charged_during_read.clone();
                let parent = temp.0.clone();
                move |conn| {
                    entered.store(true, Ordering::Release);
                    charged_during_read.store(stages(&parent) == 2, Ordering::Release);
                    if cancel_during_read {
                        cancel.store(true, Ordering::Release);
                    }
                    // This hook runs after a real staged file has been closed,
                    // while the same source read transaction is still active.
                    // Hashing prevents a closed-form range aggregate shortcut.
                    match conn.execute_batch("SELECT sum(hash(i)) FROM range(1000000000000) t(i)") {
                        Ok(()) => Err(anyhow::anyhow!(
                            "test query completed without its expected interrupt"
                        )),
                        Err(error) => {
                            interrupted.store(
                                error.to_string().to_ascii_uppercase().contains("INTERRUPT"),
                                Ordering::Release,
                            );
                            Err(error.into())
                        }
                    }
                }
            }));
            let timeout_ms = if cancel_during_read { 10_000 } else { 2_000 };
            let error = dataset
                .export_bounded(&request, 128 * 1024, timeout_ms, Some(&temp.0), &cancel)
                .unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.contains("cancel") || text.contains("deadline") || text.contains("timed out"),
                "{text}"
            );
            assert!(
                entered.load(Ordering::Acquire),
                "cancellation must occur inside the transaction"
            );
            assert!(
                interrupted.load(Ordering::Acquire),
                "the real engine statement must be interrupted"
            );
            assert!(charged_during_read.load(Ordering::Acquire));
            assert!(!request.output.exists());
            assert_eq!(
                stages(&temp.0),
                1,
                "only the native task cap remains after proven stage cleanup"
            );
            assert!(!dataset.catalog_status().unwrap().quarantined);
            cancel.store(false, Ordering::Release);
            dataset
                .record_event(
                    "note",
                    Some("sim:0"),
                    None,
                    "after interrupt",
                    &crate::context::Context::new(),
                )
                .unwrap();
            dataset.checkpoint_catalog().unwrap();
            assert_eq!(
                dataset
                    .connection()
                    .unwrap()
                    .query_row("SELECT count(*) FROM events", [], |row| row
                        .get::<_, u64>(0))
                    .unwrap(),
                3
            );
            remove(&mut dataset, &lease);
            assert_eq!(stages(&temp.0), 0);
        }
    }

    #[test]
    fn cumulative_stage_cap_and_cancellation_are_checked_before_each_disk_write() {
        let temp = Temp::new();
        let request = temp.request();
        let cancel = AtomicBool::new(false);
        let mut budget = budget(&cancel, 8);
        let mut stage = Stage::reserve(&temp.0, &request, 8).unwrap();
        assert!(retained(&stage));
        let mut writer = stage.writer("first", &mut budget).unwrap();
        writer.write_all(b"123456").unwrap();
        writer.finish().unwrap();
        let mut writer = stage.writer("second", &mut budget).unwrap();
        assert!(writer.write_all(b"789").is_err());
        writer.write_all(b"78").unwrap();
        cancel.store(true, Ordering::Release);
        assert!(writer.write_all(b"9").is_err());
        writer.finish().unwrap();
        assert_eq!(
            fs::read(stage.anchor.path().join("first")).unwrap(),
            b"123456"
        );
        assert_eq!(fs::read(stage.anchor.path().join("second")).unwrap(), b"78");
        assert_eq!(budget.used, 8);
        assert!(retained(&stage));
        stage.cleanup().unwrap();
        assert!(!retained(&stage));
    }

    #[test]
    fn uncertain_close_keeps_full_stage_charge_and_is_never_retried() {
        let temp = Temp::new();
        let request = temp.request();
        let cancel = AtomicBool::new(false);
        let mut budget = budget(&cancel, 4096);
        let mut stage = Stage::reserve(&temp.0, &request, 4096).unwrap();
        let attempts = AtomicUsize::new(0);
        let mut writer = stage.writer("partial", &mut budget).unwrap();
        writer.write_all(b"known bytes").unwrap();
        let error = writer
            .file
            .close_with(|fd| {
                attempts.fetch_add(1, Ordering::AcqRel);
                // SAFETY: the test owns this fd. Simulate a lost close
                // acknowledgement after the real close, without leaking an fd.
                assert_eq!(unsafe { libc::close(fd) }, 0);
                Err(io::Error::from(io::ErrorKind::Interrupted))
            })
            .unwrap_err();
        assert!(error.to_string().contains("uncertain"));
        drop(writer);
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        let error = stage
            .cleanup()
            .unwrap_err()
            .context(stage.retained_context())
            .context("export stopped");
        let context = error
            .downcast_ref::<RetainedExportStage>()
            .expect("typed context survives outer error wrapping");
        assert_eq!(context.path(), stage.anchor.path());
        assert_eq!(context.namespace_id(), Some(stage.lease.id()));
        assert!(format!("{error:#}").contains(&stage.anchor.path().display().to_string()));
        assert!(retained(&stage));
        assert_eq!(
            fs::read(stage.anchor.path().join("partial")).unwrap(),
            b"known bytes"
        );
        assert!(
            stage
                .retained_context()
                .to_string()
                .contains(stage.lease.id())
        );
        // Test-only recovery: the injected acknowledgement loss above is
        // known to have closed the fd. Production never clears uncertainty.
        stage.uncertain.store(false, Ordering::Release);
        stage.cleanup().unwrap();
        assert!(!retained(&stage));
    }

    #[test]
    fn changed_links_retain_staging_and_growing_files_cannot_expand_publication() {
        let temp = Temp::new();
        let request = temp.request();
        let cancel = AtomicBool::new(false);
        let mut budget = budget(&cancel, 4096);
        let mut stage = Stage::reserve(&temp.0, &request, 4096).unwrap();
        let mut writer = stage.writer("sample", &mut budget).unwrap();
        writer.write_all(b"1234").unwrap();
        let (bytes, hash) = writer.finish().unwrap();
        stage.describe("sample", bytes, json!({}), hash);
        let path = stage.anchor.path().join("sample");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"unexpected")
            .unwrap();
        let publication =
            crate::storage::OutputDirectoryPlan::prepare(&request.output, Some(&temp.0))
                .unwrap()
                .publish()
                .unwrap();
        assert!(copy_file(&stage, &stage.files[0], &publication, &request, &budget).is_err());
        assert!(fs::read_dir(&request.output).unwrap().next().is_none());
        let link = temp.0.join("outside-link");
        fs::hard_link(&path, &link).unwrap();
        assert!(stage.cleanup().is_err());
        assert!(retained(&stage));
        assert!(path.exists() && link.exists());
        fs::remove_file(link).unwrap();
        stage.cleanup_failed = false; // test knows why removal was refused
        stage.cleanup().unwrap();
    }
}
