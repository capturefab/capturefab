//! New-version checks against the public release list.
//!
//! The only request is a plain GET of the static `releases.json` that the
//! website publishes: no identifiers, cookies, query string or version in the
//! User-Agent. The desktop app checks at most once a day unless that is
//! turned off (Settings › Updates, `CAPTUREFAB_UPDATE_CHECK=off` or a policy
//! file); `capturefab version --check` and "Check for Updates…" check on
//! request. Nothing is ever downloaded or installed: a found release is shown
//! with its notes, its download link and the file's SHA-256.
//!
//! What the last check found lives in `<state>/updates.json`, outside the GUI
//! preferences so that `doctor` and `version` read it in any build. Whether a
//! release is new is decided again on every read, against this build's
//! version, so an upgrade never leaves a stale "available".
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// The release list the website publishes.
pub const UPDATE_URL: &str = "https://capturefab.com/releases.json";
/// Where every release can be downloaded by hand.
pub const DOWNLOADS_URL: &str = "https://capturefab.com/downloads.html";
/// Overrides the release list URL, for mirrors and tests.
pub const URL_ENV: &str = "CAPTUREFAB_UPDATE_URL";
/// `off` turns every check off, automatic and requested.
pub const CHECK_ENV: &str = "CAPTUREFAB_UPDATE_CHECK";
/// Automatic checks run at most this often.
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// The most of the release list that is read.
pub const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_STATE: u64 = 64 * 1024;
const MAX_POLICY: u64 = 4 * 1024;
const MAX_HIGHLIGHTS: usize = 6;
const MAX_TEXT: usize = 200;
const STATE_FILE: &str = "updates.json";
const LOCK_FILE: &str = "updates.lock";
/// Links a release list may point to, besides an override's own origin.
const ALLOWED_LINKS: [&str; 2] = [
    "https://capturefab.com/",
    "https://github.com/capturefab/capturefab/releases/",
];

/// This build's version.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------- errors

/// Why a check found nothing. The texts are fixed and never carry what a
/// server said; the category decides the exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateError {
    BadUrl,
    Offline,
    Unreachable,
    Timeout,
    Status(u16),
    TooLarge,
    Malformed,
    UnsupportedSchema,
    Redirected,
    PolicyOff(PolicySource),
    NotCompiled,
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadUrl => f.write_str(
                "CAPTUREFAB_UPDATE_URL must be an https:// URL, or http:// to this computer (127.0.0.1, [::1] or localhost), without a query",
            ),
            Self::Offline => f.write_str("No network connection"),
            Self::Unreachable => f.write_str("The release list server could not be reached"),
            Self::Timeout => f.write_str("The release list server didn't respond within 10 seconds"),
            Self::Status(code) => write!(f, "The release list server returned an error (HTTP {code})"),
            Self::TooLarge => f.write_str("The release list is larger than 256 KiB"),
            Self::Malformed => {
                f.write_str("The release list was not in a format this version understands")
            }
            Self::UnsupportedSchema => {
                f.write_str("This version can't read the release list. See the downloads page.")
            }
            Self::Redirected => {
                f.write_str("The release list moved; this version doesn't follow redirects")
            }
            Self::PolicyOff(source) => write!(
                f,
                "Update checks are turned off on this computer by {}",
                source.describe()
            ),
            Self::NotCompiled => f.write_str("This build has no update check."),
        }
    }
}

impl std::error::Error for UpdateError {}

impl UpdateError {
    /// The short name kept in `updates.json`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadUrl => "bad_url",
            Self::Offline => "offline",
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
            Self::Status(_) => "http_status",
            Self::TooLarge => "too_large",
            Self::Malformed => "malformed",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::Redirected => "redirected",
            Self::PolicyOff(_) => "policy_off",
            Self::NotCompiled => "not_compiled",
        }
    }

    fn from_code(code: &str, status: Option<u16>) -> Option<Self> {
        Some(match code {
            "offline" => Self::Offline,
            "unreachable" => Self::Unreachable,
            "timeout" => Self::Timeout,
            "http_status" => Self::Status(status.unwrap_or(0)),
            "too_large" => Self::TooLarge,
            "malformed" => Self::Malformed,
            "unsupported_schema" => Self::UnsupportedSchema,
            "redirected" => Self::Redirected,
            _ => return None,
        })
    }

    /// As a CLI error: a bad override is a usage error; everything else is
    /// categorized, so message text never decides the exit code.
    pub fn into_anyhow(self) -> anyhow::Error {
        use crate::types::{Categorized, ErrorKind};
        let kind = match &self {
            Self::BadUrl => return crate::cli::InvalidInput(self.to_string()).into(),
            Self::Offline | Self::Unreachable | Self::Status(_) => ErrorKind::Unavailable,
            Self::Timeout => ErrorKind::Timeout,
            Self::TooLarge | Self::Malformed | Self::UnsupportedSchema | Self::Redirected => {
                ErrorKind::OperationFailed
            }
            Self::PolicyOff(_) | Self::NotCompiled => ErrorKind::Unsupported,
        };
        Categorized::error(kind, self.to_string())
    }
}

// ---------------------------------------------------------------- URLs

/// An http(s) URL, split just enough to resolve links and check origins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    full: String,
    origin: String,
    path: String,
    loopback: bool,
}

impl Url {
    /// An `https://` or `http://` URL without user information.
    pub fn parse(text: &str) -> Option<Self> {
        if text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        {
            return None;
        }
        let (scheme, rest) = text.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        let authority = authority.to_ascii_lowercase();
        let host = if authority.starts_with('[') {
            &authority[..authority.find(']')? + 1]
        } else {
            authority.split(':').next().unwrap_or_default()
        };
        if host.is_empty() {
            return None;
        }
        let loopback = matches!(host, "localhost" | "[::1]")
            || host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip.is_loopback());
        let origin = format!("{scheme}://{authority}");
        let path = if path.is_empty() { "/" } else { path };
        Some(Self {
            full: format!("{origin}{path}"),
            origin,
            path: path.into(),
            loopback,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.full
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn is_loopback(&self) -> bool {
        self.loopback
    }

    /// `link` resolved against this URL: absolute, rooted at this origin or
    /// relative to this file's folder. Protocol-relative `//` links and
    /// dot segments are refused.
    pub fn join(&self, link: &str) -> Option<String> {
        if link.is_empty()
            || link.starts_with("//")
            || link
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        {
            return None;
        }
        let joined = if link.contains("://") {
            Self::parse(link)?.full
        } else if link.contains(':') && !link.starts_with('/') && !link.starts_with('.') {
            // Some other scheme, such as javascript: or file:.
            return None;
        } else if link.starts_with('/') {
            format!("{}{link}", self.origin)
        } else {
            let folder = self
                .path
                .split(['?', '#'])
                .next()
                .unwrap_or("/")
                .rsplit_once('/')
                .map_or("", |(folder, _)| folder);
            format!("{}{folder}/{link}", self.origin)
        };
        let path = joined.split_once("://").map_or("", |(_, rest)| rest);
        if path
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
        {
            return None;
        }
        Some(joined)
    }
}

/// The release list to read: `UPDATE_URL`, or a valid override.
pub fn manifest_url() -> Result<Url, UpdateError> {
    manifest_url_from(std::env::var(URL_ENV).ok().as_deref())
}

pub fn manifest_url_from(value: Option<&str>) -> Result<Url, UpdateError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(Url::parse(UPDATE_URL).expect("a valid release list URL"));
    };
    let url = Url::parse(value).ok_or(UpdateError::BadUrl)?;
    let https = url.full.starts_with("https://");
    if (!https && !url.loopback) || url.path.contains(['?', '#']) {
        return Err(UpdateError::BadUrl);
    }
    Ok(url)
}

