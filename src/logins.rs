//! Camera sign-ins: user names and passwords for rtsp://, rtsps:// and
//! onvif: cameras. Passwords live only in the OS credential store (service
//! [`KEYCHAIN_SERVICE`], account `USER@ADDRESS`); the index of which
//! addresses have one (`camera-logins.json` in the state directory) never
//! holds a secret. As with hubs, the worker that opens the camera is the
//! process that reads the saved password, and only after a camera answered
//! with a sign-in request is one ever sent.
//!
//! The address a sign-in is saved under is normalized by [`key`]: scheme,
//! host and port are compared case-insensitively with the default port made
//! explicit, the path is compared exactly, and a query or fragment is
//! ignored. A changed user name or address therefore never reads another
//! entry's secret.
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroizing;

pub const KEYCHAIN_SERVICE: &str = "capturefab-camera";
const INDEX: &str = "camera-logins.json";
const LOCK: &str = "camera-logins.lock";
const MAX_FILE: usize = 1 << 20;
const MAX_LOGINS: usize = 1024;
const MAX_USER: usize = 1024;
/// The keychain value is `v1:` followed by the password, so an empty
/// password still fits the store's 1..=4096 byte limit.
const SECRET_PREFIX: &str = "v1:";
const MAX_PASSWORD: usize = 4096 - SECRET_PREFIX.len();
const SCHEMES: &str = "sign-in is supported for rtsp://, rtsps:// and onvif: addresses";

/// Whether this build can save sign-ins in the OS credential store.
pub fn can_remember() -> bool {
    cfg!(feature = "keychain")
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    crate::dataset::Usage(message.into()).into()
}

/// The normalized address a sign-in is saved under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoginKey {
    pub address: String,
}

/// Normalize a credential-free camera address to the address its sign-in
/// is saved under.
pub fn key(selector: &str) -> Result<LoginKey> {
    let selector = selector.trim();
    ensure!(
        selector.len() <= 16384 && !selector.chars().any(char::is_control),
        usage("camera address is too long or contains control characters")
    );
    if crate::onvif::is_source(selector) {
        if authority_of(&selector[6..]).is_some_and(|authority| authority.contains('@')) {
            return Err(usage(
                "leave the user name and password out of the address; use --user and --password-stdin",
            ));
        }
        let address = crate::onvif::endpoint_key(selector).map_err(|error| {
            if crate::types::error_transport_code(&error) == Some("usage") {
                error
            } else {
                usage(format!("{error:#}"))
            }
        })?;
        return Ok(LoginKey { address });
    }
    let (scheme, rest) = selector.split_once("://").ok_or_else(|| usage(SCHEMES))?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "rtsp" => 554,
        "rtsps" => 322,
        _ => return Err(usage(SCHEMES)),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return Err(usage(
            "leave the user name and password out of the address; use --user and --password-stdin",
        ));
    }
    let (host, port) = split_host_port(authority, default_port)
        .ok_or_else(|| usage("camera address needs a host and an optional numeric port"))?;
    let tail = &rest[end..];
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let path = match &tail[..path_end] {
        "" => "/",
        path => path,
    };
    Ok(LoginKey {
        address: format!("{scheme}://{host}:{port}{path}"),
    })
}

/// `host` lowercased (IPv6 kept in brackets) and the port, defaulted.
pub(crate) fn split_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        (format!("[{}]", host.to_ascii_lowercase()), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_ascii_lowercase(), port.parse().ok()?)
    } else {
        (authority.to_ascii_lowercase(), default_port)
    };
    (!host.is_empty() && host != "[]" && port > 0).then_some((host, port))
}

