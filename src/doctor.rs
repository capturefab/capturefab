//! Host checks for `doctor`: where sessions, state and temporary files live,
//! kernel limits that cap camera throughput and the storage budget against
//! the capture disk, each turned into a check with a copyable fix.
#![allow(unsafe_code)]

use crate::ipc::LoginDir;
use crate::media::FfmpegHealth;
use crate::network::{InterfaceKind, NetworkInterface};
use crate::storage::{FREE_SPACE_FLOOR, StoragePolicy};
use crate::types::human_bytes;
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
/// Running Capturefab as a Linux service, with these fixes in context.
const LINE_COMPUTER: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/blob/main/docs/line-computer.md"
);
/// The udev rule Linux archives ship in share/udev/rules.d, and the fix for
/// `usb_access` writes: the logged-in seat user and the plugdev group may
/// open any device with a USB3 Vision interface (class ef/05/00).
/// The first rule imports usb_id, which sets ID_USB_INTERFACES, as not every
/// distribution has by the time rules sorted at 70 run.
const UDEV_RULES: [&str; 2] = [
    r#"SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", IMPORT{builtin}="usb_id""#,
    r#"SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ENV{ID_USB_INTERFACES}=="*:ef0500:*", MODE="0660", GROUP="plugdev", TAG+="uaccess""#,
];
const UDEV_RULE_FILE: &str = "70-capturefab-usb3vision.rules";
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
    #[serde(rename = "name")]
    pub id: &'static str,
    #[serde(rename = "status")]
    pub level: Level,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
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
    accessible: bool,
}

/// What the checks look at, gathered once.
struct Facts {
    /// Where sessions are found, with their short-lived files.
    runtime: Place,
    /// Presets, saved sign-ins and the storage ledger.
    state: Place,
    /// CAPTUREFAB_SESSION_DIR chose both.
    session_dir_set: bool,
    /// `/run/user/<uid>` when the runtime directory fell back to disk
    /// without it, and why.
    missed_runtime_dir: Option<(PathBuf, LoginDir)>,
    /// From a login that has XDG_RUNTIME_DIR: where a service without it
    /// keeps sessions instead.
    service_runtime_dir: Option<ServiceDir>,
    limits: Limits,
    usb: Vec<UsbCamera>,
    budget: u64,
    /// Bytes counted against the budget, reservations included; None while
    /// the ledger's usage is unknown.
    used: Option<u64>,
    /// The default capture folder's disk, where `storage --json` measures.
    captures: Place,
    /// IPv4 adapters, for the full report only: `serve`'s warnings never
    /// need them, and on Windows listing them runs PowerShell.
    interfaces: Option<Vec<NetworkInterface>>,
    /// Whether FFmpeg runs, or why not, for the full report only: `serve`
    /// would otherwise start FFmpeg (extracting it first) on every launch.
    ffmpeg: Option<Result<FfmpegHealth, String>>,
    /// CAPTUREFAB_FFMPEG chose FFmpeg.
    ffmpeg_overridden: bool,
    /// Files in the state directory that version 0.5 wrote and nothing
    /// reads any more.
    leftovers: Vec<PathBuf>,
    /// The runtime or state directory, where it exists but sessions would
    /// refuse it, and why.
    private_dirs: Vec<(PathBuf, String)>,
}
/// Where a service started without XDG_RUNTIME_DIR keeps sessions, out of
/// this login's sight, the login runtime directory it passes over and why,
/// and the sessions running there.
struct ServiceDir {
    dir: PathBuf,
    login: PathBuf,
    why: LoginDir,
    running: Vec<String>,
}
impl Facts {
    /// `report` also lists network adapters and runs FFmpeg, which only
    /// `doctor` shows.
    fn gather(report: bool) -> Self {
        let state = crate::ipc::state_dir();
        let (budget, used) = ledger(&state);
        let mut private_dirs = Vec::new();
        for dir in [crate::ipc::runtime_dir(), state.clone()] {
            if dir.exists()
                && !private_dirs.iter().any(|(seen, _)| *seen == dir)
                && let Err(error) = crate::ipc::check_private_dir(&dir)
            {
                private_dirs.push((dir, format!("{error:#}")));
            }
        }
        let leftovers = LEFTOVERS
            .iter()
            .map(|name| state.join(name))
            .filter(|path| path.symlink_metadata().is_ok())
            .collect();
        Self {
            leftovers,
            interfaces: report.then(|| crate::network::interfaces().unwrap_or_default()),
            ffmpeg: report
                .then(|| crate::media::ffmpeg_health().map_err(|error| format!("{error:#}"))),
            ffmpeg_overridden: std::env::var_os("CAPTUREFAB_FFMPEG").is_some(),
            private_dirs,
            budget,
            used,
            runtime: Place::of(crate::ipc::runtime_dir()),
            state: Place::of(state),
            session_dir_set: std::env::var_os("CAPTUREFAB_SESSION_DIR")
                .is_some_and(|dir| !dir.is_empty()),
            missed_runtime_dir: crate::ipc::missed_runtime_dir(),
            service_runtime_dir: crate::ipc::service_runtime_dir().map(|(dir, login, why)| {
                let running = crate::ipc::sessions_elsewhere()
                    .map(|(_, names)| names)
                    .unwrap_or_default();
                ServiceDir {
                    dir,
                    login,
                    why,
                    running,
                }
            }),
            limits: limits(),
            usb: usb_cameras(),
            captures: capture_place(),
        }
    }
}

