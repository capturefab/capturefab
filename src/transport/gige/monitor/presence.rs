//! Mandatory local presence reservations; files persist and kernel locks release on drop.
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::{Context as _, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

#[derive(Debug)]
#[must_use = "retain presence across all monitor I/O or the complete network mutation"]
pub(crate) struct Presence {
    _file: File,
}

#[derive(Clone, Copy)]
enum Mode {
    Monitor,
    Network,
}

fn mac(physical_id: &str) -> Result<&str> {
    let value = physical_id.strip_prefix("gige:").unwrap_or_default();
    ensure!(
        value.len() == 12
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        Usage("camera presence requires canonical gige:lowercase12hex physical id".into())
    );
    Ok(value)
}

/// Hold shared local presence without acquiring camera control.
///
/// # Errors
/// Rejects a noncanonical id, unavailable private runtime storage, unsafe file,
/// or a concurrent exclusive network reservation. No best-effort fallback exists.
pub(crate) fn acquire_monitor(physical_id: &str) -> Result<Presence> {
    acquire(physical_id, Mode::Monitor)
}

/// Exclude every local monitor for the full network mutation.
///
/// # Errors
/// Returns the strict presence errors described by [`acquire_monitor`], including
/// `camera_busy` when any monitor or another network reservation holds the file.
pub(crate) fn reserve_network(physical_id: &str) -> Result<Presence> {
    acquire(physical_id, Mode::Network)
}

fn acquire(physical_id: &str, mode: Mode) -> Result<Presence> {
    mac(physical_id)?;
    let directory = crate::dirs::runtime()?.join("camera-presence");
    acquire_in(physical_id, &directory, mode)
}

fn acquire_in(physical_id: &str, directory: &Path, mode: Mode) -> Result<Presence> {
    let mac = mac(physical_id)?;
    crate::dirs::ensure_private_dir(directory)?;
    let path = directory.join(format!("{mac}.lock"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            Categorized::error(
                ErrorKind::PermissionDenied,
                "camera presence path must be a plain file"
            )
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect mandatory camera presence reservation"),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options
        .open(&path)
        .context("open mandatory camera presence reservation")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        Categorized::error(
            ErrorKind::PermissionDenied,
            "camera presence must be a regular file"
        )
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let named = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.uid() == crate::dirs::effective_uid()
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1
                && named.dev() == metadata.dev()
                && named.ino() == metadata.ino()
                && !named.file_type().is_symlink(),
            Categorized::error(
                ErrorKind::PermissionDenied,
                "camera presence must be a private single-link file of this user"
            )
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: file owns the valid handle; info is writable through this synchronous call.
        let queried = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) };
        ensure!(queried != 0, std::io::Error::last_os_error());
        ensure!(
            info.nNumberOfLinks == 1 && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            Categorized::error(
                ErrorKind::PermissionDenied,
                "camera presence must be a plain single-link file"
            )
        );
    }
    let locked = match mode {
        Mode::Monitor => fs2::FileExt::try_lock_shared(&file),
        Mode::Network => fs2::FileExt::try_lock_exclusive(&file),
    };
    if let Err(error) = locked {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            return Err(Categorized::new(ErrorKind::CameraBusy,
                "camera has a local monitor or exclusive network reservation")
                .details(serde_json::json!({"camera":physical_id,"reservation":match mode { Mode::Monitor => "monitor", Mode::Network => "network" }})).into());
        }
        return Err(error).context("lock mandatory camera presence reservation");
    }
    Ok(Presence { _file: file })
}

#[cfg(test)]
pub(crate) fn test_monitor_in(id: &str, directory: &Path) -> Result<Presence> {
    acquire_in(id, directory, Mode::Monitor)
}

