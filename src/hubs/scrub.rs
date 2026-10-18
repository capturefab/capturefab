//! A last line of defence for hub secrets: every secret a process resolves
//! (tokens, passwords, the Basic credentials and session tokens derived from
//! them) is registered here, and text that leaves the process — worker
//! replies, discovery warnings, CLI errors, remote API error bodies — has
//! each registered literal replaced. Hub errors are worded without secrets in
//! the first place; this only catches what a dependency might echo.
use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::{Mutex, PoisonError},
};

/// Shorter values are too likely to match ordinary text.
const MIN_LEN: usize = 8;
/// A process holds a handful of hub secrets; the oldest are forgotten first.
const MAX_SECRETS: usize = 64;
const REDACTED: &str = "<redacted>";

#[cfg(feature = "hubs")]
type Held = zeroize::Zeroizing<String>;
#[cfg(not(feature = "hubs"))]
type Held = String;

/// Registered secrets, oldest first.
struct Scrubber(VecDeque<Held>);

impl Scrubber {
    const fn new() -> Self {
        Self(VecDeque::new())
    }
    fn register(&mut self, secret: &str) {
        if secret.len() < MIN_LEN || self.0.iter().any(|held| held.as_str() == secret) {
            return;
        }
        if self.0.len() == MAX_SECRETS {
            self.0.pop_front();
        }
        self.0.push_back(Held::from(secret.to_owned()));
    }
    fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut result = Cow::Borrowed(text);
        for secret in &self.0 {
            if result.contains(secret.as_str()) {
                result = Cow::Owned(result.replace(secret.as_str(), REDACTED));
            }
        }
        result
    }
}

static SECRETS: Mutex<Scrubber> = Mutex::new(Scrubber::new());

/// Remember a secret so [`scrub`] removes it from outgoing text.
pub fn register(secret: &str) {
    SECRETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .register(secret);
}

/// Replace every registered secret in `text`.
pub fn scrub(text: &str) -> Cow<'_, str> {
    SECRETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .scrub(text)
}

/// Whether any secret has been registered, so callers can skip work.
pub fn active() -> bool {
    !SECRETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .0
        .is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registered_secrets_are_replaced_and_short_values_ignored() {
        register("short");
        register("scrub-test-CANARY-token");
        register("scrub-test-CANARY-token");
        assert_eq!(scrub("short and sweet"), "short and sweet");
        assert_eq!(
            scrub("Bearer scrub-test-CANARY-token rejected; scrub-test-CANARY-token"),
            "Bearer <redacted> rejected; <redacted>"
        );
        assert!(matches!(scrub("nothing secret here"), Cow::Borrowed(_)));
        assert!(active());
    }
    #[test]
    fn the_oldest_secret_is_forgotten_at_the_cap() {
        let mut scrubber = Scrubber::new();
        scrubber.register("scrub-test-first-secret");
        for index in 0..MAX_SECRETS {
            scrubber.register(&format!("scrub-test-filler-{index:04}"));
        }
        assert_eq!(scrubber.0.len(), MAX_SECRETS);
        assert_eq!(
            scrubber.scrub("scrub-test-first-secret"),
            "scrub-test-first-secret"
        );
        assert_eq!(scrubber.scrub("scrub-test-filler-0063"), "<redacted>");
    }
}
