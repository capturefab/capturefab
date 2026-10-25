//! Versioned newline-delimited JSON over loopback, with a private per-session bearer token.
use crate::session::{ActivityOrigin, SessionActor, SessionCommand, SessionHandle};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub const VERSION: u32 = 1;
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Gui,
    Serve,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub version: u32,
    pub name: String,
    pub kind: SessionKind,
    pub pid: u32,
    pub address: SocketAddr,
    pub token: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    token: String,
    #[serde(default)]
    camera: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actor: Option<SessionActor>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_command",
        skip_serializing_if = "Option::is_none"
    )]
    command: Option<SessionCommand>,
    /// Authenticated liveness checks bypass the camera coordinator entirely.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    probe: bool,
}
fn deserialize_optional_command<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<SessionCommand>, D::Error> {
    crate::session::deserialize_command(deserializer).map(Some)
}
/// One of the bounded concurrent RPC client slots, released on drop.
struct ClientSlot(Arc<AtomicUsize>);
impl ClientSlot {
    fn acquire(clients: &Arc<AtomicUsize>) -> Option<Self> {
        Self::acquire_limit(clients, MAX_CLIENTS)
    }
    fn acquire_limit(clients: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        // Count first; the guard undoes the increment if over the limit. Unlike
        // a CAS loop this works on every supported toolchain.
        let previous = clients.fetch_add(1, Ordering::AcqRel);
        let slot = Self(clients.clone());
        (previous < limit).then_some(slot)
    }
}
impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
const MAX_CLIENTS: usize = 16;
// Long polls must leave capacity for the actions they are waiting to observe.
const MAX_OBSERVERS: usize = MAX_CLIENTS / 2;
pub struct Server {
    pub name: String,
    path: PathBuf,
    token: String,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    // Keep the lock inode in place: unlinking it lets competing processes lock
    // different files and both claim the same name.
    _name_lock: fs::File,
    _observers: Arc<AtomicUsize>,
}

