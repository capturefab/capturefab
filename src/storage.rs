//! Writing captures and recordings: a file never replaces another, a disk
//! is never filled past its free-space floor, and a file is on disk before
//! it is reported.
//!
//! A still is written to a hidden `.NAME.<pid>-<rand>.partial` beside its
//! name, synced, and published by a rename that refuses to replace
//! anything, so nobody reads half an image and two writers never share a
//! name: the second gets [`OutputExists`], which naming answers with the
//! next free name. File systems that cannot publish without replacing
//! (exFAT, some network shares) get the still created under its name
//! instead. A recording streams from FFmpeg into a new file under its name,
//! up to its size limit. The folders a job or command published in are
//! synced once when it ends ([`Folders`]), and a folder a write creates is
//! synced into its parent as it is created, so the names survive a power
//! cut too.
//!
//! Writes leave a free-space floor on their disk: the camera worker's own
//! (`CAPTUREFAB_MIN_FREE` where it runs, else 1% of the disk), or the
//! higher one the command that asked for the file set, which a session's
//! worker, running with the session's environment, gets with the request.
use crate::types::{Categorized, ErrorKind, Usage, hex, human_bytes};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// Overrides the free-space floor: bytes, or a size such as `2G`; `0`
/// turns it off.
pub const MIN_FREE_ENV: &str = "CAPTUREFAB_MIN_FREE";
/// The floor is 1% of the disk, within these bounds: enough for the system
/// to keep working on a small card, and no more than a big array needs.
const FLOOR_MIN: u64 = 512 << 20;
const FLOOR_MAX: u64 = 20 << 30;
/// A recording looks at the free space after this many bytes, or this
/// long, whichever comes first.
const LOOK_BYTES: u64 = 64 << 20;
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// The output path is already taken. Callers that pick names retry with the
/// next free one when this is in the error chain; nothing is ever replaced.
#[derive(Debug)]
pub struct OutputExists(pub PathBuf);
impl std::fmt::Display for OutputExists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "output already exists: {}; Capturefab never overwrites files, so choose a new name",
            self.0.display()
        )
    }
}
impl std::error::Error for OutputExists {}

/// A recording reached the size it was given, which ends it as asked.
#[derive(Debug)]
pub struct MaxSize(pub u64);
impl std::fmt::Display for MaxSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size = human_bytes(self.0);
        write!(f, "the recording reached its maximum size of {size}")
    }
}
impl std::error::Error for MaxSize {}

/// Whether files reach the disk before they are reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Each file's data as it is written, and its folder when the job ends.
    #[default]
    File,
    /// Nothing: the system writes files back in its own time, for scratch
    /// captures a power cut may lose.
    #[value(name = "none")]
    #[serde(rename = "none")]
    Off,
}
impl SyncMode {
    pub fn is_file(&self) -> bool {
        *self == Self::File
    }
}

/// A saved file's size and, when asked for, its SHA-256 in lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub bytes: u64,
    pub sha256: Option<String>,
}

/// Free and total bytes for unprivileged writes on one disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub free: u64,
    pub total: u64,
}

/// The disk holding `path`, or its nearest existing parent, as its file
/// system reports it; None when it does not, as some network ones do not.
pub fn space(path: &Path) -> Option<Space> {
    let path = std::path::absolute(path).ok()?;
    let existing = path.ancestors().find(|dir| dir.exists())?;
    #[cfg(test)]
    if let Some(space) = tests::SPACE.get() {
        return space(existing);
    }
    let stats = fs2::statvfs(existing).ok()?;
    (stats.total_space() > 0).then(|| Space {
        free: stats.available_space(),
        total: stats.total_space(),
    })
}

/// The free space writes leave on a disk of `total` bytes: 1% of it, at
/// least 512 MiB and at most 20 GiB, or `CAPTUREFAB_MIN_FREE`.
pub fn floor(total: u64) -> Result<u64> {
    floor_with(std::env::var_os(MIN_FREE_ENV).as_deref(), total)
}
/// The `CAPTUREFAB_MIN_FREE` this process was given, in bytes, when it
/// was: what a command sends with the writes it asks a camera worker for.
pub fn min_free() -> Result<Option<u64>> {
    min_free_with(std::env::var_os(MIN_FREE_ENV).as_deref())
}
fn min_free_with(setting: Option<&std::ffi::OsStr>) -> Result<Option<u64>> {
    let setting = setting.filter(|value| !value.is_empty());
    setting
        .map(|setting| floor_with(Some(setting), 0))
        .transpose()
}
/// The floor a write leaves on a disk of `total` bytes: this process's, or
/// `min_free`, the command's own, when that is higher. True when it is this
/// process's.
fn floor_for(total: u64, min_free: Option<u64>) -> Result<(u64, bool)> {
    let own = floor(total)?;
    Ok(match min_free {
        Some(asked) if asked > own => (asked, false),
        _ => (own, true),
    })
}
fn floor_with(setting: Option<&std::ffi::OsStr>, total: u64) -> Result<u64> {
    let Some(setting) = setting.filter(|value| !value.is_empty()) else {
        return Ok((total / 100).clamp(FLOOR_MIN, FLOOR_MAX));
    };
    match setting.to_str().map(str::trim) {
        Some("0") => Ok(0),
        text => text
            .and_then(|text| crate::types::parse_bytes(text).ok())
            .ok_or_else(|| {
                Usage(format!(
                    "{MIN_FREE_ENV} must be bytes or a size such as 2G, or 0 to turn the floor off"
                ))
                .into()
            }),
    }
}

