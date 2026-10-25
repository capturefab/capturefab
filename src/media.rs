//! FFmpeg-backed media inputs and bounded frame forwarding. The optional FFmpeg
//! executable is embedded at build time and extracted into a private directory.
//! No shell is involved in launching decoders or encoders.
#[cfg(any(test, target_os = "linux"))]
mod v4l2;

use crate::types::{
    Backend, CameraInfo, Categorized, ErrorKind, Frame, RGB8, RegisterIo, Transport,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Reverse,
    collections::{HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const EMBEDDED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/capturefab-ffmpeg.bin"));
const LICENSE: &str = include_str!(concat!(env!("OUT_DIR"), "/capturefab-ffmpeg-license.txt"));
const MAX_FRAME: usize = 256 * 1024 * 1024;
const MAX_LOG: usize = 16 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
static HWACCEL: OnceLock<String> = OnceLock::new();
pub fn configure_hwaccel(value: &str) -> Result<()> {
    validate_hwaccel(value)?;
    if let Err(value) = HWACCEL.set(value.to_owned()) {
        ensure!(
            HWACCEL.get() == Some(&value),
            "hardware acceleration is already configured for {}; restart the process to select {value}",
            HWACCEL.get().unwrap()
        );
    }
    Ok(())
}

pub fn validate_hwaccel(value: &str) -> Result<()> {
    ensure!(
        [
            "auto",
            "none",
            "off",
            "videotoolbox",
            "cuda",
            "qsv",
            "vaapi",
            "d3d11va",
            "dxva2",
            "vulkan",
            "drm",
            "opencl"
        ]
        .contains(&value),
        "unsupported hardware acceleration method {value}; use auto, none, videotoolbox, cuda, qsv, vaapi, d3d11va, dxva2, vulkan, drm or opencl"
    );
    Ok(())
}
pub fn hwaccel() -> String {
    HWACCEL
        .get()
        .cloned()
        .or_else(|| std::env::var("CAPTUREFAB_HWACCEL").ok())
        .unwrap_or_else(|| "auto".into())
}

/// Credential-bearing query names use several case and separator conventions.
/// Decode the name once, as URL query parsers do, while preserving the original
/// spelling when the URL is redacted.
pub(crate) fn credential_query_key(key: &str) -> bool {
    let mut normalized = String::with_capacity(key.len());
    let mut bytes = key.bytes();
    while let Some(mut byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let low = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return false;
            };
            byte = (high * 16 + low) as u8;
        }
        if !matches!(byte, b'-' | b'_') {
            normalized.push(char::from(byte.to_ascii_lowercase()));
        }
    }
    matches!(
        normalized.as_str(),
        "password"
            | "passphrase"
            | "passwd"
            | "pwd"
            | "pw"
            | "token"
            | "key"
            | "auth"
            | "authsig"
            | "signature"
            | "secret"
            | "accesstoken"
            | "streamid"
            | "apikey"
            | "clientsecret"
            | "xamzcredential"
            | "xamzsignature"
            | "xamzsecuritytoken"
            | "xgoogcredential"
            | "xgoogsignature"
            | "xgoogsecuritytoken"
            | "sig"
    )
}

pub fn redact_url(url: &str) -> String {
    let mut result = url.to_string();
    if let Some(scheme_end) = result.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = result[authority_start..]
            .find(['/', '?', '#'])
            .map(|n| authority_start + n)
            .unwrap_or(result.len());
        if let Some(at) = result[authority_start..authority_end].rfind('@') {
            result.replace_range(authority_start..authority_start + at, "<redacted>");
        }
    }
    if let Some(query) = result.find('?') {
        let path = result[..query].to_string();
        let params = result[query + 1..]
            .split('&')
            .map(|item| {
                let (key, value) = item.split_once('=').unwrap_or((item, ""));
                if credential_query_key(key) {
                    format!("{key}=<redacted>")
                } else if value.is_empty() {
                    key.to_string()
                } else {
                    format!("{key}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        result = format!("{path}?{params}");
    }
    // RTMP publishes use the path after the application name as a stream
    // key. Redact it here as well as in tool output so worker status,
    // activity and FFmpeg diagnostics never expose a typed stream password.
    if let Some((protocol, rest)) = result.split_once("://")
        && (protocol.eq_ignore_ascii_case("rtmp") || protocol.eq_ignore_ascii_case("rtmps"))
        && let Some(path) = rest.find('/')
    {
        let start = protocol.len() + 3 + path + 1;
        let end = result[start..]
            .find(['?', '#'])
            .map_or(result.len(), |offset| start + offset);
        if let Some(slash) = result[start..end].find('/') {
            let key = start + slash + 1;
            if key < end {
                result.replace_range(key..end, "<redacted>");
            }
        }
    }
    result
}
fn redact_log(message: &str, source: &str) -> String {
    let canonical = canonical_scheme(source);
    message
        .replace(source, &redact_url(source))
        .replace(&canonical, &redact_url(&canonical))
}
fn scheme(url: &str) -> Option<String> {
    if url.len() >= 3 && url.as_bytes()[1] == b':' && matches!(url.as_bytes()[2], b'\\' | b'/') {
        return None;
    }
    let (name, _) = url.split_once(':')?;
    if name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        Some(name.to_ascii_lowercase())
    } else {
        None
    }
}
/// FFmpeg protocol names are case-sensitive even though URL schemes are not.
/// Preserve the authority, path and query exactly as supplied.
fn canonical_scheme(url: &str) -> String {
    match scheme(url) {
        Some(protocol) => format!("{protocol}:{}", url.split_once(':').unwrap().1),
        None => url.to_owned(),
    }
}
/// Validate the authority and escaping of a network URL without resolving DNS
/// or contacting the service. Error messages never repeat credential-bearing URLs.
pub(crate) fn validate_network_url(url: &str, label: &str) -> Result<()> {
    ensure!(
        url.len() <= 16384 && !url.chars().any(|c| c.is_control() || c.is_whitespace()),
        "{label} must not contain whitespace or control characters; percent-encode URL values"
    );
    let (protocol, rest) = url.split_once("://").context(format!(
        "{label} must include a protocol and host, for example rtsp://camera/live"
    ))?;
    let protocol = protocol.to_ascii_lowercase();
    ensure!(
        scheme(url).as_deref() == Some(protocol.as_str()),
        "{label} protocol is invalid"
    );
    let mut bytes = url.as_bytes().iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'%' {
            ensure!(
                bytes.next().is_some_and(u8::is_ascii_hexdigit)
                    && bytes.next().is_some_and(u8::is_ascii_hexdigit),
                "{label} contains invalid percent escaping; use % followed by two hexadecimal digits"
            );
        }
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = if matches!(protocol.as_str(), "udp" | "rtp") {
        authority.strip_prefix('@').unwrap_or(authority)
    } else if let Some((userinfo, host)) = authority.rsplit_once('@') {
        ensure!(
            !userinfo.is_empty() && !userinfo.contains('@'),
            "{label} credentials are invalid"
        );
        host
    } else {
        authority
    };
    let (host, port) = if let Some(ipv6) = host_port.strip_prefix('[') {
        let (host, suffix) = ipv6.split_once(']').context(format!(
            "{label} IPv6 address is invalid; enclose it in brackets"
        ))?;
        let address = host.split_once("%25").map_or(host, |(ip, _)| ip);
        ensure!(
            address.parse::<std::net::Ipv6Addr>().is_ok(),
            "{label} IPv6 address is invalid"
        );
        if let Some((_, zone)) = host.split_once("%25") {
            ensure!(
                !zone.is_empty()
                    && zone
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "{label} IPv6 zone is invalid"
            );
        }
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .context(format!("{label} port is invalid"))?,
            )
        };
        (host, port)
    } else if let Some((host, port)) = host_port.rsplit_once(':') {
        ensure!(
            !host.contains(':'),
            "{label} IPv6 addresses must be enclosed in brackets"
        );
        (host, Some(port))
    } else {
        (host_port, None)
    };
    let binding = matches!(protocol.as_str(), "udp" | "tcp" | "srt" | "rtp") && port.is_some();
    ensure!(!host.is_empty() || binding, "{label} needs a host");
    if !host_port.starts_with('[') {
        ensure!(
            host.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)),
            "{label} host is invalid; use a DNS name or an IP address"
        );
    }
    if let Some(port) = port {
        ensure!(
            !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port > 0),
            "{label} port must be an integer within 1..65535"
        );
    }
    Ok(())
}

/// Validate media source syntax without opening files, devices or sockets.
pub fn validate_source_shape(url: &str) -> Result<()> {
    ensure!(
        !url.is_empty() && url.len() <= 16384 && !url.chars().any(char::is_control),
        "invalid media source URL"
    );
    if let Some(protocol) = scheme(url) {
        ensure!(
            matches!(
                protocol.as_str(),
                "rtsp"
                    | "rtsps"
                    | "srt"
                    | "rtmp"
                    | "rtmps"
                    | "http"
                    | "https"
                    | "udp"
                    | "tcp"
                    | "rtp"
                    | "file"
                    | "avfoundation"
                    | "v4l2"
                    | "dshow"
            ),
            "unsupported media protocol {protocol}"
        );
        if !matches!(
            protocol.as_str(),
            "file" | "avfoundation" | "v4l2" | "dshow"
        ) {
            validate_network_url(url, "media source URL")?;
        } else if protocol == "file" {
            local_file_path(url)?;
        } else {
            native_source(url)?;
        }
    }
    Ok(())
}

pub fn validate_source(url: &str) -> Result<()> {
    validate_source_shape(url)?;
    if is_recording(url) {
        let path = local_file_path(url)?;
        let metadata = fs::metadata(&path).map_err(|error| {
            let message = if error.kind() == std::io::ErrorKind::NotFound {
                format!("media file not found: {}", path.display())
            } else {
                format!("cannot inspect media file {}", path.display())
            };
            anyhow::Error::new(error).context(message)
        })?;
        ensure!(
            metadata.is_file(),
            "media source path must name a regular file"
        );
    }
    Ok(())
}
/// Whether `output` is a local file a forward records to (a path or a
/// `file:` URL) rather than a URL it streams to.
pub fn is_recording(output: &str) -> bool {
    matches!(scheme(output).as_deref(), None | Some("file"))
}
pub fn is_source(url: &str) -> bool {
    is_url_source(url) || std::path::Path::new(url).is_file()
}
/// Whether `url` names a source by its scheme alone, without asking the
/// disk whether it is a file.
pub fn is_url_source(url: &str) -> bool {
    scheme(url).is_some_and(|s| {
        matches!(
            s.as_str(),
            "rtsp"
                | "rtsps"
                | "srt"
                | "rtmp"
                | "rtmps"
                | "http"
                | "https"
                | "udp"
                | "tcp"
                | "rtp"
                | "file"
                | "avfoundation"
                | "v4l2"
                | "dshow"
        )
    })
}
pub fn info(url: &str) -> Result<CameraInfo> {
    validate_source(url)?;
    let safe_url = redact_url(url);
    let hash = safe_url.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    Ok(CameraInfo {
        id: format!("media:{hash:016x}"),
        transport: Transport::Media,
        vendor: "Media".into(),
        model: format!(
            "{} input",
            scheme(url).unwrap_or_else(|| "file".into()).to_uppercase()
        ),
        serial: format!("{hash:016x}"),
        address: Some(safe_url),
        profile: Default::default(),
    })
}
pub(crate) struct Executable {
    path: PathBuf,
    /// This process's own copy, removed when its last user drops it.
    directory: Option<PathBuf>,
    /// Probe results kept beside a shared copy, valid for its build and boot.
    probes: Option<PathBuf>,
    /// Held shared while this process may run the shared copy, so another
    /// build's cleanup leaves it in place.
    _in_use: Option<fs::File>,
}
impl Executable {
    fn external(path: PathBuf) -> Self {
        Self {
            path,
            directory: None,
            probes: None,
            _in_use: None,
        }
    }
}
impl Drop for Executable {
    fn drop(&mut self) {
        if let Some(dir) = &self.directory {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(dir);
        }
    }
}
const EMBEDDED_SHA256: &str = env!("CAPTUREFAB_FFMPEG_SHA256");
const FFMPEG_NAME: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};
#[derive(Default)]
struct Extracted {
    /// The copy in the user's cache, kept for the life of the process.
    shared: Option<Arc<Executable>>,
    /// A fallback copy in the temporary directory, removed once no decoder
    /// or encoder holds it.
    private: Weak<Executable>,
    /// The cache failed this process once, so it uses private copies.
    uncached: bool,
}
static EXECUTABLE: OnceLock<Mutex<Extracted>> = OnceLock::new();
#[cfg(test)]
thread_local! {
    /// A stand-in FFmpeg for the current test thread.
    static FAKE_FFMPEG: std::cell::RefCell<Option<Arc<Executable>>> =
        const { std::cell::RefCell::new(None) };
}
/// Makes `path` this test thread's FFmpeg, or, with `None`, stops.
#[cfg(test)]
pub(crate) fn set_fake_ffmpeg(path: Option<PathBuf>) {
    FAKE_FFMPEG.set(path.map(|path| Arc::new(Executable::external(path))));
}
pub(crate) fn executable() -> Result<Arc<Executable>> {
    #[cfg(test)]
    if let Some(fake) = FAKE_FFMPEG.with_borrow(Clone::clone) {
        return Ok(fake);
    }
    if let Some(path) = std::env::var_os("CAPTUREFAB_FFMPEG") {
        ensure!(
            !path.is_empty(),
            "CAPTUREFAB_FFMPEG must be an executable path"
        );
        return Ok(Arc::new(Executable::external(PathBuf::from(path))));
    }
    let mut extracted = EXECUTABLE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Another process can remove or replace the shared copy, so its owner,
    // mode and length are checked again before each use.
    if let Some(shared) = &extracted.shared
        && trusted(&shared.path, EMBEDDED.len()).is_ok()
    {
        return Ok(shared.clone());
    }
    if let Some(private) = extracted.private.upgrade() {
        return Ok(private);
    }
    if !extracted.uncached {
        let shared = crate::ipc::cache_dir()
            .context("no user cache directory")
            .and_then(|root| cached_copy(&root, EMBEDDED, EMBEDDED_SHA256))
            .and_then(|copy| {
                ensure!(
                    runs(&copy),
                    "the cache directory does not allow running programs"
                );
                Ok(copy)
            });
        match shared {
            Ok(copy) => return Ok(extracted.shared.insert(Arc::new(copy)).clone()),
            // A read-only, foreign or noexec cache directory still leaves
            // FFmpeg usable from the temporary directory.
            Err(_) => extracted.uncached = true,
        }
    }
    let copy = Arc::new(private_copy(EMBEDDED)?);
    extracted.private = Arc::downgrade(&copy);
    Ok(copy)
}
/// The system lets this user start `exe`: a home directory mounted noexec,
/// or an execution policy, can refuse what the temporary directory allows.
fn runs(exe: &Executable) -> bool {
    match command(exe)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        // Anything else, such as a copy another thread still holds open for
        // writing, is for the real launch to report.
        Err(error) => error.kind() != std::io::ErrorKind::PermissionDenied,
    }
}
/// The shared copy of `bytes` under `root`, named by its digest. It is
/// written once, under a lock, to a new file that is renamed into place;
/// an existing copy is checked against the digest before this process
/// first runs it.
fn cached_copy(root: &std::path::Path, bytes: &[u8], digest: &str) -> Result<Executable> {
    ensure!(digest.len() == 64, "bundled FFmpeg has no digest");
    private_dir(root)?;
    let dir = root.join(format!("ffmpeg-{}", &digest[..16]));
    let in_use = hold(&dir)?;
    let path = dir.join(FFMPEG_NAME);
    let intact = || {
        trusted(&path, bytes.len()).is_ok() && sha256_file(&path).ok().as_deref() == Some(digest)
    };
    if !intact() {
        let extracting = lock_file(&dir.join("extract.lock"))?;
        fs2::FileExt::lock_exclusive(&extracting)?;
        // Another process may have finished the copy while this one waited.
        if !intact() {
            let temporary = dir.join(format!("{FFMPEG_NAME}.{}.tmp", nonce()?));
            let written = write_new(&temporary, bytes, true).and_then(|()| {
                // Windows cannot rename over an existing file.
                #[cfg(windows)]
                let _ = fs::remove_file(&path);
                Ok(fs::rename(&temporary, &path)?)
            });
            if let Err(error) = written {
                let _ = fs::remove_file(&temporary);
                return Err(error.context("cannot extract bundled FFmpeg"));
            }
            // A file system that ignores modes would fail every later check.
            trusted(&path, bytes.len())?;
        }
    }
    prune(root, &dir);
    Ok(Executable {
        path,
        directory: None,
        probes: Some(dir.join("probes.json")),
        _in_use: Some(in_use),
    })
}
/// Lock the copy in `dir` shared. Another build's cleanup may move the
/// directory away while this process waits for the lock, so the lock must
/// still be the one in `dir` once it is held.
fn hold(dir: &std::path::Path) -> Result<fs::File> {
    let path = dir.join("in-use.lock");
    for _ in 0..3 {
        private_dir(dir)?;
        let lock = lock_file(&path)?;
        fs2::FileExt::lock_shared(&lock)?;
        if same_file(&lock, &path) {
            return Ok(lock);
        }
    }
    bail!("the FFmpeg cache directory keeps changing")
}
#[cfg(unix)]
fn same_file(file: &fs::File, path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(open), Ok(named)) => (open.dev(), open.ino()) == (named.dev(), named.ino()),
        _ => false,
    }
}
/// Windows does not move a directory while a file in it is open.
#[cfg(not(unix))]
fn same_file(_: &fs::File, path: &std::path::Path) -> bool {
    path.exists()
}
/// Remove copies left by other builds that no process is using. A copy in
/// use, or one Windows will not delete while it runs, stays.
fn prune(root: &std::path::Path, keep: &std::path::Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if dir == keep || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        // Left by a cleanup that was cut short.
        if name.starts_with("removed-") {
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        if !name.starts_with("ffmpeg-") {
            continue;
        }
        let Ok(lock) = lock_file(&dir.join("in-use.lock")) else {
            continue;
        };
        if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
            discard(root, &dir, lock);
        }
    }
}
/// Delete an unused copy. It is moved aside while still locked, so a process
/// of that build starting now finds no copy rather than one about to go.
#[cfg(unix)]
fn discard(root: &std::path::Path, dir: &std::path::Path, _lock: fs::File) {
    let Ok(nonce) = nonce() else {
        return;
    };
    let removed = root.join(format!("removed-{nonce}"));
    if fs::rename(dir, &removed).is_ok() {
        let _ = fs::remove_dir_all(&removed);
    }
}
/// Windows moves no directory with an open file in it, and deletes no copy
/// that is running.
#[cfg(not(unix))]
fn discard(_: &std::path::Path, dir: &std::path::Path, lock: fs::File) {
    drop(lock);
    let _ = fs::remove_dir_all(dir);
}
/// A fallback copy private to this process, in a new temporary directory.
fn private_copy(bytes: &[u8]) -> Result<Executable> {
    let directory = std::env::temp_dir().join(format!("capturefab-ffmpeg-{}", nonce()?));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&directory)
        .context("cannot create private FFmpeg directory")?;
    let path = directory.join(FFMPEG_NAME);
    if let Err(error) = write_new(&path, bytes, true) {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&directory);
        return Err(error.context("cannot extract bundled FFmpeg"));
    }
    Ok(Executable {
        path,
        directory: Some(directory),
        probes: None,
        _in_use: None,
    })
}
/// A private file that did not exist before. It is not synced: a copy cut
/// short by a crash fails validation and is written again.
fn write_new(path: &std::path::Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    options.open(path)?.write_all(bytes)?;
    Ok(())
}
fn nonce() -> Result<String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|e| anyhow!("cannot generate FFmpeg extraction nonce: {e}"))?;
    Ok(nonce.iter().map(|b| format!("{b:02x}")).collect())
}
/// Create `dir` for this user alone, or accept it when it already is.
fn private_dir(dir: &std::path::Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    ensure!(meta.is_dir(), "{} is not a directory", dir.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            meta.uid() == crate::ipc::effective_uid(),
            "{} belongs to another user",
            dir.display()
        );
        if meta.mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}
/// A regular file of the expected length that only this user can change.
fn trusted(path: &std::path::Path, length: usize) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.len() == length as u64,
        "extracted FFmpeg is missing or incomplete"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.uid() == crate::ipc::effective_uid() && meta.mode() & 0o022 == 0,
            "extracted FFmpeg is writable by another user"
        );
    }
    Ok(())
}
fn lock_file(path: &std::path::Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::Digest;
    let mut file = fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    let mut bytes = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
/// One of this user's live decoders or software encoders on the host,
/// counted through lock files in the cache that holders keep locked; a
/// process that dies releases its slot.
struct Active {
    _slot: Option<fs::File>,
}
impl Active {
    const NONE: Self = Self { _slot: None };
    const SLOTS: usize = 32;
    /// Claim a slot of `kind`, and count the active ones, this one included.
    fn claim(kind: &str) -> (Self, usize) {
        let Some(dir) = crate::ipc::cache_dir()
            .map(|root| root.join("active"))
            .filter(|dir| private_dir(dir).is_ok())
        else {
            return (Self::NONE, 1);
        };
        let mut mine = None;
        let mut others = 0;
        for slot in 0..Self::SLOTS {
            let Ok(file) = lock_file(&dir.join(format!("{kind}-{slot}.lock"))) else {
                break;
            };
            if fs2::FileExt::try_lock_exclusive(&file).is_err() {
                others += 1;
            } else if mine.is_none() {
                mine = Some(file);
            }
        }
        (Self { _slot: mine }, others + 1)
    }
}
fn cores() -> usize {
    crate::resources::processing_threads(
        thread::available_parallelism().map_or(1, |cores| cores.get()),
    )
}
/// FFmpeg's default frame threading runs cores + 1 threads per stream, each
/// holding frames and adding latency; share the cores between live streams.
fn decoder_threads(cores: usize, streams: usize, efficient: bool) -> usize {
    if efficient {
        1
    } else {
        (cores / streams.max(1)).clamp(1, 4)
    }
}
/// x264's preset for a recording: faster as the pixel rate per available
/// core rises, and one step faster in Efficient mode.
fn x264_preset(width: u32, height: u32, fps: f64, cores: usize, efficient: bool) -> &'static str {
    const PRESETS: [&str; 3] = ["veryfast", "superfast", "ultrafast"];
    let per_core = width as f64 * height as f64 * fps / cores.max(1) as f64;
    let step = if per_core <= 12e6 {
        0
    } else if per_core <= 24e6 {
        1
    } else {
        2
    };
    PRESETS[(step + efficient as usize).min(2)]
}
/// The configured decoding acceleration. `auto` becomes `none` where it
/// would find nothing, since trying costs every decoder launch a device set-up.
fn decode_acceleration(exe: &Executable) -> Result<String> {
    let acceleration = hwaccel();
    validate_hwaccel(&acceleration)?;
    Ok(match acceleration.as_str() {
        "off" => "none".into(),
        "auto" if !hwaccel_available(exe) => "none".into(),
        _ => acceleration,
    })
}
/// ARM Linux boards have nothing the bundled FFmpeg decodes with; other
/// hosts ask once per boot whether `-hwaccel auto` initializes a device.
fn hwaccel_available(exe: &Executable) -> bool {
    if cfg!(all(
        target_os = "linux",
        any(target_arch = "aarch64", target_arch = "arm")
    )) {
        return false;
    }
    let found = probed(
        exe,
        |probes| probes.hwaccel,
        |probes, found| probes.hwaccel = Some(found),
        || {
            // One black 64x64 baseline H.264 frame.
            const CLIP: [u8; 65] = [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x04, 0x26, 0xc0, 0x44, 0x00,
                0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0x08, 0x3c, 0x48, 0x99, 0x20, 0x00,
                0x00, 0x00, 0x01, 0x68, 0xcb, 0x83, 0xcb, 0x20, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84,
                0x05, 0x7f, 0xff, 0xff, 0x0f, 0x45, 0x00, 0x01, 0x42, 0xdf, 0x27, 0x27, 0x27, 0x5d,
                0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x80,
            ];
            let args = [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "verbose",
                "-hwaccel",
                "auto",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-f",
                "null",
                "-",
            ]
            .map(str::to_string);
            let (_, log, timed_out) = timed_output(exe, &args, &CLIP, Duration::from_secs(5))?;
            Ok((!timed_out).then(|| log.contains("Using auto hwaccel type")))
        },
    );
    // An FFmpeg that cannot run or finish the probe keeps `auto`.
    found.ok().flatten().unwrap_or(true)
}
/// Context for a decoder whose FFmpeg could not be extracted or started,
/// unlike a source FFmpeg reached and failed on. Callers test for it with
/// `error.downcast_ref::<FfmpegUnavailable>()`, never by message text:
/// explained source failures quote FFmpeg's log too.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FfmpegUnavailable;
impl std::fmt::Display for FfmpegUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot start FFmpeg decoder")
    }
}
pub(crate) fn command(exe: &Executable) -> Command {
    let mut cmd = Command::new(&exe.path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.stdin(Stdio::null());
    // Camera hub secrets may live in this process's environment;
    // FFmpeg never needs them.
    for (name, _) in std::env::vars_os() {
        if is_hub_secret_variable(&name) {
            cmd.env_remove(name);
        }
    }
    cmd
}
/// Environment variables that hold camera hub secrets start with this.
pub(crate) const HUB_SECRET_PREFIX: &str = "CAPTUREFAB_HUB_";
fn is_hub_secret_variable(name: &std::ffi::OsStr) -> bool {
    // Windows environment names are case-insensitive, including when the
    // inherited spelling differs from the saved uppercase variable name.
    name.to_str().is_some_and(|name| {
        [HUB_SECRET_PREFIX].iter().any(|expected| {
            name.get(..expected.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(expected))
        })
    })
}
fn bounded_output(exe: &Executable, args: &[String], timeout: Duration) -> Result<(bool, String)> {
    let (success, output, _) = timed_output(exe, args, &[], timeout)?;
    Ok((success, output))
}
/// Run FFmpeg with a deadline, feeding it `input` when there is any.
fn timed_output(
    exe: &Executable,
    args: &[String],
    input: &[u8],
    timeout: Duration,
) -> Result<(bool, String, bool)> {
    let mut command = command(exe);
    if !input.is_empty() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot launch FFmpeg")?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let reader = |mut input: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = input.by_ref().take(1024 * 1024).read_to_end(&mut bytes);
            bytes
        })
    };
    let out = reader(Box::new(stdout));
    let err = reader(Box::new(stderr));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let success = loop {
        if let Some(status) = child.try_wait()? {
            break status.success();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut bytes = out.join().unwrap_or_default();
    bytes.extend(err.join().unwrap_or_default());
    Ok((
        success,
        String::from_utf8_lossy(&bytes).to_string(),
        timed_out,
    ))
}
/// Which FFmpeg this process would run, and whether it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FfmpegHealth {
    /// "bundled", or "CAPTUREFAB_FFMPEG" for an override.
    pub source: &'static str,
    pub path: String,
    /// FFmpeg's first `-version` line, such as `ffmpeg version 7.1 ...`.
    pub version: String,
}
/// Run FFmpeg's `-version` with a 3 s deadline, for `doctor`. Extracting
/// the bundled copy goes to the cache directory, as for any first use.
pub(crate) fn ffmpeg_health() -> Result<FfmpegHealth> {
    let source = if std::env::var_os("CAPTUREFAB_FFMPEG").is_some() {
        "CAPTUREFAB_FFMPEG"
    } else {
        "bundled"
    };
    let exe = executable()?;
    let (ok, output) = bounded_output(&exe, &["-version".into()], Duration::from_secs(3))
        .with_context(|| format!("cannot run {}", exe.path.display()))?;
    let version = output.lines().next().unwrap_or_default().trim().to_owned();
    ensure!(
        ok && version.to_ascii_lowercase().starts_with("ffmpeg version"),
        "{} -version failed{}",
        exe.path.display(),
        if version.is_empty() {
            String::new()
        } else {
            format!(": {version}")
        }
    );
    Ok(FfmpegHealth {
        source,
        path: exe.path.display().to_string(),
        version,
    })
}
pub fn ffmpeg_info() -> Result<Value> {
    let exe = executable()?;
    let (ok, version) = bounded_output(&exe, &["-version".into()], Duration::from_secs(5))?;
    ensure!(ok, "FFmpeg version query failed");
    let (_, protocols) = bounded_output(
        &exe,
        &["-hide_banner".into(), "-protocols".into()],
        Duration::from_secs(5),
    )?;
    let (_, encoders) = bounded_output(
        &exe,
        &["-hide_banner".into(), "-encoders".into()],
        Duration::from_secs(5),
    )?;
    Ok(
        json!({"embedded":!EMBEDDED.is_empty(),"embedded_bytes":EMBEDDED.len(),"version":version,"protocols":protocols,"encoders":encoders,"license_notice":LICENSE}),
    )
}
/// How long a failed encoder listing is remembered before FFmpeg runs again.
const ENCODER_RETRY: Duration = Duration::from_secs(60);
/// What `encoder_names` learned from FFmpeg so far in this process.
enum EncoderListing {
    Unknown,
    Listed(Arc<Vec<String>>),
    Failed { at: Instant, error: String },
}
static ENCODER_NAMES: Mutex<EncoderListing> = Mutex::new(EncoderListing::Unknown);

/// Names of the encoders in this FFmpeg build, such as `libx264` and
/// `h264_videotoolbox`. FFmpeg runs once per process, with only `-encoders`;
/// a failure is remembered for a minute, so callers that refresh often do
/// not relaunch (or re-extract) a missing or broken FFmpeg each time.
pub fn encoder_names() -> Result<Arc<Vec<String>>> {
    listed_encoders(&ENCODER_NAMES, Instant::now(), || {
        let exe = executable()?;
        // Share the process/boot cache used by recording encoder probes.
        let listing = encoder_listing(&exe)?;
        let names = parse_encoder_names(&listing);
        ensure!(!names.is_empty(), "FFmpeg listed no encoders");
        Ok(names)
    })
}

fn listed_encoders(
    cache: &Mutex<EncoderListing>,
    now: Instant,
    list: impl FnOnce() -> Result<Vec<String>>,
) -> Result<Arc<Vec<String>>> {
    // Held while FFmpeg runs, so concurrent callers share one run.
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    match &*cache {
        EncoderListing::Listed(names) => return Ok(names.clone()),
        EncoderListing::Failed { at, error }
            if now.saturating_duration_since(*at) < ENCODER_RETRY =>
        {
            bail!("{error}")
        }
        _ => {}
    }
    match list() {
        Ok(names) => {
            let names = Arc::new(names);
            *cache = EncoderListing::Listed(names.clone());
            Ok(names)
        }
        Err(error) => {
            *cache = EncoderListing::Failed {
                at: now,
                error: format!("{error:#}"),
            };
            Err(error)
        }
    }
}

/// Encoder names from `ffmpeg -encoders` output: the second column of each
/// row after the `------` line that ends the flag legend. Rows start with
/// six or more capital-letter-or-dot flags, which skips stderr noise.
fn parse_encoder_names(listing: &str) -> Vec<String> {
    let separator = |line: &str| {
        let line = line.trim();
        !line.is_empty() && line.bytes().all(|b| b == b'-')
    };
    listing
        .lines()
        .skip_while(|line| !separator(line))
        .skip(1)
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let (flags, name) = (columns.next()?, columns.next()?);
            (flags.len() >= 6
                && flags.bytes().all(|b| b == b'.' || b.is_ascii_uppercase())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
            .then(|| name.to_owned())
        })
        .collect()
}
pub fn native_devices() -> Result<Value> {
    native_devices_with_timeout(Duration::from_secs(8))
}
pub fn native_devices_with_timeout(timeout: Duration) -> Result<Value> {
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let _ = timeout;
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + timeout;
        let devices = v4l2_devices(std::path::Path::new("/sys/class/video4linux"), |path| {
            captures_by(deadline, path, v4l2::captures_video)
        });
        return Ok(json!({"platform":"linux","devices":devices}));
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let exe = executable()?;
        let backend = if cfg!(target_os = "macos") {
            "avfoundation"
        } else {
            "dshow"
        };
        let args = [
            "-hide_banner",
            "-f",
            backend,
            "-list_devices",
            "true",
            "-i",
            if cfg!(target_os = "macos") {
                ""
            } else {
                "dummy"
            },
        ]
        .map(str::to_string);
        let (_, listing, timed_out) = timed_output(&exe, &args, &[], timeout)?;
        ensure!(
            !timed_out,
            "native camera discovery timed out after {} s; check camera permissions and host drivers, or retry with a larger --timeout-ms",
            timeout.as_secs_f64()
        );
        ensure!(
            !listing.contains("Unknown input format"),
            "FFmpeg does not support the {backend} native camera input; use a build containing this host camera backend"
        );
        let devices = if backend == "avfoundation" {
            avfoundation_devices(&listing)
        } else {
            dshow_devices(&listing)
        };
        return Ok(json!({"platform":std::env::consts::OS,"devices":devices,"diagnostic":listing}));
    }
    #[allow(unreachable_code)]
    Err(anyhow!(
        "native camera enumeration is supported on macOS, Linux and Windows"
    ))
}
/// The video capture nodes in the `video4linux` sysfs class, in node order.
/// The class also holds UVC metadata, codec, ISP, subdevice, VBI and radio
/// nodes, which share the camera's name but cannot capture its video.
#[cfg(any(test, target_os = "linux"))]
fn v4l2_devices(
    class: &std::path::Path,
    captures: impl Fn(&std::path::Path) -> bool,
) -> Vec<Value> {
    let mut nodes = fs::read_dir(class)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|node| {
            node.starts_with("video") && captures(std::path::Path::new(&format!("/dev/{node}")))
        })
        .collect::<Vec<_>>();
    nodes.sort_by_key(|node| (node.len(), node.clone()));
    nodes
        .into_iter()
        .map(|node| {
            let name = fs::read_to_string(class.join(&node).join("name")).unwrap_or_default();
            json!({"id":format!("v4l2:/dev/{node}"),"name":name.trim(),"backend":"v4l2"})
        })
        .collect()
}
/// Whether `captures` accepts the node at `path`, or `true` when it has not
/// answered by `deadline`: opening a node can wait on a USB camera resuming
/// from autosuspend.
#[cfg(any(test, target_os = "linux"))]
fn captures_by(
    deadline: Instant,
    path: &std::path::Path,
    captures: fn(&std::path::Path) -> bool,
) -> bool {
    let (sender, receiver) = std::sync::mpsc::channel();
    let path = path.to_owned();
    thread::spawn(move || sender.send(captures(&path)));
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(true)
}
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn avfoundation_devices(listing: &str) -> Vec<Value> {
    let mut video = true;
    let mut found = Vec::new();
    for line in listing.lines() {
        if line.contains("audio devices:") {
            video = false;
        } else if line.contains("video devices:") {
            video = true;
        } else if video
            && let Some(device) = line.match_indices('[').find_map(|(start, _)| {
                let (index, name) = line[start + 1..].split_once("] ")?;
                index
                    .parse::<u32>()
                    .is_ok()
                    .then_some((index, name.trim_end()))
            })
        {
            found.push(device);
        }
    }
    let selectable = |name: &str| {
        !name.is_empty()
            && !name.contains([':', '?'])
            && !name.starts_with(|c: char| {
                c.is_ascii_digit() || c.is_whitespace() || matches!(c, '+' | '-')
            })
            && !["none", "default"]
                .iter()
                .any(|word| name.starts_with(word))
            && found
                .iter()
                .filter(|(_, other)| other.starts_with(name))
                .count()
                == 1
    };
    found
        .iter()
        .map(|&(index, name)| {
            let id = if selectable(name) { name } else { index };
            json!({"id":format!("avfoundation:{id}"),"name":name,"backend":"avfoundation"})
        })
        .collect()
}