/// Session descriptors and their locks, and files that matter only while
/// the processes using them run, such as scanner and GPU JPEG staging.
/// CAPTUREFAB_SESSION_DIR overrides it together with the state directory.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = var("CAPTUREFAB_SESSION_DIR") {
        return dir;
    }
    if cfg!(test) {
        return test_dir();
    }
    let login = login_runtime_dir().filter(|(_, login)| *login == LoginDir::Usable);
    runtime_dir_in(&var, login.map(|(dir, _)| dir))
}
/// What must outlive a reboot: GUI settings, saved destinations and the storage ledger. The Linux runtime directory is a RAM disk
/// that logout clears, so there they live under XDG_STATE_HOME; elsewhere
/// the two are one directory.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = var("CAPTUREFAB_SESSION_DIR") {
        return dir;
    }
    if cfg!(test) {
        return test_dir();
    }
    state_dir_in(&var, cfg!(target_os = "linux"))
}
/// The per-user cache, for what can be made again: the extracted FFmpeg, its
/// encoder probes and, on Linux, dataset staging. CAPTUREFAB_CACHE_DIR
/// overrides it.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = var("CAPTUREFAB_CACHE_DIR") {
        return Some(dir);
    }
    // Tests never touch the user's cache.
    if cfg!(test) {
        return Some(std::env::temp_dir().join("capturefab-test-cache"));
    }
    cache_dir_in(&var)
}
/// Unit tests never touch the user's sessions or storage ledger: without CAPTUREFAB_SESSION_DIR each test process gets its own
/// private directory, which its tests share as they would the real one.
fn test_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir().join(format!(
            "capturefab-test-sessions-{}-{nanos}",
            std::process::id()
        ))
    })
    .clone()
}
/// An environment variable that is set to something.
fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
/// An XDG base directory; the specification ignores relative ones.
fn xdg(var: &impl Fn(&str) -> Option<PathBuf>, name: &str) -> Option<PathBuf> {
    var(name).filter(|dir| dir.is_absolute())
}
fn home(var: &impl Fn(&str) -> Option<PathBuf>) -> PathBuf {
    var("HOME").unwrap_or_else(std::env::temp_dir)
}
/// The shared runtime and state directory on macOS and Windows: the
/// login's runtime directory, or a folder on disk.
fn session_dir_in(var: &impl Fn(&str) -> Option<PathBuf>) -> PathBuf {
    xdg(var, "XDG_RUNTIME_DIR").map_or_else(|| disk_session_dir(var), |dir| dir.join("capturefab"))
}
/// The folder on disk sessions fall back to without a runtime directory.
fn disk_session_dir(var: &impl Fn(&str) -> Option<PathBuf>) -> PathBuf {
    if cfg!(windows)
        && let Some(dir) = var("LOCALAPPDATA")
    {
        return dir.join("capturefab").join("sessions");
    }
    home(var).join(".cache").join("capturefab").join("sessions")
}
/// Without XDG_RUNTIME_DIR, the login's own runtime directory when it is
/// usable, so a service and an SSH shell find the same sessions.
fn runtime_dir_in(
    var: &impl Fn(&str) -> Option<PathBuf>,
    user_runtime: Option<PathBuf>,
) -> PathBuf {
    match user_runtime {
        Some(dir) if xdg(var, "XDG_RUNTIME_DIR").is_none() => dir.join("capturefab"),
        _ => session_dir_in(var),
    }
}
fn state_dir_in(var: &impl Fn(&str) -> Option<PathBuf>, linux: bool) -> PathBuf {
    if !linux {
        return session_dir_in(var);
    }
    xdg(var, "XDG_STATE_HOME")
        .unwrap_or_else(|| home(var).join(".local").join("state"))
        .join("capturefab")
}
/// ~/Library/Caches on macOS, %LOCALAPPDATA% on Windows, and $XDG_CACHE_HOME
/// or ~/.cache elsewhere.
fn cache_dir_in(var: &impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return var("HOME").map(|home| home.join("Library/Caches/capturefab"));
    }
    if cfg!(windows) {
        return var("LOCALAPPDATA").map(|dir| dir.join("capturefab").join("cache"));
    }
    xdg(var, "XDG_CACHE_HOME")
        .or_else(|| var("HOME").map(|home| home.join(".cache")))
        .map(|dir| dir.join("capturefab"))
}
/// Whether a process without XDG_RUNTIME_DIR can keep its sessions in the
/// login's runtime directory, /run/user/<uid>, where logins look for them.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginDir {
    /// This user's alone, and kept from boot to shutdown for a lingering user.
    Usable,
    /// logind keeps it only while the user is logged in, so a service's
    /// session would vanish at logout.
    NotLingering,
    /// The user lingers, but logind has not made it yet.
    Missing,
    /// Another user's, or open to others.
    NotPrivate,
}
#[cfg(any(target_os = "linux", all(unix, test)))]
impl LoginDir {
    fn of(dir: &Path, uid: u32, lingers: bool) -> Self {
        match fs::symlink_metadata(dir) {
            Ok(_) if !owned_private(dir, uid) => Self::NotPrivate,
            Ok(_) if lingers => Self::Usable,
            Err(_) if lingers => Self::Missing,
            _ => Self::NotLingering,
        }
    }
}
/// The login's runtime directory and whether it is usable, decided once, so
/// a process keeps its descriptor, locks and buffers in one place however
/// logins come and go.
#[cfg(target_os = "linux")]
fn login_runtime_dir() -> Option<(PathBuf, LoginDir)> {
    static LOGIN: std::sync::OnceLock<(PathBuf, LoginDir)> = std::sync::OnceLock::new();
    let login = LOGIN.get_or_init(|| {
        let uid = effective_uid();
        // Integration tests stand in for the user's real one.
        let dir = var("CAPTUREFAB_LOGIN_RUNTIME_DIR_TEST")
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
        let login = LoginDir::of(&dir, uid, lingers(uid));
        (dir, login)
    });
    Some(login.clone())
}
#[cfg(not(target_os = "linux"))]
fn login_runtime_dir() -> Option<(PathBuf, LoginDir)> {
    None
}
/// Whether logind keeps `uid`'s runtime directory from boot to shutdown
/// (`loginctl enable-linger`), not only while the user is logged in.
#[cfg(target_os = "linux")]
fn lingers(uid: u32) -> bool {
    user_name(uid).is_some_and(|name| {
        !name.is_empty() && Path::new("/var/lib/systemd/linger").join(name).exists()
    })
}
/// The name passwd gives `uid`.
#[cfg(target_os = "linux")]
fn user_name(uid: u32) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: getpwuid_r fills `entry`, with strings stored in `buffer`, and
    // points `found` at it only when it found the user.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }
    // SAFETY: pw_name is a NUL-terminated string in `buffer`, still alive.
    let name = unsafe { std::ffi::CStr::from_ptr((*found).pw_name) };
    Some(std::ffi::OsStr::from_bytes(name.to_bytes()).to_owned())
}
/// The login runtime directory that runtime_dir() passed over for a folder
/// on disk, where logins look for sessions instead, and why.
pub(crate) fn missed_runtime_dir() -> Option<(PathBuf, LoginDir)> {
    if cfg!(test)
        || var("CAPTUREFAB_SESSION_DIR").is_some()
        || xdg(&var, "XDG_RUNTIME_DIR").is_some()
    {
        return None;
    }
    let logind = Path::new("/run/systemd/seats").is_dir() || Path::new("/run/user").is_dir();
    missed(login_runtime_dir(), logind)
}
/// In a login with XDG_RUNTIME_DIR, where a service started without it
/// keeps its sessions instead, with the login runtime directory it passes
/// over and why. An SSH login on a line computer would otherwise see no sign
/// of a service whose sessions it cannot find.
pub(crate) fn service_runtime_dir() -> Option<(PathBuf, PathBuf, LoginDir)> {
    if cfg!(test)
        || var("CAPTUREFAB_SESSION_DIR").is_some()
        || xdg(&var, "XDG_RUNTIME_DIR").is_none()
    {
        return None;
    }
    let logind = Path::new("/run/systemd/seats").is_dir() || Path::new("/run/user").is_dir();
    let (login, why) = missed(login_runtime_dir(), logind)?;
    Some((disk_session_dir(&var), login, why))
}
/// The responsive sessions in `service_runtime_dir`, which this login's
/// commands do not find, and that folder.
pub(crate) fn sessions_elsewhere() -> Option<(PathBuf, Vec<String>)> {
    let (dir, ..) = service_runtime_dir()?;
    let names: Vec<String> = descriptors_in(&dir)
        .ok()?
        .into_iter()
        .filter(descriptor_active)
        .map(|descriptor| descriptor.name)
        .collect();
    (!names.is_empty()).then_some((dir, names))
}
/// A host without logind, such as a container, has no logins to miss.
fn missed(login: Option<(PathBuf, LoginDir)>, logind: bool) -> Option<(PathBuf, LoginDir)> {
    login.filter(|(_, login)| logind && *login != LoginDir::Usable)
}
/// A real directory owned by `uid` with mode 0700.
#[cfg(any(target_os = "linux", all(unix, test)))]
fn owned_private(dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == uid && meta.mode() & 0o777 == 0o700)
}
#[cfg(unix)]
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and always succeeds.
    unsafe { libc::geteuid() }
}
/// A lock file only this user can open.
fn lock_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
pub fn validate_name(name: &str) -> Result<()> {
    let lower = name.to_ascii_lowercase();
    ensure!(
        !matches!(
            lower.as_str(),
            "storage" | "storage-policy" | "storage-summary" | "destinations" | "uploads" | "hubs"
        ),
        "session name is reserved for application bookkeeping; choose a different name"
    );
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "session names use 1..64 letters, digits, '-' or '_'"
    );
    // Windows treats these as device names even when an extension is present.
    // Reject them on every OS so scripts and session names remain portable.
    ensure!(
        !matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
            && !(lower.len() == 4
                && (lower.starts_with("com") || lower.starts_with("lpt"))
                && matches!(lower.as_bytes()[3], b'1'..=b'9')),
        "session name is reserved by Windows; choose a portable name"
    );
    Ok(())
}
fn descriptor_path_in(dir: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(dir.join(format!("{name}.json")))
}
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut d = fs::DirBuilder::new();
            d.recursive(true).mode(0o700);
            d.create(path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(path)?;
    }
    check_private_dir(path)
}
/// A real directory that only this user can enter. One another user owns,
/// such as a squatted /tmp or /dev/shm name, is refused even if private.
pub(crate) fn check_private_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        "session directory must be a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            m.uid() == effective_uid(),
            "session directory {} belongs to another user",
            path.display()
        );
        ensure!(
            m.mode() & 0o077 == 0,
            "session directory must be private (chmod 700 {})",
            path.display()
        );
    }
    Ok(())
}
pub fn read_descriptor(name: &str) -> Result<Descriptor> {
    let dir = runtime_dir();
    read_descriptor_in(&dir, name).map_err(|error| {
        let absent = |dir: &Path| {
            descriptor_path_in(dir, name).is_ok_and(|path| fs::symlink_metadata(path).is_err())
        };
        match service_runtime_dir() {
            Some((service, ..)) if service != dir && absent(&dir) && !absent(&service) => {
                anyhow::anyhow!(
                    "{error:#}; a session named {name} is in {}, where a service started without XDG_RUNTIME_DIR keeps it and this login does not look: `capturefab doctor` shows the fix under runtime_dir",
                    service.display()
                )
            }
            _ => error,
        }
    })
}
fn read_descriptor_in(dir: &Path, name: &str) -> Result<Descriptor> {
    let path = descriptor_path_in(dir, name)?;
    let meta=fs::symlink_metadata(&path).with_context(||format!("session '{name}' not found; run capturefab gui --name {name} or capturefab serve --name {name}"))?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 4096,
        "invalid session descriptor"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "session descriptor is not private"
        );
    }
    let d: Descriptor = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        d.version == VERSION
            && d.name == name
            && d.address.ip() == Ipv4Addr::LOCALHOST
            && d.address.port() != 0
            && d.pid != 0
            && d.token.len() == 64
            && d.token.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid session descriptor or protocol version"
    );
    Ok(d)
}
#[cfg(test)]
fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut data = Vec::new();
    reader.take(limit as u64 + 1).read_until(b'\n', &mut data)?;
    ensure!(
        data.len() <= limit && data.last() == Some(&b'\n'),
        "RPC message exceeds limit or is unterminated"
    );
    Ok(data)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("RPC deadline exceeded")
}
fn rpc_deadline(timeout: Duration) -> Result<Instant> {
    ensure!(!timeout.is_zero(), "RPC timeout must be positive");
    Instant::now()
        .checked_add(timeout)
        .context("RPC timeout is too large")
}
/// Apply one deadline to the whole message, including peers that trickle bytes.
fn read_rpc_line(stream: TcpStream, limit: usize, deadline: Instant) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(stream);
    let mut data = Vec::new();
    loop {
        reader
            .get_ref()
            .set_read_timeout(Some(remaining(deadline)?))?;
        let buffer = reader.fill_buf().map_err(|error| match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                anyhow::anyhow!("RPC deadline exceeded")
            }
            _ => error.into(),
        })?;
        ensure!(
            !buffer.is_empty(),
            "RPC connection closed before a complete response"
        );
        let end = buffer.iter().position(|byte| *byte == b'\n');
        let length = end.map_or(buffer.len(), |position| position + 1);
        ensure!(
            length <= limit.saturating_sub(data.len()),
            "RPC message exceeds limit"
        );
        data.extend_from_slice(&buffer[..length]);
        reader.consume(length);
        if end.is_some() {
            return Ok(data);
        }
    }
}
fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0, |v, (x, y)| v | (x ^ y)) == 0
}
fn serve_client(
    mut stream: TcpStream,
    h: SessionHandle,
    token: String,
    observers: Arc<AtomicUsize>,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let result = (|| -> Result<Value> {
        let line = read_rpc_line(
            stream.try_clone()?,
            MAX_REQUEST,
            rpc_deadline(Duration::from_secs(5))?,
        )?;
        let request: Request = serde_json::from_slice(&line).context("invalid RPC request")?;
        ensure!(
            request.version == VERSION,
            "session protocol version mismatch"
        );
        ensure!(
            constant_eq(&token, &request.token),
            "unauthorized RPC request"
        );
        if request.probe {
            ensure!(
                request.command.is_none() && request.camera.is_none() && request.actor.is_none(),
                "session probes cannot include a command, camera or actor"
            );
            return Ok(json!({"probe":true}));
        }
        let command = request.command.context("RPC request requires a command")?;
        command.validate()?;
        let _observer = if matches!(&command, SessionCommand::Activity { after: Some(after), timeout_ms }
            if *timeout_ms > 0 && *after == h.activity_revision())
        {
            Some(ClientSlot::acquire_limit(&observers, MAX_OBSERVERS)
                .context("session activity observer queue is full (8 waiting clients); retry after an observer returns")?)
        } else {
            None
        };
        ensure!(
            !matches!(&command,SessionCommand::Capture{output,..} if output=="-"),
            "remote captures require an output file"
        );
        let actor = request.actor.unwrap_or_else(|| {
            SessionActor::new(
                format!(
                    "ipc:{}",
                    stream.peer_addr().map_or(0, |address| address.port())
                ),
                "CLI / API",
                crate::session::ActorKind::Human,
            )
            .expect("valid anonymous RPC actor")
        });
        if let Some(camera) = request.camera {
            h.request_to_as(&camera, command, ActivityOrigin::Client, actor)
        } else {
            h.request_as(command, ActivityOrigin::Client, actor)
        }
    })();
    let response = match result {
        Ok(v) => json!({"version":VERSION,"ok":true,"result":v}),
        Err(e) => {
            let mut error = json!({"message":format!("{e:#}")});
            // A request the caller must change keeps exit code 2 across the
            // connection, and a categorized failure its category; the text
            // alone may not say so.
            if let Some(code) = crate::types::error_transport_code(&e) {
                error["code"] = json!(code);
            }
            json!({"version":VERSION,"ok":false,"error":error})
        }
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}
impl Server {
    pub fn start_with_kind(h: SessionHandle, name: &str, kind: SessionKind) -> Result<Self> {
        Self::start_in(h, name, kind, &runtime_dir())
    }
    fn start_in(h: SessionHandle, name: &str, kind: SessionKind, dir: &Path) -> Result<Self> {
        validate_name(name)?;
        ensure_private_dir(dir)?;
        let path = descriptor_path_in(dir, name)?;
        let name_lock = lock_file(&dir.join(format!("{name}.session.lock")))?;
        name_lock.try_lock_exclusive().with_context(|| {
            format!(
                "session '{name}' already exists; choose another --name or use --session {name}"
            )
        })?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random session token: {e}"))?;
        let token = random
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let descriptor = Descriptor {
            version: VERSION,
            name: name.into(),
            kind,
            pid: std::process::id(),
            address: listener.local_addr()?,
            token: token.clone(),
        };
        if let Ok(existing) = read_descriptor_in(dir, name) {
            ensure!(
                !descriptor_active(&existing),
                "session '{name}' already exists; choose another --name or use --session {name}"
            );
            fs::remove_file(&path)?;
        }
        ensure!(
            !path.exists(),
            "session '{name}' already exists with an invalid descriptor; inspect {} before removing it",
            path.display()
        );
        // Publish a complete descriptor atomically so discovery never reads a
        // partially-written JSON file during simultaneous GUI/CLI startup.
        let temp_path = dir.join(format!(".{name}.{}.tmp", &token[..16]));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let publish = (|| -> Result<()> {
            let mut file = options
                .open(&temp_path)
                .with_context(|| format!("cannot create session '{name}' in {}", dir.display()))?;
            serde_json::to_writer(&mut file, &descriptor)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp_path, &path)?;
            Ok(())
        })();
        if let Err(error) = publish {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let server_token = token.clone();
        let clients = Arc::new(AtomicUsize::new(0));
        let observers = Arc::new(AtomicUsize::new(0));
        let thread_observers = observers.clone();
        // Accept blocks instead of polling; Drop wakes it with a loopback connect.
        let thread = std::thread::Builder::new()
            .name("capturefab-rpc".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let stream = match stream {
                        Ok(stream) => stream,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    };
                    let Some(slot) = ClientSlot::acquire(&clients) else {
                        drop(stream);
                        continue;
                    };
                    let h = h.clone();
                    let token = server_token.clone();
                    let observers = thread_observers.clone();
                    let spawned = std::thread::Builder::new()
                        .name("capturefab-rpc-client".into())
                        .spawn(move || {
                            let _slot = slot;
                            let _ = serve_client(stream, h, token, observers);
                        });
                    if let Err(e) = spawned {
                        eprintln!("capturefab: rejected RPC client: {e}");
                    }
                }
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error.into());
            }
        };
        Ok(Self {
            name: name.into(),
            path,
            token,
            address,
            stop,
            thread: Some(thread),
            _name_lock: name_lock,
            _observers: observers,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // Unblock accept(). If even a loopback connect fails, detach rather
            // than risk joining a thread that can never wake.
            if TcpStream::connect_timeout(&self.address, Duration::from_secs(1)).is_ok() {
                let _ = thread.join();
            }
        }
        if fs::read(&self.path)
            .ok()
            .and_then(|v| serde_json::from_slice::<Descriptor>(&v).ok())
            .is_some_and(|d| d.token == self.token)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn call(name: &str, command: SessionCommand, timeout: Duration) -> Result<Value> {
    call_to(name, None, command, timeout)
}
pub fn call_to(
    name: &str,
    camera: Option<&str>,
    command: SessionCommand,
    timeout: Duration,
) -> Result<Value> {
    call_to_as(
        name,
        camera,
        command,
        timeout,
        Some(&SessionActor::process(ActivityOrigin::Client)),
    )
}
pub fn call_to_as(
    name: &str,
    camera: Option<&str>,
    command: SessionCommand,
    timeout: Duration,
    actor: Option<&SessionActor>,
) -> Result<Value> {
    command.validate()?;
    if let Some(actor) = actor {
        actor.validate()?;
    }
    ensure!(
        !matches!(&command, SessionCommand::Capture { output, .. } if output == "-"),
        "remote captures require an output file"
    );
    if let Some(camera) = camera {
        crate::session::validate_selector(camera)?;
    }
    ensure!(!timeout.is_zero(), "RPC timeout must be positive");
    let descriptor = read_descriptor(name)?;
    send_request_as(&descriptor, camera, Some(command), false, timeout, actor)
}
fn rpc_network_error(name: &str, phase: &str, error: std::io::Error) -> anyhow::Error {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            anyhow::anyhow!("RPC deadline exceeded while {phase} to session '{name}'")
        }
        _ => anyhow::Error::new(error)
            .context(format!("session '{name}' is not responding while {phase}")),
    }
}
fn send_request(
    descriptor: &Descriptor,
    camera: Option<&str>,
    command: Option<SessionCommand>,
    probe: bool,
    timeout: Duration,
) -> Result<Value> {
    send_request_as(descriptor, camera, command, probe, timeout, None)
}
fn send_request_as(
    descriptor: &Descriptor,
    camera: Option<&str>,
    command: Option<SessionCommand>,
    probe: bool,
    timeout: Duration,
    actor: Option<&SessionActor>,
) -> Result<Value> {
    let deadline = rpc_deadline(timeout)?;
    let request = serde_json::to_vec(&Request {
        version: VERSION,
        token: descriptor.token.clone(),
        camera: camera.map(str::to_owned),
        actor: actor.cloned(),
        command,
        probe,
    })?;
    ensure!(request.len() < MAX_REQUEST, "RPC request exceeds 1 MiB");
    let mut stream = TcpStream::connect_timeout(&descriptor.address, remaining(deadline)?)
        .map_err(|error| rpc_network_error(&descriptor.name, "connecting", error))?;
    // A single budget covers connect, sending, and the response. A timeout is
    // never retried because the coordinator may already have applied a write.
    let mut message = request;
    message.push(b'\n');
    let mut offset = 0;
    while offset < message.len() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let count = stream
            .write(&message[offset..])
            .map_err(|error| rpc_network_error(&descriptor.name, "sending a request", error))?;
        ensure!(
            count != 0,
            "session '{}' closed while sending a request",
            descriptor.name
        );
        offset += count;
    }
    let line = read_rpc_line(stream, MAX_RESPONSE, deadline).with_context(|| {
        format!(
            "waiting for session '{}' response; an accepted command may still complete",
            descriptor.name
        )
    })?;
    let response: Value = serde_json::from_slice(&line).context("invalid RPC response")?;
    ensure!(
        response["version"] == VERSION,
        "session protocol version mismatch"
    );
    if response["ok"] != true {
        let message = response["error"]["message"]
            .as_str()
            .unwrap_or("session request failed")
            .to_owned();
        return Err(crate::types::error_from_transport(
            response["error"]["code"].as_str(),
            message,
        ));
    }
    response
        .get("result")
        .cloned()
        .context("invalid RPC response: missing result")
}
fn descriptor_active(descriptor: &Descriptor) -> bool {
    send_request(descriptor, None, None, true, PROBE_TIMEOUT)
        .is_ok_and(|response| response == json!({"probe":true}))
}
fn descriptors_in(dir: &Path) -> Result<Vec<Descriptor>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    ensure_private_dir(dir)?;
    let mut descriptors = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned);
        if entry.path().extension().is_some_and(|s| s == "json")
            && let Some(name) = name
            && let Ok(d) = read_descriptor_in(dir, &name)
        {
            descriptors.push(d);
        }
    }
    descriptors.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(descriptors)
}
/// Discover authenticated, responsive sessions without touching camera state
/// or adding entries to their activity logs.
pub fn active_sessions() -> Result<Vec<Descriptor>> {
    Ok(descriptors_in(&runtime_dir())?
        .into_iter()
        .filter(descriptor_active)
        .collect())
}
pub fn list() -> Result<Value> {
    let sessions: Vec<_> = descriptors_in(&runtime_dir())?
        .into_iter()
        .map(|descriptor| {
            let active = descriptor_active(&descriptor);
            json!({"name":descriptor.name,"kind":descriptor.kind,"pid":descriptor.pid,"active":active})
        })
        .collect();
    Ok(json!({"sessions":sessions}))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "capturefab-ipc-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            ensure_private_dir(&path).unwrap();
            Self(path)
        }
        fn write_descriptor(&self, descriptor: &Descriptor) {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(descriptor_path_in(&self.0, &descriptor.name).unwrap())
                .unwrap();
            serde_json::to_writer(&mut file, descriptor).unwrap();
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn test_descriptor(name: &str, address: SocketAddr) -> Descriptor {
        Descriptor {
            version: VERSION,
            name: name.into(),
            kind: SessionKind::Serve,
            pid: std::process::id(),
            address,
            token: "a".repeat(64),
        }
    }
    #[test]
    fn tests_never_default_to_the_users_session_directory() {
        let dir = test_dir();
        assert!(dir.starts_with(std::env::temp_dir()), "{}", dir.display());
        assert_eq!(dir, test_dir(), "one directory per process");
        if std::env::var_os("CAPTUREFAB_SESSION_DIR").is_none() {
            assert_eq!(runtime_dir(), dir);
            assert_eq!(state_dir(), dir);
        }
        if std::env::var_os("CAPTUREFAB_CACHE_DIR").is_none() {
            assert!(cache_dir().unwrap().starts_with(std::env::temp_dir()));
        }
        assert_eq!(missed_runtime_dir(), None);
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> {
        let vars: Vec<(String, PathBuf)> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.into()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(set, _)| set == name)
                .map(|(_, value)| value.clone())
        }
    }

    // XDG paths are absolute only on Unix.
    #[cfg(unix)]
    #[test]
    fn the_runtime_directory_is_the_same_with_or_without_xdg_runtime_dir() {
        let login = Some(PathBuf::from("/run/user/1000"));
        let shell = env(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("HOME", "/home/pi")]);
        let service = env(&[("HOME", "/home/pi")]);
        let expected = PathBuf::from("/run/user/1000/capturefab");
        assert_eq!(runtime_dir_in(&shell, login.clone()), expected);
        assert_eq!(runtime_dir_in(&shell, None), expected);
        assert_eq!(runtime_dir_in(&service, login.clone()), expected);
        // XDG_RUNTIME_DIR, such as a unit's RuntimeDirectory, comes first.
        let unit = env(&[("XDG_RUNTIME_DIR", "/run/capturefab"), ("HOME", "/home/pi")]);
        assert_eq!(
            runtime_dir_in(&unit, login.clone()),
            PathBuf::from("/run/capturefab/capturefab")
        );
        // Without a usable login directory, sessions fall back to disk;
        // a relative XDG_RUNTIME_DIR counts as unset.
        let fallback = PathBuf::from("/home/pi/.cache/capturefab/sessions");
        assert_eq!(runtime_dir_in(&service, None), fallback);
        let relative = env(&[("XDG_RUNTIME_DIR", "run"), ("HOME", "/home/pi")]);
        assert_eq!(runtime_dir_in(&relative, None), fallback);
        assert_eq!(runtime_dir_in(&relative, login), expected);
    }

    #[cfg(unix)]
    #[test]
    fn linux_keeps_state_out_of_the_runtime_directory() {
        let shell = env(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("HOME", "/home/pi")]);
        let service = env(&[("HOME", "/home/pi")]);
        let state = PathBuf::from("/home/pi/.local/state/capturefab");
        assert_eq!(state_dir_in(&shell, true), state);
        assert_eq!(state_dir_in(&service, true), state);
        let custom = env(&[("XDG_STATE_HOME", "/data/state"), ("HOME", "/home/pi")]);
        assert_eq!(
            state_dir_in(&custom, true),
            PathBuf::from("/data/state/capturefab")
        );
        let relative = env(&[("XDG_STATE_HOME", "state"), ("HOME", "/home/pi")]);
        assert_eq!(state_dir_in(&relative, true), state);
        // Elsewhere nothing moves: state stays in the session directory.
        assert_eq!(
            state_dir_in(&shell, false),
            PathBuf::from("/run/user/1000/capturefab")
        );
        assert_eq!(
            state_dir_in(&service, false),
            PathBuf::from("/home/pi/.cache/capturefab/sessions")
        );
    }

    #[test]
    fn the_cache_follows_each_platform() {
        let home = env(&[("HOME", "/home/pi"), ("XDG_CACHE_HOME", "/data/cache")]);
        let expected = if cfg!(target_os = "macos") {
            Some("/home/pi/Library/Caches/capturefab")
        } else if cfg!(windows) {
            None
        } else {
            Some("/data/cache/capturefab")
        };
        assert_eq!(cache_dir_in(&home), expected.map(PathBuf::from));
        if !cfg!(any(target_os = "macos", windows)) {
            let plain = env(&[("HOME", "/home/pi"), ("XDG_CACHE_HOME", "cache")]);
            assert_eq!(
                cache_dir_in(&plain),
                Some(PathBuf::from("/home/pi/.cache/capturefab"))
            );
            assert_eq!(cache_dir_in(&env(&[])), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_a_private_runtime_directory_of_this_user_is_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new();
        let uid = effective_uid();
        let login = dir.0.join("1000");
        fs::create_dir(&login).unwrap();
        fs::set_permissions(&login, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(owned_private(&login, uid));
        assert!(!owned_private(&login, uid + 1), "another user's");
        fs::set_permissions(&login, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!owned_private(&login, uid), "readable by others");
        fs::set_permissions(&login, fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&login, &link).unwrap();
        assert!(!owned_private(&link, uid), "a symbolic link");
        assert!(!owned_private(&dir.0.join("missing"), uid));
    }

    #[cfg(unix)]
    #[test]
    fn the_login_directory_is_used_only_for_a_lingering_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new();
        let uid = effective_uid();
        let login = dir.0.join("1000");
        assert_eq!(LoginDir::of(&login, uid, false), LoginDir::NotLingering);
        assert_eq!(LoginDir::of(&login, uid, true), LoginDir::Missing);
        fs::create_dir(&login).unwrap();
        fs::set_permissions(&login, fs::Permissions::from_mode(0o700)).unwrap();
        // Without lingering, logind removes it at logout, and a service's
        // session with it, however private it is now.
        assert_eq!(LoginDir::of(&login, uid, false), LoginDir::NotLingering);
        assert_eq!(LoginDir::of(&login, uid, true), LoginDir::Usable);
        assert_eq!(LoginDir::of(&login, uid + 1, true), LoginDir::NotPrivate);
        fs::set_permissions(&login, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(LoginDir::of(&login, uid, true), LoginDir::NotPrivate);
        assert_eq!(LoginDir::of(&login, uid, false), LoginDir::NotPrivate);
    }

    #[test]
    fn a_host_without_logind_misses_no_login_directory() {
        let dir = PathBuf::from("/run/user/1000");
        for login in [
            LoginDir::NotLingering,
            LoginDir::Missing,
            LoginDir::NotPrivate,
        ] {
            assert_eq!(
                missed(Some((dir.clone(), login)), true),
                Some((dir.clone(), login))
            );
            // A container has no logins that would look there.
            assert_eq!(missed(Some((dir.clone(), login)), false), None);
        }
        assert_eq!(missed(Some((dir, LoginDir::Usable)), true), None);
        assert_eq!(missed(None, true), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linger_is_looked_up_by_user_name() {
        assert_eq!(user_name(0).as_deref(), Some(std::ffi::OsStr::new("root")));
        assert_eq!(user_name(u32::MAX - 1), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_of_another_user_is_refused() {
        // The root directory belongs to root; its mode alone would not
        // refuse a private one that another user created first.
        if effective_uid() != 0 {
            let error = ensure_private_dir(Path::new("/")).unwrap_err().to_string();
            assert!(error.contains("belongs to another user"), "{error}");
        }
        let dir = TestDir::new();
        ensure_private_dir(&dir.0.join("a/b")).unwrap();
    }

    #[test]
    fn authenticated_probes_do_not_change_camera_state_or_activity() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server = Server::start_in(handle.clone(), "desktop", SessionKind::Gui, &dir.0).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "desktop").unwrap();
        assert_eq!(descriptor.kind, SessionKind::Gui);
        assert!(descriptor_active(&descriptor));
        let mut wrong_token = descriptor.clone();
        wrong_token.token = "f".repeat(64);
        assert!(!descriptor_active(&wrong_token));
        let error = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Status),
            true,
            PROBE_TIMEOUT,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("probes cannot include a command"));
        let snapshot = handle.snapshot();
        assert!(snapshot.logs.is_empty());
        assert!(snapshot.cameras.is_empty());
        assert!(snapshot.last_error.is_none());
        drop(server);
        assert!(!descriptor_path_in(&dir.0, "desktop").unwrap().exists());
        handle.shutdown();
    }
    #[test]
    fn rpc_attributes_independent_participants_and_rejects_invalid_identity_before_queueing() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server =
            Server::start_in(handle.clone(), "shared", SessionKind::Serve, &dir.0).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "shared").unwrap();
        let actors = [
            SessionActor::new("human:alice", "Alice", crate::session::ActorKind::Human).unwrap(),
            SessionActor::new("agent:planner", "Planner", crate::session::ActorKind::Agent)
                .unwrap(),
        ];
        let clients: Vec<_> = actors
            .iter()
            .map(|actor| {
                let descriptor = descriptor.clone();
                let actor = actor.clone();
                std::thread::spawn(move || {
                    send_request_as(
                        &descriptor,
                        Some("sim:0"),
                        Some(SessionCommand::Start),
                        false,
                        Duration::from_secs(2),
                        Some(&actor),
                    )
                })
            })
            .collect();
        for client in clients {
            assert!(client.join().unwrap().is_err());
        }
        let view = handle.activity();
        assert_eq!(view.activity.len(), 2);
        for actor in actors {
            assert!(view.activity.iter().any(|entry| entry.actor == actor));
        }
        let revision = view.revision;
        let invalid = SessionActor {
            id: "invalid id".into(),
            name: "Invalid".into(),
            kind: crate::session::ActorKind::Agent,
        };
        let error = send_request_as(
            &descriptor,
            Some("sim:0"),
            Some(SessionCommand::Start),
            false,
            Duration::from_secs(2),
            Some(&invalid),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("actor id must use"));
        let status = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Status),
            false,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status["activity_revision"], revision);
        assert_eq!(handle.activity().activity.len(), 2);
        drop(server);
        handle.shutdown();
    }
    #[test]
    fn local_rpc_activity_observers_wait_for_changes_without_remote_http_or_camera() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server =
            Server::start_in(handle.clone(), "shared", SessionKind::Serve, &dir.0).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "shared").unwrap();
        let initial = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Activity {
                after: None,
                timeout_ms: 25_000,
            }),
            false,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(initial, json!({"revision":0,"activity":[]}));
        let (started, ready) = std::sync::mpsc::channel();
        let observers: Vec<_> = (0..2)
            .map(|_| {
                let descriptor = descriptor.clone();
                let started = started.clone();
                std::thread::spawn(move || {
                    started.send(()).unwrap();
                    send_request(
                        &descriptor,
                        Some("unconnected-camera"),
                        Some(SessionCommand::Activity {
                            after: Some(0),
                            timeout_ms: 25_000,
                        }),
                        false,
                        Duration::from_secs(2),
                    )
                    .unwrap()
                })
            })
            .collect();
        for _ in 0..2 {
            ready.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        let actor = SessionActor::new(
            "agent:controller",
            "Controller",
            crate::session::ActorKind::Agent,
        )
        .unwrap();
        assert!(
            send_request_as(
                &descriptor,
                Some("sim:0"),
                Some(SessionCommand::Start),
                false,
                Duration::from_secs(2),
                Some(&actor)
            )
            .is_err()
        );
        for observer in observers {
            let view = observer.join().unwrap();
            assert!(view["revision"].as_u64().unwrap() > 0);
            assert_eq!(view["activity"][0]["actor"]["id"], actor.id);
        }
        let revision = handle.activity_revision();
        let started = Instant::now();
        let unchanged = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Activity {
                after: Some(revision),
                timeout_ms: 20,
            }),
            false,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(10));
        assert_eq!(unchanged["revision"], revision);
        assert_eq!(unchanged["activity"].as_array().unwrap().len(), 1);
        assert_eq!(
            handle.activity().activity.len(),
            1,
            "long polling is read only"
        );
        drop(server);
        handle.shutdown();
    }
    #[test]
    fn waiting_observers_leave_room_for_actions_and_immediate_status() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server =
            Server::start_in(handle.clone(), "shared", SessionKind::Serve, &dir.0).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "shared").unwrap();
        let observers: Vec<_> = (0..MAX_OBSERVERS)
            .map(|_| {
                let descriptor = descriptor.clone();
                std::thread::spawn(move || {
                    send_request(
                        &descriptor,
                        None,
                        Some(SessionCommand::Activity {
                            after: Some(0),
                            timeout_ms: 25_000,
                        }),
                        false,
                        Duration::from_secs(3),
                    )
                })
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(2);
        while server._observers.load(Ordering::Acquire) != MAX_OBSERVERS
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            server._observers.load(Ordering::Acquire),
            MAX_OBSERVERS,
            "all observer requests are now live"
        );
        let rejected = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Activity {
                after: Some(0),
                timeout_ms: 25_000,
            }),
            false,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(format!("{rejected:#}").contains("observer queue is full"));
        let immediate = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Activity {
                after: None,
                timeout_ms: 0,
            }),
            false,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(immediate["revision"], 0);
        let status = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Status),
            false,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(status["activity_revision"], 0);
        let action = send_request(
            &descriptor,
            Some("sim:0"),
            Some(SessionCommand::Start),
            false,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(
            format!("{action:#}").contains("not connected"),
            "the action reached the camera coordinator"
        );
        for observer in observers {
            let view = observer.join().unwrap().unwrap();
            assert!(view["revision"].as_u64().unwrap() > 0);
            assert_eq!(view["activity"].as_array().unwrap().len(), 1);
        }
        drop(server);
        handle.shutdown();
    }
    #[test]
    fn a_reused_tcp_port_does_not_make_a_stale_session_active() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let descriptor = test_descriptor("stale", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_line(
                &mut BufReader::new(stream.try_clone().unwrap()),
                MAX_REQUEST,
            )
            .unwrap();
            // Another loopback service may speak JSON, but it is not this session.
            stream
                .write_all(b"{\"version\":1,\"ok\":true,\"result\":{\"probe\":false}}\n")
                .unwrap();
        });
        assert!(!descriptor_active(&descriptor));
        peer.join().unwrap();
    }
    #[test]
    fn concurrent_stale_session_claims_have_exactly_one_owner() {
        let dir = TestDir::new();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let descriptor = test_descriptor("stale", listener.local_addr().unwrap());
        drop(listener);
        dir.write_descriptor(&descriptor);
        let handle = SessionHandle::new();
        let gate = Arc::new(std::sync::Barrier::new(4));
        let claimants: Vec<_> = (0..4)
            .map(|_| {
                let handle = handle.clone();
                let dir = dir.0.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    Server::start_in(handle, "stale", SessionKind::Serve, &dir)
                })
            })
            .collect();
        let claims: Vec<_> = claimants
            .into_iter()
            .map(|claim| claim.join().unwrap())
            .collect();
        assert_eq!(claims.iter().filter(|claim| claim.is_ok()).count(), 1);
        for error in claims.iter().filter_map(|claim| claim.as_ref().err()) {
            assert!(format!("{error:#}").contains("already exists"), "{error:#}");
        }
        let owner = read_descriptor_in(&dir.0, "stale").unwrap();
        assert_ne!(owner.token, descriptor.token);
        assert!(descriptor_active(&owner));
        drop(claims);
        assert!(!descriptor_path_in(&dir.0, "stale").unwrap().exists());
        handle.shutdown();
    }
    #[test]
    fn rpc_deadline_bounds_a_response_that_trickles_bytes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let descriptor = test_descriptor("slow", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_line(
                &mut BufReader::new(stream.try_clone().unwrap()),
                MAX_REQUEST,
            )
            .unwrap();
            for _ in 0..30 {
                if stream.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        let started = Instant::now();
        let error = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Status),
            false,
            Duration::from_millis(50),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            format!("{error:#}").contains("deadline exceeded"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("may still complete"));
        assert!(elapsed < Duration::from_millis(300), "{elapsed:?}");
        peer.join().unwrap();
    }
    #[test]
    fn descriptor_discovery_is_sorted_and_ignores_application_state() {
        let dir = TestDir::new();
        for name in ["zebra", "alpha"] {
            dir.write_descriptor(&test_descriptor(name, "127.0.0.1:12345".parse().unwrap()));
        }
        fs::write(dir.0.join("destinations.json"), b"{}").unwrap();
        fs::write(dir.0.join("broken.json"), b"{").unwrap();
        let descriptors = descriptors_in(&dir.0).unwrap();
        assert_eq!(
            descriptors
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zebra"]
        );
    }
    #[test]
    fn malformed_requests_return_actionable_errors() {
        for (request, field) in [
            (
                "{\"version\":1,\"token\":\"t\",\"command\":{\"op\":\"status\"},\"camrea\":\"sim:0\"}\n",
                "camrea",
            ),
            (
                "{\"version\":1,\"token\":\"t\",\"command\":{\"op\":\"status\",\"unknown\":1}}\n",
                "unknown",
            ),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let handle = SessionHandle::new();
            let server = std::thread::spawn({
                let handle = handle.clone();
                move || serve_client(stream, handle, "t".into(), Arc::new(AtomicUsize::new(0)))
            });
            client.write_all(request.as_bytes()).unwrap();
            let response: Value =
                serde_json::from_slice(&read_line(&mut BufReader::new(client), 1 << 20).unwrap())
                    .unwrap();
            assert_eq!(response["ok"], false);
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("unknown field `{field}`"))
            );
            server.join().unwrap().unwrap();
            let snapshot = handle.snapshot();
            assert!(snapshot.logs.is_empty());
            assert!(snapshot.cameras.is_empty());
            assert!(snapshot.last_error.is_none());
            handle.shutdown();
        }
    }
    #[test]
    fn bounded_rpc_lines_and_auth() {
        assert!(read_line(&mut BufReader::new(b"abc\n".as_slice()), 3).is_err());
        assert!(read_line(&mut BufReader::new(b"abc".as_slice()), 10).is_err());
        assert_eq!(
            read_line(&mut BufReader::new(b"abc\n".as_slice()), 4).unwrap(),
            b"abc\n"
        );
        assert!(constant_eq("abc", "abc"));
        assert!(!constant_eq("abc", "abd"));
        assert!(!constant_eq("abc", "abcd"));
    }
    #[test]
    fn accepted_clients_wait_for_slow_requests() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let h = SessionHandle::new();
        let server = std::thread::spawn({
            let h = h.clone();
            move || serve_client(stream, h, "t".into(), Arc::new(AtomicUsize::new(0)))
        });
        std::thread::sleep(Duration::from_millis(200));
        serde_json::to_writer(
            &mut client,
            &Request {
                version: VERSION,
                token: "t".into(),
                camera: None,
                actor: None,
                command: Some(SessionCommand::Jobs),
                probe: false,
            },
        )
        .unwrap();
        client.write_all(b"\n").unwrap();
        let response: Value =
            serde_json::from_slice(&read_line(&mut BufReader::new(client), 1 << 20).unwrap())
                .unwrap();
        assert_eq!(response["version"], VERSION);
        server.join().unwrap().unwrap();
        h.shutdown();
    }
    #[test]
    fn names_cannot_escape_directory() {
        for name in [
            "../a", "a/b", "", "a.b", "a b", "CON", "aux", "NUL", "Prn", "COM1", "lpt9",
        ] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("bench-01").is_ok());
        assert!(validate_name("storage").is_err());
        assert!(validate_name("storage-policy").is_err());
        assert!(validate_name("storage-summary").is_err());
        assert!(validate_name("Storage").is_err());
        assert!(validate_name("destinations").is_err());
        assert!(validate_name("uploads").is_err());
        assert!(validate_name("Hubs").is_err());
        assert!(validate_name("com10").is_ok());
        assert!(validate_name("lpt0").is_ok());
    }
}
