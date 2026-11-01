//! Where Capturefab keeps files: the runtime folder (session descriptors,
//! camera locks, previews and other files needed only while processes run),
//! the state folder (presets, saved sign-ins, sessions' jobs and logs, kept
//! across reboots) and the cache (the extracted FFmpeg and GenICam XML, made
//! again when missing).
//!
//! Each is the first of its candidates that is, or can be made, a private
//! folder of this user's (0700, owned, not a symbolic link) that this
//! process may write, so a read-only home or root file system falls back to
//! the tmpfs folders such systems keep writable: `/run/user/<uid>`,
//! `/run/capturefab-<uid>` for root, then `$TMPDIR` or `/tmp`. A folder set
//! by a variable is used as given, never second-guessed. Folders are found
//! once per process, the first time a command needs one: a command that
//! needs none never looks, and one that finds none fails only where it
//! needs it, with a typed error naming the variable that fixes it.
#![allow(unsafe_code)]
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Chooses the runtime folder, and the state folder when
/// `CAPTUREFAB_STATE_DIR` does not.
pub(crate) const SESSION_DIR_ENV: &str = "CAPTUREFAB_SESSION_DIR";
/// Chooses the state folder.
pub(crate) const STATE_DIR_ENV: &str = "CAPTUREFAB_STATE_DIR";
/// Chooses the cache.
pub(crate) const CACHE_DIR_ENV: &str = "CAPTUREFAB_CACHE_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Runtime,
    State,
    Cache,
}

/// A folder a kind may live in, and what chose it, as doctor names it: a
/// variable, `login` (`/run/user/<uid>`), `home`, `run`
/// (`/run/capturefab-<uid>`), `temp`, `runtime` (the cache in the runtime
/// folder) or `test`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub path: PathBuf,
    pub from: &'static str,
    /// Set by a variable: used as given, with no fallback.
    pub explicit: bool,
}
impl Candidate {
    fn new(path: PathBuf, from: &'static str) -> Self {
        Self {
            path,
            from,
            explicit: false,
        }
    }
    fn explicit(path: PathBuf, from: &'static str) -> Self {
        Self {
            path,
            from,
            explicit: true,
        }
    }
}

/// The folder chosen for a kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chosen {
    pub path: PathBuf,
    pub from: &'static str,
    /// False for a state folder this process can read and not write, such
    /// as one in a read-only image.
    pub writable: bool,
}

/// A candidate passed over, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Passed {
    pub path: PathBuf,
    pub from: &'static str,
    pub why: String,
}

/// What looking for a kind's folder found.
#[derive(Debug)]
pub(crate) struct Found {
    pub chosen: Option<Chosen>,
    pub passed: Vec<Passed>,
    /// Why there is none, as commands that need it report it.
    refusal: Option<Refusal>,
}
#[derive(Debug, Clone)]
struct Refusal {
    kind: ErrorKind,
    message: String,
    hint: Option<String>,
}
impl Found {
    /// Why there is no folder, as commands that need one report it.
    pub(crate) fn missing(&self) -> Option<&str> {
        self.chosen
            .is_none()
            .then(|| {
                self.refusal
                    .as_ref()
                    .map(|refusal| refusal.message.as_str())
            })
            .flatten()
    }
    fn error(&self) -> anyhow::Error {
        let refusal = self.refusal.clone().unwrap_or(Refusal {
            kind: ErrorKind::PermissionDenied,
            message: "no folder".into(),
            hint: None,
        });
        let mut error = Categorized::new(refusal.kind, refusal.message);
        error.hint = refusal.hint;
        error.into()
    }
    fn path(&self) -> Result<PathBuf> {
        self.chosen
            .as_ref()
            .map(|chosen| chosen.path.clone())
            .ok_or_else(|| self.error())
    }
}

static RUNTIME: OnceLock<Found> = OnceLock::new();
static STATE: OnceLock<Found> = OnceLock::new();
static CACHE: OnceLock<Found> = OnceLock::new();
fn cell(kind: Kind) -> &'static OnceLock<Found> {
    match kind {
        Kind::Runtime => &RUNTIME,
        Kind::State => &STATE,
        Kind::Cache => &CACHE,
    }
}

/// What looking for `kind`'s folder found in this process.
pub(crate) fn found(kind: Kind) -> &'static Found {
    cell(kind).get_or_init(|| match kind {
        Kind::Runtime => choose(
            kind,
            &runtime_candidates(&var, uid(), login_runtime(), cfg!(target_os = "linux")),
        ),
        Kind::State => choose(kind, &state_candidates(&var, cfg!(target_os = "linux"))),
        Kind::Cache => {
            let mut candidates = cache_candidates(&var);
            if !candidates.iter().any(|candidate| candidate.explicit)
                && let Ok(runtime) = runtime()
            {
                candidates.push(Candidate::new(runtime.join("cache"), "runtime"));
            }
            choose(kind, &candidates)
        }
    })
}

/// Session descriptors and their locks, camera locks, inspection previews,
/// images captured to stdout through a session and the discovery cache:
/// files that matter only while the processes using them run. Without one,
/// no session can start and commands open their cameras for themselves.
pub(crate) fn runtime() -> Result<PathBuf> {
    found(Kind::Runtime).path()
}

/// What must outlive a reboot: presets, saved sign-ins, sessions' jobs and
/// logs. It may be read-only (`Chosen::writable`), as in a read-only image;
/// writing there fails with the variable that fixes it.
pub(crate) fn state() -> Result<PathBuf> {
    found(Kind::State).path()
}

