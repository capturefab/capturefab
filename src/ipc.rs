//! Versioned newline-delimited JSON over loopback, with a private per-session bearer token.
use crate::session::{SessionCommand, SessionHandle};
use crate::types::hex;
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
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// The session protocol: descriptors, greetings, requests and replies. A
/// session of another version, such as one started by Capturefab 0.5, is
/// never listed, chosen or sent a request.
pub const VERSION: u32 = 3;
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);
/// The largest descriptor: a session's cameras with their aliases.
const MAX_DESCRIPTOR: u64 = 256 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub version: u32,
    pub name: String,
    pub pid: u32,
    pub address: SocketAddr,
    pub token: String,
    /// When the session started, UTC Unix milliseconds.
    #[serde(default)]
    pub started_ms: u64,
    /// The cameras it holds, rewritten as they connect and disconnect, so a
    /// client finds the session for a camera without asking every one.
    #[serde(default)]
    pub cameras: Vec<HeldCamera>,
}
/// A camera a session holds, as its descriptor lists it. Every field is
/// redacted: a client compares its own selector, redacted the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldCamera {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Selectors it was connected by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}
impl HeldCamera {
    pub fn new<'a>(
        info: &crate::types::CameraInfo,
        aliases: impl Iterator<Item = &'a str>,
    ) -> Self {
        let redact = |text: &str| crate::media::redact_url(text);
        let mut aliases = aliases.map(redact).collect::<Vec<_>>();
        aliases.sort();
        aliases.dedup();
        Self {
            id: redact(&info.id),
            serial: (!info.serial.is_empty()).then(|| redact(&info.serial)),
            address: info.address.as_deref().map(redact),
            mac: info.id.strip_prefix("gige:").and_then(mac),
            aliases,
        }
    }
    /// Whether `selector`, as a person typed it, names this camera: 0 by
    /// its ID, 1 by its serial, address, MAC or a selector it was opened by.
    pub fn named_by(&self, selector: &str) -> Option<u8> {
        let shown = crate::media::redact_url(selector);
        if shown == self.id {
            return Some(0);
        }
        (self.serial.as_deref() == Some(&shown)
            || self.address.as_deref() == Some(&shown)
            || self.aliases.contains(&shown)
            || self.mac.is_some()
                && self.mac == mac(selector.strip_prefix("gige:").unwrap_or(selector)))
        .then_some(1)
    }
}
/// A MAC address as twelve lowercase hex digits, from any usual spelling.
fn mac(text: &str) -> Option<String> {
    let digits = text
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect::<String>()
        .to_ascii_lowercase();
    (digits.len() == 12 && digits.bytes().all(|b| b.is_ascii_hexdigit())).then_some(digits)
}
/// The line a client opens a connection with: a challenge the session
/// answers with [`Proof`], before the client sends its token or request,
/// so a program that took the port of a session that died learns neither.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    version: u32,
    /// 64 random hex digits, new for each connection.
    hello: String,
}
/// A session's answer to [`Hello`]: that it holds the descriptor's token.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    version: u32,
    /// HMAC-SHA256 of the challenge under the token, in hex.
    proof: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    token: String,
    #[serde(default)]
    camera: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_command",
        skip_serializing_if = "Option::is_none"
    )]
    command: Option<SessionCommand>,
    /// Authenticated liveness checks bypass the camera coordinator entirely;
    /// with a camera, the reply says when that camera is let go unused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    probe: bool,
    /// Keep the session, and the camera the request uses, at least this
    /// long after its last use (`--keep`). With a camera and no command,
    /// the request says a command is about to use that camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keep_ms: Option<u64>,
    /// Shut the session down now: running jobs end as interrupted, and
    /// recordings are finalized before the process exits.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    stop: bool,
}
/// Why a session ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quit {
    /// SIGINT, SIGTERM or SIGHUP.
    Signal,
    /// `session stop`.
    Stop,
    /// No requests and no jobs for its idle time.
    Idle,
}
/// What a session's request threads share with the process that serves it.
struct Shared {
    dir: PathBuf,
    /// The published descriptor, rewritten as cameras come and go.
    descriptor: std::sync::Mutex<Descriptor>,
    /// The last request that counted as use.
    used: std::sync::Mutex<Instant>,
    /// Requests in progress.
    busy: AtomicUsize,
    /// Clients waiting for a job.
    waiting: AtomicUsize,
    /// Clients that proved they hold the token, each served now.
    clients: Arc<AtomicUsize>,
    /// Connections that have yet to.
    greetings: Greetings,
    /// How long the session waits unused before it exits; None for ever.
    idle: std::sync::Mutex<Option<Duration>>,
    quit: (std::sync::Mutex<Option<Quit>>, std::sync::Condvar),
}
impl Shared {
    fn locked<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn used(&self) {
        *Self::locked(&self.used) = Instant::now();
    }
    fn quit(&self, why: Quit) {
        let (quit, changed) = &self.quit;
        Self::locked(quit).get_or_insert(why);
        changed.notify_all();
    }
    /// Whether the session is ending: it answers until its cameras are
    /// let go, but takes no more use.
    fn quitting(&self) -> bool {
        Self::locked(&self.quit.0).is_some()
    }
    /// Keep the session at least `keep` after its last use; a session that
    /// never idles out stays that way.
    fn keep(&self, keep: Duration) {
        let mut idle = Self::locked(&self.idle);
        if let Some(idle) = idle.as_mut() {
            *idle = (*idle).max(keep);
        }
    }
    /// How long until the session ends unused, when it has an idle time:
    /// all of it while a request runs.
    fn idle_left(&self) -> Option<Duration> {
        let idle = (*Self::locked(&self.idle))?;
        if self.busy.load(Ordering::Acquire) > 0 {
            return Some(idle);
        }
        Some(idle.saturating_sub(Self::locked(&self.used).elapsed()))
    }
    /// Rewrite the descriptor when the cameras it lists changed.
    fn publish(&self, cameras: Vec<HeldCamera>) {
        let mut descriptor = Self::locked(&self.descriptor);
        if descriptor.cameras == cameras {
            return;
        }
        descriptor.cameras = cameras;
        if let Err(error) = write_descriptor(&self.dir, &descriptor) {
            crate::eprintln!("capturefab: cannot update the session descriptor: {error:#}");
        }
    }
}
/// Ends a session from a signal handler.
#[derive(Clone)]
pub struct Quitter(Arc<Shared>);
impl Quitter {
    pub fn quit(&self, why: Quit) {
        self.0.quit(why);
    }
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
        // Count first; the guard undoes the increment if over the limit. Unlike
        // a CAS loop this works on every supported toolchain.
        let previous = clients.fetch_add(1, Ordering::AcqRel);
        let slot = Self(clients.clone());
        (previous < MAX_CLIENTS).then_some(slot)
    }
}
impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
const MAX_CLIENTS: usize = 16;
/// Clients that may wait for jobs at once, so others can still connect.
const MAX_WAITERS: usize = 12;
/// The longest wait of a client past `MAX_WAITERS`.
const SHORT_WAIT_MS: u64 = 500;
/// Connections greeting the session at once; past it, the oldest is closed.
/// A client greets, proves itself and sends its request within a moment,
/// so connections that say nothing never keep a client out.
const MAX_GREETINGS: usize = 64;
/// How long a connection has to greet the session and send its request.
const GREETING: Duration = Duration::from_secs(5);
/// The longest greeting, and the longest answer to one.
const MAX_HELLO: usize = 64 * 1024;

