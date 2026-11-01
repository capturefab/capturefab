//! What this host lets Capturefab use: CPU cores and memory as the
//! scheduler and cgroups (containers, systemd units such as
//! `systemd-run -p MemoryMax=128M`) limit them, so thread pools and frame
//! queues scale from a 128 MiB board to a many-core server busy with other
//! work.
#![allow(unsafe_code)]
use serde_json::{Value, json};
use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Overrides the processing thread limit, for example `CAPTUREFAB_THREADS=1`
/// on a computer that also runs inference.
pub const THREADS_ENV: &str = "CAPTUREFAB_THREADS";

/// The CPUs this process may run on: its affinity mask and cgroup CPU
/// quota, as the standard library reads them on Linux. At least one.
pub fn cpus() -> usize {
    static CPUS: OnceLock<usize> = OnceLock::new();
    *CPUS.get_or_init(|| std::thread::available_parallelism().map_or(1, |cpus| cpus.get()))
}

/// Threads for CPU-bound processing: `wanted`, at most the CPUs this process
/// may use (up to eight), or at most `CAPTUREFAB_THREADS` when that is set.
pub fn threads(wanted: usize) -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    let limit =
        *LIMIT.get_or_init(|| thread_limit(std::env::var(THREADS_ENV).ok().as_deref(), cpus()));
    wanted.clamp(1, limit)
}

fn thread_limit(setting: Option<&str>, cores: usize) -> usize {
    setting
        .and_then(|setting| setting.trim().parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .map_or(cores.clamp(1, 8), |threads| threads.min(256))
}

/// What the host and the cgroups this process is in say about memory, in
/// bytes; None where they say nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Memory {
    /// The host's memory.
    pub total: Option<u64>,
    /// What the host can still give without swapping: Linux's MemAvailable,
    /// macOS's memory status level.
    pub available: Option<u64>,
    /// The tightest cgroup memory limit, and what that cgroup uses of it
    /// now, less the page cache it can drop.
    pub limit: Option<u64>,
    pub used: Option<u64>,
    /// The room left under every cgroup limit on the way to the root.
    pub room: Option<u64>,
}
impl Memory {
    /// The bytes this process may still allocate without pressing the host
    /// or its cgroup: the least of what the host has available and the room
    /// under its cgroups.
    pub fn budget(&self) -> Option<u64> {
        match (self.available, self.room) {
            (Some(available), Some(room)) => Some(available.min(room)),
            (available, room) => available.or(room),
        }
    }
}

/// The memory this process may still use, as `Memory::budget`, read at most
/// once a second: queues size themselves by it as frames arrive.
pub fn memory_budget() -> Option<u64> {
    memory().budget()
}

/// What the host and cgroups say about memory now, read at most once a
/// second.
pub fn memory() -> Memory {
    static READ: Mutex<Option<(Instant, Memory)>> = Mutex::new(None);
    let mut read = READ.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, memory)) = *read
        && at.elapsed() < Duration::from_secs(1)
    {
        return memory;
    }
    let memory = platform::memory();
    *read = Some((Instant::now(), memory));
    memory
}

/// The cgroup CPU quota, in CPUs, when one is set (`cpu.max`).
pub fn cpu_quota() -> Option<f64> {
    platform::cpu_quota()
}

/// What doctor reports of the host.
pub fn report() -> Value {
    let memory = memory();
    json!({
        "cpus": cpus(),
        "threads": threads(usize::MAX),
        "cpu_quota": cpu_quota(),
        "memory": {
            "total_bytes": memory.total,
            "available_bytes": memory.available,
            "cgroup_limit_bytes": memory.limit,
            "cgroup_used_bytes": memory.used,
            "budget_bytes": memory.budget(),
        },
    })
}

#[cfg(target_os = "linux")]
mod platform {
    use super::Memory;
    use std::path::{Path, PathBuf};

