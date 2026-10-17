//! A small S3-compatible client for uploading finished captures: AWS Signature
//! Version 4 over a pure-Rust HTTPS stack that trusts the OS certificate
//! store. Payloads are signed with their SHA-256, so the service rejects bytes
//! that changed in transit, and writes are conditional (`If-None-Match: *`),
//! so an existing object is never replaced. Services that ignore conditional
//! writes (found with `probe_conditional_writes`) use a HEAD check first.
//!
//! Credentials come from the OS credential store, an AWS shared-credentials
//! profile, or the environment; secrets are never written to Capturefab files
//! or sent over the wire.
use crate::destination::{Bucket, Credentials, NoOverwrite};
use anyhow::{Context, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

/// Keychain service under which secret keys are stored, by access key ID.
const KEYCHAIN_SERVICE: &str = "capturefab-s3";
/// Files above this size use multipart upload, in parts of `PART_BYTES`.
const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024;
const PART_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RESPONSE: u64 = 1024 * 1024;

pub use crate::transfer::{Permanent, retryable};

// ---------------------------------------------------------------------------
// Credentials.

pub struct Keys {
    pub id: String,
    secret: String,
    token: Option<String>,
}

/// Store a secret access key in the OS credential store.
pub fn store_secret(access_key_id: &str, secret: &str) -> Result<()> {
    Credentials::Keychain {
        access_key_id: access_key_id.into(),
    }
    .validate()?;
    validate_secret(secret)?;
    crate::transfer::store_secret(KEYCHAIN_SERVICE, access_key_id, secret)
}

pub fn delete_secret(access_key_id: &str) -> Result<()> {
    Credentials::Keychain {
        access_key_id: access_key_id.into(),
    }
    .validate()?;
    crate::transfer::delete_secret(KEYCHAIN_SERVICE, access_key_id)
}

/// Whether the OS credential store holds a secret for this access key ID.
pub fn has_secret(access_key_id: &str) -> bool {
    crate::transfer::has_secret(KEYCHAIN_SERVICE, access_key_id)
}

/// Read a key for an in-memory undo transaction. Missing keys differ from a
/// locked or unavailable credential store: a save must never mistake an
/// unreadable existing key for an empty slot and then overwrite it.
pub(crate) fn read_secret(access_key_id: &str) -> Result<Option<String>> {
    Credentials::Keychain {
        access_key_id: access_key_id.into(),
    }
    .validate()?;
    crate::transfer::read_secret(KEYCHAIN_SERVICE, access_key_id)
}

/// The `[profile]` section of an AWS shared credentials file.
fn profile_keys(text: &str, profile: &str) -> Option<Keys> {
    let mut section = String::new();
    let (mut id, mut secret, mut token) = (None, None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        if section != profile || line.starts_with(['#', ';']) {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "aws_access_key_id" => id = Some(value),
            "aws_secret_access_key" => secret = Some(value),
            "aws_session_token" => token = Some(value),
            _ => {}
        }
    }
    Some(Keys {
        id: id?,
        secret: secret?,
        token,
    })
}

fn credentials_file() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE") {
        return Some(path.into());
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(
        std::path::PathBuf::from(home)
            .join(".aws")
            .join("credentials"),
    )
}

pub fn validate_secret(secret: &str) -> Result<()> {
    ensure!(
        !secret.is_empty() && secret.len() <= 4096,
        "S3 secret access key must contain 1..4096 bytes"
    );
    Ok(())
}

impl Keys {
    fn validate(&self) -> Result<()> {
        Credentials::Keychain {
            access_key_id: self.id.clone(),
        }
        .validate()?;
        validate_secret(&self.secret)?;
        if let Some(token) = &self.token {
            ensure!(
                !token.is_empty()
                    && token.len() <= 16384
                    && token.bytes().all(|b| b.is_ascii_graphic()),
                "AWS session token must contain 1..16384 printable ASCII characters without whitespace"
            );
        }
        Ok(())
    }
}

