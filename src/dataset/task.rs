//! A separate engine and private namespace for task-owned storage.
//!
//! This foundation is deliberately not connected to the shared session Slot.
//! Its cap covers combined logical file EOF extents, including WAL/recovery
//! files and spill. Filesystem blocks, metadata and OS journals are excluded.
//! Retained files keep their charges in the shared budget after engine close;
//! `remove` is the explicit, anchored cleanup operation. No Connection or clone
//! can escape the raw database owner's lifetime.
//! Only this adapter may mutate files in a fresh task namespace. Attaching an
//! existing or externally mutated namespace requires reconciliation and is not
//! exposed by this API.
use crate::storage::{
    NamespaceRemovalReceipt, OutputDirectory, OutputDirectoryPlan, RetainedNamespaceLease,
};
use anyhow::{Context as _, Result, ensure};
use duckdb::{Connection, ffi};
use serde::Serialize;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::{Arc, Mutex},
};
#[path = "task_export.rs"]
mod export;
#[cfg(all(test, unix))]
#[path = "task_model_tests.rs"]
mod model_tests;
#[path = "task_snapshot.rs"]
mod snapshot_impl;
#[path = "task_writer.rs"]
mod writer;
use super::snapshot;
pub use super::snapshot::{SnapshotLimits, SnapshotProjection, SnapshotReceipt};
pub use writer::{TaskCatalogIdentity, TaskCatalogStatus};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ExtentUsage {
    pub limit: u64,
    pub used: u64,
    pub peak: u64,
    pub denied: u64,
    pub uncertain_files: u64,
    pub poisoned: u64,
    pub spill_opens: u64,
    pub wal_opens: u64,
    pub recovery_opens: u64,
}

unsafe extern "C" {
    fn capturefab_task_budget_new(
        limit: u64,
        out: *mut *mut c_void,
        error: *mut *mut c_char,
    ) -> c_int;
    fn capturefab_task_budget_free(budget: *mut c_void);
    fn capturefab_task_budget_usage(
        budget: *mut c_void,
        out: *mut ExtentUsage,
        error: *mut *mut c_char,
    ) -> c_int;
    fn capturefab_task_namespace_new(
        path: *const c_char,
        expected_device: u64,
        expected_inode: u64,
        budget: *mut c_void,
        out: *mut *mut c_void,
        error: *mut *mut c_char,
    ) -> c_int;
    fn capturefab_task_namespace_free(space: *mut c_void);
    fn capturefab_task_namespace_remove(space: *mut c_void, error: *mut *mut c_char) -> c_int;
    fn capturefab_task_database_open(
        space: *mut c_void,
        out: *mut ffi::duckdb_database,
        error: *mut *mut c_char,
    ) -> c_int;
    fn capturefab_task_database_close(
        database: *mut ffi::duckdb_database,
        error: *mut *mut c_char,
    ) -> c_int;
    #[cfg(test)]
    fn capturefab_task_fs_probes(space: *mut c_void, error: *mut *mut c_char) -> c_int;
}

fn bridge(operation: impl FnOnce(*mut *mut c_char) -> c_int) -> Result<()> {
    let mut error = ptr::null_mut();
    let result = operation(&mut error);
    let message = if error.is_null() {
        "isolated task filesystem failed without a diagnostic".into()
    } else {
        // SAFETY: every bridge error is NUL-terminated and allocated by DuckDB.
        let message = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::duckdb_free(error.cast()) };
        message
    };
    ensure!(result == 0, "{message}");
    Ok(())
}