/// The video input devices in FFmpeg's DirectShow listing. FFmpeg lists the
/// video category before the audio one, each device as its friendly name,
/// the media types of its capture pins ("(none)" when it could not read
/// them, as for some virtual cameras) and its moniker ("Alternative name").
/// The moniker selects a device whose friendly name another device shares
/// or FFmpeg cannot parse; its `?` is written `%3F`, since `?` starts the
/// locator's options.
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn dshow_devices(listing: &str) -> Vec<Value> {
    const AUDIO_CATEGORY: &str = "{33D9A762-90C8-11D0-BD43-00A0C911CE86}";
    let mut found: Vec<(&str, bool, Option<&str>)> = Vec::new();
    let (mut kept, mut audio) = (false, false);
    for line in listing.lines() {
        let text = match line.strip_prefix('[') {
            Some(rest) => rest.split_once("] ").map_or(line, |(_, text)| text),
            None => line,
        };
        if let Some(moniker) = text
            .trim_start()
            .strip_prefix("Alternative name \"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            if kept && let Some(device) = found.last_mut() {
                device.2 = Some(moniker);
            }
        } else if let Some((name, types)) = text
            .strip_prefix('"')
            .and_then(|rest| rest.rsplit_once('"'))
        {
            let types = types.trim().trim_start_matches('(').trim_end_matches(')');
            let none = types == "none";
            kept = types.split(", ").any(|kind| kind == "video") || none && !audio;
            audio |= types == "audio";
            if kept {
                found.push((name, none, None));
            }
        }
    }
    found.retain(|&(_, none, moniker)| {
        !(none
            && moniker.is_some_and(|moniker| moniker.to_ascii_uppercase().contains(AUDIO_CATEGORY)))
    });
    found
        .iter()
        .map(|&(name, _, moniker)| {
            let unique = found.iter().filter(|(other, ..)| *other == name).count() == 1
                && !name.contains([':', '?'])
                && !name.starts_with("@device");
            let id = match moniker {
                Some(moniker) if !unique => moniker.replace('?', "%3F"),
                _ => name.to_owned(),
            };
            json!({"id":format!("dshow:video={id}"),"name":name,"backend":"dshow"})
        })
        .collect()
}

