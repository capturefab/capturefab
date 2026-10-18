//! Host checks for `doctor`: where sessions, state and temporary files live,
//! kernel limits that cap camera throughput, the storage budget against the
//! capture disk and the graphics adapter the desktop window would get, each
//! turned into a check with a copyable fix.
#![allow(unsafe_code)]

use crate::analytics::human_bytes;
use crate::efficiency::{self, Mode, Resolved};
use crate::storage::{FREE_SPACE_FLOOR, StoragePolicy};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Where the checks are explained.
const GUIDE: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "#host-checks-and-efficiency-mode"
);
/// Room for two frames of a 16 MiB GigE camera; the value the GigE transport
/// already suggests when its receive buffer comes up short.
const GIGE_RECEIVE_BUFFER: u64 = 32 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub level: Level,
    pub message: String,
    pub fix: Option<String>,
}
impl Check {
    fn ok(id: &'static str, message: String) -> Self {
        Self {
            id,
            level: Level::Ok,
            message,
            fix: None,
        }
    }
    fn warn(id: &'static str, message: String, fix: String) -> Self {
        Self {
            id,
            level: Level::Warn,
            message,
            fix: Some(fix),
        }
    }
}

/// A file system holding something Capturefab writes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Place {
    pub path: PathBuf,
    /// When false, the facts describe the nearest existing parent folder.
    pub exists: bool,
    pub fs_type: Option<String>,
    /// tmpfs or ramfs: fast and easy on SD cards, but gone after a reboot.
    pub ram_backed: bool,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}
impl Place {
    fn of(path: PathBuf) -> Self {
        let existing = path
            .ancestors()
            .find(|p| !p.as_os_str().is_empty() && p.exists())
            .unwrap_or(Path::new("."));
        let fs_type = file_system(existing);
        Self {
            exists: existing == path,
            ram_backed: fs_type.as_deref().is_some_and(ram_backed),
            fs_type,
            total_bytes: fs2::total_space(existing).ok(),
            free_bytes: fs2::available_space(existing).ok(),
            path,
        }
    }
    fn described(&self) -> String {
        match &self.fs_type {
            Some(fs) => format!("{} ({fs})", self.path.display()),
            None => self.path.display().to_string(),
        }
    }
}
fn ram_backed(fs_type: &str) -> bool {
    matches!(fs_type, "tmpfs" | "ramfs")
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Limits {
    /// Linux caps every socket receive buffer here, whatever GigE asks for.
    pub rmem_max: Option<u64>,
    /// Linux's budget for zero-copy USB transfers, shared by all devices.
    pub usbfs_memory_mb: Option<u64>,
}

/// Only Linux reads these from sysfs.
#[cfg_attr(not(all(target_os = "linux", feature = "usb")), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
struct UsbCamera {
    name: String,
    node: PathBuf,
    vendor: String,
    accessible: bool,
}

/// What the checks look at, gathered once.
struct Facts {
    efficiency: Resolved,
    /// The GPU probe's answer, when it ran.
    graphics: Option<Value>,
    /// Where sessions are found, with their short-lived files.
    runtime: Place,
    /// Settings, saved destinations, the upload queue and the storage ledger.
    state: Place,
    /// CAPTUREFAB_SESSION_DIR chose both.
    session_dir_set: bool,
    /// `/run/user/<uid>` when the runtime directory fell back to disk
    /// without it.
    missed_runtime_dir: Option<PathBuf>,
    limits: Limits,
    usb: Vec<UsbCamera>,
    budget: u64,
    /// Bytes counted against the budget, reservations included; None while
    /// the ledger's usage is unknown.
    used: Option<u64>,
    working_directory: Place,
    /// The default capture folder's disk, where `storage --json` measures.
    captures: Place,
}
impl Facts {
    fn gather() -> Self {
        let state = crate::ipc::state_dir();
        let (budget, used) = ledger(&state);
        Self {
            efficiency: efficiency::current(),
            graphics: None,
            budget,
            used,
            runtime: Place::of(crate::ipc::runtime_dir()),
            state: Place::of(state),
            session_dir_set: std::env::var_os("CAPTUREFAB_SESSION_DIR")
                .is_some_and(|dir| !dir.is_empty()),
            missed_runtime_dir: crate::ipc::missed_runtime_dir(),
            limits: limits(),
            usb: usb_cameras(),
            working_directory: Place::of(std::env::current_dir().unwrap_or_else(|_| ".".into())),
            captures: capture_place(),
        }
    }
}

/// The host part of `doctor`. The GPU probe starts first and runs in a
/// helper process while the rest of the report is gathered.
pub struct HostReport {
    graphics: Option<std::thread::JoinHandle<Value>>,
}
impl HostReport {
    pub fn start() -> Self {
        Self {
            #[cfg(feature = "gui")]
            graphics: Some(std::thread::spawn(graphics)),
            #[cfg(not(feature = "gui"))]
            graphics: None,
        }
    }
    pub fn finish(self, report: &mut Value) {
        let mut facts = Facts::gather();
        facts.graphics = self.graphics.map(|probe| {
            probe
                .join()
                .unwrap_or_else(|_| json!({"available":false,"reason":"the GPU probe failed"}))
        });
        report["host"] = json!(efficiency::host());
        report["efficiency"] = json!(facts.efficiency);
        report["paths"] = json!({
            "session": facts.runtime,
            "runtime": facts.runtime,
            "state": facts.state,
            "cache": crate::ipc::cache_dir().map(Place::of),
            "temp": Place::of(std::env::temp_dir()),
            "working_directory": facts.working_directory,
        });
        report["limits"] = json!(facts.limits);
        report["graphics"] = facts
            .graphics
            .clone()
            .unwrap_or_else(|| json!({"available":false,"reason":"not compiled in"}));
        report["checks"] = json!(checks(&facts));
    }
}

/// For `serve`: one line naming what `doctor` warns about.
pub fn print_warnings() {
    let warnings: Vec<String> = checks(&Facts::gather())
        .into_iter()
        .filter(|check| check.level == Level::Warn)
        .map(|check| check.message)
        .collect();
    if !warnings.is_empty() {
        eprintln!(
            "capturefab: {} Run `capturefab doctor` for fixes.",
            warnings.join(" ")
        );
    }
}

/// Human `doctor`: the host on one line, then one line per check with its fix.
pub fn print_checklist(out: &mut impl Write, report: &Value) -> io::Result<()> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let host = &report["host"];
    let mut summary = vec![format!(
        "Capturefab {} on {} {}",
        text(&report["version"]),
        text(&report["os"]),
        text(&report["arch"])
    )];
    summary.push(format!("{} CPU cores", host["cpu_cores"]));
    if let Some(total) = host["memory_total"].as_u64() {
        summary.push(match host["memory_available"].as_u64() {
            Some(free) => format!(
                "{} memory, {} available",
                human_bytes(total),
                human_bytes(free)
            ),
            None => format!("{} memory", human_bytes(total)),
        });
    }
    if host["on_battery"] == true {
        summary.push("on battery".into());
    }
    if host["low_power"] == true {
        summary.push("low-power mode".into());
    }
    writeln!(out, "{}", summary.join(" · "))?;
    for check in report["checks"].as_array().into_iter().flatten() {
        writeln!(
            out,
            "{:<5} {}",
            text(&check["level"]),
            text(&check["message"])
        )?;
        if let Some(fix) = check["fix"].as_str() {
            writeln!(out, "      Fix: {fix}")?;
        }
    }
    writeln!(
        out,
        "More: {GUIDE}. doctor --json adds compiled features, scanners, signal protocols, JPEG encoders and network interfaces."
    )
}