/// One lease shared by all task engines and their files. Native mutex state
/// serializes reservation, I/O and reconciliation across every attached engine.
pub struct TaskExtentBudget {
    native: NonNull<c_void>,
    admission: Mutex<BudgetAdmission>,
}
#[derive(Default)]
struct BudgetAdmission {
    attempted: bool,
    reserved: bool,
}
// SAFETY: native Budget state is mutex protected; its Arc-like C++ owner is
// retained separately by every namespace/filesystem and live file handle.
unsafe impl Send for TaskExtentBudget {}
unsafe impl Sync for TaskExtentBudget {}
impl TaskExtentBudget {
    pub fn new(limit: u64) -> Result<Arc<Self>> {
        let mut native = ptr::null_mut();
        // SAFETY: writable outputs and a positive limit checked by the bridge.
        bridge(|error| unsafe { capturefab_task_budget_new(limit, &mut native, error) })?;
        Ok(Arc::new(Self {
            native: NonNull::new(native).context("missing task budget")?,
            admission: Mutex::new(BudgetAdmission::default()),
        }))
    }
    pub fn usage(&self) -> Result<ExtentUsage> {
        let mut usage = ExtentUsage::default();
        // SAFETY: this object keeps its native owner live for the entire call.
        bridge(|error| unsafe {
            capturefab_task_budget_usage(self.native.as_ptr(), &mut usage, error)
        })?;
        Ok(usage)
    }
    fn claim_namespace(&self, reserved: bool) -> Result<()> {
        let mut admission = self
            .admission
            .lock()
            .map_err(|_| anyhow::anyhow!("task budget admission state is poisoned"))?;
        ensure!(
            !admission.reserved,
            "reserved task budgets cannot be shared"
        );
        if reserved {
            let usage = self.usage()?;
            ensure!(
                !admission.attempted
                    && usage.used == 0
                    && usage.peak == 0
                    && usage.denied == 0
                    && usage.uncertain_files == 0
                    && usage.poisoned == 0,
                "a retained task namespace requires a fresh exclusive extent budget"
            );
            admission.reserved = true;
        }
        admission.attempted = true;
        Ok(())
    }
}
impl Drop for TaskExtentBudget {
    fn drop(&mut self) {
        // SAFETY: consume only this native shared_ptr owner, not other leases.
        unsafe { capturefab_task_budget_free(self.native.as_ptr()) };
    }
}

