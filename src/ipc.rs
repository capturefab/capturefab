//! Versioned newline-delimited JSON over loopback, with a private per-session bearer token.
use crate::session::{SessionCommand, SessionHandle};
use anyhow::{Context, Result, ensure};
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
    time::Duration,
};

pub const VERSION: u32 = 1;
const MAX_REQUEST: usize = 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub version: u32,
    pub name: String,
    pub pid: u32,
    pub address: SocketAddr,
    pub token: String,
}
#[derive(Serialize, Deserialize)]
struct Request {
    version: u32,
    token: String,
    #[serde(default)]
    camera: Option<String>,
    command: SessionCommand,
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
pub struct Server {
    pub name: String,
    path: PathBuf,
    token: String,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
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
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !matches!(name, "storage" | "storage-policy"),
        "session name is reserved for storage bookkeeping; choose a different name"
    );
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "session names use 1..64 letters, digits, '-' or '_'"
    );
    Ok(())
}
fn descriptor_path(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(session_dir().join(format!("{name}.json")))
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
    let path = descriptor_path(name)?;
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
            && d.token.len() == 64,
        "invalid session descriptor or protocol version"
    );
    Ok(d)
}
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
fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0, |v, (x, y)| v | (x ^ y)) == 0
}
fn serve_client(mut stream: TcpStream, h: SessionHandle, token: String) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let line = read_line(&mut BufReader::new(stream.try_clone()?), MAX_REQUEST)?;
    let request: Request = serde_json::from_slice(&line)?;
    ensure!(
        request.version == VERSION && constant_eq(&token, &request.token),
        "unauthorized RPC request"
    );
    ensure!(
        !matches!(&request.command,SessionCommand::Capture{output,..} if output=="-"),
        "remote captures require an output file"
    );
    let result = if let Some(camera) = request.camera {
        h.request_to(&camera, request.command)
    } else {
        h.request(request.command)
    };
    let response = match result {
        Ok(v) => json!({"version":VERSION,"ok":true,"result":v}),
        Err(e) => json!({"version":VERSION,"ok":false,"error":{"message":format!("{e:#}")}}),
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}
impl Server {
    pub fn start(h: SessionHandle, name: &str) -> Result<Self> {
        validate_name(name)?;
        let dir = session_dir();
        ensure_private_dir(&dir)?;
        let path = descriptor_path(name)?;
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
            pid: std::process::id(),
            address: listener.local_addr()?,
            token: token.clone(),
        };
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file=options.open(&path).with_context(||format!("session '{name}' already exists or cannot be created; choose another --name (stale descriptors are in {})",dir.display()))?;
        if let Err(e) = serde_json::to_writer(&mut file, &descriptor)
            .and_then(|_| file.write_all(b"\n").map_err(serde_json::Error::io))
        {
            let _ = fs::remove_file(&path);
            return Err(e.into());
        }
        file.sync_all()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let server_token = token.clone();
        let clients = Arc::new(AtomicUsize::new(0));
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
                    let spawned = std::thread::Builder::new()
                        .name("capturefab-rpc-client".into())
                        .spawn(move || {
                            let _slot = slot;
                            let _ = serve_client(stream, h, token);
                        });
                    if let Err(e) = spawned {
                        eprintln!("capturefab: rejected RPC client: {e}");
                    }
                }
            })?;
        Ok(Self {
            name: name.into(),
            path,
            token,
            address,
            stop,
            thread: Some(thread),
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
    let descriptor = read_descriptor(name)?;
    let mut stream = TcpStream::connect_timeout(&descriptor.address, Duration::from_secs(2))
        .with_context(|| format!("session '{name}' is not responding; it may be stale"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    serde_json::to_writer(
        &mut stream,
        &Request {
            version: VERSION,
            token: descriptor.token,
            camera: camera.map(str::to_owned),
            command,
        },
    )?;
    stream.write_all(b"\n")?;
    let response: Value =
        serde_json::from_slice(&read_line(&mut BufReader::new(stream), 16 * 1024 * 1024)?)?;
    ensure!(
        response["version"] == VERSION,
        "session protocol version mismatch"
    );
    ensure!(
        response["ok"] == true,
        "{}",
        response["error"]["message"]
            .as_str()
            .unwrap_or("session request failed")
    );
    Ok(response["result"].clone())
}
pub fn list() -> Result<Value> {
    let dir = session_dir();
    if !dir.exists() {
        return Ok(json!({"sessions":[]}));
    }
    let mut sessions = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned);
        if entry.path().extension().is_some_and(|s| s == "json")
            && let Some(name) = name
            && let Ok(d) = read_descriptor(&name)
        {
            let active = TcpStream::connect_timeout(&d.address, Duration::from_millis(100)).is_ok();
            sessions.push(json!({"name":name,"pid":d.pid,"active":active}));
        }
    }
    sessions.sort_by_key(|s| s["name"].as_str().unwrap_or("").to_owned());
    Ok(json!({"sessions":sessions}))
}

#[cfg(test)]
mod tests {
    use super::*;
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
            move || serve_client(stream, h, "t".into())
        });
        std::thread::sleep(Duration::from_millis(200));
        serde_json::to_writer(
            &mut client,
            &Request {
                version: VERSION,
                token: "t".into(),
                camera: None,
                command: SessionCommand::Jobs,
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
        for name in ["../a", "a/b", "", "a.b", "a b"] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("bench-01").is_ok());
        assert!(validate_name("storage").is_err());
        assert!(validate_name("storage-policy").is_err());
    }
}
