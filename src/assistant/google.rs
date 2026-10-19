//! Google Cloud access tokens. ADC files are data, never instructions to run
//! credential helpers or contact a URL named by the file.
use super::{
    credentials::{Auth, Source},
    http::HttpRequest,
};
use anyhow::{Result, anyhow, bail, ensure};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const METADATA_URL: &str =
    "http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const MAX_ADC: usize = 64 * 1024;
const GCLOUD_ERROR: &str = "gcloud could not print an access token.";

pub(super) struct GoogleToken {
    pub value: Zeroizing<String>,
    pub quota_project: Option<String>,
}

struct CachedToken {
    token: GoogleToken,
    refresh_at: u64,
}

#[derive(Default)]
struct TokenCache {
    tokens: HashMap<String, CachedToken>,
    metadata_failed: bool,
}

fn cache() -> &'static Mutex<TokenCache> {
    static CACHE: OnceLock<Mutex<TokenCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(TokenCache::default()))
}

fn cached(source: &dyn Source, key: &str) -> Option<GoogleToken> {
    if !source.cache_tokens() {
        return None;
    }
    let mut cache = cache().lock().ok()?;
    if cache
        .tokens
        .get(key)
        .is_some_and(|entry| source.now() >= entry.refresh_at)
    {
        cache.tokens.remove(key);
    }
    cache.tokens.get(key).map(|entry| GoogleToken {
        value: Zeroizing::new(entry.token.value.to_string()),
        quota_project: entry.token.quota_project.clone(),
    })
}

fn remember(source: &dyn Source, key: String, token: &GoogleToken, lifetime: u64, early: u64) {
    if !source.cache_tokens() {
        return;
    }
    if let Ok(mut cache) = cache().lock() {
        // There is only one token per credential file in ordinary use. Bound
        // replacements so repeatedly editing ADC cannot retain old secrets.
        if cache.tokens.len() >= 16 && !cache.tokens.contains_key(&key) {
            cache.tokens.clear();
        }
        cache.tokens.insert(
            key,
            CachedToken {
                token: GoogleToken {
                    value: Zeroizing::new(token.value.to_string()),
                    quota_project: token.quota_project.clone(),
                },
                refresh_at: source.now().saturating_add(lifetime.saturating_sub(early)),
            },
        );
    }
}

struct Secret(Zeroizing<String>);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer).map(|value| Self(Zeroizing::new(value)))
    }
}

#[derive(Deserialize)]
struct Adc {
    #[serde(rename = "type")]
    kind: String,
    private_key: Option<Secret>,
    private_key_id: Option<String>,
    client_email: Option<String>,
    client_id: Option<Secret>,
    client_secret: Option<Secret>,
    refresh_token: Option<Secret>,
    token_uri: Option<String>,
    quota_project_id: Option<String>,
}

fn parse_adc(bytes: &[u8]) -> Result<Adc> {
    ensure!(bytes.len() <= MAX_ADC, "Google ADC file exceeds 64 KiB");
    let adc: Adc = serde_json::from_slice(bytes)
        .map_err(|_| anyhow!("Google ADC file is not valid credential JSON"))?;
    if !matches!(adc.kind.as_str(), "service_account" | "authorized_user") {
        let kind = match adc.kind.as_str() {
            "external_account"
            | "external_account_authorized_user"
            | "impersonated_service_account" => adc.kind.as_str(),
            _ => "unknown",
        };
        bail!(
            "Google ADC type {kind} is unsupported; use a service-account key, `gcloud auth application-default login`, or a Vertex express API key."
        );
    }
    ensure!(
        adc.token_uri.as_deref().is_none_or(|uri| uri == TOKEN_URL),
        "Google ADC token_uri must be https://oauth2.googleapis.com/token"
    );
    if adc.kind == "service_account" {
        ensure!(
            adc.token_uri.as_deref() == Some(TOKEN_URL),
            "Google ADC token_uri must be https://oauth2.googleapis.com/token"
        );
        ensure!(
            adc.private_key.is_some()
                && adc
                    .client_email
                    .as_ref()
                    .is_some_and(|value| !value.is_empty()
                        && value.len() <= 320
                        && value.bytes().all(|byte| byte.is_ascii_graphic())),
            "Google service-account credentials need a private key and client email"
        );
    } else {
        ensure!(
            [&adc.client_id, &adc.client_secret, &adc.refresh_token]
                .iter()
                .all(|value| value.as_ref().is_some_and(|value| !value.0.is_empty())),
            "Google authorized-user credentials need client_id, client_secret and refresh_token"
        );
    }
    if let Some(project) = &adc.quota_project_id {
        ensure!(
            !project.is_empty()
                && project.len() <= 128
                && project.bytes().all(|byte| byte.is_ascii_alphanumeric()
                    || byte == b'-'
                    || byte == b'_'
                    || byte == b'.'),
            "Google ADC quota project is invalid"
        );
    }
    Ok(adc)
}

