//! FTP and explicit-FTPS uploads over a synchronous client (no async
//! runtime). TLS uses rustls with ring and the OS trust store.
//!
//! FTP has no atomic create-if-absent, so a file is never written under its
//! final name: it is stored as `.NAME.HASH.part`, checked, and renamed only
//! while the final name is still free. RNTO on most servers replaces an
//! existing file, so a writer racing between that check and the rename can
//! still be overwritten; give each Capturefab destination its own folder.
use super::{HashingReader, Permanent, Uploader, check_request};
use crate::destination::{FtpServer, FtpTls, Remote};
use anyhow::{Result, anyhow};
use std::{
    fs::File,
    io::Read,
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    sync::Arc,
    time::Duration,
};
use suppaftp::{FtpError, Mode, RustlsConnector, RustlsFtpStream, types::FileType};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A stalled control or data channel fails after this long.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

pub struct FtpUploader {
    server: FtpServer,
    password: String,
    /// `ftp(s)://host:port/dir`, for messages; never the user or password.
    location: String,
    tls: Option<Arc<rustls::ClientConfig>>,
    /// The open connection and the folder it signed in to.
    conn: Option<(RustlsFtpStream, String)>,
}

/// TLS for explicit FTPS: rustls with ring and the OS certificate store.
fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_platform_verifier()?
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The reply code of a refused command, if it was one.
fn code(error: &FtpError) -> Option<u32> {
    match error {
        FtpError::UnexpectedResponse(response) => Some(response.status.code()),
        _ => None,
    }
}

/// The TLS failure behind a broken control or data channel, if it was one.
/// rustls handshakes lazily, so an untrusted certificate surfaces as an I/O
/// error on the first command after AUTH TLS (or on a data connection).
fn tls_failure(error: &FtpError) -> Option<&rustls::Error> {
    match error {
        FtpError::ConnectionError(io) => io.get_ref()?.downcast_ref::<rustls::Error>(),
        _ => None,
    }
}

/// The server's reply text, or the client error.
fn detail(error: &FtpError) -> String {
    if let Some(tls) = tls_failure(error) {
        return match tls {
            rustls::Error::InvalidCertificate(rustls::CertificateError::Other(other)) => {
                format!("TLS certificate rejected: {other}")
            }
            rustls::Error::InvalidCertificate(reason) => {
                format!("TLS certificate rejected: {reason:?}")
            }
            other => format!("TLS failed: {other}"),
        };
    }
    match error {
        FtpError::UnexpectedResponse(response) => {
            let body = String::from_utf8_lossy(&response.body);
            let body = body.trim();
            if body.is_empty() {
                format!("reply {}", response.status.code())
            } else {
                format!("reply {body}")
            }
        }
        other => other.to_string(),
    }
}

impl FtpUploader {
    /// `secret` is the password; without one, sign in anonymously.
    pub fn new(server: FtpServer, secret: Option<String>) -> Result<Self> {
        server.validate()?;
        let tls = match server.tls {
            FtpTls::Explicit => Some(tls_config()?),
            FtpTls::Plain => None,
        };
        Ok(Self {
            location: Remote::Ftp(server.clone()).location(),
            server,
            password: secret.unwrap_or_else(|| "anonymous@".into()),
            tls,
            conn: None,
        })
    }

    fn server_name(&self) -> String {
        format!(
            "{}://{}:{}",
            if self.tls.is_some() { "ftps" } else { "ftp" },
            self.server.host,
            self.server.port
        )
    }

    /// Classify a failed command: refusals (5xx) and TLS failures need a
    /// settings change; connection trouble and 4xx replies are retried.
    fn fail(&self, what: &str, error: FtpError) -> anyhow::Error {
        let message = format!("{what} on {}: {}", self.server_name(), detail(&error));
        if let Some(rustls::Error::InvalidCertificate(_)) = tls_failure(&error) {
            // Retrying cannot fix trust; the operator has to act.
            return Permanent(format!(
                "{message}; trust the server's certificate (or the CA that signed it) in the OS certificate store, or use --plain only on a trusted network"
            ))
            .into();
        }
        match (&error, code(&error)) {
            (FtpError::SecureError(_), _) => Permanent(message).into(),
            (_, _)
                if matches!(
                    tls_failure(&error),
                    Some(rustls::Error::PeerIncompatible(_) | rustls::Error::AlertReceived(_))
                ) =>
            {
                Permanent(message).into()
            }
            (_, Some(500..=599)) => Permanent(message).into(),
            _ => anyhow!(message),
        }
    }

