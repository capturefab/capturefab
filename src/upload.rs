//! A persistent, cross-process queue of finished captures waiting for upload to
//! network destinations (S3-compatible buckets, FTP(S) and SFTP servers).
//!
//! Workers add files as they finish; the storage ledger holds each one back
//! from retention until it is uploaded. One process at a time uploads (the GUI,
//! `serve`, or a CLI capture that waits for its uploads): whichever holds
//! `uploader.lock`. Network and service failures are retried with backoff;
//! refused credentials, a missing bucket or an existing object wait for a
//! manual retry. After a verified upload the local copy is deleted (only if
//! unchanged) unless the destination keeps it.
//!
//! S3 items keep their original `bucket` field, so queues holding only S3
//! uploads stay readable by older Capturefab versions; FTP and SFTP items use
//! `remote`. Completed uploads are logged in their own file,
//! `uploads-completed.json`, for status displays and the dataset catalog.
//!
//! Changes are appended to `uploads.journal` and folded into both files once
//! it outgrows them, so queueing or finishing an upload costs a short append
//! however long the backlog. Older versions read only the files, which can
//! lag the journal by that much.
use crate::destination::{Bucket, Remote};
use crate::transfer::{Permanent, Uploader};
use anyhow::{Context, Result, anyhow, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, OnceLock, PoisonError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_ITEMS: usize = 100_000;
const MAX_BACKOFF: u64 = 15 * 60;
/// Completed uploads kept in `uploads-completed.json`.
const MAX_COMPLETED: usize = 1000;
/// Connections idle this long are closed; at most this many are kept.
const CLIENT_IDLE: Duration = Duration::from_secs(60);
const MAX_CLIENTS: usize = 16;
/// How often an idle uploader looks for files other processes queued.
const POLL: Duration = Duration::from_secs(2);
/// How soon the uploader tries again to hold files queued unheld.
const HOLD_RETRY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Uploading,
    /// Needs attention; retried only on request.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub destination: String,
    /// The bucket of an S3 upload (the original queue format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<Bucket>,
    /// The server of an FTP or SFTP upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    pub path: PathBuf,
    pub key: String,
    pub bytes: u64,
    pub sha256: String,
    pub queued: u64,
    pub attempts: u32,
    pub next_attempt: u64,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Queued while the storage ledger was too busy to hold the file back
    /// from retention; the uploader holds it as soon as it can, or stops
    /// trying once the ledger no longer has it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unheld: bool,
}
impl Item {
    /// Where this item uploads to.
    pub fn remote(&self) -> Remote {
        match (&self.bucket, &self.remote) {
            (Some(bucket), _) => Remote::S3(bucket.clone()),
            (None, Some(remote)) => remote.clone(),
            (None, None) => unreachable!("validated upload names a server"),
        }
    }
    fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        crate::destination::validate_name(&self.destination)?;
        ensure!(
            matches!(
                (&self.bucket, &self.remote),
                (Some(_), None) | (None, Some(Remote::Ftp(_) | Remote::Sftp(_)))
            ),
            "queued upload must name exactly one destination server"
        );
        self.remote().validate()?;
        crate::destination::validate_object_key(&self.key)?;
        ensure!(
            self.path.is_absolute()
                && !self
                    .path
                    .as_os_str()
                    .as_encoded_bytes()
                    .iter()
                    .any(|b| b.is_ascii_control()),
            "queued upload path must be absolute without control characters"
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "queued upload checksum must be a lowercase SHA-256 hash (64 hexadecimal digits)"
        );
        Ok(())
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 16
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "upload ID must contain 16 lowercase hexadecimal digits; choose an ID from upload status"
    );
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Queue {
    version: u32,
    items: Vec<Item>,
    #[serde(flatten)]
    totals: Totals,
}

/// The queue's running totals and destination backoffs; a journal change
/// that alters them carries them whole.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Totals {
    #[serde(default)]
    uploaded: u64,
    #[serde(default)]
    uploaded_bytes: u64,
    /// Times an upload has failed for good; retries don't lower it.
    #[serde(default)]
    failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    /// Destinations whose last upload failed in a way worth retrying, by
    /// name: none of their uploads tried before starts again until the
    /// backoff ends, so an unreachable server is tried once per wait (and
    /// once per new file) rather than once per file per wait.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    backoff: BTreeMap<String, Backoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Backoff {
    /// Failures in a row.
    failures: u32,
    /// Unix seconds.
    until: u64,
}

/// Seconds to wait after `failures` failed attempts in a row.
fn backoff(failures: u32) -> u64 {
    (5u64 << failures.min(8)).min(MAX_BACKOFF)
}

impl Queue {
    /// When `item` may start, in Unix seconds: after its own retry time and,
    /// once it has been tried, its destination's backoff. A new or retried
    /// upload goes ahead, so a direct capture or a manual retry tries its
    /// files, and the first to get through ends the backoff for the rest.
    fn due_at(&self, item: &Item) -> u64 {
        let backoff = (item.attempts > 0)
            .then(|| self.totals.backoff.get(&item.destination))
            .flatten();
        item.next_attempt.max(backoff.map_or(0, |b| b.until))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn dir() -> Result<PathBuf> {
    let dir = crate::ipc::session_dir();
    crate::ipc::ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Hold `uploads.lock` while `f` runs; it guards the queue, its journal and
/// the completed log. Readers share it.
fn with_lock<T>(shared: bool, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let dir = dir()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("uploads.lock"))?;
    if shared {
        FileExt::lock_shared(&lock)
    } else {
        FileExt::lock_exclusive(&lock)
    }
    .context("lock the upload queue")?;
    f(&dir)
}

/// Bytes written to the queue files, so tests can bound them.
#[cfg(test)]
static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Write `bytes` to `dir/name` atomically, readable only by this user.
fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    #[cfg(test)]
    WRITTEN.fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let mut file = private_options().truncate(true).open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, dir.join(name))?;
    Ok(())
}

/// Tells versions of a file apart: a replaced file has another inode (on
/// Windows, creation time) and modification time; an appended one, length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    len: u64,
}
impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(meta),
            #[cfg(not(unix))]
            created: meta.created().ok(),
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
    fn read(path: &Path) -> Result<Option<Self>> {
        match fs::metadata(path) {
            Ok(meta) => Ok(Some(Self::of(&meta))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("inspect {}", path.display())),
        }
    }
    fn same_file(&self, other: &Self) -> bool {
        #[cfg(unix)]
        return self.inode == other.inode;
        #[cfg(not(unix))]
        return self.created == other.created;
    }
}

const QUEUE_FILE: &str = "uploads.json";
const COMPLETED_FILE: &str = "uploads-completed.json";
const JOURNAL_FILE: &str = "uploads.journal";
/// The journal is folded into the queue and completed log once it outgrows
/// them, or this, so a change costs a short append however long the queue,
/// and readers replay little.
const JOURNAL_MIN: u64 = 64 * 1024;
const MAX_QUEUE_BYTES: u64 = 256 * 1024 * 1024;

/// One change to the uploads: a line of the journal. It holds new values,
/// never increments, so replaying it twice is harmless.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Change {
    /// Items added or updated, matched by ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    put: Vec<Item>,
    /// IDs of items that left the queue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    totals: Option<Totals>,
    /// A verified upload for the completed log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed: Option<Completed>,
    /// Harmless to lose in a crash, so not flushed: the uploading mark,
    /// which the next uploader clears anyway.
    #[serde(skip)]
    volatile: bool,
}
impl Change {
    fn put(put: Vec<Item>) -> Self {
        Self {
            put,
            ..Self::default()
        }
    }
    fn is_empty(&self) -> bool {
        self.put.is_empty()
            && self.remove.is_empty()
            && self.totals.is_none()
            && self.completed.is_none()
    }
}