/// The `doctor` document: checks with fixes, this build, its folders and
/// the FFmpeg it runs.
pub fn report() -> Value {
    let facts = Facts::gather(true);
    let runtime = crate::ipc::runtime_dir();
    let mut report = json!({
        "checks": checks(&facts),
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "features": {
            "usb": cfg!(feature = "usb"),
            "jpeg": cfg!(feature = "jpeg"),
            "keychain": cfg!(feature = "keychain"),
        },
        "dirs": {
            "inspect": runtime.join("inspect"),
            "runtime": runtime,
            "state": crate::ipc::state_dir(),
            "cache": crate::ipc::cache_dir(),
        },
    });
    if let Some(ffmpeg) = &facts.ffmpeg {
        report["ffmpeg"] = match ffmpeg {
            Ok(health) => {
                json!({"source": health.source, "path": health.path, "version": health.version})
            }
            Err(error) => json!({ "error": error }),
        };
    }
    report
}

/// For `serve`: one line naming what `doctor` warns about.
pub fn print_warnings() {
    let warnings: Vec<String> = checks(&Facts::gather(false))
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

/// Human `doctor`: the build on one line, then one line per check with
/// its fix.
pub fn print_checklist(out: &mut impl Write, report: &Value) -> io::Result<()> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    writeln!(
        out,
        "Capturefab {} on {} {} · {} CPU cores",
        text(&report["version"]),
        text(&report["os"]),
        text(&report["arch"]),
        std::thread::available_parallelism().map_or(1, |cores| cores.get())
    )?;
    for check in report["checks"].as_array().into_iter().flatten() {
        writeln!(
            out,
            "{:<5} {}",
            text(&check["status"]),
            text(&check["message"])
        )?;
        if let Some(fix) = check["fix"].as_str() {
            writeln!(out, "      Fix: {fix}")?;
        }
    }
    writeln!(
        out,
        "More: {GUIDE}. See also capturefab network; --json adds features, folders and FFmpeg."
    )?;
    if report["os"] == "linux" {
        writeln!(out, "Running it as a service: {LINE_COMPUTER}")?;
    }
    Ok(())
}

fn checks(facts: &Facts) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.extend(
        facts
            .missed_runtime_dir
            .as_ref()
            .map(|(dir, login)| runtime_dir_check(dir, *login, &facts.runtime, &facts.state)),
    );
    checks.extend(facts.service_runtime_dir.as_ref().map(|service| {
        service_runtime_dir_check(
            &service.dir,
            &service.login,
            service.why,
            &service.running,
            &facts.runtime,
        )
    }));
    checks.push(session_check(&facts.state, facts.session_dir_set));
    checks.extend(private_dirs_check(&facts.private_dirs));
    checks.extend(facts.interfaces.as_deref().map(camera_network_check));
    checks.extend(
        facts
            .ffmpeg
            .as_ref()
            .map(|health| ffmpeg_check(health, facts.ffmpeg_overridden, cfg!(debug_assertions))),
    );
    checks.extend(leftovers_check(&facts.leftovers));
    checks.extend(facts.limits.rmem_max.map(receive_buffer_check));
    checks.extend(usb_check(&facts.usb));
    checks.extend(
        facts
            .used
            .and_then(|used| budget_check(facts.budget, used, &facts.captures)),
    );
    checks
}