struct Log {
    bytes: Mutex<VecDeque<u8>>,
}
impl Log {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(VecDeque::new()),
        })
    }
    fn text(&self, source: &str) -> String {
        let bytes = self
            .bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect::<Vec<_>>();
        redact_log(&String::from_utf8_lossy(&bytes), source)
    }
    fn collect(self: &Arc<Self>, mut input: impl Read + Send + 'static) -> JoinHandle<()> {
        let log = self.clone();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut stored = log.bytes.lock().unwrap_or_else(|e| e.into_inner());
                stored.extend(&buffer[..count]);
                let excess = stored.len().saturating_sub(MAX_LOG);
                stored.drain(..excess);
            }
        })
    }
}
struct Decoded {
    frames: VecDeque<Frame>,
    error: Option<String>,
}
/// The decoder has exited: every later read fails the same way until the
/// source is opened again. The engine counts it as a transport error even when
/// FFmpeg's reason mentions a timeout.
#[derive(Debug)]
pub(crate) struct StreamEnded(pub String);
impl std::fmt::Display for StreamEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for StreamEnded {}
struct DecoderQueue {
    state: Mutex<Decoded>,
    changed: Condvar,
    stopped: AtomicBool,
}
struct Decoder {
    child: Child,
    thread: Option<JoinHandle<()>>,
    log_thread: Option<JoinHandle<()>>,
    queue: Arc<DecoderQueue>,
    log: Arc<Log>,
    _exe: Arc<Executable>,
    source: String,
    /// Why FFmpeg's output ended, formatted once with its log.
    failure: Option<String>,
    status: Option<std::process::ExitStatus>,
    /// This stream's share of the host's live decoders.
    _active: Active,
}
fn ppm_token(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut token = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        ensure!(!available.is_empty(), "media stream ended");
        let byte = available[0];
        reader.consume(1);
        if token.is_empty() && byte == b'#' {
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line)?;
            ensure!(line.len() <= 4096, "PPM comment is too long");
            continue;
        }
        if byte.is_ascii_whitespace() {
            if token.is_empty() { continue } else { break }
        }
        token.push(byte);
        ensure!(token.len() <= 32, "PPM header token is too long");
    }
    Ok(token)
}
fn read_ppm(reader: &mut impl BufRead, id: u64, timestamp_ns: u64) -> Result<Frame> {
    ensure!(
        ppm_token(reader)? == b"P6",
        "FFmpeg emitted an unsupported image format"
    );
    let integer = |bytes: Vec<u8>| -> Result<u32> {
        std::str::from_utf8(&bytes)?
            .parse()
            .context("invalid PPM dimensions")
    };
    let width = integer(ppm_token(reader)?)?;
    let height = integer(ppm_token(reader)?)?;
    ensure!(
        integer(ppm_token(reader)?)? == 255,
        "unsupported PPM sample depth"
    );
    let size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .context("media frame dimension overflow")?;
    ensure!(
        width > 0 && height > 0 && size <= MAX_FRAME,
        "media frame exceeds 256 MiB limit"
    );
    let mut data = vec![0; size];
    reader
        .read_exact(&mut data)
        .context("truncated FFmpeg RGB frame")?;
    Ok(Frame {
        id,
        width,
        height,
        pixel_format: RGB8,
        timestamp_ns,
        data,
    })
}
impl Decoder {
    #[cfg(test)]
    fn spawn(source: &str, timeout: Duration) -> Result<Self> {
        Self::spawn_configured(source, timeout, &[])
    }
    fn spawn_configured(source: &str, timeout: Duration, options: &[String]) -> Result<Self> {
        Self::spawn_with_output(source, timeout, options, &[])
    }
    fn spawn_with_output(
        source: &str,
        timeout: Duration,
        options: &[String],
        output_options: &[String],
    ) -> Result<Self> {
        validate_source(source)?;
        ensure!(
            !timeout.is_zero() && Instant::now().checked_add(timeout).is_some(),
            "media timeout must be positive and fit in the monotonic clock"
        );
        let exe = executable().context(FfmpegUnavailable)?;
        let mut cmd = command(&exe);
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
        let configured = |name: &str| options.iter().any(|option| option == name);
        if !configured("-hwaccel") {
            let acceleration = decode_acceleration(&exe)?;
            if acceleration != "none" {
                cmd.args(["-hwaccel", &acceleration]);
            }
        }
        let active = if configured("-threads") {
            Active::NONE
        } else {
            let (active, streams) = Active::claim("decode");
            let threads = decoder_threads(cores(), streams, crate::efficiency::efficient());
            cmd.args(["-threads", &threads.to_string()]);
            active
        };
        let input = match native_input(source)? {
            Some((format, input)) => {
                cmd.args(["-f", format]);
                input
            }
            None if scheme(source).as_deref() == Some("file") => {
                format!("file:{}", local_file_path(source)?.to_string_lossy())
            }
            None => canonical_scheme(source),
        };
        cmd.args(options);
        if matches!(scheme(source).as_deref(), Some("rtsp" | "rtsps")) {
            // RTP interleaved over TCP first. A camera that answers SETUP with
            // 461 Unsupported Transport gets RTP over UDP, the transport ONVIF
            // requires of every camera; FFmpeg keeps rtsps on TCP.
            cmd.args([
                "-rtsp_flags",
                "prefer_tcp",
                "-rtsp_transport",
                "tcp+udp",
                "-timeout",
                &timeout.as_micros().max(1).to_string(),
            ]);
        } else if scheme(source)
            .is_some_and(|s| !matches!(s.as_str(), "file" | "avfoundation" | "v4l2" | "dshow"))
        {
            cmd.args(["-rw_timeout", &timeout.as_micros().max(1).to_string()]);
        }
        cmd.args(["-i", &input])
            .args(output_options)
            .args([
                "-map",
                "0:v:0",
                "-an",
                "-sn",
                "-dn",
                "-fps_mode",
                "passthrough",
                "-f",
                "image2pipe",
                "-c:v",
                "ppm",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context(FfmpegUnavailable)?;
        let stdout = child.stdout.take().unwrap();
        let log = Log::new();
        let log_thread = log.collect(child.stderr.take().unwrap());
        let queue = Arc::new(DecoderQueue {
            state: Mutex::new(Decoded {
                frames: VecDeque::new(),
                error: None,
            }),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
        });
        let shared = queue.clone();
        let epoch = Instant::now();
        let live = scheme(source).is_some_and(|s| s != "file");
        let thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut id = 0;
            while !shared.stopped.load(Ordering::Relaxed) {
                id += 1;
                // Stamp a frame when it has arrived, not when its wait began.
                let result = read_ppm(&mut reader, id, 0).map(|mut frame| {
                    frame.timestamp_ns = epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    frame
                });
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(frame) => {
                        if live {
                            while state.frames.len() >= 2 {
                                state.frames.pop_front();
                            }
                        } else {
                            while state.frames.len() >= 2 && !shared.stopped.load(Ordering::Relaxed)
                            {
                                state = shared
                                    .changed
                                    .wait(state)
                                    .unwrap_or_else(|e| e.into_inner());
                            }
                        }
                        if shared.stopped.load(Ordering::Relaxed) {
                            break;
                        }
                        state.frames.push_back(frame);
                    }
                    Err(e) => {
                        state.error = Some(format!("{e:#}"));
                        shared.changed.notify_all();
                        break;
                    }
                }
                shared.changed.notify_all();
            }
        });
        Ok(Self {
            child,
            thread: Some(thread),
            log_thread: Some(log_thread),
            queue,
            log,
            _exe: exe,
            source: source.into(),
            failure: None,
            status: None,
            _active: active,
        })
    }
    fn next(&mut self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now() + timeout;
        let queue = self.queue.clone();
        let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(frame) = state.frames.pop_front() {
                queue.changed.notify_all();
                return Ok(frame);
            }
            if let Some(error) = state.error.clone() {
                drop(state);
                return Err(StreamEnded(self.fail(&error).to_owned()).into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "media frame capture timed out: {}",
                redact_url(&self.source)
            );
            state = queue
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    /// Why the stream ended, formatted the first time it is asked for. FFmpeg
    /// gets a moment to exit first, so the log it leaves is complete.
    fn fail(&mut self, error: &str) -> &str {
        if self.failure.is_none() {
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                self.status = self.child.try_wait().ok().flatten();
                if self.status.is_some() || Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            if self.status.is_some()
                && let Some(thread) = self.log_thread.take()
            {
                let _ = thread.join();
            }
            let (error, log) = (redact_log(error, &self.source), self.log.text(&self.source));
            self.failure = Some(match log.trim() {
                "" => error,
                log => format!("{error}: {log}"),
            });
        }
        self.failure.as_deref().unwrap_or_default()
    }
    /// FFmpeg's output has ended, as the last frame error reported.
    fn ended(&self) -> bool {
        self.failure.is_some()
    }
    /// FFmpeg exited successfully, for example at the end of a file.
    fn succeeded(&mut self) -> bool {
        if self.status.is_none() {
            self.status = self.child.try_wait().ok().flatten();
        }
        self.status.is_some_and(|status| status.success())
    }
    fn stop(&mut self) {
        self.queue.stopped.store(true, Ordering::Relaxed);
        self.queue.changed.notify_all();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.log_thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Finite local decoding used by the virtual camera. This shares the media
/// decoder's bounded queue and process lifetime, but distinguishes a clean end
/// of a clip from a damaged input so the simulator can advance its playlist.
pub(crate) struct ReplayDecoder(Decoder);
impl ReplayDecoder {
    pub(crate) fn open(
        path: &std::path::Path,
        width: u32,
        height: u32,
        fps: f64,
        image: bool,
        timeout: Duration,
    ) -> Result<Self> {
        let mut options = vec![
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-xerror".into(),
            "-hwaccel".into(),
            "none".into(),
            "-threads".into(),
            "1".into(),
        ];
        if image {
            // Literal still names must not be interpreted as image2 patterns.
            options.extend([
                "-f".into(),
                "image2".into(),
                "-pattern_type".into(),
                "none".into(),
            ]);
        } else {
            // Do not autodetect a renamed playlist or manifest as local video.
            options.extend(["-f".into(), Self::video_format(path)?.into()]);
        }
        let mut output = Self::output(width, height, (!image).then_some(fps));
        if image {
            output.extend(["-frames:v".into(), "1".into()]);
        }
        Self::spawn(path, timeout, &options, &output)
    }
    fn video_format(path: &std::path::Path) -> Result<&'static str> {
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        Ok(match extension.as_str() {
            "mp4" | "m4v" | "mov" => "mov",
            "mkv" | "webm" => "matroska",
            "ts" | "mts" | "m2ts" => "mpegts",
            "mpg" | "mpeg" | "vob" => "mpeg",
            "avi" => "avi",
            "flv" => "flv",
            "wmv" => "asf",
            "ogv" => "ogg",
            _ => bail!("unsupported simulation video extension: {extension}"),
        })
    }
    pub(crate) fn sequence(
        manifest: &std::path::Path,
        width: u32,
        height: u32,
        fps: f64,
        count: usize,
        timeout: Duration,
    ) -> Result<Self> {
        let options = [
            "-protocol_whitelist",
            "file,pipe",
            "-xerror",
            "-hwaccel",
            "none",
            "-threads",
            "1",
            "-f",
            "concat",
            "-safe",
            "0",
            "-reinit_filter",
            "0",
        ]
        .map(str::to_string);
        let mut output = Self::output(width, height, Some(fps));
        output.extend(["-frames:v".into(), count.to_string()]);
        Self::spawn(manifest, timeout, &options, &output)
    }
    fn output(width: u32, height: u32, fps: Option<f64>) -> Vec<String> {
        let mut filter = format!(
            "scale={width}:{height}:force_original_aspect_ratio=decrease:eval=frame,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:eval=frame"
        );
        if let Some(fps) = fps {
            filter.push_str(&format!(",fps={fps}"));
        }
        vec!["-vf".into(), filter, "-filter_threads".into(), "1".into()]
    }
    fn spawn(
        path: &std::path::Path,
        timeout: Duration,
        options: &[String],
        output: &[String],
    ) -> Result<Self> {
        let source = path
            .to_str()
            .context("simulation source path is not valid UTF-8")?;
        Decoder::spawn_with_output(source, timeout, options, output).map(Self)
    }
    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("simulation frame timeout exceeds the monotonic clock")?;
        loop {
            let mut state = self.0.queue.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(frame) = state.frames.pop_front() {
                self.0.queue.changed.notify_all();
                return Ok(Some(frame));
            }
            if let Some(error) = &state.error {
                let error = error.clone();
                drop(state);
                if error == "media stream ended" {
                    if let Some(status) = self.0.child.try_wait()? {
                        if status.success() {
                            return Ok(None);
                        }
                        bail!(
                            "cannot decode simulation media {}: {}",
                            self.0.source,
                            self.0.log.text(&self.0.source).trim()
                        );
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "simulation media decoder did not exit before the frame timeout"
                    );
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                bail!(
                    "cannot decode simulation media {}: {}: {}",
                    self.0.source,
                    error,
                    self.0.log.text(&self.0.source).trim()
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "simulation media frame timed out: {}",
                self.0.source
            );
            let _ = self
                .0
                .queue
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}
fn native_input(source: &str) -> Result<Option<(&'static str, String)>> {
    Ok(Some(
        if let Some(device) = source.strip_prefix("avfoundation:") {
            ensure!(
                cfg!(target_os = "macos"),
                "AVFoundation capture is available on macOS"
            );
            (
                "avfoundation",
                format!("{}:none", device.split(':').next().unwrap_or(device)),
            )
        } else if let Some(path) = source.strip_prefix("v4l2:") {
            ensure!(
                cfg!(target_os = "linux"),
                "V4L2 capture is available on Linux"
            );
            ensure!(
                v4l2_path(path),
                "V4L2 URI must point to /dev/videoN or a /dev/v4l/by-id or /dev/v4l/by-path link"
            );
            ("v4l2", path.into())
        } else if let Some(name) = source.strip_prefix("dshow:") {
            ensure!(
                cfg!(target_os = "windows"),
                "DirectShow capture is available on Windows"
            );
            ensure!(
                name.starts_with("video="),
                "DirectShow URI must begin dshow:video="
            );
            ("dshow", dshow_input(name))
        } else {
            return Ok(None);
        },
    ))
}
/// FFmpeg's DirectShow input for a `dshow:` locator, with the `?` restored
/// in a moniker as `dshow_devices` lists it.
fn dshow_input(name: &str) -> String {
    if name.starts_with("video=@device") {
        name.replace("%3F", "?")
    } else {
        name.into()
    }
}
/// A V4L2 device node, or a persistent link udev makes to one, which stays
/// bound to the same camera when node numbers change across reboots and
/// replugs.
fn v4l2_path(path: &str) -> bool {
    path.starts_with("/dev/video")
        || ["/dev/v4l/by-id/", "/dev/v4l/by-path/"]
            .iter()
            .any(|links| {
                path.strip_prefix(links)
                    .is_some_and(|name| !matches!(name, "" | "." | "..") && !name.contains('/'))
            })
}
fn device_name(source: &str) -> &str {
    let name = source.split_once(':').map_or(source, |(_, name)| name);
    let name = name.strip_prefix("video=").unwrap_or(name);
    if name.parse::<u32>().is_ok() {
        source
    } else {
        name
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mode {
    width: u32,
    height: u32,
    fps: Option<(f64, f64)>,
    input_format: Option<usize>,
    /// The DirectShow frame intervals, in 100 ns, to request at the top and
    /// bottom of `fps`: FFmpeg only opens a capability whose interval range
    /// holds the interval it derives from `-framerate`.
    intervals: Option<(u32, u32)>,
    /// The smallest size of a DirectShow capability that takes any size up
    /// to this mode's.
    min_size: Option<(u32, u32)>,
}
impl Mode {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn fits(&self, (width, height): (u32, u32)) -> bool {
        self.size() == (width, height)
            || self.min_size.is_some_and(|(low_width, low_height)| {
                (low_width..=self.width).contains(&width)
                    && (low_height..=self.height).contains(&height)
            })
    }
    fn honors(&self, fps: f64) -> bool {
        self.fps
            .is_none_or(|(low, high)| fps > low - 0.01 && fps < high + 0.01)
    }
    fn label(&self) -> String {
        match self.fps {
            Some((_, high)) => format!("{}x{}@{high}", self.width, self.height),
            None => format!("{}x{}", self.width, self.height),
        }
    }
    fn rank(&self) -> (bool, u64, u64, u64, u32) {
        let fps = self
            .fps
            .map_or(0, |(_, high)| (high * 100.0).round() as u64);
        let area = self.width as u64 * self.height as u64;
        let smooth = fps >= 2500;
        (smooth, if smooth { area } else { 0 }, fps, area, self.width)
    }
}
fn best(modes: &[Mode], keep: impl Fn(&Mode) -> bool) -> Option<usize> {
    modes
        .iter()
        .enumerate()
        .filter(|(_, mode)| keep(mode))
        .max_by_key(|(_, mode)| mode.rank())
        .map(|(index, _)| index)
}
fn rounded(fps: f64) -> f64 {
    (fps * 100.0).round() / 100.0
}
fn dimensions(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    let size = (width.parse().ok()?, height.parse().ok()?);
    ((16..=8192).contains(&size.0) && (16..=8192).contains(&size.1)).then_some(size)
}
fn rate(text: &str) -> Option<f64> {
    let fps = rounded(text.parse().ok()?);
    (fps.is_finite() && fps > 0.0).then_some(fps)
}
fn merge(mut modes: Vec<Mode>, mode: Mode) -> Vec<Mode> {
    match modes.iter_mut().find(|m| m.label() == mode.label()) {
        Some(m) => {
            m.fps = m
                .fps
                .zip(mode.fps)
                .map(|((a, high), (b, _))| (a.min(b), high));
            m.intervals = m
                .intervals
                .zip(mode.intervals)
                .map(|((a, c), (b, d))| (a.max(b), c.max(d)));
            m.min_size = m.min_size.or(mode.min_size);
        }
        None => modes.push(mode),
    }
    modes
}
/// The modes FFmpeg's AVFoundation input can open, from its listing of each
/// format's frame rate ranges. It makes the last format of the requested
/// size active and applies the range of any same-size format whose maximum
/// is the requested rate, which the active format accepts when one of its
/// own ranges holds it.
fn avfoundation_modes(log: &str) -> Vec<Mode> {
    type Format = ((u32, u32), Vec<(f64, f64)>);
    let mut formats: Vec<Format> = Vec::new();
    for (size, range) in log.lines().filter_map(|line| {
        let (head, tail) = line.trim_end().split_once("@[")?;
        let size = dimensions(head.rsplit(' ').next()?)?;
        let (low, high) = tail.strip_suffix("]fps")?.split_once(' ')?;
        Some((size, (rate(low)?, rate(high)?)))
    }) {
        // A format lists its ranges from the fastest down.
        match formats.last_mut() {
            Some((last, ranges))
                if *last == size && ranges.last().is_some_and(|r| r.1 > range.1) =>
            {
                ranges.push(range)
            }
            _ => formats.push((size, vec![range])),
        }
    }
    let mut modes: Vec<Mode> = Vec::new();
    for (index, (size, ranges)) in formats.iter().enumerate() {
        if formats[index + 1..].iter().any(|(other, _)| other == size) {
            continue;
        }
        let earlier = formats[..index]
            .iter()
            .filter(|(other, _)| other == size)
            .flat_map(|(_, earlier)| earlier.iter().map(|range| range.1))
            .filter(|&high| {
                ranges
                    .iter()
                    .any(|&(low, top)| high > low - 0.01 && high < top + 0.01)
            });
        for high in ranges.iter().map(|range| range.1).chain(earlier) {
            if !modes.iter().any(|m| m.size() == *size && m.honors(high)) {
                modes.push(Mode {
                    width: size.0,
                    height: size.1,
                    fps: Some((high, high)),
                    input_format: None,
                    intervals: None,
                    min_size: None,
                });
            }
        }
    }
    modes
}
fn dshow_modes(log: &str) -> Vec<Mode> {
    log.lines()
        .filter_map(|line| {
            let (min, max) = line.split_once(" min s=")?.1.split_once(" max s=")?;
            fn caps(text: &str) -> Option<(&str, &str)> {
                let (size, fps) = text.split_once(" fps=")?;
                Some((size, fps.split([' ', '(']).next()?))
            }
            let ((smallest, slowest), (size, fastest)) = (caps(min)?, caps(max)?);
            let (low, high) = (rate(slowest)?, rate(fastest)?);
            let (width, height) = dimensions(size)?;
            let (fast, slow) = (dshow_intervals(fastest), dshow_intervals(slowest));
            let intervals = if fastest == slowest {
                dshow_nominal(&fast).map(|interval| (interval, interval))
            } else {
                fast.last().copied().zip(slow.first().copied())
            };
            Some(Mode {
                width,
                height,
                fps: Some((low.min(high), high)),
                input_format: None,
                intervals: intervals.filter(|(fast, slow)| fast <= slow),
                min_size: dimensions(smallest).filter(|&(low_width, low_height)| {
                    (low_width, low_height) != (width, height)
                        && low_width <= width
                        && low_height <= height
                }),
            })
        })
        .fold(Vec::new(), merge)
}
/// The frame intervals, in 100 ns, that FFmpeg's DirectShow listing prints
/// as the rate `text`, in ascending order. It prints `1e7 / interval` to six
/// significant digits, so 24 fps can stand for 416666 or 416667. Taking the
/// largest at the top of a range and the smallest at its bottom stays inside
/// it.
fn dshow_intervals(text: &str) -> Vec<u32> {
    let six = |fps: f64| format!("{fps:.5e}").parse::<f64>().ok();
    let Some(fps) = text.parse::<f64>().ok().filter(|fps| *fps >= 0.1) else {
        return Vec::new();
    };
    let center = 1e7 / fps;
    let spread = (center * 6e-6).ceil() + 2.0;
    ((center - spread).max(1.0) as u32..=(center + spread) as u32)
        .filter(|&interval| six(1e7 / interval as f64) == six(fps))
        .collect()
}
/// The interval a device with a single listed rate most likely uses: the
/// whole or NTSC rate it approximates, truncated to 100 ns as UVC cameras
/// (15 fps as 666666) and Blackmagic cards (60 fps as 166666) do.
fn dshow_nominal(intervals: &[u32]) -> Option<u32> {
    let &first = intervals.first()?;
    let fps = 1e7 / first as f64;
    let whole = fps.round().max(1.0);
    let ntsc = (fps * 1.001).round() / 1.001;
    let nominal = 1e7
        / if (whole - fps).abs() <= (ntsc - fps).abs() {
            whole
        } else {
            ntsc
        };
    let truncated = nominal.floor() as u32;
    if intervals.contains(&truncated) {
        return Some(truncated);
    }
    intervals.iter().copied().min_by(|a, b| {
        (*a as f64 - nominal)
            .abs()
            .total_cmp(&(*b as f64 - nominal).abs())
    })
}
/// Modes from FFmpeg's V4L2 format listing, with the frame rate bounds
/// `rates` reports for a format's listed description and a size.
fn v4l2_modes(
    log: &str,
    rates: impl Fn(&str, (u32, u32)) -> Option<(f64, f64)>,
) -> (Vec<Mode>, Vec<String>) {
    let mut formats = log
        .lines()
        .filter_map(|line| {
            let (kind, fields) = line.split_once("] ")?.1.split_once(':')?;
            let compressed = match kind.trim_end() {
                "Raw" => false,
                "Compressed" => true,
                _ => return None,
            };
            let mut fields = fields.splitn(3, " : ");
            let (name, description) = (fields.next()?.trim(), fields.next()?.trim());
            let mut sizes = Vec::new();
            for size in fields.next()?.split_whitespace().filter_map(dimensions) {
                if !sizes.contains(&size) {
                    sizes.push(size);
                }
            }
            (name != "Unsupported" && !sizes.is_empty()).then_some((
                compressed,
                name,
                description,
                sizes,
            ))
        })
        .collect::<Vec<_>>();
    // MJPEG frames decode independently, without H.264's inter-frame delay.
    formats.sort_by_key(|&(compressed, name, ..)| (compressed, name != "mjpeg"));
    let mut names = Vec::new();
    let mut modes: Vec<Mode> = Vec::new();
    for (_, name, description, sizes) in formats {
        let input_format = Some(names.len());
        names.push(name.to_string());
        for (width, height) in sizes {
            let fps = rates(description, (width, height))
                .map(|(low, high)| (rounded(low), rounded(high)))
                .filter(|&(_, high)| high > 0.0);
            // Prefer a raw format when the same size is available in both,
            // and keep a compressed one where it reaches a higher frame rate:
            // USB 2.0 webcams offer HD sizes in YUYV at 5-10 fps but in MJPEG
            // at 30.
            let faster = modes
                .iter()
                .filter(|mode| mode.size() == (width, height))
                .all(|mode| {
                    mode.fps
                        .zip(fps)
                        .is_some_and(|((_, kept), (_, high))| high > kept)
                });
            if faster {
                modes.push(Mode {
                    width,
                    height,
                    fps,
                    input_format,
                    intervals: None,
                    min_size: None,
                });
            }
        }
    }
    (modes, names)
}
/// The frame rate bounds the driver reports for a format, by its description
/// in FFmpeg's listing, and a size; FFmpeg lists only the sizes.
#[cfg(target_os = "linux")]
fn v4l2_rates(path: &str) -> impl Fn(&str, (u32, u32)) -> Option<(f64, f64)> {
    let formats = v4l2::Formats::open(std::path::Path::new(path)).ok();
    move |description, (width, height)| formats.as_ref()?.frame_rates(description, width, height)
}
#[cfg(not(target_os = "linux"))]
fn v4l2_rates(_: &str) -> impl Fn(&str, (u32, u32)) -> Option<(f64, f64)> {
    |_, _| None
}

fn probe(source: &str) -> (Vec<Mode>, Vec<String>) {
    let Ok(Some((format, input))) = native_input(source) else {
        return Default::default();
    };
    let options = match format {
        "avfoundation" => ["-framerate", "1234", "-video_size", "16x16"].as_slice(),
        "v4l2" => &["-list_formats", "all"],
        _ => &["-list_options", "true"],
    };
    let args = ["-hide_banner", "-f", format]
        .iter()
        .chain(options)
        .map(|arg| arg.to_string())
        .chain(["-i".into(), input.clone()])
        .collect::<Vec<_>>();
    let Ok((_, log)) = executable().and_then(|exe| bounded_output(&exe, &args, PROBE_TIMEOUT))
    else {
        return Default::default();
    };
    match format {
        "avfoundation" => (avfoundation_modes(&log), Vec::new()),
        "v4l2" => v4l2_modes(&log, v4l2_rates(&input)),
        _ => (dshow_modes(&log), Vec::new()),
    }
}
/// How a host camera's connect error begins when the system refused access;
/// `cli::error_code` and the desktop's settings shortcut both read it.
pub const CAMERA_ACCESS_DENIED: &str = "camera access denied";

/// Whether `error` says the system refused access to a host camera, however
/// session or IPC layers prefixed it.
pub fn is_camera_access_denied(error: &str) -> bool {
    error
        .lines()
        .next()
        .is_some_and(|line| line.to_ascii_lowercase().contains(CAMERA_ACCESS_DENIED))
}

fn native_failure(log: &str, stalled: bool, source: &str, timeout: Duration) -> Option<String> {
    let lower = log.to_ascii_lowercase();
    let platform = scheme(source);
    if log.contains("Cannot use ")
        || [
            "permission denied",
            "access denied",
            "operation not permitted",
            "0x80070005",
        ]
        .iter()
        .any(|text| lower.contains(text))
    {
        let guidance = match platform.as_deref() {
            Some("v4l2") => {
                "check access to the video node and membership in its owning group (commonly video)"
            }
            Some("dshow") => {
                "allow desktop apps to access the camera in Windows Settings > Privacy & security > Camera"
            }
            _ => "allow this app in System Settings > Privacy & Security > Camera",
        };
        Some(format!("{CAMERA_ACCESS_DENIED}: {guidance}"))
    } else if platform.as_deref() == Some("dshow") && log.contains("Could not run graph") {
        // DirectShow reports a device another app holds and one Windows
        // camera privacy blocks alike, without the underlying error.
        Some(format!(
            "camera is in use by another application, or {CAMERA_ACCESS_DENIED} by Windows privacy settings: close other apps using the camera, or allow desktop apps to access the camera in Windows Settings > Privacy & security > Camera"
        ))
    } else if log.contains("Cannot Use ")
        || ["device or resource busy", "already in use"]
            .iter()
            .any(|text| lower.contains(text))
    {
        Some("camera is in use by another application".into())
    } else if log.contains("Not a video capture device.")
        // ENOTTY, as glibc and musl word it.
        || ["Inappropriate ioctl for device", "Not a tty"]
            .iter()
            .any(|text| log.contains(&format!("ioctl(VIDIOC_G_INPUT): {text}")))
    {
        Some(format!(
            "video capture is not supported by {}, which may be a metadata, codec or ISP node; run capturefab native to list capture nodes",
            device_name(source)
        ))
    } else if [
        "video device not found",
        "could not find video device",
        "no such file or directory",
    ]
    .iter()
    .any(|text| lower.contains(text))
    {
        Some(format!(
            "camera device not found: {}; reconnect the device and run capturefab native to obtain its current locator",
            device_name(source)
        ))
    } else {
        let guidance = if platform.as_deref() == Some("avfoundation") {
            "the camera may be suspended (for example a closed MacBook lid), in use, or blocked by permissions"
        } else {
            "check camera permissions, whether another app is using it, and the selected video mode"
        };
        stalled.then(|| {
            format!(
                "no frames from {} (timed out after {} s): {guidance}",
                device_name(source),
                timeout.as_secs_f64()
            )
        })
    }
}

/// The scheme, host, explicit port and path of a network source, without
/// its credentials or query.
fn network_parts(source: &str) -> Option<(String, String, Option<u16>, String)> {
    let (scheme, rest) = source.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host, port.parse().ok())
        }
        _ => (host_port, None),
    };
    let path = rest[end..].split(['?', '#']).next().unwrap_or_default();
    (!host.is_empty()).then(|| {
        (
            scheme.to_ascii_lowercase(),
            host.to_owned(),
            port,
            path.to_owned(),
        )
    })
}

/// Why a network stream failed to open, in one sentence with its likely
/// fix, from FFmpeg's log of the attempt. `timed_out` is when no frame came
/// within `timeout` while FFmpeg still ran. The category is decided here, so
/// exit codes no longer depend on FFmpeg's wording: refused, unresolved,
/// unreachable and missing streams are `unavailable`, rejected credentials
/// `operation_failed` (as before: asking again cannot fix them) and silence
/// `timeout`. None keeps the error FFmpeg gave, for logs this does not know.
fn network_failure(
    source: &str,
    log: &str,
    timed_out: bool,
    timeout: Duration,
) -> Option<anyhow::Error> {
    // Parsed from the redacted URL, so the path suggested back never
    // carries an RTMP stream key or a query token.
    let url = redact_url(source);
    let (scheme, host, port, path) = network_parts(&url)?;
    let at = match port {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };
    // The first log line saying one of `texts`, which follows the
    // explanation as FFmpeg's own words.
    let line = |texts: &[&str]| {
        log.lines().find(|line| {
            let line = line.to_ascii_lowercase();
            texts.iter().any(|text| line.contains(text))
        })
    };
    let (kind, message, said) = if let Some(said) =
        line(&["connection refused", "actively refused"])
    {
        let hint = if scheme == "rtsp" && port.is_none() {
            " RTSP normally uses port 554."
        } else {
            ""
        };
        (
            ErrorKind::Unavailable,
            format!(
                "Nothing accepted a connection at {at} ({url}). Check the camera's stream port and that streaming is turned on.{hint}"
            ),
            Some(said),
        )
    } else if let Some(said) = log.lines().find(|line| refused(line)) {
        (
            ErrorKind::OperationFailed,
            format!(
                "{host} rejected the username or password ({url}). Check them, or add them as {scheme}://USER:PASS@{at}{path} if none were given."
            ),
            Some(said),
        )
    } else if let Some(said) = line(&["404 not found", "failed: 404", "stream not found"]) {
        let path = if path.is_empty() { "/" } else { path.as_str() };
        (
            ErrorKind::Unavailable,
            format!(
                "{at} answered but has no stream at {path} ({url}). Check the path in the camera's settings, or use onvif://USER:PASS@{host}/onvif/device_service to look it up."
            ),
            Some(said),
        )
    } else if let Some(said) = line(&[
        "failed to resolve hostname",
        "nodename nor servname",
        "name or service not known",
        "no such host",
        "temporary failure in name resolution",
    ]) {
        (
            ErrorKind::Unavailable,
            format!("Cannot resolve {host} ({url}). Check the address."),
            Some(said),
        )
    } else if let Some(said) = line(&["no route to host", "network is unreachable", "host is down"])
    {
        let hint = host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .ok()
            .filter(|_| said.to_ascii_lowercase().contains("no route to host"))
            .and_then(|ip| {
                crate::network::local_network_hint(
                    &std::io::Error::from(std::io::ErrorKind::HostUnreachable),
                    Some(ip),
                )
            })
            .map(|hint| format!(" {hint}."))
            .unwrap_or_default();
        (
            ErrorKind::Unavailable,
            format!("{host} is not reachable from this computer ({url}).{hint}"),
            Some(said),
        )
    } else if let Some(said) = line(&["connection timed out", "operation timed out"]) {
        (
            ErrorKind::Timeout,
            format!(
                "No reply from {at} ({url}). Check it is on this network and streaming{}.",
                longer_wait(timeout)
                    .map(|ms| format!(", or retry with a longer timeout (--timeout-ms {ms})"))
                    .unwrap_or_default()
            ),
            Some(said),
        )
    } else if timed_out && log.trim().is_empty() {
        // FFmpeg logs only errors, so silence is also a stream that
        // connected and is slow to send its first frame, such as a long
        // GOP waiting for a keyframe.
        (
            ErrorKind::Timeout,
            format!(
                "No video arrived from {at} within {} s ({url}). It may be unreachable or slow to send its first frame; {}.",
                timeout.as_secs_f64(),
                longer_wait(timeout).map_or_else(
                    || "check it is on this network and streaming".to_owned(),
                    |ms| format!("retry with a longer timeout (--timeout-ms {ms})")
                )
            ),
            None,
        )
    } else {
        // Anything else keeps FFmpeg's own error.
        return None;
    };
    let detail = said
        .map(ffmpeg_words)
        .filter(|said| !said.is_empty())
        .map(|said| format!(" (FFmpeg: {said})"))
        .unwrap_or_default();
    Some(Categorized::error(kind, format!("{message}{detail}")))
}
/// A longer timeout in milliseconds to suggest, while one up to the 60 s
/// limit is left to try: double the wait, and at least 10 s. The advice
/// names the CLI flag in parentheses, after words the desktop reads too.
fn longer_wait(timeout: Duration) -> Option<u128> {
    let current = timeout.as_millis();
    let suggested = (current * 2).clamp(10_000, 60_000);
    (suggested > current).then_some(suggested)
}
/// A log line without FFmpeg's internals: its `[tcp @ 0x…]` component
/// prefix and the `?timeout=` Capturefab passed, with any URL redacted.
fn ffmpeg_words(line: &str) -> String {
    let mut line = line.trim();
    while let Some((_, rest)) = line
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
        .filter(|(component, _)| component.contains(" @ 0x"))
    {
        line = rest.trim_start();
    }
    line.split(' ')
        .map(|word| {
            let word = match word.find("?timeout=") {
                Some(query) => &word[..query],
                None => word,
            };
            if word.contains("://") {
                redact_url(word)
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy, Debug, Default)]
struct NativeOptions {
    size: Option<(u32, u32)>,
    fps: Option<f64>,
}
/// Whether `selector` names a camera the operating system attaches to this
/// computer (`avfoundation:`, `v4l2:` or `dshow:`), such as a webcam, rather
/// than a network stream, a file or a machine-vision camera.
pub fn is_host_camera(selector: &str) -> bool {
    matches!(
        scheme(selector).as_deref(),
        Some("avfoundation" | "v4l2" | "dshow")
    )
}
fn native_source(source: &str) -> Result<(String, Option<NativeOptions>)> {
    if !is_host_camera(source) {
        return Ok((source.into(), None));
    }
    let (base, query) = source.split_once('?').unwrap_or((source, ""));
    ensure!(
        !base.split_once(':').unwrap().1.trim().is_empty(),
        "native media source needs a device name or index"
    );
    ensure!(
        !(base.ends_with('\\') && query.starts_with('\\')),
        "write the ? of a DirectShow alternative name as %3F, as capturefab native lists it"
    );
    let mut options = NativeOptions::default();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .context("native camera options need key=value")?;
        match key {
            "fps" => {
                ensure!(
                    options.fps.is_none(),
                    "native camera fps option was supplied more than once"
                );
                let fps: f64 = value
                    .parse()
                    .context("native camera fps must be a number")?;
                ensure!(
                    fps.is_finite() && (0.1..=240.0).contains(&fps),
                    "native camera fps must be 0.1..240"
                );
                options.fps = Some(fps);
            }
            "size" => {
                ensure!(
                    options.size.is_none(),
                    "native camera size option was supplied more than once"
                );
                let (w, h) = value
                    .split_once('x')
                    .context("native camera size must be WIDTHxHEIGHT")?;
                let w = w.parse::<u32>()?;
                let h = h.parse::<u32>()?;
                ensure!(
                    (16..=8192).contains(&w)
                        && (16..=8192).contains(&h)
                        && w as usize * h as usize <= MAX_FRAME / 3,
                    "native camera size is outside 16..8192 or frame memory bound"
                );
                options.size = Some((w, h));
            }
            _ => bail!("unsupported native camera option {key}; use size=1280x720 and fps=30"),
        }
    }
    Ok((base.into(), Some(options)))
}
struct MediaBackend {
    source: String,
    timeout: Duration,
    decoder: Option<Decoder>,
    first: Option<Frame>,
    width: u32,
    height: u32,
    native: bool,
    requested_size: bool,
    fps: f64,
    running: bool,
    modes: Vec<Mode>,
    mode: Option<usize>,
    formats: Vec<String>,
    /// Set while a network stream is being reopened after FFmpeg exited.
    reconnect: Option<Reconnect>,
    /// A video file plays at its own frame rate, like a camera, unless
    /// PlaybackSpeed asks for every frame as fast as it decodes.
    paced: bool,
    /// Whether this acquisition's first frame was asked for on demand, by a
    /// capture, rather than taken by a live view; unknown until then.
    on_demand: Option<bool>,
}
/// How a media source's stream ended, for callers that act on the end rather
/// than on the wording of its error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamEnd {
    /// FFmpeg exited and the network source is being reopened. The message
    /// says "timed out", so a caller waiting for a frame keeps waiting.
    Lost,
    /// A video file played to its end.
    Finished,
    /// The server rejected the credentials of several reopenings in a row.
    Abandoned,
}
#[derive(Debug)]
struct Ended {
    end: StreamEnd,
    message: String,
}
impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Ended {}
/// The stream end a frame error reports, if it reports one.
pub fn stream_end(error: &anyhow::Error) -> Option<StreamEnd> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Ended>())
        .map(|ended| ended.end)
}
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Launches in a row whose credentials the server rejects before
/// reconnecting stops.
const MAX_REFUSED: u32 = 5;
/// A stream that stays up this long ends its outage; one that drops again
/// sooner keeps backing off.
const STEADY: Duration = Duration::from_secs(10);
/// Reopening a network stream, with backoff from 1 s to 30 s between launches.
struct Reconnect {
    /// The launch in progress or due next, from 1 in each outage.
    attempt: u32,
    retry_at: Instant,
    backoff: Duration,
    /// Launches in a row that ended with rejected credentials.
    refused: u32,
    /// When frames came back, while the stream has not yet stayed up for
    /// `STEADY`.
    resumed: Option<Instant>,
}
impl Reconnect {
    fn new() -> Self {
        Self {
            attempt: 1,
            retry_at: Instant::now() + RECONNECT_BACKOFF,
            backoff: RECONNECT_BACKOFF * 2,
            refused: 0,
            resumed: None,
        }
    }
    fn retry_later(&mut self) {
        self.attempt += 1;
        self.retry_at = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
    fn waiting(&self, source: &str) -> anyhow::Error {
        anyhow!(
            "reconnecting to {} (attempt {}): timed out",
            redact_url(source),
            self.attempt
        )
    }
}
/// FFmpeg's wording when the server rejects the credentials, which asking
/// again cannot fix. A missing stream path can: a relay such as MediaMTX
/// answers 404 while its camera is away.
fn refused(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    ["401 ", "403 ", "unauthorized", "forbidden"]
        .iter()
        .any(|text| reason.contains(text))
}
impl MediaBackend {
    /// Network streams are reopened when FFmpeg exits; files and local
    /// devices report their end instead.
    fn reconnects(&self) -> bool {
        scheme(&self.source)
            .is_some_and(|s| !matches!(s.as_str(), "file" | "avfoundation" | "v4l2" | "dshow"))
    }
    /// The next frame within `timeout`. A network stream whose FFmpeg exited
    /// is relaunched with backoff, and the wait for the next launch fits in
    /// `timeout`, so a caller polling for frames never spins.
    fn decode(&mut self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now() + timeout;
        let (reconnects, file, clip) = (self.reconnects(), self.file(), self.clip());
        loop {
            if let Some(decoder) = self.decoder.as_mut() {
                let error = match decoder.next(deadline.saturating_duration_since(Instant::now())) {
                    Ok(frame) => {
                        self.delivered();
                        return Ok(frame);
                    }
                    Err(error) => error,
                };
                if !decoder.ended() {
                    return Err(match &self.reconnect {
                        Some(reconnect) if reconnect.resumed.is_none() => {
                            reconnect.waiting(&self.source)
                        }
                        _ => error,
                    });
                }
                if (file || clip) && decoder.succeeded() {
                    let name = if file {
                        local_file_path(&self.source)?.display().to_string()
                    } else {
                        redact_url(&self.source)
                    };
                    return Err(Ended {
                        end: StreamEnd::Finished,
                        message: format!("video file finished: {name}"),
                    }
                    .into());
                }
                if !reconnects {
                    return Err(error);
                }
                self.decoder = None;
                return Err(self.lost(format!("{error:#}")));
            }
            let reconnect = self.reconnect.as_mut().context("media decoder stopped")?;
            let wait = reconnect.retry_at.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
                if Instant::now() < reconnect.retry_at {
                    return Err(reconnect.waiting(&self.source));
                }
            }
            match Decoder::spawn_configured(&self.source, self.timeout, &self.input_options()) {
                Ok(decoder) => self.decoder = Some(decoder),
                Err(error) => return Err(self.lost(format!("{error:#}"))),
            }
        }
    }
    /// A frame arrived: an outage ends once the stream has stayed up for
    /// `STEADY`.
    fn delivered(&mut self) {
        if let Some(reconnect) = &mut self.reconnect
            && reconnect.resumed.get_or_insert_with(Instant::now).elapsed() >= STEADY
        {
            self.reconnect = None;
        }
    }
    /// The network stream's FFmpeg exited: schedule the next launch, or
    /// stop once the server has rejected the credentials several times in a
    /// row. A loss is reported when the stream was up, and the backoff of
    /// an outage carries on until the stream stays up.
    fn lost(&mut self, reason: String) -> anyhow::Error {
        let url = redact_url(&self.source);
        let attempt = match self.reconnect.as_mut() {
            None => self.reconnect.insert(Reconnect::new()).attempt,
            // Frames came back, then stopped before the stream settled.
            Some(reconnect) if reconnect.resumed.is_some() => {
                (reconnect.resumed, reconnect.refused) = (None, 0);
                reconnect.retry_later();
                reconnect.attempt
            }
            Some(reconnect) => {
                reconnect.refused = if refused(&reason) {
                    reconnect.refused + 1
                } else {
                    0
                };
                if reconnect.refused >= MAX_REFUSED {
                    let attempts = reconnect.attempt;
                    self.reconnect = None;
                    return Ended {
                        end: StreamEnd::Abandoned,
                        message: format!(
                            "stopped reconnecting to {url} after {attempts} attempts ({reason})"
                        ),
                    }
                    .into();
                }
                reconnect.retry_later();
                return reconnect.waiting(&self.source);
            }
        };
        Ended {
            end: StreamEnd::Lost,
            message: format!(
                "reconnecting to {url} (attempt {attempt}): timed out after the stream ended ({reason})"
            ),
        }
        .into()
    }
    fn configure(&mut self, options: NativeOptions) -> Result<()> {
        self.fps = options.fps.unwrap_or(30.0);
        if let Some((width, height)) = options.size {
            (self.width, self.height, self.requested_size) = (width, height, true);
        }
        if self.modes.is_empty()
            || options.size.is_none() && self.modes.iter().all(|m| m.fps.is_none())
        {
            return Ok(());
        }
        let index = best(&self.modes, |m| {
            options.size.is_none_or(|size| m.fits(size))
                && options.fps.is_none_or(|fps| m.honors(fps))
        })
        .ok_or_else(|| {
            let (width, height) = options.size.unwrap_or_else(|| {
                best(&self.modes, |_| true).map_or((0, 0), |i| self.modes[i].size())
            });
            self.unsupported(width, height, self.fps)
        })?;
        let index = options.fps.map_or(index, |fps| self.raw_first(index, fps));
        self.select(index, options.fps);
        if let Some(size) = options.size {
            (self.width, self.height) = size;
        }
        Ok(())
    }
    /// The first listed V4L2 mode of the size at `index` that reaches `fps`.
    /// Raw formats are listed first, so this is raw wherever a raw format
    /// delivers the rate, rather than the faster compressed format ranking
    /// prefers.
    fn raw_first(&self, index: usize, fps: f64) -> usize {
        let size = self.modes[index].size();
        self.modes
            .iter()
            .position(|m| m.size() == size && m.honors(fps) && m.input_format.is_some())
            .unwrap_or(index)
    }
    fn select(&mut self, index: usize, fps: Option<f64>) {
        let mode = self.modes[index];
        self.mode = Some(index);
        (self.width, self.height) = mode.size();
        self.fps = mode.fps.map_or(fps.unwrap_or(self.fps), |(low, high)| {
            fps.map_or(high, |fps| fps.clamp(low, high))
        });
    }
    fn unsupported(&self, width: u32, height: u32, fps: f64) -> anyhow::Error {
        let mut listed = self.modes.clone();
        listed.sort_by_key(|m| (m.size() != (width, height), Reverse(m.rank())));
        let mut seen = HashSet::new();
        listed.retain(|m| m.size() == (width, height) || seen.insert(m.size()));
        let listed = listed.iter().take(8).map(Mode::label).collect::<Vec<_>>();
        anyhow!(
            "{width}x{height}@{} is not supported by {}; supported: {}",
            rounded(fps),
            device_name(&self.source),
            listed.join(", ")
        )
    }
    fn file(&self) -> bool {
        matches!(scheme(&self.source).as_deref(), None | Some("file"))
    }
    /// An http(s) URL naming a video file rather than a live stream: it
    /// finishes like a local file instead of reconnecting.
    fn clip(&self) -> bool {
        const VIDEO: [&str; 10] = [
            "mp4", "m4v", "mov", "mkv", "webm", "avi", "mpg", "mpeg", "wmv", "ogv",
        ];
        let path = self.source.split(['?', '#']).next().unwrap_or_default();
        matches!(scheme(&self.source).as_deref(), Some("http" | "https"))
            && path
                .rsplit('/')
                .next()
                .and_then(|name| name.rsplit_once('.'))
                .is_some_and(|(_, extension)| {
                    VIDEO
                        .iter()
                        .any(|video| extension.eq_ignore_ascii_case(video))
                })
    }
    fn input_options(&self) -> Vec<String> {
        if self.file() && self.paced && self.on_demand != Some(true) {
            return vec!["-readrate".into(), "1".into()];
        }
        if !self.native {
            return Vec::new();
        }
        let rate = match self.mode.map(|index| self.modes[index]) {
            Some(Mode {
                fps: Some((low, high)),
                intervals: Some((fast, slow)),
                ..
            }) if self.fps >= high || self.fps <= low => {
                // A rate FFmpeg turns back into exactly this interval.
                let interval = if self.fps >= high { fast } else { slow };
                format!("{:.9}", 1e7 / (interval as f64 + 0.5))
            }
            _ => self.fps.to_string(),
        };
        let mut options = vec!["-framerate".to_string(), rate];
        if let Some((width, height)) =
            (self.mode.is_some() || self.requested_size).then_some((self.width, self.height))
        {
            options.extend(["-video_size".into(), format!("{width}x{height}")]);
        }
        let format = self
            .mode
            .and_then(|index| self.modes[index].input_format)
            .and_then(|index| self.formats.get(index))
            .or_else(|| self.formats.first());
        if let Some(format) = format {
            options.extend(["-input_format".into(), format.clone()]);
        }
        if self.source.starts_with("avfoundation:") {
            options.extend(["-pixel_format".into(), "uyvy422".into()]);
        }
        options
    }
    fn launch(&mut self) -> Result<()> {
        let mut decoder =
            Decoder::spawn_configured(&self.source, self.timeout, &self.input_options())?;
        let first = if self.native {
            let timeout = self.timeout.max(FIRST_FRAME_TIMEOUT);
            decoder.next(timeout).map_err(|error| {
                let stalled = decoder
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error
                    .is_none();
                decoder.stop();
                let log = decoder.log.text(&self.source);
                native_failure(&log, stalled, &self.source, timeout)
                    .map_or(error, |message| anyhow!(message))
            })?
        } else {
            decoder.next(self.timeout).map_err(|error| {
                if !self.reconnects() {
                    return error.context("open media input");
                }
                let timed_out = !error.chain().any(|cause| cause.is::<StreamEnded>());
                decoder.stop();
                let log = decoder.log.text(&self.source);
                match network_failure(&self.source, &log, timed_out, self.timeout) {
                    // FFmpeg's own words stay below the explanation, for
                    // `{error:#}` and whoever debugs it.
                    Some(explained) => explained,
                    None => error.context("open media input"),
                }
            })?
        };
        if self.native && (self.requested_size || self.mode.is_some()) {
            ensure!(
                (first.width, first.height) == (self.width, self.height),
                "requested native video size {}x{} is not supported by {}; device returned {}x{}",
                self.width,
                self.height,
                device_name(&self.source),
                first.width,
                first.height
            );
        }
        (self.width, self.height) = (first.width, first.height);
        self.mode = self.mode.or_else(|| {
            self.modes
                .iter()
                .position(|m| m.size() == (first.width, first.height) && m.honors(self.fps))
        });
        self.first = Some(first);
        self.decoder = Some(decoder);
        Ok(())
    }
}
pub fn open(info: &CameraInfo, timeout: Duration) -> Result<Box<dyn Backend>> {
    ensure!(
        info.transport == Transport::Media,
        "camera is not a media input"
    );
    let source = info
        .address
        .clone()
        .context("media camera has no source address")?;
    ensure!(
        !source.contains("<redacted>"),
        "credential-bearing media sources must be opened through open_url"
    );
    open_url(&source, timeout)
}
pub fn open_url(source: &str, timeout: Duration) -> Result<Box<dyn Backend>> {
    open_url_with(source, None, timeout)
}
/// FFmpeg's wording when an RTSP server asks for credentials or refuses them.
/// 403 is left out: RTSP servers often answer it for an address or ACL
/// restriction that no password changes.
fn auth_refused(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("401 ") || reason.contains("unauthorized")
}
/// Open a media URL, with a camera sign-in given beside it. A sign-in request
/// or refusal from an rtsp:// or rtsps:// server becomes a typed
/// `auth_required` or `auth_rejected` error naming the address without
/// credentials.
pub fn open_url_with(
    source: &str,
    login: Option<&crate::logins::CameraLogin>,
    timeout: Duration,
) -> Result<Box<dyn Backend>> {
    let url = match login {
        Some(login) => crate::logins::with_credentials(source, login)?,
        None => zeroize::Zeroizing::new(source.to_owned()),
    };
    open_source(&url, timeout).map_err(|error| {
        let rtsp = scheme(source).is_some_and(|s| s == "rtsp" || s == "rtsps");
        if rtsp && auth_refused(&format!("{error:#}")) {
            let had_credentials = login.is_some()
                || source.split_once('@').is_some_and(|(before, _)| {
                    before
                        .split_once("://")
                        .is_some_and(|(_, authority)| !authority.contains('/'))
                });
            crate::logins::auth_error(
                &redact_url(&crate::logins::strip_userinfo(source)),
                had_credentials,
            )
        } else {
            error
        }
    })
}
/// FFmpeg's RTSP client copies the user name and password, as written in the
/// address, into a 128-byte buffer and silently cuts what does not fit
/// (`av_url_split` in `ff_rtsp_connect`), so the camera refuses a cut-off
/// password.
fn check_rtsp_userinfo(source: &str) -> Result<()> {
    const MAX: usize = 127;
    let Some((_, rest)) = source.split_once("://") else {
        return Ok(());
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    match authority.rsplit_once('@') {
        Some((userinfo, _)) if userinfo.len() > MAX => Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "the user name and password for {} take {} bytes in the address, but FFmpeg's RTSP client keeps only {MAX}; use a shorter password (symbols and non-ASCII letters take 3 bytes or more when percent-encoded)",
                redact_url(&crate::logins::strip_userinfo(source)),
                userinfo.len()
            ),
        )),
        _ => Ok(()),
    }
}
fn open_source(source: &str, timeout: Duration) -> Result<Box<dyn Backend>> {
    validate_source(source)?;
    if matches!(scheme(source).as_deref(), Some("rtsp" | "rtsps")) {
        check_rtsp_userinfo(source)?;
    }
    ensure!(
        !timeout.is_zero() && Instant::now().checked_add(timeout).is_some(),
        "media timeout must be positive and fit in the monotonic clock"
    );
    let (source, options) = native_source(source)?;
    let mut backend = MediaBackend {
        source,
        timeout,
        decoder: None,
        first: None,
        width: 0,
        height: 0,
        native: options.is_some(),
        requested_size: false,
        fps: 0.0,
        running: false,
        modes: Vec::new(),
        mode: None,
        formats: Vec::new(),
        reconnect: None,
        paced: true,
        on_demand: None,
    };
    if let Some(options) = options {
        (backend.modes, backend.formats) = probe(&backend.source);
        backend.configure(options)?;
    }
    backend.launch()?;
    Ok(Box::new(backend))
}
fn backend_xml(native: bool, modes: &[Mode], playback: bool) -> String {
    let video_mode = if !modes.is_empty() {
        "<pFeature>VideoMode</pFeature>"
    } else if playback {
        "<pFeature>PlaybackSpeed</pFeature>"
    } else {
        ""
    };
    let mut xml = format!(
        "<RegisterDescription><Category Name='Root'><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>PixelFormat</pFeature><pFeature>PayloadSize</pFeature><pFeature>AcquisitionFrameRate</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature>{video_mode}</Category>"
    );
    for (name, address) in [
        ("Width", 0),
        ("Height", 4),
        ("PixelFormat", 8),
        ("PayloadSize", 12),
    ] {
        let writable = native && matches!(name, "Width" | "Height");
        let access = if writable { "RW" } else { "RO" };
        let bounds = if writable {
            "<Min>16</Min><Max>8192</Max><pIsLocked>IsAcquiring</pIsLocked>"
        } else {
            ""
        };
        xml.push_str(&format!("<Integer Name='{name}'><pValue>{name}Reg</pValue>{bounds}</Integer><IntReg Name='{name}Reg'><Address>{address}</Address><Length>4</Length><AccessMode>{access}</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"));
    }
    let access = if native { "RW" } else { "RO" };
    xml.push_str(&format!("<Float Name='AcquisitionFrameRate'><pValue>FrameRateReg</pValue><Min>0.1</Min><Max>240</Max><Unit>Hz</Unit><pIsLocked>IsAcquiring</pIsLocked></Float><FloatReg Name='FrameRateReg'><Address>20</Address><Length>8</Length><AccessMode>{access}</AccessMode><Endianess>LittleEndian</Endianess></FloatReg><Integer Name='IsAcquiring'><pValue>AcquiringReg</pValue></Integer><IntReg Name='AcquiringReg'><Address>16</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"));
    if !modes.is_empty() {
        let entries = modes
            .iter()
            .enumerate()
            .map(|(index, mode)| {
                format!(
                    "<EnumEntry Name='{}'><Value>{index}</Value></EnumEntry>",
                    mode.label()
                )
            })
            .collect::<String>();
        xml.push_str(&format!("<Enumeration Name='VideoMode'><pValue>VideoModeReg</pValue>{entries}<pIsLocked>IsAcquiring</pIsLocked></Enumeration><IntReg Name='VideoModeReg'><Address>28</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"));
    } else if playback {
        xml.push_str("<Enumeration Name='PlaybackSpeed'><ToolTip>RealTime plays the file at its frame rate in live view and recordings, while captures read it as fast as they ask; Fastest always decodes every frame as fast as possible</ToolTip><EnumEntry Name='RealTime'><Value>1</Value></EnumEntry><EnumEntry Name='Fastest'><Value>0</Value></EnumEntry><pValue>PlaybackReg</pValue><pIsLocked>IsAcquiring</pIsLocked></Enumeration><IntReg Name='PlaybackReg'><Address>32</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>");
    }
    xml.push_str("<Command Name='AcquisitionStart'><pValue>Control</pValue><CommandValue>1</CommandValue></Command><Command Name='AcquisitionStop'><pValue>Control</pValue><CommandValue>0</CommandValue></Command><IntReg Name='Control'><Address>16</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg></RegisterDescription>");
    xml
}
/// The 32-byte register file every media-like source exposes: width,
/// height, pixel format, payload size, acquisition state, frame rate and
/// video mode.
fn registers(width: u32, height: u32, running: bool, fps: f64, mode: u32) -> [u8; 32] {
    let mut registers = [0u8; 32];
    registers[0..4].copy_from_slice(&width.to_le_bytes());
    registers[4..8].copy_from_slice(&height.to_le_bytes());
    registers[8..12].copy_from_slice(&RGB8.to_le_bytes());
    registers[12..16].copy_from_slice(&(width * height * 3).to_le_bytes());
    registers[16..20].copy_from_slice(&(running as u32).to_le_bytes());
    registers[20..28].copy_from_slice(&fps.to_le_bytes());
    registers[28..32].copy_from_slice(&mode.to_le_bytes());
    registers
}
fn register_range(registers: &[u8], address: u64, length: usize) -> Result<Vec<u8>> {
    let start = usize::try_from(address)?;
    let end = start
        .checked_add(length)
        .context("media register range overflow")?;
    Ok(registers
        .get(start..end)
        .context("unknown media register")?
        .to_vec())
}
/// Registers of a read-only RGB8 source, such as a camera hub stream.
#[cfg(feature = "hubs")]
pub(crate) fn read_only_registers(
    width: u32,
    height: u32,
    running: bool,
    fps: f64,
    address: u64,
    length: usize,
) -> Result<Vec<u8>> {
    register_range(
        &registers(width, height, running, fps, u32::MAX),
        address,
        length,
    )
}
/// GenICam XML for a read-only RGB8 source over [`read_only_registers`]:
/// Width, Height, PixelFormat, PayloadSize and AcquisitionFrameRate are read
/// only; AcquisitionStart and AcquisitionStop write register 16.
#[cfg(feature = "hubs")]
pub(crate) fn read_only_xml() -> String {
    backend_xml(false, &[], false)
}
impl RegisterIo for MediaBackend {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let mut registers = [0u8; 36];
        registers[..32].copy_from_slice(&self::registers(
            self.width,
            self.height,
            self.running,
            self.fps,
            self.mode.map_or(u32::MAX, |i| i as u32),
        ));
        registers[32..36].copy_from_slice(&(self.paced as u32).to_le_bytes());
        register_range(&registers, address, length)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        if address == 16 && data.len() == 4 {
            return Ok(());
        }
        if address == 32 && data.len() == 4 && self.file() {
            ensure!(
                !self.running,
                "stop acquisition before changing the playback speed"
            );
            self.paced = u32::from_le_bytes(data.try_into()?) != 0;
            self.decoder.take();
            self.first = None;
            return Ok(());
        }
        ensure!(
            self.native,
            "media format features are read-only; configure the source camera through ONVIF or its administration interface"
        );
        ensure!(
            !self.running,
            "stop native acquisition before changing video mode"
        );
        match (address, data.len()) {
            (0 | 4, 4) => {
                let value = u32::from_le_bytes(data.try_into()?);
                ensure!(
                    (16..=8192).contains(&value),
                    "native dimensions must be 16..8192"
                );
                let (w, h) = if address == 0 {
                    (value, self.height)
                } else {
                    (self.width, value)
                };
                ensure!(
                    w as usize * h as usize <= MAX_FRAME / 3,
                    "native video mode exceeds frame memory bound"
                );
                if self.modes.is_empty() {
                    (self.width, self.height, self.requested_size) = (w, h, true);
                } else {
                    let index = self
                        .modes
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| {
                            let (low_width, low_height) = m.min_size.unwrap_or(m.size());
                            if address == 0 {
                                (low_width..=m.width).contains(&w)
                            } else {
                                (low_height..=m.height).contains(&h)
                            }
                        })
                        .max_by_key(|&(index, m)| {
                            let current = self.mode == Some(index);
                            (m.fits((w, h)), m.honors(self.fps), current, m.rank())
                        })
                        .map(|(index, _)| index)
                        .ok_or_else(|| self.unsupported(w, h, self.fps))?;
                    let index = if self.mode == Some(index) {
                        index
                    } else {
                        self.raw_first(index, self.fps)
                    };
                    self.select(index, Some(self.fps));
                    if self.modes[index].fits((w, h)) {
                        (self.width, self.height) = (w, h);
                    }
                }
            }
            (20, 8) => {
                let value = f64::from_le_bytes(data.try_into()?);
                ensure!(
                    value.is_finite() && (0.1..=240.0).contains(&value),
                    "native frame rate must be 0.1..240"
                );
                if self.modes.is_empty() {
                    self.fps = value;
                } else {
                    let size = (self.width, self.height);
                    let index = self
                        .mode
                        .filter(|&i| self.modes[i].fits(size) && self.modes[i].honors(value))
                        .or_else(|| {
                            best(&self.modes, |m| m.fits(size) && m.honors(value))
                                .map(|i| self.raw_first(i, value))
                        })
                        .ok_or_else(|| self.unsupported(size.0, size.1, value))?;
                    self.select(index, Some(value));
                    (self.width, self.height) = size;
                }
            }
            (28, 4) => {
                let index = u32::from_le_bytes(data.try_into()?) as usize;
                ensure!(index < self.modes.len(), "unknown native video mode");
                self.select(index, None);
            }
            _ => bail!("unsupported native camera register"),
        }
        self.decoder.take();
        self.first = None;
        self.reconnect = None;
        Ok(())
    }
}
impl Backend for MediaBackend {
    fn xml(&mut self) -> Result<String> {
        Ok(backend_xml(self.native, &self.modes, self.file()))
    }
    fn start(&mut self, _: usize) -> Result<()> {
        self.on_demand = None;
        if self.decoder.is_none() {
            self.launch()?;
        }
        self.running = true;
        Ok(())
    }
    /// A capture that asks for frames before a live view takes any reads a
    /// paced file as fast as it asks, like frames from a triggered camera;
    /// the file starts again unpaced.
    fn request_frame(&mut self) -> Result<()> {
        if self.on_demand.is_none() {
            self.on_demand = Some(true);
            if self.file() && self.paced {
                (self.decoder, self.first) = (None, None);
                self.decoder = Some(Decoder::spawn_configured(
                    &self.source,
                    self.timeout,
                    &self.input_options(),
                )?);
            }
        }
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.running, "media acquisition has not started");
        self.on_demand.get_or_insert(false);
        let frame = match self.first.take() {
            Some(frame) => frame,
            None => self.decode(timeout)?,
        };
        self.width = frame.width;
        self.height = frame.height;
        Ok(frame)
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.decoder.take();
        self.first = None;
        self.reconnect = None;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardConfig {
    pub output: String,
    pub codec: String,
    pub encoder: String,
    pub fps: f64,
    pub bitrate: String,
    #[serde(default)]
    pub storage: crate::storage::StoragePolicy,
    #[serde(default = "default_recording_cap")]
    pub max_file_bytes: u64,
    /// A saved destination for local recordings; `output` is then a file
    /// name inside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}
impl ForwardConfig {
    /// Validate forwarding settings before starting a process, changing camera
    /// settings, reserving storage or looking up a saved destination.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.fps.is_finite() && (0.1..=240.0).contains(&self.fps),
            "forwarding FPS must be a finite number within 0.1..240"
        );
        let family = codec_family(&self.codec)?;
        validate_encoder(&self.codec, &self.encoder)?;
        if jetson_encoder(&self.encoder) {
            jetson_framerate(self.fps)?;
        }
        validate_bitrate(&self.bitrate)?;
        output_args(&self.output)?;
        crate::destination::validate_output(self.destination.as_deref(), &self.output)?;
        let recording = recording_output(&self.output)?;
        if self.destination.is_some() {
            ensure!(
                recording.is_some(),
                "a destination holds recordings; omit --destination for network streams"
            );
        }
        // Storage fields remain valid even when a network stream does not use them.
        self.storage.validate()?;
        ensure!(
            self.max_file_bytes > 0,
            "recording reservation must be positive"
        );
        if let Some((_, container)) = recording {
            ensure!(
                self.max_file_bytes <= self.storage.max_bytes,
                "recording reservation must be no greater than the storage byte budget"
            );
            ensure!(
                container != "webm" || family == "av1",
                "WebM recordings require the av1 codec; use .mkv or .mp4 for h264 or hevc"
            );
        }
        Ok(())
    }
}
pub fn default_recording_cap() -> u64 {
    512 * 1024 * 1024
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForwardStats {
    pub accepted: u64,
    pub written: u64,
    pub dropped: u64,
    /// Frames replaced while FFmpeg was still taking an earlier one: the
    /// encoder cannot keep up. `dropped` includes them.
    #[serde(default)]
    pub late: u64,
    /// No new camera frame for 2 s or more; nothing new reaches the encoder
    /// meanwhile.
    #[serde(default)]
    pub stalled: bool,
    pub encoder: String,
    pub error: Option<String>,
    /// The finished local recording, set once its file is complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<crate::dataset::RecordedFile>,
}
struct WriterState {
    latest: Option<Vec<u8>>,
    error: Option<String>,
}
struct WriterQueue {
    state: Mutex<WriterState>,
    changed: Condvar,
    running: AtomicBool,
    accepted: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    late: AtomicU64,
    /// The writer is blocked handing a frame to FFmpeg.
    busy: AtomicBool,
    /// Writing the encoded recording failed, for example on a full disk.
    output_failed: AtomicBool,
    epoch: Instant,
    /// When the newest frame arrived, in milliseconds since `epoch`.
    arrived_ms: AtomicU64,
}
/// Without a new frame for this long a recording counts as stalled.
const STALL: Duration = Duration::from_secs(2);
/// A recording this small holds at most its container's header, no video.
const HEADER_BYTES: u64 = 4096;
impl WriterQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(WriterState {
                latest: None,
                error: None,
            }),
            changed: Condvar::new(),
            running: AtomicBool::new(true),
            accepted: AtomicU64::new(0),
            written: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            late: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            output_failed: AtomicBool::new(false),
            epoch: Instant::now(),
            arrived_ms: AtomicU64::new(0),
        }
    }
    fn stalled(&self) -> bool {
        let arrived = Duration::from_millis(self.arrived_ms.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(arrived) >= STALL
    }
    /// Wait until `slot` and a new frame. With `repeat` the slot alone is
    /// enough, and `Some(None)` asks for the previous frame again. `None`
    /// once the writer stops.
    fn take(&self, slot: Instant, repeat: bool) -> Option<Option<Vec<u8>>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !self.running.load(Ordering::Acquire) {
                return None;
            }
            let ready = repeat || state.latest.is_some();
            let wait = slot.saturating_duration_since(Instant::now());
            if ready && wait.is_zero() {
                return Some(state.latest.take());
            }
            let wait = if ready { wait } else { Duration::from_secs(1) };
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    fn fail(&self, error: String) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .get_or_insert(error);
    }
}
struct EncoderProcess {
    child: Child,
    writer: Option<JoinHandle<()>>,
    output_thread: Option<JoinHandle<EncodedOutput>>,
    log_thread: Option<JoinHandle<()>>,
    queue: Arc<WriterQueue>,
    log: Arc<Log>,
    _exe: Arc<Executable>,
    width: u32,
    height: u32,
    raw: RawInput,
    /// Set when a GStreamer Jetson encoder process precedes FFmpeg.
    jetson: Option<JetsonInput>,
    stage: Option<Child>,
    stage_log: Option<JoinHandle<()>>,
    encoder: String,
    /// When the recording file was reserved, UTC Unix milliseconds.
    started_ms: u64,
    spawned: Instant,
    /// This recording's share of the host's software encodes.
    _active: Active,
}
struct EncodedOutput {
    storage: crate::storage::StorageWriter,
    error: Option<String>,
}
pub struct Forwarder {
    config: ForwardConfig,
    process: Option<EncoderProcess>,
    final_stats: ForwardStats,
    /// The recording's local folder destination.
    destination: Option<crate::destination::Resolved>,
    /// The recording file, created when its name was picked so no other
    /// writer can take the name before the first frame.
    reserved: Option<crate::storage::StorageWriter>,
    /// An automatically chosen encoder that exited early; the recording
    /// falls back once to the next candidate.
    failed: Option<String>,
}
fn codec_family(codec: &str) -> Result<&'static str> {
    match codec {
        "h264" | "avc" => Ok("h264"),
        "h265" | "hevc" => Ok("hevc"),
        "av1" => Ok("av1"),
        _ => bail!("unsupported forwarding codec {codec}; use h264, hevc or av1"),
    }
}
/// The codec an FFmpeg encoder writes, `h264`, `hevc` or `av1`, or None for
/// encoders of other codecs.
pub fn encoder_family(encoder: &str) -> Option<&'static str> {
    match encoder {
        "h264" | "libx264" | "libx264rgb" | "libopenh264" | "nvv4l2h264enc" => Some("h264"),
        "hevc" | "libx265" | "nvv4l2h265enc" => Some("hevc"),
        "av1" | "libsvtav1" | "libaom-av1" | "librav1e" | "nvv4l2av1enc" => Some("av1"),
        name if name.starts_with("h264_") => Some("h264"),
        name if name.starts_with("hevc_") => Some("hevc"),
        name if name.starts_with("av1_") => Some("av1"),
        _ => None,
    }
}
pub fn validate_encoder(codec: &str, encoder: &str) -> Result<()> {
    let family = codec_family(codec)?;
    ensure!(
        !encoder.is_empty()
            && encoder.len() <= 64
            && encoder.as_bytes()[0].is_ascii_alphanumeric()
            && encoder
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid forwarding encoder name; use auto or an FFmpeg video encoder such as libx264"
    );
    if let Some(encoder_family) = encoder_family(encoder) {
        ensure!(
            family == encoder_family,
            "forwarding encoder {encoder} encodes {encoder_family}, but the selected codec is {family}"
        );
    }
    Ok(())
}

