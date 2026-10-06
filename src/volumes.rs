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

/// Mounted external, removable and network volumes, by name.
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
    let kind_of = |path: &Path| -> &'static str {
        let needle = format!(" on {} (", path.display());
        types
            .lines()
            .find(|line| line.contains(&needle))
            .and_then(|line| line.split(&needle).nth(1))
            .and_then(|rest| rest.split([',', ')']).next())
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
            let (total_bytes, free_bytes) = space(&path);
            Some(Volume {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: kind_of(&path),
                path,
                total_bytes,
                free_bytes,
            })
        })
        .collect()
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
            && let Ok(value) = u8::from_str_radix(&field[i + 1..i + 4], 8)
        {
            out.push(value);
            i += 4;
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
        let [_, mount, fs, ..] = fields[..] else {
            continue;
        };
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

/// The mount point an external volume path must be under: /Volumes/NAME on
/// macOS, /media/USER/NAME or /run/media/USER/NAME on Linux, the drive root on
/// Windows. `None` for paths elsewhere, which need no mount.
pub fn mount_root(path: &Path) -> Option<PathBuf> {
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
        Some("media") if cfg!(target_os = "linux") => 3,
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

/// Refuse a path on an external volume that is not mounted, instead of
/// silently creating its directories on the system disk.
pub fn ensure_present(path: &Path) -> Result<()> {
    let Some(root) = mount_root(path) else {
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
                     //nas/share /home/ana/share cifs rw 0 0\n";
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
    fn octal_escapes_decode() {
        assert_eq!(unescape("a\\040b\\011c"), "a b\tc");
        assert_eq!(unescape("plain"), "plain");
    }
}
