//! AWS Signature Version 4 and the AWS credential sources Capturefab reads:
//! environment variables and the shared credentials and config files.
//! Amazon Bedrock signs its requests with it.
//!
//! Secrets stay in buffers that are cleared when dropped, are never logged or
//! serialized, and leave the process only as HMAC signatures. Capturefab does
//! not run SSO sign-ins, `credential_process` helpers or instance-metadata
//! lookups.
use anyhow::{Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};
use std::{borrow::Cow, ffi::OsString, fmt::Write as _, path::PathBuf};
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// Credentials.

/// An access key ID, its secret, and the session token of temporary
/// credentials. Deliberately no Debug, Clone or Serialize: the secret stays
/// within the code that signs with it and is cleared when the keys drop.
pub struct Keys {
    id: String,
    secret: Zeroizing<String>,
    token: Option<Zeroizing<String>>,
}

impl Keys {
    pub(crate) fn new(id: String, secret: String, token: Option<String>) -> Self {
        Self {
            id,
            secret: Zeroizing::new(secret),
            token: token.map(Zeroizing::new),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether these are temporary credentials with a session token.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }

    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref().map(String::as_str)
    }

    /// Refuse keys that cannot be signed with or sent in a header.
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 128
                && self.id.bytes().all(|b| b.is_ascii_graphic()),
            "AWS access key ID is empty or invalid"
        );
        ensure!(
            !self.secret.is_empty() && self.secret.len() <= 4096,
            "AWS secret access key must contain 1..4096 bytes"
        );
        if let Some(token) = &self.token {
            validate_session_token(token)?;
        }
        Ok(())
    }
}

/// A session token travels in the `x-amz-security-token` header.
pub(crate) fn validate_session_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty() && token.len() <= 16384 && token.bytes().all(|b| b.is_ascii_graphic()),
        "AWS session token must contain 1..16384 printable ASCII characters without whitespace"
    );
    Ok(())
}

/// Reads one environment variable: the process environment, or a fixed set
/// in tests, which must never change the environment other tests share.
pub(crate) type Vars<'a> = &'a dyn Fn(&str) -> Option<OsString>;