fn checks(facts: &Facts) -> Vec<Check> {
    let mut checks = vec![efficiency_check(&facts.efficiency)];
    checks.extend(facts.graphics.as_ref().map(graphics_check));
    checks.extend(
        facts
            .missed_runtime_dir
            .as_deref()
            .map(|dir| runtime_dir_check(dir, &facts.runtime, &facts.state)),
    );
    checks.push(session_check(&facts.state, facts.session_dir_set));
    checks.extend(facts.limits.rmem_max.map(receive_buffer_check));
    checks.extend(usb_check(&facts.usb));
    checks.extend(
        facts
            .used
            .and_then(|used| budget_check(facts.budget, used, &facts.captures)),
    );
    checks
}

fn efficiency_check(resolved: &Resolved) -> Check {
    let mode = if resolved.efficient {
        "Efficient"
    } else {
        "Smooth"
    };
    let why = if resolved.reasons.is_empty() {
        "auto found no constraint".to_owned()
    } else {
        resolved.reasons.join(", ")
    };
    let mut check = Check::ok("efficiency", format!("{mode} mode: {why}."));
    if resolved.efficient && resolved.setting == Mode::Auto {
        check.fix = Some(
            "To stay smooth anyway, use --efficiency smooth or CAPTUREFAB_EFFICIENCY=smooth."
                .into(),
        );
    }
    check
}

fn graphics_check(graphics: &Value) -> Check {
    let adapter = graphics["adapter"].as_str().unwrap_or("unknown adapter");
    if graphics["available"] != true {
        Check::warn(
            "graphics",
            format!(
                "No GPU adapter ({}), so the desktop window would draw in software.",
                graphics["reason"].as_str().unwrap_or("unknown reason")
            ),
            "Install the GPU's Vulkan, Metal, Direct3D 12 or OpenGL driver.".into(),
        )
    } else if graphics["software"] == true {
        Check::warn(
            "graphics",
            format!(
                "The desktop window would draw on the CPU ({adapter}): no hardware GPU driver is in use."
            ),
            "Install the GPU's Vulkan or OpenGL driver.".into(),
        )
    } else {
        Check::ok(
            "graphics",
            format!(
                "GPU: {adapter} ({}, {}).",
                graphics["backend"].as_str().unwrap_or("unknown backend"),
                graphics["device_type"].as_str().unwrap_or("unknown type")
            ),
        )
    }
}

