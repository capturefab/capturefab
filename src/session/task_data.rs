//! Local coordinator capabilities for task-owned data. Public command JSON and
//! actor labels cannot select these workspaces or their persistence routes.
use super::{Coordinator, SessionHandle, locked};
use crate::{
    dataset::task::{TaskCatalogStatus, TaskDataset, TaskExtentBudget},
    signals::{
        CaptureLimits, Sample, SampleSink, SignalInfo, SignalSinkStatus, SourceStatus, TagInfo,
        auth::AuthBinding, hub::PrivateRoute,
    },
    storage::{RetainedNamespaceLease, StoragePolicy},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, TryLockError, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// This performs file and engine I/O; construct it on a worker, never in a view.
#[derive(Clone, Debug)]
pub struct TaskDataPolicy {
    pub parent: PathBuf,
    pub storage: StoragePolicy,
    /// Combined logical DB, WAL and spill EOF extents, reserved globally first.
    pub max_bytes: u64,
    /// Includes namespace reservation, engine initialization and capture startup.
    pub lifetime: Duration,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskDataPhase {
    Open,
    Closing,
    Retained,
    RetainedError,
    Removed,
}

/// A local receipt. Paths are for the trusted application; this is not a model
/// DTO and does not serialize the capability used to access the workspace.
#[derive(Clone, Debug, Serialize)]
pub struct TaskDataStatus {
    pub id: String,
    pub phase: TaskDataPhase,
    pub catalog: TaskCatalogStatus,
    pub sink: SignalSinkStatus,
    pub captures: BTreeMap<String, String>,
    pub error: Option<String>,
}

/// An unforgeable local sideband: neither actor IDs nor serialized slot IDs can
/// construct it. Dropping a client handle does not refund retained file charges.
#[derive(Clone)]
pub struct TaskDataHandle {
    registry: Weak<Registry>,
    entry: Arc<Entry>,
}

impl std::fmt::Debug for TaskDataHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskDataHandle")
            .field("id", &self.entry.id)
            .finish_non_exhaustive()
    }
}

pub(super) struct Registry {
    owner: Weak<Coordinator>,
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
}

struct Lifecycle {
    phase: TaskDataPhase,
    error: Option<String>,
}

struct Entry {
    id: String,
    deadline: Instant,
    route: PrivateRoute,
    sink: Arc<TaskSink>,
    lease: Arc<RetainedNamespaceLease>,
    lifecycle: Mutex<Lifecycle>,
    close_turn: Mutex<()>,
    captures: Mutex<BTreeMap<String, String>>,
    cancelled: AtomicBool,
}

/// The route writer owns calls into this sink. Keeping it separate from Entry
/// avoids an Entry -> route -> sink -> Entry reference cycle.
struct TaskSink {
    data: Mutex<Option<TaskDataset>>,
    cached: Mutex<TaskCatalogStatus>,
}

impl TaskSink {
    fn edit_until<T>(
        &self,
        deadline: Instant,
        cancel: &AtomicBool,
        apply: impl FnOnce(&mut TaskDataset, u64) -> Result<T>,
    ) -> Result<T> {
        let mut slot = loop {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "task data operation was cancelled"
            );
            ensure!(
                Instant::now() < deadline,
                "task data operation exceeded its deadline"
            );
            match self.data.try_lock() {
                Ok(slot) => break slot,
                Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
            }
        };
        ensure!(
            !cancel.load(Ordering::Acquire),
            "task data operation was cancelled"
        );
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        ensure!(remaining > 0, "task data operation exceeded its deadline");
        let data = slot
            .as_mut()
            .context("task data engine is no longer available")?;
        let result = apply(data, u64::try_from(remaining)?);
        if let Ok(status) = data.catalog_status() {
            *locked(&self.cached) = status;
        }
        let value = result?;
        ensure!(
            !cancel.load(Ordering::Acquire),
            "task data operation was cancelled"
        );
        ensure!(
            Instant::now() < deadline,
            "task data operation exceeded its deadline"
        );
        Ok(value)
    }

    fn remove_unstarted(&self, lease: &RetainedNamespaceLease) -> Result<()> {
        let mut slot = locked(&self.data);
        let data = slot
            .as_mut()
            .context("task data engine is no longer available")?;
        let receipt = data.remove_reserved()?;
        lease.confirm_removed(&receipt)?;
        slot.take();
        Ok(())
    }

    fn edit<T>(&self, apply: impl FnOnce(&mut TaskDataset) -> Result<T>) -> Result<T> {
        let mut slot = locked(&self.data);
        let data = slot
            .as_mut()
            .context("task data engine is no longer available")?;
        let result = apply(data);
        if let Ok(status) = data.catalog_status() {
            *locked(&self.cached) = status;
        }
        result
    }

    fn close(&self) -> Result<()> {
        self.edit(TaskDataset::close)
    }
}

