//! Cheap live host measurements. Unknown readings stay unknown; pressure
//! recovery needs several healthy samples so safeguards do not chatter.
use serde::Serialize;
use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(1);
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    #[default]
    Normal,
    Busy,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct HostSample {
    /// Busy share of the whole machine, 0..100; not one process's CPU usage.
    pub cpu_percent: Option<f32>,
    pub cpu_cores: usize,
    pub memory_total: Option<u64>,
    pub memory_available: Option<u64>,
    pub pressure: Pressure,
}

impl HostSample {
    /// A shared explanation for automatic safeguards. A healthy recovery
    /// interval and missing measurements are distinct from measured overload.
    pub fn pressure_reason(&self) -> Option<&'static str> {
        if self.pressure == Pressure::Normal {
            return None;
        }
        if memory_pressure(self.memory_total, self.memory_available)
            .is_some_and(|pressure| pressure >= Pressure::Busy)
        {
            return Some("Available memory is low");
        }
        if self
            .cpu_percent
            .is_some_and(|cpu| cpu.is_finite() && cpu >= 85.0)
        {
            return Some("CPU is busy");
        }
        if self.cpu_percent.is_none_or(|cpu| !cpu.is_finite())
            || self.memory_total.is_none()
            || self.memory_available.is_none()
        {
            return Some("Checking available resources");
        }
        Some("Giving capture extra headroom")
    }
}

#[derive(Clone, Copy, Debug)]
struct CpuTicks {
    busy: u64,
    total: u64,
}
impl CpuTicks {
    fn percent_since(self, previous: Self) -> Option<f32> {
        let total = self.total.checked_sub(previous.total)?;
        let busy = self.busy.checked_sub(previous.busy)?;
        if total == 0 || busy > total {
            None
        } else {
            Some((100.0 * busy as f64 / total as f64) as f32)
        }
    }
}

#[derive(Default)]
struct Sampler {
    at: Option<Instant>,
    ticks: Option<CpuTicks>,
    sample: HostSample,
    recovered: u8,
}
impl Sampler {
    fn update(
        &mut self,
        at: Instant,
        ticks: Option<CpuTicks>,
        memory: (Option<u64>, Option<u64>),
        cpu_cores: usize,
    ) -> HostSample {
        let cpu_percent = ticks
            .zip(self.ticks)
            .and_then(|(now, old)| now.percent_since(old));
        self.ticks = ticks;
        let (memory_total, memory_available) = memory;
        let candidate = pressure(cpu_percent, memory_total, memory_available);
        let previous = self.sample.pressure;
        if candidate >= previous {
            self.sample.pressure = candidate;
            self.recovered = 0;
        } else if healthy_for_recovery(previous, cpu_percent, memory_total, memory_available) {
            self.recovered = self.recovered.saturating_add(1);
            if self.recovered >= 3 {
                self.sample.pressure = candidate;
                self.recovered = 0;
            }
        } else {
            self.recovered = 0;
        }
        self.at = Some(at);
        self.sample = HostSample {
            cpu_percent,
            cpu_cores: cpu_cores.max(1),
            memory_total,
            memory_available,
            pressure: self.sample.pressure,
        };
        self.sample
    }
}