/// Without XDG_RUNTIME_DIR, Capturefab uses the login's runtime directory
/// when it exists, so a service and an SSH shell find the same sessions.
/// logind creates it only for a logged-in or lingering user, so a service
/// started at boot without lingering finds it missing and falls back to
/// disk. Frame rings are anonymous memory either way, but the nvJPEG and
/// VA-API helpers' staging buffers are files there (`accel`).
fn runtime_dir_check(login: &Path, runtime: &Place, state: &Place) -> Check {
    let dir = login.display();
    let runtime = runtime.path.display();
    let state = state.path.display();
    let uid = login
        .file_name()
        .map_or_else(|| "$USER".into(), |uid| uid.to_string_lossy());
    Check::warn(
        "runtime_dir",
        format!(
            "XDG_RUNTIME_DIR is unset and {dir} is missing or not private to this user, so sessions are on disk in {runtime}, while logins look for them in {dir}/capturefab."
        ),
        format!(
            "Run `sudo loginctl enable-linger {uid}` so {dir} exists from boot and survives logout, and give a service Wants= and After=user@{uid}.service; Capturefab then uses {dir} without XDG_RUNTIME_DIR. Settings, destinations, the upload queue and the storage ledger stay in {state} either way."
        ),
    )
}

/// Informational rather than a warning: by default state is on disk, so
/// memory means the user chose it, and only they know whether it must
/// outlive a reboot.
fn session_check(state: &Place, session_dir_set: bool) -> Check {
    if !state.ram_backed {
        return Check::ok(
            "session_storage",
            format!(
                "Settings, saved destinations, the upload queue and the storage ledger are in {}.",
                state.described()
            ),
        );
    }
    let mut check = Check::ok(
        "session_storage",
        format!(
            "Settings, saved destinations, the upload queue and the storage ledger are in memory in {} and are cleared at reboot.",
            state.described()
        ),
    );
    check.fix = Some(if session_dir_set {
        "To keep them, unset CAPTUREFAB_SESSION_DIR. A folder on disk keeps them too, but JPEG captures encoded by nvJPEG or VA-API then pass their pixels through a mapped file there.".into()
    } else if cfg!(target_os = "linux") {
        "To keep them, point XDG_STATE_HOME at a folder on disk.".into()
    } else {
        "To keep them, set CAPTUREFAB_SESSION_DIR to a folder on disk.".into()
    });
    check
}

fn receive_buffer_check(rmem_max: u64) -> Check {
    let frame = human_bytes(rmem_max / 2);
    if rmem_max < GIGE_RECEIVE_BUFFER {
        Check::warn(
            "receive_buffer",
            format!(
                "net.core.rmem_max is {}, so GigE Vision frames above {frame} can lose packets.",
                human_bytes(rmem_max)
            ),
            format!(
                "sudo sysctl -w net.core.rmem_max={GIGE_RECEIVE_BUFFER} && echo net.core.rmem_max={GIGE_RECEIVE_BUFFER} | sudo tee /etc/sysctl.d/60-capturefab.conf"
            ),
        )
    } else {
        Check::ok(
            "receive_buffer",
            format!(
                "net.core.rmem_max is {}: room for two GigE Vision frames of {frame}.",
                human_bytes(rmem_max)
            ),
        )
    }
}

fn usb_check(cameras: &[UsbCamera]) -> Option<Check> {
    if cameras.is_empty() {
        return None;
    }
    let denied: Vec<&UsbCamera> = cameras.iter().filter(|c| !c.accessible).collect();
    if denied.is_empty() {
        let names: Vec<&str> = cameras.iter().map(|c| c.name.as_str()).collect();
        return Some(Check::ok(
            "usb_access",
            format!("USB3 Vision cameras are accessible: {}.", names.join(", ")),
        ));
    }
    let names: Vec<String> = denied
        .iter()
        .map(|c| format!("{} ({})", c.name, c.node.display()))
        .collect();
    let mut vendors: Vec<&str> = denied.iter().map(|c| c.vendor.as_str()).collect();
    vendors.sort_unstable();
    vendors.dedup();
    // One file per vendor, so a later fix for another camera cannot replace
    // the rule an earlier one wrote.
    let rules: Vec<String> = vendors
        .iter()
        .map(|vendor| {
            format!(
                "echo 'SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{vendor}\", MODE=\"0660\", GROUP=\"plugdev\", TAG+=\"uaccess\"' | sudo tee /etc/udev/rules.d/60-capturefab-{vendor}.rules >/dev/null"
            )
        })
        .collect();
    Some(Check::warn(
        "usb_access",
        format!(
            "No read/write access to USB3 Vision camera {}.",
            names.join(", ")
        ),
        format!(
            "{} && sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=usb; services also need their user in the plugdev group",
            rules.join(" && ")
        ),
    ))
}