/// The origin of a valid `CAPTUREFAB_UPDATE_URL`, whose links are trusted
/// like the website's.
pub fn override_origin() -> Option<String> {
    std::env::var(URL_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .and_then(|value| manifest_url_from(Some(&value)).ok())
        .map(|url| url.origin)
}

/// Whether a link from the release list may be shown or opened.
pub fn link_allowed(link: &str, override_origin: Option<&str>) -> bool {
    Url::parse(link).is_some_and(|url| {
        url.full == link
            && (ALLOWED_LINKS.iter().any(|prefix| link.starts_with(prefix))
                || override_origin.is_some_and(|origin| link.starts_with(&format!("{origin}/"))))
    })
}

// ---------------------------------------------------------------- policy

/// What turned checks off on a managed computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicySource {
    Env,
    File(PathBuf),
}

impl PolicySource {
    pub fn describe(&self) -> String {
        match self {
            Self::Env => CHECK_ENV.into(),
            Self::File(path) => path.display().to_string(),
        }
    }
    fn json(&self) -> Value {
        match self {
            Self::Env => json!({"source":"env"}),
            Self::File(path) => json!({"source":"file","path":path}),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    Allowed,
    Off(PolicySource),
}

impl Policy {
    pub fn allowed(&self) -> bool {
        *self == Self::Allowed
    }
}

/// Where a managed install can turn checks off: `{"update_check":"off"}`.
pub fn policy_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        paths.push(PathBuf::from(
            "/Library/Application Support/Capturefab/policy.json",
        ));
    }
    if cfg!(unix) {
        paths.push(PathBuf::from("/etc/capturefab/policy.json"));
    }
    if cfg!(windows)
        && let Some(data) = std::env::var_os("ProgramData")
    {
        paths.push(PathBuf::from(data).join("Capturefab").join("policy.json"));
    }
    paths
}

fn off_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "off" | "0" | "false" | "never"
    )
}

/// Whether checks may run here: `CAPTUREFAB_UPDATE_CHECK`, then the policy
/// files. Any other value of the variable is ignored, never read as "on".
pub fn policy() -> Policy {
    policy_from(std::env::var(CHECK_ENV).ok().as_deref(), &policy_paths())
}

pub fn policy_from(env: Option<&str>, files: &[PathBuf]) -> Policy {
    if env.is_some_and(off_value) {
        return Policy::Off(PolicySource::Env);
    }
    for path in files {
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let mut text = String::new();
        if file.take(MAX_POLICY + 1).read_to_string(&mut text).is_err()
            || text.len() as u64 > MAX_POLICY
        {
            continue;
        }
        if serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value["update_check"].as_str().map(off_value))
            .unwrap_or(false)
        {
            return Policy::Off(PolicySource::File(path.clone()));
        }
    }
    Policy::Allowed
}

/// A `CAPTUREFAB_UPDATE_CHECK` value that means nothing, for doctor.
pub fn unrecognized_env() -> Option<String> {
    unrecognized(std::env::var(CHECK_ENV).ok().as_deref())
}

fn unrecognized(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.trim().is_empty() && !off_value(value))
        .map(|value| value.chars().filter(|c| !c.is_control()).take(40).collect())
}

// ---------------------------------------------------------------- versions

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ident {
    // Numeric identifiers sort below alphanumeric ones (SemVer 2.0 §11).
    Num(u64),
    Alpha(String),
}

/// A SemVer 2.0 version; build metadata is accepted and ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<Ident>,
}

fn numeric(part: &str) -> Option<u64> {
    if part.is_empty()
        || !part.bytes().all(|b| b.is_ascii_digit())
        || (part.len() > 1 && part.starts_with('0'))
    {
        return None;
    }
    part.parse().ok()
}

fn identifier_chars(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let (text, build) = match text.split_once('+') {
            Some((text, build)) => (text, Some(build)),
            None => (text, None),
        };
        if build.is_some_and(|build| !build.split('.').all(identifier_chars)) {
            return None;
        }
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let (major, minor, patch) = (
            numeric(parts.next()?)?,
            numeric(parts.next()?)?,
            numeric(parts.next()?)?,
        );
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|part| {
                    if !identifier_chars(part) {
                        None
                    } else if part.bytes().all(|b| b.is_ascii_digit()) {
                        numeric(part).map(Ident::Num)
                    } else {
                        Some(Ident::Alpha(part.into()))
                    }
                })
                .collect::<Option<_>>()?,
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether `latest` should be offered to a build of `current`: newer, and
/// never a prerelease to a stable build.
pub fn offer(latest: &Version, current: &Version) -> bool {
    latest > current && (!latest.is_prerelease() || current.is_prerelease())
}

// ---------------------------------------------------------------- the release list

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Manifest {
    latest: Option<String>,
    releases: Vec<ManifestRelease>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ManifestRelease {
    version: String,
    date: Option<String>,
    url: Option<String>,
    checksums_url: Option<String>,
    highlights: Vec<Value>,
    assets: Vec<ManifestAsset>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ManifestAsset {
    name: Option<String>,
    os: String,
    arch: String,
    available: bool,
    sha256: Option<String>,
    size: Option<u64>,
    url: Option<String>,
}

/// The download for this computer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Download {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub platform: String,
}

/// What a check found: the newest release (`latest` is None when none is
/// published) or, with `error`, why the last check failed. A failure keeps
/// what an earlier check found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Outcome {
    pub latest: Option<String>,
    pub date: Option<String>,
    pub url: Option<String>,
    pub checksums_url: Option<String>,
    pub highlights: Vec<String>,
    pub download: Option<Download>,
    pub platform: Option<String>,
    pub error: Option<String>,
    pub status: Option<u16>,
}