/// Samples at most once a second even when several windows and camera threads
/// ask. The first CPU percentage is unavailable until a second reading exists.
pub fn sample() -> HostSample {
    static STATE: OnceLock<Mutex<Sampler>> = OnceLock::new();
    let mut state = STATE
        .get_or_init(|| Mutex::new(Sampler::default()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let now = Instant::now();
    if state
        .at
        .is_some_and(|at| now.saturating_duration_since(at) < INTERVAL)
    {
        return state.sample;
    }
    state.update(
        now,
        cpu_ticks(),
        memory(),
        std::thread::available_parallelism().map_or(1, |cores| cores.get()),
    )
}

fn memory_pressure(total: Option<u64>, available: Option<u64>) -> Option<Pressure> {
    let (total, available) = total.zip(available)?;
    Some(if available <= (total / 20).max(128 * MIB) {
        Pressure::Critical
    } else if available <= (total / 10).max(384 * MIB) {
        Pressure::Busy
    } else {
        Pressure::Normal
    })
}
fn pressure(cpu: Option<f32>, total: Option<u64>, available: Option<u64>) -> Pressure {
    let memory = memory_pressure(total, available);
    let cpu = cpu.filter(|value| value.is_finite()).map(|value| {
        if value >= 95.0 {
            Pressure::Critical
        } else if value >= 85.0 {
            Pressure::Busy
        } else {
            Pressure::Normal
        }
    });
    match (cpu, memory) {
        (Some(cpu), Some(memory)) => cpu.max(memory),
        (cpu, memory) => cpu
            .into_iter()
            .chain(memory)
            .max()
            .unwrap_or(Pressure::Busy)
            .max(Pressure::Busy),
    }
}
fn healthy_for_recovery(
    from: Pressure,
    cpu: Option<f32>,
    total: Option<u64>,
    available: Option<u64>,
) -> bool {
    let Some((cpu, (total, available))) = cpu.zip(total.zip(available)) else {
        return false;
    };
    let (cpu_limit, memory_floor) = match from {
        Pressure::Critical => (88.0, (total.saturating_mul(8) / 100).max(192 * MIB)),
        Pressure::Busy | Pressure::Normal => {
            (70.0, (total.saturating_mul(15) / 100).max(512 * MIB))
        }
    };
    cpu.is_finite() && cpu < cpu_limit && available > memory_floor
}

fn memory() -> (Option<u64>, Option<u64>) {
    let memory = crate::efficiency::live_memory();
    #[cfg(target_os = "linux")]
    {
        constrained_memory(memory)
    }
    #[cfg(not(target_os = "linux"))]
    {
        memory
    }
}

#[cfg(target_os = "linux")]
fn constrained_memory(mut memory: (Option<u64>, Option<u64>)) -> (Option<u64>, Option<u64>) {
    use std::path::{Component, Path, PathBuf};
    let root = Path::new("/sys/fs/cgroup");
    let mut directories = vec![(root.to_owned(), true), (root.join("memory"), false)];
    if let Ok(groups) = std::fs::read_to_string("/proc/self/cgroup") {
        for line in groups.lines() {
            let mut parts = line.splitn(3, ':');
            let (Some(hierarchy), Some(controllers), Some(group)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let unified = hierarchy == "0" && controllers.is_empty();
            if !unified && !controllers.split(',').any(|name| name == "memory") {
                continue;
            }
            let relative = Path::new(group.trim_start_matches('/'));
            if relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
            {
                continue;
            }
            let base = if unified {
                root.to_owned()
            } else {
                root.join("memory")
            };
            let mut directory: PathBuf = base.join(relative);
            for _ in 0..32 {
                directories.push((directory.clone(), unified));
                if directory == base || !directory.pop() {
                    break;
                }
            }
        }
    }
    for (directory, unified) in directories {
        let names = if unified {
            ("memory.max", "memory.current")
        } else {
            ("memory.limit_in_bytes", "memory.usage_in_bytes")
        };
        let limit = std::fs::read_to_string(directory.join(names.0)).ok();
        let used = std::fs::read_to_string(directory.join(names.1)).ok();
        memory = apply_memory_constraint(memory, limit.as_deref(), used.as_deref());
    }
    memory
}
#[cfg(any(target_os = "linux", test))]
fn apply_memory_constraint(
    (total, available): (Option<u64>, Option<u64>),
    limit: Option<&str>,
    used: Option<&str>,
) -> (Option<u64>, Option<u64>) {
    let limit = limit
        .and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|limit| *limit > 0 && *limit < (1 << 60));
    let Some(limit) = limit else {
        return (total, available);
    };
    let total = Some(total.map_or(limit, |total| total.min(limit)));
    let remaining = used
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(|used| limit.saturating_sub(used));
    let available = match (available, remaining) {
        (Some(available), Some(remaining)) => Some(available.min(remaining)),
        (_, Some(remaining)) => Some(remaining),
        (available, None) => available.map(|available| available.min(limit)),
    };
    (total, available)
}

#[cfg(target_os = "linux")]
fn cpu_ticks() -> Option<CpuTicks> {
    linux_cpu(&std::fs::read_to_string("/proc/stat").ok()?)
}
#[cfg(any(target_os = "linux", test))]
fn linux_cpu(text: &str) -> Option<CpuTicks> {
    let mut fields = text
        .lines()
        .find(|line| line.starts_with("cpu "))?
        .split_whitespace();
    fields.next()?;
    let values = fields
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    let total = values
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))?;
    let idle = values[3].checked_add(values.get(4).copied().unwrap_or(0))?;
    Some(CpuTicks {
        busy: total.checked_sub(idle)?,
        total,
    })
}

#[cfg(target_os = "macos")]
fn cpu_ticks() -> Option<CpuTicks> {
    // mach/host_info.h: four natural_t counters, returned as integer_t words.
    // Declare the stable system ABI directly: libc deprecates its Mach wrappers.
    #[repr(C)]
    struct CpuLoadInfo {
        ticks: [u32; 4],
    }
    unsafe extern "C" {
        fn mach_host_self() -> u32;
        static mach_task_self_: u32;
        fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
    }
    const HOST_CPU_LOAD_INFO: i32 = 3;
    const COUNT: u32 = (size_of::<CpuLoadInfo>() / size_of::<i32>()) as u32;
    let mut info = CpuLoadInfo { ticks: [0; 4] };
    let mut count = COUNT;
    // SAFETY: host_statistics receives a correctly sized writable C structure;
    // mach_task_self_ is the process's initialized task port. Release the host
    // port right after use, including when sampling fails.
    let status = unsafe {
        let host = mach_host_self();
        let status = host_statistics(
            host,
            HOST_CPU_LOAD_INFO,
            (&raw mut info).cast(),
            &raw mut count,
        );
        mach_port_deallocate(mach_task_self_, host);
        status
    };
    if status != 0 || count != COUNT {
        return None;
    }
    let total = info
        .ticks
        .iter()
        .map(|value| u64::from(*value))
        .sum::<u64>();
    let idle = u64::from(info.ticks[2]);
    Some(CpuTicks {
        busy: total.checked_sub(idle)?,
        total,
    })
}

