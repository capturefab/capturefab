//! Cross-process admission for shared camera buffers. A lease remains counted
//! while either the coordinator or its camera worker holds the OS lock.
use super::host::{self, HostSample};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

const MIB: u64 = 1024 * 1024;
const INHERITED_LEASE: &str = "CAPTUREFAB_BUFFER_LEASE";
const LOCK_WAIT: Duration = Duration::from_millis(750);
const RECORD_LIMIT: u64 = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BufferUsage {
    pub used_bytes: u64,
    pub allocations: u32,
}
/// Capacity a new camera may reserve now. Unknown physical availability
/// leaves the budget as an upper bound rather than inventing free RAM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BufferHeadroom {
    pub budget_bytes: u64,
    pub physical_bytes: Option<u64>,
    pub effective_bytes: u64,
}
fn free_memory_floor(host: HostSample) -> u64 {
    host.memory_total
        .map_or(256 * MIB, |total| (total / 20).max(256 * MIB))
}
pub fn headroom(used_bytes: u64, limit: u64, host: HostSample) -> BufferHeadroom {
    let budget_bytes = limit.saturating_sub(used_bytes);
    let physical_bytes = host
        .memory_available
        .map(|available| available.saturating_sub(free_memory_floor(host)));
    BufferHeadroom {
        budget_bytes,
        physical_bytes,
        effective_bytes: physical_bytes.map_or(budget_bytes, |physical| physical.min(budget_bytes)),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    capacity_bytes: u64,
}

#[derive(Clone)]
pub struct BufferLease(Arc<Lease>);
struct Lease {
    file: Option<File>,
    path: PathBuf,
    capacity_bytes: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        // End this owner's lock explicitly: an unrelated fork can temporarily
        // retain a copy of the open-file description until exec. Closing only
        // our descriptor would leave its flock behind. A real worker opens an
        // independent description, so its own shared lock still keeps the
        // allocation counted after this last Arc owner releases it.
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
            drop(file);
        }
        let Some(root) = self.path.parent() else {
            return;
        };
        let Ok(_gate) = gate(root) else { return };
        let Ok(file) = open_existing(&self.path) else {
            return;
        };
        if file.try_lock_exclusive().is_ok() {
            let _ = fs::remove_file(&self.path);
        }
    }
}
impl BufferLease {
    pub(crate) fn capacity_bytes(&self) -> u64 {
        self.0.capacity_bytes
    }
    pub(crate) fn pass_to(&self, command: &mut Command) {
        command.env(INHERITED_LEASE, &self.0.path);
    }
}
pub(crate) fn clear_inheritance(command: &mut Command) {
    command.env_remove(INHERITED_LEASE);
}

fn directory() -> Result<PathBuf> {
    std::path::absolute(crate::ipc::runtime_dir().join("camera-buffers"))
        .context("resolve camera-buffer directory")
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn open_existing(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Camera buffer reservation is not a regular file."
    );
    Ok(options().open(path)?)
}
fn contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}
fn gate(root: &Path) -> Result<File> {
    let file = options()
        .create(true)
        .truncate(false)
        .open(root.join("admission.lock"))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if contended(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                return Err(error).context("Camera buffer manager is busy; try again shortly.");
            }
        }
    }
}
fn lease_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.strip_prefix("lease-")
                .and_then(|name| name.strip_suffix(".json"))
                .is_some_and(|nonce| {
                    nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        })
}
fn read_record(file: &mut File) -> Result<Record> {
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= RECORD_LIMIT,
        "Camera buffer reservation is too large."
    );
    let record: Record =
        serde_json::from_slice(&bytes).context("Camera buffer reservation is unreadable.")?;
    ensure!(
        record.version == 1 && record.capacity_bytes > 0,
        "Camera buffer reservation has an invalid size or version."
    );
    Ok(record)
}
/// Caller holds admission.lock, so a legitimate new lease cannot appear
/// halfway through counting. A crashed owner's released lock is the proof
/// needed to remove a stale record, regardless of PID reuse.
fn collect(root: &Path) -> Result<BufferUsage> {
    let mut usage = BufferUsage::default();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if !lease_name(&path) {
            continue;
        }
        let mut file = match open_existing(&path) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                fs::remove_file(&path)?;
            }
            Err(error) if contended(&error) => {
                let record = read_record(&mut file)?;
                usage.used_bytes = usage
                    .used_bytes
                    .checked_add(record.capacity_bytes)
                    .context("Camera buffer reservations overflowed.")?;
                usage.allocations = usage
                    .allocations
                    .checked_add(1)
                    .context("Too many camera buffer reservations.")?;
            }
            Err(error) => return Err(error).context("read camera buffer reservation lock"),
        }
    }
    Ok(usage)
}