/// Whether the part of the budget not yet used fits on the disk captures
/// land on, less the floor every save leaves free there; what is already
/// stored is on that disk too.
fn budget_check(budget: u64, used: u64, place: &Place) -> Option<Check> {
    let free = place.free_bytes?;
    let usable = free.saturating_sub(FREE_SPACE_FLOOR);
    let path = place.path.display();
    let room = budget.saturating_sub(used);
    Some(if room > usable {
        Check::warn(
            "storage_budget",
            format!(
                "The storage budget leaves room for {} more ({} of {} used), but only {} is free on {path} and saves keep {} of it free, so the disk can fill first.",
                human_bytes(room),
                human_bytes(used),
                human_bytes(budget),
                human_bytes(free),
                human_bytes(FREE_SPACE_FLOOR)
            ),
            format!(
                "capturefab storage configure --max-space {}",
                suggested_budget(used, usable)
            ),
        )
    } else {
        Check::ok(
            "storage_budget",
            format!(
                "Storage budget {}, {} used; {} free on {path}.",
                human_bytes(budget),
                human_bytes(used),
                human_bytes(free)
            ),
        )
    })
}
/// What is stored plus three quarters of the usable space, in whole units
/// `storage configure` accepts. Never below what is stored: retention would
/// delete the oldest captures to get under it, or stop every capture.
fn suggested_budget(used: u64, free: u64) -> String {
    let budget = used.saturating_add(free / 4 * 3);
    let gib = budget >> 30;
    if gib > 0 && gib << 30 >= used {
        format!("{gib}GiB")
    } else {
        format!("{}MiB", (budget >> 20).max(used.div_ceil(1 << 20)).max(1))
    }
}

/// The configured budget and the bytes counted against it, from the storage
/// ledger's lock-free reads, so `doctor` never waits behind capture
/// bookkeeping; its usage summary includes the journal's tail. Usage is None
/// while unknown, such as after an older version saved, which omits the
/// check. Without a state directory nothing is stored yet, and `doctor`
/// does not create one to find that out, unless the ledger of a version
/// before the split may be waiting to move into it.
fn ledger(state: &Path) -> (u64, Option<u64>) {
    let default = StoragePolicy::default().max_bytes;
    let moving = crate::ipc::legacy_dir().is_some_and(|old| old.is_dir());
    if !state.is_dir() && !moving {
        return (default, Some(0));
    }
    let budget = crate::storage::configured_policy().map_or(default, |policy| policy.max_bytes);
    let used = crate::storage::usage()
        .ok()
        .flatten()
        .map(|usage| usage.allocated_bytes);
    (budget, used)
}

