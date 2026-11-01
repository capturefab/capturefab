//! `capturefab session`: start a session in the background, stop it, and
//! see what it holds. `--keep` and `--background` start one the same way.
use super::{CONNECT_TIMEOUT, Global, args::CamerasArg, output::Report};
use crate::{
    ipc,
    session::SessionCommand as Request,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Subcommand)]
pub enum SessionAction {
    /// Start a session in the background; it ends after --idle unused
    #[command(
        after_help = "Examples:\n  capturefab session start -c sim:0\n  capturefab session start --name line1 -c gige:00c0df123456 --live --idle 8h"
    )]
    Start(StartArgs),
    /// Stop a session once its recordings and captures are finished
    #[command(after_help = "Examples:\n  capturefab session stop\n  capturefab session stop line1")]
    Stop {
        /// The session (default: the only one running)
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// A session's cameras, their state and jobs
    #[command(
        after_help = "Examples:\n  capturefab session status\n  capturefab --json session status line1"
    )]
    Status {
        /// The session (default: the only one running)
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// The running sessions
    #[command(
        after_help = "Examples:\n  capturefab session list\n  capturefab --json session list"
    )]
    List,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Session name other commands pass to --session
    #[arg(long, default_value = "default", value_name = "NAME")]
    pub name: String,
    #[command(flatten)]
    pub cameras: CamerasArg,
    /// Keep the cameras streaming, so captures take the newest frame at once
    #[arg(long)]
    pub live: bool,
    /// End after this long with no requests and no running jobs
    #[arg(long, default_value = "30m", value_name = "DUR", value_parser = super::args::duration)]
    pub idle: Duration,
}

/// The largest ready line a starting session writes.
const MAX_READY: u64 = 64 * 1024;
/// Session logs are rotated at this size, keeping one older file.
const MAX_LOG: u64 = 10 * 1024 * 1024;
/// How long `session stop` waits for recordings to finish and the session
/// to exit.
const STOP_WAIT: Duration = Duration::from_secs(30);

pub fn run(global: &Global, action: SessionAction) -> Result<Report> {
    match action {
        SessionAction::Start(args) => start(global, args),
        SessionAction::Stop { name } => stop(global, name),
        SessionAction::Status { name } => status(global, name),
        SessionAction::List => list(),
    }
}

fn start(global: &Global, args: StartArgs) -> Result<Report> {
    ipc::validate_name(&args.name).map_err(super::input)?;
    if args.idle.is_zero() {
        return Err(Usage("--idle must be longer than 0".into()).into());
    }
    let cameras = args
        .cameras
        .cameras
        .iter()
        .map(|selector| super::caller_selector(selector))
        .collect::<Result<Vec<_>>>()?;
    let mut warnings = Vec::new();
    if ipc::runtime_dir_ends_at_logout() {
        warnings.push(format!(
            "{} is removed when you log out, and the session with it; run `loginctl enable-linger` to keep it",
            ipc::runtime_dir().display()
        ));
    }
    let _turn = ipc::start_turn(&args.name)?;
    // A running session of that name is kept: it takes the cameras asked
    // for and stays at least the idle time asked for.
    let (mut value, started) = match ipc::running_session(&args.name) {
        Some(descriptor) => {
            ipc::keep(&args.name, args.idle)?;
            for selector in &cameras {
                ipc::call(
                    &args.name,
                    Request::connect(selector.clone(), super::millis(CONNECT_TIMEOUT)),
                    crate::session::default_request_timeout(&Request::Status),
                )?;
            }
            let descriptor = ipc::read_descriptor(&args.name).unwrap_or(descriptor);
            if args.live {
                // The cameras asked for, or without -c every one it holds.
                let live = if cameras.is_empty() {
                    descriptor
                        .cameras
                        .iter()
                        .map(|camera| camera.id.clone())
                        .collect()
                } else {
                    cameras.clone()
                };
                for camera in &live {
                    ipc::call_to(
                        &args.name,
                        Some(camera),
                        Request::Start,
                        crate::session::default_request_timeout(&Request::Start),
                    )?;
                }
            }
            (
                json!({
                    "session": descriptor.name,
                    "pid": descriptor.pid,
                    "cameras": descriptor.cameras.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                }),
                false,
            )
        }
        None => (
            detach(&args.name, &cameras, args.live, args.idle, &mut warnings)?,
            true,
        ),
    };
    value["started"] = json!(started);
    value["log"] = json!(log_path(&args.name));
    if !warnings.is_empty() {
        value["warnings"] = json!(warnings);
    }
    let out = global.output();
    for warning in &warnings {
        out.note(format_args!("capturefab: {warning}"));
    }
    let name = &args.name;
    let text = format!(
        "Session {name} {} (pid {}). Commands use it for its cameras; stop it with capturefab session stop {name}.\n",
        if started { "started" } else { "is running" },
        value["pid"]
    );
    Ok(Report::json(value).text(text))
}

