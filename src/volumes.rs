//! A guard against writing to an external volume that is not mounted: an
//! unplugged drive's mount point is otherwise just an empty directory on the
//! system disk.
#![forbid(unsafe_code)]
#![deny(clippy::missing_const_for_fn)]
#![deny(clippy::cast_precision_loss)]
use anyhow::Result;
use std::path::{Component, Path, PathBuf};

/// Decode the octal escapes fstab uses for spaces and tabs.
#[cfg(any(target_os = "linux", test))]
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = u16::from(bytes[i + 1] - b'0') * 64
                + u16::from(bytes[i + 2] - b'0') * 8
                + u16::from(bytes[i + 3] - b'0');
            if let Ok(value) = u8::try_from(value) {
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A directory is a mount point when it lives on a different device than its
/// parent directory.
#[cfg(unix)]
fn mounted(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    match (std::fs::metadata(path), std::fs::metadata(parent)) {
        (Ok(own), Ok(up)) => own.is_dir() && own.dev() != up.dev(),
        _ => false,
    }
}

/// Mount points /etc/fstab declares, other than the root file system, swap and
/// bind mounts: drives that are expected to be there, such as an SSD mounted
/// with `nofail`, whose mount point is an empty directory on the system disk
/// while the drive is missing.
#[cfg(any(target_os = "linux", test))]
fn from_fstab(table: &str) -> Vec<PathBuf> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?;
            let mount = fields.next()?;
            let fs = fields.next()?;
            let bind = fields
                .next()
                .is_some_and(|options| options.split(',').any(|o| o == "bind" || o == "rbind"));
            if device.starts_with('#') || !mount.starts_with('/') || fs == "swap" || bind {
                return None;
            }
            let path = PathBuf::from(unescape(mount));
            (path != Path::new("/")).then_some(path)
        })
        .collect()
}

/// The declared mount points, read again only when /etc/fstab changes.
#[cfg(target_os = "linux")]
fn declared() -> Vec<PathBuf> {
    use std::sync::{Mutex, PoisonError};
    static CACHE: Mutex<Option<(std::time::SystemTime, Vec<PathBuf>)>> = Mutex::new(None);
    let Ok(modified) = std::fs::metadata("/etc/fstab").and_then(|meta| meta.modified()) else {
        return Vec::new();
    };
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    match &*cache {
        Some((at, points)) if *at == modified => points.clone(),
        _ => {
            let points = from_fstab(&std::fs::read_to_string("/etc/fstab").unwrap_or_default());
            *cache = Some((modified, points.clone()));
            points
        }
    }
}

#[cfg(not(target_os = "linux"))]
const fn declared() -> Vec<PathBuf> {
    Vec::new()
}

/// The mount point an external volume path must be under: /Volumes/NAME on
/// macOS, /media/USER/NAME or /run/media/USER/NAME on Linux, the drive root on
/// Windows. `None` for paths elsewhere, which need no mount.
fn root_in(path: &Path, declared: &[PathBuf]) -> Option<PathBuf> {
    if cfg!(windows) {
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            return Some(PathBuf::from(format!(
                "{}\\",
                prefix.as_os_str().to_string_lossy()
            )));
        }
        return None;
    }
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if !path.is_absolute() {
        return None;
    }
    let depth = match parts.first().map(String::as_str) {
        Some("Volumes") if cfg!(target_os = "macos") => 2,
        // usbmount and hand-written fstab entries mount drives at
        // /media/NAME; desktops use /media/USER/NAME. A /media that is a
        // volume of its own, as a container's or one fstab mounts there,
        // holds plain folders, under the mount point it is.
        Some("media") if cfg!(target_os = "linux") => match parts.get(1) {
            _ if own_mount(Path::new("/media"), declared) => return None,
            Some(name) if own_mount(&Path::new("/media").join(name), declared) => 2,
            _ => 3,
        },
        Some("run")
            if cfg!(target_os = "linux") && parts.get(1).map(String::as_str) == Some("media") =>
        {
            4
        }
        _ => return None,
    };
    (parts.len() >= depth).then(|| {
        let mut root = PathBuf::from("/");
        root.extend(&parts[..depth]);
        root
    })
}