    /// A cgroup limit this large is no limit (cgroup v1 writes one).
    const UNLIMITED: u64 = 1 << 60;

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
    fn number(path: &Path) -> Option<u64> {
        read(path)?.trim().parse().ok()
    }
    /// A `key value` line's value in a file such as memory.stat.
    fn field(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(char::is_whitespace)?;
            (name == key)
                .then(|| value.trim().trim_end_matches(" kB").trim().parse().ok())
                .flatten()
        })
    }

    pub(super) fn memory() -> Memory {
        let meminfo = read(Path::new("/proc/meminfo")).unwrap_or_default();
        let kib = |key: &str| field(&meminfo, key).map(|kib| kib.saturating_mul(1024));
        let mut memory = Memory {
            total: kib("MemTotal:"),
            available: kib("MemAvailable:"),
            ..Memory::default()
        };
        let cgroups = read(Path::new("/proc/self/cgroup")).unwrap_or_default();
        for (limit, used) in limits(&cgroups) {
            let room = limit.saturating_sub(used);
            if memory.room.is_none_or(|least| room < least) {
                memory.room = Some(room);
                memory.limit = Some(limit);
                memory.used = Some(used);
            }
        }
        memory
    }

    /// Every cgroup memory limit over this process, with what its cgroup
    /// uses, from the leaf to the root.
    fn limits(cgroups: &str) -> Vec<(u64, u64)> {
        let mut found = Vec::new();
        // cgroup v2: one hierarchy, `0::/path`.
        if let Some(path) = cgroups.lines().find_map(|line| line.strip_prefix("0::")) {
            let root = Path::new("/sys/fs/cgroup");
            for dir in hierarchy(root, path) {
                let Some(text) = read(&dir.join("memory.max")) else {
                    continue;
                };
                let Ok(limit) = text.trim().parse::<u64>() else {
                    continue;
                };
                let current = number(&dir.join("memory.current")).unwrap_or(0);
                let cache = read(&dir.join("memory.stat"))
                    .and_then(|stat| field(&stat, "inactive_file"))
                    .unwrap_or(0);
                found.push((limit, current.saturating_sub(cache)));
            }
        }
        // cgroup v1: the memory controller's own hierarchy.
        if let Some(path) = cgroups.lines().find_map(|line| {
            let mut parts = line.splitn(3, ':');
            let (_, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
            controllers
                .split(',')
                .any(|c| c == "memory")
                .then_some(path)
        }) {
            let root = Path::new("/sys/fs/cgroup/memory");
            for dir in hierarchy(root, path) {
                let Some(limit) = number(&dir.join("memory.limit_in_bytes")) else {
                    continue;
                };
                if limit >= UNLIMITED {
                    continue;
                }
                let usage = number(&dir.join("memory.usage_in_bytes")).unwrap_or(0);
                let cache = read(&dir.join("memory.stat"))
                    .and_then(|stat| field(&stat, "total_inactive_file"))
                    .unwrap_or(0);
                found.push((limit, usage.saturating_sub(cache)));
            }
        }
        found
    }

    /// The folders of cgroup `path` under `root`, from the leaf up. In a
    /// container's cgroup namespace `path` may not exist under `root`,
    /// whose own files are then the container's.
    fn hierarchy(root: &Path, path: &str) -> Vec<PathBuf> {
        let leaf = root.join(path.trim_start_matches('/'));
        if !leaf.starts_with(root) || !leaf.is_dir() {
            return vec![root.to_path_buf()];
        }
        leaf.ancestors()
            .take_while(|dir| dir.starts_with(root))
            .map(Path::to_path_buf)
            .collect()
    }

    pub(super) fn cpu_quota() -> Option<f64> {
        let cgroups = read(Path::new("/proc/self/cgroup"))?;
        let path = cgroups.lines().find_map(|line| line.strip_prefix("0::"))?;
        let root = Path::new("/sys/fs/cgroup");
        hierarchy(root, path)
            .iter()
            .filter_map(|dir| {
                let text = read(&dir.join("cpu.max"))?;
                let mut parts = text.split_whitespace();
                let quota = parts.next()?.parse::<f64>().ok()?;
                let period = parts.next()?.parse::<f64>().ok()?;
                (period > 0.0).then_some(quota / period)
            })
            .reduce(f64::min)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fields_read_meminfo_and_memory_stat() {
            let meminfo =
                "MemTotal:        8112340 kB\nMemFree:  1 kB\nMemAvailable:    7683072 kB\n";
            assert_eq!(field(meminfo, "MemAvailable:"), Some(7_683_072));
            assert_eq!(field(meminfo, "MemTotal:"), Some(8_112_340));
            assert_eq!(field(meminfo, "Missing:"), None);
            let stat = "anon 1000\nfile 5000\ninactive_file 4096\nactive_file 904\n";
            assert_eq!(field(stat, "inactive_file"), Some(4096));
            assert_eq!(field(stat, "file"), Some(5000));
        }

        #[test]
        fn this_process_reads_its_own_cgroups() {
            // Whatever this host has; it must read without failing.
            let memory = memory();
            assert!(memory.total.is_some() && memory.available.is_some());
            if let (Some(limit), Some(used)) = (memory.limit, memory.used) {
                assert!(limit > 0 && memory.room == Some(limit.saturating_sub(used)));
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Memory;

    fn sysctl(name: &str) -> Option<u64> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut value = 0u64;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: a NUL-terminated name, and a buffer of `size` bytes the
        // call fills; the size it reports back says how much it wrote.
        let status = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut value).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (status == 0 && size <= std::mem::size_of::<u64>()).then_some(value)
    }

    /// macOS reports how much of memory is available as a percentage
    /// (`kern.memorystatus_level`), which is what its memory pressure uses.
    pub(super) fn memory() -> Memory {
        let total = sysctl("hw.memsize");
        // A 32-bit value, read into the low bytes.
        let level = sysctl("kern.memorystatus_level").map(|level| level & 0xffff_ffff);
        Memory {
            total,
            available: total
                .zip(level)
                .map(|(total, level)| total / 100 * level.min(100)),
            ..Memory::default()
        }
    }

    pub(super) fn cpu_quota() -> Option<f64> {
        None
    }
}

#[cfg(windows)]
mod platform {
    use super::Memory;

    pub(super) fn memory() -> Memory {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: MEMORYSTATUSEX is plain data; the call requires dwLength
        // set to its size and fills the rest.
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `status` is a valid MEMORYSTATUSEX with dwLength set.
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return Memory::default();
        }
        Memory {
            total: Some(status.ullTotalPhys),
            available: Some(status.ullAvailPhys),
            ..Memory::default()
        }
    }

    pub(super) fn cpu_quota() -> Option<f64> {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::Memory;

    pub(super) fn memory() -> Memory {
        Memory::default()
    }

    pub(super) fn cpu_quota() -> Option<f64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thread_limit_is_the_cores_up_to_eight_or_the_setting() {
        assert_eq!(thread_limit(None, 1), 1);
        assert_eq!(thread_limit(None, 4), 4);
        assert_eq!(thread_limit(None, 64), 8);
        assert_eq!(thread_limit(Some("2"), 64), 2);
        assert_eq!(thread_limit(Some(" 12 "), 4), 12);
        for ignored in ["0", "-1", "many", ""] {
            assert_eq!(thread_limit(Some(ignored), 4), 4, "{ignored}");
        }
        assert_eq!(thread_limit(Some("100000"), 4), 256);
        assert_eq!(threads(0), 1);
        assert!(threads(usize::MAX) <= 256);
        assert!(cpus() >= 1);
    }

    #[test]
    fn the_budget_is_the_least_of_host_and_cgroup() {
        let host = Memory {
            available: Some(6 << 30),
            ..Memory::default()
        };
        assert_eq!(host.budget(), Some(6 << 30));
        let limited = Memory {
            room: Some(100 << 20),
            ..host
        };
        assert_eq!(limited.budget(), Some(100 << 20));
        assert_eq!(Memory::default().budget(), None);
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            assert!(memory_budget().is_some_and(|budget| budget > 0));
        }
    }
}