pub fn validate_bitrate(bitrate: &str) -> Result<()> {
    bits_per_second(bitrate).map(|_| ())
}
/// The bytes a recording at `bitrate` (such as `4M`) writes in `duration_ms`:
/// bits per second times seconds, over 8, rounded up. Encoders overshoot and
/// containers add a little; `recording_reservation` allows for both.
pub fn estimate_recording_bytes(bitrate: &str, duration_ms: u64) -> Result<u64> {
    Ok(estimated_bytes(bits_per_second(bitrate)?, duration_ms))
}
/// The `max_file_bytes` to reserve for a recording at `bitrate` that lasts
/// `duration_ms`: the estimate plus 25% and 16 MiB, rounded up to a whole
/// MiB. A recording stops when it outgrows its reservation, so err high.
pub fn recording_reservation(bitrate: &str, duration_ms: u64) -> Result<u64> {
    Ok(reservation_bytes(bits_per_second(bitrate)?, duration_ms))
}
/// The longest recording at `bitrate`, in whole minutes (as milliseconds,
/// at most 10 years), whose `recording_reservation` fits in
/// `max_file_bytes`; 0 when not even one minute fits.
pub fn max_duration_for(bitrate: &str, max_file_bytes: u64) -> Result<u64> {
    const MINUTE: u64 = 60_000;
    let bits = bits_per_second(bitrate)?;
    let fits = |minutes: u64| reservation_bytes(bits, minutes * MINUTE) <= max_file_bytes;
    // The reservation grows with the duration: find the last minute that
    // fits. `low` is 0 or a count of minutes that fits.
    let (mut low, mut high) = (0, crate::scheduling::MAX_DURATION_MS / MINUTE);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(low * MINUTE)
}
fn estimated_bytes(bits_per_second: u64, duration_ms: u64) -> u64 {
    let bits = u128::from(bits_per_second) * u128::from(duration_ms);
    // Bit-milliseconds over 8 bits per byte and 1000 ms per second.
    u64::try_from(bits.div_ceil(8000)).unwrap_or(u64::MAX)
}
fn reservation_bytes(bits_per_second: u64, duration_ms: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    let estimate = estimated_bytes(bits_per_second, duration_ms);
    estimate
        .saturating_add(estimate.div_ceil(4))
        .saturating_add(16 * MIB)
        .div_ceil(MIB)
        .saturating_mul(MIB)
}
/// What probing one FFmpeg found: remembered for the process and, beside a
/// shared extracted copy, for the rest of the boot. A missing, corrupt or
/// older file is ignored and the probes run again.
#[derive(Default, Serialize, Deserialize)]
struct Probes {
    #[serde(default)]
    boot: String,
    /// `ffmpeg -encoders` output.
    #[serde(default)]
    listing: Option<String>,
    /// Test encodes by encoder, input and size, such as `h264_nvenc yuv 1920x1080`.
    #[serde(default)]
    encoders: std::collections::BTreeMap<String, Found>,
    /// Whether any decoding acceleration initialized.
    #[serde(default)]
    hwaccel: Option<Found>,
}
/// A probe's answer and when it was found, in UTC Unix milliseconds.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Found {
    works: bool,
    at_ms: u64,
}
/// How long a failed probe is believed: a busy device, for example one with
/// all its encode sessions in use, can refuse once and work soon after.
const FAILURE_KEPT: Duration = Duration::from_secs(300);
impl Found {
    fn now(works: bool) -> Self {
        Self {
            works,
            at_ms: crate::scheduling::now_ms(),
        }
    }
    /// The answer, unless it is a failure found too long ago.
    fn current(self) -> Option<bool> {
        let age = crate::scheduling::now_ms().checked_sub(self.at_ms);
        (self.works || age.is_some_and(|age| age < FAILURE_KEPT.as_millis() as u64))
            .then_some(self.works)
    }
    /// The later of two answers.
    fn newer(self, other: Option<Self>) -> Self {
        other
            .filter(|other| other.at_ms > self.at_ms)
            .unwrap_or(self)
    }
}
static PROBES: OnceLock<Mutex<std::collections::HashMap<PathBuf, Probes>>> = OnceLock::new();
fn with_probes<T>(exe: &Executable, read: impl FnOnce(&mut Probes) -> T) -> T {
    let mut memo = PROBES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    read(
        memo.entry(exe.path.clone())
            .or_insert_with(|| load_probes(exe)),
    )
}
/// A remembered probe answer of `exe`, or the answer of running the probe.
/// A probe that cannot tell, for example because it timed out, returns
/// `None`, which is not remembered.
fn probed(
    exe: &Executable,
    get: impl FnOnce(&Probes) -> Option<Found>,
    set: impl FnOnce(&mut Probes, Found),
    run: impl FnOnce() -> Result<Option<bool>>,
) -> Result<Option<bool>> {
    if let Some(works) = with_probes(exe, |probes| get(probes)).and_then(Found::current) {
        return Ok(Some(works));
    }
    // Probe unlocked: a test encode can take seconds.
    let works = run()?;
    if let Some(works) = works {
        remember(exe, |probes| set(probes, Found::now(works)));
    }
    Ok(works)
}
fn remember(exe: &Executable, change: impl FnOnce(&mut Probes)) {
    with_probes(exe, |probes| {
        change(probes);
        save_probes(exe, probes);
    })
}
fn load_probes(exe: &Executable) -> Probes {
    let stored = exe
        .probes
        .as_ref()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Probes>(&bytes).ok());
    stored
        .filter(|probes| boot_id().is_some_and(|boot| boot == probes.boot))
        .unwrap_or_default()
}
fn save_probes(exe: &Executable, probes: &mut Probes) {
    let (Some(path), Some(boot)) = (&exe.probes, boot_id()) else {
        return;
    };
    // Keep what other processes found since this one read the file.
    let stored = load_probes(exe);
    probes.boot = boot.to_owned();
    probes.listing = probes.listing.take().or(stored.listing);
    probes.hwaccel = probes
        .hwaccel
        .map(|found| found.newer(stored.hwaccel))
        .or(stored.hwaccel);
    for (key, found) in stored.encoders {
        let mine = probes.encoders.entry(key).or_insert(found);
        *mine = mine.newer(Some(found));
    }
    let Ok(bytes) = serde_json::to_vec(probes) else {
        return;
    };
    let Ok(nonce) = nonce() else {
        return;
    };
    let temporary = path.with_extension(format!("{nonce}.tmp"));
    if write_new(&temporary, &bytes, false).is_err() || fs::rename(&temporary, path).is_err() {
        let _ = fs::remove_file(&temporary);
    }
}
/// Identifies this boot, so remembered probes do not outlive a driver or
/// hardware change. Without one, probes are remembered by the process only.
fn boot_id() -> Option<&'static str> {
    static BOOT: OnceLock<Option<String>> = OnceLock::new();
    BOOT.get_or_init(|| {
        let id = if cfg!(target_os = "linux") {
            fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()
        } else if cfg!(target_os = "macos") {
            Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.boottime"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            None
        };
        id.map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
    })
    .as_deref()
}
fn encoder_listing(exe: &Executable) -> Result<String> {
    if let Some(listing) = with_probes(exe, |probes| probes.listing.clone()) {
        return Ok(listing);
    }
    let (ok, listing) = bounded_output(
        exe,
        &["-hide_banner".into(), "-encoders".into()],
        Duration::from_secs(5),
    )?;
    ensure!(ok, "FFmpeg could not list its encoders");
    remember(exe, |probes| probes.listing = Some(listing.clone()));
    Ok(listing)
}
fn listed(listing: &str, encoder: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(encoder))
}
fn probe_key(encoder: &str, input: EncoderInput, width: u32, height: u32) -> String {
    let input = match input {
        EncoderInput::Yuv => "yuv",
        EncoderInput::Rgb => "rgb",
    };
    format!("{encoder} {input} {width}x{height}")
}
/// One test frame at the recording's size: hardware encoders have size
/// limits that a small probe would pass. A probe that timed out counts as a
/// failure for this choice only.
fn encoder_works(
    exe: &Executable,
    encoder: &str,
    input: EncoderInput,
    width: u32,
    height: u32,
) -> Result<bool> {
    let key = probe_key(encoder, input, width, height);
    let works = probed(
        exe,
        |probes| probes.encoders.get(&key).copied(),
        |probes, found| {
            probes.encoders.insert(key.clone(), found);
        },
        || probe_encoder(exe, encoder, input, width, height),
    )?;
    Ok(works.unwrap_or(false))
}
fn jetson_works(exe: &Executable, encoder: &str) -> Result<bool> {
    let key = format!("{encoder} gstreamer");
    let works = probed(
        exe,
        |probes| probes.encoders.get(&key).copied(),
        |probes, found| {
            probes.encoders.insert(key.clone(), found);
        },
        || Ok(probe_jetson(encoder)),
    )?;
    Ok(works.unwrap_or(false))
}
/// The encoder is in this FFmpeg build, or is a Jetson GStreamer element,
/// and encodes a test frame of this size.
fn usable(exe: &Executable, encoder: &str, width: u32, height: u32) -> Result<bool> {
    if jetson_encoder(encoder) {
        return jetson_works(exe, encoder);
    }
    // One that just failed the small check frame has no device here.
    let small = probe_key(encoder, EncoderInput::Yuv, PROBE_SIZE.0, PROBE_SIZE.1);
    let failed = with_probes(exe, |probes| probes.encoders.get(&small).copied())
        .and_then(Found::current)
        == Some(false);
    Ok(listed(&encoder_listing(exe)?, encoder)
        && !failed
        && encoder_works(exe, encoder, EncoderInput::Yuv, width, height)?)
}
/// The size probed before a recording's first frame says its real size.
const PROBE_SIZE: (u32, u32) = (640, 480);
/// Encoders to try for a codec on this OS: vendor encoders first, then
/// cross-vendor Vulkan Video, then software.
fn encoder_candidates(family: &str) -> Vec<&'static str> {
    match (family, std::env::consts::OS) {
        ("h264", "macos") => vec!["h264_videotoolbox", "libx264"],
        ("hevc", "macos") => vec!["hevc_videotoolbox", "libx265"],
        ("av1", "macos") => vec!["libsvtav1", "libaom-av1"],
        ("h264", "linux") => vec![
            "h264_nvenc",
            "nvv4l2h264enc",
            "h264_amf",
            "h264_qsv",
            "h264_vaapi",
            "h264_vulkan",
            "h264_v4l2m2m",
            "libx264",
        ],
        ("hevc", "linux") => vec![
            "hevc_nvenc",
            "nvv4l2h265enc",
            "hevc_amf",
            "hevc_qsv",
            "hevc_vaapi",
            "hevc_vulkan",
            "hevc_v4l2m2m",
            "libx265",
        ],
        ("av1", "linux") => vec![
            "av1_nvenc",
            "nvv4l2av1enc",
            "av1_amf",
            "av1_qsv",
            "av1_vaapi",
            "av1_vulkan",
            "libsvtav1",
            "libaom-av1",
        ],
        ("h264", "windows") => vec![
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "h264_vulkan",
            "h264_mf",
            "libx264",
        ],
        ("hevc", "windows") => vec![
            "hevc_nvenc",
            "hevc_qsv",
            "hevc_amf",
            "hevc_vulkan",
            "hevc_mf",
            "libx265",
        ],
        ("av1", "windows") => vec![
            "av1_nvenc",
            "av1_qsv",
            "av1_amf",
            "av1_vulkan",
            "av1_mf",
            "libsvtav1",
            "libaom-av1",
        ],
        ("h264", _) => vec!["libx264"],
        ("hevc", _) => vec!["libx265"],
        _ => vec!["av1_nvenc", "libsvtav1", "libaom-av1"],
    }
}
/// Every candidate must be in the FFmpeg build and pass a real test encode
/// at the recording's size before it is chosen. `skip` holds encoders that
/// already failed this recording.
fn select_encoder(
    exe: &Executable,
    config: &ForwardConfig,
    (width, height): (u32, u32),
    skip: &[String],
) -> Result<String> {
    let family = codec_family(&config.codec)?;
    if config.encoder != "auto" {
        validate_encoder(&config.codec, &config.encoder)?;
        if jetson_encoder(&config.encoder) {
            ensure!(
                jetson_works(exe, &config.encoder)?,
                "forwarding encoder {} is unavailable or cannot encode on this device",
                config.encoder
            );
        } else {
            validate_encoder_listing(&encoder_listing(exe)?, &config.encoder, family)?;
        }
        return Ok(config.encoder.clone());
    }
    for candidate in encoder_candidates(family) {
        if !skip.iter().any(|failed| failed == candidate) && usable(exe, candidate, width, height)?
        {
            return Ok(candidate.into());
        }
    }
    bail!("{}", unsupported_codec(exe, family, Some((width, height))))
}
/// Before a recording starts, refuse a codec no encoder here can produce.
fn check_codec(config: &ForwardConfig) -> Result<()> {
    // Without FFmpeg, or when probing itself fails, the first frame reports why.
    let Ok(exe) = executable() else {
        return Ok(());
    };
    let family = codec_family(&config.codec)?;
    if config.encoder != "auto"
        || encoder_candidates(family)
            .iter()
            .any(|candidate| usable(&exe, candidate, PROBE_SIZE.0, PROBE_SIZE.1).unwrap_or(true))
    {
        return Ok(());
    }
    bail!("{}", unsupported_codec(&exe, family, None))
}
/// Says which codecs do work, since the bundled FFmpeg has no software HEVC
/// or AV1 encoder and many hosts have no hardware one either.
fn unsupported_codec(exe: &Executable, family: &str, size: Option<(u32, u32)>) -> String {
    let working = ["h264", "hevc", "av1"]
        .into_iter()
        .filter(|&other| {
            other != family
                && encoder_candidates(other).iter().any(|candidate| {
                    usable(exe, candidate, PROBE_SIZE.0, PROBE_SIZE.1).unwrap_or(false)
                })
        })
        .collect::<Vec<_>>();
    let at = size.map_or_else(String::new, |(width, height)| {
        format!(" at {width}x{height}")
    });
    format!(
        "{family} recording{at} is not supported on this host: no {family} encoder in FFmpeg works here; working codecs: {}. Choose another codec, or an installed encoder with --encoder",
        if working.is_empty() {
            "none".to_owned()
        } else {
            working.join(", ")
        }
    )
}

fn validate_encoder_listing(listing: &str, encoder: &str, family: &str) -> Result<()> {
    let line = listing.lines().find(|line| line.split_whitespace().nth(1) == Some(encoder))
        .with_context(|| format!("forwarding encoder {encoder} is unavailable in this FFmpeg build; use auto or choose an installed encoder"))?;
    ensure!(
        line.split_whitespace()
            .next()
            .is_some_and(|flags| flags.starts_with('V')),
        "forwarding encoder {encoder} must be a video encoder"
    );
    let declared = line
        .split_once("(codec ")
        .and_then(|(_, suffix)| suffix.split_once(')').map(|(codec, _)| codec));
    let declared = declared.or_else(|| encoder_family(encoder));
    ensure!(
        declared == Some(family),
        "forwarding encoder {encoder} does not encode the selected {family} codec; choose a matching encoder or change --codec"
    );
    Ok(())
}
/// NVIDIA Jetson hardware encoders, reached through NVIDIA's GStreamer
/// elements: on Jetson the video engine is a V4L2 device that FFmpeg's NVENC
/// and generic V4L2 wrappers cannot drive. GStreamer runs as a separate
/// process that encodes to MPEG-TS for FFmpeg to remux and deliver.
fn jetson_encoder(encoder: &str) -> bool {
    matches!(encoder, "nvv4l2h264enc" | "nvv4l2h265enc" | "nvv4l2av1enc")
}
fn jetson_parser(encoder: &str) -> &'static str {
    match encoder {
        "nvv4l2h264enc" => "h264parse",
        "nvv4l2h265enc" => "h265parse",
        _ => "av1parse",
    }
}

/// GStreamer fraction fields use signed 32-bit numerator and denominator.
/// Continued fractions recover rates such as 30000/1001 without rounding to
/// decimal milliseconds. Accept a rate only if the fraction reproduces the
/// requested floating-point value exactly.
fn jetson_framerate(fps: f64) -> Result<(u32, u32)> {
    ensure!(
        fps.is_finite() && (0.1..=240.0).contains(&fps),
        "forwarding FPS must be a finite number within 0.1..240"
    );
    const LIMIT: u64 = i32::MAX as u64;
    let (mut previous_numerator, mut numerator) = (0u64, 1u64);
    let (mut previous_denominator, mut denominator) = (1u64, 0u64);
    let mut remainder = fps;
    for _ in 0..64 {
        let coefficient = remainder.floor();
        let numerator_limit = (LIMIT - previous_numerator)
            .checked_div(numerator)
            .unwrap_or(LIMIT);
        let denominator_limit = (LIMIT - previous_denominator)
            .checked_div(denominator)
            .unwrap_or(LIMIT);
        let maximum = numerator_limit.min(denominator_limit);
        let multiplier = if coefficient <= maximum as f64 {
            coefficient as u64
        } else {
            maximum
        };
        let next_numerator = multiplier * numerator + previous_numerator;
        let next_denominator = multiplier * denominator + previous_denominator;
        if next_denominator > 0 && next_numerator as f64 / next_denominator as f64 == fps {
            return Ok((next_numerator as u32, next_denominator as u32));
        }
        if multiplier as f64 != coefficient {
            break;
        }
        let fraction = remainder - coefficient;
        if fraction <= 0.0 {
            break;
        }
        remainder = 1.0 / fraction;
        (previous_numerator, numerator) = (numerator, next_numerator);
        (previous_denominator, denominator) = (denominator, next_denominator);
    }
    bail!(
        "Jetson forwarding FPS {fps} cannot be represented exactly with 32-bit fraction fields; choose a simpler frame rate or a different encoder"
    )
}
/// Run a helper tool briefly: whether it succeeded, or `None` when it had to
/// be stopped at the deadline.
fn tool_succeeds(program: &str, args: &[&str], timeout: Duration) -> Option<bool> {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Some(false);
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
/// The element exists and encodes a short test clip on this device; Orin,
/// for example, lists the AV1 element but has no AV1 encoder.
fn probe_jetson(encoder: &str) -> Option<bool> {
    if !cfg!(target_os = "linux")
        || !tool_succeeds("gst-inspect-1.0", &[encoder], Duration::from_secs(5))?
    {
        return Some(false);
    }
    tool_succeeds(
        "gst-launch-1.0",
        &[
            "-q",
            "videotestsrc",
            "num-buffers=3",
            "!",
            "video/x-raw,width=640,height=480,format=I420",
            "!",
            "nvvidconv",
            "!",
            "video/x-raw(memory:NVMM),format=NV12",
            "!",
            encoder,
            "!",
            jetson_parser(encoder),
            "!",
            "fakesink",
        ],
        Duration::from_secs(10),
    )
}
/// Bits per second from a bitrate like `4M`, `2500k` or `800000`.
fn bits_per_second(bitrate: &str) -> Result<u64> {
    ensure!(
        !bitrate.is_empty() && bitrate.len() <= 32,
        "forwarding bitrate needs a positive bit rate, for example 4M or 2500k"
    );
    let (number, power) = match bitrate.chars().last() {
        Some('k' | 'K') => (&bitrate[..bitrate.len() - 1], 3u32),
        Some('M') => (&bitrate[..bitrate.len() - 1], 6),
        Some('G') => (&bitrate[..bitrate.len() - 1], 9),
        _ => (bitrate, 0),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    ensure!(
        !number.is_empty()
            && number.bytes().any(|b| b.is_ascii_digit())
            && whole
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit()),
        "invalid forwarding bitrate; use a number with an optional k, M or G suffix (for example 4M)"
    );
    let fraction = fraction.trim_end_matches('0');
    ensure!(
        fraction.len() <= power as usize,
        "forwarding bitrate must resolve to a whole number of bits per second"
    );
    let whole = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .context("forwarding bitrate is too large")?
    };
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>()? * 10u64.pow(power - fraction.len() as u32)
    };
    let value = whole
        .checked_mul(10u64.pow(power))
        .and_then(|whole| whole.checked_add(fractional));
    ensure!(
        value.is_some_and(|value| (1..=u32::MAX as u64).contains(&value)),
        "forwarding bitrate must be within 1..4294967295 bits per second"
    );
    Ok(value.unwrap())
}
/// Frames handed to a Jetson encoder: formats `nvvidconv` accepts in system
/// memory and converts on the VIC engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JetsonInput {
    /// Gray as the luma plane with neutral chroma.
    I420,
    Rgba,
}
impl JetsonInput {
    fn caps(self) -> &'static str {
        match self {
            Self::I420 => "i420",
            Self::Rgba => "rgba",
        }
    }
    fn bytes(self, frame: &Frame) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Rgba => crate::frame::rgba(frame)?,
            Self::I420 => {
                let mut data = crate::frame::convert(frame, |[gray, _, _]| gray)?;
                let chroma =
                    (frame.width as usize).div_ceil(2) * (frame.height as usize).div_ceil(2);
                data.resize(data.len() + 2 * chroma, 128);
                data
            }
        })
    }
}
/// Pixel layout handed to the encoder after FFmpeg's input conversion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EncoderInput {
    /// Planar or semi-planar 4:2:0 converted by FFmpeg's SIMD swscale.
    Yuv,
    /// Packed RGB; the encoder's own color-conversion hardware makes YUV.
    Rgb,
}
/// NVENC and AMF accept packed RGB and convert it to YUV on the GPU, which
/// keeps that matrix multiply off the CPU for color sources.
fn hardware_rgb_input(encoder: &str) -> bool {
    encoder.ends_with("_nvenc") || encoder.ends_with("_amf")
}
/// Choose the encoder input for a source. RGB input is used only when the
/// encoder accepts it on this machine, so a driver without it keeps the YUV
/// path instead of falling back to software encoding.
fn encoder_input(
    exe: &Executable,
    encoder: &str,
    color: bool,
    (width, height): (u32, u32),
) -> Result<EncoderInput> {
    if color
        && hardware_rgb_input(encoder)
        && encoder_works(exe, encoder, EncoderInput::Rgb, width, height)?
    {
        Ok(EncoderInput::Rgb)
    } else {
        Ok(EncoderInput::Yuv)
    }
}
fn probe_encoder(
    exe: &Executable,
    candidate: &str,
    input: EncoderInput,
    width: u32,
    height: u32,
) -> Result<Option<bool>> {
    let mut args = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        &format!("color=size={width}x{height}:rate=1"),
        "-frames:v",
        "1",
        "-c:v",
        candidate,
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    args.extend(encoder_device_options(candidate));
    args.extend(encoder_output_options(candidate, input));
    args.extend(["-f".into(), "null".into(), "-".into()]);
    // Large sensors take longer to encode even once.
    let pixels = width as u64 * height as u64;
    let timeout = Duration::from_secs(3) + Duration::from_millis(pixels / 10_000);
    let (works, _, timed_out) = timed_output(exe, &args, &[], timeout)?;
    Ok((!timed_out).then_some(works))
}
fn encoder_device_options(encoder: &str) -> Vec<String> {
    if encoder.ends_with("_vaapi") {
        vec![
            "-vaapi_device".into(),
            std::env::var("CAPTUREFAB_VAAPI_DEVICE")
                .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
        ]
    } else if encoder.ends_with("_vulkan") {
        // The first Vulkan device with video encode; FFmpeg loads the system
        // Vulkan loader at run time.
        ["-init_hw_device", "vulkan=vk", "-filter_hw_device", "vk"]
            .map(str::to_owned)
            .to_vec()
    } else {
        Vec::new()
    }
}
fn encoder_output_options(encoder: &str, input: EncoderInput) -> Vec<String> {
    let mut options = if encoder.ends_with("_vaapi") || encoder.ends_with("_vulkan") {
        // Convert and pad on the CPU, then upload into the device's frames.
        vec![
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2,format=nv12,hwupload",
            "-pix_fmt",
            if encoder.ends_with("_vaapi") {
                "vaapi"
            } else {
                "vulkan"
            },
        ]
    } else {
        vec![
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-pix_fmt",
            if input == EncoderInput::Rgb {
                "bgr0"
            } else if encoder.ends_with("_qsv")
                || encoder.ends_with("_mf")
                || encoder.ends_with("_videotoolbox")
            {
                // The native surface layout of these encoders: no repacking copy.
                "nv12"
            } else {
                "yuv420p"
            },
        ]
    }
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if encoder.ends_with("_videotoolbox") {
        options.extend(["-allow_sw", "0"].map(str::to_owned));
    }
    if encoder.ends_with("_mf") {
        options.extend(["-hw_encoding", "1"].map(str::to_owned));
    }
    options
}
/// Check a forwarding URL or recording path without opening an encoder.
pub fn validate_forward_output(output: &str) -> Result<()> {
    output_args(output)?;
    recording_output(output)?;
    Ok(())
}

fn output_args(output: &str) -> Result<Vec<String>> {
    ensure!(
        !output.is_empty() && output.len() <= 16384 && !output.chars().any(char::is_control),
        "invalid forwarding destination"
    );
    if !is_recording(output) {
        validate_network_url(output, "forwarding destination URL")?;
    }
    let mut args = Vec::new();
    match scheme(output).as_deref() {
        Some("rtsp" | "rtsps") => {
            args.extend(["-f", "rtsp", "-rtsp_transport", "tcp"].map(str::to_string))
        }
        Some("rtmp" | "rtmps") => args.extend(["-f", "flv"].map(str::to_string)),
        Some("srt" | "udp" | "tcp") => args.extend(["-f", "mpegts"].map(str::to_string)),
        Some("http" | "https") => args.extend(["-f", "mpegts"].map(str::to_string)),
        Some("file") | None => {
            ensure!(
                !output.to_ascii_lowercase().ends_with(".m3u8"),
                "unsupported local HLS output: use MediaMTX for HLS or record a bounded .mkv/.mp4 file"
            );
        }
        Some(other) => bail!("unsupported forwarding destination protocol {other}"),
    }
    let destination = if scheme(output).is_none() && output.starts_with('-') {
        format!("file:{output}")
    } else {
        canonical_scheme(output)
    };
    args.push(destination);
    Ok(args)
}
fn recording_output(output: &str) -> Result<Option<(PathBuf, &'static str)>> {
    if !is_recording(output) {
        return Ok(None);
    }
    let path = local_file_path(output)?;
    ensure!(
        !path.as_os_str().is_empty()
            && path != std::path::Path::new("-")
            && !output.ends_with('/')
            && !(cfg!(windows) && output.ends_with('\\'))
            && path.file_name().is_some(),
        "recording needs an output file"
    );
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mkv")
        .to_ascii_lowercase();
    let container = match extension.as_str() {
        "mp4" | "mov" => "mp4",
        "ts" | "mpegts" => "mpegts",
        "webm" => "webm",
        "mkv" | "matroska" => "matroska",
        _ => bail!("unsupported recording container: use .mkv, .mp4, .mov, .ts or .webm"),
    };
    Ok(Some((path, container)))
}

pub fn local_file_path(value: &str) -> Result<PathBuf> {
    if scheme(value).as_deref() != Some("file") {
        return Ok(value.into());
    }
    let path = value.split_once(':').unwrap().1;
    ensure!(!path.is_empty(), "file URL needs a path");
    if let Some(rest) = path.strip_prefix("//") {
        ensure!(
            !rest.contains(['?', '#']),
            "file URL cannot contain a query or fragment; percent-encode filename characters or use a plain path"
        );
        let (authority, path) = rest
            .split_once('/')
            .context("file URL needs a local file path")?;
        ensure!(
            authority.is_empty() || authority.eq_ignore_ascii_case("localhost") || cfg!(windows),
            "file URL host must be localhost; use a mounted network folder for other hosts"
        );
        let path = decode_file_url_path(path)?;
        if authority.is_empty() || authority.eq_ignore_ascii_case("localhost") {
            if cfg!(windows)
                && path.len() >= 3
                && path.as_bytes()[0].is_ascii_alphabetic()
                && path.as_bytes()[1] == b':'
                && path.as_bytes()[2] == b'/'
            {
                return Ok(path.into());
            }
            return Ok(PathBuf::from(format!("/{path}")));
        }
        return Ok(PathBuf::from(format!("//{authority}/{path}")));
    }
    Ok(path.into())
}