/// Without XDG_RUNTIME_DIR, Capturefab uses the login's runtime directory
/// for a lingering user, so a service and an SSH shell find the same
/// sessions. Otherwise logind removes it at logout, taking a service's
/// session with it, so Capturefab falls back to disk.
fn runtime_dir_check(login: &Path, why: LoginDir, runtime: &Place, state: &Place) -> Check {
    let dir = login.display();
    let runtime = runtime.path.display();
    let state = state.path.display();
    let (why, fix) = login_dir_problem(login, why);
    Check::warn(
        "runtime_dir",
        format!(
            "XDG_RUNTIME_DIR is unset and {why}, so sessions are on disk in {runtime}, while logins look for them in {dir}/capturefab."
        ),
        format!(
            "{fix} Presets, saved sign-ins and the storage ledger stay in {state} either way. A service unit that does this: {LINE_COMPUTER}"
        ),
    )
}

/// The same problem seen from a login, such as SSH, that has
/// XDG_RUNTIME_DIR: a service started without it keeps its sessions in
/// `service` instead, out of this login's sight. A warning once such a
/// session runs, otherwise a note, as most logins never start a service.
fn service_runtime_dir_check(
    service: &Path,
    login: &Path,
    why: LoginDir,
    running: &[String],
    runtime: &Place,
) -> Check {
    let (why, fix) = login_dir_problem(login, why);
    let (service, runtime) = (service.display(), runtime.path.display());
    let fix = format!(
        "{fix} To see what such a service sees, run `env -u XDG_RUNTIME_DIR capturefab doctor`. A service unit that does this: {LINE_COMPUTER}"
    );
    if running.is_empty() {
        let mut check = Check::ok(
            "runtime_dir",
            format!(
                "Sessions are in {runtime}. A service started without XDG_RUNTIME_DIR would keep its own in {service}, where logins do not look, as {why}."
            ),
        );
        check.fix = Some(fix);
        check
    } else {
        Check::warn(
            "runtime_dir",
            format!(
                "{} {} from {service}, where this login does not look: a service started without XDG_RUNTIME_DIR keeps sessions there, as {why}.",
                running.join(", "),
                if running.len() == 1 { "runs" } else { "run" }
            ),
            fix,
        )
    }
}

/// Why the login runtime directory cannot hold a service's sessions, and
/// the fix.
fn login_dir_problem(login: &Path, why: LoginDir) -> (String, String) {
    let dir = login.display();
    let uid = login
        .file_name()
        .map_or_else(|| "$USER".into(), |uid| uid.to_string_lossy());
    match why {
        LoginDir::NotPrivate => (
            format!("{dir} is not private to this user"),
            format!(
                "Make {dir} this user's with mode 0700, as logind creates it: `sudo chown {uid} {dir} && sudo chmod 700 {dir}`."
            ),
        ),
        LoginDir::Missing => (
            format!("{dir} does not exist yet"),
            format!(
                "Give a service Wants= and After=user@{uid}.service, so it starts once logind has made {dir}."
            ),
        ),
        LoginDir::NotLingering | LoginDir::Usable => (
            format!("this user does not linger (logind keeps {dir} only while they are logged in)"),
            format!(
                "Run `sudo loginctl enable-linger {uid}` so {dir} exists from boot and survives logout, and give a service Wants= and After=user@{uid}.service; Capturefab then uses {dir} without XDG_RUNTIME_DIR."
            ),
        ),
    }
}

/// Informational rather than a warning: by default state is on disk, so
/// memory means the user chose it, and only they know whether it must
/// outlive a reboot.
fn session_check(state: &Place, session_dir_set: bool) -> Check {
    if !state.ram_backed {
        return Check::ok(
            "session_storage",
            format!(
                "Presets, saved sign-ins and the storage ledger are in {}.",
                state.described()
            ),
        );
    }
    let mut check = Check::ok(
        "session_storage",
        format!(
            "Presets, saved sign-ins and the storage ledger are in memory in {} and are cleared at reboot.",
            state.described()
        ),
    );
    check.fix = Some(if session_dir_set {
        "To keep them, unset CAPTUREFAB_SESSION_DIR.".into()
    } else if cfg!(target_os = "linux") {
        "To keep them, point XDG_STATE_HOME at a folder on disk.".into()
    } else {
        "To keep them, set CAPTUREFAB_SESSION_DIR to a folder on disk.".into()
    });
    check
}

