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
use crate::destination::{Bucket, Remote};
use crate::transfer::{Permanent, Uploader};
use anyhow::{Context, Result, anyhow, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, OnceLock},
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
    #[serde(default)]
    uploaded: u64,
    #[serde(default)]
    uploaded_bytes: u64,
    /// Times an upload has failed for good; retries don't lower it.
    #[serde(default)]
    failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
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

/// Hold `uploads.lock` while `f` runs; it guards the queue and the completed
/// log.
fn with_lock<T>(f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let dir = dir()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("uploads.lock"))?;
    lock.lock_exclusive().context("lock the upload queue")?;
    f(&dir)
}

/// Files written to the queue directory, so tests can prove idle paths
/// write nothing.
#[cfg(test)]
static WRITES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Write `bytes` to `dir/name` atomically, readable only by this user.
fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    #[cfg(test)]
    WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, dir.join(name))?;
    Ok(())
}

/// Run `update` on the queue under an exclusive lock, persisting the result
/// when it changed anything.
fn with_queue<T>(update: impl FnOnce(&mut Queue) -> Result<T>) -> Result<T> {
    with_lock(|dir| with_queue_locked(dir, update))
}

fn with_queue_locked<T>(dir: &Path, update: impl FnOnce(&mut Queue) -> Result<T>) -> Result<T> {
    let (mut queue, read) = read_queue(dir)?;
    let result = update(&mut queue)?;
    // An idle uploader and status displays change nothing; writing only real
    // changes keeps them off the disk.
    let bytes = serde_json::to_vec(&queue)?;
    if bytes != read {
        write_private(dir, "uploads.json", &bytes)?;
    }
    Ok(result)
}

