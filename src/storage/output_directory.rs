//! Directory anchors for checked exports. Every final file is created through
//! the directory that was opened before staging, never through a later alias.
use super::identity;
#[cfg(windows)]
use super::private_options;
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

struct Directory {
    file: File,
    path: PathBuf,
    // Windows denies renames/reparse changes through these handles. Keeping
    // the same ancestry on Unix also lets a moved output fail conservatively.
    parent: Option<Arc<Directory>>,
}
impl Directory {
    fn verify(&self) -> Result<()> {
        if let Some(parent) = &self.parent {
            parent.verify()?;
        }
        let named = fs::symlink_metadata(&self.path)
            .with_context(|| format!("export folder changed: {}", self.path.display()))?;
        ensure!(
            named.is_dir()
                && !named.file_type().is_symlink()
                && identity(&named) == identity(&self.file.metadata()?),
            "export folder changed; review its location before continuing: {}",
            self.path.display()
        );
        Ok(())
    }
    fn child(self: &Arc<Self>, name: &OsStr) -> io::Result<Arc<Self>> {
        plain_name(name)?;
        let path = self.path.join(name);
        #[cfg(unix)]
        let file = {
            use std::os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            };
            let name = std::ffi::CString::new(name.as_bytes())?;
            // SAFETY: a live parent fd and a NUL-terminated single component;
            // O_NOFOLLOW and O_DIRECTORY refuse symlinks and non-directories.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openat returned a new owned file descriptor.
            unsafe { File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = open_windows_directory(&path)?;
        #[cfg(not(any(unix, windows)))]
        return Err(io::Error::other(
            "anchored exports unsupported on this platform",
        ));
        #[cfg(any(unix, windows))]
        Ok(Arc::new(Self {
            file,
            path,
            parent: Some(self.clone()),
        }))
    }
    fn mkdir(&self, name: &OsStr) -> io::Result<()> {
        plain_name(name)?;
        #[cfg(unix)]
        {
            use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
            let name = std::ffi::CString::new(name.as_bytes())?;
            // SAFETY: a live parent fd and one NUL-terminated path component.
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            fs::create_dir(self.path.join(name))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::other(
                "anchored exports unsupported on this platform",
            ))
        }
    }
    fn create_file(&self, name: &OsStr) -> io::Result<File> {
        plain_name(name)?;
        #[cfg(unix)]
        {
            use std::os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            };
            let name = std::ffi::CString::new(name.as_bytes())?;
            // SAFETY: a live anchored directory fd and one filename; exclusive
            // creation refuses existing files, including dangling symlinks.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openat returned a new owned file descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            };
            private_options()
                .create_new(true)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(self.path.join(name))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::other(
                "anchored exports unsupported on this platform",
            ))
        }
    }
}

fn plain_name(name: &OsStr) -> io::Result<()> {
    if !matches!(
        Path::new(name).components().next(),
        Some(Component::Normal(_))
    ) || Path::new(name).components().count() != 1
        || name
            .as_encoded_bytes()
            .iter()
            .any(|byte| byte.is_ascii_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export requires one ordinary path component",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_windows_directory(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ,
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export path cannot traverse a reparse point",
        ));
    }
    Ok(file)
}