fn decode_file_url_path(path: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(path.len());
    let mut source = path.as_bytes().iter();
    while let Some(&byte) = source.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = source.next().and_then(|b| (*b as char).to_digit(16));
        let low = source.next().and_then(|b| (*b as char).to_digit(16));
        let (Some(high), Some(low)) = (high, low) else {
            bail!("file URL path contains invalid percent escaping");
        };
        bytes.push((high * 16 + low) as u8);
    }
    let decoded = String::from_utf8(bytes).context("file URL path must be valid UTF-8")?;
    ensure!(
        !decoded.chars().any(char::is_control),
        "file URL path must not contain encoded control characters"
    );
    Ok(decoded)
}
/// How a camera frame is handed to FFmpeg: its rawvideo pixel format, bytes
/// per pixel and whether it carries color. Sending sensor-native bytes
/// avoids a CPU conversion on the camera thread and, for mono and Bayer, moves
/// a third of the bytes RGB24 would through the pipe. FFmpeg reads no packed
/// samples and no 10- or 12-bit Bayer, so those are unpacked and widened to
/// its 16-bit formats first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawInput {
    pixel_format: u32,
    ffmpeg: &'static str,
    bytes_per_pixel: usize,
    color: bool,
    /// Packed samples, unpacked before the pipe.
    unpack: bool,
    /// Bits each 16-bit sample is shifted up to span the full range.
    widen: u32,
    /// Full-range BT.601 Y'CbCr, as PFNC defines its YUV and YCbCr formats.
    full_range: bool,
}
impl RawInput {
    fn for_format(pixel_format: u32) -> Result<Self> {
        let unpacked = crate::frame::packing(pixel_format).map_or(pixel_format, |(v, ..)| v);
        let (ffmpeg, bytes_per_pixel, color) = match unpacked {
            crate::types::MONO8 => ("gray", 1, false),
            0x0110_0003 => ("gray10le", 2, false),
            0x0110_0005 => ("gray12le", 2, false),
            0x0110_0025 => ("gray14le", 2, false),
            0x0110_0007 => ("gray16le", 2, false),
            crate::types::RGB8 => ("rgb24", 3, true),
            0x0218_0015 => ("bgr24", 3, true),
            0x0220_0016 => ("rgba", 4, true),
            0x0220_0017 => ("bgra", 4, true),
            0x0210_001f | 0x0210_0043 => ("uyvy422", 2, true),
            0x0210_0032 | 0x0210_003b => ("yuyv422", 2, true),
            0x0108_0008 => ("bayer_grbg8", 1, true),
            0x0108_0009 => ("bayer_rggb8", 1, true),
            0x0108_000a => ("bayer_gbrg8", 1, true),
            0x0108_000b => ("bayer_bggr8", 1, true),
            0x0110_000c | 0x0110_0010 | 0x0110_002e => ("bayer_grbg16le", 2, true),
            0x0110_000d | 0x0110_0011 | 0x0110_002f => ("bayer_rggb16le", 2, true),
            0x0110_000e | 0x0110_0012 | 0x0110_0030 => ("bayer_gbrg16le", 2, true),
            0x0110_000f | 0x0110_0013 | 0x0110_0031 => ("bayer_bggr16le", 2, true),
            _ => bail!(
                "unsupported PFNC format 0x{pixel_format:08x}; capture with --format raw to preserve bytes"
            ),
        };
        let widen = match unpacked {
            0x0110_000c..=0x0110_000f => 6,
            0x0110_0010..=0x0110_0013 => 4,
            _ => 0,
        };
        Ok(Self {
            pixel_format,
            ffmpeg,
            bytes_per_pixel,
            color,
            unpack: unpacked != pixel_format,
            widen,
            full_range: matches!(ffmpeg, "uyvy422" | "yuyv422"),
        })
    }
    /// YUV 4:2:2, where each two pixels share one Cb and one Cr.
    fn pairs(&self) -> bool {
        matches!(self.ffmpeg, "uyvy422" | "yuyv422")
    }
    /// FFmpeg input options for the frames' color range and matrix.
    fn color_options(&self) -> &'static [&'static str] {
        if self.full_range {
            &["-color_range", "pc", "-colorspace", "bt470bg"]
        } else {
            &[]
        }
    }
    /// The bytes FFmpeg reads for `frame`; the sensor buffer itself unless
    /// its samples are packed or widened, or its 4:2:2 rows end in half a
    /// pixel pair.
    fn bytes(&self, frame: Frame) -> Result<Vec<u8>> {
        let (width, height) = (frame.width as usize, frame.height as usize);
        let length = width * height * self.bytes_per_pixel;
        let name = crate::frame::pixel_format_name(frame.pixel_format);
        let mut data = if self.unpack {
            crate::frame::unpacked(&frame)?.into_owned().data
        } else {
            frame.data
        };
        ensure!(data.len() >= length, "truncated {name} frame");
        data.truncate(length);
        if self.widen > 0 {
            for sample in data.as_chunks_mut::<2>().0 {
                let value = u16::from_le_bytes(*sample).min(u16::MAX >> self.widen);
                *sample = (value << self.widen).to_le_bytes();
            }
        }
        if self.pairs() && !width.is_multiple_of(2) {
            // A camera ends each odd-width 4:2:2 row with a lone pixel and its
            // Cb, but FFmpeg reads rows of whole pairs, 4 * ceil(width / 2)
            // bytes. Each row gets the missing luma, which lies outside the
            // image, and a neutral Cr, as `frame::unpacked` decodes that pixel.
            let row = width * 2;
            let mut padded = Vec::with_capacity((row + 2) * height);
            for line in data.chunks_exact(row) {
                padded.extend_from_slice(line);
                padded.extend_from_slice(&[128, 128]);
            }
            data = padded;
        }
        Ok(data)
    }
}
impl Forwarder {
    pub fn new(mut config: ForwardConfig) -> Result<Self> {
        config.validate()?;
        check_codec(&config)?;
        let destination = match config.destination.as_deref() {
            Some(name) => {
                ensure!(
                    recording_output(&config.output)?.is_some(),
                    "a destination holds recordings, not network streams; omit --destination for {}",
                    redact_url(&config.output)
                );
                let resolved = crate::destination::resolve(Some(name), &config.output)?;
                config.output = resolved.output.clone();
                Some(resolved)
            }
            None => None,
        };
        Ok(Self {
            config,
            process: None,
            final_stats: ForwardStats::default(),
            destination,
            reserved: None,
            failed: None,
        })
    }
    /// Use an output the caller already resolved against `destination`
    /// (and named), so the destination is not looked up a second time.
    /// `reserved` is the already created recording file, if any.
    pub fn with_resolved(
        config: ForwardConfig,
        destination: Option<crate::destination::Resolved>,
        reserved: Option<crate::storage::StorageWriter>,
    ) -> Result<Self> {
        if let Err(error) = config.validate().and_then(|()| check_codec(&config)) {
            if let Some(reserved) = reserved {
                reserved.abort().context("remove unused recording output")?;
            }
            return Err(error);
        }
        Ok(Self {
            config,
            process: None,
            final_stats: ForwardStats::default(),
            destination,
            reserved,
            failed: None,
        })
    }
    /// The local file this forwarder records to, if it records.
    pub fn recording_path(&self) -> Option<PathBuf> {
        recording_output(&self.config.output)
            .ok()
            .flatten()
            .map(|(path, _)| path)
    }
    fn spawn(&mut self, width: u32, height: u32, raw: RawInput) -> Result<()> {
        ensure!(
            width > 0 && height > 0 && width as usize * height as usize <= MAX_FRAME / 3,
            "invalid forwarding dimensions"
        );
        let recording = recording_output(&self.config.output)?;
        let local = recording.is_some();
        let exe = executable()?;
        let skip = self.failed.as_slice();
        let encoder = select_encoder(&exe, &self.config, (width, height), skip)?;
        let jetson = jetson_encoder(&encoder).then_some(if raw.color {
            JetsonInput::Rgba
        } else {
            JetsonInput::I420
        });
        let fps = self.config.fps.to_string();
        let mut cmd = command(&exe);
        let mut stage = None;
        let mut active = Active::NONE;
        if let Some(input) = jetson {
            let (fps_numerator, fps_denominator) = jetson_framerate(self.config.fps)?;
            let gop = ((self.config.fps * 2.0).round().max(1.0) as u32).to_string();
            let mut gst = Command::new("gst-launch-1.0");
            gst.args([
                "-q",
                "fdsrc",
                "fd=0",
                "!",
                "rawvideoparse",
                &format!("width={width}"),
                &format!("height={height}"),
                &format!("format={}", input.caps()),
                &format!("framerate={fps_numerator}/{fps_denominator}"),
                "!",
                "nvvidconv",
                "!",
                "video/x-raw(memory:NVMM),format=NV12",
                "!",
                &encoder,
                &format!("bitrate={}", bits_per_second(&self.config.bitrate)?),
                &format!("iframeinterval={gop}"),
                &format!("idrinterval={gop}"),
                "insert-sps-pps=true",
                "!",
                jetson_parser(&encoder),
                "config-interval=-1",
                "!",
                "mpegtsmux",
                "!",
                "fdsink",
                "fd=1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            let mut child = gst
                .spawn()
                .context("cannot launch GStreamer Jetson encoder")?;
            let encoded = child.stdout.take().context("GStreamer stdout")?;
            cmd.stdin(Stdio::from(encoded));
            stage = Some(child);
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-f",
                "mpegts",
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                "copy",
            ]);
        } else {
            let input = encoder_input(&exe, &encoder, raw.color, (width, height))?;
            cmd.args(encoder_device_options(&encoder));
            // Each frame is stamped when FFmpeg reads it and keeps that time
            // in the output, so the file lasts as long as the recording did
            // even when frames arrive late, slower than `fps`, or not at all
            // for a while. Repeating frames instead would make FFmpeg encode
            // a whole outage's worth of copies at once when frames return.
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-f",
                "rawvideo",
                "-pix_fmt",
                raw.ffmpeg,
                "-video_size",
                &format!("{width}x{height}"),
                "-framerate",
                &fps,
                "-use_wallclock_as_timestamps",
                "1",
            ]);
            cmd.args(raw.color_options());
            cmd.args([
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                &encoder,
                "-b:v",
                &self.config.bitrate,
                "-g",
                &((self.config.fps * 2.0).round().max(1.0) as u32).to_string(),
            ]);
            cmd.args(encoder_output_options(&encoder, input));
            cmd.args(["-fps_mode", "vfr"]);
            if matches!(encoder.as_str(), "libx264" | "libx265") {
                // Concurrent software recordings share the cores.
                let (claimed, recordings) = Active::claim("encode");
                active = claimed;
                let cores = (cores() / recordings).max(1);
                let preset = x264_preset(
                    width,
                    height,
                    self.config.fps,
                    cores,
                    crate::efficiency::efficient(),
                );
                cmd.args(["-preset", preset, "-threads", &cores.to_string()]);
                if encoder == "libx264" {
                    // Rate control by the frames' times rather than `fps`, so
                    // a camera slower than `fps` still gets the bitrate asked.
                    cmd.args(["-tune", "zerolatency", "-x264-params", "force-cfr=0"]);
                }
            } else if encoder.ends_with("_videotoolbox") {
                cmd.args(["-realtime", "1"]);
            }
        }
        if let Some((_, container)) = &recording {
            if *container == "mp4" {
                cmd.args(["-movflags", "frag_keyframe+empty_moov"]);
            }
            cmd.args(["-f", container, "pipe:1"]);
        } else {
            cmd.args(output_args(&self.config.output)?);
        }
        if stage.is_none() {
            cmd.stdin(Stdio::piped());
        }
        cmd.stdout(if local { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::piped());
        // Validate/probe before creating a file, then reserve the global budget.
        let storage = match self.reserved.take() {
            Some(reserved) => Some(reserved),
            None => recording
                .map(|(path, _)| {
                    crate::storage::create_writer(
                        &path,
                        &self.config.storage,
                        self.config.max_file_bytes,
                    )
                })
                .transpose()?,
        };
        let started_ms = crate::scheduling::now_ms();
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(mut stage) = stage {
                    let _ = stage.kill();
                    let _ = stage.wait();
                }
                if let Some(storage) = storage {
                    storage.abort().context("remove failed recording output")?;
                }
                return Err(error).context("cannot launch FFmpeg forwarder");
            }
        };
        let log = Log::new();
        // Frames go to the first process: GStreamer when it encodes, else FFmpeg.
        let (mut input, stage_log) = match stage.as_mut() {
            Some(stage) => (
                stage.stdin.take().context("GStreamer stdin")?,
                Some(log.collect(stage.stderr.take().context("GStreamer stderr")?)),
            ),
            None => (child.stdin.take().context("FFmpeg stdin")?, None),
        };
        let log_thread = log.collect(child.stderr.take().unwrap());
        let queue = Arc::new(WriterQueue::new());
        let output_thread = storage.map(|mut storage| {
            let mut output = child.stdout.take().expect("piped encoded output");
            let shared = queue.clone();
            thread::spawn(move || {
                let error = std::io::copy(&mut output, &mut storage)
                    .err()
                    .map(|error| format!("write bounded recording: {error}"));
                if let Some(error) = &error {
                    shared.fail(error.clone());
                    shared.output_failed.store(true, Ordering::Release);
                    shared.running.store(false, Ordering::Release);
                    shared.changed.notify_all();
                }
                // Only the owner observing the child's exit status may commit
                // a recording. EOF alone can also mean an encoder failure.
                EncodedOutput { storage, error }
            })
        });
        let shared = queue.clone();
        let interval = Duration::from_secs_f64(1.0 / self.config.fps);
        // FFmpeg stamps a recording's frames itself, so it gets each new
        // frame once, at most one per slot, and a stall leaves a gap.
        // GStreamer stamps frames by count, so the Jetson writer sends one on
        // every slot, repeating the last until the camera stalls. A stream's
        // receivers may drop a silent publisher, so it gets the last picture
        // on every slot throughout.
        let (repeat, through_stalls) = (jetson.is_some() || !local, !local);
        let writer = thread::spawn(move || {
            let mut next = Instant::now();
            let mut last = None;
            loop {
                let again = repeat && last.is_some() && (through_stalls || !shared.stalled());
                let Some(fresh) = shared.take(next, again) else {
                    break;
                };
                if fresh.is_some() {
                    last = fresh;
                }
                let Some(bytes) = &last else {
                    continue;
                };
                shared.busy.store(true, Ordering::Relaxed);
                let written = input.write_all(bytes);
                shared.busy.store(false, Ordering::Relaxed);
                if let Err(e) = written {
                    shared.fail(format!("FFmpeg input failed: {e}"));
                    break;
                }
                shared.written.fetch_add(1, Ordering::Relaxed);
                if !repeat {
                    last = None;
                }
                // Slots stay on a fixed grid; a writer that fell behind skips
                // the slots it missed instead of catching up in a burst.
                let now = Instant::now();
                next = (next + interval).max(now.checked_sub(interval).unwrap_or(now));
            }
            // Closing stdin lets FFmpeg flush its muxer and finalize files.
            drop(input);
            shared.running.store(false, Ordering::Release);
            shared.changed.notify_all();
        });
        self.process = Some(EncoderProcess {
            child,
            writer: Some(writer),
            output_thread,
            log_thread: Some(log_thread),
            queue,
            log,
            _exe: exe,
            width,
            height,
            raw,
            jetson,
            stage,
            stage_log,
            encoder,
            started_ms,
            spawned: Instant::now(),
            _active: active,
        });
        Ok(())
    }
    /// An automatically chosen encoder that exits within its first seconds
    /// of a local recording, before writing any video, usually rejected this
    /// frame size or ran out of hardware sessions. Its file is removed and
    /// the recording starts again, once, in a new file with the next
    /// candidate. One that had written video ends the recording and keeps
    /// it. A stream's early exit is more likely its server's doing, and a
    /// failed write the disk's.
    fn fall_back(&mut self, status: std::process::ExitStatus) -> Result<bool> {
        let Some(process) = self.process.as_ref() else {
            return Ok(false);
        };
        if status.success()
            || self.config.encoder != "auto"
            || self.failed.is_some()
            || process.output_thread.is_none()
            || process.queue.output_failed.load(Ordering::Acquire)
            || process.spawned.elapsed() > Duration::from_secs(10)
        {
            return Ok(false);
        }
        self.final_stats = self.stats();
        let mut process = self.process.take().expect("encoder process");
        let encoder = process.encoder.clone();
        let (width, height, raw) = (process.width, process.height, process.raw);
        let finished = finish(&mut process);
        match finished.1 {
            Some(output) if output.storage.bytes_written() <= HEADER_BYTES => output
                .storage
                .abort()
                .context("remove failed recording output")?,
            Some(_) => {
                self.settle(process, finished)?;
                bail!("FFmpeg forwarding exited ({status})");
            }
            None => {}
        }
        if let Ok(exe) = executable() {
            remember(&exe, |probes| {
                probes.encoders.insert(
                    probe_key(&encoder, EncoderInput::Yuv, width, height),
                    Found::now(false),
                );
            });
        }
        self.failed = Some(encoder);
        self.spawn(width, height, raw)?;
        Ok(true)
    }
    /// Queue a frame for encoding; it is owned so its buffer moves to the
    /// writer thread without a copy.
    pub fn push(&mut self, frame: Frame) -> Result<bool> {
        if self.process.is_none() {
            self.spawn(
                frame.width,
                frame.height,
                RawInput::for_format(frame.pixel_format)?,
            )?
        }
        let process = self.process.as_mut().unwrap();
        ensure!(
            frame.width == process.width && frame.height == process.height,
            "the camera's resolution changed from {}x{} to {}x{}; start a new recording to continue",
            process.width,
            process.height,
            frame.width,
            frame.height
        );
        ensure!(
            frame.pixel_format == process.raw.pixel_format,
            "the camera's pixel format changed; start a new recording to continue"
        );
        // An encoder that died at start leaves a broken pipe behind; falling
        // back to the next one comes first.
        if let Some(status) = process.child.try_wait()?
            && self.fall_back(status)?
        {
            return self.push(frame);
        }
        let process = self.process.as_mut().unwrap();
        let stored_error = process
            .queue
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone();
        if let Some(error) = stored_error {
            bail!("{error}: {}", process.log.text(&self.config.output).trim());
        }
        if let Some(status) = process.child.try_wait()? {
            bail!(
                "FFmpeg forwarding exited ({status}): {}",
                process.log.text(&self.config.output).trim()
            );
        }
        // FFmpeg converts on its own threads with SIMD swscale (or the encoder
        // hardware), so the sensor buffer itself is queued for the pipe.
        // Jetson's converter takes RGBA or I420, built here in one pass.
        let bytes = match process.jetson {
            Some(input) => input.bytes(&frame)?,
            None => process.raw.bytes(frame)?,
        };
        let queue = &process.queue;
        let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &state.error {
            bail!("{error}: {}", process.log.text(&self.config.output).trim());
        }
        ensure!(
            queue.running.load(Ordering::Acquire),
            "FFmpeg forwarding writer stopped"
        );
        let replaced = state.latest.replace(bytes).is_some();
        queue.accepted.fetch_add(1, Ordering::Relaxed);
        let arrived = queue.epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        queue.arrived_ms.store(arrived, Ordering::Relaxed);
        if replaced {
            queue.dropped.fetch_add(1, Ordering::Relaxed);
            if queue.busy.load(Ordering::Relaxed) {
                queue.late.fetch_add(1, Ordering::Relaxed);
            }
        }
        drop(state);
        queue.changed.notify_one();
        Ok(!replaced)
    }
    pub fn stats(&self) -> ForwardStats {
        if let Some(p) = &self.process {
            ForwardStats {
                accepted: p.queue.accepted.load(Ordering::Relaxed),
                written: p.queue.written.load(Ordering::Relaxed),
                dropped: p.queue.dropped.load(Ordering::Relaxed),
                late: p.queue.late.load(Ordering::Relaxed),
                stalled: p.queue.stalled(),
                encoder: p.encoder.clone(),
                error: p
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error
                    .clone(),
                recording: None,
            }
        } else {
            self.final_stats.clone()
        }
    }
    pub fn stop(&mut self) -> Result<()> {
        self.final_stats = self.stats();
        // A recording that never got a frame leaves no empty file behind.
        if let Some(reserved) = self.reserved.take() {
            reserved.abort().context("remove unused recording output")?;
        }
        let Some(mut process) = self.process.take() else {
            return Ok(());
        };
        let finished = finish(&mut process);
        self.settle(process, finished)
    }
    /// Keep or remove the recording of a finished encoder process, and say
    /// how it ended.
    fn settle(&mut self, process: EncoderProcess, finished: Finished) -> Result<()> {
        let (status, recording, mut error) = finished;
        self.final_stats.written = process.queue.written.load(Ordering::Relaxed);
        if let Some(output) = recording {
            // Video the encoder wrote stays even when it failed or had to be
            // stopped, or when the file reached its maximum size or a write
            // to it failed, since every recording container plays up to its
            // last complete part. The cause still ends the recording.
            let cut = output.storage.cut();
            let kept =
                (output.error.is_none() || cut) && output.storage.bytes_written() > HEADER_BYTES;
            if error.is_none() {
                error = output.error;
            }
            let valid = kept
                || status.as_ref().is_some_and(|status| status.success())
                    && error.is_none()
                    && output.storage.bytes_written() > 0;
            let path = output.storage.path().to_path_buf();
            let (bytes, sha256) = (output.storage.bytes_written(), output.storage.sha256());
            let result = if valid {
                output.storage.finish_cut().map(|()| {
                    // The complete local file is recorded in the storage ledger.
                    let recorded = crate::dataset::RecordedFile {
                        path: path.display().to_string(),
                        bytes,
                        sha256,
                        started_ms: process.started_ms,
                        ended_ms: crate::scheduling::now_ms(),
                        codec: self.config.codec.clone(),
                        encoder: process.encoder.clone(),
                        fps: self.config.fps,
                        destination: self
                            .destination
                            .as_ref()
                            .and_then(|d| d.destination.clone()),
                    };
                    self.final_stats.recording = Some(recorded);
                })
            } else {
                output.storage.abort()
            };
            if let Err(failure) = result {
                error.get_or_insert_with(|| format!("recording finalization failed: {failure:#}"));
            }
        }
        if error.is_none() {
            error = match status {
                Some(status) if status.success() => None,
                Some(status) => Some(format!(
                    "FFmpeg forwarding failed ({status}): {}",
                    process.log.text(&self.config.output).trim()
                )),
                None => {
                    Some("FFmpeg forwarding shutdown timed out; subprocess was terminated".into())
                }
            };
        }
        self.final_stats.error = error.clone();
        if let Some(error) = error {
            let log = process.log.text(&self.config.output);
            if log.trim().is_empty() {
                bail!("{error}");
            }
            bail!("{error}: {}", log.trim());
        }
        Ok(())
    }
}
/// How an encoder process ended: its exit status (`None` when it had to be
/// killed), the recording output its writer holds, and the first error any
/// of its threads reported.
type Finished = (
    Option<std::process::ExitStatus>,
    Option<EncodedOutput>,
    Option<String>,
);
/// Stop an encoder process and its threads.
fn finish(process: &mut EncoderProcess) -> Finished {
    process.queue.running.store(false, Ordering::Release);
    process.queue.changed.notify_all();
    // A GStreamer stage must drain the hardware encoder before FFmpeg sees EOF.
    let grace = if process.stage.is_some() { 6 } else { 3 };
    let deadline = Instant::now() + Duration::from_secs(grace);
    let mut shutdown_error = None;
    let status = loop {
        match process.child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                shutdown_error = Some(format!("cannot inspect FFmpeg forwarding process: {error}"));
                let _ = process.child.kill();
                let _ = process.child.wait();
                break None;
            }
        }
        if Instant::now() >= deadline {
            let _ = process.child.kill();
            let _ = process.child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    if let Some(mut stage) = process.stage.take() {
        // FFmpeg has exited; the encoder stage normally already has too.
        if !matches!(stage.try_wait(), Ok(Some(_))) {
            let _ = stage.kill();
        }
        let _ = stage.wait();
    }
    if let Some(thread) = process.stage_log.take() {
        let _ = thread.join();
    }
    if let Some(thread) = process.writer.take()
        && thread.join().is_err()
    {
        shutdown_error
            .get_or_insert_with(|| "FFmpeg input writer terminated unexpectedly".to_owned());
    }
    let mut recording = None;
    if let Some(thread) = process.output_thread.take() {
        match thread.join() {
            Ok(output) => recording = Some(output),
            Err(_) => {
                shutdown_error.get_or_insert_with(|| {
                    "FFmpeg recording writer terminated unexpectedly".to_owned()
                });
            }
        }
    }
    let error = process
        .queue
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .error
        .clone()
        .or(shutdown_error);
    if let Some(thread) = process.log_thread.take() {
        let _ = thread.join();
    }
    (status, recording, error)
}
impl Drop for Forwarder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed `ffmpeg -hide_banner -encoders` output of FFmpeg 9.0.2 on
    /// macOS, with a stderr line appended as `bounded_output` does.
    const ENCODER_LISTING: &str = "Encoders:
 V..... = Video
 A..... = Audio
 S..... = Subtitle
 .F.... = Frame-level multithreading
 ..S... = Slice-level multithreading
 ...X.. = Codec is experimental
 ....B. = Supports draw_horiz_band
 .....D = Supports direct rendering method 1
 ------
 V....D a64multi             Multicolor charset for Commodore 64 (codec a64_multi)
 V..... libsvtav1            SVT-AV1(Scalable Video Technology for AV1) encoder (codec av1)
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)
 V....D libx264rgb           libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 RGB (codec h264)
 V....D h264_videotoolbox    VideoToolbox H.264 Encoder (codec h264)
 V....D libx265              libx265 H.265 / HEVC (codec hevc)
 V....D hevc_videotoolbox    VideoToolbox H.265 Encoder (codec hevc)
 VFS..D mjpeg                MJPEG (Motion JPEG)
 V..X.D avui                 Avid Meridien Uncompressed
 A....D aac                  AAC (Advanced Audio Coding)
 A..... aac_at               aac (AudioToolbox) (codec aac)
 S..... dvbsub               DVB subtitles (codec dvb_subtitle)
[warning] Some stderr line from FFmpeg
";

    #[test]
    fn encoder_names_come_from_the_table_after_the_legend() {
        assert_eq!(
            parse_encoder_names(ENCODER_LISTING),
            [
                "a64multi",
                "libsvtav1",
                "libx264",
                "libx264rgb",
                "h264_videotoolbox",
                "libx265",
                "hevc_videotoolbox",
                "mjpeg",
                "avui",
                "aac",
                "aac_at",
                "dvbsub"
            ]
        );
        // Without the legend's end there is no table to read.
        assert!(parse_encoder_names(" V....D libx264 libx264 H.264").is_empty());
        assert!(parse_encoder_names("").is_empty());
    }

    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_encoder_names_list_the_build_once() {
        let names = encoder_names().unwrap();
        assert!(names.iter().any(|name| name == "png"), "{names:?}");
        assert!(Arc::ptr_eq(&names, &encoder_names().unwrap()));
    }

    #[test]
    fn encoder_listing_is_kept_and_failures_retried_after_a_minute() {
        let cache = Mutex::new(EncoderListing::Unknown);
        let start = Instant::now();
        let runs = std::cell::Cell::new(0);
        let failing = || {
            runs.set(runs.get() + 1);
            Err(anyhow!("FFmpeg encoder query failed"))
        };
        let listing = || {
            runs.set(runs.get() + 1);
            Ok(vec!["libx264".to_string()])
        };
        let error = listed_encoders(&cache, start, failing).unwrap_err();
        assert_eq!(error.to_string(), "FFmpeg encoder query failed");
        // Within a minute the failure is answered without running FFmpeg.
        let error = listed_encoders(&cache, start + Duration::from_secs(59), listing).unwrap_err();
        assert_eq!(error.to_string(), "FFmpeg encoder query failed");
        assert_eq!(runs.get(), 1);
        let names = listed_encoders(&cache, start + ENCODER_RETRY, listing).unwrap();
        assert_eq!(*names, ["libx264"]);
        // A listing is kept for the rest of the process.
        let again = listed_encoders(&cache, start + Duration::from_secs(3600), failing).unwrap();
        assert!(Arc::ptr_eq(&names, &again));
        assert_eq!(runs.get(), 2);
    }
    use crate::genicam::NodeMap;
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-media-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    /// A stand-in for FFmpeg on this test thread: a shell script that can
    /// keep notes in `$dir`.
    #[cfg(unix)]
    struct Fake {
        dir: PathBuf,
    }
    #[cfg(unix)]
    impl Fake {
        fn new(name: &str, body: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_dir(name);
            let path = dir.join("ffmpeg");
            let script = format!("#!/bin/sh\ndir='{}'\n{body}\n", dir.display());
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            FAKE_FFMPEG.set(Some(Arc::new(Executable::external(path))));
            Self { dir }
        }
        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.dir.join(name)).unwrap_or_default()
        }
        /// Launches whose arguments contain every one of `parts`.
        fn launches(&self, parts: &[&str]) -> usize {
            self.read("argv")
                .lines()
                .filter(|line| parts.iter().all(|part| line.contains(part)))
                .count()
        }
    }
    #[cfg(unix)]
    impl Drop for Fake {
        fn drop(&mut self) {
            FAKE_FFMPEG.set(None);
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
    const PAYLOAD: &[u8] = b"#!/bin/sh\necho stand-in for FFmpeg\n";
    fn digest(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }
    #[test]
    #[cfg(unix)]
    fn bundled_ffmpeg_is_extracted_once_and_shared() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = temp_dir("cache");
        let root = base.join("capturefab");
        let digest = digest(PAYLOAD);
        // Concurrent first uses produce one complete copy.
        let copies = thread::scope(|scope| {
            let workers = (0..8)
                .map(|_| scope.spawn(|| cached_copy(&root, PAYLOAD, &digest).unwrap()))
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let path = copies[0].path.clone();
        assert!(copies.iter().all(|copy| copy.path == path));
        assert_eq!(fs::read(&path).unwrap(), PAYLOAD);
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        for dir in [&root, &path.parent().unwrap().to_path_buf()] {
            assert_eq!(
                fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|entry| {
                let name = entry.as_ref().unwrap().file_name();
                name.to_string_lossy().ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
        // A later use reuses the file instead of writing it again, and the
        // copy outlives its users.
        drop(copies);
        let again = cached_copy(&root, PAYLOAD, &digest).unwrap();
        let reused = fs::metadata(&again.path).unwrap();
        assert_eq!(
            (reused.ino(), reused.mtime(), reused.mtime_nsec()),
            (meta.ino(), meta.mtime(), meta.mtime_nsec())
        );
        drop(again);
        assert!(path.exists());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn tampered_or_loosened_copies_fail_validation_and_are_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp_dir("tamper");
        let root = base.join("capturefab");
        let digest = digest(PAYLOAD);
        let path = cached_copy(&root, PAYLOAD, &digest).unwrap().path.clone();
        // Same length, different bytes: only the digest notices.
        let mut tampered = PAYLOAD.to_vec();
        tampered[16] ^= 1;
        fs::write(&path, &tampered).unwrap();
        assert!(trusted(&path, PAYLOAD.len()).is_ok());
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PAYLOAD);
        // Writable by others, or cut short: the check before each use fails.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o722)).unwrap();
        assert!(trusted(&path, PAYLOAD.len()).is_err());
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        fs::write(&path, &PAYLOAD[..10]).unwrap();
        assert!(trusted(&path, PAYLOAD.len()).is_err());
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PAYLOAD);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn copies_of_other_builds_are_removed_unless_in_use() {
        let base = temp_dir("prune");
        let root = base.join("capturefab");
        let (busy, idle) = (
            root.join("ffmpeg-0000000000000001"),
            root.join("ffmpeg-0000000000000002"),
        );
        for dir in [&busy, &idle] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join(FFMPEG_NAME), b"old").unwrap();
        }
        // Left by a cleanup that was cut short.
        let leftover = root.join("removed-0123");
        fs::create_dir_all(&leftover).unwrap();
        let holder = lock_file(&busy.join("in-use.lock")).unwrap();
        fs2::FileExt::lock_shared(&holder).unwrap();
        let copy = cached_copy(&root, PAYLOAD, &digest(PAYLOAD)).unwrap();
        assert!(busy.exists() && !idle.exists() && copy.path.exists());
        let names = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|name| name.starts_with("ffmpeg-")));
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn a_copy_removed_while_waiting_for_its_lock_is_extracted_again() {
        let base = temp_dir("prune-race");
        let root = base.join("capturefab");
        let dir = root.join(format!("ffmpeg-{}", &digest(PAYLOAD)[..16]));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FFMPEG_NAME), PAYLOAD).unwrap();
        // Another build's cleanup holds the copy's lock.
        let cleanup = lock_file(&dir.join("in-use.lock")).unwrap();
        fs2::FileExt::lock_exclusive(&cleanup).unwrap();
        let copy = thread::scope(|scope| {
            let starting = scope.spawn(|| cached_copy(&root, PAYLOAD, &digest(PAYLOAD)));
            thread::sleep(Duration::from_millis(200));
            discard(&root, &dir, cleanup);
            starting.join().unwrap().unwrap()
        });
        // The copy this process holds is in place, not the one removed.
        assert_eq!(fs::read(&copy.path).unwrap(), PAYLOAD);
        assert!(same_file(
            copy._in_use.as_ref().unwrap(),
            &dir.join("in-use.lock")
        ));
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn copies_the_system_will_not_start_are_detected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("noexec");
        let path = dir.join(FFMPEG_NAME);
        fs::write(&path, PAYLOAD).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(runs(&Executable::external(path.clone())));
        // What a noexec mount or an execution policy reports.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!runs(&Executable::external(path)));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn extracted_copy_is_reused_by_another_process() {
        use std::os::unix::fs::MetadataExt;
        if let Some(root) = std::env::var_os("CAPTUREFAB_TEST_EXTRACT_ROOT") {
            let copy = cached_copy(std::path::Path::new(&root), PAYLOAD, &digest(PAYLOAD)).unwrap();
            println!("inode {}", fs::metadata(&copy.path).unwrap().ino());
            return;
        }
        let base = temp_dir("shared");
        let root = base.join("capturefab");
        let copy = cached_copy(&root, PAYLOAD, &digest(PAYLOAD)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "media::tests::extracted_copy_is_reused_by_another_process",
                "--nocapture",
            ])
            .env("CAPTUREFAB_TEST_EXTRACT_ROOT", &root)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        let inode = fs::metadata(&copy.path).unwrap().ino();
        assert!(stdout.contains(&format!("inode {inode}\n")), "{stdout}");
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn dropped_network_stream_reconnects_with_backoff_without_spinning() {
        let fake = Fake::new(
            "reconnect",
            r#"case "$*" in *rtsp*) ;; *) exit 0 ;; esac
n=$(($(cat "$dir/launches" 2>/dev/null || echo 0) + 1)); echo $n > "$dir/launches"
frame() { printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; }
case $n in
1) frame; exit 0 ;;
2) echo 'Connection refused' >&2; exit 1 ;;
*) while frame; do sleep 0.05; done ;;
esac"#,
        );
        let mut camera = open_url(
            "rtsp://user:secret@127.0.0.1:9/live",
            Duration::from_secs(2),
        )
        .unwrap();
        camera.start(12).unwrap();
        assert_eq!(
            camera.next_frame(Duration::from_millis(100)).unwrap().width,
            2
        );
        let lost = camera.next_frame(Duration::from_millis(100)).unwrap_err();
        assert_eq!(stream_end(&lost), Some(StreamEnd::Lost));
        let message = format!("{lost:#}");
        assert!(
            message.starts_with(
                "reconnecting to rtsp://<redacted>@127.0.0.1:9/live (attempt 1): timed out"
            ),
            "{message}"
        );
        assert_eq!(crate::cli::error_code(&lost).1, 4);
        // Until a relaunch is due, each poll waits out its budget instead of
        // failing at once; the first relaunch is refused, the next streams.
        let started = Instant::now();
        let mut polls = 0u128;
        let frame = loop {
            match camera.next_frame(Duration::from_millis(100)) {
                Ok(frame) => break frame,
                Err(error) => {
                    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
                    assert!(!format!("{error:#}").contains("secret"));
                    assert_eq!(stream_end(&error), None);
                    polls += 1;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(10));
        };
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(2500), "{elapsed:?}");
        assert!(
            polls <= elapsed.as_millis() / 80 + 2,
            "{polls} polls in {elapsed:?}"
        );
        assert_eq!(frame.width, 2);
        assert_eq!(fake.read("launches").trim(), "3");
        camera.stop().unwrap();
    }
    #[test]
    #[cfg(all(unix, feature = "hubs"))]
    fn hub_restreams_return_decoder_exits_to_their_pump() {
        use crate::hubs::pump::{Cx, FrameSession, Next};
        use crate::types::ErrorKind;

        let fake = Fake::new(
            "hub-reconnect",
            r#"case "$*" in *rtsp*) ;; *) exit 0 ;; esac
echo launch >> "$dir/launches"
printf 'P6\n2 2\n255\n'
head -c 12 /dev/zero
echo 'Error during demuxing: Operation timed out' >&2
exit 1"#,
        );
        let mut backend = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(2)).unwrap();
        backend.start(12).unwrap();
        let mut session = crate::hubs::sources::rtsp_session_for_test(backend, "hub:test/live");
        let cx = Cx::new();
        let Next::Frame(frame) = session.next(&cx).unwrap() else {
            panic!("restream did not deliver its first frame");
        };
        assert_eq!((frame.width, frame.height), (2, 2));

        let started = Instant::now();
        let failure = match session.next(&cx) {
            Err(failure) => failure,
            Ok(_) => panic!("ended restream did not return its failure"),
        };
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(failure.kind, ErrorKind::OperationFailed);
        assert_eq!(failure.message, "the RTSP restream of hub:test/live ended");
        // An ordinary transient failure takes the pump's reconnect backoff;
        // it must not run MediaBackend's reconnect loop inside the pump.
        assert!(!failure.permanent && !failure.quiet);
        assert_eq!(fake.read("launches").lines().count(), 1);

        cx.cancel.cancel();
        let started = Instant::now();
        let cancelled = match session.next(&cx) {
            Err(failure) => failure,
            Ok(_) => panic!("cancelled restream kept reading"),
        };
        assert_eq!(cancelled.message, "cancelled");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(fake.read("launches").lines().count(), 1);
    }
    #[test]
    fn first_open_failures_explain_ffmpeg_in_one_sentence() {
        let wait = Duration::from_secs(2);
        let source = "rtsp://user:pw@127.0.0.1:1/x";
        let explained = |source: &str, log: &str, timed_out: bool| {
            let error = network_failure(source, log, timed_out, wait).expect(log);
            (crate::cli::error_code(&error).0, format!("{error:#}"))
        };
        let refused = "[tcp @ 0x7b8820000] Connection to tcp://127.0.0.1:1?timeout=2000000 failed: Connection refused\n[in#0 @ 0x993014000] Error opening input: Connection refused\nError opening input file rtsp://<redacted>@127.0.0.1:1/x.";
        let (code, message) = explained(source, refused, false);
        assert_eq!(code, "unavailable");
        assert_eq!(
            message,
            "Nothing accepted a connection at 127.0.0.1:1 (rtsp://<redacted>@127.0.0.1:1/x). Check the camera's stream port and that streaming is turned on. (FFmpeg: Connection to tcp://127.0.0.1:1 failed: Connection refused)"
        );
        // Without a port, RTSP's default is the likely fix.
        let (_, message) = explained("rtsp://10.0.0.5/live", "Connection refused", false);
        assert!(
            message.contains("RTSP normally uses port 554."),
            "{message}"
        );
        let (code, message) = explained(
            source,
            "[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized",
            false,
        );
        assert_eq!(code, "operation_failed");
        assert!(
            message.starts_with("127.0.0.1 rejected the username or password (rtsp://<redacted>@127.0.0.1:1/x). Check them, or add them as rtsp://USER:PASS@127.0.0.1:1/x if none were given."),
            "{message}"
        );
        let (code, message) = explained(
            source,
            "[rtsp @ 0x1] method DESCRIBE failed: 404 Not Found",
            false,
        );
        assert_eq!(code, "unavailable");
        assert!(
            message.starts_with("127.0.0.1:1 answered but has no stream at /x"),
            "{message}"
        );
        assert!(message.contains("onvif://USER:PASS@127.0.0.1/onvif/device_service"));
        // An RTMP stream key never comes back in the suggested URL or path.
        for log in [
            "[rtmp @ 0x1] Server error: 401 Unauthorized",
            "[rtmp @ 0x1] stream not found",
        ] {
            let (_, message) = explained("rtmp://10.0.0.5/live/SECRETKEY", log, false);
            assert!(!message.contains("SECRETKEY"), "{message}");
        }
        let (code, message) = explained(
            "rtsp://nonexistent.invalid/live",
            "[tcp @ 0x2] Failed to resolve hostname nonexistent.invalid: nodename nor servname provided, or not known",
            false,
        );
        assert_eq!(code, "unavailable");
        assert!(
            message.starts_with("Cannot resolve nonexistent.invalid"),
            "{message}"
        );
        let (code, message) = explained(
            "srt://203.0.113.9:9000",
            "Connection to tcp://203.0.113.9:9000 failed: No route to host",
            false,
        );
        assert_eq!(code, "unavailable");
        assert!(
            message.starts_with("203.0.113.9 is not reachable from this computer"),
            "{message}"
        );
        // Silence within the wait is a timeout, worded for a stream that
        // may be merely slow: FFmpeg logs only errors.
        let (code, message) = explained("rtsp://user:pw@10.255.255.1/stream", "", true);
        assert_eq!(code, "timeout");
        assert_eq!(
            message,
            "No video arrived from 10.255.255.1 within 2 s (rtsp://<redacted>@10.255.255.1/stream). It may be unreachable or slow to send its first frame; retry with a longer timeout (--timeout-ms 10000)."
        );
        // A connect that timed out says the host did not reply.
        let (code, message) = explained(
            "rtsp://10.255.255.1/stream",
            "[tcp @ 0x3] Connection to tcp://10.255.255.1:554?timeout=2000000 failed: Operation timed out",
            false,
        );
        assert_eq!(code, "timeout");
        assert!(
            message.starts_with("No reply from 10.255.255.1 (rtsp://10.255.255.1/stream). Check it is on this network and streaming, or retry with a longer timeout (--timeout-ms 10000). (FFmpeg: Connection to tcp://10.255.255.1:554 failed: Operation timed out)"),
            "{message}"
        );
        // A longer wait is suggested only while one is left to try.
        assert_eq!(longer_wait(Duration::from_secs(20)), Some(40_000));
        assert_eq!(longer_wait(Duration::from_secs(40)), Some(60_000));
        assert_eq!(longer_wait(Duration::from_secs(60)), None);
        let (_, message) = network_failure(source, "", true, Duration::from_secs(60))
            .map(|error| ((), format!("{error:#}")))
            .unwrap();
        assert!(
            message.ends_with("It may be unreachable or slow to send its first frame; check it is on this network and streaming."),
            "{message}"
        );
        for (log, timed_out) in [
            (refused, false),
            ("", true),
            ("method DESCRIBE failed: 401 Unauthorized", false),
        ] {
            let (_, message) = explained(source, log, timed_out);
            for internal in ["@ 0x", "?timeout=", "user:pw", "\n"] {
                assert!(!message.contains(internal), "{message}");
            }
        }
        // Anything else keeps FFmpeg's own error.
        assert!(
            network_failure(
                source,
                "Invalid data found when processing input",
                false,
                wait
            )
            .is_none()
        );
        assert!(network_failure(source, "", false, wait).is_none());
        assert!(network_failure("file:/tmp/x.mp4", refused, false, wait).is_none());
    }
    #[test]
    #[cfg(unix)]
    fn a_refused_first_open_says_so_in_one_line() {
        let _fake = Fake::new(
            "first-open-refused",
            r#"case "$*" in *rtsp*) ;; *) exit 0 ;; esac
echo '[tcp @ 0x7b8820000] Connection to tcp://127.0.0.1:1?timeout=2000000 failed: Connection refused' >&2
exit 1"#,
        );
        let error = match open_url("rtsp://user:secret@127.0.0.1:1/x", Duration::from_secs(2)) {
            Err(error) => error,
            Ok(_) => panic!("a refused stream opened"),
        };
        let message = format!("{error:#}");
        assert_eq!(
            crate::cli::error_code(&error),
            ("unavailable", 3),
            "{message}"
        );
        assert!(
            message.starts_with("Nothing accepted a connection at 127.0.0.1:1"),
            "{message}"
        );
        assert!(
            !message.contains("secret") && !message.contains('\n'),
            "{message}"
        );
    }
    #[test]
    fn reconnecting_stops_after_repeated_refusals() {
        let mut camera = native("rtsp://camera/live", Vec::new());
        camera.native = false;
        let lost = camera.lost("media stream ended: 401 Unauthorized".into());
        assert_eq!(stream_end(&lost), Some(StreamEnd::Lost));
        let refusal = "media stream ended: method DESCRIBE failed: 401 Unauthorized";
        for attempt in 2..=3 {
            let waiting = camera.lost(refusal.into());
            assert_eq!(stream_end(&waiting), None);
            assert!(format!("{waiting}").contains(&format!("(attempt {attempt}): timed out")));
        }
        // An unrelated failure resets the count; backoff keeps doubling
        // up to its limit.
        camera.lost("media stream ended: Connection refused".into());
        let backoff = camera.reconnect.as_ref().unwrap().backoff;
        assert_eq!(backoff, Duration::from_secs(16));
        for _ in 1..MAX_REFUSED {
            assert_eq!(stream_end(&camera.lost(refusal.into())), None);
        }
        assert_eq!(
            camera.reconnect.as_ref().unwrap().backoff,
            RECONNECT_BACKOFF_MAX
        );
        let abandoned = camera.lost(refusal.into());
        assert_eq!(stream_end(&abandoned), Some(StreamEnd::Abandoned));
        assert!(
            format!("{abandoned}")
                .starts_with("stopped reconnecting to rtsp://camera/live after 8 attempts")
        );
        assert!(camera.reconnect.is_none());
        // A relay answers 404 while its camera is away: keep trying.
        camera.lost("media stream ended: Connection refused".into());
        for _ in 0..MAX_REFUSED * 4 {
            let missing = "media stream ended: method DESCRIBE failed: 404 Not Found";
            assert_eq!(stream_end(&camera.lost(missing.into())), None);
        }
        assert_eq!(
            camera.reconnect.as_ref().unwrap().backoff,
            RECONNECT_BACKOFF_MAX
        );
    }
    #[test]
    #[cfg(unix)]
    fn rtsp_sign_in_requests_and_refusals_are_typed() {
        let fake = Fake::new(
            "auth",
            r#"echo "$*" >> "$dir/argv"
case "$*" in
  *missing*) echo '[rtsp @ 0x1] method DESCRIBE failed: 404 Not Found' >&2 ;;
  *blocked*) echo '[rtsp @ 0x1] method DESCRIBE failed: 403 Forbidden' >&2 ;;
  *) echo '[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized' >&2 ;;
