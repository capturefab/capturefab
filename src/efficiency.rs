//! Efficiency mode: one setting that trades preview smoothness for CPU,
//! memory and power on small computers. Captures and recordings always keep
//! every frame; only display and polling work may be reduced.
//!
//! The setting comes from, in order of precedence: the global `--efficiency`
//! flag, the `CAPTUREFAB_EFFICIENCY` environment variable, the desktop
//! preference ([`set`]), then `auto`. Auto is efficient on a computer with at
//! most two CPU cores or 2 GiB of memory, in the operating system's low-power
//! or battery-saver mode, or while the desktop reports a runtime [`hint`] such
//! as software rendering. Running on battery alone does not count: someone
//! focusing a camera on a laptop still wants a smooth preview.
//!
//! Host facts are read once per process, and [`efficient`] is an atomic read
//! cheap enough to call for every frame.
#![allow(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    sync::{
        Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicU8, Ordering},
    },
};

pub const ENV: &str = "CAPTUREFAB_EFFICIENCY";
/// Auto is efficient at or below this much memory: a 2 GB board, not the
/// 4 GB Pi 5s and laptops that preview smoothly.
const SMALL_MEMORY: u64 = 2 << 30;
const SMALL_CPU: usize = 2;

/// Unknown values, such as a choice from a newer version, fall back to auto.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Smooth,
    Efficient,
    #[default]
    #[serde(other)]
    Auto,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Smooth, Self::Efficient];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Smooth => "smooth",
            Self::Efficient => "efficient",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| value.trim().eq_ignore_ascii_case(mode.name()))
            .ok_or_else(|| "efficiency mode must be auto, smooth or efficient".into())
    }
}

/// What this computer offers, read once per process.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HostFacts {
    /// Cores this process may use, after affinity and cgroup quotas.
    pub cpu_cores: usize,
    pub memory_total: Option<u64>,
    pub memory_available: Option<u64>,
    /// Reported only; it never makes auto efficient by itself.
    pub on_battery: bool,
    /// macOS Low Power Mode, Windows battery saver or the Linux
    /// `low-power` platform profile.
    pub low_power: bool,
}

pub fn host() -> &'static HostFacts {
    static HOST: OnceLock<HostFacts> = OnceLock::new();
    HOST.get_or_init(|| {
        let (memory_total, memory_available) = memory();
        let (on_battery, low_power) = power();
        HostFacts {
            cpu_cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
            memory_total,
            memory_available,
            on_battery,
            low_power,
        }
    })
}

/// The setting in force and whether it means efficient right now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Resolved {
    pub setting: Mode,
    pub efficient: bool,
    /// Why: the source of an explicit choice, or what auto found.
    pub reasons: Vec<String>,
}

struct Overrides {
    flag: Option<Mode>,
    preference: Option<Mode>,
    hints: Vec<&'static str>,
}
const UNRESOLVED: u8 = 0;
const SMOOTH: u8 = 1;
const EFFICIENT: u8 = 2;

/// The overrides, and their resolution published for lock-free reads.
struct State {
    overrides: Mutex<Overrides>,
    published: AtomicU8,
}
impl State {
    const fn new() -> Self {
        Self {
            overrides: Mutex::new(Overrides {
                flag: None,
                preference: None,
                hints: Vec::new(),
            }),
            published: AtomicU8::new(UNRESOLVED),
        }
    }
    fn overrides(&self) -> MutexGuard<'_, Overrides> {
        self.overrides
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
    /// Changes only mark the state stale: reading host facts the first time
    /// is not free, and most commands never ask.
    fn update(&self, change: impl FnOnce(&mut Overrides)) {
        let mut overrides = self.overrides();
        change(&mut overrides);
        self.published.store(UNRESOLVED, Ordering::Relaxed);
    }
    fn current(&self, environment: Option<Mode>, host: &HostFacts) -> Resolved {
        // Resolved and published under the lock, so a change cannot be lost.
        let overrides = self.overrides();
        let (setting, source) = choose(overrides.flag, environment, overrides.preference);
        let resolved = resolve(setting, source.as_deref(), host, &overrides.hints);
        let state = if resolved.efficient {
            EFFICIENT
        } else {
            SMOOTH
        };
        self.published.store(state, Ordering::Relaxed);
        resolved
    }
    fn efficient(&self, resolve: impl FnOnce() -> Resolved) -> bool {
        match self.published.load(Ordering::Relaxed) {
            UNRESOLVED => resolve().efficient,
            state => state == EFFICIENT,
        }
    }
}
static STATE: State = State::new();