#[cfg(test)]
pub(crate) fn test_network_in(id: &str, directory: &Path) -> Result<Presence> {
    acquire_in(id, directory, Mode::Network)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    const ID: &str = "gige:003053061a58";

    #[test]
    fn canonical_ids_fail_before_creating_a_presence_directory() {
        let folder = tempfile::tempdir().unwrap();
        let directory = folder.path().join("absent");
        for id in [
            "",
            "gige:003053061A58",
            "003053061a58",
            "gige:003053061a5",
            "gige:003053061a580",
            "gige:../abcdefghi",
            "gige-monitor://192.0.2.1",
        ] {
            let error = acquire_in(id, &directory, Mode::Monitor).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
            assert!(!directory.exists());
        }
    }

    #[test]
    fn shared_monitors_exclude_force_ip_until_every_guard_drops() {
        let folder = tempfile::tempdir().unwrap();
        let directory = folder.path().join("presence");
        let first = acquire_in(ID, &directory, Mode::Monitor).unwrap();
        let second = acquire_in(ID, &directory, Mode::Monitor).unwrap();
        assert_eq!(
            crate::types::classify(&acquire_in(ID, &directory, Mode::Network).unwrap_err()).kind,
            ErrorKind::CameraBusy
        );
        drop(first);
        assert_eq!(
            crate::types::classify(&acquire_in(ID, &directory, Mode::Network).unwrap_err()).kind,
            ErrorKind::CameraBusy
        );
        drop(second);
        let network = acquire_in(ID, &directory, Mode::Network).unwrap();
        assert_eq!(
            crate::types::classify(&acquire_in(ID, &directory, Mode::Monitor).unwrap_err()).kind,
            ErrorKind::CameraBusy
        );
        assert_eq!(
            crate::types::classify(&acquire_in(ID, &directory, Mode::Network).unwrap_err()).kind,
            ErrorKind::CameraBusy
        );
        let other = acquire_in("gige:003053061a59", &directory, Mode::Monitor).unwrap();
        drop(network);
        drop(acquire_in(ID, &directory, Mode::Monitor).unwrap());
        assert!(directory.join("003053061a58.lock").is_file());
        assert_eq!(
            std::fs::read(directory.join("003053061a58.lock")).unwrap(),
            Vec::<u8>::new()
        );
        drop(other);
        fn send<T: Send>() {}
        send::<Presence>();
    }

    #[test]
    fn concurrent_monitor_and_network_reservations_cannot_both_win() {
        let folder = tempfile::tempdir().unwrap();
        let directory = folder.path().join("race");
        drop(acquire_in(ID, &directory, Mode::Network).unwrap());
        let (results, received) = mpsc::channel();
        let mut starts = Vec::new();
        let mut releases = Vec::new();
        let mut threads = Vec::new();
        for mode in [Mode::Monitor, Mode::Network] {
            let (start, ready) = mpsc::channel();
            let (release, done) = mpsc::channel();
            let results = results.clone();
            let directory = directory.clone();
            threads.push(std::thread::spawn(move || {
                ready.recv_timeout(Duration::from_secs(2)).unwrap();
                let guard = acquire_in(ID, &directory, mode);
                results.send(guard.is_ok()).unwrap();
                let _ = done.recv_timeout(Duration::from_secs(2));
                drop(guard);
            }));
            starts.push(start);
            releases.push(release);
        }
        for start in starts {
            start.send(()).unwrap();
        }
        let winners = [
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        for release in releases {
            let _ = release.send(());
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(winners.into_iter().filter(|won| *won).count(), 1);
    }

    #[test]
    fn unavailable_or_unsafe_presence_storage_has_no_fallback() {
        let folder = tempfile::tempdir().unwrap();
        let directory = folder.path().join("not-directory");
        std::fs::write(&directory, b"foreign").unwrap();
        assert_eq!(
            crate::types::classify(&acquire_in(ID, &directory, Mode::Monitor).unwrap_err()).kind,
            ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(&directory).unwrap(), b"foreign");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let directory = folder.path().join("unsafe");
            crate::dirs::ensure_private_dir(&directory).unwrap();
            let target = directory.join("003053061a58.lock");
            std::fs::write(&target, b"").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                crate::types::classify(&acquire_in(ID, &directory, Mode::Monitor).unwrap_err())
                    .kind,
                ErrorKind::PermissionDenied
            );
            std::fs::remove_file(&target).unwrap();
            std::os::unix::fs::symlink(directory.join("foreign"), &target).unwrap();
            assert_eq!(
                crate::types::classify(&acquire_in(ID, &directory, Mode::Monitor).unwrap_err())
                    .kind,
                ErrorKind::PermissionDenied
            );
            assert!(!directory.join("foreign").exists());
        }
    }
}