/// Make sure session `name` runs, holding `selector` if one is given, and
/// stays at least `idle` after its last use: `--keep` and `--background`.
pub(super) fn ensure(
    name: &str,
    selector: Option<&str>,
    idle: Duration,
    global: &Global,
) -> Result<()> {
    // Commands that need the session at once (the cameras of one capture,
    // or several agents' calls) take turns: one starts it, the rest use it.
    let _turn = ipc::start_turn(name)?;
    if ipc::running_session(name).is_some() {
        return ipc::keep(name, idle);
    }
    let cameras = selector.map(str::to_owned).into_iter().collect::<Vec<_>>();
    let mut warnings = Vec::new();
    match detach(name, &cameras, false, idle, &mut warnings) {
        Ok(_) => {
            for warning in warnings {
                global.output().note(format_args!("capturefab: {warning}"));
            }
            Ok(())
        }
        // A session started another way, such as serve, took the name.
        Err(_) if ipc::running_session(name).is_some() => ipc::keep(name, idle),
        Err(error) => Err(error),
    }
}

/// Session `name`'s descriptor when it runs, or `session_not_found`; what a
/// session that was killed left under that name is removed.
fn running(name: &str) -> Result<ipc::Descriptor> {
    let descriptor = ipc::read_descriptor(name)?;
    if ipc::running_session(name).is_some() {
        return Ok(descriptor);
    }
    let _ = ipc::remove_stale(name);
    Err(Categorized::new(
        ErrorKind::SessionNotFound,
        format!("session {name} is not running; it ended without cleaning up, and what it left is removed"),
    )
    .hint(format!("start it again with capturefab session start --name {name}"))
    .into())
}

/// Where a detached session's output goes.
pub(super) fn log_path(name: &str) -> PathBuf {
    ipc::state_dir()
        .join("sessions")
        .join(format!("{name}.log"))
}

/// The session's log, private, with an older one kept once it grows past
/// `MAX_LOG`.
fn open_log(name: &str) -> Result<std::fs::File> {
    let path = log_path(name);
    let dir = path.parent().context("session log folder")?;
    ipc::ensure_private_dir(dir)?;
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_LOG) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)
        .with_context(|| format!("cannot open the session log {}", path.display()))
}

/// Keep a running session's log near `MAX_LOG`: once it is larger, copy it
/// over the older one and empty it. The session and its camera workers
/// append to it, so they go on writing at its new end; what they write
/// between the copy and the emptying is lost.
pub(super) fn rotate_log(name: &str) {
    rotate(&log_path(name), MAX_LOG);
}
fn rotate(path: &std::path::Path, most: u64) {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > most)
        && std::fs::copy(path, path.with_extension("log.1")).is_ok()
        && let Ok(file) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let _ = file.set_len(0);
    }
}