pub(crate) fn process_vars(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// A set, non-empty, UTF-8 variable.
fn var(vars: Vars<'_>, name: &str) -> Option<String> {
    vars(name)
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
}

/// The `[name]` profile of the AWS shared credentials file
/// (`AWS_SHARED_CREDENTIALS_FILE`, else `~/.aws/credentials`), with its
/// session token; the caller validates the selected credentials.
pub(crate) fn keys_from_profile_in(name: &str, vars: Vars<'_>) -> Result<Keys> {
    let path =
        credentials_file(vars).ok_or_else(|| anyhow!("cannot locate the AWS credentials file"))?;
    let text = Zeroizing::new(
        std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?,
    );
    profile_keys(&text, name).ok_or_else(|| {
        anyhow!(
            "profile [{name}] in {} has no aws_access_key_id and aws_secret_access_key",
            path.display()
        )
    })
}

/// Keys for Amazon Bedrock and a short description of where they came from,
/// in the AWS CLI's order: the chosen `profile`, else the environment's
/// keys, then `AWS_PROFILE`, then the `default` profile. Unlike S3
/// destinations, which name exactly one source, this falls back.
pub fn bedrock_chain(profile: Option<&str>) -> Result<(Keys, String)> {
    bedrock_chain_in(profile, &process_vars)
}

pub(crate) fn bedrock_chain_in(profile: Option<&str>, vars: Vars<'_>) -> Result<(Keys, String)> {
    // Like the AWS CLI's `--profile`, a chosen profile ignores keys in the
    // environment, which may belong to another account. AWS_PROFILE does
    // not: exported keys win over it, as in the CLI.
    if let Some(id) = var(vars, "AWS_ACCESS_KEY_ID").filter(|_| profile.is_none()) {
        let secret = var(vars, "AWS_SECRET_ACCESS_KEY").ok_or_else(|| {
            anyhow!(
                "AWS_ACCESS_KEY_ID is set without AWS_SECRET_ACCESS_KEY; set both, or unset AWS_ACCESS_KEY_ID to use an AWS profile"
            )
        })?;
        let keys = Keys::new(id, secret, var(vars, "AWS_SESSION_TOKEN"));
        keys.validate()?;
        return Ok((keys, "AWS_ACCESS_KEY_ID".into()));
    }
    let (name, source, chosen) = match (profile, var(vars, "AWS_PROFILE")) {
        (Some(name), _) => (name.to_owned(), format!("AWS profile {name}"), true),
        (None, Some(name)) => (
            name.clone(),
            format!("AWS profile {name} (AWS_PROFILE)"),
            true,
        ),
        (None, None) => (
            "default".to_owned(),
            "AWS profile default".to_owned(),
            false,
        ),
    };
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "AWS profile name must contain 1-64 ASCII letters, digits, dots, underscores or hyphens"
    );
    // In the AWS CLI's order, a profile set up for SSO signs in with SSO even
    // when it also has static keys, and credential_process runs only without
    // them. Capturefab can do neither, so it refuses rather than sign with
    // keys the CLI would not use. This is the user's configuration, not a
    // missing capability: the message avoids the words that would make the
    // CLI report it as unsupported, so it exits 1 (operation_failed).
    let refuse = || {
        anyhow!(
            "AWS profile {name} uses SSO or credential_process, which Capturefab cannot read; export AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY or use a Bedrock API key (AWS_BEARER_TOKEN_BEDROCK)."
        )
    };
    let settings = profile_settings(&name, vars);
    if settings.iter().any(|key| key.starts_with("sso_")) {
        return Err(refuse());
    }
    match keys_from_profile_in(&name, vars) {
        Ok(keys) => {
            keys.validate()?;
            Ok((keys, source))
        }
        Err(_) if settings.iter().any(|key| key == "credential_process") => Err(refuse()),
        // Nothing configured at all: say how to configure it.
        Err(_) if !chosen && !credentials_section_may_exist(&name, vars) => bail!(
            "no AWS credentials are configured; set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, choose an AWS profile, or use a Bedrock API key (AWS_BEARER_TOKEN_BEDROCK)"
        ),
        Err(error) => Err(error),
    }
}

/// The `region` of a profile in the AWS config file (`AWS_CONFIG_FILE`, else
/// `~/.aws/config`), if it is set to a plausible region name.
pub fn profile_region(profile: &str) -> Option<String> {
    profile_region_from_env(profile, process_vars)
}

/// Non-secret profile configuration lookup with an explicit environment. This
/// reads only the config file and never resolves keys or credential processes.
pub(crate) fn profile_region_from_env(
    profile: &str,
    vars: impl Fn(&str) -> Option<OsString>,
) -> Option<String> {
    profile_region_in(profile, &vars)
}

fn profile_region_in(profile: &str, vars: Vars<'_>) -> Option<String> {
    let text = std::fs::read_to_string(config_file(vars)?).ok()?;
    let region = ini_values(&text, |section| config_section_is(section, profile))
        .into_iter()
        .rev()
        .find(|(key, _)| *key == "region")?
        .1;
    // It becomes part of a host name and of the signed scope.
    (!region.is_empty()
        && region.len() <= 64
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then(|| region.to_owned())
}

fn home_file(vars: Vars<'_>, override_var: &str, name: &str) -> Option<PathBuf> {
    if let Some(path) = vars(override_var) {
        return Some(path.into());
    }
    let home = vars(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".aws").join(name))
}

fn credentials_file(vars: Vars<'_>) -> Option<PathBuf> {
    home_file(vars, "AWS_SHARED_CREDENTIALS_FILE", "credentials")
}

fn config_file(vars: Vars<'_>) -> Option<PathBuf> {
    home_file(vars, "AWS_CONFIG_FILE", "config")
}

/// The name in an INI section header line such as `[profile lab]`.
fn section_header(line: &str) -> Option<&str> {
    let line = line.trim();
    (line.starts_with('[') && line.ends_with(']')).then(|| line[1..line.len() - 1].trim())
}