impl Outcome {
    pub fn failure(&self) -> Option<UpdateError> {
        self.error
            .as_deref()
            .map(|code| UpdateError::from_code(code, self.status).unwrap_or(UpdateError::Malformed))
    }

    /// The release as the CLI reports it.
    pub fn release_json(&self) -> Value {
        match &self.latest {
            None => Value::Null,
            Some(version) => json!({
                "version": version,
                "date": self.date,
                "url": self.url,
                "checksums_url": self.checksums_url,
                "highlights": self.highlights,
                "download": self.download,
            }),
        }
    }
}

/// Plain display text: no control characters or direction overrides, one
/// space between words, at most `MAX_TEXT` characters.
pub fn sanitize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
                || (c.is_control() && !c.is_whitespace())
            {
                ' '
            } else {
                c
            }
        })
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TEXT)
        .collect()
}

/// The release list's name for this processor.
pub fn manifest_arch(arch: &str) -> &str {
    match arch {
        "arm" => "armv7",
        other => other,
    }
}

/// "macOS ARM64", as the downloads page names it.
pub fn platform_label(os: &str, arch: &str) -> String {
    let os = match os {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "ARM64",
        "armv7" | "arm" => "ARMv7",
        other => other,
    };
    format!("{os} {arch}")
}

/// This computer's platform name.
pub fn host_platform() -> String {
    platform_label(std::env::consts::OS, manifest_arch(std::env::consts::ARCH))
}

/// Read a release list fetched from `base`: the release named `latest`, with
/// only allowed links and the one download for `os`/`arch`.
pub fn select(
    body: &[u8],
    base: &Url,
    override_origin: Option<&str>,
    os: &str,
    arch: &str,
) -> Result<Outcome, UpdateError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| UpdateError::Malformed)?;
    if !value.is_object() {
        return Err(UpdateError::Malformed);
    }
    if value.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(
            if value.get("schema_version").is_some_and(Value::is_number) {
                UpdateError::UnsupportedSchema
            } else {
                UpdateError::Malformed
            },
        );
    }
    let manifest: Manifest = serde_json::from_value(value).map_err(|_| UpdateError::Malformed)?;
    let arch = manifest_arch(arch);
    let platform = platform_label(os, arch);
    let Some(latest) = manifest.latest else {
        return Ok(Outcome {
            platform: Some(platform),
            ..Outcome::default()
        });
    };
    let release = manifest
        .releases
        .into_iter()
        .find(|release| release.version == latest)
        .ok_or(UpdateError::Malformed)?;
    Version::parse(&latest).ok_or(UpdateError::Malformed)?;
    let link = |link: Option<&str>| {
        link.and_then(|link| base.join(link))
            .filter(|link| link_allowed(link, override_origin))
    };
    let date = release.date.filter(|date| {
        date.len() == 10 && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
    });
    let highlights = release
        .highlights
        .iter()
        .filter_map(Value::as_str)
        .map(sanitize)
        .filter(|line| !line.is_empty())
        .take(MAX_HIGHLIGHTS)
        .collect();
    let download = release
        .assets
        .iter()
        .find(|asset| asset.available && asset.os == os && asset.arch == arch)
        .and_then(|asset| {
            let sha256 = asset.sha256.as_deref()?.to_ascii_lowercase();
            if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            Some(Download {
                name: sanitize(asset.name.as_deref()?),
                url: link(asset.url.as_deref())?,
                sha256,
                size: asset.size.filter(|size| *size > 0)?,
                platform: platform.clone(),
            })
        });
    Ok(Outcome {
        latest: Some(latest),
        date,
        url: link(release.url.as_deref()),
        checksums_url: link(release.checksums_url.as_deref()),
        highlights,
        download,
        platform: Some(platform),
        error: None,
        status: None,
    })
}

// ---------------------------------------------------------------- status

/// What the last check means for this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Never checked.
    Unknown,
    /// No release is published yet.
    None,
    Current,
    /// This build is newer than the newest release.
    Newer(String),
    Available(String),
    Failed(UpdateError),
}

/// Decided on every read against `current`, never stored.
pub fn status(outcome: Option<&Outcome>, current: &str) -> Status {
    let Some(outcome) = outcome else {
        return Status::Unknown;
    };
    let known = outcome.latest.as_deref().and_then(|latest| {
        let (latest_version, current_version) = (Version::parse(latest)?, Version::parse(current)?);
        Some(if offer(&latest_version, &current_version) {
            Status::Available(latest.into())
        } else if latest_version < current_version {
            Status::Newer(latest.into())
        } else {
            Status::Current
        })
    });
    match (known, outcome.failure()) {
        (Some(Status::Available(version)), _) => Status::Available(version),
        (_, Some(error)) => Status::Failed(error),
        (Some(status), None) => status,
        (None, None) => Status::None,
    }
}

// ---------------------------------------------------------------- saved state

/// `<state>/updates.json`. Every change goes through `Settings::update`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    /// Automatic daily checks; unset means on.
    pub automatic: Option<bool>,
    /// A version the person chose not to be told about again.
    pub skipped: Option<String>,
    /// RFC 3339, UTC.
    pub last_checked: Option<String>,
    pub last_outcome: Option<Outcome>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            automatic: None,
            skipped: None,
            last_checked: None,
            last_outcome: None,
        }
    }
}

fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

impl Settings {
    pub fn automatic(&self) -> bool {
        self.automatic != Some(false)
    }

    pub fn last_checked_at(&self) -> Option<DateTime<Utc>> {
        self.last_checked
            .as_deref()
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.with_timezone(&Utc))
    }

    pub fn status(&self) -> Status {
        status(self.last_outcome.as_ref(), current())
    }

    /// An available version that was not skipped: what the menu and the
    /// notice announce.
    pub fn announce(&self) -> Option<String> {
        match self.status() {
            Status::Available(version) if self.skipped.as_deref() != Some(&version) => {
                Some(version)
            }
            _ => None,
        }
    }

    /// The saved state, or the default when it is missing, too large or
    /// unreadable.
    pub fn load(dir: &Path) -> Self {
        let Ok(file) = fs::File::open(dir.join(STATE_FILE)) else {
            return Self::default();
        };
        let mut bytes = Vec::new();
        if file.take(MAX_STATE + 1).read_to_end(&mut bytes).is_err()
            || bytes.len() as u64 > MAX_STATE
        {
            return Self::default();
        }
        serde_json::from_slice::<Self>(&bytes)
            .ok()
            .filter(|settings| settings.schema_version == 1)
            .unwrap_or_default()
    }

    fn save(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join(STATE_FILE);
        let temp = dir.join(format!("{STATE_FILE}.{}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&temp)?;
        output.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temp, path)
    }

    /// Change the saved state under `updates.lock`: re-read, apply
    /// `change`, write atomically.
    pub fn update(dir: &Path, change: impl FnOnce(&mut Self)) -> io::Result<Self> {
        let lock = open_lock(dir)?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let mut settings = Self::load(dir);
        change(&mut settings);
        let saved = settings.save(dir);
        let _ = fs2::FileExt::unlock(&lock);
        saved.map(|()| settings)
    }
}