/// What can be made again: the extracted FFmpeg, its encoder probes and
/// GenICam XML. None turns caching off.
pub(crate) fn cache() -> Option<PathBuf> {
    found(Kind::Cache)
        .chosen
        .as_ref()
        .map(|chosen| chosen.path.clone())
}

/// A private folder for short-lived files, such as previews, when the
/// runtime folder cannot hold them: the runtime folder, else this user's
/// folder in the system's temporary folder.
pub(crate) fn scratch() -> Result<PathBuf> {
    runtime().or_else(|error| temp().ok_or(error))
}

/// This user's private folder in the system's temporary folder, which
/// read-only systems keep writable: `$TMPDIR/capturefab-<uid>`, or
/// `%TEMP%\capturefab` on Windows. Made once, when first asked for.
pub(crate) fn temp() -> Option<PathBuf> {
    static TEMP: OnceLock<Option<PathBuf>> = OnceLock::new();
    TEMP.get_or_init(|| {
        let path = temp_candidate(&var, uid()).path;
        prepare(&path, false).ok().map(|_| path)
    })
    .clone()
}

/// The first usable candidate. An explicit one that is not usable ends the
/// search: the variable says where files go, and nowhere else will do.
fn choose(kind: Kind, candidates: &[Candidate]) -> Found {
    let mut passed = Vec::new();
    for candidate in candidates {
        if kind == Kind::Cache {
            make_private(&candidate.path);
        }
        match prepare(&candidate.path, kind == Kind::State) {
            Ok(writable) => {
                return Found {
                    chosen: Some(Chosen {
                        path: candidate.path.clone(),
                        from: candidate.from,
                        writable,
                    }),
                    passed,
                    refusal: None,
                };
            }
            Err(error) => {
                passed.push(Passed {
                    path: candidate.path.clone(),
                    from: candidate.from,
                    why: why(&error),
                });
                if candidate.explicit {
                    let refusal = match error.downcast_ref::<std::io::Error>() {
                        // A folder that cannot be made or written, as on a
                        // read-only file system, is named with the reason.
                        Some(io) => Refusal {
                            kind: ErrorKind::PermissionDenied,
                            message: format!(
                                "cannot write in {}: {}",
                                candidate.path.display(),
                                io_why(io)
                            ),
                            hint: Some(variable_hint(kind)),
                        },
                        None => {
                            let classified = crate::types::classify(&error);
                            Refusal {
                                kind: classified.kind,
                                message: format!("{error:#}"),
                                hint: classified.hint.or_else(|| Some(variable_hint(kind))),
                            }
                        }
                    };
                    return Found {
                        chosen: None,
                        passed,
                        refusal: Some(refusal),
                    };
                }
            }
        }
    }
    let tried = passed
        .iter()
        .map(|passed| format!("{}: {}", passed.path.display(), passed.why))
        .collect::<Vec<_>>();
    let message = format!(
        "no writable {} folder{}",
        match kind {
            Kind::Runtime => "runtime",
            Kind::State => "state",
            Kind::Cache => "cache",
        },
        if tried.is_empty() {
            String::new()
        } else {
            format!(" ({})", tried.join("; "))
        }
    );
    Found {
        chosen: None,
        passed,
        refusal: Some(Refusal {
            kind: ErrorKind::PermissionDenied,
            message,
            hint: Some(variable_hint(kind)),
        }),
    }
}

/// The variable that chooses `kind`'s folder, as the fix for a missing one.
fn variable_hint(kind: Kind) -> String {
    match kind {
        Kind::Runtime => format!(
            "set {SESSION_DIR_ENV} to a writable folder of your own, such as one `mktemp -d` makes"
        ),
        Kind::State => format!(
            "set {STATE_DIR_ENV} to a writable folder that outlives a reboot, such as {STATE_DIR_ENV}=/data/capturefab"
        ),
        Kind::Cache => format!("set {CACHE_DIR_ENV} to a writable folder of your own"),
    }
}

/// Why a folder cannot be used, briefly: `read-only file system`,
/// `belongs to another user`, ...
fn why(error: &anyhow::Error) -> String {
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        return io_why(io);
    }
    let text = format!("{error:#}");
    if text.contains("must be a real directory") {
        return "not a real directory".into();
    }
    ["belongs to another user", "group or others may enter it"]
        .into_iter()
        .find(|known| text.contains(known))
        .map_or(text.clone(), str::to_owned)
}
fn io_why(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::ReadOnlyFilesystem => "read-only file system".into(),
        std::io::ErrorKind::PermissionDenied => "not writable by this user".into(),
        std::io::ErrorKind::NotFound => "its parent does not exist".into(),
        _ => error.to_string(),
    }
}

