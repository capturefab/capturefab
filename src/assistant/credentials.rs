//! Credential resolution for the assistant. Secrets are deliberately absent
//! from status reports and are resolved only on a request worker.
use super::{
    google, http,
    provider::{AuthScheme, ProviderId, ProviderSettings},
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "capturefab-ai";
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Authentication is retained for one run, never formatted or serialized.
pub enum Auth {
    None,
    Header {
        name: &'static str,
        /// The complete header value, including a Bearer prefix when needed.
        value: Zeroizing<String>,
        quota_project: Option<String>,
    },
    Aws {
        keys: crate::aws::Keys,
        region: String,
    },
}

impl Auth {
    /// Vertex express keys use a different URL from Google Cloud credentials.
    pub fn vertex_express(&self) -> bool {
        matches!(
            self,
            Self::Header {
                name: "x-goog-api-key",
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CredentialStatus {
    pub ready: bool,
    pub source: String,
    pub missing: Vec<String>,
}

/// Effective non-secret settings travel with authentication so an AWS profile
/// region or environment-derived project cannot disagree with the request URL.
pub struct ResolvedCredentials {
    pub auth: Auth,
    pub settings: ProviderSettings,
}

/// Every source with side effects has a test seam. Stub implementations never
/// fall through to the process environment, real files, keychain or network.
pub(super) trait Source {
    fn environment(&self, name: &str) -> Result<Option<String>>;
    fn keychain(&self, account: &str) -> Result<Option<String>>;
    fn save_key(&self, _account: &str, _key: &str) -> Result<()> {
        bail!("credential store is unavailable")
    }
    fn delete_key(&self, _account: &str) -> Result<()> {
        bail!("credential store is unavailable")
    }
    fn saved_hint(&self, _id: ProviderId, _saved: bool) -> Result<()> {
        Ok(())
    }
    /// Only regular, non-symlink, bounded files may be returned.
    fn file(&self, path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>>;
    fn gcloud(&self) -> Result<Option<PathBuf>>;
    fn run_gcloud(&self, path: &Path) -> Result<Zeroizing<String>>;
    fn run_gcloud_bounded(
        &self,
        path: &Path,
        _timeout: Duration,
        _cancel: &AtomicBool,
    ) -> Result<Zeroizing<String>> {
        self.run_gcloud(path)
    }

    fn metadata_allowed(&self) -> bool;
    fn send_json(&self, req: http::HttpRequest<'_>) -> Result<(Value, http::Headers)>;
    fn send_json_bounded(
        &self,
        mut req: http::HttpRequest<'_>,
        timeout: Duration,
        _cancel: &AtomicBool,
    ) -> Result<(Value, http::Headers)> {
        req.timeout = Some(req.timeout.unwrap_or(timeout).min(timeout));
        self.send_json(req)
    }

    fn aws_chain(&self, profile: Option<&str>) -> Result<(crate::aws::Keys, String)>;
    fn aws_region(&self, profile: &str) -> Option<String>;
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
    fn cache_tokens(&self) -> bool {
        false
    }
}

pub(super) struct NativeSource;

impl Source for NativeSource {
    fn environment(&self, name: &str) -> Result<Option<String>> {
        match std::env::var(name) {
            Ok(value) => Ok((!value.is_empty()).then_some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => bail!("{name} contains invalid text"),
        }
    }

    fn keychain(&self, account: &str) -> Result<Option<String>> {
        match native_entry(account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => bail!("could not read the OS credential store"),
        }
    }

    fn save_key(&self, account: &str, key: &str) -> Result<()> {
        native_entry(account)?
            .set_password(key)
            .map_err(|_| anyhow!("could not save the API key in the OS credential store"))
    }

    fn delete_key(&self, account: &str) -> Result<()> {
        match native_entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => bail!("could not remove the API key from the OS credential store"),
        }
    }

    fn saved_hint(&self, id: ProviderId, saved: bool) -> Result<()> {
        super::settings::set_saved_key(id, saved)
    }

    fn file(&self, path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
        bounded_regular_file(path)
    }

    fn gcloud(&self) -> Result<Option<PathBuf>> {
        google::find_gcloud(self)
    }

    fn run_gcloud(&self, path: &Path) -> Result<Zeroizing<String>> {
        google::run_gcloud(path)
    }
    fn run_gcloud_bounded(
        &self,
        path: &Path,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Zeroizing<String>> {
        google::run_gcloud_bounded(path, timeout, cancel)
    }

    fn metadata_allowed(&self) -> bool {
        google::native_metadata_allowed(self)
    }

    fn send_json(&self, req: http::HttpRequest<'_>) -> Result<(Value, http::Headers)> {
        http::Http::new()
            .send_json(req, &AtomicBool::new(false))
            .map_err(Into::into)
    }

    fn send_json_bounded(
        &self,
        mut req: http::HttpRequest<'_>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<(Value, http::Headers)> {
        req.timeout = Some(req.timeout.unwrap_or(timeout).min(timeout));
        http::Http::new().send_json(req, cancel).map_err(Into::into)
    }
    fn aws_chain(&self, profile: Option<&str>) -> Result<(crate::aws::Keys, String)> {
        crate::aws::bedrock_chain(profile)
    }

    fn aws_region(&self, profile: &str) -> Option<String> {
        crate::aws::profile_region(profile)
    }

    fn cache_tokens(&self) -> bool {
        true
    }
}

/// One startup budget spans every credential lookup and token refresh. OS
/// keychain/file calls may finish late; the runtime discards their result and
/// this wrapper prevents subsequent network or subprocess work after expiry.
struct BoundedSource<'a> {
    inner: &'a dyn Source,
    deadline: Instant,
    cancel: &'a AtomicBool,
}
impl BoundedSource<'_> {
    fn remaining(&self) -> Result<Duration> {
        ensure!(
            !self.cancel.load(Ordering::SeqCst),
            "The assistant task was stopped."
        );
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(http::ProviderError::new(http::ProviderErrorKind::Timeout).into());
        }
        Ok(remaining)
    }
    fn checked<T>(&self, run: impl FnOnce() -> Result<T>) -> Result<T> {
        self.remaining()?;
        let result = run();
        self.remaining()?;
        result
    }
}
impl Source for BoundedSource<'_> {
    fn environment(&self, name: &str) -> Result<Option<String>> {
        self.checked(|| self.inner.environment(name))
    }
    fn keychain(&self, account: &str) -> Result<Option<String>> {
        self.checked(|| self.inner.keychain(account))
    }
    fn file(&self, path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
        self.checked(|| self.inner.file(path))
    }
    fn gcloud(&self) -> Result<Option<PathBuf>> {
        self.checked(|| self.inner.gcloud())
    }
    fn run_gcloud(&self, path: &Path) -> Result<Zeroizing<String>> {
        let remaining = self.remaining()?;
        self.checked(|| self.inner.run_gcloud_bounded(path, remaining, self.cancel))
    }
    fn metadata_allowed(&self) -> bool {
        self.remaining().is_ok() && self.inner.metadata_allowed()
    }
    fn send_json(&self, req: http::HttpRequest<'_>) -> Result<(Value, http::Headers)> {
        let remaining = self.remaining()?;
        self.checked(|| self.inner.send_json_bounded(req, remaining, self.cancel))
    }
    fn aws_chain(&self, profile: Option<&str>) -> Result<(crate::aws::Keys, String)> {
        self.checked(|| self.inner.aws_chain(profile))
    }
    fn aws_region(&self, profile: &str) -> Option<String> {
        self.remaining().ok()?;
        let region = self.inner.aws_region(profile);
        self.remaining().ok()?;
        region
    }
    fn now(&self) -> u64 {
        self.inner.now()
    }
    fn cache_tokens(&self) -> bool {
        self.inner.cache_tokens()
    }
}

fn native_entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, account)
        .map_err(|_| anyhow!("could not open the OS credential store"))
}

fn bounded_regular_file(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("Google ADC file could not be read"),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Google ADC file must be a regular, non-symlink file"
    );
    ensure!(
        metadata.len() <= MAX_FILE_BYTES as u64,
        "Google ADC file exceeds 64 KiB"
    );
    let file = File::open(path).map_err(|_| anyhow!("Google ADC file could not be read"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow!("Google ADC file could not be read"))?;
    ensure!(
        opened.is_file() && opened.len() <= MAX_FILE_BYTES as u64,
        "Google ADC file must be a regular file no larger than 64 KiB"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.dev() == opened.dev() && metadata.ino() == opened.ino(),
            "Google ADC file changed while being opened"
        );
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("Google ADC file could not be read"))?;
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "Google ADC file exceeds 64 KiB"
    );
    Ok(Some(bytes))
}