/// The `key = value` lines of the INI sections `matches` accepts, in file
/// order. Lines starting with `#` or `;` are comments.
fn ini_values(text: &str, matches: impl Fn(&str) -> bool) -> Vec<(&str, &str)> {
    let mut inside = false;
    let mut values = Vec::new();
    for line in text.lines() {
        if let Some(section) = section_header(line) {
            inside = matches(section);
            continue;
        }
        let line = line.trim();
        if !inside || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.push((key.trim(), value.trim()));
        }
    }
    values
}

/// Config files name profiles `[profile NAME]`; the default profile may
/// also be plain `[default]`.
fn config_section_is(section: &str, profile: &str) -> bool {
    (profile == "default" && section == "default")
        || section
            .strip_prefix("profile")
            .is_some_and(|rest| rest.starts_with(char::is_whitespace) && rest.trim() == profile)
}

/// The `[profile]` section of an AWS shared credentials file.
fn profile_keys(text: &str, profile: &str) -> Option<Keys> {
    let (mut id, mut secret, mut token) = (None, None, None);
    for (key, value) in ini_values(text, |section| section == profile) {
        match key {
            "aws_access_key_id" => id = Some(value),
            "aws_secret_access_key" => secret = Some(value),
            "aws_session_token" => token = Some(value),
            _ => {}
        }
    }
    Some(Keys::new(
        id?.to_owned(),
        secret?.to_owned(),
        token.map(str::to_owned),
    ))
}

/// Setting names of a profile in the config and credentials files.
fn profile_settings(profile: &str, vars: Vars<'_>) -> Vec<String> {
    let read = |path: Option<PathBuf>| {
        path.and_then(|path| std::fs::read_to_string(path).ok())
            .map(Zeroizing::new)
    };
    let mut names = Vec::new();
    if let Some(text) = read(config_file(vars)) {
        names.extend(
            ini_values(&text, |section| config_section_is(section, profile))
                .into_iter()
                .map(|(key, _)| key.to_owned()),
        );
    }
    if let Some(text) = read(credentials_file(vars)) {
        names.extend(
            ini_values(&text, |section| section == profile)
                .into_iter()
                .map(|(key, _)| key.to_owned()),
        );
    }
    names
}

/// False only when the credentials file cannot be located, does not exist,
/// or has no `[profile]` section: then nothing at all is configured. A file
/// that exists but cannot be read (permissions, not UTF-8) may hold the
/// profile, and its read error is the one to report.
fn credentials_section_may_exist(profile: &str, vars: Vars<'_>) -> bool {
    let Some(path) = credentials_file(vars) else {
        return false;
    };
    match std::fs::read_to_string(path).map(Zeroizing::new) {
        Ok(text) => text
            .lines()
            .any(|line| section_header(line) == Some(profile)),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

// ---------------------------------------------------------------------------
// Signature Version 4.

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// Percent-encode for SigV4: everything except the RFC 3986 unreserved
/// characters, and `/` too unless it separates path segments.
pub fn uri_encode(segment: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (b == b'/' && !encode_slash) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// How the request path appears in the canonical request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UriMode {
    /// Amazon S3 signs the encoded path exactly as sent.
    S3,
    /// Every other service encodes each segment of the encoded path once
    /// more, so `%3A` in a Bedrock model ID is signed as `%253A`. The path
    /// must already be normalized (no `.`, `..` or empty segments).
    Standard,
}

/// A request to sign: method, host, path (already encoded with
/// `uri_encode`), unencoded query pairs and the headers to include besides
/// host and the x-amz ones.
pub struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub headers: &'a [(&'a str, String)],
    /// Lowercase hex SHA-256 of the body.
    pub payload_sha256: &'a str,
}

/// The Authorization header for `request` and the headers that must
/// accompany it: `x-amz-content-sha256`, `x-amz-date` and, with temporary
/// credentials, `x-amz-security-token`. `amz_date` is `YYYYMMDDTHHMMSSZ`.
pub fn sign(
    keys: &Keys,
    region: &str,
    service: &str,
    mode: UriMode,
    amz_date: &str,
    request: &Request,
) -> (String, Vec<(String, String)>) {
    let (canonical, signed, extra) = canonical_request(keys, mode, amz_date, request);
    let (scope, signature) = signature(&keys.secret, amz_date, region, service, &canonical);
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        keys.id
    );
    (authorization, extra)
}