/// Refuse to add `adding` bytes at `path` when that would leave less than
/// the floor free on its disk ([`floor_for`] with `min_free`):
/// `storage_full`, with `{path, free_bytes}`. A disk that does not report
/// its space is not refused.
fn ensure_room(path: &Path, adding: u64, min_free: Option<u64>) -> Result<()> {
    let Some(space) = space(path) else {
        return Ok(());
    };
    let (floor, own) = floor_for(space.total, min_free)?;
    if floor > 0 && space.free.saturating_sub(adding) < floor {
        let free = space.free;
        return Err(full(path, NoRoom::Floor { free, floor, own }));
    }
    Ok(())
}

/// Why a disk has no room for more of a file.
#[derive(Clone, Debug)]
enum NoRoom {
    /// `free` bytes are left, below the `floor` Capturefab keeps: this
    /// process's own when `own`, else the one the command set.
    Floor { free: u64, floor: u64, own: bool },
    /// A write was refused: the disk, or the user's quota on it, is full.
    Refused(String),
}

/// `storage_full` for `path`, with `{path, free_bytes}` either way.
fn full(path: &Path, no_room: NoRoom) -> anyhow::Error {
    let (message, free, hint) = match no_room {
        NoRoom::Floor { free, floor, own } => {
            let hint = format!(
                "free some space, write to another disk, or lower {MIN_FREE_ENV} (0 turns the floor off)"
            );
            (
                format!(
                    "{} free on the disk of {}, and Capturefab leaves {} free",
                    human_bytes(free),
                    path.display(),
                    human_bytes(floor)
                ),
                Some(free),
                // A session's own floor is the one it started with.
                if own {
                    crate::session::with_environment_hint(hint, MIN_FREE_ENV)
                } else {
                    hint
                },
            )
        }
        NoRoom::Refused(error) => (
            format!("cannot write {}: {error}", path.display()),
            space(path).map(|space| space.free),
            "free some space or write to another disk".into(),
        ),
    };
    Categorized::new(ErrorKind::StorageFull, message)
        .hint(hint)
        .details(serde_json::json!({ "path": path, "free_bytes": free }))
        .into()
}

/// Whether a write failed for want of room rather than for another reason.
fn no_room(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

/// The folder `path` goes in, created when missing, unless the path has
/// control characters, it is under the mount point of a drive that is not
/// there, or `adding` bytes there would leave less than the floor free.
/// With `sync`, each folder it creates is synced into its parent.
fn folder(path: &Path, adding: u64, sync: SyncMode, min_free: Option<u64>) -> Result<PathBuf> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.iter().any(u8::is_ascii_control) {
        return Err(Usage("an output path must not contain control characters".into()).into());
    }
    path.file_name()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| anyhow!("capture output must name a file"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    crate::volumes::ensure_present(parent)?;
    ensure_room(path, adding, min_free)?;
    // The folders it lacks, deepest first: a new folder's name is in its
    // parent, which syncing the folder itself does not make durable.
    let missing: Vec<&Path> = parent
        .ancestors()
        .take_while(|dir| {
            !dir.as_os_str().is_empty()
                && matches!(fs::symlink_metadata(dir), Err(error) if error.kind() == io::ErrorKind::NotFound)
        })
        .collect();
    let cannot = || format!("cannot create folder {}", parent.display());
    fs::create_dir_all(parent).with_context(cannot)?;
    if sync.is_file() {
        for dir in missing.iter().rev() {
            let up = dir.parent().filter(|up| !up.as_os_str().is_empty());
            let up = up.unwrap_or_else(|| Path::new("."));
            sync_dir(up).with_context(|| format!("cannot sync {}", up.display()))?;
        }
    }
    Ok(parent.to_path_buf())
}

/// A hidden name beside `path` for its bytes until they are published,
/// unique to this process and write.
fn partial(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("no file name")?.to_string_lossy();
    // Room for the rest within the 255 bytes most file systems allow.
    let mut end = name.len().min(200);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let (pid, random) = (std::process::id(), crate::types::random_hex(6)?);
    let partial = format!(".{}.{pid}-{random}.partial", &name[..end]);
    Ok(path.with_file_name(partial))
}

/// Whether a file name is a still being written: numbering skips these.
pub fn is_partial(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".partial")
}

/// Folders whose file systems cannot publish without replacing, where
/// stills are created under their names, so the rest of a burst does not
/// write each one twice.
static DIRECT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
fn direct(dir: &Path, remember: bool) -> bool {
    let mut known = DIRECT.lock().unwrap_or_else(PoisonError::into_inner);
    if remember {
        if known.len() >= 64 {
            known.remove(0);
        }
        known.push(dir.to_path_buf());
    }
    known.iter().any(|known| known == dir)
}

/// Save encoded bytes as a new file at `path`, never replacing anything
/// there, leaving the floor free with the command's own `min_free` (see
/// [`floor_for`]), with its SHA-256 when `hash`. A failure leaves nothing
/// behind.
pub fn save(
    bytes: &[u8],
    path: &Path,
    sync: SyncMode,
    min_free: Option<u64>,
    hash: bool,
) -> Result<Stored> {
    save_parts(&[bytes], path, sync, min_free, hash)
}