fn own_mount(path: &Path, declared: &[PathBuf]) -> bool {
    #[cfg(unix)]
    let mounted = mounted(path);
    #[cfg(not(unix))]
    let mounted = false;
    mounted || declared.iter().any(|point| point == path)
}

/// `path` with its nearest existing folder replaced by its real path, so a
/// symlink where a volume would be mounted, such as macOS's
/// /Volumes/Macintosh HD to /, is judged by the folder it leads to.
#[cfg(unix)]
fn real(path: &Path) -> PathBuf {
    for existing in path.ancestors() {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return match path.strip_prefix(existing) {
                Ok(rest) if !rest.as_os_str().is_empty() => real.join(rest),
                _ => real,
            };
        }
    }
    path.to_path_buf()
}
#[cfg(not(unix))]
fn real(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Refuse a path on an external volume, or under a mount point fstab
/// declares, that is not mounted, instead of silently creating its
/// directories on the system disk.
pub(crate) fn ensure_present(path: &Path) -> Result<()> {
    present_in(path, &declared())
}

fn present_in(path: &Path, declared: &[PathBuf]) -> Result<()> {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let real = real(&path);
    let under = || {
        declared
            .iter()
            .filter(|point| path.starts_with(point) || real.starts_with(point))
            .max_by_key(|point| point.components().count())
            .cloned()
    };
    let Some(root) = root_in(&real, declared).or_else(under) else {
        return Ok(());
    };
    #[cfg(unix)]
    let present = mounted(&root);
    #[cfg(not(unix))]
    let present = root.is_dir();
    if !present {
        return Err(crate::types::Categorized::new(
            crate::types::ErrorKind::Unavailable,
            format!(
                "external volume {} is not mounted; reconnect it or choose another destination",
                root.display()
            ),
        )
        .details(serde_json::json!({"path": root}))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_paths_need_their_volume_mounted() {
        if cfg!(target_os = "macos") {
            let root = root_in(Path::new("/Volumes/CAPTURES/run 1/shot.png"), &[]);
            assert_eq!(root, Some(PathBuf::from("/Volumes/CAPTURES")));
            let missing = Path::new("/Volumes/capturefab-missing-volume-4f1c/shot.png");
            assert!(
                ensure_present(missing)
                    .unwrap_err()
                    .to_string()
                    .contains("is not mounted")
            );
        }
        if cfg!(target_os = "linux") {
            assert_eq!(
                root_in(Path::new("/run/media/ana/SD/a.png"), &[]),
                Some(PathBuf::from("/run/media/ana/SD"))
            );
        }
        // Where /media is a folder of the system disk, as on desktops.
        #[cfg(target_os = "linux")]
        if !mounted(Path::new("/media"))
            && !declared().iter().any(|point| point == Path::new("/media"))
        {
            assert_eq!(
                root_in(Path::new("/media/ana/USB/x/y.png"), &[]),
                Some(PathBuf::from("/media/ana/USB"))
            );
            assert!(ensure_present(Path::new("/media/capturefab-missing/USB/a.png")).is_err());
        }
        assert_eq!(root_in(Path::new("captures/a.png"), &[]), None);
        if cfg!(unix) {
            assert_eq!(root_in(Path::new("/tmp/a.png"), &[]), None);
            ensure_present(Path::new("/tmp/capturefab/a.png")).unwrap();
        }
    }

    #[test]
    fn fstab_declares_drives_but_not_root_swap_or_bind_mounts() {
        let table = "# /etc/fstab\n\
                     PARTUUID=1-02 / ext4 defaults,noatime 0 1\n\
                     PARTUUID=1-01 /boot/firmware vfat defaults 0 2\n\
                     UUID=ab /mnt/ssd ext4 defaults,nofail 0 2\n\
                     /dev/sdb1 /media/usb0 vfat noauto\n\
                     /swapfile none swap sw 0 0\n\
                     /srv/data /mnt/data none bind 0 0\n\
                     nas:/export /mnt/nas\\040share nfs4 defaults 0 0\n";
        assert_eq!(
            from_fstab(table),
            [
                PathBuf::from("/boot/firmware"),
                PathBuf::from("/mnt/ssd"),
                PathBuf::from("/media/usb0"),
                PathBuf::from("/mnt/nas share"),
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    fn paths_under_a_declared_mount_need_it_mounted() {
        // A plain directory standing in for a nofail drive that is missing.
        let point =
            std::env::temp_dir().join(format!("capturefab-unmounted-{}", std::process::id()));
        std::fs::create_dir_all(&point).unwrap();
        let declared = [point.clone()];
        let error = present_in(&point.join("captures/a.png"), &declared).unwrap_err();
        assert!(error.to_string().contains("is not mounted"), "{error}");
        assert_eq!(crate::cli::output::error_code(&error), ("unavailable", 3));
        present_in(Path::new("/tmp/capturefab/a.png"), &declared).unwrap();
        std::fs::remove_dir_all(&point).unwrap();
        #[cfg(target_os = "linux")]
        if !mounted(Path::new("/media")) {
            let usb = [PathBuf::from("/media/usb0")];
            assert_eq!(
                root_in(Path::new("/media/usb0/2026/a.png"), &usb),
                Some(PathBuf::from("/media/usb0"))
            );
            assert_eq!(
                root_in(Path::new("/media/ana/USB/a.png"), &usb),
                Some(PathBuf::from("/media/ana/USB"))
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_where_volumes_mount_are_judged_by_where_they_lead() {
        let root = std::env::temp_dir().join(format!("capturefab-volume-{}", std::process::id()));
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let real_target = std::fs::canonicalize(&target).unwrap();
        assert_eq!(real(&link.join("new/a.png")), real_target.join("new/a.png"));
        assert_eq!(real(&link), real_target);
        // A link to a folder that is gone leads nowhere: it stays a name.
        std::os::unix::fs::symlink(root.join("gone"), root.join("dangling")).unwrap();
        let real_root = std::fs::canonicalize(&root).unwrap();
        assert_eq!(real(&root.join("dangling/a")), real_root.join("dangling/a"));
        std::fs::remove_dir_all(&root).unwrap();
        // The boot volume's own entry, a link to /, is the system disk.
        if cfg!(target_os = "macos")
            && let Some(boot) = std::fs::read_dir("/Volumes")
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .find(|entry| std::fs::read_link(entry).is_ok_and(|to| to == Path::new("/")))
        {
            let folder = std::env::temp_dir();
            let through = boot.join(folder.strip_prefix("/").unwrap()).join("a.png");
            present_in(&through, &[]).unwrap();
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_media_volume_of_its_own_holds_plain_folders() {
        let media = [PathBuf::from("/media")];
        assert_eq!(
            root_in(Path::new("/media/line1/2026-11-02/a.png"), &media),
            None
        );
        assert_eq!(root_in(Path::new("/media/ana/USB/a.png"), &media), None);
        // The mount point fstab declares is checked instead.
        match present_in(Path::new("/media/line1/2026-11-02/a.png"), &media) {
            Ok(()) => assert!(mounted(Path::new("/media"))),
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("external volume /media is not mounted"),
                "{error}"
            ),
        }
    }

    #[test]
    fn octal_escapes_decode() {
        assert_eq!(unescape("a\\040b\\011c"), "a b\tc");
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("a\\中b"), "a\\中b");
        assert_eq!(unescape("a\\777b"), "a\\777b");
    }
}
