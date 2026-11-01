//! Device GenICam XML kept in `<cache>/xml/`, so opening a GigE or USB3
//! Vision camera again skips downloading it, which over GVCP takes hundreds
//! of round trips. A file is named by what identifies the XML the device
//! serves: its URL or manifest entry (address and length, a SHA-1 where the
//! device gives one), its model and firmware version, and the bytes at the
//! end of the file, which for a ZIP hold the checksum of the XML inside. A
//! cached file that does not parse, or links to a node it does not have, is
//! forgotten. Best effort: a cache that cannot be read or written is no
//! cache.
use sha2::{Digest, Sha256};
use std::{io::Read, path::PathBuf};

/// The largest XML kept, and read back.
const MAX_XML: u64 = 32 * 1024 * 1024;
/// The files kept; the oldest go first.
const MAX_FILES: usize = 64;
/// How many bytes at the end of a device's XML file go into its key.
pub(crate) const TAIL: usize = 256;

/// The name of the XML that `parts`, in order, identify.
pub(crate) fn key(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(part);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn dir() -> Option<PathBuf> {
    crate::dirs::cache().map(|cache| cache.join("xml"))
}

fn path(key: &str) -> Option<PathBuf> {
    // Keys are this module's hex digests; nothing else names a file.
    (key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| dir().map(|dir| dir.join(format!("{key}.xml"))))
        .flatten()
}

/// The XML kept under `key`.
pub(crate) fn load(key: &str) -> Option<String> {
    let file = std::fs::File::open(path(key)?).ok()?;
    let mut text = String::new();
    file.take(MAX_XML + 1).read_to_string(&mut text).ok()?;
    (text.len() as u64 <= MAX_XML && !text.is_empty()).then_some(text)
}

/// Keep `xml` under `key`.
pub(crate) fn store(key: &str, xml: &str) {
    if xml.len() as u64 > MAX_XML {
        return;
    }
    let (Some(dir), Some(path)) = (dir(), path(key)) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let temp = dir.join(format!(".{key}.{}.tmp", std::process::id()));
    let written = std::fs::write(&temp, xml).and_then(|()| std::fs::rename(&temp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
        return;
    }
    prune(&dir);
}

/// Forget the XML kept under `key`, which turned out wrong.
pub(crate) fn forget(key: &str) {
    if let Some(path) = path(key) {
        let _ = std::fs::remove_file(path);
    }
}

/// Keep the newest `MAX_FILES` files.
fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "xml"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect::<Vec<_>>();
    if files.len() <= MAX_FILES {
        return;
    }
    files.sort();
    for (_, path) in &files[..files.len() - MAX_FILES] {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_tell_their_parts_apart() {
        let one = key(&[b"Local:a.zip;1000;2000", b"1.0"]);
        assert_eq!(one.len(), 64);
        assert_eq!(one, key(&[b"Local:a.zip;1000;2000", b"1.0"]));
        assert_ne!(one, key(&[b"Local:a.zip;1000;2000", b"1.1"]));
        // Where one part ends matters, not only the bytes.
        assert_ne!(key(&[b"ab", b"c"]), key(&[b"a", b"bc"]));
        assert!(path("../../etc/passwd").is_none());
    }
}