/// Connections that have yet to prove they hold the token, oldest first,
/// with a handle each that closes it.
#[derive(Default)]
struct Greetings {
    next: AtomicU64,
    open: std::sync::Mutex<std::collections::VecDeque<(u64, TcpStream)>>,
}
/// A connection greeting the session, listed until it proves itself or
/// closes.
struct Greeting<'a> {
    greetings: &'a Greetings,
    key: u64,
}
impl<'a> Greeting<'a> {
    /// List `stream`, closing the oldest connection still greeting when
    /// `MAX_GREETINGS` are.
    fn enter(greetings: &'a Greetings, stream: &TcpStream) -> Self {
        let key = greetings.next.fetch_add(1, Ordering::Relaxed);
        let mut open = Shared::locked(&greetings.open);
        while open.len() >= MAX_GREETINGS {
            if let Some((_, oldest)) = open.pop_front() {
                let _ = oldest.shutdown(std::net::Shutdown::Both);
            }
        }
        if let Ok(clone) = stream.try_clone() {
            open.push_back((key, clone));
        }
        Self { greetings, key }
    }
}
impl Drop for Greeting<'_> {
    fn drop(&mut self) {
        Shared::locked(&self.greetings.open).retain(|(key, _)| *key != self.key);
    }
}
pub struct Server {
    path: PathBuf,
    token: String,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    /// The cameras it serves, which its descriptor lists.
    handle: SessionHandle,
    // Keep the lock inode in place: unlinking it lets competing processes lock
    // different files and both claim the same name.
    _name_lock: fs::File,
}

/// The responsive sessions in `service_runtime_dir`, which this login's
/// commands do not find, and that folder.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn sessions_elsewhere() -> Option<(PathBuf, Vec<String>)> {
    let (dir, ..) = crate::dirs::service_runtime_dir()?;
    let names: Vec<String> = descriptors_in(&dir)
        .ok()?
        .into_iter()
        .filter(descriptor_active)
        .map(|descriptor| descriptor.name)
        .collect();
    (!names.is_empty()).then_some((dir, names))
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
/// No descriptor exists for this name. Only [`read_descriptor`] turns it
/// into advice, so internal lookups never probe other sessions.
#[derive(Debug)]
struct MissingSession(String);
impl std::fmt::Display for MissingSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = &self.0;
        write!(
            f,
            "session '{name}' not found; start it with capturefab session start --name {name}"
        )
    }
}
impl std::error::Error for MissingSession {}

/// Why `--session NAME` found nothing, naming the sessions that do run and
/// the one a typo most likely meant.
fn missing_session_error(name: &str, active: &[Descriptor]) -> anyhow::Error {
    let message = if active.is_empty() {
        format!(
            "session '{name}' is not running, and no sessions are running; start one with capturefab session start --name {name}"
        )
    } else {
        let running = active
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let close: Vec<_> = active
            .iter()
            .filter(|session| {
                crate::hint::edit_distance(name, &session.name) <= 2 || {
                    let (typed, candidate) = (name.to_lowercase(), session.name.to_lowercase());
                    typed.len() >= 2
                        && (candidate.starts_with(&typed) || typed.starts_with(&candidate))
                }
            })
            .collect();
        let guess = match close.as_slice() {
            [session] => format!("; did you mean --session {}?", session.name),
            _ => String::new(),
        };
        format!("session '{name}' is not running. Running sessions: {running}{guess}")
    };
    crate::types::Categorized::error(crate::types::ErrorKind::SessionNotFound, message)
}
/// Session `name`'s descriptor, when that session runs: a process holds
/// its name lock. What a session that was killed left is removed, and
/// never sent a request, so a program that took its port later learns
/// nothing.
pub fn read_descriptor(name: &str) -> Result<Descriptor> {
    let descriptor = published(name)?;
    let dir = crate::dirs::runtime()?;
    if running_in(&dir, &descriptor) {
        return Ok(descriptor);
    }
    let _ = remove_stale(name);
    Err(crate::types::Categorized::new(
        crate::types::ErrorKind::SessionNotFound,
        format!(
            "session {name} is not running; it ended without cleaning up, and what it left is removed"
        ),
    )
    .hint(format!(
        "start it again with capturefab session start --name {name}"
    ))
    .into())
}
/// The descriptor published under `name`, whether or not its session
/// still runs.
fn published(name: &str) -> Result<Descriptor> {
    let dir = crate::dirs::runtime().map_err(|error| {
        // No runtime folder, so no session could have started.
        let mut missing = crate::types::Categorized::new(
            crate::types::ErrorKind::SessionNotFound,
            format!("session '{name}' not found: {error:#}"),
        );
        missing.hint = crate::types::classify(&error).hint;
        anyhow::Error::from(missing)
    })?;
    read_descriptor_in(&dir, name).map_err(|error| {
        if !error.is::<MissingSession>() {
            return error;
        }
        #[cfg(any(target_os = "linux", test))]
        {
            let absent = |dir: &Path| {
                descriptor_path_in(dir, name).is_ok_and(|path| fs::symlink_metadata(path).is_err())
            };
            if let Some((service, ..)) = crate::dirs::service_runtime_dir()
                && service != dir
                && absent(&dir)
                && !absent(&service)
            {
                return crate::types::Categorized::error(
                    crate::types::ErrorKind::SessionNotFound,
                    format!(
                        "{error}; a session named {name} is in {}, where a service started without XDG_RUNTIME_DIR keeps it and this login does not look: `capturefab doctor` shows the fix under runtime_dir",
                        service.display()
                    ),
                );
            }
        }
        missing_session_error(name, &active_sessions().unwrap_or_default())
    })
}
fn read_descriptor_in(dir: &Path, name: &str) -> Result<Descriptor> {
    let path = descriptor_path_in(dir, name)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(MissingSession(name.into()).into());
        }
        Err(error) => {
            return Err(anyhow::Error::from(error).context(format!(
                "session '{name}' not found; start it with capturefab session start --name {name}"
            )));
        }
    };
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_DESCRIPTOR,
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
/// Whether `path` is a session's descriptor, of this protocol version or
/// another: a file that names the session it is named after.
pub(crate) fn is_descriptor(path: &Path) -> bool {
    #[derive(Deserialize)]
    struct Published {
        name: String,
        #[serde(rename = "pid")]
        _pid: u32,
        #[serde(rename = "token")]
        _token: String,
    }
    let Some(name) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return false;
    };
    fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_DESCRIPTOR
    }) && fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Published>(&bytes).ok())
        .is_some_and(|published| published.name == name)
}
/// Whether `path` is the descriptor of session `name` from another protocol
/// version, such as one Capturefab 0.5 published.
fn other_protocol(path: &Path, name: &str) -> bool {
    #[derive(Deserialize)]
    struct Published {
        version: u32,
        name: String,
    }
    fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_DESCRIPTOR
    }) && fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Published>(&bytes).ok())
        .is_some_and(|published| published.version != VERSION && published.name == name)
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
        .ok_or_else(|| deadline_exceeded("RPC deadline exceeded".into()))
}
fn deadline_exceeded(message: String) -> anyhow::Error {
    crate::types::Categorized::error(crate::types::ErrorKind::Timeout, message)
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
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            // Linux interrupts a read with a timeout for a signal's
            // handler, SA_RESTART or not; the deadline still holds.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(match error.kind() {
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                        deadline_exceeded("RPC deadline exceeded".into())
                    }
                    _ if closed(&error) => closed_early(Some(error)),
                    _ => error.into(),
                });
            }
        };
        if buffer.is_empty() {
            return Err(closed_early(None));
        }
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
/// Whether `error` says the other end closed or reset the connection.
fn closed(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}
/// A connection that ended before a whole line came: the session, or the
/// client, is gone. `unavailable`, as a session that cannot be reached is;
/// the reset, when there was one, is its source.
#[derive(Debug)]
pub(crate) struct Closed(Option<std::io::Error>);
impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the connection closed before a complete message")
    }
}
impl std::error::Error for Closed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}
/// What a session that may have ended suggests.
pub(crate) const CLOSED_HINT: &str =
    "the session may have ended; capturefab session list shows those running";