/// The queue and completed log as the files hold them: `uploads.json` and
/// `uploads-completed.json` with the journal replayed. Kept between calls,
/// so a process reads only what others appended since.
struct Uploads {
    dir: PathBuf,
    queue: Queue,
    log: CompletedLog,
    /// The queue file and journal as last read or written.
    seen: (Option<Stamp>, Option<Stamp>),
    /// Journal bytes replayed, up to its last complete line.
    replayed: u64,
    /// Bytes in the queue file and completed log.
    folded: u64,
}

static UPLOADS: Mutex<Option<Uploads>> = Mutex::new(None);

impl Uploads {
    /// Read the queue and completed log files, before any journal.
    fn read(dir: &Path, stamp: Option<Stamp>) -> Result<Self> {
        let path = dir.join(QUEUE_FILE);
        let (queue, bytes) = match fs::read(&path) {
            Ok(bytes) => {
                ensure!(
                    bytes.len() as u64 <= MAX_QUEUE_BYTES,
                    "upload queue is too large (maximum 256 MiB)"
                );
                let queue: Queue = serde_json::from_slice(&bytes)
                    .with_context(|| format!("invalid upload queue {}", path.display()))?;
                (queue, bytes.len() as u64)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                Queue {
                    version: 1,
                    ..Queue::default()
                },
                0,
            ),
            Err(e) => return Err(e).context("read the upload queue"),
        };
        ensure!(queue.version == 1, "unsupported upload queue version");
        ensure!(
            queue.items.len() <= MAX_ITEMS,
            "upload queue contains too many items (maximum {MAX_ITEMS})"
        );
        let mut ids = std::collections::HashSet::new();
        for item in &queue.items {
            item.validate()
                .with_context(|| format!("invalid queued upload {}", item.id))?;
            ensure!(ids.insert(&item.id), "duplicate upload ID {}", item.id);
        }
        // A damaged log is started again rather than blocking uploads.
        let (log, logged) = read_completed(dir).unwrap_or_default();
        Ok(Self {
            dir: dir.into(),
            queue,
            log,
            seen: (stamp, None),
            replayed: 0,
            folded: bytes + logged,
        })
    }

    /// When the next waiting upload is due, in Unix seconds.
    fn next_due_at(&self) -> Option<u64> {
        self.queue
            .items
            .iter()
            .filter(|i| i.state == State::Pending)
            .map(|i| self.queue.due_at(i))
            .min()
    }

    /// Whether `journal` is the one replayed so far, perhaps longer.
    fn continues(&self, journal: Option<Stamp>) -> bool {
        match (self.seen.1, journal) {
            (Some(seen), Some(journal)) => seen.same_file(&journal) && journal.len >= self.replayed,
            _ => self.replayed == 0,
        }
    }

    /// Apply the journal's complete lines after those already replayed.
    fn replay(&mut self, dir: &Path, journal: Option<Stamp>) -> Result<()> {
        if let Some(stamp) = journal {
            ensure!(
                stamp.len <= MAX_QUEUE_BYTES,
                "upload queue journal is too large (maximum 256 MiB)"
            );
            let path = dir.join(JOURNAL_FILE);
            let mut file = fs::File::open(&path).context("read the upload queue journal")?;
            file.seek(SeekFrom::Start(self.replayed))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            // A writer that stopped mid-line left the rest; the next one
            // writes over it.
            let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            let mut changes = Vec::new();
            for line in bytes[..complete]
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
            {
                let change: Change = serde_json::from_slice(line)
                    .with_context(|| format!("invalid upload queue journal {}", path.display()))?;
                for item in &change.put {
                    item.validate()
                        .with_context(|| format!("invalid queued upload {}", item.id))?;
                }
                changes.push(change);
            }
            self.apply(changes);
            ensure!(
                self.queue.items.len() <= MAX_ITEMS,
                "upload queue contains too many items (maximum {MAX_ITEMS})"
            );
            self.replayed += complete as u64;
        }
        self.seen.1 = journal;
        Ok(())
    }

    /// Apply changes in order, in one pass over the queue.
    fn apply(&mut self, changes: impl IntoIterator<Item = Change>) {
        let mut latest: HashMap<String, Option<Item>> = HashMap::new();
        let mut order = Vec::new();
        for change in changes {
            for item in change.put {
                if !latest.contains_key(&item.id) {
                    order.push(item.id.clone());
                }
                latest.insert(item.id.clone(), Some(item));
            }
            for id in change.remove {
                latest.insert(id, None);
            }
            if let Some(totals) = change.totals {
                self.queue.totals = totals;
            }
            if let Some(entry) = change.completed {
                self.log.add(entry);
            }
        }
        if latest.is_empty() {
            return;
        }
        self.queue
            .items
            .retain_mut(|item| match latest.remove(&item.id) {
                Some(Some(new)) => {
                    *item = new;
                    true
                }
                Some(None) => false,
                None => true,
            });
        // What is left was not queued before.
        self.queue.items.extend(
            order
                .into_iter()
                .filter_map(|id| latest.remove(&id).flatten()),
        );
    }

    /// Append `change` to the journal, durably, and apply it.
    fn append(&mut self, dir: &Path, change: Change) -> Result<()> {
        let mut line = serde_json::to_vec(&change)?;
        line.push(b'\n');
        #[cfg(test)]
        WRITTEN.fetch_add(line.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let mut file = private_options()
            .read(true)
            .open(dir.join(JOURNAL_FILE))
            .context("open the upload queue journal")?;
        // Drop a line left unfinished by a writer that stopped.
        file.set_len(self.replayed)?;
        file.seek(SeekFrom::Start(self.replayed))?;
        let written = file.write_all(&line).and_then(|()| {
            if change.volatile {
                Ok(())
            } else {
                file.sync_data()
            }
        });
        if let Err(error) = written {
            let _ = file.set_len(self.replayed);
            return Err(error).context("write the upload queue journal");
        }
        self.seen.1 = Some(Stamp::of(&file.metadata()?));
        self.replayed += line.len() as u64;
        self.apply([change]);
        Ok(())
    }

    /// Fold the journal into the queue file and completed log.
    fn compact(&mut self, dir: &Path) -> Result<()> {
        let log = serde_json::to_vec(&self.log)?;
        let queue = serde_json::to_vec(&self.queue)?;
        write_private(dir, COMPLETED_FILE, &log)?;
        write_private(dir, QUEUE_FILE, &queue)?;
        // Both must be in place before the journal goes.
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?;
        fs::remove_file(dir.join(JOURNAL_FILE))?;
        self.seen = (Stamp::read(&dir.join(QUEUE_FILE))?, None);
        self.replayed = 0;
        self.folded = (log.len() + queue.len()) as u64;
        Ok(())
    }
}

/// Bring the cached uploads up to date: read everything when the queue file
/// was replaced, and otherwise only the journal's new lines. The caller
/// holds `uploads.lock`.
fn refresh<'a>(dir: &Path, cached: &'a mut Option<Uploads>) -> Result<&'a mut Uploads> {
    let queue = Stamp::read(&dir.join(QUEUE_FILE))?;
    let journal = Stamp::read(&dir.join(JOURNAL_FILE))?;
    let mut uploads = match cached.take() {
        Some(uploads)
            if uploads.dir == dir && uploads.seen.0 == queue && uploads.continues(journal) =>
        {
            uploads
        }
        _ => Uploads::read(dir, queue)?,
    };
    if uploads.seen.1 != journal {
        uploads.replay(dir, journal)?;
    }
    Ok(cached.insert(uploads))
}

