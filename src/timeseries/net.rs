//! Shared native protocol transport: every socket/TLS operation uses one total deadline.
use crate::types::Usage;
#[cfg(not(feature = "mqtt-tls"))]
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde::Deserialize;
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::Instant,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsConfig {
    /// Certificate identity and SNI; the endpoint socket remains numeric.
    pub server_name: String,
    #[serde(default)]
    pub ca_certificate: Option<PathBuf>,
    #[serde(default)]
    pub client_certificate: Option<PathBuf>,
    #[serde(default)]
    pub client_key: Option<PathBuf>,
}

pub(crate) trait Transport: Read + Write + Send {
    fn set_deadline(&mut self, deadline: Instant);
}

struct Socket {
    stream: TcpStream,
    deadline: Instant,
}
impl Socket {
    fn remaining(&self) -> io::Result<std::time::Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "protocol I/O deadline expired",
            ));
        }
        Ok(remaining)
    }
}
impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}
impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
impl Transport for Socket {
    fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }
}
pub(crate) fn plain(stream: TcpStream, deadline: Instant) -> Box<dyn Transport> {
    Box::new(Socket { stream, deadline })
}

pub(crate) struct Prepared {
    #[cfg(feature = "mqtt-tls")]
    tls: Option<(
        std::sync::Arc<rustls::ClientConfig>,
        rustls::pki_types::ServerName<'static>,
    )>,
}

pub(crate) fn prepare(config: Option<&TlsConfig>) -> Result<Prepared> {
    let Some(config) = config else {
        return Ok(Prepared {
            #[cfg(feature = "mqtt-tls")]
            tls: None,
        });
    };
    if config.server_name.is_empty() {
        return Err(Usage("TLS server_name must be nonempty".into()).into());
    }
    if config.client_certificate.is_some() != config.client_key.is_some() {
        return Err(Usage(
            "TLS client_certificate and client_key must be supplied together".into(),
        )
        .into());
    }
    #[cfg(feature = "mqtt-tls")]
    {
        use rustls::pki_types::{PrivateKeyDer, ServerName, pem::PemObject as _};
        let server_name = ServerName::try_from(config.server_name.clone())
            .map_err(|_| Usage("TLS server_name must be a DNS name or IP address".into()))?;
        let mut roots = rustls::RootCertStore::empty();
        if let Some(path) = &config.ca_certificate {
            let certificates = certificates(path)?;
            for certificate in certificates {
                roots
                    .add(certificate)
                    .map_err(|_| Usage("invalid TLS CA certificate".into()))?;
            }
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let builder = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots);
        let tls = match (&config.client_certificate, &config.client_key) {
            (Some(certificate), Some(key)) => {
                let chain = certificates(certificate)?;
                let bytes = zeroize::Zeroizing::new(read_pem(key)?);
                let key = PrivateKeyDer::from_pem_slice(&bytes).map_err(|_| {
                    Usage(
                        "invalid TLS client key; use unencrypted PKCS#1, PKCS#8 or SEC1 PEM".into(),
                    )
                })?;
                builder.with_client_auth_cert(chain, key).map_err(|_| {
                    Usage("TLS client certificate/key are invalid or do not match".into())
                })?
            }
            _ => builder.with_no_client_auth(),
        };
        // Certificate verification is always enabled; custom roots replace the
        // public trust store so a plant CA does not silently broaden trust.
        Ok(Prepared {
            tls: Some((std::sync::Arc::new(tls), server_name)),
        })
    }
    #[cfg(not(feature = "mqtt-tls"))]
    {
        let _ = &config.ca_certificate;
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "TLS requires a build with the mqtt-tls feature",
        ))
    }
}

#[cfg(feature = "mqtt-tls")]
fn certificates(path: &std::path::Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::{CertificateDer, pem::PemObject as _};
    let bytes = read_pem(path)?;
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Usage("invalid TLS PEM certificate file".into()))?;
    if certificates.is_empty() {
        return Err(Usage("TLS certificate file contains no certificates".into()).into());
    }
    Ok(certificates)
}

#[cfg(feature = "mqtt-tls")]
const MAX_PEM: usize = 1 << 20;

#[cfg(feature = "mqtt-tls")]
fn read_pem(path: &std::path::Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((MAX_PEM + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PEM {
        return Err(Usage("TLS PEM file exceeds 1 MiB".into()).into());
    }
    Ok(bytes)
}

impl Prepared {
    #[cfg_attr(
        not(feature = "mqtt-tls"),
        expect(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "the shared transport interface retains fallible TLS state when the optional TLS feature is enabled"
        )
    )]
    pub(crate) fn connect(
        &self,
        stream: TcpStream,
        deadline: Instant,
    ) -> Result<Box<dyn Transport>> {
        #[cfg(feature = "mqtt-tls")]
        if let Some((config, name)) = &self.tls {
            let connection = rustls::ClientConnection::new(config.clone(), name.clone())?;
            return Ok(Box::new(rustls::StreamOwned::new(
                connection,
                Socket { stream, deadline },
            )));
        }
        Ok(plain(stream, deadline))
    }
}

#[cfg(feature = "mqtt-tls")]
impl Transport for rustls::StreamOwned<rustls::ClientConnection, Socket> {
    fn set_deadline(&mut self, deadline: Instant) {
        self.sock.deadline = deadline;
    }
}