pub struct TaskDataset {
    conn: Option<Connection>,
    database: ffi::duckdb_database,
    native: NonNull<c_void>,
    budget: Arc<TaskExtentBudget>,
    anchor: Arc<OutputDirectory>,
    retained: Option<Arc<RetainedNamespaceLease>>,
    close_uncertain: bool,
    removed: bool,
    mode: TaskMode,
    model_projection: bool,
    catalog: Option<writer::Catalog>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskMode {
    Empty,
    Imported,
    Fresh,
    Failed,
}
// SAFETY: Connection supports Send; all native namespace and budget operations
// are protected by their shared mutex. There is one private raw database owner.
unsafe impl Send for TaskDataset {}
impl TaskDataset {
    /// Match the pinned native adapter's path grammar before publishing a
    /// namespace or consuming its one-shot engine admission. Unix permits
    /// names which DuckDB's private filesystem deliberately cannot address.
    pub(crate) fn validate_namespace_path(path: &Path) -> Result<()> {
        ensure!(path.is_absolute(), "task namespace must be absolute");
        let path = path.to_str().context("task namespace requires UTF-8")?;
        ensure!(
            path.split('/').filter(|part| !part.is_empty()).all(|part| {
                part != "."
                    && part != ".."
                    && !part.contains(':')
                    && !part.bytes().any(|byte| byte < 32)
            }),
            "task namespace path requires ordinary components without colons or control characters"
        );
        Ok(())
    }
    /// An orphan owner lock is held throughout. No engine is opened and no WAL
    /// is discarded as part of a read: this is explicit, confirmed deletion.
    pub(crate) fn remove_recovered(
        lease: Arc<RetainedNamespaceLease>,
    ) -> Result<NamespaceRemovalReceipt> {
        let anchor = lease.recovery_anchor()?;
        Self::validate_namespace_path(anchor.path())?;
        let budget = TaskExtentBudget::new(lease.cap())?;
        let encoded = CString::new(
            anchor
                .path()
                .to_str()
                .context("workspace path requires UTF-8")?,
        )?;
        let (device, inode) = anchor.native_identity()?;
        let mut native = ptr::null_mut();
        // SAFETY: anchored identity, live budget and valid output pointers.
        bridge(|error| unsafe {
            capturefab_task_namespace_new(
                encoded.as_ptr(),
                device,
                inode,
                budget.native.as_ptr(),
                &mut native,
                error,
            )
        })?;
        let mut data = Self {
            conn: None,
            database: ptr::null_mut(),
            native: NonNull::new(native).context("missing recovery namespace")?,
            budget,
            anchor,
            retained: Some(lease),
            close_uncertain: false,
            removed: false,
            mode: TaskMode::Empty,
            model_projection: false,
            catalog: None,
        };
        data.remove_reserved()
    }
    pub fn create(parent: &Path, budget: Arc<TaskExtentBudget>) -> Result<Self> {
        ensure!(
            cfg!(unix),
            "isolated task storage is unsupported on this platform; shared dataset workflows remain available"
        );
        crate::storage::ensure_not_task_namespace(parent)?;
        let parent = parent
            .canonicalize()
            .context("resolve task storage parent")?;
        Self::validate_namespace_path(&parent)?;
        budget.claim_namespace(false)?;
        let path = parent.join(format!("task-{}", super::db::random_name()?));
        let anchor = OutputDirectoryPlan::prepare(&path, Some(&parent))?.publish()?;
        Self::open_anchor(anchor, budget, None)
    }
    /// Create a camera catalog workspace whose full storage allowance remains
    /// charged after the engine closes or the process exits. Recovery commands
    /// explicitly preserve or remove these retained files.
    pub fn create_retained(
        parent: &Path,
        policy: &crate::storage::StoragePolicy,
        max_bytes: u64,
    ) -> Result<Self> {
        ensure!(
            cfg!(unix),
            "isolated task storage is unsupported on this platform"
        );
        let budget = TaskExtentBudget::new(max_bytes)?;
        let lease = crate::storage::reserve_task_namespace(parent, policy, max_bytes)?;
        match Self::create_reserved(lease.clone(), budget) {
            Ok(dataset) => Ok(dataset),
            Err(error) => {
                // Only an unconsumed admission can be cancelled here. Once the
                // engine has claimed it, create_reserved owns native cleanup
                // and a failed or uncertain cleanup must retain its full charge.
                if let Ok(receipt) = lease.cancel_unclaimed() {
                    lease.confirm_removed(&receipt).with_context(|| {
                        format!(
                            "task setup failed: {error:#}; namespace removal could not be journaled"
                        )
                    })?;
                }
                Err(error)
            }
        }
    }
    /// The global storage reservation is durable before the same fresh anchor
    /// is handed to the native engine. Its full cap survives close and Drop.
    pub(crate) fn create_reserved(
        lease: Arc<RetainedNamespaceLease>,
        budget: Arc<TaskExtentBudget>,
    ) -> Result<Self> {
        ensure!(
            cfg!(unix),
            "isolated task storage is unsupported on this platform; shared dataset workflows remain available"
        );
        ensure!(
            budget.usage()?.limit == lease.cap(),
            "task extent cap must match the durable namespace reservation"
        );
        Self::validate_namespace_path(lease.anchor().path())?;
        budget.claim_namespace(true)?;
        let anchor = lease.claim_for_engine()?;
        Self::open_anchor(anchor, budget, Some(lease))
    }
    fn open_anchor(
        anchor: Arc<OutputDirectory>,
        budget: Arc<TaskExtentBudget>,
        retained: Option<Arc<RetainedNamespaceLease>>,
    ) -> Result<Self> {
        let reservation = retained
            .as_ref()
            .map(|lease| format!(" reservation {}", lease.id()))
            .unwrap_or_default();
        let encoded = CString::new(
            anchor
                .path()
                .to_str()
                .with_context(|| {
                    format!(
                        "task namespace{reservation} retained at {}; namespace requires UTF-8 and has no native cleanup proof",
                        anchor.path().display()
                    )
                })?,
        )
        .with_context(|| {
            format!(
                "task namespace{reservation} retained at {}; namespace encoding failed without native cleanup proof",
                anchor.path().display()
            )
        })?;
        let (expected_device, expected_inode) = anchor.native_identity()?;
        let mut native = ptr::null_mut();
        // SAFETY: live namespace locator, shared budget and writable outputs.
        bridge(|error| unsafe {
            capturefab_task_namespace_new(
                encoded.as_ptr(),
                expected_device,
                expected_inode,
                budget.native.as_ptr(),
                &mut native,
                error,
            )
        })
        .with_context(|| {
            format!(
                "task namespace{reservation} retained at {}; no native cleanup proof is available",
                anchor.path().display()
            )
        })?;
        let mut dataset = Self {
            conn: None,
            database: ptr::null_mut(),
            native: NonNull::new(native).context("missing task namespace")?,
            budget,
            anchor,
            retained,
            close_uncertain: false,
            removed: false,
            mode: TaskMode::Empty,
            model_projection: false,
            catalog: None,
        };
        let result = (|| -> Result<()> {
            // SAFETY: the native namespace and budget outlive the new engine.
            bridge(|error| unsafe {
                capturefab_task_database_open(dataset.native.as_ptr(), &mut dataset.database, error)
            })?;
            // SAFETY: successful open returned a valid database; open_from_raw
            // explicitly does not own it. We drop every Connection first.
            dataset.conn = Some(unsafe { Connection::open_from_raw(dataset.database) }?);
            let root = super::db::sql_text(&dataset.path().to_string_lossy());
            dataset.connection()?.execute_batch(&format!(
                "SET temp_directory = '{root}/spill'; SET max_temp_directory_size = '{}B'; SET allowed_directories = ['{root}/']; SET enable_external_access = false; SET lock_configuration = true;",
                dataset.budget.usage()?.limit
            ))?;
            Ok(())
        })();
        if let Err(error) = result {
            let path = dataset.path().to_owned();
            if let Some(lease) = dataset.retained.clone() {
                let cleanup = dataset
                    .remove_reserved()
                    .and_then(|receipt| lease.confirm_removed(&receipt));
                return match cleanup {
                    Ok(()) => Err(error.context(format!(
                        "task setup failed; namespace reservation {} at {} was removed",
                        lease.id(),
                        path.display()
                    ))),
                    Err(cleanup) => Err(error.context(format!(
                        "task namespace reservation {} retained at {}; cleanup failed: {cleanup:#}",
                        lease.id(),
                        path.display()
                    ))),
                };
            }
            return Err(error.context(format!("task namespace retained at {}", path.display())));
        }
        Ok(dataset)
    }
    fn connection(&self) -> Result<&Connection> {
        self.conn.as_ref().context("task database is closed")
    }
    pub fn path(&self) -> &Path {
        self.anchor.path()
    }
    pub fn database_path(&self) -> PathBuf {
        self.path().join("task.duckdb")
    }
    pub fn usage(&self) -> Result<ExtentUsage> {
        self.budget.usage()
    }
    /// Internal foundation API; callers still need query validation, output
    /// bounds and cancellation before exposing SQL through an assistant tool.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        ensure!(
            self.catalog.is_none(),
            "fresh task catalogs require the typed writer API"
        );
        Ok(self.connection()?.execute_batch(sql)?)
    }
    pub fn scalar_i64(&self, sql: &str) -> Result<i64> {
        ensure!(
            self.catalog.is_none(),
            "fresh task catalogs require the typed writer API"
        );
        Ok(self.connection()?.query_row(sql, [], |row| row.get(0))?)
    }
    pub fn checkpoint(&self) -> Result<()> {
        self.execute_batch("CHECKPOINT")
    }
    /// Project a fresh catalog into a separate sealed in-memory engine before
    /// any arbitrary model SQL runs. The caller retains this writer's exclusive
    /// borrow throughout; no connection or clone leaves the task namespace.
    /// Transfer caps and engine memory settings are not process RSS limits.
    pub fn query_model_bounded(
        &mut self,
        sql: &str,
        limit: u32,
        timeout_ms: u64,
        max_bytes: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<serde_json::Value> {
        let limits =
            super::model_snapshot::ModelQueryLimits::bounded(limit, timeout_ms, max_bytes)?;
        self.query_model_with_limits(sql, limits, cancel)
    }
    fn query_model_with_limits(
        &mut self,
        sql: &str,
        limits: super::model_snapshot::ModelQueryLimits,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<serde_json::Value> {
        ensure!(
            self.mode == TaskMode::Fresh && !self.close_uncertain,
            "model queries require a live, nonquarantined fresh task catalog"
        );
        let identity = self
            .catalog_identity()
            .context("task capture catalog is absent")?;
        let source =
            snapshot::Source::task(self.connection()?, &identity.dataset_id, &identity.slot_id);
        super::model_snapshot::query_source(source, sql, limits, cancel)
    }
    /// Local analysis only: SELECTs reuse the read-only, row, complete-JSON-byte
    /// and timeout checks, but on-disk engine metadata can expose task paths.
    /// Model/provider runtime queries must use Reader::query_model_bounded's
    /// portable boundary instead of this foundation API.
    pub fn query_bounded(
        &self,
        sql: &str,
        limit: u32,
        timeout_ms: u64,
        max_bytes: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<serde_json::Value> {
        ensure!(
            self.mode != TaskMode::Failed,
            "this task snapshot failed; remove its workspace before creating a new snapshot"
        );
        ensure!(
            self.model_projection,
            "model queries require a projected task snapshot"
        );
        ensure!(
            timeout_ms <= super::query::MAX_TIMEOUT_MS,
            "dataset query timeout must be at most 300000 milliseconds"
        );
        let timeout = if timeout_ms == 0 {
            super::query::DEFAULT_TIMEOUT_MS
        } else {
            timeout_ms
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout);
        let conn = self.connection()?;
        super::query::ConnectionQuery {
            conn,
            path: &self.database_path(),
            read_only: false,
            cancelled: cancel,
        }
        .query_bounded_with_watchdog(sql, limit, timeout_ms, max_bytes, |_, operation| {
            snapshot::with_watchdog(cancel, None, conn, deadline, operation)
        })
    }
    /// Close while the namespace and budget lease remain live. A failed close
    /// is never retried or followed by deleting files that may still be open.
    pub fn close(&mut self) -> Result<()> {
        ensure!(
            !self.close_uncertain,
            "task engine close is uncertain; retained files must not be removed"
        );
        drop(self.conn.take());
        if !self.database.is_null() {
            let mut database = std::mem::replace(&mut self.database, ptr::null_mut());
            // SAFETY: every Connection is gone, and ownership moves into this call.
            let result =
                bridge(|error| unsafe { capturefab_task_database_close(&mut database, error) });
            self.close_uncertain = result.is_err();
            if let Err(error) = &result {
                self.mode = TaskMode::Failed;
                if let Some(catalog) = &mut self.catalog {
                    catalog.record_error(error);
                }
            }
            result?;
        }
        if !self.removed
            && let Some(lease) = &self.retained
        {
            // A journal failure can be retried on the next close; native
            // shutdown already succeeded and the full storage cap stays held.
            lease.mark_retained()?;
        }
        Ok(())
    }
    /// Remove only this anchored namespace after engine close. Open/deleted or
    /// uncertain extents retain their charges until disappearance is proven.
    pub fn remove(mut self) -> Result<()> {
        if let Some(lease) = self.retained.clone() {
            let receipt = self.remove_reserved()?;
            lease.confirm_removed(&receipt)
        } else {
            self.remove_native()
        }
    }
    fn remove_native(&mut self) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        self.close()?;
        // SAFETY: the engine is closed and the native namespace owner is live.
        bridge(|error| unsafe { capturefab_task_namespace_remove(self.native.as_ptr(), error) })?;
        self.removed = true;
        Ok(())
    }
    /// Return proof only after definite native close and anchored removal. The
    /// registry must still journal the lease release with `confirm_removed`.
    pub(crate) fn remove_reserved(&mut self) -> Result<NamespaceRemovalReceipt> {
        ensure!(
            self.retained.is_some(),
            "this task namespace has no retained reservation"
        );
        self.remove_native()?;
        let usage = self.usage()?;
        ensure!(
            usage.used == 0 && usage.uncertain_files == 0 && usage.poisoned == 0,
            "task namespace removal cannot prove that all native extents are released"
        );
        self.anchor.removed_receipt()
    }
}
impl Drop for TaskDataset {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            crate::eprintln!(
                "capturefab: task storage retained at {}: {error:#}",
                self.path().display()
            );
        }
        // SAFETY: filesystem/handle leases have independent shared ownership.
        unsafe { capturefab_task_namespace_free(self.native.as_ptr()) };
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn parent(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "capturefab-task-{label}-{}-{}",
            std::process::id(),
            super::super::db::random_name().unwrap()
        ));
        crate::ipc::ensure_private_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }
    fn extents(path: &Path) -> u64 {
        std::fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    extents(&path)
                } else {
                    path.metadata().unwrap().len()
                }
            })
            .sum()
    }
    #[test]
    fn unsupported_parent_is_refused_before_engine_budget_admission() {
        use std::os::unix::ffi::OsStringExt;
        let root = parent("path-preflight");
        for name in [
            std::ffi::OsString::from("capture:root"),
            std::ffi::OsString::from_vec(b"capture-\xff-root".to_vec()),
        ] {
            let invalid = root.join(name);
            let created = match std::fs::create_dir(&invalid) {
                Ok(()) => true,
                Err(error) => {
                    // APFS cannot materialize this fixture, unlike Unix
                    // filesystems which support arbitrary filename bytes.
                    assert!(
                        invalid.to_str().is_none() && error.raw_os_error() == Some(libc::EILSEQ),
                        "cannot create unsupported-parent fixture: {error}"
                    );
                    false
                }
            };
            assert!(TaskDataset::validate_namespace_path(&invalid).is_err());
            let budget = TaskExtentBudget::new(8 << 20).unwrap();
            assert!(TaskDataset::create(&invalid, budget.clone()).is_err());
            if created {
                assert!(std::fs::read_dir(&invalid).unwrap().next().is_none());
            } else {
                assert!(!invalid.exists());
            }
            // Rejection must not consume this budget's one-shot admission.
            TaskDataset::create(&root, budget)
                .unwrap()
                .remove()
                .unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn linked_bridge_opens_queries_and_closes_a_private_engine() {
        let parent = parent("bridge");
        let budget = TaskExtentBudget::new(8 * 1024 * 1024).unwrap();
        let mut dataset = TaskDataset::create(&parent, budget.clone()).unwrap();
        dataset
            .execute_batch("CREATE TABLE proof AS SELECT 42 AS answer")
            .unwrap();
        dataset.checkpoint().unwrap();
        assert_eq!(dataset.scalar_i64("SELECT answer FROM proof").unwrap(), 42);
        assert!(dataset.usage().unwrap().used >= extents(dataset.path()));
        dataset.close().unwrap();
        assert!(dataset.usage().unwrap().peak <= 8 * 1024 * 1024);
        assert!(dataset.database_path().exists());
        dataset.remove().unwrap();
        assert_eq!(budget.usage().unwrap().used, 0);
        std::fs::remove_dir(parent).unwrap();
    }
    #[test]
    fn engine_creation_refuses_to_extend_past_a_tiny_shared_budget() {
        let parent = parent("tiny");
        let budget = TaskExtentBudget::new(1024).unwrap();
        let error = TaskDataset::create(&parent, budget.clone()).err().unwrap();
        assert!(format!("{error:#}").contains("extent limit"), "{error:#}");
        let usage = budget.usage().unwrap();
        assert!(usage.denied > 0);
        assert!(usage.peak <= 1024);
        assert!(extents(&parent) <= 1024);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn native_admission_refuses_a_replacement_of_the_original_anchored_directory() {
        let parent = parent("replaced-admission");
        let path = parent.join("namespace");
        let moved = parent.join("moved-original");
        let anchor = OutputDirectoryPlan::prepare(&path, Some(&parent))
            .unwrap()
            .publish()
            .unwrap();
        let identity = anchor.native_identity().unwrap();
        std::fs::rename(&path, &moved).unwrap();
        crate::ipc::ensure_private_dir(&path).unwrap();
        assert_eq!(anchor.native_identity().unwrap(), identity);
        let budget = TaskExtentBudget::new(8 * 1024 * 1024).unwrap();
        let error = TaskDataset::open_anchor(anchor, budget.clone(), None)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("identity changed before native admission"),
            "{error:#}"
        );
        assert_eq!(budget.usage().unwrap().used, 0);
        assert!(std::fs::read_dir(&path).unwrap().next().is_none());
        assert!(std::fs::read_dir(&moved).unwrap().next().is_none());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn native_adapter_probes_cover_concurrency_partial_failure_and_unlinked_files() {
        let parent = parent("native-probes");
        let budget = TaskExtentBudget::new(128).unwrap();
        let path = parent.join("namespace");
        let anchor = OutputDirectoryPlan::prepare(&path, Some(&parent))
            .unwrap()
            .publish()
            .unwrap();
        let path = CString::new(anchor.path().to_str().unwrap()).unwrap();
        let (expected_device, expected_inode) = anchor.native_identity().unwrap();
        let mut namespace = ptr::null_mut();
        // SAFETY: valid live budget, namespace locator and writable output.
        bridge(|error| unsafe {
            capturefab_task_namespace_new(
                path.as_ptr(),
                expected_device,
                expected_inode,
                budget.native.as_ptr(),
                &mut namespace,
                error,
            )
        })
        .unwrap();
        // SAFETY: regression-only probe owns every opened test handle.
        let result = bridge(|error| unsafe { capturefab_task_fs_probes(namespace, error) });
        unsafe { capturefab_task_namespace_free(namespace) };
        result.unwrap();
        let usage = budget.usage().unwrap();
        assert_eq!(usage.used, 20, "uncertain close must retain its charge");
        assert_eq!(usage.uncertain_files, 1);
        assert_eq!(usage.poisoned, 1);
        assert!(usage.peak <= usage.limit);
        drop(anchor);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn real_engine_wal_and_checkpoint_cannot_overspend_the_extent_budget() {
        let parent = parent("engine-cap");
        let budget = TaskExtentBudget::new(4 * 1024 * 1024).unwrap();
        let mut dataset = TaskDataset::create(&parent, budget.clone()).unwrap();
        dataset
            .execute_batch("CREATE TABLE payload (id BIGINT, value VARCHAR)")
            .unwrap();
        let mut refused = false;
        for offset in 0..12 {
            let result = dataset.execute_batch(&format!(
                "INSERT INTO payload SELECT i, repeat(md5(i::VARCHAR),64) FROM range({}, {}) t(i)",
                offset * 1000,
                (offset + 1) * 1000
            ));
            if let Err(error) = result {
                assert!(format!("{error:#}").contains("extent limit"), "{error:#}");
                refused = true;
                break;
            }
            assert!(extents(dataset.path()) <= budget.usage().unwrap().limit);
        }
        assert!(
            refused,
            "real engine writes must eventually exhaust a 4 MiB lease"
        );
        let _ = dataset.checkpoint();
        dataset.close().unwrap();
        let usage = budget.usage().unwrap();
        assert!(usage.denied > 0);
        assert!(usage.wal_opens > 0);
        assert!(usage.peak <= usage.limit);
        assert!(extents(dataset.path()) <= usage.limit);
        dataset.remove().unwrap();
        assert_eq!(budget.usage().unwrap().used, 0);
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn isolated_engines_share_one_lease_until_their_retained_files_are_removed() {
        let parent = parent("shared-lease");
        let budget = TaskExtentBudget::new(8 * 1024 * 1024).unwrap();
        let first = TaskDataset::create(&parent, budget.clone()).unwrap();
        first
            .execute_batch("CREATE TABLE first AS SELECT 42 AS answer")
            .unwrap();
        first.checkpoint().unwrap();
        let first_bytes = budget.usage().unwrap().used;
        let second = TaskDataset::create(&parent, budget.clone()).unwrap();
        second
            .execute_batch("CREATE TABLE second AS SELECT 43 AS answer")
            .unwrap();
        second.checkpoint().unwrap();
        assert!(budget.usage().unwrap().used > first_bytes);
        assert!(budget.usage().unwrap().used >= extents(first.path()) + extents(second.path()));
        assert_eq!(first.scalar_i64("SELECT answer FROM first").unwrap(), 42);
        assert_eq!(second.scalar_i64("SELECT answer FROM second").unwrap(), 43);
        first.remove().unwrap();
        assert!(budget.usage().unwrap().used >= extents(second.path()));
        second.remove().unwrap();
        assert_eq!(budget.usage().unwrap().used, 0);
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn real_engine_sort_spills_only_through_the_bounded_namespace() {
        let parent = parent("spill");
        let budget = TaskExtentBudget::new(128 * 1024 * 1024).unwrap();
        let dataset = TaskDataset::create(&parent, budget.clone()).unwrap();
        dataset.execute_batch("CREATE TABLE source AS SELECT i, md5(i::VARCHAR) AS key FROM range(300000) t(i); CREATE TABLE sorted AS SELECT * FROM source ORDER BY key").unwrap();
        assert_eq!(
            dataset.scalar_i64("SELECT count(*) FROM sorted").unwrap(),
            300000
        );
        let usage = budget.usage().unwrap();
        assert!(
            usage.spill_opens > 0,
            "sort must exercise real bounded spill: {usage:?}"
        );
        assert!(usage.peak <= usage.limit);
        assert!(extents(dataset.path()) <= usage.limit);
        dataset.remove().unwrap();
        assert_eq!(budget.usage().unwrap().used, 0);
        std::fs::remove_dir(parent).unwrap();
    }
}
