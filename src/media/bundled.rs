//! The FFmpeg built into this executable, extracted on first use to a
//! private folder that every later process reuses: the cache, else a cache
//! in the runtime folder, else this user's temporary folder, so read-only
//! and noexec homes still run it. A copy is checked once against its digest
//! when it is written or first found; a marker beside it then records the
//! file it checked, and later processes compare the file against the marker
//! instead of hashing it again. A copy the system would not start gets a
//! marker of its own, so later processes only ask the system again.
use super::{Executable, FFMPEG_NAME, nonce};
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

// Keep this envelope in sync with build.rs and scripts/release.py. The raw
// length and SHA-256 are checked while decoding, before publishing a cache file.
const PAYLOAD_MAGIC: &[u8] = b"CAPTUREFAB-FFMPEG-ZLIB\0";
/// Beside a checked copy: its digest and the identity of the file checked.
const MARKER: &str = "verified";
/// The same for a checked copy this system would not start, as under an
/// execution policy or on a noexec mount this platform cannot detect.
const REFUSED_MARKER: &str = "refused";
/// How long a process that found nowhere to run the copy uses what else
/// there is before trying again: each try starts the copy in each folder.
const RETRY: Duration = Duration::from_secs(60);

pub(super) struct Payload<'a> {
    bytes: &'a [u8],
    length: usize,
    digest: String,
    #[cfg(test)]
    raw: bool,
}
impl<'a> Payload<'a> {
    pub(super) fn compressed(bytes: &'a [u8]) -> Result<Self> {
        let header = PAYLOAD_MAGIC.len() + 8 + 32;
        ensure!(
            bytes.len() > header && bytes.starts_with(PAYLOAD_MAGIC),
            "bundled FFmpeg has an invalid compressed header"
        );
        let length = u64::from_le_bytes(
            bytes[PAYLOAD_MAGIC.len()..PAYLOAD_MAGIC.len() + 8]
                .try_into()
                .expect("eight length bytes"),
        );
        let length =
            usize::try_from(length).context("bundled FFmpeg is too large for this host")?;
        ensure!(length > 0, "bundled FFmpeg is empty");
        let digest = bytes[PAYLOAD_MAGIC.len() + 8..header]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            bytes: &bytes[header..],
            length,
            digest,
            #[cfg(test)]
            raw: false,
        })
    }
    #[cfg(all(test, unix))]
    fn raw(bytes: &'a [u8], digest: &str) -> Self {
        Self {
            bytes,
            length: bytes.len(),
            digest: digest.to_owned(),
            raw: true,
        }
    }
    /// Decode directly to the file with fixed working memory. No decompressed
    /// executable is retained in Capturefab's address space.
    fn write_to(&self, output: &mut impl Write) -> Result<()> {
        #[cfg(test)]
        if self.raw {
            output.write_all(self.bytes)?;
            return Ok(());
        }
        use sha2::Digest;
        let mut decoder = flate2::bufread::ZlibDecoder::new(self.bytes);
        let mut hash = sha2::Sha256::new();
        let mut length = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = decoder
                .read(&mut buffer)
                .context("cannot decompress bundled FFmpeg")?;
            if count == 0 {
                break;
            }
            ensure!(
                count <= self.length.saturating_sub(length),
                "bundled FFmpeg exceeds its expected length"
            );
            length += count;
            hash.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
        }
        ensure!(
            length == self.length,
            "bundled FFmpeg has an incomplete payload"
        );
        ensure!(
            decoder.total_in() == self.bytes.len() as u64,
            "bundled FFmpeg has trailing compressed data"
        );
        ensure!(
            format!("{:x}", hash.finalize()) == self.digest,
            "bundled FFmpeg checksum mismatch"
        );
        Ok(())
    }
}

