//! Saved hubs: `hubs.json` in the private session directory. A record names
//! a server and where its secret comes from; it never holds the secret.
//! Updates are serialized across processes with an OS lock and written to a
//! sibling file that is renamed into place, as destinations are.
use super::Stream;
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// The keychain service that holds every hub secret.
pub const KEYCHAIN_SERVICE: &str = "capturefab-hub";
const MAX_FILE: usize = 1 << 20;
const MAX_HUBS: usize = 256;
const MAX_URL: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    HomeAssistant,
    Frigate,
    Go2rtc,
    UnifiProtect,
}

impl Kind {
    pub const ALL: [Kind; 4] = [
        Kind::HomeAssistant,
        Kind::Frigate,
        Kind::Go2rtc,
        Kind::UnifiProtect,
    ];
    /// The product name shown to people.
    pub fn label(self) -> &'static str {
        match self {
            Self::HomeAssistant => "Home Assistant",
            Self::Frigate => "Frigate",
            Self::Go2rtc => "go2rtc",
            Self::UnifiProtect => "UniFi Protect",
        }
    }
    /// The `kind` value in JSON and keychain accounts.
    pub fn id(self) -> &'static str {
        match self {
            Self::HomeAssistant => "home_assistant",
            Self::Frigate => "frigate",
            Self::Go2rtc => "go2rtc",
            Self::UnifiProtect => "unifi_protect",
        }
    }
    /// The `hub` subcommand that saves this kind.
    pub fn command(self) -> &'static str {
        match self {
            Self::HomeAssistant => "add-home-assistant",
            Self::Frigate => "add-frigate",
            Self::Go2rtc => "add-go2rtc",
            Self::UnifiProtect => "add-unifi-protect",
        }
    }
    /// What the secret is, for prompts and messages.
    pub fn secret_noun(self) -> &'static str {
        match self {
            Self::HomeAssistant => "access token",
            Self::Frigate | Self::Go2rtc => "password",
            Self::UnifiProtect => "API key",
        }
    }
    /// What a camera identifier looks like on this kind of hub.
    pub fn camera_example(self) -> &'static str {
        match self {
            Self::HomeAssistant => "camera.ENTITY",
            Self::Frigate => "CAMERA",
            Self::Go2rtc => "STREAM",
            Self::UnifiProtect => "CAMERA_ID",
        }
    }
    /// The streams `stream=auto` tries, in order.
    pub fn auto_order(self) -> &'static [Stream] {
        match self {
            Self::HomeAssistant => &[Stream::Mjpeg, Stream::Snapshot],
            Self::Frigate | Self::Go2rtc => &[Stream::Rtsp, Stream::Snapshot],
            Self::UnifiProtect => &[Stream::Snapshot],
        }
    }
    /// Streams that can be asked for explicitly.
    pub fn streams(self) -> &'static [Stream] {
        match self {
            Self::HomeAssistant => &[Stream::Mjpeg, Stream::Snapshot],
            Self::Frigate | Self::Go2rtc => &[Stream::Rtsp, Stream::Mjpeg, Stream::Snapshot],
            Self::UnifiProtect => &[Stream::Snapshot],
        }
    }
    /// Snapshot polling rate: (minimum, maximum, default) frames per second.
    /// Snapshots load the server, and UniFi consoles share a small request
    /// budget among all clients.
    pub fn snapshot_fps(self) -> (f64, f64, f64) {
        match self {
            Self::HomeAssistant | Self::Go2rtc => (0.01, 2.0, 1.0),
            Self::Frigate => (0.01, 5.0, 1.0),
            Self::UnifiProtect => (0.01, 1.0, 0.5),
        }
    }
    /// Check a camera identifier and return its canonical form.
    pub fn validate_camera(self, camera: &str, hub: &str) -> Result<String> {
        let hint = || format!("list cameras with `capturefab hub cameras {hub}`");
        match self {
            Self::HomeAssistant => {
                let entity = camera.strip_prefix("camera.").unwrap_or("");
                usage(
                    !entity.is_empty()
                        && entity.len() <= 248
                        && entity
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                    || {
                        format!(
                            "{camera} is not a Home Assistant camera entity ID (camera.<lowercase letters, digits, _>); {}",
                            hint()
                        )
                    },
                )?;
                Ok(camera.into())
            }
            Self::Frigate => {
                usage(
                    (1..=64).contains(&camera.len())
                        && camera
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                    || {
                        format!(
                            "{camera} is not a Frigate camera name (letters, digits, _ and -); {}",
                            hint()
                        )
                    },
                )?;
                Ok(camera.into())
            }
            // Any name the shape check accepts; opening requires an exact
            // match with a stream the server lists.
            Self::Go2rtc => Ok(camera.into()),
            Self::UnifiProtect => {
                usage(
                    camera.len() == 24 && camera.bytes().all(|b| b.is_ascii_hexdigit()),
                    || {
                        format!(
                            "{camera} is not a UniFi Protect camera ID (24 hexadecimal digits); {}",
                            hint()
                        )
                    },
                )?;
                Ok(camera.to_ascii_lowercase())
            }
        }
    }
    /// Check a selector's stream and frame rate against this kind of hub.
    pub fn validate_options(self, stream: Stream, fps: Option<f64>) -> Result<()> {
        if stream != Stream::Auto && !self.streams().contains(&stream) {
            let allowed = std::iter::once("auto")
                .chain(self.streams().iter().map(|s| s.name()))
                .collect::<Vec<_>>()
                .join(", ");
            let works = if stream == Stream::Rtsp {
                "stream=rtsp works only with Frigate and go2rtc hubs; "
            } else {
                ""
            };
            return Err(crate::dataset::Usage(format!(
                "{works}{} hubs take stream={allowed}",
                self.label()
            ))
            .into());
        }
        let Some(fps) = fps else { return Ok(()) };
        match (self, stream) {
            (_, Stream::Snapshot) => {
                let (min, max, _) = self.snapshot_fps();
                usage((min..=max).contains(&fps), || {
                    format!(
                        "fps for {} snapshots must be {}..{}",
                        self.label(),
                        format_number(min),
                        format_number(max)
                    )
                })
            }
            (Self::Frigate, Stream::Mjpeg) => {
                usage((1.0..=10.0).contains(&fps) && fps.fract() == 0.0, || {
                    "fps for Frigate MJPEG must be a whole number 1..10".into()
                })
            }
            _ => Err(crate::dataset::Usage(format!(
                "fps applies to stream=snapshot{}; add stream=snapshot to poll {} snapshots",
                if self == Self::Frigate {
                    " and stream=mjpeg"
                } else {
                    ""
                },
                self.label()
            ))
            .into()),
        }
    }
}

