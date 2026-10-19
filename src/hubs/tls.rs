//! TLS for hub requests: rustls with ring, trusting either the OS certificate
//! store (optionally against a different DNS name than the URL host) or
//! exactly one pinned certificate. A refused certificate reports its SHA-256
//! fingerprint, so the user can confirm it with the administrator and pin it.
use anyhow::Result;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, OtherError,
    SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex, PoisonError};

/// What a hub's certificate is checked against.
#[derive(Clone, Debug)]
pub(crate) enum Trust {
    /// The OS certificate store, for the URL host.
    Platform,
    /// The OS certificate store, for another DNS name (a certificate issued to
    /// a domain, reached by LAN address).
    ServerName(String),
    /// Exactly this certificate; nothing else is accepted.
    Pinned([u8; 32]),
    /// Test roots in place of the OS store.
    #[cfg(test)]
    Roots(Arc<rustls::RootCertStore>),
}

impl Trust {
    pub(crate) fn of(hub: &super::Hub) -> Self {
        if let Some(pin) = hub.pin() {
            Self::Pinned(pin)
        } else if let Some(name) = &hub.tls_server_name {
            Self::ServerName(name.clone())
        } else {
            Self::Platform
        }
    }
}

/// Why a certificate was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    Untrusted,
    NameMismatch,
    Changed { expected: [u8; 32] },
}

/// A refused certificate, carried inside the connection's own TLS error so
/// concurrent connections cannot mix up their reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CertRejected {
    pub offered: [u8; 32],
    pub reason: Reason,
}
impl std::fmt::Display for CertRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("certificate refused")
    }
}
impl std::error::Error for CertRejected {}

impl CertRejected {
    /// The message for a hub; never mentions anything a server could choose
    /// except its certificate's fingerprint.
    pub(crate) fn describe(&self, label: &str, authority: &str) -> String {
        let offered = super::registry::format_fingerprint(&self.offered);
        match self.reason {
            Reason::Untrusted => format!(
                "the certificate of {authority} is not trusted ({offered}); if the administrator confirms this fingerprint, add {label} again with --certificate {offered} or --accept-certificate"
            ),
            Reason::NameMismatch => format!(
                "the certificate of {authority} does not match that address ({offered}); add {label} again with --tls-server-name NAME (the name on the certificate), --certificate or --accept-certificate"
            ),
            Reason::Changed { expected } => format!(
                "{label} presented a different certificate: expected {}, offered {offered}; confirm with the server administrator and add the hub again with --certificate {offered}",
                super::registry::format_fingerprint(&expected)
            ),
        }
    }
}

pub(crate) fn fingerprint(certificate: &[u8]) -> [u8; 32] {
    Sha256::digest(certificate).into()
}

fn refused(offered: [u8; 32], reason: Reason) -> TlsError {
    TlsError::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(
        CertRejected { offered, reason },
    ))))
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts exactly one certificate, ignoring names, chains and dates, and
/// still checks that the server holds its private key.
#[derive(Debug)]
struct Pinned {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}
impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let offered = fingerprint(end_entity);
        if offered == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(refused(offered, Reason::Changed { expected: self.pin }))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The OS verifier, reporting a refused certificate's fingerprint.
#[derive(Debug)]
struct Reporting(Arc<dyn ServerCertVerifier>);
impl ServerCertVerifier for Reporting {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        self.0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
            .map_err(|error| {
                let reason = match error {
                    TlsError::InvalidCertificate(
                        CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. },
                    ) => Reason::NameMismatch,
                    _ => Reason::Untrusted,
                };
                refused(fingerprint(end_entity), reason)
            })
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Records the certificate a server offers and then ends the handshake, so
/// no application data is ever sent: used to show a fingerprint before the
/// user decides to trust it.
#[derive(Debug)]
struct Capture {
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    provider: Arc<CryptoProvider>,
}
impl ServerCertVerifier for Capture {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint(end_entity));
        Err(TlsError::General("certificate recorded".into()))
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("certificate recorded".into()))
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("certificate recorded".into()))
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn with_verifier(verifier: Arc<dyn ServerCertVerifier>) -> Result<Arc<ClientConfig>> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// A client configuration for `trust`.
pub(crate) fn client_config(trust: &Trust) -> Result<Arc<ClientConfig>> {
    match trust {
        Trust::Pinned(pin) => with_verifier(Arc::new(Pinned {
            pin: *pin,
            provider: provider(),
        })),
        Trust::Platform | Trust::ServerName(_) => with_verifier(Arc::new(Reporting(Arc::new(
            rustls_platform_verifier::Verifier::new(provider())?,
        )))),
        #[cfg(test)]
        Trust::Roots(roots) => with_verifier(Arc::new(Reporting(
            rustls::client::WebPkiServerVerifier::builder_with_provider(roots.clone(), provider())
                .build()?,
        ))),
    }
}

/// A configuration that only records the server's certificate, and where
/// the fingerprint lands.
/// Where a capturing handshake leaves the fingerprint it saw.
pub(crate) type Seen = Arc<Mutex<Option<[u8; 32]>>>;

pub(crate) fn capture_config() -> Result<(Arc<ClientConfig>, Seen)> {
    let seen = Arc::new(Mutex::new(None));
    let config = with_verifier(Arc::new(Capture {
        seen: seen.clone(),
        provider: provider(),
    }))?;
    Ok((config, seen))
}

/// The name a certificate is checked against and sent as SNI (no SNI is sent
/// for IP addresses).
pub(crate) fn server_name(host: &str, trust: &Trust) -> Result<ServerName<'static>> {
    let name = match trust {
        Trust::ServerName(name) => name.as_str(),
        _ => host,
    };
    ServerName::try_from(name.to_owned()).map_err(|_| {
        anyhow::Error::from(crate::dataset::Usage(format!(
            "{name} cannot be checked against a TLS certificate"
        )))
    })
}

/// The refused certificate in a TLS error, if that is why it failed.
pub(crate) fn rejection(error: &TlsError) -> Option<CertRejected> {
    match error {
        TlsError::InvalidCertificate(CertificateError::Other(OtherError(inner))) => {
            inner.downcast_ref::<CertRejected>().copied()
        }
        _ => None,
    }
}
