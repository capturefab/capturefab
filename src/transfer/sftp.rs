//! SFTP uploads over pure-Rust SSH (russh with ring). The server must present
//! the pinned host key; there is no trust-on-first-use here, only through
//! `fetch_host_key` when the user asks for it.
//!
//! A file is written as `.NAME.HASH.part`, opened exclusively, and renamed
//! with SSH_FXP_RENAME, which refuses to replace an existing file.
//!
//! russh is asynchronous: each uploader owns a current-thread tokio runtime
//! that only runs while a call is in progress. Its background tasks are idle
//! between uploads, so a dropped connection is noticed and replaced before
//! the next one.
use super::{HashingReader, Permanent, Uploader, check_request};
use crate::destination::{Remote, SftpServer};
use anyhow::{Result, anyhow};
use russh::client::{self, AuthResult};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{OpenFlags, StatusCode};
use std::{
    future::Future,
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 256 * 1024;

/// Accepts only the pinned host key, and records the key it was shown.
struct Pin {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Pin {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let data = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.key_data(),
            PublicKeyOrCertificate::Certificate(certificate) => certificate.public_key(),
        };
        let fingerprint = data.fingerprint(HashAlg::Sha256).to_string();
        let trusted = self.expected.as_deref() == Some(fingerprint.as_str());
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fingerprint);
        Ok(trusted)
    }
}

pub struct SftpUploader {
    server: SftpServer,
    secret: Option<String>,
    /// `sftp://host:port/dir`, for messages; never the user.
    location: String,
    /// Declared before the runtime, so the session drops while it still runs.
    conn: Option<(client::Handle<Pin>, SftpSession)>,
    runtime: tokio::runtime::Runtime,
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn host(server: &SftpServer) -> &str {
    server.host.trim_start_matches('[').trim_end_matches(']')
}

fn server_name(server: &SftpServer) -> String {
    format!("sftp://{}:{}", server.host, server.port)
}

fn config() -> Arc<client::Config> {
    Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(120)),
        keepalive_interval: Some(Duration::from_secs(15)),
        ..Default::default()
    })
}

/// Run `future` with a deadline; a missed deadline is retried later.
async fn within<T, E: std::fmt::Display>(
    limit: Duration,
    what: &str,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T> {
    match tokio::time::timeout(limit, future).await {
        Ok(result) => result.map_err(|e| anyhow!("{what}: {e}")),
        Err(_) => Err(anyhow!("{what} timed out after {} s", limit.as_secs())),
    }
}

/// Open an SSH connection, checking the host key. Returns the connection or
/// the error, and the fingerprint the server offered.
async fn handshake(
    server: &SftpServer,
    expected: Option<String>,
) -> (Result<client::Handle<Pin>>, Option<String>) {
    let seen = Arc::new(Mutex::new(None));
    let pin = Pin {
        expected: expected.clone(),
        seen: seen.clone(),
    };
    let name = server_name(server);
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(config(), (host(server), server.port), pin),
    )
    .await;
    let offered = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let result = match connected {
        Ok(Ok(handle)) => Ok(handle),
        Err(_) => Err(anyhow!(
            "connecting to {name} timed out after {} s",
            CONNECT_TIMEOUT.as_secs()
        )),
        Ok(Err(error)) => match (&offered, &expected) {
            (Some(offered), Some(expected)) if offered != expected => Err(Permanent(format!(
                "SFTP host key changed: expected {expected}, server offered {offered}; confirm with the server administrator and update the destination"
            ))
            .into()),
            _ => Err(anyhow!("connecting to {name}: {error}")),
        },
    };
    (result, offered)
}

/// The SHA256 fingerprint of a server's host key, without signing in. Used
/// only when the user explicitly accepts the key on first use.
pub fn fetch_host_key(host_name: &str, port: u16) -> Result<String> {
    let server = SftpServer {
        host: host_name.into(),
        port,
        user: "capturefab".into(),
        dir: String::new(),
        host_key: format!("SHA256:{}", "A".repeat(43)),
        key_file: None,
        password: Default::default(),
        keep_local: false,
    };
    runtime()?.block_on(async {
        let (result, offered) = handshake(&server, None).await;
        match (offered, result) {
            (Some(fingerprint), _) => Ok(fingerprint),
            (None, Err(error)) => Err(error),
            (None, Ok(_)) => Err(anyhow!("{} offered no host key", server_name(&server))),
        }
    })
}

