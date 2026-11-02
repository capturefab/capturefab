//! Redaction of secrets in outgoing text.
//!
//! A last line of defence for camera passwords: every password a process
//! resolves is registered here, and text that leaves the process (worker
//! replies, CLI errors) has each registered literal replaced. Errors are
//! worded without secrets in the first place; this only catches what a
//! dependency might echo. URL credentials in free text such as log lines
//! and job errors are redacted by [`redact_urls`].
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, VecDeque},
    sync::{Mutex, PoisonError},
};

/// Shorter values are too likely to match ordinary text.
const MIN_LEN: usize = 8;
/// A process holds a handful of secrets; the oldest are forgotten first.
const MAX_SECRETS: usize = 64;
const REDACTED: &str = "<redacted>";

type Held = zeroize::Zeroizing<String>;

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
        // Match the original text before replacing anything. Replacing a
        // shorter secret first can otherwise expose the rest of a longer
        // one, or hide an overlapping match from the next pass.
        // Keep only the next match per secret. A repeated credential in an
        // untrusted response must not allocate one range per occurrence.
        let mut matches = BinaryHeap::new();
        for (index, secret) in self.0.iter().enumerate() {
            if let Some(start) = text.find(secret.as_str()) {
                matches.push(Reverse((start, start + secret.len(), index)));
            }
        }
        if matches.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut result = String::new();
        let mut copied = 0;
        let mut pending: Option<(usize, usize)> = None;
        while let Some(Reverse((start, end, index))) = matches.pop() {
            let secret = &self.0[index];
            let from = start + secret.chars().next().expect("nonempty secret").len_utf8();
            if let Some(offset) = text[from..].find(secret.as_str()) {
                let next = from + offset;
                matches.push(Reverse((next, next + secret.len(), index)));
            }
            match &mut pending {
                Some((_, previous_end)) if start <= *previous_end => {
                    *previous_end = (*previous_end).max(end);
                }
                Some((previous_start, previous_end)) => {
                    result.push_str(&text[copied..*previous_start]);
                    result.push_str(REDACTED);
                    copied = *previous_end;
                    pending = Some((start, end));
                }
                None => pending = Some((start, end)),
            }
        }
        if let Some((start, end)) = pending {
            result.push_str(&text[copied..start]);
            result.push_str(REDACTED);
            copied = end;
        }
        result.push_str(&text[copied..]);
        Cow::Owned(result)
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

/// `text` with the credentials of every URL in it redacted, including in
/// diagnostics that name several sources.
pub fn redact_urls(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|part| {
            let body = part.trim_end_matches(char::is_whitespace);
            let starts: Vec<_> = body
                .match_indices("://")
                .map(|(at, _)| {
                    body[..at]
                        .char_indices()
                        .rev()
                        .take_while(|(_, character)| {
                            character.is_ascii_alphanumeric()
                                || matches!(character, '+' | '-' | '.')
                        })
                        .last()
                        .map_or(at, |(start, _)| start)
                })
                .collect();
            if starts.is_empty() {
                return part.to_owned();
            }
            let mut redacted = body[..starts[0]].to_owned();
            for (index, start) in starts.iter().enumerate() {
                let end = starts.get(index + 1).copied().unwrap_or(body.len());
                redacted.push_str(&crate::media::redact_url(&body[*start..end]));
            }
            redacted.push_str(&part[body.len()..]);
            redacted
        })
        .collect()
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

    #[test]
    fn overlapping_secrets_are_completely_removed_in_either_order() {
        for secrets in [
            ["abcdefgh", "abcdefghijklm", "fghijklm"],
            ["fghijklm", "abcdefghijklm", "abcdefgh"],
        ] {
            let mut scrubber = Scrubber::new();
            for secret in secrets {
                scrubber.register(secret);
            }
            assert_eq!(scrubber.scrub("x abcdefghijklm y"), "x <redacted> y");
        }
        let mut scrubber = Scrubber::new();
        scrubber.register("éééé");
        assert_eq!(scrubber.scrub("ééééé"), "<redacted>");
    }

    #[test]
    fn every_url_in_a_sentence_is_redacted_and_its_spacing_kept() {
        let text = redact_urls(
            "failed rtsp://user:password@one/live,rtsp://user:secret@two/live then srt://three?passphrase=secret next",
        );
        assert!(
            !text.contains("password") && !text.contains("secret"),
            "{text}"
        );
        assert!(text.contains("<redacted>"), "{text}");
        assert!(text.starts_with("failed rtsp://"), "{text}");
        assert!(
            text.ends_with(" then srt://three?passphrase=<redacted> next"),
            "{text}"
        );
        assert_eq!(redact_urls("no address here\n"), "no address here\n");
    }
    #[test]
    fn camera_password_parameters_are_redacted_in_diagnostics() {
        let text = redact_urls(
            "Nothing accepted a connection at 127.0.0.1:9 (http://127.0.0.1:9/video.cgi?user=admin&pass=hunter2secret). \
             srt://127.0.0.1:9?passphrase=SECRETPASS12345&pass=ALSOSECRET99 and http://cam/snap?loginuse=a&loginpas=LOGINSECRET",
        );
        for secret in [
            "hunter2secret",
            "SECRETPASS12345",
            "ALSOSECRET99",
            "LOGINSECRET",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("user=admin&pass=<redacted>"), "{text}");
        assert!(text.contains("loginuse=a&loginpas=<redacted>"), "{text}");
    }
}