fn adc_path(source: &dyn Source) -> Result<(Option<PathBuf>, bool)> {
    if let Some(path) = source
        .environment("GOOGLE_APPLICATION_CREDENTIALS")?
        .filter(|value| !value.is_empty())
    {
        return Ok((Some(path.into()), true));
    }
    #[cfg(windows)]
    let root = source
        .environment("APPDATA")?
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    #[cfg(not(windows))]
    let root = match source
        .environment("CLOUDSDK_CONFIG")?
        .filter(|value| !value.is_empty())
    {
        Some(root) => {
            return Ok((
                Some(PathBuf::from(root).join("application_default_credentials.json")),
                false,
            ));
        }
        None => match source
            .environment("XDG_CONFIG_HOME")?
            .filter(|value| !value.is_empty())
        {
            Some(root) => Some(root.into()),
            None => source
                .environment("HOME")?
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config")),
        },
    };
    Ok((
        root.map(|root| root.join("gcloud/application_default_credentials.json")),
        false,
    ))
}

/// Local readiness only. Finding a command is permitted; executing it or
/// asking the metadata server is reserved for resolve on a worker.
pub(super) fn status(source: &dyn Source) -> Result<Option<String>> {
    let (path, explicit) = adc_path(source)?;
    if let Some(path) = path {
        if let Some(bytes) = source.file(&path)? {
            parse_adc(&bytes)?;
            return Ok(Some("Google Application Default Credentials".into()));
        }
        ensure!(
            !explicit,
            "GOOGLE_APPLICATION_CREDENTIALS does not name a readable ADC file"
        );
    }
    if source.gcloud()?.is_some() {
        return Ok(Some("gcloud (resolved when a request is sent)".into()));
    }
    if source.metadata_allowed() {
        return Ok(Some(
            "Google Compute Engine credentials (resolved when a request is sent)".into(),
        ));
    }
    Ok(None)
}

pub(super) fn resolve(source: &dyn Source) -> Result<Option<GoogleToken>> {
    let (path, explicit) = adc_path(source)?;
    if let Some(path) = path {
        if let Some(bytes) = source.file(&path)? {
            let adc = parse_adc(&bytes)?;
            let key = format!("adc:{:x}", Sha256::digest(&*bytes));
            if let Some(token) = cached(source, &key) {
                return Ok(Some(token));
            }
            let (token, lifetime) = exchange(source, adc)?;
            remember(source, key, &token, lifetime, 300);
            return Ok(Some(token));
        }
        ensure!(
            !explicit,
            "GOOGLE_APPLICATION_CREDENTIALS does not name a readable ADC file"
        );
    }
    if let Some(path) = source.gcloud()? {
        let key = format!("gcloud:{}", path.to_string_lossy());
        if let Some(token) = cached(source, &key) {
            return Ok(Some(token));
        }
        let value = source
            .run_gcloud(&path)
            .map_err(|_| anyhow!(GCLOUD_ERROR))?;
        let value = value.trim();
        validate_token(value).map_err(|_| anyhow!(GCLOUD_ERROR))?;
        let token = GoogleToken {
            value: Zeroizing::new(value.to_owned()),
            quota_project: None,
        };
        remember(source, key, &token, 45 * 60, 0);
        return Ok(Some(token));
    }
    metadata(source)
}