/// Start `serve` as a session of its own, detached from this terminal, and
/// wait for it to say it is ready: its ready document, or its failure.
fn detach(
    name: &str,
    cameras: &[String],
    live: bool,
    idle: Duration,
    warnings: &mut Vec<String>,
) -> Result<Value> {
    let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE")
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)?;
    let log = open_log(name)?;
    let (reader, writer) = std::io::pipe().context("session ready pipe")?;
    let mut command = Command::new(executable);
    command
        .args(["--json", "serve", "--name", name, "--idle"])
        .arg(super::args::duration_text(idle));
    for camera in cameras {
        command.args(["-c", camera]);
    }
    if live {
        command.arg("--live");
    }
    for variable in [
        "CAPTUREFAB_SESSION",
        "CAPTUREFAB_CAMERA",
        "CAPTUREFAB_KEEP",
        "CAPTUREFAB_JSON",
    ] {
        command.env_remove(variable);
    }
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = platform::spawn(&mut command, &writer, warnings)?;
    // Only the session holds the write end now: a session that dies
    // before it is ready closes the pipe.
    drop(writer);
    let wait = Duration::from_secs(30) + CONNECT_TIMEOUT * (cameras.len() as u32 + 1);
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("capturefab-ready".into())
        .spawn(move || {
            use std::io::{BufRead, Read};
            let mut line = Vec::new();
            let read = std::io::BufReader::new(reader)
                .take(MAX_READY)
                .read_until(b'\n', &mut line);
            let _ = sent.send(read.map(|_| line));
        })
        .context("session ready reader")?;
    let line = match received.recv_timeout(wait) {
        Ok(read) => read.context("reading the session's ready line")?,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Categorized::new(
                ErrorKind::Timeout,
                format!(
                    "session {name} did not start within {}",
                    super::args::duration_text(wait)
                ),
            )
            .hint(format!("see {}", log_path(name).display()))
            .into());
        }
    };
    let value = serde_json::from_slice::<Value>(&line).ok();
    match value {
        Some(value) if value.get("error").is_none() && value["session"].is_string() => {
            // The session runs on its own; nothing waits for it.
            drop(child);
            Ok(value)
        }
        Some(value) => {
            let _ = child.wait();
            let error = &value["error"];
            let tag = serde_json::from_value::<crate::types::ErrorTag>(error.clone()).ok();
            Err(crate::types::error_from_tag(
                tag,
                error["message"]
                    .as_str()
                    .unwrap_or("the session could not start")
                    .to_owned(),
            ))
        }
        None => {
            let status = child.wait().ok();
            Err(Categorized::new(
                ErrorKind::OperationFailed,
                format!(
                    "session {name} exited before it was ready{}",
                    status
                        .map(|status| format!(" ({status})"))
                        .unwrap_or_default()
                ),
            )
            .hint(format!("see {}", log_path(name).display()))
            .into())
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::{fd::AsRawFd, unix::process::CommandExt};

    /// Spawn `command` in a new Unix session, out of this terminal's reach,
    /// with `ready` inherited and named by `--ready-fd`.
    pub(super) fn spawn(
        command: &mut Command,
        ready: &std::io::PipeWriter,
        _warnings: &mut Vec<String>,
    ) -> Result<std::process::Child> {
        let fd = ready.as_raw_fd();
        command.args(["--ready-fd", &fd.to_string()]);
        // SAFETY: setsid and fcntl are async-signal-safe, as pre_exec
        // requires; the descriptor is this process's own pipe.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().context("start the session")
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{io::AsRawHandle, process::CommandExt};

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    /// Spawn `command` detached from this console and, when the job it runs
    /// in allows, from that job too, so closing the terminal or the tool
    /// that ran this leaves the session running.
    pub(super) fn spawn(
        command: &mut Command,
        ready: &std::io::PipeWriter,
        warnings: &mut Vec<String>,
    ) -> Result<std::process::Child> {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        let handle = ready.as_raw_handle();
        // SAFETY: the handle is this process's own pipe end.
        if unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
            == 0
        {
            return Err(std::io::Error::last_os_error()).context("session ready pipe");
        }
        command.args(["--ready-fd", &(handle as usize).to_string()]);
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
        command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                warnings.push(
                    "the job this runs in keeps its processes; the session ends with it".into(),
                );
                command.creation_flags(flags);
                command.spawn().context("start the session")
            }
            spawned => spawned.context("start the session"),
        }
    }
}

