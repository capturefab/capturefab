//! A persistent, cross-process queue of finished captures waiting for upload to
//! S3-compatible destinations.
//!
//! Workers add files as they finish; the storage ledger holds each one back
//! from retention until it is uploaded. One process at a time uploads (the GUI,
//! `serve`, or a CLI capture that waits for its uploads): whichever holds
//! `uploader.lock`. Network and service failures are retried with backoff;
//! refused credentials, a missing bucket or an existing object wait for a
//! manual retry. After a verified upload the local copy is deleted (only if
//! unchanged) unless the destination keeps it.
use crate::destination::Bucket;
use anyhow::{Context, Result, anyhow, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_ITEMS: usize = 100_000;
const MAX_BACKOFF: u64 = 15 * 60;

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
    pub bucket: Bucket,
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct Queue {
    version: u32,
    items: Vec<Item>,
    #[serde(default)]
    uploaded: u64,
    #[serde(default)]
    uploaded_bytes: u64,
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

/// Run `update` on the queue under an exclusive lock, persisting the result.
fn with_queue<T>(update: impl FnOnce(&mut Queue) -> Result<T>) -> Result<T> {
    let dir = dir()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("uploads.lock"))?;
    lock.lock_exclusive().context("lock the upload queue")?;
    let path = dir.join("uploads.json");
    let mut queue = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid upload queue {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Queue {
            version: 1,
            ..Queue::default()
        },
        Err(e) => return Err(e).context("read the upload queue"),
    };
    ensure!(queue.version == 1, "unsupported upload queue version");
    let result = update(&mut queue)?;
    let temp = dir.join(format!("uploads.json.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(&serde_json::to_vec(&queue)?)?;
    file.sync_all()?;
    fs::rename(&temp, &path)?;
    Ok(result)
}

/// Wakes this process's uploader when it queues a file itself.
static WAKE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

fn id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Queue a finished, ledger-recorded file for upload as `key`.
pub fn enqueue(destination: &str, bucket: &Bucket, path: &Path, key: &str) -> Result<()> {
    let sha256 = crate::storage::hold_for_upload(path, true)?;
    let path = fs::canonicalize(path)?;
    let bytes = fs::metadata(&path)?.len();
    with_queue(|queue| {
        ensure!(
            queue.items.len() < MAX_ITEMS,
            "the upload queue is full ({MAX_ITEMS} files); check the destination and retry failed uploads"
        );
        queue.items.push(Item {
            id: id(),
            destination: destination.into(),
            bucket: bucket.clone(),
            path,
            key: key.into(),
            bytes,
            sha256,
            queued: now(),
            attempts: 0,
            next_attempt: 0,
            state: State::Pending,
            error: None,
        });
        Ok(())
    })?;
    WAKE.1.notify_all();
    Ok(())
}

/// Queue summary and the most recent items, for status displays.
pub fn status() -> Result<Value> {
    let active = uploader_active();
    with_queue(|queue| {
        let count = |state| queue.items.iter().filter(|i| i.state == state).count();
        let waiting_bytes: u64 = queue
            .items
            .iter()
            .filter(|i| i.state != State::Failed)
            .map(|i| i.bytes)
            .sum();
        let recent: Vec<_> = queue.items.iter().rev().take(200).collect();
        Ok(json!({
            "pending": count(State::Pending),
            "uploading": count(State::Uploading),
            "failed": count(State::Failed),
            "waiting_bytes": waiting_bytes,
            "uploaded": queue.uploaded,
            "uploaded_bytes": queue.uploaded_bytes,
            "last_error": queue.last_error,
            "uploader_running": active,
            "items": recent,
        }))
    })
}

/// Move failed uploads (all, or one by id) back to pending.
pub fn retry(id: Option<&str>) -> Result<usize> {
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
        Ok(changed)
    })?;
    WAKE.1.notify_all();
    Ok(changed)
}

/// Drop a failed upload from the queue; its local file is kept and released
/// to normal retention.
pub fn forget(id: &str) -> Result<Item> {
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
    let mut clients: HashMap<String, crate::s3::Client> = HashMap::new();
    loop {
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
        let _ = with_queue(|queue| {
            let Some(index) = queue.items.iter().position(|i| i.id == item.id) else {
                return Ok(());
            };
            match &result {
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
                    if crate::s3::retryable(error) {
                        entry.state = State::Pending;
                        entry.next_attempt =
                            now() + (5u64 << entry.attempts.min(8)).min(MAX_BACKOFF);
                    } else {
                        entry.state = State::Failed;
                    }
                    entry.error = Some(text.clone());
                    queue.last_error = Some(format!("{}: {text}", item.key));
                }
            }
            Ok(())
        });
        if result.is_ok() {
            finish_local(&item);
        } else {
            // A client with stale credentials is rebuilt on the next attempt.
            clients.remove(&client_key(&item));
        }
    }
}

fn client_key(item: &Item) -> String {
    serde_json::to_string(&item.bucket).unwrap_or_default()
}

fn upload_one(clients: &mut HashMap<String, crate::s3::Client>, item: &Item) -> Result<()> {
    let meta = fs::metadata(&item.path).map_err(|e| {
        crate::s3::Permanent(format!("local file {} is gone: {e}", item.path.display()))
    })?;
    let key = client_key(item);
    if !clients.contains_key(&key) {
        clients.insert(key.clone(), crate::s3::Client::new(&item.bucket)?);
    }
    clients[&key].upload(&item.path, &item.key, &item.sha256, meta.len())
}

/// After a verified upload: delete the local copy, or release it to normal
/// retention when the destination keeps local copies.
fn finish_local(item: &Item) {
    let result = if item.bucket.keep_local {
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
    start();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let busy = with_queue(|queue| {
            let now = now();
            Ok(queue.items.iter().any(|i| {
                i.state == State::Uploading || (i.state == State::Pending && i.next_attempt <= now)
            }))
        })?;
        if !busy || std::time::Instant::now() >= deadline {
            return status();
        }
        thread::sleep(Duration::from_millis(200));
    }
}