/// Make `path` a private folder of this user's, or accept it when it is
/// one, and say whether this process may write in it. A folder it may not
/// write is refused, unless `read_only` accepts one that exists.
fn prepare(path: &Path, read_only: bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            check_private_dir(path)?;
            match writable(path) {
                Ok(()) => Ok(true),
                Err(_) if read_only => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private(path)?;
            check_private_dir(path)?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

/// Make a cache folder of this user's that others may enter private, such
/// as one made with plain `mkdir`, as extracting FFmpeg there always did:
/// it holds nothing others could have read. A runtime or state folder is
/// refused instead, with `chmod 700` as the fix: others may already have
/// read the session tokens or sign-in index in it.
fn make_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.is_dir()
            && meta.uid() == effective_uid()
            && meta.mode() & 0o077 != 0
        {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn create_private(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Whether this process may create files in `dir`: a read-only mount or a
/// folder without write permission says no. Root writes anywhere but on a
/// read-only mount.
pub(crate) fn writable(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        if unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    // Windows marks no folder read-only for this; creating a file there
    // is what finds out.
    #[cfg(not(unix))]
    {
        fs::metadata(dir).map(|_| ())
    }
}

/// Make `path` a private folder this process can write in, before writing
/// there. A folder that cannot be is `permission_denied`, with the variable
/// that moves what goes there as the hint.
pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    if !path.exists()
        && let Err(error) = create_private(path)
    {
        return Err(unwritable(path, &error));
    }
    check_private_dir(path)?;
    writable(path).map_err(|error| unwritable(path, &error))
}

/// `permission_denied` for writing in `path`, whose cause is `error`. The
/// hint names the variable of the folder `path` is in, among those this
/// process looked for: wording a hint never makes another folder.
fn unwritable(path: &Path, error: &std::io::Error) -> anyhow::Error {
    let kind = [Kind::State, Kind::Runtime, Kind::Cache]
        .into_iter()
        .find(|kind| {
            cell(*kind)
                .get()
                .and_then(|found| found.chosen.as_ref())
                .is_some_and(|chosen| path.starts_with(&chosen.path))
        })
        .unwrap_or(Kind::Runtime);
    Categorized::new(
        ErrorKind::PermissionDenied,
        format!("cannot write in {}: {}", path.display(), io_why(error)),
    )
    .hint(variable_hint(kind))
    .into()
}

/// A real directory that only this user can enter. One another user owns,
/// such as a squatted /tmp or /dev/shm name, is refused even if private.
/// A refusal is `permission_denied`, with what fixes it as the hint.
pub(crate) fn check_private_dir(path: &Path) -> Result<()> {
    let refused = |message: String, hint: String| -> anyhow::Error {
        Categorized::new(ErrorKind::PermissionDenied, message)
            .hint(hint)
            .into()
    };
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err(refused(
            format!("folder {} must be a real directory", path.display()),
            format!("set {SESSION_DIR_ENV} to a folder of your own, such as one `mktemp -d` makes"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.uid() != effective_uid() {
            return Err(refused(
                format!("folder {} belongs to another user", path.display()),
                format!(
                    "set {SESSION_DIR_ENV} to a folder of your own, such as one `mktemp -d` makes"
                ),
            ));
        }
        if m.mode() & 0o077 != 0 {
            let dir = crate::hint::shell_quote(&path.display().to_string());
            return Err(refused(
                format!(
                    "folder {} must be private, but its group or others may enter it",
                    path.display()
                ),
                format!("chmod 700 {dir}"),
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and always succeeds.
    unsafe { libc::geteuid() }
}
#[cfg(unix)]
fn uid() -> Option<u32> {
    Some(effective_uid())
}
#[cfg(not(unix))]
fn uid() -> Option<u32> {
    None
}

/// An environment variable that is set to something.
fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
/// An XDG base directory; the specification ignores relative ones.
fn xdg(var: &impl Fn(&str) -> Option<PathBuf>, name: &str) -> Option<PathBuf> {
    var(name).filter(|dir| dir.is_absolute())
}

/// Unit tests never touch the user's sessions or saved state: without
/// CAPTUREFAB_SESSION_DIR each test process gets its own private folder,
/// which its tests share as they would the real one.
fn test_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir().join(format!(
            "capturefab-test-sessions-{}-{nanos}",
            std::process::id()
        ))
    })
    .clone()
}

/// The folder sessions keep on disk without a runtime folder of the login:
/// `~/.cache/capturefab/sessions`, or `%LOCALAPPDATA%` on Windows. On
/// macOS and Windows it holds state too.
fn disk_session_dir(var: &impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    if cfg!(windows)
        && let Some(dir) = var("LOCALAPPDATA")
    {
        return Some(dir.join("capturefab").join("sessions"));
    }
    var("HOME").map(|home| home.join(".cache").join("capturefab").join("sessions"))
}

/// This user's folder in the system's temporary folder.
fn temp_candidate(var: &impl Fn(&str) -> Option<PathBuf>, uid: Option<u32>) -> Candidate {
    let temp = if cfg!(windows) {
        var("TEMP").or_else(|| var("TMP"))
    } else {
        xdg(var, "TMPDIR")
    }
    .unwrap_or_else(|| {
        if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        }
    });
    let name = match uid {
        Some(uid) => format!("capturefab-{uid}"),
        None => "capturefab".into(),
    };
    Candidate::new(temp.join(name), "temp")
}

/// The runtime folder's candidates: CAPTUREFAB_SESSION_DIR alone when
/// set, else the defaults.
fn runtime_candidates(
    var: &impl Fn(&str) -> Option<PathBuf>,
    uid: Option<u32>,
    login: Option<PathBuf>,
    linux: bool,
) -> Vec<Candidate> {
    if let Some(dir) = var(SESSION_DIR_ENV) {
        return vec![Candidate::explicit(dir, SESSION_DIR_ENV)];
    }
    if cfg!(test) {
        return vec![Candidate::explicit(test_dir(), "test")];
    }
    runtime_defaults(var, uid, login, linux)
}
/// The runtime folder's default candidates in order:
/// `$XDG_RUNTIME_DIR/capturefab`; without it, the login's `/run/user/<uid>`
/// when usable (`login`), so a service and an SSH shell find the same
/// sessions; the folder on disk; `/run/capturefab-<uid>` for root on Linux;
/// and this user's temporary folder.
fn runtime_defaults(
    var: &impl Fn(&str) -> Option<PathBuf>,
    uid: Option<u32>,
    login: Option<PathBuf>,
    linux: bool,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    match xdg(var, "XDG_RUNTIME_DIR") {
        Some(dir) => candidates.push(Candidate::new(dir.join("capturefab"), "XDG_RUNTIME_DIR")),
        None => candidates.extend(login.map(|dir| Candidate::new(dir.join("capturefab"), "login"))),
    }
    candidates.extend(disk_session_dir(var).map(|dir| Candidate::new(dir, "home")));
    if linux && uid == Some(0) {
        candidates.push(Candidate::new(PathBuf::from("/run/capturefab-0"), "run"));
    }
    candidates.push(temp_candidate(var, uid));
    candidates
}

/// The state folder's candidates: CAPTUREFAB_STATE_DIR, else
/// CAPTUREFAB_SESSION_DIR, alone when set; else the default.
fn state_candidates(var: &impl Fn(&str) -> Option<PathBuf>, linux: bool) -> Vec<Candidate> {
    let session = var(SESSION_DIR_ENV);
    // A CAPTUREFAB_STATE_DIR exported in the shell, as for a service,
    // must not take unit tests to the user's presets: only one that also
    // sets CAPTUREFAB_SESSION_DIR chooses its own folders.
    if cfg!(test) && session.is_none() {
        return vec![Candidate::explicit(test_dir(), "test")];
    }
    if let Some(dir) = var(STATE_DIR_ENV) {
        return vec![Candidate::explicit(dir, STATE_DIR_ENV)];
    }
    if let Some(dir) = session {
        return vec![Candidate::explicit(dir, SESSION_DIR_ENV)];
    }
    state_default(var, linux).into_iter().collect()
}
/// XDG_STATE_HOME or `~/.local/state` on Linux, whose runtime folder is a
/// RAM disk that logout clears, and the folder sessions keep on disk
/// elsewhere. Nothing without a home: there is nowhere state would outlive
/// a reboot.
fn state_default(var: &impl Fn(&str) -> Option<PathBuf>, linux: bool) -> Option<Candidate> {
    if linux {
        return match xdg(var, "XDG_STATE_HOME") {
            Some(dir) => Some(Candidate::new(dir.join("capturefab"), "XDG_STATE_HOME")),
            None => {
                var("HOME").map(|home| Candidate::new(home.join(".local/state/capturefab"), "home"))
            }
        };
    }
    match xdg(var, "XDG_RUNTIME_DIR") {
        Some(dir) => Some(Candidate::new(dir.join("capturefab"), "XDG_RUNTIME_DIR")),
        None => disk_session_dir(var).map(|dir| Candidate::new(dir, "home")),
    }
}

/// The cache's candidates: CAPTUREFAB_CACHE_DIR alone when set, else the
/// platform's. `found` adds the runtime folder.
fn cache_candidates(var: &impl Fn(&str) -> Option<PathBuf>) -> Vec<Candidate> {
    if let Some(dir) = var(CACHE_DIR_ENV) {
        return vec![Candidate::explicit(dir, CACHE_DIR_ENV)];
    }
    // Tests never touch the user's cache.
    if cfg!(test) {
        return vec![Candidate::explicit(
            std::env::temp_dir().join("capturefab-test-cache"),
            "test",
        )];
    }
    cache_default(var).into_iter().collect()
}
/// ~/Library/Caches on macOS, %LOCALAPPDATA% on Windows, and
/// $XDG_CACHE_HOME or ~/.cache elsewhere.
fn cache_default(var: &impl Fn(&str) -> Option<PathBuf>) -> Option<Candidate> {
    if cfg!(target_os = "macos") {
        return var("HOME")
            .map(|home| Candidate::new(home.join("Library/Caches/capturefab"), "home"));
    }
    if cfg!(windows) {
        return var("LOCALAPPDATA")
            .map(|dir| Candidate::new(dir.join("capturefab").join("cache"), "home"));
    }
    match xdg(var, "XDG_CACHE_HOME") {
        Some(dir) => Some(Candidate::new(dir.join("capturefab"), "XDG_CACHE_HOME")),
        None => {
            var("HOME").map(|home| Candidate::new(home.join(".cache").join("capturefab"), "home"))
        }
    }
}

/// Whether a process without XDG_RUNTIME_DIR can keep its sessions in the
/// login's runtime folder, /run/user/<uid>, where logins look for them.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginDir {
    /// This user's alone, and kept from boot to shutdown for a lingering user.
    Usable,
    /// logind keeps it only while the user is logged in, so a service's
    /// session would vanish at logout.
    NotLingering,
    /// The user lingers, but logind has not made it yet.
    Missing,
    /// Another user's, or open to others.
    NotPrivate,
}
#[cfg(any(target_os = "linux", all(unix, test)))]
impl LoginDir {
    fn of(dir: &Path, uid: u32, lingers: bool) -> Self {
        match fs::symlink_metadata(dir) {
            Ok(_) if !owned_private(dir, uid) => Self::NotPrivate,
            Ok(_) if lingers => Self::Usable,
            Err(_) if lingers => Self::Missing,
            _ => Self::NotLingering,
        }
    }
}
/// The login's runtime folder when it is usable.
#[cfg(target_os = "linux")]
fn login_runtime() -> Option<PathBuf> {
    login_runtime_dir()
        .filter(|(_, login)| *login == LoginDir::Usable)
        .map(|(dir, _)| dir)
}
#[cfg(not(target_os = "linux"))]
fn login_runtime() -> Option<PathBuf> {
    None
}
/// The login's runtime folder and whether it is usable, decided once, so
/// a process keeps its descriptor, locks and buffers in one place however
/// logins come and go.
#[cfg(target_os = "linux")]
fn login_runtime_dir() -> Option<(PathBuf, LoginDir)> {
    static LOGIN: OnceLock<(PathBuf, LoginDir)> = OnceLock::new();
    let login = LOGIN.get_or_init(|| {
        let uid = effective_uid();
        let dir = PathBuf::from(format!("/run/user/{uid}"));
        let login = LoginDir::of(&dir, uid, lingers(uid));
        (dir, login)
    });
    Some(login.clone())
}
#[cfg(all(test, not(target_os = "linux")))]
fn login_runtime_dir() -> Option<(PathBuf, LoginDir)> {
    None
}
/// Whether logind keeps `uid`'s runtime folder from boot to shutdown
/// (`loginctl enable-linger`), not only while the user is logged in.
#[cfg(target_os = "linux")]
fn lingers(uid: u32) -> bool {
    user_name(uid).is_some_and(|name| {
        !name.is_empty() && Path::new("/var/lib/systemd/linger").join(name).exists()
    })
}
/// The name passwd gives `uid`.
#[cfg(target_os = "linux")]
fn user_name(uid: u32) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: getpwuid_r fills `entry`, with strings stored in `buffer`, and
    // points `found` at it only when it found the user.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }
    // SAFETY: pw_name is a NUL-terminated string in `buffer`, still alive.
    let name = unsafe { std::ffi::CStr::from_ptr((*found).pw_name) };
    Some(std::ffi::OsStr::from_bytes(name.to_bytes()).to_owned())
}
/// The login runtime folder that the runtime folder passed over for one on
/// disk or in a temporary folder, where logins look for sessions instead,
/// and why.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn missed_runtime_dir() -> Option<(PathBuf, LoginDir)> {
    if cfg!(test) || var(SESSION_DIR_ENV).is_some() || xdg(&var, "XDG_RUNTIME_DIR").is_some() {
        return None;
    }
    let logind = Path::new("/run/systemd/seats").is_dir() || Path::new("/run/user").is_dir();
    missed(login_runtime_dir(), logind)
}
/// In a login with XDG_RUNTIME_DIR, where a service started without it
/// keeps its sessions instead, with the login runtime folder it passes over
/// and why. An SSH login on a line computer would otherwise see no sign of a
/// service whose sessions it cannot find. The service's folder is the first
/// of its fallbacks that exists, as it made it, else the first.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn service_runtime_dir() -> Option<(PathBuf, PathBuf, LoginDir)> {
    if cfg!(test) || var(SESSION_DIR_ENV).is_some() || xdg(&var, "XDG_RUNTIME_DIR").is_none() {
        return None;
    }
    let logind = Path::new("/run/systemd/seats").is_dir() || Path::new("/run/user").is_dir();
    let (login, why) = missed(login_runtime_dir(), logind)?;
    let without_xdg = |name: &str| (name != "XDG_RUNTIME_DIR").then(|| var(name)).flatten();
    let fallbacks = runtime_defaults(&without_xdg, uid(), None, true);
    let private = |path: &Path| {
        #[cfg(unix)]
        {
            owned_private(path, effective_uid())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            false
        }
    };
    let dir = fallbacks
        .iter()
        .find(|candidate| private(&candidate.path))
        .or(fallbacks.first())?
        .path
        .clone();
    Some((dir, login, why))
}
/// A host without logind, such as a container, has no logins to miss.
#[cfg(any(target_os = "linux", test))]
fn missed(login: Option<(PathBuf, LoginDir)>, logind: bool) -> Option<(PathBuf, LoginDir)> {
    login.filter(|(_, login)| logind && *login != LoginDir::Usable)
}
/// A real directory owned by `uid` with mode 0700.
#[cfg(any(target_os = "linux", all(unix, test)))]
fn owned_private(dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == uid && meta.mode() & 0o777 == 0o700)
}