fn open_lock(dir: &Path) -> io::Result<fs::File> {
    crate::ipc::ensure_private_dir(dir).map_err(io::Error::other)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(dir.join(LOCK_FILE))
}

/// Whether an automatic check is due: on, and never checked, checked a day
/// or more ago, or "checked" more than a day in the future.
pub fn due(settings: &Settings, now: DateTime<Utc>) -> bool {
    if !settings.automatic() {
        return false;
    }
    let Some(last) = settings.last_checked_at() else {
        return true;
    };
    let day = chrono::Duration::from_std(INTERVAL).expect("a day");
    now - last >= day || last - now > day
}

// ---------------------------------------------------------------- fetching

/// GETs the release list.
pub trait Fetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>, UpdateError>;
}

/// Fetch and read the release list at `url`.
pub fn fetch(fetcher: &dyn Fetcher, url: &Url) -> Result<Outcome, UpdateError> {
    let body = fetcher.get(url)?;
    if body.len() as u64 > MAX_MANIFEST {
        return Err(UpdateError::TooLarge);
    }
    let origin = (url.as_str() != UPDATE_URL).then(|| url.origin().to_owned());
    select(
        &body,
        url,
        origin.as_deref(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// Keep what a check found: only `last_checked` and `last_outcome` change.
pub fn record(
    dir: &Path,
    now: DateTime<Utc>,
    result: &Result<Outcome, UpdateError>,
) -> io::Result<Settings> {
    Settings::update(dir, |settings| {
        settings.last_checked = Some(timestamp(now));
        settings.last_outcome = Some(match result {
            Ok(outcome) => outcome.clone(),
            Err(error) => {
                let mut kept = settings.last_outcome.clone().unwrap_or_default();
                kept.error = Some(error.code().into());
                kept.status = match error {
                    UpdateError::Status(code) => Some(*code),
                    _ => None,
                };
                kept
            }
        });
    })
}

/// One requested check: policy, URL, fetch, then record the result.
pub fn check_with(
    fetcher: &dyn Fetcher,
    dir: &Path,
    url: Result<Url, UpdateError>,
    policy: &Policy,
    now: DateTime<Utc>,
) -> Result<(Outcome, Settings), UpdateError> {
    if let Policy::Off(source) = policy {
        return Err(UpdateError::PolicyOff(source.clone()));
    }
    let result = fetch(fetcher, &url?);
    let settings = record(dir, now, &result).unwrap_or_else(|_| Settings::load(dir));
    result.map(|outcome| (outcome, settings))
}

/// An automatic check, run only when it is due and no other process is
/// starting one. The attempt is recorded before the fetch, so a second
/// window that re-reads the state sees it is no longer due. `None` when
/// nothing ran.
pub fn automatic_with(
    fetcher: &dyn Fetcher,
    dir: &Path,
    now: DateTime<Utc>,
) -> Option<(Result<Outcome, UpdateError>, Settings)> {
    if !policy().allowed() {
        return None;
    }
    let url = manifest_url().ok()?;
    {
        let lock = open_lock(dir).ok()?;
        fs2::FileExt::try_lock_exclusive(&lock).ok()?;
        let mut settings = Settings::load(dir);
        let claimed = due(&settings, now) && {
            settings.last_checked = Some(timestamp(now));
            settings.save(dir).is_ok()
        };
        let _ = fs2::FileExt::unlock(&lock);
        if !claimed {
            return None;
        }
    }
    let result = fetch(fetcher, &url);
    let settings = record(dir, now, &result).unwrap_or_else(|_| Settings::load(dir));
    Some((result, settings))
}

/// The fetcher this build uses.
pub fn native() -> Result<Box<dyn Fetcher + Send + Sync>, UpdateError> {
    #[cfg(feature = "updates")]
    {
        Ok(Box::new(NativeFetcher::new()))
    }
    #[cfg(not(feature = "updates"))]
    {
        Err(UpdateError::NotCompiled)
    }
}

#[cfg(feature = "updates")]
pub struct NativeFetcher {
    remote: ureq::Agent,
    loopback: ureq::Agent,
}

#[cfg(feature = "updates")]
impl Default for NativeFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "updates")]
impl NativeFetcher {
    pub fn new() -> Self {
        Self::with_timeouts(Duration::from_secs(5), Duration::from_secs(10))
    }

    /// The remote agent uses the environment's proxy, as a browser would;
    /// the loopback agent, for a local override, never does.
    pub fn with_timeouts(connect: Duration, global: Duration) -> Self {
        let agent = |loopback: bool| {
            let tls = ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build();
            let mut config = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .https_only(!loopback)
                .max_redirects(0)
                .save_redirect_history(false)
                .user_agent("capturefab")
                .timeout_connect(Some(connect))
                .timeout_global(Some(global))
                .tls_config(tls);
            if loopback {
                config = config.proxy(None);
            }
            config.build().new_agent()
        };
        Self {
            remote: agent(false),
            loopback: agent(true),
        }
    }
}

#[cfg(feature = "updates")]
fn network_error(error: ureq::Error) -> UpdateError {
    use ureq::Error;
    match error {
        Error::Timeout(_) => UpdateError::Timeout,
        Error::Io(ref error) if error.kind() == io::ErrorKind::TimedOut => UpdateError::Timeout,
        Error::BodyExceedsLimit(_) => UpdateError::TooLarge,
        Error::TooManyRedirects | Error::RedirectFailed => UpdateError::Redirected,
        Error::HostNotFound => UpdateError::Offline,
        Error::Io(ref error) if error.kind() == io::ErrorKind::NetworkUnreachable => {
            UpdateError::Offline
        }
        Error::Protocol(_) | Error::BadUri(_) | Error::Http(_) => UpdateError::Malformed,
        _ => UpdateError::Unreachable,
    }
}

#[cfg(feature = "updates")]
impl Fetcher for NativeFetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>, UpdateError> {
        let agent = if url.is_loopback() && url.as_str().starts_with("http://") {
            &self.loopback
        } else {
            &self.remote
        };
        let mut response = agent
            .get(url.as_str())
            .header("Accept", "application/json")
            .call()
            .map_err(network_error)?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(UpdateError::Redirected);
        }
        if status != 200 {
            return Err(UpdateError::Status(status));
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_MANIFEST)
            .read_to_vec()
            .map_err(network_error)
    }
}