/// The queue and its bytes as last written, without creating the file. The
/// caller holds `uploads.lock`.
fn read_queue(dir: &Path) -> Result<(Queue, Vec<u8>)> {
    let path = dir.join("uploads.json");
    let (queue, bytes) = match fs::read(&path) {
        Ok(bytes) => {
            ensure!(
                bytes.len() <= 256 * 1024 * 1024,
                "upload queue is too large (maximum 256 MiB)"
            );
            let queue: Queue = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid upload queue {}", path.display()))?;
            (queue, bytes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let queue = Queue {
                version: 1,
                ..Queue::default()
            };
            let bytes = serde_json::to_vec(&queue)?;
            (queue, bytes)
        }
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
    Ok((queue, bytes))
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct CompletedLog {
    version: u32,
    seq: u64,
    entries: VecDeque<Completed>,
}

const COMPLETED_FILE: &str = "uploads-completed.json";

/// Read the completed log; the caller holds `uploads.lock`.
fn read_completed(dir: &Path) -> Result<CompletedLog> {
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
            Ok(log)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CompletedLog {
            version: 1,
            ..CompletedLog::default()
        }),
        Err(e) => Err(e).context("read the completed upload log"),
    }
}

/// Append to the completed log, keeping the newest `MAX_COMPLETED`; the
/// caller holds `uploads.lock`. A damaged log is started again rather than
/// blocking uploads.
fn record_completed(dir: &Path, mut entry: Completed) -> Result<u64> {
    let mut log = read_completed(dir).unwrap_or(CompletedLog {
        version: 1,
        ..CompletedLog::default()
    });
    log.seq += 1;
    entry.seq = log.seq;
    log.entries.push_back(entry);
    while log.entries.len() > MAX_COMPLETED {
        log.entries.pop_front();
    }
    write_private(dir, COMPLETED_FILE, &serde_json::to_vec(&log)?)?;
    Ok(log.seq)
}

/// Uploads completed after `seq`, oldest first, and the latest sequence
/// number. Reads only the completed log; the queue is not rewritten.
pub fn completed_since(seq: u64) -> Result<(u64, Vec<Completed>)> {
    with_lock(|dir| {
        let log = read_completed(dir)?;
        Ok((
            log.seq,
            log.entries.into_iter().filter(|e| e.seq > seq).collect(),
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
    let mut held = false;
    let result = with_queue(|queue| {
        if let Some(existing) = queue.items.iter().find(|item| item.path == path) {
            ensure!(
                existing.destination == destination
                    && existing.remote() == *remote
                    && existing.key == key,
                "this file is already queued for a different upload; retry or forget the existing upload first"
            );
            return Ok(existing.id.clone());
        }
        ensure!(
            queue.items.len() < MAX_ITEMS,
            "the upload queue is full ({MAX_ITEMS} files); check the destination and retry failed uploads"
        );
        let id = id()?;
        ensure!(
            !queue.items.iter().any(|item| item.id == id),
            "upload ID collision; retry queueing this file"
        );
        let sha256 = crate::storage::hold_for_upload(&path, true)?;
        held = true;
        let (bucket, other) = match remote {
            Remote::S3(bucket) => (Some(bucket.clone()), None),
            other => (None, Some(other.clone())),
        };
        queue.items.push(Item {
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
        });
        Ok(id)
    });
    match result {
        Ok(id) => {
            WAKE.1.notify_all();
            Ok(id)
        }
        Err(error) => {
            if held {
                crate::storage::hold_for_upload(&path, false)
                    .context("release capture after it could not be queued for upload")?;
            }
            Err(error)
        }
    }
}

/// Queue summary and the most recent items, for status displays.
pub fn status() -> Result<Value> {
    let active = uploader_active();
    with_lock(|dir| {
        let completed = read_completed(dir).unwrap_or_default();
        let mut value = queue_status(&read_queue(dir)?.0)?;
        value["uploader_running"] = json!(active);
        value["completed_seq"] = json!(completed.seq);
        value["recent_completed"] =
            json!(completed.entries.iter().rev().take(20).collect::<Vec<_>>());
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
    Ok(json!({
        "pending": count(State::Pending),
        "uploading": count(State::Uploading),
        "failed": count(State::Failed),
        "waiting_bytes": waiting_bytes,
        "uploaded": queue.uploaded,
        "uploaded_bytes": queue.uploaded_bytes,
        "failures": queue.failures,
        "last_error": queue.last_error,
        "items": recent,
    }))
}

/// Move failed uploads (all, or one by id) back to pending.
pub fn retry(id: Option<&str>) -> Result<usize> {
    if let Some(id) = id {
        validate_id(id)?;
    }
    let changed = with_queue(|queue| {
        let mut changed = 0;
        for item in &mut queue.items {
            if item.state == State::Failed && id.is_none_or(|id| id == item.id) {
                item.state = State::Pending;
                item.attempts = 0;
                item.next_attempt = 0;
                changed += 1;
            }
        }
        ensure!(
            id.is_none() || changed > 0,
            "no failed upload with the requested ID; choose an ID from upload status"
        );
        Ok(changed)
    })?;
    WAKE.1.notify_all();
    Ok(changed)
}

/// Drop a failed upload from the queue; its local file is kept and released
/// to normal retention.
pub fn forget(id: &str) -> Result<Item> {
    validate_id(id)?;
    let item = with_queue(|queue| {
        let index = queue
            .items
            .iter()
            .position(|i| i.id == id && i.state == State::Failed)
            .ok_or_else(|| anyhow!("no failed upload {id}"))?;
        Ok(queue.items.remove(index))
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
    let _ = with_queue(|queue| {
        for item in &mut queue.items {
            if item.state == State::Uploading {
                item.state = State::Pending;
            }
        }
        Ok(())
    });
    let mut clients: HashMap<String, (Box<dyn Uploader>, Instant)> = HashMap::new();
    loop {
        clients.retain(|_, (_, used)| used.elapsed() < CLIENT_IDLE);
        let next = with_queue(|queue| {
            let now = now();
            let due = queue
                .items
                .iter_mut()
                .filter(|i| i.state == State::Pending && i.next_attempt <= now)
                .min_by_key(|i| (i.next_attempt, i.queued));
            Ok(due.map(|item| {
                item.state = State::Uploading;
                item.clone()
            }))
        });
        let item = match next {
            Ok(Some(item)) => item,
            Ok(None) | Err(_) => {
                let guard = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
                let _ = WAKE.1.wait_timeout(guard, Duration::from_secs(2));
                continue;
            }
        };
        let result = upload_one(&mut clients, &item);
        let _ = with_lock(|dir| {
            if result.is_ok() {
                let remote = item.remote();
                if let Err(error) = record_completed(
                    dir,
                    Completed {
                        seq: 0,
                        id: item.id.clone(),
                        destination: item.destination.clone(),
                        kind: remote.kind().into(),
                        path: item.path.clone(),
                        key: item.key.clone(),
                        url: remote.object_url(&item.key),
                        bytes: item.bytes,
                        sha256: item.sha256.clone(),
                        uploaded_ms: now_ms(),
                    },
                ) {
                    eprintln!("capturefab: cannot record a completed upload: {error:#}");
                }
            }
            with_queue_locked(dir, |queue| update_after(queue, &item, &result))
        });
        if result.is_ok() {
            finish_local(&item);
        } else {
            // A client with stale credentials or a dropped connection is
            // rebuilt on the next attempt.
            clients.remove(&client_key(&item));
        }
    }
}

/// Record an upload attempt's outcome in the queue.
fn update_after(queue: &mut Queue, item: &Item, result: &Result<()>) -> Result<()> {
    let Some(index) = queue.items.iter().position(|i| i.id == item.id) else {
        return Ok(());
    };
    match result {
        Ok(()) => {
            queue.items.remove(index);
            queue.uploaded += 1;
            queue.uploaded_bytes += item.bytes;
            if !queue.items.iter().any(|i| i.state == State::Failed) {
                queue.last_error = None;
            }
        }
        Err(error) => {
            let text = format!("{error:#}");
            let entry = &mut queue.items[index];
            entry.attempts = entry.attempts.saturating_add(1);
            if crate::transfer::retryable(error) {
                entry.state = State::Pending;
                entry.next_attempt = now() + (5u64 << entry.attempts.min(8)).min(MAX_BACKOFF);
            } else {
                entry.state = State::Failed;
                queue.failures += 1;
            }
            entry.error = Some(text.clone());
            queue.last_error = Some(format!("{}: {text}", item.key));
        }
    }
    Ok(())
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
        let busy = with_lock(|dir| {
            let now = now();
            Ok(read_queue(dir)?.0.items.iter().any(|i| {
                i.state == State::Uploading || (i.state == State::Pending && i.next_attempt <= now)
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
    use std::sync::atomic::Ordering;

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
            assert!(retry(Some(id)).is_err());
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

    /// A file's identity and version: inode (on Unix), size and
    /// modification time.
    fn version(path: &Path) -> (u64, u64, SystemTime) {
        let meta = fs::metadata(path).unwrap();
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        (inode, meta.len(), meta.modified().unwrap())
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

    #[test]
    fn status_and_an_idle_uploader_write_nothing() {
        if !isolated("status_and_an_idle_uploader_write_nothing") {
            return;
        }
        let queue = crate::ipc::session_dir().join("uploads.json");
        status().unwrap();
        assert!(!queue.exists(), "status creates no queue");
        enqueue("line", &ftp(), &staged("shot.png"), "shot.png").unwrap();
        // Its next attempt is an hour away, so the uploader has nothing to do.
        with_queue(|queue| {
            queue.items[0].next_attempt = now() + 3600;
            Ok(())
        })
        .unwrap();
        let (before, writes) = (version(&queue), WRITES.load(Ordering::Relaxed));
        status().unwrap();
        start();
        let owner = || *OWNER.get().unwrap().lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !owner() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(owner(), "this process uploads");
        // More than two idle passes of the uploader.
        thread::sleep(Duration::from_millis(4500));
        assert_eq!(status().unwrap()["pending"], 1);
        assert_eq!(WRITES.load(Ordering::Relaxed), writes);
        assert_eq!(version(&queue), before);
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
        with_lock(|dir| {
            for n in 1..=1005 {
                assert_eq!(record_completed(dir, entry(n))?, n);
            }
            Ok(())
        })
        .unwrap();
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
        let before = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let status = status().unwrap();
        assert_eq!(status["completed_seq"], 1005);
        assert_eq!(status["recent_completed"].as_array().unwrap().len(), 20);
        assert_eq!(status["recent_completed"][0]["seq"], 1005);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }
}
