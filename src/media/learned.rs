//! Which sources earlier processes found to send Motion JPEG, kept in the
//! cache: a new camera worker then reads such a source's own JPEGs from its
//! first launch, instead of decoding it once to learn its codec and opening
//! it again. A source is named by a digest of its address, so the file
//! holds no address or credential. An answer that turns out wrong costs one
//! launch and is corrected.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The most sources kept; the least recently found go first.
const KEPT: usize = 256;

/// The digests of sources found to send Motion JPEG, with when, in UTC
/// Unix milliseconds.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Learned {
    #[serde(default)]
    mjpeg: BTreeMap<String, u64>,
}

fn key(source: &str) -> String {
    Sha256::digest(source.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether `source` was found to send Motion JPEG.
pub(super) fn motion_jpeg(source: &str) -> bool {
    load().mjpeg.contains_key(&key(source))
}

/// Remember whether `source` sends Motion JPEG, as FFmpeg just found.
pub(super) fn found(source: &str, mjpeg: bool) {
    let mut learned = load();
    let key = key(source);
    if mjpeg {
        learned.mjpeg.insert(key, crate::scheduling::now_ms());
        while learned.mjpeg.len() > KEPT {
            let oldest = learned
                .mjpeg
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone());
            learned.mjpeg.remove(&oldest.unwrap_or_default());
        }
    } else if learned.mjpeg.remove(&key).is_none() {
        return;
    }
    store(&learned);
}

#[cfg(not(test))]
fn path() -> Option<std::path::PathBuf> {
    Some(crate::dirs::cache()?.join("sources.json"))
}

#[cfg(not(test))]
fn load() -> Learned {
    path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Best effort: without the file, the next worker decodes once to learn.
#[cfg(not(test))]
fn store(learned: &Learned) {
    let Some(path) = path() else {
        return;
    };
    if let (Some(dir), Ok(bytes)) = (path.parent(), serde_json::to_vec(learned))
        && crate::dirs::ensure_private_dir(dir).is_ok()
    {
        let _ = crate::dirs::write_private(&path, &bytes, false);
    }
}

// Each test thread learns for itself, so tests that reuse an address with
// different stand-in FFmpegs stay independent of each other and of the
// user's cache.
#[cfg(test)]
thread_local! {
    static LEARNED: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn load() -> Learned {
    LEARNED.with_borrow(|bytes| serde_json::from_slice(bytes).unwrap_or_default())
}

#[cfg(test)]
fn store(learned: &Learned) {
    LEARNED.set(serde_json::to_vec(learned).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_remembered_by_digest_and_forgotten_when_they_change() {
        let source = "http://admin:secret@cam.local/video.mjpg";
        assert!(!motion_jpeg(source));
        found(source, true);
        assert!(motion_jpeg(source));
        assert!(!motion_jpeg("http://cam.local/other.mjpg"));
        // The stored form names no address or credential.
        let stored = LEARNED.with_borrow(|bytes| String::from_utf8(bytes.clone()).unwrap());
        assert!(
            !stored.contains("secret") && !stored.contains("cam.local"),
            "{stored}"
        );
        found(source, false);
        assert!(!motion_jpeg(source));
        // Only the newest are kept.
        for index in 0..KEPT + 10 {
            found(&format!("http://cam{index}/video.mjpg"), true);
        }
        assert_eq!(load().mjpeg.len(), KEPT);
    }
}