/// The copy this process runs, kept for its life.
static EXTRACTED: OnceLock<Mutex<Option<Arc<Executable>>>> = OnceLock::new();
/// Why the last extraction found no folder, and when, for doctor and for
/// the calls until `RETRY` has passed.
static REFUSED: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// Why the bundled FFmpeg could not be extracted, when it could not.
pub(super) fn refused() -> Option<String> {
    REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(_, message)| message.clone())
}

/// `ffmpeg_missing` for a copy that could not be extracted, for `why`.
fn nowhere(why: String) -> anyhow::Error {
    Categorized::new(ErrorKind::FfmpegMissing, why)
        .hint(format!(
            "set {} to a writable folder that allows running programs, or CAPTUREFAB_FFMPEG to an FFmpeg executable",
            crate::dirs::CACHE_DIR_ENV
        ))
        .into()
}

/// The built-in FFmpeg, extracted once for every process of this user to
/// the first of `roots` that allows running programs.
pub(super) fn extracted(embedded: &[u8]) -> Result<Arc<Executable>> {
    let payload = Payload::compressed(embedded)?;
    let mut extracted = EXTRACTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Another process can remove or replace the copy, so its owner, mode
    // and length are checked again before each use.
    if let Some(copy) = extracted.as_ref()
        && trusted(&copy.path, payload.length).is_ok()
    {
        return Ok(copy.clone());
    }
    if let Some((when, why)) = &*REFUSED.lock().unwrap_or_else(|e| e.into_inner())
        && when.elapsed() < RETRY
    {
        return Err(nowhere(why.clone()));
    }
    let roots = roots();
    // A cache folder passed over, such as one CAPTUREFAB_CACHE_DIR names
    // that another user owns, says why first.
    let mut refused = crate::dirs::found(crate::dirs::Kind::Cache)
        .passed
        .iter()
        .filter(|passed| !roots.contains(&passed.path))
        .map(|passed| format!("{}: {}", passed.path.display(), passed.why))
        .collect::<Vec<_>>();
    for root in roots {
        if noexec(&root) {
            refused.push(format!("{}: mounted noexec", root.display()));
            continue;
        }
        match cached_payload(&root, &payload, runs) {
            Ok(copy) => return Ok(extracted.insert(Arc::new(copy)).clone()),
            Err(error) => refused.push(format!("{}: {error:#}", root.display())),
        }
    }
    let message = format!(
        "cannot extract the bundled FFmpeg to a folder that runs programs ({})",
        if refused.is_empty() {
            "no folder to extract it to".to_owned()
        } else {
            refused.join("; ")
        }
    );
    *REFUSED.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), message.clone()));
    Err(nowhere(message))
}

/// Where the bundled FFmpeg may be extracted, in order: the cache, a cache
/// in the runtime folder, and one in this user's temporary folder.
fn roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let candidates = [
        crate::dirs::cache(),
        crate::dirs::runtime().ok().map(|dir| dir.join("cache")),
        crate::dirs::temp().map(|dir| dir.join("cache")),
    ];
    for root in candidates.into_iter().flatten() {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// Whether the file system `root` is on, or would be on, is mounted noexec.
fn noexec(root: &Path) -> bool {
    root.ancestors()
        .find(|dir| dir.exists())
        .is_some_and(crate::dirs::noexec_mount)
}

/// The system lets this user start `exe`: a home directory mounted noexec,
/// or an execution policy, can refuse what the temporary directory allows.
pub(super) fn runs(exe: &Executable) -> bool {
    match super::command(exe)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        // Anything else, such as a copy another thread still holds open for
        // writing, is for the real launch to report.
        Err(error) => error.kind() != std::io::ErrorKind::PermissionDenied,
    }
}