impl SampleSink for TaskSink {
    fn dataset_slot_id(&self) -> Option<String> {
        Some(locked(&self.cached).identity.slot_id.clone())
    }
    fn register(&self, source: &SignalInfo, tags: &[TagInfo]) -> Result<()> {
        self.edit(|data| data.register(source, tags))
    }
    fn write(&self, source_id: &str, batch: &[Sample]) -> Result<()> {
        self.edit(|data| data.append_samples(source_id, batch).map(|_| ()))
    }
    fn event(&self, kind: &str, source_id: &str, text: &str) -> Result<()> {
        self.edit(|data| {
            data.record_event(
                kind,
                None,
                Some(source_id),
                text,
                &crate::context::Context::new(),
            )
            .map(|_| ())
        })
    }
    fn finish(&self) -> Result<()> {
        self.edit(TaskDataset::checkpoint_catalog)
    }
    fn describe(&self) -> String {
        "task-owned dataset".into()
    }
}

impl Entry {
    fn capture_receipt(&self, status: SourceStatus) -> Result<SourceStatus> {
        let lifecycle = locked(&self.lifecycle);
        if lifecycle.phase != TaskDataPhase::Removed {
            locked(&self.captures).insert(status.id.clone(), status.capture_id.clone());
        }
        ensure!(
            lifecycle.phase == TaskDataPhase::Open && Instant::now() < self.deadline,
            "task data admission closed while capture was starting; inspect its retained status"
        );
        Ok(status)
    }

    fn cleanup_turn(&self, deadline: Instant) -> Result<MutexGuard<'_, ()>> {
        loop {
            match self.close_turn.try_lock() {
                Ok(turn) => return Ok(turn),
                Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => bail!("task data cleanup is still running"),
            }
        }
    }

    fn status(&self) -> TaskDataStatus {
        let lifecycle = locked(&self.lifecycle);
        TaskDataStatus {
            id: self.id.clone(),
            phase: lifecycle.phase,
            catalog: locked(&self.sink.cached).clone(),
            sink: self.route.status(),
            captures: locked(&self.captures).clone(),
            error: lifecycle.error.clone(),
        }
    }

    fn begin_close(&self) {
        self.cancelled.store(true, Ordering::Release);
        {
            let mut lifecycle = locked(&self.lifecycle);
            if lifecycle.phase == TaskDataPhase::Open {
                lifecycle.phase = TaskDataPhase::Closing;
            }
        }
        self.route.begin_close();
    }

    fn remember_error(&self, error: &anyhow::Error, phase: TaskDataPhase) {
        let mut lifecycle = locked(&self.lifecycle);
        lifecycle.phase = phase;
        lifecycle.error.get_or_insert_with(|| format!("{error:#}"));
    }

    fn close(&self, wait: Duration) -> Result<TaskDataStatus> {
        self.begin_close();
        let deadline = Instant::now() + wait;
        let _turn = self.cleanup_turn(deadline)?;
        if matches!(
            locked(&self.lifecycle).phase,
            TaskDataPhase::Retained | TaskDataPhase::Removed
        ) {
            return Ok(self.status());
        }
        let drained = self
            .route
            .close(deadline.saturating_duration_since(Instant::now()));
        if !self.route.is_closed() {
            let error = drained
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("task data reader or writer has not finished"));
            self.remember_error(&error, TaskDataPhase::Closing);
            return Err(error);
        }
        if Instant::now() >= deadline {
            let error = anyhow::anyhow!("task data cleanup deadline reached before engine close");
            self.remember_error(&error, TaskDataPhase::Closing);
            return Err(error);
        }
        // A sink finish error can still carry a definite reader/writer fence.
        // Close the engine in that case, retaining both its error and files.
        let closed = self.sink.close();
        let retained = if closed.is_ok() {
            if Instant::now() >= deadline {
                Err(anyhow::anyhow!(
                    "task data engine closed; storage retention update is pending"
                ))
            } else {
                self.lease.mark_retained()
            }
        } else {
            Ok(())
        };
        let result = drained.and(closed).and(retained);
        match result {
            Ok(()) => {
                locked(&self.lifecycle).phase = TaskDataPhase::Retained;
                Ok(self.status())
            }
            Err(error) => {
                self.remember_error(&error, TaskDataPhase::RetainedError);
                Err(error)
            }
        }
    }
}

