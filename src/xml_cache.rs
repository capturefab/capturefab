//! Device `GenICam` XML kept in `<cache>/xml/`, so opening a `GigE` or USB3
//! Vision camera again skips downloading it, which over GVCP takes hundreds
//! of round trips. A file is named by what identifies the XML the device
//! serves: its URL or manifest entry (address and length, a SHA-1 where the
//! device gives one), its model and firmware version, and the bytes at the
//! end of the file, which for a ZIP hold the checksum of the XML inside. A
//! cached file that does not parse, or links to a node it does not have, is
//! forgotten. Best effort: a cache that cannot be read or written is no
//! cache.
#![forbid(unsafe_code)]
#![deny(clippy::cast_precision_loss)]
use sha2::{Digest as _, Sha256};
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
    crate::types::hex(digest.finalize())
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
    read_xml(file)
}

fn read_xml(reader: impl Read) -> Option<String> {
    let mut reader = reader.take(MAX_XML + 1);
    let mut text = String::new();
    reader.read_to_string(&mut text).ok()?;
    (reader.limit() != 0 && !text.is_empty()).then_some(text)
}

/// Keep `xml` under `key`.
pub(crate) fn store(key: &str, xml: &str) {
    if !u64::try_from(xml.len()).is_ok_and(|length| length <= MAX_XML) {
        return;
    }
    let (Some(dir), Some(path)) = (dir(), path(key)) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if crate::dirs::write_private(&path, xml.as_bytes(), false).is_ok() {
        prune(&dir);
    }
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
        .filter_map(|entry| {
            let path = entry.path();
            path.extension().is_some_and(|e| e == "xml").then_some(())?;
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect::<Vec<_>>();
    if files.len() <= MAX_FILES {
        return;
    }
    let excess = files.len() - MAX_FILES;
    let (expired, _, _) = files.select_nth_unstable(excess);
    for (_, path) in expired {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pruning_keeps_the_newest_files_with_path_ties() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-xml-prune-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        for count in [0, MAX_FILES, MAX_FILES + 1, MAX_FILES * 3] {
            let dir = root.join(count.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let unrelated = dir.join("keep.txt");
            std::fs::write(&unrelated, "keep").unwrap();
            let mut expected = Vec::new();
            for index in 0..count {
                let path = dir.join(format!("{index:03}.xml"));
                let file = std::fs::File::create(&path).unwrap();
                file.set_modified(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(u64::try_from(index % 3).unwrap()),
                )
                .unwrap();
                expected.push((file.metadata().unwrap().modified().unwrap(), path));
            }
            expected.sort();
            let mut expected = expected
                .split_off(count.saturating_sub(MAX_FILES))
                .into_iter()
                .map(|(_, path)| path)
                .collect::<Vec<_>>();
            expected.sort();
            prune(&dir);
            let mut actual = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "xml"))
                .collect::<Vec<_>>();
            actual.sort();
            assert_eq!(actual, expected, "{count} entries");
            assert!(unrelated.is_file());
            prune(&dir);
            assert!(expected.iter().all(|path| path.is_file()));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_xml_reads_are_bounded_and_require_nonempty_utf8() {
        assert_eq!(read_xml(b"<xml/>".as_slice()).as_deref(), Some("<xml/>"));
        assert_eq!(read_xml([].as_slice()), None);
        assert_eq!(read_xml(b"\xff".as_slice()), None);

        let length = usize::try_from(MAX_XML).unwrap();
        let mut bytes = vec![b' '; length];
        assert_eq!(read_xml(bytes.as_slice()).unwrap().len(), length);
        bytes.extend_from_slice(b"  ");
        let mut reader = std::io::Cursor::new(bytes);
        assert_eq!(read_xml(&mut reader), None);
        assert_eq!(reader.position(), MAX_XML + 1);
    }

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
