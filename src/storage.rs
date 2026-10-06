//! Cross-process capture quotas and conservative retention of Capturefab-owned files.
//! Reservations are protected by OS locks, so an interrupted writer cannot leave
//! an unlimited recording or prevent its unused reservation being reclaimed.
use crate::types::Frame;
use anyhow::{Context, Result, anyhow, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LEDGER_LIMIT: u64 = 64 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StoragePolicy {
    pub max_bytes: u64,
    pub max_files: u32,
    pub max_age_seconds: Option<u64>,
    pub on_full: String,
}
impl Default for StoragePolicy {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024 * 1024,
            max_files: 10_000,
            max_age_seconds: None,
            on_full: "stop".into(),
        }
    }
}
impl StoragePolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.max_bytes > 0, "storage byte limit must be positive");
        ensure!(
            self.max_files > 0 && self.max_files <= 100_000,
            "storage file limit must be 1..100000"
        );
        ensure!(
            matches!(self.on_full.as_str(), "stop" | "delete-oldest"),
            "storage on-full policy must be stop or delete-oldest"
        );
        ensure!(
            self.max_age_seconds != Some(0),
            "storage maximum age must be positive"
        );
        Ok(())
    }
    fn constrained_by(&self, configured: &Self) -> Self {
        Self {
            max_bytes: self.max_bytes.min(configured.max_bytes),
            max_files: self.max_files.min(configured.max_files),
            max_age_seconds: match (self.max_age_seconds, configured.max_age_seconds) {
                (Some(request), Some(global)) => Some(request.min(global)),
                (request, global) => request.or(global),
            },
            on_full: if self.on_full == "delete-oldest" || configured.on_full == "delete-oldest" {
                "delete-oldest"
            } else {
                "stop"
            }
            .into(),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct PolicyFile {
    version: u32,
    policy: StoragePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Identity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    created_ns: Option<u64>,
}
fn nanos(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()
}
fn identity(meta: &Metadata) -> Identity {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Identity {
        #[cfg(unix)]
        device: meta.dev(),
        #[cfg(unix)]
        inode: meta.ino(),
        created_ns: meta.created().ok().and_then(nanos),
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("storage reservation nonce: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    path: PathBuf,
    base: PathBuf,
    identity: Identity,
    created: u64,
    bytes: u64,
    reserved: u64,
    lease: Option<String>,
    sha256: Option<String>,
    modified_ns: Option<u64>,
    #[serde(default)]
    changed: bool,
    /// Queued for upload: retention must not delete it before then.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pending_upload: bool,
}
#[derive(Debug, Serialize, Deserialize)]
struct Ledger {
    version: u32,
    entries: Vec<Entry>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}
struct Manager {
    directory: PathBuf,
}
struct Locked {
    directory: PathBuf,
    lock: File,
    ledger: Ledger,
}
impl Drop for Locked {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
    }
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn regular_private(path: &Path, max_bytes: u64) -> Result<Metadata> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= max_bytes,
        "invalid storage bookkeeping file {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "storage bookkeeping file must be private: {}",
            path.display()
        );
    }
    Ok(meta)
}
fn contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}
fn lock_wait(file: &File) -> Result<()> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(e) if contended(&e) => {
                ensure!(
                    Instant::now() < deadline,
                    "storage manager is busy; retry after other capture processes finish bookkeeping"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).context("lock storage manager"),
        }
    }
}
impl Manager {
    fn global() -> Self {
        Self {
            directory: crate::ipc::session_dir(),
        }
    }
    fn lock(&self) -> Result<Locked> {
        crate::ipc::ensure_private_dir(&self.directory)?;
        let directory = fs::canonicalize(&self.directory)?;
        let lock_path = directory.join("storage.lock");
        // create_new avoids following an existing symbolic link on the initial creation.
        let lock = match private_options().create_new(true).open(&lock_path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                regular_private(&lock_path, 0)?;
                private_options().open(&lock_path)?
            }
            Err(e) => return Err(e).context("create storage lock"),
        };
        regular_private(&lock_path, 0)?;
        lock_wait(&lock)?;
        let ledger_path = directory.join("storage.json");
        let ledger = if ledger_path.exists() {
            regular_private(&ledger_path, LEDGER_LIMIT)?;
            serde_json::from_slice(&fs::read(&ledger_path)?).with_context(|| {
                format!(
                    "invalid storage ledger {}; preserve it and inspect before resetting ownership",
                    ledger_path.display()
                )
            })?
        } else {
            Ledger::default()
        };
        let mut result = Locked {
            directory,
            lock,
            ledger,
        };
        result.validate()?;
        result.reconcile()?;
        Ok(result)
    }
    fn writer(&self, path: &Path, policy: &StoragePolicy, reserve: u64) -> Result<StorageWriter> {
        policy.validate()?;
        ensure!(
            reserve > 0 && reserve <= policy.max_bytes,
            "Storage full: reservation {reserve} bytes exceeds the {} byte budget; lower --max-file-size or explicitly configure a larger storage budget",
            policy.max_bytes
        );
        let (base, path) = output_path(path)?;
        ensure!(
            !path.exists() && fs::symlink_metadata(&path).is_err(),
            "capture output already exists: {}; choose a new file name",
            path.display()
        );
        let mut locked = self.lock()?;
        let policy = policy.constrained_by(&locked.configured_policy()?);
        ensure!(
            reserve <= policy.max_bytes,
            "Storage full: reservation {reserve} bytes exceeds the configured {} byte global budget; lower the recording reservation or explicitly configure a larger storage policy",
            policy.max_bytes
        );
        ensure!(
            fs::symlink_metadata(&path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
            "capture output already exists or cannot be inspected: {}; choose a new file name and check directory permissions",
            path.display()
        );
        locked.make_room(&policy, reserve)?;
        let id = nonce()?;
        let lease_name = format!("storage-{id}.lease");
        let lease_path = locked.directory.join(&lease_name);
        let lease = private_options().create_new(true).open(&lease_path)?;
        lease.lock_exclusive()?;
        let file = match private_options().create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) => {
                drop(lease);
                let _ = fs::remove_file(&lease_path);
                return Err(error).with_context(|| format!("cannot create capture output {}; check parent directory permissions, free disk space, and the storage limits", path.display()));
            }
        };
        let file_identity = identity(&file.metadata()?);
        locked.ledger.entries.push(Entry {
            id: id.clone(),
            path: path.clone(),
            base,
            identity: file_identity.clone(),
            created: now(),
            bytes: 0,
            reserved: reserve,
            lease: Some(lease_name),
            sha256: None,
            modified_ns: None,
            changed: false,
            pending_upload: false,
        });
        if let Err(error) = locked.persist() {
            drop(file);
            drop(lease);
            if fs::symlink_metadata(&path)
                .is_ok_and(|meta| identity(&meta) == file_identity && meta.len() == 0)
            {
                let _ = fs::remove_file(&path);
            }
            let _ = fs::remove_file(&lease_path);
            return Err(error);
        }
        Ok(StorageWriter {
            manager: Self {
                directory: locked.directory.clone(),
            },
            id,
            path,
            identity: file_identity,
            file: Some(file),
            lease: Some(lease),
            lease_path,
            reserve,
            written: 0,
            hash: Sha256::new(),
            failed: false,
            finalized: false,
        })
    }
}