/// The canonical request, its signed header names, and the x-amz headers to
/// send with it.
fn canonical_request(
    keys: &Keys,
    mode: UriMode,
    amz_date: &str,
    request: &Request,
) -> (String, String, Vec<(String, String)>) {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.push(("host".into(), request.host.into()));
    headers.push(("x-amz-content-sha256".into(), request.payload_sha256.into()));
    headers.push(("x-amz-date".into(), amz_date.into()));
    if let Some(token) = &keys.token {
        headers.push(("x-amz-security-token".into(), token.as_str().into()));
    }
    headers.sort();
    let mut query: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let signed = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canonical = format!(
        "{}\n{}\n{canonical_query}\n{canonical_headers}\n{signed}\n{}",
        request.method,
        canonical_uri(request.path, mode),
        request.payload_sha256
    );
    let extra = headers
        .into_iter()
        .filter(|(k, _)| k.starts_with("x-amz-"))
        .collect();
    (canonical, signed, extra)
}

fn canonical_uri(path: &str, mode: UriMode) -> Cow<'_, str> {
    match mode {
        UriMode::S3 => path.into(),
        UriMode::Standard if path.is_empty() => "/".into(),
        UriMode::Standard => path
            .split('/')
            .map(|segment| uri_encode(segment, true))
            .collect::<Vec<_>>()
            .join("/")
            .into(),
    }
}

