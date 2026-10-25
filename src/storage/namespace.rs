//! Durable promises for private task namespaces. Native extent admission is
//! bounded by this upfront promise; ordinary file finalization cannot refund it.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum NamespacePhase {
    Pending,
    Active,
    Retained,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NamespaceRecord {
    cap: u64,
    phase: NamespacePhase,
    uncertain: bool,
    parent_identity: Identity,
}

/// Owned by the coordinator and its engine, not a caller's connection handle.
/// Drop closes the liveness lock but never releases the durable full cap.
/// This first slice deliberately does not shrink reservations after close.
pub(crate) struct RetainedNamespaceLease {
    manager: Manager,
    id: String,
    cap: u64,
    anchor: Arc<OutputDirectory>,
    _lease: File,
    lease_path: PathBuf,
    admission: AtomicU8,
    released: AtomicBool,
    recovery: bool,
}
impl RetainedNamespaceLease {
    pub(crate) fn recovery_anchor(&self) -> Result<Arc<OutputDirectory>> {
        ensure!(self.recovery, "namespace was not acquired for recovery");
        self.anchor.verify()?;
        let locked = self.manager.lock()?;
        self.entry(&locked)?;
        Ok(self.anchor.clone())
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn cap(&self) -> u64 {
        self.cap
    }
    pub(crate) fn anchor(&self) -> Arc<OutputDirectory> {
        self.anchor.clone()
    }
    pub(crate) fn claim_for_engine(&self) -> Result<Arc<OutputDirectory>> {
        self.claim_fresh()
    }
    /// Rust-managed export staging uses the same exclusive one-shot admission;
    /// its owner must close every file and prove anchored removal before refund.
    pub(crate) fn claim_for_staging(&self) -> Result<Arc<OutputDirectory>> {
        self.claim_fresh()
    }
    fn claim_fresh(&self) -> Result<Arc<OutputDirectory>> {
        ensure!(
            !self.released.load(Ordering::Acquire),
            "task namespace reservation was released"
        );
        self.anchor.verify()?;
        self.admission
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow!("task namespace engine admission was already consumed"))?;
        ensure!(
            self.anchor.is_empty()?,
            "task namespace is no longer fresh; engine admission refused"
        );
        let locked = self.manager.lock()?;
        let entry = self.entry(&locked)?;
        ensure!(
            entry
                .namespace
                .as_ref()
                .is_some_and(|ns| ns.phase == NamespacePhase::Active && !ns.uncertain),
            "task namespace reservation is no longer active"
        );
        Ok(self.anchor.clone())
    }
    /// Consume a never-admitted namespace without opening an engine. The
    /// returned proof is reusable with confirm_removed if journaling is busy.
    /// Cancellation is retryable, but can never cancel an engine admission.
    pub(crate) fn cancel_unclaimed(&self) -> Result<NamespaceRemovalReceipt> {
        match self
            .admission
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(2) => {}
            Err(_) => bail!(
                "task namespace engine admission was consumed; native removal proof is required"
            ),
        }
        self.anchor.remove_empty()?;
        self.anchor.removed_receipt()
    }
    fn entry<'a>(&self, locked: &'a Locked) -> Result<&'a Entry> {
        let entry = locked
            .ledger
            .entries
            .iter()
            .find(|entry| entry.id == self.id)
            .context("task namespace reservation is absent from the storage ledger")?;
        ensure!(
            entry.path == self.anchor.path()
                && entry.reserved == self.cap
                && entry
                    .namespace
                    .as_ref()
                    .is_some_and(|ns| ns.cap == self.cap),
            "task namespace reservation identity changed"
        );
        ensure!(
            entry.identity == identity(&self.anchor.metadata()?),
            "task namespace directory identity changed"
        );
        Ok(entry)
    }
    /// The coordinator calls this only after definite engine shutdown. This
    /// changes inventory state, never the charge or eligibility for retention.
    pub(crate) fn mark_retained(&self) -> Result<()> {
        ensure!(
            !self.released.load(Ordering::Acquire),
            "task namespace reservation was released"
        );
        self.anchor.verify()?;
        let mut locked = self.manager.lock()?;
        self.entry(&locked)?;
        let index = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.id == self.id)
            .expect("verified entry");
        locked
            .entry_mut(index)
            .namespace
            .as_mut()
            .expect("namespace")
            .phase = NamespacePhase::Retained;
        locked.persist()
    }
    /// Reusable proof allows a coordinator to retry a failed ledger commit.
    /// The task adapter issues it only after native close/removal proves no
    /// retained or uncertain native extents remain.
    pub(crate) fn confirm_removed(&self, receipt: &NamespaceRemovalReceipt) -> Result<()> {
        ensure!(
            receipt.matches(&self.anchor),
            "task namespace removal receipt belongs to another reservation"
        );
        if self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut locked = self.manager.lock()?;
        if let Some(index) = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.id == self.id)
        {
            self.entry(&locked)?;
            locked.remove(index);
            locked.persist()?;
        }
        // A durable removal may have succeeded before its acknowledgement was
        // lost. The original receipt still proves this namespace is gone;
        // replayed absence needs no second refund or journal mutation.
        self.released.store(true, Ordering::Release);
        let _ = fs::remove_file(&self.lease_path);
        Ok(())
    }
}

/// Reserve a full logical extent cap durably, then publish exactly one fresh
/// anchored directory. A native engine must claim it before opening any files.
pub(crate) fn reserve_task_namespace(
    parent: &Path,
    policy: &StoragePolicy,
    cap: u64,
) -> Result<Arc<RetainedNamespaceLease>> {
    Manager::global().reserve_namespace(parent, policy, cap)
}

