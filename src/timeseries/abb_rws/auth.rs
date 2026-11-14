//! Private bounded credentials, Digest state and origin-scoped cookies.
use super::{Api, incomplete};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::time::{Duration, Instant};
use zeroize::{Zeroize as _, Zeroizing};

pub(super) struct Session {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
    challenge: Zeroizing<Vec<String>>,
    identity: Zeroizing<String>,
    count: u32,
    pub(super) cookies: Cookies,
}
impl Session {
    pub(super) fn new(user: &str, password: &str) -> Result<Self> {
        Self::with_credentials(secret(user)?, secret(password)?)
    }
    pub(super) fn with_credentials(
        username: Zeroizing<String>,
        password: Zeroizing<String>,
    ) -> Result<Self> {
        if username.is_empty() || username.contains(':') {
            return Err(
                Usage("ABB HTTP username must be nonempty and contain no colon".into()).into(),
            );
        }
        for value in [&username, &password] {
            if value.len() > 4096 || value.chars().any(char::is_control) {
                return Err(Usage(
                    "ABB credentials must be at most4096 UTF-8 bytes without control characters"
                        .into(),
                )
                .into());
            }
        }
        Ok(Self {
            username,
            password,
            challenge: Zeroizing::new(Vec::new()),
            identity: Zeroizing::new(String::new()),
            count: 0,
            cookies: Cookies::default(),
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
                "ABB RWS1 did not provide a Digest challenge",
            ));
        }
        self.challenge = Zeroizing::new(values);
        Ok(())
    }
    pub(super) fn authorization(
        &mut self,
        api: Api,
        path: &str,
    ) -> Result<Option<Zeroizing<String>>> {
        if api == Api::Rws2 {
            let input = Zeroizing::new(format!("{}:{}", *self.username, *self.password));
            let encoded = Zeroizing::new(STANDARD.encode(input.as_bytes()));
            return Ok(Some(Zeroizing::new(format!("Basic {}", *encoded))));
        }
        if self.challenge.is_empty() {
            return Ok(None);
        }
        let mut random = Zeroizing::new([0_u8; 16]);
        getrandom::fill(&mut *random).map_err(|_| {
            Categorized::error(
                ErrorKind::Unavailable,
                "ABB Digest client nonce generation failed",
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
                    "ABB RWS Digest challenge is unsupported or malformed",
                )
            })
        };
        // The shared formatter chooses the strongest offered Digest challenge.
        // Read its generated quoted fields rather than independently choose one.
        let probe = make(1)?;
        let nonce = parameter(&probe, "nonce")?;
        let realm = parameter(&probe, "realm")?;
        // Nonce counts cover all requests with this realm and nonce, regardless
        // of the selected Digest algorithm (RFC 7616 section 3.4).
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
    pub(super) fn redact(&self, text: &str, authorization: Option<&str>) -> String {
        let credentials = Zeroizing::new(format!("{}:{}", *self.username, *self.password));
        let encoded = Zeroizing::new(STANDARD.encode(credentials.as_bytes()));
        let private = |value: &str| !value.is_empty() && text.contains(value);
        let sensitive = private(&self.username)
            || private(&self.password)
            || private(&encoded)
            || authorization.is_some_and(private)
            || self.cookies.0.iter().any(|cookie| private(&cookie.value))
            || self.identity.split('\0').take(2).any(private)
            || authorization.is_some_and(|header| {
                ["cnonce", "nonce", "realm"]
                    .iter()
                    .any(|key| parameter(header, key).is_ok_and(|value| private(&value)))
            });
        if sensitive {
            "[redacted ABB controller diagnostic]".into()
        } else {
            text.to_owned()
        }
    }
    pub(super) fn clear(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
        self.challenge.zeroize();
        self.identity.zeroize();
        self.count = 0;
        self.cookies.0.clear();
    }
}
fn secret(name: &str) -> Result<Zeroizing<String>> {
    let text = Zeroizing::new(std::env::var(name).map_err(|_| {
        Categorized::error(
            ErrorKind::PermissionDenied,
            "ABB credential environment variable is missing or not UTF-8",
        )
    })?);
    if text.len() > 4096 || text.chars().any(char::is_control) {
        return Err(Usage(
            "ABB credentials must be at most4096 UTF-8 bytes without control characters".into(),
        )
        .into());
    }
    crate::scrub::register(&text);
    Ok(text)
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
struct Cookie {
    name: Zeroizing<String>,
    value: Zeroizing<String>,
    path: String,
    secure: bool,
    expires: Option<Instant>,
}
#[derive(Default)]
pub(super) struct Cookies(Vec<Cookie>);
impl Cookies {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
    pub(super) fn header(&mut self, path: &str, secure: bool) -> Zeroizing<String> {
        let now = Instant::now();
        self.0
            .retain(|cookie| cookie.expires.is_none_or(|end| end > now));
        let mut header = Zeroizing::new(String::new());
        for cookie in &self.0 {
            let matches = path == cookie.path
                || (path.starts_with(&cookie.path)
                    && (cookie.path.ends_with('/')
                        || path.as_bytes().get(cookie.path.len()) == Some(&b'/')));
            if matches && (!cookie.secure || secure) {
                if !header.is_empty() {
                    header.push_str("; ");
                }
                header.push_str(&cookie.name);
                header.push('=');
                header.push_str(&cookie.value);
            }
        }
        header
    }
    pub(super) fn receive<S: AsRef<str>>(
        &mut self,
        values: impl Iterator<Item = S>,
        host: &str,
        request_path: &str,
        secure: bool,
        now: Instant,
    ) -> Result<()> {
        self.0
            .retain(|cookie| cookie.expires.is_none_or(|end| end > now));
        for value in values {
            let value = value.as_ref();
            if value.len() > 4096 || !value.is_ascii() || value.bytes().any(|b| b < 32 || b == 127)
            {
                return Err(incomplete("invalid or oversized Set-Cookie"));
            }
            let mut parts = value.split(';');
            let (name, content) = parts
                .next()
                .and_then(|v| v.trim().split_once('='))
                .ok_or_else(|| incomplete("invalid Set-Cookie pair"))?;
            let cookie_octets = if let Some(inner) = content.strip_prefix('"') {
                inner
                    .strip_suffix('"')
                    .ok_or_else(|| incomplete("unbalanced quoted cookie value"))?
            } else {
                content
            };
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                || !cookie_octets.bytes().all(|b| {
                    b == 0x21
                        || (0x23..=0x2b).contains(&b)
                        || (0x2d..=0x3a).contains(&b)
                        || (0x3c..=0x5b).contains(&b)
                        || (0x5d..=0x7e).contains(&b)
                })
            {
                return Err(incomplete("invalid cookie name or value"));
            }
            let mut path = request_path
                .rsplit_once('/')
                .map_or(
                    "/",
                    |(parent, _)| if parent.is_empty() { "/" } else { parent },
                )
                .to_owned();
            let mut is_secure = false;
            let mut expires = None;
            let mut age = None;
            let mut seen = std::collections::BTreeSet::new();
            for part in parts {
                let (key, text) = part
                    .trim()
                    .split_once('=')
                    .map_or_else(|| (part.trim(), None), |(k, v)| (k.trim(), Some(v.trim())));
                let key = key.to_ascii_lowercase();
                if !seen.insert(key.clone()) {
                    return Err(incomplete("duplicate cookie attribute"));
                }
                match key.as_str() {
                    "domain" => {
                        let domain = text
                            .ok_or_else(|| incomplete("cookie Domain missing"))?
                            .trim_start_matches('.');
                        if !domain.eq_ignore_ascii_case(host) {
                            return Err(incomplete(
                                "cookie Domain is outside the configured origin",
                            ));
                        }
                    }
                    "path" => {
                        let text = text.ok_or_else(|| incomplete("cookie Path missing"))?;
                        if !text.starts_with('/') || text.len() > 2048 {
                            return Err(incomplete("invalid cookie Path"));
                        }
                        text.clone_into(&mut path);
                    }
                    "secure" => {
                        if text.is_some() {
                            return Err(incomplete("invalid Secure cookie attribute"));
                        }
                        is_secure = true;
                    }
                    "max-age" => {
                        age = Some(
                            text.and_then(|v| v.parse::<i64>().ok())
                                .ok_or_else(|| incomplete("invalid cookie Max-Age"))?,
                        );
                    }
                    "expires" => {
                        let date = text
                            .and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok())
                            .ok_or_else(|| incomplete("unsupported cookie Expires date"))?;
                        let left = date
                            .signed_duration_since(chrono::Utc::now())
                            .to_std()
                            .unwrap_or(Duration::ZERO);
                        expires = Some(
                            now.checked_add(left)
                                .ok_or_else(|| incomplete("cookie expiry overflows host clock"))?,
                        );
                    }
                    _ => {}
                }
            }
            if is_secure && !secure {
                continue;
            }
            if let Some(seconds) = age {
                expires = Some(expiry(now, seconds)?);
            }
            self.0
                .retain(|cookie| !(cookie.name.as_str() == name && cookie.path == path));
            if expires.is_some_and(|end| end <= now) {
                continue;
            }
            self.0.push(Cookie {
                name: Zeroizing::new(name.to_owned()),
                value: Zeroizing::new(content.to_owned()),
                path,
                secure: is_secure,
                expires,
            });
            let size: usize = self
                .0
                .iter()
                .map(|cookie| cookie.name.len() + cookie.value.len() + cookie.path.len() + 4)
                .sum();
            if self.0.len() > 8 || size > 8192 {
                return Err(incomplete("cookie session exceeds8 entries or8KiB"));
            }
        }
        Ok(())
    }
}
fn expiry(now: Instant, seconds: i64) -> Result<Instant> {
    if seconds <= 0 {
        return Ok(now);
    }
    now.checked_add(Duration::from_secs(u64::try_from(seconds)?))
        .ok_or_else(|| incomplete("cookie expiry overflows host clock"))
}