    fn connect(&self) -> Result<(RustlsFtpStream, String)> {
        let host = self
            .server
            .host
            .trim_start_matches('[')
            .trim_end_matches(']');
        let addresses: Vec<_> = (host, self.server.port)
            .to_socket_addrs()
            .map_err(|e| anyhow!("cannot resolve {}: {e}", self.server_name()))?
            .collect();
        let mut last = None;
        let mut connected = None;
        for address in addresses {
            match RustlsFtpStream::connect_timeout(address, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    connected = Some((stream, address));
                    break;
                }
                Err(error) => last = Some(error),
            }
        }
        let (stream, address) = match (connected, last) {
            (Some(found), _) => found,
            (None, Some(error)) => return Err(self.fail("connecting", error)),
            (None, None) => {
                return Err(anyhow!("{} has no address", self.server_name()));
            }
        };
        let mut stream = stream.passive_stream_builder(|address| {
            let socket = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
                .map_err(FtpError::ConnectionError)?;
            socket
                .set_read_timeout(Some(IO_TIMEOUT))
                .and_then(|()| socket.set_write_timeout(Some(IO_TIMEOUT)))
                .map_err(FtpError::ConnectionError)?;
            Ok(socket)
        });
        {
            let socket = stream.get_ref();
            socket.set_read_timeout(Some(IO_TIMEOUT))?;
            socket.set_write_timeout(Some(IO_TIMEOUT))?;
        }
        // Servers behind NAT often announce a private address for passive
        // connections; reuse the control connection's address instead.
        stream.set_passive_nat_workaround(true);
        stream.set_mode(if address.is_ipv6() {
            Mode::ExtendedPassive
        } else {
            Mode::Passive
        });
        if let Some(tls) = &self.tls {
            stream = stream
                .into_secure(RustlsConnector::from(tls.clone()), host)
                .map_err(|error| match code(&error) {
                    Some(500..=599) => Permanent(format!(
                        "{} does not offer FTPS (AUTH TLS refused: {}); use --plain only on a trusted network",
                        self.server_name(),
                        detail(&error)
                    ))
                    .into(),
                    _ => self.fail("starting TLS", error),
                })?;
        }
        stream
            .login(self.server.user.as_str(), self.password.as_str())
            .map_err(|error| match code(&error) {
                Some(500..=599) => Permanent(format!(
                    "FTP sign-in refused for {}@{}:{} ({})",
                    self.server.user,
                    self.server.host,
                    self.server.port,
                    detail(&error)
                ))
                .into(),
                _ => self.fail("signing in", error),
            })?;
        // Binary transfers: the default ASCII type would rewrite line endings.
        stream
            .transfer_type(FileType::Binary)
            .map_err(|e| self.fail("selecting binary transfers", e))?;
        let home = stream
            .pwd()
            .map_err(|e| self.fail("reading the login folder", e))?;
        Ok((stream, home))
    }

    /// A connection in its login folder: the cached one if it still
    /// answers, otherwise a new one.
    fn ready(&mut self) -> Result<&mut RustlsFtpStream> {
        if let Some((stream, home)) = &mut self.conn
            && (stream.noop().is_err() || stream.cwd(home.as_str()).is_err())
        {
            self.conn = None;
        }
        if self.conn.is_none() {
            self.conn = Some(self.connect()?);
        }
        Ok(&mut self.conn.as_mut().expect("connected").0)
    }

    /// Enter the destination folder plus `sub`, creating folders on the way.
    fn enter(&mut self, sub: &[&str]) -> Result<()> {
        let dir = self.server.dir.clone();
        let absolute = dir.starts_with('/');
        let parts: Vec<String> = dir
            .split('/')
            .chain(sub.iter().copied())
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        let name = self.server_name();
        let stream = self.ready()?;
        if absolute {
            stream
                .cwd("/")
                .map_err(|e| anyhow!("entering / on {name}: {}", detail(&e)))?;
        }
        for part in &parts {
            if stream.cwd(part).is_ok() {
                continue;
            }
            // Another uploader may create it at the same moment; only the
            // final CWD decides.
            let made = stream.mkdir(part);
            if let Err(error) = stream.cwd(part) {
                let error = made.err().unwrap_or(error);
                return Err(self.fail(&format!("creating folder {part}"), error));
            }
        }
        Ok(())
    }

    /// Whether `name` exists in the current folder: SIZE, then MLST.
    fn exists(&mut self, name: &str) -> Result<bool> {
        Ok(self.remote_size(name)?.is_some())
    }

    /// The size of `name` in the current folder, or `None` if it is absent.
    fn remote_size(&mut self, name: &str) -> Result<Option<u64>> {
        let stream = &mut self.conn.as_mut().expect("connected").0;
        let unsupported = |e: &FtpError| matches!(code(e), Some(500 | 502 | 504));
        match stream.size(name) {
            Ok(size) => return Ok(Some(size as u64)),
            Err(e) if code(&e) == Some(550) => return Ok(None),
            Err(e) if unsupported(&e) => {}
            Err(e) => return Err(self.fail("checking a file", e)),
        }
        match stream.mlst(Some(name)) {
            Ok(facts) => Ok(Some(
                facts
                    .split(';')
                    .find_map(|fact| fact.trim().strip_prefix("size="))
                    .and_then(|size| size.parse().ok())
                    .unwrap_or(u64::MAX),
            )),
            Err(e) if code(&e) == Some(550) => Ok(None),
            Err(e) if unsupported(&e) => Err(Permanent(format!(
                "{} supports neither SIZE nor MLST; capturefab cannot guarantee it will not overwrite",
                self.server_name()
            ))
            .into()),
            Err(e) => Err(self.fail("checking a file", e)),
        }
    }

    /// Whether `name` already holds exactly these bytes: an earlier attempt
    /// whose rename went through but whose reply was lost.
    fn holds(&mut self, name: &str, sha256: &str, size: u64) -> Result<bool> {
        if self.remote_size(name)? != Some(size) {
            return Ok(false);
        }
        let stream = &mut self.conn.as_mut().expect("connected").0;
        let read = stream.retr(name, |data| {
            let mut reader = HashingReader::new(data.take(size.saturating_add(1)));
            std::io::copy(&mut reader, &mut std::io::sink()).map_err(FtpError::ConnectionError)?;
            Ok(reader.finish())
        });
        match read {
            Ok((actual, bytes)) => Ok(actual == sha256 && bytes == size),
            Err(error) => Err(self.fail("checking an existing file", error)),
        }
    }

    fn exists_error(&self, key: &str) -> anyhow::Error {
        Permanent(format!(
            "uploading: {key} already exists on {}; it was not replaced",
            self.location
        ))
        .into()
    }

    fn try_upload(&mut self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
        let (folders, name) = key.rsplit_once('/').unwrap_or(("", key));
        let folders: Vec<&str> = folders.split('/').collect();
        self.enter(&folders)?;
        if self.exists(name)? {
            if self.holds(name, sha256, size)? {
                return Ok(());
            }
            return Err(self.exists_error(key));
        }
        let temp = format!(".{name}.{}.part", &sha256[..16]);
        let stream = &mut self.conn.as_mut().expect("connected").0;
        // A previous attempt may have left its temporary file.
        let _ = stream.rm(&temp);
        let file = File::open(path)
            .map_err(|e| Permanent(format!("cannot open {}: {e}", path.display())))?;
        let mut reader = HashingReader::new(file.take(size));
        let stored = stream.put_file(&temp, &mut reader);
        let (actual, bytes) = reader.finish();
        if let Err(error) = stored {
            let _ = stream.rm(&temp);
            return Err(self.fail("storing the file", error));
        }
        if actual != sha256 || bytes != size {
            let _ = stream.rm(&temp);
            return Err(Permanent(format!(
                "{} changed after it was queued; not uploaded",
                path.display()
            ))
            .into());
        }
        let stored = self.remote_size(&temp)?;
        let stream = &mut self.conn.as_mut().expect("connected").0;
        if stored != Some(size) {
            let _ = stream.rm(&temp);
            return Err(anyhow!(
                "the server stored {} of {size} bytes for {key}; retrying",
                stored.map_or("none".into(), |s| s.to_string())
            ));
        }
        // Checked again just before the rename, which would replace it.
        if self.exists(name)? {
            let _ = self.conn.as_mut().expect("connected").0.rm(&temp);
            return Err(self.exists_error(key));
        }
        let stream = &mut self.conn.as_mut().expect("connected").0;
        if let Err(error) = stream.rename(temp.as_str(), name) {
            let _ = stream.rm(&temp);
            return Err(self.fail("renaming the uploaded file", error));
        }
        Ok(())
    }
}