/// Join SFTP path parts with `/`, keeping a leading `/` of the first.
fn join(parts: &[&str]) -> String {
    let absolute = parts.first().is_some_and(|p| p.starts_with('/'));
    let joined = parts
        .iter()
        .flat_map(|p| p.split('/'))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

fn no_such_file(error: &russh_sftp::client::error::Error) -> bool {
    matches!(error, russh_sftp::client::error::Error::Status(status) if status.status_code == StatusCode::NoSuchFile)
}

impl SftpUploader {
    /// `secret` is the password, or the key file's passphrase.
    pub fn new(server: SftpServer, secret: Option<String>) -> Result<Self> {
        server.validate()?;
        Ok(Self {
            location: Remote::Sftp(server.clone()).location(),
            server,
            secret,
            runtime: runtime()?,
            conn: None,
        })
    }

    async fn open(
        server: &SftpServer,
        secret: Option<&str>,
    ) -> Result<(client::Handle<Pin>, SftpSession)> {
        let name = server_name(server);
        let (handle, _) = handshake(server, Some(server.host_key.clone())).await;
        let mut handle = handle?;
        let account = format!("{}@{}:{}", server.user, server.host, server.port);
        let result = match &server.key_file {
            Some(path) => {
                let key = russh::keys::load_secret_key(path, secret).map_err(|e| {
                    Permanent(format!("cannot load SFTP key {}: {e}", path.display()))
                })?;
                let hash = within(
                    AUTH_TIMEOUT,
                    "negotiating the key signature",
                    handle.best_supported_rsa_hash(),
                )
                .await?
                .flatten();
                within(
                    AUTH_TIMEOUT,
                    "signing in",
                    handle.authenticate_publickey(
                        &server.user,
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    ),
                )
                .await?
            }
            None => match secret {
                Some(password) => {
                    within(
                        AUTH_TIMEOUT,
                        "signing in",
                        handle.authenticate_password(&server.user, password),
                    )
                    .await?
                }
                None => {
                    within(
                        AUTH_TIMEOUT,
                        "signing in",
                        handle.authenticate_none(&server.user),
                    )
                    .await?
                }
            },
        };
        if !matches!(result, AuthResult::Success) {
            return Err(Permanent(format!("SFTP sign-in refused for {account}")).into());
        }
        let channel = within(
            REQUEST_TIMEOUT,
            "opening an SSH channel",
            handle.channel_open_session(),
        )
        .await?;
        within(
            REQUEST_TIMEOUT,
            "starting SFTP",
            channel.request_subsystem(true, "sftp"),
        )
        .await?;
        let sftp = within(
            REQUEST_TIMEOUT,
            &format!("starting SFTP on {name}"),
            SftpSession::new(channel.into_stream()),
        )
        .await?;
        sftp.set_timeout(REQUEST_TIMEOUT.as_secs());
        Ok((handle, sftp))
    }

    /// A live session: the cached one unless its connection closed.
    fn ready(&mut self) -> Result<()> {
        if self
            .conn
            .as_ref()
            .is_some_and(|(handle, _)| handle.is_closed())
        {
            self.conn = None;
        }
        if self.conn.is_none() {
            let (server, secret) = (&self.server, self.secret.as_deref());
            self.conn = Some(self.runtime.block_on(Self::open(server, secret))?);
        }
        Ok(())
    }

    /// Create `dir` and each parent; an existing folder is fine.
    async fn make_dirs(sftp: &SftpSession, dir: &[&str]) -> Result<()> {
        let mut path: Vec<&str> = Vec::new();
        for part in dir.iter().copied() {
            path.push(part);
            let current = join(&path);
            if current.is_empty() || current == "/" {
                continue;
            }
            if let Err(error) = sftp.create_dir(current.as_str()).await {
                let is_dir = sftp
                    .metadata(current.as_str())
                    .await
                    .is_ok_and(|m| m.is_dir());
                if !is_dir {
                    return Err(
                        Permanent(format!("cannot create folder {current}: {error}")).into(),
                    );
                }
            }
        }
        Ok(())
    }

    async fn put(
        sftp: &SftpSession,
        server: &SftpServer,
        location: &str,
        path: &Path,
        key: &str,
        sha256: &str,
        size: u64,
    ) -> Result<()> {
        let (folder, name) = key.rsplit_once('/').unwrap_or(("", key));
        let dir = join(&[server.dir.as_str(), folder]);
        let mut parts = vec![if server.dir.starts_with('/') { "/" } else { "" }];
        parts.extend(dir.split('/'));
        Self::make_dirs(sftp, &parts).await?;
        let target = join(&[dir.as_str(), name]);
        let temp = join(&[dir.as_str(), &format!(".{name}.{}.part", &sha256[..16])]);
        let exists = || async {
            within(
                REQUEST_TIMEOUT,
                "checking a file",
                sftp.try_exists(target.as_str()),
            )
            .await
        };
        if exists().await? {
            return Err(Permanent(format!(
                "uploading: {key} already exists on {location}; it was not replaced"
            ))
            .into());
        }
        // A previous attempt may have left its temporary file, which would
        // make the exclusive open below fail forever.
        let _ = sftp.remove_file(temp.as_str()).await;
        let mut remote = within(
            REQUEST_TIMEOUT,
            "creating the temporary file",
            sftp.open_with_flags(
                temp.as_str(),
                OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            ),
        )
        .await?;
        let written: Result<()> = async {
            let file = std::fs::File::open(path)
                .map_err(|e| Permanent(format!("cannot open {}: {e}", path.display())))?;
            let mut reader = HashingReader::new(file.take(size));
            let mut buffer = vec![0; CHUNK];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                within(WRITE_TIMEOUT, "writing", remote.write_all(&buffer[..read])).await?;
            }
            within(WRITE_TIMEOUT, "finishing the file", remote.shutdown()).await?;
            let (actual, bytes) = reader.finish();
            if actual != sha256 || bytes != size {
                return Err(Permanent(format!(
                    "{} changed after it was queued; not uploaded",
                    path.display()
                ))
                .into());
            }
            within(
                REQUEST_TIMEOUT,
                "renaming the uploaded file",
                sftp.rename(temp.as_str(), target.as_str()),
            )
            .await
            .map_err(|error| {
                // SFTP version 3 rename refuses an existing target.
                anyhow!("{error:#}; {key} may already exist on {location}")
            })?;
            Ok(())
        }
        .await;
        if let Err(error) = written {
            let _ = sftp.remove_file(temp.as_str()).await;
            return Err(error);
        }
        let stored = within(
            REQUEST_TIMEOUT,
            "checking the upload",
            sftp.metadata(target.as_str()),
        )
        .await?
        .size;
        if stored != Some(size) {
            return Err(Permanent(format!(
                "{key} on {location} has {} bytes after upload, expected {size}",
                stored.map_or("unknown".into(), |s| s.to_string())
            ))
            .into());
        }
        Ok(())
    }
}

