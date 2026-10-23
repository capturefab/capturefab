//! Hub secrets in memory, in the OS credential store and in the environment.
//! A secret is read in the process that needs it, held in a zeroizing buffer
//! without `Debug`, `Clone` or `Serialize`, and registered with the scrubber
//! so a dependency that echoes it cannot leak it.
#[cfg(feature = "keychain")]
use super::registry::KEYCHAIN_SERVICE;
use super::{Hub, Kind, SecretSource, scrub};
use anyhow::Result;

#[cfg(feature = "hubs")]
type Text = zeroize::Zeroizing<String>;
#[cfg(not(feature = "hubs"))]
type Text = String;

/// A token, password or API key. Deliberately not `Debug`, `Clone` or
/// `Serialize`: the value can only be read through [`Secret::expose`].
pub struct Secret(Text);

impl Secret {
    /// Check a secret for `kind` and take ownership of it.
    pub fn new(kind: Kind, value: String) -> Result<Self> {
        let value = Text::from(value);
        validate(kind, &value)?;
        scrub::register(&value);
        Ok(Self(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// A second copy, for a client that must own its secret. Explicit
    /// rather than `Clone`, so copies are never made by accident.
    #[cfg(feature = "hubs")]
    pub(crate) fn duplicate(&self) -> Self {
        Self(Text::from(self.0.to_string()))
    }
}

/// Tokens and API keys travel in HTTP headers, so they must be visible ASCII;
/// passwords may be any text without control characters. Messages never
/// include the value.
pub fn validate(kind: Kind, value: &str) -> Result<()> {
    let noun = kind.secret_noun();
    let ok = match kind {
        Kind::HomeAssistant | Kind::UnifiProtect => {
            (1..=4096).contains(&value.len()) && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
        }
        Kind::Frigate | Kind::Go2rtc => {
            (1..=4096).contains(&value.len()) && !value.chars().any(char::is_control)
        }
    };
    if ok {
        Ok(())
    } else {
        let rule = match kind {
            Kind::HomeAssistant | Kind::UnifiProtect => {
                "1-4096 visible ASCII characters with no spaces"
            }
            Kind::Frigate | Kind::Go2rtc => "1-4096 bytes with no control characters",
        };
        Err(crate::dataset::Usage(format!("the {noun} must be {rule}")).into())
    }
}

/// Read a secret piped to stdin. A terminal gets a prompt on stderr first.
pub fn read_stdin(kind: Kind) -> Result<Secret> {
    use std::io::IsTerminal;
    let stdin = std::io::stdin();
    let value = if stdin.is_terminal() {
        eprintln!("Paste the {}, then press Enter:", kind.secret_noun());
        // A terminal never ends its input: stop at the line break.
        read_line_bounded(stdin.lock(), kind.secret_noun())?
    } else {
        read_bounded(stdin.lock(), kind.secret_noun())?
    };
    Secret::new(kind, value)
}

/// One line of at most 4096 bytes, without its line break.
fn read_line_bounded(reader: impl std::io::BufRead, what: &str) -> Result<String> {
    use std::io::BufRead;
    let mut line = Vec::with_capacity(4099);
    reader.take(4099).read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") && line.len() > 4096 {
        line.fill(0);
        return Err(crate::dataset::Usage(format!("the {what} exceeds 4096 bytes")).into());
    }
    read_bounded(line.as_slice(), what)
}

/// At most 4096 bytes plus one terminal newline (LF or CRLF), which is
/// removed. The buffer moves into the returned string without a copy.
fn read_bounded(reader: impl std::io::Read, what: &str) -> Result<String> {
    use std::io::Read;
    let usage = |message: String| anyhow::Error::from(crate::dataset::Usage(message));
    let mut bytes = Vec::with_capacity(4099);
    reader.take(4099).read_to_end(&mut bytes)?;
    if bytes.len() > 4098 {
        bytes.fill(0);
        return Err(usage(format!(
            "the {what} on stdin exceeds 4096 bytes plus a terminal newline"
        )));
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    let text = String::from_utf8(bytes).map_err(|error| {
        error.into_bytes().fill(0);
        usage(format!("the {what} on stdin must be UTF-8"))
    })?;
    if text.is_empty() {
        return Err(usage(format!("the {what} on stdin is empty")));
    }
    Ok(text)
}

/// Store `secret` under `account`, returning the value it replaced so a
/// failed save can put it back.
pub fn keychain_replace(account: &str, secret: &Secret) -> Result<Option<Secret>> {
    #[cfg(feature = "keychain")]
    {
        let previous = keychain_read_raw(account)?;
        crate::keychain::store_secret(KEYCHAIN_SERVICE, account, secret.expose()).map_err(
            |_| {
                anyhow::anyhow!(
                    "cannot save the hub secret in the OS credential store; unlock it and try again"
                )
            },
        )?;
        Ok(previous.map(|value| Secret(Text::from(value))))
    }
    #[cfg(not(feature = "keychain"))]
    {
        let _ = (account, secret);
        Err(super::registry::keychain_unsupported())
    }
}

/// Undo [`keychain_replace`].
pub fn keychain_restore(account: &str, previous: Option<&Secret>) -> Result<()> {
    #[cfg(feature = "keychain")]
    {
        match previous {
            Some(secret) => {
                crate::keychain::store_secret(KEYCHAIN_SERVICE, account, secret.expose())
            }
            None => crate::keychain::delete_secret(KEYCHAIN_SERVICE, account),
        }
        .map_err(|_| {
            anyhow::anyhow!("cannot restore the previous hub secret in the OS credential store")
        })
    }
    #[cfg(not(feature = "keychain"))]
    {
        let _ = (account, previous);
        Ok(())
    }
}

pub fn keychain_delete(account: &str) -> Result<()> {
    #[cfg(feature = "keychain")]
    {
        crate::keychain::delete_secret(KEYCHAIN_SERVICE, account).map_err(|_| {
            anyhow::anyhow!(
                "cannot remove the hub secret from the OS credential store; unlock it and try again"
            )
        })
    }
    #[cfg(not(feature = "keychain"))]
    {
        let _ = account;
        Err(super::registry::keychain_unsupported())
    }
}

/// Whether the credential store holds a secret for `account`. `None` when
/// this build has no credential store.
pub fn keychain_has(account: &str) -> Option<bool> {
    #[cfg(feature = "keychain")]
    {
        Some(crate::keychain::has_secret(KEYCHAIN_SERVICE, account))
    }
    #[cfg(not(feature = "keychain"))]
    {
        let _ = account;
        None
    }
}

#[cfg(feature = "keychain")]
fn keychain_read_raw(account: &str) -> Result<Option<String>> {
    crate::keychain::read_secret(KEYCHAIN_SERVICE, account).map_err(|_| {
        anyhow::anyhow!(
            "cannot read the hub secret from the OS credential store; unlock it and try again"
        )
    })
}

/// The secret `hub` uses, read from where its record says.
pub fn resolve(hub: &Hub) -> Result<Option<Secret>> {
    let missing = |message: String| -> anyhow::Error {
        crate::types::Categorized::error(crate::types::ErrorKind::OperationFailed, message)
    };
    match hub.secret {
        SecretSource::None => Ok(None),
        SecretSource::Environment => {
            let variable = hub.secret_env.as_deref().unwrap_or_default();
            match std::env::var(variable) {
                Ok(value) if !value.is_empty() => {
                    Secret::new(hub.kind, value).map(Some).map_err(|_| {
                        missing(format!(
                            "{variable} does not hold a valid {} for hub {}",
                            hub.kind.secret_noun(),
                            hub.name
                        ))
                    })
                }
                _ => Err(missing(format!(
                    "{variable} is not set in the process that opened hub {}; a GUI started from the Dock, Finder or Start menu does not see shell variables; save the secret with --secret-stdin instead",
                    hub.name
                ))),
            }
        }
        SecretSource::Keychain => {
            #[cfg(feature = "keychain")]
            {
                match keychain_read_raw(&hub.account()?)? {
                    Some(value) => Secret::new(hub.kind, value).map(Some).map_err(|_| {
                        missing(format!(
                            "the saved secret for hub {} is not a valid {}; add the hub again with --secret-stdin",
                            hub.name,
                            hub.kind.secret_noun()
                        ))
                    }),
                    None => Err(missing(format!(
                        "no secret saved for hub {}; add it again with `capturefab hub {} {} --url URL --secret-stdin`",
                        hub.name,
                        hub.kind.command(),
                        hub.name
                    ))),
                }
            }
            #[cfg(not(feature = "keychain"))]
            {
                Err(crate::types::Categorized::error(
                    crate::types::ErrorKind::Unsupported,
                    format!(
                        "hub {} keeps its secret in the OS credential store, which is unsupported in this build; add it again with --secret-env VAR",
                        hub.name
                    ),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_are_checked_without_echoing_them() {
        assert!(Secret::new(Kind::HomeAssistant, "abc.DEF-123_xyz".into()).is_ok());
        for bad in ["", "has space", "line\nbreak", "tab\tbed"] {
            let error = Secret::new(Kind::HomeAssistant, bad.into())
                .err()
                .expect("rejected");
            assert!(bad.is_empty() || !format!("{error:#}").contains(bad));
            assert_eq!(crate::cli::error_code(&error), ("usage", 2));
        }
        assert!(Secret::new(Kind::Frigate, "pass word ünicode".into()).is_ok());
        assert!(Secret::new(Kind::Frigate, "nul\0".into()).is_err());
        assert!(Secret::new(Kind::UnifiProtect, "k".repeat(4097)).is_err());
    }
    #[test]
    fn stdin_secrets_lose_one_newline_and_are_bounded() {
        for (input, expected) in [
            ("token\n", "token"),
            ("token\r\n", "token"),
            ("token", "token"),
            ("to ken \n", "to ken "),
        ] {
            assert_eq!(read_bounded(input.as_bytes(), "token").unwrap(), expected);
        }
        for bad in [&b"\n"[..], &[0xff, 0xfe][..], &[b'x'; 5000][..]] {
            assert!(read_bounded(bad, "token").is_err());
        }
        // A terminal line ends at Enter, whatever follows.
        assert_eq!(
            read_line_bounded(&b"token\r\nignored"[..], "token").unwrap(),
            "token"
        );
        assert!(read_line_bounded(&[b'x'; 5000][..], "token").is_err());
    }
    #[test]
    fn environment_secrets_are_read_and_missing_ones_explained() {
        let mut hub = Hub {
            name: "home".into(),
            kind: Kind::HomeAssistant,
            url: "http://ha:8123".into(),
            user: None,
            secret: SecretSource::Environment,
            secret_env: Some("CAPTUREFAB_HUB_SECRET_TEST_TOKEN".into()),
            certificate: None,
            tls_server_name: None,
            rtsp_port: None,
            rejected: None,
            revision: [0; 16],
        };
        let error = resolve(&hub).err().expect("unset");
        assert!(format!("{error}").contains("is not set in the process that opened hub home"));
        // SAFETY: only this test uses the variable.
        unsafe { std::env::set_var("CAPTUREFAB_HUB_SECRET_TEST_TOKEN", "CANARY-env-token") };
        let secret = resolve(&hub).unwrap().unwrap();
        assert_eq!(secret.expose(), "CANARY-env-token");
        assert_eq!(scrub::scrub("x CANARY-env-token y"), "x <redacted> y");
        unsafe { std::env::remove_var("CAPTUREFAB_HUB_SECRET_TEST_TOKEN") };
        hub.secret = SecretSource::None;
        hub.secret_env = None;
        assert!(resolve(&hub).unwrap().is_none());
    }
}
