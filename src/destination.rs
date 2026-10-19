//! Named capture destinations: a folder (on any disk, including external and
//! network volumes), an S3-compatible bucket, or an FTP(S) or SFTP server.
//! Captures, schedules and recordings refer to a destination by name, so
//! commands never carry secrets; the list lives in the private session
//! directory and holds no credentials.
//!
//! Every file is still written locally through `storage` (budgets, retention,
//! never overwriting). A network destination writes into a local staging
//! folder and queues each finished file for upload (see `upload`).
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub(crate) mod secrets;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Destination {
    pub name: String,
    #[serde(flatten)]
    pub target: Target,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Folder { path: PathBuf },
    S3(Bucket),
    Ftp(FtpServer),
    Sftp(SftpServer),
}

/// Keychain services for FTP and SFTP passwords and SFTP key passphrases,
/// each under the account `user@host:port`.
pub const FTP_SERVICE: &str = "capturefab-ftp";
pub const SFTP_SERVICE: &str = "capturefab-sftp";
pub const SFTP_KEY_SERVICE: &str = "capturefab-sftp-key";

/// How an upload avoids replacing an existing object.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoOverwrite {
    /// The service refuses the write when the key exists (`If-None-Match: *`).
    #[default]
    IfNoneMatch,
    /// The service ignores conditional writes: look the key up first. Another
    /// writer can still race between the check and the write.
    CheckFirst,
}

impl NoOverwrite {
    pub fn is_default(&self) -> bool {
        *self == Self::IfNoneMatch
    }
}

/// Where an FTP or SFTP password (or SFTP key passphrase) comes from. The
/// secret itself is never stored in Capturefab files.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretSource {
    /// The OS credential store, under `user@host:port`.
    #[default]
    Keychain,
    /// `CAPTUREFAB_FTP_PASSWORD` or `CAPTUREFAB_SFTP_PASSWORD` in the
    /// environment of the uploading process.
    Environment,
    /// No password: anonymous FTP, or an SFTP key without a passphrase.
    #[serde(rename = "none")]
    NoSecret,
}

/// Explicit FTPS (`AUTH TLS` on the control port) or plain FTP.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FtpTls {
    #[default]
    Explicit,
    Plain,
}

fn ftp_port() -> u16 {
    21
}
fn ssh_port() -> u16 {
    22
}

/// An FTP or explicit-FTPS server. Files are uploaded under a temporary name
/// and renamed only while the final name is free.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FtpServer {
    /// DNS name, IPv4 address or `[IPv6]` address.
    pub host: String,
    #[serde(default = "ftp_port")]
    pub port: u16,
    pub user: String,
    /// Folder for uploads: empty for the login folder, relative to it, or
    /// absolute with a leading `/`.
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub tls: FtpTls,
    #[serde(default)]
    pub password: SecretSource,
    #[serde(default)]
    pub keep_local: bool,
}

/// An SFTP server, pinned to one host key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SftpServer {
    pub host: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub dir: String,
    /// OpenSSH SHA256 fingerprint of the server's host key, `SHA256:...`.
    pub host_key: String,
    /// Private key to sign in with; `password` is then its passphrase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    #[serde(default)]
    pub password: SecretSource,
    #[serde(default)]
    pub keep_local: bool,
}

/// A network target, as carried by `Resolved` and the upload queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Remote {
    S3(Bucket),
    Ftp(FtpServer),
    Sftp(SftpServer),
}

/// An S3-compatible bucket: AWS S3, Cloudflare R2, Backblaze B2, MinIO, Wasabi
/// and others that implement the S3 API with Signature Version 4.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Bucket {
    /// Service URL, for example `https://ACCOUNT.r2.cloudflarestorage.com`;
    /// empty for AWS S3, whose endpoint follows from the region.
    #[serde(default)]
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix, without leading slash, for example `cameras/line-1/`.
    #[serde(default)]
    pub prefix: String,
    /// Address the bucket in the URL path rather than the host name. Most
    /// non-AWS services and MinIO need it.
    #[serde(default)]
    pub path_style: bool,
    pub credentials: Credentials,
    /// Keep the local copy after a verified upload.
    #[serde(default)]
    pub keep_local: bool,
    /// How uploads avoid replacing existing objects; `check_first` only for
    /// services that ignore conditional writes.
    #[serde(default, skip_serializing_if = "NoOverwrite::is_default")]
    pub no_overwrite: NoOverwrite,
}

/// Where an S3 destination's keys come from. Only the access key ID is stored
/// here; secrets stay in the OS credential store or the standard AWS sources.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum Credentials {
    /// The OS credential store (macOS Keychain, Windows Credential Manager,
    /// Secret Service on Linux), under this access key ID.
    Keychain { access_key_id: String },
    /// A profile in the AWS shared credentials file.
    Profile { name: String },
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
    /// `AWS_SESSION_TOKEN` in the environment of the uploading process.
    Environment,
}

impl<'de> Deserialize<'de> for Credentials {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Empty struct variants enforce unknown-field checks, which serde
        // skips for internally tagged unit variants.
        #[derive(Deserialize)]
        #[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
        enum Input {
            Keychain { access_key_id: String },
            Profile { name: String },
            Environment {},
        }
        Ok(match Input::deserialize(deserializer)? {
            Input::Keychain { access_key_id } => Self::Keychain { access_key_id },
            Input::Profile { name } => Self::Profile { name },
            Input::Environment {} => Self::Environment,
        })
    }
}

impl Bucket {
    /// The service URL, defaulting to AWS S3 for the region.
    pub fn service_url(&self) -> String {
        if self.endpoint.is_empty() {
            format!("https://s3.{}.amazonaws.com", self.region)
        } else {
            self.endpoint.trim_end_matches('/').to_string()
        }
    }
    /// The service is reached without TLS, so object data travels unencrypted
    /// (requests are still signed; the secret key is never sent).
    pub fn insecure(&self) -> bool {
        self.endpoint
            .split_once("://")
            .is_some_and(|(protocol, _)| protocol.eq_ignore_ascii_case("http"))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.endpoint.is_empty()
                || self
                    .endpoint
                    .split_once("://")
                    .is_some_and(|(protocol, _)| {
                        protocol.eq_ignore_ascii_case("https")
                            || protocol.eq_ignore_ascii_case("http")
                    }),
            "S3 endpoint must be an http:// or https:// URL"
        );
        ensure!(
            !self.endpoint.contains(['?', '#', '@', ' ']),
            "S3 endpoint must be a plain service URL without credentials, query or fragment"
        );
        if !self.endpoint.is_empty() {
            crate::media::validate_network_url(&self.endpoint, "S3 endpoint")?;
            let (_, rest) = self.endpoint.split_once("://").unwrap();
            ensure!(
                rest.split_once('/')
                    .is_none_or(|(_, path)| path.trim_matches('/').is_empty()),
                "S3 endpoint must be a service URL without a path; put the object folder in S3 key prefix"
            );
            if !self.path_style {
                let host = rest.split('/').next().unwrap();
                ensure!(
                    !host.starts_with('['),
                    "S3 endpoint with an IPv6 address requires path-style addressing"
                );
            }
        }
        ensure!(
            !self.region.is_empty()
                && self.region.len() <= 64
                && self
                    .region
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "S3 region must be a name such as us-east-1 or auto"
        );
        // S3 naming rules; dots are allowed but break virtual-hosted TLS.
        ensure!(
            (3..=63).contains(&self.bucket.len())
                && self.bucket.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'.')
                && self.bucket.as_bytes()[0].is_ascii_alphanumeric()
                && self.bucket.as_bytes()[self.bucket.len() - 1].is_ascii_alphanumeric()
                && !self.bucket.contains("..")
                && self.bucket.parse::<std::net::Ipv4Addr>().is_err(),
            "S3 bucket names are 3-63 lowercase letters, digits, dots and hyphens, must start/end with a letter or digit, and cannot contain adjacent dots or be an IP address"
        );
        validate_prefix(&self.prefix, "S3 key prefix")?;
        self.credentials.validate()?;
        Ok(())
    }
}

