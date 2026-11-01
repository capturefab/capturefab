//! The OS credential store (macOS Keychain, Windows Credential Manager, the
//! Secret Service on Linux), for saved camera sign-ins.
//! Only secrets go here; the settings that name them live in the state
//! directory. Unit tests use a store in memory instead and never reach it.
#[cfg(not(test))]
use anyhow::{Result, bail, ensure};

/// A server without a desktop login may have no Secret Service (D-Bus session).
fn store_failed(action: &str, error: keyring::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot {action} the OS credential store: {error}; without one, as on a server with no desktop login, put camera credentials in the address"
    )
}

/// Store a secret in the OS credential store.
#[cfg(not(test))]
pub fn store_secret(service: &str, account: &str, secret: &str) -> Result<()> {
    ensure!(
        !secret.is_empty() && secret.len() <= 4096,
        "secret must contain 1..4096 bytes"
    );
    keyring::Entry::new(service, account)
        .and_then(|entry| entry.set_password(secret))
        .map_err(|e| store_failed("save the secret in", e))
}

/// Read a secret. A missing entry is `None`; a locked or unavailable store is
/// an error, so a caller never mistakes it for an empty slot.
#[cfg(not(test))]
pub fn read_secret(service: &str, account: &str) -> Result<Option<String>> {
    match keyring::Entry::new(service, account).and_then(|entry| entry.get_password()) {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(store_failed("read the secret from", e)),
    }
}

#[cfg(not(test))]
pub fn delete_secret(service: &str, account: &str) -> Result<()> {
    match keyring::Entry::new(service, account).and_then(|entry| entry.inner.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => bail!("cannot remove the secret from the OS credential store: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failed_credential_store_names_what_works_without_one() {
        let error = store_failed("save the secret in", keyring::Error::NoDefaultStore);
        let message = format!("{error:#}");
        assert!(
            message.contains("camera credentials in the address"),
            "{message}"
        );
        assert_eq!(crate::cli::error_code(&error), ("failed", 1), "{message}");
    }
}