/// The shared copy of `bytes` under `root`, named by its digest. It is
/// written once, under a lock, to a new file that is renamed into place;
/// an existing copy is checked against the digest before this process
/// first runs it.
#[cfg(all(test, unix))]
fn cached_copy(root: &Path, bytes: &[u8], digest: &str) -> Result<Executable> {
    cached_payload(root, &Payload::raw(bytes, digest), |_| true)
}
/// The copy of `payload` under `root`. A copy the marker vouches for is
/// used as it is. Otherwise it is checked against the digest, or written
/// anew under a lock, then `usable` must accept it before the marker
/// vouches for it: a copy this system refuses to start never gets one,
/// only the refusal marker, which spares the next process the digest but
/// not the question, as a remount or a new policy may let it start.
fn cached_payload(
    root: &Path,
    payload: &Payload<'_>,
    usable: impl Fn(&Executable) -> bool,
) -> Result<Executable> {
    let digest = &payload.digest;
    ensure!(digest.len() == 64, "bundled FFmpeg has no digest");
    private_dir(root)?;
    let dir = root.join(format!("ffmpeg-{}", &digest[..16]));
    let in_use = hold(&dir)?;
    let path = dir.join(FFMPEG_NAME);
    let copy = Executable {
        path: path.clone(),
        probes: Some(dir.join("probes.json")),
        _in_use: Some(in_use),
    };
    let checked =
        |marker| trusted(&path, payload.length).is_ok() && marked(&dir, marker, &path, digest);
    let intact = || {
        trusted(&path, payload.length).is_ok()
            && sha256_file(&path).ok().as_deref() == Some(digest.as_str())
    };
    if !checked(MARKER) {
        let extracting = lock_file(&dir.join("extract.lock"))?;
        fs2::FileExt::lock_exclusive(&extracting)?;
        // Another process may have finished the copy while this one waited.
        if !checked(MARKER) {
            let refused_before = checked(REFUSED_MARKER);
            if !refused_before && !intact() {
                extract(&path, payload)?;
            }
            // Best effort: without a marker the next process hashes the
            // copy again.
            if !usable(&copy) {
                if !refused_before {
                    let _ = mark(&dir, REFUSED_MARKER, &path, digest);
                }
                bail!("the folder does not allow running programs");
            }
            let _ = mark(&dir, MARKER, &path, digest);
            if refused_before {
                let _ = fs::remove_file(dir.join(REFUSED_MARKER));
            }
        }
    }
    prune(root, &dir);
    Ok(copy)
}
/// Write `payload` to `path` through a new file renamed into place.
fn extract(path: &Path, payload: &Payload<'_>) -> Result<()> {
    let temporary = path.with_file_name(format!("{FFMPEG_NAME}.{}.tmp", nonce()?));
    let written = write_payload(&temporary, payload).and_then(|()| {
        // Windows cannot rename over an existing file.
        #[cfg(windows)]
        let _ = fs::remove_file(path);
        Ok(fs::rename(&temporary, path)?)
    });
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error.context("cannot extract bundled FFmpeg"));
    }
    // A file system that ignores modes would fail every later check.
    trusted(path, payload.length)
}