impl Credentials {
    pub fn validate(&self) -> Result<()> {
        match self {
            Credentials::Keychain { access_key_id } => ensure!(
                !access_key_id.is_empty()
                    && access_key_id.len() <= 128
                    && access_key_id.bytes().all(|b| b.is_ascii_graphic()),
                "S3 access key ID is empty or invalid"
            ),
            Credentials::Profile { name } => ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "AWS profile name must contain 1-64 ASCII letters, digits, dots, underscores or hyphens"
            ),
            Credentials::Environment => {}
        }
        Ok(())
    }
}

fn validate_host(host: &str, what: &str) -> Result<()> {
    let ipv6 = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok());
    let name = (1..=253).contains(&host.len())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !host.starts_with(['-', '.'])
        && !host.contains("..");
    ensure!(
        ipv6 || name,
        "{what} host must be a DNS name, IPv4 address or [IPv6] address without scheme, port or user"
    );
    Ok(())
}

fn validate_user(user: &str, what: &str) -> Result<()> {
    ensure!(
        (1..=128).contains(&user.len()) && user.bytes().all(|b| b.is_ascii_graphic()),
        "{what} user must be 1-128 printable characters without spaces"
    );
    Ok(())
}

fn validate_port(port: u16) -> Result<()> {
    ensure!(port != 0, "port must be 1-65535");
    Ok(())
}

fn validate_remote_dir(dir: &str) -> Result<()> {
    ensure!(
        dir.len() <= 512
            && !dir.contains('\\')
            && !dir.chars().any(char::is_control)
            && !dir.split('/').any(|part| matches!(part, "." | "..")),
        "remote folder must be at most 512 bytes without backslashes, control characters or . and .. parts"
    );
    Ok(())
}

/// Check an OpenSSH SHA256 host-key fingerprint, `SHA256:` and 43 unpadded
/// base64 characters.
pub fn validate_host_key(fingerprint: &str) -> Result<()> {
    ensure!(
        fingerprint.strip_prefix("SHA256:").is_some_and(|hash| {
            hash.len() == 43
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
        }),
        "SFTP host key must be an OpenSSH SHA256 fingerprint such as SHA256:Kx3UiE8JjkbQ/8lkXw8yS4pLnYgxwZWwB9VfYzX3T0o"
    );
    Ok(())
}

impl FtpServer {
    pub fn validate(&self) -> Result<()> {
        validate_host(&self.host, "FTP")?;
        validate_port(self.port)?;
        validate_user(&self.user, "FTP")?;
        validate_remote_dir(&self.dir)
    }
}

impl SftpServer {
    pub fn validate(&self) -> Result<()> {
        validate_host(&self.host, "SFTP")?;
        validate_port(self.port)?;
        validate_user(&self.user, "SFTP")?;
        validate_remote_dir(&self.dir)?;
        validate_host_key(&self.host_key)?;
        if let Some(key) = &self.key_file {
            ensure!(
                key.is_absolute()
                    && key
                        .to_str()
                        .is_some_and(|k| !k.chars().any(char::is_control)),
                "SFTP key file must be an absolute path"
            );
        }
        Ok(())
    }
}

/// `scheme://host:port/dir`, with a doubled slash for an absolute folder.
fn server_url(scheme: &str, host: &str, port: u16, dir: &str) -> String {
    format!("{scheme}://{host}:{port}/{dir}")
}

impl Remote {
    pub fn validate(&self) -> Result<()> {
        match self {
            Remote::S3(bucket) => bucket.validate(),
            Remote::Ftp(server) => server.validate(),
            Remote::Sftp(server) => server.validate(),
        }
    }
    pub fn keep_local(&self) -> bool {
        match self {
            Remote::S3(bucket) => bucket.keep_local,
            Remote::Ftp(server) => server.keep_local,
            Remote::Sftp(server) => server.keep_local,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Remote::S3(_) => "s3",
            Remote::Ftp(_) => "ftp",
            Remote::Sftp(_) => "sftp",
        }
    }
    /// Prefix joined to object keys; FTP and SFTP uploaders add their folder
    /// themselves, so their keys are relative to it.
    pub fn key_prefix(&self) -> &str {
        match self {
            Remote::S3(bucket) => &bucket.prefix,
            Remote::Ftp(_) | Remote::Sftp(_) => "",
        }
    }
    /// Where files go, for lists, logs and the dataset; never with the user
    /// name or a secret.
    pub fn location(&self) -> String {
        match self {
            Remote::S3(bucket) => format!(
                "s3://{}/{} at {}",
                bucket.bucket,
                bucket.prefix,
                crate::media::redact_url(&bucket.service_url())
            ),
            Remote::Ftp(server) => server_url(
                if server.tls == FtpTls::Plain {
                    "ftp"
                } else {
                    "ftps"
                },
                &server.host,
                server.port,
                &server.dir,
            ),
            Remote::Sftp(server) => server_url("sftp", &server.host, server.port, &server.dir),
        }
    }
    /// The URL of an uploaded object `key` (as returned by `object_key`).
    pub fn object_url(&self, key: &str) -> String {
        match self {
            Remote::S3(bucket) => format!("s3://{}/{key}", bucket.bucket),
            Remote::Ftp(_) | Remote::Sftp(_) => {
                let base = self.location();
                if base.ends_with('/') {
                    format!("{base}{key}")
                } else {
                    format!("{base}/{key}")
                }
            }
        }
    }
    /// Files (and for FTP, the password) travel unencrypted.
    pub fn insecure(&self) -> bool {
        match self {
            Remote::S3(bucket) => bucket.insecure(),
            Remote::Ftp(server) => server.tls == FtpTls::Plain,
            Remote::Sftp(_) => false,
        }
    }
    /// The credential-store service and account holding this server's
    /// password or key passphrase, when it is kept there.
    pub fn secret_ref(&self) -> Option<(&'static str, String)> {
        let (service, user, host, port) = match self {
            Remote::S3(_) => return None,
            Remote::Ftp(s) if s.password == SecretSource::Keychain => {
                (FTP_SERVICE, &s.user, &s.host, s.port)
            }
            Remote::Sftp(s) if s.password == SecretSource::Keychain => (
                if s.key_file.is_some() {
                    SFTP_KEY_SERVICE
                } else {
                    SFTP_SERVICE
                },
                &s.user,
                &s.host,
                s.port,
            ),
            _ => return None,
        };
        Some((
            service,
            format!("{user}@{}:{port}", host.to_ascii_lowercase()),
        ))
    }
}