fn usage(ok: bool, message: impl FnOnce() -> String) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(crate::dataset::Usage(message()).into())
    }
}

/// A frame rate as people write it: `2`, `0.5`, `0.125`.
pub fn format_number(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Where a hub's secret comes from. The secret itself is never stored in
/// Capturefab files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSource {
    /// The OS credential store, service `capturefab-hub`.
    Keychain,
    /// The variable named by `secret_env`, in the process that opens or
    /// lists the camera.
    Environment,
    /// No secret: the server accepts anonymous requests.
    None,
}

/// A secret the server refused, recorded so that discovery stops sending it:
/// every Home Assistant 401 raises a "Login attempt failed" notification and
/// counts toward an IP ban.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rejected {
    /// RFC 3339 UTC.
    pub at: String,
    pub status: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hub {
    pub name: String,
    pub kind: Kind,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Required: an old or hand-written record never defaults to a keychain
    /// lookup that might belong to another server.
    pub secret: SecretSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_env: Option<String>,
    /// SHA-256 of the only certificate this server may present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    /// Verify the certificate against this DNS name instead of the URL host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_server_name: Option<String>,
    /// Frigate's RTSP restream port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtsp_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<Rejected>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}
impl Scheme {
    pub fn name(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// A hub URL split into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: Scheme,
    /// A DNS name or IP address, without IPv6 brackets.
    pub host: String,
    pub port: u16,
    /// A base path without a trailing `/`, or empty.
    pub path: String,
}
impl Url {
    /// `host:port` for messages and the `Host` header, IPv6 in brackets.
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == self.scheme.default_port() {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
    /// The host as written in a URL: IPv6 addresses in brackets.
    pub fn authority_host(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }
    /// `scheme://host[:port]`, for messages.
    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme.name(), self.authority())
    }
    pub fn is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }
}

const URL_HINT: &str = "hub URL must be http(s)://HOST[:PORT][/PATH] without a user name, password, query or fragment; give secrets with --secret-stdin or --secret-env";

