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

pub fn session_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("CAPTUREFAB_SESSION_DIR") {
        return p.into();
    }
    if let Some(p) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(p).join("capturefab");
    }
    #[cfg(windows)]
    if let Some(p) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(p).join("capturefab").join("sessions");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache").join("capturefab").join("sessions")
}
pub fn validate_name(name: &str) -> Result<()> {
    let lower = name.to_ascii_lowercase();
    ensure!(
        !matches!(
            lower.as_str(),
            "storage" | "storage-policy" | "destinations" | "uploads" | "hubs"
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
    if path.exists() {
        let m = fs::symlink_metadata(path)?;
        ensure!(
            m.is_dir() && !m.file_type().is_symlink(),
            "session directory must be a real directory"
        );
    } else {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "session directory must be private (chmod 700 {})",
            path.display()
        );
    }
    Ok(())
}
pub fn read_descriptor(name: &str) -> Result<Descriptor> {
    read_descriptor_in(&session_dir(), name)
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
    pub fn start(h: SessionHandle, name: &str) -> Result<Self> {
        Self::start_with_kind(h, name, SessionKind::Serve)
    }
    pub fn start_with_kind(h: SessionHandle, name: &str, kind: SessionKind) -> Result<Self> {
        Self::start_in(h, name, kind, &session_dir())
    }
    fn start_in(h: SessionHandle, name: &str, kind: SessionKind, dir: &Path) -> Result<Self> {
        validate_name(name)?;
        ensure_private_dir(dir)?;
        let path = descriptor_path_in(dir, name)?;
        let mut lock_options = fs::OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.mode(0o600);
        }
        let name_lock = lock_options.open(dir.join(format!("{name}.session.lock")))?;
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
    Ok(descriptors_in(&session_dir())?
        .into_iter()
        .filter(descriptor_active)
        .collect())
}
pub fn list() -> Result<Value> {
    let sessions: Vec<_> = descriptors_in(&session_dir())?
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
            assert!(
                view.activity
                    .iter()
                    .any(|entry| entry.actor.as_ref() == Some(&actor))
            );
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
        assert!(validate_name("Storage").is_err());
        assert!(validate_name("destinations").is_err());
        assert!(validate_name("uploads").is_err());
        assert!(validate_name("Hubs").is_err());
        assert!(validate_name("com10").is_ok());
        assert!(validate_name("lpt0").is_ok());
    }
}
