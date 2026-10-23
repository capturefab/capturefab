//! The OS credential store (macOS Keychain, Windows Credential Manager, the
//! Secret Service on Linux), shared by AI providers and camera hubs.
//! Only secrets go here; the settings that name them live in the state
//! directory.
use anyhow::{Result, anyhow, bail, ensure};

pub fn validate_secret(secret: &str) -> Result<()> {
    ensure!(
        !secret.is_empty() && secret.len() <= 4096,
        "secret must contain 1..4096 bytes"
    );
    Ok(())
}

/// A server without a desktop login may have no Secret Service (D-Bus session).
fn store_failed(action: &str, error: keyring::Error) -> anyhow::Error {
    anyhow!(
        "cannot {action} the OS credential store: {error}; without one, as on a server with no desktop login, use provider environment credentials or hub --secret-env (CAPTUREFAB_HUB_* variables)"
    )
}

/// Store a secret in the OS credential store.
pub fn store_secret(service: &str, account: &str, secret: &str) -> Result<()> {
    validate_secret(secret)?;
    keyring::Entry::new(service, account)
        .and_then(|entry| entry.set_password(secret))
        .map_err(|e| store_failed("save the secret in", e))
}

/// Read a secret. A missing entry is `None`; a locked or unavailable store is
/// an error, so a caller never mistakes it for an empty slot.
pub fn read_secret(service: &str, account: &str) -> Result<Option<String>> {
    match keyring::Entry::new(service, account).and_then(|entry| entry.get_password()) {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(store_failed("read the secret from", e)),
    }
}

pub fn delete_secret(service: &str, account: &str) -> Result<()> {
    match keyring::Entry::new(service, account).and_then(|entry| entry.inner.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => bail!("cannot remove the secret from the OS credential store: {e}"),
    }
}

/// Whether the OS credential store holds this secret.
pub fn has_secret(service: &str, account: &str) -> bool {
    keyring::Entry::new(service, account)
        .and_then(|entry| entry.get_password())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failed_credential_store_names_what_works_without_one() {
        let error = store_failed("save the secret in", keyring::Error::NoDefaultStore);
        let message = format!("{error:#}");
        for alternative in ["environment credentials", "CAPTUREFAB_HUB_*"] {
            assert!(message.contains(alternative), "{message}");
        }
        assert_eq!(
            crate::cli::error_code(&error),
            ("operation_failed", 1),
            "{message}"
        );
    }
}