/// Parse and validate a hub URL. The message never repeats the URL, which a
/// user may have pasted with a password in it.
pub fn parse_url(url: &str) -> Result<Url> {
    let usage = || anyhow::Error::from(crate::dataset::Usage(URL_HINT.into()));
    if url.len() > MAX_URL || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(usage());
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(usage)?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        _ => return Err(usage()),
    };
    if rest.contains(['?', '#', '@']) {
        return Err(usage());
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']').ok_or_else(usage)?;
        if host.contains('%') || host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(usage());
        }
        let port = match suffix {
            "" => None,
            _ => Some(suffix.strip_prefix(':').ok_or_else(usage)?),
        };
        (host.to_owned(), port)
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if host.is_empty()
            || host.len() > 253
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
        {
            return Err(usage());
        }
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        None => scheme.default_port(),
        Some(text) => text
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0 && !text.starts_with('0'))
            .ok_or_else(usage)?,
    };
    let path = path.trim_end_matches('/');
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        let ok = if byte == b'%' {
            bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                && bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
        } else {
            byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte)
        };
        if !ok {
            return Err(usage());
        }
    }
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(usage());
    }
    Ok(Url {
        scheme,
        host,
        port,
        path: path.to_owned(),
    })
}

pub fn validate_name(name: &str) -> Result<()> {
    usage(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && !name.starts_with('.'),
        || "hub names are 1-64 letters, digits, '.', '-' and '_' and cannot start with '.'".into(),
    )
}

/// The variable a hub reads its secret from. Restricting it to one prefix
/// keeps a saved record from naming any other credential (AWS keys, AI
/// provider keys) and lets FFmpeg's environment be scrubbed.
pub fn validate_secret_env(name: &str) -> Result<()> {
    usage(
        name.strip_prefix(crate::media::HUB_SECRET_PREFIX)
            .is_some_and(|rest| {
                (1..=48).contains(&rest.len())
                    && rest
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            }),
        || {
            "--secret-env takes a variable name such as CAPTUREFAB_HUB_HOME_TOKEN (CAPTUREFAB_HUB_ followed by A-Z, 0-9 and _), not the secret itself".into()
        },
    )
}

/// The hub user name for Frigate and go2rtc.
pub fn validate_user(user: &str) -> Result<()> {
    usage(
        (1..=256).contains(&user.len())
            && !user.chars().any(|c| c.is_control())
            && !user.contains(':'),
        || "hub user names are 1-256 characters without ':' or control characters".into(),
    )
}

/// A DNS name to verify the certificate against.
pub fn validate_server_name(name: &str) -> Result<()> {
    usage(
        !name.is_empty()
            && name.len() <= 253
            && name.parse::<std::net::IpAddr>().is_err()
            && name.split('.').all(|label| {
                (1..=63).contains(&label.len())
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            }),
        || {
            "--tls-server-name takes the DNS name on the server's certificate, such as ha.example.com".into()
        },
    )
}

/// Accept `SHA256:AB:CD:…`, `ab:cd:…` or 64 hexadecimal digits in any case.
pub fn parse_fingerprint(text: &str) -> Result<[u8; 32]> {
    let hint = || {
        anyhow::Error::from(crate::dataset::Usage(
            "certificate fingerprints are 32 SHA-256 bytes in hexadecimal, as `openssl x509 -noout -fingerprint -sha256` prints them (SHA256:AB:CD:…)".into(),
        ))
    };
    let text = text.trim();
    let digits = match text.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("sha256:") => &text[7..],
        _ => text,
    };
    let digits = digits
        .strip_prefix("sha256 Fingerprint=")
        .or_else(|| digits.strip_prefix("SHA256 Fingerprint="))
        .unwrap_or(digits)
        .replace(':', "");
    if digits.len() != 64 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(hint());
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[index * 2..index * 2 + 2], 16).map_err(|_| hint())?;
    }
    Ok(bytes)
}

/// The openssl form, `SHA256:AB:CD:…`.
pub fn format_fingerprint(bytes: &[u8; 32]) -> String {
    let hex = bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    format!("SHA256:{hex}")
}