impl Target {
    /// The network target files are uploaded to, if any.
    pub fn remote(&self) -> Option<Remote> {
        match self {
            Target::Folder { .. } => None,
            Target::S3(bucket) => Some(Remote::S3(bucket.clone())),
            Target::Ftp(server) => Some(Remote::Ftp(server.clone())),
            Target::Sftp(server) => Some(Remote::Sftp(server.clone())),
        }
    }
    pub fn is_remote(&self) -> bool {
        !matches!(self, Target::Folder { .. })
    }
    /// The serialized `kind`: `folder`, `s3`, `ftp` or `sftp`.
    pub fn kind(&self) -> &'static str {
        match self {
            Target::Folder { .. } => "folder",
            Target::S3(_) => "s3",
            Target::Ftp(_) => "ftp",
            Target::Sftp(_) => "sftp",
        }
    }
}

/// Refuse a network destination whose upload support is not compiled in.
fn ensure_compiled(remote: &Remote) -> Result<()> {
    let (compiled, name, feature) = match remote {
        Remote::S3(_) => (cfg!(feature = "s3"), "S3", "s3"),
        Remote::Ftp(_) => (cfg!(feature = "ftp"), "FTP", "ftp"),
        Remote::Sftp(_) => (cfg!(feature = "sftp"), "SFTP", "sftp"),
    };
    ensure!(
        compiled,
        "{name} destinations are unsupported in this build; rebuild with --features {feature}"
    );
    Ok(())
}

impl Destination {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        match &self.target {
            Target::Folder { path } => {
                ensure!(
                    path.to_str().is_some(),
                    "a folder destination needs a path with valid UTF-8; rename the folder or choose another destination"
                );
                ensure!(
                    path.is_absolute()
                        && !path
                            .as_os_str()
                            .as_encoded_bytes()
                            .iter()
                            .any(|b| b.is_ascii_control()),
                    "a folder destination needs an absolute path without control characters"
                );
            }
            Target::S3(bucket) => bucket.validate()?,
            Target::Ftp(server) => server.validate()?,
            Target::Sftp(server) => server.validate()?,
        }
        Ok(())
    }
    /// Local directory files for this destination are written under.
    pub fn local_root(&self) -> PathBuf {
        match &self.target {
            Target::Folder { path } => path.clone(),
            Target::S3(_) | Target::Ftp(_) | Target::Sftp(_) => staging_root().join(&self.name),
        }
    }
    /// Short description for lists and logs, without secrets.
    pub fn describe(&self) -> String {
        match (&self.target, self.target.remote()) {
            (_, Some(remote)) => remote.location(),
            (Target::Folder { path }, None) => path.display().to_string(),
            (_, None) => unreachable!("every non-folder destination is remote"),
        }
    }
}

/// Whether an S3 endpoint can only be reached with path-style addressing:
/// its host is an IP address or localhost, so `BUCKET.host` cannot resolve.
pub fn endpoint_needs_path_style(endpoint: &str) -> bool {
    let Some((_, rest)) = endpoint.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.starts_with('[') {
        return true;
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::Ipv4Addr>().is_ok()
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && !name.starts_with('.'),
        "destination names are 1-64 letters, digits, '.', '-' and '_' and cannot start with '.'"
    );
    Ok(())
}

