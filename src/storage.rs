//! Cross-process capture quotas and conservative retention of Capturefab-owned files.
//! Reservations are protected by OS locks, so an interrupted writer cannot leave
//! an unlimited recording or prevent its unused reservation being reclaimed.
//!
//! The ledger is a snapshot, storage.json, plus storage.journal: one JSON line
//! per changed entry, appended and made durable with fdatasync. A save appends a
//! few hundred bytes instead of rewriting every entry, and each process keeps
//! the parsed ledger and reads only the records appended since its last lock.
//! Records are numbered and the snapshot names the last one it includes; the
//! rare changes outside entries, such as marking a disk where removals freed no
//! space, write a new snapshot instead. Compaction writes a new snapshot and
//! then starts a new journal, once the journal is over COMPACT_BYTES and larger
//! than the snapshot, so the bytes written per save stay constant however many
//! files are kept. A crash between those two steps leaves records that loading
//! skips by number. A crash during an append leaves an unterminated last line,
//! which the next locker truncates; any other unreadable record fails closed.
//! Version 2 marks this format, so older binaries refuse the ledger as
//! unsupported instead of ignoring the journal; a version 1 ledger is converted
//! on its first lock.
use crate::types::Frame;
use anyhow::{Context, Result, anyhow, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, Metadata, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LEDGER_LIMIT: u64 = 64 * 1024 * 1024;
const LEDGER_VERSION: u32 = 2;
const SNAPSHOT: &str = "storage.json";
const JOURNAL: &str = "storage.journal";
const POLICY: &str = "storage-policy.json";
const SUMMARY: &str = "storage-summary.json";
pub(crate) static STATE: crate::ipc::StateSet = crate::ipc::StateSet {
    what: "the storage ledger and budget",
    names: &[POLICY, SUMMARY, JOURNAL, SNAPSHOT],
    lock: Some("storage.lock"),
};
/// Tests compact far sooner, so that they exercise it.
const COMPACT_BYTES: u64 = if cfg!(test) { 64 * 1024 } else { 1024 * 1024 };
/// Entries without a live reservation only change when something else touches
/// their files, so they are compared with the disk at most this often, and
/// always before retention, a Storage full refusal or a status report.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Lock timeouts a saved file waits through to be recorded in the ledger.
const COMMIT_TRIES: u32 = 3;
/// Expired files removed per save while the budget has room, so that a
/// backlog, such as a new age limit or a machine that was off, cannot hold
/// the lock for long. A full budget removes as many as it needs.
const EXPIRED_BATCH: usize = 32;
/// Free space a save must leave on the disk it writes to, as datasets do.
/// A fixed amount rather than a share of the disk: the free space reported
/// to unprivileged processes already excludes what ext4 reserves for the
/// system (5%), and a percentage would refuse saves on a large, mostly full
/// disk that still has gigabytes free.
pub const FREE_SPACE_FLOOR: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// This request policy within the configured one: the smaller limit of
    /// each, and delete-oldest when either side asks for it.
    pub(crate) fn constrained_by(&self, configured: &Self) -> Self {
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
    /// The last journal record included.
    #[serde(default)]
    sequence: u64,
    entries: Vec<Entry>,
    /// Disks, by device number, where removing files to keep the free-space
    /// floor did not free space, for example while a snapshot held it. Saves
    /// there remove nothing more for space until one fits without it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stalled: Vec<u64>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            sequence: 0,
            entries: Vec::new(),
            stalled: Vec::new(),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalHeader {
    version: u32,
    /// Random per journal file, so a cached read position is never applied
    /// to a different journal.
    journal: String,
}
/// One changed entry: its new state, or its removal.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    put: Option<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remove: Option<String>,
}
/// How a file looked when it was read, to tell whether it was replaced since.
#[derive(Clone, PartialEq)]
struct Stamp {
    identity: Identity,
    bytes: u64,
    modified_ns: Option<u64>,
}
/// What the parsed ledger was read from.
#[derive(Clone, Default)]
struct Position {
    snapshot: Option<Stamp>,
    journal: Option<String>,
    /// Journal bytes applied, including the header.
    offset: u64,
}
/// A process's parsed ledger, reused by its next lock while the snapshot and
/// journal it was read from are still in place.
struct Cached {
    directory: PathBuf,
    ledger: Ledger,
    position: Position,
    reconciled: Option<Instant>,
}
static CACHE: Mutex<Vec<Cached>> = Mutex::new(Vec::new());
fn take_cached(directory: &Path) -> Option<Cached> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let index = cache.iter().position(|c| c.directory == directory)?;
    Some(cache.swap_remove(index))
}

struct Manager {
    directory: PathBuf,
}
struct Locked {
    directory: PathBuf,
    lock: File,
    ledger: Ledger,
    position: Position,
    journal: Option<File>,
    /// Ids changed since the last commit.
    dirty: Vec<String>,
    /// When every entry was last compared with the disk, and whether that
    /// happened under this lock.
    reconciled: Option<Instant>,
    checked: bool,
    /// Memory matches the files on disk, so the next lock may reuse it.
    cacheable: bool,
    #[cfg(test)]
    commits: usize,
}
impl Drop for Locked {
    fn drop(&mut self) {
        if self.cacheable && self.dirty.is_empty() {
            CACHE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Cached {
                    directory: self.directory.clone(),
                    ledger: std::mem::take(&mut self.ledger),
                    position: std::mem::take(&mut self.position),
                    reconciled: self.reconciled,
                });
        }
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
/// Other processes held the storage lock for the whole wait; a later try
/// may succeed. Callers that retry recognize it with [`is_busy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "storage manager is busy; retry after other capture processes finish bookkeeping",
        )
    }
}
impl std::error::Error for Busy {}
/// Whether an error, or one it wraps, is the storage lock timing out.
pub fn is_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Busy>())
}
fn lock_wait(file: &File) -> Result<()> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(e) if contended(&e) => {
                if Instant::now() >= deadline {
                    return Err(Busy.into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).context("lock storage manager"),
        }
    }
}
impl Manager {
    /// The one place the storage bookkeeping directory is chosen; every
    /// ledger, policy and summary path is resolved from it. Leases of
    /// writers an older version left running are not moved, so their
    /// reservations are reclaimed as interrupted.
    fn global() -> Self {
        static ADOPTED: std::sync::Once = std::sync::Once::new();
        ADOPTED.call_once(|| crate::ipc::adopt_state(&STATE));
        Self {
            directory: crate::ipc::state_dir(),
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
        let mut result = Locked {
            directory,
            lock,
            ledger: Ledger::default(),
            position: Position::default(),
            journal: None,
            dirty: Vec::new(),
            reconciled: None,
            checked: false,
            cacheable: false,
            #[cfg(test)]
            commits: 0,
        };
        result.load()?;
        result.reconcile(false)?;
        Ok(result)
    }
    /// One journal commit records the deletion, or that the file changed.
    fn remove(&self, path: &Path) -> Result<bool> {
        let mut locked = self.lock()?;
        let Ok(index) = locked.find(path) else {
            return Ok(false);
        };
        let removed = locked.delete(index);
        locked.persist()?;
        removed
    }
    fn writer(&self, path: &Path, policy: &StoragePolicy, reserve: u64) -> Result<StorageWriter> {
        policy.validate()?;
        ensure!(reserve > 0, "capture storage reservation must be positive");
        ensure!(
            reserve <= policy.max_bytes,
            "Storage full: reservation {reserve} bytes exceeds the {} byte budget; lower --max-file-size or explicitly configure a larger storage budget",
            policy.max_bytes
        );
        let mut locked = self.lock()?;
        let policy = policy.constrained_by(&locked.configured_policy()?);
        ensure!(
            reserve <= policy.max_bytes,
            "Storage full: reservation {reserve} bytes exceeds the configured {} byte global budget; lower the recording reservation or explicitly configure a larger storage policy",
            policy.max_bytes
        );
        let (base, path) = output_path(path)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err(OutputExists(path).into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "capture output cannot be inspected: {}; check directory permissions",
                        path.display()
                    )
                });
            }
        }
        // A file removed outside Capturefab keeps its entry until a full
        // pass drops it. An entry whose writer is still live keeps its name.
        // Recorded paths are canonical, so comparing bytes is enough.
        let tracked = |locked: &Locked| {
            locked
                .ledger
                .entries
                .iter()
                .any(|entry| entry.path.as_os_str() == path.as_os_str())
        };
        if tracked(&locked) {
            locked.reconcile(true)?;
            if tracked(&locked) {
                return Err(OutputExists(path).into());
            }
        }
        locked.make_room(&policy, reserve, &base, device(&base))?;
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
                if error.kind() == io::ErrorKind::AlreadyExists {
                    return Err(OutputExists(path).into());
                }
                return Err(error).with_context(|| format!("cannot create capture output {}; check parent directory permissions, free disk space, and the storage limits", path.display()));
            }
        };
        let file_identity = identity(&file.metadata()?);
        locked.push(Entry {
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
            reread: true,
            failed: false,
            cut: false,
            finalized: false,
        })
    }
}