/// What identifies the file at `path`: replacing it or writing to it
/// changes its identity.
fn identity(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!(
            "{} {} {} {}.{} {}.{}",
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec()
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Some(format!(
            "{} {} {}",
            meta.len(),
            meta.last_write_time(),
            meta.creation_time()
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let modified = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(format!("{} {}", meta.len(), modified.as_nanos()))
    }
}
/// The marker line for the copy at `path` of `digest`.
fn marker_line(path: &Path, digest: &str) -> Option<String> {
    Some(format!("{digest} {}\n", identity(path)?))
}
/// Whether `marker` in `dir` vouches for the copy at `path` as it is now.
fn marked(dir: &Path, marker: &str, path: &Path, digest: &str) -> bool {
    let Some(expected) = marker_line(path, digest) else {
        return false;
    };
    let Ok(file) = fs::File::open(dir.join(marker)) else {
        return false;
    };
    let mut text = String::new();
    file.take(1024).read_to_string(&mut text).is_ok() && text == expected
}
/// Vouch with `marker` for the copy at `path`, just checked against
/// `digest`.
fn mark(dir: &Path, marker: &str, path: &Path, digest: &str) -> Result<()> {
    let line = marker_line(path, digest).context("the copy is gone")?;
    let temporary = dir.join(format!("{marker}.{}.tmp", nonce()?));
    let written = super::write_new(&temporary, line.as_bytes(), false)
        .and_then(|()| Ok(fs::rename(&temporary, dir.join(marker))?));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Lock the copy in `dir` shared. Another build's cleanup may move the
/// directory away while this process waits for the lock, so the lock must
/// still be the one in `dir` once it is held.
fn hold(dir: &Path) -> Result<fs::File> {
    let path = dir.join("in-use.lock");
    for _ in 0..3 {
        private_dir(dir)?;
        let lock = lock_file(&path)?;
        fs2::FileExt::lock_shared(&lock)?;
        if same_file(&lock, &path) {
            return Ok(lock);
        }
    }
    bail!("the FFmpeg cache directory keeps changing")
}
#[cfg(unix)]
fn same_file(file: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(open), Ok(named)) => (open.dev(), open.ino()) == (named.dev(), named.ino()),
        _ => false,
    }
}
/// Windows does not move a directory while a file in it is open.
#[cfg(not(unix))]
fn same_file(_: &fs::File, path: &Path) -> bool {
    path.exists()
}
/// Remove copies left by other builds that no process is using. A copy in
/// use, or one Windows will not delete while it runs, stays.
fn prune(root: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if dir == keep || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        // Left by a cleanup that was cut short.
        if name.starts_with("removed-") {
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        if !name.starts_with("ffmpeg-") {
            continue;
        }
        let Ok(lock) = lock_file(&dir.join("in-use.lock")) else {
            continue;
        };
        if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
            discard(root, &dir, lock);
        }
    }
}
/// Delete an unused copy. It is moved aside while still locked, so a process
/// of that build starting now finds no copy rather than one about to go.
#[cfg(unix)]
fn discard(root: &Path, dir: &Path, _lock: fs::File) {
    let Ok(nonce) = nonce() else {
        return;
    };
    let removed = root.join(format!("removed-{nonce}"));
    if fs::rename(dir, &removed).is_ok() {
        let _ = fs::remove_dir_all(&removed);
    }
}
/// Windows moves no directory with an open file in it, and deletes no copy
/// that is running.
#[cfg(not(unix))]
fn discard(_: &Path, dir: &Path, lock: fs::File) {
    drop(lock);
    let _ = fs::remove_dir_all(dir);
}
/// Create with the same private permissions as other extracted files, then
/// validate the entire compressed stream before callers can run or rename it.
fn write_payload(path: &Path, payload: &Payload<'_>) -> Result<()> {
    let mut file = super::new_private_file(path, true)?;
    payload.write_to(&mut file)
}
/// Create `dir` for this user alone, or accept it when it already is.
fn private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    ensure!(meta.is_dir(), "{} is not a directory", dir.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            meta.uid() == crate::dirs::effective_uid(),
            "{} belongs to another user",
            dir.display()
        );
        if meta.mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}