/// The authority after `scheme://`, when the text has one.
fn authority_of(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The keychain account: user name and address, so changing either never
/// reads another entry's secret.
pub fn account(user: &str, address: &str) -> String {
    format!("{}@{address}", crate::hubs::encode(user))
}

/// A short form for messages and the UI: the address without its scheme and
/// default port, for example `cam.local/live`.
pub fn short(address: &str) -> String {
    let address = crate::media::redact_url(address);
    let rest = address
        .strip_prefix("onvif:")
        .unwrap_or(&address)
        .split_once("://")
        .map_or(address.as_str(), |(_, rest)| rest);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let authority = [":554", ":322", ":80"]
        .iter()
        .find_map(|port| authority.strip_suffix(port))
        .unwrap_or(authority);
    let tail = if tail == "/" { "" } else { tail };
    format!("{authority}{tail}")
}

/// A camera user name and password. The password is cleared from memory when
/// the last copy drops and never appears in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct CameraLogin {
    pub user: String,
    password: Zeroizing<String>,
}

impl CameraLogin {
    pub fn new(user: impl Into<String>, password: impl Into<String>) -> Result<Self> {
        let user = user.into();
        let password = Zeroizing::new(password.into());
        ensure!(
            (1..=MAX_USER).contains(&user.len()),
            usage(format!("camera user name must contain 1..{MAX_USER} bytes"))
        );
        ensure!(
            password.len() <= MAX_PASSWORD,
            usage(format!(
                "camera password must contain at most {MAX_PASSWORD} bytes"
            ))
        );
        ensure!(
            !user.chars().any(char::is_control) && !password.chars().any(char::is_control),
            usage("camera user names and passwords must not contain control characters")
        );
        crate::hubs::scrub::register(&password);
        Ok(Self { user, password })
    }
    pub fn expose(&self) -> &str {
        &self.password
    }
}

impl std::fmt::Debug for CameraLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraLogin")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Plaintext, for the worker pipe and the token-authenticated loopback IPC
/// only; the remote API refuses commands that carry a login.
impl Serialize for CameraLogin {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("CameraLogin", 2)?;
        state.serialize_field("user", &self.user)?;
        state.serialize_field("password", self.password.as_str())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CameraLogin {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            user: String,
            password: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        let password = Zeroizing::new(wire.password);
        CameraLogin::new(wire.user, password.as_str()).map_err(serde::de::Error::custom)
    }
}

/// Percent-decode URL userinfo.
pub(crate) fn percent_decode(text: &str) -> Result<String> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(text.len()));
    let input = text.as_bytes();
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            ensure!(
                index + 2 < input.len(),
                "truncated ONVIF credential percent escape"
            );
            let hex = std::str::from_utf8(&input[index + 1..index + 3])?;
            bytes.push(
                u8::from_str_radix(hex, 16).context("invalid ONVIF credential percent escape")?,
            );
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(std::mem::take(&mut *bytes))
        .map_err(|_| anyhow!("ONVIF credentials are not UTF-8"))?;
    ensure!(
        !decoded.chars().any(char::is_control),
        "ONVIF credentials must not contain control characters"
    );
    Ok(decoded)
}

/// Byte range of the `user[:password]@` prefix of the authority, if any.
fn userinfo_range(url: &str) -> Option<(usize, usize)> {
    let start = url.find("://")? + 3;
    let end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |n| start + n);
    let at = url[start..end].rfind('@')?;
    Some((start, start + at + 1))
}

/// Split credentials out of an address: the address without them, and the
/// decoded login when it had one.
pub fn split_userinfo(url: &str) -> Result<(String, Option<CameraLogin>)> {
    let Some((start, end)) = userinfo_range(url) else {
        return Ok((url.to_owned(), None));
    };
    let userinfo = &url[start..end - 1];
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let password = Zeroizing::new(percent_decode(password)?);
    let login = CameraLogin::new(percent_decode(user)?, password.as_str())?;
    Ok((format!("{}{}", &url[..start], &url[end..]), Some(login)))
}