/// Local recovery authority acquired under the ledger and owner locks. Never
/// construct this from action-log JSON or a caller-supplied filesystem path.
pub(crate) struct RecoveredNamespace {
    lease: Arc<RetainedNamespaceLease>,
    removed: Mutex<Option<NamespaceRemovalReceipt>>,
}
impl RecoveredNamespace {
    pub(crate) fn save_copy(&self, output: &Path, policy: &StoragePolicy) -> Result<()> {
        let removed = self.removed.lock().unwrap_or_else(PoisonError::into_inner);
        ensure!(removed.is_none(), "Workspace was already removed.");
        self.verify()?;
        self.lease
            .manager
            .copy_namespace(&self.lease.anchor, self.lease.cap, output, policy)
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.lease.recovery_anchor().map(|_| ())
    }
    pub(crate) fn remove(&self) -> Result<()> {
        let mut receipt = self.removed.lock().unwrap_or_else(PoisonError::into_inner);
        if receipt.is_none() {
            *receipt = Some(crate::dataset::task::TaskDataset::remove_recovered(
                self.lease.clone(),
            )?);
        }
        self.lease
            .confirm_removed(receipt.as_ref().expect("removal proof"))
    }
}

pub(crate) struct NamespaceRecovery {
    pub id: String,
    pub path: PathBuf,
    pub cap: u64,
    pub error: Option<String>,
    pub handle: Option<Arc<RecoveredNamespace>>,
}

pub(crate) fn recover_task_namespaces() -> Result<Vec<NamespaceRecovery>> {
    Manager::global().recover_namespaces()
}