fn valid_key(value: String, source: &str) -> Result<Zeroizing<String>> {
    let value = Zeroizing::new(value);
    ensure!(
        !value.is_empty()
            && value.len() <= 4096
            && value.bytes().all(|byte| byte.is_ascii_graphic()),
        "{source} must contain 1..4096 printable ASCII characters without whitespace"
    );
    Ok(value)
}

fn env_key(source: &dyn Source, names: &[&str]) -> Result<Option<Zeroizing<String>>> {
    Ok(env_key_with_source(source, names)?.map(|(key, _)| key))
}

fn env_key_with_source(
    source: &dyn Source,
    names: &[&str],
) -> Result<Option<(Zeroizing<String>, String)>> {
    for name in names {
        if let Some(value) = source.environment(name)?.filter(|value| !value.is_empty()) {
            return valid_key(value, name).map(|key| Some((key, (*name).to_owned())));
        }
    }
    Ok(None)
}

fn saved_key(source: &dyn Source, id: ProviderId) -> Result<Option<Zeroizing<String>>> {
    source
        .keychain(id.info().keychain_account)?
        .map(|key| valid_key(key, "saved API key"))
        .transpose()
}

fn header(name: &'static str, key: Zeroizing<String>, bearer: bool) -> Auth {
    Auth::Header {
        name,
        value: if bearer {
            Zeroizing::new(format!("Bearer {}", key.as_str()))
        } else {
            key
        },
        quota_project: None,
    }
}