/// What version 0.5 kept in the state directory for the desktop app,
/// hubs, destinations, the assistant, resource limits and update checks.
const LEFTOVERS: [&str; 12] = [
    "gui.prefs.json",
    "hubs.json",
    "hubs.lock",
    "destinations.json",
    "destinations.lock",
    "assistant.json",
    "assistant.lock",
    "assistant",
    "resources.json",
    "resources.lock",
    "updates.json",
    "updates.lock",
];

/// Leftovers of version 0.5, with the command that removes them.
fn leftovers_check(leftovers: &[PathBuf]) -> Option<Check> {
    if leftovers.is_empty() {
        return None;
    }
    let names = leftovers
        .iter()
        .map(|path| crate::hint::shell_quote(&path.display().to_string()))
        .collect::<Vec<_>>();
    let fix = if cfg!(windows) {
        format!("Remove-Item -Recurse -Force {}", names.join(","))
    } else {
        format!("rm -r {}", names.join(" "))
    };
    Some(Check::warn(
        "leftover_state",
        format!(
            "{} file{} from Capturefab 0.5 that nothing reads any more (desktop, hubs, destinations, assistant or update settings).",
            leftovers.len(),
            if leftovers.len() == 1 { "" } else { "s" }
        ),
        fix,
    ))
}

