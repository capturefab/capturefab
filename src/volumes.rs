//! External and network volumes a capture destination can live on, and a guard
//! against writing to one that is not mounted: an unplugged drive's mount point
//! is otherwise just an empty directory on the system disk.
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Volume {
    /// Display name: the volume label or mount directory name.
    pub name: String,
    pub path: PathBuf,
    /// `removable`, `network` or `fixed` (a second internal disk).
    pub kind: &'static str,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

fn space(path: &Path) -> (u64, u64) {
    (
        fs2::total_space(path).unwrap_or(0),
        fs2::available_space(path).unwrap_or(0),
    )
}

const NETWORK: [&str; 10] = [
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "afpfs",
    "webdav",
    "fuse.sshfs",
    "fuse.rclone",
    "9p",
];

/// Mounted, writable external, removable and network volumes, by name.
/// macOS and Linux leave out read-only mounts; Windows lists every drive.
pub fn list() -> Vec<Volume> {
    let mut volumes = platform();
    volumes.sort_by_key(|v| v.name.to_lowercase());
    volumes
}

#[cfg(target_os = "macos")]
fn platform() -> Vec<Volume> {
    // `mount` names each file system type; /Volumes holds every mounted volume
    // plus a link to the system volume.
    let types = std::process::Command::new("/sbin/mount")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let kind_of = |line: Option<&str>| -> &'static str {
        line.and_then(|options| options.split([',', ')']).next())
            .map_or("removable", |fs| {
                if NETWORK.contains(&fs.trim()) {
                    "network"
                } else {
                    "removable"
                }
            })
    };
    let Ok(entries) = std::fs::read_dir("/Volumes") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            // The system volume appears as a link to /.
            if std::fs::canonicalize(&path).ok()? == Path::new("/") || !mounted(&path) {
                return None;
            }
            // Mounted disk images, Capturefab's own among them, and
            // write-locked cards are read-only: no destination can live there.
            let line = mount_line(&types, &path);
            if line.is_some_and(read_only) {
                return None;
            }
            let (total_bytes, free_bytes) = space(&path);
            Some(Volume {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: kind_of(line),
                path,
                total_bytes,
                free_bytes,
            })
        })
        .collect()
}

/// The options of `path`'s line in macOS `mount` output, after the opening
/// parenthesis: `hfs, local, nodev, nosuid, read-only, ...)`.
#[cfg(any(target_os = "macos", test))]
fn mount_line<'a>(types: &'a str, path: &Path) -> Option<&'a str> {
    let needle = format!(" on {} (", path.display());
    types
        .lines()
        .find(|line| line.contains(&needle))
        .and_then(|line| line.split(&needle).nth(1))
}

#[cfg(any(target_os = "macos", test))]
fn read_only(options: &str) -> bool {
    options.split(')').next().is_some_and(|options| {
        options
            .split(',')
            .any(|option| option.trim() == "read-only")
    })
}

/// Where a macOS app runs from when its path stops working later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallLocation {
    /// A mounted, read-only disk image: gone once the image is ejected.
    DiskImage,
    /// A temporary copy macOS made of a quarantined app (App Translocation).
    Translocated,
}