/// `save` for a file made of `parts` in order, written together, without
/// putting them in one buffer first: an encoded still's header or metadata
/// and the frame's own bytes.
pub fn save_parts(
    parts: &[&[u8]],
    path: &Path,
    sync: SyncMode,
    min_free: Option<u64>,
    hash: bool,
) -> Result<Stored> {
    let length: usize = parts.iter().map(|part| part.len()).sum();
    let dir = folder(path, length as u64, sync, min_free)?;
    crate::naming::check_free(path)?;
    let stored = Stored {
        bytes: length as u64,
        sha256: hash.then(|| {
            let mut digest = Sha256::new();
            parts.iter().for_each(|part| digest.update(part));
            hex(digest.finalize())
        }),
    };
    let taken = |error| failed(path, error);
    if !direct(&dir, false) {
        let partial = partial(path)?;
        write_new(&partial, parts, sync).map_err(taken)?;
        #[cfg(test)]
        if let Some(race) = tests::RACE.get() {
            race(path);
        }
        match publish(&partial, path) {
            Ok(true) => return Ok(stored),
            published => {
                let _ = fs::remove_file(&partial);
                published.map_err(taken)?;
                direct(&dir, true);
            }
        }
    }
    write_new(path, parts, sync).map_err(taken)?;
    Ok(stored)
}

/// Save one file a command writes on its own, such as a preview or an
/// exported preset, as [`save`] does, with its folder synced too.
pub fn save_one(bytes: &[u8], path: &Path, min_free: Option<u64>) -> Result<Stored> {
    let stored = save(bytes, path, SyncMode::File, min_free, false)?;
    Folders::of(path).sync()?;
    Ok(stored)
}

/// `OutputExists` for a name that is taken, `storage_full` for a disk with
/// no room, else what failed writing it.
fn failed(path: &Path, error: io::Error) -> anyhow::Error {
    if error.kind() == io::ErrorKind::AlreadyExists {
        return OutputExists(path.to_path_buf()).into();
    }
    if no_room(&error) {
        return full(path, NoRoom::Refused(error.to_string()));
    }
    anyhow::Error::from(error).context(format!("cannot write {}", path.display()))
}

/// Create `path`, which must not exist, holding `parts` in order; removed
/// again when writing them fails.
fn write_new(path: &Path, parts: &[&[u8]], sync: SyncMode) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut slices: Vec<io::IoSlice> = parts.iter().map(|part| io::IoSlice::new(part)).collect();
    crate::copies::record(
        crate::copies::Site::FileWrite,
        parts.iter().map(|part| part.len()).sum(),
    );
    let mut written = crate::frame::write_all_vectored(&mut file, &mut slices);
    if sync.is_file() {
        written = written.and_then(|()| file.sync_data());
    }
    if written.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    written
}