esac
exit 1"#,
        );
        let timeout = Duration::from_secs(10);
        let code = |result: Result<Box<dyn Backend>>| {
            let error = result.err().expect("open fails");
            (
                crate::types::error_transport_code(&error),
                format!("{error:#}"),
            )
        };
        let (kind, text) = code(open_url("rtsp://cam.local/live", timeout));
        assert_eq!(kind, Some("auth_required"), "{text}");
        assert!(text.contains("rtsp://cam.local/live"), "{text}");
        // TCP first, then UDP for a camera that answers 461 to TCP.
        assert_eq!(
            fake.launches(&["-rtsp_flags prefer_tcp -rtsp_transport tcp+udp -timeout"]),
            1
        );
        let login = crate::logins::CameraLogin::new("ad min", "s3cret:pw").unwrap();
        let (kind, text) = code(open_url_with(
            "rtsp://cam.local/live",
            Some(&login),
            timeout,
        ));
        assert_eq!(kind, Some("auth_rejected"), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
        assert_eq!(fake.launches(&["ad%20min:s3cret%3Apw@cam.local/live"]), 1);
        let (kind, text) = code(open_url("rtsp://admin:typed-pw@cam.local/live", timeout));
        assert_eq!(kind, Some("auth_rejected"), "{text}");
        assert!(!text.contains("typed-pw"), "{text}");
        // FFmpeg keeps 127 bytes of `user:password`, percent-encoded.
        let fits = format!("{}a", "!".repeat(40));
        let login = crate::logins::CameraLogin::new("admin", &fits).unwrap();
        let (kind, _) = code(open_url_with(
            "rtsp://cam.local/live",
            Some(&login),
            timeout,
        ));
        assert_eq!(kind, Some("auth_rejected"));
        assert_eq!(
            fake.launches(&[&format!("admin:{}a@", "%21".repeat(40))]),
            1
        );
        let long = format!("{}ab", "!".repeat(40));
        let login = crate::logins::CameraLogin::new("admin", &long).unwrap();
        let (kind, text) = code(open_url_with(
            "rtsp://cam.local/live",
            Some(&login),
            timeout,
        ));
        assert_eq!(kind, Some("unsupported"), "{text}");
        assert!(text.contains("rtsp://cam.local/live") && text.contains("128 bytes"));
        assert!(!text.contains("%21"), "{text}");
        assert_eq!(fake.launches(&["ab@cam.local"]), 0);
        for other in ["rtsp://cam.local/missing", "rtsp://cam.local/blocked"] {
            // Not a sign-in problem; the connection diagnosis may still
            // categorize it (a 404 is unavailable).
            let (kind, text) = code(open_url(other, timeout));
            assert!(
                !matches!(kind, Some("auth_required" | "auth_rejected")),
                "{text}"
            );
        }
    }
    #[test]
    fn a_stream_that_drops_again_soon_keeps_backing_off() {
        let mut camera = native("rtsp://camera/live", Vec::new());
        camera.native = false;
        let first = camera.lost("media stream ended".into());
        assert_eq!(stream_end(&first), Some(StreamEnd::Lost));
        // One frame, then the stream ends again: reported, with the backoff
        // carried on instead of starting again from 1 s.
        camera.delivered();
        assert!(camera.reconnect.is_some());
        let again = camera.lost("media stream ended".into());
        assert_eq!(stream_end(&again), Some(StreamEnd::Lost));
        assert!(
            format!("{again}").contains("(attempt 2): timed out"),
            "{again}"
        );
        let reconnect = camera.reconnect.as_ref().unwrap();
        assert_eq!(reconnect.backoff, Duration::from_secs(4));
        assert!(reconnect.retry_at > Instant::now() + Duration::from_secs(1));
        // A stream that stays up ends the outage.
        camera.delivered();
        camera.reconnect.as_mut().unwrap().resumed = Instant::now().checked_sub(STEADY);
        camera.delivered();
        assert!(camera.reconnect.is_none());
        let fresh = camera.lost("media stream ended".into());
        assert!(
            format!("{fresh}").contains("(attempt 1): timed out"),
            "{fresh}"
        );
    }
    #[test]
    #[cfg(unix)]
    fn video_files_play_in_real_time_and_finish() {
        let fake = Fake::new(
            "file",
            r#"echo "$*" >> "$dir/argv"
case "$*" in *clip.mkv*) ;; *) exit 0 ;; esac
for i in 1 2 3; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; done"#,
        );
        let clip = fake.dir.join("clip.mkv");
        fs::write(&clip, b"").unwrap();
        let mut camera = open_url(clip.to_str().unwrap(), Duration::from_secs(2)).unwrap();
        let argv = fake.read("argv");
        let launch = argv.lines().find(|line| line.contains("clip.mkv")).unwrap();
        let input = launch.find(" -i ").unwrap();
        assert!(
            launch.find("-readrate 1").is_some_and(|at| at < input),
            "{launch}"
        );
        assert!(
            launch.find("-threads ").is_some_and(|at| at < input),
            "{launch}"
        );
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        assert_eq!(
            nodes.get(camera.as_mut(), "PlaybackSpeed").unwrap(),
            "RealTime"
        );
        camera.start(12).unwrap();
        for _ in 0..3 {
            camera.next_frame(Duration::from_secs(2)).unwrap();
        }
        for _ in 0..2 {
            let finished = camera.next_frame(Duration::from_secs(2)).unwrap_err();
            assert_eq!(stream_end(&finished), Some(StreamEnd::Finished));
            assert!(format!("{finished}").starts_with("video file finished"));
        }
        // Fastest decodes every frame unpaced, for extraction.
        camera.stop().unwrap();
        nodes
            .set(camera.as_mut(), "PlaybackSpeed", "Fastest")
            .unwrap();
        camera.start(12).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        let relaunch = fake.read("argv");
        assert!(!relaunch.lines().last().unwrap().contains("-readrate"));
        camera.stop().unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn captures_read_video_files_as_fast_as_they_ask() {
        let fake = Fake::new(
            "on-demand",
            r#"case "$*" in *clip.mkv*) echo "$*" >> "$dir/argv" ;; *) exit 0 ;; esac
for i in 1 2 3; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; done"#,
        );
        let clip = fake.dir.join("clip.mkv");
        fs::write(&clip, b"").unwrap();
        let mut camera = open_url(clip.to_str().unwrap(), Duration::from_secs(2)).unwrap();
        let paced = |line: &str| line.contains("-readrate 1");
        // A capture's own start: the file starts again unpaced, once.
        camera.start(12).unwrap();
        for _ in 0..3 {
            camera.request_frame().unwrap();
            camera.next_frame(Duration::from_secs(2)).unwrap();
        }
        let launches = fake.read("argv");
        let launches = launches.lines().collect::<Vec<_>>();
        assert_eq!(launches.len(), 2, "{launches:?}");
        assert!(paced(launches[0]) && !paced(launches[1]), "{launches:?}");
        camera.stop().unwrap();
        // Captures during a live view keep its pace.
        camera.start(12).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        camera.request_frame().unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        let launches = fake.read("argv");
        assert_eq!(launches.lines().count(), 3, "{launches}");
        assert!(paced(launches.lines().last().unwrap()), "{launches}");
        camera.stop().unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn http_video_files_finish_and_http_streams_reconnect() {
        let _fake = Fake::new(
            "http",
            r#"case "$*" in *127.0.0.1*) ;; *) exit 0 ;; esac
for i in 1 2; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; done"#,
        );
        for (url, end) in [
            (
                "http://127.0.0.1:9/media/clip.MP4?token=1",
                StreamEnd::Finished,
            ),
            ("http://127.0.0.1:9/video.mjpg", StreamEnd::Lost),
            ("http://127.0.0.1:9/", StreamEnd::Lost),
        ] {
            let mut camera = open_url(url, Duration::from_secs(2)).unwrap();
            camera.start(12).unwrap();
            for _ in 0..2 {
                camera.next_frame(Duration::from_secs(2)).unwrap();
            }
            let ended = camera.next_frame(Duration::from_secs(2)).unwrap_err();
            assert_eq!(stream_end(&ended), Some(end), "{url}: {ended:#}");
            if end == StreamEnd::Finished {
                assert_eq!(
                    format!("{ended}"),
                    "video file finished: http://127.0.0.1:9/media/clip.MP4?token=<redacted>"
                );
            }
            camera.stop().unwrap();
        }
    }
    #[test]
    #[cfg(unix)]
    fn replay_decoders_limit_threads_on_the_decoder() {
        let fake = Fake::new("replay", r#"echo "$*" >> "$dir/argv""#);
        let clip = fake.dir.join("clip.mkv");
        fs::write(&clip, b"").unwrap();
        let decoder = ReplayDecoder::open(&clip, 64, 48, 30.0, false, Duration::from_millis(200));
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.read("argv").is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        drop(decoder);
        let launch = fake.read("argv");
        let input = launch.find(" -i ").unwrap();
        assert!(
            launch.find("-threads 1").is_some_and(|at| at < input),
            "{launch}"
        );
        assert!(
            launch
                .find("-filter_threads 1")
                .is_some_and(|at| at > input),
            "{launch}"
        );
        assert_eq!(launch.matches("-threads ").count(), 1, "{launch}");
        assert!(launch.contains("-hwaccel none"), "{launch}");
    }
    #[test]
    fn decoders_and_software_encoders_are_sized_to_the_host() {
        assert_eq!(decoder_threads(16, 1, false), 4);
        assert_eq!(decoder_threads(8, 3, false), 2);
        assert_eq!(decoder_threads(4, 6, false), 1);
        assert_eq!(decoder_threads(16, 1, true), 1);
        // 1080p30 is 62 Mpx/s.
        assert_eq!(x264_preset(1920, 1080, 30.0, 14, false), "veryfast");
        assert_eq!(x264_preset(1920, 1080, 30.0, 4, false), "superfast");
        assert_eq!(x264_preset(1920, 1080, 30.0, 2, false), "ultrafast");
        assert_eq!(x264_preset(1920, 1080, 30.0, 14, true), "superfast");
        assert_eq!(x264_preset(3840, 2160, 30.0, 2, true), "ultrafast");
    }
    #[test]
    #[cfg(unix)]
    fn auto_hwaccel_is_dropped_where_no_device_initializes() {
        for (log, expected) in [
            ("", "none"),
            (
                "[dec:h264] Using auto hwaccel type videotoolbox with new default device.",
                "auto",
            ),
        ] {
            let _fake = Fake::new("hwaccel", &format!("cat > /dev/null; echo '{log}' >&2"));
            let exe = executable().unwrap();
            let arm_linux = cfg!(all(
                target_os = "linux",
                any(target_arch = "aarch64", target_arch = "arm")
            ));
            let expected = if arm_linux { "none" } else { expected };
            if hwaccel() == "auto" {
                assert_eq!(decode_acceleration(&exe).unwrap(), expected, "{log}");
            }
        }
    }
    #[test]
    fn probe_results_persist_for_the_boot_and_ignore_corrupt_files() {
        let Some(boot) = boot_id() else {
            return;
        };
        let dir = temp_dir("probes");
        let path = dir.join("probes.json");
        let exe = Executable {
            path: dir.join("ffmpeg"),
            directory: None,
            probes: Some(path.clone()),
            _in_use: None,
        };
        remember(&exe, |probes| {
            probes
                .encoders
                .insert("libx264 yuv 64x48".into(), Found::now(true));
        });
        // Another process starts from the file.
        let stored = load_probes(&exe);
        assert_eq!(stored.boot, boot);
        let found = stored.encoders.get("libx264 yuv 64x48").copied();
        assert_eq!(found.and_then(Found::current), Some(true));
        let mut previous = stored;
        previous.boot = "an earlier boot".into();
        fs::write(&path, serde_json::to_vec(&previous).unwrap()).unwrap();
        assert!(load_probes(&exe).encoders.is_empty());
        fs::write(&path, b"{not json").unwrap();
        assert!(load_probes(&exe).encoders.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_probes_expire_and_timed_out_probes_are_not_remembered() {
        let dir = temp_dir("probe-failures");
        let exe = Executable {
            path: dir.join("ffmpeg"),
            directory: None,
            probes: Some(dir.join("probes.json")),
            _in_use: None,
        };
        // The answer, and whether the probe ran to give it.
        let probe = |key: &str, answer: Option<bool>| {
            let mut ran = false;
            let works = probed(
                &exe,
                |probes| probes.encoders.get(key).copied(),
                |probes, found| {
                    probes.encoders.insert(key.into(), found);
                },
                || {
                    ran = true;
                    Ok(answer)
                },
            )
            .unwrap();
            (works, ran)
        };
        // A probe that timed out tells nothing, so it runs again next time.
        assert_eq!(probe("slow", None), (None, true));
        assert_eq!(probe("slow", Some(true)), (Some(true), true));
        assert_eq!(probe("slow", None), (Some(true), false));
        // A failure is believed for a while, then probed again.
        assert_eq!(probe("busy", Some(false)), (Some(false), true));
        assert_eq!(probe("busy", Some(true)), (Some(false), false));
        with_probes(&exe, |probes| {
            probes.encoders.get_mut("busy").unwrap().at_ms -= FAILURE_KEPT.as_millis() as u64;
        });
        assert_eq!(probe("busy", Some(true)), (Some(true), true));
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    fn tiny(width: u32, height: u32) -> Frame {
        Frame {
            id: 1,
            width,
            height,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data: vec![7; (width * height * 3) as usize],
        }
    }
    #[cfg(unix)]
    fn stream(encoder: &str, fps: f64) -> ForwardConfig {
        ForwardConfig {
            output: "tcp://127.0.0.1:9".into(),
            codec: "h264".into(),
            encoder: encoder.into(),
            fps,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
            destination: None,
        }
    }
    #[cfg(unix)]
    fn recording(path: &std::path::Path, encoder: &str) -> ForwardConfig {
        ForwardConfig {
            output: path.to_string_lossy().into(),
            ..stream(encoder, 30.0)
        }
    }
    /// Run the calling test again in a child process with its own session
    /// directory, since recordings reserve storage in its ledger. True in
    /// the child, which does the work.
    #[cfg(unix)]
    fn in_own_session(test: &str) -> bool {
        if std::env::var_os("CAPTUREFAB_OWN_SESSION_TEST").is_some() {
            return true;
        }
        let dir = temp_dir("session").join("session");
        crate::ipc::ensure_private_dir(&dir).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("media::tests::{test}"), "--nocapture"])
            .env("CAPTUREFAB_SESSION_DIR", &dir)
            .env("CAPTUREFAB_OWN_SESSION_TEST", "1")
            .output()
            .unwrap();
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }
    #[test]
    #[cfg(unix)]
    fn recordings_get_each_new_frame_once_on_the_wall_clock() {
        if !in_own_session("recordings_get_each_new_frame_once_on_the_wall_clock") {
            return;
        }
        let fake = Fake::new(
            "unique",
            r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat ;;
esac"#,
        );
        let output = fake.dir.join("unique.mkv");
        let mut forward = Forwarder::new(recording(&output, "libx264")).unwrap();
        for _ in 0..3 {
            forward.push(tiny(8, 4)).unwrap();
            thread::sleep(Duration::from_millis(200));
        }
        assert_eq!(forward.stats().written, 3);
        assert!(!forward.stats().stalled);
        thread::sleep(STALL);
        assert!(forward.stats().stalled);
        assert_eq!(forward.stats().written, 3);
        forward.stop().unwrap();
        // No repeated frames reach FFmpeg, and nothing fills the stall: the
        // output keeps the time each frame arrived.
        assert_eq!(fs::read(&output).unwrap().len(), 3 * 8 * 4 * 3);
        assert_eq!(
            fake.launches(&[
                "-use_wallclock_as_timestamps 1 -i pipe:0",
                "-fps_mode vfr",
                "-preset ",
                "-threads ",
                "-x264-params force-cfr=0"
            ]),
            1
        );
        assert!(!fake.read("argv").contains(" -r "));
    }
    #[test]
    #[cfg(unix)]
    fn streams_get_the_last_frame_again_through_a_stall() {
        let fake = Fake::new(
            "repeat",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat > "$dir/encoded" ;;
esac"#,
        );
        let mut forward = Forwarder::new(stream("libx264", 20.0)).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        thread::sleep(STALL + Duration::from_millis(500));
        let stats = forward.stats();
        // About 50 slots: receivers keep getting a picture.
        assert!(stats.stalled && stats.written >= 30, "{stats:?}");
        forward.stop().unwrap();
        let sent = fake.read("encoded").len() as u64;
        assert!(sent >= stats.written * 8 * 4 * 3, "{sent}");
    }
    #[test]
    #[cfg(unix)]
    fn a_recording_keeps_its_video_when_ffmpeg_must_be_stopped() {
        if !in_own_session("a_recording_keeps_its_video_when_ffmpeg_must_be_stopped") {
            return;
        }
        let fake = Fake::new(
            "stuck",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) head -c 10000 /dev/zero; cat > /dev/null; exec sleep 30 ;;
esac"#,
        );
        let output = fake.dir.join("stuck.mkv");
        let mut forward = Forwarder::new(recording(&output, "libx264")).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        thread::sleep(Duration::from_millis(300));
        let error = forward.stop().unwrap_err();
        assert!(
            format!("{error:#}").contains("shutdown timed out"),
            "{error:#}"
        );
        assert_eq!(forward.stats().recording.unwrap().bytes, 10_000);
        assert_eq!(fs::read(&output).unwrap().len(), 10_000);
    }
    #[test]
    #[cfg(unix)]
    fn a_resolution_change_stops_the_recording_with_a_reason() {
        let _fake = Fake::new(
            "resize",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat > /dev/null ;;
esac"#,
        );
        let mut forward = Forwarder::new(stream("libx264", 30.0)).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        let error = forward.push(tiny(16, 8)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the camera's resolution changed from 8x4 to 16x8; start a new recording to continue"
        );
        forward.stop().unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn encoder_probes_run_once_per_process() {
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "memo",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat > /dev/null ;;
esac"#
            ),
        );
        let mut probes = 0;
        for _ in 0..3 {
            let mut forward = Forwarder::new(stream("auto", 30.0)).unwrap();
            forward.push(tiny(16, 16)).unwrap();
            forward.stop().unwrap();
            assert_eq!(forward.stats().encoder, first);
            if probes == 0 {
                probes = fake.launches(&["-f null"]);
            }
        }
        // One listing, and one probe for each encoder, input and size; later
        // recordings only launch their encoder.
        assert_eq!(fake.launches(&["-encoders"]), 1);
        assert_eq!(fake.launches(&["size=640x480", "-f null"]), 1);
        assert!(fake.launches(&["size=16x16", "-f null"]) >= 1);
        assert_eq!(fake.launches(&["-f null"]), probes);
        assert_eq!(fake.launches(&["pipe:0"]), 3);
    }
    #[test]
    #[cfg(unix)]
    fn codecs_without_a_working_encoder_fail_before_recording() {
        let _fake = Fake::new(
            "unsupported",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n V....D hevc_bogus bogus (codec hevc)\n' ;;
*"-c:v hevc_bogus"*) exit 1 ;;
esac"#,
        );
        let mut config = stream("auto", 30.0);
        config.codec = "hevc".into();
        let error = Forwarder::new(config).err().unwrap();
        let message = format!("{error:#}");
        assert!(
            message.starts_with("hevc recording is not supported on this host"),
            "{message}"
        );
        assert!(message.contains("working codecs: h264."), "{message}");
        assert_eq!(crate::cli::error_code(&error), ("unsupported", 5));
    }
    #[test]
    #[cfg(unix)]
    fn a_stream_whose_encoder_exits_does_not_fall_back() {
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "stream-exit",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) echo 'Connection refused' >&2; exit 1 ;;
esac"#
            ),
        );
        let mut forward = Forwarder::new(stream("auto", 30.0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.push(tiny(16, 16)).is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(30));
        }
        let _ = forward.stop();
        assert_eq!(fake.launches(&["pipe:0"]), 1);
        assert_eq!(fake.launches(&["-c:v libx264", "pipe:0"]), 0);
    }
    #[test]
    #[cfg(unix)]
    fn an_encoder_that_exits_at_start_falls_back_once_to_a_new_file() {
        if !in_own_session("an_encoder_that_exits_at_start_falls_back_once_to_a_new_file") {
            return;
        }
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "fallback",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*"-c:v {first}"*) printf 'partial' ; exit 1 ;;
*"-c:v libx264"*) [ -e "$dir/x264-fails" ] && exit 1; cat ;;
esac"#
            ),
        );
        let output = fake.dir.join("clip.mkv");
        let mut config = stream("auto", 30.0);
        config.output = output.to_string_lossy().into();
        let mut forward = Forwarder::new(config.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.stats().encoder != "libx264" || forward.stats().written < 2 {
            assert!(Instant::now() < deadline, "{:?}", forward.stats());
            forward.push(tiny(16, 16)).unwrap();
            thread::sleep(Duration::from_millis(30));
        }
        forward.stop().unwrap();
        // The failed encoder's bytes never reach the new file.
        let written = forward.stats().written as usize;
        assert_eq!(fs::read(&output).unwrap().len(), written * 16 * 16 * 3);
        assert_eq!(fake.launches(&[&format!("-c:v {first}"), "pipe:0"]), 1);
        assert_eq!(fake.launches(&["-c:v libx264", "pipe:0"]), 1);
        // A second early exit is reported instead of trying again.
        fs::write(fake.dir.join("x264-fails"), b"").unwrap();
        config.output = fake.dir.join("again.mkv").to_string_lossy().into();
        let mut forward = Forwarder::new(config).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.push(tiny(32, 32)).is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(30));
        }
        let _ = forward.stop();
        assert_eq!(fake.launches(&[&format!("-c:v {first}"), "pipe:0"]), 2);
        assert_eq!(fake.launches(&["-c:v libx264", "pipe:0"]), 2);
        assert!(!fake.dir.join("again.mkv").exists());
    }
    #[test]
    #[cfg(unix)]
    fn an_encoder_that_fails_after_writing_video_keeps_it_instead_of_falling_back() {
        if !in_own_session(
            "an_encoder_that_fails_after_writing_video_keeps_it_instead_of_falling_back",
        ) {
            return;
        }
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "late-failure",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*"-c:v {first}"*) head -c 10000 /dev/zero; exit 1 ;;
*) cat ;;
esac"#
            ),
        );
        let output = fake.dir.join("late.mkv");
        let mut forward = Forwarder::new(recording(&output, "auto")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.push(tiny(16, 16)).is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(30));
        }
        forward.stop().unwrap();
        let stats = forward.stats();
        assert!(stats.error.is_some(), "{stats:?}");
        assert_eq!(stats.recording.unwrap().bytes, 10_000);
        assert_eq!(fs::read(&output).unwrap().len(), 10_000);
        assert_eq!(fake.launches(&["-c:v libx264", "pipe:0"]), 0);
    }
    #[test]
    fn ppm_preserves_whitespace_pixels_and_multiple_frames() {
        let mut bytes = b"P6\n2 1\n255\n".to_vec();
        bytes.extend([10, 32, 13, 0, 255, 1]);
        bytes.extend(b"P6\n1 1\n255\n");
        bytes.extend([1, 2, 3]);
        let mut reader = BufReader::new(bytes.as_slice());
        let a = read_ppm(&mut reader, 1, 0).unwrap();
        assert_eq!(a.data, [10, 32, 13, 0, 255, 1]);
        assert_eq!(read_ppm(&mut reader, 2, 0).unwrap().data, [1, 2, 3]);
    }
    #[test]
    fn ppm_rejects_oversize_truncation_and_sample_depth() {
        for bytes in [
            &b"P6\n9999999 9999999\n255\n"[..],
            &b"P6\n1 1\n65535\n"[..],
            &b"P6\n1 1\n255\n\0"[..],
        ] {
            assert!(read_ppm(&mut BufReader::new(bytes), 1, 0).is_err());
        }
    }
    #[test]
    fn sources_redact_credentials_and_forbid_command_protocols() {
        assert_eq!(
            redact_url("rtsp://user:pass@camera:554/live?token=abc&fps=30"),
            "rtsp://<redacted>@camera:554/live?token=<redacted>&fps=30"
        );
        assert_eq!(
            redact_url("srt://host:8890?passphrase=secret&mode=caller"),
            "srt://host:8890?passphrase=<redacted>&mode=caller"
        );
        assert_eq!(
            redact_url("RTMPS://user:pass@host/app/instance/KEY?token=SECRET&fps=30"),
            "RTMPS://<redacted>@host/app/<redacted>?token=<redacted>&fps=30"
        );
        assert_eq!(redact_url("rtmp://host/app/"), "rtmp://host/app/");
        assert_eq!(redact_url("rtmp://host/app"), "rtmp://host/app");
        assert_eq!(
            redact_url("rtmp://host/app/<redacted>"),
            "rtmp://host/app/<redacted>"
        );
        assert!(info("rtsp://camera/live").is_ok());
        assert!(info("concat:secret|other").is_err());
        assert!(info("rtsp://camera/live\n-x").is_err());
        // Vendor and signed-URL credentials (Foscam pwd, Blue Iris pw,
        // Home Assistant authSig, API keys) never reach logs either.
        assert_eq!(
            redact_url(
                "http://cam/cgi?usr=a&PWD=b&pw=c&api_key=d&ApiKey=e&authSig=f&sig=g&signature=h&stream=main&fps=5"
            ),
            "http://cam/cgi?usr=a&PWD=<redacted>&pw=<redacted>&api_key=<redacted>&ApiKey=<redacted>&authSig=<redacted>&sig=<redacted>&signature=<redacted>&stream=main&fps=5"
        );
    }
    #[test]
    fn ffmpeg_never_inherits_hub_secrets() {
        for name in [
            "CAPTUREFAB_HUB_TEST_SECRET",
            "capturefab_hub_test_secret",
            "Capturefab_Hub_Secret",
        ] {
            assert!(is_hub_secret_variable(std::ffi::OsStr::new(name)));
        }
        assert!(!is_hub_secret_variable(std::ffi::OsStr::new(
            "CAPTUREFAB_FFMPEG"
        )));
    }
    #[test]
    fn query_credentials_are_redacted_across_case_separators_and_percent_encoding() {
        for key in [
            "api_key",
            "API-Key",
            "client_secret",
            "ClIeNt-SeCrEt",
            "X-Amz-Credential",
            "x-AmZ-SiGnAtUrE",
            "X-Amz-Security-Token",
            "X-Goog-Credential",
            "x-GoOg-SiGnAtUrE",
            "X-Goog-Security-Token",
            "sig",
            "SiG",
            "%61pI%5fkEy",
            "%41PI%2dKeY",
            "cLiEnT%5FsEcReT",
            "%58-AmZ-%43reDential",
            "X%2daMz%2dSiGnAtUrE",
            "x-AmZ-sEcUrItY%2dtOkEn",
            "%78-gOoG-CrEdEnTiAl",
            "X%2DgOoG%2DsIgNaTuRe",
            "x-GoOg-SeCuRiTy%2DtOkEn",
            "%73%49%67",
            "aCcEsS%5FtOkEn",
            "%50aSsWoRd",
            "%50wD",
            "%70W",
            "AuTh%53Ig",
            "%73IgNaTuRe",
        ] {
            let url = format!("https://camera/stream?{key}=PRIVATE%2fvalue&fps=30");
            assert!(credential_query_key(key), "{key}");
            assert_eq!(
                redact_url(&url),
                format!("https://camera/stream?{key}=<redacted>&fps=30"),
                "{key}"
            );
        }
    }
    #[test]
    fn query_redaction_preserves_noncredential_parameters() {
        let url = "https://camera/stream?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20261018T120000Z&X-Amz-Expires=60&X-Goog-SignedHeaders=host&sv=2026-02-06&se=2026-10-19&sp=r&fps=30&format=h264&request_id=value%2fkept";
        assert_eq!(redact_url(url), url);
        for key in [
            "X-Amz-Date",
            "X-Goog-Algorithm",
            "expires",
            "client_id",
            "keyframe_interval",
            "apikey_hint",
            "api%252dkey",
            "api%key",
            "api%2",
            "api%zzkey",
        ] {
            assert!(!credential_query_key(key), "{key}");
        }
    }
    #[test]
    fn backend_urls_lowercase_only_the_scheme_and_keep_credentials_redacted() {
        let source = "RTSP://User:P%40ss@Camera.Example:554/Live/%aB?token=Secret%2f&Mode=TCP";
        let canonical = "rtsp://User:P%40ss@Camera.Example:554/Live/%aB?token=Secret%2f&Mode=TCP";
        assert_eq!(canonical_scheme(source), canonical);
        assert_eq!(output_args(source).unwrap().last().unwrap(), canonical);
        assert_eq!(
            canonical_scheme(" C:\\My Captures\\record.mkv "),
            " C:\\My Captures\\record.mkv "
        );
        assert_eq!(
            canonical_scheme("C:\\My Captures\\record.mkv"),
            "C:\\My Captures\\record.mkv"
        );
        let log = redact_log(&format!("cannot open {source} or {canonical}"), source);
        assert!(!log.contains("P%40ss"));
        assert!(!log.contains("Secret%2f"));
        assert!(log.contains("Camera.Example:554/Live/%aB"));
        assert!(log.contains("Mode=TCP"));
    }
    #[test]
    fn network_sources_validate_hosts_ports_and_escaping_without_disclosing_credentials() {
        for source in [
            "rtsp://user:secret@camera:554/live",
            "RTSP://camera/live",
            "https://camera/stream?token=a%2Fb",
            "srt://[::1]:8890",
            "udp://@239.1.2.3:5000",
            "tcp://:5000?listen=1",
        ] {
            assert!(validate_source(source).is_ok(), "{source}");
        }
        for source in [
            "rtsp:",
            "rtsp:///live",
            "https://",
            "rtsp://camera:0/live",
            "rtsp://camera:65536/live",
            "rtsp://camera:/live",
            "rtsp://[oops]:554/live",
            "rtsp://::1:554/live",
            "https://camera/a%2",
            "https://camera/a b",
            "avfoundation:?fps=30",
            "v4l2:/dev/video0?fps=30&fps=60",
            "dshow:video=Camera?size=640x480&size=1280x720",
        ] {
            assert!(validate_source(source).is_err(), "{source}");
        }
        let error = validate_source("rtsp://user:secret@camera:99999/live").unwrap_err();
        assert!(error.to_string().contains("1..65535"));
        assert!(!format!("{error:#}").contains("secret"));
        assert!(local_file_path("file://remote/a.mkv").is_err() || cfg!(windows));
        assert_eq!(
            local_file_path("file://localhost/tmp/a.mkv").unwrap(),
            PathBuf::from("/tmp/a.mkv")
        );
        assert_eq!(
            local_file_path("file:///tmp/clip%20%C3%A9%23.mkv").unwrap(),
            PathBuf::from("/tmp/clip é#.mkv")
        );
        assert_eq!(
            local_file_path("file:literal%20name.mkv").unwrap(),
            PathBuf::from("literal%20name.mkv")
        );
        assert!(local_file_path("file:///tmp/clip%00.mkv").is_err());
        assert!(local_file_path("file:///tmp/clip%QQ.mkv").is_err());
    }
    #[test]
    fn forwarding_output_selection() {
        assert_eq!(
            output_args("rtsp://host/live").unwrap(),
            ["-f", "rtsp", "-rtsp_transport", "tcp", "rtsp://host/live"]
        );
        assert!(
            output_args("srt://host:8890")
                .unwrap()
                .contains(&"mpegts".into())
        );
        assert!(output_args("record.m3u8").is_err());
        assert!(recording_output("record.unknown").is_err());
        assert_eq!(
            recording_output("file:record.mp4").unwrap().unwrap().1,
            "mp4"
        );
    }
    #[test]
    fn jetson_inputs_and_bitrates() {
        for (fps, expected) in [
            (30.0, (30, 1)),
            (0.1, (1, 10)),
            (30.12345, (602469, 20000)),
            (30000.0 / 1001.0, (30000, 1001)),
            (240.0, (240, 1)),
        ] {
            let fraction = jetson_framerate(fps).unwrap();
            assert_eq!(fraction, expected);
            assert_eq!(fraction.0 as f64 / fraction.1 as f64, fps);
        }
        let unrepresentable = f64::from_bits(30.0f64.to_bits() + 1);
        assert!(
            jetson_framerate(unrepresentable)
                .unwrap_err()
                .to_string()
                .contains("cannot be represented exactly")
        );
        assert!(jetson_framerate(f64::NAN).is_err());
        assert_eq!(bits_per_second("4M").unwrap(), 4_000_000);
        assert_eq!(bits_per_second("2500k").unwrap(), 2_500_000);
        assert_eq!(bits_per_second("800000").unwrap(), 800_000);
        assert!(bits_per_second("5G").is_err() && bits_per_second("x").is_err());
        let gray = Frame {
            id: 1,
            width: 3,
            height: 3,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: (1..=9).collect(),
        };
        // Luma plane, then 2x2 U and V planes for odd dimensions.
        let i420 = JetsonInput::I420.bytes(&gray).unwrap();
        assert_eq!(i420.len(), 9 + 2 * 4);
        assert_eq!((&i420[..9], &i420[9..]), (&gray.data[..], &[128; 8][..]));
        let rgba = JetsonInput::Rgba.bytes(&gray).unwrap();
        assert_eq!(&rgba[..8], [1, 1, 1, 255, 2, 2, 2, 255]);
        assert!(jetson_encoder("nvv4l2h264enc") && !jetson_encoder("h264_nvenc"));
    }
    #[test]
    fn raw_inputs_unpack_and_widen_what_ffmpeg_cannot_read() {
        let frame = |pixel_format, width, data| Frame {
            id: 1,
            width,
            height: 1,
            pixel_format,
            timestamp_ns: 0,
            data,
        };
        let le =
            |samples: &[u16]| -> Vec<u8> { samples.iter().flat_map(|v| v.to_le_bytes()).collect() };
        // Mono12p 0xabc, 0x123 as gray12le; Mono12Packed the same samples.
        for (format, data) in [
            (0x010c_0047, vec![0xbc, 0x3a, 0x12]),
            (0x010c_0006, vec![0xab, 0x3c, 0x12]),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(
                (raw.ffmpeg, raw.bytes_per_pixel, raw.color),
                ("gray12le", 2, false)
            );
            assert_eq!(
                raw.bytes(frame(format, 2, data)).unwrap(),
                le(&[0xabc, 0x123])
            );
        }
        // 10- and 12-bit Bayer fill FFmpeg's 16-bit Bayer formats.
        let raw = RawInput::for_format(0x0110_0011).unwrap();
        assert_eq!(raw.ffmpeg, "bayer_rggb16le");
        let samples = frame(0x0110_0011, 2, le(&[0xfff, 0x001]));
        assert_eq!(raw.bytes(samples).unwrap(), le(&[0xfff0, 0x0010]));
        let raw = RawInput::for_format(0x010c_0029).unwrap();
        assert_eq!(raw.ffmpeg, "bayer_bggr16le");
        let packed = frame(0x010c_0029, 2, vec![0x55, 0x21, 0xaa]);
        assert_eq!(raw.bytes(packed).unwrap(), le(&[0x155 << 6, 0x2aa << 6]));
        assert!(raw.bytes(frame(0x010c_0029, 2, vec![0x55, 0x21])).is_err());
        for (format, ffmpeg) in [
            (0x0110_0025, "gray14le"),
            (0x0110_002e, "bayer_grbg16le"),
            (0x010a_0056, "bayer_grbg16le"),
            (0x010c_002d, "bayer_bggr16le"),
            (0x0220_0016, "rgba"),
            (0x0220_0017, "bgra"),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!((raw.ffmpeg, raw.full_range), (ffmpeg, false));
            assert!(raw.color_options().is_empty());
        }
        // YUV and YCbCr 4:2:2 go as they are, tagged full-range BT.601.
        for (format, ffmpeg) in [
            (0x0210_001f, "uyvy422"),
            (0x0210_0043, "uyvy422"),
            (0x0210_0032, "yuyv422"),
            (0x0210_003b, "yuyv422"),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(
                (raw.ffmpeg, raw.bytes_per_pixel, raw.color),
                (ffmpeg, 2, true)
            );
            assert_eq!(
                raw.color_options(),
                ["-color_range", "pc", "-colorspace", "bt470bg"]
            );
            let pair = vec![1, 2, 3, 4, 5];
            assert_eq!(raw.bytes(frame(format, 2, pair)).unwrap(), [1, 2, 3, 4]);
        }
        let error = RawInput::for_format(0x0110_0099).unwrap_err().to_string();
        assert!(error.contains("0x01100099") && error.contains("--format raw"));
    }
    #[test]
    fn odd_width_yuv_422_rows_fill_whole_pixel_pairs() {
        // FFmpeg reads 4:2:2 rows of 4 * ceil(width / 2) bytes; a 3x2 frame
        // has rows of 6, so each gains an outside luma and a neutral Cr.
        for format in [0x0210_0032, 0x0210_001f, 0x0210_003b, 0x0210_0043] {
            let raw = RawInput::for_format(format).unwrap();
            let frame = |width, height, data| Frame {
                id: 1,
                width,
                height,
                pixel_format: format,
                timestamp_ns: 0,
                data,
            };
            let padded = raw.bytes(frame(3, 2, (1..=13).collect())).unwrap();
            assert_eq!(
                padded,
                [1, 2, 3, 4, 5, 6, 128, 128, 7, 8, 9, 10, 11, 12, 128, 128],
                "{format:x}"
            );
            assert_eq!(
                raw.bytes(frame(1, 1, vec![9, 7])).unwrap(),
                [9, 7, 128, 128]
            );
            assert!(raw.bytes(frame(3, 2, vec![0; 11])).is_err());
            // Even widths and other formats pass through unchanged.
            let even = (1..=8).collect::<Vec<u8>>();
            assert_eq!(raw.bytes(frame(2, 2, even.clone())).unwrap(), even);
        }
        let mono = RawInput::for_format(crate::types::MONO8).unwrap();
        let odd = Frame {
            id: 1,
            width: 3,
            height: 1,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![1, 2, 3],
        };
        assert_eq!(mono.bytes(odd).unwrap(), [1, 2, 3]);
    }
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_reads_odd_width_yuv_422_like_capturefab() {
        // Chroma is constant along each row, with Cr neutral, so the lone last
        // pixel decodes the same whichever way FFmpeg spreads chroma.
        let exe = executable().unwrap();
        let (width, height) = (5usize, 3usize);
        for (format, order) in [(0x0210_0032, [0, 1, 2, 3]), (0x0210_001f, [1, 0, 3, 2])] {
            let raw = RawInput::for_format(format).unwrap();
            let data = (0..height)
                .flat_map(|y| {
                    let cb = 60 + 40 * y as u8;
                    (0..width).step_by(2).flat_map(move |x| {
                        let pair = [40 + 30 * x as u8, cb, 55 + 30 * x as u8, 128];
                        let bytes = order.map(|i| pair[i]);
                        bytes.into_iter().take(if x + 1 < width { 4 } else { 2 })
                    })
                })
                .collect::<Vec<u8>>();
            assert_eq!(data.len(), width * height * 2);
            let frame = Frame {
                id: 1,
                width: width as u32,
                height: height as u32,
                pixel_format: format,
                timestamp_ns: 0,
                data,
            };
            let expected = crate::frame::rgb(&frame).unwrap();
            let mut child = command(&exe)
                .args(["-hide_banner", "-loglevel", "error", "-f", "rawvideo"])
                .args(["-pix_fmt", raw.ffmpeg])
                .args(raw.color_options())
                .args(["-video_size", &format!("{width}x{height}"), "-i", "pipe:0"])
                .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let bytes = raw.bytes(frame).unwrap();
            let feeder = thread::spawn(move || stdin.write_all(&bytes));
            let output = child.wait_with_output().unwrap();
            feeder.join().unwrap().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{}: {stderr}", raw.ffmpeg);
            assert_eq!(output.stdout.len(), expected.len(), "{}", raw.ffmpeg);
            for (i, (got, want)) in output.stdout.iter().zip(&expected).enumerate() {
                let difference = got.abs_diff(*want);
                assert!(difference <= 3, "{} byte {i}: {got} vs {want}", raw.ffmpeg);
            }
        }
    }
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_raw_inputs_decode_like_capturefab() {
        // Forwarding hands sensor bytes to FFmpeg. Each mapping must describe
        // the same image Capturefab's own converter produces; a swapped Bayer
        // phase or channel order shows up as a large color error.
        let exe = executable().unwrap();
        let (width, height) = (64usize, 48usize);
        let rgb: Vec<[u8; 3]> = (0..width * height)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                [(40 + 2 * x) as u8, (60 + 3 * y) as u8, (200 - x - y) as u8]
            })
            .collect();
        let luma =
            |p: &[u8; 3]| ((p[0] as u32 * 54 + p[1] as u32 * 183 + p[2] as u32 * 19) >> 8) as u16;
        let wide = |shift: u16| -> Vec<u8> {
            rgb.iter()
                .flat_map(|p| (luma(p) << shift).to_le_bytes())
                .collect()
        };
        let lumas: Vec<u16> = rgb.iter().map(luma).collect();
        let deep = |samples: &[u8], shift: u16| -> Vec<u16> {
            samples.iter().map(|&v| u16::from(v) << shift).collect()
        };
        let mosaic = |cfa| bayer(&rgb, width, cfa);
        let (gr, rg, gb, bg) = ([1, 0, 2, 1], [0, 1, 1, 2], [1, 2, 0, 1], [2, 1, 1, 0]);
        // JFIF Y'CbCr, the chroma of each pair averaged, in `order`.
        let yuv = |order: [usize; 4]| -> Vec<u8> {
            let ycbcr = |p: &[u8; 3]| {
                let [r, g, b] = p.map(f64::from);
                [
                    0.299 * r + 0.587 * g + 0.114 * b,
                    -0.168736 * r - 0.331264 * g + 0.5 * b + 128.0,
                    0.5 * r - 0.418688 * g - 0.081312 * b + 128.0,
                ]
            };
            rgb.as_chunks::<2>()
                .0
                .iter()
                .flat_map(|[a, b]| {
                    let (a, b) = (ycbcr(a), ycbcr(b));
                    let values = [a[0], (a[1] + b[1]) / 2.0, b[0], (a[2] + b[2]) / 2.0];
                    order.map(|i| values[i].round().clamp(0.0, 255.0) as u8)
                })
                .collect()
        };
        let formats: Vec<(u32, Vec<u8>)> = vec![
            (
                crate::types::MONO8,
                rgb.iter().map(|p| luma(p) as u8).collect(),
            ),
            (0x0110_0003, wide(2)),
            (0x0110_0005, wide(4)),
            (0x0110_0025, wide(6)),
            (0x0110_0007, wide(8)),
            (crate::types::RGB8, rgb.iter().flatten().copied().collect()),
            (
                0x0218_0015,
                rgb.iter().flat_map(|p| [p[2], p[1], p[0]]).collect(),
            ),
            (
                0x0220_0016,
                rgb.iter().flat_map(|p| [p[0], p[1], p[2], 0x80]).collect(),
            ),
            (
                0x0220_0017,
                rgb.iter().flat_map(|p| [p[2], p[1], p[0], 0x80]).collect(),
            ),
            (0x0108_0008, mosaic(gr)),
            (0x0108_0009, mosaic(rg)),
            (0x0108_000a, mosaic(gb)),
            (0x0108_000b, mosaic(bg)),
            (0x010c_0047, lsb(&deep_luma(&lumas, 4), 12)),
            (0x010c_0004, gige(&deep_luma(&lumas, 2), 10)),
            (0x0110_0011, le(&deep(&mosaic(rg), 4))),
            (0x0110_0030, le(&deep(&mosaic(gb), 8))),
            (0x010a_0052, lsb(&deep(&mosaic(bg), 2), 10)),
            (0x010c_002a, gige(&deep(&mosaic(gr), 4), 12)),
            (0x0210_0032, yuv([0, 1, 2, 3])),
            (0x0210_0043, yuv([1, 0, 3, 2])),
        ];
        fn bayer(rgb: &[[u8; 3]], width: usize, cfa: [usize; 4]) -> Vec<u8> {
            (0..rgb.len())
                .map(|i| rgb[i][cfa[((i / width) & 1) * 2 + ((i % width) & 1)]])
                .collect()
        }
        fn deep_luma(lumas: &[u16], shift: u16) -> Vec<u16> {
            lumas.iter().map(|v| v << shift).collect()
        }
        fn le(samples: &[u16]) -> Vec<u8> {
            samples.iter().flat_map(|v| v.to_le_bytes()).collect()
        }
        // PFNC lsb packing, bit by bit; rows of 64 samples end on whole bytes.
        fn lsb(samples: &[u16], bits: usize) -> Vec<u8> {
            let mut out = vec![0u8; (samples.len() * bits).div_ceil(8)];
            for (i, &v) in samples.iter().enumerate() {
                for b in (0..bits).filter(|b| v >> b & 1 == 1) {
                    out[(i * bits + b) / 8] |= 1 << ((i * bits + b) % 8);
                }
            }
            out
        }
        // GigE Vision Packed: high bits outside, low bits in the middle byte.
        fn gige(samples: &[u16], bits: u16) -> Vec<u8> {
            let low = bits - 8;
            let mask = (1 << low) - 1;
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|&[a, b]| {
                    [
                        (a >> low) as u8,
                        (a & mask | (b & mask) << 4) as u8,
                        (b >> low) as u8,
                    ]
                })
                .collect()
        }
        for (pixel_format, data) in formats {
            let raw = RawInput::for_format(pixel_format).unwrap();
            let frame = Frame {
                id: 1,
                width: width as u32,
                height: height as u32,
                pixel_format,
                timestamp_ns: 0,
                data,
            };
            let expected = crate::frame::rgb(&frame).unwrap();
            let mut child = command(&exe)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                ])
                .arg(raw.ffmpeg)
                .args(raw.color_options())
                .args(["-video_size", &format!("{width}x{height}"), "-i", "pipe:0"])
                .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let bytes = raw.bytes(frame).unwrap();
            let feeder = thread::spawn(move || stdin.write_all(&bytes));
            let output = child.wait_with_output().unwrap();
            feeder.join().unwrap().unwrap();
            assert_eq!(output.stdout.len(), expected.len(), "{}", raw.ffmpeg);
            // Compare away from the border, where demosaic edge rules differ.
            let mut error = 0u64;
            let mut count = 0u64;
            for y in 2..height - 2 {
                for x in 2..width - 2 {
                    for c in 0..3 {
                        let i = (y * width + x) * 3 + c;
                        error += (output.stdout[i] as i64 - expected[i] as i64).unsigned_abs();
                        count += 1;
                    }
                }
            }
            let mean = error as f64 / count as f64;
            assert!(mean < 3.0, "{}: mean error {mean:.2}", raw.ffmpeg);
        }
    }
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_file_decoding_and_recording() {
        let exe = executable().unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "capturefab-media-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        let input = dir.join("source.mkv");
        let output = dir.join("forward.mp4");
        let args = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=5",
            "-frames:v",
            "5",
            "-c:v",
            "ffv1",
            input.to_str().unwrap(),
        ]
        .map(str::to_string);
        let (success, log) = bounded_output(&exe, &args, Duration::from_secs(5)).unwrap();
        assert!(success, "{log}");
        let mut camera = open_url(input.to_str().unwrap(), Duration::from_secs(3)).unwrap();
        camera.start(64 * 48 * 3).unwrap();
        let mut frames = Vec::new();
        for id in 1..=5 {
            let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
            assert_eq!(frame.id, id);
            assert_eq!(
                (frame.width, frame.height, frame.data.len()),
                (64, 48, 64 * 48 * 3)
            );
            frames.push(frame);
        }
        camera.stop().unwrap();
        let mut forward = Forwarder::new(ForwardConfig {
            output: output.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 10.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
            destination: None,
        })
        .unwrap();
        for frame in frames {
            forward.push(frame).unwrap();
            thread::sleep(Duration::from_millis(110));
        }
        forward.stop().unwrap();
        assert!(output.metadata().unwrap().len() > 100);
        assert!(forward.stats().written >= 4);
        let mut replay = open_url(output.to_str().unwrap(), Duration::from_secs(3)).unwrap();
        replay.start(64 * 48 * 3).unwrap();
        assert_eq!(replay.next_frame(Duration::from_secs(3)).unwrap().width, 64);
        replay.stop().unwrap();
        fs::remove_file(input).unwrap();
        fs::remove_file(output).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    #[ignore = "requires FFmpeg with libx264; loads every core for about 10 s"]
    fn ffmpeg_recording_lasts_as_long_as_the_wall_clock_under_load() {
        let dir = temp_dir("timeline");
        let output = dir.join("load.mkv");
        let busy = Arc::new(AtomicBool::new(true));
        let hogs = (0..cores() * 2)
            .map(|_| {
                let busy = busy.clone();
                thread::spawn(move || {
                    let mut x = 1u64;
                    while busy.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(6364136223846793005) + 1);
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut forward = Forwarder::new(ForwardConfig {
            output: output.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 30.0,
            bitrate: "8M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
            destination: None,
        })
        .unwrap();
        let (width, height) = (1920usize, 1080usize);
        // Moving content keeps the encoder busy.
        let pictures = (0..8u8)
            .map(|shift| {
                (0..width * height * 3)
                    .map(|i| (i / 3 % width) as u8 ^ (i / (3 * width)) as u8 ^ (shift * 32))
                    .collect::<Vec<u8>>()
            })
            .collect::<Vec<_>>();
        let interval = Duration::from_secs_f64(1.0 / 30.0);
        let mut next = Instant::now();
        let mut started = None;
        for id in 0u64.. {
            forward
                .push(Frame {
                    id,
                    width: width as u32,
                    height: height as u32,
                    pixel_format: RGB8,
                    timestamp_ns: 0,
                    data: pictures[id as usize % pictures.len()].clone(),
                })
                .unwrap();
            // The recording starts when FFmpeg takes its first frame.
            if started.is_none() && forward.stats().written > 0 {
                started = Some(Instant::now());
            }
            if started.is_some_and(|started| started.elapsed() >= Duration::from_secs(10)) {
                break;
            }
            next += interval;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        let wall = started.unwrap().elapsed().as_secs_f64();
        let stats = forward.stats();
        forward.stop().unwrap();
        busy.store(false, Ordering::Relaxed);
        for hog in hogs {
            hog.join().unwrap();
        }
        let (frames, duration) = recorded(&output);
        eprintln!(
            "wall {wall:.3} s, file {duration:.3} s, frames {frames}, accepted {}, written {}, late {}",
            stats.accepted, stats.written, stats.late
        );
        assert!(
            (duration - wall).abs() / wall < 0.02,
            "{duration} vs {wall}"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    /// A recording's frame count and the seconds its packets span.
    fn recorded(path: &std::path::Path) -> (usize, f64) {
        let exe = executable().unwrap();
        let args = ["-hide_banner", "-i", path.to_str().unwrap()]
            .into_iter()
            .chain(["-c", "copy", "-f", "framecrc", "-"])
            .map(str::to_string)
            .collect::<Vec<_>>();
        let (ok, log) = bounded_output(&exe, &args, Duration::from_secs(60)).unwrap();
        assert!(ok, "{log}");
        let base = log
            .lines()
            .find_map(|line| line.strip_prefix("#tb 0: "))
            .and_then(|base| base.split_once('/'))
            .map(|(num, den)| {
                num.trim().parse::<f64>().unwrap() / den.trim().parse::<f64>().unwrap()
            })
            .unwrap();
        // stream, dts, pts, duration, size, checksum
        let packets = log
            .lines()
            .filter(|line| line.starts_with("0,"))
            .map(|line| {
                let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
                let number = |index: usize| fields[index].parse::<f64>().unwrap();
                (number(2), number(2) + number(3))
            })
            .collect::<Vec<_>>();
        let start = packets.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let end = packets.iter().map(|p| p.1).fold(0.0, f64::max);
        (packets.len(), (end - start) * base)
    }
    #[test]
    #[ignore = "requires FFmpeg with libx264"]
    fn ffmpeg_recording_leaves_a_gap_for_a_stall_and_stops_at_once() {
        let dir = temp_dir("gap");
        let output = dir.join("gap.mkv");
        let mut forward = Forwarder::new(ForwardConfig {
            output: output.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
            destination: None,
        })
        .unwrap();
        let (width, height) = (1280usize, 720usize);
        let push = |forward: &mut Forwarder, id: u64| {
            forward
                .push(Frame {
                    id,
                    width: width as u32,
                    height: height as u32,
                    pixel_format: RGB8,
                    timestamp_ns: 0,
                    data: vec![(id * 8) as u8; width * height * 3],
                })
                .unwrap();
            thread::sleep(Duration::from_secs_f64(1.0 / 30.0));
        };
        let started = Instant::now();
        for id in 0..30 {
            push(&mut forward, id);
        }
        // The camera is gone for 8 s, then returns.
        thread::sleep(Duration::from_secs(8));
        let resumed = Instant::now();
        for id in 30..60 {
            push(&mut forward, id);
        }
        let catching_up = resumed.elapsed();
        let wall = started.elapsed().as_secs_f64();
        let stopping = Instant::now();
        forward.stop().unwrap();
        let stopped = stopping.elapsed();
        let (frames, duration) = recorded(&output);
        eprintln!(
            "wall {wall:.3} s, file {duration:.3} s, frames {frames}, resumed in {catching_up:?}, stopped in {stopped:?}"
        );
        // Each frame once, the outage a gap rather than copies to encode.
        assert!(frames <= 60, "{frames}");
        assert!((duration - wall).abs() < 0.5, "{duration} vs {wall}");
        assert!(catching_up < Duration::from_secs(2), "{catching_up:?}");
        assert!(stopped < Duration::from_secs(2), "{stopped:?}");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[ignore = "requires FFmpeg and permission to bind localhost sockets"]
    fn ffmpeg_stalled_network_input_times_out_and_cancels() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("tcp://{}", listener.local_addr().unwrap());
        let (release, stop) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            let _ = stop.recv_timeout(Duration::from_secs(5));
        });
        let start = Instant::now();
        let mut decoder = Decoder::spawn(&url, Duration::from_millis(150)).unwrap();
        assert!(decoder.next(Duration::from_millis(150)).is_err());
        decoder.stop();
        assert!(start.elapsed() < Duration::from_secs(2));
        let _ = release.send(());
        server.join().unwrap();
    }
    #[test]
    #[ignore = "requires FFmpeg and permission to bind localhost sockets"]
    fn ffmpeg_stalled_forwarding_has_bounded_latest_frame_queue() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let output = format!("tcp://{}", listener.local_addr().unwrap());
        let (release, stop) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            socket2::SockRef::from(&stream)
                .set_recv_buffer_size(1024)
                .unwrap();
            let _ = stop.recv_timeout(Duration::from_secs(8));
        });
        let mut forward = Forwarder::new(ForwardConfig {
            output,
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 60.0,
            bitrate: "32M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
            destination: None,
        })
        .unwrap();
        let mut noise = 123u32;
        let data = (0..640 * 480 * 3)
            .map(|_| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                noise as u8
            })
            .collect::<Vec<_>>();
        let frame = Frame {
            id: 1,
            width: 640,
            height: 480,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data,
        };
        forward.push(frame.clone()).unwrap();
        let start = Instant::now();
        for _ in 0..120 {
            forward.push(frame.clone()).unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(forward.stats().dropped > 0);
        let stop_started = Instant::now();
        let _ = forward.stop();
        assert!(stop_started.elapsed() < Duration::from_secs(5));
        let _ = release.send(());
        server.join().unwrap();
    }
    #[test]
    #[ignore = "requires FFmpeg"]
    fn ffmpeg_forwarding_reports_file_muxer_failure() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capturefab-invalid-encoder-{nonce}.mp4"));
        let mut forward = Forwarder::new(ForwardConfig {
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "capturefab_nonexistent_encoder".into(),
            fps: 30.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
            destination: None,
        })
        .unwrap();
        let frame = Frame {
            id: 1,
            width: 64,
            height: 48,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data: vec![0; 64 * 48 * 3],
        };
        let error = forward.push(frame.clone()).unwrap_err();
        assert!(format!("{error:#}").contains("is unavailable in this FFmpeg build"));
        forward.stop().unwrap();
        assert!(
            !path.exists(),
            "invalid recordings must be removed and release their reservation"
        );
    }
    #[test]
    #[ignore = "requires FFmpeg"]
    fn ffmpeg_local_recording_reservation_is_bounded_and_removed_on_failure() {
        // A full disk is not an encoder failure, so auto does not fall back.
        for encoder in ["libx264", "auto"] {
            bounded_recording_is_removed_on_failure(encoder);
        }
    }
    fn bounded_recording_is_removed_on_failure(encoder: &str) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capturefab-bounded-recording-{nonce}.mp4"));
        let mut forward = Forwarder::new(ForwardConfig {
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: encoder.into(),
            fps: 30.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: 512,
            destination: None,
        })
        .unwrap();
        let frame = Frame {
            id: 1,
            width: 64,
            height: 48,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data: vec![127; 64 * 48 * 3],
        };
        forward.push(frame.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while forward.stats().error.is_none() && Instant::now() < deadline {
            let _ = forward.push(frame.clone());
            thread::sleep(Duration::from_millis(20));
        }
        let encoder = forward.stats().encoder;
        let error = forward.stop().unwrap_err().to_string();
        assert!(error.contains("Storage full"), "{error}");
        assert_eq!(forward.stats().encoder, encoder);
        assert!(
            !path.exists(),
            "a recording cut before it holds any video must be removed"
        );
    }
    #[test]
    #[ignore = "requires FFmpeg"]
    fn ffmpeg_a_recording_that_reaches_its_maximum_size_keeps_the_video_before() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capturefab-capped-recording-{nonce}.mkv"));
        let limit = 256 << 10;
        let mut forward = Forwarder::new(ForwardConfig {
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 30.0,
            bitrate: "8M".into(),
            storage: Default::default(),
            max_file_bytes: limit,
            destination: None,
        })
        .unwrap();
        // Noise compresses poorly, so the limit comes within seconds.
        let mut seed = 1u32;
        let deadline = Instant::now() + Duration::from_secs(20);
        for id in 1.. {
            if forward.stats().error.is_some() || Instant::now() >= deadline {
                break;
            }
            let data = (0..160 * 120 * 3)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            let frame = Frame {
                id,
                width: 160,
                height: 120,
                pixel_format: RGB8,
                timestamp_ns: id * 33_333_333,
                data,
            };
            let _ = forward.push(frame);
            thread::sleep(Duration::from_millis(33));
        }
        let error = forward.stop().unwrap_err().to_string();
        assert!(error.contains("maximum file size"), "{error}");
        let recorded = forward
            .stats()
            .recording
            .expect("the recording up to its limit is kept");
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes > HEADER_BYTES && bytes <= limit, "{bytes}");
        assert_eq!(recorded.bytes, bytes);
        let exe = executable().unwrap();
        let args = [
            "-v",
            "error",
            "-i",
            &path.to_string_lossy(),
            "-f",
            "framemd5",
            "-",
        ]
        .map(String::from);
        let (_, log) = bounded_output(&exe, &args, Duration::from_secs(30)).unwrap();
        let frames = log.lines().filter(|line| line.starts_with("0,")).count();
        assert!(frames > 0, "{log}");
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn forwarding_configuration_fails_closed() {
        assert!(validate_hwaccel("cuda").is_ok());
        assert!(validate_hwaccel("unexpected").is_err());
        let config = ForwardConfig {
            output: "video.mp4".into(),
            codec: "h264".into(),
            encoder: "auto".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
            destination: None,
        };
        assert!(Forwarder::new(config.clone()).is_ok());
        let mut serialized = serde_json::to_value(&config).unwrap();
        serialized["bitarte"] = json!("8M");
        assert!(
            serde_json::from_value::<ForwardConfig>(serialized)
                .unwrap_err()
                .to_string()
                .contains("unknown field `bitarte`")
        );
        for bitrate in [
            "0",
            "0M",
            "NaN",
            "inf",
            "1.2.3M",
            "4MM",
            "5G",
            "0.0000001M",
            "4294967296",
        ] {
            let mut invalid = config.clone();
            invalid.bitrate = bitrate.into();
            assert!(invalid.validate().is_err(), "{bitrate}");
        }
        for (bitrate, expected) in [
            ("2.01k", 2010),
            (".5M", 500_000),
            ("1.000000M", 1_000_000),
            ("0.000001M", 1),
            ("4294967295", u32::MAX as u64),
        ] {
            assert_eq!(bits_per_second(bitrate).unwrap(), expected, "{bitrate}");
        }
        for encoder in ["", "-i", "libx264;", "libx265"] {
            let mut invalid = config.clone();
            invalid.encoder = encoder.into();
            assert!(invalid.validate().is_err(), "{encoder}");
        }
        assert!(validate_encoder("av1", "libaom-av1").is_ok());
        for output in [
            "https://",
            "rtsp://camera:70000/live",
            "file:",
            "video.mp4/",
            "video.webm",
        ] {
            let mut invalid = config.clone();
            invalid.output = output.into();
            assert!(invalid.validate().is_err(), "{output}");
        }
        let mut invalid = config;
        invalid.fps = f64::NAN;
        assert!(Forwarder::new(invalid).is_err());
    }
    #[test]
    fn recording_sizes_are_estimated_and_reserved_with_headroom() {
        const MIB: u64 = 1024 * 1024;
        let hour = 3_600_000;
        // 4 Mbit/s for an hour is 1.8 GB, far above the default 512 MiB cap.
        assert_eq!(estimate_recording_bytes("4M", hour).unwrap(), 1_800_000_000);
        assert_eq!(estimate_recording_bytes("1", 1).unwrap(), 1);
        assert_eq!(estimate_recording_bytes("4M", 0).unwrap(), 0);
        assert_eq!(
            estimate_recording_bytes("4294967295", crate::scheduling::MAX_DURATION_MS).unwrap(),
            169_423_574_885_865_000
        );
        assert!(estimate_recording_bytes("4MM", hour).is_err());
        // 1.8 GB plus 25% and 16 MiB is 2,266,777,216 bytes: 2162 MiB, rounded up.
        let reserved = recording_reservation("4M", hour).unwrap();
        assert_eq!(reserved, 2162 * MIB);
        assert!(reserved >= 1_800_000_000 / 4 * 5 + 16 * MIB);
        assert_eq!(recording_reservation("4M", 0).unwrap(), 16 * MIB);
        assert_eq!(recording_reservation("1", 1).unwrap(), 17 * MIB);
        assert_eq!(
            recording_reservation("4294967295", crate::scheduling::MAX_DURATION_MS).unwrap() % MIB,
            0
        );
        assert!(recording_reservation("", hour).is_err());
    }
    #[test]
    fn the_longest_recording_that_fits_is_in_whole_minutes() {
        let minute = 60_000;
        // 4 Mbit/s reserves about 36 MiB a minute plus 16 MiB.
        let longest = max_duration_for("4M", default_recording_cap()).unwrap();
        assert_eq!(longest, 13 * minute);
        assert!(recording_reservation("4M", longest).unwrap() <= default_recording_cap());
        assert!(recording_reservation("4M", longest + minute).unwrap() > default_recording_cap());
        let hour = recording_reservation("4M", 60 * minute).unwrap();
        assert_eq!(max_duration_for("4M", hour).unwrap(), 60 * minute);
        assert_eq!(max_duration_for("4M", hour - 1).unwrap(), 59 * minute);
        // Not even a minute: the 16 MiB margin alone, or nothing.
        assert_eq!(max_duration_for("4M", 16 * 1024 * 1024).unwrap(), 0);
        assert_eq!(max_duration_for("4M", 0).unwrap(), 0);
        assert_eq!(
            max_duration_for("1", u64::MAX).unwrap(),
            crate::scheduling::MAX_DURATION_MS
        );
        assert!(max_duration_for("fast", u64::MAX).is_err());
    }
    #[test]
    fn host_cameras_are_named_by_their_operating_system_scheme() {
        for selector in [
            "avfoundation:FaceTime HD Camera",
            "avfoundation:0",
            "AVFoundation:1?fps=30",
            "v4l2:/dev/video0",
            "dshow:video=Integrated Camera",
        ] {
            assert!(is_host_camera(selector), "{selector}");
        }
        for selector in [
            "sim:0",
            "rtsp://camera/live",
            "file:clip.mp4",
            "clip.mp4",
            "C:/video/clip.mp4",
            "gige:192.168.1.20",
            "scanner:sane:x",
            "",
        ] {
            assert!(!is_host_camera(selector), "{selector}");
        }
    }
    #[test]
    fn explicit_encoders_must_be_installed_video_encoders_for_the_selected_codec() {
        let listing = " V....D libx264 H.264 (codec h264)\n V....D libaom-av1 AV1 (codec av1)\n A..... aac AAC\n V..... mjpeg Motion JPEG\n V....D custom New encoder (codec hevc)\n";
        assert!(validate_encoder_listing(listing, "libx264", "h264").is_ok());
        assert!(validate_encoder_listing(listing, "libaom-av1", "av1").is_ok());
        assert!(validate_encoder_listing(listing, "custom", "hevc").is_ok());
        for (encoder, family) in [
            ("libx264", "av1"),
            ("aac", "h264"),
            ("mjpeg", "h264"),
            ("missing", "h264"),
        ] {
            assert!(validate_encoder_listing(listing, encoder, family).is_err());
        }
    }
    const C920_MODES: &str = "\
[in#0 @ 0x77dac14000] Selected video size (16x16) is not supported by the device.
[in#0 @ 0x77dac14000] Supported modes:
[in#0 @ 0x77dac14000]   1280x720@[30.000030 30.000030]fps
[in#0 @ 0x77dac14000]   1280x720@[24.000038 24.000038]fps
[in#0 @ 0x77dac14000]   1280x720@[20.000000 20.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[15.000015 15.000015]fps
[in#0 @ 0x77dac14000]   1280x720@[10.000000 10.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[7.500002 7.500002]fps
[in#0 @ 0x77dac14000]   1280x720@[5.000000 5.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[10.000000 10.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[7.500002 7.500002]fps
[in#0 @ 0x77dac14000]   1280x720@[5.000000 5.000000]fps
[in#0 @ 0x77dac14000]   1920x1080@[30.000030 30.000030]fps
[in#0 @ 0x77dac14000]   1920x1080@[24.000038 24.000038]fps
[in#0 @ 0x77dac14000]   1920x1080@[20.000000 20.000000]fps
[in#0 @ 0x77dac14000]   1920x1080@[15.000015 15.000015]fps
[in#0 @ 0x77dac14000]   1920x1080@[10.000000 10.000000]fps
[in#0 @ 0x77dac14000]   1920x1080@[7.500002 7.500002]fps
[in#0 @ 0x77dac14000]   1920x1080@[5.000000 5.000000]fps
[in#0 @ 0x77da808000] Error opening input: Input/output error
Error opening input file 0:none.
Error opening input files: Input/output error
";
    const IPHONE_MODES: &str = "\
[in#0 @ 0x7ad0c24000] Selected video size (16x16) is not supported by the device.
[in#0 @ 0x7ad0c24000] Supported modes:
[in#0 @ 0x7ad0c24000]   640x480@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c24000]   640x480@[1.000000 60.000000]fps
[in#0 @ 0x7ad0c24000]   1280x720@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c24000]   1280x720@[2.000000 60.000000]fps
[in#0 @ 0x7ad0c24000]   1920x1080@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c24000]   1920x1080@[2.000000 60.000000]fps
[in#0 @ 0x7ad0c24000]   1920x1440@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c20000] Error opening input: Input/output error
Error opening input file 2:none.
Error opening input files: Input/output error
";
    fn labels(modes: &[Mode]) -> Vec<String> {
        modes.iter().map(Mode::label).collect()
    }
    fn native(source: &str, modes: Vec<Mode>) -> MediaBackend {
        MediaBackend {
            source: source.into(),
            timeout: Duration::from_secs(1),
            decoder: None,
            first: None,
            width: 0,
            height: 0,
            native: true,
            requested_size: false,
            fps: 0.0,
            running: false,
            modes,
            mode: None,
            formats: Vec::new(),
            reconnect: None,
            paced: true,
            on_demand: None,
        }
    }
    #[test]
    fn avfoundation_modes_keep_range_maxima_of_the_last_format_per_size() {
        assert_eq!(
            labels(&avfoundation_modes(C920_MODES)),
            [
                "1280x720@10",
                "1280x720@7.5",
                "1280x720@5",
                "1920x1080@30",
                "1920x1080@24",
                "1920x1080@20",
                "1920x1080@15",
                "1920x1080@10",
                "1920x1080@7.5",
                "1920x1080@5"
            ]
        );
        let iphone = avfoundation_modes(IPHONE_MODES);
        // Continuity Camera: the 60 fps format of each size also runs at the
        // 30 fps its sibling format lists.
        assert_eq!(
            labels(&iphone),
            [
                "640x480@60",
                "640x480@30",
                "1280x720@60",
                "1280x720@30",
                "1920x1080@60",
                "1920x1080@30",
                "1920x1440@30"
            ]
        );
        assert!(iphone[0].honors(59.995) && !iphone[0].honors(59.98) && !iphone[0].honors(30.0));
        assert!(iphone[1].honors(30.0) && !iphone.iter().any(|m| m.honors(45.0)));
        let mut camera = native("avfoundation:iPhone Camera", iphone.clone());
        camera
            .configure(NativeOptions {
                size: Some((1920, 1080)),
                fps: Some(30.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options()[..4],
            ["-framerate", "30", "-video_size", "1920x1080"]
        );
        assert_eq!(
            iphone[best(&iphone, |_| true).unwrap()].label(),
            "1920x1440@30"
        );
        let slow =
            [(1600, 896, 7.5), (640, 480, 15.0), (1280, 720, 10.0)].map(|(width, height, fps)| {
                Mode {
                    width,
                    height,
                    fps: Some((fps, fps)),
                    input_format: None,
                    intervals: None,
                    min_size: None,
                }
            });
        assert_eq!(slow[best(&slow, |_| true).unwrap()].label(), "640x480@15");
    }
    #[test]
    fn native_video_modes_are_chosen_validated_and_switched() {
        let mut camera = native(
            "avfoundation:HD Pro Webcam C920",
            avfoundation_modes(C920_MODES),
        );
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "30",
                "-video_size",
                "1920x1080",
                "-pixel_format",
                "uyvy422"
            ]
        );
        let rejected = NativeOptions {
            size: Some((1280, 720)),
            fps: Some(30.0),
        };
        assert_eq!(
            camera.configure(rejected).unwrap_err().to_string(),
            "1280x720@30 is not supported by HD Pro Webcam C920; supported: 1280x720@10, 1280x720@7.5, 1280x720@5, 1920x1080@30"
        );
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: None,
            })
            .unwrap();
        assert_eq!((camera.width, camera.height, camera.fps), (1280, 720, 10.0));
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@10");
        assert_eq!(nodes.choices(&mut camera, "VideoMode").unwrap().len(), 10);
        nodes.set(&mut camera, "Width", "1920").unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1920x1080@10");
        let error = nodes
            .set(&mut camera, "AcquisitionFrameRate", "29.97")
            .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("1920x1080@29.97 is not supported by HD Pro Webcam C920; supported: 1920x1080@30, 1920x1080@24,")
        );
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "30")
            .unwrap();
        nodes.set(&mut camera, "VideoMode", "1280x720@5").unwrap();
        assert_eq!((camera.width, camera.height, camera.fps), (1280, 720, 5.0));
        assert!(nodes.set(&mut camera, "Height", "600").is_err());
        assert!(camera.write_memory(28, &10u32.to_le_bytes()).is_err());
        camera.running = true;
        let locked = nodes.set(&mut camera, "VideoMode", "1920x1080@30");
        assert!(locked.unwrap_err().to_string().contains("locked"));
        let many = native(
            "dshow:video=Capture Card",
            (1..=12)
                .map(|i| Mode {
                    width: 160 * i,
                    height: 90 * i,
                    fps: Some((5.0, 60.0)),
                    input_format: None,
                    intervals: None,
                    min_size: None,
                })
                .collect(),
        );
        let message = many.unsupported(100, 100, 30.0).to_string();
        assert!(
            message.starts_with(
                "100x100@30 is not supported by Capture Card; supported: 1920x1080@60, "
            )
        );
        assert_eq!(message.matches('@').count(), 9);
        let mut stream = native("rtsp://camera/live", Vec::new());
        stream.native = false;
        assert!(
            !NodeMap::parse(&stream.xml().unwrap())
                .unwrap()
                .has("VideoMode")
        );
    }
    #[test]
    fn dshow_and_v4l2_listings_become_honored_modes() {
        let dshow = dshow_modes(concat!(
            "[dshow @ 000001b5e2a6c3c0] DirectShow video device options (from video devices)\n",
            "[dshow @ 000001b5e2a6c3c0]  Pin \"Capture\" (alternative pin name \"0\")\n",
            "[dshow @ 000001b5e2a6c3c0]   vcodec=mjpeg  min s=1920x1080 fps=10 max s=1920x1080 fps=30\n",
            "[dshow @ 000001b5e2a6c3c0]   vcodec=mjpeg  min s=1920x1080 fps=10 max s=1920x1080 fps=30 (pc, bt470bg/bt709/unknown, center)\n",
            "[dshow @ 000001b5e2a6c3c0]   pixel_format=yuyv422  min s=1920x1080 fps=5 max s=1920x1080 fps=30\n",
            "[dshow @ 000001b5e2a6c3c0]   pixel_format=yuyv422  min s=640x480 fps=7.5 max s=640x480 fps=29.97(left)\n",
        ));
        assert_eq!(labels(&dshow), ["1920x1080@30", "640x480@29.97"]);
        assert_eq!(dshow[0].fps, Some((5.0, 30.0)));
        let mut camera = native("dshow:video=Integrated Webcam", dshow);
        camera
            .configure(NativeOptions {
                size: None,
                fps: Some(15.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options(),
            ["-framerate", "15", "-video_size", "1920x1080"]
        );
        // A driver that enumerates no frame intervals leaves rates unbounded.
        let (modes, format) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Compressed:       mjpeg :          Motion-JPEG : 1920x1080 1280x720 640x480\n",
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       : Unsupported :         10-bit Bayer : 4056x3040\n",
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720 640x480\n",
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       :        gray :            8-bit Greyscale : {8-1920, 8}x{8-1080, 8}\n",
            ),
            |_, _| None,
        );
        assert_eq!(labels(&modes), ["640x480", "1280x720", "1920x1080"]);
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = format;
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            ["-framerate", "30", "-input_format", "yuyv422"]
        );
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: Some(60.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "60",
                "-video_size",
                "1280x720",
                "-input_format",
                "yuyv422"
            ]
        );
        let nodes = crate::genicam::NodeMap::parse(&camera.xml().unwrap()).unwrap();
        assert_eq!(
            nodes.choices(&mut camera, "VideoMode").unwrap(),
            ["640x480", "1280x720", "1920x1080"]
        );
        nodes.set(&mut camera, "VideoMode", "1920x1080").unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "60",
                "-video_size",
                "1920x1080",
                "-input_format",
                "mjpeg"
            ]
        );
        nodes.set(&mut camera, "VideoMode", "640x480").unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "60",
                "-video_size",
                "640x480",
                "-input_format",
                "yuyv422"
            ]
        );
    }
    #[test]
    fn dshow_rates_are_requested_as_the_listed_frame_intervals() {
        // A Blackmagic DeckLink listing from FFmpeg's capture wiki: each
        // capability has a single frame interval, which the requested rate
        // must reproduce exactly.
        let decklink = dshow_modes(concat!(
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=23.976 max s=1920x1080 fps=23.976\n",
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=24 max s=1920x1080 fps=24\n",
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=29.97 max s=1920x1080 fps=29.97\n",
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=60.0002 max s=1920x1080 fps=60.0002\n",
        ));
        assert_eq!(
            decklink.iter().map(|m| m.intervals).collect::<Vec<_>>(),
            [
                Some((417083, 417083)),
                Some((416666, 416666)),
                Some((333667, 333667)),
                Some((166666, 166666))
            ]
        );
        let mut camera = native("dshow:video=Decklink Video Capture", decklink);
        for (fps, rate, interval) in [
            (23.976, "23.976014395", 417083),
            (24.0, "24.000009600", 416666),
            (60.0, "60.000060000", 166666),
        ] {
            camera
                .configure(NativeOptions {
                    size: Some((1920, 1080)),
                    fps: Some(fps),
                })
                .unwrap();
            assert_eq!(camera.input_options()[..2], ["-framerate", rate]);
            // FFmpeg floors 1e7 / rate into 100 ns units.
            assert_eq!((1e7 / rate.parse::<f64>().unwrap()) as u32, interval);
        }
        // A webcam whose fastest interval is 333334 rather than 333333:
        // asking for 30 fps would fall outside its range.
        let webcam = dshow_modes(
            "[dshow @ 000001b5e2a6c3c0]   pixel_format=yuyv422  min s=1280x720 fps=5 max s=1280x720 fps=29.9999\n",
        );
        assert_eq!(webcam[0].intervals, Some((333335, 1999999)));
        let mut camera = native("dshow:video=USB Camera", webcam);
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(camera.input_options()[..2], ["-framerate", "29.999805001"]);
        camera
            .configure(NativeOptions {
                size: None,
                fps: Some(5.0),
            })
            .unwrap();
        assert_eq!(camera.input_options()[..2], ["-framerate", "5.000001250"]);
        camera
            .configure(NativeOptions {
                size: None,
                fps: Some(15.0),
            })
            .unwrap();
        assert_eq!(camera.input_options()[..2], ["-framerate", "15"]);
    }
    #[test]
    fn dshow_size_ranges_take_the_sizes_inside_them() {
        let modes = dshow_modes(concat!(
            "[dshow @ 000002a3c8b4e0c0]   vcodec=mjpeg  min s=1920x1080 fps=5 max s=1920x1080 fps=30\n",
            "[dshow @ 000002a3c8b4e0c0]   pixel_format=bgr24  min s=320x240 fps=1 max s=1920x1080 fps=60\n",
        ));
        assert_eq!(modes[0].min_size, None);
        assert_eq!(modes[1].min_size, Some((320, 240)));
        let mut camera = native("dshow:video=Capture Filter", modes);
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: Some(30.0),
            })
            .unwrap();
        assert_eq!(camera.mode, Some(1));
        assert_eq!(
            camera.input_options(),
            ["-framerate", "30", "-video_size", "1280x720"]
        );
        let nodes = crate::genicam::NodeMap::parse(&camera.xml().unwrap()).unwrap();
        nodes.set(&mut camera, "Width", "640").unwrap();
        nodes.set(&mut camera, "Height", "480").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "15")
            .unwrap();
        assert_eq!(
            camera.input_options(),
            ["-framerate", "15", "-video_size", "640x480"]
        );
        assert!(
            camera
                .configure(NativeOptions {
                    size: Some((160, 120)),
                    fps: None,
                })
                .is_err()
        );
    }
    #[test]
    fn v4l2_modes_take_a_compressed_format_where_raw_cannot_reach_the_rate() {
        // The UVC camera of FFmpeg trac #9878, whose v4l2-ctl listing gives
        // YUYV 1280x720 at 10 fps and every other format and size at 30.
        let (modes, formats) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x5694e62800] Compressed:       mjpeg :          Motion-JPEG : 1280x720 320x240 640x360 640x480\n",
                "[video4linux2,v4l2 @ 0x5694e62800] Raw       :     yuyv422 :           YUYV 4:2:2 : 1280x720 320x240 640x360 640x480\n",
            ),
            |description, size| match (description, size) {
                ("YUYV 4:2:2", (1280, 720)) => Some((10.0, 10.0)),
                ("YUYV 4:2:2" | "Motion-JPEG", _) => Some((30.0, 30.0)),
                _ => None,
            },
        );
        assert_eq!(formats, ["yuyv422", "mjpeg"]);
        assert_eq!(
            labels(&modes),
            [
                "1280x720@10",
                "320x240@30",
                "640x360@30",
                "640x480@30",
                "1280x720@30"
            ]
        );
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = formats;
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "30",
                "-video_size",
                "1280x720",
                "-input_format",
                "mjpeg"
            ]
        );
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: Some(10.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "10",
                "-video_size",
                "1280x720",
                "-input_format",
                "yuyv422"
            ]
        );
        let rejected = NativeOptions {
            size: Some((1280, 720)),
            fps: Some(60.0),
        };
        assert_eq!(
            camera.configure(rejected).unwrap_err().to_string(),
            "1280x720@60 is not supported by /dev/video0; supported: 1280x720@30, 1280x720@10, 640x480@30, 640x360@30, 320x240@30"
        );
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "30")
            .unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@30");
        assert_eq!(camera.input_options()[5], "mjpeg");
        assert!(
            nodes
                .set(&mut camera, "AcquisitionFrameRate", "15")
                .is_err()
        );
        nodes.set(&mut camera, "VideoMode", "1280x720@10").unwrap();
        assert_eq!(camera.fps, 10.0);
        assert_eq!(camera.input_options()[5], "yuyv422");
    }
    #[test]
    fn v4l2_raw_format_is_kept_where_it_reaches_the_requested_rate() {
        // A C920's uvcvideo intervals: YUYV 1280x720 at 10, 7.5 and 5 fps,
        // and MJPEG 1280x720 and both 640x480 formats at 30 down to 5.
        let (modes, formats) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720\n",
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Compressed:       mjpeg :          Motion-JPEG : 640x480 1280x720\n",
            ),
            |description, size| match (description, size) {
                ("YUYV 4:2:2", (1280, 720)) => Some((5.0, 10.0)),
                _ => Some((5.0, 30.0)),
            },
        );
        assert_eq!(labels(&modes), ["640x480@30", "1280x720@10", "1280x720@30"]);
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = formats;
        let format = |camera: &MediaBackend| camera.input_options()[5].clone();
        for (fps, expected) in [
            (Some(10.0), "yuyv422"),
            (Some(30.0), "mjpeg"),
            (None, "mjpeg"),
        ] {
            let size = Some((1280, 720));
            camera.configure(NativeOptions { size, fps }).unwrap();
            assert_eq!(format(&camera), expected, "{fps:?}");
        }
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        nodes.set(&mut camera, "VideoMode", "1280x720@10").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "10")
            .unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@10");
        assert_eq!(format(&camera), "yuyv422");
        nodes.set(&mut camera, "VideoMode", "1280x720@30").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "10")
            .unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@30");
        nodes.set(&mut camera, "VideoMode", "640x480@30").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "10")
            .unwrap();
        nodes.set(&mut camera, "Width", "1280").unwrap();
        nodes.set(&mut camera, "Height", "720").unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@10");
        assert_eq!(format(&camera), "yuyv422");
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "30")
            .unwrap();
        assert_eq!(format(&camera), "mjpeg");
    }
    #[test]
    fn v4l2_modes_prefer_mjpeg_over_h264() {
        // The original C920 (046d:082d) lists YUYV, then H.264, then MJPEG,
        // with both compressed formats at 30 down to 5 fps.
        let (modes, formats) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720 1920x1080\n",
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Compressed:        h264 :                H.264 : 640x480 1280x720 1920x1080\n",
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Compressed:       mjpeg :          Motion-JPEG : 640x480 1280x720 1920x1080\n",
            ),
            |description, size| match (description, size) {
                ("YUYV 4:2:2", (1280, 720)) => Some((5.0, 10.0)),
                ("YUYV 4:2:2", (1920, 1080)) => Some((5.0, 5.0)),
                _ => Some((5.0, 30.0)),
            },
        );
        assert_eq!(formats, ["yuyv422", "mjpeg", "h264"]);
        assert_eq!(
            modes.iter().map(|m| m.input_format).collect::<Vec<_>>(),
            [Some(0), Some(0), Some(0), Some(1), Some(1)]
        );
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = formats;
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "30",
                "-video_size",
                "1920x1080",
                "-input_format",
                "mjpeg"
            ]
        );
    }
    #[test]
    #[cfg(unix)]
    fn discovery_command_distinguishes_timeout_from_expected_nonzero_exit() {
        let exe = Executable::external("/bin/sh".into());
        let start = Instant::now();
        let (success, _, timed_out) = timed_output(
            &exe,
            &["-c".into(), "exec sleep 5".into()],
            &[],
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(!success);
        assert!(timed_out);
        assert!(start.elapsed() < Duration::from_secs(2));
        let (success, log, timed_out) = timed_output(
            &exe,
            &["-c".into(), "echo devices; exit 1".into()],
            &[],
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(!success);
        assert!(!timed_out);
        assert_eq!(log.trim(), "devices");
    }
    #[test]
    fn native_errors_give_platform_specific_recovery() {
        for (source, log, guidance) in [
            (
                "v4l2:/dev/video0",
                "Cannot open video device /dev/video0: Permission denied",
                "owning group",
            ),
            (
                "dshow:video=Webcam",
                "Could not run graph (sometimes caused by a device already in use): 0x80070005",
                "Windows Settings",
            ),
        ] {
            let message = native_failure(log, false, source, Duration::from_secs(2)).unwrap();
            assert!(message.contains(guidance));
            assert_eq!(crate::cli::error_code(&anyhow!(message)).1, 3);
            let timeout = native_failure("", true, source, Duration::from_secs(2)).unwrap();
            assert!(!timeout.contains("MacBook"));
            assert!(timeout.contains("selected video mode"));
        }
        let busy = native_failure(
            "Device or resource busy",
            false,
            "v4l2:/dev/video0",
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(crate::cli::error_code(&anyhow!(busy)).1, 3);
        assert!(
            native_failure(
                "unknown decoding error",
                false,
                "v4l2:/dev/video0",
                Duration::from_secs(2)
            )
            .is_none()
        );
        for (node, log) in [
            (
                "/dev/video1",
                concat!(
                    "[video4linux2,v4l2 @ 0x55d0c8a0c0] ioctl(VIDIOC_G_INPUT): Inappropriate ioctl for device\n",
                    "[in#0 @ 0x55d0c8a000] Error opening input: Inappropriate ioctl for device\n",
                    "Error opening input file /dev/video1.\n",
                ),
            ),
            (
                "/dev/video3",
                concat!(
                    "[video4linux2,v4l2 @ 0x7f3a1c4c40] ioctl(VIDIOC_G_INPUT): Not a tty\n",
                    "[in#0 @ 0x7f3a1c4b80] Error opening input: Not a tty\n",
                    "Error opening input file /dev/video3.\n",
                ),
            ),
            (
                "/dev/video10",
                concat!(
                    "[video4linux2,v4l2 @ 0x55d0c8a0c0] Not a video capture device.\n",
                    "[in#0 @ 0x55d0c8a000] Error opening input: No such device\n",
                    "Error opening input file /dev/video10.\n",
                ),
            ),
        ] {
            let source = format!("v4l2:{node}");
            let message = native_failure(log, false, &source, Duration::from_secs(2)).unwrap();
            assert_eq!(
                message,
                format!(
                    "video capture is not supported by {node}, which may be a metadata, codec or ISP node; run capturefab native to list capture nodes"
                )
            );
            assert_eq!(crate::cli::error_code(&anyhow!(message)).1, 5);
        }
    }
    #[test]
    fn v4l2_discovery_lists_capture_nodes_only() {
        // A Raspberry Pi with a USB webcam: uvcvideo's metadata node shares
        // the camera's name, and the codec, subdevice and VBI nodes sit in
        // the same class.
        let class = temp_dir("video4linux");
        for (node, name) in [
            ("video0", "HD Pro Webcam C920"),
            ("video1", "HD Pro Webcam C920"),
            ("video10", "bcm2835-codec-decode"),
            ("video2", "unicam-image"),
            ("v4l-subdev0", "imx219 10-0010"),
            ("vbi0", "saa7134"),
        ] {
            fs::create_dir(class.join(node)).unwrap();
            fs::write(class.join(node).join("name"), format!("{name}\n")).unwrap();
        }
        let devices = v4l2_devices(&class, |path| {
            !matches!(path.to_str(), Some("/dev/video1" | "/dev/video10"))
        });
        assert_eq!(
            devices,
            [
                json!({"id":"v4l2:/dev/video0","name":"HD Pro Webcam C920","backend":"v4l2"}),
                json!({"id":"v4l2:/dev/video2","name":"unicam-image","backend":"v4l2"}),
            ]
        );
        fs::remove_dir_all(class).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(!captures_by(deadline, "/dev/video1".as_ref(), |_| false));
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        let wedged = |_: &std::path::Path| {
            thread::sleep(Duration::from_secs(5));
            false
        };
        assert!(captures_by(deadline, "/dev/video0".as_ref(), wedged));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn v4l2_locators_accept_persistent_udev_links() {
        let by_path = "/dev/v4l/by-path/pci-0000:00:14.0-usb-0:1:1.0-video-index0";
        for path in [
            "/dev/video0",
            "/dev/v4l/by-id/usb-046d_HD_Pro_Webcam_C920_8A4F3D5F-video-index0",
            by_path,
            "/dev/v4l/by-path/platform-fe801000.csi-video-index0",
        ] {
            assert!(v4l2_path(path), "{path}");
        }
        for path in [
            "/dev/v4l/by-id/",
            "/dev/v4l/by-id/..",
            "/dev/v4l/by-path/../../sda",
            "/dev/v4l-subdev0",
            "/dev/sda",
        ] {
            assert!(!v4l2_path(path), "{path}");
        }
        let source = format!("v4l2:{by_path}?size=1280x720");
        validate_source_shape(&source).unwrap();
        let (base, options) = native_source(&source).unwrap();
        assert_eq!(base, format!("v4l2:{by_path}"));
        assert_eq!(options.unwrap().size, Some((1280, 720)));
        let missing = native_failure(
            &format!("[video4linux2,v4l2 @ 0x55d0c8a0c0] Cannot open video device {by_path}: No such file or directory\n"),
            false,
            &base,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(
            missing,
            format!(
                "camera device not found: {by_path}; reconnect the device and run capturefab native to obtain its current locator"
            )
        );
    }
    #[test]
    fn dshow_devices_keep_video_sources_and_tell_identical_cameras_apart() {
        let devices = dshow_devices(concat!(
            "[dshow @ 000001eb780baf00] \"HD Pro Webcam C920\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_046d&pid_082d&mi_00#7&1d6d0b0e&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"HD Pro Webcam C920\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"USB Capture HDMI\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_2935&pid_0006&mi_00#8&3027539&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"Game Capture HD60 S\" (audio, video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_0fd9&pid_004e&mi_00#7&2bc4c6a&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"OBS Virtual Camera\" (none)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}\"\n",
            "[dshow @ 000001eb780baf00] \"Studio: Left\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\\{5C2CD55C-92AD-4999-8666-912BD3E70010}\"\n",
            "[dshow @ 000001eb780baf00] \"VoiceMeeter Aux Output (VB-Audio VoiceMeeter AUX VAIO)\" (audio)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\\wave_{024F58BD-C3C7-401F-B905-455306BF3F33}\"\n",
            "[dshow @ 000001eb780baf00] \"Line In (Realtek Audio)\" (none)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\\wave_{0881BEEB-1C3E-4C22-997E-C1A757D757DF}\"\n",
            "dummy: Immediate exit requested\n",
        ));
        let ids = devices
            .iter()
            .map(|device| device["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                r"dshow:video=@device_pnp_\\%3F\usb#vid_046d&pid_082d&mi_00#7&1d6d0b0e&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global",
                r"dshow:video=@device_pnp_\\%3F\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global",
                "dshow:video=USB Capture HDMI",
                "dshow:video=Game Capture HD60 S",
                "dshow:video=OBS Virtual Camera",
                r"dshow:video=@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\{5C2CD55C-92AD-4999-8666-912BD3E70010}",
            ]
        );
        assert_eq!(devices[1]["name"], "HD Pro Webcam C920");
        assert_eq!(devices[5]["name"], "Studio: Left");
        let (base, options) = native_source(&format!("{}?size=1280x720&fps=30", ids[1])).unwrap();
        assert_eq!(base, ids[1]);
        assert_eq!(options.unwrap().size, Some((1280, 720)));
        assert_eq!(
            dshow_input(base.strip_prefix("dshow:").unwrap()),
            r"video=@device_pnp_\\?\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global"
        );
        assert_eq!(
            dshow_input("video=HD Pro Webcam C920"),
            "video=HD Pro Webcam C920"
        );
        let pasted = native_source(r"dshow:video=@device_pnp_\\?\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global").unwrap_err();
        assert!(format!("{pasted}").contains("%3F"), "{pasted}");
    }
    #[test]
    fn native_devices_prefer_names_and_failures_are_concise() {
        let devices = avfoundation_devices(concat!(
            "[AVFoundation indev @ 0x7b45020140] AVFoundation video devices:\n",
            "[AVFoundation indev @ 0x7b45020140] [0] HD Pro Webcam C920\n",
            "[AVFoundation indev @ 0x7b45020140] [1] iPhone Camera\n",
            "[AVFoundation indev @ 0x7b45020140] [2] FaceTime HD Camera\n",
            "[AVFoundation indev @ 0x7b45020140] [3] Capture screen 0\n",
            "[AVFoundation indev @ 0x7b45020140] [4] Cam\n",
            "[AVFoundation indev @ 0x7b45020140] [5] Cam 2\n",
            "[AVFoundation indev @ 0x7b45020140] [6] 4K Camera\n",
            "[AVFoundation indev @ 0x7b45020140] [7] Desk: Left\n",
            "[AVFoundation indev @ 0x7b45020140] AVFoundation audio devices:\n",
            "[AVFoundation indev @ 0x7b45020140] [0] MacBook Pro Microphone\n",
            "[in#0 @ 0x7b45020000] Error opening input: Input/output error\n",
        ));
        let ids = devices
            .iter()
            .map(|device| device["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "avfoundation:HD Pro Webcam C920",
                "avfoundation:iPhone Camera",
                "avfoundation:FaceTime HD Camera",
                "avfoundation:Capture screen 0",
                "avfoundation:4",
                "avfoundation:Cam 2",
                "avfoundation:6",
                "avfoundation:7"
            ]
        );
        assert_eq!(devices[7]["name"], "Desk: Left");
        assert_eq!(device_name("avfoundation:2"), "avfoundation:2");
        assert_eq!(
            device_name("dshow:video=Integrated Webcam"),
            "Integrated Webcam"
        );
        let timeout = Duration::from_secs(5);
        let failure = |log| native_failure(log, false, "avfoundation:FaceTime HD Camera", timeout);
        assert_eq!(
            failure("[in#0 @ 0x1] Failed to create AV capture input device: Cannot use FaceTime HD Camera").unwrap(),
            "camera access denied: allow this app in System Settings > Privacy & Security > Camera"
        );
        for (source, log) in [
            (
                "avfoundation:FaceTime HD Camera",
                "Cannot use FaceTime HD Camera",
            ),
            (
                "dshow:video=Integrated Webcam",
                "Error opening input: access denied",
            ),
            ("v4l2:/dev/video0", "Permission denied"),
        ] {
            let denied = native_failure(log, false, source, timeout).unwrap();
            assert!(is_camera_access_denied(&denied), "{denied}");
            assert!(is_camera_access_denied(&format!(
                "connect avfoundation:0: {denied}\nmore detail"
            )));
        }
        assert!(!is_camera_access_denied(
            "camera is in use by another application"
        ));
        let blocked = native_failure(
            "[dshow @ 000001b5e2a6c3c0] Could not run graph (sometimes caused by a device already in use by other application)\n[in#0 @ 000001b5e2a6c000] Error opening input: I/O error\n",
            false,
            "dshow:video=Integrated Webcam",
            timeout,
        )
        .unwrap();
        assert!(is_camera_access_denied(&blocked), "{blocked}");
        assert!(blocked.contains("in use by another application"));
        assert!(blocked.contains("Privacy & security > Camera"));
        assert_eq!(crate::cli::error_code(&anyhow!(blocked)).1, 3);
        assert!(!is_camera_access_denied("timed out\ncamera access denied"));
        assert_eq!(
            failure("[in#0 @ 0x1] Failed to create AV capture input device: Cannot Use FaceTime HD Camera").unwrap(),
            "camera is in use by another application"
        );
        let missing = failure("[in#0 @ 0x1] Video device not found").unwrap();
        assert!(missing.contains("run capturefab native"));
        assert_eq!(crate::cli::error_code(&anyhow!(missing)).1, 3);
        let stalled = native_failure("", true, "avfoundation:FaceTime HD Camera", timeout).unwrap();
        assert_eq!(
            stalled,
            "no frames from FaceTime HD Camera (timed out after 5 s): the camera may be suspended (for example a closed MacBook lid), in use, or blocked by permissions"
        );
        assert_eq!(crate::cli::error_code(&anyhow!(stalled)).1, 4);
    }
}