fn output_path(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("capture output must name a file"))?;
    ensure!(
        name != "." && name != "..",
        "invalid capture output filename"
    );
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    crate::volumes::ensure_present(parent)?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "cannot create capture directory {}; check permissions and free disk space",
            parent.display()
        )
    })?;
    let base = fs::canonicalize(parent)?;
    ensure!(base.is_dir(), "capture output parent is not a directory");
    Ok((base.clone(), base.join(name)))
}
/// A path's canonical directory and file name, without creating anything.
fn resolved_existing(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("capture path must name a file"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let base = fs::canonicalize(parent)?;
    Ok((base.clone(), base.join(name)))
}
fn current_meta(entry: &Entry, verified: &mut HashSet<PathBuf>) -> Result<Option<Metadata>> {
    ensure!(
        entry.path.is_absolute()
            && entry.base.is_absolute()
            && entry.path.parent() == Some(entry.base.as_path()),
        "storage ledger path escaped its recorded directory"
    );
    let meta = match fs::symlink_metadata(&entry.path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect retained capture {}", entry.path.display()));
        }
    };
    if !verified.contains(&entry.base) {
        ensure!(
            fs::canonicalize(&entry.base)? == entry.base,
            "capture directory identity changed: {}; retention stopped",
            entry.base.display()
        );
        verified.insert(entry.base.clone());
    }
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "tracked capture became a non-file: {}; retention stopped",
        entry.path.display()
    );
    Ok(Some(meta))
}
fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
impl Locked {
    fn configured_policy(&self) -> Result<StoragePolicy> {
        let path = self.directory.join("storage-policy.json");
        match regular_private(&path, 4096) {
            Ok(_) => {
                let config: PolicyFile = serde_json::from_slice(&fs::read(&path)?).context("invalid persisted storage policy; inspect storage-policy.json before changing it")?;
                ensure!(
                    config.version == 1,
                    "unsupported persisted storage policy version"
                );
                config.policy.validate()?;
                Ok(config.policy)
            }
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(StoragePolicy::default())
            }
            Err(error) => Err(error),
        }
    }
    fn configure_policy(&self, policy: &StoragePolicy) -> Result<()> {
        policy.validate()?;
        self.persist_bytes(
            "storage-policy.json",
            &serde_json::to_vec(&PolicyFile {
                version: 1,
                policy: policy.clone(),
            })?,
        )
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.ledger.version == 1 && self.ledger.entries.len() <= 100_000,
            "unsupported or oversized storage ledger"
        );
        let mut ids = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for entry in &self.ledger.entries {
            ensure!(
                entry.id.len() == 32
                    && entry.id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && ids.insert(&entry.id),
                "invalid storage ledger reservation id"
            );
            ensure!(
                entry.path.is_absolute()
                    && entry.base.is_absolute()
                    && entry.path.parent() == Some(entry.base.as_path())
                    && paths.insert(&entry.path),
                "invalid storage ledger capture path"
            );
            ensure!(
                entry
                    .lease
                    .as_ref()
                    .is_none_or(|name| name == &format!("storage-{}.lease", entry.id)),
                "invalid storage reservation lease"
            );
            ensure!(
                entry
                    .sha256
                    .as_ref()
                    .is_none_or(|hash| hash.len() == 64
                        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())),
                "invalid storage capture hash"
            );
        }
        Ok(())
    }
    fn persist(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.ledger)?;
        ensure!(
            bytes.len() as u64 <= LEDGER_LIMIT,
            "storage ledger exceeds bookkeeping limit; reduce retained file count"
        );
        self.persist_bytes("storage.json", &bytes)
    }
    fn persist_bytes(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let temp = self.directory.join(format!("storage-{}.tmp", nonce()?));
        let result = (|| -> Result<()> {
            let mut file = private_options().create_new(true).open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, self.directory.join(name))?;
            #[cfg(unix)]
            File::open(&self.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.context("commit storage ledger; check private session directory permissions and free disk space")
    }
    fn reconcile(&mut self) -> Result<()> {
        let mut changed = false;
        let mut missing = Vec::new();
        let mut verified = HashSet::new();
        for (index, entry) in self.ledger.entries.iter_mut().enumerate() {
            let meta = current_meta(entry, &mut verified)?;
            let current_bytes = meta.as_ref().map_or(0, Metadata::len);
            if let Some(meta) = &meta {
                if identity(meta) != entry.identity && !entry.changed {
                    entry.changed = true;
                    changed = true;
                }
                if entry.lease.is_none()
                    && (entry.bytes != meta.len()
                        || entry.modified_ns != meta.modified().ok().and_then(nanos))
                    && !entry.changed
                {
                    entry.changed = true;
                    changed = true;
                }
            }
            if entry.bytes != current_bytes {
                entry.bytes = current_bytes;
                // Live lengths are read again on each lock; the durable
                // reservation already covers growth until finalization.
                changed |= entry.lease.is_none();
            }
            let mut stale = entry.lease.is_none();
            let mut stale_lease = None;
            if let Some(name) = &entry.lease {
                let path = self.directory.join(name);
                match regular_private(&path, 0) {
                    Ok(_) => {
                        let lease = private_options().open(&path)?;
                        match lease.try_lock_exclusive() {
                            Ok(()) => {
                                stale = true;
                                stale_lease = Some((lease, path));
                            }
                            Err(error) if contended(&error) => {}
                            Err(error) => {
                                return Err(error).context("inspect capture reservation lease");
                            }
                        }
                    }
                    Err(error)
                        if error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                    {
                        stale = true;
                    }
                    Err(error) => return Err(error),
                }
            }
            if !stale {
                continue;
            }
            match meta {
                None => {
                    missing.push(index);
                    changed = true;
                }
                Some(meta) if entry.lease.is_some() => {
                    entry.reserved = 0;
                    entry.lease = None;
                    // An interrupted writer has no committed content hash.
                    // Reclaim its unused reservation but preserve the file.
                    entry.changed = true;
                    entry.sha256 = None;
                    entry.modified_ns = meta.modified().ok().and_then(nanos);
                    changed = true;
                }
                Some(_) => {}
            }
            if let Some((lease, path)) = stale_lease {
                drop(lease);
                let _ = fs::remove_file(path);
            }
        }
        for index in missing.into_iter().rev() {
            self.ledger.entries.remove(index);
        }
        if changed {
            self.persist()?;
        }
        Ok(())
    }
    fn allocated(&self) -> Result<u64> {
        self.ledger.entries.iter().try_fold(0u64, |sum, entry| {
            sum.checked_add(entry.bytes.max(entry.reserved))
                .ok_or_else(|| anyhow!("storage quota accounting overflow"))
        })
    }
    fn status(&self) -> Result<Value> {
        let allocated = self.allocated()?;
        let policy = self.configured_policy()?;
        let over_limit = allocated > policy.max_bytes
            || self.ledger.entries.len() > policy.max_files as usize
            || self
                .ledger
                .entries
                .iter()
                .any(|entry| self.expired(entry, &policy));
        Ok(json!({
            "ledger": self.directory.join("storage.json"),
            "policy": policy,
            "over_limit": over_limit,
            "tracked_bytes": self.ledger.entries.iter().map(|entry| entry.bytes).sum::<u64>(),
            "allocated_bytes": allocated,
            "reserved_bytes": self.ledger.entries.iter().map(|entry| entry.reserved.saturating_sub(entry.bytes)).sum::<u64>(),
            "files": self.ledger.entries.len(),
            "active_reservations": self.ledger.entries.iter().filter(|entry| entry.lease.is_some()).count(),
            "externally_changed_files": self.ledger.entries.iter().filter(|entry| entry.changed).count(),
        }))
    }
    fn expired(&self, entry: &Entry, policy: &StoragePolicy) -> bool {
        policy
            .max_age_seconds
            .is_some_and(|age| now().saturating_sub(entry.created) >= age)
    }
    fn removable(entry: &Entry) -> bool {
        entry.lease.is_none() && !entry.changed && entry.sha256.is_some() && !entry.pending_upload
    }
    /// The ledger entry for a finished file, by its resolved path.
    fn find(&self, path: &Path) -> Result<usize> {
        let (_, path) = resolved_existing(path)?;
        self.ledger
            .entries
            .iter()
            .position(|entry| entry.path == path && entry.lease.is_none())
            .ok_or_else(|| anyhow!("{} is not a finished Capturefab capture", path.display()))
    }
    fn delete(&mut self, index: usize) -> Result<bool> {
        let entry = &mut self.ledger.entries[index];
        let Some(meta) = current_meta(entry, &mut HashSet::new())? else {
            self.ledger.entries.remove(index);
            self.persist()?;
            return Ok(true);
        };
        if !Self::removable(entry)
            || identity(&meta) != entry.identity
            || meta.len() != entry.bytes
            || meta.modified().ok().and_then(nanos) != entry.modified_ns
            || hash_file(&entry.path)? != entry.sha256.as_deref().unwrap()
        {
            entry.changed = true;
            entry.bytes = meta.len();
            self.persist()?;
            return Ok(false);
        }
        let recheck = current_meta(entry, &mut HashSet::new())?
            .ok_or_else(|| anyhow!("capture disappeared during retention verification"))?;
        ensure!(
            identity(&recheck) == entry.identity
                && recheck.len() == entry.bytes
                && recheck.modified().ok().and_then(nanos) == entry.modified_ns,
            "capture changed during retention verification; deletion stopped"
        );
        fs::remove_file(&entry.path).with_context(|| format!("retention cannot remove {}; check directory permissions or select a different output directory", entry.path.display()))?;
        self.ledger.entries.remove(index);
        self.persist()?;
        Ok(true)
    }
    fn make_room(&mut self, policy: &StoragePolicy, reserve: u64) -> Result<()> {
        loop {
            let allocated = self.allocated()?;
            let expired = self
                .ledger
                .entries
                .iter()
                .position(|entry| self.expired(entry, policy));
            let full = allocated
                .checked_add(reserve)
                .is_none_or(|sum| sum > policy.max_bytes)
                || self.ledger.entries.len() >= policy.max_files as usize;
            if !full && expired.is_none() {
                return Ok(());
            }
            if policy.on_full == "stop" {
                bail!(
                    "Storage full: {allocated} allocated bytes and {} retained files (limits: {} bytes, {} files{}); enable explicit --delete-oldest retention, remove old captures yourself, or use storage configure --max-space/--max-files to increase the budget",
                    self.ledger.entries.len(),
                    policy.max_bytes,
                    policy.max_files,
                    if expired.is_some() {
                        "; maximum age reached"
                    } else {
                        ""
                    }
                );
            }
            let candidate = self
                .ledger
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    Self::removable(entry) && (full || self.expired(entry, policy))
                })
                .min_by_key(|(_, entry)| entry.created)
                .map(|(index, _)| index);
            let Some(index) = candidate else {
                bail!(
                    "Storage full: no unchanged, completed Capturefab files can be removed safely; active recordings and externally changed files are protected. Stop recordings, remove files yourself, or increase the storage limits"
                );
            };
            self.delete(index)?;
        }
    }
}