fn jwt(adc: &Adc, now: u64) -> Result<Zeroizing<String>> {
    let private = adc
        .private_key
        .as_ref()
        .ok_or_else(|| anyhow!("Google service-account private key is missing"))?;
    let pem = private.0.trim();
    let pem = pem
        .strip_prefix("-----BEGIN PRIVATE KEY-----")
        .and_then(|value| value.strip_suffix("-----END PRIVATE KEY-----"))
        .ok_or_else(|| anyhow!("Google service-account private key must be PKCS#8 PEM"))?;
    let encoded = Zeroizing::new(
        pem.chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>(),
    );
    let der = Zeroizing::new(
        STANDARD
            .decode(encoded.as_bytes())
            .map_err(|_| anyhow!("Google service-account private key is invalid"))?,
    );
    let mut header = json!({"alg":"RS256","typ":"JWT"});
    if let Some(id) = &adc.private_key_id {
        header["kid"] = json!(id);
    }
    let claims = json!({"iss":adc.client_email,"scope":SCOPE,"aud":TOKEN_URL,"iat":now,"exp":now.checked_add(3600).ok_or_else(|| anyhow!("system time is invalid"))?});
    let signing = Zeroizing::new(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?)
    ));
    let signature = {
        let pair = RsaKeyPair::from_pkcs8(&der)
            .map_err(|_| anyhow!("Google service-account private key is invalid"))?;
        let mut signature = Zeroizing::new(vec![0; pair.public().modulus_len()]);
        pair.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing.as_bytes(),
            &mut signature,
        )
        .map_err(|_| anyhow!("Google service-account token could not be signed"))?;
        signature
    };
    Ok(Zeroizing::new(format!(
        "{}.{}",
        signing.as_str(),
        URL_SAFE_NO_PAD.encode(&*signature)
    )))
}

fn exchange(source: &dyn Source, adc: Adc) -> Result<(GoogleToken, u64)> {
    let auth = Auth::None;
    let assertion;
    let pairs = if adc.kind == "service_account" {
        assertion = jwt(&adc, source.now())?;
        vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ]
    } else {
        vec![
            ("grant_type", "refresh_token"),
            ("client_id", adc.client_id.as_ref().unwrap().0.as_str()),
            (
                "client_secret",
                adc.client_secret.as_ref().unwrap().0.as_str(),
            ),
            (
                "refresh_token",
                adc.refresh_token.as_ref().unwrap().0.as_str(),
            ),
        ]
    };
    let (response, _) = source
        .send_json(HttpRequest::form(TOKEN_URL, &pairs, &auth))
        .map_err(|_| anyhow!("Google Cloud access token could not be refreshed"))?;
    token_response(response, adc.quota_project_id)
}

fn validate_token(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 16384
            && value.bytes().all(|byte| byte.is_ascii_graphic()),
        "Google access token is invalid"
    );
    Ok(())
}

fn token_response(
    mut response: Value,
    quota_project: Option<String>,
) -> Result<(GoogleToken, u64)> {
    let value = response
        .get_mut("access_token")
        .map(Value::take)
        .and_then(|value| match value {
            Value::String(value) => Some(Zeroizing::new(value)),
            _ => None,
        })
        .ok_or_else(|| anyhow!("Google token response has no access token"))?;
    validate_token(&value)?;
    ensure!(
        response
            .get("token_type")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("Bearer")),
        "Google token response has an invalid token type"
    );
    let lifetime = response
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0 && *value <= 86400)
        .ok_or_else(|| anyhow!("Google token response has an invalid expiry"))?;
    Ok((
        GoogleToken {
            value,
            quota_project,
        },
        lifetime,
    ))
}