impl Hub {
    pub fn parsed_url(&self) -> Result<Url> {
        parse_url(&self.url)
    }
    /// Check every field and the rules for this kind of hub.
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        let url = self.parsed_url()?;
        if let Some(user) = &self.user {
            validate_user(user)?;
        }
        match (&self.secret, &self.secret_env) {
            (SecretSource::Environment, Some(variable)) => validate_secret_env(variable)?,
            (SecretSource::Environment, None) => bail!(
                "hub {} reads its secret from the environment but names no variable",
                self.name
            ),
            (_, Some(_)) => bail!(
                "hub {} names a secret variable but does not read the environment",
                self.name
            ),
            _ => {}
        }
        if let Some(fingerprint) = &self.certificate {
            parse_fingerprint(fingerprint)?;
        }
        if let Some(name) = &self.tls_server_name {
            validate_server_name(name)?;
        }
        let https = url.scheme == Scheme::Https;
        usage(
            https || (self.certificate.is_none() && self.tls_server_name.is_none()),
            || {
                "--certificate, --accept-certificate and --tls-server-name need an https:// hub URL"
                    .into()
            },
        )?;
        usage(
            self.certificate.is_none() || self.tls_server_name.is_none(),
            || {
                "use either --certificate (a pinned certificate) or --tls-server-name, not both"
                    .into()
            },
        )?;
        let has_secret = self.secret != SecretSource::None;
        match self.kind {
            Kind::HomeAssistant => {
                usage(has_secret, || {
                    "Home Assistant hubs need a long-lived access token: --secret-stdin or --secret-env VAR".into()
                })?;
                usage(self.user.is_none(), || {
                    "Home Assistant hubs take an access token, not --user".into()
                })?;
            }
            Kind::Frigate => {
                usage(self.user.is_some() == has_secret, || {
                    "Frigate hubs take --user with --secret-stdin or --secret-env VAR, or neither for the unauthenticated port 5000".into()
                })?;
                usage(!has_secret || https, || {
                    "Frigate passwords are only sent over https://; use https://HOST:8971, or http://HOST:5000 without --user".into()
                })?;
            }
            Kind::Go2rtc => usage(self.user.is_some() == has_secret, || {
                "go2rtc hubs take --user with --secret-stdin or --secret-env VAR, or neither when the API has no password".into()
            })?,
            Kind::UnifiProtect => {
                usage(https, || {
                    "UniFi Protect hubs need https://CONSOLE; the integration API is only served over HTTPS".into()
                })?;
                usage(has_secret, || {
                    "UniFi Protect hubs need an API key: --secret-stdin or --secret-env VAR".into()
                })?;
                usage(self.user.is_none(), || {
                    "UniFi Protect hubs take an API key, not --user".into()
                })?;
            }
        }
        usage(
            self.rtsp_port.is_none() || self.kind == Kind::Frigate,
            || "--rtsp-port applies to Frigate hubs".into(),
        )?;
        if self.rtsp_port == Some(0) {
            bail!("--rtsp-port must be 1..65535");
        }
        if self.kind == Kind::Frigate {
            ensure!(
                self.rtsp_port.is_some(),
                "Frigate hub {} has no RTSP port",
                self.name
            );
        }
        Ok(())
    }
    /// The keychain account for this hub's secret. It names the server, so a
    /// changed URL, or an isolated session directory pointing at a test
    /// server, can never read or overwrite another server's secret.
    pub fn account(&self) -> Result<String> {
        let url = self.parsed_url()?;
        let host = if url.host.contains(':') {
            format!("[{}]", url.host)
        } else {
            url.host.to_ascii_lowercase()
        };
        let user = self
            .user
            .as_deref()
            .map(|user| format!("{}@", super::encode(user)))
            .unwrap_or_default();
        Ok(format!(
            "{}:{}@{}://{user}{host}:{}{}",
            self.kind.id(),
            self.name,
            url.scheme.name(),
            url.port,
            url.path
        ))
    }
    /// A digest of everything that decides where and how requests go, so a
    /// worker that started before the hub was saved again cannot mark the
    /// new record rejected, and cached listings are dropped when it changes.
    pub fn digest(&self) -> [u8; 32] {
        let mut record = self.clone();
        record.rejected = None;
        let bytes = serde_json::to_vec(&record).expect("hub records serialize");
        Sha256::digest(bytes).into()
    }
    /// Whether the secret crosses the network unencrypted.
    pub fn insecure(&self) -> bool {
        self.secret != SecretSource::None
            && self
                .parsed_url()
                .is_ok_and(|url| url.scheme == Scheme::Http)
    }
    pub fn pin(&self) -> Option<[u8; 32]> {
        self.certificate
            .as_deref()
            .and_then(|fingerprint| parse_fingerprint(fingerprint).ok())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    hubs: Vec<Hub>,
}

