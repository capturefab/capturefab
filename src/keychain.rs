//! The OS credential store (macOS Keychain, Windows Credential Manager, the
//! Secret Service on Linux), shared by network destinations and camera hubs.
//! Only secrets go here; the settings that name them live in the session
//! directory.
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::io::Read;

pub fn validate_secret(secret: &str) -> Result<()> {
    ensure!(
        !secret.is_empty() && secret.len() <= 4096,
        "secret must contain 1..4096 bytes"
    );
    Ok(())
}

/// Store a secret in the OS credential store.
pub fn store_secret(service: &str, account: &str, secret: &str) -> Result<()> {
    validate_secret(secret)?;
    keyring::Entry::new(service, account)
        .and_then(|entry| entry.set_password(secret))
        .map_err(|e| anyhow!("cannot save the secret in the OS credential store: {e}"))
}

/// Read a secret. A missing entry is `None`; a locked or unavailable store is
/// an error, so a caller never mistakes it for an empty slot.
pub fn read_secret(service: &str, account: &str) -> Result<Option<String>> {
    match keyring::Entry::new(service, account).and_then(|entry| entry.get_password()) {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => bail!("cannot read the secret from the OS credential store: {e}"),
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

/// Read a secret from stdin: at most 4096 bytes plus one terminal newline
/// (LF or CRLF), which is removed. Other bytes, including spaces, are kept.
pub fn read_stdin_secret(reader: impl Read, what: &str) -> Result<String> {
    let mut data = Vec::new();
    // Allow a maximum-size secret followed by CRLF, plus one byte to detect
    // an oversized input without reading an unbounded pipe into memory.
    reader.take(4099).read_to_end(&mut data)?;
    ensure!(
        data.len() <= 4098,
        "{what} stdin exceeds 4096 bytes plus a terminal newline"
    );
    if data.last() == Some(&b'\n') {
        data.pop();
        if data.last() == Some(&b'\r') {
            data.pop();
        }
    }
    let secret = String::from_utf8(data).with_context(|| format!("{what} stdin must be UTF-8"))?;
    ensure!(
        !secret.is_empty() && secret.len() <= 4096,
        "{what} must contain 1..4096 bytes"
    );
    Ok(secret)
}