fn metadata(source: &dyn Source) -> Result<Option<GoogleToken>> {
    if !source.metadata_allowed() {
        return Ok(None);
    }
    if let Some(token) = cached(source, "metadata") {
        return Ok(Some(token));
    }
    if source.cache_tokens()
        && cache()
            .lock()
            .map(|cache| cache.metadata_failed)
            .unwrap_or(true)
    {
        return Ok(None);
    }
    let auth = Auth::None;
    let mut req = HttpRequest::get(METADATA_URL, &auth);
    req.direct = true;
    req.timeout = Some(Duration::from_secs(1));
    req.headers
        .push(("Metadata-Flavor".into(), "Google".into()));
    let result = source.send_json(req).and_then(|(response, headers)| {
        ensure!(
            headers
                .get("metadata-flavor")
                .is_some_and(|value| value == "Google"),
            "Google metadata response is missing Metadata-Flavor: Google"
        );
        token_response(response, None)
    });
    match result {
        Ok((token, lifetime)) => {
            remember(source, "metadata".into(), &token, lifetime, 300);
            Ok(Some(token))
        }
        Err(_) => {
            if source.cache_tokens()
                && let Ok(mut cache) = cache().lock()
            {
                cache.metadata_failed = true;
            }
            Ok(None)
        }
    }
}

/// An environment variable can enable a probe, but can never change its
/// destination. Otherwise only a machine-identification file enables it.
pub(super) fn native_metadata_allowed(source: &dyn Source) -> bool {
    if source
        .environment("GCE_METADATA_HOST")
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        source
            .file(Path::new("/sys/class/dmi/id/product_name"))
            .ok()
            .flatten()
            .is_some_and(|bytes| String::from_utf8_lossy(&bytes).trim() == "Google Compute Engine")
    }
    #[cfg(windows)]
    {
        // The hardware registry is local and does not start a helper process.
        windows_google_bios()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    false
}

#[cfg(windows)]
fn windows_google_bios() -> bool {
    // `reg query` is intentionally not used: credential resolution does not
    // execute machine-identity helpers. Read the local BIOS value through the
    // registry API, available in Windows itself in every build.
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegGetValueW(
            key: *mut c_void,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            bytes: *mut u32,
        ) -> i32;
    }
    let subkey: Vec<u16> = "HARDWARE\\DESCRIPTION\\System\\BIOS\0"
        .encode_utf16()
        .collect();
    let name: Vec<u16> = "SystemManufacturer\0".encode_utf16().collect();
    let mut data = [0u16; 256];
    let mut bytes = std::mem::size_of_val(&data) as u32;
    // RRF_RT_REG_SZ; HKEY_LOCAL_MACHINE is the sign-extended predefined handle.
    let result = unsafe {
        RegGetValueW(
            0x8000_0002u32 as i32 as isize as *mut c_void,
            subkey.as_ptr(),
            name.as_ptr(),
            2,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    result == 0
        && String::from_utf16_lossy(&data[..(bytes as usize / 2).min(data.len())])
            .trim_end_matches('\0')
            .starts_with("Google")
}

pub(super) fn find_gcloud(source: &dyn Source) -> Result<Option<PathBuf>> {
    let Some(path) = source.environment("PATH")? else {
        return Ok(None);
    };
    Ok(find_on_path(&path, cfg!(windows), |candidate| {
        let Ok(metadata) = std::fs::metadata(candidate) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        true
    }))
}

fn find_on_path(path: &str, windows: bool, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let names: &[&str] = if windows {
        &["gcloud.cmd", "gcloud.exe"]
    } else {
        &["gcloud"]
    };
    std::env::split_paths(path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .find_map(|directory| {
            names
                .iter()
                .map(|name| directory.join(name))
                .find(|candidate| exists(candidate))
        })
}

pub(super) fn run_gcloud(path: &Path) -> Result<Zeroizing<String>> {
    run_gcloud_bounded(path, Duration::from_secs(10), &AtomicBool::new(false))
}

pub(super) fn run_gcloud_bounded(
    path: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Zeroizing<String>> {
    ensure!(
        !cancel.load(Ordering::SeqCst) && !timeout.is_zero(),
        GCLOUD_ERROR
    );
    let deadline = Instant::now() + timeout.min(Duration::from_secs(10));
    let mut command = gcloud_command(path);
    let mut child = command.spawn().map_err(|_| anyhow!(GCLOUD_ERROR))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!(GCLOUD_ERROR))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Zeroizing::new(Vec::new());
        let result = stdout.take(16386).read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if !cancel.load(Ordering::SeqCst) && Instant::now() < deadline => {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(25)),
                );
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(GCLOUD_ERROR);
            }
        }
    };
    ensure!(status.success(), GCLOUD_ERROR);
    let bytes = receiver
        .recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        )
        .map_err(|_| anyhow!(GCLOUD_ERROR))?
        .map_err(|_| anyhow!(GCLOUD_ERROR))?;
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!(GCLOUD_ERROR))?
        .trim();
    validate_token(value).map_err(|_| anyhow!(GCLOUD_ERROR))?;
    ensure!(
        !cancel.load(Ordering::SeqCst) && Instant::now() < deadline,
        GCLOUD_ERROR
    );
    Ok(Zeroizing::new(value.to_owned()))
}