/// The credential scope and the signature of a canonical request.
fn signature(
    secret: &str,
    amz_date: &str,
    region: &str,
    service: &str,
    canonical: &str,
) -> (String, String) {
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let key = [region, service, "aws4_request"].iter().fold(
        hmac(
            Zeroizing::new(format!("AWS4{secret}")).as_bytes(),
            date.as_bytes(),
        ),
        |key, part| hmac(&key, part.as_bytes()),
    );
    (scope, hex(&hmac(&key, to_sign.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, ffi::OsStr};

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    /// The example keys of the AWS Signature Version 4 documentation.
    const DOC_ID: &str = "AKIDEXAMPLE";
    const DOC_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn doc_keys(token: Option<&str>) -> Keys {
        Keys::new(DOC_ID.into(), DOC_SECRET.into(), token.map(str::to_owned))
    }

    /// A fixed environment: every variable not listed is unset, so lookups
    /// never fall back to the real home directory.
    fn vars(pairs: &[(&str, &dyn AsRef<OsStr>)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_ref().to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    /// A private scratch directory for one test's AWS files.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-aws-{}-{test}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn error_text<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => format!("{error:#}"),
        }
    }

    /// The GET Object example from the AWS Signature Version 4 documentation
    /// for Amazon S3 ("Signature Calculations for the Authorization Header").
    #[test]
    fn signature_matches_the_aws_documentation_example() {
        let keys = Keys::new(
            "AKIAIOSFODNN7EXAMPLE".into(),
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            None,
        );
        let (authorization, extra) = sign(
            &keys,
            "us-east-1",
            "s3",
            UriMode::S3,
            "20130524T000000Z",
            &Request {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: &[],
                headers: &[("Range", "bytes=0-9".into())],
                payload_sha256: EMPTY_SHA256,
            },
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(
            extra
                .iter()
                .any(|(k, v)| k == "x-amz-date" && v == "20130524T000000Z")
        );
    }

    /// The IAM ListUsers example from the AWS Signature Version 4
    /// documentation ("Create a canonical request" through "Calculate the
    /// signature"): the service names the scope and the signing key. Its
    /// canonical request does not sign x-amz-content-sha256, which `sign`
    /// always adds, so it starts from the documented canonical request.
    #[test]
    fn signature_matches_the_aws_iam_documentation_example() {
        let canonical = format!(
            "GET\n/\nAction=ListUsers&Version=2010-05-08\ncontent-type:application/x-www-form-urlencoded; charset=utf-8\nhost:iam.amazonaws.com\nx-amz-date:20150830T123600Z\n\ncontent-type;host;x-amz-date\n{EMPTY_SHA256}"
        );
        assert_eq!(
            hex(&Sha256::digest(canonical.as_bytes())),
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        assert_eq!(
            signature(
                DOC_SECRET,
                "20150830T123600Z",
                "us-east-1",
                "iam",
                &canonical
            ),
            (
                "20150830/us-east-1/iam/aws4_request".into(),
                "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7".into()
            )
        );
    }

    /// A Bedrock Converse request with a versioned model ID. The expected
    /// signatures come from an independent implementation written from the
    /// SigV4 specification (Python hashlib/hmac), which reproduces both AWS
    /// documentation examples above; the canonical requests are spelled out
    /// so a change to either side shows where they differ.
    #[test]
    fn bedrock_requests_sign_the_double_encoded_path() {
        let body_sha256 = hex(&Sha256::digest(br#"{"messages":[]}"#));
        assert_eq!(
            body_sha256,
            "5e4ce7b36ba37b78a5d5f9fd08e6b7b54ba6879d651aa46ec9e1d6fa24ebe30a"
        );
        let path = format!(
            "/model/{}/converse",
            uri_encode("anthropic.claude-3-5-sonnet-20240620-v1:0", true)
        );
        let headers = [("Content-Type", "application/json".to_string())];
        let request = Request {
            method: "POST",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: &path,
            query: &[],
            headers: &headers,
            payload_sha256: &body_sha256,
        };
        for (token, signed, signature) in [
            (
                None,
                "content-type;host;x-amz-content-sha256;x-amz-date",
                "4a0ddfbaa8b3f8f2f0d6711aa25d3294169d4785803661014eb4d6d860ae9c46",
            ),
            (
                Some("session/token+value="),
                "content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token",
                "79c53168f66bd8a0e7137e73a628ec525eda9b65cc42b9dc529212d8ccfd69fd",
            ),
        ] {
            let keys = doc_keys(token);
            let token_line = token
                .map(|token| format!("x-amz-security-token:{token}\n"))
                .unwrap_or_default();
            let (canonical, signed_headers, _) =
                canonical_request(&keys, UriMode::Standard, "20150830T123600Z", &request);
            assert_eq!(
                canonical,
                format!(
                    "POST\n/model/anthropic.claude-3-5-sonnet-20240620-v1%253A0/converse\n\ncontent-type:application/json\nhost:bedrock-runtime.us-east-1.amazonaws.com\nx-amz-content-sha256:{body_sha256}\nx-amz-date:20150830T123600Z\n{token_line}\n{signed}\n{body_sha256}"
                )
            );
            assert_eq!(signed_headers, signed);
            let (authorization, extra) = sign(
                &keys,
                "us-east-1",
                "bedrock",
                UriMode::Standard,
                "20150830T123600Z",
                &request,
            );
            assert_eq!(
                authorization,
                format!(
                    "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders={signed}, Signature={signature}"
                )
            );
            let names: Vec<&str> = extra.iter().map(|(k, _)| k.as_str()).collect();
            let mut expected = vec!["x-amz-content-sha256", "x-amz-date"];
            if token.is_some() {
                expected.push("x-amz-security-token");
            }
            assert_eq!(names, expected);
            assert_eq!(keys.has_token(), token.is_some());
        }
    }

    #[test]
    fn bedrock_model_ids_are_encoded_in_the_path_and_again_when_signed() {
        for (model, path, canonical) in [
            (
                "anthropic.claude-3-5-sonnet-20240620-v1:0",
                "/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse",
                "/model/anthropic.claude-3-5-sonnet-20240620-v1%253A0/converse",
            ),
            (
                "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc",
                "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Aapplication-inference-profile%2Fabc/converse",
                "/model/arn%253Aaws%253Abedrock%253Aus-east-1%253A123456789012%253Aapplication-inference-profile%252Fabc/converse",
            ),
        ] {
            let encoded = format!("/model/{}/converse", uri_encode(model, true));
            assert_eq!(encoded, path);
            assert_eq!(canonical_uri(&encoded, UriMode::Standard), canonical);
            // S3 signs the path as sent.
            assert_eq!(canonical_uri(&encoded, UriMode::S3), path);
        }
        assert_eq!(canonical_uri("", UriMode::Standard), "/");
        assert_eq!(canonical_uri("/", UriMode::Standard), "/");
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block size is hashed first.
        assert_eq!(
            hex(&hmac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn uri_encode_keeps_only_unreserved_characters() {
        assert_eq!(uri_encode("run 1/a+b.png", false), "run%201/a%2Bb.png");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("A-z_0.9~", true), "A-z_0.9~");
        assert_eq!(uri_encode("é:%", true), "%C3%A9%3A%25");
    }

    #[test]
    fn profiles_parse_from_the_shared_credentials_file() {
        let file = "[default]\naws_access_key_id = A1\naws_secret_access_key=S1\n\n# comment\n[lab]\naws_access_key_id=A2\naws_secret_access_key = S2\naws_session_token = T2\n[empty]\n";
        let lab = profile_keys(file, "lab").unwrap();
        assert_eq!(
            (lab.id(), lab.secret(), lab.token()),
            ("A2", "S2", Some("T2"))
        );
        assert_eq!(profile_keys(file, "default").unwrap().id(), "A1");
        assert!(profile_keys(file, "empty").is_none());
        assert!(profile_keys(file, "missing").is_none());

        let dir = scratch("profiles");
        let credentials = dir.join("credentials");
        std::fs::write(&credentials, file).unwrap();
        let env = vars(&[
            ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
            ("HOME", &"/nonexistent"),
        ]);
        let keys = keys_from_profile_in("lab", &env).unwrap();
        assert_eq!((keys.id(), keys.has_token()), ("A2", true));
        assert!(
            error_text(keys_from_profile_in("empty", &env)).contains(&format!(
                "profile [empty] in {} has no aws_access_key_id and aws_secret_access_key",
                credentials.display()
            ))
        );
        // Without the override, the home directory's file is read.
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(
            home.join(".aws").join("credentials"),
            "[lab]\naws_access_key_id=H\naws_secret_access_key=HS\n",
        )
        .unwrap();
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        assert_eq!(
            keys_from_profile_in("lab", &vars(&[(home_var, &home)]))
                .unwrap()
                .id(),
            "H"
        );
        assert!(
            error_text(keys_from_profile_in("lab", &vars(&[])))
                .contains("cannot locate the AWS credentials file")
        );
        let missing = dir.join("missing");
        assert!(
            error_text(keys_from_profile_in(
                "lab",
                &vars(&[("AWS_SHARED_CREDENTIALS_FILE", &missing)])
            ))
            .starts_with(&format!("cannot read {}", missing.display()))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bedrock_chain_tries_chosen_profile_environment_aws_profile_then_default() {
        let dir = scratch("chain");
        let credentials = dir.join("credentials");
        std::fs::write(
            &credentials,
            "[default]\naws_access_key_id=DEFAULT\naws_secret_access_key=d\n[lab]\naws_access_key_id=LAB\naws_secret_access_key=l\n[ops]\naws_access_key_id=OPS\naws_secret_access_key=o\naws_session_token=t\n",
        )
        .unwrap();
        let config = dir.join("config");
        std::fs::write(&config, "").unwrap();
        let chain = |profile: Option<&str>, extra: &[(&str, &dyn AsRef<OsStr>)]| {
            let mut pairs: Vec<(&str, &dyn AsRef<OsStr>)> = vec![
                ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
                ("AWS_CONFIG_FILE", &config),
            ];
            pairs.extend_from_slice(extra);
            bedrock_chain_in(profile, &vars(&pairs))
                .map(|(keys, source)| (keys.id().to_owned(), keys.has_token(), source))
        };
        let environment: [(&str, &dyn AsRef<OsStr>); 3] = [
            ("AWS_ACCESS_KEY_ID", &"ENV"),
            ("AWS_SECRET_ACCESS_KEY", &"e"),
            ("AWS_PROFILE", &"ops"),
        ];
        // A chosen profile ignores exported keys, which may belong to another
        // account, as the AWS CLI's --profile does; AWS_PROFILE loses to them.
        assert_eq!(
            chain(Some("lab"), &environment).unwrap(),
            ("LAB".into(), false, "AWS profile lab".into())
        );
        assert_eq!(
            chain(None, &environment).unwrap(),
            ("ENV".into(), false, "AWS_ACCESS_KEY_ID".into())
        );
        assert_eq!(
            chain(
                Some("lab"),
                &[("AWS_ACCESS_KEY_ID", &"ENV"), ("AWS_SESSION_TOKEN", &"t")]
            )
            .unwrap(),
            ("LAB".into(), false, "AWS profile lab".into())
        );
        assert_eq!(
            chain(Some("lab"), &[("AWS_PROFILE", &"ops")]).unwrap(),
            ("LAB".into(), false, "AWS profile lab".into())
        );
        assert_eq!(
            chain(None, &[("AWS_PROFILE", &"ops")]).unwrap(),
            ("OPS".into(), true, "AWS profile ops (AWS_PROFILE)".into())
        );
        assert_eq!(
            chain(None, &[]).unwrap(),
            ("DEFAULT".into(), false, "AWS profile default".into())
        );
        assert!(
            error_text(chain(None, &[("AWS_ACCESS_KEY_ID", &"ENV")]))
                .contains("AWS_ACCESS_KEY_ID is set without AWS_SECRET_ACCESS_KEY")
        );
        // A chosen profile that does not exist is an error, not a fallback.
        assert!(error_text(chain(Some("missing"), &[])).contains("profile [missing] in"));
        assert!(error_text(chain(Some("missing"), &environment)).contains("profile [missing] in"));
        assert!(
            error_text(chain(None, &[("AWS_PROFILE", &"bad name")]))
                .contains("AWS profile name must contain")
        );
        // Keys are checked before they are signed with.
        assert!(
            error_text(chain(
                None,
                &[
                    ("AWS_ACCESS_KEY_ID", &"bad\tid"),
                    ("AWS_SECRET_ACCESS_KEY", &"e")
                ]
            ))
            .contains("AWS access key ID is empty or invalid")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bedrock_chain_refuses_sso_and_credential_process_profiles() {
        let dir = scratch("sso");
        let credentials = dir.join("credentials");
        let config = dir.join("config");
        std::fs::write(
            &credentials,
            "[stale]\naws_access_key_id=OLD\naws_secret_access_key=o\n[keys]\naws_access_key_id=KEYS\naws_secret_access_key=k\n[helper]\ncredential_process = /usr/bin/true\n",
        )
        .unwrap();
        std::fs::write(
            &config,
            "[profile stale]\nsso_session = corp\nsso_account_id = 123456789012\n[profile sso]\nsso_start_url = https://corp.awsapps.com/start\n[default]\nsso_session = corp\n[profile keys]\ncredential_process = /usr/bin/true\n[sso-session corp]\nsso_region = us-east-1\n",
        )
        .unwrap();
        let env = vars(&[
            ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
            ("AWS_CONFIG_FILE", &config),
        ]);
        for profile in ["sso", "stale", "helper", "default"] {
            let message = error_text(bedrock_chain_in(Some(profile), &env));
            assert_eq!(
                message,
                format!(
                    "AWS profile {profile} uses SSO or credential_process, which Capturefab cannot read; export AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY or use a Bedrock API key (AWS_BEARER_TOKEN_BEDROCK)."
                )
            );
            // A configuration problem, not a missing capability.
            assert!(!message.contains("unsupported") && !message.contains("not found"));
        }
        assert!(error_text(bedrock_chain_in(None, &env)).contains("AWS profile default uses SSO"));
        // Static keys come before credential_process, as in the AWS CLI.
        assert_eq!(bedrock_chain_in(Some("keys"), &env).unwrap().0.id(), "KEYS");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bedrock_chain_explains_missing_credentials() {
        let dir = scratch("none");
        let credentials = dir.join("credentials");
        let default_chain = || {
            error_text(bedrock_chain_in(
                None,
                &vars(&[
                    ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
                    ("AWS_CONFIG_FILE", &dir.join("config")),
                ]),
            ))
        };
        let nothing_configured = |message: &str| {
            assert!(
                message.starts_with("no AWS credentials are configured;")
                    && message.contains("AWS_BEARER_TOKEN_BEDROCK"),
                "{message}"
            );
            assert!(!message.contains("not found") && !message.contains("unsupported"));
        };
        // No credentials file, or one without a [default] profile.
        nothing_configured(&default_chain());
        std::fs::write(
            &credentials,
            "[lab]\naws_access_key_id=LAB\naws_secret_access_key=l\n",
        )
        .unwrap();
        nothing_configured(&default_chain());
        assert!(
            error_text(bedrock_chain_in(
                None,
                &vars(&[("HOME", &"/nonexistent"), ("USERPROFILE", &"/nonexistent")])
            ))
            .starts_with("no AWS credentials are configured;")
        );
        // A [default] profile without keys says what it lacks.
        std::fs::write(&credentials, "[default]\nregion = us-east-1\n").unwrap();
        assert!(
            default_chain().starts_with("profile [default] in"),
            "{}",
            default_chain()
        );
        // A file that exists but cannot be read may hold the keys: its read
        // error is the problem to fix, not missing configuration.
        let cannot_read = format!("cannot read {}: ", credentials.display());
        std::fs::write(&credentials, b"[default]\naws_access_key_id=\xff\n").unwrap();
        assert!(
            default_chain().starts_with(&cannot_read),
            "{}",
            default_chain()
        );
        std::fs::remove_file(&credentials).unwrap();
        std::fs::create_dir(&credentials).unwrap();
        assert!(
            default_chain().starts_with(&cannot_read),
            "{}",
            default_chain()
        );
        std::fs::remove_dir(&credentials).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                &credentials,
                "[default]\naws_access_key_id=A\naws_secret_access_key=S\n",
            )
            .unwrap();
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads it anyway; the check applies only where it fails.
            if std::fs::read(&credentials).is_err() {
                let message = default_chain();
                assert!(
                    message.starts_with(&cannot_read) && message.contains("ermission denied"),
                    "{message}"
                );
            }
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn profile_regions_come_from_the_config_file() {
        let dir = scratch("region");
        let config = dir.join("config");
        std::fs::write(
            &config,
            "[default]\nregion = us-west-2\n[profile lab]\n# region = eu-west-1\nregion=eu-central-1\n[profile  spaced ]\nregion = ap-south-1\n[profile bad]\nregion = us-east-1.evil.example/\n[lab]\nregion = wrong-section\n[profilex]\nregion = wrong-section\n",
        )
        .unwrap();
        let env = vars(&[("AWS_CONFIG_FILE", &config)]);
        assert_eq!(
            profile_region_in("default", &env).as_deref(),
            Some("us-west-2")
        );
        assert_eq!(
            profile_region_in("lab", &env).as_deref(),
            Some("eu-central-1")
        );
        assert_eq!(
            profile_region_in("spaced", &env).as_deref(),
            Some("ap-south-1")
        );
        assert_eq!(profile_region_in("bad", &env), None);
        assert_eq!(profile_region_in("x", &env), None);
        assert_eq!(profile_region_in("missing", &env), None);
        std::fs::write(&config, "[profile default]\nregion = sa-east-1\n").unwrap();
        assert_eq!(
            profile_region_in("default", &env).as_deref(),
            Some("sa-east-1")
        );
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(
            home.join(".aws").join("config"),
            "[profile lab]\nregion = ca-central-1\n",
        )
        .unwrap();
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        assert_eq!(
            profile_region_in("lab", &vars(&[(home_var, &home)])).as_deref(),
            Some("ca-central-1")
        );
        assert_eq!(profile_region_in("lab", &vars(&[])), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