pub fn current() -> Resolved {
    STATE.current(environment(), host())
}

/// Whether to save work now; cheap enough for every frame.
pub fn efficient() -> bool {
    #[cfg(test)]
    if let Some(efficient) = PINNED.get() {
        return efficient;
    }
    STATE.efficient(current)
}

#[cfg(test)]
thread_local! {
    static PINNED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Make `efficient` answer the same for this test's thread whatever the
/// host is, such as a two-core runner or a laptop in Low Power Mode.
#[cfg(test)]
pub(crate) fn pin(efficient: bool) {
    PINNED.set(Some(efficient));
}

/// The `--efficiency` flag of this process, if given.
pub fn configure(flag: Option<Mode>) {
    STATE.update(|overrides| overrides.flag = flag);
}

/// The desktop preference; the flag and environment variable still win.
pub fn set(mode: Mode) {
    STATE.update(|overrides| overrides.preference = Some(mode));
}

/// A runtime signal, such as software rendering, that makes auto efficient
/// until cleared.
pub fn hint(reason: &'static str) {
    STATE.update(|overrides| {
        if !overrides.hints.contains(&reason) {
            overrides.hints.push(reason);
        }
    });
}

pub fn clear_hint(reason: &'static str) {
    STATE.update(|overrides| overrides.hints.retain(|hint| *hint != reason));
}

/// The setting to give camera workers this process starts, so they resolve
/// to the same answer: efficient whenever it is efficient here, including
/// for a flag, desktop preference or hint a worker cannot see.
pub fn for_workers() -> Mode {
    worker_mode(&current())
}
fn worker_mode(resolved: &Resolved) -> Mode {
    if resolved.efficient {
        Mode::Efficient
    } else {
        resolved.setting
    }
}

fn environment() -> Option<Mode> {
    static ENVIRONMENT: OnceLock<Option<Mode>> = OnceLock::new();
    *ENVIRONMENT.get_or_init(|| parse_environment(std::env::var_os(ENV)))
}
/// Unset or empty means no choice; an unknown value means auto rather than
/// failing every command started with it.
fn parse_environment(value: Option<OsString>) -> Option<Mode> {
    let value = value?;
    let value = value.to_string_lossy();
    (!value.trim().is_empty()).then(|| value.parse().unwrap_or_default())
}

/// The first explicit choice in precedence order, and where it came from.
fn choose(
    flag: Option<Mode>,
    environment: Option<Mode>,
    preference: Option<Mode>,
) -> (Mode, Option<String>) {
    if let Some(mode) = flag {
        (mode, Some(format!("set by --efficiency {mode}")))
    } else if let Some(mode) = environment {
        (mode, Some(format!("set by {ENV}={mode}")))
    } else if let Some(mode) = preference {
        (mode, Some("set in the desktop settings".into()))
    } else {
        (Mode::Auto, None)
    }
}

fn resolve(setting: Mode, source: Option<&str>, host: &HostFacts, hints: &[&str]) -> Resolved {
    let (efficient, reasons) = match setting {
        Mode::Auto => {
            let mut reasons = constraints(host);
            reasons.extend(hints.iter().map(|hint| hint.to_string()));
            (!reasons.is_empty(), reasons)
        }
        explicit => (
            explicit == Mode::Efficient,
            source.map(str::to_owned).into_iter().collect(),
        ),
    };
    Resolved {
        setting,
        efficient,
        reasons,
    }
}

/// What makes auto choose efficient on this host.
fn constraints(host: &HostFacts) -> Vec<String> {
    let mut reasons = Vec::new();
    if host.cpu_cores <= SMALL_CPU {
        let plural = if host.cpu_cores == 1 { "" } else { "s" };
        reasons.push(format!("{} CPU core{plural}", host.cpu_cores));
    }
    if let Some(total) = host.memory_total.filter(|total| *total <= SMALL_MEMORY) {
        reasons.push(format!(
            "{} of memory",
            crate::analytics::human_bytes(total)
        ));
    }
    if host.low_power {
        reasons.push(LOW_POWER.into());
    }
    reasons
}

#[cfg(target_os = "macos")]
const LOW_POWER: &str = "Low Power Mode is on";
#[cfg(windows)]
const LOW_POWER: &str = "battery saver is on";
#[cfg(target_os = "linux")]
const LOW_POWER: &str = "the power profile is low-power";
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
const LOW_POWER: &str = "low-power mode is on";

#[cfg(any(target_os = "linux", test))]
fn meminfo(text: &str, key: &str) -> Option<u64> {
    let kib = text.lines().find_map(|line| line.strip_prefix(key))?;
    Some(kib.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()? * 1024)
}

#[cfg(target_os = "linux")]
fn memory() -> (Option<u64>, Option<u64>) {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    (meminfo(&text, "MemTotal:"), meminfo(&text, "MemAvailable:"))
}

/// A battery other than a peripheral's that is discharging, and the ACPI
/// platform profile that power-profiles-daemon sets for power saving.
#[cfg(target_os = "linux")]
fn power() -> (bool, bool) {
    let read = |path: std::path::PathBuf| std::fs::read_to_string(path).unwrap_or_default();
    let on_battery = std::fs::read_dir("/sys/class/power_supply")
        .into_iter()
        .flatten()
        .flatten()
        .any(|supply| {
            let path = supply.path();
            read(path.join("type")).trim() == "Battery"
                && read(path.join("scope")).trim() != "Device"
                && read(path.join("status")).trim() == "Discharging"
        });
    let low_power = read("/sys/firmware/acpi/platform_profile".into()).trim() == "low-power";
    (on_battery, low_power)
}

#[cfg(target_os = "macos")]
fn sysctl<T: Copy + Default>(name: &std::ffi::CStr) -> Option<T> {
    let mut value = T::default();
    let mut size = size_of::<T>();
    // SAFETY: `value` is a plain integer of `size` bytes and `name` is
    // NUL-terminated; nothing is written to the kernel.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0 && size == size_of::<T>()).then_some(value)
}

/// Available memory is the free percentage behind macOS memory pressure.
#[cfg(target_os = "macos")]
fn memory() -> (Option<u64>, Option<u64>) {
    let total = sysctl::<u64>(c"hw.memsize");
    let free_percent = sysctl::<i32>(c"kern.memorystatus_level")
        .and_then(|level| u64::try_from(level).ok())
        .filter(|level| *level <= 100);
    let available = total
        .zip(free_percent)
        .map(|(total, percent)| total / 100 * percent);
    (total, available)
}

#[cfg(target_os = "macos")]
mod power_sources {
    use std::ffi::{CStr, c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFStringRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(object: CFTypeRef);
    }

    /// Whether the Mac draws from its battery rather than AC or a UPS.
    pub fn on_battery() -> bool {
        let mut name = [0 as c_char; 32];
        // SAFETY: the snapshot is released after use; the providing type is
        // a constant string owned by IOKit, copied into a bounded buffer.
        unsafe {
            let snapshot = IOPSCopyPowerSourcesInfo();
            if snapshot.is_null() {
                return false;
            }
            let source = IOPSGetProvidingPowerSourceType(snapshot);
            let copied = !source.is_null()
                && CFStringGetCString(source, name.as_mut_ptr(), name.len() as isize, UTF8) != 0;
            CFRelease(snapshot);
            copied && CStr::from_ptr(name.as_ptr()) == c"Battery Power"
        }
    }
}

#[cfg(target_os = "macos")]
fn power() -> (bool, bool) {
    (power_sources::on_battery(), low_power_mode())
}

/// NSProcessInfo knows; it has no answer before macOS 12.
#[cfg(all(target_os = "macos", feature = "gui"))]
fn low_power_mode() -> bool {
    use objc2::{runtime::NSObjectProtocol, sel};
    let info = objc2_foundation::NSProcessInfo::processInfo();
    info.respondsToSelector(sel!(isLowPowerModeEnabled)) && info.isLowPowerModeEnabled()
}

/// Headless builds link no Foundation binding, so ask pmset once, briefly.
#[cfg(all(target_os = "macos", not(feature = "gui")))]
fn low_power_mode() -> bool {
    use std::{io::Read, process::Stdio, time::Duration, time::Instant};
    let Ok(mut child) = std::process::Command::new("/usr/bin/pmset")
        .arg("-g")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    // Its few hundred bytes fit in the pipe, so it can exit before we read.
    while child.try_wait().ok().flatten().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut text = String::new();
    let _ = child
        .stdout
        .take()
        .map(|mut out| out.read_to_string(&mut text));
    pmset_low_power(&text)
}

/// `lowpowermode 1`, or `powermode 1` on Macs that also offer High Power.
#[cfg(any(all(target_os = "macos", not(feature = "gui")), test))]
fn pmset_low_power(text: &str) -> bool {
    text.lines().any(|line| {
        let mut words = line.split_whitespace();
        matches!(words.next(), Some("lowpowermode" | "powermode")) && words.next() == Some("1")
    })
}

#[cfg(windows)]
fn memory() -> (Option<u64>, Option<u64>) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength is set as the API requires.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return (None, None);
    }
    (Some(status.ullTotalPhys), Some(status.ullAvailPhys))
}