/// A reserved, bounded file writer. Finish flushes and commits actual size; errors
/// remove a partial file only if its identity and content still belong to this writer.
pub struct StorageWriter {
    manager: Manager,
    id: String,
    path: PathBuf,
    identity: Identity,
    file: Option<File>,
    lease: Option<File>,
    lease_path: PathBuf,
    reserve: u64,
    written: u64,
    hash: Sha256,
    failed: bool,
    finalized: bool,
}
impl Write for StorageWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::other(
                "capture writer stopped after an earlier storage failure",
            ));
        }
        if self
            .written
            .checked_add(bytes.len() as u64)
            .is_none_or(|size| size > self.reserve)
        {
            self.failed = true;
            return Err(io::Error::other(format!(
                "Storage full: {} reached its {} byte recording reservation; increase the reservation or record shorter segments",
                self.path.display(),
                self.reserve
            )));
        }
        let result = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("capture writer already finished"))?
            .write(bytes);
        match result {
            Ok(count) => {
                self.written += count as u64;
                self.hash.update(&bytes[..count]);
                Ok(count)
            }
            Err(error) => {
                self.failed = true;
                Err(io::Error::new(
                    error.kind(),
                    format!(
                        "cannot write capture {}: {error}; check free disk space, permissions, and storage limits",
                        self.path.display()
                    ),
                ))
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        let result = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("capture writer already finished"))?
            .flush();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl StorageWriter {
    pub fn finish(mut self) -> Result<()> {
        let result = self.finalize();
        // If committing the ledger failed, releasing the lease allows the next
        // manager to reconcile conservatively instead of retrying from Drop.
        self.finalized = true;
        result
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn bytes_written(&self) -> u64 {
        self.written
    }
    /// Abandon an invalid recording. An externally altered output is preserved.
    pub fn abort(mut self) -> Result<()> {
        self.failed = true;
        self.file.take();
        let result = self.cleanup();
        self.finalized = true;
        result
    }
    fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        if let Some(file) = self.file.take()
            && let Err(error) = file.sync_all()
        {
            self.failed = true;
            drop(file);
            self.cleanup()?;
            return Err(error).with_context(|| {
                format!(
                    "cannot flush capture {}; check disk space and permissions",
                    self.path.display()
                )
            });
        }
        if self.failed {
            self.cleanup()?;
            bail!(
                "partial capture removed after a storage failure: {}",
                self.path.display()
            );
        }
        let mut locked = self.manager.lock()?;
        let entry = locked
            .ledger
            .entries
            .iter_mut()
            .find(|entry| entry.id == self.id)
            .ok_or_else(|| anyhow!("capture reservation disappeared from storage ledger"))?;
        let meta = current_meta(entry, &mut HashSet::new())?
            .ok_or_else(|| anyhow!("capture output disappeared: {}", self.path.display()))?;
        ensure!(
            identity(&meta) == self.identity && meta.len() == self.written,
            "capture output changed externally; preserving {} without enabling retention deletion",
            self.path.display()
        );
        let hash = hash_file(&self.path)?;
        ensure!(
            hash == format!("{:x}", self.hash.clone().finalize()),
            "capture content changed externally; preserving {} without enabling retention deletion",
            self.path.display()
        );
        entry.bytes = self.written;
        entry.reserved = 0;
        entry.lease = None;
        entry.sha256 = Some(hash);
        entry.modified_ns = meta.modified().ok().and_then(nanos);
        locked.persist()?;
        self.finalized = true;
        self.lease.take();
        let _ = fs::remove_file(&self.lease_path);
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        let mut locked = self.manager.lock()?;
        if let Some(index) = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.id == self.id)
        {
            let entry = &locked.ledger.entries[index];
            let owned = current_meta(entry, &mut HashSet::new())?
                .is_some_and(|meta| identity(&meta) == self.identity && meta.len() == self.written);
            if owned && hash_file(&self.path)? == format!("{:x}", self.hash.clone().finalize()) {
                fs::remove_file(&self.path)?;
                locked.ledger.entries.remove(index);
            } else {
                let entry = &mut locked.ledger.entries[index];
                entry.changed = true;
                entry.reserved = 0;
                entry.lease = None;
            }
            locked.persist()?;
        }
        self.finalized = true;
        self.lease.take();
        let _ = fs::remove_file(&self.lease_path);
        Ok(())
    }
}
impl Drop for StorageWriter {
    fn drop(&mut self) {
        if !self.finalized
            && let Err(error) = self.finalize()
        {
            eprintln!("capturefab: storage finalization failed: {error:#}");
        }
    }
}