/// Whether the runtime folder is a login's /run/user that logind removes
/// at logout: a detached session's descriptor would vanish with it.
pub(crate) fn runtime_ends_at_logout() -> bool {
    #[cfg(target_os = "linux")]
    {
        let uid = effective_uid();
        runtime().is_ok_and(|dir| dir.starts_with(format!("/run/user/{uid}"))) && !lingers(uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Whether the file system holding `path` is mounted read-only, as a
/// Yocto-style root file system is.
#[cfg(unix)]
pub(crate) fn read_only_mount(path: &Path) -> Option<bool> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs fills `stats` for a NUL-terminated path and returns 0
    // only then.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded, so `stats` is initialized.
    let flags = unsafe { stats.assume_init() }.f_flag;
    #[allow(clippy::unnecessary_cast)]
    Some(flags as u64 & libc::ST_RDONLY as u64 != 0)
}
#[cfg(not(unix))]
pub(crate) fn read_only_mount(_: &Path) -> Option<bool> {
    None
}

/// Whether programs may not be started from the file system holding
/// `path` (mounted noexec), as /tmp and /dev/shm often are on hardened and
/// embedded systems. Unknown counts as allowed.
pub(crate) fn noexec_mount(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: statvfs fills `stats` for a NUL-terminated path and
        // returns 0 only then.
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        // SAFETY: statvfs succeeded, so `stats` is initialized.
        let flags = unsafe { stats.assume_init() }.f_flag;
        #[allow(clippy::unnecessary_cast)]
        {
            flags as u64 & libc::ST_NOEXEC as u64 != 0
        }
    }
    // macOS's statvfs has no noexec flag; statfs does.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: statfs fills `stats` for a NUL-terminated path and returns
        // 0 only then.
        if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        // SAFETY: statfs succeeded, so `stats` is initialized.
        let flags = unsafe { stats.assume_init() }.f_flags;
        flags & libc::MNT_NOEXEC as u32 != 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "capturefab-dirs-{name}-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            ensure_private_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for entry in walk(&self.0) {
                    let _ = fs::set_permissions(entry, fs::Permissions::from_mode(0o700));
                }
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(unix)]
    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![dir.to_path_buf()];
        let mut index = 0;
        while index < dirs.len() {
            let children = fs::read_dir(&dirs[index])
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .collect::<Vec<_>>();
            dirs.extend(children);
            index += 1;
        }
        dirs
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> {
        let vars: Vec<(String, PathBuf)> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.into()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(set, _)| set == name)
                .map(|(_, value)| value.clone())
        }
    }
    fn paths(candidates: &[Candidate]) -> Vec<PathBuf> {
        candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect()
    }
    #[test]
    fn tests_never_default_to_the_users_folders() {
        let dir = test_dir();
        assert!(dir.starts_with(std::env::temp_dir()), "{}", dir.display());
        assert_eq!(dir, test_dir(), "one folder per process");
        if std::env::var_os(SESSION_DIR_ENV).is_none() {
            assert_eq!(runtime().unwrap(), dir);
            // Even with CAPTUREFAB_STATE_DIR exported.
            assert_eq!(state().unwrap(), dir);
            let exported = env(&[(STATE_DIR_ENV, "/data/state")]);
            assert_eq!(
                paths(&state_candidates(&exported, true)),
                std::slice::from_ref(&dir)
            );
        }
        if std::env::var_os(CACHE_DIR_ENV).is_none() {
            assert!(cache().unwrap().starts_with(std::env::temp_dir()));
        }
        assert_eq!(missed_runtime_dir(), None);
    }

    #[test]
    fn a_variable_chooses_a_folder_alone() {
        let set = env(&[
            (SESSION_DIR_ENV, "/data/sessions"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/pi"),
        ]);
        let runtime = runtime_candidates(&set, Some(1000), None, true);
        assert_eq!(
            runtime,
            vec![Candidate::explicit(
                "/data/sessions".into(),
                SESSION_DIR_ENV
            )]
        );
        // CAPTUREFAB_SESSION_DIR moves state too, unless
        // CAPTUREFAB_STATE_DIR says where it goes.
        assert_eq!(
            paths(&state_candidates(&set, true)),
            [PathBuf::from("/data/sessions")]
        );
        let both = env(&[
            (SESSION_DIR_ENV, "/data/sessions"),
            (STATE_DIR_ENV, "/data/state"),
        ]);
        let state = state_candidates(&both, true);
        assert_eq!(
            state,
            vec![Candidate::explicit("/data/state".into(), STATE_DIR_ENV)]
        );
        let cache = cache_candidates(&env(&[(CACHE_DIR_ENV, "/data/cache")]));
        assert_eq!(
            cache,
            vec![Candidate::explicit("/data/cache".into(), CACHE_DIR_ENV)]
        );
    }

    #[test]
    fn an_explicit_folder_that_cannot_be_used_is_not_replaced() {
        let dir = TestDir::new("explicit");
        let file = dir.0.join("file");
        fs::write(&file, b"").unwrap();
        let found = choose(
            Kind::Runtime,
            &[
                Candidate::explicit(file.clone(), SESSION_DIR_ENV),
                Candidate::new(dir.0.join("fallback"), "temp"),
            ],
        );
        assert_eq!(found.chosen, None);
        assert_eq!(found.passed.len(), 1);
        let error = found.error();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind.code(), ("permission_denied", 3));
        assert!(!dir.0.join("fallback").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_falls_back_to_the_next_candidate() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("fallback");
        let home = dir.0.join("home");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();
        // Root writes in any folder that is not on a read-only mount.
        if fs::create_dir(home.join("probe")).is_ok() {
            return;
        }
        let shared = dir.0.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let temp = dir.0.join("tmp/capturefab-1");
        let found = choose(
            Kind::Runtime,
            &[
                Candidate::new(home.join(".cache/capturefab/sessions"), "home"),
                Candidate::new(shared.clone(), "XDG_RUNTIME_DIR"),
                Candidate::new(temp.clone(), "temp"),
            ],
        );
        let chosen = found.chosen.as_ref().unwrap();
        assert_eq!(
            (chosen.path.as_path(), chosen.from),
            (temp.as_path(), "temp")
        );
        assert!(chosen.writable);
        let whys = found
            .passed
            .iter()
            .map(|passed| passed.why.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            whys,
            ["not writable by this user", "group or others may enter it"]
        );
        assert_eq!(
            fs::metadata(&temp).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Nothing usable: permission_denied, naming each folder tried and
        // the variable that fixes it.
        let none = choose(
            Kind::Runtime,
            &[Candidate::new(home.join("sessions"), "home")],
        );
        let classified = crate::types::classify(&none.error());
        assert_eq!(classified.kind.code(), ("permission_denied", 3));
        assert!(
            classified.hint.unwrap().contains(SESSION_DIR_ENV),
            "{}",
            none.error()
        );
        assert!(none.error().to_string().contains("sessions: not writable"));
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_cache_of_this_user_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("shared-cache");
        let cache = dir.0.join("cache");
        fs::create_dir(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
        let found = choose(
            Kind::Cache,
            &[Candidate::explicit(cache.clone(), CACHE_DIR_ENV)],
        );
        assert_eq!(found.chosen.unwrap().path, cache);
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A runtime folder others may enter is refused with the fix.
        let runtime = dir.0.join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        let found = choose(
            Kind::Runtime,
            &[Candidate::explicit(runtime.clone(), SESSION_DIR_ENV)],
        );
        assert_eq!(found.chosen, None);
        let classified = crate::types::classify(&found.error());
        let quoted = crate::hint::shell_quote(&runtime.display().to_string());
        assert_eq!(classified.hint, Some(format!("chmod 700 {quoted}")));
    }

    #[cfg(unix)]
    #[test]
    fn a_variable_naming_a_folder_that_cannot_be_made_says_which() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("unmakeable");
        let parent = dir.0.join("read-only");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        if fs::create_dir(parent.join("probe")).is_ok() {
            return;
        }
        let state = parent.join("state");
        let found = choose(
            Kind::State,
            &[Candidate::explicit(state.clone(), STATE_DIR_ENV)],
        );
        let error = found.error();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind.code(), ("permission_denied", 3));
        assert_eq!(
            error.to_string(),
            format!(
                "cannot write in {}: not writable by this user",
                state.display()
            )
        );
        assert!(classified.hint.unwrap().contains(STATE_DIR_ENV));
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_state_folder_is_still_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("read-only-state");
        let state = dir.0.join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o500)).unwrap();
        if fs::create_dir(state.join("probe")).is_ok() {
            return;
        }
        let found = choose(Kind::State, &[Candidate::new(state.clone(), "home")]);
        let chosen = found.chosen.unwrap();
        assert_eq!(chosen.path, state);
        assert!(!chosen.writable);
        // Writing there says which variable moves it.
        let error = ensure_private_dir(&state.join("setups")).unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind.code(), ("permission_denied", 3));
        assert!(classified.hint.is_some());
        assert!(error.to_string().contains("not writable"), "{error}");
    }

    // XDG paths are absolute only on Unix.
    #[cfg(unix)]
    #[test]
    fn the_runtime_folder_is_the_same_with_or_without_xdg_runtime_dir() {
        let login = Some(PathBuf::from("/run/user/1000"));
        let shell = env(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("HOME", "/home/pi")]);
        let service = env(&[("HOME", "/home/pi")]);
        let expected = PathBuf::from("/run/user/1000/capturefab");
        let first = |var: &dyn Fn(&str) -> Option<PathBuf>, login: Option<PathBuf>| {
            runtime_defaults(&var, Some(1000), login, true)[0].clone()
        };
        assert_eq!(first(&shell, login.clone()).path, expected);
        assert_eq!(first(&shell, None).path, expected);
        assert_eq!(first(&service, login.clone()).path, expected);
        assert_eq!(first(&service, login.clone()).from, "login");
        // XDG_RUNTIME_DIR, such as a unit's RuntimeDirectory, comes first.
        let unit = env(&[("XDG_RUNTIME_DIR", "/run/capturefab"), ("HOME", "/home/pi")]);
        assert_eq!(
            first(&unit, login.clone()).path,
            PathBuf::from("/run/capturefab/capturefab")
        );
        // Without a usable login folder, sessions fall back to disk, then
        // to this user's temporary folder; a relative XDG_RUNTIME_DIR
        // counts as unset.
        let disk = PathBuf::from("/home/pi/.cache/capturefab/sessions");
        assert_eq!(
            paths(&runtime_defaults(&service, Some(1000), None, true)),
            [disk.clone(), PathBuf::from("/tmp/capturefab-1000")]
        );
        let relative = env(&[("XDG_RUNTIME_DIR", "run"), ("HOME", "/home/pi")]);
        assert_eq!(first(&relative, None).path, disk);
        assert_eq!(first(&relative, login).path, expected);
        // A Yocto service: root, no HOME or XDG variables, a read-only
        // root file system with tmpfs /run and /tmp.
        let bare = env(&[("TMPDIR", "/var/volatile/tmp")]);
        assert_eq!(
            paths(&runtime_defaults(&bare, Some(0), None, true)),
            [
                PathBuf::from("/run/capturefab-0"),
                PathBuf::from("/var/volatile/tmp/capturefab-0")
            ]
        );
    }
    #[cfg(unix)]
    #[test]
    fn linux_keeps_state_out_of_the_runtime_folder() {
        let shell = env(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("HOME", "/home/pi")]);
        let service = env(&[("HOME", "/home/pi")]);
        let state = state_defaults(&shell, true);
        assert_eq!(state, [PathBuf::from("/home/pi/.local/state/capturefab")]);
        assert_eq!(state_defaults(&service, true), state);
        let custom = env(&[("XDG_STATE_HOME", "/data/state"), ("HOME", "/home/pi")]);
        assert_eq!(
            state_defaults(&custom, true),
            [PathBuf::from("/data/state/capturefab")]
        );
        let relative = env(&[("XDG_STATE_HOME", "state"), ("HOME", "/home/pi")]);
        assert_eq!(state_defaults(&relative, true), state);
        // Elsewhere nothing moves: state stays in the session folder.
        assert_eq!(
            state_defaults(&shell, false),
            [PathBuf::from("/run/user/1000/capturefab")]
        );
        assert_eq!(
            state_defaults(&service, false),
            [PathBuf::from("/home/pi/.cache/capturefab/sessions")]
        );
        // Without HOME, nothing: commands that need state say so.
        assert!(state_defaults(&env(&[]), true).is_empty());
    }
    #[cfg(unix)]
    fn state_defaults(var: &impl Fn(&str) -> Option<PathBuf>, linux: bool) -> Vec<PathBuf> {
        state_default(var, linux)
            .map(|candidate| candidate.path)
            .into_iter()
            .collect()
    }

    #[test]
    fn the_cache_follows_each_platform() {
        let home = env(&[("HOME", "/home/pi"), ("XDG_CACHE_HOME", "/data/cache")]);
        let expected = if cfg!(target_os = "macos") {
            Some("/home/pi/Library/Caches/capturefab")
        } else if cfg!(windows) {
            None
        } else {
            Some("/data/cache/capturefab")
        };
        assert_eq!(
            cache_default(&home).map(|candidate| candidate.path),
            expected.map(PathBuf::from)
        );
        if !cfg!(any(target_os = "macos", windows)) {
            let plain = env(&[("HOME", "/home/pi"), ("XDG_CACHE_HOME", "cache")]);
            assert_eq!(
                cache_default(&plain).map(|candidate| candidate.path),
                Some(PathBuf::from("/home/pi/.cache/capturefab"))
            );
            assert_eq!(cache_default(&env(&[])), None);
        }
    }
    #[cfg(unix)]
    #[test]
    fn only_a_private_runtime_folder_of_this_user_is_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("owned");
        let uid = effective_uid();
        let login = dir.0.join("1000");
        fs::create_dir(&login).unwrap();
        fs::set_permissions(&login, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(owned_private(&login, uid));
        assert!(!owned_private(&login, uid + 1), "another user's");
        fs::set_permissions(&login, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!owned_private(&login, uid), "readable by others");
        fs::set_permissions(&login, fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&login, &link).unwrap();
        assert!(!owned_private(&link, uid), "a symbolic link");
        assert!(!owned_private(&dir.0.join("missing"), uid));
    }

    #[cfg(unix)]
    #[test]
    fn the_login_folder_is_used_only_for_a_lingering_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("login");
        let uid = effective_uid();
        let login = dir.0.join("1000");
        assert_eq!(LoginDir::of(&login, uid, false), LoginDir::NotLingering);
        assert_eq!(LoginDir::of(&login, uid, true), LoginDir::Missing);
        fs::create_dir(&login).unwrap();
        fs::set_permissions(&login, fs::Permissions::from_mode(0o700)).unwrap();
        // Without lingering, logind removes it at logout, and a service's
        // session with it, however private it is now.
        assert_eq!(LoginDir::of(&login, uid, false), LoginDir::NotLingering);
        assert_eq!(LoginDir::of(&login, uid, true), LoginDir::Usable);
        assert_eq!(LoginDir::of(&login, uid + 1, true), LoginDir::NotPrivate);
        fs::set_permissions(&login, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(LoginDir::of(&login, uid, true), LoginDir::NotPrivate);
        assert_eq!(LoginDir::of(&login, uid, false), LoginDir::NotPrivate);
    }

    #[test]
    fn a_host_without_logind_misses_no_login_folder() {
        let dir = PathBuf::from("/run/user/1000");
        for login in [
            LoginDir::NotLingering,
            LoginDir::Missing,
            LoginDir::NotPrivate,
        ] {
            assert_eq!(
                missed(Some((dir.clone(), login)), true),
                Some((dir.clone(), login))
            );
            // A container has no logins that would look there.
            assert_eq!(missed(Some((dir.clone(), login)), false), None);
        }
        assert_eq!(missed(Some((dir, LoginDir::Usable)), true), None);
        assert_eq!(missed(None, true), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linger_is_looked_up_by_user_name() {
        assert_eq!(user_name(0).as_deref(), Some(std::ffi::OsStr::new("root")));
        assert_eq!(user_name(u32::MAX - 1), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_of_another_user_is_refused() {
        // The root folder belongs to root; its mode alone would not refuse
        // a private one that another user created first.
        if effective_uid() != 0 {
            let error = ensure_private_dir(Path::new("/")).unwrap_err().to_string();
            assert!(error.contains("belongs to another user"), "{error}");
        }
        let dir = TestDir::new("foreign");
        ensure_private_dir(&dir.0.join("a/b")).unwrap();
        // A folder others may enter is refused with the command that fixes it.
        use std::os::unix::fs::PermissionsExt;
        let shared = dir.0.join("shared folder");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let error = check_private_dir(&shared).unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind.code(), ("permission_denied", 3));
        assert_eq!(
            classified.hint.as_deref(),
            Some(format!("chmod 700 '{}'", shared.display()).as_str())
        );
        let file = dir.0.join("file");
        fs::write(&file, b"").unwrap();
        let classified = crate::types::classify(&check_private_dir(&file).unwrap_err());
        assert_eq!(classified.kind.code(), ("permission_denied", 3));
        assert!(classified.hint.unwrap().contains(SESSION_DIR_ENV));
    }

    #[cfg(unix)]
    #[test]
    fn mounts_say_whether_they_are_read_only() {
        let dir = TestDir::new("mount");
        assert_eq!(read_only_mount(&dir.0), Some(false));
        assert_eq!(read_only_mount(&dir.0.join("missing")), None);
        // Linux mounts /proc noexec; elsewhere nothing is known to be.
        if cfg!(target_os = "linux") {
            assert!(noexec_mount(Path::new("/proc")));
        } else {
            assert!(!noexec_mount(&dir.0));
        }
    }
}