/// The default capture folder, or its nearest existing parent, where
/// `storage --json` reports free space too.
fn capture_place() -> Place {
    let root = crate::storage::capture_root();
    Place {
        free_bytes: crate::storage::free_bytes(&root),
        ..Place::of(root)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn statfs(path: &Path) -> Option<libc::statfs> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: a NUL-terminated path and a buffer the call fills on success,
    // only read after it succeeded.
    unsafe { (libc::statfs(path.as_ptr(), stat.as_mut_ptr()) == 0).then(|| stat.assume_init()) }
}

/// Names for the magic numbers Linux reports; others show in hex.
#[cfg(any(target_os = "linux", test))]
const FILE_SYSTEMS: [(u32, &str); 17] = [
    (0x0102_1994, "tmpfs"),
    (0x8584_58f6, "ramfs"),
    (0xef53, "ext4"),
    (0x9123_683e, "btrfs"),
    (0x5846_5342, "xfs"),
    (0xf2f5_2010, "f2fs"),
    (0x2fc1_2fc1, "zfs"),
    (0x4d44, "vfat"),
    (0x2011_bab0, "exfat"),
    (0x5346_544e, "ntfs"),
    (0x6969, "nfs"),
    (0xff53_4d42, "cifs"),
    (0xfe53_4d42, "smb2"),
    (0x6573_5546, "fuse"),
    (0x794c_7630, "overlay"),
    (0x7371_7368, "squashfs"),
    (0x0102_1997, "9p"),
];
#[cfg(any(target_os = "linux", test))]
fn file_system_name(magic: u32) -> String {
    FILE_SYSTEMS
        .iter()
        .find(|(known, _)| *known == magic)
        .map_or_else(|| format!("{magic:#x}"), |(_, name)| (*name).to_owned())
}

#[cfg(target_os = "linux")]
fn file_system(path: &Path) -> Option<String> {
    // f_type is a signed word on glibc; the magic numbers are its low 32 bits.
    Some(file_system_name(statfs(path)?.f_type as u32))
}

#[cfg(target_os = "macos")]
fn file_system(path: &Path) -> Option<String> {
    let stat = statfs(path)?;
    // SAFETY: the kernel NUL-terminates the type name within the array.
    let name = unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn file_system(_path: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn limits() -> Limits {
    let number = |path: &str| std::fs::read_to_string(path).ok()?.trim().parse().ok();
    Limits {
        rmem_max: number("/proc/sys/net/core/rmem_max"),
        usbfs_memory_mb: number("/sys/module/usbcore/parameters/usbfs_memory_mb"),
    }
}
#[cfg(not(target_os = "linux"))]
fn limits() -> Limits {
    Limits::default()
}

/// USB3 Vision devices from sysfs, by their control interface, and whether
/// this user may open their usbfs nodes.
#[cfg(all(target_os = "linux", feature = "usb"))]
fn usb_cameras() -> Vec<UsbCamera> {
    use std::os::unix::ffi::OsStrExt;
    let root = Path::new("/sys/bus/usb/devices");
    let read = |path: PathBuf| {
        std::fs::read_to_string(path)
            .map(|text| text.trim().to_owned())
            .unwrap_or_default()
    };
    let mut devices: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let (device, _) = name.split_once(':')?;
            let interface = entry.path();
            (read(interface.join("bInterfaceClass")) == "ef"
                && read(interface.join("bInterfaceSubClass")) == "05"
                && read(interface.join("bInterfaceProtocol")) == "00")
                .then(|| device.to_owned())
        })
        .collect();
    devices.sort();
    devices.dedup();
    devices
        .into_iter()
        .filter_map(|device| {
            let dir = root.join(&device);
            let bus: u32 = read(dir.join("busnum")).parse().ok()?;
            let number: u32 = read(dir.join("devnum")).parse().ok()?;
            let node = PathBuf::from(format!("/dev/bus/usb/{bus:03}/{number:03}"));
            let path = std::ffi::CString::new(node.as_os_str().as_bytes()).ok()?;
            // SAFETY: a NUL-terminated path; access only checks permissions.
            let accessible = unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) } == 0;
            let product = read(dir.join("product"));
            Some(UsbCamera {
                name: if product.is_empty() { device } else { product },
                node,
                vendor: read(dir.join("idVendor")),
                accessible,
            })
        })
        .collect()
}
#[cfg(not(all(target_os = "linux", feature = "usb")))]
fn usb_cameras() -> Vec<UsbCamera> {
    Vec::new()
}

/// How long the GPU probe may take: a healthy driver answers in well under a
/// second, a broken one may never answer.
#[cfg(feature = "gui")]
const GPU_PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// The adapter the desktop window would get, asked in a helper process: a
/// broken Vulkan or OpenGL driver can crash or hang whoever loads it.
#[cfg(feature = "gui")]
fn graphics() -> Value {
    probe_graphics()
        .unwrap_or_else(|error| json!({"available":false,"reason":format!("{error:#}")}))
}

#[cfg(feature = "gui")]
fn probe_graphics() -> anyhow::Result<Value> {
    use anyhow::{Context, anyhow, ensure};
    use std::{
        io::{BufRead, BufReader, Read},
        process::{Command, Stdio},
        sync::mpsc,
    };
    let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| "capturefab".into());
    let mut child = crate::session::spawn_helper(
        Command::new(executable)
            .arg("__gpu-probe")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .context("launch the GPU probe")?;
    let stdout = child.stdout.take().context("GPU probe output")?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout.take(64 * 1024)).read_line(&mut line);
        let _ = sender.send(line);
    });
    let line = receiver.recv_timeout(GPU_PROBE_DEADLINE);
    // A process stuck in a graphics driver may survive even SIGKILL, so it
    // is reaped in the background rather than waited for.
    let _ = child.kill();
    std::thread::spawn(move || child.wait());
    let line = line.map_err(|_| {
        anyhow!(
            "the graphics driver did not answer within {} s",
            GPU_PROBE_DEADLINE.as_secs()
        )
    })?;
    ensure!(
        !line.is_empty(),
        "the GPU probe stopped without an answer; the graphics driver may have crashed"
    );
    Ok(serde_json::from_str(&line)?)
}

/// Hidden `__gpu-probe` mode: print the adapter iced would request, without
/// a window, as one JSON line.
#[cfg(feature = "gui")]
pub fn run_gpu_probe() -> anyhow::Result<()> {
    use iced::{executor::Executor, wgpu};
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all()),
        ..Default::default()
    });
    let adapter = iced::executor::Default::new()?.block_on(instance.request_adapter(
        &wgpu::RequestAdapterOptions {
            power_preference:
                wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance),
            compatible_surface: None,
            force_fallback_adapter: false,
        },
    ));
    let report = match adapter {
        Ok(adapter) => adapter_report(&adapter.get_info(), &adapter.limits()),
        Err(error) => json!({"available":false,"reason":error.to_string()}),
    };
    crate::cli::write_json(&report)
}