/// Give `from` the name `to` unless something has that name: an
/// `AlreadyExists` error then. False where the file system can neither
/// rename without replacing nor make hard links.
fn publish(from: &Path, to: &Path) -> io::Result<bool> {
    match rename_noreplace(from, to) {
        Err(error) if unsupported(&error) => {}
        renamed => return renamed.map(|()| true),
    }
    match fs::hard_link(from, to) {
        Ok(()) => {
            // Left behind, it is only a hidden second name of the file.
            let _ = fs::remove_file(from);
            Ok(true)
        }
        Err(error) if unsupported(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// A file system that lacks the operation, rather than one that refused it:
/// vfat answers link() with EPERM, Linux renameat2 flags a file system
/// lacks are EINVAL, and macOS volumes without RENAME_EXCL say ENOTSUP.
fn unsupported(error: &io::Error) -> bool {
    #[cfg(unix)]
    let lacking = {
        use libc::{EINVAL, ENOSYS, ENOTSUP, EOPNOTSUPP, EPERM};
        [EINVAL, ENOSYS, ENOTSUP, EOPNOTSUPP, EPERM]
    };
    #[cfg(not(unix))]
    let lacking: [i32; 0] = [];
    error.kind() == io::ErrorKind::Unsupported
        || error
            .raw_os_error()
            .is_some_and(|code| lacking.contains(&code))
}

#[cfg(unix)]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// A C call's result: 0, or the error it left.
#[cfg(unix)]
fn zero(result: impl Into<i64>) -> io::Result<()> {
    (result.into() == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    let (call, at, flags) = (libc::SYS_renameat2, libc::AT_FDCWD, libc::RENAME_NOREPLACE);
    // The system call, as glibc has its wrapper only since 2.28.
    // SAFETY: two NUL-terminated paths, both relative to the working folder.
    zero(unsafe { libc::syscall(call, at, from.as_ptr(), at, to.as_ptr(), flags) })
}

#[cfg(target_vendor = "apple")]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: two NUL-terminated paths.
    zero(unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) })
}

/// `path` as the NUL-terminated wide string a Windows call takes. From
/// 248 units with its NUL, where std stops passing paths as they are (some
/// calls take less than MAX_PATH), it is made absolute and normal and
/// given its `\\?\` form, which has no such limit, as std does: a still
/// whose partial file std wrote beside a long name is published under it.
#[cfg(windows)]
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let plain: Vec<u16> = path.as_os_str().encode_wide().collect();
    let wide = if plain.len() + 1 < 248 {
        plain
    } else {
        let full = std::path::absolute(path)?;
        verbatim(&full.as_os_str().encode_wide().collect::<Vec<u16>>())
    };
    Ok(wide.into_iter().chain([0]).collect())
}
/// The `\\?\` form of an absolute, normal Windows path: `C:\x` is
/// `\\?\C:\x`, `\\server\share` is `\\?\UNC\server\share` and
/// `\\.\` becomes `\\?\`; one in that form already, or any other,
/// stays as it is.
#[cfg(any(windows, test))]
fn verbatim(full: &[u16]) -> Vec<u16> {
    const SEP: u16 = b'\\' as u16;
    const COLON: u16 = b':' as u16;
    const QUERY: u16 = b'?' as u16;
    const DOT: u16 = b'.' as u16;
    let prefix = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let (head, rest) = match full {
        [_, COLON, SEP, ..] => (prefix(r"\\?\"), full),
        [SEP, SEP, QUERY, SEP, ..] | [SEP, QUERY, QUERY, SEP, ..] => (Vec::new(), full),
        [SEP, SEP, DOT, SEP, rest @ ..] => (prefix(r"\\?\"), rest),
        [SEP, SEP, rest @ ..] => (prefix(r"\\?\UNC\"), rest),
        _ => (Vec::new(), full),
    };
    [head.as_slice(), rest].concat()
}

#[cfg(windows)]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::ERROR_SHARING_VIOLATION, Storage::FileSystem::MoveFileExW,
    };
    let (from, to) = (wide(from)?, wide(to)?);
    // Antivirus and search indexers hold a new file open for a moment.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // Without MOVEFILE_REPLACE_EXISTING, a name that exists fails it.
        // SAFETY: two NUL-terminated wide paths that outlive the call.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SHARING_VIOLATION as i32)
            || Instant::now() >= deadline
        {
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn rename_noreplace(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Make the names published in `dir` durable. Folders that cannot be
/// synced, as on some network shares, are skipped.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    tests::SYNCED.with_borrow_mut(|synced| synced.push(dir.to_path_buf()));
    // NTFS journals its folders, and std cannot open one as a file.
    #[cfg(unix)]
    match File::open(dir).and_then(|dir| dir.sync_all()) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied || unsupported(&error) => {}
        synced => return synced,
    }
    let _ = dir;
    Ok(())
}

/// The folders a job or command published files in, synced once when it
/// ends rather than after every file.
#[derive(Debug, Default)]
pub struct Folders(BTreeSet<PathBuf>);
impl Folders {
    /// The folder of one published file.
    pub fn of(file: &Path) -> Self {
        let mut folders = Self::default();
        folders.add(file);
        folders
    }
    /// Note the folder of a published file.
    pub fn add(&mut self, file: &Path) {
        let dir = file.parent().filter(|dir| !dir.as_os_str().is_empty());
        self.0
            .insert(dir.unwrap_or_else(|| Path::new(".")).to_owned());
    }
    /// Sync each folder noted since the last call: the first failure, after
    /// trying them all.
    pub fn sync(&mut self) -> Result<()> {
        let synced = std::mem::take(&mut self.0)
            .into_iter()
            .map(|dir| sync_dir(&dir).with_context(|| format!("cannot sync {}", dir.display())));
        synced.collect::<Vec<_>>().into_iter().collect()
    }
}

/// Why a recording's file took no more bytes before it was finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    /// It reached the size it was given: a normal end.
    MaxSize,
    /// A write failed, as on a full disk; the bytes before it stand.
    Failed,
}

/// What a recording found about its disk, for whoever stops the
/// recording, which a disk below its floor or full must do.
#[derive(Debug)]
pub struct Alarm {
    path: PathBuf,
    /// Why the disk has no room, once it has none.
    no_room: Mutex<Option<NoRoom>>,
}
impl Alarm {
    /// The `storage_full` error to stop the recording with, once its disk
    /// is below the floor or refused a write for want of room.
    pub fn error(&self) -> Option<anyhow::Error> {
        let no_room = self.no_room.lock().unwrap_or_else(PoisonError::into_inner);
        no_room.clone().map(|no_room| full(&self.path, no_room))
    }
    fn raise(&self, no_room: NoRoom) {
        *self.no_room.lock().unwrap_or_else(PoisonError::into_inner) = Some(no_room);
    }
}

/// A recording's file: bytes streamed into a new file under its name, up to
/// an exact size when it has one. Its disk is looked at every 64 MiB or
/// second; below the floor its [`Alarm`] goes off while it keeps taking
/// bytes, so the recording can be finished properly. A write the full disk
/// refuses sets it off too.
pub struct Recording {
    file: Option<File>,
    path: PathBuf,
    limit: Option<u64>,
    written: u64,
    /// The command's own floor, as [`save`] takes it.
    min_free: Option<u64>,
    /// Bytes written, and when, at the last look at free space.
    looked: (u64, Instant),
    alarm: Arc<Alarm>,
    cut: Option<Cut>,
}
impl Recording {
    /// Create `path`, which must not exist, for at most `limit` bytes,
    /// leaving the floor free with the command's own `min_free`.
    pub fn create(path: &Path, limit: Option<u64>, min_free: Option<u64>) -> Result<Self> {
        ensure!(
            limit != Some(0),
            "a recording's maximum size must be positive"
        );
        folder(path, 0, SyncMode::File, min_free)?;
        // Read too, so its container can be finished in place (`amend`).
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| failed(path, error))?;
        Ok(Self {
            file: Some(file),
            path: path.to_path_buf(),
            limit,
            written: 0,
            min_free,
            looked: (0, Instant::now()),
            alarm: Arc::new(Alarm {
                path: path.to_path_buf(),
                no_room: Mutex::new(None),
            }),
            cut: None,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn bytes_written(&self) -> u64 {
        self.written
    }
    /// Why it stopped taking bytes, if it did.
    pub fn cut(&self) -> Option<Cut> {
        self.cut
    }
    pub fn alarm(&self) -> Arc<Alarm> {
        self.alarm.clone()
    }
    /// Look at the free space, after enough bytes or time.
    fn look(&mut self) {
        let (bytes, at) = self.looked;
        if self.written - bytes < LOOK_BYTES && at.elapsed() < LOOK_EVERY {
            return;
        }
        self.looked = (self.written, Instant::now());
        // The setting was checked when the file was created.
        if let Some(space) = space(&self.path)
            && let Ok((floor, own)) = floor_for(space.total, self.min_free)
            && space.free < floor
        {
            let free = space.free;
            self.alarm.raise(NoRoom::Floor { free, floor, own });
        }
    }
    /// Read and change the bytes written so far before the file is
    /// finished, as a container's index and length written after its video:
    /// `edit` gets the file and the bytes it may still grow by, and leaves
    /// its position anywhere.
    pub fn amend<T>(&mut self, edit: impl FnOnce(&mut File, u64) -> io::Result<T>) -> Result<T> {
        let file = self.file.as_mut().context("recording already finished")?;
        let room = self.limit.map_or(u64::MAX, |limit| limit - self.written);
        let cannot = || format!("cannot finish recording {}", self.path.display());
        let result = edit(file, room).with_context(cannot);
        // Its size, whatever the edit managed.
        let len = io::Seek::seek(file, io::SeekFrom::End(0)).with_context(cannot)?;
        self.written = len;
        ensure!(
            self.limit.is_none_or(|limit| len <= limit),
            "recording {} outgrew its maximum size",
            self.path.display()
        );
        result
    }
    /// Close the file with its bytes on disk, and its folder: its size. A
    /// file cut short keeps the bytes before the cut, as every recording
    /// container plays up to its last complete part.
    pub fn finish(mut self) -> Result<u64> {
        let file = self.file.take().context("recording already finished")?;
        let cannot = || format!("cannot flush recording {}", self.path.display());
        file.sync_all().with_context(cannot)?;
        Folders::of(&self.path).sync()?;
        Ok(self.written)
    }
    /// Remove a recording that holds no video.
    pub fn abort(mut self) -> Result<()> {
        self.file.take();
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("cannot remove {}", self.path.display()))
            }
            _ => Ok(()),
        }
    }
}
impl Write for Recording {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = self.limit.map_or(u64::MAX, |limit| limit - self.written);
        if room == 0 {
            self.cut = Some(Cut::MaxSize);
        }
        match self.cut {
            Some(Cut::MaxSize) => {
                let limit = MaxSize(self.written);
                return Err(io::Error::new(io::ErrorKind::FileTooLarge, limit));
            }
            Some(Cut::Failed) => return Err(io::Error::other("an earlier write failed")),
            None => {}
        }
        // Exactly up to the limit: the bytes that fit, then nothing more.
        let bytes = &bytes[..bytes.len().min(usize::try_from(room).unwrap_or(usize::MAX))];
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("finished"))?;
        let count = file.write(bytes).map_err(|error| {
            // A failed write wrote nothing, so the bytes before it stand.
            self.cut = Some(Cut::Failed);
            if no_room(&error) {
                self.alarm.raise(NoRoom::Refused(error.to_string()));
            }
            let message = format!("cannot write recording {}: {error}", self.path.display());
            io::Error::new(error.kind(), message)
        })?;
        self.written += count as u64;
        self.look();
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), Write::flush)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    type Measure = fn(&Path) -> Option<Space>;
    thread_local! {
        /// The free space a test reports for every disk.
        pub(super) static SPACE: Cell<Option<Measure>> = const { Cell::new(None) };
        /// What a test does between a still's write and its publish.
        pub(super) static RACE: Cell<Option<fn(&Path)>> = const { Cell::new(None) };
        /// The folders synced so far.
        pub(super) static SYNCED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    }

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let random = crate::types::random_hex(8).unwrap();
            let dir = std::env::temp_dir().join(format!("capturefab-{name}-{random}"));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn partials(&self) -> Vec<String> {
            fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| is_partial(name))
                .collect()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            SPACE.set(None);
            RACE.set(None);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn reason(error: &anyhow::Error) -> ErrorKind {
        crate::types::classify(error).kind
    }

    #[test]
    fn a_name_taken_before_publishing_is_never_replaced_and_numbering_moves_on() {
        use crate::naming::{NameInput, Namer, OnExists, Plan};
        let temp = Temp::new("publish-race");
        // Another writer takes the first name while this one writes.
        RACE.set(Some(|path| {
            if path.ends_with("shot-0001.png") || path.ends_with("fixed.png") {
                fs::write(path, b"theirs").unwrap();
            }
        }));
        let template = temp.path("shot-{seq}.png");
        let plan = Plan::new(template.to_str().unwrap(), 2, "png");
        let (camera, context) = (Default::default(), Default::default());
        let input = NameInput {
            camera: &camera,
            now: crate::naming::clock(None),
            index: 0,
            context: &context,
        };
        let namer = Mutex::new(Namer::default());
        let first = namer
            .lock()
            .unwrap()
            .candidate(&plan, &input, OnExists::Fail)
            .unwrap();
        assert_eq!(first, temp.path("shot-0001.png"));
        let (path, stored) =
            crate::naming::save_named(&namer, &plan, &input, OnExists::Fail, first, |path| {
                save(b"ours", path, SyncMode::File, None, true)
            })
            .unwrap();
        assert_eq!(path, temp.path("shot-0002.png"));
        assert_eq!(fs::read(temp.path("shot-0001.png")).unwrap(), b"theirs");
        assert_eq!(fs::read(&path).unwrap(), b"ours");
        assert_eq!(stored.sha256, Some(hex(Sha256::digest(b"ours"))));
        // A fixed name fails as taken, and its partial file is gone.
        let error = save(
            b"ours",
            &temp.path("fixed.png"),
            SyncMode::File,
            None,
            false,
        )
        .unwrap_err();
        assert_eq!(reason(&error), ErrorKind::Exists);
        assert_eq!(fs::read(temp.path("fixed.png")).unwrap(), b"theirs");
        assert!(temp.partials().is_empty(), "{:?}", temp.partials());
    }

    #[test]
    fn parts_are_saved_in_order_with_one_hash_and_never_over_a_file() {
        let temp = Temp::new("parts");
        let parts: [&[u8]; 4] = [b"P5\n2 1\n255\n", &[], &[7, 9], b""];
        let whole = parts.concat();
        let path = temp.path("shot.pgm");
        let stored = save_parts(&parts, &path, SyncMode::File, None, true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), whole);
        assert_eq!(
            (stored.bytes, stored.sha256),
            (whole.len() as u64, Some(hex(Sha256::digest(&whole))))
        );
        // The name is taken: nothing is replaced and no partial file stays.
        let error = save_parts(&[b"other"], &path, SyncMode::File, None, false).unwrap_err();
        assert_eq!(reason(&error), ErrorKind::Exists);
        assert_eq!(fs::read(&path).unwrap(), whole);
        assert!(temp.partials().is_empty(), "{:?}", temp.partials());
        // Published through a partial file, as one file is.
        RACE.set(Some(|path| {
            assert!(!path.exists(), "published only once written");
        }));
        let second = temp.path("second.pgm");
        save_parts(&parts, &second, SyncMode::Off, None, false).unwrap();
        assert_eq!(fs::read(&second).unwrap(), whole);
        // Nothing to write is an empty file.
        save_parts(&[], &temp.path("empty"), SyncMode::Off, None, true).unwrap();
        assert_eq!(fs::read(temp.path("empty")).unwrap(), b"");
    }

    #[test]
    fn file_systems_that_cannot_publish_get_files_created_under_their_names() {
        let temp = Temp::new("publish-direct");
        direct(&temp.0, true);
        // A partial file would be seen here.
        RACE.set(Some(|_| panic!("no partial file on such a file system")));
        let path = temp.path("shot.raw");
        save(b"one", &path, SyncMode::Off, None, false).unwrap();
        let error = save(b"two", &path, SyncMode::File, None, false).unwrap_err();
        assert_eq!(reason(&error), ErrorKind::Exists);
        assert_eq!(fs::read(&path).unwrap(), b"one");
    }

    #[test]
    fn output_paths_with_control_characters_create_nothing() {
        let temp = Temp::new("control");
        for name in ["a\nb.png", "a\0b.png"] {
            let path = temp.path("new").join(name);
            let errors = [
                save(b"x", &path, SyncMode::File, None, false).unwrap_err(),
                save_one(b"x", &path, None).unwrap_err(),
                Recording::create(&path, None, None).err().unwrap(),
            ];
            for error in errors {
                assert_eq!(reason(&error), ErrorKind::Usage, "{error:#}");
            }
            assert!(!temp.path("new").exists(), "{name:?}");
        }
    }

    #[test]
    fn a_disk_with_no_room_is_storage_full_with_its_free_space() {
        let temp = Temp::new("no-room");
        let mut recording = Recording::create(&temp.path("clip.mkv"), None, None).unwrap();
        SPACE.set(Some(|_| {
            Some(Space {
                free: 4096,
                total: 10 << 30,
            })
        }));
        let path = temp.path("shot.png");
        let error = failed(&path, io::ErrorKind::StorageFull.into());
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(details["path"], path.to_str().unwrap());
        assert_eq!(details["free_bytes"], 4096);
        // A recording whose disk refuses a write raises its alarm, as one
        // below the floor does, where a device that is always full exists.
        let Ok(device) = OpenOptions::new().write(true).open("/dev/full") else {
            return;
        };
        recording.write_all(b"header").unwrap();
        recording.file = Some(device);
        let alarm = recording.alarm();
        recording.write_all(b"video").unwrap_err();
        assert_eq!(recording.cut(), Some(Cut::Failed));
        let error = alarm.error().expect("no room");
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(details["free_bytes"], 4096);
    }

    #[test]
    fn partial_names_are_hidden_beside_their_file_and_never_too_long() {
        let path = Path::new("/data/run").join(format!("{}.png", "é".repeat(150)));
        let partial = partial(&path).unwrap();
        let name = partial.file_name().unwrap().to_str().unwrap();
        assert_eq!(partial.parent(), path.parent());
        assert!(is_partial(name) && name.len() <= 255, "{name}");
        assert_ne!(partial, super::partial(&path).unwrap());
        assert!(!is_partial("shot.png") && !is_partial(".hidden.png"));
    }

    #[test]
    fn long_windows_paths_take_their_verbatim_form() {
        let form = |path: &str| {
            String::from_utf16(&verbatim(&path.encode_utf16().collect::<Vec<_>>())).unwrap()
        };
        assert_eq!(form(r"C:\Users\ana\shot.png"), r"\\?\C:\Users\ana\shot.png");
        assert_eq!(form(r"\\nas\share\shot.png"), r"\\?\UNC\nas\share\shot.png");
        assert_eq!(form(r"\\.\C:\shot.png"), r"\\?\C:\shot.png");
        for kept in [
            r"\\?\C:\shot.png",
            r"\\?\UNC\nas\s\a",
            r"\??\C:\a",
            "shot.png",
        ] {
            assert_eq!(form(kept), kept);
        }
    }

    #[test]
    fn the_floor_is_one_percent_within_bounds_or_the_setting() {
        let os = |text: &str| Some(std::ffi::OsString::from(text));
        for (setting, total, floor) in [
            (None, 16_000_000_000, 512 << 20),
            (None, 1_000_000_000_000, 10_000_000_000),
            (None, 16_000_000_000_000, 20 << 30),
            (os(""), 1_000_000_000_000, 10_000_000_000),
            (os("0"), 1_000_000_000_000, 0),
            (os("2G"), 1_000_000_000_000, 2 << 30),
            (os("1000"), 0, 1000),
        ] {
            assert_eq!(floor_with(setting.as_deref(), total).unwrap(), floor);
        }
        let error = floor_with(os("lots").as_deref(), 0).unwrap_err();
        assert_eq!(reason(&error), ErrorKind::Usage);
        // What a command sends a session's worker: bytes, when it set one.
        for (setting, sent) in [
            (None, None),
            (os(""), None),
            (os("0"), Some(0)),
            (os("2G"), Some(2 << 30)),
        ] {
            assert_eq!(min_free_with(setting.as_deref()).unwrap(), sent);
        }
        let error = min_free_with(os("lots").as_deref()).unwrap_err();
        assert_eq!(reason(&error), ErrorKind::Usage);
    }

    #[test]
    fn writes_leave_the_free_space_floor() {
        let temp = Temp::new("floor");
        // 512 MiB and 100 bytes free on a 10 GiB disk, whose floor is 512 MiB.
        SPACE.set(Some(|_| {
            Some(Space {
                free: (512 << 20) + 100,
                total: 10 << 30,
            })
        }));
        save(
            &[1; 100],
            &temp.path("fits.raw"),
            SyncMode::File,
            None,
            false,
        )
        .unwrap();
        let path = temp.path("big.raw");
        let error = save(&[1; 101], &path, SyncMode::File, None, false).unwrap_err();
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(details["path"], path.to_str().unwrap());
        assert_eq!(details["free_bytes"], (512 << 20) + 100);
        assert!(!path.exists() && temp.partials().is_empty());
        // A command's own floor, as a session's worker gets it, holds when
        // it is higher, and a lower one leaves the worker's in place.
        let error = save(&[1; 1], &path, SyncMode::File, Some(1 << 30), false).unwrap_err();
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        assert!(
            format!("{error}").contains("leaves 1.00 GiB free"),
            "{error}"
        );
        let hint = crate::types::classify(&error).hint.unwrap();
        assert!(hint.contains("lower CAPTUREFAB_MIN_FREE"), "{hint}");
        let error = save(&[1; 101], &path, SyncMode::File, Some(0), false).unwrap_err();
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        save(&[1; 50], &path, SyncMode::File, Some(1000), false).unwrap();
        // A recording starts only above the floor too.
        let clip = temp.path("clip.mkv");
        let error = Recording::create(&clip, None, Some(1 << 30)).err().unwrap();
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        assert!(!clip.exists());
        SPACE.set(Some(|_| {
            Some(Space {
                free: 1 << 20,
                total: 10 << 30,
            })
        }));
        let error = Recording::create(&clip, None, None).err().unwrap();
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        assert!(!clip.exists());
    }

    #[test]
    fn new_folders_are_synced_into_their_parents() {
        let temp = Temp::new("new-folders");
        SYNCED.take();
        save(
            b"x",
            &temp.path("a/b/c/one.png"),
            SyncMode::File,
            None,
            false,
        )
        .unwrap();
        let parents = [temp.0.clone(), temp.path("a"), temp.path("a/b")];
        assert_eq!(SYNCED.take(), parents);
        // Folders that exist, or files not synced at all, sync none here.
        save(
            b"x",
            &temp.path("a/b/c/two.png"),
            SyncMode::File,
            None,
            false,
        )
        .unwrap();
        save(b"x", &temp.path("d/e/one.png"), SyncMode::Off, None, false).unwrap();
        assert_eq!(SYNCED.take(), Vec::<PathBuf>::new());
        // A recording's new folder too, and its own once it is finished.
        let clip = temp.path("f/g/clip.mkv");
        let mut recording = Recording::create(&clip, None, None).unwrap();
        assert_eq!(SYNCED.take(), [temp.0.clone(), temp.path("f")]);
        recording.write_all(b"video").unwrap();
        assert_eq!(recording.finish().unwrap(), 5);
        assert_eq!(SYNCED.take(), [temp.path("f/g")]);
    }

    #[test]
    fn a_recording_below_the_floor_raises_its_alarm_and_keeps_its_bytes() {
        let temp = Temp::new("recording-alarm");
        let path = temp.path("clip.mkv");
        let mut recording = Recording::create(&path, None, None).unwrap();
        let alarm = recording.alarm();
        recording.write_all(b"header").unwrap();
        assert!(alarm.error().is_none());
        SPACE.set(Some(|_| {
            Some(Space {
                free: 1 << 20,
                total: 10 << 30,
            })
        }));
        // A second after the last look, the next bytes look again.
        recording.looked.1 -= LOOK_EVERY;
        recording.write_all(b"video").unwrap();
        let error = alarm.error().expect("below the floor");
        assert_eq!(reason(&error), ErrorKind::StorageFull);
        assert_eq!(
            crate::types::classify(&error).details.unwrap()["free_bytes"],
            1 << 20
        );
        assert_eq!(recording.cut(), None);
        assert_eq!(recording.finish().unwrap(), 11);
        assert_eq!(fs::read(&path).unwrap(), b"headervideo");
        // Above the worker's 512 MiB floor, a command's own higher one
        // sets it off.
        SPACE.set(Some(|_| {
            Some(Space {
                free: 2 << 30,
                total: 10 << 30,
            })
        }));
        let mut recording = Recording::create(&temp.path("own.mkv"), None, Some(1 << 30)).unwrap();
        recording.write_all(b"header").unwrap();
        SPACE.set(Some(|_| {
            Some(Space {
                free: 768 << 20,
                total: 10 << 30,
            })
        }));
        recording.looked.1 -= LOOK_EVERY;
        recording.write_all(b"video").unwrap();
        let error = recording
            .alarm()
            .error()
            .expect("below the command's floor");
        assert!(
            format!("{error}").contains("leaves 1.00 GiB free"),
            "{error}"
        );
    }

    #[test]
    fn a_recording_ends_exactly_at_its_maximum_size() {
        let temp = Temp::new("recording-max-size");
        let path = temp.path("clip.mkv");
        let mut recording = Recording::create(&path, Some(10), None).unwrap();
        let error = io::copy(&mut &b"0123456789abcdef"[..], &mut recording).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(recording.cut(), Some(Cut::MaxSize));
        assert_eq!(recording.finish().unwrap(), 10);
        assert_eq!(fs::read(&path).unwrap(), b"0123456789");
        // Without one, a recording past the old 512 MiB default goes on.
        let mut open = Recording::create(&temp.path("long.mkv"), None, None).unwrap();
        open.written = 600 << 20;
        open.write_all(b"more").unwrap();
        assert_eq!((open.cut(), open.bytes_written()), (None, (600 << 20) + 4));
        // Its name is its own: a second recording cannot take it.
        let taken = Recording::create(&path, None, None).err().unwrap();
        assert_eq!(reason(&taken), ErrorKind::Exists);
    }

    /// A container's index and length go in after its video, within the
    /// recording's size, which counts them.
    #[test]
    fn a_recording_is_amended_in_place_within_its_size() {
        use std::io::{Read, Seek, SeekFrom};
        let temp = Temp::new("recording-amend");
        let path = temp.path("clip.mkv");
        let mut recording = Recording::create(&path, Some(16), None).unwrap();
        recording.write_all(b"head0123").unwrap();
        let read = recording
            .amend(|file, room| {
                assert_eq!(room, 8);
                let mut start = [0; 4];
                file.seek(SeekFrom::Start(0))?;
                file.read_exact(&mut start)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(b"HEAD")?;
                file.seek(SeekFrom::End(0))?;
                file.write_all(b"cues")?;
                Ok(start)
            })
            .unwrap();
        assert_eq!(&read, b"head");
        assert_eq!(recording.bytes_written(), 12);
        // One that outgrows it fails, and still counts what it wrote.
        let error = recording
            .amend(|file, _| {
                file.seek(SeekFrom::End(0))?;
                file.write_all(b"too long!")
            })
            .unwrap_err();
        assert!(error.to_string().contains("maximum size"), "{error:#}");
        assert_eq!(recording.bytes_written(), 21);
        assert_eq!(recording.finish().unwrap(), 21);
        assert_eq!(fs::read(&path).unwrap(), b"HEAD0123cuestoo long!");
    }

    #[test]
    fn an_aborted_recording_is_removed_and_folders_sync_once() {
        let temp = Temp::new("recording-abort");
        let path = temp.path("empty.mkv");
        Recording::create(&path, None, None)
            .unwrap()
            .abort()
            .unwrap();
        assert!(!path.exists());
        let mut folders = Folders::default();
        folders.add(&temp.path("a.png"));
        folders.add(&temp.path("b.png"));
        folders.add(&temp.path("sub/c.png"));
        assert_eq!(folders.0.len(), 2);
        fs::create_dir_all(temp.path("sub")).unwrap();
        folders.sync().unwrap();
        assert!(folders.0.is_empty());
    }
}