/// A runtime or state directory that sessions refuse to use.
fn private_dirs_check(problems: &[(PathBuf, String)]) -> Option<Check> {
    if problems.is_empty() {
        return None;
    }
    let messages: Vec<&str> = problems.iter().map(|(_, why)| why.as_str()).collect();
    let foreign = problems
        .iter()
        .any(|(_, why)| why.contains("belongs to another user") || why.contains("real directory"));
    let dirs: Vec<String> = problems
        .iter()
        .map(|(dir, _)| shell_word(&dir.display().to_string()))
        .collect();
    Some(Check::warn(
        "private_dirs",
        format!(
            "Sessions, captures and discovery cannot start: {}.",
            messages.join("; ")
        ),
        if foreign {
            "Set CAPTUREFAB_SESSION_DIR to a folder of your own.".into()
        } else {
            format!(
                "chmod 700 {}  # or set CAPTUREFAB_SESSION_DIR to another folder",
                dirs.join(" ")
            )
        },
    ))
}
/// `word` quoted for a POSIX shell when it needs it.
fn shell_word(word: &str) -> String {
    if word
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@".contains(&b))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Wired adapters a GigE Vision camera could use. Never a warning: most
/// computers have none and need none, and `serve` prints every warning.
fn camera_network_check(interfaces: &[NetworkInterface]) -> Check {
    let wired: Vec<String> = interfaces
        .iter()
        .filter(|interface| {
            interface.kind == InterfaceKind::Wired
                && interface.up != Some(false)
                && !interface.address.is_loopback()
        })
        .map(|interface| {
            let prefix = u32::from(interface.netmask).count_ones();
            let mut text = format!("{} {}/{prefix}", interface.name, interface.address);
            match interface.mtu {
                Some(mtu) if mtu > 1500 => text.push_str(&format!(", MTU {mtu}")),
                Some(mtu) => text.push_str(&format!(", MTU {mtu} (no jumbo frames)")),
                None => {}
            }
            if interface.address.is_link_local() {
                text.push_str(", link-local (used by cameras without DHCP)");
            }
            text
        })
        .collect();
    Check::ok(
        "camera_network",
        if wired.is_empty() {
            "No wired Ethernet adapter is up. GigE Vision cameras need one; webcams, USB3 and network streams do not.".into()
        } else {
            format!("Wired Ethernet for GigE Vision: {}.", wired.join("; "))
        },
    )
}

/// Whether FFmpeg, which every webcam, stream, file and recording goes
/// through, starts. `dev` is a development build, which embeds whatever
/// `scripts/build-ffmpeg.sh` left.
fn ffmpeg_check(health: &Result<FfmpegHealth, String>, overridden: bool, dev: bool) -> Check {
    match health {
        Ok(health) => {
            let named = match health
                .version
                .strip_prefix("ffmpeg version ")
                .and_then(|rest| rest.split_whitespace().next())
            {
                Some(version) => format!("FFmpeg {version}"),
                None => "FFmpeg".into(),
            };
            let from = if health.source == "bundled" {
                "bundled".to_owned()
            } else {
                format!("{}={}", health.source, health.path)
            };
            Check::ok(
                "ffmpeg",
                format!("{named} ({from}) runs; webcams, RTSP/SRT, files and recording use it."),
            )
        }
        Err(error) => Check::warn(
            "ffmpeg",
            format!(
                "Webcams, RTSP/SRT, files and recording need FFmpeg, which does not run: {}.",
                error.trim_end_matches('.')
            ),
            if overridden {
                "Set CAPTUREFAB_FFMPEG to the absolute path of an FFmpeg executable, or unset it to use the bundled one.".into()
            } else if dev {
                "Run scripts/build-ffmpeg.sh and rebuild, or set CAPTUREFAB_FFMPEG to an FFmpeg executable.".into()
            } else {
                "Reinstall Capturefab from https://capturefab.com/downloads.html, or set CAPTUREFAB_FFMPEG to an FFmpeg executable.".into()
            },
        ),
    }
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
    Some(Check::warn(
        "usb_access",
        format!(
            "No read/write access to USB3 Vision camera {}.",
            names.join(", ")
        ),
        usb_access_fix(),
    ))
}

/// The paste-ready command that gives this user read/write access to USB3
/// Vision cameras through the shipped udev rule. It uses sudo, so it is only
/// ever shown or copied, never run.
pub fn usb_access_fix() -> String {
    // One rule for the USB3 Vision class covers every camera plugged in
    // later. The note is a shell comment, so the whole line still pastes
    // and runs.
    format!(
        "printf '%s\\n' '{}' '{}' | sudo tee /etc/udev/rules.d/{UDEV_RULE_FILE} >/dev/null && sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=usb --action=add  # services also need their user in the plugdev group; see {LINE_COMPUTER}",
        UDEV_RULES[0], UDEV_RULES[1]
    )
}

/// How the error for a USB3 Vision camera this user may not open or claim
/// starts (see `transport::usb`).
pub const USB_ACCESS_DENIED: &str = "No permission to";

/// `message` with [`usb_access_fix`] on a line of its own (see
/// [`fix_line`]), so a caller can offer it on its own.
pub fn with_usb_access_fix(message: impl std::fmt::Display) -> String {
    format!("{message}\n{FIX_PREFIX}{}", usb_access_fix())
}

/// The error for a USB3 Vision camera Linux refused to open, at usbfs
/// `node`, as `transport::usb` reports it: permission denied, carrying the
/// fix.
pub fn usb_access_error(name: &str, node: &Path) -> anyhow::Error {
    crate::types::Categorized::error(
        crate::types::ErrorKind::PermissionDenied,
        with_usb_access_fix(format!(
            "{USB_ACCESS_DENIED} open USB3 Vision camera {name} ({}). Run `capturefab doctor` for the udev rule that fixes it.",
            node.display()
        )),
    )
}

/// What starts the line of an error message that carries a paste-ready fix,
/// as `doctor` prints it; see [`fix_line`].
pub const FIX_PREFIX: &str = "Fix: ";

/// The fix an error message carries on a line of its own, if any.
pub fn fix_line(message: &str) -> Option<&str> {
    message
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(FIX_PREFIX))
        .map(str::trim)
        .filter(|fix| !fix.is_empty())
}