pub fn create_writer(
    path: &Path,
    policy: &StoragePolicy,
    reserve_bytes: u64,
) -> Result<StorageWriter> {
    Manager::global().writer(path, policy, reserve_bytes)
}
pub fn save(frame: &Frame, path: &Path, format: &str, policy: &StoragePolicy) -> Result<()> {
    let bytes = crate::frame::encode(frame, format)?;
    let mut writer = create_writer(path, policy, bytes.len() as u64)?;
    writer.write_all(&bytes)?;
    writer.finish()
}
/// Hold a finished capture back from retention while it waits for upload,
/// or release it. Returns the SHA-256 recorded when the file was finished.
pub fn hold_for_upload(path: &Path, held: bool) -> Result<String> {
    let mut locked = Manager::global().lock()?;
    let index = locked.find(path)?;
    let entry = &mut locked.ledger.entries[index];
    let sha256 = entry
        .sha256
        .clone()
        .ok_or_else(|| anyhow!("{} has no recorded checksum", entry.path.display()))?;
    if entry.pending_upload != held {
        entry.pending_upload = held;
        locked.persist()?;
    }
    Ok(sha256)
}
/// Delete the local copy of an uploaded capture, only if it is still exactly
/// the file Capturefab wrote (identity, size, time and SHA-256).
pub fn remove_uploaded(path: &Path) -> Result<bool> {
    let mut locked = Manager::global().lock()?;
    let index = locked.find(path)?;
    locked.ledger.entries[index].pending_upload = false;
    locked.delete(index)
}
pub fn quota_status() -> Result<Value> {
    Manager::global().lock()?.status()
}
pub fn configured_policy() -> Result<StoragePolicy> {
    Manager::global().lock()?.configured_policy()
}
pub fn configure_policy(policy: &StoragePolicy) -> Result<Value> {
    let locked = Manager::global().lock()?;
    locked.configure_policy(policy)?;
    Ok(json!({ "policy": policy, "quota": locked.status()? }))
}
/// Parse byte counts: 10GiB/10G use binary units; 10GB uses decimal units.
pub fn parse_bytes(value: &str) -> Result<u64> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..split]
        .parse()
        .context("storage size must start with an integer")?;
    let suffix = value[split..].trim().to_ascii_lowercase();
    let multiplier: u64 = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "ki" | "kib" => 1024,
        "m" | "mi" | "mib" => 1024u64.pow(2),
        "g" | "gi" | "gib" => 1024u64.pow(3),
        "t" | "ti" | "tib" => 1024u64.pow(4),
        "kb" => 1000,
        "mb" => 1000u64.pow(2),
        "gb" => 1000u64.pow(3),
        "tb" => 1000u64.pow(4),
        _ => bail!("unknown storage size unit; use B, KiB, MiB, GiB, TiB, KB, MB, GB or TB"),
    };
    let bytes = number
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("storage size overflows u64"))?;
    ensure!(bytes > 0, "storage size must be positive");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("capturefab-storage-test-{}", nonce().unwrap()));
            crate::ipc::ensure_private_dir(&path).unwrap();
            Self(path)
        }
        fn manager(&self) -> Manager {
            Manager {
                directory: self.0.join("private"),
            }
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn policy(bytes: u64, files: u32, retention: &str) -> StoragePolicy {
        StoragePolicy {
            max_bytes: bytes,
            max_files: files,
            max_age_seconds: None,
            on_full: retention.into(),
        }
    }
    fn completed(manager: &Manager, path: &Path, policy: &StoragePolicy, data: &[u8]) {
        let mut writer = manager.writer(path, policy, data.len() as u64).unwrap();
        writer.write_all(data).unwrap();
        writer.finish().unwrap();
    }
    #[test]
    fn byte_units_are_checked() {
        assert_eq!(parse_bytes("10GiB").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_bytes("10G").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_bytes("10GB").unwrap(), 10_000_000_000);
        assert_eq!(parse_bytes(" 5 KB ").unwrap(), 5000);
        for invalid in ["0", "-1", "1.5GB", "10words", "18446744073709551615TB"] {
            assert!(parse_bytes(invalid).is_err());
        }
    }
    #[test]
    fn reservations_are_global_across_managers_and_directories() {
        let temp = Temp::new();
        let first = temp.manager();
        let second = temp.manager();
        let policy = policy(10, 4, "stop");
        let mut a = first
            .writer(&temp.path("camera-a/frame"), &policy, 8)
            .unwrap();
        a.write_all(b"ab").unwrap();
        assert!(
            second
                .writer(&temp.path("camera-b/frame"), &policy, 3)
                .is_err()
        );
        a.finish().unwrap();
        let mut b = second
            .writer(&temp.path("camera-b/frame"), &policy, 8)
            .unwrap();
        b.write_all(b"12345678").unwrap();
        b.finish().unwrap();
        let locked = first.lock().unwrap();
        assert_eq!(locked.allocated().unwrap(), 10);
        assert_eq!(locked.ledger.entries.len(), 2);
        drop(locked);
        assert!(
            first
                .writer(&temp.path("camera-a/next"), &policy, 1)
                .is_err()
        );
    }
    #[test]
    fn concurrent_reservations_cannot_overbook_the_global_budget() {
        let temp = Temp::new();
        drop(temp.manager().lock().unwrap());
        let threads = (0..12)
            .map(|index| {
                let manager = temp.manager();
                let path = temp.path(&format!("camera-{index}/video"));
                thread::spawn(move || {
                    manager
                        .writer(&path, &policy(10, 20, "stop"), 3)
                        .ok()
                        .map(|mut writer| {
                            writer.write_all(b"abc").unwrap();
                            writer
                        })
                })
            })
            .collect::<Vec<_>>();
        let writers = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(writers.len(), 3);
        assert_eq!(temp.manager().lock().unwrap().allocated().unwrap(), 9);
        for writer in writers {
            writer.finish().unwrap();
        }
        assert_eq!(temp.manager().lock().unwrap().allocated().unwrap(), 9);
    }
    #[test]
    fn configured_ceiling_persists_and_per_capture_cannot_raise_it() {
        let temp = Temp::new();
        let manager = temp.manager();
        {
            let locked = manager.lock().unwrap();
            assert_eq!(
                locked.configured_policy().unwrap().max_bytes,
                10 * 1024u64.pow(3)
            );
            locked.configure_policy(&policy(6, 2, "stop")).unwrap();
        }
        completed(
            &manager,
            &temp.path("first"),
            &policy(1000, 100, "stop"),
            b"1234",
        );
        assert!(
            manager
                .writer(&temp.path("exceed"), &policy(1000, 100, "stop"), 3)
                .is_err()
        );
        {
            let locked = manager.lock().unwrap();
            assert_eq!(locked.configured_policy().unwrap().max_bytes, 6);
            locked.configure_policy(&policy(2, 1, "stop")).unwrap();
            assert_eq!(locked.status().unwrap()["over_limit"], true);
        }
        assert!(
            temp.path("first").exists(),
            "configuration itself must not delete retained files"
        );
        assert!(
            manager
                .writer(&temp.path("blocked"), &policy(1000, 100, "stop"), 1)
                .is_err()
        );
        {
            let locked = manager.lock().unwrap();
            locked
                .configure_policy(&policy(6, 2, "delete-oldest"))
                .unwrap();
        }
        completed(
            &manager,
            &temp.path("replacement"),
            &policy(1000, 100, "stop"),
            b"12345",
        );
        assert!(!temp.path("first").exists());
    }
    #[test]
    fn most_restrictive_age_and_counts_are_preserved() {
        let mut request = policy(100, 30, "stop");
        request.max_age_seconds = Some(20);
        let mut global = policy(50, 10, "stop");
        global.max_age_seconds = Some(10);
        let effective = request.constrained_by(&global);
        assert_eq!(
            (
                effective.max_bytes,
                effective.max_files,
                effective.max_age_seconds
            ),
            (50, 10, Some(10))
        );
        request.max_age_seconds = None;
        assert_eq!(request.constrained_by(&global).max_age_seconds, Some(10));
        global.max_age_seconds = None;
        request.max_age_seconds = Some(20);
        assert_eq!(request.constrained_by(&global).max_age_seconds, Some(20));
        request.on_full = "delete-oldest".into();
        assert_eq!(request.constrained_by(&global).on_full, "delete-oldest");
    }
    #[test]
    fn oldest_retention_only_removes_unchanged_owned_files() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(6, 1, "delete-oldest");
        fs::write(temp.path("unowned"), b"an existing user file").unwrap();
        completed(&manager, &temp.path("old"), &policy, b"old");
        completed(&manager, &temp.path("new"), &policy, b"newer");
        assert!(!temp.path("old").exists());
        assert_eq!(
            fs::read(temp.path("unowned")).unwrap(),
            b"an existing user file"
        );
        fs::write(temp.path("new"), b"other").unwrap();
        assert!(manager.writer(&temp.path("blocked"), &policy, 1).is_err());
        assert_eq!(fs::read(temp.path("new")).unwrap(), b"other");
        assert!(manager.writer(&temp.path("unowned"), &policy, 1).is_err());
    }
    #[test]
    fn quota_failure_removes_partial_and_releases_reservation() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(4, 1, "stop");
        let path = temp.path("partial");
        let mut writer = manager.writer(&path, &policy, 4).unwrap();
        writer.write_all(b"abc").unwrap();
        assert!(
            writer
                .write_all(b"de")
                .unwrap_err()
                .to_string()
                .contains("Storage full")
        );
        assert!(writer.finish().is_err());
        assert!(!path.exists());
        completed(&manager, &temp.path("complete"), &policy, b"1234");
        assert_eq!(manager.lock().unwrap().allocated().unwrap(), 4);
    }
    #[test]
    fn io_failure_removes_partial_without_overwriting() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(16, 2, "stop");
        let path = temp.path("failed");
        let mut writer = manager.writer(&path, &policy, 8).unwrap();
        writer.file = Some(File::open(&path).unwrap()); // Inject a real read-only-handle write failure.
        assert!(writer.write_all(b"data").is_err());
        drop(writer);
        assert!(!path.exists());
        assert_eq!(manager.lock().unwrap().allocated().unwrap(), 0);
        fs::write(&path, b"existing").unwrap();
        assert!(manager.writer(&path, &policy, 8).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing");
    }
    #[test]
    fn drop_commits_actual_bytes_and_crash_releases_unused_budget() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(10, 4, "stop");
        let mut writer = manager.writer(&temp.path("finished"), &policy, 8).unwrap();
        writer.write_all(b"ab").unwrap();
        drop(writer);
        assert_eq!(manager.lock().unwrap().allocated().unwrap(), 2);
        let mut crashed = manager.writer(&temp.path("crashed"), &policy, 8).unwrap();
        crashed.write_all(b"123").unwrap();
        // Emulate process teardown without the normal writer finalizer.
        crashed.finalized = true;
        crashed.file.take();
        crashed.lease.take();
        drop(crashed);
        let locked = manager.lock().unwrap();
        assert_eq!(locked.allocated().unwrap(), 5);
        assert_eq!(
            locked
                .ledger
                .entries
                .iter()
                .filter(|entry| entry.lease.is_some())
                .count(),
            0
        );
        assert!(
            locked
                .ledger
                .entries
                .iter()
                .find(|entry| entry.path.ends_with("crashed"))
                .unwrap()
                .changed
        );
        drop(locked);
        completed(&manager, &temp.path("next"), &policy, b"45678");
    }
    #[test]
    fn live_recording_growth_is_tracked_without_rewriting_the_ledger() {
        let temp = Temp::new();
        let manager = temp.manager();
        let mut writer = manager
            .writer(&temp.path("live"), &policy(10, 2, "stop"), 8)
            .unwrap();
        writer.write_all(b"abc").unwrap();
        let ledger = manager.directory.join("storage.json");
        let before = identity(&fs::metadata(&ledger).unwrap());
        assert_eq!(manager.lock().unwrap().ledger.entries[0].bytes, 3);
        assert_eq!(identity(&fs::metadata(&ledger).unwrap()), before);
        writer.finish().unwrap();
        assert_eq!(manager.lock().unwrap().ledger.entries[0].bytes, 3);
    }
    #[test]
    fn age_retention_never_removes_live_recordings() {
        let temp = Temp::new();
        let manager = temp.manager();
        let mut policy = policy(20, 4, "delete-oldest");
        policy.max_age_seconds = Some(10);
        completed(&manager, &temp.path("old"), &policy, b"old");
        {
            let mut locked = manager.lock().unwrap();
            locked.ledger.entries[0].created = now().saturating_sub(100);
            locked.persist().unwrap();
        }
        let live = manager.writer(&temp.path("live"), &policy, 10).unwrap();
        assert!(!temp.path("old").exists());
        {
            let mut locked = manager.lock().unwrap();
            locked.ledger.entries[0].created = now().saturating_sub(100);
            locked.persist().unwrap();
        }
        assert!(manager.writer(&temp.path("next"), &policy, 1).is_err());
        assert!(temp.path("live").exists());
        drop(live);
    }
    #[test]
    fn replaced_file_and_tampered_ledger_fail_closed() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(8, 1, "delete-oldest");
        completed(&manager, &temp.path("capture"), &policy, b"original");
        fs::remove_file(temp.path("capture")).unwrap();
        fs::write(temp.path("capture"), b"original").unwrap();
        assert!(manager.writer(&temp.path("next"), &policy, 1).is_err());
        let ledger_path = manager.directory.join("storage.json");
        let mut ledger: Ledger = serde_json::from_slice(&fs::read(&ledger_path).unwrap()).unwrap();
        ledger.entries[0].base = temp.path("wrong-base");
        fs::write(ledger_path, serde_json::to_vec(&ledger).unwrap()).unwrap();
        assert!(manager.lock().is_err());
        assert_eq!(fs::read(temp.path("capture")).unwrap(), b"original");
    }
}
