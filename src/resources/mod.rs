//! Host safeguards and explicit budgets for camera work.
//!
//! These limits govern work Capturefab can bound directly: pixel/codec threads,
//! shared camera buffers. They are not OS-wide CPU or resident-memory quotas.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

pub mod buffers;
pub mod host;
pub use host::{HostSample, Pressure};

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;
const FILE: &str = "resources.json";
const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const REFRESH: Duration = Duration::from_secs(1);

/// `None` uses the automatic safeguard for that resource. Overrides remain
/// upper bounds: critical host pressure can make work more conservative.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Policy {
    pub cpu_threads: Option<u32>,
    pub buffer_bytes: Option<u64>,
}
impl<'de> Deserialize<'de> for Policy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Stored {
            cpu_threads: Option<u32>,
            buffer_bytes: Option<u64>,
            // Ignore a retired upload setting when loading an existing file.
            #[serde(rename = "upload_bytes_per_second")]
            _retired_upload_limit: Option<serde::de::IgnoredAny>,
        }
        let value = Stored::deserialize(deserializer)?;
        Ok(Self {
            cpu_threads: value.cpu_threads,
            buffer_bytes: value.buffer_bytes,
        })
    }
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.cpu_threads.is_none_or(|n| (1..=64).contains(&n)),
            "Choose 1 to 64 processing threads, or Automatic."
        );
        ensure!(
            self.buffer_bytes
                .is_none_or(|n| (64 * MIB..=64 * GIB).contains(&n)),
            "Choose a camera-buffer budget from 64 MiB to 64 GiB, or Automatic."
        );
        Ok(())
    }
}

struct Cache {
    path: PathBuf,
    checked: Instant,
    policy: Policy,
}
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn path() -> PathBuf {
    crate::ipc::state_dir().join(FILE)
}

/// Read without creating directories. A missing file means Automatic.
pub fn load() -> Result<Policy> {
    load_path(&path())
}
fn load_path(path: &Path) -> Result<Policy> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Resource limits must be a regular file."
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Policy::default()),
        Err(error) => return Err(error).context("inspect resource limits"),
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Policy::default()),
        Err(error) => return Err(error).context("read resource limits"),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "Resource limits file is too large."
    );
    let policy: Policy = serde_json::from_slice(&bytes).context("read resource limits")?;
    policy.validate()?;
    Ok(policy)
}

/// Cheap cached read for hot paths; every process notices changes within a
/// second. An unreadable replacement never silently lifts its last valid cap.
pub fn current() -> Policy {
    let path = path();
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(cached) = cache.as_ref()
        && cached.path == path
        && cached.checked.elapsed() < REFRESH
    {
        return cached.policy.clone();
    }
    let previous = cache
        .as_ref()
        .filter(|cached| cached.path == path)
        .map(|cached| cached.policy.clone());
    let policy = load_path(&path).unwrap_or_else(|_| previous.unwrap_or_default());
    *cache = Some(Cache {
        path,
        checked: Instant::now(),
        policy: policy.clone(),
    });
    policy
}

/// Replace all work limits, for a full settings restore.
pub fn configure(policy: &Policy) -> Result<Policy> {
    policy.validate()?;
    edit_at(&path(), |_| Ok(policy.clone()))
}