fn resolved(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(_) => path.canonicalize(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::other("export path needs a file name"))?;
            plain_name(name)?;
            Ok(resolved(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

pub(crate) struct OutputDirectoryPlan {
    anchor: Arc<Directory>,
    remaining: Vec<OsString>,
    path: PathBuf,
}
impl OutputDirectoryPlan {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    /// Resolve aliases once before staging, then open every existing root
    /// component without following links. Missing parents are created later.
    pub fn prepare(output: &Path, approved_root: Option<&Path>) -> Result<Self> {
        ensure!(output.is_absolute(), "export folder must be absolute");
        let path = resolved(output)?;
        let root = approved_root
            .map(Path::to_owned)
            .unwrap_or_else(|| path.parent().unwrap_or(Path::new("/")).to_owned());
        ensure!(root.is_absolute(), "approved export root must be absolute");
        let root = resolved(&root).context("resolve approved export root")?;
        let relative = path
            .strip_prefix(&root)
            .context("export folder leaves its approved capture root")?;
        let tail: Vec<_> = relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name.to_owned()),
                _ => bail!("export folder must stay inside its approved capture root"),
            })
            .collect::<Result<_>>()?;
        ensure!(
            !tail.is_empty(),
            "export requires a new folder inside its capture root"
        );
        let mut base = PathBuf::new();
        let mut names = Vec::new();
        for component in root.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => base.push(component.as_os_str()),
                Component::Normal(name) => names.push(name.to_owned()),
                _ => bail!("approved export root must be canonical"),
            }
        }
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&base)?
        };
        #[cfg(windows)]
        let file = open_windows_directory(&base)?;
        #[cfg(not(any(unix, windows)))]
        bail!("anchored exports unsupported on this platform");
        #[cfg(any(unix, windows))]
        {
            let mut anchor = Arc::new(Directory {
                file,
                path: base,
                parent: None,
            });
            let mut missing = names.len();
            for (index, name) in names.iter().enumerate() {
                match anchor.child(name) {
                    Ok(child) => anchor = child,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        missing = index;
                        break;
                    }
                    Err(error) => {
                        return Err(error).context(
                            "cannot anchor the approved export root without symbolic links",
                        );
                    }
                }
            }
            let mut remaining = names[missing..].to_vec();
            remaining.extend(tail);
            Ok(Self {
                anchor,
                remaining,
                path,
            })
        }
    }
    pub fn publish(self) -> Result<Arc<OutputDirectory>> {
        let mut directory = self.anchor;
        let count = self.remaining.len();
        for (index, name) in self.remaining.into_iter().enumerate() {
            directory.verify()?;
            match directory.mkdir(&name) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && index + 1 < count => {
                }
                Err(error) => {
                    return Err(error).context("cannot create a new anchored export folder");
                }
            }
            directory = directory
                .child(&name)
                .context("export parent changed; symbolic links are refused")?;
        }
        directory.verify()?;
        Ok(Arc::new(OutputDirectory {
            directory,
            path: self.path,
        }))
    }
}

pub(crate) struct OutputDirectory {
    directory: Arc<Directory>,
    path: PathBuf,
}