/// Best-effort removal of credentials from text that may be a half-typed
/// address, for undo history. A complete `user[:password]@` prefix is
/// removed; so is a `:password` still being typed before its `@` (a
/// non-numeric port), keeping the user name.
pub fn strip_userinfo(text: &str) -> String {
    if let Some((start, end)) = userinfo_range(text) {
        return format!("{}{}", &text[..start], &text[end..]);
    }
    let Some(start) = text.find("://").map(|n| n + 3) else {
        return text.to_owned();
    };
    let end = text[start..]
        .find(['/', '?', '#'])
        .map_or(text.len(), |n| start + n);
    let authority = &text[start..end];
    if !authority.starts_with('[')
        && let Some(colon) = authority.find(':')
        && !authority[colon + 1..].bytes().all(|b| b.is_ascii_digit())
    {
        return format!("{}{}", &text[..start + colon], &text[end..]);
    }
    text.to_owned()
}

/// The address with these credentials, percent-encoded, in its authority.
pub fn with_credentials(url: &str, login: &CameraLogin) -> Result<Zeroizing<String>> {
    let start = url.find("://").context("camera address has no authority")? + 3;
    let stripped_from = userinfo_range(url).map_or(start, |(_, end)| end);
    Ok(Zeroizing::new(format!(
        "{}{}:{}@{}",
        &url[..start],
        crate::hubs::encode(&login.user),
        crate::hubs::encode(login.expose()),
        &url[stripped_from..]
    )))
}

/// The error for a camera that asked for, or refused, a sign-in.
/// `address` must already be credential-free.
pub fn auth_error(address: &str, had_credentials: bool) -> anyhow::Error {
    let address = crate::media::redact_url(address);
    if had_credentials {
        return Categorized::error(
            ErrorKind::AuthRejected,
            format!("{address} refused the user name or password"),
        );
    }
    let advice = if can_remember() {
        format!(
            "sign in from the app or run `capturefab login add {address} --user USER --password-stdin`"
        )
    } else {
        let scheme = address.split("://").next().unwrap_or("rtsp");
        format!("include them in the address ({scheme}://USER:PASSWORD@…)")
    };
    Categorized::error(
        ErrorKind::AuthRequired,
        format!("{address} needs a user name and password; {advice}"),
    )
}

/// Whether an error is a sign-in request or refusal.
pub fn is_auth_error(error: &anyhow::Error) -> bool {
    matches!(
        crate::types::error_transport_code(error),
        Some("auth_required" | "auth_rejected")
    )
}

/// Where secrets are kept: the OS credential store, or memory in tests.
pub(crate) trait Store: Send + Sync {
    fn read(&self, account: &str) -> Result<Option<Zeroizing<String>>>;
    fn write(&self, account: &str, secret: &str) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

#[cfg(all(feature = "keychain", not(test)))]
struct SystemStore;
#[cfg(all(feature = "keychain", not(test)))]
impl Store for SystemStore {
    fn read(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        Ok(crate::keychain::read_secret(KEYCHAIN_SERVICE, account)?.map(Zeroizing::new))
    }
    fn write(&self, account: &str, secret: &str) -> Result<()> {
        crate::keychain::store_secret(KEYCHAIN_SERVICE, account, secret)
    }
    fn delete(&self, account: &str) -> Result<()> {
        crate::keychain::delete_secret(KEYCHAIN_SERVICE, account)
    }
}

/// A store in memory, for tests and screenshots.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryStore {
    pub(crate) secrets: std::sync::Mutex<std::collections::BTreeMap<String, Zeroizing<String>>>,
    pub(crate) reads: std::sync::atomic::AtomicUsize,
    pub(crate) fail: std::sync::atomic::AtomicBool,
}
#[cfg(test)]
impl Store for MemoryStore {
    fn read(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        use std::sync::atomic::Ordering;
        self.reads.fetch_add(1, Ordering::SeqCst);
        ensure!(
            !self.fail.load(Ordering::SeqCst),
            "the credential store is locked"
        );
        Ok(self.secrets.lock().unwrap().get(account).cloned())
    }
    fn write(&self, account: &str, secret: &str) -> Result<()> {
        ensure!(
            !secret.is_empty() && secret.len() <= 4096,
            "secret must contain 1..4096 bytes"
        );
        ensure!(
            !self.fail.load(std::sync::atomic::Ordering::SeqCst),
            "the credential store is locked"
        );
        self.secrets
            .lock()
            .unwrap()
            .insert(account.into(), Zeroizing::new(secret.into()));
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.secrets.lock().unwrap().remove(account);
        Ok(())
    }
}