/// A regular file of the expected length that only this user can change.
fn trusted(path: &Path, length: usize) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.len() == length as u64,
        "extracted FFmpeg is missing or incomplete"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.uid() == crate::dirs::effective_uid() && meta.mode() & 0o022 == 0,
            "extracted FFmpeg is writable by another user"
        );
    }
    Ok(())
}
fn lock_file(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
#[cfg(test)]
thread_local! {
    /// Copies this test thread hashed.
    static HASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    #[cfg(test)]
    HASHED.set(HASHED.get() + 1);
    let mut file = fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    let mut bytes = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{process::Command, thread, time::Duration};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-bundled-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    const PAYLOAD: &[u8] = b"#!/bin/sh\necho stand-in for FFmpeg\n";
    #[cfg(unix)]
    fn digest(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }
    fn compressed_payload(bytes: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        let mut packed = PAYLOAD_MAGIC.to_vec();
        packed.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        packed.extend_from_slice(&sha2::Sha256::digest(bytes));
        let mut encoder = flate2::write::ZlibEncoder::new(packed, flate2::Compression::best());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    #[test]
    fn compressed_ffmpeg_extraction_preserves_bytes_and_reuses_the_cache() {
        let base = temp_dir("compressed-cache");
        let root = base.join("capturefab");
        let bytes = PAYLOAD.repeat(10000);
        let packed = compressed_payload(&bytes);
        assert!(packed.len() < bytes.len());
        let payload = Payload::compressed(&packed).unwrap();
        let copy = cached_payload(&root, &payload, |_| true).unwrap();
        assert_eq!(fs::read(&copy.path).unwrap(), bytes);
        // Existing copies need no valid compressed stream: no decoding occurs
        // when the length and digest already match the file in the cache.
        let corrupt = Payload {
            bytes: b"invalid",
            ..payload
        };
        assert_eq!(
            cached_payload(&root, &corrupt, |_| true).unwrap().path,
            copy.path
        );
        drop(copy);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn compressed_ffmpeg_rejects_corruption_and_length_mismatches_without_publishing() {
        let base = temp_dir("compressed-invalid");
        let good = compressed_payload(PAYLOAD);
        let header = PAYLOAD_MAGIC.len() + 8 + 32;
        let mut checksum = good.clone();
        *checksum.last_mut().unwrap() ^= 1;
        let mut digest = good.clone();
        digest[header - 1] ^= 1;
        let mut shorter = good.clone();
        shorter[PAYLOAD_MAGIC.len()..PAYLOAD_MAGIC.len() + 8]
            .copy_from_slice(&((PAYLOAD.len() - 1) as u64).to_le_bytes());
        let mut longer = good.clone();
        longer[PAYLOAD_MAGIC.len()..PAYLOAD_MAGIC.len() + 8]
            .copy_from_slice(&((PAYLOAD.len() + 1) as u64).to_le_bytes());
        let mut trailing = good.clone();
        trailing.push(0);
        for (index, bad) in [
            checksum,
            digest,
            shorter,
            longer,
            trailing,
            good[..good.len() - 1].to_vec(),
        ]
        .iter()
        .enumerate()
        {
            let root = base.join(index.to_string());
            let payload = Payload::compressed(bad).unwrap();
            assert!(cached_payload(&root, &payload, |_| true).is_err());
            let dir = root.join(format!("ffmpeg-{}", &payload.digest[..16]));
            assert!(!dir.join(FFMPEG_NAME).exists());
            assert!(!dir.join(MARKER).exists());
            assert!(fs::read_dir(dir).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            }));
        }
        assert!(Payload::compressed(&good[..header]).is_err());
        assert!(Payload::compressed(b"invalid").is_err());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn bundled_ffmpeg_is_extracted_once_and_shared() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = temp_dir("cache");
        let root = base.join("capturefab");
        let digest = digest(PAYLOAD);
        // Concurrent first uses produce one complete copy.
        let copies = thread::scope(|scope| {
            let workers = (0..8)
                .map(|_| scope.spawn(|| cached_copy(&root, PAYLOAD, &digest).unwrap()))
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let path = copies[0].path.clone();
        assert!(copies.iter().all(|copy| copy.path == path));
        assert_eq!(fs::read(&path).unwrap(), PAYLOAD);
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        for dir in [&root, &path.parent().unwrap().to_path_buf()] {
            assert_eq!(
                fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|entry| {
                let name = entry.as_ref().unwrap().file_name();
                name.to_string_lossy().ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
        // A later use reuses the file instead of writing it again, and the
        // copy outlives its users.
        drop(copies);
        let again = cached_copy(&root, PAYLOAD, &digest).unwrap();
        let reused = fs::metadata(&again.path).unwrap();
        assert_eq!(
            (reused.ino(), reused.mtime(), reused.mtime_nsec()),
            (meta.ino(), meta.mtime(), meta.mtime_nsec())
        );
        drop(again);
        assert!(path.exists());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn a_checked_copy_is_not_hashed_again() {
        let base = temp_dir("marker");
        let root = base.join("capturefab");
        let digest = digest(PAYLOAD);
        // The first look finds no copy, hashing nothing, and writes one,
        // checked while decoding.
        let before = HASHED.get();
        let copy = cached_copy(&root, PAYLOAD, &digest).unwrap();
        let dir = copy.path.parent().unwrap().to_path_buf();
        assert!(marked(&dir, MARKER, &copy.path, &digest));
        // Later processes trust the marker: hundreds of runs a minute on a
        // small board would otherwise each hash the whole executable.
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        let first = HASHED.get();
        assert_eq!(first, before);
        // A copy whose marker is gone, as one an older build left, is
        // hashed once and marked.
        fs::remove_file(dir.join(MARKER)).unwrap();
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        assert_eq!(HASHED.get(), first + 1);
        assert!(marked(&dir, MARKER, &copy.path, &digest));
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        assert_eq!(HASHED.get(), first + 1);
        // A marker for another digest, or nonsense, vouches for nothing.
        assert!(!marked(&dir, MARKER, &copy.path, &"0".repeat(64)));
        fs::write(dir.join(MARKER), "nonsense\n").unwrap();
        assert!(!marked(&dir, MARKER, &copy.path, &digest));
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn a_copy_this_system_will_not_start_gets_no_marker() {
        let base = temp_dir("refused");
        let root = base.join("capturefab");
        let digest = digest(PAYLOAD);
        let refused = cached_payload(&root, &Payload::raw(PAYLOAD, &digest), |_| false);
        assert!(refused.is_err());
        let dir = root.join(format!("ffmpeg-{}", &digest[..16]));
        let path = dir.join(FFMPEG_NAME);
        assert!(!dir.join(MARKER).exists());
        assert!(marked(&dir, REFUSED_MARKER, &path, &digest));
        // Asked again, it is asked to start again, without hashing the
        // copy: a folder that runs nothing would cost every command that.
        let hashed = HASHED.get();
        let asked = std::cell::Cell::new(false);
        let again = cached_payload(&root, &Payload::raw(PAYLOAD, &digest), |_| {
            asked.set(true);
            false
        });
        assert!(again.is_err());
        assert!(asked.get());
        assert_eq!(HASHED.get(), hashed);
        // Once the system starts it, as after a remount, it is vouched for.
        cached_payload(&root, &Payload::raw(PAYLOAD, &digest), |_| true).unwrap();
        assert_eq!(HASHED.get(), hashed);
        assert!(marked(&dir, MARKER, &path, &digest));
        assert!(!dir.join(REFUSED_MARKER).exists());
        // A refusal marker vouches only for the file it saw.
        fs::remove_file(dir.join(MARKER)).unwrap();
        mark(&dir, REFUSED_MARKER, &path, &digest).unwrap();
        let mut tampered = PAYLOAD.to_vec();
        tampered[16] ^= 1;
        thread::sleep(Duration::from_millis(10));
        fs::write(&path, &tampered).unwrap();
        let _ = cached_payload(&root, &Payload::raw(PAYLOAD, &digest), |_| false);
        assert_eq!(HASHED.get(), hashed + 1);
        assert_eq!(fs::read(&path).unwrap(), PAYLOAD);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn tampered_or_loosened_copies_fail_validation_and_are_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp_dir("tamper");
        let root = base.join("capturefab");
        let digest = digest(PAYLOAD);
        let path = cached_copy(&root, PAYLOAD, &digest).unwrap().path.clone();
        // Same length, different bytes: the write changes the file the
        // marker vouched for, and the digest notices.
        let mut tampered = PAYLOAD.to_vec();
        tampered[16] ^= 1;
        thread::sleep(Duration::from_millis(10));
        fs::write(&path, &tampered).unwrap();
        assert!(trusted(&path, PAYLOAD.len()).is_ok());
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PAYLOAD);
        // Writable by others, or cut short: the check before each use fails.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o722)).unwrap();
        assert!(trusted(&path, PAYLOAD.len()).is_err());
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        fs::write(&path, &PAYLOAD[..10]).unwrap();
        assert!(trusted(&path, PAYLOAD.len()).is_err());
        cached_copy(&root, PAYLOAD, &digest).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PAYLOAD);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn copies_of_other_builds_are_removed_unless_in_use() {
        let base = temp_dir("prune");
        let root = base.join("capturefab");
        let (busy, idle) = (
            root.join("ffmpeg-0000000000000001"),
            root.join("ffmpeg-0000000000000002"),
        );
        for dir in [&busy, &idle] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join(FFMPEG_NAME), b"old").unwrap();
        }
        // Left by a cleanup that was cut short.
        let leftover = root.join("removed-0123");
        fs::create_dir_all(&leftover).unwrap();
        let holder = lock_file(&busy.join("in-use.lock")).unwrap();
        fs2::FileExt::lock_shared(&holder).unwrap();
        let copy = cached_copy(&root, PAYLOAD, &digest(PAYLOAD)).unwrap();
        assert!(busy.exists() && !idle.exists() && copy.path.exists());
        let names = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|name| name.starts_with("ffmpeg-")));
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn a_copy_removed_while_waiting_for_its_lock_is_extracted_again() {
        let base = temp_dir("prune-race");
        let root = base.join("capturefab");
        let dir = root.join(format!("ffmpeg-{}", &digest(PAYLOAD)[..16]));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FFMPEG_NAME), PAYLOAD).unwrap();
        // Another build's cleanup holds the copy's lock.
        let cleanup = lock_file(&dir.join("in-use.lock")).unwrap();
        fs2::FileExt::lock_exclusive(&cleanup).unwrap();
        let copy = thread::scope(|scope| {
            let starting = scope.spawn(|| cached_copy(&root, PAYLOAD, &digest(PAYLOAD)));
            thread::sleep(Duration::from_millis(200));
            discard(&root, &dir, cleanup);
            starting.join().unwrap().unwrap()
        });
        // The copy this process holds is in place, not the one removed.
        assert_eq!(fs::read(&copy.path).unwrap(), PAYLOAD);
        assert!(same_file(
            copy._in_use.as_ref().unwrap(),
            &dir.join("in-use.lock")
        ));
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn copies_the_system_will_not_start_are_detected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("noexec");
        let path = dir.join(FFMPEG_NAME);
        fs::write(&path, PAYLOAD).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(runs(&Executable::external(path.clone())));
        // What a noexec mount or an execution policy reports.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!runs(&Executable::external(path)));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn extracted_copy_is_reused_by_another_process() {
        use std::os::unix::fs::MetadataExt;
        if let Some(root) = std::env::var_os("CAPTUREFAB_TEST_EXTRACT_ROOT") {
            let copy = cached_copy(Path::new(&root), PAYLOAD, &digest(PAYLOAD)).unwrap();
            println!("inode {}", fs::metadata(&copy.path).unwrap().ino());
            return;
        }
        let base = temp_dir("shared");
        let root = base.join("capturefab");
        let copy = cached_copy(&root, PAYLOAD, &digest(PAYLOAD)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "media::bundled::tests::extracted_copy_is_reused_by_another_process",
                "--nocapture",
            ])
            .env("CAPTUREFAB_TEST_EXTRACT_ROOT", &root)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        let inode = fs::metadata(&copy.path).unwrap().ino();
        assert!(stdout.contains(&format!("inode {inode}\n")), "{stdout}");
        fs::remove_dir_all(base).unwrap();
    }
}