/// Where network destinations stage files before upload: the platform's
/// per-user data directory, or `CAPTUREFAB_STAGING_DIR`. An explicit
/// `CAPTUREFAB_SESSION_DIR` keeps its staging inside it, so isolated
/// sessions (and tests) never share staged files with the user's own.
pub fn staging_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("CAPTUREFAB_STAGING_DIR") {
        return dir.into();
    }
    if let Some(dir) = std::env::var_os("CAPTUREFAB_SESSION_DIR") {
        return PathBuf::from(dir).join("staging");
    }
    // Unit tests stage in their own session directory, never the user's.
    if cfg!(test) {
        return crate::ipc::state_dir().join("staging");
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support/Capturefab"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Capturefab"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(|d| PathBuf::from(d).join("capturefab"))
            .or_else(|| home().map(|h| h.join(".local/share/capturefab")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("uploads")
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    version: u32,
    destinations: Vec<Destination>,
    /// The destination the GUI last saved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preferred: Option<String>,
}

pub(crate) static STATE: crate::ipc::StateSet = crate::ipc::StateSet {
    what: "saved destinations",
    names: &["destinations.json"],
    lock: Some("destinations.lock"),
};

/// The directory holding the saved list and its lock.
fn dir() -> PathBuf {
    static ADOPTED: std::sync::Once = std::sync::Once::new();
    ADOPTED.call_once(|| crate::ipc::adopt_state(&STATE));
    crate::ipc::state_dir()
}

/// The file holding the saved list.
pub(crate) fn path() -> PathBuf {
    dir().join("destinations.json")
}

/// All saved destinations.
pub fn list() -> Result<Vec<Destination>> {
    Ok(read()?.destinations)
}

fn read() -> Result<File> {
    let path = path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(File {
                version: 1,
                ..File::default()
            });
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    ensure!(bytes.len() <= 1 << 20, "destination list is too large");
    let file: File = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid destination list {}", path.display()))?;
    ensure!(file.version == 1, "unsupported destination list version");
    ensure!(
        file.destinations.len() <= 256,
        "too many saved destinations (maximum 256)"
    );
    let mut names = std::collections::HashSet::new();
    for destination in &file.destinations {
        destination
            .validate()
            .with_context(|| format!("invalid saved destination {}", destination.name))?;
        ensure!(
            names.insert(&destination.name),
            "duplicate saved destination name {}",
            destination.name
        );
    }
    Ok(file)
}

/// The destination the GUI last saved to, if it still exists.
pub fn preferred() -> Option<String> {
    let file = read().ok()?;
    file.preferred
        .filter(|name| file.destinations.iter().any(|d| &d.name == name))
}

pub fn set_preferred(name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        validate_name(name)?;
    }
    locked(|file| {
        ensure!(
            name.is_none_or(|name| file.destinations.iter().any(|d| d.name == name)),
            "preferred destination no longer exists"
        );
        file.preferred = name.map(str::to_string);
        Ok(())
    })
}

pub fn get(name: &str) -> Result<Destination> {
    validate_name(name)?;
    list()?.into_iter().find(|d| d.name == name).ok_or_else(|| {
        anyhow!("destination {name} not found; list them with `capturefab destination list`")
    })
}

fn write(file: &File) -> Result<()> {
    let dir = dir();
    crate::ipc::ensure_private_dir(&dir)?;
    // Write a sibling and rename it into place, so readers never see a
    // partial list.
    let temp = dir.join(format!("destinations.json.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp)?;
    output.write_all(&serde_json::to_vec_pretty(file)?)?;
    output.sync_all()?;
    fs::rename(&temp, path())?;
    Ok(())
}

/// Serialize read-modify-write updates across processes.
fn locked<T>(update: impl FnOnce(&mut File) -> Result<T>) -> Result<T> {
    locked_with_rollback(update, |_| Ok(()))
}

fn locked_with_rollback<T>(
    update: impl FnOnce(&mut File) -> Result<T>,
    rollback: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    use fs2::FileExt;
    let dir = dir();
    crate::ipc::ensure_private_dir(&dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("destinations.lock"))?;
    lock.lock_exclusive()?;
    let mut file = read()?;
    file.version = 1;
    let result = update(&mut file)?;
    if let Err(error) = write(&file) {
        rollback(&result)
            .context("restore credentials after the destination could not be saved")?;
        return Err(error);
    }
    Ok(result)
}

/// Replace exactly the destination the caller observed. Renames are one
/// transaction, and a concurrent editor cannot silently lose its changes.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn replace(before: Option<&Destination>, after: Option<&Destination>) -> Result<()> {
    replace_with(before, after, || Ok(()), |_| Ok(()))
}

/// Apply an accompanying credential change under the destination lock.
/// Restore it if writing the settings fails, so a failed save has no hidden
/// credential side effect.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn replace_with<T>(
    before: Option<&Destination>,
    after: Option<&Destination>,
    prepare: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    replace_with_selection(before, after, None, prepare, rollback)
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn replace_with_selection<T>(
    before: Option<&Destination>,
    after: Option<&Destination>,
    selection: Option<(Option<&str>, bool)>,
    prepare: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    if let Some(after) = after {
        after.validate()?;
    }
    locked_with_rollback(
        |file| {
            replace_in_file(file, before, after)?;
            if let Some((selected, remember)) = selection {
                if let Some(name) = selected {
                    ensure!(
                        file.destinations.iter().any(|d| d.name == name),
                        "destination {name} no longer exists; cannot restore its selection"
                    );
                }
                if remember {
                    file.preferred = selected.map(str::to_owned);
                }
            }
            prepare()
        },
        rollback,
    )
}

fn replace_in_file(
    file: &mut File,
    before: Option<&Destination>,
    after: Option<&Destination>,
) -> Result<()> {
    for name in before.into_iter().chain(after).map(|d| &d.name) {
        let expected = before.filter(|d| &d.name == name);
        let current = file.destinations.iter().find(|d| &d.name == name);
        ensure!(
            current == expected,
            "destination {name} changed outside this edit; reload it before trying again"
        );
    }
    let old_name = before.map(|d| &d.name);
    if before.is_some() {
        file.destinations.retain(|d| Some(&d.name) != old_name);
    }
    if let Some(after) = after {
        ensure!(file.destinations.len() < 256, "too many destinations");
        file.destinations.push(after.clone());
        file.destinations.sort_by(|a, b| a.name.cmp(&b.name));
    }
    if file.preferred.as_ref() == old_name && old_name.is_some() {
        file.preferred = after.map(|d| d.name.clone());
    }
    Ok(())
}

/// Add a destination, or replace the one with the same name.
pub fn save(destination: Destination) -> Result<()> {
    destination.validate()?;
    locked(|file| {
        let all = &mut file.destinations;
        match all.iter_mut().find(|d| d.name == destination.name) {
            Some(existing) => *existing = destination,
            None => {
                ensure!(all.len() < 256, "too many destinations");
                all.push(destination);
            }
        }
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    })
}

/// Add a new destination, never replacing a saved one with the same name.
/// A folder destination's directory is made first; `secret` is stored in
/// the OS credential store under an S3 destination's access key ID while the
/// list is written, and put back if the list cannot be. Answers whether a
/// secret was stored. The credential store can prompt, so never call this
/// on the GUI thread.
pub fn create(destination: &Destination, secret: Option<Zeroizing<String>>) -> Result<bool> {
    destination.validate()?;
    let write = match secret {
        None => None,
        Some(_) if !cfg!(feature = "s3") => {
            bail!("S3 destinations are unsupported in this build; rebuild with --features s3")
        }
        Some(secret) => {
            let Target::S3(Bucket {
                credentials: Credentials::Keychain { access_key_id },
                ..
            }) = &destination.target
            else {
                bail!("only an S3 destination with keychain credentials takes a secret key");
            };
            #[cfg(feature = "s3")]
            crate::s3::validate_secret(&secret)?;
            secrets::SecretWrite::queue(Some((access_key_id.clone(), secret)))
        }
    };
    // Checked again under the lock: a destination saved meanwhile is still
    // never replaced, only reported less plainly.
    let name = &destination.name;
    ensure!(
        !list()?.iter().any(|saved| &saved.name == name),
        "a destination named {name} already exists; choose another name"
    );
    Ok(secrets::save_with_secret(None, destination, None, write)?.is_some())
}

pub fn remove(name: &str) -> Result<Destination> {
    validate_name(name)?;
    locked(|file| {
        let all = &mut file.destinations;
        let index = all
            .iter()
            .position(|d| d.name == name)
            .ok_or_else(|| anyhow!("destination {name} not found"))?;
        Ok(all.remove(index))
    })
}

/// A capture output resolved against an optional destination: the local path
/// pattern to write, and where finished files are uploaded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resolved {
    /// Output pattern (file, directory or `{frame}` template) on local disk.
    pub output: String,
    /// The destination's name, for messages and the upload queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// Upload finished files here; their keys are the prefix plus their path
    /// below `root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<Remote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
}

/// Resolve `output` for capture. Without a destination it is used as is; with
/// one it must be relative (a file name, folder or `{frame}` template) and is
/// placed inside the destination's folder or staging area.
pub fn resolve(name: Option<&str>, output: &str) -> Result<Resolved> {
    validate_output(name, output)?;
    let Some(name) = name else {
        return Ok(Resolved {
            output: output.into(),
            destination: None,
            upload: None,
            root: None,
        });
    };
    let destination = get(name)?;
    resolve_with(&destination, output)
}

pub fn resolve_with(destination: &Destination, output: &str) -> Result<Resolved> {
    destination.validate()?;
    validate_output(Some(&destination.name), output)?;
    let relative = Path::new(output);
    let root = destination.local_root();
    ensure!(
        root.is_absolute(),
        "destination staging folder must use an absolute path; check CAPTUREFAB_STAGING_DIR"
    );
    let output_path = root.join(relative);
    let output = output_path.to_str().context("destination output path must be valid UTF-8; rename the folder or choose another destination")?.to_owned();
    let canonical_root = resolve_existing_ancestor(&root)?;
    let candidate = resolve_existing_ancestor(&output_path)?;
    ensure!(
        candidate.starts_with(&canonical_root),
        "destination output escapes its folder through a symbolic link; choose a path inside the destination"
    );
    if let Target::Folder { path } = &destination.target {
        crate::volumes::ensure_present(path)?;
        ensure!(
            path.is_dir() || crate::volumes::mount_root(path).is_none(),
            "destination folder {} does not exist",
            path.display()
        );
    }
    let upload = destination.target.remote();
    if let Some(remote) = &upload {
        ensure_compiled(remote)?;
    }
    Ok(Resolved {
        output,
        destination: Some(destination.name.clone()),
        upload,
        root: Some(root),
    })
}

/// Check an output's shape before settings or storage can change. Network
/// forwarding can use URLs without a destination; named destinations use paths.
pub fn validate_output(name: Option<&str>, output: &str) -> Result<()> {
    ensure!(
        !output.trim().is_empty() && output.len() <= 16384 && !output.chars().any(char::is_control),
        "capture output needs a path without control characters (maximum 16384 bytes)"
    );
    if let Some(name) = name {
        validate_name(name)?;
        let relative = Path::new(output);
        ensure!(
            relative.is_relative()
                && !output.contains("://")
                && !output.starts_with("file:")
                && !relative.components().any(|c| matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                )),
            "with a destination, the output must be a relative name inside it, such as captures/run-1 or shot-{{frame}}.png"
        );
    }
    Ok(())
}