fn effective(
    source: &dyn Source,
    id: ProviderId,
    settings: &ProviderSettings,
) -> Result<ProviderSettings> {
    let names: &[&str] = match id {
        ProviderId::Azure => &["AZURE_OPENAI_ENDPOINT"],
        ProviderId::Cloudflare => &["CLOUDFLARE_ACCOUNT_ID"],
        ProviderId::Bedrock => &["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_PROFILE"],
        ProviderId::Vertex => &["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION"],
        ProviderId::Ollama => &["OLLAMA_HOST"],
        _ => &[],
    };
    let mut vars = std::collections::BTreeMap::new();
    for name in names {
        if let Some(value) = source.environment(name)?.filter(|value| !value.is_empty()) {
            vars.insert((*name).to_owned(), value);
        }
    }
    let mut settings = settings.clone();
    if id == ProviderId::Bedrock
        && settings.region.is_none()
        && !vars.contains_key("AWS_REGION")
        && !vars.contains_key("AWS_DEFAULT_REGION")
    {
        let profile = settings
            .profile
            .as_deref()
            .or_else(|| vars.get("AWS_PROFILE").map(String::as_str))
            .unwrap_or("default");
        settings.region = source.aws_region(profile);
    }
    settings
        .effective(id, |name| vars.get(name).cloned())
        .map_err(|errors| {
            anyhow!(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
}

/// This only inspects local configuration. It never executes gcloud or probes
/// metadata, and reads the selected keychain account only when requested.
pub fn status(
    id: ProviderId,
    settings: &ProviderSettings,
    check_keychain: bool,
) -> CredentialStatus {
    status_in(&NativeSource, id, settings, check_keychain)
}

fn status_in(
    source: &dyn Source,
    id: ProviderId,
    settings: &ProviderSettings,
    check_keychain: bool,
) -> CredentialStatus {
    let result = (|| -> Result<String> {
        let effective = effective(source, id, settings)?;
        if id == ProviderId::Vertex {
            if let Some(description) = google::status(source)? {
                check_endpoint(id, &effective, false)?;
                return Ok(description);
            }
            ensure!(
                !effective.model(id).starts_with("claude-"),
                "Claude on Vertex AI needs Google Cloud credentials (ADC). API keys work only for Gemini models."
            );
        }
        check_endpoint(id, &effective, id == ProviderId::Vertex)?;
        if id == ProviderId::Ollama && optional_key(id, &effective)? {
            return Ok("No API key required".into());
        }
        let names: &[&str] = if id == ProviderId::Vertex {
            &["GOOGLE_CLOUD_API_KEY"]
        } else if id == ProviderId::Bedrock {
            &["AWS_BEARER_TOKEN_BEDROCK"]
        } else {
            id.info().key_envs
        };
        if let Some((_, name)) = env_key_with_source(source, names)? {
            return Ok(name);
        }
        if id == ProviderId::Bedrock && aws_preferred(source, settings)? {
            validate_aws_environment(source, settings.profile.is_some())?;
            return source
                .aws_chain(settings.profile.as_deref())
                .map(|(_, description)| description);
        }
        if check_keychain {
            if saved_key(source, id)?.is_some() {
                return Ok("OS credential store".into());
            }
        } else if effective.saved_key {
            return Ok("Saved API key (not checked)".into());
        }
        if id == ProviderId::Bedrock {
            return source
                .aws_chain(settings.profile.as_deref())
                .map(|(_, description)| description);
        }
        if optional_key(id, &effective)? {
            return Ok("No API key required".into());
        }
        bail!("No credentials configured for {}", id.info().name)
    })();
    match result {
        Ok(description) => CredentialStatus {
            ready: true,
            source: description,
            missing: Vec::new(),
        },
        Err(error) => CredentialStatus {
            ready: false,
            source: "Not configured".into(),
            missing: vec![error.to_string()],
        },
    }
}

fn optional_key(id: ProviderId, settings: &ProviderSettings) -> Result<bool> {
    match id {
        ProviderId::Custom | ProviderId::LmStudio => Ok(true),
        ProviderId::Ollama => {
            let base = settings
                .base_url
                .as_deref()
                .unwrap_or("http://localhost:11434/v1");
            let url: ureq::http::Uri = base.parse().map_err(|_| anyhow!("Base URL is invalid"))?;
            Ok(url
                .host()
                .is_none_or(|host| !host.eq_ignore_ascii_case("ollama.com")))
        }
        _ => Ok(false),
    }
}

fn check_endpoint(id: ProviderId, settings: &ProviderSettings, express: bool) -> Result<()> {
    let endpoints =
        super::provider::endpoints(id, settings, settings.model(id)).map_err(|errors| {
            anyhow!(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })?;
    endpoints.select(express).map_err(Into::into).map(|_| ())
}

fn aws_preferred(source: &dyn Source, settings: &ProviderSettings) -> Result<bool> {
    if settings.profile.is_some() {
        return Ok(true);
    }
    for name in [
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ] {
        if source
            .environment(name)?
            .map(Zeroizing::new)
            .is_some_and(|value| !value.is_empty())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_aws_environment(source: &dyn Source, explicit_profile: bool) -> Result<()> {
    if explicit_profile {
        return Ok(());
    }
    let id = source
        .environment("AWS_ACCESS_KEY_ID")?
        .filter(|value| !value.is_empty());
    let secret = source
        .environment("AWS_SECRET_ACCESS_KEY")?
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    let token = source
        .environment("AWS_SESSION_TOKEN")?
        .filter(|value| !value.is_empty())
        .map(Zeroizing::new);
    if id.is_some() || secret.is_some() || token.is_some() {
        ensure!(
            id.is_some() && secret.is_some(),
            "Set both AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or unset AWS credential variables to use a profile"
        );
        let id = id.unwrap();
        ensure!(
            id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_graphic()),
            "AWS_ACCESS_KEY_ID is invalid"
        );
        let secret = secret.unwrap();
        ensure!(
            secret.len() <= 4096,
            "AWS_SECRET_ACCESS_KEY exceeds 4096 bytes"
        );
        if let Some(token) = token {
            crate::aws::validate_session_token(&token)?;
        }
    }
    Ok(())
}

pub fn resolve_with_settings(
    id: ProviderId,
    settings: &ProviderSettings,
) -> Result<ResolvedCredentials> {
    resolve_in(&NativeSource, id, settings)
}

pub fn resolve_with_settings_bounded(
    id: ProviderId,
    settings: &ProviderSettings,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<ResolvedCredentials> {
    let source = BoundedSource {
        inner: &NativeSource,
        deadline: Instant::now() + timeout,
        cancel,
    };
    source.checked(|| resolve_in(&source, id, settings))
}

/// An established ADC session must keep using Google Cloud credentials. A
/// later missing ADC file must never change it to an express or saved API key.
pub(super) fn refresh_google_bounded(timeout: Duration, cancel: &AtomicBool) -> Result<Auth> {
    let source = BoundedSource {
        inner: &NativeSource,
        deadline: Instant::now() + timeout,
        cancel,
    };
    source.checked(|| refresh_google_in(&source))
}

fn refresh_google_in(source: &dyn Source) -> Result<Auth> {
    let token = google::resolve(source)?.ok_or_else(|| {
        anyhow!(
            "Google Cloud credentials are no longer available. Sign in again before continuing."
        )
    })?;
    Ok(Auth::Header {
        name: "authorization",
        value: Zeroizing::new(format!("Bearer {}", token.value.as_str())),
        quota_project: token.quota_project,
    })
}

fn resolve_in(
    source: &dyn Source,
    id: ProviderId,
    settings: &ProviderSettings,
) -> Result<ResolvedCredentials> {
    let explicit_profile = settings.profile.as_deref();
    let settings = effective(source, id, settings)?;
    if id == ProviderId::Vertex {
        if google::status(source)?.is_some() {
            check_endpoint(id, &settings, false)?;
        }
        if let Some(token) = google::resolve(source)? {
            return Ok(ResolvedCredentials {
                auth: Auth::Header {
                    name: "authorization",
                    value: Zeroizing::new(format!("Bearer {}", token.value.as_str())),
                    quota_project: token.quota_project,
                },
                settings,
            });
        }
        ensure!(
            !settings.model(id).starts_with("claude-"),
            "Claude on Vertex AI needs Google Cloud credentials (ADC). API keys work only for Gemini models."
        );
        check_endpoint(id, &settings, true)?;
        let key = match env_key(source, &["GOOGLE_CLOUD_API_KEY"])? {
            Some(key) => key,
            None => saved_key(source, id)?.ok_or_else(|| {
                anyhow!("No Google Cloud credentials or Vertex express API key are configured")
            })?,
        };
        return Ok(ResolvedCredentials {
            auth: header("x-goog-api-key", key, false),
            settings,
        });
    }
    check_endpoint(id, &settings, false)?;
    if id == ProviderId::Bedrock {
        let region = settings
            .region
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Region is required for Amazon Bedrock"))?;
        if let Some(key) = env_key(source, &["AWS_BEARER_TOKEN_BEDROCK"])? {
            return Ok(ResolvedCredentials {
                auth: header("authorization", key, true),
                settings,
            });
        }
        let prefer_aws = aws_preferred(source, &settings)?;
        if !prefer_aws && let Some(key) = saved_key(source, id)? {
            return Ok(ResolvedCredentials {
                auth: header("authorization", key, true),
                settings,
            });
        }
        validate_aws_environment(source, explicit_profile.is_some())?;
        let (keys, _) = source.aws_chain(explicit_profile)?;
        return Ok(ResolvedCredentials {
            auth: Auth::Aws { keys, region },
            settings,
        });
    }
    if id == ProviderId::Ollama && optional_key(id, &settings)? {
        return Ok(ResolvedCredentials {
            auth: Auth::None,
            settings,
        });
    }
    let key = match env_key(source, id.info().key_envs)? {
        Some(key) => Some(key),
        None => saved_key(source, id)?,
    };
    let auth = match key {
        Some(key) => match id.info().auth {
            AuthScheme::ApiKey => header(
                if id == ProviderId::Anthropic {
                    "x-api-key"
                } else {
                    "api-key"
                },
                key,
                false,
            ),
            AuthScheme::GoogleApiKey => header("x-goog-api-key", key, false),
            _ => header("authorization", key, true),
        },
        None if optional_key(id, &settings)? => Auth::None,
        None => bail!("No credentials configured for {}", id.info().name),
    };
    Ok(ResolvedCredentials { auth, settings })
}

pub fn store_key(id: ProviderId, key: String) -> Result<()> {
    store_key_in(&NativeSource, id, key)
}

fn store_key_in(source: &dyn Source, id: ProviderId, key: String) -> Result<()> {
    let key = valid_key(key, "API key")?;
    source.save_key(id.info().keychain_account, &key)?;
    source.saved_hint(id, true)
}

pub fn delete_key(id: ProviderId) -> Result<()> {
    delete_key_in(&NativeSource, id)
}

fn delete_key_in(source: &dyn Source, id: ProviderId) -> Result<()> {
    source.delete_key(id.info().keychain_account)?;
    source.saved_hint(id, false)
}

#[cfg(test)]
pub(super) mod test_source {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::{BTreeMap, BTreeSet, VecDeque},
        time::Duration,
    };

    pub struct Request {
        pub url: String,
        pub body: String,
        pub headers: Vec<(String, String)>,
        pub direct: bool,
        pub timeout: Option<Duration>,
    }

    #[derive(Default)]
    pub struct Stub {
        pub env: BTreeMap<String, String>,
        pub invalid_env: BTreeSet<String>,
        pub key: Option<String>,
        pub files: BTreeMap<PathBuf, Vec<u8>>,
        pub gcloud_path: Option<PathBuf>,
        pub command_output: Option<String>,
        pub metadata: bool,
        pub responses: RefCell<VecDeque<(Value, http::Headers)>>,
        pub requests: RefCell<Vec<Request>>,
        pub calls: RefCell<Vec<String>>,
        pub clock: Cell<u64>,
        pub cache: bool,
        pub aws: bool,
        pub region: Option<String>,
        pub keychain_error: bool,
        pub write_error: bool,
    }

    impl Stub {
        pub fn vars(mut self, values: &[(&str, &str)]) -> Self {
            self.env.extend(
                values
                    .iter()
                    .map(|(name, value)| ((*name).into(), (*value).into())),
            );
            self
        }

        pub fn called(&self, prefix: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with(prefix))
                .count()
        }
    }

    impl Source for Stub {
        fn environment(&self, name: &str) -> Result<Option<String>> {
            self.calls.borrow_mut().push(format!("env:{name}"));
            ensure!(
                !self.invalid_env.contains(name),
                "{name} contains invalid text"
            );
            Ok(self.env.get(name).cloned())
        }
        fn keychain(&self, account: &str) -> Result<Option<String>> {
            self.calls.borrow_mut().push(format!("keychain:{account}"));
            ensure!(!self.keychain_error, "credential store unavailable");
            Ok(self.key.clone())
        }
        fn save_key(&self, account: &str, _key: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("save:{account}"));
            ensure!(!self.write_error, "credential store unavailable");
            Ok(())
        }
        fn delete_key(&self, account: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("delete:{account}"));
            ensure!(!self.write_error, "credential store unavailable");
            Ok(())
        }
        fn saved_hint(&self, id: ProviderId, saved: bool) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("hint:{}:{saved}", id.id()));
            Ok(())
        }
        fn file(&self, path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
            self.calls
                .borrow_mut()
                .push(format!("file:{}", path.display()));
            Ok(self.files.get(path).cloned().map(Zeroizing::new))
        }
        fn gcloud(&self) -> Result<Option<PathBuf>> {
            self.calls.borrow_mut().push("find-gcloud".into());
            Ok(self.gcloud_path.clone())
        }
        fn run_gcloud(&self, _path: &Path) -> Result<Zeroizing<String>> {
            self.calls.borrow_mut().push("run-gcloud".into());
            self.command_output
                .clone()
                .map(Zeroizing::new)
                .ok_or_else(|| anyhow!("raw command diagnostic must never escape"))
        }
        fn metadata_allowed(&self) -> bool {
            self.metadata
        }
        fn send_json(&self, req: http::HttpRequest<'_>) -> Result<(Value, http::Headers)> {
            self.requests.borrow_mut().push(Request {
                url: req.url,
                body: String::from_utf8_lossy(&req.body).into_owned(),
                headers: req.headers,
                direct: req.direct,
                timeout: req.timeout,
            });
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| anyhow!("scripted request failure"))
        }
        fn aws_chain(&self, profile: Option<&str>) -> Result<(crate::aws::Keys, String)> {
            self.calls
                .borrow_mut()
                .push(format!("aws:{}", profile.unwrap_or("<chain>")));
            ensure!(self.aws, "no AWS credentials are configured");
            Ok((
                crate::aws::Keys::new("TESTKEY".into(), "test-secret".into(), None),
                "scripted AWS credentials".into(),
            ))
        }
        fn aws_region(&self, profile: &str) -> Option<String> {
            self.calls.borrow_mut().push(format!("region:{profile}"));
            self.region.clone()
        }
        fn now(&self) -> u64 {
            self.clock.get()
        }
        fn cache_tokens(&self) -> bool {
            self.cache
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_source::Stub, *};

    fn header_value(auth: &Auth) -> (&str, &str) {
        match auth {
            Auth::Header { name, value, .. } => (name, value),
            _ => panic!("expected a header"),
        }
    }

    #[test]
    fn adc_refresh_uses_the_google_cache_and_never_falls_back_to_an_express_key() {
        let mut source = Stub::default().vars(&[
            ("GOOGLE_APPLICATION_CREDENTIALS", "/refresh-fixture.json"),
            ("GOOGLE_CLOUD_API_KEY", "express-key-must-not-be-used"),
        ]);
        source.cache = true;
        source.key = Some("saved-key-must-not-be-used".into());
        source.clock.set(50_000);
        source.files.insert(
            "/refresh-fixture.json".into(),
            serde_json::to_vec(&serde_json::json!({
                "type":"authorized_user", "client_id":"refresh-fixture-client",
                "client_secret":"refresh-fixture-secret", "refresh_token":"refresh-fixture-token",
                "quota_project_id":"refresh-quota"
            }))
            .unwrap(),
        );
        for token in ["first-access-token", "second-access-token"] {
            source.responses.borrow_mut().push_back((
                serde_json::json!({"access_token":token,"token_type":"Bearer","expires_in":3600}),
                Default::default(),
            ));
        }
        assert_eq!(
            header_value(&refresh_google_in(&source).unwrap()).1,
            "Bearer first-access-token"
        );
        source.clock.set(53_299);
        assert_eq!(
            header_value(&refresh_google_in(&source).unwrap()).1,
            "Bearer first-access-token"
        );
        assert_eq!(source.requests.borrow().len(), 1);
        source.clock.set(53_300);
        let auth = refresh_google_in(&source).unwrap();
        assert_eq!(header_value(&auth).1, "Bearer second-access-token");
        assert!(
            matches!(auth, Auth::Header { quota_project:Some(ref project), .. } if project=="refresh-quota")
        );
        assert_eq!(source.requests.borrow().len(), 2);
        source.files.clear();
        assert!(
            refresh_google_in(&source).is_err(),
            "missing ADC must not use express credentials"
        );
        assert_eq!(source.called("keychain:"), 0);
        assert_eq!(source.called("env:GOOGLE_CLOUD_API_KEY"), 0);
    }

    #[test]
    fn bounded_sources_stop_before_any_side_effect_and_cap_each_auth_request() {
        let source = Stub::default();
        let cancel = AtomicBool::new(true);
        let bounded = BoundedSource {
            inner: &source,
            deadline: Instant::now() + Duration::from_secs(1),
            cancel: &cancel,
        };
        assert!(resolve_in(&bounded, ProviderId::OpenAi, &ProviderSettings::default()).is_err());
        assert!(source.calls.borrow().is_empty());
        cancel.store(false, Ordering::SeqCst);
        let expired = BoundedSource {
            inner: &source,
            deadline: Instant::now(),
            cancel: &cancel,
        };
        assert!(expired.environment("OPENAI_API_KEY").is_err());
        assert!(source.calls.borrow().is_empty());
        source
            .responses
            .borrow_mut()
            .push_back((serde_json::json!({"ok":true}), http::Headers::new()));
        let bounded = BoundedSource {
            inner: &source,
            deadline: Instant::now() + Duration::from_secs(2),
            cancel: &cancel,
        };
        let mut request = http::HttpRequest::get("https://example.test/token", &Auth::None);
        request.timeout = Some(Duration::from_secs(90));
        bounded.send_json(request).unwrap();
        let requests = source.requests.borrow();
        assert!(requests[0].timeout.unwrap() <= Duration::from_secs(2));
        assert!(!requests[0].timeout.unwrap().is_zero());
    }

    #[test]
    fn environment_precedence_skips_empty_and_never_opens_keychain() {
        let source = Stub {
            keychain_error: true,
            ..Stub::default()
        }
        .vars(&[("GOOGLE_API_KEY", ""), ("GEMINI_API_KEY", "gemini-test")]);
        let auth = resolve_in(&source, ProviderId::Gemini, &ProviderSettings::default())
            .unwrap()
            .auth;
        assert_eq!(header_value(&auth), ("x-goog-api-key", "gemini-test"));
        assert_eq!(source.called("keychain:"), 0);
        let source = Stub {
            keychain_error: true,
            ..Stub::default()
        }
        .vars(&[("GOOGLE_API_KEY", "first"), ("GEMINI_API_KEY", "second")]);
        let auth = resolve_in(&source, ProviderId::Gemini, &ProviderSettings::default())
            .unwrap()
            .auth;
        assert_eq!(header_value(&auth).1, "first");
    }

    #[test]
    fn invalid_nonempty_environment_is_not_a_keychain_fallback() {
        for value in ["bad key", "line\nbreak", "é", &"x".repeat(4097)] {
            let source = Stub {
                key: Some("valid-saved-key".into()),
                ..Stub::default()
            }
            .vars(&[("OPENAI_API_KEY", value)]);
            let error = resolve_in(&source, ProviderId::OpenAi, &ProviderSettings::default())
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("OPENAI_API_KEY"));
            assert!(!error.contains(value));
            assert_eq!(source.called("keychain:"), 0);
        }
    }

    #[test]
    fn another_providers_key_and_environment_are_not_used() {
        let mut source = Stub::default().vars(&[("OPENAI_API_KEY", "only-openai")]);
        source.invalid_env.insert("AWS_REGION".into());
        assert!(resolve_in(&source, ProviderId::Groq, &ProviderSettings::default()).is_err());
        assert_eq!(source.called("env:OPENAI_API_KEY"), 0);
        assert_eq!(source.called("env:AWS_REGION"), 0);
    }

    #[test]
    fn saved_measurement_accounts_and_auth_headers_are_reused() {
        for (id, account, name, value) in [
            (
                ProviderId::OpenAi,
                "openai",
                "authorization",
                "Bearer saved",
            ),
            (ProviderId::Anthropic, "claude", "x-api-key", "saved"),
            (ProviderId::Gemini, "gemini", "x-goog-api-key", "saved"),
        ] {
            let source = Stub {
                key: Some("saved".into()),
                ..Stub::default()
            };
            let auth = resolve_in(&source, id, &ProviderSettings::default())
                .unwrap()
                .auth;
            assert_eq!(header_value(&auth), (name, value));
            assert_eq!(source.called(&format!("keychain:{account}")), 1);
        }
    }

    #[test]
    fn status_never_runs_commands_or_sends_requests_and_honors_keychain_flag() {
        let source = Stub {
            key: Some("saved".into()),
            gcloud_path: Some("/fake/gcloud".into()),
            ..Stub::default()
        };
        let hinted = ProviderSettings {
            saved_key: true,
            ..ProviderSettings::default()
        };
        assert!(status_in(&source, ProviderId::OpenAi, &hinted, false).ready);
        assert_eq!(source.called("keychain:"), 0);
        let vertex = ProviderSettings {
            project: Some("test-project".into()),
            ..ProviderSettings::default()
        };
        assert!(status_in(&source, ProviderId::Vertex, &vertex, true).ready);
        assert_eq!(source.called("run-gcloud"), 0);
        assert!(source.requests.borrow().is_empty());
        assert!(
            status_in(
                &source,
                ProviderId::OpenAi,
                &ProviderSettings::default(),
                true
            )
            .ready
        );
        assert_eq!(source.called("keychain:openai"), 1);
        assert!(
            !status_in(
                &Stub::default(),
                ProviderId::OpenAi,
                &ProviderSettings::default(),
                false
            )
            .ready
        );
    }

    #[test]
    fn status_includes_missing_request_settings_without_prompting_keychain() {
        for id in [
            ProviderId::Custom,
            ProviderId::LmStudio,
            ProviderId::Azure,
            ProviderId::Cloudflare,
            ProviderId::Bedrock,
        ] {
            let source = Stub {
                key: Some("saved".into()),
                ..Stub::default()
            };
            let status = status_in(&source, id, &ProviderSettings::default(), true);
            assert!(!status.ready, "{id} cannot request with missing settings");
            assert!(!status.missing.is_empty());
            assert_eq!(source.called("keychain:"), 0);
            assert!(source.requests.borrow().is_empty());
        }
        let source = Stub {
            gcloud_path: Some("/fake/gcloud".into()),
            ..Stub::default()
        };
        assert!(
            !status_in(
                &source,
                ProviderId::Vertex,
                &ProviderSettings::default(),
                true
            )
            .ready
        );
        assert_eq!(source.called("run-gcloud"), 0);
    }

    #[test]
    fn save_remove_hints_follow_successful_keychain_actions() {
        let source = Stub::default();
        store_key_in(&source, ProviderId::Anthropic, "valid".into()).unwrap();
        delete_key_in(&source, ProviderId::Anthropic).unwrap();
        assert_eq!(
            &*source.calls.borrow(),
            &[
                "save:claude",
                "hint:anthropic:true",
                "delete:claude",
                "hint:anthropic:false"
            ]
        );
        let source = Stub {
            write_error: true,
            ..Stub::default()
        };
        assert!(store_key_in(&source, ProviderId::OpenAi, "valid".into()).is_err());
        assert!(delete_key_in(&source, ProviderId::OpenAi).is_err());
        assert_eq!(source.called("hint:"), 0);
        assert!(store_key_in(&source, ProviderId::OpenAi, "bad key".into()).is_err());
        assert_eq!(source.called("save:"), 1);
    }

    #[test]
    fn vertex_express_and_claude_adc_rules_are_separate() {
        let source = Stub {
            keychain_error: true,
            ..Stub::default()
        }
        .vars(&[("GOOGLE_CLOUD_API_KEY", "express")]);
        let auth = resolve_in(&source, ProviderId::Vertex, &ProviderSettings::default())
            .unwrap()
            .auth;
        assert!(auth.vertex_express());
        assert_eq!(source.called("keychain:"), 0);
        let settings = ProviderSettings {
            model: Some("claude-sonnet-5-5".into()),
            ..ProviderSettings::default()
        };
        let error = resolve_in(&source, ProviderId::Vertex, &settings)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            error,
            "Claude on Vertex AI needs Google Cloud credentials (ADC). API keys work only for Gemini models."
        );
        let mut invalid = Stub::default().vars(&[
            ("GOOGLE_CLOUD_API_KEY", "express"),
            ("GOOGLE_APPLICATION_CREDENTIALS", "/adc.json"),
        ]);
        invalid.files.insert(
            "/adc.json".into(),
            br#"{"type":"external_account"}"#.to_vec(),
        );
        assert!(
            resolve_in(&invalid, ProviderId::Vertex, &ProviderSettings::default())
                .err()
                .unwrap()
                .to_string()
                .contains("external_account")
        );
    }

    #[test]
    fn bedrock_bearer_wins_and_regions_travel_with_credentials() {
        let source = Stub {
            region: Some("eu-west-2".into()),
            keychain_error: true,
            ..Stub::default()
        }
        .vars(&[("AWS_BEARER_TOKEN_BEDROCK", "bearer")]);
        let resolved =
            resolve_in(&source, ProviderId::Bedrock, &ProviderSettings::default()).unwrap();
        assert_eq!(
            header_value(&resolved.auth),
            ("authorization", "Bearer bearer")
        );
        assert_eq!(resolved.settings.region.as_deref(), Some("eu-west-2"));
        assert_eq!(source.called("aws:"), 0);
        assert_eq!(source.called("keychain:"), 0);
        let source = Stub {
            region: Some("eu-west-2".into()),
            ..Stub::default()
        }
        .vars(&[
            ("AWS_BEARER_TOKEN_BEDROCK", "bearer"),
            ("AWS_REGION", "us-east-1"),
            ("AWS_DEFAULT_REGION", "us-west-2"),
        ]);
        let resolved =
            resolve_in(&source, ProviderId::Bedrock, &ProviderSettings::default()).unwrap();
        assert_eq!(resolved.settings.region.as_deref(), Some("us-east-1"));
        assert_eq!(source.called("region:"), 0);
    }

    #[test]
    fn bedrock_profile_environment_and_saved_bearer_precedence() {
        let settings = ProviderSettings {
            region: Some("us-east-1".into()),
            ..ProviderSettings::default()
        };
        for vars in [
            vec![("AWS_PROFILE", "work")],
            vec![
                ("AWS_ACCESS_KEY_ID", "id"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
            ],
        ] {
            let source = Stub {
                aws: true,
                key: Some("saved-bearer".into()),
                ..Stub::default()
            }
            .vars(&vars);
            assert!(matches!(
                resolve_in(&source, ProviderId::Bedrock, &settings)
                    .unwrap()
                    .auth,
                Auth::Aws { .. }
            ));
            assert_eq!(source.called("keychain:"), 0);
            assert_eq!(source.called("aws:<chain>"), 1);
        }
        let source = Stub {
            aws: true,
            key: Some("saved-bearer".into()),
            ..Stub::default()
        }
        .vars(&[("AWS_ACCESS_KEY_ID", "ignored-incomplete")]);
        let explicit = ProviderSettings {
            profile: Some("chosen".into()),
            ..settings.clone()
        };
        assert!(matches!(
            resolve_in(&source, ProviderId::Bedrock, &explicit)
                .unwrap()
                .auth,
            Auth::Aws { .. }
        ));
        assert_eq!(source.called("aws:chosen"), 1);
        let source = Stub {
            key: Some("saved-bearer".into()),
            ..Stub::default()
        };
        let auth = resolve_in(&source, ProviderId::Bedrock, &settings)
            .unwrap()
            .auth;
        assert_eq!(header_value(&auth).1, "Bearer saved-bearer");
        assert_eq!(source.called("aws:"), 0);
        let source = Stub {
            key: Some("saved-bearer".into()),
            aws: true,
            ..Stub::default()
        }
        .vars(&[("AWS_SECRET_ACCESS_KEY", "incomplete")]);
        assert!(resolve_in(&source, ProviderId::Bedrock, &settings).is_err());
        assert_eq!(source.called("keychain:"), 0);
        let source = Stub {
            aws: true,
            ..Stub::default()
        }
        .vars(&[("AWS_PROFILE", "work"), ("AWS_ACCESS_KEY_ID", "incomplete")]);
        assert!(resolve_in(&source, ProviderId::Bedrock, &settings).is_err());
        assert_eq!(source.called("aws:"), 0);
        // AWS_PROFILE is a fallback after complete environment credentials;
        // only an explicitly chosen settings.profile ignores exported keys.
        let source = Stub {
            aws: true,
            ..Stub::default()
        }
        .vars(&[
            ("AWS_PROFILE", "work"),
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]);
        assert!(matches!(
            resolve_in(&source, ProviderId::Bedrock, &settings)
                .unwrap()
                .auth,
            Auth::Aws { .. }
        ));
        assert_eq!(source.called("aws:<chain>"), 1);
    }

    #[test]
    fn local_ollama_has_no_auth_and_optional_local_services_work_without_keys() {
        let source = Stub {
            keychain_error: true,
            ..Stub::default()
        }
        .vars(&[("OLLAMA_API_KEY", "not-sent-locally")]);
        assert!(matches!(
            resolve_in(&source, ProviderId::Ollama, &ProviderSettings::default())
                .unwrap()
                .auth,
            Auth::None
        ));
        assert_eq!(source.called("keychain:"), 0);
        let settings = ProviderSettings {
            base_url: Some("https://ollama.com/v1".into()),
            ..ProviderSettings::default()
        };
        assert_eq!(
            header_value(
                &resolve_in(&source, ProviderId::Ollama, &settings)
                    .unwrap()
                    .auth
            )
            .1,
            "Bearer not-sent-locally"
        );
        let source = Stub::default();
        let settings = ProviderSettings {
            model: Some("local-model".into()),
            ..ProviderSettings::default()
        };
        assert!(matches!(
            resolve_in(&source, ProviderId::LmStudio, &settings)
                .unwrap()
                .auth,
            Auth::None
        ));
    }

    #[test]
    fn adc_native_reader_rejects_symlinks_directories_and_oversized_files() {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-adc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("adc.json");
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(&*bounded_regular_file(&path).unwrap().unwrap(), b"{}");
        assert!(bounded_regular_file(&dir).is_err());
        #[cfg(unix)]
        {
            let link = dir.join("link.json");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(bounded_regular_file(&link).is_err());
        }
        std::fs::write(&path, vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        assert!(bounded_regular_file(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