/// Read the uploads under a shared lock.
fn read<T>(f: impl FnOnce(&Uploads) -> Result<T>) -> Result<T> {
    with_lock(true, |dir| {
        let mut cached = UPLOADS.lock().unwrap_or_else(PoisonError::into_inner);
        f(refresh(dir, &mut cached)?)
    })
}

/// Change the uploads under an exclusive lock: `update` reads them and says
/// what changes, which is journaled and applied. Nothing is written when
/// nothing changes.
fn with_queue<T>(update: impl FnOnce(&Uploads) -> Result<(T, Change)>) -> Result<T> {
    with_lock(false, |dir| {
        let mut cached = UPLOADS.lock().unwrap_or_else(PoisonError::into_inner);
        let uploads = refresh(dir, &mut cached)?;
        let (result, change) = update(uploads)?;
        if change.is_empty() {
            return Ok(result);
        }
        if let Err(error) = uploads.append(dir, change) {
            *cached = None;
            return Err(error);
        }
        if uploads.replayed > uploads.folded.max(JOURNAL_MIN)
            && let Err(error) = uploads.compact(dir)
        {
            // The change is safe in the journal; read everything next time.
            *cached = None;
            eprintln!("capturefab: cannot fold the upload journal into the queue: {error:#}");
        }
        Ok(result)
    })
}

/// A verified upload, as recorded in the completed log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Completed {
    /// Increases by one per completed upload, across processes.
    pub seq: u64,
    pub id: String,
    pub destination: String,
    /// `s3`, `ftp` or `sftp`.
    pub kind: String,
    pub path: PathBuf,
    pub key: String,
    /// The object's URL, without user name or secret.
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
    pub uploaded_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompletedLog {
    version: u32,
    seq: u64,
    entries: VecDeque<Completed>,
}
impl Default for CompletedLog {
    fn default() -> Self {
        Self {
            version: 1,
            seq: 0,
            entries: VecDeque::new(),
        }
    }
}
impl CompletedLog {
    /// Add a newer entry, keeping the newest `MAX_COMPLETED`.
    fn add(&mut self, entry: Completed) {
        if entry.seq <= self.seq {
            return;
        }
        self.seq = entry.seq;
        self.entries.push_back(entry);
        while self.entries.len() > MAX_COMPLETED {
            self.entries.pop_front();
        }
    }
}

/// The completed log file and its size, before any journal.
fn read_completed(dir: &Path) -> Result<(CompletedLog, u64)> {
    let path = dir.join(COMPLETED_FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            ensure!(
                bytes.len() <= 16 * 1024 * 1024,
                "completed upload log is too large (maximum 16 MiB)"
            );
            let log: CompletedLog = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid completed upload log {}", path.display()))?;
            ensure!(log.version == 1, "unsupported completed upload log version");
            Ok((log, bytes.len() as u64))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((CompletedLog::default(), 0)),
        Err(e) => Err(e).context("read the completed upload log"),
    }
}