/// Resolve existing symlinks without creating directories for an invalid output.
fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::canonicalize(path)
            .with_context(|| format!("cannot resolve destination path {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("destination path has no existing ancestor")?;
            let name = path
                .file_name()
                .context("invalid destination output path")?;
            Ok(resolve_existing_ancestor(parent)?.join(name))
        }
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect destination path {}", path.display())),
    }
}

/// A finished file queued for upload: its upload ID, the remote key and the
/// object's URL (never with credentials).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Queued {
    pub id: String,
    pub key: String,
    pub url: String,
}

impl Resolved {
    /// Queue a file this resolution produced for upload, if it has a network
    /// target. Called after the file is complete and recorded in the storage
    /// ledger, with the SHA-256 its writer computed when the caller has it.
    /// Returns what was queued, or `None` for a local destination.
    pub fn finished(&self, path: &Path, sha256: Option<&str>) -> Result<Option<Queued>> {
        let (Some(remote), Some(root), Some(name)) = (&self.upload, &self.root, &self.destination)
        else {
            return Ok(None);
        };
        let key = object_key(remote.key_prefix(), root, path)?;
        ensure_compiled(remote)?;
        #[cfg(feature = "uploads")]
        {
            let id = crate::upload::enqueue(name, remote, path, &key, sha256)?;
            let url = remote.object_url(&key);
            Ok(Some(Queued { id, key, url }))
        }
        #[cfg(not(feature = "uploads"))]
        {
            let _ = (name, key, sha256);
            anyhow::bail!("uploads are unsupported in this build")
        }
    }

    /// Refuse a path that leaves the destination's folder through a symbolic
    /// link, checked again after a template expands below the root.
    pub fn contain(&self, path: &Path) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        ensure!(
            resolve_existing_ancestor(path)?.starts_with(resolve_existing_ancestor(root)?),
            "destination output escapes its folder through a symbolic link; choose a path inside the destination"
        );
        Ok(())
    }
}

/// `what` opens the message, which the GUI uses to find the field at fault.
fn validate_prefix(prefix: &str, what: &str) -> Result<()> {
    ensure!(prefix.len() <= 512, "{what} must be at most 512 bytes");
    ensure!(
        !prefix.starts_with('/')
            && !prefix.contains('\\')
            && !prefix.chars().any(char::is_control)
            && !prefix.split('/').any(|part| matches!(part, "." | "..")),
        "{what} must be relative, without backslashes, control characters or . and .. components, like cameras/line-1/"
    );
    Ok(())
}

/// The object key for a staged file: the prefix plus its path below the
/// staging root, with forward slashes.
pub fn object_key(prefix: &str, root: &Path, path: &Path) -> Result<String> {
    validate_prefix(prefix, "remote key prefix")?;
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| anyhow!("{} is outside the staging folder", path.display()))?;
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "object key must remain inside the staging folder"
    );
    let parts: Vec<String> = relative
        .components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .map(str::to_owned)
                .context("remote object paths must contain valid UTF-8")
        })
        .collect::<Result<_>>()?;
    ensure!(!parts.is_empty(), "empty object key");
    let mut key = prefix.to_string();
    if !key.is_empty() && !key.ends_with('/') {
        key.push('/');
    }
    key.push_str(&parts.join("/"));
    validate_object_key(&key)?;
    Ok(key)
}