pub fn keys(credentials: &Credentials) -> Result<Keys> {
    credentials.validate()?;
    let keys: Result<Keys> = match credentials {
        Credentials::Keychain { access_key_id } => {
            let secret = keyring::Entry::new(KEYCHAIN_SERVICE, access_key_id)
                .and_then(|entry| entry.get_password())
                .map_err(|e| {
                    Permanent(format!(
                        "no secret key for {access_key_id} in the OS credential store ({e}); enter it again in the destination settings or `capturefab destination add-s3 --secret-stdin`"
                    ))
                })?;
            Ok(Keys {
                id: access_key_id.clone(),
                secret,
                token: None,
            })
        }
        Credentials::Profile { name } => {
            let path = credentials_file()
                .ok_or_else(|| Permanent("cannot locate the AWS credentials file".into()))?;
            let text = std::fs::read_to_string(&path)
                .map_err(|e| Permanent(format!("cannot read {}: {e}", path.display())))?;
            Ok(profile_keys(&text, name).ok_or_else(|| {
                Permanent(format!(
                    "profile [{name}] in {} has no aws_access_key_id and aws_secret_access_key",
                    path.display()
                ))
            })?)
        }
        Credentials::Environment => {
            let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
            match (get("AWS_ACCESS_KEY_ID"), get("AWS_SECRET_ACCESS_KEY")) {
                (Some(id), Some(secret)) => Ok(Keys {
                    id,
                    secret,
                    token: get("AWS_SESSION_TOKEN"),
                }),
                _ => Err(Permanent(
                    "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not set in the uploading process"
                        .into(),
                )
                .into()),
            }
        }
    };
    let keys = keys?;
    keys.validate()
        .map_err(|error| Permanent(error.to_string()))?;
    Ok(keys)
}

// ---------------------------------------------------------------------------
// Signature Version 4.

fn hex(bytes: &[u8]) -> String {
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

/// Percent-encode for SigV4: everything except unreserved characters, and
/// `/` too unless it separates path segments.
fn encode(text: &str, slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (b == b'/' && !slash) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A request to sign: method, host, path (already encoded), query pairs and
/// the headers to include besides host and the x-amz ones.
struct Request<'a> {
    method: &'a str,
    host: &'a str,
    path: &'a str,
    query: &'a [(&'a str, &'a str)],
    headers: &'a [(&'a str, String)],
    payload_sha256: &'a str,
}

/// The Authorization header and the headers that must accompany it.
fn sign(
    keys: &Keys,
    region: &str,
    amz_date: &str,
    request: &Request,
) -> (String, Vec<(String, String)>) {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.push(("host".into(), request.host.into()));
    headers.push(("x-amz-content-sha256".into(), request.payload_sha256.into()));
    headers.push(("x-amz-date".into(), amz_date.into()));
    if let Some(token) = &keys.token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort();
    let mut query: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(k, v)| (encode(k, true), encode(v, true)))
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
        request.method, request.path, request.payload_sha256
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let key = [region, "s3", "aws4_request"].iter().fold(
        hmac(format!("AWS4{}", keys.secret).as_bytes(), date.as_bytes()),
        |key, part| hmac(&key, part.as_bytes()),
    );
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        keys.id
    );
    let extra = headers
        .into_iter()
        .filter(|(k, _)| k.starts_with("x-amz-"))
        .collect();
    (authorization, extra)
}

// ---------------------------------------------------------------------------
// Client.

pub struct Client {
    bucket: Bucket,
    keys: Keys,
    agent: ureq::Agent,
    /// Scheme and authority, for example `https://s3.us-east-1.amazonaws.com`.
    origin: String,
    host: String,
}

struct Response {
    status: u16,
    etag: Option<String>,
    body: String,
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pgm") => "image/x-portable-graymap",
        Some("ppm") => "image/x-portable-pixmap",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("ts") => "video/mp2t",
        _ => "application/octet-stream",
    }
}

/// The text of an S3 XML error's Code and Message.
fn error_text(body: &str) -> String {
    let doc = roxmltree::Document::parse(body).ok();
    let field = |name: &str| {
        doc.as_ref()
            .and_then(|d| d.descendants().find(|n| n.has_tag_name(name)))
            .and_then(|n| n.text())
            .map(str::to_string)
    };
    match (field("Code"), field("Message")) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        _ => body.chars().take(200).collect(),
    }
}

impl Client {
    pub fn new(bucket: &Bucket) -> Result<Self> {
        bucket.validate()?;
        let keys = keys(&bucket.credentials)?;
        Self::with_keys(bucket, keys)
    }

    /// Check a typed credential without changing the shared credential store.
    pub(crate) fn with_secret(bucket: &Bucket, secret: String) -> Result<Self> {
        let Credentials::Keychain { access_key_id } = &bucket.credentials else {
            bail!("a typed secret requires an access key ID");
        };
        bucket.validate()?;
        validate_secret(&secret)?;
        Self::with_keys(
            bucket,
            Keys {
                id: access_key_id.clone(),
                secret,
                token: None,
            },
        )
    }