/// Proof tied to the held original directory, never a caller-supplied path.
/// Task engines mint it only after native close/removal and extent cleanup.
pub(crate) struct NamespaceRemovalReceipt {
    directory: Arc<Directory>,
    path: PathBuf,
}
impl NamespaceRemovalReceipt {
    pub(super) fn matches(&self, directory: &OutputDirectory) -> bool {
        Arc::ptr_eq(&self.directory, &directory.directory) && self.path == directory.path
    }
}
impl OutputDirectory {
    pub(super) fn open_existing(path: &Path) -> Result<Arc<Self>> {
        let plan = OutputDirectoryPlan::prepare(&path.join(".anchor-probe"), Some(path))?;
        ensure!(
            plan.remaining.len() == 1 && plan.anchor.path == path,
            "retained namespace ancestry changed"
        );
        plan.anchor.verify()?;
        Ok(Arc::new(Self {
            directory: plan.anchor,
            path: path.to_owned(),
        }))
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn verify(&self) -> Result<()> {
        self.directory.verify()
    }
    pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
        self.directory.create_file(name)
    }
    /// Open a staged file through the original directory without following a
    /// replacement symlink. The caller verifies its saved file identity.
    pub(crate) fn open_file(&self, name: &OsStr) -> Result<File> {
        plain_name(name)?;
        self.verify()?;
        #[cfg(unix)]
        {
            use std::os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            };
            let name = std::ffi::CString::new(name.as_bytes())?;
            // SAFETY: held directory fd and a single NUL-terminated filename.
            let fd = unsafe {
                libc::openat(
                    self.directory.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error()).context("open anchored staging file");
            }
            // SAFETY: openat returned a new owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
        #[cfg(not(unix))]
        bail!("anchored staging reads are unsupported on this platform")
    }
    /// Remove only a regular, singly-linked staged file with the saved identity.
    pub(crate) fn remove_file(
        &self,
        name: &OsStr,
        expected: (u64, u64),
        original: &File,
    ) -> Result<()> {
        plain_name(name)?;
        self.verify()?;
        #[cfg(unix)]
        {
            use std::os::{
                fd::AsRawFd,
                unix::{ffi::OsStrExt, fs::MetadataExt},
            };
            let held = original.metadata()?;
            ensure!(
                held.is_file() && held.nlink() == 1 && (held.dev(), held.ino()) == expected,
                "original staging file changed or gained links; namespace reservation retained"
            );
            let name = std::ffi::CString::new(name.as_bytes())?;
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: held fd, single filename and valid writable stat output.
            if unsafe {
                libc::fstatat(
                    self.directory.file.as_raw_fd(),
                    name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
            {
                return Err(io::Error::last_os_error())
                    .context("inspect anchored staging file before removal");
            }
            // SAFETY: successful fstatat initialized the complete structure.
            let metadata = unsafe { metadata.assume_init() };
            // Platform dev_t/ino_t widths differ between macOS and Linux.
            #[allow(clippy::unnecessary_cast)]
            let named_identity = (metadata.st_dev as u64, metadata.st_ino as u64);
            ensure!(
                named_identity == expected
                    && metadata.st_mode & libc::S_IFMT == libc::S_IFREG
                    && metadata.st_nlink == 1,
                "staging file changed or gained links; namespace reservation retained"
            );
            // SAFETY: held fd and one previously checked regular filename.
            if unsafe { libc::unlinkat(self.directory.file.as_raw_fd(), name.as_ptr(), 0) } < 0 {
                return Err(io::Error::last_os_error()).context("remove anchored staging file");
            }
            ensure!(
                original.metadata()?.nlink() == 0,
                "original staging file remains linked; namespace reservation retained"
            );
            Ok(())
        }
        #[cfg(not(unix))]
        bail!("anchored staging removal is unsupported on this platform")
    }
    pub(super) fn metadata(&self) -> io::Result<fs::Metadata> {
        self.directory.file.metadata()
    }
    /// The original held directory identity, never a later pathname lookup.
    /// Native task admission must compare this before opening any engine file.
    pub(crate) fn native_identity(&self) -> Result<(u64, u64)> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = self.directory.file.metadata()?;
            ensure!(
                metadata.is_dir(),
                "original namespace anchor is not a directory"
            );
            Ok((metadata.dev(), metadata.ino()))
        }
        #[cfg(not(unix))]
        bail!("native task directory identity is unsupported on this platform")
    }
    pub(super) fn is_empty(&self) -> Result<bool> {
        self.verify()?;
        let empty = fs::read_dir(self.path())?.next().is_none();
        self.verify()?;
        Ok(empty)
    }
    pub(crate) fn remove_empty(&self) -> Result<()> {
        if self.removed_receipt().is_ok() {
            return Ok(());
        }
        self.verify()?;
        ensure!(
            self.is_empty()?,
            "unused task namespace contains files; storage reservation retained"
        );
        #[cfg(unix)]
        {
            use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
            let parent = self
                .directory
                .parent
                .as_ref()
                .context("namespace has no parent anchor")?;
            let name = std::ffi::CString::new(
                self.path
                    .file_name()
                    .context("namespace has no name")?
                    .as_bytes(),
            )?;
            // SAFETY: verified parent fd and one ordinary name; rmdir refuses
            // nonempty directories and never recursively deletes contents.
            if unsafe { libc::unlinkat(parent.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                < 0
            {
                return Err(io::Error::last_os_error())
                    .context("remove unused anchored task namespace");
            }
            Ok(())
        }
        #[cfg(not(unix))]
        bail!("unused task namespace removal is unsupported on this platform")
    }
    /// Requires the original anchored directory to be unlinked, with its
    /// original parent still verified and no replacement at the old name.
    /// This does not certify engine shutdown: the task adapter must first
    /// prove native handles/uncertain extents have gone, then mint this proof.
    pub(crate) fn removed_receipt(&self) -> Result<NamespaceRemovalReceipt> {
        let parent = self
            .directory
            .parent
            .as_ref()
            .context("namespace has no parent anchor")?;
        parent.verify()?;
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => bail!("task namespace name remains or was replaced; storage reservation retained"),
        }
        #[cfg(unix)]
        {
            self.require_unlinked_original()?;
            // The proof concerns both the held inode and its admitted name.
            // Refuse a parent replacement or new occupant during the check.
            parent.verify()?;
            match fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => bail!(
                    "task namespace name remains or was replaced; storage reservation retained"
                ),
            }
            Ok(NamespaceRemovalReceipt {
                directory: self.directory.clone(),
                path: self.path.clone(),
            })
        }
        #[cfg(not(unix))]
        bail!("confirmed task namespace removal is unsupported on this platform")
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn require_unlinked_original(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            self.directory.file.metadata()?.nlink() == 0,
            "original task namespace still exists; storage reservation retained"
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn require_unlinked_original(&self) -> Result<()> {
        use std::os::fd::AsRawFd;
        let before = self.directory.file.metadata()?;
        ensure!(
            before.is_dir(),
            "original namespace anchor is not a directory"
        );
        let probe =
            std::ffi::CString::new(format!(".capturefab-unlink-proof-{}", super::nonce()?))?;
        // APFS retains a positive st_nlink and the last F_GETPATH even after
        // rmdir. A removed directory cannot admit a new child through its held
        // fd. A live or renamed original can, so remove only this fresh empty
        // marker and refuse proof. This transient metadata is outside the
        // logical file-EOF cap; it makes no physical disk quota guarantee.
        // SAFETY: the original live directory fd and one fresh NUL-terminated
        // component; mkdirat never replaces an existing entry.
        let created =
            unsafe { libc::mkdirat(self.directory.file.as_raw_fd(), probe.as_ptr(), 0o700) };
        if created == 0 {
            // SAFETY: this exact name was exclusively created above; rmdir
            // refuses nonempty replacements and follows no child links.
            if unsafe {
                libc::unlinkat(
                    self.directory.file.as_raw_fd(),
                    probe.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } < 0
            {
                return Err(io::Error::last_os_error()).context(
                    "cannot remove live-directory proof marker; storage reservation retained",
                );
            }
            bail!("original task namespace still exists; storage reservation retained");
        }
        let error = io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(libc::ENOENT),
            "cannot prove original task namespace removal; storage reservation retained: {error}"
        );
        ensure!(
            identity(&self.directory.file.metadata()?) == identity(&before),
            "original task namespace identity changed; storage reservation retained"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "capturefab-anchor-{name}-{}",
            crate::storage::nonce().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn creates_missing_parents_and_refuses_existing_export_folder() {
        let root = temp_root("new");
        let output = root.join("new-parent/bundle");
        let plan = OutputDirectoryPlan::prepare(&output, Some(&root)).unwrap();
        assert!(!output.exists());
        let directory = plan.publish().unwrap();
        directory
            .create_new(OsStr::new("value.csv"))
            .unwrap()
            .write_all(b"value\n1\n")
            .unwrap();
        assert_eq!(fs::read(output.join("value.csv")).unwrap(), b"value\n1\n");
        assert_eq!(
            directory
                .create_new(OsStr::new("value.csv"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(
            OutputDirectoryPlan::prepare(&output, Some(&root))
                .unwrap()
                .publish()
                .is_err()
        );
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn approved_root_alias_resolves_once_and_cannot_redirect_publication() {
        use std::os::unix::fs::symlink;
        let root = temp_root("root-alias");
        let outside = temp_root("root-alias-outside");
        let alias = root.join("alias");
        fs::create_dir(root.join("actual")).unwrap();
        symlink(root.join("actual"), &alias).unwrap();
        let plan = OutputDirectoryPlan::prepare(&alias.join("bundle"), Some(&alias)).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&outside, &alias).unwrap();
        let directory = plan.publish().unwrap();
        directory
            .create_new(OsStr::new("sample.csv"))
            .unwrap()
            .write_all(b"value\n1\n")
            .unwrap();
        assert_eq!(
            fs::read(root.join("actual/bundle/sample.csv")).unwrap(),
            b"value\n1\n"
        );
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        drop(directory);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_swapped_during_staging_cannot_redirect_publication() {
        use std::os::unix::fs::symlink;
        let root = temp_root("parent-swap");
        let outside = temp_root("parent-outside");
        fs::create_dir(root.join("parent")).unwrap();
        let output = root.join("parent/bundle");
        let plan = OutputDirectoryPlan::prepare(&output, Some(&root)).unwrap();
        fs::rename(root.join("parent"), root.join("old-parent")).unwrap();
        symlink(&outside, root.join("parent")).unwrap();
        assert!(plan.publish().is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_final_folder_swapped_after_validation_cannot_redirect_file_creation() {
        use std::os::unix::fs::symlink;
        let root = temp_root("final-swap");
        let outside = temp_root("final-outside");
        let output = root.join("bundle");
        let directory = OutputDirectoryPlan::prepare(&output, Some(&root))
            .unwrap()
            .publish()
            .unwrap();
        directory.verify().unwrap();
        fs::rename(&output, root.join("original-bundle")).unwrap();
        symlink(&outside, &output).unwrap();
        // Reproduce a swap immediately after the ledger's path validation.
        // Kernel-relative creation stays attached to the original directory.
        directory
            .create_new(OsStr::new("data.csv"))
            .unwrap()
            .write_all(b"original")
            .unwrap();
        assert_eq!(
            fs::read(root.join("original-bundle/data.csv")).unwrap(),
            b"original"
        );
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        assert!(
            directory.verify().is_err(),
            "a moved folder cannot be reported as a published output"
        );
        drop(directory);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn refuses_output_outside_the_pinned_root() {
        let root = temp_root("scope");
        let outside = temp_root("scope-outside");
        assert!(OutputDirectoryPlan::prepare(&outside.join("bundle"), Some(&root)).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn removal_proof_distinguishes_the_held_original_from_moved_and_replaced_names() {
        use std::os::unix::fs::MetadataExt;
        let root = temp_root("removal-proof");
        let output = root.join("task");
        let anchor = OutputDirectoryPlan::prepare(&output, Some(&root))
            .unwrap()
            .publish()
            .unwrap();
        let metadata = fs::metadata(&output).unwrap();
        let original = (metadata.dev(), metadata.ino());
        assert_eq!(anchor.native_identity().unwrap(), original);
        assert!(
            anchor.removed_receipt().is_err(),
            "live original is retained"
        );
        assert!(fs::read_dir(&output).unwrap().next().is_none());

        let moved = root.join("moved");
        fs::rename(&output, &moved).unwrap();
        fs::write(moved.join("keep"), b"not a proof marker").unwrap();
        assert_eq!(anchor.native_identity().unwrap(), original);
        assert!(anchor.removed_receipt().is_err(), "rename is not removal");
        assert_eq!(fs::read(moved.join("keep")).unwrap(), b"not a proof marker");
        let entries = fs::read_dir(&moved)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [OsString::from("keep")], "no probe is retained");

        fs::create_dir(&output).unwrap();
        fs::write(output.join("replacement"), b"keep replacement").unwrap();
        fs::remove_file(moved.join("keep")).unwrap();
        fs::remove_dir(&moved).unwrap();
        assert!(
            anchor.removed_receipt().is_err(),
            "old name has a replacement"
        );
        assert_eq!(
            fs::read(output.join("replacement")).unwrap(),
            b"keep replacement"
        );
        fs::remove_file(output.join("replacement")).unwrap();
        fs::remove_dir(&output).unwrap();
        assert!(
            anchor.removed_receipt().is_ok(),
            "held original is unlinked"
        );
        assert!(anchor.removed_receipt().is_ok(), "proof can be retried");
        assert_eq!(anchor.native_identity().unwrap(), original);
        assert!(fs::read_dir(&root).unwrap().next().is_none());
        drop(anchor);
        fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn staging_unlink_checks_the_named_and_held_original_then_proves_it_is_unlinked() {
        use std::os::unix::fs::MetadataExt;
        let root = temp_root("file-removal-proof");
        let output = root.join("stage");
        let anchor = OutputDirectoryPlan::prepare(&output, Some(&root))
            .unwrap()
            .publish()
            .unwrap();
        let mut original = anchor.create_new(OsStr::new("sample")).unwrap();
        original.write_all(b"original").unwrap();
        let metadata = original.metadata().unwrap();
        let expected = (metadata.dev(), metadata.ino());
        fs::rename(output.join("sample"), output.join("moved")).unwrap();
        fs::write(output.join("sample"), b"replacement").unwrap();
        assert!(
            anchor
                .remove_file(OsStr::new("sample"), expected, &original)
                .is_err()
        );
        assert_eq!(fs::read(output.join("sample")).unwrap(), b"replacement");
        assert_eq!(fs::read(output.join("moved")).unwrap(), b"original");
        fs::remove_file(output.join("sample")).unwrap();
        fs::rename(output.join("moved"), output.join("sample")).unwrap();
        anchor
            .remove_file(OsStr::new("sample"), expected, &original)
            .unwrap();
        assert_eq!(original.metadata().unwrap().nlink(), 0);
        assert!(fs::read_dir(&output).unwrap().next().is_none());
        drop(original);
        anchor.remove_empty().unwrap();
        assert!(anchor.removed_receipt().is_ok());
        drop(anchor);
        fs::remove_dir(root).unwrap();
    }
}