impl Manager {
    fn copy_namespace(
        &self,
        anchor: &Arc<OutputDirectory>,
        cap: u64,
        output: &Path,
        policy: &StoragePolicy,
    ) -> Result<()> {
        let plan = OutputDirectoryPlan::prepare(output, None)?;
        anchor.verify()?;
        let mut inputs = Vec::new();
        let mut total = 0u64;
        for name in ["task.duckdb", "task.duckdb.wal"] {
            let file = match anchor.open_file(std::ffi::OsStr::new(name)) {
                Ok(file) => file,
                Err(error) if name.ends_with(".wal") && not_found(&error) => continue,
                Err(error) => return Err(error),
            };
            let meta = file.metadata()?;
            ensure!(meta.is_file(), "Retained database is not a regular file.");
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(meta.nlink() == 1, "Retained database has additional links.");
            }
            total = total
                .checked_add(meta.len())
                .context("Workspace copy size overflow")?;
            ensure!(
                total <= cap,
                "Workspace files exceed their reservation; copy refused."
            );
            inputs.push((name, file, meta.len()));
        }
        {
            let locked = self.lock()?;
            ensure_write_allowed(&locked, plan.path())?;
            let effective = policy.constrained_by(&locked.configured_policy()?);
            ensure!(
                total <= effective.max_bytes.saturating_sub(locked.allocated()?),
                "Storage full: workspace copy needs {total} additional bytes."
            );
            ensure!(
                locked.ledger.entries.len().saturating_add(inputs.len())
                    <= effective.max_files as usize,
                "Storage full: workspace copy exceeds the file limit."
            );
        }
        let directory = plan.publish()?;
        let result = (|| -> Result<()> {
            // Keep every output leased until the complete DB/WAL pair has
            // been written. Retention must not evict the DB to admit its WAL.
            let mut writers = Vec::new();
            for (name, mut input, length) in inputs {
                anchor.verify()?;
                let mut writer = self.writer_anchored(
                    &directory.path().join(name),
                    policy,
                    length.max(1),
                    Some(directory.clone()),
                )?;
                let copied = io::copy(
                    &mut std::io::Read::by_ref(&mut input).take(length),
                    &mut writer,
                )?;
                ensure!(
                    copied == length && input.metadata()?.len() == length,
                    "Retained database changed while copying."
                );
                writers.push(writer);
            }
            for writer in writers {
                writer.finish()?;
            }
            Ok(())
        })();
        result.with_context(|| {
            format!(
                "Workspace copy at {} may be incomplete; inspect it before retrying.",
                output.display()
            )
        })
    }
    fn recover_namespaces(&self) -> Result<Vec<NamespaceRecovery>> {
        let mut locked = self.lock()?;
        let mut rows = Vec::new();
        for index in 0..locked.ledger.entries.len() {
            let entry = locked.ledger.entries[index].clone();
            let Some(namespace) = &entry.namespace else {
                continue;
            };
            let mut row = NamespaceRecovery {
                id: entry.id.clone(),
                path: entry.path.clone(),
                cap: namespace.cap,
                error: None,
                handle: None,
            };
            let acquired = (|| -> Result<Arc<RecoveredNamespace>> {
                ensure!(
                    entry.lease.is_none(),
                    "This workspace is still owned by a running session."
                );
                ensure!(
                    namespace.phase != NamespacePhase::Pending,
                    "Workspace publication did not finish; reservation retained for review."
                );
                ensure!(
                    parent_matches(&entry.base, &namespace.parent_identity),
                    "The original workspace parent changed; reservation retained."
                );
                let anchor = OutputDirectory::open_existing(&entry.path)?;
                ensure!(
                    identity(&anchor.metadata()?) == entry.identity,
                    "The original workspace directory changed; reservation retained."
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let meta = anchor.metadata()?;
                    ensure!(
                        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
                        "Retained workspace must remain private to its owner."
                    );
                }
                let name = format!("storage-{}.lease", entry.id);
                let lease_path = locked.directory.join(&name);
                let lease = match private_options().create_new(true).open(&lease_path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        regular_private(&lease_path, 0)?;
                        private_options().open(&lease_path)?
                    }
                    Err(error) => return Err(error.into()),
                };
                lease
                    .try_lock_exclusive()
                    .context("another recovery owns this workspace")?;
                let id = entry.id.clone();
                let cap = namespace.cap;
                locked.entry_mut(index).lease = Some(name);
                locked.persist()?;
                Ok(Arc::new(RecoveredNamespace {
                    lease: Arc::new(RetainedNamespaceLease {
                        manager: Manager {
                            directory: locked.directory.clone(),
                        },
                        id,
                        cap,
                        anchor,
                        _lease: lease,
                        lease_path,
                        admission: AtomicU8::new(1),
                        released: AtomicBool::new(false),
                        recovery: true,
                    }),
                    removed: Mutex::new(None),
                }))
            })();
            match acquired {
                Ok(handle) => row.handle = Some(handle),
                Err(error) => row.error = Some(format!("{error:#}")),
            }
            rows.push(row);
        }
        Ok(rows)
    }
    fn reserve_namespace(
        &self,
        parent: &Path,
        policy: &StoragePolicy,
        cap: u64,
    ) -> Result<Arc<RetainedNamespaceLease>> {
        ensure!(
            cfg!(unix),
            "task namespace reservations are unsupported on this platform"
        );
        policy.validate()?;
        ensure!(
            cap > 0,
            "task namespace extent reservation must be positive"
        );
        let approved_parent =
            std::path::absolute(parent).context("resolve approved task namespace parent")?;
        crate::dataset::task::TaskDataset::validate_namespace_path(&resolved_candidate(
            &approved_parent,
        )?)?;
        let mut locked = self.lock()?;
        ensure_write_allowed(&locked, &approved_parent)?;
        let policy = policy.constrained_by(&locked.configured_policy()?);
        ensure!(
            cap <= policy.max_bytes,
            "Storage full: task namespace extent cap exceeds the configured storage byte budget"
        );
        crate::volumes::ensure_present(&approved_parent)?;
        // A first-run capture root need not exist. Create only this approved
        // parent through the existing held-ancestor publication helper; normal
        // existing roots need not be private and are never chmodded here.
        let parent_anchor = match approved_parent.canonicalize() {
            Ok(parent) => {
                ensure!(parent.is_dir(), "task namespace parent is not a directory");
                None
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let plan = OutputDirectoryPlan::prepare(&approved_parent, None)?;
                crate::dataset::task::TaskDataset::validate_namespace_path(plan.path())?;
                // prepare pins aliases to their physical target. Recheck that
                // actual target under the same storage lock before mkdir.
                ensure_write_allowed(&locked, plan.path())?;
                crate::volumes::ensure_present(plan.path())?;
                Some(plan.publish()?)
            }
            Err(error) => return Err(error).context("resolve task namespace parent"),
        };
        let parent = if let Some(anchor) = &parent_anchor {
            anchor.verify()?;
            anchor.path().to_owned()
        } else {
            approved_parent
                .canonicalize()
                .context("resolve task namespace parent")?
        };
        crate::dataset::task::TaskDataset::validate_namespace_path(&parent)?;
        ensure_write_allowed(&locked, &parent)?;
        locked.make_room(
            &policy,
            cap,
            &parent,
            device(&parent),
            super::SaveKind::File,
        )?;
        let id = nonce()?;
        let parent_identity = identity(&fs::metadata(&parent)?);
        let path = parent.join(format!("task-{id}"));
        let plan = OutputDirectoryPlan::prepare(&path, Some(&parent))?;
        let lease_name = format!("storage-{id}.lease");
        let lease_path = locked.directory.join(&lease_name);
        let lease = private_options().create_new(true).open(&lease_path)?;
        lease.lock_exclusive()?;
        locked.push(Entry {
            id: id.clone(),
            path: path.clone(),
            base: parent.clone(),
            identity: parent_identity.clone(),
            created: now(),
            bytes: 0,
            reserved: cap,
            lease: Some(lease_name),
            sha256: None,
            modified_ns: None,
            changed: true,
            pending_upload: false,
            namespace: Some(NamespaceRecord {
                cap,
                phase: NamespacePhase::Pending,
                uncertain: false,
                parent_identity: parent_identity.clone(),
            }),
        });
        // No namespace or engine exists until the global promise is durable.
        locked.persist()?;
        if let Some(parent_anchor) = &parent_anchor {
            parent_anchor.verify().with_context(|| format!(
                "task namespace reservation {id} retained at {}; approved parent changed before publication",
                path.display()
            ))?;
        }
        let anchor = match plan.publish() {
            Ok(anchor) => anchor,
            Err(error) => {
                // No engine admission escaped. A definitely unpublished path
                // may refund; an existing/uncertain directory stays charged.
                if fs::symlink_metadata(&path)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                    && parent_matches(&parent, &parent_identity)
                {
                    let index = locked
                        .ledger
                        .entries
                        .iter()
                        .position(|entry| entry.id == id)
                        .expect("pending entry");
                    locked.remove(index);
                    locked.persist()?;
                    let _ = fs::remove_file(&lease_path);
                }
                return Err(error).with_context(|| format!("task namespace reservation {id}: inspect storage status for retained charges"));
            }
        };
        // A newly created parent remains held across namespace publication.
        // Never hand an engine a directory admitted under a replacement root.
        if let Some(parent_anchor) = &parent_anchor {
            parent_anchor.verify().with_context(|| {
                format!(
                    "task namespace reservation {id} retained at {}; approved parent changed",
                    path.display()
                )
            })?;
        }
        ensure!(
            parent_matches(&parent, &parent_identity),
            "task namespace reservation {id} retained at {}; original parent changed",
            path.display()
        );
        anchor.verify()?;
        let index = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .expect("pending entry");
        let entry = locked.entry_mut(index);
        entry.identity = identity(&anchor.metadata()?);
        entry.namespace.as_mut().expect("namespace").phase = NamespacePhase::Active;
        locked.persist().with_context(|| {
            format!(
                "task namespace retained without engine admission at {}",
                path.display()
            )
        })?;
        Ok(Arc::new(RetainedNamespaceLease {
            manager: Manager {
                directory: locked.directory.clone(),
            },
            id,
            cap,
            anchor,
            _lease: lease,
            lease_path,
            admission: AtomicU8::new(0),
            released: AtomicBool::new(false),
            recovery: false,
        }))
    }
}