    fn with_keys(bucket: &Bucket, keys: Keys) -> Result<Self> {
        bucket.validate()?;
        keys.validate()?;
        let service = bucket.service_url();
        let (scheme, authority) = service
            .split_once("://")
            .ok_or_else(|| anyhow!("invalid S3 endpoint"))?;
        let authority = authority.split('/').next().unwrap_or(authority);
        ensure!(!authority.is_empty(), "invalid S3 endpoint");
        let host = if bucket.path_style {
            authority.to_string()
        } else {
            format!("{}.{authority}", bucket.bucket)
        };
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(600)))
            .max_redirects(0)
            .tls_config(tls)
            .build()
            .new_agent();
        Ok(Self {
            bucket: bucket.clone(),
            keys,
            agent,
            origin: format!("{}://{host}", scheme.to_ascii_lowercase()),
            host,
        })
    }

    /// The encoded request path for a key (empty for the bucket itself).
    fn path(&self, key: &str) -> String {
        let key = encode(key, false);
        match (self.bucket.path_style, key.is_empty()) {
            (true, true) => format!("/{}", self.bucket.bucket),
            (true, false) => format!("/{}/{key}", self.bucket.bucket),
            (false, _) => format!("/{key}"),
        }
    }

    fn send(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, String)],
        payload_sha256: &str,
        body: Option<&mut dyn Read>,
    ) -> Result<Response> {
        let path = self.path(key);
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let (authorization, signed) = sign(
            &self.keys,
            &self.bucket.region,
            &amz_date,
            &Request {
                method,
                host: &self.host,
                path: &path,
                query,
                headers,
                payload_sha256,
            },
        );
        let mut url = format!("{}{path}", self.origin);
        if !query.is_empty() {
            url.push('?');
            url.push_str(
                &query
                    .iter()
                    .map(|(k, v)| format!("{}={}", encode(k, true), encode(v, true)))
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
        let mut builder = ureq::http::Request::builder()
            .method(method)
            .uri(&url)
            .header("authorization", authorization);
        for (k, v) in headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .chain(signed)
        {
            builder = builder.header(k, v);
        }
        let result = match body {
            Some(reader) => self
                .agent
                .run(builder.body(ureq::SendBody::from_reader(reader))?),
            None => self.agent.run(builder.body(())?),
        };
        let mut response = result.with_context(|| {
            format!(
                "S3 {method} to {} failed",
                crate::media::redact_url(&self.origin)
            )
        })?;
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE)
            .read_to_string()
            .unwrap_or_default();
        Ok(Response { status, etag, body })
    }

    /// Classify a non-success response.
    fn failure(&self, what: &str, response: &Response) -> anyhow::Error {
        let detail = error_text(&response.body);
        let message = format!(
            "{what}: HTTP {} {}",
            response.status,
            if detail.is_empty() {
                "(no details)"
            } else {
                &detail
            }
        );
        match response.status {
            412 => Permanent(format!(
                "{what}: an object with this key already exists in s3://{}; it was not replaced",
                self.bucket.bucket
            ))
            .into(),
            408 | 429 | 500..=599 => anyhow!(message),
            // Access, signature, missing bucket and request errors need a
            // configuration change, not a retry.
            // Also errors reported inside a 200 (multipart completion).
            _ if detail.starts_with("RequestTimeout")
                || detail.starts_with("SlowDown")
                || detail.starts_with("InternalError") =>
            {
                anyhow!(message)
            }
            _ => Permanent(message).into(),
        }
    }

    /// A refused conditional write means the service does not implement
    /// `If-None-Match` at all, rather than that the key exists.
    fn conditional_unsupported(response: &Response) -> bool {
        response.status == 501
            || (response.status == 400 && error_text(&response.body).starts_with("NotImplemented"))
    }

    /// Classify a failed write; without conditional-write support, say how to
    /// switch to check-first uploads instead of retrying forever.
    fn write_failure(&self, what: &str, response: &Response) -> anyhow::Error {
        if self.bucket.no_overwrite == NoOverwrite::IfNoneMatch
            && Self::conditional_unsupported(response)
        {
            return Permanent(format!(
                "{what}: the service does not support conditional writes (HTTP {}); run capturefab destination test NAME --probe-writes to switch to check-first uploads",
                response.status
            ))
            .into();
        }
        self.failure(what, response)
    }

    /// For check-first uploads: confirm no object has this key yet.
    fn ensure_free(&self, key: &str) -> Result<()> {
        let empty = hex(&Sha256::digest([]));
        let response = self.send("HEAD", key, &[], &[], &empty, None)?;
        match response.status {
            200 => Err(Permanent(format!(
                "uploading: an object with this key already exists in s3://{}; it was not replaced",
                self.bucket.bucket
            ))
            .into()),
            404 => Ok(()),
            403 => Err(Permanent(
                "cannot confirm the key is free (HTTP 403 on HEAD); allow s3:GetObject or use a service with conditional writes".into(),
            )
            .into()),
            _ => Err(self.failure("checking the key", &response)),
        }
    }

    /// Headers that make a write conditional, when the service supports it.
    fn condition(&self) -> Option<(&'static str, String)> {
        (self.bucket.no_overwrite == NoOverwrite::IfNoneMatch)
            .then(|| ("if-none-match", "*".into()))
    }

    /// Find out whether the service refuses to replace an object on
    /// `If-None-Match: *`: write a one-byte probe twice, then delete it.
    /// Returns the result and whether the probe object was removed.
    pub fn probe_conditional_writes(&self) -> Result<(ConditionalWrites, bool)> {
        let mut key = self.bucket.prefix.clone();
        if !key.is_empty() && !key.ends_with('/') {
            key.push('/');
        }
        key.push_str(&format!(".capturefab-probe-{}", crate::transfer::token()));
        crate::destination::validate_object_key(&key)?;
        let body = b"x";
        let sha = hex(&Sha256::digest(body));
        let put = || {
            self.send(
                "PUT",
                &key,
                &[],
                &[
                    ("content-length", "1".into()),
                    ("if-none-match", "*".into()),
                ],
                &sha,
                Some(&mut &body[..]),
            )
        };
        let first = put()?;
        let result = if Self::conditional_unsupported(&first) {
            ConditionalWrites::Unsupported
        } else if first.status != 200 {
            return Err(self.failure("writing a probe object", &first));
        } else {
            let second = put()?;
            match second.status {
                412 => ConditionalWrites::Enforced,
                200 => ConditionalWrites::Ignored,
                _ if Self::conditional_unsupported(&second) => ConditionalWrites::Unsupported,
                _ => {
                    let _ = self.remove(&key);
                    return Err(self.failure("writing a probe object", &second));
                }
            }
        };
        Ok((result, self.remove(&key)))
    }

    /// Delete an object, best effort; true when it is gone.
    fn remove(&self, key: &str) -> bool {
        let empty = hex(&Sha256::digest([]));
        self.send("DELETE", key, &[], &[], &empty, None)
            .is_ok_and(|r| matches!(r.status, 200 | 204 | 404))
    }

    /// Confirm the bucket is reachable with these credentials.
    pub fn check(&self) -> Result<()> {
        let empty = hex(&Sha256::digest([]));
        let response = self.send("HEAD", "", &[], &[], &empty, None)?;
        match response.status {
            200 => Ok(()),
            // HEAD has no body, so a 403 cannot tell refused credentials from
            // write-only keys (or, on some services, a missing bucket); ask
            // again with a request whose error names the reason.
            403 => self.explain_forbidden(),
            404 => bail!(Permanent(format!(
                "bucket {} not found",
                self.bucket.bucket
            ))),
            301 => bail!(Permanent(format!(
                "bucket {} is in another region; set its region",
                self.bucket.bucket
            ))),
            _ => Err(self.failure("checking the bucket", &response)),
        }
    }

    /// After HEAD on the bucket was refused: GetBucketLocation returns an
    /// error body, so refused credentials are reported as such instead of
    /// being mistaken for write-only keys.
    fn explain_forbidden(&self) -> Result<()> {
        let empty = hex(&Sha256::digest([]));
        let response = self.send("GET", "", &[("location", "")], &[], &empty, None)?;
        if response.status == 200 {
            return Ok(());
        }
        let detail = error_text(&response.body);
        let code = detail.split(':').next().unwrap_or_default();
        match code {
            "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "InvalidToken"
            | "ExpiredToken"
            | "TokenRefreshRequired" => bail!(Permanent(format!(
                "S3 credentials refused for bucket {}: {detail}",
                self.bucket.bucket
            ))),
            "NoSuchBucket" => bail!(Permanent(format!(
                "bucket {} not found",
                self.bucket.bucket
            ))),
            _ if matches!(response.status, 408 | 429 | 500..=599) => {
                Err(self.failure("checking the bucket", &response))
            }
            _ => bail!(Permanent(format!(
                "the bucket {} exists but these credentials may not inspect it (HTTP 403); uploads can still work with write-only keys",
                self.bucket.bucket
            ))),
        }
    }

    /// Upload a local file to `key`, unless an object with that key exists.
    /// `sha256` is the file's expected hash; it is re-verified while reading.
    pub fn upload(&self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
        crate::destination::validate_object_key(key)
            .map_err(|error| Permanent(error.to_string()))?;
        ensure!(
            sha256.len() == 64
                && sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            Permanent(
                "S3 upload checksum must be a lowercase SHA-256 hash (64 hexadecimal digits)"
                    .into()
            )
        );
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("inspect upload file {}", path.display()))?;
        ensure!(
            metadata.is_file() && metadata.len() == size,
            Permanent(format!(
                "{} changed size after it was queued; not uploaded",
                path.display()
            ))
        );
        if size > MULTIPART_THRESHOLD {
            return self.upload_parts(path, key, sha256, size);
        }
        let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let actual = hash_reader(&mut file)?;
        ensure!(
            actual == sha256,
            Permanent(format!(
                "{} changed after it was queued; not uploaded",
                path.display()
            ))
        );
        file.seek(SeekFrom::Start(0))?;
        let mut body = (&mut file).take(size);
        let mut headers = vec![
            ("content-length", size.to_string()),
            ("content-type", content_type(path).to_string()),
        ];
        match self.condition() {
            Some(condition) => headers.push(condition),
            None => self.ensure_free(key)?,
        }
        let response = self.send("PUT", key, &[], &headers, sha256, Some(&mut body))?;
        if response.status == 200 {
            Ok(())
        } else {
            Err(self.write_failure("uploading", &response))
        }
    }

    fn upload_parts(&self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
        let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        ensure!(
            hash_reader(&mut file)? == sha256,
            Permanent(format!(
                "{} changed after it was queued; not uploaded",
                path.display()
            ))
        );
        if self.condition().is_none() {
            self.ensure_free(key)?;
        }
        let empty = hex(&Sha256::digest([]));
        let response = self.send(
            "POST",
            key,
            &[("uploads", "")],
            &[("content-type", content_type(path).to_string())],
            &empty,
            None,
        )?;
        if response.status != 200 {
            return Err(self.failure("starting a multipart upload", &response));
        }
        let upload_id = roxmltree::Document::parse(&response.body)
            .ok()
            .and_then(|d| {
                d.descendants()
                    .find(|n| n.has_tag_name("UploadId"))
                    .and_then(|n| n.text().map(str::to_string))
            })
            .ok_or_else(|| anyhow!("multipart upload response has no UploadId"))?;
        let result = (|| -> Result<()> {
            let mut parts = Vec::new();
            let mut buffer = Vec::with_capacity(PART_BYTES as usize);
            file.seek(SeekFrom::Start(0))?;
            let mut whole = Sha256::new();
            for number in 1.. {
                buffer.clear();
                (&mut file).take(PART_BYTES).read_to_end(&mut buffer)?;
                if buffer.is_empty() {
                    break;
                }
                whole.update(&buffer);
                let part_sha = hex(&Sha256::digest(&buffer));
                let number_text = number.to_string();
                let response = self.send(
                    "PUT",
                    key,
                    &[("partNumber", &number_text), ("uploadId", &upload_id)],
                    &[("content-length", buffer.len().to_string())],
                    &part_sha,
                    Some(&mut buffer.as_slice()),
                )?;
                if response.status != 200 {
                    return Err(self.failure("uploading a part", &response));
                }
                let etag = response
                    .etag
                    .ok_or_else(|| anyhow!("uploaded part has no ETag"))?;
                parts.push((number, etag));
            }
            ensure!(
                hex(&whole.finalize()) == sha256,
                Permanent(format!("{} changed during upload", path.display()))
            );
            let mut xml = String::from("<CompleteMultipartUpload>");
            for (number, etag) in &parts {
                let etag = etag.replace('&', "&amp;").replace('<', "&lt;");
                let _ = write!(
                    xml,
                    "<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>"
                );
            }
            xml.push_str("</CompleteMultipartUpload>");
            let body_sha = hex(&Sha256::digest(xml.as_bytes()));
            let mut headers = vec![
                ("content-length", xml.len().to_string()),
                ("content-type", "application/xml".into()),
            ];
            match self.condition() {
                Some(condition) => headers.push(condition),
                // Another writer may have finished the key while parts went up.
                None => self.ensure_free(key)?,
            }
            let response = self.send(
                "POST",
                key,
                &[("uploadId", &upload_id)],
                &headers,
                &body_sha,
                Some(&mut xml.as_bytes()),
            )?;
            // Completion can fail inside a 200 response.
            if response.status != 200 || response.body.contains("<Error>") {
                return Err(self.write_failure("completing a multipart upload", &response));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = self.send(
                "DELETE",
                key,
                &[("uploadId", &upload_id)],
                &[],
                &empty,
                None,
            );
        }
        let _ = size;
        result
    }
}

/// How a service treats `If-None-Match: *` on writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalWrites {
    /// A second write to the same key is refused (412).
    Enforced,
    /// The header is accepted but the object is replaced.
    Ignored,
    /// The service rejects conditional writes (501 or NotImplemented).
    Unsupported,
}