pub fn usage() -> Result<BufferUsage> {
    usage_at(&directory()?)
}
fn usage_at(root: &Path) -> Result<BufferUsage> {
    if !root.try_exists()? {
        return Ok(BufferUsage::default());
    }
    let _gate = gate(root)?;
    collect(root)
}

/// Reserve the exact three-slot ring allocation before mapping its memory.
/// A policy reduction never evicts an existing camera; it stops new admission.
pub fn reserve(capacity_bytes: u64) -> Result<BufferLease> {
    reserve_at(
        &directory()?,
        capacity_bytes,
        super::buffer_limit(),
        host::sample(),
    )
}
fn reserve_at(
    root: &Path,
    capacity_bytes: u64,
    limit: u64,
    host: HostSample,
) -> Result<BufferLease> {
    ensure!(
        capacity_bytes > 0,
        "Camera buffer reservation must be positive."
    );
    crate::ipc::ensure_private_dir(root)?;
    let _gate = gate(root)?;
    let usage = collect(root)?;
    ensure!(
        usage
            .used_bytes
            .checked_add(capacity_bytes)
            .is_some_and(|total| total <= limit),
        "Camera buffer limit reached: {} reserved, {} needed, {} limit. Disconnect a camera or raise the camera-buffer budget.",
        crate::analytics::human_bytes(usage.used_bytes),
        crate::analytics::human_bytes(capacity_bytes),
        crate::analytics::human_bytes(limit)
    );
    let room = headroom(usage.used_bytes, limit, host);
    if let Some(available) = host.memory_available {
        let reserve = free_memory_floor(host);
        ensure!(
            room.physical_bytes
                .is_some_and(|physical| physical >= capacity_bytes),
            "Not enough available memory for camera buffers: {} available, {} needed, plus {} kept free. Close unused cameras or other applications.",
            crate::analytics::human_bytes(available),
            crate::analytics::human_bytes(capacity_bytes),
            crate::analytics::human_bytes(reserve)
        );
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("camera buffer reservation name: {error}"))?;
    let name = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = root.join(format!("lease-{name}.json"));
    let mut file = options().create_new(true).open(&path)?;
    let result = (|| -> Result<()> {
        // admission.lock prevents collectors from seeing this record until it
        // is fully written and its live-owner lock is held. Windows shared
        // byte-range locks prohibit writes, even through the owning handle.
        file.write_all(&serde_json::to_vec(&Record {
            version: 1,
            capacity_bytes,
        })?)?;
        file.flush()?;
        file.lock_shared()?;
        Ok(())
    })();
    if let Err(error) = result {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(BufferLease(Arc::new(Lease {
        file: Some(file),
        path,
        capacity_bytes,
    })))
}

/// The child's independent shared lock keeps this same allocation counted
/// even if its coordinator exits first. The initial connection handshake keeps
/// the parent lease alive until this join succeeds.
pub(crate) fn inherited() -> Result<Option<BufferLease>> {
    let Some(path) = std::env::var_os(INHERITED_LEASE) else {
        return Ok(None);
    };
    join_at(&directory()?, &PathBuf::from(path)).map(Some)
}
fn join_at(root: &Path, path: &Path) -> Result<BufferLease> {
    ensure!(
        path.parent() == Some(root) && lease_name(path),
        "Invalid inherited camera buffer reservation."
    );
    let _gate = gate(root)?;
    let mut file = open_existing(path)
        .context("Camera buffer reservation expired before its worker started.")?;
    file.try_lock_shared()
        .context("Camera buffer reservation is unavailable.")?;
    let record = read_record(&mut file)?;
    Ok(BufferLease(Arc::new(Lease {
        file: Some(file),
        path: path.to_owned(),
        capacity_bytes: record.capacity_bytes,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root(name: &str) -> PathBuf {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        std::env::temp_dir().join(format!(
            "capturefab-buffers-{name}-{}-{:016x}",
            std::process::id(),
            u64::from_ne_bytes(nonce)
        ))
    }
    fn plenty() -> HostSample {
        HostSample {
            memory_total: Some(8 << 30),
            memory_available: Some(6 << 30),
            ..Default::default()
        }
    }
    #[test]
    fn native_lock_contention_is_retryable() {
        let root = root("contention");
        crate::ipc::ensure_private_dir(&root).unwrap();
        let held = gate(&root).unwrap();
        let competing = options().open(root.join("admission.lock")).unwrap();
        assert!(contended(&competing.try_lock_exclusive().unwrap_err()));
        drop(competing);
        drop(held);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn aggregate_admission_and_last_owner_release_are_exact() {
        let root = root("accounting");
        let lease = reserve_at(&root, 600, 1000, plenty()).unwrap();
        let clone = lease.clone();
        assert_eq!(
            usage_at(&root).unwrap(),
            BufferUsage {
                used_bytes: 600,
                allocations: 1
            }
        );
        assert!(reserve_at(&root, 401, 1000, plenty()).is_err());
        drop(lease);
        assert_eq!(usage_at(&root).unwrap().used_bytes, 600);
        let child = join_at(&root, &clone.0.path).unwrap();
        drop(clone);
        assert_eq!(usage_at(&root).unwrap().used_bytes, 600);
        drop(child);
        assert_eq!(usage_at(&root).unwrap(), BufferUsage::default());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn an_unrelated_descriptor_copy_does_not_retain_a_released_allocation() {
        let root = root("inherited-description");
        let lease = reserve_at(&root, 600, 1000, plenty()).unwrap();
        // dup shares the same open-file description as fork inheritance, so
        // keeping this handle open deterministically models a child that has
        // not reached exec yet. It is not an independently joined owner.
        let inherited = lease.0.file.as_ref().unwrap().try_clone().unwrap();
        drop(lease);
        assert_eq!(usage_at(&root).unwrap(), BufferUsage::default());
        assert!(inherited.metadata().unwrap().is_file());
        drop(inherited);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn ring_mapping_counters_keep_the_reservation_alive() {
        let root = root("mapping");
        let bytes = crate::shared_memory::SharedRing::allocation_bytes(4096).unwrap();
        let lease = reserve_at(&root, bytes, bytes * 2, plenty()).unwrap();
        let mut ring = crate::shared_memory::SharedRing::create(&root, 4096).unwrap();
        ring.attach_buffer_lease(lease).unwrap();
        let counters = ring.counters();
        drop(ring);
        assert_eq!(usage_at(&root).unwrap().used_bytes, bytes);
        drop(counters);
        assert_eq!(usage_at(&root).unwrap().used_bytes, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ring_rejects_another_allocation_reservation() {
        let root = root("wrong-size");
        let lease = reserve_at(&root, 1, 1000, plenty()).unwrap();
        let mut ring = crate::shared_memory::SharedRing::create(&root, 4096).unwrap();
        assert!(ring.attach_buffer_lease(lease).is_err());
        assert_eq!(usage_at(&root).unwrap().used_bytes, 0);
        drop(ring);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn headroom_matches_physical_admission_and_keeps_unknowns_explicit() {
        let host = HostSample {
            memory_total: Some(1 << 30),
            memory_available: Some(300 * MIB),
            ..Default::default()
        };
        let room = headroom(64 * MIB, 512 * MIB, host);
        assert_eq!(room.budget_bytes, 448 * MIB);
        assert_eq!(room.physical_bytes, Some(44 * MIB));
        assert_eq!(room.effective_bytes, 44 * MIB);
        let root = root("headroom");
        let lease = reserve_at(&root, room.effective_bytes, 512 * MIB, host).unwrap();
        assert!(reserve_at(&root, room.effective_bytes + 1, 512 * MIB, host).is_err());
        drop(lease);
        fs::remove_dir_all(root).unwrap();
        assert_eq!(headroom(513 * MIB, 512 * MIB, host).effective_bytes, 0);
        assert_eq!(
            headroom(64, 512, HostSample::default()),
            BufferHeadroom {
                budget_bytes: 448,
                physical_bytes: None,
                effective_bytes: 448,
            }
        );
    }

    #[test]
    fn low_memory_refuses_before_creating_a_lease() {
        let root = root("pressure");
        let low = HostSample {
            memory_total: Some(1 << 30),
            memory_available: Some(300 * MIB),
            ..Default::default()
        };
        let error = reserve_at(&root, 64 * MIB, 512 * MIB, low)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("available memory"), "{error}");
        assert_eq!(usage_at(&root).unwrap(), BufferUsage::default());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unknown_memory_is_not_fabricated_and_budget_still_applies() {
        let root = root("unknown");
        let lease = reserve_at(&root, 50, 100, HostSample::default()).unwrap();
        assert!(reserve_at(&root, 51, 100, HostSample::default()).is_err());
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn child_fixture() {
        let Some(root) = std::env::var_os("CAPTUREFAB_BUFFER_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        match std::env::var("CAPTUREFAB_BUFFER_TEST_MODE")
            .unwrap()
            .as_str()
        {
            "crash" => {
                let lease = reserve_at(&root, 600, 1000, plenty()).unwrap();
                std::mem::forget(lease);
                std::process::exit(0);
            }
            "full" => assert!(reserve_at(&root, 401, 1000, plenty()).is_err()),
            "join" => {
                let path = PathBuf::from(std::env::var_os(INHERITED_LEASE).unwrap());
                let _lease = join_at(&root, &path).unwrap();
                fs::write(root.join("ready"), b"ready").unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !root.join("release").exists() {
                    assert!(
                        Instant::now() < deadline,
                        "parent did not release buffer fixture"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            _ => panic!("unknown fixture"),
        }
    }
    #[test]
    fn process_locks_enforce_aggregate_budget_and_crashes_release_it() {
        let root = root("process");
        let child = |mode| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "resources::buffers::tests::child_fixture",
                    "--nocapture",
                ])
                .env("CAPTUREFAB_BUFFER_TEST_ROOT", &root)
                .env("CAPTUREFAB_BUFFER_TEST_MODE", mode)
                .output()
                .unwrap()
        };
        let lease = reserve_at(&root, 600, 1000, plenty()).unwrap();
        let result = child("full");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(usage_at(&root).unwrap().used_bytes, 600);
        drop(lease);
        let result = child("crash");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(usage_at(&root).unwrap(), BufferUsage::default());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn worker_handoff_keeps_the_lease_after_the_coordinator_releases_it() {
        let root = root("handoff");
        let lease = reserve_at(&root, 600, 1000, plenty()).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "resources::buffers::tests::child_fixture",
                "--nocapture",
            ])
            .env("CAPTUREFAB_BUFFER_TEST_ROOT", &root)
            .env("CAPTUREFAB_BUFFER_TEST_MODE", "join");
        lease.pass_to(&mut command);
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("ready").exists() {
            assert!(
                Instant::now() < deadline,
                "worker did not join buffer reservation"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(lease);
        assert_eq!(usage_at(&root).unwrap().used_bytes, 600);
        fs::write(root.join("release"), b"release").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(usage_at(&root).unwrap().used_bytes, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_live_reservation_fails_closed() {
        let root = root("corrupt");
        crate::ipc::ensure_private_dir(&root).unwrap();
        let lease = {
            // A synthetic corrupted record is locked after writing: Windows
            // correctly prohibits modifying a live shared-locked record.
            let _gate = gate(&root).unwrap();
            let path = root.join("lease-00000000000000000000000000000000.json");
            let mut file = options().create_new(true).open(&path).unwrap();
            file.write_all(b"bad").unwrap();
            file.flush().unwrap();
            file.lock_shared().unwrap();
            BufferLease(Arc::new(Lease {
                file: Some(file),
                path,
                capacity_bytes: 600,
            }))
        };
        assert!(usage_at(&root).is_err());
        assert!(reserve_at(&root, 1, 1000, plenty()).is_err());
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
}
