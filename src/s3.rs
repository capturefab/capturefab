//! A small S3-compatible client for uploading finished captures: AWS Signature
//! Version 4 (see `aws`) over a pure-Rust HTTPS stack that trusts the OS
//! certificate store. Payloads are signed with their SHA-256, so the service
//! rejects bytes that changed in transit, and writes are conditional
//! (`If-None-Match: *`), so an existing object is never replaced. Services
//! that ignore conditional writes (found with `probe_conditional_writes`) use
//! a HEAD check first.
//!
//! Credentials come from the OS credential store, an AWS shared-credentials
//! profile, or the environment; secrets are never written to Capturefab files
//! or sent over the wire.
use crate::aws::{Request, UriMode, hex, uri_encode};
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
/// Files up to this size are read once and checked in memory before they are
/// sent; larger single uploads are hashed first, then read again as the body.
const BUFFERED_PUT: u64 = 16 * 1024 * 1024;
const MAX_RESPONSE: u64 = 1024 * 1024;

pub use crate::transfer::{Permanent, retryable};

// ---------------------------------------------------------------------------
// Credentials.

pub use crate::aws::Keys;

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
#[cfg(feature = "gui")]
pub(crate) fn read_secret(access_key_id: &str) -> Result<Option<String>> {
    Credentials::Keychain {
        access_key_id: access_key_id.into(),
    }
    .validate()?;
    crate::transfer::read_secret(KEYCHAIN_SERVICE, access_key_id)
}

pub fn validate_secret(secret: &str) -> Result<()> {
    ensure!(
        !secret.is_empty() && secret.len() <= 4096,
        "S3 secret access key must contain 1..4096 bytes"
    );
    Ok(())
}

fn validate_keys(keys: &Keys) -> Result<()> {
    Credentials::Keychain {
        access_key_id: keys.id().into(),
    }
    .validate()?;
    validate_secret(keys.secret())?;
    if let Some(token) = keys.token() {
        crate::aws::validate_session_token(token)?;
    }
    Ok(())
}

pub fn keys(credentials: &Credentials) -> Result<Keys> {
    keys_in(credentials, &crate::aws::process_vars)
}