/// Uploads completed after `seq`, oldest first, and the latest sequence
/// number.
pub fn completed_since(seq: u64) -> Result<(u64, Vec<Completed>)> {
    read(|uploads| {
        Ok((
            uploads.log.seq,
            uploads
                .log
                .entries
                .iter()
                .filter(|e| e.seq > seq)
                .cloned()
                .collect(),
        ))
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Wakes this process's uploader when it queues a file itself.
static WAKE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

fn id() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("cannot generate upload ID: {error}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Queue a finished, ledger-recorded file for upload as `key` and return its
/// upload ID. Queueing the same file for the same upload again returns the
/// existing ID.
pub fn enqueue(destination: &str, remote: &Remote, path: &Path, key: &str) -> Result<String> {
    crate::destination::validate_name(destination)?;
    remote.validate()?;
    crate::destination::validate_object_key(key)?;
    let path = fs::canonicalize(path)?;
    let bytes = fs::metadata(&path)?.len();
    // The upload already queued for this file, if any.
    let queued = |uploads: &Uploads| {
        let existing = uploads.queue.items.iter().find(|item| item.path == path)?;
        Some(
            if existing.destination == destination
                && existing.remote() == *remote
                && existing.key == key
            {
                Ok(existing.id.clone())
            } else {
                Err(anyhow!(
                    "this file is already queued for a different upload; retry or forget the existing upload first"
                ))
            },
        )
    };
    if let Some(id) = read(|uploads| Ok(queued(uploads)))? {
        return id;
    }
    // Holding waits while the storage ledger is busy, and the fallback reads
    // the whole file, so neither keeps the queue locked.
    let (sha256, unheld) = match hold(&path) {
        Ok(sha256) => (sha256, false),
        // Queue the file anyway and let the uploader hold it, rather than
        // leave a capture that retention may remove before it ever uploads.
        Err(error) if busy(&error) => (file_sha256(&path, bytes).map_err(|_| error)?, true),
        Err(error) => return Err(error),
    };
    let mut release = !unheld;
    let result = with_queue(|uploads| {
        if let Some(id) = queued(uploads) {
            // Queued meanwhile; that upload needs the file held.
            release = false;
            return Ok((id?, Change::default()));
        }
        let queue = &uploads.queue;
        ensure!(
            queue.items.len() < MAX_ITEMS,
            "the upload queue is full ({MAX_ITEMS} files); check the destination and retry failed uploads"
        );
        let id = id()?;
        ensure!(
            !queue.items.iter().any(|item| item.id == id),
            "upload ID collision; retry queueing this file"
        );
        let (bucket, other) = match remote {
            Remote::S3(bucket) => (Some(bucket.clone()), None),
            other => (None, Some(other.clone())),
        };
        let item = Item {
            id: id.clone(),
            destination: destination.into(),
            bucket,
            remote: other,
            path: path.clone(),
            key: key.into(),
            bytes,
            sha256,
            queued: now(),
            attempts: 0,
            next_attempt: 0,
            state: State::Pending,
            error: None,
            unheld,
        };
        Ok((id, Change::put(vec![item])))
    });
    match result {
        Ok(id) => {
            WAKE.1.notify_all();
            Ok(id)
        }
        Err(error) => {
            if release {
                crate::storage::hold_for_upload(&path, false)
                    .context("release capture after it could not be queued for upload")?;
            }
            Err(error)
        }
    }
}

/// Hold a queued file back from retention; returns the SHA-256 the storage
/// ledger recorded for it.
fn hold(path: &Path) -> Result<String> {
    #[cfg(test)]
    ensure!(
        !tests::HOLD_FAILS.load(std::sync::atomic::Ordering::Relaxed),
        "storage manager is busy"
    );
    crate::storage::hold_for_upload(path, true)
}

/// Whether a hold failed only because the storage ledger was busy, so a
/// later try may succeed.
fn busy(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("storage manager is busy")
}

/// The SHA-256 of a file that should hold `bytes` bytes.
fn file_sha256(path: &Path, bytes: u64) -> Result<String> {
    let mut reader = crate::transfer::HashingReader::new(fs::File::open(path)?);
    std::io::copy(&mut reader, &mut std::io::sink())?;
    let (sha256, read) = reader.finish();
    ensure!(
        read == bytes,
        "{} changed while it was queued",
        path.display()
    );
    Ok(sha256)
}

/// Hold files that were queued unheld. A hold waits while the storage
/// ledger is busy, so none runs under the queue lock, and the first that
/// finds it busy, or a second's work, ends this try; the next comes soon.
fn hold_unheld() -> Result<()> {
    let unheld: Vec<(String, PathBuf)> = read(|uploads| {
        Ok(uploads
            .queue
            .items
            .iter()
            .filter(|i| i.unheld)
            .map(|i| (i.id.clone(), i.path.clone()))
            .collect())
    })?;
    let started = Instant::now();
    let mut held = Vec::new();
    for (id, path) in unheld {
        match hold(&path) {
            Ok(sha256) => held.push((id, path, Some(sha256))),
            Err(error) if busy(&error) => break,
            // Gone, or no longer in the ledger, so retention cannot remove
            // it either: nothing is left to hold.
            Err(_) => held.push((id, path, None)),
        }
        if started.elapsed() > Duration::from_secs(1) {
            break;
        }
    }
    if held.is_empty() {
        return Ok(());
    }
    let orphans = with_queue(|uploads| {
        let (mut put, mut orphans) = (Vec::new(), Vec::new());
        let items = &uploads.queue.items;
        for (id, path, sha256) in held {
            match items.iter().find(|i| i.id == id) {
                // The ledger's checksum is the one to upload against.
                Some(item) if item.unheld => put.push(Item {
                    sha256: sha256.unwrap_or_else(|| item.sha256.clone()),
                    unheld: false,
                    ..item.clone()
                }),
                Some(_) => {}
                None if sha256.is_some() && !items.iter().any(|i| i.path == path) => {
                    orphans.push(path)
                }
                None => {}
            }
        }
        Ok((orphans, Change::put(put)))
    })?;
    // Their uploads were forgotten meanwhile, so release them again.
    for path in orphans {
        let _ = crate::storage::hold_for_upload(&path, false);
    }
    Ok(())
}

/// Queue summary and the most recent items, for status displays.
pub fn status() -> Result<Value> {
    let active = uploader_active();
    read(|uploads| {
        let mut value = queue_status(&uploads.queue)?;
        value["uploader_running"] = json!(active);
        value["completed_seq"] = json!(uploads.log.seq);
        value["recent_completed"] = json!(
            uploads
                .log
                .entries
                .iter()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
        );
        Ok(value)
    })
}

fn queue_status(queue: &Queue) -> Result<Value> {
    let count = |state| queue.items.iter().filter(|i| i.state == state).count();
    let waiting_bytes: u64 = queue
        .items
        .iter()
        .filter(|i| i.state != State::Failed)
        .map(|i| i.bytes)
        .try_fold(0u64, |total, bytes| {
            total
                .checked_add(bytes)
                .context("queued upload byte count exceeds u64")
        })?;
    let recent: Vec<_> = queue.items.iter().rev().take(200).collect();
    let totals = &queue.totals;
    Ok(json!({
        "pending": count(State::Pending),
        "uploading": count(State::Uploading),
        "failed": count(State::Failed),
        "waiting_bytes": waiting_bytes,
        "uploaded": totals.uploaded,
        "uploaded_bytes": totals.uploaded_bytes,
        "failures": totals.failures,
        "last_error": totals.last_error,
        "destinations": destinations(queue),
        "items": recent,
    }))
}

/// One destination's uploads in the queue, all of them counted.
#[derive(Default, Serialize)]
struct Backlog {
    pending: usize,
    uploading: usize,
    failed: usize,
    waiting_bytes: u64,
    /// When its next waiting upload may start, in Unix seconds: at or
    /// before now while one is due, later while it waits to retry.
    next_attempt: Option<u64>,
    /// The newest error of a failed upload, or else of a waiting one.
    last_error: Option<String>,
}

/// Each destination with queued uploads, by name. The caller has checked
/// that the queue's bytes add up.
fn destinations(queue: &Queue) -> BTreeMap<&str, Backlog> {
    let mut destinations = BTreeMap::<&str, Backlog>::new();
    for item in &queue.items {
        let backlog = destinations.entry(&item.destination).or_default();
        match item.state {
            State::Pending => {
                backlog.pending += 1;
                let due = queue.due_at(item);
                backlog.next_attempt = Some(backlog.next_attempt.map_or(due, |next| next.min(due)));
            }
            State::Uploading => backlog.uploading += 1,
            State::Failed => backlog.failed += 1,
        }
        if item.state != State::Failed {
            backlog.waiting_bytes += item.bytes;
        }
        if let Some(error) = &item.error
            && (item.state == State::Failed || backlog.failed == 0)
        {
            backlog.last_error = Some(error.clone());
        }
    }
    destinations
}

/// Move failed uploads (all, or one by ID) back to pending. With `waiting`,
/// uploads waiting to retry start now too, and their destinations' backoff
/// ends. Returns how many uploads this makes due.
pub fn retry(id: Option<&str>, waiting: bool) -> Result<usize> {
    if let Some(id) = id {
        validate_id(id)?;
    }
    let changed = with_queue(|uploads| {
        let (queue, now) = (&uploads.queue, now());
        let chosen: Vec<&Item> = queue
            .items
            .iter()
            .filter(|item| {
                id.is_none_or(|id| id == item.id)
                    && (item.state == State::Failed
                        || (waiting && item.state == State::Pending && queue.due_at(item) > now))
            })
            .collect();
        ensure!(
            id.is_none() || !chosen.is_empty(),
            "no {} upload with the requested ID; choose an ID from upload status",
            if waiting {
                "failed or waiting"
            } else {
                "failed"
            }
        );
        let mut totals = queue.totals.clone();
        for item in chosen.iter().filter(|_| waiting) {
            totals.backoff.remove(&item.destination);
        }
        let put = chosen
            .iter()
            .filter(|item| item.state == State::Failed || item.next_attempt > now)
            .map(|item| Item {
                state: State::Pending,
                attempts: 0,
                next_attempt: 0,
                ..(*item).clone()
            })
            .collect();
        let change = Change {
            put,
            totals: (totals.backoff.len() < queue.totals.backoff.len()).then_some(totals),
            ..Change::default()
        };
        Ok((chosen.len(), change))
    })?;
    WAKE.1.notify_all();
    Ok(changed)
}

/// Drop a failed upload from the queue; its local file is kept and released
/// to normal retention.
pub fn forget(id: &str) -> Result<Item> {
    validate_id(id)?;
    let item = with_queue(|uploads| {
        let item = uploads
            .queue
            .items
            .iter()
            .find(|i| i.id == id && i.state == State::Failed)
            .ok_or_else(|| anyhow!("no failed upload {id}"))?
            .clone();
        let change = Change {
            remove: vec![item.id.clone()],
            ..Change::default()
        };
        Ok((item, change))
    })?;
    let _ = crate::storage::hold_for_upload(&item.path, false);
    Ok(item)
}

/// Whether some process currently holds the uploader lock.
fn uploader_active() -> bool {
    let Ok(dir) = dir() else { return false };
    let Ok(file) = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("uploader.lock"))
    else {
        return false;
    };
    if OWNER
        .get()
        .is_some_and(|owner| *owner.lock().unwrap_or_else(|e| e.into_inner()))
    {
        return true;
    }
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Whether this process's uploader holds the lock.
static OWNER: OnceLock<Mutex<bool>> = OnceLock::new();

/// Start this process's background uploader once. It uploads whenever it
/// holds the cross-process uploader lock, and otherwise waits for it.
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        OWNER.get_or_init(|| Mutex::new(false));
        let _ = thread::Builder::new()
            .name("capturefab-uploader".into())
            .spawn(run);
    });
}

fn run() {
    // Held for the life of the process: this process is the uploader.
    let _lock = loop {
        if let Ok(file) = dir().and_then(|dir| {
            Ok(fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("uploader.lock"))?)
        }) && file.try_lock_exclusive().is_ok()
        {
            break file;
        }
        thread::sleep(Duration::from_secs(5));
    };
    *OWNER
        .get()
        .expect("owner")
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = true;
    // A previous uploader may have stopped mid-upload.
    let _ = with_queue(|uploads| {
        let put = uploads
            .queue
            .items
            .iter()
            .filter(|item| item.state == State::Uploading)
            .map(|item| Item {
                state: State::Pending,
                ..item.clone()
            })
            .collect();
        Ok(((), Change::put(put)))
    });
    let mut clients: HashMap<String, (Box<dyn Uploader>, Instant)> = HashMap::new();
    let mut hold_at = Instant::now();
    loop {
        clients.retain(|_, (_, used)| used.elapsed() < CLIENT_IDLE);
        if Instant::now() >= hold_at {
            let _ = hold_unheld();
            hold_at = Instant::now() + HOLD_RETRY;
        }
        let item = match with_queue(next_due) {
            Ok(Some(item)) => item,
            Ok(None) | Err(_) => {
                idle(hold_at);
                continue;
            }
        };
        let result = upload_one(&mut clients, &item);
        let _ = with_queue(|uploads| Ok(((), update_after(uploads, &item, &result))));
        if result.is_ok() {
            finish_local(&item);
        } else {
            // A client with stale credentials or a dropped connection is
            // rebuilt on the next attempt.
            clients.remove(&client_key(&item));
        }
    }
}