impl Uploader for FtpUploader {
    fn check(&mut self) -> Result<()> {
        self.enter(&[])
    }

    fn upload(&mut self, path: &Path, key: &str, sha256: &str, size: u64) -> Result<()> {
        check_request(path, key, sha256, size)?;
        let result = self.try_upload(path, key, sha256, size);
        if result.is_err() {
            // Start the next attempt on a fresh connection.
            self.conn = None;
        }
        result
    }

    fn probe(&mut self) -> Result<String> {
        self.enter(&[])?;
        let name = format!(".capturefab-write-test-{}", super::token());
        let stream = &mut self.conn.as_mut().expect("connected").0;
        if let Err(error) = stream.put_file(&name, &mut &b"x"[..]) {
            return Err(self.fail("writing a test file", error));
        }
        stream
            .rm(&name)
            .map_err(|e| self.fail("removing the test file", e))?;
        Ok(format!("wrote and removed {name}"))
    }
}

#[cfg(test)]
#[path = "../../tests/support/fake_ftp.rs"]
mod fake_ftp;

#[cfg(test)]
mod tests {
    use super::fake_ftp::{FakeFtp, HOME};
    use super::*;
    use crate::destination::SecretSource;
    use crate::transfer::retryable;

    fn server(port: u16) -> FtpServer {
        FtpServer {
            host: "127.0.0.1".into(),
            port,
            user: "cam".into(),
            dir: "in".into(),
            tls: FtpTls::Plain,
            password: SecretSource::Keychain,
            keep_local: false,
        }
    }