/// The session a command without a name is for: `--session`, else the only
/// one running.
fn named(global: &Global, name: Option<String>) -> Result<String> {
    if let Some(name) = name.or_else(|| global.session.clone()) {
        ipc::validate_name(&name).map_err(super::input)?;
        return Ok(name);
    }
    let sessions = ipc::sessions();
    match sessions.as_slice() {
        [session] => Ok(session.name.clone()),
        [] => Err(
            Categorized::new(ErrorKind::SessionNotFound, "no session is running")
                .hint("start one with capturefab session start -c CAMERA")
                .into(),
        ),
        _ => Err(Categorized::new(
            ErrorKind::Usage,
            format!(
                "{} sessions are running ({}); name one",
                sessions.len(),
                sessions
                    .iter()
                    .map(|session| session.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .details(json!({"sessions": sessions.iter().map(|s| s.name.clone()).collect::<Vec<_>>()}))
        .into()),
    }
}

fn stop(global: &Global, name: Option<String>) -> Result<Report> {
    let name = named(global, name)?;
    let descriptor = running(&name)?;
    let pid = descriptor.pid;
    let asked = ipc::stop(&descriptor);
    if let Err(error) = &asked {
        // A session that does not answer is ended by signal, once it is
        // known to be the one that holds this name.
        if !platform_stop::terminate(&name, pid) {
            return Err(crate::types::rephrase(
                anyhow::anyhow!("{error:#}"),
                format!("session {name} did not answer"),
            ));
        }
    }
    let deadline = Instant::now() + STOP_WAIT;
    while ipc::process_alive(pid) && ipc::running_session(&name).is_some() {
        if Instant::now() >= deadline {
            return Err(Categorized::new(
                ErrorKind::Timeout,
                format!(
                    "session {name} is still finishing after {}",
                    super::args::duration_text(STOP_WAIT)
                ),
            )
            .hint(format!("see {}", log_path(&name).display()))
            .into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = format!("Stopped session {name}.\n");
    Ok(Report::json(json!({"session": name, "pid": pid, "stopped": true})).text(text))
}

mod platform_stop {
    /// Send SIGTERM to session `name`'s process `pid` when it is that
    /// session: its name lock is held and the process is this program's
    /// serve. Whether it was sent.
    #[cfg(unix)]
    pub(super) fn terminate(name: &str, pid: u32) -> bool {
        let lock = crate::ipc::runtime_dir().join(format!("{name}.session.lock"));
        // Closing the file releases a lock this took.
        let held = std::fs::File::open(&lock).is_ok_and(|file| file.try_lock_shared().is_err());
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if !held || !serves(pid, name) {
            return false;
        }
        // SAFETY: kill sends a signal to the process checked above.
        unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
    }
    /// Whether process `pid` runs `capturefab serve` for `name`, as far as
    /// this system can tell.
    #[cfg(target_os = "linux")]
    fn serves(pid: libc::pid_t, name: &str) -> bool {
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
            let args = cmdline
                .split(|byte| *byte == 0)
                .map(String::from_utf8_lossy)
                .collect::<Vec<_>>();
            args.iter().any(|arg| arg == "serve") && args.iter().any(|arg| arg == name)
        })
    }
    #[cfg(target_os = "macos")]
    fn serves(pid: libc::pid_t, _name: &str) -> bool {
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: proc_pidpath writes at most the buffer's length.
        let length =
            unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if length <= 0 {
            return false;
        }
        path.truncate(length as usize);
        let theirs = std::path::PathBuf::from(String::from_utf8_lossy(&path).into_owned());
        std::env::current_exe().is_ok_and(|ours| theirs.file_name() == ours.file_name())
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    fn serves(_pid: libc::pid_t, _name: &str) -> bool {
        true
    }
    #[cfg(not(unix))]
    pub(super) fn terminate(_name: &str, _pid: u32) -> bool {
        false
    }
}

fn uptime(descriptor: &ipc::Descriptor) -> u64 {
    crate::scheduling::now_ms().saturating_sub(descriptor.started_ms) / 1000
}

fn status(global: &Global, name: Option<String>) -> Result<Report> {
    let name = named(global, name)?;
    let descriptor = running(&name)?;
    let state = ipc::call(&name, Request::Status, Duration::from_secs(10))?;
    let cameras = state["cameras"]
        .as_array()
        .into_iter()
        .flatten()
        .map(camera_state)
        .collect::<Vec<_>>();
    let mut text = format!(
        "Session {name} (pid {}, up {}):\n",
        descriptor.pid,
        crate::scheduling::format_duration(uptime(&descriptor) * 1000)
    );
    if cameras.is_empty() {
        text.push_str("  no cameras\n");
    }
    for camera in &cameras {
        let _ = writeln!(
            text,
            "  {}  {}{}",
            camera["id"].as_str().unwrap_or_default(),
            camera["state"].as_str().unwrap_or_default(),
            camera["error"]["message"]
                .as_str()
                .or(camera["last_error"].as_str())
                .map(|error| format!("  last error: {error}"))
                .unwrap_or_default()
        );
        for job in camera["jobs"].as_array().into_iter().flatten() {
            let _ = writeln!(text, "    {}", super::jobs::line(job));
        }
    }
    let value = json!({
        "session": name,
        "pid": descriptor.pid,
        "uptime_s": uptime(&descriptor),
        "cameras": cameras,
    });
    Ok(Report::json(value).text(text))
}

/// A camera as `session status` shows it.
fn camera_state(camera: &Value) -> Value {
    let state = if camera["exited"] == true {
        "error"
    } else if camera["forwarding"].is_string() {
        "recording"
    } else if camera["streaming"] == true {
        "streaming"
    } else {
        "idle"
    };
    let mut value = json!({
        "id": crate::media::redact_url(camera["info"]["id"].as_str().unwrap_or_default()),
        "state": state,
        "jobs": super::jobs::of_camera(camera),
    });
    if let Some(frames) = camera["frames"].as_u64().filter(|frames| *frames > 0) {
        value["frames"] = json!(frames);
        value["dropped"] = json!(camera["dropped"].as_u64().unwrap_or(0));
        value["lost"] = json!(camera["lost"].as_u64().unwrap_or(0));
        if let Some(incomplete) = camera["incomplete"].as_u64() {
            value["incomplete"] = json!(incomplete);
        }
        if let Some(resent) = camera["transport"]["resend_recovered"].as_u64() {
            value["resent"] = json!(resent);
        }
    }
    // Why the worker is gone, or could not answer: its reason, and for a
    // crash its exit, signal and last lines of stderr.
    if let Some(error) = camera["error"].as_object() {
        let mut shown = json!({
            "code": error.get("code").and_then(Value::as_str).unwrap_or("failed"),
            "message": crate::scrub::redact_urls(
                error.get("message").and_then(Value::as_str).unwrap_or_default()
            ),
        });
        if let Some(hint) = error.get("hint").and_then(Value::as_str) {
            shown["hint"] = json!(crate::scrub::redact_urls(hint));
        }
        if let Some(details) = error.get("details").filter(|details| details.is_object()) {
            let mut details = details.clone();
            redact_texts(&mut details);
            shown["details"] = details;
        }
        value["error"] = shown;
    } else if let Some(error) = camera["last_error"].as_str() {
        value["last_error"] = json!(crate::scrub::redact_urls(error));
    }
    if let Some(config) = camera["config_state"].as_str() {
        value["config_state"] = json!(config);
    }
    value
}

/// Redact URL credentials in every string of `value`, wherever in the text
/// they appear, as in a worker's stderr lines.
fn redact_texts(value: &mut Value) {
    match value {
        Value::String(text) => *text = crate::scrub::redact_urls(text),
        Value::Array(values) => values.iter_mut().for_each(redact_texts),
        Value::Object(fields) => fields.values_mut().for_each(redact_texts),
        _ => {}
    }
}

fn list() -> Result<Report> {
    let sessions = ipc::active_sessions()?
        .into_iter()
        .map(|descriptor| {
            json!({
                "session": descriptor.name,
                "pid": descriptor.pid,
                "uptime_s": uptime(&descriptor),
                "cameras": descriptor.cameras.iter().map(|camera| camera.id.clone()).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let mut text = String::new();
    if sessions.is_empty() {
        text.push_str(
            "No sessions are running. Start one with capturefab session start -c CAMERA.\n",
        );
    }
    for session in &sessions {
        let _ = writeln!(
            text,
            "{}  pid {}  {}",
            session["session"].as_str().unwrap_or_default(),
            session["pid"],
            session["cameras"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(Report::json(json!({ "sessions": sessions })).text(text))
}

/// Result fields, for `schema`.
pub const START_FIELDS: &[(&str, &str)] = &[
    ("session", "string — its name, for --session"),
    ("pid", "int"),
    ("cameras", "[string] — camera IDs it holds"),
    ("started", "bool — false when it was already running"),
    ("log", "string — where its output goes"),
    ("warnings", "[string]"),
];
pub const STOP_FIELDS: &[(&str, &str)] = &[
    ("session", "string"),
    ("pid", "int"),
    ("stopped", "bool — true once the process has exited"),
];
pub const STATUS_FIELDS: &[(&str, &str)] = &[
    ("session", "string"),
    ("pid", "int"),
    ("uptime_s", "int"),
    (
        "cameras",
        "[{id,state:idle|streaming|recording|error,error?:{code,message,hint?,details?:{exit?,signal?,stderr_tail?}},last_error?,config_state?:unknown,frames?,dropped?,lost?,incomplete?,resent?,jobs:[job]}] — job as jobs lists it; error: the camera's worker exited (code worker_crashed, with its exit status or signal and last lines of stderr) or did not answer, and the next command on the camera starts another; last_error: the worker's last logged error; lost: frames lost on the way, incomplete: frames the transport received incomplete, resent: packets the camera sent again, where it can tell; config_state unknown: it exited during a settings change, until settings are written again",
    ),
];
pub const LIST_FIELDS: &[(&str, &str)] =
    &[("sessions", "[{session,pid,uptime_s,cameras:[string]}]")];
pub const SERVE_FIELDS: &[(&str, &str)] = &[
    ("session", "string — its name, for --session"),
    ("pid", "int"),
    ("cameras", "[string] — camera IDs it holds"),
    ("ready", "bool — printed once the session takes requests"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_running_sessions_log_is_rotated_under_its_writers() {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-session-log-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bench.log");
        // The session and its workers hold the log open to append.
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writer.write_all(b"small").unwrap();
        rotate(&path, 16);
        assert_eq!(std::fs::read(&path).unwrap(), b"small");
        writer.write_all(b" and then much more").unwrap();
        rotate(&path, 16);
        assert_eq!(
            std::fs::read(path.with_extension("log.1")).unwrap(),
            b"small and then much more"
        );
        writer.write_all(b"next").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"next");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