/// Wait while nothing is due: until the next upload is, `hold_at` passes
/// with files queued unheld, this process queues or retries one, or the
/// queue files change. Another process's change is noticed with a look at
/// the files every `POLL`, without reading or locking them.
fn idle(hold_at: Instant) {
    let cached = UPLOADS.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((due, unheld, dir, seen)) = cached.as_ref().map(|u| {
        let unheld = u.queue.items.iter().any(|i| i.unheld);
        (u.next_due_at(), unheld, u.dir.clone(), u.seen)
    }) else {
        drop(cached);
        thread::sleep(POLL);
        return;
    };
    drop(cached);
    let stamp = |name| Stamp::read(&dir.join(name)).ok().flatten();
    loop {
        let now = now();
        let mut wait = match due {
            Some(due) if due <= now => return,
            Some(due) => POLL.min(Duration::from_secs(due - now)),
            None => POLL,
        };
        if unheld {
            let left = hold_at.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            wait = wait.min(left);
        }
        let guard = WAKE.0.lock().unwrap_or_else(PoisonError::into_inner);
        let woken = !WAKE
            .1
            .wait_timeout(guard, wait)
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .timed_out();
        if woken || (stamp(QUEUE_FILE), stamp(JOURNAL_FILE)) != seen {
            return;
        }
    }
}

/// The upload to start now, marked as uploading.
fn next_due(uploads: &Uploads) -> Result<(Option<Item>, Change)> {
    let now = now();
    let due = uploads
        .queue
        .items
        .iter()
        .filter(|i| i.state == State::Pending && uploads.queue.due_at(i) <= now)
        .min_by_key(|i| (i.next_attempt, i.queued));
    Ok(match due {
        Some(item) => {
            let item = Item {
                state: State::Uploading,
                ..item.clone()
            };
            let change = Change {
                volatile: true,
                ..Change::put(vec![item.clone()])
            };
            (Some(item), change)
        }
        None => (None, Change::default()),
    })
}

/// What an upload attempt's outcome changes: a verified upload leaves the
/// queue for the completed log; a failure waits or needs attention.
fn update_after(uploads: &Uploads, item: &Item, result: &Result<()>) -> Change {
    let queue = &uploads.queue;
    let Some(entry) = queue.items.iter().find(|i| i.id == item.id) else {
        return Change::default();
    };
    let mut totals = queue.totals.clone();
    match result {
        Ok(()) => {
            totals.uploaded += 1;
            totals.uploaded_bytes += item.bytes;
            totals.backoff.remove(&item.destination);
            if !queue.items.iter().any(|i| i.state == State::Failed) {
                totals.last_error = None;
            }
            let remote = item.remote();
            Change {
                remove: vec![item.id.clone()],
                totals: Some(totals),
                completed: Some(Completed {
                    seq: uploads.log.seq + 1,
                    id: item.id.clone(),
                    destination: item.destination.clone(),
                    kind: remote.kind().into(),
                    path: item.path.clone(),
                    key: item.key.clone(),
                    url: remote.object_url(&item.key),
                    bytes: item.bytes,
                    sha256: item.sha256.clone(),
                    uploaded_ms: now_ms(),
                }),
                ..Change::default()
            }
        }
        Err(error) => {
            let text = format!("{error:#}");
            let mut entry = entry.clone();
            entry.attempts = entry.attempts.saturating_add(1);
            if crate::transfer::retryable(error) {
                let now = now();
                entry.state = State::Pending;
                entry.next_attempt = now + backoff(entry.attempts);
                let failures = totals
                    .backoff
                    .get(&item.destination)
                    .map_or(0, |b| b.failures)
                    .saturating_add(1);
                let until = now + backoff(failures);
                totals
                    .backoff
                    .insert(item.destination.clone(), Backoff { failures, until });
            } else {
                entry.state = State::Failed;
                totals.failures += 1;
            }
            entry.error = Some(text.clone());
            totals.last_error = Some(format!("{}: {text}", item.key));
            Change {
                put: vec![entry],
                totals: Some(totals),
                ..Change::default()
            }
        }
    }
}

fn client_key(item: &Item) -> String {
    serde_json::to_string(&item.remote()).unwrap_or_default()
}

fn upload_one(
    clients: &mut HashMap<String, (Box<dyn Uploader>, Instant)>,
    item: &Item,
) -> Result<()> {
    let meta = fs::metadata(&item.path)
        .map_err(|e| Permanent(format!("local file {} is gone: {e}", item.path.display())))?;
    ensure!(
        meta.is_file() && meta.len() == item.bytes,
        Permanent(format!(
            "local file {} changed size after it was queued; not uploaded",
            item.path.display()
        ))
    );
    let key = client_key(item);
    if !clients.contains_key(&key) {
        if clients.len() >= MAX_CLIENTS {
            clients.clear();
        }
        let client = crate::transfer::connect(&item.remote())?;
        clients.insert(key.clone(), (client, Instant::now()));
    }
    let (client, used) = clients.get_mut(&key).expect("client was just inserted");
    *used = Instant::now();
    client.upload(&item.path, &item.key, &item.sha256, item.bytes)
}

/// After a verified upload: delete the local copy, or release it to normal
/// retention when the destination keeps local copies.
fn finish_local(item: &Item) {
    let result = if item.remote().keep_local() {
        crate::storage::hold_for_upload(&item.path, false).map(|_| true)
    } else {
        crate::storage::remove_uploaded(&item.path)
    };
    match result {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "capturefab: kept {} after upload because it changed since capture",
            item.path.display()
        ),
        Err(error) => eprintln!(
            "capturefab: uploaded {} but could not tidy the local copy: {error:#}",
            item.path.display()
        ),
    }
}