#[cfg(windows)]
fn power() -> (bool, bool) {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: a plain output structure.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return (false, false);
    }
    // ACLineStatus 0 is offline; SystemStatusFlag 1 is battery saver.
    (status.ACLineStatus == 0, status.SystemStatusFlag == 1)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn memory() -> (Option<u64>, Option<u64>) {
    (None, None)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn power() -> (bool, bool) {
    (false, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(cpu_cores: usize, memory_gib: u64) -> HostFacts {
        HostFacts {
            cpu_cores,
            memory_total: Some(memory_gib << 30),
            memory_available: Some(memory_gib << 29),
            on_battery: false,
            low_power: false,
        }
    }
    fn auto(host: &HostFacts) -> Resolved {
        resolve(Mode::Auto, None, host, &[])
    }

    #[test]
    fn auto_is_efficient_on_small_computers_only() {
        let two_cores = auto(&host(2, 8));
        assert!(two_cores.efficient);
        assert_eq!(two_cores.reasons, ["2 CPU cores"]);
        assert!(auto(&host(1, 8)).efficient);
        let small = auto(&host(4, 2));
        assert!(small.efficient, "2 GiB counts");
        assert_eq!(small.reasons, ["2.00 GiB of memory"]);
        // A 4 GB Pi 5 or laptop on mains keeps smooth previews.
        let pi = auto(&host(4, 4));
        assert_eq!(
            pi,
            Resolved {
                setting: Mode::Auto,
                efficient: false,
                reasons: Vec::new()
            }
        );
        let unknown_memory = HostFacts {
            memory_total: None,
            ..host(4, 4)
        };
        assert!(!auto(&unknown_memory).efficient);
    }

    #[test]
    fn battery_alone_stays_smooth_but_low_power_mode_does_not() {
        let battery = HostFacts {
            on_battery: true,
            ..host(8, 16)
        };
        assert!(!auto(&battery).efficient);
        let saver = HostFacts {
            low_power: true,
            ..battery
        };
        let resolved = auto(&saver);
        assert!(resolved.efficient);
        assert_eq!(resolved.reasons, [LOW_POWER]);
    }

    #[test]
    fn hints_count_only_under_auto() {
        let big = host(8, 16);
        let hinted = resolve(Mode::Auto, None, &big, &["software rendering"]);
        assert!(hinted.efficient);
        assert_eq!(hinted.reasons, ["software rendering"]);
        let (setting, source) = choose(Some(Mode::Smooth), None, None);
        let chosen = resolve(
            setting,
            source.as_deref(),
            &host(2, 1),
            &["software rendering"],
        );
        assert!(!chosen.efficient);
        assert_eq!(chosen.reasons, ["set by --efficiency smooth"]);
    }

    #[test]
    fn flag_beats_environment_beats_preference_beats_auto() {
        use Mode::*;
        assert_eq!(choose(None, None, None), (Auto, None));
        assert_eq!(
            choose(None, None, Some(Efficient)).0,
            Efficient,
            "the desktop preference beats auto"
        );
        let (setting, source) = choose(None, Some(Smooth), Some(Efficient));
        assert_eq!(setting, Smooth);
        assert_eq!(source.unwrap(), "set by CAPTUREFAB_EFFICIENCY=smooth");
        let (setting, source) = choose(Some(Efficient), Some(Smooth), Some(Smooth));
        assert_eq!(setting, Efficient);
        assert_eq!(source.unwrap(), "set by --efficiency efficient");
        // An explicit auto still decides from the host.
        let (setting, _) = choose(None, Some(Auto), Some(Efficient));
        assert!(!resolve(setting, None, &host(8, 16), &[]).efficient);
        // The environment beats what auto would choose.
        let (setting, source) = choose(None, Some(Efficient), None);
        assert!(resolve(setting, source.as_deref(), &host(8, 16), &[]).efficient);
    }

    #[test]
    fn workers_resolve_to_the_same_answer() {
        let big = host(8, 16);
        let resolved = |setting, hints: &[&str]| resolve(setting, None, &big, hints);
        // A hint only this process sees makes auto efficient: the worker is
        // told efficient rather than auto, which it would resolve as smooth.
        let hinted = resolved(Mode::Auto, &["software rendering"]);
        assert_eq!(worker_mode(&hinted), Mode::Efficient);
        let worker = parse_environment(Some(worker_mode(&hinted).name().into())).unwrap();
        assert!(resolve(worker, None, &big, &[]).efficient);
        // Otherwise the worker gets the setting and decides from the same host.
        assert_eq!(worker_mode(&resolved(Mode::Auto, &[])), Mode::Auto);
        assert_eq!(worker_mode(&auto(&host(2, 8))), Mode::Efficient);
        assert_eq!(worker_mode(&resolved(Mode::Smooth, &[])), Mode::Smooth);
        assert_eq!(
            worker_mode(&resolved(Mode::Efficient, &[])),
            Mode::Efficient
        );
    }

    #[test]
    fn environment_and_preferences_fall_back_to_auto() {
        assert_eq!(parse_environment(None), None);
        assert_eq!(parse_environment(Some("".into())), None);
        assert_eq!(parse_environment(Some(" ".into())), None);
        assert_eq!(
            parse_environment(Some("Efficient".into())),
            Some(Mode::Efficient)
        );
        assert_eq!(parse_environment(Some("turbo".into())), Some(Mode::Auto));
        assert_eq!("smooth".parse(), Ok(Mode::Smooth));
        assert!("turbo".parse::<Mode>().is_err());
        for mode in Mode::ALL {
            assert_eq!(mode.to_string().parse(), Ok(mode));
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{mode}\""));
            assert_eq!(serde_json::from_str::<Mode>(&json).unwrap(), mode);
        }
        assert_eq!(
            serde_json::from_str::<Mode>("\"turbo\"").unwrap(),
            Mode::Auto
        );
    }

    #[test]
    fn changes_republish_the_resolved_state() {
        let state = State::new();
        let big = host(8, 16);
        let resolve = || state.current(None, &big);
        assert!(!state.efficient(resolve));
        state.update(|overrides| overrides.hints.push("overload"));
        assert!(state.efficient(resolve), "a hint makes auto efficient");
        state.update(|overrides| overrides.preference = Some(Mode::Smooth));
        assert!(
            !state.efficient(resolve),
            "an explicit choice ignores hints"
        );
        state.update(|overrides| overrides.flag = Some(Mode::Efficient));
        assert!(state.efficient(resolve));
        // Published, so later reads need no resolution.
        assert!(state.efficient(|| unreachable!()));
        let resolved = state.current(Some(Mode::Smooth), &big);
        assert_eq!(resolved.reasons, ["set by --efficiency efficient"]);
    }

    #[test]
    fn host_facts_parse_platform_text() {
        let meminfo_text = "MemTotal:        3884096 kB\nMemFree:          112000 kB\nMemAvailable:    2048000 kB\n";
        assert_eq!(meminfo(meminfo_text, "MemTotal:"), Some(3_884_096 * 1024));
        assert_eq!(
            meminfo(meminfo_text, "MemAvailable:"),
            Some(2_048_000 * 1024)
        );
        assert_eq!(meminfo(meminfo_text, "SwapTotal:"), None);
        assert!(pmset_low_power(" sleep 1\n lowpowermode         1\n"));
        assert!(pmset_low_power(" powermode            1\n"));
        assert!(!pmset_low_power(" lowpowermode         0\n powermode 2\n"));
        let facts = super::host();
        assert!(facts.cpu_cores >= 1);
        assert!(facts.memory_total.is_some_and(|total| total > 0));
        assert!(facts.memory_available <= facts.memory_total);
    }
}
