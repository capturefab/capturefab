//! Private explicit HTTP authentication; no cookies or implicit Basic fallback.
use super::{AuthKind, Config, incomplete};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use zeroize::{Zeroize as _, Zeroizing};

pub(super) struct Session {
    kind: AuthKind,
    username: Zeroizing<String>,
    password: Zeroizing<String>,
    token: Zeroizing<String>,
    challenge: Zeroizing<Vec<String>>,
    identity: Zeroizing<String>,
    count: u32,
}
impl Session {
    pub(super) fn new(config: &Config) -> Result<Self> {
        let username = config
            .username_env
            .as_deref()
            .map(secret)
            .transpose()?
            .unwrap_or_default();
        let password = config
            .password_env
            .as_deref()
            .map(secret)
            .transpose()?
            .unwrap_or_default();
        let token = config
            .bearer_token_env
            .as_deref()
            .map(secret)
            .transpose()?
            .unwrap_or_default();
        if matches!(config.auth, AuthKind::Basic | AuthKind::Digest)
            && (username.is_empty() || username.contains(':'))
        {
            return Err(Usage(
                "MTConnect HTTP username must be nonempty and contain no colon".into(),
            )
            .into());
        }
        if config.auth == AuthKind::Bearer
            && (token.is_empty()
                || !token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)))
        {
            return Err(
                Usage("MTConnect bearer token must be nonempty HTTP token68 text".into()).into(),
            );
        }
        Ok(Self {
            kind: config.auth,
            username,
            password,
            token,
            challenge: Zeroizing::new(Vec::new()),
            identity: Zeroizing::new(String::new()),
            count: 0,
        })
    }
    pub(super) fn challenge(&mut self, headers: &[(String, String)]) -> Result<()> {
        let values: Vec<_> = headers
            .iter()
            .filter(|(name, _)| name == "www-authenticate")
            .map(|(_, value)| value.clone())
            .collect();
        if values.is_empty() {
            return Err(Categorized::error(
                ErrorKind::AuthRejected,
                "MTConnect agent did not offer Digest authentication",
            ));
        }
        self.challenge = Zeroizing::new(values);
        Ok(())
    }
    pub(super) fn authorization(&mut self, path: &str) -> Result<Option<Zeroizing<String>>> {
        match self.kind {
            AuthKind::None => return Ok(None),
            AuthKind::Basic => {
                let input = Zeroizing::new(format!("{}:{}", *self.username, *self.password));
                let encoded = Zeroizing::new(STANDARD.encode(input.as_bytes()));
                return Ok(Some(Zeroizing::new(format!("Basic {}", *encoded))));
            }
            AuthKind::Bearer => return Ok(Some(Zeroizing::new(format!("Bearer {}", *self.token)))),
            AuthKind::Digest => {}
        }
        if self.challenge.is_empty() {
            return Ok(None);
        }
        let mut random = Zeroizing::new([0_u8; 16]);
        getrandom::fill(&mut *random).map_err(|_| {
            Categorized::error(
                ErrorKind::Unavailable,
                "MTConnect Digest client nonce generation failed",
            )
        })?;
        let cnonce = Zeroizing::new(crate::types::hex(*random));
        let make = |count| {
            crate::onvif::digest_only_authorization(
                &self.challenge,
                &self.username,
                &self.password,
                "GET",
                path,
                &cnonce,
                count,
            )
            .map(Zeroizing::new)
            .map_err(|_| {
                Categorized::error(
                    ErrorKind::Unsupported,
                    "MTConnect Digest challenge is unsupported or malformed",
                )
            })
        };
        let probe = make(1)?;
        let nonce = parameter(&probe, "nonce")?;
        let realm = parameter(&probe, "realm")?;
        // RFC 7616 nonce count identity is realm+nonce, regardless of algorithm.
        let identity = Zeroizing::new(format!("{}\0{}", *realm, *nonce));
        let count = if *identity == *self.identity {
            self.count
                .checked_add(1)
                .ok_or_else(|| incomplete("Digest nonce count exhausted"))?
        } else {
            1
        };
        let header = if count == 1 { probe } else { make(count)? };
        self.identity = identity;
        self.count = count;
        Ok(Some(header))
    }
    pub(super) fn redact_error(&self, error: &mut anyhow::Error) {
        if let Some(error) = error.downcast_mut::<Categorized>()
            && let Some(codes) = error
                .details
                .as_mut()
                .and_then(|d| d.get_mut("mtconnect_error_codes"))
                .and_then(serde_json::Value::as_array_mut)
        {
            for code in codes {
                let sensitive = code.as_str().is_some_and(|text| {
                    [&*self.username, &*self.password, &*self.token]
                        .into_iter()
                        .any(|secret| !secret.is_empty() && text.contains(secret))
                        || self
                            .identity
                            .split('\0')
                            .any(|secret| !secret.is_empty() && text.contains(secret))
                });
                if sensitive {
                    *code = serde_json::Value::String("[redacted]".into());
                }
            }
        }
    }
    pub(super) fn clear(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
        self.token.zeroize();
        self.challenge.zeroize();
        self.identity.zeroize();
        self.count = 0;
    }
}
fn secret(name: &str) -> Result<Zeroizing<String>> {
    let value = Zeroizing::new(std::env::var(name).map_err(|_| {
        Categorized::error(
            ErrorKind::PermissionDenied,
            "MTConnect credential environment variable is missing or not UTF-8",
        )
    })?);
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(Usage(
            "MTConnect credentials exceed4096 bytes or contain control characters".into(),
        )
        .into());
    }
    crate::scrub::register(&value);
    Ok(value)
}
fn parameter(header: &str, wanted: &str) -> Result<Zeroizing<String>> {
    let mut remaining = header
        .strip_prefix("Digest ")
        .ok_or_else(|| incomplete("invalid Digest formatter result"))?;
    while !remaining.is_empty() {
        let (name, tail) = remaining
            .split_once('=')
            .ok_or_else(|| incomplete("invalid Digest formatter parameter"))?;
        let name = name.trim();
        let mut tail = tail.trim_start();
        let mut value = Zeroizing::new(String::new());
        if let Some(quoted) = tail.strip_prefix('"') {
            let mut escaped = false;
            let mut end = None;
            for (at, ch) in quoted.char_indices() {
                if escaped {
                    value.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    end = Some(at + 1);
                    break;
                } else {
                    value.push(ch);
                }
            }
            tail = quoted
                .get(end.ok_or_else(|| incomplete("unterminated Digest formatter value"))?..)
                .ok_or_else(|| incomplete("invalid Digest formatter value"))?;
        } else {
            let (text, next) = tail
                .split_once(',')
                .map_or((tail, ""), |(text, next)| (text, next));
            value.push_str(text.trim());
            if name == wanted {
                return Ok(value);
            }
            remaining = next.trim_start();
            continue;
        }
        if name == wanted {
            return Ok(value);
        }
        let next = tail.trim_start();
        remaining = if next.is_empty() {
            ""
        } else {
            next.strip_prefix(',')
                .ok_or_else(|| incomplete("invalid Digest formatter delimiter"))?
                .trim_start()
        };
    }
    Err(incomplete("Digest formatter omitted its nonce or realm"))
}