// ---------------------------------------------------------------- CLI and doctor

/// `version --json`'s `update_check`, and doctor's; never fetches.
pub fn report(settings: &Settings, policy: &Policy) -> Value {
    let status = settings.status();
    json!({
        "compiled": cfg!(feature = "updates"),
        "automatic": settings.automatic() && policy.allowed() && cfg!(feature = "updates"),
        "policy": match policy {
            Policy::Allowed => Value::Null,
            Policy::Off(source) => source.json(),
        },
        "skipped": settings.skipped,
        "last_checked": settings.last_checked,
        "latest": settings.last_outcome.as_ref().and_then(|outcome| outcome.latest.clone()),
        "update_available": match status {
            Status::Unknown => Value::Null,
            Status::Failed(_) if settings.last_outcome.as_ref().is_none_or(|o| o.latest.is_none()) => Value::Null,
            Status::Available(_) => json!(true),
            _ => json!(false),
        },
        "last_error": settings
            .last_outcome
            .as_ref()
            .and_then(Outcome::failure)
            .map(|error| error.code()),
    })
}

fn base(dir: &Path) -> Value {
    json!({
        "version": current(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "update_check": report(&Settings::load(dir), &policy()),
    })
}

/// `capturefab version [--check]`.
pub fn version_command(check: bool) -> anyhow::Result<Value> {
    let dir = crate::ipc::state_dir();
    if !check {
        return Ok(base(&dir));
    }
    let policy = policy();
    if let Policy::Off(source) = &policy {
        return Err(UpdateError::PolicyOff(source.clone()).into_anyhow());
    }
    let url = manifest_url().map_err(UpdateError::into_anyhow)?;
    let fetcher = native().map_err(UpdateError::into_anyhow)?;
    let now = Utc::now();
    let (outcome, settings) = check_with(fetcher.as_ref(), &dir, Ok(url), &policy, now)
        .map_err(UpdateError::into_anyhow)?;
    let available = matches!(status(Some(&outcome), current()), Status::Available(_));
    let mut value = base(&dir);
    value["latest"] = json!(outcome.latest);
    value["update_available"] = json!(available);
    value["skipped"] =
        json!(outcome.latest.is_some() && settings.skipped.as_deref() == outcome.latest.as_deref());
    value["checked_at"] = json!(timestamp(now));
    value["release"] = outcome.release_json();
    Ok(value)
}

/// Plain `version` output.
pub fn human(value: &Value) -> String {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let check = &value["update_check"];
    let mut out = format!(
        "capturefab {} ({} {})\n",
        text(&value["version"]),
        text(&value["os"]),
        text(&value["arch"])
    );
    if value.get("checked_at").is_some() {
        let latest = &value["latest"];
        if latest.is_null() {
            out.push_str("No releases are published yet.\n");
        } else if value["update_available"] == true {
            out.push_str(&format!(
                "Capturefab {} is available{}.\n",
                text(latest),
                if value["skipped"] == true {
                    " (skipped in the desktop app)"
                } else {
                    ""
                }
            ));
            let release = &value["release"];
            for line in release["highlights"].as_array().into_iter().flatten() {
                out.push_str(&format!("  - {}\n", text(line)));
            }
            if let Some(download) = release["download"].as_object() {
                out.push_str(&format!(
                    "Download: {}\nSHA-256:  {}\n",
                    text(&download["url"]),
                    text(&download["sha256"])
                ));
            } else {
                out.push_str(&format!("Download: {DOWNLOADS_URL}\n"));
            }
            if let Some(url) = release["url"].as_str() {
                out.push_str(&format!("Release notes: {url}\n"));
            }
        } else if let Status::Newer(latest) = status(
            Some(&Outcome {
                latest: latest.as_str().map(str::to_owned),
                ..Outcome::default()
            }),
            &text(&value["version"]),
        ) {
            out.push_str(&format!(
                "This build ({}) is newer than the latest release, {latest}.\n",
                text(&value["version"])
            ));
        } else {
            out.push_str(&format!(
                "This is the latest version (latest release: {}).\n",
                text(latest)
            ));
        }
        return out;
    }
    if !check["policy"].is_null() {
        out.push_str(&format!(
            "Update checks: off on this computer ({}).\n",
            check["policy"]["path"].as_str().unwrap_or(CHECK_ENV)
        ));
    } else if check["compiled"] != true {
        out.push_str("Update checks: not in this build.\n");
    } else if check["automatic"] == true {
        out.push_str(
            "Update checks: once a day in the desktop app. Turn off in Settings › Updates.\n",
        );
    } else {
        out.push_str(
            "Update checks: off. Turn on in Settings › Updates, or run capturefab version --check.\n",
        );
    }
    match check["last_checked"].as_str() {
        None => out.push_str("Last check: never\n"),
        Some(at) => {
            let found = match (&check["latest"], &check["update_available"]) {
                (Value::String(latest), Value::Bool(true)) => format!("{latest} is available"),
                (Value::String(latest), _) => format!("latest release {latest}"),
                _ => "no release found".into(),
            };
            let failed = check["last_error"]
                .as_str()
                .map(|_| ", which failed")
                .unwrap_or_default();
            out.push_str(&format!("Last check: {at}{failed} ({found})\n"));
        }
    }
    out
}

/// Doctor's notes: an available version, and a meaningless
/// `CAPTUREFAB_UPDATE_CHECK`. Never fetches.
pub fn doctor_notes() -> Vec<(String, String)> {
    let mut notes = Vec::new();
    let settings = Settings::load(&crate::ipc::state_dir());
    if let Some(version) = settings.announce() {
        notes.push((
            format!("Capturefab {version} is available (this is {})", current()),
            format!("Download it from {DOWNLOADS_URL}"),
        ));
    }
    if let Some(value) = unrecognized_env() {
        notes.push((
            format!("{CHECK_ENV}={value} is not a recognized value and is ignored"),
            format!("Set {CHECK_ENV}=off to turn update checks off, or unset it"),
        ));
    }
    notes
}

/// For `capturefab schema`.
pub fn schema_contract() -> Value {
    json!({
        "manifest": UPDATE_URL,
        "automatic": "desktop app only: at most once a day, on unless turned off in Settings › Updates; never from the CLI or serve",
        "manual": "capturefab version --check",
        "request": "GET with Accept: application/json and User-Agent: capturefab; no cookies, query string, identifiers or version; redirects are not followed; at most 256 KiB",
        "state": "updates.json in the state directory: automatic, skipped, last_checked, last_outcome",
        "environment": {
            CHECK_ENV: "off (also 0, false, never) disables automatic and requested checks; other values are ignored",
            URL_ENV: "an https:// mirror of releases.json, or http:// on loopback",
        },
        "policy_file": {
            "paths": [
                "/Library/Application Support/Capturefab/policy.json (macOS)",
                "/etc/capturefab/policy.json (Linux and macOS)",
                "%ProgramData%\\Capturefab\\policy.json (Windows)",
            ],
            "content": {"update_check": "off"},
        },
        "exit_codes": {"2": "invalid CAPTUREFAB_UPDATE_URL", "3": "offline, unreachable or HTTP error", "4": "timeout", "1": "unreadable or redirected release list", "5": "turned off by policy or not in this build"},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-updates-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        crate::ipc::ensure_private_dir(&dir).unwrap();
        dir
    }

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text}"))
    }

    fn base() -> Url {
        Url::parse(UPDATE_URL).unwrap()
    }

    fn manifest(latest: &str, extra: Value) -> Vec<u8> {
        let mut release = json!({
            "version": latest,
            "date": "2026-11-20",
            "url": format!("https://github.com/capturefab/capturefab/releases/tag/v{latest}"),
            "checksums_url": format!("https://github.com/capturefab/capturefab/releases/download/v{latest}/SHA256SUMS"),
            "assets": [
                {"name": "a-macos.dmg", "os": "macos", "arch": "aarch64", "available": true,
                 "sha256": "ab".repeat(32), "size": 10,
                 "url": format!("https://github.com/capturefab/capturefab/releases/download/v{latest}/a-macos.dmg")},
                {"name": "a-linux.tar.gz", "os": "linux", "arch": "x86_64", "available": false,
                 "sha256": "cd".repeat(32), "size": 10, "url": "downloads/a-linux.tar.gz"},
                {"name": "a-linux-arm.tar.gz", "os": "linux", "arch": "armv7", "available": true,
                 "sha256": "ef".repeat(32), "size": 10, "url": "downloads/a-linux-arm.tar.gz"}
            ]
        });
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                release[key] = value;
            }
        }
        serde_json::to_vec(
            &json!({"schema_version": 1, "latest": latest, "releases": [release], "future": 1}),
        )
        .unwrap()
    }

    #[test]
    fn semver_orders_releases_and_prereleases() {
        let order = [
            "0.4.0",
            "0.4.1",
            "0.5.0-alpha",
            "0.5.0-alpha.1",
            "0.5.0-beta",
            "0.5.0-rc.1",
            "0.5.0-rc.2",
            "0.5.0-rc.10",
            "0.5.0",
            "1.0.0",
        ];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("0.5.0+build.7"), v("0.5.0"));
        for bad in [
            "v0.5.0",
            "01.0.0",
            "0.05.0",
            "0.5",
            "0.5.0-rc.01",
            "0.5.0-",
            "0.5.0+",
            "0.5.0.1",
            "",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn stable_builds_are_never_offered_a_prerelease() {
        // Synthetic: the published list only ever names the newest stable.
        assert!(!offer(&v("0.5.0-rc.1"), &v("0.4.0")));
        assert!(offer(&v("0.5.0-rc.2"), &v("0.5.0-rc.1")));
        assert!(offer(&v("0.5.0"), &v("0.5.0-rc.1")));
        assert!(offer(&v("0.5.0"), &v("0.4.0")));
        assert!(!offer(&v("0.4.0"), &v("0.4.0")));
        let outcome = |latest: &str| Outcome {
            latest: Some(latest.into()),
            ..Outcome::default()
        };
        assert_eq!(
            status(Some(&outcome("0.4.0")), "0.5.0"),
            Status::Newer("0.4.0".into())
        );
        assert_eq!(status(Some(&outcome("0.5.0")), "0.5.0"), Status::Current);
        assert_eq!(
            status(Some(&outcome("0.5.0")), "0.4.0"),
            Status::Available("0.5.0".into())
        );
        assert_eq!(status(None, "0.4.0"), Status::Unknown);
        assert_eq!(status(Some(&Outcome::default()), "0.4.0"), Status::None);
        let mut failed = outcome("0.4.0");
        failed.error = Some("timeout".into());
        assert_eq!(
            status(Some(&failed), "0.4.0"),
            Status::Failed(UpdateError::Timeout)
        );
        // A failure keeps an earlier find on offer.
        failed.latest = Some("0.5.0".into());
        assert_eq!(
            status(Some(&failed), "0.4.0"),
            Status::Available("0.5.0".into())
        );
    }

    #[test]
    fn the_release_list_is_read_strictly_and_its_links_allowlisted() {
        let outcome = select(
            &manifest("0.5.0", json!({})),
            &base(),
            None,
            "macos",
            "aarch64",
        )
        .unwrap();
        assert_eq!(outcome.latest.as_deref(), Some("0.5.0"));
        assert_eq!(outcome.date.as_deref(), Some("2026-11-20"));
        let download = outcome.download.unwrap();
        assert_eq!(download.name, "a-macos.dmg");
        assert_eq!(download.platform, "macOS ARM64");
        // An unavailable asset is no download.
        let linux = select(
            &manifest("0.5.0", json!({})),
            &base(),
            None,
            "linux",
            "x86_64",
        )
        .unwrap();
        assert!(linux.download.is_none());
        // Relative paths resolve against the list itself.
        let arm = select(&manifest("0.5.0", json!({})), &base(), None, "linux", "arm").unwrap();
        assert_eq!(
            arm.download.unwrap().url,
            "https://capturefab.com/downloads/a-linux-arm.tar.gz"
        );
        // Links elsewhere are dropped.
        for evil in [
            "//evil.example/x",
            "https://evil.example/x",
            "http://capturefab.com/x",
            "javascript:alert(1)",
            "https://github.com/capturefab/capturefab/releases/../../other",
            "https://capturefab.com.evil.example/x",
        ] {
            let outcome = select(
                &manifest("0.5.0", json!({"url": evil})),
                &base(),
                None,
                "macos",
                "aarch64",
            )
            .unwrap();
            assert_eq!(outcome.url, None, "{evil}");
        }
        // A mirror's own links are trusted.
        let mirror = Url::parse("http://127.0.0.1:8080/r/releases.json").unwrap();
        let outcome = select(
            &manifest("0.5.0", json!({"url": "notes.html"})),
            &mirror,
            Some(mirror.origin()),
            "macos",
            "aarch64",
        )
        .unwrap();
        assert_eq!(
            outcome.url.as_deref(),
            Some("http://127.0.0.1:8080/r/notes.html")
        );
        // Schema and consistency.
        let null = br#"{"schema_version":1,"latest":null,"releases":[]}"#;
        assert_eq!(
            select(null, &base(), None, "macos", "aarch64")
                .unwrap()
                .latest,
            None
        );
        let missing = br#"{"schema_version":1,"latest":"0.5.0","releases":[]}"#;
        assert_eq!(
            select(missing, &base(), None, "macos", "aarch64"),
            Err(UpdateError::Malformed)
        );
        let two = br#"{"schema_version":2,"latest":null,"releases":[]}"#;
        assert_eq!(
            select(two, &base(), None, "macos", "aarch64"),
            Err(UpdateError::UnsupportedSchema)
        );
        assert_eq!(
            select(b"<html>", &base(), None, "macos", "aarch64"),
            Err(UpdateError::Malformed)
        );
    }

    #[test]
    fn highlights_are_plain_bounded_text() {
        let long = "x".repeat(500);
        let outcome = select(
            &manifest(
                "0.5.0",
                json!({"highlights": ["  Faster\u{202E}  previews\n", "\u{0007}bell", long, 7, "4", "5", "6", "7", "8"]}),
            ),
            &base(),
            None,
            "macos",
            "aarch64",
        )
        .unwrap();
        assert_eq!(outcome.highlights.len(), 6);
        assert_eq!(outcome.highlights[0], "Faster previews");
        assert_eq!(outcome.highlights[1], "bell");
        assert_eq!(outcome.highlights[2].chars().count(), 200);
        assert_eq!(manifest_arch("arm"), "armv7");
        assert_eq!(platform_label("macos", "aarch64"), "macOS ARM64");
        assert_eq!(platform_label("windows", "x86_64"), "Windows x64");
    }

    #[test]
    fn override_urls_must_be_https_or_loopback() {
        assert_eq!(manifest_url_from(None).unwrap().as_str(), UPDATE_URL);
        for good in [
            "https://mirror.example/releases.json",
            "http://127.0.0.1:9/releases.json",
            "http://localhost:8080/r.json",
            "http://[::1]:80/r.json",
        ] {
            assert!(manifest_url_from(Some(good)).is_ok(), "{good}");
        }
        for bad in [
            "ftp://x",
            "http://mirror.example/releases.json",
            "https://user@mirror.example/r.json",
            "https://mirror.example/r.json?id=1",
            "file:///etc/passwd",
            "http://127.0.0.1.evil.example/r.json",
        ] {
            assert_eq!(
                manifest_url_from(Some(bad)),
                Err(UpdateError::BadUrl),
                "{bad}"
            );
        }
        assert!(link_allowed("https://capturefab.com/downloads.html", None));
        assert!(!link_allowed("https://capturefab.com", None));
        assert!(link_allowed(
            "http://127.0.0.1:9/x",
            Some("http://127.0.0.1:9")
        ));
        assert!(!link_allowed(
            "http://127.0.0.1:99/x",
            Some("http://127.0.0.1:9")
        ));
    }

    #[test]
    fn checks_are_due_daily_and_failures_count() {
        let now = Utc::now();
        let mut settings = Settings::default();
        assert!(due(&settings, now));
        settings.last_checked = Some(timestamp(now - chrono::Duration::hours(23)));
        assert!(!due(&settings, now));
        settings.last_checked = Some(timestamp(now - chrono::Duration::hours(25)));
        assert!(due(&settings, now));
        settings.last_checked = Some(timestamp(now + chrono::Duration::hours(2)));
        assert!(!due(&settings, now));
        settings.last_checked = Some(timestamp(now + chrono::Duration::hours(49)));
        assert!(due(&settings, now), "a clock moved back is stale");
        settings.last_checked = None;
        settings.automatic = Some(false);
        assert!(!due(&settings, now));
        settings.automatic = None;
        assert!(settings.automatic(), "on unless turned off");
    }

    struct Canned(Result<Vec<u8>, UpdateError>);
    impl Fetcher for Canned {
        fn get(&self, _: &Url) -> Result<Vec<u8>, UpdateError> {
            self.0.clone()
        }
    }

    #[test]
    fn settings_round_trip_privately_and_reject_junk() {
        let dir = dir("roundtrip");
        assert_eq!(Settings::load(&dir), Settings::default());
        let saved = Settings::update(&dir, |s| s.skipped = Some("0.5.0".into())).unwrap();
        assert_eq!(Settings::load(&dir), saved);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(STATE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::write(dir.join(STATE_FILE), b"{not json").unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        fs::write(dir.join(STATE_FILE), vec![b' '; MAX_STATE as usize + 10]).unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        fs::write(
            dir.join(STATE_FILE),
            br#"{"schema_version":1,"automatic":false,"unknown":1}"#,
        )
        .unwrap();
        assert_eq!(Settings::load(&dir).automatic, Some(false));
    }

    #[test]
    fn concurrent_updates_and_checks_keep_each_others_fields() {
        let dir = dir("concurrent");
        Settings::update(&dir, |s| s.automatic = Some(false)).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let (dir, barrier) = (dir.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    if i == 0 {
                        Settings::update(&dir, |s| s.skipped = Some("9.9.0".into())).unwrap();
                    } else {
                        check_with(
                            &Canned(Ok(manifest("9.9.0", json!({})))),
                            &dir,
                            Ok(base()),
                            &Policy::Allowed,
                            Utc::now(),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let settings = Settings::load(&dir);
        assert_eq!(settings.skipped.as_deref(), Some("9.9.0"));
        assert_eq!(
            settings.automatic,
            Some(false),
            "a check never changes automatic"
        );
        assert_eq!(
            settings.last_outcome.unwrap().latest.as_deref(),
            Some("9.9.0")
        );
        // A failure is a check, and keeps what was found before.
        let failed = check_with(
            &Canned(Err(UpdateError::Timeout)),
            &dir,
            Ok(base()),
            &Policy::Allowed,
            Utc::now(),
        );
        assert_eq!(failed.unwrap_err(), UpdateError::Timeout);
        let settings = Settings::load(&dir);
        assert_eq!(settings.status(), Status::Available("9.9.0".into()));
        assert_eq!(settings.announce(), None, "skipped");
        assert_eq!(
            settings.last_outcome.unwrap().failure(),
            Some(UpdateError::Timeout)
        );
        // A policy refuses before fetching.
        assert_eq!(
            check_with(
                &Canned(Ok(Vec::new())),
                &dir,
                Ok(base()),
                &Policy::Off(PolicySource::Env),
                Utc::now()
            )
            .unwrap_err(),
            UpdateError::PolicyOff(PolicySource::Env)
        );
    }

    #[test]
    fn automatic_checks_skip_while_another_process_holds_the_lock() {
        let dir = dir("automatic");
        let lock = open_lock(&dir).unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let fetcher = Canned(Ok(manifest("9.9.0", json!({}))));
        // fs2 locks are per open file, so this thread's second handle is refused too.
        assert!(automatic_with(&fetcher, &dir, Utc::now()).is_none());
        fs2::FileExt::unlock(&lock).unwrap();
        if policy().allowed() && manifest_url().is_ok() {
            let (result, settings) = automatic_with(&fetcher, &dir, Utc::now()).unwrap();
            assert!(result.is_ok());
            assert!(settings.last_checked.is_some());
            assert!(
                automatic_with(&fetcher, &dir, Utc::now()).is_none(),
                "not due again"
            );
        }
    }

    #[test]
    fn policy_turns_checks_off_from_the_environment_or_a_file() {
        for off in ["off", "OFF", "0", "false", "never", " Never "] {
            assert_eq!(
                policy_from(Some(off), &[]),
                Policy::Off(PolicySource::Env),
                "{off}"
            );
        }
        assert_eq!(policy_from(Some("yes"), &[]), Policy::Allowed);
        assert_eq!(unrecognized(Some("yes")).as_deref(), Some("yes"));
        assert_eq!(unrecognized(Some("off")), None);
        assert_eq!(unrecognized(None), None);
        let dir = dir("policy");
        let file = dir.join("policy.json");
        fs::write(&file, br#"{"update_check":"off"}"#).unwrap();
        let missing = dir.join("missing.json");
        assert_eq!(
            policy_from(None, &[missing.clone(), file.clone()]),
            Policy::Off(PolicySource::File(file.clone()))
        );
        fs::write(&file, br#"{"update_check":"on"}"#).unwrap();
        assert_eq!(
            policy_from(None, std::slice::from_ref(&file)),
            Policy::Allowed
        );
        fs::write(
            &file,
            format!(r#"{{"update_check":"off","pad":"{}"}}"#, "x".repeat(5000)),
        )
        .unwrap();
        assert_eq!(
            policy_from(None, &[file]),
            Policy::Allowed,
            "oversized files are ignored"
        );
    }

    #[test]
    fn every_error_has_its_exit_code_whatever_its_text() {
        for (error, expected) in [
            (UpdateError::BadUrl, ("usage", 2)),
            (UpdateError::Offline, ("unavailable", 3)),
            (UpdateError::Unreachable, ("unavailable", 3)),
            (UpdateError::Status(404), ("unavailable", 3)),
            (UpdateError::Status(503), ("unavailable", 3)),
            (UpdateError::Timeout, ("timeout", 4)),
            (UpdateError::Malformed, ("operation_failed", 1)),
            (UpdateError::UnsupportedSchema, ("operation_failed", 1)),
            (UpdateError::TooLarge, ("operation_failed", 1)),
            (UpdateError::Redirected, ("operation_failed", 1)),
            (
                UpdateError::PolicyOff(PolicySource::Env),
                ("unsupported", 5),
            ),
            (UpdateError::NotCompiled, ("unsupported", 5)),
        ] {
            let text = error.to_string().to_ascii_lowercase();
            assert!(
                !text.contains("not found") && !text.contains("disabled"),
                "{text}"
            );
            assert_eq!(
                crate::cli::error_code(&error.clone().into_anyhow()),
                expected,
                "{error:?}"
            );
        }
    }

    #[test]
    fn version_check_text_tells_a_newer_build_from_the_latest() {
        let checked = |version: &str, latest: &str| {
            human(&json!({
                "version": version, "os": "macos", "arch": "aarch64",
                "checked_at": "2026-10-25T09:14:00Z", "latest": latest,
                "update_available": false, "release": {},
            }))
        };
        assert!(
            checked("0.5.0", "0.4.0")
                .contains("This build (0.5.0) is newer than the latest release, 0.4.0.")
        );
        assert!(checked("0.4.0", "0.4.0").contains("This is the latest version"));
    }

    #[test]
    fn network_errors_never_name_one_server() {
        for error in [
            UpdateError::Unreachable,
            UpdateError::Timeout,
            UpdateError::Status(502),
        ] {
            assert!(!error.to_string().contains("capturefab.com"), "{error}");
        }
    }

    #[cfg(feature = "updates")]
    mod native {
        use super::*;
        use std::{
            io::{BufRead, BufReader},
            net::TcpListener,
            time::Instant,
        };

        /// A one-connection server answering with `response` after `delay`.
        fn serve(response: Vec<u8>, delay: Duration) -> (Url, std::thread::JoinHandle<String>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                std::thread::sleep(delay);
                let _ = stream.write_all(&response);
                request
            });
            (
                Url::parse(&format!("http://127.0.0.1:{port}/releases.json")).unwrap(),
                handle,
            )
        }

        fn fetcher() -> NativeFetcher {
            NativeFetcher::with_timeouts(Duration::from_millis(200), Duration::from_millis(500))
        }

        fn reply(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
            let mut out = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            out.extend_from_slice(body);
            out
        }

        #[test]
        fn the_request_is_bare_and_answers_map_to_errors() {
            let (url, server) = serve(reply("200 OK", "", b"{}"), Duration::ZERO);
            assert_eq!(fetcher().get(&url).unwrap(), b"{}");
            let request = server.join().unwrap().to_ascii_lowercase();
            assert!(
                request.starts_with("get /releases.json http/1.1\r\n"),
                "{request}"
            );
            assert!(request.contains("user-agent: capturefab\r\n"), "{request}");
            assert!(
                request.contains("accept: application/json\r\n"),
                "{request}"
            );
            assert!(!request.contains("cookie") && !request.contains("authorization"));

            let (url, _) = serve(
                reply("302 Found", "Location: http://127.0.0.1:1/\r\n", b""),
                Duration::ZERO,
            );
            assert_eq!(fetcher().get(&url), Err(UpdateError::Redirected));
            let (url, _) = serve(reply("503 Service Unavailable", "", b""), Duration::ZERO);
            assert_eq!(fetcher().get(&url), Err(UpdateError::Status(503)));
            let (url, _) = serve(reply("200 OK", "", &vec![b' '; 300 * 1024]), Duration::ZERO);
            assert_eq!(fetcher().get(&url), Err(UpdateError::TooLarge));
            let (url, _) = serve(reply("200 OK", "", b"{}"), Duration::from_secs(2));
            let started = Instant::now();
            assert_eq!(fetcher().get(&url), Err(UpdateError::Timeout));
            assert!(started.elapsed() < Duration::from_secs(1));
            let closed = Url::parse("http://127.0.0.1:9/releases.json").unwrap();
            // Windows retries a refused connect for longer than this client's
            // short connect timeout, so it may report a timeout instead.
            let answer = fetcher().get(&closed);
            if cfg!(windows) {
                assert!(
                    matches!(answer, Err(UpdateError::Unreachable | UpdateError::Timeout)),
                    "{answer:?}"
                );
            } else {
                assert_eq!(answer, Err(UpdateError::Unreachable));
            }
        }
    }
}