    fn uploader(fake: &FakeFtp) -> FtpUploader {
        FtpUploader::new(server(fake.port), Some("secret".into())).unwrap()
    }

    /// A local file and its SHA-256.
    fn local(name: &str, data: &[u8]) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("capturefab-ftp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        let mut hash = HashingReader::new(data);
        std::io::copy(&mut hash, &mut std::io::sink()).unwrap();
        (path, hash.finish().0)
    }

    #[test]
    fn uploads_via_temp_and_rename_creating_dirs() {
        let fake = FakeFtp::start(|_| {});
        let (path, sha) = local("a.png", b"image bytes");
        uploader(&fake)
            .upload(&path, "run/1/a.png", &sha, 11)
            .unwrap();
        assert_eq!(
            fake.file(&format!("{HOME}/in/run/1/a.png")).unwrap(),
            b"image bytes"
        );
        let commands = fake.commands();
        let stored = commands.iter().position(|c| c.starts_with("STOR")).unwrap();
        assert!(commands[stored].starts_with(&format!("STOR .a.png.{}.part", &sha[..16])));
        assert!(commands[stored..].iter().any(|c| c == "RNTO a.png"));
        assert!(commands.iter().any(|c| c == "TYPE I"));
        assert_eq!(fake.state.lock().unwrap().files.len(), 1);
    }

    #[test]
    fn binary_type_keeps_lf_bytes() {
        let fake = FakeFtp::start(|_| {});
        let data = b"line\nline\r\n\n\x00\xff";
        let (path, sha) = local("lf.bin", data);
        uploader(&fake)
            .upload(&path, "lf.bin", &sha, data.len() as u64)
            .unwrap();
        assert_eq!(fake.file(&format!("{HOME}/in/lf.bin")).unwrap(), data);
    }