/// Whether this executable runs from a mounted Capturefab disk image or a
/// translocated copy, whose path stops working once the image is ejected or
/// the app quits. `None` on other systems and for a real install.
pub fn install_location(exe: &Path) -> Option<InstallLocation> {
    #[cfg(target_os = "macos")]
    {
        let path = exe.to_string_lossy();
        // `mount` runs only for an app under /Volumes, to read its volume's options.
        let mounts = if path.starts_with("/Volumes/") {
            std::process::Command::new("/sbin/mount")
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        classify_install(&path, &mounts)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = exe;
        None
    }
}

/// `path`'s install location, given macOS `mount` output. An app under
/// /Volumes counts as a disk image only on a read-only volume, so an app kept
/// on a writable external drive does not; without a mount line for it, the
/// volume name decides (the disk image's is "Capturefab VERSION").
#[cfg(any(target_os = "macos", test))]
fn classify_install(path: &str, mounts: &str) -> Option<InstallLocation> {
    if path.contains("/AppTranslocation/") {
        return Some(InstallLocation::Translocated);
    }
    let rest = path.strip_prefix("/Volumes/")?;
    if !path.contains("/Capturefab.app/Contents/MacOS/") {
        return None;
    }
    let name = rest.split('/').next()?;
    let image = match mount_line(mounts, &Path::new("/Volumes").join(name)) {
        Some(line) => read_only(line),
        None => name.starts_with("Capturefab"),
    };
    image.then_some(InstallLocation::DiskImage)
}

/// Decode the octal escapes /proc/self/mounts uses for spaces and tabs.
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
            let value = (bytes[i + 1] - b'0') as u16 * 64
                + (bytes[i + 2] - b'0') as u16 * 8
                + (bytes[i + 3] - b'0') as u16;
            if value <= u8::MAX as u16 {
                out.push(value as u8);
                i += 4;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Volumes worth offering from a mount table: desktop automounts, /mnt
/// mounts and network shares; never the system's own file systems.
#[cfg(any(target_os = "linux", test))]
fn from_mounts(table: &str) -> Vec<(String, PathBuf, &'static str)> {
    let mut found = Vec::new();
    for line in table.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [_, mount, fs, options, ..] = fields[..] else {
            continue;
        };
        // Read-only mounts (an ISO, a squashfs image, a write-locked card)
        // cannot hold a destination.
        if options.split(',').any(|option| option == "ro") {
            continue;
        }
        let path = PathBuf::from(unescape(mount));
        let text = path.to_string_lossy();
        let network = NETWORK.contains(&fs);
        let removable = text.starts_with("/media/")
            || text.starts_with("/run/media/")
            || (text.starts_with("/mnt/") && !matches!(fs, "tmpfs" | "overlay" | "squashfs"));
        if !(network || removable) || found.iter().any(|(_, p, _)| p == &path) {
            continue;
        }
        let name = path
            .file_name()
            .map_or_else(|| text.to_string(), |n| n.to_string_lossy().into_owned());
        found.push((name, path, if network { "network" } else { "removable" }));
    }
    found
}

#[cfg(target_os = "linux")]
fn platform() -> Vec<Volume> {
    let table = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    from_mounts(&table)
        .into_iter()
        .filter(|(_, path, _)| path.is_dir())
        .map(|(name, path, kind)| {
            let (total_bytes, free_bytes) = space(&path);
            Volume {
                name,
                path,
                kind,
                total_bytes,
                free_bytes,
            }
        })
        .collect()
}

#[cfg(windows)]
fn platform() -> Vec<Volume> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    // GetDriveTypeW: 2 removable, 3 fixed, 4 remote.
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    // SAFETY: no arguments; returns a bitmask of drive letters.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let letter = (b'A' + bit) as char;
            let root = format!("{letter}:\\");
            let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
            // SAFETY: a NUL-terminated root path.
            let kind = match unsafe { GetDriveTypeW(wide.as_ptr()) } {
                2 => "removable",
                4 => "network",
                3 if !system.eq_ignore_ascii_case(&root[..2]) => "fixed",
                _ => return None,
            };
            let path = PathBuf::from(&root);
            let (total_bytes, free_bytes) = space(&path);
            (total_bytes > 0).then(|| Volume {
                name: format!("{letter}:"),
                path,
                kind,
                total_bytes,
                free_bytes,
            })
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn platform() -> Vec<Volume> {
    Vec::new()
}

/// A directory is a mount point when it lives on a different device than its
/// parent directory.
#[cfg(unix)]
fn mounted(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().unwrap_or(Path::new("/"));
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
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [device, mount, fs, ref rest @ ..] = fields[..] else {
                return None;
            };
            let bind = rest
                .first()
                .is_some_and(|options| options.split(',').any(|o| o == "bind" || o == "rbind"));
            let path = PathBuf::from(unescape(mount));
            (!device.starts_with('#')
                && mount.starts_with('/')
                && path != Path::new("/")
                && fs != "swap"
                && !bind)
                .then_some(path)
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
fn declared() -> Vec<PathBuf> {
    Vec::new()
}

/// The mount point an external volume path must be under: /Volumes/NAME on
/// macOS, /media/USER/NAME or /run/media/USER/NAME on Linux, the drive root on
/// Windows. `None` for paths elsewhere, which need no mount.
pub fn mount_root(path: &Path) -> Option<PathBuf> {
    root_in(path, &declared())
}

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
        // /media/NAME; desktops use /media/USER/NAME.
        Some("media") if cfg!(target_os = "linux") => match parts.get(1) {
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

/// Refuse a path on an external volume, or under a mount point fstab
/// declares, that is not mounted, instead of silently creating its
/// directories on the system disk.
pub fn ensure_present(path: &Path) -> Result<()> {
    present_in(path, &declared())
}

fn present_in(path: &Path, declared: &[PathBuf]) -> Result<()> {
    let under = || {
        let path = std::path::absolute(path).ok()?;
        declared
            .iter()
            .filter(|point| path.starts_with(point))
            .max_by_key(|point| point.components().count())
            .cloned()
    };
    let Some(root) = root_in(path, declared).or_else(under) else {
        return Ok(());
    };
    #[cfg(unix)]
    let present = mounted(&root);
    #[cfg(not(unix))]
    let present = root.is_dir();
    if !present {
        bail!(
            "external volume {} is not mounted; reconnect it or choose another destination",
            root.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_tables_offer_external_and_network_volumes_only() {
        let table = "/dev/nvme0n1p2 / ext4 rw 0 0\n\
                     proc /proc proc rw 0 0\n\
                     /dev/sda1 /media/ana/CAM\\040DISK exfat rw 0 0\n\
                     /dev/sdb1 /run/media/ana/SD vfat rw 0 0\n\
                     nas:/export /srv/nas nfs4 rw 0 0\n\
                     tmpfs /mnt/scratch tmpfs rw 0 0\n\
                     /dev/sdc1 /mnt/archive ext4 rw 0 0\n\
                     //nas/share /home/ana/share cifs rw 0 0\n\
                     /dev/sr0 /media/ana/DISC iso9660 ro,nosuid,nodev 0 0\n\
                     /dev/sdd1 /run/media/ana/LOCKED vfat ro,relatime 0 0\n\
                     /dev/sde1 /media/ana/ROOM ext4 rw,errors=remount-ro 0 0\n";
        let found = from_mounts(table);
        let names: Vec<_> = found.iter().map(|(n, _, k)| (n.as_str(), *k)).collect();
        assert_eq!(
            names,
            [
                ("CAM DISK", "removable"),
                ("SD", "removable"),
                ("nas", "network"),
                ("archive", "removable"),
                ("share", "network"),
                ("ROOM", "removable"),
            ]
        );
        assert_eq!(found[0].1, PathBuf::from("/media/ana/CAM DISK"));
    }

    #[test]
    fn external_paths_need_their_volume_mounted() {
        if cfg!(target_os = "macos") {
            let root = mount_root(Path::new("/Volumes/CAPTURES/run 1/shot.png"));
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
                mount_root(Path::new("/run/media/ana/SD/a.png")),
                Some(PathBuf::from("/run/media/ana/SD"))
            );
            assert_eq!(
                mount_root(Path::new("/media/ana/USB/x/y.png")),
                Some(PathBuf::from("/media/ana/USB"))
            );
            assert!(ensure_present(Path::new("/media/capturefab-missing/USB/a.png")).is_err());
        }
        assert_eq!(mount_root(Path::new("captures/a.png")), None);
        if cfg!(unix) {
            assert_eq!(mount_root(Path::new("/tmp/a.png")), None);
            ensure_present(Path::new("/tmp/capturefab/a.png")).unwrap();
        }
        assert!(list().iter().all(|v| v.path.is_absolute()));
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
        present_in(Path::new("/tmp/capturefab/a.png"), &declared).unwrap();
        std::fs::remove_dir_all(&point).unwrap();
        if cfg!(target_os = "linux") {
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
    fn read_only_mounts_are_recognized() {
        let table = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
            /dev/disk40s1 on /Volumes/Capturefab 0.5.0 (hfs, local, nodev, nosuid, read-only, noowners, quarantine, mounted by ana)\n\
            /dev/disk41s1 on /Volumes/CARD (exfat, local, nodev, nosuid, noowners, mounted by ana)\n\
            //ana@nas/share on /Volumes/share (smbfs, nodev, nosuid, mounted by ana)\n";
        let image = mount_line(table, Path::new("/Volumes/Capturefab 0.5.0")).unwrap();
        assert!(read_only(image));
        let card = mount_line(table, Path::new("/Volumes/CARD")).unwrap();
        assert!(!read_only(card));
        assert!(card.starts_with("exfat"));
        assert!(!read_only(
            mount_line(table, Path::new("/Volumes/share")).unwrap()
        ));
        assert_eq!(mount_line(table, Path::new("/Volumes/Other")), None);
        // An option list ends at its parenthesis; a later name never counts.
        assert!(!read_only("hfs, local) read-only"));
    }

    #[test]
    fn disk_image_installs_are_recognized() {
        let mounts = "/dev/disk40s1 on /Volumes/Capturefab 0.5.0 (hfs, local, nodev, nosuid, read-only, noowners, quarantine, mounted by ana)\n\
            /dev/disk41s1 on /Volumes/SSD (apfs, local, nodev, nosuid, journaled, noowners, mounted by ana)\n";
        let image = "/Volumes/Capturefab 0.5.0/Capturefab.app/Contents/MacOS/capturefab";
        assert_eq!(
            classify_install(image, mounts),
            Some(InstallLocation::DiskImage)
        );
        // Without `mount` output, the image's volume name still tells.
        assert_eq!(
            classify_install(image, ""),
            Some(InstallLocation::DiskImage)
        );
        assert_eq!(
            classify_install(
                "/private/var/folders/x/T/AppTranslocation/1A2B/d/Capturefab.app/Contents/MacOS/capturefab",
                ""
            ),
            Some(InstallLocation::Translocated)
        );
        // An app kept on a writable external drive is installed there.
        assert_eq!(
            classify_install(
                "/Volumes/SSD/Applications/Capturefab.app/Contents/MacOS/capturefab",
                mounts
            ),
            None
        );
        assert_eq!(
            classify_install(
                "/Volumes/SSD/Applications/Capturefab.app/Contents/MacOS/capturefab",
                ""
            ),
            None
        );
        assert_eq!(
            classify_install("/Applications/Capturefab.app/Contents/MacOS/capturefab", ""),
            None
        );
        assert_eq!(
            classify_install("/Volumes/SSD/bin/capturefab", mounts),
            None
        );
        if !cfg!(target_os = "macos") {
            assert_eq!(install_location(Path::new(image)), None);
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
