//! Host checks for `doctor`: where sessions, state and temporary files live,
//! kernel limits that cap camera throughput and the free space captures
//! have, each turned into a check with a copyable fix.
#![allow(unsafe_code)]

#[cfg(any(target_os = "linux", test))]
use crate::ipc::LoginDir;
use crate::media::FfmpegHealth;
use crate::network::{InterfaceKind, NetworkInterface};
use crate::types::human_bytes;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Where the checks are explained.
const GUIDE: &str = "https://capturefab.com/docs.html#doctor";
/// Running Capturefab as a Linux service, with these fixes in context.
const LINE_COMPUTER: &str = "https://capturefab.com/guides/line-computer.md";
/// The udev rule Linux archives ship in share/udev/rules.d, and the fix for
/// `usb_access` writes: the logged-in seat user and the plugdev group may
/// open any device with a USB3 Vision interface (class ef/05/00).
/// The first rule imports usb_id, which sets ID_USB_INTERFACES, as not every
/// distribution has by the time rules sorted at 70 run.
#[cfg(any(feature = "usb", test))]
const UDEV_RULES: [&str; 2] = [
    r#"SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", IMPORT{builtin}="usb_id""#,
    r#"SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ENV{ID_USB_INTERFACES}=="*:ef0500:*", MODE="0660", GROUP="plugdev", TAG+="uaccess""#,
];
#[cfg(any(feature = "usb", test))]
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
#[cfg(any(all(target_os = "linux", feature = "usb"), test))]
#[derive(Debug, Clone, PartialEq)]
struct UsbCamera {
    name: String,
    node: PathBuf,
    accessible: bool,
}

/// What the checks look at, gathered once.
struct Facts {
    /// Where sessions are found, with their short-lived files.
    #[cfg(any(target_os = "linux", test))]
    runtime: Place,
    /// Presets, saved sign-ins and sessions' jobs.
    state: Place,
    /// CAPTUREFAB_SESSION_DIR chose both.
    session_dir_set: bool,
    /// `/run/user/<uid>` when the runtime directory fell back to disk
    /// without it, and why.
    #[cfg(any(target_os = "linux", test))]
    missed_runtime_dir: Option<(PathBuf, LoginDir)>,
    /// From a login that has XDG_RUNTIME_DIR: where a service without it
    /// keeps sessions instead.
    #[cfg(any(target_os = "linux", test))]
    service_runtime_dir: Option<ServiceDir>,
    limits: Limits,
    #[cfg(any(all(target_os = "linux", feature = "usb"), test))]
    usb: Vec<UsbCamera>,
    /// Where captures go without -o: this folder.
    captures: Place,
    /// The free space writes leave on that disk, or why the setting is
    /// wrong.
    floor: Result<u64, String>,
    /// IPv4 adapters, for the full report only: `serve`'s warnings never
    /// need them, and on Windows listing them runs PowerShell.
    interfaces: Option<Vec<NetworkInterface>>,
    /// Whether FFmpeg runs, or why not, for the full report only: `serve`
    /// would otherwise start FFmpeg (extracting it first) on every launch.
    ffmpeg: Option<Result<FfmpegHealth, String>>,
    /// CAPTUREFAB_FFMPEG chose FFmpeg.
    ffmpeg_overridden: bool,
    /// Files in the state directory that version 0.5 wrote and nothing
    /// reads any more, its storage ledger among them.
    leftovers: Vec<PathBuf>,
    /// The runtime or state directory, where it exists but sessions would
    /// refuse it, and why.
    private_dirs: Vec<(PathBuf, String)>,
}
/// Where a service started without XDG_RUNTIME_DIR keeps sessions, out of
/// this login's sight, the login runtime directory it passes over and why,
/// and the sessions running there.
#[cfg(any(target_os = "linux", test))]
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
        let mut private_dirs = Vec::new();
        for dir in [crate::ipc::runtime_dir(), state.clone()] {
            if dir.exists()
                && !private_dirs.iter().any(|(seen, _)| *seen == dir)
                && let Err(error) = crate::ipc::check_private_dir(&dir)
            {
                private_dirs.push((dir, format!("{error:#}")));
            }
        }
        let leases = std::fs::read_dir(&state)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("storage-") && name.ends_with(".lease"))
            });
        let mut leftovers: Vec<PathBuf> = LEFTOVERS
            .iter()
            .map(|name| state.join(name))
            .filter(|path| path.symlink_metadata().is_ok())
            .chain(leases)
            .collect();
        leftovers.sort();
        let captures = Place::of(std::env::current_dir().unwrap_or_else(|_| ".".into()));
        let floor = crate::storage::floor(captures.total_bytes.unwrap_or(0))
            .map_err(|error| format!("{error:#}"));
        Self {
            leftovers,
            interfaces: report.then(|| crate::network::interfaces().unwrap_or_default()),
            ffmpeg: report
                .then(|| crate::media::ffmpeg_health().map_err(|error| format!("{error:#}"))),
            ffmpeg_overridden: std::env::var_os("CAPTUREFAB_FFMPEG").is_some(),
            private_dirs,
            #[cfg(any(target_os = "linux", test))]
            runtime: Place::of(crate::ipc::runtime_dir()),
            state: Place::of(state),
            session_dir_set: std::env::var_os("CAPTUREFAB_SESSION_DIR")
                .is_some_and(|dir| !dir.is_empty()),
            #[cfg(any(target_os = "linux", test))]
            missed_runtime_dir: crate::ipc::missed_runtime_dir(),
            #[cfg(any(target_os = "linux", test))]
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
            #[cfg(any(all(target_os = "linux", feature = "usb"), test))]
            usb: usb_cameras(),
            captures,
            floor,
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
            "shell": cfg!(feature = "shell"),
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
    #[cfg(any(target_os = "linux", test))]
    checks.extend(
        facts
            .missed_runtime_dir
            .as_ref()
            .map(|(dir, login)| runtime_dir_check(dir, *login, &facts.runtime, &facts.state)),
    );
    #[cfg(any(target_os = "linux", test))]
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
            .map(|health| ffmpeg_check(health, facts.ffmpeg_overridden, crate::media::bundled())),
    );
    checks.extend(leftovers_check(&facts.leftovers));
    checks.extend(facts.limits.rmem_max.map(receive_buffer_check));
    #[cfg(any(all(target_os = "linux", feature = "usb"), test))]
    checks.extend(usb_check(&facts.usb));
    checks.extend(free_space_check(&facts.captures, &facts.floor));
    checks
}