impl ConditionalWrites {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enforced => "enforced",
            Self::Ignored => "ignored",
            Self::Unsupported => "unsupported",
        }
    }
}

impl crate::transfer::Uploader for Client {
    fn check(&mut self) -> Result<()> {
        Client::check(self)
    }
    fn upload(&mut self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
        Client::upload(self, path, key, sha256, size)
    }
    fn probe(&mut self) -> Result<String> {
        let (writes, removed) = self.probe_conditional_writes()?;
        Ok(format!(
            "conditional writes {}{}",
            writes.as_str(),
            if removed {
                ""
            } else {
                "; the probe object could not be removed"
            }
        ))
    }
}

/// Hex SHA-256 of a reader's remaining bytes.
pub fn hash_reader(reader: &mut impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hex(&hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Keys {
        Keys {
            id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            token: None,
        }
    }

    /// The GET Object example from the AWS Signature Version 4 documentation
    /// for Amazon S3 ("Signature Calculations for the Authorization Header").
    #[test]
    fn signature_matches_the_aws_documentation_example() {
        let (authorization, extra) = sign(
            &example(),
            "us-east-1",
            "20130524T000000Z",
            &Request {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: &[],
                headers: &[("Range", "bytes=0-9".into())],
                payload_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
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

    /// Against a live S3-compatible service: set CAPTUREFAB_S3_TEST_ENDPOINT,
    /// CAPTUREFAB_S3_TEST_BUCKET, AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY.
    /// Uploads a multipart-sized file, then checks it is never replaced.
    #[test]
    #[ignore]
    fn live_multipart_upload_and_no_overwrite() {
        let endpoint = std::env::var("CAPTUREFAB_S3_TEST_ENDPOINT").expect("endpoint");
        let bucket = Bucket {
            endpoint,
            region: "us-east-1".into(),
            bucket: std::env::var("CAPTUREFAB_S3_TEST_BUCKET").expect("bucket"),
            prefix: String::new(),
            path_style: true,
            credentials: Credentials::Environment,
            keep_local: false,
            no_overwrite: NoOverwrite::IfNoneMatch,
        };
        let client = Client::new(&bucket).unwrap();
        client.check().unwrap();
        let path =
            std::env::temp_dir().join(format!("capturefab-multipart-{}.bin", std::process::id()));
        let size = MULTIPART_THRESHOLD + PART_BYTES / 2 + 12345;
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let data: Vec<u8> = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        std::fs::write(&path, &data).unwrap();
        let sha = hex(&Sha256::digest(&data));
        let key = format!("multipart/{}.bin", std::process::id());
        client.upload(&path, &key, &sha, size).unwrap();
        let again = client.upload(&path, &key, &sha, size).unwrap_err();
        assert!(!retryable(&again), "{again:#}");
        let changed = client
            .upload(&path, "multipart/other.bin", &"0".repeat(64), size)
            .unwrap_err();
        assert!(format!("{changed:#}").contains("changed"));
        let _ = std::fs::remove_file(path);
        eprintln!("uploaded {size} bytes as {key}, sha256 {sha}");
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
    fn keys_encode_and_profiles_parse() {
        assert_eq!(encode("run 1/a+b.png", false), "run%201/a%2Bb.png");
        assert_eq!(encode("a/b", true), "a%2Fb");
        let file = "[default]\naws_access_key_id = A1\naws_secret_access_key=S1\n\n# comment\n[lab]\naws_access_key_id=A2\naws_secret_access_key = S2\naws_session_token = T2\n[empty]\n";
        let lab = profile_keys(file, "lab").unwrap();
        assert_eq!(
            (lab.id.as_str(), lab.secret.as_str(), lab.token.as_deref()),
            ("A2", "S2", Some("T2"))
        );
        assert_eq!(profile_keys(file, "default").unwrap().id, "A1");
        assert!(profile_keys(file, "empty").is_none());
        assert!(profile_keys(file, "missing").is_none());
        assert_eq!(
            error_text(
                "<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"
            ),
            "AccessDenied: Access Denied"
        );
        assert_eq!(content_type(Path::new("a/b.JPG")), "image/jpeg");
    }
    #[test]
    fn invalid_s3_keys_and_upload_requests_fail_before_credentials_or_network() {
        assert!(validate_secret("").is_err());
        assert!(validate_secret(&"x".repeat(4097)).is_err());
        for secret in ["has whitespace", "line\nfeed", "unicodeé"] {
            assert!(validate_secret(secret).is_ok());
        }
        let valid = Keys {
            id: "access-key".into(),
            secret: "printable/secret+value=".into(),
            token: Some("token+value=".into()),
        };
        assert!(valid.validate().is_ok());
        let invalid = Keys {
            token: Some("bad\r\nheader".into()),
            ..valid
        };
        assert!(invalid.validate().is_err());
        let bucket = Bucket {
            endpoint: "https://".into(),
            region: "us-east-1".into(),
            bucket: "camera-archive".into(),
            prefix: String::new(),
            path_style: true,
            credentials: Credentials::Environment,
            keep_local: true,
            no_overwrite: NoOverwrite::IfNoneMatch,
        };
        assert!(
            Client::new(&bucket)
                .err()
                .unwrap()
                .to_string()
                .contains("S3 endpoint")
        );
        assert!(
            store_secret("", "secret")
                .unwrap_err()
                .to_string()
                .contains("access key ID")
        );
    }

    #[test]
    fn client_lowercases_only_the_endpoint_scheme() {
        let bucket = Bucket {
            endpoint: "HTTPS://MinIO.Example:9000/".into(),
            region: "us-east-1".into(),
            bucket: "camera-archive".into(),
            prefix: "Exact %2f /".into(),
            path_style: true,
            credentials: Credentials::Environment,
            keep_local: true,
            no_overwrite: NoOverwrite::IfNoneMatch,
        };
        let client = Client::with_keys(
            &bucket,
            Keys {
                id: "access".into(),
                secret: "secret".into(),
                token: None,
            },
        )
        .unwrap();
        assert_eq!(client.origin, "https://MinIO.Example:9000");
        assert_eq!(client.host, "MinIO.Example:9000");
        assert_eq!(client.bucket.prefix, "Exact %2f /");
        assert_eq!(
            client.path("Exact %2f /clip.mkv"),
            "/camera-archive/Exact%20%252f%20/clip.mkv"
        );
    }

    /// A loopback HTTP server answering each request with the next status,
    /// recording the method, path and lowercase header names.
    fn scripted(statuses: &[u16]) -> (Bucket, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let responses: Vec<(u16, &'static str)> = statuses.iter().map(|&s| (s, "")).collect();
        scripted_with(&responses)
    }

    /// Like `scripted`, with a body for each response.
    fn scripted_with(
        responses: &[(u16, &'static str)],
    ) -> (Bucket, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let mut statuses: std::collections::VecDeque<(u16, &'static str)> =
            responses.iter().copied().collect();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut out = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                loop {
                    let mut request = String::new();
                    if reader.read_line(&mut request).unwrap_or(0) == 0 {
                        break;
                    }
                    let mut headers = Vec::new();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let line = line.trim_end();
                        if line.is_empty() {
                            break;
                        }
                        let (name, value) = line.split_once(':').unwrap();
                        let name = name.trim().to_ascii_lowercase();
                        if name == "content-length" {
                            length = value.trim().parse().unwrap();
                        }
                        headers.push(name);
                    }
                    let mut body = vec![0; length];
                    std::io::Read::read_exact(&mut reader, &mut body).unwrap();
                    let line: Vec<&str> = request.split_whitespace().collect();
                    log.lock().unwrap().push(format!(
                        "{} {} [{}]",
                        line[0],
                        line[1],
                        headers.join(",")
                    ));
                    let (status, reply) = statuses.pop_front().unwrap_or((500, ""));
                    let length = if line[0] == "HEAD" { 0 } else { reply.len() };
                    write!(
                        out,
                        "HTTP/1.1 {status} Scripted\r\ncontent-length: {length}\r\n\r\n{}",
                        &reply[..length]
                    )
                    .unwrap();
                }
            }
        });
        let bucket = Bucket {
            endpoint: format!("http://127.0.0.1:{port}"),
            region: "us-east-1".into(),
            bucket: "camera-archive".into(),
            prefix: "line-1".into(),
            path_style: true,
            credentials: Credentials::Environment,
            keep_local: false,
            no_overwrite: NoOverwrite::IfNoneMatch,
        };
        (bucket, seen)
    }

    fn client(bucket: &Bucket) -> Client {
        Client::with_keys(
            bucket,
            Keys {
                id: "access".into(),
                secret: "secret".into(),
                token: None,
            },
        )
        .unwrap()
    }

    fn upload_file() -> (std::path::PathBuf, String) {
        let path = std::env::temp_dir().join(format!("capturefab-s3-{}.bin", std::process::id()));
        std::fs::write(&path, b"frame").unwrap();
        (path, hex(&Sha256::digest(b"frame")))
    }

    #[test]
    fn probe_classifies_enforced_ignored_unsupported() {
        for (statuses, expected, removed) in [
            (&[200, 412, 204][..], ConditionalWrites::Enforced, true),
            (&[200, 200, 204][..], ConditionalWrites::Ignored, true),
            (&[501, 404][..], ConditionalWrites::Unsupported, true),
            (&[200, 412, 403][..], ConditionalWrites::Enforced, false),
        ] {
            let (bucket, seen) = scripted(statuses);
            assert_eq!(
                client(&bucket).probe_conditional_writes().unwrap(),
                (expected, removed)
            );
            let seen = seen.lock().unwrap();
            assert!(
                seen[0].starts_with("PUT /camera-archive/line-1/.capturefab-probe-")
                    && seen[0].contains("if-none-match"),
                "{seen:?}"
            );
            assert!(seen.last().unwrap().starts_with("DELETE "));
        }
    }

    #[test]
    fn check_names_refused_credentials_after_a_bodiless_403() {
        let error_body = |code: &str| -> &'static str {
            Box::leak(
                format!("<Error><Code>{code}</Code><Message>m</Message></Error>").into_boxed_str(),
            )
        };
        for (code, expected) in [
            (
                "SignatureDoesNotMatch",
                "S3 credentials refused for bucket camera-archive",
            ),
            (
                "InvalidAccessKeyId",
                "S3 credentials refused for bucket camera-archive",
            ),
            ("NoSuchBucket", "bucket camera-archive not found"),
            ("AccessDenied", "may not inspect it (HTTP 403)"),
        ] {
            let (bucket, seen) = scripted_with(&[(403, ""), (403, error_body(code))]);
            let error = client(&bucket).check().unwrap_err();
            assert!(!retryable(&error), "{code}: {error:#}");
            assert!(error.to_string().contains(expected), "{code}: {error:#}");
            if code != "AccessDenied" {
                assert!(!error.to_string().contains("exists"), "{error:#}");
            }
            let seen = seen.lock().unwrap();
            assert!(
                seen[1].starts_with("GET /camera-archive?location"),
                "{seen:?}"
            );
        }
        // Write-only keys refused HEAD but allowed the follow-up.
        let (bucket, _) = scripted_with(&[(403, ""), (200, "<LocationConstraint/>")]);
        client(&bucket).check().unwrap();
    }

    #[test]
    fn internal_error_inside_a_200_completion_is_retried() {
        let (bucket, _) = scripted(&[]);
        let response = Response {
            status: 200,
            etag: None,
            body: "<Error><Code>InternalError</Code><Message>try again</Message></Error>".into(),
        };
        let error = client(&bucket).write_failure("completing a multipart upload", &response);
        assert!(retryable(&error), "{error:#}");
    }

    #[test]
    fn check_first_head_200_refuses_without_put() {
        let (mut bucket, seen) = scripted(&[200]);
        bucket.no_overwrite = NoOverwrite::CheckFirst;
        let (path, sha) = upload_file();
        let error = client(&bucket)
            .upload(&path, "line-1/a.png", &sha, 5)
            .unwrap_err();
        assert!(!retryable(&error));
        assert!(error.to_string().contains("already exists"));
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(seen.lock().unwrap()[0].starts_with("HEAD /camera-archive/line-1/a.png"));
        let (mut bucket, _) = scripted(&[403]);
        bucket.no_overwrite = NoOverwrite::CheckFirst;
        let error = client(&bucket)
            .upload(&path, "line-1/a.png", &sha, 5)
            .unwrap_err();
        assert!(!retryable(&error) && error.to_string().contains("HTTP 403 on HEAD"));
    }

    #[test]
    fn check_first_404_puts_without_if_none_match() {
        let (mut bucket, seen) = scripted(&[404, 200]);
        bucket.no_overwrite = NoOverwrite::CheckFirst;
        let (path, sha) = upload_file();
        client(&bucket)
            .upload(&path, "line-1/a.png", &sha, 5)
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].starts_with("PUT /camera-archive/line-1/a.png"));
        assert!(!seen[1].contains("if-none-match"), "{seen:?}");
    }

    #[test]
    fn if_none_match_501_is_permanent_with_probe_hint() {
        let (bucket, seen) = scripted(&[501]);
        let (path, sha) = upload_file();
        let error = client(&bucket)
            .upload(&path, "line-1/a.png", &sha, 5)
            .unwrap_err();
        assert!(!retryable(&error), "501 used to be retried forever");
        assert!(error.to_string().contains("--probe-writes"), "{error:#}");
        assert!(seen.lock().unwrap()[0].contains("if-none-match"));
        // Without the conditional header a 501 stays an ordinary server error.
        let (mut bucket, _) = scripted(&[404, 501]);
        bucket.no_overwrite = NoOverwrite::CheckFirst;
        let error = client(&bucket)
            .upload(&path, "line-1/a.png", &sha, 5)
            .unwrap_err();
        assert!(retryable(&error));
    }
}