fn closed_early(error: Option<std::io::Error>) -> anyhow::Error {
    Closed(error).into()
}
/// HMAC-SHA256 of `message` under `key` (RFC 2104).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner = Sha256::new()
        .chain_update(block.map(|byte| byte ^ 0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(block.map(|byte| byte ^ 0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}
/// 32 random bytes in hex: a token or a challenge.
fn random_hex() -> Result<String> {
    crate::types::random_hex(32)
}
/// Whether `text` is 64 hex digits, as tokens and challenges are.
fn is_secret_shaped(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}
/// What proves to a client that greeted with `challenge` that the session
/// holds `token`.
fn proof(token: &str, challenge: &str) -> String {
    let mut message = b"capturefab session proof\n".to_vec();
    message.extend_from_slice(challenge.as_bytes());
    hex(hmac_sha256(token.as_bytes(), &message))
}
/// Write `value` as one line within `deadline`.
fn write_line(stream: &mut TcpStream, value: &impl Serialize, deadline: Instant) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut offset = 0;
    while offset < line.len() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let count = match stream.write(&line[offset..]) {
            Ok(count) => count,
            // As a read is, by a signal's handler.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(match error.kind() {
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                        deadline_exceeded("RPC deadline exceeded".into())
                    }
                    _ if closed(&error) => closed_early(Some(error)),
                    _ => error.into(),
                });
            }
        };
        if count == 0 {
            return Err(closed_early(None));
        }
        offset += count;
    }
    Ok(())
}
fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0, |v, (x, y)| v | (x ^ y)) == 0
}
/// Greet a client: answer its challenge with proof of the token, then read
/// its request and check that it holds the token too. Until then the
/// connection holds no client slot, and a newer one may close it.
fn authenticate(stream: &mut TcpStream, token: &str, deadline: Instant) -> Result<Request> {
    let line = read_rpc_line(stream.try_clone()?, MAX_HELLO, deadline)?;
    let hello: Hello = serde_json::from_slice(&line).context("invalid RPC greeting")?;
    ensure!(
        hello.version == VERSION,
        "session protocol version mismatch"
    );
    ensure!(
        is_secret_shaped(&hello.hello),
        "an RPC greeting is 64 hex digits"
    );
    let answer = Proof {
        version: VERSION,
        proof: proof(token, &hello.hello),
    };
    write_line(stream, &answer, deadline)?;
    let line = read_rpc_line(stream.try_clone()?, MAX_REQUEST, deadline)?;
    let request: Request = serde_json::from_slice(&line).context("invalid RPC request")?;
    ensure!(
        request.version == VERSION,
        "session protocol version mismatch"
    );
    ensure!(
        constant_eq(token, &request.token),
        "unauthorized RPC request"
    );
    Ok(request)
}
fn serve_client(
    mut stream: TcpStream,
    h: SessionHandle,
    token: String,
    shared: Arc<Shared>,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    let greeting = Greeting::enter(&shared.greetings, &stream);
    let request = authenticate(&mut stream, &token, rpc_deadline(GREETING)?);
    drop(greeting);
    let slot = ClientSlot::acquire(&shared.clients);
    let result = match (request, slot) {
        (Err(error), _) => Err(error),
        (Ok(_), None) => Err(busy(format!(
            "the session is serving {MAX_CLIENTS} clients already"
        ))),
        (Ok(request), Some(_slot)) => {
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            answer(request, &stream, &h, &shared)
        }
    };
    let response = match result {
        Ok(v) => json!({"version":VERSION,"ok":true,"result":v}),
        // A request the caller must change keeps exit code 2 across the
        // connection, and every failure its reason, hint and details; the
        // text alone may not say so.
        Err(e) => json!({
            "version": VERSION,
            "ok": false,
            "error": crate::session::protocol::wire_error(&e),
        }),
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}
/// What a client that proved itself is answered: `stream` is its
/// connection, which tells whether it went away.
fn answer(
    request: Request,
    stream: &TcpStream,
    h: &SessionHandle,
    shared: &Shared,
) -> Result<Value> {
    let keep = request.keep_ms.map(Duration::from_millis);
    if request.probe {
        ensure!(
            request.command.is_none(),
            "session probes cannot include a command"
        );
        let mut probe = json!({"probe": true});
        let session = shared.idle_left().filter(|_| !h.has_jobs());
        // A camera is let go when its own keep runs out, or else with
        // the session.
        let left = match request.camera.as_deref() {
            None => session,
            Some(camera) => match h.unused_left(camera) {
                Some(crate::session::Unused::After(left)) => {
                    Some(session.map_or(left, |session| session.min(left)))
                }
                Some(crate::session::Unused::WithSession) => session,
                Some(crate::session::Unused::InUse) | None => None,
            },
        };
        if let Some(left) = left {
            probe["idle_ms"] = json!(millis(left));
        }
        return Ok(probe);
    }
    if request.stop {
        ensure!(
            request.command.is_none() && request.camera.is_none(),
            "a stop request cannot include a command or camera"
        );
        shared.quit(Quit::Stop);
        return Ok(json!({"stopping":true}));
    }
    // A session that is ending takes no keep and holds no camera for a
    // command about to use it: told so, a client that did not choose this
    // session goes on without it, as when it is gone.
    if request.command.is_none() && shared.quitting() {
        return Err(crate::session::stopping());
    }
    if let Some(keep) = keep {
        shared.keep(keep);
        shared.used();
    }
    let Some(command) = request.command else {
        let Some(camera) = request.camera.as_deref() else {
            ensure!(keep.is_some(), "RPC request requires a command");
            return Ok(json!({"kept":true}));
        };
        // A command is about to use the camera: it stays, unless it was
        // let go already, which the command connects again.
        shared.used();
        if h.hold_camera(Some(camera), keep).is_none() {
            return Err(crate::types::Categorized::new(
                crate::types::ErrorKind::CameraNotFound,
                format!(
                    "camera '{}' is not connected in this session",
                    crate::media::redact_url(camera)
                ),
            )
            .into());
        }
        return Ok(json!({"kept":true}));
    };
    command.validate()?;
    ensure!(
        !matches!(&command, SessionCommand::Capture { stdout: None, .. }),
        "a capture to stdout through a session needs a stdout file"
    );
    // Looking at a session, or waiting for its jobs, is not using it.
    let use_ = !matches!(
        command,
        SessionCommand::Status | SessionCommand::Jobs | SessionCommand::Wait { .. }
    );
    // Past MAX_WAITERS, a wait is answered after a moment instead of
    // held open, so waits never take every client slot; its client asks
    // again, which keeps the job's lease as a long wait does.
    let (command, _waiting) = match command {
        SessionCommand::Wait {
            job,
            after_seq,
            timeout_ms,
        } => {
            let waiter = Waiter::enter(&shared.waiting);
            let timeout_ms = match waiter {
                Some(_) => timeout_ms,
                None => timeout_ms.min(SHORT_WAIT_MS),
            };
            let wait = SessionCommand::Wait {
                job,
                after_seq,
                timeout_ms,
            };
            (wait, waiter)
        }
        command => (command, None),
    };
    let cameras = matches!(
        command,
        SessionCommand::Connect { .. } | SessionCommand::Disconnect
    );
    // The camera a command uses stays while it runs, and its keep counts
    // from its end.
    let using = (use_
        && !cameras
        && !matches!(
            command,
            SessionCommand::NetworkScan { .. } | SessionCommand::StopJob { .. }
        ))
    .then(|| h.hold_camera(request.camera.as_deref(), keep))
    .flatten();
    if use_ {
        shared.busy.fetch_add(1, Ordering::AcqRel);
        shared.used();
    }
    let result = h.request_for(request.camera.as_deref(), command, &|| peer_closed(stream));
    drop(using);
    if use_ {
        shared.used();
        shared.busy.fetch_sub(1, Ordering::AcqRel);
    }
    if cameras {
        // A camera connected is kept as a command's would be.
        if let Some(id) = result
            .as_ref()
            .ok()
            .and_then(|result| result["connected"]["id"].as_str())
        {
            drop(h.hold_camera(Some(id), keep));
        }
        shared.publish(h.held_cameras());
    }
    result
}
/// Whether the client on `stream` went away while its request ran. It
/// sends nothing after its request, so the end of its stream, or an error,
/// means it closed the connection or died.
fn peer_closed(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let closed = match stream.peek(&mut [0u8; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ),
    };
    let _ = stream.set_nonblocking(false);
    closed
}
/// A client waiting for a job, counted while it waits; none past
/// `MAX_WAITERS`.
struct Waiter<'a>(&'a AtomicUsize);
impl<'a> Waiter<'a> {
    fn enter(waiting: &'a AtomicUsize) -> Option<Self> {
        let waiter = Self(waiting);
        (waiting.fetch_add(1, Ordering::AcqRel) < MAX_WAITERS).then_some(waiter)
    }
}
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
/// The session cannot take another client now.
fn busy(message: String) -> anyhow::Error {
    crate::types::Categorized::new(crate::types::ErrorKind::Unavailable, message)
        .hint("retry in a moment")
        .into()
}
/// Publish `descriptor` atomically, so discovery never reads a partially
/// written file. It is not synced to the disk: it means something only
/// while its session runs, which no power loss outlives.
fn write_descriptor(dir: &Path, descriptor: &Descriptor) -> Result<()> {
    let path = descriptor_path_in(dir, &descriptor.name)?;
    let mut bytes = serde_json::to_vec(descriptor)?;
    bytes.push(b'\n');
    crate::dirs::write_private(&path, &bytes, false).with_context(|| {
        format!(
            "cannot create session '{}' in {}",
            descriptor.name,
            dir.display()
        )
    })
}
impl Server {
    /// Serve `h` as session `name`, which exits after `idle` without use
    /// when one is given.
    pub fn start(h: SessionHandle, name: &str, idle: Option<Duration>) -> Result<Self> {
        Self::start_in(h, name, &crate::dirs::runtime()?, idle)
    }
    fn start_in(h: SessionHandle, name: &str, dir: &Path, idle: Option<Duration>) -> Result<Self> {
        validate_name(name)?;
        crate::dirs::ensure_private_dir(dir)?;
        let path = descriptor_path_in(dir, name)?;
        let name_lock = lock_file(&dir.join(format!("{name}.session.lock")))?;
        if let Err(error) = name_lock.try_lock_exclusive() {
            if error.raw_os_error() != fs2::lock_contended_error().raw_os_error() {
                return Err(anyhow::Error::from(error).context(format!(
                    "session '{name}' already exists; choose another --name or use --session {name}"
                )));
            }
            let owner = read_descriptor_in(dir, name)
                .map(|owner| format!(" (pid {})", owner.pid))
                .unwrap_or_default();
            anyhow::bail!(
                "session '{name}' is already running{owner}; use --session {name} to talk to it or choose another --name"
            );
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        let token = random_hex().context("session token")?;
        let descriptor = Descriptor {
            version: VERSION,
            name: name.into(),
            pid: std::process::id(),
            address: listener.local_addr()?,
            token: token.clone(),
            started_ms: crate::scheduling::now_ms(),
            cameras: h.held_cameras(),
        };
        if let Ok(existing) = read_descriptor_in(dir, name) {
            ensure!(
                !descriptor_active(&existing),
                "session '{name}' is already running (pid {}); use --session {name} to talk to it or choose another --name",
                existing.pid
            );
            fs::remove_file(&path)?;
        } else if other_protocol(&path, name) {
            // Sessions of every version hold the name lock while they run,
            // so this one was left by a session that crashed or was killed.
            fs::remove_file(&path)?;
        }
        ensure!(
            !path.exists(),
            "session '{name}' already exists with an invalid descriptor; inspect {} before removing it",
            path.display()
        );
        write_descriptor(dir, &descriptor)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let server_token = token.clone();
        let shared = Arc::new(Shared {
            dir: dir.to_owned(),
            descriptor: std::sync::Mutex::new(descriptor),
            used: std::sync::Mutex::new(Instant::now()),
            busy: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            clients: Arc::default(),
            greetings: Greetings::default(),
            idle: std::sync::Mutex::new(idle),
            quit: Default::default(),
        });
        let thread_shared = shared.clone();
        let handle = h.clone();
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
                    // Each connection greets on a thread of its own, which
                    // takes a client slot once it proves itself.
                    let h = h.clone();
                    let token = server_token.clone();
                    let shared = thread_shared.clone();
                    let spawned = std::thread::Builder::new()
                        .name("capturefab-rpc-client".into())
                        .spawn(move || {
                            let _ = serve_client(stream, h, token, shared);
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
            path,
            token,
            address,
            stop,
            thread: Some(thread),
            shared,
            handle,
            _name_lock: name_lock,
        })
    }
    /// What a signal handler ends the session with.
    pub fn quitter(&self) -> Quitter {
        Quitter(self.shared.clone())
    }
    /// The descriptor as published now.
    pub fn descriptor(&self) -> Descriptor {
        Shared::locked(&self.shared.descriptor).clone()
    }
    /// Block until the session should end: a signal, `session stop`, or its
    /// idle time passed with no request and no job running. `jobs_running`
    /// is asked only once the idle time has passed.
    pub fn wait(&self, mut jobs_running: impl FnMut() -> bool) -> Quit {
        let (quit, changed) = &self.shared.quit;
        let mut why = Shared::locked(quit);
        loop {
            if let Some(why) = *why {
                return why;
            }
            // Cameras let go once their keep ran out unused leave the
            // descriptor within a second.
            drop(why);
            self.shared.publish(self.handle.held_cameras());
            why = Shared::locked(quit);
            if let Some(why) = *why {
                return why;
            }
            let idle = *Shared::locked(&self.shared.idle);
            let wait = match idle {
                None => Duration::from_secs(3600),
                Some(idle) => {
                    let unused = Shared::locked(&self.shared.used).elapsed();
                    if unused >= idle && self.shared.busy.load(Ordering::Acquire) == 0 {
                        drop(why);
                        let running = jobs_running();
                        why = Shared::locked(quit);
                        if !running && why.is_none() {
                            *why = Some(Quit::Idle);
                            continue;
                        }
                        // Jobs count as use: look again a second later.
                        self.shared.used();
                        Duration::from_secs(1)
                    } else {
                        idle.saturating_sub(unused).max(Duration::from_millis(50))
                    }
                }
            };
            why = changed
                .wait_timeout(why, wait.min(Duration::from_secs(1)))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
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
    call_keeping(name, camera, command, timeout, None)
}
/// `call_to`, and keep the session at least `keep` after this request.
pub fn call_keeping(
    name: &str,
    camera: Option<&str>,
    command: SessionCommand,
    timeout: Duration,
    keep: Option<Duration>,
) -> Result<Value> {
    command.validate()?;
    ensure!(
        !matches!(&command, SessionCommand::Capture { stdout: None, .. }),
        "a capture to stdout through a session needs a stdout file"
    );
    if let Some(camera) = camera {
        crate::session::validate_selector(camera)?;
    }
    ensure!(!timeout.is_zero(), "RPC timeout must be positive");
    let descriptor = read_descriptor(name)?;
    send(
        &descriptor,
        Request {
            camera: camera.map(str::to_owned),
            command: Some(command),
            keep_ms: keep.map(millis),
            ..Request::to(&descriptor)
        },
        timeout,
    )
}
/// Say to the session `descriptor` names that a command is about to use
/// its `camera`: keep the session, and the camera, at least `keep` after
/// now. `camera_not_found` when the session let go of the camera since the
/// descriptor listed it.
pub fn touch(descriptor: &Descriptor, camera: &str, keep: Option<Duration>) -> Result<()> {
    send(
        descriptor,
        Request {
            camera: Some(camera.to_owned()),
            keep_ms: keep.map(millis),
            ..Request::to(descriptor)
        },
        Duration::from_secs(2),
    )
    .map(drop)
}
/// Keep session `name`, and its `camera` when given, at least `keep` after
/// now.
pub fn keep(name: &str, camera: Option<&str>, keep: Duration) -> Result<()> {
    let descriptor = read_descriptor(name)?;
    send(
        &descriptor,
        Request {
            camera: camera.map(str::to_owned),
            keep_ms: Some(millis(keep)),
            ..Request::to(&descriptor)
        },
        Duration::from_secs(2),
    )
    .map(drop)
}
/// Ask session `name` to finish its recordings and exit.
pub fn stop(descriptor: &Descriptor) -> Result<()> {
    send(
        descriptor,
        Request {
            stop: true,
            ..Request::to(descriptor)
        },
        Duration::from_secs(2),
    )
    .map(drop)
}
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
impl Request {
    /// A request to `descriptor` with nothing in it yet.
    fn to(descriptor: &Descriptor) -> Self {
        Self {
            version: VERSION,
            token: descriptor.token.clone(),
            camera: None,
            command: None,
            probe: false,
            keep_ms: None,
            stop: false,
        }
    }
}
fn rpc_network_error(name: &str, phase: &str, error: std::io::Error) -> anyhow::Error {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => deadline_exceeded(
            format!("RPC deadline exceeded while {phase} to session '{name}'"),
        ),
        _ => anyhow::Error::new(error)
            .context(format!("session '{name}' is not responding while {phase}")),
    }
}
/// A session that does not answer its descriptor's challenge with proof
/// of the token: whatever listens on its port is not that session.
fn unproven(name: &str) -> anyhow::Error {
    crate::types::Categorized::new(
        crate::types::ErrorKind::Unavailable,
        format!("session '{name}' did not prove it is the session its descriptor names"),
    )
    .hint("another program may have the session's port; capturefab session list shows the sessions running")
    .into()
}
fn send_request(
    descriptor: &Descriptor,
    camera: Option<&str>,
    command: Option<SessionCommand>,
    probe: bool,
    timeout: Duration,
) -> Result<Value> {
    send(
        descriptor,
        Request {
            camera: camera.map(str::to_owned),
            command,
            probe,
            ..Request::to(descriptor)
        },
        timeout,
    )
}
/// Requests in flight by their owner, a command a shell runs or one that
/// stops waiting for its own, so it can close those it no longer waits
/// for: the session then cancels what only they waited for, as it does
/// when a command's process exits.
static OWNED: std::sync::Mutex<Vec<(u64, u64, TcpStream)>> = std::sync::Mutex::new(Vec::new());
static NEXT_OWNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static NEXT_OWNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
thread_local! {
    static OWNER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// An owner for requests that close together.
pub fn new_owner() -> u64 {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}
/// Requests this thread sends from now on belong to `owner`.
pub fn own_requests(owner: u64) {
    OWNER.with(|cell| cell.set(owner));
}
/// The owner of the requests this thread sends, 0 for none.
pub fn requests_owner() -> u64 {
    OWNER.with(std::cell::Cell::get)
}
/// Close the connections of `owner`'s requests in flight.
pub fn close_requests(owner: u64) {
    let owned = OWNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (_, _, stream) in owned.iter().filter(|(of, _, _)| *of == owner) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}
/// A request's connection, listed while it is in flight.
struct Owned(Option<u64>);
impl Owned {
    fn list(stream: &TcpStream) -> Self {
        let owner = OWNER.with(std::cell::Cell::get);
        let Some(clone) = (owner != 0).then(|| stream.try_clone().ok()).flatten() else {
            return Self(None);
        };
        let key = NEXT_OWNED.fetch_add(1, Ordering::Relaxed);
        OWNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((owner, key, clone));
        Self(Some(key))
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(key) = self.0 {
            OWNED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|(_, listed, _)| *listed != key);
        }
    }
}

fn send(descriptor: &Descriptor, request: Request, timeout: Duration) -> Result<Value> {
    let deadline = rpc_deadline(timeout)?;
    let request = serde_json::to_vec(&request)?;
    ensure!(request.len() < MAX_REQUEST, "RPC request exceeds 1 MiB");
    let name = &descriptor.name;
    let mut stream = TcpStream::connect_timeout(&descriptor.address, remaining(deadline)?)
        .map_err(|error| rpc_network_error(name, "connecting", error))?;
    let _owned = Owned::list(&stream);
    // The session proves it holds the token before it is sent the token
    // or the request, which may carry a camera's credentials.
    let challenge = random_hex()?;
    let hello = Hello {
        version: VERSION,
        hello: challenge.clone(),
    };
    write_line(&mut stream, &hello, deadline)
        .with_context(|| format!("greeting session '{name}'"))?;
    let line = read_rpc_line(stream.try_clone()?, MAX_HELLO, deadline)
        .with_context(|| format!("greeting session '{name}'"))?;
    let answer: Value = serde_json::from_slice(&line).map_err(|_| unproven(name))?;
    if answer["version"] == VERSION && answer["ok"] == false {
        return Err(reply_error(&answer));
    }
    let proven = serde_json::from_value::<Proof>(answer).is_ok_and(|answer| {
        answer.version == VERSION
            && constant_eq(&answer.proof, &proof(&descriptor.token, &challenge))
    });
    if !proven {
        return Err(unproven(name));
    }
    // A single budget covers connect, sending, and the response. A timeout is
    // never retried because the coordinator may already have applied a write.
    let mut message = request;
    message.push(b'\n');
    let mut offset = 0;
    while offset < message.len() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let count = match stream.write(&message[offset..]) {
            Ok(count) => count,
            // As a read is, by a signal's handler.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if closed(&error) => {
                return Err(closed_early(Some(error))
                    .context(format!("sending a request to session '{name}'")));
            }
            Err(error) => return Err(rpc_network_error(name, "sending a request", error)),
        };
        if count == 0 {
            return Err(
                closed_early(None).context(format!("sending a request to session '{name}'"))
            );
        }
        offset += count;
    }
    let line = read_rpc_line(stream, MAX_RESPONSE, deadline).with_context(|| {
        format!("waiting for session '{name}' response; an accepted command may still complete")
    })?;
    let response: Value = serde_json::from_slice(&line).context("invalid RPC response")?;
    ensure!(
        response["version"] == VERSION,
        "session protocol version mismatch"
    );
    if response["ok"] != true {
        return Err(reply_error(&response));
    }
    response
        .get("result")
        .cloned()
        .context("invalid RPC response: missing result")
}
/// The error a session replied with, with its reason, hint and details.
fn reply_error(response: &Value) -> anyhow::Error {
    let message = response["error"]["message"]
        .as_str()
        .unwrap_or("session request failed")
        .to_owned();
    let tag = serde_json::from_value::<crate::types::ErrorTag>(response["error"].clone()).ok();
    crate::types::error_from_tag(tag, message)
}
fn descriptor_active(descriptor: &Descriptor) -> bool {
    send_request(descriptor, None, None, true, PROBE_TIMEOUT)
        .is_ok_and(|response| response["probe"] == true)
}
fn descriptors_in(dir: &Path) -> Result<Vec<Descriptor>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    crate::dirs::check_private_dir(dir)?;
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
/// or their logs.
pub fn active_sessions() -> Result<Vec<Descriptor>> {
    // Without a runtime folder no session can run.
    let Ok(dir) = crate::dirs::runtime() else {
        return Ok(Vec::new());
    };
    Ok(descriptors_in(&dir)?
        .into_iter()
        .filter(descriptor_active)
        .collect())
}
/// The running sessions' descriptors, read locally without asking any of
/// them: a client matches its camera against these and sends one request.
pub fn sessions() -> Vec<Descriptor> {
    let Ok(dir) = crate::dirs::runtime() else {
        return Vec::new();
    };
    descriptors_in(&dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|descriptor| running_in(&dir, descriptor))
        .collect()
}
/// Whether `descriptor` is a running session's: a process holds its name
/// lock, as every session does while it runs. A session that was killed
/// leaves its descriptor until the next one by that name, and its pid may
/// name another process by then.
fn running_in(dir: &Path, descriptor: &Descriptor) -> bool {
    let held = fs::File::open(dir.join(format!("{}.session.lock", descriptor.name)))
        .is_ok_and(|lock| FileExt::try_lock_shared(&lock).is_err());
    held && process_alive(descriptor.pid)
}
/// How long until session `name` lets go of `camera` unused, when it runs
/// and nothing uses the camera now: what a command the camera it holds
/// refuses is told. The camera goes when its own keep runs out, or else
/// when the session ends unused.
pub fn idle_left(name: &str, camera: &str) -> Option<Duration> {
    let descriptor = running_session(name)?;
    let probe = send_request(&descriptor, Some(camera), None, true, PROBE_TIMEOUT).ok()?;
    probe["idle_ms"].as_u64().map(Duration::from_millis)
}
/// Wait up to `wait` for session `name`, which is ending, to be gone, so
/// another may start under its name: whether it is. The wait is for its
/// name lock, which it holds until it exits, so it ends as the session does.
pub fn wait_ended(name: &str, wait: Duration) -> bool {
    let Ok(lock) = crate::dirs::runtime()
        .and_then(|dir| Ok(fs::File::open(dir.join(format!("{name}.session.lock")))?))
    else {
        return running_session(name).is_none();
    };
    let (ended, gone) = std::sync::mpsc::channel();
    // A wait that outlasts `wait` goes on alone until the session ends;
    // nothing waits for it then.
    let waiting = std::thread::Builder::new()
        .name("capturefab-session-end".into())
        .spawn(move || {
            // Let go at once: a session started next under the name takes
            // the lock for itself.
            if FileExt::lock_shared(&lock).is_ok() {
                let _ = FileExt::unlock(&lock);
            }
            let _ = ended.send(());
        });
    if waiting.is_ok() {
        let _ = gone.recv_timeout(wait);
    }
    running_session(name).is_none()
}
/// Session `name`'s descriptor, when that session runs.
pub fn running_session(name: &str) -> Option<Descriptor> {
    let dir = crate::dirs::runtime().ok()?;
    read_descriptor_in(&dir, name)
        .ok()
        .filter(|descriptor| running_in(&dir, descriptor))
}
/// Remove the descriptor a session that no longer runs left under `name`.
/// False when there was none, or a session holds the name.
pub fn remove_stale(name: &str) -> Result<bool> {
    validate_name(name)?;
    let dir = crate::dirs::runtime()?;
    let lock = lock_file(&dir.join(format!("{name}.session.lock")))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(false);
    }
    match fs::remove_file(descriptor_path_in(&dir, name)?) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