/// Without XDG_RUNTIME_DIR, Capturefab uses the login's runtime directory
/// for a lingering user, so a service and an SSH shell find the same
/// sessions. Otherwise logind removes it at logout, taking a service's
/// session with it, so Capturefab falls back to disk.
#[cfg(any(target_os = "linux", test))]
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
            "{fix} Presets, saved sign-ins and sessions' jobs stay in {state} either way. A service unit that does this: {LINE_COMPUTER}"
        ),
    )
}

/// The same problem seen from a login, such as SSH, that has
/// XDG_RUNTIME_DIR: a service started without it keeps its sessions in
/// `service` instead, out of this login's sight. A warning once such a
/// session runs, otherwise a note, as most logins never start a service.
#[cfg(any(target_os = "linux", test))]
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
#[cfg(any(target_os = "linux", test))]
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
                "Presets, saved sign-ins and sessions' jobs are in {}.",
                state.described()
            ),
        );
    }
    let mut check = Check::ok(
        "session_storage",
        format!(
            "Presets, saved sign-ins and sessions' jobs are in memory in {} and are cleared at reboot.",
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
/// hubs, destinations, the assistant, resource limits, update checks and
/// the storage ledger, beside the ledger's `storage-*.lease` files.
const LEFTOVERS: [&str; 17] = [
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
    "storage.json",
    "storage.journal",
    "storage.lock",
    "storage-policy.json",
    "storage-summary.json",
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
            "{} file{} from Capturefab 0.5 that nothing reads any more (desktop, hubs, destinations, assistant, update settings or the storage ledger, whose budget and retention are gone).",
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
/// through, starts, and where it was found. `bundled` is a build with
/// FFmpeg built in.
fn ffmpeg_check(health: &Result<FfmpegHealth, String>, overridden: bool, bundled: bool) -> Check {
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
            let from = match health.source {
                "bundled" => "bundled".to_owned(),
                "next_to_executable" => format!("next to capturefab: {}", health.path),
                "path" => format!("on PATH: {}", health.path),
                source => format!("{source}={}", health.path),
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
                "Set CAPTUREFAB_FFMPEG to the absolute path of an FFmpeg executable, or unset it to use the one Capturefab finds.".into()
            } else if bundled {
                "Reinstall Capturefab from https://capturefab.com/downloads.html, or set CAPTUREFAB_FFMPEG to an FFmpeg executable.".into()
            } else {
                let hint = crate::media::ffmpeg_hint();
                let mut fix = hint[..1].to_uppercase() + &hint[1..];
                fix.push('.');
                fix
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

#[cfg(any(all(target_os = "linux", feature = "usb"), test))]
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
#[cfg(any(feature = "usb", test))]
pub fn usb_access_fix() -> String {
    // One rule for the USB3 Vision class covers every camera plugged in
    // later. The note is a shell comment, so the whole line still pastes
    // and runs.
    format!(
        "printf '%s\\n' '{}' '{}' | sudo tee /etc/udev/rules.d/{UDEV_RULE_FILE} >/dev/null && sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=usb --action=add  # services also need their user in the plugdev group; see {LINE_COMPUTER}",
        UDEV_RULES[0], UDEV_RULES[1]
    )
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

/// Whether captures in this folder have room: its disk's free space
/// against the floor every write leaves free.
fn free_space_check(place: &Place, floor: &Result<u64, String>) -> Option<Check> {
    let free = place.free_bytes?;
    let path = place.path.display();
    let floor = match floor {
        Ok(floor) => *floor,
        Err(error) => {
            return Some(Check::warn(
                "free_space",
                format!("{error}."),
                format!(
                    "Set {} to bytes or a size such as 2G, or unset it for the default.",
                    crate::storage::MIN_FREE_ENV
                ),
            ));
        }
    };
    Some(if free < floor {
        Check::warn(
            "free_space",
            format!(
                "{} free on the disk of {path}, below the {} Capturefab leaves free, so captures there stop with storage_full.",
                human_bytes(free),
                human_bytes(floor)
            ),
            format!(
                "Free some space, capture to another disk with -o, or lower {} (0 turns the floor off).",
                crate::storage::MIN_FREE_ENV
            ),
        )
    } else {
        Check::ok(
            "free_space",
            format!(
                "{} free on the disk of {path}; captures leave {} free.",
                human_bytes(free),
                human_bytes(floor)
            ),
        )
    })
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
#[cfg(all(test, not(all(target_os = "linux", feature = "usb"))))]
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
    fn the_old_storage_ledger_is_a_leftover_with_its_leases() {
        let state = crate::ipc::state_dir();
        crate::ipc::ensure_private_dir(&state).unwrap();
        let files =
            ["storage.journal", "storage-0123456789abcdef.lease"].map(|name| state.join(name));
        for file in &files {
            std::fs::write(file, b"").unwrap();
        }
        let leftovers = Facts::gather(false).leftovers;
        for file in &files {
            let _ = std::fs::remove_file(file);
        }
        for file in &files {
            assert!(leftovers.contains(file), "{leftovers:?}");
        }
        let check = leftovers_check(&leftovers).unwrap();
        assert!(
            check.message.contains("storage ledger"),
            "{}",
            check.message
        );
        assert!(check.fix.unwrap().contains("storage.journal"));
    }

    #[test]
    fn free_space_below_the_floor_warns() {
        let card = place("/home/pi/captures", "ext4", 300 << 20);
        let floor = Ok(512 << 20);
        let check = free_space_check(&card, &floor).unwrap();
        assert_eq!(check.level, Level::Warn);
        assert_eq!(
            check.message,
            "300.0 MiB free on the disk of /home/pi/captures, below the 512.0 MiB Capturefab leaves free, so captures there stop with storage_full."
        );
        assert!(check.fix.unwrap().contains("CAPTUREFAB_MIN_FREE"));
        let roomy = place("/data", "xfs", 100 << 30);
        assert_eq!(
            free_space_check(&roomy, &floor).unwrap().message,
            "100.00 GiB free on the disk of /data; captures leave 512.0 MiB free."
        );
        // The floor turned off never warns; a disk that does not say is not
        // checked; a wrong setting is.
        assert_eq!(free_space_check(&card, &Ok(0)).unwrap().level, Level::Ok);
        let unknown = Place {
            free_bytes: None,
            ..card.clone()
        };
        assert_eq!(free_space_check(&unknown, &floor), None);
        let wrong = free_space_check(&card, &Err("CAPTUREFAB_MIN_FREE must be bytes".into()));
        assert_eq!(wrong.unwrap().level, Level::Warn);
    }

    #[test]
    fn state_on_disk_is_the_default_and_memory_explains_how_to_keep_it() {
        let disk = place("/home/pi/.local/state/capturefab", "ext4", 6 << 30);
        let check = session_check(&disk, false);
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            check.message,
            "Presets, saved sign-ins and sessions' jobs are in /home/pi/.local/state/capturefab (ext4)."
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
        assert!(
            fix.ends_with("https://capturefab.com/guides/line-computer.md"),
            "{fix}"
        );
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
            fix.ends_with("  # services also need their user in the plugdev group; see https://capturefab.com/guides/line-computer.md"),
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
        let check = ffmpeg_check(&bundled, false, true);
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
        let found = Ok(FfmpegHealth {
            source: "path",
            path: "/usr/bin/ffmpeg".into(),
            version: "ffmpeg version 6.1.1-3ubuntu5".into(),
        });
        assert_eq!(
            ffmpeg_check(&found, false, false).message,
            "FFmpeg 6.1.1-3ubuntu5 (on PATH: /usr/bin/ffmpeg) runs; webcams, RTSP/SRT, files and recording use it."
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
                .starts_with("Reinstall Capturefab")
        );
        let fix = ffmpeg_check(&missing, false, false).fix.unwrap();
        assert!(fix.starts_with("Install FFmpeg with "), "{fix}");
        assert!(
            fix.contains("CAPTUREFAB_FFMPEG") && fix.ends_with('.'),
            "{fix}"
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
            lines[4].contains("https://capturefab.com/docs.html#doctor"),
            "{text}"
        );
        // What plain doctor leaves to --json.
        assert!(lines[4].contains("features, folders and FFmpeg"), "{text}");
        // Linux hosts also get the service guide that walks through these fixes.
        assert!(
            lines[5].ends_with("https://capturefab.com/guides/line-computer.md"),
            "{text}"
        );
        assert_eq!(lines.len(), 6);
        let mut out = Vec::new();
        let mut mac = report.clone();
        mac["os"] = json!("macos");
        print_checklist(&mut out, &mac).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 5);
    }
}