/// Apply an individual adjustment to the latest saved policy under the same
/// cross-process lock as the write. Independent windows cannot lose edits.
pub fn update(change: impl FnOnce(&mut Policy) -> Result<()>) -> Result<Policy> {
    update_at(&path(), change)
}
fn update_at(path: &Path, change: impl FnOnce(&mut Policy) -> Result<()>) -> Result<Policy> {
    edit_at(path, |path| {
        let mut policy = load_path(path)?;
        change(&mut policy)?;
        Ok(policy)
    })
}
fn edit_at(path: &Path, change: impl FnOnce(&Path) -> Result<Policy>) -> Result<Policy> {
    let parent = path.parent().context("resource limits directory")?;
    crate::ipc::ensure_private_dir(parent)?;
    let mut lock_options = OpenOptions::new();
    lock_options
        .create(true)
        .truncate(false)
        .write(true)
        .read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options.mode(0o600);
    }
    let lock = lock_options.open(parent.join("resources.lock"))?;
    lock.lock_exclusive()?;
    let policy = change(path)?;
    policy.validate()?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("resource limits temporary name: {error}"))?;
    let temporary = parent.join(format!(
        ".resources-{:016x}.tmp",
        u64::from_ne_bytes(random)
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(&policy)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.context("save resource limits")?;
    *CACHE.lock().unwrap_or_else(|error| error.into_inner()) = Some(Cache {
        path: path.to_owned(),
        checked: Instant::now(),
        policy: policy.clone(),
    });
    Ok(policy)
}

pub fn processing_threads(default: usize) -> usize {
    let host = host::sample();
    let automatic = host.cpu_cores.saturating_sub(1).clamp(1, 8);
    let requested = current().cpu_threads.map_or(automatic, |n| n as usize);
    let pressure_cap = match host.pressure {
        Pressure::Normal => usize::MAX,
        Pressure::Busy => 2,
        Pressure::Critical => 1,
    };
    default.max(1).min(requested).min(pressure_cap)
}

pub fn buffer_limit() -> u64 {
    current().buffer_bytes.unwrap_or_else(|| {
        host::sample()
            .memory_total
            .map_or(512 * MIB, |total| (total / 4).clamp(512 * MIB, 2 * GIB))
    })
}

/// One bounded read for the desktop, CLI and remote status endpoint. Failure
/// of one meter does not replace the other meters with reassuring zeroes.
pub fn snapshot() -> Result<serde_json::Value> {
    let policy = load()?;
    let host = host::sample();
    let buffer_limit = buffer_limit();
    let buffer_usage = buffers::usage();
    let encode_error = |error: anyhow::Error| serde_json::json!({"error":error.to_string()});
    Ok(serde_json::json!({
        "policy": policy,
        "effective": {
            "cpu_threads": processing_threads(host.cpu_cores),
            "buffer_bytes": buffer_limit,
        },
        "host": host,
        "buffers": match buffer_usage {
            Ok(usage) => {
                let room = buffers::headroom(usage.used_bytes, buffer_limit, host);
                serde_json::json!({"used_bytes":usage.used_bytes,"allocations":usage.allocations,"headroom_bytes":room.effective_bytes,"budget_headroom_bytes":room.budget_bytes,"physical_headroom_bytes":room.physical_bytes})
            },
            Err(error) => encode_error(error),
        },
        "storage": crate::storage::quota_status().unwrap_or_else(encode_error),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simultaneous_field_edits_keep_both_limits_and_invalid_edits_keep_saved_policy() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-resource-edits-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        let path = root.join(FILE);
        let gate = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                gate.wait();
                update_at(&path, |policy| {
                    policy.cpu_threads = Some(2);
                    Ok(())
                })
                .unwrap();
            });
            scope.spawn(|| {
                gate.wait();
                update_at(&path, |policy| {
                    policy.buffer_bytes = Some(GIB);
                    Ok(())
                })
                .unwrap();
            });
        });
        let saved = load_path(&path).unwrap();
        assert_eq!(saved.cpu_threads, Some(2));
        assert_eq!(saved.buffer_bytes, Some(GIB));
        assert!(
            update_at(&path, |policy| {
                policy.cpu_threads = Some(0);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(load_path(&path).unwrap(), saved);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn budgets_are_positive_and_bounded_before_any_write() {
        for policy in [
            Policy {
                cpu_threads: Some(0),
                ..Policy::default()
            },
            Policy {
                cpu_threads: Some(65),
                ..Policy::default()
            },
            Policy {
                buffer_bytes: Some(1),
                ..Policy::default()
            },
        ] {
            assert!(policy.validate().is_err());
        }
        assert!(Policy::default().validate().is_ok());
        assert!(
            Policy {
                cpu_threads: Some(1),
                buffer_bytes: Some(512 * MIB)
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn missing_limits_are_automatic_and_bad_limits_do_not_become_valid() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-resource-policy-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(FILE);
        assert_eq!(load_path(&path).unwrap(), Policy::default());
        fs::write(&path, br#"{"cpu_threads":0}"#).unwrap();
        assert!(load_path(&path).is_err());
        fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(load_path(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