impl Uploader for SftpUploader {
    fn check(&mut self) -> Result<()> {
        self.ready()?;
        let (_, sftp) = self.conn.as_ref().expect("connected");
        let dir = self.server.dir.clone();
        let result = self.runtime.block_on(async {
            let parts: Vec<&str> = std::iter::once(if dir.starts_with('/') { "/" } else { "" })
                .chain(dir.split('/'))
                .collect();
            Self::make_dirs(sftp, &parts).await
        });
        if result.is_err() {
            self.conn = None;
        }
        result
    }

    fn upload(&mut self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
        check_request(path, key, sha256, size)?;
        self.ready()?;
        let (_, sftp) = self.conn.as_ref().expect("connected");
        let result = self.runtime.block_on(Self::put(
            sftp,
            &self.server,
            &self.location,
            path,
            key,
            sha256,
            size,
        ));
        if result.as_ref().is_err_and(super::retryable) {
            self.conn = None;
        }
        result
    }

    fn probe(&mut self) -> Result<String> {
        self.check()?;
        let (_, sftp) = self.conn.as_ref().expect("connected");
        let name = format!(".capturefab-write-test-{}", super::token());
        let path = join(&[self.server.dir.as_str(), &name]);
        self.runtime.block_on(async {
            let mut file = within(
                REQUEST_TIMEOUT,
                "writing a test file",
                sftp.open_with_flags(
                    path.as_str(),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                ),
            )
            .await?;
            within(WRITE_TIMEOUT, "writing a test file", file.write_all(b"x")).await?;
            within(WRITE_TIMEOUT, "writing a test file", file.shutdown()).await?;
            match sftp.remove_file(path.as_str()).await {
                Ok(()) => Ok(()),
                Err(error) if no_such_file(&error) => Ok(()),
                Err(error) => Err(anyhow!("removing the test file: {error}")),
            }
        })?;
        Ok(format!("wrote and removed {name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::sftp_test_server::TestServer;
    use super::*;
    use crate::destination::SecretSource;
    use crate::transfer::retryable;

    fn server(test: &TestServer) -> SftpServer {
        SftpServer {
            host: "127.0.0.1".into(),
            port: test.port,
            user: "cam".into(),
            dir: "in".into(),
            host_key: test.fingerprint.clone(),
            key_file: None,
            password: SecretSource::Keychain,
            keep_local: false,
        }
    }

    fn local(name: &str, data: &[u8]) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("capturefab-sftp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        let mut hash = HashingReader::new(data);
        std::io::copy(&mut hash, &mut std::io::sink()).unwrap();
        (path, hash.finish().0)
    }

    #[test]
    fn pinned_key_uploads_exclusively() {
        let test = TestServer::start();
        let mut uploader = SftpUploader::new(server(&test), Some("secret".into())).unwrap();
        let (path, sha) = local("a.png", b"sftp bytes\n");
        uploader.upload(&path, "run/a.png", &sha, 11).unwrap();
        assert_eq!(
            test.file("/home/cam/in/run/a.png").unwrap(),
            b"sftp bytes\n"
        );
        assert_eq!(test.files().len(), 1, "no temporary file left");
        assert!(uploader.probe().unwrap().starts_with("wrote and removed"));
        assert_eq!(test.files().len(), 1);
        assert_eq!(
            fetch_host_key("127.0.0.1", test.port).unwrap(),
            test.fingerprint
        );
    }

    #[test]
    fn mismatched_key_is_permanent_with_both_fingerprints() {
        let test = TestServer::start();
        let mut pinned = server(&test);
        pinned.host_key = format!("SHA256:{}", "B".repeat(43));
        let error = SftpUploader::new(pinned.clone(), Some("secret".into()))
            .unwrap()
            .check()
            .unwrap_err();
        assert!(!retryable(&error));
        let text = error.to_string();
        assert!(
            text.contains(&pinned.host_key) && text.contains(&test.fingerprint),
            "{text}"
        );
        assert!(text.starts_with("SFTP host key changed"));
    }

    #[test]
    fn existing_target_is_permanent_and_temp_removed() {
        let test = TestServer::start();
        test.put("/home/cam/in/a.png", b"original");
        let (path, sha) = local("exists.png", b"new");
        let error = SftpUploader::new(server(&test), Some("secret".into()))
            .unwrap()
            .upload(&path, "a.png", &sha, 3)
            .unwrap_err();
        assert!(!retryable(&error));
        assert!(error.to_string().contains("already exists"));
        assert_eq!(test.file("/home/cam/in/a.png").unwrap(), b"original");
        assert_eq!(test.files().len(), 1);
        // Content that changed after queueing leaves no temporary file.
        let (changed, _) = local("changed.png", b"abc");
        let error = SftpUploader::new(server(&test), Some("secret".into()))
            .unwrap()
            .upload(&changed, "changed.png", &"0".repeat(64), 3)
            .unwrap_err();
        assert!(!retryable(&error) && error.to_string().contains("changed after it was queued"));
        assert_eq!(test.files().len(), 1);
    }

    #[test]
    fn stale_temp_is_replaced_before_exclusive_open() {
        let test = TestServer::start();
        let (path, sha) = local("stale.png", b"fresh");
        test.put(
            &format!("/home/cam/in/.stale.png.{}.part", &sha[..16]),
            b"partial",
        );
        SftpUploader::new(server(&test), Some("secret".into()))
            .unwrap()
            .upload(&path, "stale.png", &sha, 5)
            .unwrap();
        assert_eq!(test.file("/home/cam/in/stale.png").unwrap(), b"fresh");
        assert_eq!(test.files().len(), 1);
    }

    #[test]
    fn bad_password_is_permanent() {
        let test = TestServer::start();
        let error = SftpUploader::new(server(&test), Some("wrong".into()))
            .unwrap()
            .check()
            .unwrap_err();
        assert!(!retryable(&error));
        assert_eq!(
            error.to_string(),
            format!("SFTP sign-in refused for cam@127.0.0.1:{}", test.port)
        );
        let refused = SftpUploader::new(
            SftpServer {
                port: 1,
                ..server(&test)
            },
            None,
        )
        .unwrap()
        .check()
        .unwrap_err();
        assert!(retryable(&refused));
        assert!(
            format!("{refused:#}")
                .to_lowercase()
                .contains("connection refused")
        );
    }
}