#[cfg(feature = "gui")]
fn adapter_report(info: &iced::wgpu::AdapterInfo, allowed: &iced::wgpu::Limits) -> Value {
    use iced::wgpu::{DeviceType, Limits};
    // iced asks for the default limits, then the downlevel ones; frames wider
    // or taller than the granted texture size are converted on the CPU.
    let texture = [Limits::default(), Limits::downlevel_defaults()]
        .into_iter()
        .map(|limits| Limits {
            max_bind_groups: 2,
            max_non_sampler_bindings: 2048,
            ..limits
        })
        .find(|limits| limits.check_limits(allowed))
        .map(|limits| limits.max_texture_dimension_2d);
    let device_type = match info.device_type {
        DeviceType::IntegratedGpu => "integrated",
        DeviceType::DiscreteGpu => "discrete",
        DeviceType::VirtualGpu => "virtual",
        DeviceType::Cpu => "cpu",
        DeviceType::Other => "other",
    };
    json!({
        "available": true,
        "adapter": info.name,
        "backend": info.backend.to_str(),
        "device_type": device_type,
        "driver": info.driver,
        "max_texture_dimension_2d": texture,
        "software": software_adapter(info.device_type == DeviceType::Cpu, &info.name),
    })
}

/// Software rasterizers, including GL drivers that do not report a CPU type.
#[cfg(any(feature = "gui", test))]
fn software_adapter(cpu: bool, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    cpu || [
        "llvmpipe",
        "lavapipe",
        "softpipe",
        "swiftshader",
        "basic render",
    ]
    .iter()
    .any(|software| name.contains(software))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(path: &str, fs: &str, free: u64) -> Place {
        Place {
            path: path.into(),
            exists: true,
            fs_type: Some(fs.into()),
            ram_backed: ram_backed(fs),
            total_bytes: Some(free * 2),
            free_bytes: Some(free),
        }
    }

    #[test]
    fn receive_buffer_needs_room_for_two_large_frames() {
        let small = receive_buffer_check(212_992);
        assert_eq!(small.level, Level::Warn);
        assert_eq!(
            small.message,
            "net.core.rmem_max is 208.0 KiB, so GigE Vision frames above 104.0 KiB can lose packets."
        );
        let fix = small.fix.unwrap();
        assert!(
            fix.contains("sysctl -w net.core.rmem_max=33554432"),
            "{fix}"
        );
        assert!(fix.contains("/etc/sysctl.d/"), "{fix}");
        let tuned = receive_buffer_check(GIGE_RECEIVE_BUFFER);
        assert_eq!(tuned.level, Level::Ok);
        assert!(tuned.message.contains("16.0 MiB"), "{}", tuned.message);
    }

    #[test]
    fn budget_larger_than_free_space_warns_with_a_smaller_budget() {
        let budget = crate::storage::StoragePolicy::default().max_bytes;
        let card = place("/home/pi", "ext4", 6 << 30);
        let check = budget_check(budget, 0, &card).unwrap();
        assert_eq!(check.level, Level::Warn);
        assert_eq!(
            check.message,
            "The storage budget leaves room for 10.00 GiB more (0.0 KiB of 10.00 GiB used), but only 6.00 GiB is free on /home/pi and saves keep 512.0 MiB of it free, so the disk can fill first."
        );
        assert_eq!(
            check.fix.as_deref(),
            Some("capturefab storage configure --max-space 4GiB")
        );
        assert_eq!(
            budget_check(budget, 0, &place("/data", "xfs", 100 << 30))
                .unwrap()
                .level,
            Level::Ok
        );
        // Saves keep the floor free, so a budget that fits only into it warns.
        let tight = place("/home/pi", "ext4", 10 << 30);
        assert_eq!(
            budget_check(10 << 30, 0, &tight).unwrap().level,
            Level::Warn
        );
        assert_eq!(
            budget_check(10 << 30, 512 << 20, &tight).unwrap().level,
            Level::Ok
        );
        assert_eq!(suggested_budget(0, 100 << 20), "75MiB");
        assert_eq!(suggested_budget(0, 0), "1MiB");
        let unknown = Place {
            free_bytes: None,
            ..card
        };
        assert_eq!(budget_check(budget, 0, &unknown), None);
    }

    #[test]
    fn budget_check_counts_what_is_already_stored() {
        // A 10 GiB budget with 9.5 GiB stored has 0.5 GiB left: 5 GiB free
        // is plenty, so nothing warns.
        let budget = 10 << 30;
        let stored = 19 << 29;
        let card = place("/home/pi/captures", "ext4", 5 << 30);
        let check = budget_check(budget, stored, &card).unwrap();
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            check.message,
            "Storage budget 10.00 GiB, 9.50 GiB used; 5.00 GiB free on /home/pi/captures."
        );
        // With 2 GiB stored, 8 GiB would not fit, and the suggestion keeps
        // what is stored plus three quarters of the free space.
        let check = budget_check(budget, 2 << 30, &card).unwrap();
        assert_eq!(check.level, Level::Warn);
        assert!(check.message.contains("(2.00 GiB of 10.00 GiB used)"));
        assert_eq!(
            check.fix.as_deref(),
            Some("capturefab storage configure --max-space 5GiB")
        );
        // A suggestion never drops below what is stored, even where whole
        // GiB would round under it.
        for (used, free) in [
            (stored, 200 << 20),
            (stored + 100, 0),
            ((1 << 30) - 1, 0),
            (3 << 30, 100 << 30),
        ] {
            let suggested = crate::storage::parse_bytes(&suggested_budget(used, free)).unwrap();
            assert!(suggested >= used, "{used} {free}: {suggested}");
            assert!(
                suggested <= used + free / 4 * 3 + (1 << 20),
                "{used} {free}"
            );
        }
        assert_eq!(suggested_budget(stored, 200 << 20), "9878MiB");
    }

    #[test]
    fn storage_is_measured_on_the_capture_folder_disk_without_creating_a_session() {
        let missing = std::env::temp_dir().join(format!(
            "capturefab-doctor-ledger-{}/sessions",
            std::process::id()
        ));
        assert_eq!(
            ledger(&missing),
            (StoragePolicy::default().max_bytes, Some(0)),
            "nothing is stored before a session directory exists"
        );
        assert!(!missing.exists());
        // Where storage --json reports free space: the default capture
        // folder, or its nearest existing parent.
        // Free space moves while other processes write, so only whether the
        // disk reports it is compared.
        let captures = capture_place();
        assert_eq!(captures.path, crate::storage::capture_root());
        assert_eq!(
            captures.free_bytes.is_some(),
            crate::storage::free_bytes(&captures.path).is_some()
        );
    }

    #[test]
    fn state_on_disk_is_the_default_and_memory_explains_how_to_keep_it() {
        let disk = place("/home/pi/.local/state/capturefab", "ext4", 6 << 30);
        let check = session_check(&disk, false);
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            check.message,
            "Settings, saved destinations, the upload queue and the storage ledger are in /home/pi/.local/state/capturefab (ext4)."
        );
        assert_eq!(check.fix, None);
        // Memory is only ever chosen, so it informs rather than warns, with
        // the setting that put it there.
        let memory = place("/tmp/capturefab", "tmpfs", 400 << 20);
        let chosen = session_check(&memory, true);
        assert_eq!(chosen.level, Level::Ok);
        assert!(chosen.message.contains("(tmpfs)"), "{}", chosen.message);
        assert!(chosen.message.contains("cleared at reboot"));
        let fix = chosen.fix.unwrap();
        assert!(fix.starts_with("To keep them, unset CAPTUREFAB_SESSION_DIR."));
        assert!(fix.contains("nvJPEG or VA-API"), "{fix}");
        let fix = session_check(&memory, false).fix.unwrap();
        assert!(
            fix.contains(if cfg!(target_os = "linux") {
                "XDG_STATE_HOME"
            } else {
                "CAPTUREFAB_SESSION_DIR"
            }),
            "{fix}"
        );
    }

    #[test]
    fn a_runtime_directory_on_disk_warns_with_lingering() {
        let runtime = place("/home/pi/.cache/capturefab/sessions", "ext4", 6 << 30);
        let state = place("/home/pi/.local/state/capturefab", "ext4", 6 << 30);
        let fallback = runtime_dir_check(Path::new("/run/user/1000"), &runtime, &state);
        assert_eq!(fallback.level, Level::Warn);
        assert_eq!(
            fallback.message,
            "XDG_RUNTIME_DIR is unset and /run/user/1000 is missing or not private to this user, so sessions are on disk in /home/pi/.cache/capturefab/sessions, while logins look for them in /run/user/1000/capturefab."
        );
        // Lingering makes the login directory exist for a service at boot;
        // state no longer moves with it.
        let fix = fallback.fix.unwrap();
        assert!(
            fix.starts_with(
                "Run `sudo loginctl enable-linger 1000` so /run/user/1000 exists from boot"
            ),
            "{fix}"
        );
        assert!(fix.contains("After=user@1000.service"), "{fix}");
        assert!(
            fix.contains("stay in /home/pi/.local/state/capturefab either way"),
            "{fix}"
        );
        assert!(!fix.contains("export XDG_RUNTIME_DIR"), "{fix}");
    }

    #[test]
    fn usb_check_names_inaccessible_cameras_and_their_udev_rule() {
        assert_eq!(usb_check(&[]), None);
        let camera = UsbCamera {
            name: "acA1920-155um".into(),
            node: "/dev/bus/usb/002/003".into(),
            vendor: "2676".into(),
            accessible: true,
        };
        assert_eq!(
            usb_check(std::slice::from_ref(&camera)).unwrap().level,
            Level::Ok
        );
        let denied = UsbCamera {
            accessible: false,
            ..camera
        };
        let check = usb_check(&[denied.clone(), denied.clone()]).unwrap();
        assert_eq!(check.level, Level::Warn);
        assert!(
            check
                .message
                .contains("acA1920-155um (/dev/bus/usb/002/003)")
        );
        let fix = check.fix.unwrap();
        assert_eq!(fix.matches("idVendor").count(), 1, "{fix}");
        assert!(fix.contains(r#"ATTR{idVendor}=="2676""#), "{fix}");
        assert!(fix.contains("udevadm trigger"), "{fix}");
        // Each vendor gets its own file, so fixing a camera plugged in later
        // keeps the rule that already gives this one access.
        let other = UsbCamera {
            name: "BFS-U3-16S2M".into(),
            node: "/dev/bus/usb/002/004".into(),
            vendor: "1e10".into(),
            accessible: false,
        };
        let fix = usb_check(std::slice::from_ref(&other))
            .unwrap()
            .fix
            .unwrap();
        assert!(
            fix.contains("sudo tee /etc/udev/rules.d/60-capturefab-1e10.rules"),
            "{fix}"
        );
        assert!(!fix.contains("2676"), "{fix}");
        assert!(!fix.contains("tee /etc/udev/rules.d/60-capturefab.rules"));
        let both = usb_check(&[denied, other]).unwrap().fix.unwrap();
        for file in ["60-capturefab-2676.rules", "60-capturefab-1e10.rules"] {
            assert_eq!(both.matches(file).count(), 1, "{both}");
        }
    }

    #[test]
    fn efficiency_and_graphics_checks_explain_themselves() {
        let auto = Resolved {
            setting: Mode::Auto,
            efficient: true,
            reasons: vec!["2 CPU cores".into()],
        };
        let check = efficiency_check(&auto);
        assert_eq!(
            (check.level, check.message.as_str()),
            (Level::Ok, "Efficient mode: 2 CPU cores.")
        );
        assert!(check.fix.unwrap().contains("--efficiency smooth"));
        let smooth = efficiency_check(&Resolved {
            setting: Mode::Auto,
            efficient: false,
            reasons: Vec::new(),
        });
        assert_eq!(smooth.message, "Smooth mode: auto found no constraint.");
        assert_eq!(smooth.fix, None);
        let software = json!({"available":true,"adapter":"llvmpipe (LLVM 17.0.6, 256 bits)","backend":"vulkan","device_type":"cpu","software":true});
        assert_eq!(graphics_check(&software).level, Level::Warn);
        let missing = json!({"available":false,"reason":"no adapter"});
        assert!(graphics_check(&missing).message.contains("no adapter"));
        let metal = json!({"available":true,"adapter":"Apple M4 Max","backend":"metal","device_type":"integrated","software":false});
        assert_eq!(
            graphics_check(&metal).message,
            "GPU: Apple M4 Max (metal, integrated)."
        );
        assert!(software_adapter(false, "llvmpipe (LLVM 15.0.7, 128 bits)"));
        assert!(software_adapter(true, "Unknown"));
        assert!(!software_adapter(false, "V3D 7.1"));
    }

    #[test]
    fn places_describe_their_file_system() {
        assert_eq!(file_system_name(0x0102_1994), "tmpfs");
        assert_eq!(file_system_name(0x1234), "0x1234");
        assert!(ram_backed("tmpfs") && ram_backed("ramfs") && !ram_backed("ext4"));
        let temp = Place::of(std::env::temp_dir());
        assert!(temp.exists && temp.free_bytes.is_some());
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(temp.fs_type.is_some());
        }
        let missing = Place::of(std::env::temp_dir().join("capturefab-doctor-missing/a/b"));
        assert!(!missing.exists && missing.free_bytes.is_some());
        assert_eq!(missing.fs_type, temp.fs_type);
    }

    #[test]
    fn checklist_prints_one_line_per_check_with_fixes() {
        let report = json!({
            "version":"0.2.2","os":"linux","arch":"aarch64",
            "host":{"cpu_cores":4,"memory_total":4_u64 << 30,"memory_available":3_u64 << 30,"on_battery":false,"low_power":false},
            "checks":[
                Check::ok("efficiency", "Smooth mode: auto found no constraint.".into()),
                receive_buffer_check(212_992),
            ],
        });
        let mut out = Vec::new();
        print_checklist(&mut out, &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Capturefab 0.2.2 on linux aarch64 · 4 CPU cores · 4.00 GiB memory, 3.00 GiB available"
        );
        assert_eq!(lines[1], "ok    Smooth mode: auto found no constraint.");
        assert!(lines[2].starts_with("warn  net.core.rmem_max is 208.0 KiB"));
        assert!(lines[3].starts_with("      Fix: sudo sysctl -w"));
        assert!(
            lines[4].contains("#host-checks-and-efficiency-mode"),
            "{text}"
        );
        // What plain doctor no longer prints, so the docs' pointers still work.
        assert!(lines[4].contains("scanners, signal protocols"), "{text}");
        assert_eq!(lines.len(), 5);
    }
}