    #[test]
    fn existing_target_is_permanent_and_never_stored() {
        let fake = FakeFtp::start(|s| {
            s.dirs.insert(format!("{HOME}/in"));
            s.files
                .insert(format!("{HOME}/in/a.png"), b"original".to_vec());
        });
        let (path, sha) = local("exists.png", b"new");
        let error = uploader(&fake).upload(&path, "a.png", &sha, 3).unwrap_err();
        assert!(!retryable(&error));
        assert!(error.to_string().contains("already exists"), "{error:#}");
        assert!(!fake.commands().iter().any(|c| c.starts_with("STOR")));
        assert_eq!(fake.file(&format!("{HOME}/in/a.png")).unwrap(), b"original");
    }

    #[test]
    fn mlst_fallback_when_size_unsupported() {
        let fake = FakeFtp::start(|s| s.size = false);
        let (path, sha) = local("m.png", b"mlst");
        let mut uploader = uploader(&fake);
        uploader.upload(&path, "m.png", &sha, 4).unwrap();
        assert!(fake.commands().iter().any(|c| c.starts_with("MLST")));
        assert_eq!(fake.file(&format!("{HOME}/in/m.png")).unwrap(), b"mlst");
        let (other, other_sha) = local("m2.png", b"mlsx");
        let error = uploader.upload(&other, "m.png", &other_sha, 4).unwrap_err();
        assert!(!retryable(&error) && error.to_string().contains("already exists"));
    }

    #[test]
    fn retry_after_a_lost_rename_reply_completes() {
        // The first attempt's rename went through but its reply was lost:
        // the identical file under the final name counts as uploaded.
        let fake = FakeFtp::start(|_| {});
        let (path, sha) = local("done.png", b"same bytes");
        let mut uploader = uploader(&fake);
        uploader.upload(&path, "done.png", &sha, 10).unwrap();
        let stored = fake.commands().len();
        uploader.upload(&path, "done.png", &sha, 10).unwrap();
        assert!(
            !fake.commands()[stored..]
                .iter()
                .any(|c| c.starts_with("STOR") || c.starts_with("RNTO"))
        );
        assert_eq!(
            fake.file(&format!("{HOME}/in/done.png")).unwrap(),
            b"same bytes"
        );
    }

    #[test]
    fn neither_size_nor_mlst_is_permanent() {
        let fake = FakeFtp::start(|s| {
            s.size = false;
            s.mlst = false;
        });
        let (path, sha) = local("n.png", b"none");
        let error = uploader(&fake).upload(&path, "n.png", &sha, 4).unwrap_err();
        assert!(!retryable(&error));
        assert!(error.to_string().contains("neither SIZE nor MLST"));
        assert!(!fake.commands().iter().any(|c| c.starts_with("STOR")));
    }

    #[test]
    fn login_530_is_permanent() {
        let fake = FakeFtp::start(|_| {});
        let mut uploader = FtpUploader::new(server(fake.port), Some("wrong".into())).unwrap();
        let error = uploader.check().unwrap_err();
        assert!(!retryable(&error));
        assert!(
            error
                .to_string()
                .starts_with("FTP sign-in refused for cam@127.0.0.1:")
        );
        assert!(!error.to_string().contains("wrong"));
    }

    #[test]
    fn refused_connection_and_421_are_retryable() {
        let mut refused = FtpUploader::new(server(1), None).unwrap();
        let error = refused.check().unwrap_err();
        assert!(retryable(&error));
        assert!(
            format!("{error:#}")
                .to_lowercase()
                .contains("connection refused")
        );
        let fake = FakeFtp::start(|s| s.greeting = 421);
        let error = uploader(&fake).check().unwrap_err();
        assert!(retryable(&error), "{error:#}");
    }

    #[test]
    fn stale_part_file_removed_on_retry() {
        let (path, sha) = local("stale.png", b"fresh");
        let temp = format!("{HOME}/in/.stale.png.{}.part", &sha[..16]);
        let fake = FakeFtp::start(|s| {
            s.dirs.insert(format!("{HOME}/in"));
            s.files.insert(temp.clone(), b"partial garbage".to_vec());
        });
        uploader(&fake).upload(&path, "stale.png", &sha, 5).unwrap();
        let commands = fake.commands();
        let deleted = commands
            .iter()
            .position(|c| c.starts_with("DELE .stale.png"))
            .unwrap();
        let stored = commands.iter().position(|c| c.starts_with("STOR")).unwrap();
        assert!(deleted < stored);
        assert!(fake.file(&temp).is_none());
        assert_eq!(
            fake.file(&format!("{HOME}/in/stale.png")).unwrap(),
            b"fresh"
        );
    }

