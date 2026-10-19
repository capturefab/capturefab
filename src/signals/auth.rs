//! Local signal profiles and coordinator-bound credential generations.
//! Profile metadata never contains a password. Credentials are resolved only
//! for an explicit bind or immediately before helper stdin is written.
use super::Protocol;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::Zeroizing;

mod session;
mod store;
pub use session::{Session, SignalCredentials};
pub use store::{SecretStore, Store};
#[cfg(test)]
mod tests;

pub const ENV_PREFIX: &str = "CAPTUREFAB_SIGNAL_";
pub const KEYCHAIN_SERVICE: &str = "capturefab-signal";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRef {
    pub id: String,
    pub revision: String,
}
impl ProfileRef {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        validate_id(&self.revision)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthBinding {
    pub profile: ProfileRef,
    pub epoch: String,
}
impl AuthBinding {
    pub fn validate(&self) -> Result<()> {
        self.profile.validate()?;
        validate_id(&self.epoch)
    }
}

/// A local receipt. Its endpoint has no user information; public/model views
/// still apply their ordinary endpoint/path redaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingReceipt {
    pub endpoint: String,
    pub reference: String,
    pub auth: AuthBinding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    None,
    Keychain,
    Environment,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretSource {
    None,
    Keychain,
    Environment { variable: String },
}
impl SecretSource {
    pub fn kind(&self) -> SecretKind {
        match self {
            Self::None => SecretKind::None,
            Self::Keychain => SecretKind::Keychain,
            Self::Environment { .. } => SecretKind::Environment,
        }
    }
}
impl fmt::Debug for SecretSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind().fmt(f)
    }
}

/// Protected local editing metadata. Deliberately not serializable as a
/// command, model fact or preference; the store uses a private record type.
#[derive(Clone, PartialEq, Eq)]
pub struct ProfileDraft {
    pub name: String,
    pub endpoint: String,
    pub username: Option<String>,
    pub secret: SecretSource,
}
impl ProfileDraft {
    pub fn validate(&self) -> Result<()> {
        crate::ipc::validate_name(&self.name)?;
        validate_endpoint(&self.endpoint)?;
        if let SecretSource::Environment { variable } = &self.secret {
            ensure!(
                variable.strip_prefix(ENV_PREFIX).is_some_and(|tail| {
                    (1..=48).contains(&tail.len())
                        && tail.bytes().all(|byte| {
                            byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
                        })
                }),
                "signal password environment names must use CAPTUREFAB_SIGNAL_ followed by 1..48 uppercase letters, digits or underscores"
            );
        }
        match &self.secret {
            SecretSource::None => ensure!(
                self.username.is_none(),
                "anonymous signal profiles do not take a user name"
            ),
            _ => {
                let parsed = super::parse(&self.endpoint)?;
                ensure!(
                    matches!(parsed.protocol, Protocol::Mqtt | Protocol::Opcua),
                    "only MQTT and OPC UA profiles support a user name and password"
                );
                validate_username(self.username.as_deref().unwrap_or_default())?;
            }
        }
        Ok(())
    }
}
impl fmt::Debug for ProfileDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileDraft")
            .field("endpoint", &crate::media::redact_url(&self.endpoint))
            .field("has_username", &self.username.is_some())
            .field("secret", &self.secret.kind())
            .finish_non_exhaustive()
    }
}

/// Safe local metadata returned by listing and saving. No credential lookup
/// occurs when this DTO is constructed from the profile file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInfo {
    pub profile: ProfileRef,
    pub name: String,
    pub endpoint: String,
    pub reference: String,
    pub protocol: Protocol,
    pub secret: SecretKind,
    pub insecure: bool,
}

/// A password supplied locally or resolved at execution. It cannot be
/// serialized and its bytes are erased on drop.
pub struct Secret(Zeroizing<String>);
impl Secret {
    pub fn new(value: Zeroizing<String>) -> Result<Self> {
        ensure!(
            (1..=4096).contains(&value.len()) && !value.chars().any(char::is_control),
            "the signal password must contain 1..4096 bytes without control characters"
        );
        Ok(Self(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

fn validate_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "signal profile identities must contain 32 lowercase hexadecimal characters"
    );
    Ok(())
}

pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    ensure!(
        crate::tools::redact::for_output_text(endpoint) == endpoint,
        "signal profile endpoints cannot contain passwords or tokens; use the protected local credential fields"
    );
    #[cfg(feature = "ai")]
    ensure!(
        !crate::assistant::prompt::contains_secret(endpoint),
        "signal profile endpoints cannot contain passwords or tokens; use the protected local credential fields"
    );
    let parsed = super::parse(endpoint).map_err(|_| {
        anyhow::anyhow!("the signal profile endpoint must be a valid credential-free signal URI")
    })?;
    let authority = endpoint.split_once("://").map_or("", |(_, tail)| {
        tail.split(['/', '?', '#']).next().unwrap_or("")
    });
    ensure!(
        parsed.user.is_none() && parsed.password.is_none() && !authority.contains('@'),
        "signal profiles require a credential-free endpoint; enter the user name and password in their local fields"
    );
    Ok(())
}

fn validate_username(username: &str) -> Result<()> {
    ensure!(
        (1..=256).contains(&username.len()) && !username.chars().any(char::is_control),
        "signal user names must contain 1..256 bytes without control characters"
    );
    Ok(())
}

fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot create a signal credential identity"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