pub(super) fn validate_entry(entry: &Entry) -> Result<()> {
    if let Some(namespace) = &entry.namespace {
        ensure!(
            namespace.cap > 0
                && entry.reserved == namespace.cap
                && entry.bytes == 0
                && entry.sha256.is_none()
                && entry.changed
                && !entry.pending_upload,
            "invalid retained task namespace reservation"
        );
    }
    Ok(())
}

pub(super) struct ReconciledNamespace {
    pub changed: bool,
    pub unpublished: bool,
}

fn parent_matches(path: &Path, expected: &Identity) -> bool {
    fs::canonicalize(path).is_ok_and(|current| current == path)
        && fs::symlink_metadata(path).is_ok_and(|meta| {
            meta.is_dir() && !meta.file_type().is_symlink() && identity(&meta) == *expected
        })
}

pub(super) fn reconcile_entry(entry: &mut Entry, directory: &Path) -> Result<ReconciledNamespace> {
    let namespace = entry.namespace.as_mut().expect("namespace entry");
    let mut touched = false;
    let parent_intact = parent_matches(&entry.base, &namespace.parent_identity);
    let intact = if namespace.phase == NamespacePhase::Pending {
        parent_intact
    } else {
        fs::symlink_metadata(&entry.path).is_ok_and(|meta| {
            meta.is_dir() && !meta.file_type().is_symlink() && identity(&meta) == entry.identity
        }) && parent_intact
    };
    if !intact && !namespace.uncertain {
        namespace.uncertain = true;
        touched = true;
    }
    if let Some(name) = &entry.lease {
        let path = directory.join(name);
        let stale = match regular_private(&path, 0) {
            Ok(_) => {
                let lease = private_options().open(&path)?;
                match lease.try_lock_exclusive() {
                    Ok(()) => true,
                    Err(error) if contended(&error) => false,
                    Err(error) => return Err(error).context("inspect task namespace liveness"),
                }
            }
            Err(error) if not_found(&error) => true,
            Err(error) => return Err(error),
        };
        if stale {
            entry.lease = None;
            if namespace.phase != NamespacePhase::Retained {
                namespace.uncertain = true;
            }
            let _ = fs::remove_file(path);
            touched = true;
        }
    }
    // Publication holds storage.lock continuously through Pending -> Active,
    // and holds this separate liveness lease throughout. With both fences
    // clear, a replayed Pending entry cannot have admitted an engine. Missing
    // child + unchanged parent therefore reclaims only an unused promise.
    let unpublished = entry.lease.is_none()
        && namespace.phase == NamespacePhase::Pending
        && parent_intact
        && fs::symlink_metadata(&entry.path)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
    Ok(ReconciledNamespace {
        changed: touched,
        unpublished,
    })
}

pub(super) fn status(ledger: &Ledger) -> Vec<Value> {
    ledger.entries.iter().filter_map(|entry| entry.namespace.as_ref().map(|namespace| json!({
        "id": entry.id, "path": entry.path, "logical_extent_cap": namespace.cap,
        "allocated_bytes": entry.reserved, "phase": namespace.phase,
        "owner_live": entry.lease.is_some(), "uncertain": namespace.uncertain,
        "release": "Only confirmed anchored namespace removal releases this full-cap reservation; no automatic orphan deletion or close-time shrink."
    }))).collect()
}

fn resolved_candidate(path: &Path) -> Result<PathBuf> {
    let mut current = std::path::absolute(path)?;
    let mut tail = Vec::new();
    loop {
        match current.canonicalize() {
            Ok(mut resolved) => {
                for component in tail.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ensure!(
                    !fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink()),
                    "cannot resolve a dangling task namespace alias"
                );
                tail.push(
                    current
                        .file_name()
                        .context("output path has no existing ancestor")?
                        .to_owned(),
                );
                ensure!(current.pop(), "output path has no existing ancestor");
            }
            Err(error) => return Err(error).context("resolve output path before namespace guard"),
        }
    }
}

pub(super) fn ensure_write_allowed(locked: &Locked, path: &Path) -> Result<()> {
    let namespaces: Vec<_> = locked
        .ledger
        .entries
        .iter()
        .filter(|entry| entry.namespace.is_some())
        .collect();
    if namespaces.is_empty() {
        return Ok(());
    }
    let resolved = resolved_candidate(path)?;
    let identities = resolved
        .ancestors()
        .filter_map(|ancestor| fs::symlink_metadata(ancestor).ok())
        .filter(Metadata::is_dir)
        .map(|meta| identity(&meta))
        .collect::<Vec<_>>();
    ensure!(
        !namespaces
            .iter()
            .any(|entry| resolved.starts_with(&entry.path)
                || entry
                    .namespace
                    .as_ref()
                    .is_some_and(|namespace| namespace.phase != NamespacePhase::Pending)
                    && identities.contains(&entry.identity)),
        "output belongs to a reserved task namespace; ordinary writable dataset and capture operations are refused"
    );
    Ok(())
}