#[cfg(windows)]
fn cpu_ticks() -> Option<CpuTicks> {
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetSystemTimes};
    let mut idle: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: three independent FILETIME output values.
    if unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) } == 0 {
        return None;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let total = ticks(kernel).checked_add(ticks(user))?;
    Some(CpuTicks {
        busy: total.checked_sub(ticks(idle))?,
        total,
    })
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn cpu_ticks() -> Option<CpuTicks> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_deltas_exclude_idle_and_guest_and_refuse_resets() {
        let first = linux_cpu("cpu 10 5 10 70 5 0 0 0 500 500\ncpu0 0 0 0 0").unwrap();
        let next = linux_cpu("cpu 15 5 15 75 5 0 0 0 999 999").unwrap();
        assert!((next.percent_since(first).unwrap() - 66.66667).abs() < 0.001);
        assert!(first.percent_since(next).is_none());
        assert!(next.percent_since(next).is_none());
        assert!(linux_cpu("cpu broken").is_none());
    }
    #[test]
    fn host_pressure_rises_quickly_but_needs_three_healthy_readings_to_recover() {
        let mut sampler = Sampler::default();
        let now = Instant::now();
        let memory = (Some(8 << 30), Some(6 << 30));
        assert_eq!(
            sampler
                .update(
                    now,
                    Some(CpuTicks {
                        busy: 0,
                        total: 100
                    }),
                    memory,
                    8
                )
                .pressure,
            Pressure::Busy
        );
        assert_eq!(
            sampler
                .update(
                    now,
                    Some(CpuTicks {
                        busy: 99,
                        total: 200
                    }),
                    memory,
                    8
                )
                .pressure,
            Pressure::Critical
        );
        for step in 1..=2 {
            assert_eq!(
                sampler
                    .update(
                        now,
                        Some(CpuTicks {
                            busy: 99,
                            total: 200 + step * 100
                        }),
                        memory,
                        8
                    )
                    .pressure,
                Pressure::Critical
            );
        }
        assert_eq!(
            sampler
                .update(
                    now,
                    Some(CpuTicks {
                        busy: 99,
                        total: 500
                    }),
                    memory,
                    8
                )
                .pressure,
            Pressure::Normal
        );
    }
    #[test]
    fn missing_measurements_are_unconfirmed_and_low_memory_overrides_idle_cpu() {
        assert_eq!(pressure(None, None, None), Pressure::Busy);
        assert_eq!(
            pressure(Some(0.0), Some(8 << 30), Some(64 * MIB)),
            Pressure::Critical
        );
        assert!(!healthy_for_recovery(
            Pressure::Critical,
            None,
            Some(8 << 30),
            Some(6 << 30)
        ));
    }
    #[test]
    fn safeguard_reasons_use_the_same_thresholds_as_the_sampler() {
        let mut sample = HostSample {
            cpu_percent: Some(20.0),
            memory_total: Some(8 << 30),
            memory_available: Some(6 << 30),
            pressure: Pressure::Normal,
            cpu_cores: 8,
        };
        assert_eq!(sample.pressure_reason(), None);
        sample.pressure = Pressure::Busy;
        assert_eq!(
            sample.pressure_reason(),
            Some("Giving capture extra headroom")
        );
        sample.cpu_percent = None;
        assert_eq!(
            sample.pressure_reason(),
            Some("Checking available resources")
        );
        sample.cpu_percent = Some(85.0);
        assert_eq!(sample.pressure_reason(), Some("CPU is busy"));
        sample.memory_available = Some(256 * MIB);
        assert_eq!(sample.pressure_reason(), Some("Available memory is low"));
    }

    #[test]
    fn cgroup_limits_bound_host_memory_and_unknown_usage_never_invents_headroom() {
        assert_eq!(
            apply_memory_constraint(
                (Some(16 << 30), Some(12 << 30)),
                Some("1073741824"),
                Some("805306368")
            ),
            (Some(1 << 30), Some(256 * MIB))
        );
        assert_eq!(
            apply_memory_constraint((None, None), Some("1073741824"), None),
            (Some(1 << 30), None)
        );
        assert_eq!(
            apply_memory_constraint((Some(8 << 30), Some(6 << 30)), Some("max"), Some("1")),
            (Some(8 << 30), Some(6 << 30))
        );
    }
}