/// The saved hubs of one session directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn session() -> Self {
        Self::at(crate::ipc::session_dir())
    }
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    fn path(&self) -> PathBuf {
        self.dir.join("hubs.json")
    }
    pub fn list(&self) -> Result<Vec<Hub>> {
        Ok(self.read()?.hubs)
    }
    fn read(&self) -> Result<File> {
        let path = self.path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(File {
                    version: 1,
                    hubs: Vec::new(),
                });
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        ensure!(bytes.len() <= MAX_FILE, "the saved hub list is too large");
        let file: File = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid saved hub list {}", path.display()))?;
        ensure!(file.version == 1, "unsupported saved hub list version");
        ensure!(
            file.hubs.len() <= MAX_HUBS,
            "too many saved hubs (maximum {MAX_HUBS})"
        );
        let mut names = std::collections::HashSet::new();
        for hub in &file.hubs {
            hub.validate()
                .with_context(|| format!("invalid saved hub {}", hub.name))?;
            ensure!(
                names.insert(hub.name.to_ascii_lowercase()),
                "duplicate saved hub name {}",
                hub.name
            );
        }
        Ok(file)
    }
    /// The hub named `name`; a missing hub is reported as unavailable.
    pub fn get(&self, name: &str) -> Result<Hub> {
        validate_name(name)?;
        self.list()?
            .into_iter()
            .find(|hub| hub.name == name)
            .ok_or_else(|| missing(name))
    }
    fn write(&self, file: &File) -> Result<()> {
        crate::ipc::ensure_private_dir(&self.dir)?;
        let temp = self
            .dir
            .join(format!("hubs.json.{}.tmp", std::process::id()));
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
            ensure!(bytes.len() <= MAX_FILE, "the saved hub list is too large");
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
    /// Run `update` on the current list under the cross-process lock and
    /// write the result. When writing fails, `rollback` undoes whatever
    /// `update` did outside the file (a keychain write).
    fn locked<T>(
        &self,
        update: impl FnOnce(&mut File) -> Result<T>,
        rollback: impl FnOnce(&T) -> Result<()>,
    ) -> Result<T> {
        use fs2::FileExt;
        crate::ipc::ensure_private_dir(&self.dir)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("hubs.lock"))?;
        lock.lock_exclusive()?;
        let mut file = self.read()?;
        file.version = 1;
        let result = update(&mut file)?;
        if let Err(error) = self.write(&file) {
            rollback(&result)
                .context("restore the saved secret after the hub could not be saved")?;
            return Err(error);
        }
        Ok(result)
    }
    /// Save `hub`, replacing the hub with exactly its name. `prepare` runs
    /// under the lock with the record being replaced, before the file is
    /// written; `rollback` undoes it if writing fails.
    pub fn save_with<T>(
        &self,
        hub: &Hub,
        prepare: impl FnOnce(Option<&Hub>) -> Result<T>,
        rollback: impl FnOnce(&T) -> Result<()>,
    ) -> Result<(Option<Hub>, T)> {
        hub.validate()?;
        ensure!(
            hub.rejected.is_none(),
            "a newly saved hub is never rejected"
        );
        self.locked(
            |file| {
                if let Some(other) = file.hubs.iter().find(|other| {
                    other.name != hub.name && other.name.eq_ignore_ascii_case(&hub.name)
                }) {
                    return Err(crate::dataset::Usage(format!(
                        "a hub named {} already exists; hub names are case-insensitive",
                        other.name
                    ))
                    .into());
                }
                let index = file.hubs.iter().position(|other| other.name == hub.name);
                ensure!(
                    index.is_some() || file.hubs.len() < MAX_HUBS,
                    "too many saved hubs (maximum {MAX_HUBS})"
                );
                let previous = index.map(|index| file.hubs[index].clone());
                let prepared = prepare(previous.as_ref())?;
                match index {
                    Some(index) => file.hubs[index] = hub.clone(),
                    None => file.hubs.push(hub.clone()),
                }
                file.hubs.sort_by(|a, b| a.name.cmp(&b.name));
                Ok((previous, prepared))
            },
            |(_, prepared)| rollback(prepared),
        )
    }
    pub fn remove(&self, name: &str) -> Result<Hub> {
        validate_name(name)?;
        self.locked(
            |file| {
                let index = file
                    .hubs
                    .iter()
                    .position(|hub| hub.name == name)
                    .ok_or_else(|| missing(name))?;
                Ok(file.hubs.remove(index))
            },
            |_| Ok(()),
        )
    }
    /// Record that the server refused this record's secret. Nothing changes
    /// when the hub was saved again since `digest` was taken.
    pub fn mark_rejected(&self, name: &str, digest: &[u8; 32], status: u16) -> Result<()> {
        self.update_latch(name, digest, Some(status))
    }
    /// Forget a refusal after the secret worked again.
    pub fn clear_rejected(&self, name: &str, digest: &[u8; 32]) -> Result<()> {
        self.update_latch(name, digest, None)
    }
    fn update_latch(&self, name: &str, digest: &[u8; 32], status: Option<u16>) -> Result<()> {
        let current = self.list()?;
        let Some(hub) = current.iter().find(|hub| hub.name == name) else {
            return Ok(());
        };
        if hub.digest() != *digest || hub.rejected.is_none() == status.is_none() {
            return Ok(());
        }
        self.locked(
            |file| {
                if let Some(hub) = file
                    .hubs
                    .iter_mut()
                    .find(|hub| hub.name == name && hub.digest() == *digest)
                {
                    hub.rejected = status.map(|status| Rejected {
                        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        status,
                    });
                }
                Ok(())
            },
            |_| Ok(()),
        )
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn missing(name: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::Unavailable,
        format!("hub {name} not found; list hubs with `capturefab hub list`"),
    )
}