/// Wait for the turn to start session `name`: commands that would start it
/// at once take turns, so the first starts it and the others find it
/// running. The turn lasts while the returned file is open.
pub fn start_turn(name: &str) -> Result<fs::File> {
    validate_name(name)?;
    let dir = crate::dirs::runtime()?;
    crate::dirs::ensure_private_dir(&dir)?;
    let lock = lock_file(&dir.join(format!("{name}.start.lock")))?;
    lock.lock_exclusive()
        .with_context(|| format!("waiting to start session {name}"))?;
    Ok(lock)
}
/// Whether process `pid` exists.
pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let sent = unsafe { libc::kill(pid, 0) };
        sent == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };
        // SAFETY: the handle opened is only waited on, without waiting,
        // and closed.
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if process.is_null() {
                // One this user may not open runs all the same; a pid
                // that names no process is an invalid parameter.
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            // A process that exited is still there while a handle to it
            // is open, signalled.
            let running = WaitForSingleObject(process, 0) == WAIT_TIMEOUT;
            CloseHandle(process);
            running
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
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
            crate::dirs::ensure_private_dir(&path).unwrap();
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
    /// Request threads' shared state for a session that is not published.
    fn test_shared() -> Arc<Shared> {
        Arc::new(Shared {
            dir: std::env::temp_dir(),
            descriptor: std::sync::Mutex::new(test_descriptor("t", "127.0.0.1:1".parse().unwrap())),
            used: std::sync::Mutex::new(Instant::now()),
            busy: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            clients: Arc::default(),
            greetings: Greetings::default(),
            idle: std::sync::Mutex::new(None),
            quit: Default::default(),
        })
    }
    /// Greet the session on `client` as a client does, for `token`: its
    /// proof, checked.
    fn greet(client: &mut TcpStream, token: &str) {
        let challenge = random_hex().unwrap();
        let hello = Hello {
            version: VERSION,
            hello: challenge.clone(),
        };
        write_line(
            client,
            &hello,
            rpc_deadline(Duration::from_secs(5)).unwrap(),
        )
        .unwrap();
        let line = read_line(&mut BufReader::new(client.try_clone().unwrap()), MAX_HELLO).unwrap();
        let answer: Proof = serde_json::from_slice(&line).unwrap();
        assert_eq!(answer.proof, proof(token, &challenge));
    }
    /// What a session that holds `token` answers a greeting on `stream`
    /// with, as a stand-in for one.
    fn answer_greeting(stream: &mut TcpStream, token: &str) {
        let line = read_line(&mut BufReader::new(stream.try_clone().unwrap()), MAX_HELLO).unwrap();
        let hello: Hello = serde_json::from_slice(&line).unwrap();
        let answer = Proof {
            version: VERSION,
            proof: proof(token, &hello.hello),
        };
        write_line(
            stream,
            &answer,
            rpc_deadline(Duration::from_secs(5)).unwrap(),
        )
        .unwrap();
    }
    fn test_descriptor(name: &str, address: SocketAddr) -> Descriptor {
        Descriptor {
            version: VERSION,
            name: name.into(),
            pid: std::process::id(),
            address,
            token: "a".repeat(64),
            started_ms: 0,
            cameras: Vec::new(),
        }
    }
    #[test]
    fn a_missing_session_names_the_running_ones_and_the_likely_typo() {
        let address = "127.0.0.1:1".parse().unwrap();
        let none = missing_session_error("bnech", &[]);
        let bench = test_descriptor("bench", address);
        let lab = test_descriptor("lab", address);
        let typo = missing_session_error("bnech", &[bench.clone(), lab.clone()]);
        let unrelated = missing_session_error("factory", &[bench, lab]);
        assert!(
            none.to_string().contains("no sessions are running"),
            "{none}"
        );
        assert!(
            typo.to_string()
                .contains("Running sessions: bench, lab; did you mean --session bench?"),
            "{typo}"
        );
        assert!(
            unrelated
                .to_string()
                .contains("Running sessions: bench, lab"),
            "{unrelated}"
        );
        assert!(
            !unrelated.to_string().contains("did you mean"),
            "{unrelated}"
        );
        for error in [none, typo, unrelated] {
            assert_eq!(crate::cli::error_code(&error), ("session_not_found", 3));
            assert!(!format!("{error:#}").contains("os error"), "{error:#}");
        }
        let dir = TestDir::new();
        let error = read_descriptor_in(&dir.0, "bnech").unwrap_err();
        assert!(error.is::<MissingSession>(), "{error:#}");
    }

    #[test]
    fn a_second_server_with_the_same_name_says_it_is_running() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let _server = Server::start_in(handle.clone(), "bench", &dir.0, None).unwrap();
        let error = match Server::start_in(handle.clone(), "bench", &dir.0, None) {
            Ok(_) => panic!("a second bench started"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains("is already running (pid"), "{error}");
        assert!(!error.contains("os error"), "{error}");
        handle.shutdown();
    }

    #[test]
    fn authenticated_probes_do_not_change_camera_state_or_logs() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server = Server::start_in(handle.clone(), "bench", &dir.0, None).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "bench").unwrap();
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
        drop(server);
        assert!(!descriptor_path_in(&dir.0, "bench").unwrap().exists());
        handle.shutdown();
    }
    #[test]
    fn a_session_that_is_ending_takes_no_keep_and_holds_no_camera() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server = Server::start_in(handle.clone(), "ending", &dir.0, None).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "ending").unwrap();
        let keep = |camera: Option<&str>| {
            send(
                &descriptor,
                Request {
                    camera: camera.map(str::to_owned),
                    keep_ms: Some(60_000),
                    ..Request::to(&descriptor)
                },
                Duration::from_secs(2),
            )
        };
        keep(None).unwrap();
        server.quitter().quit(Quit::Stop);
        // As it lets its cameras go, it still answers, but a client that
        // would keep it, or use a camera it holds, is told it is stopping,
        // and goes on without it.
        for camera in [None, Some("sim:0")] {
            let error = keep(camera).unwrap_err();
            assert!(crate::session::is_stopping(&error), "{error:#}");
        }
        assert!(descriptor_active(&descriptor));
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
                .write_all(b"{\"version\":3,\"ok\":true,\"result\":{\"probe\":true}}\n")
                .unwrap();
        });
        assert!(!descriptor_active(&descriptor));
        peer.join().unwrap();
    }
    #[test]
    fn a_program_on_a_dead_sessions_port_learns_nothing_and_forges_nothing() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let descriptor = test_descriptor("lab", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = read_line(
                &mut BufReader::new(stream.try_clone().unwrap()),
                MAX_REQUEST,
            )
            .unwrap();
            // It answers as a session would, without the token.
            let challenge = serde_json::from_slice::<Hello>(&hello).unwrap().hello;
            let guess = Proof {
                version: VERSION,
                proof: proof(&"b".repeat(64), &challenge),
            };
            write_line(
                &mut stream,
                &guess,
                rpc_deadline(Duration::from_secs(5)).unwrap(),
            )
            .unwrap();
            stream
                .write_all(b"{\"version\":3,\"ok\":true,\"result\":{\"cameras\":[]}}\n")
                .unwrap();
            // What came after the greeting: nothing.
            let mut rest = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stream, &mut rest);
            (hello, rest)
        });
        let error = send_request(
            &descriptor,
            Some("rtsp://admin:Sup3rSecret@10.1.2.3/live"),
            Some(SessionCommand::Status),
            false,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(
            crate::cli::error_code(&error),
            ("unavailable", 3),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("did not prove"), "{error:#}");
        let (hello, rest) = peer.join().unwrap();
        let sent = format!(
            "{}{}",
            String::from_utf8_lossy(&hello),
            String::from_utf8_lossy(&rest)
        );
        assert!(!sent.contains(&descriptor.token), "{sent}");
        assert!(!sent.contains("Sup3rSecret"), "{sent}");
        assert!(rest.is_empty(), "{sent}");
    }
    #[test]
    fn a_session_that_dies_while_it_answers_is_unavailable() {
        for reset in [false, true] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let descriptor = test_descriptor("rv", listener.local_addr().unwrap());
            let token = descriptor.token.clone();
            let peer = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                answer_greeting(&mut stream, &token);
                read_line(
                    &mut BufReader::new(stream.try_clone().unwrap()),
                    MAX_REQUEST,
                )
                .unwrap();
                if reset {
                    // Closing with unread data resets the connection.
                    socket2_linger_zero(&stream);
                }
            });
            let error = send_request(
                &descriptor,
                None,
                Some(SessionCommand::Status),
                false,
                Duration::from_secs(5),
            )
            .unwrap_err();
            peer.join().unwrap();
            assert_eq!(
                crate::cli::error_code(&error),
                ("unavailable", 3),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("may still complete"),
                "{error:#}"
            );
        }
    }
    /// Close `stream` at once with a reset, as a process killed mid-reply
    /// may leave it.
    fn socket2_linger_zero(stream: &TcpStream) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            // SAFETY: setsockopt on a socket this test owns, with a value of
            // the size it reads.
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    (&raw const linger).cast(),
                    std::mem::size_of::<libc::linger>() as libc::socklen_t,
                );
            }
        }
        let _ = stream;
    }
    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        assert_eq!(
            hex(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block is hashed first.
        assert_eq!(
            hex(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
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
                    Server::start_in(handle, "stale", &dir, None)
                })
            })
            .collect();
        let claims: Vec<_> = claimants
            .into_iter()
            .map(|claim| claim.join().unwrap())
            .collect();
        assert_eq!(claims.iter().filter(|claim| claim.is_ok()).count(), 1);
        for error in claims.iter().filter_map(|claim| claim.as_ref().err()) {
            let error = format!("{error:#}");
            assert!(error.contains("already running"), "{error}");
            assert!(!error.contains("os error"), "{error}");
        }
        let owner = read_descriptor_in(&dir.0, "stale").unwrap();
        assert_ne!(owner.token, descriptor.token);
        assert!(descriptor_active(&owner));
        drop(claims);
        assert!(!descriptor_path_in(&dir.0, "stale").unwrap().exists());
        handle.shutdown();
    }
    /// A signal's handler interrupts a read with a timeout (on Linux even
    /// under SA_RESTART, which this handler does not ask for); the read
    /// goes on within its deadline.
    #[cfg(unix)]
    #[test]
    fn a_signal_does_not_cut_an_rpc_read_short() {
        extern "C" fn nothing(_: libc::c_int) {}
        /// The reading thread, for pthread_kill.
        struct Reader(libc::pthread_t);
        // SAFETY: only an ID, signalled while its thread runs.
        unsafe impl Send for Reader {}
        // SAFETY: a handler that does nothing, for a signal nothing else
        // in these tests sends.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = nothing as *const () as usize;
            assert_eq!(
                libc::sigaction(libc::SIGURG, &action, std::ptr::null_mut()),
                0
            );
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let (said, told) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            // SAFETY: pthread_self only names this thread.
            said.send(Reader(unsafe { libc::pthread_self() })).unwrap();
            read_rpc_line(stream, MAX_HELLO, Instant::now() + Duration::from_secs(30))
        });
        let thread = told.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: the thread runs until the line below arrives.
        assert_eq!(unsafe { libc::pthread_kill(thread.0, libc::SIGURG) }, 0);
        std::thread::sleep(Duration::from_millis(100));
        client.write_all(b"{}\n").unwrap();
        assert_eq!(reader.join().unwrap().unwrap(), b"{}\n");
    }
    #[test]
    fn a_process_lives_until_it_exits() {
        assert!(process_alive(std::process::id()));
        // This test program, listing none of its tests, ends at once.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--list", "--exact", "no-test-has-this-name"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        assert!(!process_alive(pid));
    }
    #[test]
    fn rpc_deadline_bounds_a_response_that_trickles_bytes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let descriptor = test_descriptor("slow", listener.local_addr().unwrap());
        let token = descriptor.token.clone();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            answer_greeting(&mut stream, &token);
            read_line(
                &mut BufReader::new(stream.try_clone().unwrap()),
                MAX_REQUEST,
            )
            .unwrap();
            // A byte at a time, well within any one read's timeout, for
            // far longer than the deadline, until the client hangs up.
            let trickle = Instant::now();
            while trickle.elapsed() < Duration::from_secs(20) {
                if stream.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        // The deadline leaves a busy runner time to connect and greet, so
        // it runs out while the response trickles.
        let started = Instant::now();
        let error = send_request(
            &descriptor,
            None,
            Some(SessionCommand::Status),
            false,
            Duration::from_secs(1),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            format!("{error:#}").contains("deadline exceeded"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("may still complete"));
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        peer.join().unwrap();
    }
    #[test]
    fn sessions_of_an_older_protocol_are_never_chosen_and_free_their_name_once_gone() {
        let dir = TestDir::new();
        let address = "127.0.0.1:12345".parse().unwrap();
        let mut old = test_descriptor("old", address);
        old.version = 1;
        dir.write_descriptor(&old);
        dir.write_descriptor(&test_descriptor("current", address));
        let names: Vec<_> = descriptors_in(&dir.0)
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect();
        assert_eq!(names, ["current"]);
        let error = read_descriptor_in(&dir.0, "old").unwrap_err();
        assert!(error.to_string().contains("protocol version"), "{error:#}");
        let handle = SessionHandle::new();
        // While an older session runs, it holds the name.
        let lock = lock_file(&dir.0.join("old.session.lock")).unwrap();
        lock.try_lock_exclusive().unwrap();
        let error = match Server::start_in(handle.clone(), "old", &dir.0, None) {
            Ok(_) => panic!("a running older session was replaced"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains("is already running"), "{error}");
        assert!(descriptor_path_in(&dir.0, "old").unwrap().exists());
        // Its descriptor outlives a crash, and then the name is free.
        drop(lock);
        let server = Server::start_in(handle.clone(), "old", &dir.0, None).unwrap();
        let current = read_descriptor_in(&dir.0, "old").unwrap();
        assert_eq!(current.version, VERSION);
        assert!(descriptor_active(&current));
        drop(server);
        handle.shutdown();
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
                "{\"version\":2,\"token\":\"t\",\"command\":{\"op\":\"status\"},\"camrea\":\"sim:0\"}\n",
                "camrea",
            ),
            (
                "{\"version\":2,\"token\":\"t\",\"command\":{\"op\":\"status\",\"unknown\":1}}\n",
                "unknown",
            ),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let handle = SessionHandle::new();
            let server = std::thread::spawn({
                let handle = handle.clone();
                move || serve_client(stream, handle, "t".into(), test_shared())
            });
            greet(&mut client, "t");
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
            move || serve_client(stream, h, "t".into(), test_shared())
        });
        std::thread::sleep(Duration::from_millis(200));
        greet(&mut client, "t");
        serde_json::to_writer(
            &mut client,
            &Request {
                token: "t".into(),
                command: Some(SessionCommand::Jobs),
                ..Request::to(&test_descriptor("t", "127.0.0.1:1".parse().unwrap()))
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
    /// A request sent to `serve_client` on a connection of its own: the
    /// client end, and the thread serving it.
    fn served(
        h: &SessionHandle,
        shared: Arc<Shared>,
        command: SessionCommand,
    ) -> (TcpStream, std::thread::JoinHandle<Result<()>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let server = std::thread::spawn({
            let h = h.clone();
            move || serve_client(stream, h, "t".into(), shared)
        });
        greet(&mut client, "t");
        serde_json::to_writer(
            &mut client,
            &Request {
                token: "t".into(),
                command: Some(command),
                ..Request::to(&test_descriptor("t", "127.0.0.1:1".parse().unwrap()))
            },
        )
        .unwrap();
        client.write_all(b"\n").unwrap();
        (client, server)
    }
    #[test]
    fn waits_past_the_limit_are_short_and_end_when_their_client_leaves() {
        let h = SessionHandle::new();
        let job = h.test_job("sim:0");
        let wait = || SessionCommand::Wait {
            job: job.clone(),
            after_seq: 1,
            timeout_ms: 20_000,
        };
        // Past MAX_WAITERS a wait is answered within a moment, not refused.
        let shared = test_shared();
        let waiters = (0..MAX_WAITERS)
            .map(|_| Waiter::enter(&shared.waiting).unwrap())
            .collect::<Vec<_>>();
        let asked = Instant::now();
        let (client, server) = served(&h, shared.clone(), wait());
        let response: Value =
            serde_json::from_slice(&read_line(&mut BufReader::new(client), 1 << 20).unwrap())
                .unwrap();
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["result"]["job"]["state"], "armed");
        assert!(asked.elapsed() < Duration::from_secs(3));
        server.join().unwrap().unwrap();
        drop(waiters);
        // A client that goes away ends its wait.
        let (client, server) = served(&h, shared, wait());
        std::thread::sleep(Duration::from_millis(300));
        let left = Instant::now();
        drop(client);
        let _ = server.join().unwrap();
        assert!(left.elapsed() < Duration::from_secs(3));
        h.shutdown();
    }
    #[test]
    fn a_full_session_says_so_and_waits_leave_room_for_others() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server = Server::start_in(handle.clone(), "full", &dir.0, None).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "full").unwrap();
        let status = || {
            send_request(
                &descriptor,
                None,
                Some(SessionCommand::Status),
                false,
                Duration::from_secs(5),
            )
        };
        // Clients being served take every slot: the next is told so.
        let taken = (0..MAX_CLIENTS)
            .map(|_| ClientSlot::acquire(&server.shared.clients).unwrap())
            .collect::<Vec<_>>();
        let error = status().unwrap_err();
        assert_eq!(
            crate::cli::error_code(&error),
            ("unavailable", 3),
            "{error:#}"
        );
        assert!(
            format!("{error:#}").contains("serving 16 clients"),
            "{error:#}"
        );
        drop(taken);
        status().unwrap();
        // Waits take at most MAX_WAITERS slots.
        let shared = test_shared();
        let waiters = (0..MAX_WAITERS)
            .map(|_| Waiter::enter(&shared.waiting).unwrap())
            .collect::<Vec<_>>();
        assert!(Waiter::enter(&shared.waiting).is_none());
        drop(waiters);
        assert_eq!(shared.waiting.load(Ordering::Acquire), 0);
        handle.shutdown();
    }
    #[test]
    fn connections_that_never_prove_themselves_keep_no_client_out() {
        let dir = TestDir::new();
        let handle = SessionHandle::new();
        let server = Server::start_in(handle.clone(), "flood", &dir.0, None).unwrap();
        let descriptor = read_descriptor_in(&dir.0, "flood").unwrap();
        // Many more connections than client slots, which say nothing or
        // only greet: none of them holds a slot, and the oldest are closed
        // as more come.
        let mut silent = (0..MAX_CLIENTS * 3)
            .map(|_| TcpStream::connect(descriptor.address).unwrap())
            .collect::<Vec<_>>();
        for stream in silent.iter_mut().take(MAX_CLIENTS) {
            let hello = Hello {
                version: VERSION,
                hello: random_hex().unwrap(),
            };
            write_line(
                stream,
                &hello,
                rpc_deadline(Duration::from_secs(1)).unwrap(),
            )
            .unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        for _ in 0..3 {
            send_request(
                &descriptor,
                None,
                Some(SessionCommand::Status),
                false,
                Duration::from_secs(5),
            )
            .unwrap();
        }
        assert_eq!(server.shared.clients.load(Ordering::Acquire), 0);
        let flood = (0..MAX_GREETINGS + 8)
            .map(|_| TcpStream::connect(descriptor.address).unwrap())
            .collect::<Vec<_>>();
        std::thread::sleep(Duration::from_millis(200));
        assert!(Shared::locked(&server.shared.greetings.open).len() <= MAX_GREETINGS);
        // The oldest were closed.
        let mut first = silent.remove(0);
        first
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let ended = match std::io::Read::read_to_end(&mut first, &mut Vec::new()) {
            Ok(_) => true,
            Err(error) => !matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
        };
        assert!(ended, "the oldest connection is still open");
        send_request(
            &descriptor,
            None,
            Some(SessionCommand::Status),
            false,
            Duration::from_secs(5),
        )
        .unwrap();
        // A wrong token is refused once it greeted.
        let mut wrong = descriptor.clone();
        wrong.token = "f".repeat(64);
        let error = send_request(
            &wrong,
            None,
            Some(SessionCommand::Status),
            false,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("did not prove"), "{error:#}");
        drop((silent, flood));
        handle.shutdown();
    }
    #[test]
    fn names_cannot_escape_directory() {
        for name in [
            "../a", "a/b", "", "a.b", "a b", "CON", "aux", "NUL", "Prn", "COM1", "lpt9",
        ] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("bench-01").is_ok());
        assert!(validate_name("storage").is_ok());
        assert!(validate_name("com10").is_ok());
        assert!(validate_name("lpt0").is_ok());
    }
}