/// Whether this user lacks read/write access to the usbfs `node`
/// (`/dev/bus/usb/BBB/DDD`). A permission check only: it opens nothing.
#[cfg(all(target_os = "linux", feature = "usb"))]
pub fn usb_access_denied(node: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(node.as_os_str().as_bytes()).is_ok_and(|path| {
        // SAFETY: a NUL-terminated path; access only checks permissions.
        let status = unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) };
        status != 0
    })
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
/// bookkeeping; its usage summary includes the journal's tail. Unknown
/// usage omits the check. Without a state directory nothing is stored yet,
/// and `doctor` does not create one to find that out.
fn ledger(state: &Path) -> (u64, Option<u64>) {
    let default = StoragePolicy::default().max_bytes;
    if !state.is_dir() {
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
            let accessible = !usb_access_denied(&node);
            let product = read(dir.join("product"));
            Some(UsbCamera {
                name: if product.is_empty() { device } else { product },
                node,
                accessible,
            })
        })
        .collect()
}
#[cfg(not(all(target_os = "linux", feature = "usb")))]
fn usb_cameras() -> Vec<UsbCamera> {
    Vec::new()
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
            "Presets, saved sign-ins and the storage ledger are in /home/pi/.local/state/capturefab (ext4)."
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
        assert_eq!(fix, "To keep them, unset CAPTUREFAB_SESSION_DIR.");
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
        let login = Path::new("/run/user/1000");
        let fallback = runtime_dir_check(login, LoginDir::NotLingering, &runtime, &state);
        assert_eq!(fallback.level, Level::Warn);
        assert_eq!(
            fallback.message,
            "XDG_RUNTIME_DIR is unset and this user does not linger (logind keeps /run/user/1000 only while they are logged in), so sessions are on disk in /home/pi/.cache/capturefab/sessions, while logins look for them in /run/user/1000/capturefab."
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
        assert!(fix.ends_with("/docs/line-computer.md"), "{fix}");
        // A lingering service that started before logind made the directory
        // needs only to wait for it.
        let early = runtime_dir_check(login, LoginDir::Missing, &runtime, &state);
        assert!(
            early
                .message
                .contains("and /run/user/1000 does not exist yet, so"),
            "{}",
            early.message
        );
        let fix = early.fix.unwrap();
        assert!(fix.starts_with("Give a service Wants= and After=user@1000.service"));
        assert!(!fix.contains("enable-linger"), "{fix}");
        let foreign = runtime_dir_check(login, LoginDir::NotPrivate, &runtime, &state);
        assert!(foreign.message.contains("is not private to this user"));
        assert!(
            foreign
                .fix
                .unwrap()
                .contains("sudo chown 1000 /run/user/1000 && sudo chmod 700 /run/user/1000")
        );
    }

    #[test]
    fn an_ssh_login_learns_where_a_service_keeps_its_sessions() {
        let runtime = place("/run/user/1000/capturefab", "tmpfs", 400 << 20);
        let service = Path::new("/home/pi/.cache/capturefab/sessions");
        let login = Path::new("/run/user/1000");
        let note = service_runtime_dir_check(service, login, LoginDir::NotLingering, &[], &runtime);
        assert_eq!(note.level, Level::Ok);
        assert_eq!(
            note.message,
            "Sessions are in /run/user/1000/capturefab. A service started without XDG_RUNTIME_DIR would keep its own in /home/pi/.cache/capturefab/sessions, where logins do not look, as this user does not linger (logind keeps /run/user/1000 only while they are logged in)."
        );
        let fix = note.fix.unwrap();
        assert!(
            fix.starts_with("Run `sudo loginctl enable-linger 1000`"),
            "{fix}"
        );
        assert!(
            fix.contains("`env -u XDG_RUNTIME_DIR capturefab doctor`"),
            "{fix}"
        );
        // Once such a service runs, the login is told where.
        let running = service_runtime_dir_check(
            service,
            login,
            LoginDir::NotLingering,
            &["line1".into()],
            &runtime,
        );
        assert_eq!(running.level, Level::Warn);
        assert!(
            running
                .message
                .starts_with("line1 runs from /home/pi/.cache/capturefab/sessions, where this login does not look"),
            "{}",
            running.message
        );
    }

    #[test]
    fn usb_check_names_inaccessible_cameras_and_their_udev_rule() {
        assert_eq!(usb_check(&[]), None);
        let camera = UsbCamera {
            name: "acA1920-155um".into(),
            node: "/dev/bus/usb/002/003".into(),
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
        let other = UsbCamera {
            name: "BFS-U3-16S2M".into(),
            node: "/dev/bus/usb/002/004".into(),
            accessible: false,
        };
        let check = usb_check(&[denied, other]).unwrap();
        assert_eq!(check.level, Level::Warn);
        for name in [
            "acA1920-155um (/dev/bus/usb/002/003)",
            "BFS-U3-16S2M (/dev/bus/usb/002/004)",
        ] {
            assert!(check.message.contains(name), "{}", check.message);
        }
        // One class rule, whatever the vendors, so a camera plugged in later
        // needs no fix of its own.
        let fix = check.fix.unwrap();
        assert_eq!(
            fix.matches("/etc/udev/rules.d/70-capturefab-usb3vision.rules")
                .count(),
            1,
            "{fix}"
        );
        assert!(
            fix.contains(r#"ENV{ID_USB_INTERFACES}=="*:ef0500:*""#),
            "{fix}"
        );
        assert!(
            !fix.contains("60-capturefab-") && !fix.contains("idVendor"),
            "{fix}"
        );
        assert!(fix.contains("udevadm trigger"), "{fix}");
        assert!(
            fix.ends_with("  # services also need their user in the plugdev group; see https://github.com/capturefab/capturefab/blob/main/docs/line-computer.md"),
            "{fix}"
        );
        // Pasted into a shell, it parses as commands; `sh -n` reads without running.
        #[cfg(unix)]
        {
            let parsed = std::process::Command::new("sh")
                .args(["-n", "-c", &fix])
                .output()
                .unwrap();
            assert!(
                parsed.status.success(),
                "{fix}: {}",
                String::from_utf8_lossy(&parsed.stderr)
            );
        }
    }

    fn adapter(
        name: &str,
        address: [u8; 4],
        prefix: u32,
        kind: InterfaceKind,
        mtu: u32,
    ) -> NetworkInterface {
        NetworkInterface {
            name: name.into(),
            address: address.into(),
            netmask: std::net::Ipv4Addr::from(u32::MAX << (32 - prefix)),
            mtu: Some(mtu),
            speed_mbps: Some(1000),
            kind,
            up: Some(true),
        }
    }

    #[test]
    fn camera_network_lists_wired_adapters_without_warning() {
        let wifi = adapter("en0", [192, 168, 1, 23], 24, InterfaceKind::Wireless, 1500);
        let vpn = adapter("utun3", [10, 8, 0, 2], 24, InterfaceKind::Other, 1380);
        let lo = adapter("lo0", [127, 0, 0, 1], 8, InterfaceKind::Loopback, 16384);
        let jumbo = adapter("en5", [192, 168, 10, 1], 24, InterfaceKind::Wired, 9000);
        let standard = adapter("en6", [169, 254, 7, 9], 16, InterfaceKind::Wired, 1500);
        let mut down = adapter("en7", [192, 168, 20, 1], 24, InterfaceKind::Wired, 1500);
        down.up = Some(false);
        let check = camera_network_check(&[wifi.clone(), jumbo, standard, down, vpn.clone(), lo]);
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            check.message,
            "Wired Ethernet for GigE Vision: en5 192.168.10.1/24, MTU 9000; en6 169.254.7.9/16, MTU 1500 (no jumbo frames), link-local (used by cameras without DHCP)."
        );
        let none = camera_network_check(&[wifi, vpn]);
        assert_eq!(none.level, Level::Ok);
        assert_eq!(none.fix, None);
        assert!(
            none.message.starts_with("No wired Ethernet adapter is up.")
                && !none.message.contains("capturefab network"),
            "{}",
            none.message
        );
    }

    #[test]
    fn ffmpeg_check_says_what_needs_it_and_how_to_get_it() {
        let bundled = Ok(FfmpegHealth {
            source: "bundled",
            path: "/home/pi/.cache/capturefab/ffmpeg-0123/ffmpeg".into(),
            version: "ffmpeg version 7.1.1 Copyright (c) 2000-2025 the FFmpeg developers".into(),
        });
        let check = ffmpeg_check(&bundled, false, false);
        assert_eq!(
            (check.level, check.message.as_str()),
            (
                Level::Ok,
                "FFmpeg 7.1.1 (bundled) runs; webcams, RTSP/SRT, files and recording use it."
            )
        );
        let chosen = Ok(FfmpegHealth {
            source: "CAPTUREFAB_FFMPEG",
            path: "/opt/ffmpeg/bin/ffmpeg".into(),
            version: "ffmpeg version n7.0".into(),
        });
        assert_eq!(
            ffmpeg_check(&chosen, true, false).message,
            "FFmpeg n7.0 (CAPTUREFAB_FFMPEG=/opt/ffmpeg/bin/ffmpeg) runs; webcams, RTSP/SRT, files and recording use it."
        );
        let missing =
            Err("cannot launch FFmpeg: No such file or directory (os error 2)".to_owned());
        let check = ffmpeg_check(&missing, true, false);
        assert_eq!(check.level, Level::Warn);
        assert_eq!(
            check.message,
            "Webcams, RTSP/SRT, files and recording need FFmpeg, which does not run: cannot launch FFmpeg: No such file or directory (os error 2)."
        );
        assert!(check.fix.unwrap().contains("unset it"));
        assert!(
            ffmpeg_check(&missing, false, true)
                .fix
                .unwrap()
                .starts_with("Run scripts/build-ffmpeg.sh")
        );
        assert!(
            ffmpeg_check(&missing, false, false)
                .fix
                .unwrap()
                .starts_with("Reinstall Capturefab")
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_shared_session_directory_warns_with_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("capturefab-doctor-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let problem = crate::ipc::check_private_dir(&dir).unwrap_err();
        let check = private_dirs_check(&[(dir.clone(), format!("{problem:#}"))]).unwrap();
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.message.contains("must be private"),
            "{}",
            check.message
        );
        let fix = check.fix.unwrap();
        assert!(
            fix.starts_with(&format!("chmod 700 {}", dir.display())),
            "{fix}"
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(crate::ipc::check_private_dir(&dir).is_ok());
        assert_eq!(private_dirs_check(&[]), None);
        let foreign = private_dirs_check(&[(
            dir.clone(),
            "session directory /tmp/x belongs to another user".into(),
        )])
        .unwrap();
        assert!(foreign.fix.unwrap().contains("CAPTUREFAB_SESSION_DIR"));
        assert_eq!(shell_word("/tmp/a b"), "'/tmp/a b'");
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn the_shipped_udev_rule_is_the_one_doctor_writes() {
        let shipped = include_str!("../assets/udev/70-capturefab-usb3vision.rules");
        let rules: Vec<&str> = shipped
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .collect();
        assert_eq!(rules, UDEV_RULES);
        // doctor and the USB open error offer the same command.
        assert_eq!(
            fix_line(&with_usb_access_fix("x")),
            Some(usb_access_fix().as_str())
        );
    }

    #[test]
    fn a_refused_camera_is_permission_denied_and_keeps_its_fix_across_processes() {
        let error = usb_access_error("acA1920-155um", Path::new("/dev/bus/usb/002/003"));
        assert_eq!(crate::cli::error_code(&error).1, 3);
        let code = crate::types::error_transport_code(&error);
        assert_eq!(code, Some("permission_denied"));
        // As a worker reply carries it to the coordinator and the CLI.
        let rebuilt =
            crate::types::error_from_tag(crate::types::error_tag(&error), format!("{error:#}"));
        assert_eq!(crate::cli::error_code(&rebuilt).1, 3);
        let message = format!("{rebuilt:#}");
        assert_eq!(
            message.lines().next(),
            Some(
                "No permission to open USB3 Vision camera acA1920-155um (/dev/bus/usb/002/003). Run `capturefab doctor` for the udev rule that fixes it."
            )
        );
        assert_eq!(fix_line(&message), Some(usb_access_fix().as_str()));
    }

    #[test]
    fn a_fix_line_is_read_back_from_an_error_message() {
        assert_eq!(fix_line("camera busy"), None);
        assert_eq!(
            fix_line("connect: no access\nFix: sudo true"),
            Some("sudo true")
        );
        assert_eq!(fix_line("no access\nFix:   "), None);
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
            "host":{"cpu_cores":4},
            "checks":[
                Check::ok("ffmpeg", "FFmpeg 8.0 (bundled) runs; webcams, RTSP/SRT, files and recording use it.".into()),
                receive_buffer_check(212_992),
            ],
        });
        let mut out = Vec::new();
        print_checklist(&mut out, &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with("Capturefab 0.2.2 on linux aarch64 · "),
            "{text}"
        );
        assert_eq!(
            lines[1],
            "ok    FFmpeg 8.0 (bundled) runs; webcams, RTSP/SRT, files and recording use it."
        );
        assert!(lines[2].starts_with("warn  net.core.rmem_max is 208.0 KiB"));
        assert!(lines[3].starts_with("      Fix: sudo sysctl -w"));
        assert!(
            lines[4].contains("#host-checks-and-efficiency-mode"),
            "{text}"
        );
        // What plain doctor leaves to --json.
        assert!(lines[4].contains("features, folders and FFmpeg"), "{text}");
        // Linux hosts also get the service guide that walks through these fixes.
        assert!(lines[5].ends_with("/docs/line-computer.md"), "{text}");
        assert_eq!(lines.len(), 6);
        let mut out = Vec::new();
        let mut mac = report.clone();
        mac["os"] = json!("macos");
        print_checklist(&mut out, &mac).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 5);
    }
}