impl Registry {
    fn new(owner: &Arc<Coordinator>) -> Arc<Self> {
        Arc::new(Self {
            owner: Arc::downgrade(owner),
            entries: Mutex::default(),
        })
    }

    fn owner(&self) -> Result<Arc<Coordinator>> {
        let owner = self
            .owner
            .upgrade()
            .context("task data coordinator has closed")?;
        ensure!(
            !owner.quitting.load(Ordering::Acquire),
            "task data coordinator is shutting down"
        );
        Ok(owner)
    }

    fn check(&self, entry: &Arc<Entry>) -> Result<()> {
        let entries = locked(&self.entries);
        ensure!(
            entries
                .get(&entry.id)
                .is_some_and(|current| Arc::ptr_eq(current, entry)),
            "task data capability is no longer registered"
        );
        Ok(())
    }

    pub(super) fn begin_close_all(&self) {
        for entry in locked(&self.entries).values() {
            entry.begin_close();
        }
    }

    pub(super) fn close_all(&self, wait: Duration) {
        self.begin_close_all();
        let deadline = Instant::now() + wait;
        let entries: Vec<_> = locked(&self.entries).values().cloned().collect();
        for entry in entries {
            if let Err(error) = entry.close(deadline.saturating_duration_since(Instant::now())) {
                crate::eprintln!("capturefab: task data {} retained: {error:#}", entry.id);
            }
        }
    }
}