/// `keys`, reading AWS variables through `vars`.
fn keys_in(credentials: &Credentials, vars: crate::aws::Vars<'_>) -> Result<Keys> {
    credentials.validate()?;
    let permanent = |error: anyhow::Error| Permanent(error.to_string());
    let keys = match credentials {
        Credentials::Keychain { access_key_id } => {
            let secret = keyring::Entry::new(KEYCHAIN_SERVICE, access_key_id)
                .and_then(|entry| entry.get_password())
                .map_err(|e| {
                    Permanent(format!(
                        "no secret key for {access_key_id} in the OS credential store ({e}); enter it again in the destination settings or `capturefab destination add-s3 --secret-stdin`"
                    ))
                })?;
            Keys::new(access_key_id.clone(), secret, None)
        }
        // Exactly the named profile, or exactly the environment: never the
        // AWS_PROFILE or default-profile fallbacks of `aws::bedrock_chain`,
        // which could sign uploads with another account's keys.
        Credentials::Profile { name } => {
            crate::aws::keys_from_profile_in(name, vars).map_err(permanent)?
        }
        Credentials::Environment => crate::aws::keys_from_env_in(vars).map_err(permanent)?,
    };
    validate_keys(&keys).map_err(permanent)?;
    Ok(keys)
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
        Self::with_keys(bucket, Keys::new(access_key_id.clone(), secret, None))
    }

    fn with_keys(bucket: &Bucket, keys: Keys) -> Result<Self> {
        bucket.validate()?;
        // `BUCKET.127.0.0.1` cannot resolve: an IP or localhost endpoint
        // always takes the bucket in the path.
        let mut bucket = bucket.clone();
        bucket.path_style |= crate::destination::endpoint_needs_path_style(&bucket.endpoint);
        let bucket = &bucket;
        validate_keys(&keys)?;
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
        let key = uri_encode(key, false);
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
        let (authorization, signed) = crate::aws::sign(
            &self.keys,
            &self.bucket.region,
            "s3",
            UriMode::S3,
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
                    .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
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
        let mut file = open(path)?;
        let mut body: Box<dyn Read> = if size <= BUFFERED_PUT {
            Box::new(std::io::Cursor::new(read_queued(file, size, sha256, path)?))
        } else {
            ensure!(hash_reader(&mut file)? == sha256, changed(path));
            file.seek(SeekFrom::Start(0))?;
            Box::new(file.take(size))
        };
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

    /// Upload in parts, hashing the file as they are read: completion waits
    /// for the whole hash, and a changed file aborts the upload instead.
    fn upload_parts(&self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
        let mut file = open(path)?;
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

/// Open a file to upload; tests count the bytes read from it.
fn open(path: &Path) -> Result<Opened> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    #[cfg(test)]
    let file = tests::Counted(file);
    Ok(file)
}
#[cfg(not(test))]
type Opened = File;
#[cfg(test)]
type Opened = tests::Counted<File>;

/// A queued file read whole, unless it is no longer exactly the queued bytes.
fn read_queued(reader: impl Read, size: u64, sha256: &str, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size as usize);
    reader.take(size + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == size && hex(&Sha256::digest(&bytes)) == sha256,
        changed(path)
    );
    Ok(bytes)
}

fn changed(path: &Path) -> Permanent {
    Permanent(format!(
        "{} changed after it was queued; not uploaded",
        path.display()
    ))
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
    use std::ffi::OsString;

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
    fn error_text_and_content_types() {
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
        let keys = |token: &str| {
            Keys::new(
                "access-key".into(),
                "printable/secret+value=".into(),
                Some(token.into()),
            )
        };
        assert!(validate_keys(&keys("token+value=")).is_ok());
        assert!(validate_keys(&keys("bad\r\nheader")).is_err());
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
        let client =
            Client::with_keys(&bucket, Keys::new("access".into(), "secret".into(), None)).unwrap();
        assert_eq!(client.origin, "https://MinIO.Example:9000");
        assert_eq!(client.host, "MinIO.Example:9000");
        assert_eq!(client.bucket.prefix, "Exact %2f /");
        assert_eq!(
            client.path("Exact %2f /clip.mkv"),
            "/camera-archive/Exact%20%252f%20/clip.mkv"
        );
    }

    /// S3 credentials name exactly one source: `environment` never falls
    /// back to AWS_PROFILE or the default profile, and a profile never to
    /// the environment, unlike Bedrock's chain.
    #[test]
    fn s3_credentials_never_fall_back_to_other_aws_sources() {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-s3-sources-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let credentials = dir.join("credentials");
        std::fs::write(
            &credentials,
            "[default]\naws_access_key_id=DEFAULT\naws_secret_access_key=d\n[lab]\naws_access_key_id=LAB\naws_secret_access_key=l\n",
        )
        .unwrap();
        let vars = |name: &str| -> Option<OsString> {
            match name {
                "AWS_SHARED_CREDENTIALS_FILE" => Some(credentials.clone().into()),
                "AWS_PROFILE" => Some("default".into()),
                _ => None,
            }
        };
        let error = keys_in(&Credentials::Environment, &vars).err().unwrap();
        assert_eq!(
            error.to_string(),
            "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not set in the uploading process"
        );
        assert!(!retryable(&error));
        let with_environment = |name: &str| -> Option<OsString> {
            match name {
                "AWS_ACCESS_KEY_ID" => Some("ENV".into()),
                "AWS_SECRET_ACCESS_KEY" => Some("e".into()),
                _ => vars(name),
            }
        };
        let lab = Credentials::Profile { name: "lab".into() };
        assert_eq!(keys_in(&lab, &with_environment).unwrap().id(), "LAB");
        let missing = Credentials::Profile {
            name: "missing".into(),
        };
        let error = keys_in(&missing, &with_environment).err().unwrap();
        assert!(
            error.to_string().starts_with("profile [missing] in ") && !retryable(&error),
            "{error:#}"
        );
        let invalid = |name: &str| -> Option<OsString> {
            match name {
                "AWS_ACCESS_KEY_ID" => Some("ENV".into()),
                "AWS_SECRET_ACCESS_KEY" => Some("e".into()),
                "AWS_SESSION_TOKEN" => Some("bad token".into()),
                _ => None,
            }
        };
        let error = keys_in(&Credentials::Environment, &invalid).err().unwrap();
        assert!(
            error.to_string().starts_with("AWS session token") && !retryable(&error),
            "{error:#}"
        );
        std::fs::remove_dir_all(dir).unwrap();
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
                    // A bodiless POST may come chunked: skip to its last chunk.
                    if headers.iter().any(|h| h == "transfer-encoding") {
                        loop {
                            let mut size = String::new();
                            reader.read_line(&mut size).unwrap();
                            let size = usize::from_str_radix(size.trim(), 16).unwrap();
                            let mut chunk = vec![0; size + 2];
                            std::io::Read::read_exact(&mut reader, &mut chunk).unwrap();
                            if size == 0 {
                                break;
                            }
                        }
                    }
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
                        "HTTP/1.1 {status} Scripted\r\netag: \"scripted\"\r\ncontent-length: {length}\r\n\r\n{}",
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
        Client::with_keys(bucket, Keys::new("access".into(), "secret".into(), None)).unwrap()
    }

    fn upload_file() -> (std::path::PathBuf, String) {
        // Parallel upload tests must not truncate a file another test has
        // already hashed and queued.
        let path = std::env::temp_dir().join(format!(
            "capturefab-s3-{}-{}.bin",
            std::process::id(),
            getrandom::u64().unwrap()
        ));
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

    thread_local! {
        /// Bytes this thread has read from files it uploads.
        static READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Counts the bytes read through it in `READ`.
    pub(super) struct Counted<R>(pub(super) R);
    impl<R: Read> Read for Counted<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.0.read(buffer)?;
            READ.set(READ.get() + read as u64);
            Ok(read)
        }
    }
    impl<R: Seek> Seek for Counted<R> {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(to)
        }
    }

    #[test]
    fn a_small_upload_reads_its_file_once() {
        let (bucket, seen) = scripted(&[200]);
        let (path, sha) = upload_file();
        READ.set(0);
        client(&bucket)
            .upload(&path, "line-1/a.png", &sha, 5)
            .unwrap();
        assert_eq!(READ.get(), 5);
        assert!(seen.lock().unwrap()[0].starts_with("PUT /camera-archive/line-1/a.png"));
        let staged = Path::new("/staged/a.png");
        for (bytes, size) in [(&b"frame!"[..], 5), (b"fram", 5), (b"frane", 5)] {
            let error = read_queued(bytes, size, &sha, staged).unwrap_err();
            assert!(!retryable(&error) && error.to_string().contains("changed"));
        }
        // A changed file is refused before anything is sent.
        let (bucket, seen) = scripted(&[200]);
        let error = client(&bucket)
            .upload(&path, "line-1/a.png", &"0".repeat(64), 5)
            .unwrap_err();
        assert!(!retryable(&error), "{error:#}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn multipart_upload_completes_only_with_the_queued_bytes() {
        let started = "<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>";
        let (path, sha) = upload_file();
        // The whole-file hash is checked as the parts go up; a mismatch
        // aborts the upload instead of completing it.
        let (bucket, seen) = scripted_with(&[(200, started), (200, ""), (204, "")]);
        let error = client(&bucket)
            .upload_parts(&path, "line-1/a.bin", &"0".repeat(64), 5)
            .unwrap_err();
        assert!(!retryable(&error) && error.to_string().contains("changed during upload"));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(seen[0].starts_with("POST /camera-archive/line-1/a.bin?uploads"));
        assert!(seen[1].starts_with("PUT /camera-archive/line-1/a.bin?partNumber=1&uploadId=u1"));
        assert!(seen[2].starts_with("DELETE /camera-archive/line-1/a.bin?uploadId=u1"));
        let (bucket, seen) = scripted_with(&[(200, started), (200, ""), (200, "<Done/>")]);
        READ.set(0);
        client(&bucket)
            .upload_parts(&path, "line-1/a.bin", &sha, 5)
            .unwrap();
        assert_eq!(READ.get(), 5, "one pass hashes and sends the parts");
        assert!(
            seen.lock().unwrap()[2].starts_with("POST /camera-archive/line-1/a.bin?uploadId=u1")
        );
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