/// Call before a normal writable dataset open, before creation/recovery. Native
/// task engines use their one-shot lease admission instead of this API.
pub(crate) fn ensure_not_task_namespace(path: &Path) -> Result<()> {
    let locked = Manager::global().lock()?;
    ensure_write_allowed(&locked, path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn native_incompatible_parents_are_refused_before_publication_or_reservation() {
        use std::os::unix::ffi::OsStringExt;
        let temp = Temp::new();
        for name in [
            std::ffi::OsString::from("capture:root"),
            std::ffi::OsString::from_vec(b"capture-\xff-root".to_vec()),
        ] {
            let parent = temp.root.join(name);
            for exists in [false, true] {
                if exists && let Err(error) = fs::create_dir(&parent) {
                    // APFS rejects invalid UTF-8 names at mkdir itself;
                    // other Unix filesystems permit the existing-parent
                    // fixture. The missing-parent case above still checks
                    // application refusal before any storage mutation.
                    assert!(
                        parent.to_str().is_none() && error.raw_os_error() == Some(libc::EILSEQ),
                        "cannot create existing-parent fixture: {error}"
                    );
                    assert!(!parent.exists());
                    assert!(!temp.manager.directory.exists());
                    continue;
                }
                assert!(
                    temp.manager
                        .reserve_namespace(&parent, &policy(20, "stop"), 10)
                        .is_err()
                );
                assert!(
                    !temp.manager.directory.exists(),
                    "path rejection must precede any durable storage mutation"
                );
                if exists {
                    assert!(fs::read_dir(&parent).unwrap().next().is_none());
                } else {
                    assert!(!parent.exists(), "invalid capture root must not be created");
                }
            }
        }
    }

    #[test]
    fn orphan_recovery_keeps_full_charge_copies_real_catalog_and_releases_only_after_removal() {
        use crate::dataset::task::{TaskDataset, TaskExtentBudget};
        let temp = Temp::new();
        let cap = 16 << 20;
        let policy = policy(64 << 20, "stop");
        let lease = temp
            .manager
            .reserve_namespace(&temp.parent, &policy, cap)
            .unwrap();
        let path = lease.anchor().path().to_owned();
        let mut data =
            TaskDataset::create_reserved(lease.clone(), TaskExtentBudget::new(cap).unwrap())
                .unwrap();
        data.initialize_catalog("Recovery fixture").unwrap();
        data.record_event(
            "note",
            None,
            None,
            "preserved through restart",
            &crate::context::Context::new(),
        )
        .unwrap();
        data.checkpoint_catalog().unwrap();
        assert!(
            temp.manager.recover_namespaces().unwrap()[0]
                .handle
                .is_none(),
            "live engine owner cannot be adopted"
        );
        data.close().unwrap();
        lease.mark_retained().unwrap();
        drop(data);
        drop(lease);
        assert_eq!(reload(&temp.manager).allocated().unwrap(), cap);
        let mut rows = temp.manager.recover_namespaces().unwrap();
        let recovered = rows.remove(0).handle.unwrap();
        let forbidden = path.join("copy-inside-reservation");
        assert!(recovered.save_copy(&forbidden, &policy).is_err());
        assert!(
            !forbidden.exists(),
            "ordinary copy cannot publish inside a reserved workspace"
        );
        assert!(
            temp.manager.recover_namespaces().unwrap()[0]
                .handle
                .is_none(),
            "recovery is exclusive"
        );
        let copy = temp.root.join("recovered-copy");
        recovered
            .lease
            .manager
            .copy_namespace(&recovered.lease.anchor(), cap, &copy, &policy)
            .unwrap();
        let conn = duckdb::Connection::open(copy.join("task.duckdb")).unwrap();
        let text: String = conn
            .query_row("SELECT text FROM events WHERE kind='note'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(text, "preserved through restart");
        drop(conn);
        let charged = reload(&temp.manager).allocated().unwrap();
        assert!(charged > cap, "copy consumes its own storage charge");
        assert!(
            recovered.save_copy(&copy, &policy).is_err(),
            "existing folder is not overwritten"
        );
        recovered.remove().unwrap();
        recovered.remove().unwrap();
        assert!(!path.exists());
        assert!(copy.join("task.duckdb").is_file());
        assert_eq!(reload(&temp.manager).allocated().unwrap(), charged - cap);
    }

    #[test]
    fn orphan_recovery_refuses_replaced_parents_and_directories_and_linked_files() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let lease = temp.reserve(10);
        let path = lease.anchor().path().to_owned();
        fs::write(path.join("task.duckdb"), b"data").unwrap();
        drop(lease); // Simulates owner loss without a retained acknowledgement.
        let mut rows = temp.manager.recover_namespaces().unwrap();
        let handle = rows.remove(0).handle.unwrap();
        let outside = temp.root.join("outside");
        fs::write(&outside, b"keep").unwrap();
        symlink(&outside, path.join("task.duckdb.wal")).unwrap();
        assert!(
            handle
                .save_copy(&temp.root.join("bad-copy"), &policy(20, "stop"))
                .is_err()
        );
        assert!(!temp.root.join("bad-copy").exists());
        assert!(
            handle.remove().is_err(),
            "symlink contents cannot be deleted"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 10);
        fs::remove_file(path.join("task.duckdb.wal")).unwrap();
        drop(handle);
        let moved = temp.root.join("moved-original");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(
            temp.manager.recover_namespaces().unwrap()[0]
                .handle
                .is_none()
        );
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 10);
    }

    #[test]
    fn orphan_removal_refuses_hard_links_and_stale_recovery_handles() {
        let temp = Temp::new();
        let lease = temp.reserve(10);
        let path = lease.anchor().path().to_owned();
        fs::write(path.join("task.duckdb"), b"data").unwrap();
        fs::hard_link(path.join("task.duckdb"), temp.root.join("linked")).unwrap();
        drop(lease);
        let handle = temp
            .manager
            .recover_namespaces()
            .unwrap()
            .remove(0)
            .handle
            .unwrap();
        assert!(handle.remove().is_err());
        assert_eq!(fs::read(temp.root.join("linked")).unwrap(), b"data");
        fs::remove_file(temp.root.join("linked")).unwrap();
        fs::rename(&path, temp.root.join("moved")).unwrap();
        assert!(handle.verify().is_err());
        assert!(handle.remove().is_err());
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 10);
    }

    struct Temp {
        root: PathBuf,
        manager: Manager,
        parent: PathBuf,
    }
    impl Temp {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("capturefab-namespace-{}", nonce().unwrap()));
            crate::ipc::ensure_private_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let parent = root.join("tasks");
            crate::ipc::ensure_private_dir(&parent).unwrap();
            Self {
                manager: Manager {
                    directory: root.join("ledger"),
                },
                root,
                parent,
            }
        }
        fn reserve(&self, cap: u64) -> Arc<RetainedNamespaceLease> {
            self.manager
                .reserve_namespace(&self.parent, &policy(20, "stop"), cap)
                .unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn policy(bytes: u64, on_full: &str) -> StoragePolicy {
        StoragePolicy {
            max_bytes: bytes,
            max_files: 20,
            on_full: on_full.into(),
            ..StoragePolicy::default()
        }
    }
    fn reload(manager: &Manager) -> Locked {
        take_cached(&manager.directory.canonicalize().unwrap());
        manager.lock().unwrap()
    }

    #[test]
    fn namespace_full_cap_is_global_and_survives_client_drop_close_and_restart() {
        let temp = Temp::new();
        let lease = temp.reserve(8);
        let anchor = lease.claim_for_engine().unwrap();
        assert!(Arc::ptr_eq(&anchor, &lease.anchor()));
        assert!(lease.claim_for_engine().is_err());
        fs::write(anchor.path().join("task.duckdb"), b"db").unwrap();
        fs::write(anchor.path().join("task.duckdb.wal"), b"wal").unwrap();
        fs::create_dir(anchor.path().join("spill")).unwrap();
        fs::write(anchor.path().join("spill/part"), b"x").unwrap();
        let caller = lease.clone();
        drop(caller);
        assert_eq!(temp.manager.lock().unwrap().allocated().unwrap(), 8);
        lease.mark_retained().unwrap();
        let path = anchor.path().to_owned();
        drop(anchor);
        drop(lease);
        let mut locked = reload(&temp.manager);
        assert_eq!(locked.ledger.version, LEDGER_VERSION);
        assert_eq!(locked.allocated().unwrap(), 8);
        assert_eq!(locked.ledger.entries[0].reserved, 8);
        assert!(locked.ledger.entries[0].lease.is_none());
        assert_eq!(
            locked.ledger.entries[0].namespace.as_ref().unwrap().phase,
            NamespacePhase::Retained
        );
        let status = locked.status().unwrap();
        assert_eq!(status["retained_namespaces"][0]["allocated_bytes"], 8);
        assert_eq!(status["retained_namespaces"][0]["owner_live"], false);
        assert_eq!(
            status["tracked_bytes"], 0,
            "full cap is a promise, not measured physical bytes"
        );
        drop(locked);
        let mut outside = temp
            .manager
            .writer(&temp.root.join("outside"), &policy(10, "stop"), 2)
            .unwrap();
        outside.write_all(b"ok").unwrap();
        outside.finish().unwrap();
        assert!(
            temp.manager
                .writer(&temp.root.join("over"), &policy(10, "delete-oldest"), 3)
                .is_err()
        );
        assert!(path.join("task.duckdb.wal").exists());
        assert!(path.join("spill/part").exists());
    }

    #[test]
    fn only_receipt_for_the_original_unlinked_anchor_releases_the_global_charge() {
        let temp = Temp::new();
        let a = temp.reserve(4);
        let b = temp.reserve(4);
        let anchor = a.claim_for_engine().unwrap();
        assert!(anchor.removed_receipt().is_err());
        let moved = temp.parent.join("moved-task");
        fs::rename(anchor.path(), &moved).unwrap();
        assert!(
            anchor.removed_receipt().is_err(),
            "missing name is not proof of deletion"
        );
        fs::create_dir(anchor.path()).unwrap();
        fs::remove_dir(&moved).unwrap();
        assert!(
            anchor.removed_receipt().is_err(),
            "a replacement at the old name retains the charge"
        );
        fs::remove_dir(anchor.path()).unwrap();
        let receipt = anchor.removed_receipt().unwrap();
        assert!(b.confirm_removed(&receipt).is_err());
        assert_eq!(temp.manager.lock().unwrap().allocated().unwrap(), 8);
        a.confirm_removed(&receipt).unwrap();
        a.confirm_removed(&receipt).unwrap();
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 4);
    }

    #[test]
    fn ordinary_writers_are_refused_in_active_retained_aliased_and_moved_namespaces() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let lease = temp.reserve(8);
        let path = lease.anchor().path().to_owned();
        let refusal = |path: &Path| {
            let error = temp
                .manager
                .writer(path, &policy(20, "stop"), 1)
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("reserved task namespace"),
                "{error:#}"
            );
            assert!(!path.exists());
        };
        refusal(&path.join("camera/new/frame.png"));
        lease.claim_for_engine().unwrap();
        lease.mark_retained().unwrap();
        refusal(&path.join("task.duckdb"));
        let alias = temp.root.join("alias");
        symlink(&path, &alias).unwrap();
        refusal(&alias.join("file.bin"));
        let moved = temp.parent.join("renamed");
        fs::rename(&path, &moved).unwrap();
        refusal(&moved.join("file.bin"));
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 8);
        assert!(
            temp.manager.lock().unwrap().ledger.entries[0]
                .namespace
                .as_ref()
                .unwrap()
                .uncertain
        );
    }

    #[test]
    fn namespace_admission_survives_replayed_journal_after_compaction() {
        let temp = Temp::new();
        for name in ["first", "second"] {
            let mut file = temp
                .manager
                .writer(&temp.root.join(name), &policy(20, "stop"), 2)
                .unwrap();
            file.write_all(b"ok").unwrap();
            file.finish().unwrap();
        }
        assert_eq!(reload(&temp.manager).ledger.version, LEDGER_VERSION);
        let old = fs::read(temp.manager.directory.join(JOURNAL)).unwrap();
        // A crash after the snapshot is replaced may leave its already
        // included journal behind. Replaying it must not double-charge files.
        let mut compacting = reload(&temp.manager);
        compacting.compact().unwrap();
        drop(compacting);
        fs::write(temp.manager.directory.join(JOURNAL), &old).unwrap();
        let recovered = reload(&temp.manager);
        assert_eq!(recovered.ledger.version, LEDGER_VERSION);
        assert_eq!(recovered.allocated().unwrap(), 4);
        assert!(
            recovered
                .ledger
                .entries
                .iter()
                .all(|entry| entry.namespace.is_none())
        );
        drop(recovered);

        let lease = temp.reserve(4);
        let mut locked = reload(&temp.manager);
        assert_eq!(locked.allocated().unwrap(), 8);
        assert_eq!(locked.ledger.entries.len(), 3);
        assert_eq!(locked.ledger.version, LEDGER_VERSION);
        assert!(
            locked
                .ledger
                .entries
                .iter()
                .filter(|entry| entry.namespace.is_none())
                .all(|entry| entry.sha256.is_some())
        );
        // A later snapshot must include the admitted namespace before we can
        // simulate a stale, already-included journal at compaction recovery.
        // Replacing the only post-admission journal before this snapshot
        // would instead be an external rollback of durable ownership records.
        locked.compact().unwrap();
        drop(locked);
        fs::write(temp.manager.directory.join(JOURNAL), old).unwrap();
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 8);
        assert!(lease.anchor().path().exists());
    }

    #[test]
    fn refused_namespace_does_not_publish_storage() {
        let temp = Temp::new();
        let missing = temp.root.join("never-created/captures");
        assert!(
            temp.manager
                .reserve_namespace(&missing, &policy(2, "stop"), 3)
                .is_err()
        );
        assert!(!temp.root.join("never-created").exists());
        assert!(
            temp.manager
                .reserve_namespace(&missing, &policy(2, "stop"), 0)
                .is_err()
        );
        assert!(!temp.root.join("never-created").exists());
        assert_eq!(temp.manager.lock().unwrap().ledger.version, LEDGER_VERSION);
    }

    #[test]
    fn missing_approved_parent_is_anchored_and_created_before_fresh_namespace_admission() {
        use std::os::unix::fs::PermissionsExt;
        let temp = Temp::new();
        let missing = temp.root.join("chosen/new/captures");
        let lease = temp
            .manager
            .reserve_namespace(&missing, &policy(20, "stop"), 4)
            .unwrap();
        for path in [
            temp.root.join("chosen"),
            temp.root.join("chosen/new"),
            missing.clone(),
            lease.anchor().path().to_owned(),
        ] {
            assert!(path.is_dir());
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(lease.anchor().path().parent(), Some(missing.as_path()));
        assert_eq!(temp.manager.lock().unwrap().allocated().unwrap(), 4);
        let anchor = lease.claim_for_engine().unwrap();
        anchor.verify().unwrap();
        // This admission test opened no native engine or files.
        fs::remove_dir(anchor.path()).unwrap();
        lease
            .confirm_removed(&anchor.removed_receipt().unwrap())
            .unwrap();
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 0);
        assert!(missing.is_dir(), "the approved capture root is retained");

        fs::set_permissions(&temp.parent, fs::Permissions::from_mode(0o755)).unwrap();
        let ordinary = temp.reserve(4);
        assert_eq!(
            fs::metadata(&temp.parent).unwrap().permissions().mode() & 0o777,
            0o755,
            "an existing ordinary capture folder is not chmodded"
        );
        ordinary
            .confirm_removed(&ordinary.cancel_unclaimed().unwrap())
            .unwrap();
    }

    #[test]
    fn missing_parent_under_a_reserved_namespace_or_alias_is_refused_before_any_creation() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let lease = temp.reserve(4);
        let namespace = lease.anchor().path().to_owned();
        let alias = temp.root.join("reserved-alias");
        symlink(&namespace, &alias).unwrap();
        for root in [&namespace, &alias] {
            let missing = root.join("new/captures");
            let error = temp
                .manager
                .reserve_namespace(&missing, &policy(20, "stop"), 4)
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("reserved task namespace"),
                "{error:#}"
            );
            assert!(!namespace.join("new").exists());
        }
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 4);
        lease
            .confirm_removed(&lease.cancel_unclaimed().unwrap())
            .unwrap();
    }

    #[test]
    fn pending_refund_parent_proof_rejects_a_replacement_at_the_same_canonical_path() {
        let temp = Temp::new();
        let original = identity(&fs::metadata(&temp.parent).unwrap());
        assert!(parent_matches(&temp.parent, &original));
        let moved = temp.root.join("original-parent");
        fs::rename(&temp.parent, &moved).unwrap();
        crate::ipc::ensure_private_dir(&temp.parent).unwrap();
        assert_eq!(temp.parent.canonicalize().unwrap(), temp.parent);
        assert!(!parent_matches(&temp.parent, &original));
        fs::remove_dir(&temp.parent).unwrap();
        fs::rename(&moved, &temp.parent).unwrap();
        assert!(parent_matches(&temp.parent, &original));
    }

    #[test]
    fn pending_recovery_waits_for_both_publication_fences_and_never_refunds_active_missing_paths() {
        let temp = Temp::new();
        let id = nonce().unwrap();
        let path = temp.parent.join(format!("task-{id}"));
        let mut locked = temp.manager.lock().unwrap();
        locked.ledger.version = LEDGER_VERSION;
        let lease_name = format!("storage-{id}.lease");
        let lease_path = locked.directory.join(&lease_name);
        let live = private_options()
            .create_new(true)
            .open(&lease_path)
            .unwrap();
        live.lock_exclusive().unwrap();
        let parent_identity = identity(&fs::metadata(&temp.parent).unwrap());
        locked.push(Entry {
            id: id.clone(),
            path: path.clone(),
            base: temp.parent.clone(),
            identity: parent_identity.clone(),
            created: now(),
            bytes: 0,
            reserved: 8,
            lease: Some(lease_name),
            sha256: None,
            modified_ns: None,
            changed: true,
            pending_upload: false,
            namespace: Some(NamespaceRecord {
                cap: 8,
                phase: NamespacePhase::Pending,
                uncertain: false,
                parent_identity,
            }),
        });
        locked.persist().unwrap();
        drop(locked);
        assert!(!path.exists());
        assert_eq!(
            reload(&temp.manager).allocated().unwrap(),
            8,
            "a live pending publisher may still mkdir"
        );
        // A live publisher can commit Active, then lose its name externally.
        // Reconciliation after process/lease loss must keep the full charge.
        fs::create_dir(&path).unwrap();
        let mut locked = temp.manager.lock().unwrap();
        let entry = locked.entry_mut(0);
        entry.identity = identity(&fs::metadata(&path).unwrap());
        entry.namespace.as_mut().unwrap().phase = NamespacePhase::Active;
        locked.persist().unwrap();
        drop(locked);
        fs::remove_dir(&path).unwrap();
        drop(live);
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 8);

        let mut locked = temp.manager.lock().unwrap();
        // Simulate a separate crash before engine admission. This entry never
        // became Active; no output namespace exists and no publisher owns it.
        let entry = locked.entry_mut(0);
        entry.namespace.as_mut().unwrap().phase = NamespacePhase::Pending;
        entry.identity = entry.namespace.as_ref().unwrap().parent_identity.clone();
        locked.persist().unwrap();
        drop(locked);
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 0);
    }

    #[test]
    fn unused_cancellation_is_retryable_requires_empty_anchor_and_cannot_cancel_claimed_engine() {
        let temp = Temp::new();
        let lease = temp.reserve(4);
        let path = lease.anchor().path().to_owned();
        fs::write(path.join("unexpected"), b"keep").unwrap();
        assert!(lease.cancel_unclaimed().is_err());
        assert_eq!(fs::read(path.join("unexpected")).unwrap(), b"keep");
        assert_eq!(temp.manager.lock().unwrap().allocated().unwrap(), 4);
        fs::remove_file(path.join("unexpected")).unwrap();
        let receipt = lease.cancel_unclaimed().unwrap();
        assert!(!path.exists());
        assert!(lease.claim_for_engine().is_err());
        assert_eq!(
            temp.manager.lock().unwrap().allocated().unwrap(),
            4,
            "removal proof alone is not a ledger refund"
        );
        lease.confirm_removed(&receipt).unwrap();
        lease
            .confirm_removed(&lease.cancel_unclaimed().unwrap())
            .unwrap();
        assert_eq!(temp.manager.lock().unwrap().allocated().unwrap(), 0);
        let claimed = temp.reserve(4);
        let anchor = claimed.claim_for_engine().unwrap();
        assert!(claimed.cancel_unclaimed().is_err());
        assert!(anchor.path().exists());
    }

    #[test]
    fn removal_retry_recognizes_a_committed_refund_after_its_acknowledgement_was_lost() {
        let temp = Temp::new();
        let lease = temp.reserve(4);
        let other = temp.reserve(4);
        let receipt = lease.cancel_unclaimed().unwrap();
        assert!(other.confirm_removed(&receipt).is_err());
        // Persist the removal, then simulate losing the acknowledgement before
        // the in-memory released flag and liveness-file cleanup are updated.
        let mut locked = temp.manager.lock().unwrap();
        let index = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.id == lease.id())
            .unwrap();
        locked.remove(index);
        locked.persist().unwrap();
        drop(locked);
        assert!(!lease.released.load(Ordering::Acquire));
        assert!(lease.lease_path.exists());
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 4);
        lease.confirm_removed(&receipt).unwrap();
        assert!(lease.released.load(Ordering::Acquire));
        assert!(!lease.lease_path.exists());
        lease.confirm_removed(&receipt).unwrap();
        assert!(other.confirm_removed(&receipt).is_err());
        assert_eq!(reload(&temp.manager).allocated().unwrap(), 4);
        other
            .confirm_removed(&other.cancel_unclaimed().unwrap())
            .unwrap();
    }

    #[test]
    fn cancellation_and_live_engine_admission_have_one_atomic_winner() {
        let temp = Temp::new();
        for _ in 0..8 {
            let lease = temp.reserve(4);
            let start = Arc::new(std::sync::Barrier::new(3));
            let admit = {
                let lease = lease.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    lease.claim_for_engine().is_ok()
                })
            };
            let cancel = {
                let lease = lease.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    lease.cancel_unclaimed()
                })
            };
            start.wait();
            let admitted = admit.join().unwrap();
            let cancelled = cancel.join().unwrap();
            assert_ne!(admitted, cancelled.is_ok());
            let receipt = if let Ok(receipt) = cancelled {
                receipt
            } else {
                // The test engine opened no native handles or files.
                fs::remove_dir(lease.anchor().path()).unwrap();
                lease.anchor().removed_receipt().unwrap()
            };
            lease.confirm_removed(&receipt).unwrap();
            assert_eq!(temp.manager.lock().unwrap().allocated().unwrap(), 0);
        }
    }
}