/// One saved sign-in, as listed. Never holds the password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub address: String,
    pub user: String,
    pub saved_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    logins: Vec<Entry>,
}

/// What [`Logins::save`] replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub entry: Entry,
    pub replaced: bool,
}

/// A saved sign-in looked up for a camera that is being opened.
#[derive(Debug)]
pub enum Remembered {
    /// No sign-in is saved for this address.
    None,
    Login(CameraLogin),
    /// The index names a sign-in whose secret is gone from the store.
    Missing {
        user: String,
    },
}

/// The saved sign-ins of one state directory and credential store.
#[derive(Clone)]
pub struct Logins {
    dir: PathBuf,
    store: Option<Arc<dyn Store>>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn unsupported() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Unsupported,
        "saving camera passwords in the OS credential store is unsupported in this build; put the credentials in the address instead",
    )
}

fn missing(address: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::Unavailable,
        format!("no saved sign-in for {address}; list them with `capturefab login list`"),
    )
}

impl Logins {
    /// The real state directory and OS credential store. Unit tests get a
    /// shared in-memory store instead, so they never reach a keychain.
    pub fn system() -> Self {
        #[cfg(test)]
        {
            static STORE: std::sync::OnceLock<Arc<MemoryStore>> = std::sync::OnceLock::new();
            let store: Arc<dyn Store> = STORE.get_or_init(Arc::default).clone();
            Self::with(crate::ipc::state_dir(), store)
        }
        #[cfg(not(test))]
        {
            #[cfg(feature = "keychain")]
            let store: Option<Arc<dyn Store>> = Some(Arc::new(SystemStore));
            #[cfg(not(feature = "keychain"))]
            let store: Option<Arc<dyn Store>> = None;
            Self {
                dir: crate::ipc::state_dir(),
                store,
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn with(dir: PathBuf, store: Arc<dyn Store>) -> Self {
        Self {
            dir,
            store: can_remember().then_some(store),
        }
    }
    /// The index alone, for listing without any credential store.
    pub fn index_only(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            store: None,
        }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    fn path(&self) -> PathBuf {
        self.dir.join(INDEX)
    }
    fn read(&self) -> Result<File> {
        let path = self.path();
        let input = match fs::File::open(&path) {
            Ok(input) => input,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(File {
                    version: 1,
                    logins: Vec::new(),
                });
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut bytes = Vec::new();
        input
            .take((MAX_FILE + 1) as u64)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {}", path.display()))?;
        ensure!(
            bytes.len() <= MAX_FILE,
            "the saved camera sign-in list is too large"
        );
        let file: File = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid saved camera sign-in list {}", path.display()))?;
        ensure!(
            file.version == 1,
            "unsupported saved camera sign-in list version"
        );
        ensure!(
            file.logins.len() <= MAX_LOGINS,
            "too many saved camera sign-ins (maximum {MAX_LOGINS})"
        );
        Ok(file)
    }
    fn write(&self, file: &File) -> Result<()> {
        crate::ipc::ensure_private_dir(&self.dir)?;
        let temp = self.dir.join(format!("{INDEX}.{}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> Result<()> {
            let mut output = options.open(&temp)?;
            let bytes = serde_json::to_vec_pretty(file)?;
            ensure!(
                bytes.len() <= MAX_FILE,
                "the saved camera sign-in list is too large"
            );
            output.write_all(&bytes)?;
            output.sync_all()?;
            fs::rename(&temp, self.path())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    fn open_lock(&self) -> Result<fs::File> {
        crate::ipc::ensure_private_dir(&self.dir)?;
        Ok(fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK))?)
    }
    /// Run `update` under the cross-process lock and write the result. When
    /// writing fails, `rollback` undoes what `update` did outside the file.
    fn locked<T>(
        &self,
        update: impl FnOnce(&mut File) -> Result<T>,
        rollback: impl FnOnce(&T) -> Result<()>,
    ) -> Result<T> {
        use fs2::FileExt;
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let mut file = self.read()?;
        file.version = 1;
        let result = update(&mut file)?;
        if let Err(error) = self.write(&file) {
            rollback(&result)
                .context("restore the saved password after the sign-in list could not be saved")?;
            return Err(error);
        }
        Ok(result)
    }
    fn store(&self) -> Result<&Arc<dyn Store>> {
        self.store.as_ref().ok_or_else(unsupported)
    }

    pub fn list(&self) -> Result<Vec<Entry>> {
        Ok(self.read()?.logins)
    }
    /// The saved entry for an address (index only).
    pub fn entry(&self, address: &str) -> Result<Option<Entry>> {
        Ok(self
            .read()?
            .logins
            .into_iter()
            .find(|entry| entry.address == address))
    }
    pub fn is_saved(&self, address: &str) -> Result<bool> {
        Ok(self.entry(address)?.is_some())
    }
    /// Save a sign-in that worked. The secret is written first; when the
    /// index cannot be written the previous secret is restored.
    pub fn save(&self, address: &str, login: &CameraLogin) -> Result<Saved> {
        let store = self.store()?.clone();
        let account = account(&login.user, address);
        let secret = Zeroizing::new(format!("{SECRET_PREFIX}{}", login.expose()));
        let previous = store.read(&account)?;
        store.write(&account, &secret)?;
        let restore = |previous: &Option<Zeroizing<String>>| match previous {
            Some(previous) => store.write(&account, previous),
            None => store.delete(&account),
        };
        let entry = Entry {
            address: address.to_owned(),
            user: login.user.clone(),
            saved_at: now(),
            last_used_at: None,
        };
        let result = self.locked(
            |file| {
                let old = file
                    .logins
                    .iter()
                    .position(|other| other.address == address)
                    .map(|index| file.logins.remove(index));
                ensure!(
                    file.logins.len() < MAX_LOGINS,
                    "too many saved camera sign-ins (maximum {MAX_LOGINS})"
                );
                file.logins.push(entry.clone());
                file.logins.sort_by(|a, b| a.address.cmp(&b.address));
                Ok(old)
            },
            |_| restore(&previous),
        );
        let old = match result {
            Ok(old) => old,
            Err(error) => {
                // `locked` failed before writing (for example the lock or a
                // corrupt list): the new secret must not stay behind.
                let _ = restore(&previous);
                return Err(error);
            }
        };
        if let Some(old) = &old
            && old.user != login.user
        {
            let _ = store.delete(&self::account(&old.user, address));
        }
        Ok(Saved {
            entry,
            replaced: old.is_some(),
        })
    }
    /// Forget a saved sign-in: the index entry first, then its secret.
    /// Returns whether the secret was deleted.
    pub fn forget(&self, address: &str) -> Result<bool> {
        let store = self.store()?.clone();
        let removed = self.locked(
            |file| {
                let index = file
                    .logins
                    .iter()
                    .position(|entry| entry.address == address)
                    .ok_or_else(|| missing(address))?;
                Ok(file.logins.remove(index))
            },
            |_| Ok(()),
        )?;
        Ok(store.delete(&account(&removed.user, address)).is_ok())
    }
    /// The saved sign-in for an address. The index is read first, so an
    /// address without an entry never touches the credential store.
    pub fn remembered(&self, address: &str) -> Result<Remembered> {
        let Some(store) = &self.store else {
            return Ok(Remembered::None);
        };
        let Some(entry) = self.entry(address)? else {
            return Ok(Remembered::None);
        };
        let secret = store.read(&account(&entry.user, address)).map_err(|error| {
            Categorized::error(
                ErrorKind::OperationFailed,
                format!(
                    "cannot read the saved sign-in for {} from the OS credential store: {error:#}; unlock it and try again",
                    short(address)
                ),
            )
        })?;
        let Some(secret) = secret else {
            return Ok(Remembered::Missing { user: entry.user });
        };
        let password = secret.strip_prefix(SECRET_PREFIX).ok_or_else(|| {
            Categorized::error(
                ErrorKind::OperationFailed,
                "saved sign-in has an unknown format; sign in again",
            )
        })?;
        Ok(Remembered::Login(CameraLogin::new(entry.user, password)?))
    }
    /// Record when a saved sign-in was last used. Best effort: skipped when
    /// another process holds the lock, and never an error.
    pub fn touch(&self, address: &str) {
        use fs2::FileExt;
        let Ok(lock) = self.open_lock() else {
            return;
        };
        if lock.try_lock_exclusive().is_err() {
            return;
        }
        let Ok(mut file) = self.read() else {
            return;
        };
        let Some(entry) = file.logins.iter_mut().find(|e| e.address == address) else {
            return;
        };
        entry.last_used_at = Some(now());
        let _ = self.write(&file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-logins-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }
    fn logins(name: &str) -> (Logins, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::default());
        (Logins::with(temp(name), store.clone()), store)
    }

    #[test]
    fn keys_normalize_scheme_host_port_and_path() {
        for (input, expected) in [
            ("rtsp://Cam.Local/live", "rtsp://cam.local:554/live"),
            ("RTSP://cam.local:554/live", "rtsp://cam.local:554/live"),
            ("rtsps://cam/x", "rtsps://cam:322/x"),
            ("rtsp://cam:8554", "rtsp://cam:8554/"),
            ("rtsp://cam/Live?token=1#f", "rtsp://cam:554/Live"),
            ("rtsp://[FE80::1]/s", "rtsp://[fe80::1]:554/s"),
            ("rtsp://[::1]:9000/s", "rtsp://[::1]:9000/s"),
            ("onvif://cam", "onvif:http://cam:80/onvif/device_service"),
            (
                "onvif:http://CAM:80/onvif/device_service",
                "onvif:http://cam:80/onvif/device_service",
            ),
        ] {
            assert_eq!(key(input).unwrap().address, expected, "{input}");
        }
        assert_eq!(
            key("onvif://cam").unwrap(),
            key("onvif:http://cam/onvif/device_service").unwrap()
        );
        for bad in [
            "rtsp://u:p@cam/x",
            "onvif://u:p@cam",
            "http://cam/x",
            "sim:0",
            "rtsp://cam:port/x",
            "rtsp:///x",
        ] {
            let error = key(bad).unwrap_err();
            assert_eq!(
                crate::types::error_transport_code(&error),
                Some("usage"),
                "{bad}: {error:#}"
            );
            assert!(!format!("{error:#}").contains(":p@"), "{error:#}");
        }
    }

    #[test]
    fn accounts_and_short_addresses() {
        assert_eq!(
            account("ad min", "rtsp://cam:554/live"),
            "ad%20min@rtsp://cam:554/live"
        );
        assert_eq!(short("rtsp://cam:554/live"), "cam/live");
        assert_eq!(short("rtsp://cam:8554/"), "cam:8554");
        assert_eq!(
            short("onvif:http://cam:80/onvif/device_service"),
            "cam/onvif/device_service"
        );
    }

    #[test]
    fn logins_redact_and_round_trip() {
        let login = CameraLogin::new("admin", "hunter2-secret").unwrap();
        let debug = format!("{login:?}");
        assert!(
            debug.contains("admin") && !debug.contains("hunter2"),
            "{debug}"
        );
        let json = serde_json::to_string(&login).unwrap();
        let back: CameraLogin = serde_json::from_str(&json).unwrap();
        assert_eq!(back, login);
        assert!(serde_json::from_str::<CameraLogin>(r#"{"user":"","password":"x"}"#).is_err());
        assert!(
            serde_json::from_str::<CameraLogin>(r#"{"user":"a","password":"x","extra":1}"#)
                .is_err()
        );
        assert!(CameraLogin::new("a", "x\ny").is_err());
        assert!(CameraLogin::new("a", "x".repeat(MAX_PASSWORD + 1)).is_err());
        assert!(CameraLogin::new("a", "").is_ok());
    }

    #[test]
    fn userinfo_splits_strips_and_joins() {
        let (url, login) = split_userinfo("rtsp://ad%40min:p%3Aw%2F%25%C3%A9@cam/live").unwrap();
        assert_eq!(url, "rtsp://cam/live");
        let login = login.unwrap();
        assert_eq!((login.user.as_str(), login.expose()), ("ad@min", "p:w/%é"));
        let joined = with_credentials(&url, &login).unwrap();
        assert_eq!(
            joined.as_str(),
            "rtsp://ad%40min:p%3Aw%2F%25%C3%A9@cam/live"
        );
        assert_eq!(
            with_credentials("rtsp://old:x@cam/live", &login)
                .unwrap()
                .as_str(),
            joined.as_str()
        );
        assert_eq!(
            split_userinfo("rtsp://cam/a@b").unwrap().0,
            "rtsp://cam/a@b"
        );
        assert!(split_userinfo("rtsp://cam/a@b").unwrap().1.is_none());
        assert_eq!(split_userinfo("onvif://u@cam").unwrap().0, "onvif://cam");
        for (typed, expected) in [
            ("rtsp://admin:pw@cam/live", "rtsp://cam/live"),
            ("rtsp://admin:pw", "rtsp://admin"),
            ("rtsp://admin:p", "rtsp://admin"),
            ("rtsp://cam:554/live", "rtsp://cam:554/live"),
            ("rtsp://[::1]:554/x", "rtsp://[::1]:554/x"),
            ("rtsp://admin", "rtsp://admin"),
            ("cam", "cam"),
        ] {
            assert_eq!(strip_userinfo(typed), expected, "{typed}");
        }
    }

    #[test]
    fn auth_errors_are_typed_and_name_only_the_address() {
        let required = auth_error("rtsp://cam.local:554/live", false);
        assert_eq!(
            crate::types::error_transport_code(&required),
            Some("auth_required")
        );
        let text = format!("{required:#}");
        if can_remember() {
            assert!(
                text.contains("capturefab login add rtsp://cam.local:554/live"),
                "{text}"
            );
        } else {
            assert!(text.contains("include them in the address"), "{text}");
        }
        let rejected = auth_error("rtsp://cam.local:554/live", true);
        assert_eq!(
            crate::types::error_transport_code(&rejected),
            Some("auth_rejected")
        );
        assert!(format!("{rejected:#}").contains("refused"));
        assert!(is_auth_error(&rejected.context("connect")));
        assert_eq!(crate::cli::error_code(&required), ("auth_required", 1));
    }

    #[cfg(feature = "keychain")]
    #[test]
    fn saving_replacing_and_forgetting_keep_secrets_out_of_the_index() {
        let (logins, store) = logins("save");
        let address = key("rtsp://cam/live").unwrap().address;
        // An empty password fits the store's limits through its prefix.
        let saved = logins
            .save(&address, &CameraLogin::new("admin", "").unwrap())
            .unwrap();
        assert!(!saved.replaced);
        match logins.remembered(&address).unwrap() {
            Remembered::Login(login) => assert_eq!(login.expose(), ""),
            other => panic!("{other:?}"),
        }
        let saved = logins
            .save(
                &address,
                &CameraLogin::new("viewer", "longer-password").unwrap(),
            )
            .unwrap();
        assert!(saved.replaced);
        let secrets = store.secrets.lock().unwrap().clone();
        assert_eq!(
            secrets.keys().cloned().collect::<Vec<_>>(),
            [account("viewer", &address)],
            "a user change deletes the old account"
        );
        assert_eq!(
            secrets.values().next().unwrap().as_str(),
            "v1:longer-password"
        );
        let bytes = fs::read(logins.dir().join(INDEX)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("longer-password") && text.contains("viewer"),
            "{text}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(logins.dir().join(INDEX))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(logins.list().unwrap().len(), 1);
        logins.touch(&address);
        assert!(logins.list().unwrap()[0].last_used_at.is_some());
        assert!(logins.forget(&address).unwrap());
        assert!(store.secrets.lock().unwrap().is_empty());
        let error = logins.forget(&address).unwrap_err();
        assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
    }

    #[cfg(feature = "keychain")]
    #[test]
    fn a_failed_index_write_restores_the_previous_secret() {
        let (logins, store) = logins("rollback");
        let address = "rtsp://cam:554/live";
        logins
            .save(
                address,
                &CameraLogin::new("admin", "first-password").unwrap(),
            )
            .unwrap();
        // A directory where the index belongs makes the rename fail.
        fs::remove_file(logins.dir().join(INDEX)).unwrap();
        fs::create_dir(logins.dir().join(INDEX)).unwrap();
        assert!(
            logins
                .save(
                    address,
                    &CameraLogin::new("admin", "second-password").unwrap()
                )
                .is_err()
        );
        assert_eq!(
            store.secrets.lock().unwrap()[&account("admin", address)].as_str(),
            "v1:first-password"
        );
        fs::remove_dir(logins.dir().join(INDEX)).unwrap();
        assert!(
            logins
                .save("rtsp://new:554/", &CameraLogin::new("u", "x").unwrap())
                .is_ok()
        );
    }

    #[cfg(not(feature = "keychain"))]
    #[test]
    fn without_a_credential_store_saving_is_unsupported() {
        let (logins, store) = logins("unsupported");
        let login = CameraLogin::new("admin", "pw").unwrap();
        for error in [
            logins.save("rtsp://cam:554/x", &login).unwrap_err(),
            logins.forget("rtsp://cam:554/x").unwrap_err(),
        ] {
            assert_eq!(crate::cli::error_code(&error), ("unsupported", 5));
        }
        assert!(matches!(
            logins.remembered("rtsp://cam:554/x").unwrap(),
            Remembered::None
        ));
        assert!(logins.list().unwrap().is_empty());
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    }

    #[cfg(feature = "keychain")]
    #[test]
    fn lookups_read_the_store_only_for_saved_addresses() {
        let (logins, store) = logins("lookup");
        assert!(matches!(
            logins.remembered("rtsp://cam:554/x").unwrap(),
            Remembered::None
        ));
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
        logins
            .save(
                "rtsp://cam:554/x",
                &CameraLogin::new("admin", "pw").unwrap(),
            )
            .unwrap();
        store.secrets.lock().unwrap().clear();
        assert!(matches!(
            logins.remembered("rtsp://cam:554/x").unwrap(),
            Remembered::Missing { ref user } if user == "admin"
        ));
        store.secrets.lock().unwrap().insert(
            account("admin", "rtsp://cam:554/x"),
            Zeroizing::new("v2:x".into()),
        );
        assert!(logins.remembered("rtsp://cam:554/x").is_err());
        store.fail.store(true, Ordering::SeqCst);
        let error = logins.remembered("rtsp://cam:554/x").unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("unlock it and try again"), "{text}");
        assert_eq!(crate::cli::error_code(&error), ("operation_failed", 1));
    }

    #[test]
    fn an_index_only_handle_lists_but_never_saves() {
        let logins = Logins::index_only(temp("index-only"));
        assert!(logins.list().unwrap().is_empty());
        assert!(matches!(
            logins.remembered("rtsp://cam:554/x").unwrap(),
            Remembered::None
        ));
        let error = logins
            .save("rtsp://cam:554/x", &CameraLogin::new("a", "b").unwrap())
            .unwrap_err();
        assert_eq!(crate::cli::error_code(&error), ("unsupported", 5));
        assert_eq!(
            crate::cli::error_code(&logins.forget("rtsp://cam:554/x").unwrap_err()),
            ("unsupported", 5)
        );
    }

    #[test]
    fn oversized_or_unknown_indexes_are_refused() {
        let dir = temp("oversized");
        crate::ipc::ensure_private_dir(&dir).unwrap();
        fs::write(dir.join(INDEX), vec![b' '; MAX_FILE + 1]).unwrap();
        assert!(Logins::index_only(&dir).list().is_err());
        fs::write(dir.join(INDEX), r#"{"version":2,"logins":[]}"#).unwrap();
        assert!(Logins::index_only(&dir).list().is_err());
    }
}