/// An error for a build without the network clients.
pub fn unsupported() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Unsupported,
        "camera hubs are unsupported in this build; rebuild with --features hubs",
    )
}

/// An error for `--secret-stdin` in a build without the OS credential store.
pub fn keychain_unsupported() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Unsupported,
        "saving hub secrets in the OS credential store is unsupported in this build; use --secret-env VAR",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(name: &str, kind: Kind, url: &str) -> Hub {
        Hub {
            name: name.into(),
            kind,
            url: url.into(),
            user: None,
            secret: SecretSource::Environment,
            secret_env: Some("CAPTUREFAB_HUB_TEST_TOKEN".into()),
            certificate: None,
            tls_server_name: None,
            rtsp_port: (kind == Kind::Frigate).then_some(8554),
            rejected: None,
        }
    }
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-hubs-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn urls_are_strict_and_messages_never_repeat_them() {
        let ok = parse_url("http://HomeAssistant.Local:8123").unwrap();
        assert_eq!(
            (ok.scheme, ok.host.as_str(), ok.port, ok.path.as_str()),
            (Scheme::Http, "homeassistant.local", 8123, "")
        );
        assert_eq!(ok.origin(), "http://homeassistant.local:8123");
        let base = parse_url("https://nvr.lan/frigate/").unwrap();
        assert_eq!((base.port, base.path.as_str()), (443, "/frigate"));
        assert_eq!(base.origin(), "https://nvr.lan");
        let v6 = parse_url("http://[fd00::1]:1984").unwrap();
        assert_eq!(
            (v6.host.as_str(), v6.authority()),
            ("fd00::1", "[fd00::1]:1984".into())
        );
        for bad in [
            "homeassistant.local:8123",
            "ftp://ha",
            "http://user:secret@ha:8123",
            "http://ha:8123/?token=secret",
            "http://ha:8123/#frag",
            "http://[fe80::1%25en0]:8123",
            "http://ha:0",
            "http://ha:65536",
            "http://ha:08123",
            "http://ha/a b",
            "http://ha/../etc",
            "http://",
            "http://ha/%zz",
        ] {
            let error = parse_url(bad).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.starts_with("hub URL must be"), "{bad}");
            assert!(
                !message.contains(bad) && !message.contains("user:secret"),
                "{bad}"
            );
            assert!(
                error
                    .chain()
                    .any(|cause| cause.is::<crate::dataset::Usage>())
            );
        }
    }

    #[test]
    fn records_follow_the_rules_for_their_kind() {
        let mut ha = hub("home", Kind::HomeAssistant, "http://ha.local:8123");
        ha.validate().unwrap();
        assert!(ha.insecure());
        ha.secret = SecretSource::None;
        ha.secret_env = None;
        assert!(ha.validate().is_err());
        let mut frigate = hub("nvr", Kind::Frigate, "http://nvr:5000");
        frigate.secret = SecretSource::None;
        frigate.secret_env = None;
        frigate.validate().unwrap();
        frigate.user = Some("viewer".into());
        frigate.secret = SecretSource::Keychain;
        let error = frigate.validate().unwrap_err();
        assert!(
            format!("{error}").contains("only sent over https"),
            "{error}"
        );
        frigate.url = "https://nvr:8971".into();
        frigate.certificate = Some(format_fingerprint(&[7; 32]));
        frigate.validate().unwrap();
        frigate.tls_server_name = Some("nvr.example.com".into());
        assert!(frigate.validate().is_err());
        let mut protect = hub("protect", Kind::UnifiProtect, "http://console");
        assert!(protect.validate().is_err());
        protect.url = "https://console".into();
        protect.validate().unwrap();
        let mut go2rtc = hub("g", Kind::Go2rtc, "http://g:1984");
        assert!(go2rtc.validate().is_err(), "a secret needs a user");
        go2rtc.user = Some("admin".into());
        go2rtc.validate().unwrap();
        go2rtc.rtsp_port = Some(8554);
        assert!(go2rtc.validate().is_err());
        let mut bad_env = hub("home", Kind::HomeAssistant, "http://ha");
        bad_env.secret_env = Some("AWS_SECRET_ACCESS_KEY".into());
        assert!(bad_env.validate().is_err());
    }

    #[test]
    fn secret_variables_have_one_prefix() {
        validate_secret_env("CAPTUREFAB_HUB_HOME_TOKEN").unwrap();
        for bad in [
            "CAPTUREFAB_HUB_",
            "CAPTUREFAB_HUB_home",
            "HOME_TOKEN",
            "CAPTUREFAB_FTP_PASSWORD",
            "CAPTUREFAB_HUB_A-B",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc",
        ] {
            let error = validate_secret_env(bad).unwrap_err();
            assert!(!format!("{error}").contains(bad) || bad.len() < 20, "{bad}");
        }
        assert!(validate_secret_env(&format!("CAPTUREFAB_HUB_{}", "A".repeat(49))).is_err());
    }

    #[test]
    fn fingerprints_accept_common_forms() {
        let bytes: [u8; 32] = std::array::from_fn(|i| i as u8 * 7);
        let text = format_fingerprint(&bytes);
        assert!(text.starts_with("SHA256:00:07:0E"));
        let plain = text[7..].replace(':', "").to_lowercase();
        for form in [
            text.clone(),
            text.to_lowercase(),
            text[7..].to_owned(),
            plain.clone(),
            format!("sha256 Fingerprint={}", &text[7..]),
        ] {
            assert_eq!(parse_fingerprint(&form).unwrap(), bytes, "{form}");
        }
        assert!(parse_fingerprint(&plain[2..]).is_err());
        assert!(parse_fingerprint("SHA256:zz").is_err());
    }

    #[test]
    fn accounts_name_the_server_and_the_hub() {
        let mut h = hub("home", Kind::HomeAssistant, "http://HA.local:8123");
        assert_eq!(
            h.account().unwrap(),
            "home_assistant:home@http://ha.local:8123"
        );
        h.url = "https://ha.local".into();
        assert_eq!(
            h.account().unwrap(),
            "home_assistant:home@https://ha.local:443"
        );
        let mut g = hub("g", Kind::Go2rtc, "http://[fd00::2]:1984/base");
        g.user = Some("ad min@x".into());
        assert_eq!(
            g.account().unwrap(),
            "go2rtc:g@http://ad%20min%40x@[fd00::2]:1984/base"
        );
    }

    #[test]
    fn camera_ids_and_options_follow_the_hub_kind() {
        assert_eq!(
            Kind::HomeAssistant
                .validate_camera("camera.front_door", "home")
                .unwrap(),
            "camera.front_door"
        );
        for bad in ["camera.Front", "light.kitchen", "camera.", "camera.a b"] {
            let error = Kind::HomeAssistant
                .validate_camera(bad, "home")
                .unwrap_err();
            assert!(
                error.chain().any(|c| c.is::<crate::dataset::Usage>()),
                "{bad}"
            );
        }
        assert!(Kind::Frigate.validate_camera("back_yard-2", "nvr").is_ok());
        assert!(Kind::Frigate.validate_camera("back yard", "nvr").is_err());
        assert_eq!(
            Kind::UnifiProtect
                .validate_camera("66D025B301EBC903E80003EA", "p")
                .unwrap(),
            "66d025b301ebc903e80003ea"
        );
        Kind::HomeAssistant
            .validate_options(Stream::Snapshot, Some(0.5))
            .unwrap();
        let rtsp = Kind::HomeAssistant
            .validate_options(Stream::Rtsp, None)
            .unwrap_err();
        assert!(format!("{rtsp}").starts_with("stream=rtsp works only with Frigate and go2rtc"));
        assert!(
            Kind::HomeAssistant
                .validate_options(Stream::Snapshot, Some(3.0))
                .is_err()
        );
        assert!(
            Kind::HomeAssistant
                .validate_options(Stream::Mjpeg, Some(1.0))
                .is_err()
        );
        assert!(
            Kind::HomeAssistant
                .validate_options(Stream::Auto, Some(1.0))
                .is_err()
        );
        Kind::Frigate
            .validate_options(Stream::Mjpeg, Some(5.0))
            .unwrap();
        assert!(
            Kind::Frigate
                .validate_options(Stream::Mjpeg, Some(2.5))
                .is_err()
        );
        assert!(
            Kind::Frigate
                .validate_options(Stream::Rtsp, Some(5.0))
                .is_err()
        );
        assert!(
            Kind::UnifiProtect
                .validate_options(Stream::Mjpeg, None)
                .is_err()
        );
        assert!(
            Kind::UnifiProtect
                .validate_options(Stream::Snapshot, Some(2.0))
                .is_err()
        );
    }

    #[test]
    fn the_store_keeps_records_private_and_names_unique() {
        let dir = temp("store");
        let store = Store::at(&dir);
        assert!(store.list().unwrap().is_empty());
        let home = hub("home", Kind::HomeAssistant, "http://ha:8123");
        let (previous, prepared) = store.save_with(&home, |_| Ok(7), |_| Ok(())).unwrap();
        assert!(previous.is_none());
        assert_eq!(prepared, 7);
        let mut replacement = home.clone();
        replacement.url = "http://ha2:8123".into();
        let (previous, _) = store
            .save_with(&replacement, |_| Ok(()), |_| Ok(()))
            .unwrap();
        assert_eq!(previous.as_ref(), Some(&home));
        assert_eq!(store.get("home").unwrap(), replacement);
        let clash = hub("Home", Kind::HomeAssistant, "http://ha:8123");
        let error = store.save_with(&clash, |_| Ok(()), |_| Ok(())).unwrap_err();
        assert!(format!("{error}").contains("case-insensitive"));
        let missing = store.get("nvr").unwrap_err();
        assert_eq!(crate::cli::error_code(&missing), ("unavailable", 3));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("hubs.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The latch only applies to the record it was taken from.
        let digest = replacement.digest();
        store.mark_rejected("home", &[0; 32], 401).unwrap();
        assert!(store.get("home").unwrap().rejected.is_none());
        store.mark_rejected("home", &digest, 401).unwrap();
        let latched = store.get("home").unwrap();
        assert_eq!(latched.rejected.as_ref().unwrap().status, 401);
        assert_eq!(latched.digest(), digest);
        store.clear_rejected("home", &digest).unwrap();
        assert!(store.get("home").unwrap().rejected.is_none());
        assert_eq!(store.remove("home").unwrap().name, "home");
        assert!(store.remove("home").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn records_without_a_secret_source_or_with_unknown_fields_are_refused() {
        let dir = temp("parse");
        fs::create_dir_all(&dir).unwrap();
        let store = Store::at(&dir);
        for body in [
            r#"{"version":1,"hubs":[{"name":"home","kind":"home_assistant","url":"http://ha"}]}"#,
            r#"{"version":1,"hubs":[{"name":"home","kind":"home_assistant","url":"http://ha","secret":"keychain","token":"x"}]}"#,
            r#"{"version":2,"hubs":[]}"#,
        ] {
            fs::write(dir.join("hubs.json"), body).unwrap();
            assert!(store.list().is_err(), "{body}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_rolls_back_what_prepare_did() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("rollback");
        let store = Store::at(&dir);
        let home = hub("home", Kind::HomeAssistant, "http://ha:8123");
        store.save_with(&home, |_| Ok(()), |_| Ok(())).unwrap();
        // A read-only directory refuses the sibling file the update is
        // written to, after `prepare` has run.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let rolled_back = std::cell::Cell::new(false);
        let mut changed = home.clone();
        changed.url = "http://other:8123".into();
        let result = store.save_with(
            &changed,
            |previous| {
                assert_eq!(previous, Some(&home));
                Ok("stored")
            },
            |prepared| {
                assert_eq!(*prepared, "stored");
                rolled_back.set(true);
                Ok(())
            },
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(rolled_back.get());
        assert_eq!(store.get("home").unwrap(), home);
        let _ = fs::remove_dir_all(dir);
    }
}
