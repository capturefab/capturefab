//! What every network destination shares: the uploader interface, the
//! permanent/retryable error split, credential-store access, and checksumming
//! reads. The S3, FTP and SFTP clients live in `s3`, `ftp` and `sftp`.
//!
//! Network I/O happens only in the uploader thread and in explicit checks
//! (`destination add-*`, `destination test`, the GUI's Test action), never in
//! camera workers or the session coordinator.
use crate::destination::{Remote, SecretSource};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

#[cfg(feature = "ftp")]
pub mod ftp;
#[cfg(feature = "sftp")]
pub mod sftp;
#[cfg(all(test, feature = "sftp"))]
mod sftp_test_server;

pub use crate::secret_input::read_stdin_secret;

/// An error that retrying will not fix: refused credentials, a missing
/// bucket or folder, a changed host key, or an object that already exists.
/// Other errors are retried with backoff.
#[derive(Debug)]
pub struct Permanent(pub String);
impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Permanent {}

/// Whether an upload error should be retried later.
pub fn retryable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Permanent>().is_none()
}

/// One connection to a network destination. Implementations keep the
/// connection open between uploads and reconnect when it has dropped.
pub trait Uploader: Send {
    /// Reach the server, sign in, and confirm the bucket or folder is usable.
    fn check(&mut self) -> Result<()>;
    /// Upload `path` as `key` (relative to the bucket prefix or server
    /// folder), never replacing an existing object. Fails as `Permanent` when
    /// the bytes read differ from `sha256` and `size`.
    fn upload(&mut self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()>;
    /// Write and then remove a probe object; returns a short summary.
    fn probe(&mut self) -> Result<String>;
}

/// Connect to a remote with its saved credentials.
pub fn connect(remote: &Remote) -> Result<Box<dyn Uploader>> {
    remote.validate()?;
    match remote {
        #[cfg(feature = "s3")]
        Remote::S3(bucket) => Ok(Box::new(crate::s3::Client::new(bucket)?)),
        #[cfg(feature = "ftp")]
        Remote::Ftp(server) => Ok(Box::new(ftp::FtpUploader::new(
            server.clone(),
            secret_for(remote)?,
        )?)),
        #[cfg(feature = "sftp")]
        Remote::Sftp(server) => Ok(Box::new(sftp::SftpUploader::new(
            server.clone(),
            secret_for(remote)?,
        )?)),
        #[allow(unreachable_patterns)]
        _ => Err(unsupported(remote)),
    }
}

/// Connect with a secret typed into a form, without touching the credential
/// store (the GUI's Test action before saving).
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn connect_with_secret(remote: &Remote, secret: String) -> Result<Box<dyn Uploader>> {
    remote.validate()?;
    validate_secret(&secret)?;
    match remote {
        #[cfg(feature = "s3")]
        Remote::S3(bucket) => Ok(Box::new(crate::s3::Client::with_secret(bucket, secret)?)),
        #[cfg(feature = "ftp")]
        Remote::Ftp(server) => Ok(Box::new(ftp::FtpUploader::new(
            server.clone(),
            Some(secret),
        )?)),
        #[cfg(feature = "sftp")]
        Remote::Sftp(server) => Ok(Box::new(sftp::SftpUploader::new(
            server.clone(),
            Some(secret),
        )?)),
        #[allow(unreachable_patterns)]
        _ => Err(unsupported(remote)),
    }
}

#[allow(dead_code)]
fn unsupported(remote: &Remote) -> anyhow::Error {
    let (name, feature) = match remote {
        Remote::S3(_) => ("S3", "s3"),
        Remote::Ftp(_) => ("FTP", "ftp"),
        Remote::Sftp(_) => ("SFTP", "sftp"),
    };
    Permanent(format!(
        "{name} destinations are unsupported in this build; rebuild with --features {feature}"
    ))
    .into()
}

// ---------------------------------------------------------------------------
// Credentials.

pub use crate::keychain::{delete_secret, has_secret, read_secret, store_secret, validate_secret};

/// The FTP or SFTP password (or key passphrase) for an upload. `None` means
/// sign in without one.
#[allow(dead_code)]
pub(crate) fn secret_for(remote: &Remote) -> Result<Option<String>> {
    let (source, variable) = match remote {
        Remote::S3(_) => return Ok(None),
        Remote::Ftp(server) => (server.password, "CAPTUREFAB_FTP_PASSWORD"),
        Remote::Sftp(server) => (server.password, "CAPTUREFAB_SFTP_PASSWORD"),
    };
    match source {
        SecretSource::NoSecret => Ok(None),
        SecretSource::Environment => match std::env::var(variable) {
            Ok(secret) if !secret.is_empty() => Ok(Some(secret)),
            _ => Err(Permanent(format!(
                "{variable} is not set in this process; the process that uploads, such as a serve service, needs it"
            ))
            .into()),
        },
        SecretSource::Keychain => {
            let (service, account) = remote.secret_ref().expect("keychain secret reference");
            let command = match remote {
                Remote::Ftp(_) => "add-ftp",
                _ => "add-sftp",
            };
            match read_secret(service, &account).map_err(|e| Permanent(format!("{e:#}")))? {
                Some(secret) => Ok(Some(secret)),
                None => Err(Permanent(format!(
                    "no password saved for {account}; run capturefab destination {command} ... --password-stdin"
                ))
                .into()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Checksummed reads.

/// Hashes and counts bytes as they are read, so an upload can confirm it
/// sent exactly the file that was queued.
#[allow(dead_code)]
pub(crate) struct HashingReader<R> {
    inner: R,
    hash: Sha256,
    bytes: u64,
}

#[allow(dead_code)]
impl<R: Read> HashingReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            bytes: 0,
        }
    }
    /// The lowercase hex SHA-256 and the number of bytes read.
    pub(crate) fn finish(self) -> (String, u64) {
        (hex(&self.hash.finalize()), self.bytes)
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hash.update(&buffer[..read]);
        self.bytes += read as u64;
        Ok(read)
    }
}

/// Hashes bytes handed to it, for checking a remote file read back.
#[derive(Default)]
#[allow(dead_code)]
pub(crate) struct Sha256Writer {
    hash: Sha256,
    bytes: u64,
}
#[allow(dead_code)]
impl Sha256Writer {
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.hash.update(bytes);
        self.bytes += bytes.len() as u64;
    }
    /// The lowercase hex SHA-256 and the number of bytes.
    pub(crate) fn finish(self) -> (String, u64) {
        (hex(&self.hash.finalize()), self.bytes)
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Check an upload request before any network I/O.
#[allow(dead_code)]
pub(crate) fn check_request(path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
    crate::destination::validate_object_key(key).map_err(|e| Permanent(e.to_string()))?;
    ensure!(
        sha256.len() == 64
            && sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        Permanent(
            "upload checksum must be a lowercase SHA-256 hash (64 hexadecimal digits)".into()
        )
    );
    let metadata = std::fs::metadata(path)
        .map_err(|e| Permanent(format!("local file {} is gone: {e}", path.display())))?;
    ensure!(
        metadata.is_file() && metadata.len() == size,
        Permanent(format!(
            "{} changed size after it was queued; not uploaded",
            path.display()
        ))
    );
    Ok(())
}

/// A random 16-hex-digit token for probe and temporary names.
#[allow(dead_code)]
pub(crate) fn token() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    hex(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn hashing_reader_counts_and_hashes() {
        let mut reader = HashingReader::new(&b"abc"[..]);
        std::io::copy(&mut reader, &mut std::io::sink()).unwrap();
        assert_eq!(
            reader.finish(),
            (
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
                3
            )
        );
    }

    #[test]
    fn permanent_errors_are_not_retried() {
        assert!(!retryable(&Permanent("x".into()).into()));
        assert!(!retryable(
            &anyhow::Error::from(Permanent("x".into())).context("while uploading")
        ));
        assert!(retryable(&anyhow!("connection reset")));
    }
}