fn gcloud_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    command
        .args(["auth", "print-access-token", "--quiet"])
        .env("CLOUDSDK_CORE_DISABLE_PROMPTS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn expired_or_cancelled_gcloud_budget_never_spawns_a_process() {
        let path = Path::new("/this-test-must-never-run-gcloud");
        assert!(run_gcloud_bounded(path, Duration::ZERO, &AtomicBool::new(false)).is_err());
        assert!(run_gcloud_bounded(path, Duration::from_secs(1), &AtomicBool::new(true)).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::super::credentials::test_source::Stub;
    use super::*;
    use ring::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    // Generated solely for these offline tests with openssl genpkey RSA/2048.
    // This key has never been used by an account or a real service.
    const TEST_ONLY_RSA_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC1x+M6L9rsxR3O
6iUZdSdGPB9fV42ci0kEVUzF6I22QSSxEVpa1bUzLy5hxV/YDW4PDQNyJIVkMkBu
LF8hZdn582K1DfokjY36nFLQJsvjPp9gTtnpToXLwM7W4pZIIlc1oS/ZBbRH/o0W
0rIumfMBHuIGcdeayXOaock9db1fk8zoRjNf8qYCCkviBOPHp7D0aiXUIQiAPaWy
SoawrX0A3Cf5Vdj7vnH+C3tyOsVvM2LYWPcLm1AC1RmAgWn0qCfa6sMNqZPznWTj
od8HLtW1X3MfmUsQsQofzUj9etS9ejjJGrZP5c8RH83GHKsF3So8a51J7v+dSKp2
MCUivmbFAgMBAAECggEAI2W93ewJL79vn0XxfPxW7kKRYl24dfIagRVCKqyvAb08
MZ29MEB6tTGcEM5gLEBrPNOOJvktH3Z4nOuXh7efY2CNSU/kNJ969QDhUGapn8Hn
Q5f/L4MKo0NTk70SQhAr1sXhPRuzACz1mOrk5xWz3/U9Ur9y2lGdqRAVkZn+iyB2
4XFeOvkZnrwR6DP7G+r76nzlD7+DXBt62M+psH+8RIY5287eU78nWjhDmfZIWKZG
RA38XRqLE0EiCBg+6iKq5ZCfCpNpopiUhd+O8Vt8gkAW9idh6I2X3NggGOHncQTw
RHv/RijPJ3tJ4jZag8bICnbFCxf08xbwCEIZGXd9+QKBgQDlsg3A8cIfVd+JEafW
oYc/9I/DPSTP0ZvWoyI89hA/NpIQuQ1Uro5lk/DC8mB4HSNkyzVrwv9/hCyvVgoE
DQgvl/vqAhiNQmJnHwuTVBdpyaUOOEx/nQ3sSaOk4UMF8yRNKn2F+55/7cuAU+mh
77oCPCTds7MbIYD41A0Kp14uGQKBgQDKmSBGu6eI6uQDAVokNHFQ/9Y4wHzeYomy
m74Gg86UFn9NW2t5OytSiBJURoDxn3YariesTO2s7Cky5rQPJJSUgPKtWcVokInW
6vHxeL0QcVXcB5WmtUxhCJvQO17+TxRnZBgGk4kx2sx4JMc72wefZJhNTE2TEb2N
FxlaJqF7jQKBgHxQ59y7fAMVqQw4gAxMFDVrGbvjhd7sZVm/pTI1QwnTcS6X2T0x
1GPps+7u24Nm0dclnF4chcfwM3caLAWRQBLyQSefxsX9XeDODqfIZXud5TVTByQy
TmzKVkVQncPEmqshChrP2jMG8pMb12ELrDK1doZ/UbqnYXRK4FwZxYFRAoGAa/8a
VHo9wV+s8ekrmvcVnJ70G8aICaevTTV2I1TprskbTSo5bSNcYZ9FXgVDQQnG1EwX
u+I6JB6zi32taBEQ3uSp+DxNXgMGZzlQhW7eIcC4ixTD8lzcfTEwldzAM4IGF2Mk
54SH8rp2mmjIh6CnPJFMGpBdHdOsHmS8z/CdzhUCgYEAov7ThrYZCbw9ka+/V96W
BYU17+uZ+PVWPnvarAn8OXCfEBJ2w+Wil/4Tg8iQWw5bu9ChL4KT5VUM1ShCep19
Df1Fwx/V2ysWpdxiL1C6P71cccr0+qrv5DFMCzM/6Rqq/OgTkcTJan6oHT1qTZLp
LIA6ICAWVSNUWMggNNTcT08=
-----END PRIVATE KEY-----"#;

    fn service_account() -> Value {
        json!({"type":"service_account","client_email":"camera@test-project.iam.gserviceaccount.com","private_key":TEST_ONLY_RSA_KEY,"private_key_id":"test-key-id","token_uri":TOKEN_URL})
    }

    fn authorized_user() -> Value {
        json!({"type":"authorized_user","client_id":"client-id","client_secret":"client-secret","refresh_token":"refresh+token","quota_project_id":"quota-project"})
    }

    fn file_source(value: &Value) -> Stub {
        let mut source =
            Stub::default().vars(&[("GOOGLE_APPLICATION_CREDENTIALS", "/fixture/adc.json")]);
        source.files.insert(
            "/fixture/adc.json".into(),
            serde_json::to_vec(value).unwrap(),
        );
        source.clock.set(1_700_000_000);
        source
    }

    fn response(source: &Stub, token: &str) {
        source.responses.borrow_mut().push_back((
            json!({"access_token":token,"token_type":"Bearer","expires_in":3600}),
            Default::default(),
        ));
    }

    #[test]
    fn service_account_jwt_claims_and_signature_are_valid() {
        let adc = parse_adc(&serde_json::to_vec(&service_account()).unwrap()).unwrap();
        let signed = jwt(&adc, 1_700_000_000).unwrap();
        let parts: Vec<_> = signed.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            header,
            json!({"alg":"RS256","typ":"JWT","kid":"test-key-id"})
        );
        assert_eq!(claims["iss"], "camera@test-project.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["aud"], TOKEN_URL);
        assert_eq!(claims["iat"], 1_700_000_000u64);
        assert_eq!(claims["exp"], 1_700_003_600u64);
        let pem = TEST_ONLY_RSA_KEY
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>();
        let der = STANDARD.decode(pem).unwrap();
        let pair = RsaKeyPair::from_pkcs8(&der).unwrap();
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, pair.public_key().as_ref())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn service_account_grant_is_sent_only_to_pinned_oauth_url() {
        let source = file_source(&service_account());
        response(&source, "service-token");
        let token = resolve(&source).unwrap().unwrap();
        assert_eq!(token.value.as_str(), "service-token");
        let requests = source.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, TOKEN_URL);
        assert!(
            requests[0]
                .body
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
        );
        assert!(requests[0].body.contains("assertion=eyJ"));
        assert!(!requests[0].body.contains("PRIVATE KEY"));
    }

    #[test]
    fn authorized_user_refresh_preserves_quota_project_and_encodes_form() {
        let source = file_source(&authorized_user());
        response(&source, "user-token");
        let token = resolve(&source).unwrap().unwrap();
        assert_eq!(token.quota_project.as_deref(), Some("quota-project"));
        assert_eq!(token.value.as_str(), "user-token");
        let requests = source.requests.borrow();
        assert_eq!(requests[0].url, TOKEN_URL);
        assert!(requests[0].body.contains("grant_type=refresh_token"));
        assert!(requests[0].body.contains("refresh_token=refresh%2Btoken"));
        assert!(requests[0].body.contains("client_secret=client-secret"));
    }

    #[test]
    fn token_uri_is_pinned_and_external_sources_are_never_followed() {
        for kind in ["service_account", "authorized_user"] {
            let mut adc = if kind == "service_account" {
                service_account()
            } else {
                authorized_user()
            };
            adc["token_uri"] = json!("https://attacker.invalid/token");
            let source = file_source(&adc);
            assert!(
                resolve(&source)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("token_uri")
            );
            assert!(source.requests.borrow().is_empty());
            assert_eq!(source.called("run-gcloud"), 0);
        }
        let source = file_source(
            &json!({"type":"external_account","credential_source":{"executable":{"command":"do-not-run"},"url":"http://attacker.invalid","file":"/private/key"}}),
        );
        let error = resolve(&source).err().unwrap().to_string();
        assert_eq!(
            error,
            "Google ADC type external_account is unsupported; use a service-account key, `gcloud auth application-default login`, or a Vertex express API key."
        );
        assert!(source.requests.borrow().is_empty());
        assert_eq!(source.called("file:"), 1);
        assert_eq!(source.called("run-gcloud"), 0);
    }

    #[test]
    fn adc_explicit_missing_is_error_and_well_known_paths_are_local() {
        let source = Stub::default().vars(&[("GOOGLE_APPLICATION_CREDENTIALS", "/missing.json")]);
        assert!(resolve(&source).is_err());
        let source = Stub::default().vars(&[("HOME", "/fake-home")]);
        assert!(resolve(&source).unwrap().is_none());
        #[cfg(not(windows))]
        assert_eq!(
            source.called("file:/fake-home/.config/gcloud/application_default_credentials.json"),
            1
        );
        #[cfg(not(windows))]
        {
            let source = Stub::default().vars(&[
                ("HOME", "/ignored-home"),
                ("CLOUDSDK_CONFIG", "/gcloud-config"),
            ]);
            assert!(resolve(&source).unwrap().is_none());
            assert_eq!(
                source.called("file:/gcloud-config/application_default_credentials.json"),
                1
            );
        }
    }

    #[test]
    fn gcloud_path_search_and_fake_command_runner_do_not_use_a_shell() {
        let path = std::env::join_paths([Path::new("/first"), Path::new("/second")])
            .unwrap()
            .into_string()
            .unwrap();
        assert_eq!(
            find_on_path(&path, false, |candidate| candidate
                == Path::new("/second/gcloud")),
            Some("/second/gcloud".into())
        );
        assert_eq!(
            find_on_path(&path, true, |candidate| candidate
                == Path::new("/first/gcloud.exe")),
            Some("/first/gcloud.exe".into())
        );
        assert!(find_on_path("", false, |_| true).is_none());
        let command = gcloud_command(Path::new("/fake/gcloud"));
        assert_eq!(command.get_program(), std::ffi::OsStr::new("/fake/gcloud"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["auth", "print-access-token", "--quiet"]
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "CLOUDSDK_CORE_DISABLE_PROMPTS"
                    && value == Some(std::ffi::OsStr::new("1")))
        );
        let source = Stub {
            gcloud_path: Some("/fake/gcloud".into()),
            command_output: Some(" fake-access-token\n".into()),
            ..Stub::default()
        };
        assert!(status(&source).unwrap().is_some());
        assert_eq!(source.called("run-gcloud"), 0);
        assert_eq!(
            resolve(&source).unwrap().unwrap().value.as_str(),
            "fake-access-token"
        );
        assert_eq!(source.called("run-gcloud"), 1);
        let failed = Stub {
            gcloud_path: Some("/fake/gcloud".into()),
            ..Stub::default()
        };
        assert_eq!(resolve(&failed).err().unwrap().to_string(), GCLOUD_ERROR);
        assert!(failed.requests.borrow().is_empty());
    }

    #[test]
    fn metadata_is_gated_literal_direct_and_requires_google_response_header() {
        let source = Stub::default();
        assert!(resolve(&source).unwrap().is_none());
        assert!(source.requests.borrow().is_empty());
        let source = Stub {
            metadata: true,
            ..Stub::default()
        }
        .vars(&[("GCE_METADATA_HOST", "attacker.invalid")]);
        let mut headers = super::super::http::Headers::new();
        headers.insert("metadata-flavor".into(), "Google".into());
        source.responses.borrow_mut().push_back((
            json!({"access_token":"metadata-token","token_type":"Bearer","expires_in":3600}),
            headers,
        ));
        assert_eq!(
            resolve(&source).unwrap().unwrap().value.as_str(),
            "metadata-token"
        );
        let requests = source.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, METADATA_URL);
        assert!(requests[0].direct);
        assert_eq!(requests[0].timeout, Some(Duration::from_secs(1)));
        assert!(
            requests[0]
                .headers
                .contains(&("Metadata-Flavor".into(), "Google".into()))
        );
        drop(requests);
        let source = Stub {
            metadata: true,
            ..Stub::default()
        };
        response(&source, "untrusted");
        assert!(resolve(&source).unwrap().is_none());
    }

    #[test]
    fn metadata_gate_reads_only_machine_identity_or_explicit_enable() {
        let source = Stub::default().vars(&[("GCE_METADATA_HOST", "not-a-target")]);
        assert!(native_metadata_allowed(&source));
        #[cfg(target_os = "linux")]
        {
            let mut source = Stub::default();
            source.files.insert(
                "/sys/class/dmi/id/product_name".into(),
                b"Google Compute Engine\n".to_vec(),
            );
            assert!(native_metadata_allowed(&source));
            source.files.insert(
                "/sys/class/dmi/id/product_name".into(),
                b"Other machine".to_vec(),
            );
            assert!(!native_metadata_allowed(&source));
        }
        #[cfg(not(windows))]
        assert!(!native_metadata_allowed(&Stub::default()));
    }

    #[test]
    fn tokens_refresh_five_minutes_early_and_gcloud_caches_45_minutes() {
        let mut source = file_source(&authorized_user());
        source.cache = true;
        response(&source, "cached-first");
        response(&source, "cached-second");
        assert_eq!(
            resolve(&source).unwrap().unwrap().value.as_str(),
            "cached-first"
        );
        source.clock.set(source.clock.get() + 3299);
        assert_eq!(
            resolve(&source).unwrap().unwrap().value.as_str(),
            "cached-first"
        );
        assert_eq!(source.requests.borrow().len(), 1);
        source.clock.set(source.clock.get() + 1);
        assert_eq!(
            resolve(&source).unwrap().unwrap().value.as_str(),
            "cached-second"
        );
        assert_eq!(source.requests.borrow().len(), 2);
        let source = Stub {
            cache: true,
            gcloud_path: Some("/fake/unique-cache-gcloud".into()),
            command_output: Some("cached-cli".into()),
            ..Stub::default()
        };
        source.clock.set(100);
        resolve(&source).unwrap();
        source.clock.set(2799);
        resolve(&source).unwrap();
        assert_eq!(source.called("run-gcloud"), 1);
        source.clock.set(2800);
        resolve(&source).unwrap();
        assert_eq!(source.called("run-gcloud"), 2);
    }

    #[test]
    fn failed_metadata_probe_is_remembered_for_the_process() {
        cache().lock().unwrap().metadata_failed = false;
        let source = Stub {
            cache: true,
            metadata: true,
            ..Stub::default()
        };
        assert!(resolve(&source).unwrap().is_none());
        let mut headers = super::super::http::Headers::new();
        headers.insert("metadata-flavor".into(), "Google".into());
        source.responses.borrow_mut().push_back((
            json!({"access_token":"must-not-retry","token_type":"Bearer","expires_in":3600}),
            headers,
        ));
        assert!(resolve(&source).unwrap().is_none());
        assert_eq!(source.requests.borrow().len(), 1);
        cache().lock().unwrap().metadata_failed = false;
    }
}