pub(crate) fn validate_object_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.len() <= 1024,
        "remote object key must contain 1..1024 bytes"
    );
    ensure!(
        !key.starts_with('/')
            && !key.contains('\\')
            && !key.chars().any(char::is_control)
            && !key.split('/').any(|part| matches!(part, "." | "..")),
        "remote object key must be a relative path without control characters or '.'/'..' components"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> Bucket {
        Bucket {
            endpoint: "https://minio.example:9000".into(),
            region: "us-east-1".into(),
            bucket: "camera-archive".into(),
            prefix: "line-1".into(),
            path_style: true,
            credentials: Credentials::Keychain {
                access_key_id: "AKIDEXAMPLE".into(),
            },
            keep_local: false,
            no_overwrite: NoOverwrite::IfNoneMatch,
        }
    }

    fn ftp() -> FtpServer {
        FtpServer {
            host: "10.0.0.5".into(),
            port: 21,
            user: "cam".into(),
            dir: "incoming".into(),
            tls: FtpTls::Explicit,
            password: SecretSource::Keychain,
            keep_local: false,
        }
    }

    fn sftp() -> SftpServer {
        SftpServer {
            host: "NAS.local".into(),
            port: 2222,
            user: "cam".into(),
            dir: "/srv/line-1".into(),
            host_key: format!("SHA256:{}", "A".repeat(43)),
            key_file: None,
            password: SecretSource::Environment,
            keep_local: true,
        }
    }

    fn folder(name: &str) -> Destination {
        Destination {
            name: name.into(),
            target: Target::Folder {
                path: std::env::temp_dir().join(name),
            },
        }
    }

    #[test]
    fn undo_destination_edits_preserve_unrelated_data_and_check_the_expected_version() {
        let (original, renamed, other) = (folder("original"), folder("renamed"), folder("other"));
        let mut file = File {
            version: 1,
            destinations: vec![original.clone(), other.clone()],
            preferred: Some(original.name.clone()),
        };
        replace_in_file(&mut file, Some(&original), Some(&renamed)).unwrap();
        assert_eq!(file.destinations, vec![other.clone(), renamed.clone()]);
        assert_eq!(file.preferred.as_deref(), Some("renamed"));
        // Undo is the reverse compare-and-swap, with the other destination left intact.
        replace_in_file(&mut file, Some(&renamed), Some(&original)).unwrap();
        assert_eq!(file.destinations, vec![original.clone(), other.clone()]);
        assert_eq!(file.preferred.as_deref(), Some("original"));
        let mut modified = original.clone();
        modified.target = Target::Folder {
            path: std::env::temp_dir().join("changed-elsewhere"),
        };
        file.destinations[0] = modified.clone();
        assert!(replace_in_file(&mut file, Some(&original), Some(&renamed)).is_err());
        assert_eq!(file.destinations, vec![modified, other]);
    }

    #[test]
    fn add_remove_and_rename_undo_refuse_to_overwrite_concurrent_destinations() {
        let (original, occupied) = (folder("original"), folder("occupied"));
        let mut file = File {
            version: 1,
            destinations: vec![original.clone(), occupied.clone()],
            preferred: Some(original.name.clone()),
        };
        let mut renamed = original.clone();
        renamed.name = occupied.name.clone();
        assert!(replace_in_file(&mut file, Some(&original), Some(&renamed)).is_err());
        assert_eq!(
            file.destinations,
            vec![original.clone(), occupied.clone()],
            "rename checks both names before touching either"
        );
        assert!(replace_in_file(&mut file, None, Some(&original)).is_err());
        replace_in_file(&mut file, Some(&original), None).unwrap();
        assert!(file.preferred.is_none());
        replace_in_file(&mut file, None, Some(&original)).unwrap();
        let mut concurrent = original.clone();
        concurrent.target = occupied.target.clone();
        replace_in_file(&mut file, Some(&original), Some(&concurrent)).unwrap();
        assert!(
            replace_in_file(&mut file, Some(&original), None).is_err(),
            "undo of an add cannot remove another editor's update"
        );
        assert!(file.destinations.contains(&concurrent));
    }

    #[test]
    fn persisted_undo_is_atomic_and_rolls_back_a_credential_when_writing_fails() {
        // Run the disk checks in a child with its own session directory. Tests
        // must never alter the launching user's destinations or process-wide
        // environment while other tests run.
        if std::env::var_os("CAPTUREFAB_DESTINATION_UNDO_TEST").is_none() {
            let dir = std::env::temp_dir().join(format!(
                "capturefab-destination-undo-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            crate::ipc::ensure_private_dir(&dir).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "destination::tests::persisted_undo_is_atomic_and_rolls_back_a_credential_when_writing_fails", "--nocapture"])
                .env("CAPTUREFAB_SESSION_DIR", &dir).env("CAPTUREFAB_DESTINATION_UNDO_TEST", "1")
                .output().unwrap();
            let _ = fs::remove_dir_all(dir);
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (before, after, unrelated) = (folder("before"), folder("after"), folder("unrelated"));
        save(before.clone()).unwrap();
        save(unrelated.clone()).unwrap();
        set_preferred(Some(&before.name)).unwrap();
        replace(Some(&before), Some(&after)).unwrap();
        assert_eq!(get("after").unwrap(), after);
        assert!(get("before").is_err());
        assert_eq!(preferred().as_deref(), Some("after"));
        replace(Some(&after), Some(&before)).unwrap();
        assert_eq!(get("unrelated").unwrap(), unrelated);
        assert_eq!(preferred().as_deref(), Some("before"));
        let secret = std::cell::Cell::new("old");
        let temp =
            crate::ipc::state_dir().join(format!("destinations.json.{}.tmp", std::process::id()));
        let result = replace_with(
            Some(&before),
            Some(&after),
            || {
                secret.set("new");
                fs::create_dir(&temp)?; // Make opening the atomic-write sibling fail.
                Ok(())
            },
            |_| {
                secret.set("old");
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            secret.get(),
            "old",
            "restore accompanying credential on persistence failure"
        );
        assert_eq!(
            get("before").unwrap(),
            before,
            "original settings stay readable"
        );
        assert!(get("after").is_err());
        fs::remove_dir(&temp).unwrap();
        let touched = std::cell::Cell::new(false);
        assert!(
            replace_with(
                Some(&after),
                None,
                || {
                    touched.set(true);
                    Ok(())
                },
                |_| Ok(())
            )
            .is_err()
        );
        assert!(
            !touched.get(),
            "conflict validation happens before touching credentials"
        );
    }

    #[test]
    fn create_makes_folders_and_never_replaces_a_saved_name() {
        // A child with its own session directory, like the undo test above.
        if std::env::var_os("CAPTUREFAB_DESTINATION_CREATE_TEST").is_none() {
            let dir = std::env::temp_dir().join(format!(
                "capturefab-destination-create-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            crate::ipc::ensure_private_dir(&dir).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "destination::tests::create_makes_folders_and_never_replaces_a_saved_name",
                    "--nocapture",
                ])
                .env("CAPTUREFAB_SESSION_DIR", &dir)
                .env("CAPTUREFAB_DESTINATION_CREATE_TEST", "1")
                .output()
                .unwrap();
            let _ = fs::remove_dir_all(dir);
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = crate::ipc::state_dir().join("folders");
        let folder = |name: &str, path: &str| Destination {
            name: name.into(),
            target: Target::Folder {
                path: root.join(path),
            },
        };
        let line = folder("line-1", "line-1/photos");
        assert!(!create(&line, None).unwrap());
        assert!(root.join("line-1/photos").is_dir());
        assert_eq!(get("line-1").unwrap(), line);
        // The same name is refused before anything changes.
        let error = create(&folder("line-1", "other"), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "a destination named line-1 already exists; choose another name"
        );
        assert_eq!(get("line-1").unwrap(), line);
        assert!(!root.join("other").exists());
        let secret = || Some(Zeroizing::new("secret".to_string()));
        // Builds without S3 support refuse every secret.
        let refused = |result: Result<bool>, reason: &str| {
            let error = result.unwrap_err().to_string();
            let reason = if cfg!(feature = "s3") {
                reason
            } else {
                "S3 destinations are unsupported in this build"
            };
            assert!(error.contains(reason), "{error}");
        };
        refused(
            create(&folder("line-2", "line-2"), secret()),
            "only an S3 destination with keychain credentials",
        );
        assert!(get("line-2").is_err() && !root.join("line-2").exists());
        let mut by_profile = bucket();
        by_profile.credentials = Credentials::Profile { name: "lab".into() };
        let profile = Destination {
            name: "by-profile".into(),
            target: Target::S3(by_profile),
        };
        refused(
            create(&profile, secret()),
            "only an S3 destination with keychain credentials",
        );
        assert!(!create(&profile, None).unwrap());
        // A bucket without a typed secret never touches the credential
        // store; with one, a taken name is refused before it is stored.
        let archive = Destination {
            name: "archive".into(),
            target: Target::S3(bucket()),
        };
        assert!(!create(&archive, None).unwrap());
        refused(create(&archive, secret()), "already exists");
        assert!(create(&folder("../bad", "bad"), None).is_err());
        assert_eq!(list().unwrap().len(), 3);
    }

    #[test]
    fn destinations_validate_names_paths_and_buckets() {
        let saved = Destination {
            name: "archive".into(),
            target: Target::S3(bucket()),
        };
        assert_eq!(
            serde_json::from_value::<Destination>(serde_json::to_value(&saved).unwrap()).unwrap(),
            saved
        );
        let mut serialized = serde_json::to_value(bucket()).unwrap();
        serialized["path_stlye"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<Bucket>(serialized)
                .unwrap_err()
                .to_string()
                .contains("unknown field `path_stlye`")
        );
        assert!(
            serde_json::from_value::<Credentials>(
                serde_json::json!({"source": "profile", "name": "default", "profile": "misspelled"})
            )
            .is_err()
        );
        assert_eq!(
            serde_json::from_value::<Credentials>(serde_json::json!({"source": "environment"}))
                .unwrap(),
            Credentials::Environment
        );
        assert!(
            serde_json::from_value::<Credentials>(
                serde_json::json!({"source": "environment", "profile": "misspelled"})
            )
            .unwrap_err()
            .to_string()
            .contains("unknown field `profile`")
        );
        let folder = |name: &str, path: &str| Destination {
            name: name.into(),
            target: Target::Folder { path: path.into() },
        };
        let root = if cfg!(windows) {
            "C:\\captures"
        } else {
            "/captures"
        };
        assert!(folder("usb-drive", root).validate().is_ok());
        assert!(folder("../x", root).validate().is_err());
        assert!(folder("rel", "relative/dir").validate().is_err());
        let s3 = |change: fn(&mut Bucket)| {
            let mut b = bucket();
            change(&mut b);
            Destination {
                name: "archive".into(),
                target: Target::S3(b),
            }
            .validate()
        };
        assert!(s3(|_| {}).is_ok());
        assert!(s3(|b| b.bucket = "Upper".into()).is_err());
        assert!(s3(|b| b.bucket = "ab".into()).is_err());
        assert!(s3(|b| b.bucket = "camera..archive".into()).is_err());
        assert!(s3(|b| b.bucket = "192.168.1.2".into()).is_err());
        assert!(s3(|b| b.prefix = "/abs".into()).is_err());
        assert!(s3(|b| b.prefix = "a/../b".into()).is_err());
        assert!(s3(|b| b.endpoint = "ftp://x".into()).is_err());
        assert!(s3(|b| b.endpoint = "https://user:pw@host".into()).is_err());
        for endpoint in [
            "https://",
            "https://host:0",
            "https://host:65536",
            "https://[broken]",
            "https://host/service",
            "https://host\t",
        ] {
            let mut bucket = bucket();
            bucket.endpoint = endpoint.into();
            assert!(bucket.validate().is_err(), "{endpoint}");
        }
        assert!(s3(|b| b.region = "us east".into()).is_err());
        assert!(
            s3(|b| b.credentials = Credentials::Keychain {
                access_key_id: String::new()
            })
            .is_err()
        );
        let aws = Bucket {
            endpoint: String::new(),
            ..bucket()
        };
        assert_eq!(aws.service_url(), "https://s3.us-east-1.amazonaws.com");
        assert!(!aws.insecure());
    }

    #[test]
    fn bucket_input_preserves_data_and_explains_prefix_and_profile_limits() {
        let mut b = bucket();
        b.endpoint = "HTTP://MinIO.Example:9000///".into();
        b.prefix = " camera é /Exact %2f /".into();
        b.credentials = Credentials::Profile {
            name: "Lab.Profile_1-Prod".into(),
        };
        b.validate().unwrap();
        assert!(b.insecure());
        assert_eq!(b.endpoint, "HTTP://MinIO.Example:9000///");
        assert_eq!(b.prefix, " camera é /Exact %2f /");
        let root = std::env::temp_dir().join("capturefab-prefix-validation");
        assert_eq!(
            object_key(&b.prefix, &root, &root.join(" clip.mkv ")).unwrap(),
            " camera é /Exact %2f / clip.mkv "
        );
        b.prefix = "x".repeat(513);
        assert!(b.validate().unwrap_err().to_string().contains("512 bytes"));
        assert!(
            object_key(&b.prefix, &root, &root.join("clip.mkv"))
                .unwrap_err()
                .to_string()
                .contains("512 bytes")
        );
        b.prefix = String::new();
        b.credentials = Credentials::Profile {
            name: "has spaces".into(),
        };
        let error = b.validate().unwrap_err().to_string();
        assert!(error.starts_with("AWS profile"));
        assert!(error.contains("1-64") && error.contains("letters"));
    }

    #[test]
    fn resolution_places_relative_outputs_inside_the_destination() {
        let dir = std::env::temp_dir().join(format!("capturefab-dest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let folder = Destination {
            name: "lab".into(),
            target: Target::Folder { path: dir.clone() },
        };
        let resolved = resolve_with(&folder, "run-1/shot-{frame}.png").unwrap();
        assert_eq!(
            PathBuf::from(&resolved.output),
            dir.join("run-1/shot-{frame}.png")
        );
        assert!(resolved.upload.is_none());
        for bad in [
            "/etc/x.png",
            "../x.png",
            "a/../../x.png",
            "",
            "\0file",
            "file:out.mp4",
            "https://host/live",
        ] {
            assert!(resolve_with(&folder, bad).is_err(), "{bad}");
        }
        let staged = Destination {
            name: "archive".into(),
            target: Target::S3(bucket()),
        };
        if cfg!(feature = "s3") {
            // Under the staging root, wherever the environment puts it.
            let resolved = resolve_with(&staged, "frames").unwrap();
            assert_eq!(
                PathBuf::from(&resolved.output),
                staging_root().join("archive").join("frames")
            );
            assert!(
                matches!(&resolved.upload, Some(Remote::S3(b)) if b.bucket == "camera-archive")
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn object_keys_join_the_prefix_and_relative_path() {
        let root = Path::new("/staging/archive");
        let key = |prefix: &str, path: &str| object_key(prefix, root, Path::new(path));
        assert_eq!(
            key("line-1", "/staging/archive/run/a.png").unwrap(),
            "line-1/run/a.png"
        );
        assert_eq!(
            key("line-1/", "/staging/archive/a.png").unwrap(),
            "line-1/a.png"
        );
        assert_eq!(key("", "/staging/archive/a.png").unwrap(), "a.png");
        assert!(key("x", "/elsewhere/a.png").is_err());
        assert!(key("/x", "/staging/archive/a.png").is_err());
        assert!(key("x", "/staging/archive/../outside.png").is_err());
        assert!(key("x", "/staging/archive/a\n.png").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn named_outputs_cannot_escape_through_existing_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-destination-symlink-{}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("folder")).unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("folder/link")).unwrap();
        let destination = Destination {
            name: "lab".into(),
            target: Target::Folder {
                path: root.join("folder"),
            },
        };
        let error = resolve_with(&destination, "link/new/run/shot.png").unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert!(!root.join("outside/new").exists());
        assert!(resolve_with(&destination, "inside/shot.png").is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn unsupported_folder_encoding_is_rejected_without_lossy_path_redirection() {
        use std::os::unix::ffi::OsStringExt;
        let root = std::env::temp_dir().join(std::ffi::OsString::from_vec(
            b"capturefab-invalid-utf8-\xff".to_vec(),
        ));
        let destination = Destination {
            name: "lab".into(),
            target: Target::Folder { path: root.clone() },
        };
        assert!(
            destination
                .validate()
                .unwrap_err()
                .to_string()
                .contains("valid UTF-8")
        );
        assert!(
            resolve_with(&destination, "new/shot.png")
                .unwrap_err()
                .to_string()
                .contains("valid UTF-8")
        );
        assert!(!root.join("new").exists());
        let replacement_path = PathBuf::from(root.to_string_lossy().as_ref());
        assert!(!replacement_path.join("new").exists());
    }

    #[test]
    fn ftp_sftp_targets_round_trip_with_kind_tags() {
        for target in [Target::Ftp(ftp()), Target::Sftp(sftp())] {
            let saved = Destination {
                name: "line".into(),
                target,
            };
            let value = serde_json::to_value(&saved).unwrap();
            assert_eq!(serde_json::from_value::<Destination>(value).unwrap(), saved);
            saved.validate().unwrap();
        }
        let value = serde_json::to_value(Destination {
            name: "line".into(),
            target: Target::Ftp(ftp()),
        })
        .unwrap();
        assert_eq!(value["kind"], "ftp");
        assert_eq!(value["tls"], "explicit");
        assert_eq!(value["password"], "keychain");
        let mut minimal = serde_json::json!({"name":"x","kind":"sftp","host":"h","user":"u","host_key":format!("SHA256:{}", "b".repeat(43)),"password":"none"});
        let parsed: Destination = serde_json::from_value(minimal.clone()).unwrap();
        let Target::Sftp(server) = &parsed.target else {
            panic!("sftp")
        };
        assert_eq!((server.port, server.password), (22, SecretSource::NoSecret));
        minimal["hostkey"] = serde_json::json!("typo");
        assert!(serde_json::from_value::<Destination>(minimal).is_err());
        assert_eq!(
            Remote::Sftp(sftp()).secret_ref(),
            None,
            "environment secrets are not in the keychain"
        );
        assert_eq!(
            Remote::Ftp(ftp()).secret_ref(),
            Some((FTP_SERVICE, "cam@10.0.0.5:21".into()))
        );
        let mut keyed = sftp();
        keyed.password = SecretSource::Keychain;
        keyed.key_file = Some(std::env::temp_dir().join("id_ed25519"));
        assert_eq!(
            Remote::Sftp(keyed).secret_ref(),
            Some((SFTP_KEY_SERVICE, "cam@nas.local:2222".into()))
        );
    }

    #[test]
    fn existing_folder_and_s3_files_parse_and_reserialize_identically() {
        let text = r#"{"version":1,"destinations":[{"name":"archive","kind":"s3","endpoint":"https://minio.example:9000","region":"us-east-1","bucket":"camera-archive","prefix":"line-1","path_style":true,"credentials":{"source":"environment"},"keep_local":false},{"name":"usb","kind":"folder","path":"/captures"}]}"#;
        let file: File = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&file).unwrap(), text);
        let Target::S3(bucket) = &file.destinations[0].target else {
            panic!("s3")
        };
        assert_eq!(bucket.no_overwrite, NoOverwrite::IfNoneMatch);
        let mut check_first = bucket.clone();
        check_first.no_overwrite = NoOverwrite::CheckFirst;
        assert_eq!(
            serde_json::to_value(&check_first).unwrap()["no_overwrite"],
            "check_first"
        );
    }

    #[test]
    fn ftp_rejects_crlf_user_dir_traversal_bad_host_and_port_zero() {
        let check = |change: fn(&mut FtpServer)| {
            let mut server = ftp();
            change(&mut server);
            server.validate()
        };
        assert!(check(|_| {}).is_ok());
        assert!(check(|s| s.host = "[::1]".into()).is_ok());
        assert!(check(|s| s.dir = "/abs/dir".into()).is_ok());
        assert!(check(|s| s.dir = String::new()).is_ok());
        for bad in [
            "ftp://h", "h:21", "user@h", "-h", ".h", "", "a..b", "[not-ip]", "h h",
        ] {
            let mut server = ftp();
            server.host = bad.into();
            let error = server.validate().unwrap_err().to_string();
            assert!(error.starts_with("FTP host"), "{bad}: {error}");
        }
        assert!(
            check(|s| s.user = "cam\r\nDELE x".into())
                .unwrap_err()
                .to_string()
                .contains("FTP user")
        );
        assert!(check(|s| s.user = "has space".into()).is_err());
        assert!(check(|s| s.user = String::new()).is_err());
        for dir in ["../a", "a/../b", "a\\b", "a\nb", "./a"] {
            let mut server = ftp();
            server.dir = dir.into();
            assert!(
                server
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("remote folder"),
                "{dir}"
            );
        }
        assert_eq!(
            check(|s| s.port = 0).unwrap_err().to_string(),
            "port must be 1-65535"
        );
    }

    #[test]
    fn sftp_host_key_requires_openssh_sha256() {
        assert!(validate_host_key(&format!("SHA256:{}", "a+/9".repeat(10) + "abc")).is_ok());
        for bad in [
            "",
            "bogus",
            "MD5:aa:bb",
            &format!("SHA256:{}", "a".repeat(42)),
            &format!("SHA256:{}=", "a".repeat(43)),
            &format!("sha256:{}", "a".repeat(43)),
        ] {
            assert!(validate_host_key(bad).is_err(), "{bad}");
        }
        let mut server = sftp();
        server.key_file = Some("relative/id".into());
        assert_eq!(
            server.validate().unwrap_err().to_string(),
            "SFTP key file must be an absolute path"
        );
    }

    #[test]
    fn location_omits_user_and_marks_plain_ftp_insecure() {
        let remote = Remote::Ftp(ftp());
        assert_eq!(remote.location(), "ftps://10.0.0.5:21/incoming");
        assert!(!remote.insecure());
        assert_eq!(
            remote.object_url("2024/a.png"),
            "ftps://10.0.0.5:21/incoming/2024/a.png"
        );
        let mut plain = ftp();
        plain.tls = FtpTls::Plain;
        plain.dir = String::new();
        let remote = Remote::Ftp(plain);
        assert!(remote.insecure());
        assert_eq!(remote.location(), "ftp://10.0.0.5:21/");
        assert_eq!(remote.object_url("a.png"), "ftp://10.0.0.5:21/a.png");
        let remote = Remote::Sftp(sftp());
        assert_eq!(remote.location(), "sftp://NAS.local:2222//srv/line-1");
        assert!(!remote.location().contains("cam"));
        assert_eq!(remote.kind(), "sftp");
        assert_eq!(remote.key_prefix(), "");
        assert_eq!(
            Remote::S3(bucket()).object_url("line-1/a.png"),
            "s3://camera-archive/line-1/a.png"
        );
        assert_eq!(Remote::S3(bucket()).key_prefix(), "line-1");
        let destination = Destination {
            name: "line".into(),
            target: Target::Ftp(ftp()),
        };
        assert_eq!(destination.describe(), "ftps://10.0.0.5:21/incoming");
        for (endpoint, needed) in [
            ("http://127.0.0.1:9000", true),
            ("http://LOCALHOST:9000", true),
            ("http://[::1]:9000", true),
            ("https://minio.example:9000", false),
            ("", false),
        ] {
            assert_eq!(endpoint_needs_path_style(endpoint), needed, "{endpoint}");
        }
        assert!(destination.target.is_remote());
        assert!(!folder("x").target.is_remote());
        assert!(folder("x").target.remote().is_none());
    }

    #[test]
    fn remote_resolution_stages_under_staging_root_and_queues_relative_keys() {
        // `staging_root` reads the environment, so the network targets are
        // resolved against an explicit staging folder instead.
        let root =
            std::env::temp_dir().join(format!("capturefab-ftp-staging-{}", std::process::id()));
        fs::create_dir_all(root.join("run")).unwrap();
        for target in [Target::Ftp(ftp()), Target::Sftp(sftp())] {
            let remote = target.remote().unwrap();
            let compiled = match &remote {
                Remote::Ftp(_) => cfg!(feature = "ftp"),
                Remote::Sftp(_) => cfg!(feature = "sftp"),
                Remote::S3(_) => cfg!(feature = "s3"),
            };
            let destination = Destination {
                name: "line".into(),
                target,
            };
            match resolve_with(&destination, "run/shot.png") {
                Ok(resolved) => {
                    assert!(compiled);
                    assert_eq!(resolved.upload.as_ref(), Some(&remote));
                    assert_eq!(
                        PathBuf::from(&resolved.output),
                        staging_root().join("line/run/shot.png")
                    );
                }
                Err(error) => {
                    assert!(!compiled);
                    assert!(error.to_string().contains("unsupported in this build"));
                }
            }
            let file = root.join("run/shot.png");
            fs::write(&file, b"x").unwrap();
            assert_eq!(
                object_key(remote.key_prefix(), &root, &file).unwrap(),
                "run/shot.png"
            );
        }
        let _ = fs::remove_dir_all(root);
    }
}