impl SessionHandle {
    /// Trusted application inventory returns real capabilities from this
    /// coordinator. No wire command can turn a serialized ID into one.
    #[cfg(feature = "gui")]
    pub(crate) fn retained_task_data(&self) -> Vec<TaskDataHandle> {
        self.inner
            .task_data
            .get()
            .map(|registry| {
                locked(&registry.entries)
                    .values()
                    .filter(|entry| locked(&entry.lifecycle).phase != TaskDataPhase::Removed)
                    .map(|entry| TaskDataHandle {
                        registry: Arc::downgrade(registry),
                        entry: entry.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Begin native task storage on a worker. No public SessionCommand accepts
    /// a task identifier, path or actor label as authority for this registry.
    pub fn begin_task_data(&self, policy: TaskDataPolicy) -> Result<TaskDataHandle> {
        let started = Instant::now();
        ensure!(
            !policy.lifetime.is_zero() && policy.lifetime <= Duration::from_secs(86_400),
            "task data lifetime must be positive and at most 24 hours"
        );
        ensure!(
            !policy.name.is_empty()
                && policy.name.len() <= 256
                && !policy.name.chars().any(char::is_control),
            "task catalog name must be 1..256 bytes without control characters"
        );
        let deadline = started + policy.lifetime;
        let registry = self
            .inner
            .task_data
            .get_or_init(|| Registry::new(&self.inner))
            .clone();
        let owner = registry.owner()?;
        let lease = crate::storage::reserve_task_namespace(
            &policy.parent,
            &policy.storage,
            policy.max_bytes,
        )?;
        let reservation = || {
            format!(
                "task namespace {} remains reserved at {}",
                lease.id(),
                lease.anchor().path().display()
            )
        };
        let budget = match (|| {
            registry.owner()?;
            ensure!(
                Instant::now() < deadline,
                "task data setup exceeded its deadline before engine admission"
            );
            TaskExtentBudget::new(lease.cap())
        })() {
            Ok(budget) => budget,
            Err(error) => {
                let cleanup = lease
                    .cancel_unclaimed()
                    .and_then(|receipt| lease.confirm_removed(&receipt));
                return Err(match cleanup {
                    Ok(()) => error.context("task budget setup failed; unused namespace removed"),
                    Err(cleanup) => {
                        error.context(format!("{}; cleanup: {cleanup:#}", reservation()))
                    }
                });
            }
        };
        let mut data = TaskDataset::create_reserved(lease.clone(), budget)
            .with_context(|| format!("initialize task namespace {}", lease.id()))?;
        let setup = (|| {
            data.initialize_catalog(&policy.name)?;
            ensure!(
                Instant::now() < deadline,
                "task data setup exceeded its deadline"
            );
            data.catalog_status()
        })();
        let status = match setup {
            Ok(status) => status,
            Err(error) => {
                // No route or source escaped. Refund only when the native
                // adapter proves close and removal, then the ledger accepts it.
                let cleanup = data
                    .remove_reserved()
                    .and_then(|receipt| lease.confirm_removed(&receipt));
                return Err(match cleanup {
                    Ok(()) => error.context("task data setup failed; unused namespace removed"),
                    Err(cleanup) => {
                        error.context(format!("{}; cleanup: {cleanup:#}", reservation()))
                    }
                });
            }
        };
        let id = status.identity.slot_id.clone();
        let sink = Arc::new(TaskSink {
            data: Mutex::new(Some(data)),
            cached: Mutex::new(status),
        });
        let route = match owner.signals.private_route_until(sink.clone(), deadline) {
            Ok(route) => route,
            Err(error) => {
                // Err means no private writer started or acquired this sink.
                return Err(match sink.remove_unstarted(&lease) {
                    Ok(()) => error.context("task route setup failed; unused namespace removed"),
                    Err(cleanup) => {
                        error.context(format!("{}; cleanup: {cleanup:#}", reservation()))
                    }
                });
            }
        };
        let entry = Arc::new(Entry {
            id: id.clone(),
            deadline,
            route,
            sink,
            lease,
            lifecycle: Mutex::new(Lifecycle {
                phase: TaskDataPhase::Open,
                error: None,
            }),
            close_turn: Mutex::new(()),
            captures: Mutex::default(),
            cancelled: AtomicBool::new(false),
        });
        locked(&registry.entries).insert(id, entry.clone());
        let watchdog_registry = registry.clone();
        let watchdog_entry = entry.clone();
        if let Err(error) = thread::Builder::new()
            .name("capturefab-task-data-deadline".into())
            .spawn(move || {
                loop {
                    if matches!(
                        locked(&watchdog_entry.lifecycle).phase,
                        TaskDataPhase::Retained
                            | TaskDataPhase::RetainedError
                            | TaskDataPhase::Removed
                    ) {
                        return;
                    }
                    let owner_closed = watchdog_registry
                        .owner
                        .upgrade()
                        .is_none_or(|owner| owner.quitting.load(Ordering::Acquire));
                    if owner_closed
                        || locked(&watchdog_entry.lifecycle).phase == TaskDataPhase::Closing
                        || Instant::now() >= watchdog_entry.deadline
                    {
                        let _ = watchdog_entry.close(Duration::from_secs(10));
                        return;
                    }
                    thread::sleep(
                        watchdog_entry
                            .deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(25)),
                    );
                }
            })
        {
            entry.begin_close();
            let _ = entry.close(Duration::from_secs(10));
            return Err(error.into());
        }
        Ok(TaskDataHandle {
            registry: Arc::downgrade(&registry),
            entry,
        })
    }

    /// Trusted local UI recovery can enumerate handles. Public RPC only sees
    /// normal shared commands and cannot turn these status IDs into handles.
    pub fn task_data_workspaces(&self) -> Vec<TaskDataHandle> {
        self.inner
            .task_data
            .get()
            .map_or_else(Vec::new, |registry| {
                locked(&registry.entries)
                    .values()
                    .map(|entry| TaskDataHandle {
                        registry: Arc::downgrade(registry),
                        entry: entry.clone(),
                    })
                    .collect()
            })
    }

    pub fn check_task_data(&self, handle: &TaskDataHandle) -> Result<()> {
        let registry = handle
            .registry
            .upgrade()
            .context("task data coordinator has closed")?;
        ensure!(
            self.inner
                .task_data
                .get()
                .is_some_and(|own| Arc::ptr_eq(own, &registry)),
            "task data belongs to another coordinator"
        );
        registry.check(&handle.entry)
    }
}

#[cfg(all(test, unix))]
#[path = "task_data_tests.rs"]
mod tests;

impl TaskDataHandle {
    #[cfg(feature = "gui")]
    pub(crate) fn save_copy(
        &self,
        output: &std::path::Path,
        policy: &crate::storage::StoragePolicy,
    ) -> Result<()> {
        let _turn = self
            .entry
            .cleanup_turn(Instant::now() + Duration::from_secs(10))?;
        let status = self.status();
        ensure!(
            status.catalog.closed
                && !status.catalog.quarantined
                && status.phase != TaskDataPhase::Removed,
            "Task engine must be definitely closed before saving a copy."
        );
        crate::storage::copy_task_namespace(
            &self.entry.lease.anchor(),
            self.entry.lease.cap(),
            output,
            policy,
        )
    }
    #[cfg(feature = "gui")]
    pub(crate) fn verify_workspace(&self) -> Result<()> {
        self.entry.lease.anchor().verify()
    }
    /// Signals the coordinator's watchdog; never waits or performs engine I/O.
    pub fn request_close(&self) {
        self.entry.begin_close();
    }

    fn operation_deadline(&self, slot: &str, timeout_ms: u64) -> Result<Instant> {
        let registry = self
            .registry
            .upgrade()
            .context("task data coordinator has closed")?;
        registry.check(&self.entry)?;
        registry.owner()?;
        ensure!(
            locked(&self.entry.lifecycle).phase == TaskDataPhase::Open,
            "task data admission has closed"
        );
        ensure!(
            locked(&self.entry.sink.cached).identity.slot_id == slot,
            "task data dataset instance changed"
        );
        ensure!(
            !self.entry.cancelled.load(Ordering::Acquire),
            "task data operation was cancelled"
        );
        let deadline =
            (Instant::now() + Duration::from_millis(timeout_ms)).min(self.entry.deadline);
        ensure!(Instant::now() < deadline, "task data deadline has passed");
        Ok(deadline)
    }

    pub fn model_query(
        &self,
        slot: &str,
        sql: &str,
        limit: u32,
        timeout_ms: u64,
        max_bytes: u32,
    ) -> Result<serde_json::Value> {
        ensure!(
            timeout_ms <= 300_000,
            "dataset query timeout must be at most 300000 milliseconds"
        );
        let deadline =
            self.operation_deadline(slot, if timeout_ms == 0 { 30_000 } else { timeout_ms })?;
        self.entry
            .sink
            .edit_until(deadline, &self.entry.cancelled, |data, remaining| {
                data.query_model_bounded(
                    sql,
                    limit,
                    remaining,
                    max_bytes as usize,
                    &self.entry.cancelled,
                )
            })
    }

    pub fn note(
        &self,
        slot: &str,
        text: &str,
        context: &crate::context::Context,
        camera: Option<&str>,
    ) -> Result<u64> {
        let deadline = self.operation_deadline(slot, 30_000)?;
        self.entry
            .sink
            .edit_until(deadline, &self.entry.cancelled, |data, _| {
                data.record_event("note", camera, None, text, context)
            })
    }

    pub fn export(
        &self,
        slot: &str,
        request: &crate::dataset::export::ExportRequest,
        max_bytes: u64,
        timeout_ms: u64,
        approved_root: Option<&Path>,
    ) -> Result<serde_json::Value> {
        ensure!(
            (1..=crate::dataset::export::MAX_TIMEOUT_MS).contains(&timeout_ms),
            "dataset export timeout must be 1..600000 milliseconds"
        );
        let deadline = self.operation_deadline(slot, timeout_ms)?;
        self.entry
            .sink
            .edit_until(deadline, &self.entry.cancelled, |data, remaining| {
                data.export_bounded(
                    request,
                    max_bytes,
                    remaining,
                    approved_root,
                    &self.entry.cancelled,
                )
            })
            .with_context(|| {
                format!(
                    "Task export did not acknowledge completion; files may already be saved in {}. Review that folder before retrying.",
                    request.output.display()
                )
            })
    }

    pub fn status(&self) -> TaskDataStatus {
        self.entry.status()
    }

    pub fn capture(
        &self,
        uri: &str,
        interval_ms: u64,
        tags: Vec<String>,
        limits: CaptureLimits,
        auth: Option<&AuthBinding>,
    ) -> Result<SourceStatus> {
        let registry = self
            .registry
            .upgrade()
            .context("task data coordinator has closed")?;
        registry.check(&self.entry)?;
        let owner = registry.owner()?;
        ensure!(
            locked(&self.entry.lifecycle).phase == TaskDataPhase::Open,
            "task data admission has closed"
        );
        ensure!(
            Instant::now() < self.entry.deadline,
            "task data deadline has passed"
        );
        let status = owner.signals.capture_to_private(
            &self.entry.route,
            uri,
            interval_ms,
            tags,
            limits,
            auth,
        )?;
        self.entry.capture_receipt(status)
    }

    pub fn stop_capture(&self, source_id: &str, capture_id: &str, wait: Duration) -> Result<()> {
        self.stop_capture_status(source_id, capture_id, wait)
            .map(|_| ())
    }

    pub fn stop_capture_status(
        &self,
        source_id: &str,
        capture_id: &str,
        wait: Duration,
    ) -> Result<Vec<SourceStatus>> {
        let registry = self
            .registry
            .upgrade()
            .context("task data coordinator has closed")?;
        registry.check(&self.entry)?;
        let owner = registry.owner()?;
        ensure!(
            locked(&self.entry.captures)
                .get(source_id)
                .map(String::as_str)
                == Some(capture_id),
            "this capture is not owned by the task data handle"
        );
        owner.signals.stop_if(source_id, capture_id, wait)
    }

    pub fn close(&self, wait: Duration) -> Result<TaskDataStatus> {
        self.entry.close(wait)
    }

    /// Explicit cleanup never refunds before native anchored deletion and a
    /// storage receipt. A failed close/deletion keeps the reservation charged.
    pub fn remove(&self, wait: Duration) -> Result<TaskDataStatus> {
        if locked(&self.entry.lifecycle).phase == TaskDataPhase::Removed {
            return Ok(self.status());
        }
        if let Some(registry) = self.registry.upgrade()
            && let Err(error) = registry.check(&self.entry)
        {
            if locked(&self.entry.lifecycle).phase == TaskDataPhase::Removed {
                return Ok(self.status());
            }
            return Err(error);
        }
        let deadline = Instant::now() + wait;
        let closed = self.entry.close(wait);
        ensure!(
            self.entry.route.is_closed(),
            "task data readers or writer have not finished; files remain reserved"
        );
        let _turn = self.entry.cleanup_turn(deadline)?;
        if locked(&self.entry.lifecycle).phase == TaskDataPhase::Removed {
            return Ok(self.status());
        }
        let result = (|| {
            ensure!(
                Instant::now() < deadline,
                "task data cleanup deadline reached; files remain reserved"
            );
            let mut slot = locked(&self.entry.sink.data);
            let data = slot
                .as_mut()
                .context("task data engine is no longer available")?;
            let receipt = data.remove_reserved()?;
            let status = data.catalog_status()?;
            ensure!(
                Instant::now() < deadline,
                "task namespace removed; storage reservation release is pending"
            );
            self.entry.lease.confirm_removed(&receipt)?;
            *locked(&self.entry.sink.cached) = status;
            slot.take();
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = result {
            self.entry
                .remember_error(&error, TaskDataPhase::RetainedError);
            return Err(error);
        }
        // A prior persistence failure stays visible even after successful,
        // explicitly requested removal. Cleanup cannot turn it into success.
        if let Err(error) = closed {
            self.entry
                .remember_error(&error, TaskDataPhase::RetainedError);
        }
        locked(&self.entry.lifecycle).phase = TaskDataPhase::Removed;
        if let Some(registry) = self.registry.upgrade() {
            let mut entries = locked(&registry.entries);
            if entries
                .get(&self.entry.id)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
            {
                entries.remove(&self.entry.id);
            }
        }
        Ok(self.status())
    }
}

#[cfg(all(test, unix))]
impl TaskDataHandle {
    pub(crate) fn capture_with_reader(
        &self,
        uri: &str,
        limits: CaptureLimits,
        reader: Box<dyn std::io::BufRead + Send>,
    ) -> Result<SourceStatus> {
        let registry = self
            .registry
            .upgrade()
            .context("task data coordinator has closed")?;
        registry.check(&self.entry)?;
        let owner = registry.owner()?;
        let status =
            owner
                .signals
                .capture_to_private_with_reader(&self.entry.route, uri, limits, reader)?;
        self.entry.capture_receipt(status)
    }
}