fn output_path(path: &Path) -> Result<(PathBuf, PathBuf)> {
    ensure!(
        !path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|b| b.is_ascii_control()),
        "capture output path must not contain control characters"
    );
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
/// Free space for unprivileged writes on the disk holding `path`, or None
/// when the file system does not report its size, as some network ones do not.
fn available(path: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(free) = tests::AVAILABLE.get() {
        return Some(free(path));
    }
    let stats = fs2::statvfs(path).ok()?;
    (stats.total_space() > 0).then(|| stats.available_space())
}
/// Where captures are saved by default: `Capturefab` in the user's home folder.
pub fn capture_root() -> PathBuf {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    home.filter(|home| !home.is_empty()).map_or_else(
        || PathBuf::from("Capturefab"),
        |home| PathBuf::from(home).join("Capturefab"),
    )
}
/// Free space for saves on the disk holding `path`, or its nearest existing
/// parent, when the file system reports it.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let path = std::path::absolute(path).ok()?;
    path.ancestors()
        .find(|dir| dir.exists())
        .and_then(available)
}
/// The device number of the disk holding `path`. Windows identities carry
/// none, so there files on different drives cannot be told apart.
fn device(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|meta| meta.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}
/// Whether `entry` lives on disk `device`, or None when that is unknown.
fn on_disk(entry: &Entry, device: Option<u64>) -> Option<bool> {
    #[cfg(unix)]
    {
        device.map(|device| device == entry.identity.device)
    }
    #[cfg(not(unix))]
    {
        let _ = (entry, device);
        None
    }
}
/// The persisted policy. It is only ever replaced whole, by rename, so it can
/// be read without the storage lock.
fn read_policy(directory: &Path) -> Result<StoragePolicy> {
    let path = directory.join(POLICY);
    match regular_private(&path, 4096) {
        Ok(_) => {
            let config: PolicyFile = serde_json::from_slice(&fs::read(&path)?).context(
                "invalid persisted storage policy; inspect storage-policy.json before changing it",
            )?;
            ensure!(
                config.version == 1,
                "unsupported persisted storage policy version"
            );
            config.policy.validate()?;
            Ok(config.policy)
        }
        Err(error) if not_found(&error) => Ok(StoragePolicy::default()),
        Err(error) => Err(error),
    }
}
fn not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}
fn check_entry(entry: &Entry) -> Result<()> {
    ensure!(
        entry.id.len() == 32 && entry.id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid storage ledger reservation id"
    );
    ensure!(
        entry.path.is_absolute()
            && entry.base.is_absolute()
            && entry.path.parent() == Some(entry.base.as_path()),
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
        entry.sha256.as_ref().is_none_or(
            |hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        ),
        "invalid storage capture hash"
    );
    Ok(())
}
/// A journal's header and the length of its line.
fn read_header(journal: &mut File) -> Result<(JournalHeader, u64)> {
    journal.seek(SeekFrom::Start(0))?;
    let mut line = Vec::new();
    BufReader::new(Read::by_ref(journal).take(4096)).read_until(b'\n', &mut line)?;
    let header = line
        .ends_with(b"\n")
        .then(|| serde_json::from_slice::<JournalHeader>(&line).ok())
        .flatten()
        .ok_or_else(|| anyhow!("invalid storage journal header; preserve storage.json and storage.journal and inspect them before resetting ownership"))?;
    ensure!(
        header.version == LEDGER_VERSION,
        "unsupported storage journal version"
    );
    Ok((header, line.len() as u64))
}
impl Locked {
    fn configured_policy(&self) -> Result<StoragePolicy> {
        read_policy(&self.directory)
    }
    fn configure_policy(&self, policy: &StoragePolicy) -> Result<()> {
        policy.validate()?;
        self.persist_bytes(
            POLICY,
            &serde_json::to_vec(&PolicyFile {
                version: 1,
                policy: policy.clone(),
            })?,
        )
    }
    /// The snapshot as it is now, or None before the first commit.
    fn stamp(&self) -> Result<Option<Stamp>> {
        match regular_private(&self.directory.join(SNAPSHOT), LEDGER_LIMIT) {
            Ok(meta) => Ok(Some(Stamp {
                identity: identity(&meta),
                bytes: meta.len(),
                modified_ns: meta.modified().ok().and_then(nanos),
            })),
            Err(error) if not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn open_journal(&self) -> Result<Option<File>> {
        let path = self.directory.join(JOURNAL);
        match regular_private(&path, 2 * LEDGER_LIMIT) {
            Ok(_) => Ok(Some(private_options().open(&path)?)),
            Err(error) if not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn load(&mut self) -> Result<()> {
        let snapshot = self.stamp()?;
        let resumed = match take_cached(&self.directory) {
            Some(cached) if cached.position.snapshot == snapshot => {
                self.ledger = cached.ledger;
                self.position = cached.position;
                self.reconciled = cached.reconciled;
                // Anything unexpected falls back to reading everything,
                // which fails closed.
                self.resume().unwrap_or(false)
            }
            _ => false,
        };
        if !resumed {
            self.load_all(snapshot)?;
        }
        self.cacheable = true;
        Ok(())
    }
    /// Apply what other processes appended since this one last held the lock.
    fn resume(&mut self) -> Result<bool> {
        let Some(mut journal) = self.open_journal()? else {
            return Ok(false);
        };
        let (header, _) = read_header(&mut journal)?;
        if self.position.journal.as_ref() != Some(&header.journal)
            || journal.metadata()?.len() < self.position.offset
        {
            return Ok(false);
        }
        self.replay(&mut journal, self.position.offset)?;
        self.journal = Some(journal);
        Ok(true)
    }
    fn load_all(&mut self, snapshot: Option<Stamp>) -> Result<()> {
        let path = self.directory.join(SNAPSHOT);
        self.ledger = match snapshot {
            Some(_) => serde_json::from_slice(&fs::read(&path)?).with_context(|| {
                format!(
                    "invalid storage ledger {}; preserve it and inspect before resetting ownership",
                    path.display()
                )
            })?,
            None => Ledger::default(),
        };
        ensure!(
            matches!(self.ledger.version, 1 | LEDGER_VERSION)
                && self.ledger.entries.len() <= 100_000,
            "unsupported or oversized storage ledger"
        );
        self.validate()?;
        self.position = Position {
            snapshot,
            ..Position::default()
        };
        self.journal = None;
        if self.ledger.version == 1 {
            // Version 1 had no journal. Rewriting it in this format makes
            // older binaries refuse it from now on.
            self.ledger.version = LEDGER_VERSION;
            self.compact()?;
            self.write_summary();
            return Ok(());
        }
        if let Some(mut journal) = self.open_journal()? {
            let (header, offset) = read_header(&mut journal)?;
            self.position.journal = Some(header.journal);
            self.replay(&mut journal, offset)?;
            self.journal = Some(journal);
        }
        Ok(())
    }
    /// Apply the complete records after `offset`. Records the snapshot already
    /// includes are skipped. An unterminated last line is an append that a
    /// crash cut short before anything relied on it, so it is cut off.
    fn replay(&mut self, journal: &mut File, offset: u64) -> Result<()> {
        journal.seek(SeekFrom::Start(offset))?;
        let mut tail = Vec::new();
        journal.read_to_end(&mut tail)?;
        let complete = tail
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |end| end + 1);
        if complete > 0 {
            self.apply(&tail[..complete])?;
        }
        let end = offset + complete as u64;
        if complete < tail.len() {
            journal.set_len(end)?;
            journal.sync_data()?;
        }
        self.position.offset = end;
        Ok(())
    }
    /// Apply journal lines in one pass, finding entries by id, and check the
    /// result once, so that loading takes time in proportion to the ledger
    /// and journal rather than to their product.
    fn apply(&mut self, lines: &[u8]) -> Result<()> {
        let mut index: HashMap<String, usize> = self
            .ledger
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.id.clone(), i))
            .collect();
        // A removed entry leaves a gap until the end, so indexes stay valid.
        let mut slots: Vec<Option<Entry>> = std::mem::take(&mut self.ledger.entries)
            .into_iter()
            .map(Some)
            .collect();
        for line in lines.split_inclusive(|&byte| byte == b'\n') {
            let record: Record = serde_json::from_slice(line).context("invalid storage journal record; preserve storage.json and storage.journal and inspect them before resetting ownership")?;
            if record.seq <= self.ledger.sequence {
                continue;
            }
            ensure!(
                record.seq == self.ledger.sequence + 1,
                "storage journal is missing records; preserve storage.json and storage.journal and inspect them before resetting ownership"
            );
            match (record.put, record.remove) {
                (Some(entry), None) => {
                    check_entry(&entry)?;
                    match index.get(&entry.id) {
                        Some(&i) => slots[i] = Some(entry),
                        None => {
                            index.insert(entry.id.clone(), slots.len());
                            slots.push(Some(entry));
                        }
                    }
                }
                (None, Some(id)) => {
                    if let Some(i) = index.remove(&id) {
                        slots[i] = None;
                    }
                }
                _ => bail!("invalid storage journal record"),
            }
            ensure!(
                index.len() <= 100_000,
                "unsupported or oversized storage ledger"
            );
            self.ledger.sequence = record.seq;
        }
        self.ledger.entries = slots.into_iter().flatten().collect();
        self.validate()
    }
    fn validate(&self) -> Result<()> {
        let mut ids = HashSet::new();
        let mut paths = HashSet::new();
        for entry in &self.ledger.entries {
            check_entry(entry)?;
            ensure!(
                ids.insert(&entry.id),
                "invalid storage ledger reservation id"
            );
            ensure!(
                paths.insert(&entry.path),
                "invalid storage ledger capture path"
            );
        }
        Ok(())
    }
    /// Every change to an entry records its id in `dirty`, through these or
    /// directly, so that persist() journals it.
    fn push(&mut self, entry: Entry) {
        self.dirty.push(entry.id.clone());
        self.ledger.entries.push(entry);
    }
    fn entry_mut(&mut self, index: usize) -> &mut Entry {
        let entry = &mut self.ledger.entries[index];
        self.dirty.push(entry.id.clone());
        entry
    }
    fn remove(&mut self, index: usize) {
        let entry = self.ledger.entries.remove(index);
        self.dirty.push(entry.id);
    }
    /// Commit changed entries to the journal; the first commit, or one that
    /// leaves the journal larger than the snapshot, writes a new snapshot.
    fn persist(&mut self) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.commits += 1;
        }
        if self.journal.is_some() {
            self.append()?;
            let snapshot = self
                .position
                .snapshot
                .as_ref()
                .map_or(0, |stamp| stamp.bytes);
            if self.position.offset >= COMPACT_BYTES.max(snapshot) {
                // The records are already durable; a failed compaction only
                // means the next lock reads everything again.
                let _ = self.compact();
            }
        } else {
            self.compact()?;
        }
        self.write_summary();
        Ok(())
    }
    fn append(&mut self) -> Result<()> {
        let mut lines = Vec::new();
        let mut sequence = self.ledger.sequence;
        let mut seen = HashSet::new();
        for id in self.dirty.iter().filter(|id| seen.insert(*id)) {
            let entry = self.ledger.entries.iter().find(|entry| &entry.id == id);
            sequence += 1;
            serde_json::to_writer(
                &mut lines,
                &Record {
                    seq: sequence,
                    put: entry.cloned(),
                    remove: entry.is_none().then(|| id.clone()),
                },
            )?;
            lines.push(b'\n');
        }
        let offset = self.position.offset;
        let journal = self
            .journal
            .as_mut()
            .ok_or_else(|| anyhow!("storage journal is not open"))?;
        journal
            .seek(SeekFrom::Start(offset))
            .and_then(|_| journal.write_all(&lines))
            .and_then(|()| journal.sync_data())
            .context("commit storage ledger; check private session directory permissions and free disk space")?;
        self.ledger.sequence = sequence;
        self.position.offset += lines.len() as u64;
        self.dirty.clear();
        Ok(())
    }
    /// Write every entry as a new snapshot, then start an empty journal.
    fn compact(&mut self) -> Result<()> {
        let result = self.rewrite();
        if result.is_err() {
            self.cacheable = false;
        }
        result
    }
    fn rewrite(&mut self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.ledger)?;
        ensure!(
            bytes.len() as u64 <= LEDGER_LIMIT,
            "storage ledger exceeds bookkeeping limit; reduce retained file count"
        );
        // Closed first: Windows may refuse to replace an open file.
        self.journal = None;
        self.persist_bytes(SNAPSHOT, &bytes)?;
        // The snapshot holds every change now. If the old journal is not
        // replaced below, its records are numbered at or before the
        // snapshot's sequence, so loading skips them.
        self.dirty.clear();
        self.position = Position {
            snapshot: self.stamp()?,
            ..Position::default()
        };
        let header = JournalHeader {
            version: LEDGER_VERSION,
            journal: nonce()?,
        };
        let mut bytes = serde_json::to_vec(&header)?;
        bytes.push(b'\n');
        self.persist_bytes(JOURNAL, &bytes)?;
        self.journal = self.open_journal()?;
        self.position.journal = Some(header.journal);
        self.position.offset = bytes.len() as u64;
        Ok(())
    }
    fn persist_bytes(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.replace(name, bytes, true).context(
            "commit storage ledger; check private session directory permissions and free disk space",
        )
    }
    /// Replace a bookkeeping file whole. Only a `durable` one waits for the
    /// disk, so a crash can at worst lose a derived file.
    fn replace(&self, name: &str, bytes: &[u8], durable: bool) -> Result<()> {
        let temp = self.directory.join(format!("storage-{}.tmp", nonce()?));
        let result = (|| -> Result<()> {
            let mut file = private_options().create_new(true).open(&temp)?;
            file.write_all(bytes)?;
            if durable {
                file.sync_all()?;
            }
            drop(file);
            fs::rename(&temp, self.directory.join(name))?;
            #[cfg(unix)]
            if durable {
                File::open(&self.directory)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    /// Totals for readers that must not take the lock, such as the GUI. Best
    /// effort: the ledger is the record, and the next commit rewrites this.
    fn write_summary(&self) {
        let Ok(allocated_bytes) = self.allocated() else {
            return;
        };
        let summary = UsageFile {
            version: 1,
            usage: Usage {
                allocated_bytes,
                files: self.ledger.entries.len() as u64,
            },
        };
        if let Ok(bytes) = serde_json::to_vec(&summary) {
            let _ = self.replace(SUMMARY, &bytes, false);
        }
    }
    /// Compare entries with their files. Live reservations are always read:
    /// their growth counts and a crashed writer's lease must be reclaimed. The
    /// rest only on a `full` pass, or when RECONCILE_INTERVAL has passed.
    fn reconcile(&mut self, full: bool) -> Result<()> {
        let due = self.reconciled.get_or_insert_with(Instant::now).elapsed() >= RECONCILE_INTERVAL;
        let full = (full || due) && !self.checked;
        // An error part way leaves changes that are not yet in dirty.
        self.cacheable = false;
        let mut missing = Vec::new();
        let mut verified = HashSet::new();
        for (index, entry) in self.ledger.entries.iter_mut().enumerate() {
            if !full && entry.lease.is_none() {
                continue;
            }
            let mut touched = false;
            let meta = current_meta(entry, &mut verified)?;
            let current_bytes = meta.as_ref().map_or(0, Metadata::len);
            if let Some(meta) = &meta {
                if identity(meta) != entry.identity && !entry.changed {
                    entry.changed = true;
                    touched = true;
                }
                if entry.lease.is_none()
                    && (entry.bytes != meta.len()
                        || entry.modified_ns != meta.modified().ok().and_then(nanos))
                    && !entry.changed
                {
                    entry.changed = true;
                    touched = true;
                }
            }
            if entry.bytes != current_bytes {
                entry.bytes = current_bytes;
                // Live lengths are read again on each lock; the durable
                // reservation already covers growth until finalization.
                touched |= entry.lease.is_none();
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
                    Err(error) if not_found(&error) => stale = true,
                    Err(error) => return Err(error),
                }
            }
            if stale {
                match meta {
                    None => missing.push(index),
                    Some(meta) if entry.lease.is_some() => {
                        entry.reserved = 0;
                        entry.lease = None;
                        // An interrupted writer has no committed content hash.
                        // Reclaim its unused reservation but preserve the file.
                        entry.changed = true;
                        entry.sha256 = None;
                        entry.modified_ns = meta.modified().ok().and_then(nanos);
                        touched = true;
                    }
                    Some(_) => {}
                }
                if let Some((lease, path)) = stale_lease {
                    drop(lease);
                    let _ = fs::remove_file(path);
                }
            }
            if touched {
                self.dirty.push(entry.id.clone());
            }
        }
        for index in missing.into_iter().rev() {
            self.remove(index);
        }
        if full {
            self.reconciled = Some(Instant::now());
            self.checked = true;
        }
        self.cacheable = true;
        self.persist()
    }
    fn allocated(&self) -> Result<u64> {
        self.ledger.entries.iter().try_fold(0u64, |sum, entry| {
            sum.checked_add(entry.bytes.max(entry.reserved))
                .ok_or_else(|| anyhow!("storage quota accounting overflow"))
        })
    }
    fn status(&mut self) -> Result<Value> {
        self.reconcile(true)?;
        let allocated = self.allocated()?;
        let policy = self.configured_policy()?;
        let over_limit = allocated > policy.max_bytes
            || self.ledger.entries.len() > policy.max_files as usize
            || self
                .ledger
                .entries
                .iter()
                .any(|entry| self.expiring(entry, &policy));
        Ok(json!({
            "ledger": self.directory.join(SNAPSHOT),
            "policy": policy,
            "over_limit": over_limit,
            "tracked_bytes": self.ledger.entries.iter().map(|entry| entry.bytes).sum::<u64>(),
            "allocated_bytes": allocated,
            "reserved_bytes": self.ledger.entries.iter().map(|entry| entry.reserved.saturating_sub(entry.bytes)).sum::<u64>(),
            "files": self.ledger.entries.len(),
            "active_reservations": self.ledger.entries.iter().filter(|entry| entry.lease.is_some()).count(),
            "externally_changed_files": self.ledger.entries.iter().filter(|entry| entry.changed).count(),
            "free_bytes": free_bytes(&capture_root()),
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
    /// Expired and removable. A protected file past the age limit, such as a
    /// live or interrupted recording or one awaiting upload, must not block
    /// saves that fit the budget.
    fn expiring(&self, entry: &Entry, policy: &StoragePolicy) -> bool {
        Self::removable(entry) && self.expired(entry, policy)
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
    /// Remove one capture if it is still exactly the file recorded; the
    /// caller commits.
    fn delete(&mut self, index: usize) -> Result<bool> {
        let entry = &self.ledger.entries[index];
        let Some(meta) = current_meta(entry, &mut HashSet::new())? else {
            self.remove(index);
            return Ok(true);
        };
        if !Self::removable(entry)
            || identity(&meta) != entry.identity
            || meta.len() != entry.bytes
            || meta.modified().ok().and_then(nanos) != entry.modified_ns
            || hash_file(&entry.path)? != entry.sha256.as_deref().unwrap()
        {
            let entry = self.entry_mut(index);
            entry.changed = true;
            entry.bytes = meta.len();
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
        self.remove(index);
        Ok(true)
    }
    /// Make room for a `reserve`-byte save in `base`, on disk `device`.
    fn make_room(
        &mut self,
        policy: &StoragePolicy,
        reserve: u64,
        base: &Path,
        device: Option<u64>,
    ) -> Result<()> {
        // Expired files beyond one batch, and space on the disk beyond one
        // round of removals, wait for later saves: freeing disk space can
        // fail to show, for example while a snapshot holds the blocks.
        let mut expiry_due = true;
        // The disk's shortfall before the round of removals for it, and the
        // bytes that round removed from it.
        let mut round: Option<(u64, u64)> = None;
        let mut removed = false;
        loop {
            let allocated = self.allocated()?;
            let expired = expiry_due
                && self
                    .ledger
                    .entries
                    .iter()
                    .any(|entry| self.expiring(entry, policy));
            let full = allocated
                .checked_add(reserve)
                .is_none_or(|sum| sum > policy.max_bytes)
                || self.ledger.entries.len() >= policy.max_files as usize;
            let (short, free) = self.shortfall(base, device, reserve);
            if short == 0 {
                self.stall(device, false);
            }
            if !full && !expired && short == 0 {
                return Ok(());
            }
            if !self.checked {
                // Decide on current facts: files removed since the last full
                // pass may already make room, and changed ones are protected.
                self.reconcile(true)?;
                continue;
            }
            if policy.on_full == "stop" && (full || expired) {
                bail!(
                    "Storage full: {allocated} allocated bytes and {} retained files (limits: {} bytes, {} files{}); enable explicit --delete-oldest retention, remove old captures yourself, or use storage configure --max-space/--max-files to increase the budget",
                    self.ledger.entries.len(),
                    policy.max_bytes,
                    policy.max_files,
                    if expired { "; maximum age reached" } else { "" }
                );
            }
            let low = |stalled: bool| {
                anyhow!(
                    "Storage full: {} needs at least 512 MiB free on its disk after this {reserve}-byte save, and {free} bytes are free{}; free some space, lower --max-file-size, or choose another output folder",
                    base.display(),
                    if stalled {
                        "; removing old captures there did not free space, perhaps because a snapshot still holds it, so none are removed until a save fits"
                    } else {
                        ""
                    }
                )
            };
            if short > 0 {
                // When not even a tenth of what the round removed shows as
                // free space, later rounds would only lose more files. A
                // tenth allows for compressed files freeing less than their
                // length.
                if let Some((before, freed)) = round
                    && before.saturating_sub(short).saturating_mul(10) < freed
                {
                    self.stall(device, true);
                }
                // Without device numbers, files on other drives could be
                // removed for nothing, so none are.
                let stalled = device.is_some_and(|device| self.ledger.stalled.contains(&device));
                ensure!(
                    policy.on_full != "stop" && round.is_none() && device.is_some() && !stalled,
                    low(stalled)
                );
            }
            let Some(victims) = self.victims(policy, reserve, device, short)? else {
                ensure!(full, low(false));
                if !self.ledger.entries.iter().any(Self::removable) {
                    bail!(
                        "Storage full: no unchanged, completed Capturefab files can be removed safely; active recordings and externally changed files are protected. Stop recordings, remove files yourself, or increase the storage limits"
                    );
                }
                bail!(
                    "Storage full: removing every unchanged, completed Capturefab file would still not make room, so {} removed; active recordings, files awaiting upload and externally changed files are protected. Stop recordings, remove files yourself, or increase the storage limits",
                    if removed { "no more were" } else { "none were" }
                );
            };
            let files = self.ledger.entries.len();
            let freed = self.delete_all(&victims, device)?;
            removed |= self.ledger.entries.len() < files;
            expiry_due = false;
            if short > 0 {
                round = Some((short, freed));
            }
        }
    }
    /// Record whether removing files frees space on disk `device`. This is
    /// rare, so it writes a new snapshot rather than a kind of record; if
    /// that fails, it costs at most one more round of removals.
    fn stall(&mut self, device: Option<u64>, stalled: bool) {
        let Some(device) = device else {
            return;
        };
        if self.ledger.stalled.contains(&device) == stalled {
            return;
        }
        if stalled {
            self.ledger.stalled.push(device);
        } else {
            self.ledger.stalled.retain(|&other| other != device);
        }
        let _ = self.compact();
    }
    /// Bytes to free on disk `device`, which holds `base`, so that `reserve`
    /// more, and the rest of every live reservation there, still leave
    /// FREE_SPACE_FLOOR; and the free space. Nothing when the file system
    /// does not report its size.
    fn shortfall(&self, base: &Path, device: Option<u64>, reserve: u64) -> (u64, u64) {
        let Some(free) = available(base) else {
            return (0, 0);
        };
        // A reservation on an unknown disk counts, which can only refuse a
        // save sooner.
        let promised: u64 = self
            .ledger
            .entries
            .iter()
            .filter(|entry| entry.lease.is_some() && on_disk(entry, device) != Some(false))
            .map(|entry| entry.reserved.saturating_sub(entry.bytes))
            .sum();
        let needed = FREE_SPACE_FLOOR
            .saturating_add(reserve)
            .saturating_add(promised);
        (needed.saturating_sub(free), free)
    }
    /// The files to remove so that `reserve` more bytes and one more file fit
    /// and `short` bytes are freed on disk `device`: the oldest removable
    /// ones, and up to EXPIRED_BATCH expired ones. None when even removing all
    /// of them would not be enough, so that nothing is deleted for a save that
    /// fails anyway.
    fn victims(
        &self,
        policy: &StoragePolicy,
        reserve: u64,
        device: Option<u64>,
        mut short: u64,
    ) -> Result<Option<Vec<String>>> {
        let mut candidates: Vec<&Entry> = self
            .ledger
            .entries
            .iter()
            .filter(|entry| Self::removable(entry))
            .collect();
        candidates.sort_by_key(|entry| entry.created);
        let full = |allocated: u64, files: usize| {
            allocated
                .checked_add(reserve)
                .is_none_or(|sum| sum > policy.max_bytes)
                || files >= policy.max_files as usize
        };
        let (mut allocated, mut files) = (self.allocated()?, self.ledger.entries.len());
        let mut victims = Vec::new();
        for entry in candidates {
            let here = on_disk(entry, device) == Some(true);
            if !full(allocated, files)
                && !(short > 0 && here)
                && !(victims.len() < EXPIRED_BATCH && self.expired(entry, policy))
            {
                continue;
            }
            victims.push(entry.id.clone());
            allocated = allocated.saturating_sub(entry.bytes.max(entry.reserved));
            files -= 1;
            if here {
                short = short.saturating_sub(entry.bytes);
            }
        }
        Ok((!full(allocated, files) && short == 0).then_some(victims))
    }
    /// Remove verified files, then commit once for all of them. Returns the
    /// bytes removed from disk `device`.
    fn delete_all(&mut self, ids: &[String], device: Option<u64>) -> Result<u64> {
        let mut freed = 0;
        let result = ids.iter().try_for_each(|id| {
            let Some(index) = self.ledger.entries.iter().position(|entry| &entry.id == id) else {
                return Ok(());
            };
            let entry = &self.ledger.entries[index];
            let bytes = if on_disk(entry, device) == Some(true) {
                entry.bytes
            } else {
                0
            };
            if self.delete(index)? {
                freed += bytes;
            }
            Ok(())
        });
        self.persist()?;
        result.map(|()| freed)
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
    /// Read the file back to confirm the streamed hash when finishing.
    reread: bool,
    failed: bool,
    /// Writing stopped early, at the reservation or on a write error, with
    /// every byte before it intact; see `finish_cut`.
    cut: bool,
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
            self.cut = true;
            return Err(io::Error::other(format!(
                "Storage full: {} reached its maximum file size of {} bytes; raise the maximum file size (--max-file-size) or record shorter segments",
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
                // A failed write wrote nothing, so the bytes before it stand.
                self.failed = true;
                self.cut = true;
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
    /// Finish bytes written in one call just before, as `save_bytes` does:
    /// the streamed hash stands for the file (retention still verifies it
    /// before deleting), and a busy ledger is waited out, for a caller that
    /// would otherwise save the same data again under another name, such as
    /// an automatic capture's frame.
    pub fn finish_written(mut self) -> Result<()> {
        self.reread = false;
        self.commit(COMMIT_TRIES)
    }
    /// Finish a file whose bytes are all written. Its name is now taken, so
    /// a caller that saved again would find its own file in the way: a busy
    /// ledger is waited out up to `tries` times, and is then reported as a
    /// saved file left unrecorded rather than as busy.
    fn commit(mut self, tries: u32) -> Result<()> {
        let mut result = self.finalize();
        for _ in 1..tries {
            if !result.as_ref().is_err_and(is_busy) {
                break;
            }
            result = self.finalize();
        }
        self.finalized = true;
        match result {
            Err(error) if is_busy(&error) && !self.failed => Err(anyhow!(
                "capture {} is saved, but the storage manager stayed busy and did not record it; the file is kept and never deleted by retention",
                self.path.display()
            )),
            result => result,
        }
    }
    /// Finish a recording whose writing was cut short, at its reservation
    /// or by a write error. Its container plays up to its last complete
    /// part, so the bytes written are kept and recorded like a finished
    /// file's, where `finish` removes them. Otherwise the same as `finish`.
    pub fn finish_cut(mut self) -> Result<()> {
        if self.cut {
            self.failed = false;
        }
        self.finish()
    }
    /// Whether writing stopped early, at the reservation or on a write error.
    pub fn cut(&self) -> bool {
        self.cut
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn bytes_written(&self) -> u64 {
        self.written
    }
    /// SHA-256 of the bytes written so far, as lowercase hex.
    pub fn sha256(&self) -> String {
        format!("{:x}", self.hash.clone().finalize())
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
        // Hash before taking the lock, so that reading back a long recording
        // does not hold up other processes' saves. Under the lock, the same
        // identity, length and time show that nothing touched it since.
        let disappeared = || format!("capture output disappeared: {}", self.path.display());
        let changed = || {
            anyhow!(
                "capture output changed externally; preserving {} without enabling retention deletion",
                self.path.display()
            )
        };
        let hashed = fs::symlink_metadata(&self.path).with_context(disappeared)?;
        ensure!(self.ours(&hashed, None), changed());
        let hash = self.sha256();
        ensure!(
            !self.reread || hash_file(&self.path)? == hash,
            "capture content changed externally; preserving {} without enabling retention deletion",
            self.path.display()
        );
        let mut locked = self.manager.lock()?;
        let index = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.id == self.id)
            .ok_or_else(|| anyhow!("capture reservation disappeared from storage ledger"))?;
        let meta = current_meta(&locked.ledger.entries[index], &mut HashSet::new())?
            .ok_or_else(|| anyhow!(disappeared()))?;
        ensure!(self.ours(&meta, Some(&hashed)), changed());
        let entry = locked.entry_mut(index);
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
    /// Whether `meta` is still the file this writer created, at the length it
    /// wrote and, when given, with the time it had when it was hashed.
    fn ours(&self, meta: &Metadata, hashed: Option<&Metadata>) -> bool {
        meta.is_file()
            && !meta.file_type().is_symlink()
            && identity(meta) == self.identity
            && meta.len() == self.written
            && hashed.is_none_or(|hashed| meta.modified().ok() == hashed.modified().ok())
    }
    fn cleanup(&mut self) -> Result<()> {
        // As in finalize, read the partial output before taking the lock.
        let hashed = fs::symlink_metadata(&self.path)
            .ok()
            .filter(|meta| self.ours(meta, None));
        let content = hashed.is_some() && hash_file(&self.path)? == self.sha256();
        let mut locked = self.manager.lock()?;
        if let Some(index) = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.id == self.id)
        {
            let entry = &locked.ledger.entries[index];
            let owned = content
                && current_meta(entry, &mut HashSet::new())?
                    .is_some_and(|meta| self.ours(&meta, hashed.as_ref()));
            if owned {
                fs::remove_file(&self.path)?;
                locked.remove(index);
            } else {
                let entry = locked.entry_mut(index);
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
/// A finished output's size and SHA-256, as recorded in the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub bytes: u64,
    pub sha256: String,
}
/// The output path is already taken. Callers that pick names retry with the
/// next free one when this is in the error chain; nothing is ever replaced.
#[derive(Debug)]
pub struct OutputExists(pub PathBuf);
impl std::fmt::Display for OutputExists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "capture output already exists: {}; choose a new file name",
            self.0.display()
        )
    }
}
impl std::error::Error for OutputExists {}
/// Save already encoded bytes as a new file.
pub fn save_bytes(bytes: &[u8], path: &Path, policy: &StoragePolicy) -> Result<Stored> {
    let mut writer = create_writer(path, policy, bytes.len() as u64)?;
    writer.write_all(bytes)?;
    let stored = Stored {
        bytes: writer.bytes_written(),
        sha256: writer.sha256(),
    };
    writer.finish_written()?;
    Ok(stored)
}
pub fn save_hashed(
    frame: &Frame,
    path: &Path,
    format: &str,
    policy: &StoragePolicy,
) -> Result<Stored> {
    let bytes = crate::frame::encode(frame, format)?;
    save_bytes(&bytes, path, policy)
}
pub fn save(frame: &Frame, path: &Path, format: &str, policy: &StoragePolicy) -> Result<()> {
    save_hashed(frame, path, format, policy).map(drop)
}
/// Hold a finished capture back from retention while it waits for upload,
/// or release it. Returns the SHA-256 recorded when the file was finished.
pub fn hold_for_upload(path: &Path, held: bool) -> Result<String> {
    let mut locked = Manager::global().lock()?;
    let index = locked.find(path)?;
    let entry = &locked.ledger.entries[index];
    let sha256 = entry
        .sha256
        .clone()
        .ok_or_else(|| anyhow!("{} has no recorded checksum", entry.path.display()))?;
    if entry.pending_upload != held {
        locked.entry_mut(index).pending_upload = held;
        locked.persist()?;
    }
    Ok(sha256)
}
/// Delete the local copy of an uploaded capture, only if it is still exactly
/// the file Capturefab wrote (identity, size, time and SHA-256).
pub fn remove_uploaded(path: &Path) -> Result<bool> {
    let mut locked = Manager::global().lock()?;
    let index = locked.find(path)?;
    locked.entry_mut(index).pending_upload = false;
    let removed = locked.delete(index);
    locked.persist()?;
    removed
}
/// Delete a finished capture only if it is still exactly the file Capturefab
/// wrote. False when the ledger does not track it or it changed; it is kept.
pub fn remove(path: &Path) -> Result<bool> {
    Manager::global().remove(path)
}
pub fn quota_status() -> Result<Value> {
    Manager::global().lock()?.status()
}
/// The persisted global policy, read without waiting for the storage lock.
pub fn configured_policy() -> Result<StoragePolicy> {
    let manager = Manager::global();
    crate::ipc::ensure_private_dir(&manager.directory)?;
    read_policy(&manager.directory)
}
/// Ledger totals as of the last commit, read without the storage lock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub allocated_bytes: u64,
    pub files: u64,
}
#[derive(Serialize, Deserialize)]
struct UsageFile {
    version: u32,
    usage: Usage,
}
/// Current totals for displays that refresh often, without the storage lock
/// or reading the ledger. None while unknown, until the next commit.
pub fn usage() -> Result<Option<Usage>> {
    let manager = Manager::global();
    crate::ipc::ensure_private_dir(&manager.directory)?;
    read_usage(&manager.directory)
}
fn read_usage(directory: &Path) -> Result<Option<Usage>> {
    let path = directory.join(SUMMARY);
    match regular_private(&path, 4096) {
        Ok(_) => {
            let summary: UsageFile = serde_json::from_slice(&fs::read(&path)?)
                .context("invalid storage usage summary")?;
            Ok((summary.version == 1).then_some(summary.usage))
        }
        // Nothing saved yet, or written by a version without the summary.
        Err(error) if not_found(&error) => Ok(fs::symlink_metadata(directory.join(SNAPSHOT))
            .is_err()
            .then(Usage::default)),
        Err(error) => Err(error),
    }
}
pub fn configure_policy(policy: &StoragePolicy) -> Result<Value> {
    let mut locked = Manager::global().lock()?;
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
    /// Free space a test reports for a save's directory.
    type Space = fn(&Path) -> u64;
    thread_local! {
        pub(super) static AVAILABLE: std::cell::Cell<Option<Space>> =
            const { std::cell::Cell::new(None) };
    }
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
    #[test]
    fn a_lock_held_past_the_timeout_is_busy_through_any_context() {
        let temp = Temp::new();
        let path = temp.path("storage.lock");
        let held = File::create(&path).unwrap();
        held.lock_exclusive().unwrap();
        let waiting = File::open(&path).unwrap();
        let started = Instant::now();
        let error = lock_wait(&waiting)
            .context("hold capture for upload")
            .unwrap_err();
        assert!(started.elapsed() >= LOCK_TIMEOUT);
        assert!(is_busy(&error), "{error:#}");
        // The wording callers and exit codes saw before stays the same.
        assert!(
            format!("{error:#}").ends_with(
                "storage manager is busy; retry after other capture processes finish bookkeeping"
            ),
            "{error:#}"
        );
        assert!(!is_busy(&anyhow!("lock storage manager")));
    }
    /// A saved file whose ledger record waits behind another process's lock.
    fn saved_shot(temp: &Temp) -> (StorageWriter, PathBuf, File) {
        let manager = temp.manager();
        let path = temp.path("shot.raw");
        let mut writer = manager.writer(&path, &policy(1024, 10, "stop"), 4).unwrap();
        writer.reread = false;
        writer.write_all(b"shot").unwrap();
        let held = File::open(manager.directory.join("storage.lock")).unwrap();
        held.lock_exclusive().unwrap();
        (writer, path, held)
    }
    #[test]
    fn a_saved_file_waits_out_a_ledger_busy_only_for_its_record() {
        let temp = Temp::new();
        let (writer, path, held) = saved_shot(&temp);
        let release = thread::spawn(move || {
            thread::sleep(LOCK_TIMEOUT + Duration::from_millis(500));
            drop(held);
        });
        let started = Instant::now();
        writer.commit(2).unwrap();
        assert!(started.elapsed() >= LOCK_TIMEOUT);
        release.join().unwrap();
        let locked = temp.manager().lock().unwrap();
        let entry = &locked.ledger.entries[locked.find(&path).unwrap()];
        assert_eq!(entry.bytes, 4);
        assert!(entry.sha256.is_some() && !entry.changed);
    }
    #[test]
    fn a_saved_file_the_ledger_stays_too_busy_to_record_is_kept_and_not_busy() {
        let temp = Temp::new();
        let (writer, path, held) = saved_shot(&temp);
        let error = writer.commit(1).unwrap_err();
        drop(held);
        // Saving again would only find this file in the way.
        assert!(!is_busy(&error), "{error:#}");
        assert!(
            format!("{error:#}").contains("is saved, but the storage manager stayed busy"),
            "{error:#}"
        );
        assert_eq!(crate::cli::error_code(&error), ("operation_failed", 1));
        assert_eq!(fs::read(&path).unwrap(), b"shot");
        // The next manager reclaims the reservation and keeps the file out of
        // retention, since its content was never recorded.
        let locked = temp.manager().lock().unwrap();
        let entry = &locked.ledger.entries[locked.find(&path).unwrap()];
        assert_eq!(entry.bytes, 4);
        assert!(entry.changed && entry.sha256.is_none());
    }
    fn completed(manager: &Manager, path: &Path, policy: &StoragePolicy, data: &[u8]) {
        let mut writer = manager.writer(path, policy, data.len() as u64).unwrap();
        writer.write_all(data).unwrap();
        writer.finish().unwrap();
    }
    #[test]
    fn byte_units_are_checked() {
        let error = serde_json::from_value::<StoragePolicy>(json!({"max_fiels": 5})).unwrap_err();
        assert!(error.to_string().contains("unknown field `max_fiels`"));
        assert_eq!(
            serde_json::from_value::<StoragePolicy>(json!({"max_files": 5}))
                .unwrap()
                .max_files,
            5
        );
        assert_eq!(parse_bytes("10GiB").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_bytes("10G").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_bytes("10GB").unwrap(), 10_000_000_000);
        assert_eq!(parse_bytes(" 5 KB ").unwrap(), 5000);
        for invalid in ["0", "-1", "1.5GB", "10words", "18446744073709551615TB"] {
            assert!(parse_bytes(invalid).is_err());
        }
    }
    #[test]
    fn invalid_storage_inputs_do_not_create_output_directories_or_delete_retained_files() {
        let temp = Temp::new();
        let manager = temp.manager();
        completed(
            &manager,
            &temp.path("retained"),
            &policy(10, 1, "stop"),
            b"data",
        );
        for invalid_path in ["new/control\0.png", "new/control\n.png"] {
            assert!(
                manager
                    .writer(&temp.path(invalid_path), &policy(10, 1, "delete-oldest"), 1)
                    .is_err()
            );
            assert!(!temp.path("new").exists());
            assert!(temp.path("retained").exists());
        }
        manager
            .lock()
            .unwrap()
            .configure_policy(&policy(10, 1, "stop"))
            .unwrap();
        assert!(
            manager
                .writer(
                    &temp.path("new/output.png"),
                    &policy(20, 1, "delete-oldest"),
                    11
                )
                .is_err()
        );
        assert!(!temp.path("new").exists());
        assert!(temp.path("retained").exists());
        assert!(
            manager
                .writer(&temp.path("zero/output.png"), &policy(10, 1, "stop"), 0)
                .err()
                .unwrap()
                .to_string()
                .contains("positive")
        );
        assert!(!temp.path("zero").exists());
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
            let mut locked = manager.lock().unwrap();
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
    fn removing_a_capture_deletes_it_only_if_unchanged_with_one_commit() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(100, 10, "stop");
        for name in ["kept", "gone", "edited"] {
            completed(&manager, &temp.path(name), &policy, name.as_bytes());
        }
        fs::write(temp.path("untracked"), b"a user file").unwrap();
        let journal = manager.directory.join(JOURNAL);
        let records = || fs::read_to_string(&journal).unwrap().lines().count();
        let before = records();
        assert!(manager.remove(&temp.path("gone")).unwrap());
        assert!(!temp.path("gone").exists());
        assert_eq!(records(), before + 1);
        fs::write(temp.path("edited"), b"changed").unwrap();
        assert!(!manager.remove(&temp.path("edited")).unwrap());
        assert_eq!(fs::read(temp.path("edited")).unwrap(), b"changed");
        assert_eq!(records(), before + 2, "the change is recorded");
        assert!(!manager.remove(&temp.path("untracked")).unwrap());
        assert!(!manager.remove(&temp.path("missing")).unwrap());
        assert_eq!(fs::read(temp.path("untracked")).unwrap(), b"a user file");
        assert_eq!(records(), before + 2);
        forget(&manager);
        let locked = manager.lock().unwrap();
        let entries: Vec<_> = locked
            .ledger
            .entries
            .iter()
            .map(|entry| (entry.path.file_name().unwrap().to_owned(), entry.changed))
            .collect();
        assert_eq!(entries, [("kept".into(), false), ("edited".into(), true)]);
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
    fn a_recording_cut_at_its_reservation_keeps_what_it_wrote() {
        let temp = Temp::new();
        let manager = temp.manager();
        let path = temp.path("cut.mkv");
        let mut writer = manager.writer(&path, &policy(16, 2, "stop"), 4).unwrap();
        writer.write_all(b"abc").unwrap();
        assert!(!writer.cut());
        let error = writer.write_all(b"de").unwrap_err().to_string();
        assert!(error.contains("Storage full") && error.contains("maximum file size"));
        assert!(writer.cut());
        writer.finish_cut().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"abc");
        let locked = manager.lock().unwrap();
        let entry = &locked.ledger.entries[locked.find(&path).unwrap()];
        assert_eq!((entry.bytes, entry.reserved), (3, 0));
        assert!(entry.sha256.is_some() && !entry.changed && entry.lease.is_none());
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
        let locked = without_leases(&manager);
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
    /// Another test can be between fork and exec, temporarily retaining a
    /// dropped writer's close-on-exec lease. Reclaim only after it closes.
    fn without_leases(manager: &Manager) -> Locked {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let locked = manager.lock().unwrap();
            if locked
                .ledger
                .entries
                .iter()
                .all(|entry| entry.lease.is_none())
            {
                return locked;
            }
            assert!(
                Instant::now() < deadline,
                "dropped writer lease did not close"
            );
            drop(locked);
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    #[cfg(unix)]
    fn inherited_lease_keeps_the_reservation_until_exec_releases_it() {
        const HELPER: &str = "CAPTUREFAB_TEST_STORAGE_INHERITED_LEASE";
        const COMPLETE: &str = "inherited lease proof completed";
        if std::env::var_os(HELPER).is_none() {
            // Run the deliberate fork hold after exec has closed unrelated
            // parallel tests' close-on-exec leases in the original process.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::tests::inherited_lease_keeps_the_reservation_until_exec_releases_it",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && stdout.contains(COMPLETE),
                "isolated inherited lease proof failed: {}\n{stdout}{stderr}",
                output.status
            );
            return;
        }
        use std::net::Shutdown;
        use std::os::{
            fd::AsRawFd,
            unix::{net::UnixStream, process::CommandExt},
        };
        use std::process::{ExitStatus, Stdio};
        unsafe extern "C" {
            fn read(fd: i32, buffer: *mut std::ffi::c_void, count: usize) -> isize;
            fn write(fd: i32, buffer: *const std::ffi::c_void, count: usize) -> isize;
        }
        struct Forked {
            channel: UnixStream,
            thread: Option<thread::JoinHandle<io::Result<ExitStatus>>>,
        }
        impl Forked {
            fn finish(&mut self) -> ExitStatus {
                self.channel.write_all(b"E").unwrap();
                self.thread.take().unwrap().join().unwrap().unwrap()
            }
        }
        impl Drop for Forked {
            fn drop(&mut self) {
                // Release a child even if readiness, accounting or an assertion
                // fails. The socket timeout also bounds its pre-exec wait.
                let _ = self.channel.shutdown(Shutdown::Both);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(10, 4, "stop");
        completed(&manager, &temp.path("finished"), &policy, b"ab");
        let mut crashed = manager.writer(&temp.path("crashed"), &policy, 8).unwrap();
        crashed.write_all(b"123").unwrap();
        let (parent, child) = UnixStream::pair().unwrap();
        parent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        parent
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        child
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        child
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let executable = std::env::current_exe().unwrap();
        let forked = thread::spawn(move || {
            let fd = child.as_raw_fd();
            let mut command = std::process::Command::new(executable);
            command
                .arg("--help")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: after fork this callback uses only async-signal-safe
            // read/write and errno access, with a pre-existing socket.
            unsafe {
                command.pre_exec(move || {
                    let mut byte = b'R';
                    if write(fd, std::ptr::from_ref(&byte).cast(), 1) != 1
                        || read(fd, std::ptr::from_mut(&mut byte).cast(), 1) != 1
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let result = command.status();
            drop(child);
            result
        });
        let mut forked = Forked {
            channel: parent,
            thread: Some(forked),
        };
        let mut ready = [0];
        forked.channel.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [b'R']);
        crashed.finalized = true;
        crashed.file.take();
        crashed.lease.take();
        drop(crashed);
        let retained = manager.lock().unwrap();
        assert_eq!(retained.allocated().unwrap(), 10);
        assert_eq!(
            retained
                .ledger
                .entries
                .iter()
                .filter(|entry| entry.lease.is_some())
                .count(),
            1
        );
        drop(retained);
        assert!(forked.finish().success());
        let reclaimed = manager.lock().unwrap();
        assert_eq!(reclaimed.allocated().unwrap(), 5);
        assert!(
            reclaimed
                .ledger
                .entries
                .iter()
                .all(|entry| entry.lease.is_none())
        );
        println!("{COMPLETE}");
    }
    #[test]
    fn live_recording_growth_is_tracked_without_rewriting_the_ledger() {
        let temp = Temp::new();
        let manager = temp.manager();
        let mut writer = manager
            .writer(&temp.path("live"), &policy(10, 2, "stop"), 8)
            .unwrap();
        writer.write_all(b"abc").unwrap();
        let ledger = manager.directory.join(SNAPSHOT);
        let before = identity(&fs::metadata(&ledger).unwrap());
        let journal = fs::metadata(manager.directory.join(JOURNAL)).unwrap().len();
        assert_eq!(manager.lock().unwrap().ledger.entries[0].bytes, 3);
        assert_eq!(identity(&fs::metadata(&ledger).unwrap()), before);
        assert_eq!(
            fs::metadata(manager.directory.join(JOURNAL)).unwrap().len(),
            journal
        );
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
        age(&manager, "old", 100);
        let live = manager.writer(&temp.path("live"), &policy, 10).unwrap();
        assert!(!temp.path("old").exists());
        age(&manager, "live", 100);
        // An expired live recording is protected and must not block a save
        // that fits the budget.
        completed(&manager, &temp.path("next"), &policy, b"1");
        assert!(temp.path("live").exists());
        drop(live);
    }
    fn age(manager: &Manager, name: &str, seconds: u64) {
        edit(manager, name, |entry| {
            entry.created = now().saturating_sub(seconds);
        });
    }
    fn edit(manager: &Manager, name: &str, change: impl FnOnce(&mut Entry)) {
        let mut locked = manager.lock().unwrap();
        let index = locked
            .ledger
            .entries
            .iter()
            .position(|entry| entry.path.ends_with(name))
            .unwrap();
        change(locked.entry_mut(index));
        locked.persist().unwrap();
    }
    #[test]
    fn protected_expired_files_do_not_block_saves_below_budget() {
        let temp = Temp::new();
        let manager = temp.manager();
        let mut policy = policy(100, 10, "delete-oldest");
        policy.max_age_seconds = Some(10);
        completed(&manager, &temp.path("interrupted"), &policy, b"partial");
        completed(&manager, &temp.path("queued"), &policy, b"queued");
        completed(&manager, &temp.path("expired"), &policy, b"old");
        fs::write(temp.path("interrupted"), b"changed elsewhere").unwrap();
        edit(&manager, "queued", |entry| entry.pending_upload = true);
        for name in ["interrupted", "queued", "expired"] {
            age(&manager, name, 100);
        }
        completed(&manager, &temp.path("next"), &policy, b"new");
        assert!(!temp.path("expired").exists(), "expired files still go");
        assert_eq!(
            fs::read(temp.path("interrupted")).unwrap(),
            b"changed elsewhere"
        );
        assert!(temp.path("queued").exists());
        assert_eq!(
            manager.lock().unwrap().status().unwrap()["over_limit"],
            false
        );
        // Protected files only stop a save the budget cannot hold, and then
        // nothing is deleted for it.
        let error = manager
            .writer(&temp.path("huge"), &policy, 90)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("would still not make room"), "{error}");
        assert!(temp.path("next").exists());
        edit(&manager, "next", |entry| entry.pending_upload = true);
        let error = manager
            .writer(&temp.path("huge"), &policy, 90)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("Storage full: no unchanged"), "{error}");
    }
    #[test]
    fn a_save_that_still_cannot_fit_says_whether_files_were_removed_for_it() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(20, 10, "delete-oldest");
        for (name, data) in [("a", &[1; 8][..]), ("b", &[2; 8]), ("c", &[3; 2])] {
            completed(&manager, &temp.path(name), &policy, data);
        }
        // The same length and time with other content: only its hash shows
        // that b changed, once it is about to be removed.
        let b = temp.path("b");
        let modified = fs::metadata(&b).unwrap().modified().unwrap();
        let mut file = OpenOptions::new().write(true).open(&b).unwrap();
        file.write_all(&[9; 8]).unwrap();
        file.set_modified(modified).unwrap();
        drop(file);
        let error = manager
            .writer(&temp.path("large"), &policy, 18)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("so no more were removed"), "{error}");
        assert!(!temp.path("a").exists() && temp.path("c").exists());
        assert_eq!(fs::read(&b).unwrap(), [9; 8]);
    }
    #[test]
    fn stop_policy_with_an_age_limit_stops_only_for_removable_files() {
        let temp = Temp::new();
        let manager = temp.manager();
        let mut policy = policy(100, 10, "stop");
        policy.max_age_seconds = Some(10);
        completed(&manager, &temp.path("old"), &policy, b"old");
        let live = manager.writer(&temp.path("live"), &policy, 10).unwrap();
        age(&manager, "live", 100);
        completed(&manager, &temp.path("next"), &policy, b"next");
        age(&manager, "old", 100);
        let error = manager
            .writer(&temp.path("blocked"), &policy, 1)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("maximum age reached"), "{error}");
        assert!(temp.path("old").exists());
        drop(live);
    }
    #[test]
    fn a_name_freed_outside_capturefab_can_be_saved_again() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(100, 10, "stop");
        let path = temp.path("a.png");
        completed(&manager, &path, &policy, b"first");
        fs::remove_file(&path).unwrap();
        // A new process, whose lock does not compare finished files with
        // the disk yet.
        forget(&manager);
        completed(&manager, &path, &policy, b"second");
        forget(&manager);
        let locked = manager.lock().unwrap();
        assert_eq!(locked.ledger.entries.len(), 1);
        assert_eq!(locked.allocated().unwrap(), 6);
        drop(locked);
        // A live writer keeps its name even when its file is removed.
        let live = manager.writer(&temp.path("live"), &policy, 10).unwrap();
        fs::remove_file(temp.path("live")).unwrap();
        let error = manager
            .writer(&temp.path("live"), &policy, 1)
            .err()
            .unwrap();
        assert!(error.downcast_ref::<OutputExists>().is_some(), "{error:#}");
        drop(live);
        drop(without_leases(&manager));
        forget(&manager);
        completed(&manager, &temp.path("live"), &policy, b"again");
        forget(&manager);
        assert_eq!(manager.lock().unwrap().ledger.entries.len(), 2);
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
    /// What a new process would see: its next lock reads everything again.
    fn forget(manager: &Manager) {
        take_cached(&fs::canonicalize(&manager.directory).unwrap());
    }
    /// Track `count` small finished files without a durable save for each.
    fn populate(manager: &Manager, temp: &Temp, count: usize) {
        let base = fs::canonicalize(&temp.0).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(b"x"));
        let mut locked = manager.lock().unwrap();
        for index in 0..count {
            let path = base.join(format!("filler-{index}"));
            fs::write(&path, b"x").unwrap();
            let meta = fs::metadata(&path).unwrap();
            locked.push(Entry {
                id: nonce().unwrap(),
                path,
                base: base.clone(),
                identity: identity(&meta),
                created: now(),
                bytes: 1,
                reserved: 0,
                lease: None,
                sha256: Some(sha256.clone()),
                modified_ns: meta.modified().ok().and_then(nanos),
                changed: false,
                pending_upload: false,
            });
        }
        locked.compact().unwrap();
    }
    /// Bytes `action` writes to the ledger files: what the journal grew by, or
    /// the new snapshot and journal when it compacted.
    fn ledger_writes(manager: &Manager, action: impl FnOnce()) -> (u64, bool) {
        let file = |name| fs::metadata(manager.directory.join(name)).unwrap();
        let (snapshot, journal) = (identity(&file(SNAPSHOT)), file(JOURNAL).len());
        action();
        let (after, grown) = (file(SNAPSHOT), file(JOURNAL).len());
        if identity(&after) == snapshot {
            (grown - journal, false)
        } else {
            (after.len() + grown, true)
        }
    }
    #[test]
    fn a_save_writes_the_same_ledger_bytes_however_many_files_are_kept() {
        let per_save = |kept: usize| {
            let temp = Temp::new();
            let manager = temp.manager();
            populate(&manager, &temp, kept);
            let policy = policy(1 << 30, 100_000, "stop");
            let saves = 10;
            let written: u64 = (0..saves)
                .map(|index| {
                    let path = temp.path(&format!("frame-{index}"));
                    ledger_writes(&manager, || completed(&manager, &path, &policy, &[7; 4096])).0
                })
                .sum();
            written / saves
        };
        let (few, many) = (per_save(10), per_save(5_000));
        assert!(
            many < 2048 && many <= few + 64,
            "{few} bytes per save with 10 files kept, {many} with 5,000"
        );
    }
    #[test]
    fn compaction_keeps_ledger_writes_proportional_to_changes() {
        let temp = Temp::new();
        let manager = temp.manager();
        populate(&manager, &temp, 100);
        let (mut total, mut largest, mut compactions) = (0, 0, 0);
        let rounds = 200;
        for round in 0..rounds {
            let (bytes, compacted) = ledger_writes(&manager, || {
                edit(&manager, "filler-7", |entry| {
                    entry.pending_upload = round % 2 == 0;
                });
            });
            total += bytes;
            if compacted {
                compactions += 1;
            } else {
                largest = largest.max(bytes);
            }
        }
        assert!(compactions >= 1, "the journal was never compacted");
        assert!(
            total <= 3 * rounds * largest,
            "{total} bytes for {rounds} changes of at most {largest} bytes"
        );
        let snapshot = fs::metadata(manager.directory.join(SNAPSHOT))
            .unwrap()
            .len();
        let journal = fs::metadata(manager.directory.join(JOURNAL)).unwrap().len();
        assert!(journal <= COMPACT_BYTES.max(snapshot) + largest);
    }
    #[test]
    fn loading_a_full_journal_takes_time_in_proportion_to_it() {
        let temp = Temp::new();
        let manager = temp.manager();
        let count = 5_000;
        populate(&manager, &temp, count);
        let cold = || {
            (0..3)
                .map(|_| {
                    forget(&manager);
                    let started = Instant::now();
                    drop(manager.lock().unwrap());
                    started.elapsed()
                })
                .min()
                .unwrap()
        };
        let snapshot = cold();
        // One record per entry: about as much as the journal holds before
        // it is compacted.
        let mut locked = manager.lock().unwrap();
        for index in 0..count {
            locked.entry_mut(index).pending_upload = true;
        }
        locked.append().unwrap();
        drop(locked);
        let journal = cold();
        assert!(
            journal < snapshot * 5 + Duration::from_millis(500),
            "{journal:?} to load {count} entries and {count} records, {snapshot:?} for the entries alone"
        );
    }
    #[test]
    fn a_torn_append_is_cut_off_and_an_interrupted_reservation_reclaimed() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(100, 10, "stop");
        completed(&manager, &temp.path("kept"), &policy, b"kept");
        let mut crashed = manager.writer(&temp.path("crashed"), &policy, 50).unwrap();
        crashed.write_all(b"partial").unwrap();
        // The process dies between reserve and finalize, part way through
        // its next append.
        crashed.finalized = true;
        crashed.file.take();
        crashed.lease.take();
        drop(crashed);
        let journal = manager.directory.join(JOURNAL);
        OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(br#"{"seq":99,"put":{"id":"#)
            .unwrap();
        forget(&manager);
        let mut locked = without_leases(&manager);
        let text = fs::read_to_string(&journal).unwrap();
        assert!(text.ends_with('\n') && !text.contains(r#""seq":99"#));
        assert_eq!(locked.allocated().unwrap(), 11);
        locked.compact().unwrap();
        drop(locked);
        forget(&manager);
        let locked = manager.lock().unwrap();
        assert_eq!(locked.allocated().unwrap(), 11);
        assert!(
            locked
                .ledger
                .entries
                .iter()
                .any(|entry| entry.path.ends_with("crashed") && entry.changed)
        );
        assert_eq!(fs::read_to_string(&journal).unwrap().lines().count(), 1);
    }
    #[test]
    fn records_already_in_the_snapshot_are_skipped_after_an_interrupted_compaction() {
        let temp = Temp::new();
        let manager = temp.manager();
        let policy = policy(100, 10, "stop");
        completed(&manager, &temp.path("first"), &policy, b"first");
        let journal = manager.directory.join(JOURNAL);
        let old = fs::read(&journal).unwrap();
        // Stop after the new snapshot, before the journal is replaced.
        manager.lock().unwrap().compact().unwrap();
        fs::write(&journal, old).unwrap();
        forget(&manager);
        assert_eq!(manager.lock().unwrap().ledger.entries.len(), 1);
        completed(&manager, &temp.path("second"), &policy, b"second");
        forget(&manager);
        let locked = manager.lock().unwrap();
        assert_eq!(locked.ledger.entries.len(), 2);
        assert_eq!(locked.allocated().unwrap(), 11);
    }
    #[test]
    fn altered_or_unreadable_journal_records_fail_closed() {
        let temp = Temp::new();
        let manager = temp.manager();
        completed(
            &manager,
            &temp.path("capture"),
            &policy(100, 10, "stop"),
            b"data",
        );
        let journal = manager.directory.join(JOURNAL);
        let original = fs::read_to_string(&journal).unwrap();
        let (head, last) = original.trim_end().rsplit_once('\n').unwrap();
        let mut record: Value = serde_json::from_str(last).unwrap();
        record["put"]["base"] = json!(temp.path("elsewhere"));
        // A second entry for a path that is already tracked.
        let mut twin: Value = serde_json::from_str(last).unwrap();
        twin["put"]["id"] = json!(nonce().unwrap());
        for altered in [
            format!("{head}\n{record}\n"),
            format!("{head}\n{twin}\n"),
            format!("{head}\nnot a record\n{last}\n"),
        ] {
            fs::write(&journal, altered).unwrap();
            forget(&manager);
            assert!(manager.lock().is_err());
        }
        assert_eq!(fs::read(temp.path("capture")).unwrap(), b"data");
    }
    #[test]
    fn a_version_1_ledger_is_converted_and_newer_versions_are_refused() {
        let temp = Temp::new();
        let manager = temp.manager();
        completed(
            &manager,
            &temp.path("capture"),
            &policy(100, 10, "stop"),
            b"data",
        );
        let entries = manager.lock().unwrap().ledger.entries.clone();
        let snapshot = manager.directory.join(SNAPSHOT);
        let journal = manager.directory.join(JOURNAL);
        fs::write(
            &snapshot,
            json!({"version": 1, "entries": entries}).to_string(),
        )
        .unwrap();
        fs::remove_file(&journal).unwrap();
        forget(&manager);
        assert_eq!(manager.lock().unwrap().allocated().unwrap(), 4);
        let converted: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
        assert_eq!(converted["version"], LEDGER_VERSION);
        assert!(journal.exists());
        fs::write(&snapshot, json!({"version": 3, "entries": []}).to_string()).unwrap();
        forget(&manager);
        let error = manager.lock().err().unwrap().to_string();
        assert!(error.contains("unsupported"), "{error}");
    }
    #[test]
    fn usage_and_policy_are_read_without_the_lock() {
        let temp = Temp::new();
        let manager = temp.manager();
        crate::ipc::ensure_private_dir(&manager.directory).unwrap();
        assert_eq!(
            read_usage(&manager.directory).unwrap(),
            Some(Usage::default())
        );
        let policy = policy(100, 10, "stop");
        completed(&manager, &temp.path("one"), &policy, b"1234");
        completed(&manager, &temp.path("two"), &policy, b"56");
        let held = manager.lock().unwrap();
        held.configure_policy(&policy).unwrap();
        // Either read would wait for the lock, and time out, if it took it.
        assert_eq!(
            read_usage(&manager.directory).unwrap(),
            Some(Usage {
                allocated_bytes: 6,
                files: 2
            })
        );
        assert_eq!(read_policy(&manager.directory).unwrap().max_files, 10);
        drop(held);
        fs::remove_file(manager.directory.join(SUMMARY)).unwrap();
        assert_eq!(read_usage(&manager.directory).unwrap(), None);
    }
    #[test]
    fn retention_removes_a_full_budget_of_old_files_with_one_commit() {
        let temp = Temp::new();
        let manager = temp.manager();
        populate(&manager, &temp, 2_000);
        let mut locked = manager.lock().unwrap();
        let base = fs::canonicalize(&temp.0).unwrap();
        // A lowered file limit leaves 1,901 files to remove for this save.
        locked
            .make_room(
                &policy(1 << 30, 100, "delete-oldest"),
                1,
                &base,
                device(&base),
            )
            .unwrap();
        assert_eq!(locked.ledger.entries.len(), 99);
        assert_eq!(locked.commits, 1);
        assert!(!temp.path("filler-1900").exists() && temp.path("filler-1901").exists());
        // An age limit removes at most a batch per save while there is room.
        for index in 0..locked.ledger.entries.len() {
            locked.entry_mut(index).created = now() - 100;
        }
        locked.persist().unwrap();
        let mut policy = policy(1 << 30, 100_000, "delete-oldest");
        policy.max_age_seconds = Some(10);
        locked.commits = 0;
        locked.make_room(&policy, 1, &base, device(&base)).unwrap();
        assert_eq!(locked.ledger.entries.len(), 99 - EXPIRED_BATCH);
        assert_eq!(locked.commits, 1);
    }
    #[test]
    fn finishing_a_recording_reads_it_back_without_holding_the_lock() {
        let temp = Temp::new();
        let manager = temp.manager();
        let size = 128 << 20;
        let mut writer = manager
            .writer(&temp.path("recording"), &policy(1 << 30, 10, "stop"), size)
            .unwrap();
        writer.write_all(&vec![7; size as usize]).unwrap();
        let started = Instant::now();
        let hash = hash_file(writer.path()).unwrap();
        let hashing = started.elapsed();
        // Another process holds the lock for longer than the read takes.
        let held = manager.lock().unwrap();
        let finishing = thread::spawn(move || writer.finish().map(|()| Instant::now()));
        thread::sleep((hashing * 2 + Duration::from_millis(300)).min(Duration::from_secs(4)));
        let released = Instant::now();
        drop(held);
        let finished = finishing.join().unwrap().unwrap();
        assert!(
            finished - released < hashing / 2,
            "finishing took {:?} after the lock was released; reading back takes {hashing:?}",
            finished - released
        );
        assert_eq!(manager.lock().unwrap().ledger.entries[0].sha256, Some(hash));
    }
    /// A disk with room for the floor and 100 bytes more, holding the files
    /// saved in `base`.
    fn small_disk(base: &Path) -> u64 {
        let used: u64 = fs::read_dir(base)
            .unwrap()
            .flatten()
            .filter_map(|file| file.metadata().ok())
            .filter(Metadata::is_file)
            .map(|meta| meta.len())
            .sum();
        (FREE_SPACE_FLOOR + 100).saturating_sub(used)
    }
    #[test]
    fn saves_leave_the_free_space_floor_on_their_disk() {
        let temp = Temp::new();
        let manager = temp.manager();
        AVAILABLE.set(Some(small_disk));
        let stop = policy(1 << 30, 100, "stop");
        completed(&manager, &temp.path("first"), &stop, &[1; 60]);
        let error = manager
            .writer(&temp.path("second"), &stop, 60)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("needs at least 512 MiB free on its disk"),
            "{error:#}"
        );
        assert_eq!(crate::cli::error_code(&error), ("storage_full", 6));
        assert!(!temp.path("second").exists());
        // Delete-oldest frees the oldest files on that disk instead.
        let oldest = policy(1 << 30, 100, "delete-oldest");
        completed(&manager, &temp.path("second"), &oldest, &[2; 60]);
        assert!(!temp.path("first").exists());
        // What live reservations may still write is already spoken for.
        let live = manager.writer(&temp.path("live"), &stop, 30).unwrap();
        assert!(manager.writer(&temp.path("third"), &stop, 20).is_err());
        drop(live);
        // Nothing is removed when owned files cannot free enough, and files
        // Capturefab does not own never are.
        fs::write(temp.path("user"), [3; 100]).unwrap();
        let error = manager
            .writer(&temp.path("fourth"), &oldest, 10)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("needs at least 512 MiB free"),
            "{error:#}"
        );
        assert!(temp.path("second").exists() && temp.path("user").exists());
        AVAILABLE.set(None);
    }
    /// A disk whose free space stays just under what a save needs, however
    /// many files are removed, as while a snapshot holds their blocks.
    fn held_disk(_: &Path) -> u64 {
        FREE_SPACE_FLOOR + 10
    }
    fn roomy_disk(_: &Path) -> u64 {
        1 << 40
    }
    #[test]
    fn removals_that_free_no_space_stop_until_a_save_fits_again() {
        let temp = Temp::new();
        let manager = temp.manager();
        let oldest = policy(1 << 30, 100, "delete-oldest");
        AVAILABLE.set(Some(roomy_disk));
        for index in 0..5 {
            completed(
                &manager,
                &temp.path(&format!("kept-{index}")),
                &oldest,
                &[1; 60],
            );
        }
        AVAILABLE.set(Some(held_disk));
        for attempt in 0..5 {
            // Each save from a new process, as in a scripted loop.
            forget(&manager);
            let error = manager
                .writer(&temp.path(&format!("new-{attempt}")), &oldest, 60)
                .err()
                .unwrap();
            assert!(
                format!("{error:#}").contains("did not free space"),
                "{error:#}"
            );
            assert_eq!(crate::cli::error_code(&error), ("storage_full", 6));
        }
        let kept = (0..5)
            .filter(|index| temp.path(&format!("kept-{index}")).exists())
            .count();
        assert_eq!(kept, 4, "one round of removals, then none");
        // A save that fits lets later ones remove files for space again.
        AVAILABLE.set(Some(roomy_disk));
        completed(&manager, &temp.path("fits"), &oldest, &[2; 60]);
        AVAILABLE.set(Some(held_disk));
        assert!(manager.writer(&temp.path("next"), &oldest, 60).is_err());
        assert!(!temp.path("kept-1").exists() && temp.path("kept-2").exists());
        AVAILABLE.set(None);
    }
    #[test]
    fn files_are_not_removed_for_space_where_disks_cannot_be_told_apart() {
        let temp = Temp::new();
        let manager = temp.manager();
        let oldest = policy(1 << 30, 100, "delete-oldest");
        completed(&manager, &temp.path("kept"), &oldest, &[1; 60]);
        let base = fs::canonicalize(&temp.0).unwrap();
        AVAILABLE.set(Some(small_disk));
        let mut locked = manager.lock().unwrap();
        // As on Windows, where the oldest file could be on another drive.
        let error = locked.make_room(&oldest, 60, &base, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("needs at least 512 MiB free"),
            "{error:#}"
        );
        assert!(temp.path("kept").exists());
        locked.make_room(&oldest, 60, &base, device(&base)).unwrap();
        assert!(!temp.path("kept").exists());
        AVAILABLE.set(None);
    }
}