    #[test]
    fn content_change_during_upload_deletes_temp() {
        let fake = FakeFtp::start(|_| {});
        let (path, _) = local("changed.png", b"12345");
        let wrong = "0".repeat(64);
        let error = uploader(&fake)
            .upload(&path, "changed.png", &wrong, 5)
            .unwrap_err();
        assert!(!retryable(&error));
        assert!(error.to_string().contains("changed after it was queued"));
        assert!(fake.state.lock().unwrap().files.is_empty());
    }

    #[test]
    fn cached_connection_returns_to_home_dir() {
        let fake = FakeFtp::start(|_| {});
        let mut uploader = uploader(&fake);
        let (a, sha_a) = local("one.png", b"one");
        let (b, sha_b) = local("two.png", b"two");
        uploader.upload(&a, "deep/x/one.png", &sha_a, 3).unwrap();
        uploader.upload(&b, "two.png", &sha_b, 3).unwrap();
        assert_eq!(fake.state.lock().unwrap().logins, 1, "connection reused");
        assert_eq!(fake.file(&format!("{HOME}/in/two.png")).unwrap(), b"two");
        assert_eq!(
            fake.file(&format!("{HOME}/in/deep/x/one.png")).unwrap(),
            b"one"
        );
        assert!(uploader.probe().unwrap().starts_with("wrote and removed"));
        assert_eq!(fake.state.lock().unwrap().files.len(), 2);
    }

    #[test]
    fn untrusted_tls_certificate_is_permanent_and_readable() {
        let uploader = FtpUploader::new(server(21), None).unwrap();
        let untrusted = |reason| {
            FtpError::ConnectionError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                rustls::Error::InvalidCertificate(reason),
            ))
        };
        let error = uploader.fail(
            "starting TLS",
            untrusted(rustls::CertificateError::UnknownIssuer),
        );
        assert!(!retryable(&error), "{error:#}");
        let message = error.to_string();
        assert!(
            message.contains("TLS certificate rejected: UnknownIssuer"),
            "{message}"
        );
        assert!(message.contains("OS certificate store"), "{message}");
        let platform = rustls::CertificateError::Other(rustls::OtherError(std::sync::Arc::new(
            std::io::Error::other("“127.0.0.1” certificate is not trusted"),
        )));
        let message = uploader
            .fail("starting TLS", untrusted(platform))
            .to_string();
        assert!(
            message.contains("TLS certificate rejected: “127.0.0.1” certificate is not trusted"),
            "{message}"
        );
        assert!(!message.contains("OtherError"), "{message}");
        // A dropped connection is still retried.
        let reset = FtpError::ConnectionError(std::io::ErrorKind::ConnectionReset.into());
        assert!(retryable(&uploader.fail("storing the file", reset)));
    }

    #[test]
    fn absolute_folders_start_at_the_root_and_tls_config_builds() {
        let fake = FakeFtp::start(|s| {
            s.dirs.insert("/srv".into());
        });
        let mut absolute = server(fake.port);
        absolute.dir = "/srv/drop".into();
        let mut uploader = FtpUploader::new(absolute, Some("secret".into())).unwrap();
        let (path, sha) = local("abs.png", b"abs");
        uploader.upload(&path, "abs.png", &sha, 3).unwrap();
        assert_eq!(fake.file("/srv/drop/abs.png").unwrap(), b"abs");
        let mut explicit = server(fake.port);
        explicit.tls = FtpTls::Explicit;
        let mut tls = FtpUploader::new(explicit, Some("secret".into())).unwrap();
        assert!(tls.tls.is_some());
        let error = tls.check().unwrap_err();
        assert!(!retryable(&error) && error.to_string().contains("does not offer FTPS"));
    }
}
