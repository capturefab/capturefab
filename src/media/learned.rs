//! Which sources earlier processes found to send Motion JPEG, kept in the
//! cache: a new camera worker then reads such a source's own JPEGs from its
//! first launch, instead of decoding it once to learn its codec and opening
//! it again. A source is named by a digest of its address, so the file
//! holds no address or credential. An answer that turns out wrong costs one
//! launch and is corrected.
#![forbid(unsafe_code)]
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, io::Read};

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
    crate::types::hex(&Sha256::digest(source.as_bytes())[..16])
}

/// Whether `source` was found to send Motion JPEG.
pub(super) fn motion_jpeg(source: &str) -> bool {
    load().mjpeg.contains_key(&key(source))
}

/// Remember whether `source` sends Motion JPEG, as `FFmpeg` just found.
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

/// A full 256-source cache fits comfortably within this bound.
const MAX_FILE: usize = 64 * 1024;

#[deny(clippy::arithmetic_side_effects)]
fn read_cache(reader: impl Read) -> Learned {
    let limit = u64::try_from(MAX_FILE).expect("the cache size limit fits u64");
    let mut bytes = Vec::new();
    if reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_FILE
    {
        return Learned::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

#[cfg(not(test))]
fn load() -> Learned {
    path()
        .and_then(|path| std::fs::File::open(path).ok())
        .map(read_cache)
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
    LEARNED.with_borrow(|bytes| read_cache(&**bytes))
}

#[cfg(test)]
fn store(learned: &Learned) {
    LEARNED.set(serde_json::to_vec(learned).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_cache_reads_are_bounded_and_keep_a_full_cache() {
        let full = Learned {
            mjpeg: (0..KEPT).map(|i| (key(&i.to_string()), u64::MAX)).collect(),
        };
        let mut bytes = serde_json::to_vec(&full).unwrap();
        assert!(bytes.len() < MAX_FILE);
        bytes.resize(MAX_FILE, b' ');
        assert_eq!(read_cache(&*bytes).mjpeg, full.mjpeg);
        bytes.resize(MAX_FILE + 10_000, b' ');
        let mut reader = std::io::Cursor::new(bytes);
        assert_eq!(read_cache(&mut reader).mjpeg, BTreeMap::new());
        assert_eq!(reader.position(), u64::try_from(MAX_FILE + 1).unwrap());
        assert_eq!(read_cache(&b"not JSON"[..]).mjpeg, BTreeMap::new());
    }

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