/// Start the uploader and wait until nothing is waiting to upload now, or the
/// timeout passes; for CLI captures that should not exit with uploads queued.
pub fn drain(timeout: Duration) -> Result<Value> {
    ensure!(
        std::time::Instant::now().checked_add(timeout).is_some(),
        "upload wait timeout does not fit in the monotonic clock"
    );
    start();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let busy = read(|uploads| {
            let now = now();
            let queue = &uploads.queue;
            Ok(queue.items.iter().any(|i| {
                i.state == State::Uploading || (i.state == State::Pending && queue.due_at(i) <= now)
            }))
        })?;
        if !busy || std::time::Instant::now() >= deadline {
            return status();
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::{FtpServer, FtpTls, NoOverwrite, SecretSource};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Make the storage ledger refuse to hold queued files.
    pub(super) static HOLD_FAILS: AtomicBool = AtomicBool::new(false);

    fn bucket() -> Bucket {
        Bucket {
            endpoint: "https://storage.example".into(),
            region: "auto".into(),
            bucket: "camera-archive".into(),
            prefix: String::new(),
            path_style: true,
            credentials: crate::destination::Credentials::Environment,
            keep_local: true,
            no_overwrite: NoOverwrite::IfNoneMatch,
        }
    }

    fn ftp() -> Remote {
        Remote::Ftp(FtpServer {
            host: "127.0.0.1".into(),
            port: 2121,
            user: "cam".into(),
            dir: "in".into(),
            tls: FtpTls::Plain,
            password: SecretSource::Environment,
            keep_local: false,
        })
    }

    fn item() -> Item {
        Item {
            id: "0123456789abcdef".into(),
            destination: "archive".into(),
            bucket: Some(bucket()),
            remote: None,
            path: std::env::temp_dir().join("capturefab-valid-upload.png"),
            key: "line-1/shot.png".into(),
            bytes: 10,
            sha256: "a".repeat(64),
            queued: 0,
            attempts: 0,
            next_attempt: 0,
            state: State::Pending,
            error: None,
            unheld: false,
        }
    }

    #[test]
    fn queued_upload_metadata_requires_valid_destination_key_path_and_checksum() {
        let item = item();
        assert!(item.validate().is_ok());
        let mut invalid = item.clone();
        invalid.key = "../outside".into();
        assert!(invalid.validate().is_err());
        invalid = item.clone();
        invalid.sha256 = "A".repeat(64);
        assert!(invalid.validate().is_err());
        invalid = item.clone();
        invalid.path = "relative.png".into();
        assert!(invalid.validate().is_err());
        for id in ["", "bad", "0123456789ABCDEF", "0123456789abcdeg"] {
            assert!(validate_id(id).is_err());
            assert!(retry(Some(id), false).is_err());
            assert!(forget(id).is_err());
        }
    }

    #[test]
    fn legacy_item_with_bucket_loads_as_s3_and_reserializes_as_bucket() {
        let legacy = serde_json::json!({"id":"0123456789abcdef","destination":"archive","bucket":{"endpoint":"https://storage.example","region":"auto","bucket":"camera-archive","path_style":true,"credentials":{"source":"environment"},"keep_local":true},"path":item().path,"key":"line-1/shot.png","bytes":10,"sha256":"a".repeat(64),"queued":0,"attempts":0,"next_attempt":0,"state":"pending"});
        let parsed: Item = serde_json::from_value(legacy.clone()).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.remote(), Remote::S3(bucket()));
        let value = serde_json::to_value(&parsed).unwrap();
        assert!(value.get("remote").is_none());
        assert_eq!(value["bucket"]["bucket"], "camera-archive");
        let mut server = item();
        server.bucket = None;
        server.remote = Some(ftp());
        server.validate().unwrap();
        let value = serde_json::to_value(&server).unwrap();
        assert!(value.get("bucket").is_none());
        assert_eq!(value["remote"]["kind"], "ftp");
        assert_eq!(
            serde_json::from_value::<Item>(value).unwrap().remote(),
            ftp()
        );
    }

    #[test]
    fn item_with_both_or_neither_remote_fields_is_invalid() {
        let mut both = item();
        both.remote = Some(ftp());
        assert!(
            both.validate()
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
        let mut neither = item();
        neither.bucket = None;
        assert!(neither.validate().is_err());
        let mut s3_as_remote = item();
        s3_as_remote.bucket = None;
        s3_as_remote.remote = Some(Remote::S3(bucket()));
        assert!(
            s3_as_remote.validate().is_err(),
            "S3 items keep the legacy field"
        );
    }

    /// Run queue tests in a child with its own session directory, so they
    /// never touch the launching user's queue or the process environment.
    fn isolated(name: &str) -> bool {
        if std::env::var_os("CAPTUREFAB_UPLOAD_QUEUE_TEST").is_some() {
            return true;
        }
        let dir = std::env::temp_dir().join(format!(
            "capturefab-upload-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::ipc::ensure_private_dir(&dir).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("upload::tests::{name}"), "--nocapture"])
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .env("CAPTUREFAB_UPLOAD_QUEUE_TEST", "1")
            .output()
            .unwrap();
        let _ = fs::remove_dir_all(dir);
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    #[test]
    fn enqueue_dedups_by_path_returns_existing_id() {
        if !isolated("enqueue_dedups_by_path_returns_existing_id") {
            return;
        }
        let path = staged("shot.png");
        let id = enqueue("line", &ftp(), &path, "shot.png").unwrap();
        assert_eq!(enqueue("line", &ftp(), &path, "shot.png").unwrap(), id);
        assert!(enqueue("line", &ftp(), &path, "other.png").is_err());
        assert!(enqueue("line", &Remote::S3(bucket()), &path, "shot.png").is_err());
        let status = status().unwrap();
        assert_eq!(status["pending"], 1);
        assert_eq!(status["items"][0]["remote"]["kind"], "ftp");
        assert_eq!(status["completed_seq"], 0);
    }

    /// The queue file and journal as they stand.
    fn versions() -> (Option<Stamp>, Option<Stamp>) {
        let dir = crate::ipc::session_dir();
        (
            Stamp::read(&dir.join(QUEUE_FILE)).unwrap(),
            Stamp::read(&dir.join(JOURNAL_FILE)).unwrap(),
        )
    }

    /// Put off the first queued upload's next attempt by an hour.
    fn back_off_first() {
        with_queue(|uploads| {
            let item = Item {
                next_attempt: now() + 3600,
                ..uploads.queue.items[0].clone()
            };
            Ok(((), Change::put(vec![item])))
        })
        .unwrap();
    }

    fn staged(name: &str) -> PathBuf {
        let dir = crate::ipc::session_dir().join("staged");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        crate::storage::save_bytes(
            b"0123456789",
            &path,
            &crate::storage::StoragePolicy::default(),
        )
        .unwrap();
        path
    }

    /// Start this process's uploader and wait until it holds the lock.
    fn start_uploading() {
        start();
        assert!(
            eventually(Duration::from_secs(10), || *OWNER
                .get()
                .unwrap()
                .lock()
                .unwrap()),
            "this process uploads"
        );
    }

    fn eventually(limit: Duration, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while !done() {
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
        true
    }

    #[test]
    fn status_and_an_idle_uploader_write_nothing() {
        if !isolated("status_and_an_idle_uploader_write_nothing") {
            return;
        }
        status().unwrap();
        assert_eq!(versions(), (None, None), "status creates no queue");
        enqueue("line", &ftp(), &staged("shot.png"), "shot.png").unwrap();
        // Its next attempt is an hour away, so the uploader has nothing to do.
        back_off_first();
        let (before, writes) = (versions(), WRITTEN.load(Ordering::Relaxed));
        status().unwrap();
        start_uploading();
        // More than two idle passes of the uploader.
        thread::sleep(Duration::from_millis(4500));
        assert_eq!(status().unwrap()["pending"], 1);
        assert_eq!(WRITTEN.load(Ordering::Relaxed), writes);
        assert_eq!(versions(), before);
    }

    /// Valid items for queue tests, with no files behind them.
    fn items(range: std::ops::Range<usize>) -> Vec<Item> {
        range
            .map(|n| Item {
                id: format!("{n:016x}"),
                path: std::env::temp_dir().join(format!("capturefab-queued-{n}.png")),
                key: format!("line-1/{n}.png"),
                queued: n as u64,
                ..item()
            })
            .collect()
    }

    fn put(items: Vec<Item>) {
        with_queue(|_| Ok(((), Change::put(items)))).unwrap();
    }

    /// Items an uploader has tried before, as in a backlog.
    fn tried(items: Vec<Item>) -> Vec<Item> {
        items
            .into_iter()
            .map(|item| Item {
                attempts: 1,
                ..item
            })
            .collect()
    }

    /// Record an attempt's outcome, as the uploader does.
    fn finish(item: &Item, result: Result<()>) {
        with_queue(|uploads| Ok(((), update_after(uploads, item, &result)))).unwrap();
    }

    fn pending() -> Value {
        status().unwrap()["pending"].clone()
    }

    #[test]
    fn draining_a_backlog_writes_bytes_in_proportion_to_it() {
        if !isolated("draining_a_backlog_writes_bytes_in_proportion_to_it") {
            return;
        }
        const BACKLOG: usize = 5000;
        put(items(0..BACKLOG));
        let before = WRITTEN.load(Ordering::Relaxed);
        for _ in 0..BACKLOG {
            let item = with_queue(next_due).unwrap().expect("an upload is due");
            with_queue(|uploads| {
                // Flushing to disk is not what this measures.
                let change = Change {
                    volatile: true,
                    ..update_after(uploads, &item, &Ok(()))
                };
                Ok(((), change))
            })
            .unwrap();
        }
        let written = WRITTEN.load(Ordering::Relaxed) - before;
        let status = status().unwrap();
        assert_eq!(
            (status["pending"].clone(), status["uploaded"].clone()),
            (json!(0), json!(BACKLOG))
        );
        assert_eq!(status["completed_seq"], BACKLOG);
        // Rewriting the queue for each upload would take gigabytes.
        assert!(
            written < BACKLOG as u64 * 4096,
            "{written} bytes written to drain {BACKLOG} uploads"
        );
    }

    #[test]
    fn readers_catch_up_through_the_journal() {
        if !isolated("readers_catch_up_through_the_journal") {
            return;
        }
        let take = || UPLOADS.lock().unwrap().take();
        let journal = crate::ipc::session_dir().join(JOURNAL_FILE);
        put(items(0..2));
        // Another process's view, read before the changes below.
        let stale = take();
        put(items(2..3));
        // A writer that stopped mid-line leaves part of one.
        let mut file = fs::OpenOptions::new().append(true).open(&journal).unwrap();
        file.write_all(b"{\"put\":[{\"id\"").unwrap();
        *UPLOADS.lock().unwrap() = stale;
        assert_eq!(pending(), 3, "replays only the new complete lines");
        put(items(3..4));
        take();
        assert_eq!(pending(), 4, "the next change replaced the unfinished line");
        // Folding the journal in replaces the queue file, so a stale view
        // reads it again.
        let stale = take();
        with_lock(false, |dir| {
            let mut cached = UPLOADS.lock().unwrap();
            refresh(dir, &mut cached)?.compact(dir)
        })
        .unwrap();
        assert!(!journal.exists());
        *UPLOADS.lock().unwrap() = stale;
        assert_eq!(pending(), 4);
        put(items(4..5));
        take();
        assert_eq!(pending(), 5);
        let ids: Vec<String> = status().unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            ids,
            items(0..5)
                .into_iter()
                .rev()
                .map(|i| i.id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_idle_uploader_wakes_for_other_processes_and_due_retries() {
        if !isolated("an_idle_uploader_wakes_for_other_processes_and_due_retries") {
            return;
        }
        start_uploading();
        let attempts = || status().unwrap()["items"][0]["attempts"].as_u64();
        // Queued as another process would, without waking this one's
        // uploader. Its file is missing, so the attempt fails for good.
        put(items(0..1));
        assert!(eventually(Duration::from_secs(6), || attempts() == Some(1)));
        // A retry due in three seconds starts then, not before.
        with_queue(|uploads| {
            let item = Item {
                state: State::Pending,
                next_attempt: now() + 3,
                ..uploads.queue.items[0].clone()
            };
            Ok(((), Change::put(vec![item])))
        })
        .unwrap();
        thread::sleep(Duration::from_secs(1));
        assert_eq!(attempts(), Some(1));
        assert!(eventually(Duration::from_secs(8), || attempts() == Some(2)));
    }

    #[test]
    fn a_failing_destination_waits_as_a_whole() {
        if !isolated("a_failing_destination_waits_as_a_whole") {
            return;
        }
        let mut queued = tried(items(0..3));
        queued[2].destination = "other".into();
        put(queued.clone());
        let backoff = || read(|uploads| Ok(uploads.queue.totals.backoff.get("archive").cloned()));
        let first = with_queue(next_due).unwrap().unwrap();
        assert_eq!(first.id, queued[0].id);
        finish(&first, Err(anyhow!("connection refused")));
        let backoff_until = backoff().unwrap().unwrap().until;
        assert!(backoff_until >= now() + 9);
        // The archive's other upload waits with it; another destination goes
        // ahead.
        let next = with_queue(next_due).unwrap().unwrap();
        assert_eq!(next.destination, "other");
        finish(&next, Ok(()));
        assert!(with_queue(next_due).unwrap().is_none());
        assert_eq!(
            read(|uploads| Ok(uploads.next_due_at())).unwrap(),
            Some(backoff_until)
        );
        // A new upload goes ahead and finds the destination back, which
        // clears its backoff.
        let new = items(3..4).remove(0);
        put(vec![new.clone()]);
        assert_eq!(with_queue(next_due).unwrap().unwrap().id, new.id);
        finish(&new, Ok(()));
        assert!(backoff().unwrap().is_none());
        assert_eq!(with_queue(next_due).unwrap().unwrap().id, queued[1].id);
    }

    #[test]
    fn a_retried_upload_starts_while_its_destination_backs_off() {
        if !isolated("a_retried_upload_starts_while_its_destination_backs_off") {
            return;
        }
        let queued = tried(items(0..2));
        put(queued.clone());
        finish(
            &queued[0],
            Err(Permanent("credentials refused".into()).into()),
        );
        finish(&queued[1], Err(anyhow!("connection refused")));
        assert!(with_queue(next_due).unwrap().is_none());
        // Once the credentials are fixed, a plain retry tries it at once.
        assert_eq!(retry(None, false).unwrap(), 1);
        assert_eq!(with_queue(next_due).unwrap().unwrap().id, queued[0].id);
    }

    #[test]
    fn a_capture_the_ledger_cannot_hold_is_queued_and_held_later() {
        if !isolated("a_capture_the_ledger_cannot_hold_is_queued_and_held_later") {
            return;
        }
        let path = staged("shot.png");
        HOLD_FAILS.store(true, Ordering::Relaxed);
        let id = enqueue("line", &ftp(), &path, "shot.png").unwrap();
        let first = || status().unwrap()["items"][0].clone();
        assert_eq!(first()["id"], id.as_str());
        assert_eq!(first()["unheld"], true);
        assert_eq!(
            first()["sha256"],
            crate::transfer::hex(&<sha2::Sha256 as sha2::Digest>::digest(b"0123456789"))
        );
        // The uploader's next try finds the ledger free.
        hold_unheld().unwrap();
        assert_eq!(first()["unheld"], true, "still refused");
        HOLD_FAILS.store(false, Ordering::Relaxed);
        hold_unheld().unwrap();
        assert!(first().get("unheld").is_none());
        // Held, so retention keeps it even when a new capture needs room.
        let policy = crate::storage::StoragePolicy {
            max_files: 1,
            on_full: "delete-oldest".into(),
            ..Default::default()
        };
        let next = path.with_file_name("next.png");
        let error = crate::storage::save_bytes(b"next", &next, &policy).unwrap_err();
        assert!(error.to_string().contains("Storage full"), "{error:#}");
        assert!(path.exists());
    }

    #[test]
    fn status_counts_every_upload_of_each_destination() {
        if !isolated("status_counts_every_upload_of_each_destination") {
            return;
        }
        let mut queued = tried(items(0..502));
        for (item, state) in queued[500..]
            .iter_mut()
            .zip([State::Pending, State::Failed])
        {
            item.destination = "other".into();
            item.state = state;
            item.error = Some(format!("{state:?} error"));
        }
        put(queued.clone());
        // The archive's first upload fails, and all 500 wait for its backoff.
        let first = with_queue(next_due).unwrap().unwrap();
        finish(&first, Err(anyhow!("connection refused")));
        let queue = status().unwrap();
        assert_eq!(queue["items"].as_array().unwrap().len(), 200);
        let archive = &queue["destinations"]["archive"];
        assert_eq!(
            (
                &archive["pending"],
                &archive["failed"],
                &archive["waiting_bytes"]
            ),
            (&json!(500), &json!(0), &json!(5000))
        );
        assert!(archive["next_attempt"].as_u64().unwrap() >= now() + 9);
        assert_eq!(archive["last_error"], "connection refused");
        let other = &queue["destinations"]["other"];
        assert_eq!(
            (&other["pending"], &other["failed"]),
            (&json!(1), &json!(1))
        );
        assert!(other["next_attempt"].as_u64().unwrap() <= now());
        assert_eq!(other["last_error"], "Failed error");
        // Waiting uploads are retried only on request.
        let waiting = &queued[1].id;
        assert!(retry(Some(waiting), false).is_err());
        assert!(retry(Some(&queued[500].id), true).is_err(), "already due");
        assert_eq!(retry(None, false).unwrap(), 1);
        assert_eq!(retry(None, true).unwrap(), 500);
        let archive = &status().unwrap()["destinations"]["archive"];
        assert!(archive["next_attempt"].as_u64().unwrap() <= now());
        assert!(read(|uploads| Ok(uploads.queue.totals.backoff.is_empty())).unwrap());
    }

    #[test]
    fn the_uploader_holds_unheld_captures_once_the_ledger_allows() {
        if !isolated("the_uploader_holds_unheld_captures_once_the_ledger_allows") {
            return;
        }
        HOLD_FAILS.store(true, Ordering::Relaxed);
        enqueue("line", &ftp(), &staged("shot.png"), "shot.png").unwrap();
        back_off_first();
        start_uploading();
        // Its first try is refused; the next comes while it waits.
        thread::sleep(Duration::from_secs(1));
        let unheld = || status().unwrap()["items"][0]["unheld"] == true;
        assert!(unheld());
        HOLD_FAILS.store(false, Ordering::Relaxed);
        assert!(eventually(HOLD_RETRY + Duration::from_secs(5), || !unheld()));
    }

    #[test]
    fn the_uploader_stops_holding_a_capture_the_ledger_no_longer_has() {
        if !isolated("the_uploader_stops_holding_a_capture_the_ledger_no_longer_has") {
            return;
        }
        let path = staged("shot.png");
        HOLD_FAILS.store(true, Ordering::Relaxed);
        enqueue("line", &ftp(), &path, "shot.png").unwrap();
        HOLD_FAILS.store(false, Ordering::Relaxed);
        // Removed before the uploader could hold it: there is nothing left
        // to hold, so the uploader has no reason to come back for it.
        fs::remove_file(&path).unwrap();
        hold_unheld().unwrap();
        let item = status().unwrap()["items"][0].clone();
        assert!(item.get("unheld").is_none(), "{item}");
        // Only a busy ledger queues a file unheld; one it never recorded is
        // refused.
        let stray = crate::ipc::session_dir().join("stray.png");
        fs::write(&stray, b"0123456789").unwrap();
        let error = enqueue("line", &ftp(), &stray, "stray.png").unwrap_err();
        assert!(error.to_string().contains("not a finished"), "{error:#}");
        assert_eq!(pending(), 1);
    }

    /// An FTP server nobody listens on, so connecting fails at once in a
    /// way worth retrying.
    #[cfg(feature = "ftp")]
    fn closed_ftp() -> Remote {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let Remote::Ftp(server) = ftp() else {
            unreachable!()
        };
        Remote::Ftp(FtpServer {
            port,
            password: SecretSource::NoSecret,
            ..server
        })
    }

    #[cfg(feature = "ftp")]
    #[test]
    fn a_wait_tries_every_new_upload_after_one_fails() {
        if !isolated("a_wait_tries_every_new_upload_after_one_fails") {
            return;
        }
        let remote = closed_ftp();
        let queue = |files: std::ops::Range<usize>| {
            for n in files {
                let name = format!("{n}.png");
                enqueue("line", &remote, &staged(&name), &name).unwrap();
            }
        };
        let attempts = |status: &Value| -> Vec<u64> {
            let items = status["items"].as_array().unwrap();
            items
                .iter()
                .map(|i| i["attempts"].as_u64().unwrap())
                .collect()
        };
        queue(0..3);
        // The first refusal backs the destination off, yet a direct capture
        // still tries each of its files.
        let status = drain(Duration::from_secs(30)).unwrap();
        assert_eq!(attempts(&status), [1, 1, 1]);
        assert!(
            status["destinations"]["line"]["next_attempt"]
                .as_u64()
                .unwrap()
                > now()
        );
        // So does the next one, while the destination backs off.
        queue(3..4);
        let status = drain(Duration::from_secs(30)).unwrap();
        assert_eq!(attempts(&status), [1, 1, 1, 1]);
    }

    #[test]
    fn completed_log_caps_at_1000_and_seq_is_monotonic() {
        if !isolated("completed_log_caps_at_1000_and_seq_is_monotonic") {
            return;
        }
        let entry = |n: u64| Completed {
            seq: 0,
            id: format!("{n:016x}"),
            destination: "line".into(),
            kind: "ftp".into(),
            path: PathBuf::from("/staged/a.png"),
            key: format!("{n}.png"),
            url: format!("ftp://h:21/{n}.png"),
            bytes: n,
            sha256: "a".repeat(64),
            uploaded_ms: n,
        };
        for n in 1..=1005 {
            let seq = with_queue(|uploads| {
                let seq = uploads.log.seq + 1;
                let completed = Some(Completed { seq, ..entry(n) });
                Ok((
                    seq,
                    Change {
                        completed,
                        ..Change::default()
                    },
                ))
            });
            assert_eq!(seq.unwrap(), n);
        }
        let (seq, all) = completed_since(0).unwrap();
        assert_eq!((seq, all.len()), (1005, MAX_COMPLETED));
        assert_eq!(all[0].seq, 6);
        assert!(all.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
        let (_, recent) = completed_since(1003).unwrap();
        assert_eq!(
            recent.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
            ["1004.png", "1005.png"]
        );
        // Status reads the log but never rewrites it.
        let path = crate::ipc::session_dir().join(COMPLETED_FILE);
        let before = (fs::read(&path).unwrap(), versions());
        let status = status().unwrap();
        assert_eq!(status["completed_seq"], 1005);
        assert_eq!(status["recent_completed"].as_array().unwrap().len(), 20);
        assert_eq!(status["recent_completed"][0]["seq"], 1005);
        assert_eq!((fs::read(&path).unwrap(), versions()), before);
    }
}
