//! A camera's advisory lock: the worker that owns a device camera holds
//! `<runtime>/cameras/<hash>.lock`, which names it, so another process asking
//! for the camera learns at once who holds it, and a camera still held by a
//! worker that just died is told from one another application holds.
use crate::types::{CameraInfo, Categorized, ErrorKind, Transport};
use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::{Read, Seek as _, Write as _},
    path::PathBuf,
    time::{Duration, Instant},
};

/// The lock of an open camera, released when dropped or when its worker
/// exits however it exits.
#[must_use = "retain the lock while this worker owns the camera"]
pub(crate) struct CameraLock {
    _file: std::fs::File,
    presence: Option<crate::transport::gige::monitor::presence::Presence>,
}

/// Validate the distinct explicit monitor identity before bypassing controller ownership.
fn monitor(info: &CameraInfo) -> Result<bool> {
    if !crate::transport::gige::monitor::is_selector(&info.id) {
        return Ok(false);
    }
    let spec = crate::transport::gige::monitor::MonitorSpec::parse(&info.id)?;
    anyhow::ensure!(
        info.transport == Transport::GigE && info.id == spec.canonical(),
        crate::types::Usage(
            "monitor lock admission requires canonical GigE monitor identity".into()
        )
    );
    Ok(true)
}

/// Whether a camera is a device one process at a time owns. Streams, files
/// and the simulator are read by as many as ask.
fn lockable(info: &CameraInfo) -> bool {
    match info.transport {
        Transport::GigE | Transport::Usb3 | Transport::GenTl => true,
        Transport::Media => crate::media::is_host_camera(&info.id),
        Transport::Simulator => crate::transport::simulator::exclusive(&info.id),
    }
}

/// The camera's lock file, in the runtime folder when there is one.
fn path(info: &CameraInfo) -> Option<PathBuf> {
    let digest = Sha256::digest(info.id.as_bytes());
    let name = crate::types::hex(&digest[..8]);
    let runtime = crate::dirs::runtime().ok()?;
    Some(runtime.join("cameras").join(format!("{name}.lock")))
}

/// Take the lock of the camera `info` names, for session `session`. None
/// for a camera that needs none, or when the runtime folder cannot hold
/// locks: the device itself still refuses a second owner.
pub(crate) fn acquire(info: &CameraInfo, session: &str) -> Result<Option<CameraLock>> {
    if monitor(info)? || !lockable(info) {
        return Ok(None);
    }
    let Some(path) = path(info) else {
        return Ok(None);
    };
    let Some(dir) = path.parent() else {
        return Ok(None);
    };
    if crate::dirs::ensure_private_dir(dir).is_err() {
        return Ok(None);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(&path) else {
        return Ok(None);
    };
    if !try_lock(&file) {
        return Err(busy(info, read_owner(&mut file)));
    }
    let owner = json!({"pid": std::process::id(), "session": session});
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = file.write_all(owner.to_string().as_bytes());
    let _ = file.flush();
    Ok(Some(CameraLock {
        _file: file,
        presence: None,
    }))
}

/// Unlike normal capture's best-effort lock, an explicit network mutation must
/// retain its advisory reservation or fail before sending anything.
pub(super) fn reserve_network(id: &str) -> Result<CameraLock> {
    let runtime = crate::dirs::runtime()?;
    let digest = Sha256::digest(id.as_bytes());
    let name = crate::types::hex(&digest[..8]);
    let directory = runtime.join("cameras");
    crate::dirs::ensure_private_dir(&directory)?;
    reserve_network_with(id, &directory.join(format!("{name}.lock")), || {
        crate::transport::gige::monitor::presence::reserve_network(id)
    })
}

fn reserve_network_with(
    id: &str,
    path: &std::path::Path,
    reserve: impl FnOnce() -> Result<crate::transport::gige::monitor::presence::Presence>,
) -> Result<CameraLock> {
    let mut controller = reserve_network_at(id, path)?;
    controller.presence = Some(reserve()?);
    Ok(controller)
}

fn reserve_network_at(id: &str, path: &std::path::Path) -> Result<CameraLock> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if !try_lock(&file) {
        return Err(Categorized::new(
            ErrorKind::CameraBusy,
            "ForceIP target has a Capturefab runtime owner; disconnect it first",
        )
        .details(json!({"camera":id,"owner":read_owner(&mut file)}))
        .into());
    }
    file.set_len(0)?;
    file.rewind()?;
    serde_json::to_writer(
        &mut file,
        &json!({"pid":std::process::id(),"operation":"network force-ip"}),
    )?;
    file.flush()?;
    Ok(CameraLock {
        _file: file,
        presence: None,
    })
}

/// Owner metadata is only a small diagnostic, even when the lock file is damaged.
#[deny(clippy::arithmetic_side_effects)]
fn read_owner(reader: impl Read) -> Value {
    let mut owner = String::new();
    let _ = reader.take(4097).read_to_string(&mut owner);
    if owner.len() > 4096 {
        Value::Null
    } else {
        serde_json::from_str(&owner).unwrap_or_default()
    }
}

/// Take `file`'s lock for this process: false when another holds it. The
/// lock lasts until the file is closed, as the process that holds it exits.
#[cfg(not(windows))]
fn try_lock(file: &std::fs::File) -> bool {
    fs2::FileExt::try_lock_exclusive(file).is_ok()
}
/// Windows keeps what a lock covers from being read through any other
/// handle, so the lock covers one byte far past the owner written at the
/// start, which a process asking for the camera reads. A lock on the whole
/// file, as an older Capturefab takes, overlaps it all the same.
#[cfg(windows)]
fn try_lock(file: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::{
        Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx},
        System::IO::OVERLAPPED,
    };
    let mut at = OVERLAPPED::default();
    // Byte 4 GiB: no owner is that long.
    at.Anonymous.Anonymous.OffsetHigh = 1;
    // SAFETY: the handle is the open file's, and `at` outlives the call,
    // which with LOCKFILE_FAIL_IMMEDIATELY returns at once.
    unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &raw mut at,
        ) != 0
    }
}

/// `camera_busy` for the camera `info` names, which the worker `owner`
/// names holds: a session, with how long until it lets that camera go
/// unused, or a command of its own.
fn busy(info: &CameraInfo, mut owner: Value) -> anyhow::Error {
    let camera = crate::media::redact_url(&info.id);
    let session = owner["session"]
        .as_str()
        .filter(|session| *session != crate::session::DIRECT)
        .map(str::to_owned);
    let left_secs = session
        .as_deref()
        .and_then(|session| crate::ipc::idle_left(session, &info.id))
        .map(|left| u64::try_from(left.as_millis().div_ceil(1000)).unwrap_or(u64::MAX));
    if let (Some(left), Some(owner)) = (left_secs, owner.as_object_mut()) {
        owner.insert("idle_releases_in_s".into(), json!(left));
    }
    let pid = owner["pid"]
        .as_u64()
        .map(|pid| format!(" (process {pid})"))
        .unwrap_or_default();
    let releases = left_secs
        .map(|left| {
            format!(
                ", which lets it go after {} unused",
                crate::types::duration_text(Duration::from_secs(left))
            )
        })
        .unwrap_or_default();
    let (message, hint) = match session.as_deref() {
        Some(crate::cli::AUTO_SESSION) => (
            format!("{camera} is kept open by the auto session{pid}{releases}"),
            "use it as you are (commands find it), stop it with `capturefab session stop auto`, or pass --keep 0 so commands open cameras for themselves alone".to_owned(),
        ),
        Some(name) => (
            format!("{camera} is held by session {name}{pid}{releases}"),
            format!("use that session (--session {name}), or stop it with `capturefab session stop {name}`"),
        ),
        None if owner["pid"].is_u64() => (
            format!("{camera} is held by another capturefab command{pid}"),
            "wait for it to finish".to_owned(),
        ),
        None => (
            format!("{camera} is held by another capturefab process"),
            "wait for it to finish, or stop the session that holds it".to_owned(),
        ),
    };
    let mut error = Categorized::new(ErrorKind::CameraBusy, message).hint(hint);
    if owner.is_object() {
        error = error.details(json!({ "held_by": owner }));
    }
    error.into()
}

/// Open a `GigE` camera whose control privilege may still be held by a worker
/// of this program that just died: nothing holds its lock, so retry until the
/// camera's heartbeat timeout has passed, plus half a second. `open` is the
/// attempt; `beat` keeps the caller counted as alive while it waits.
pub(crate) fn open_released<T>(
    info: &CameraInfo,
    timeout: Duration,
    mut open: impl FnMut() -> Result<T>,
    mut beat: impl FnMut(),
) -> Result<T> {
    if monitor(info)? {
        return open();
    }
    let mut deadline: Option<Instant> = None;
    loop {
        let error = match open() {
            Ok(opened) => return Ok(opened),
            Err(error) => error,
        };
        if info.transport != Transport::GigE
            || crate::types::classify(&error).kind != ErrorKind::CameraBusy
        {
            return Err(error);
        }
        let deadline = *deadline.get_or_insert_with(|| {
            let heartbeat = crate::transport::gige::heartbeat_timeout(info, timeout)
                .unwrap_or(Duration::from_secs(3))
                .min(Duration::from_secs(30));
            Instant::now() + heartbeat + Duration::from_millis(500)
        });
        if Instant::now() >= deadline {
            return Err(error);
        }
        beat();
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_reservation_is_strict_and_keeps_its_file_until_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("camera.lock");
        let id = "gige:003053061a58";
        let held = reserve_network_at(id, &path).unwrap();
        let error = reserve_network_at(id, &path).err().unwrap();
        assert_eq!(crate::cli::error_code(&error), ("camera_busy", 3));
        let owner = std::fs::File::open(&path).unwrap();
        assert_eq!(read_owner(owner)["operation"], "network force-ip");
        drop(held);
        let acquired = reserve_network_at(id, &path).unwrap();
        let mut other = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(!try_lock(&other));
        assert_eq!(read_owner(&mut other)["pid"], std::process::id());
        drop(acquired);
        assert!(try_lock(&other));
        let missing = directory.path().join("missing").join("camera.lock");
        assert!(
            reserve_network_at(id, &missing)
                .err()
                .unwrap()
                .to_string()
                .contains("os error")
        );
        assert!(!missing.exists());
    }

    #[test]
    fn lock_owner_metadata_is_bounded_before_parsing() {
        let expected = json!({"pid": 42, "session": "camera-像"});
        let mut bytes = serde_json::to_vec(&expected).unwrap();
        bytes.resize(4096, b' ');
        assert_eq!(read_owner(&*bytes), expected);
        bytes.resize(16 * 1024, b' ');
        let mut reader = std::io::Cursor::new(bytes);
        assert_eq!(read_owner(&mut reader), Value::Null);
        assert_eq!(reader.position(), 4097);
        for invalid in [&b"{"[..], &b"invalid"[..], &b"\xff"[..]] {
            assert_eq!(read_owner(invalid), Value::Null);
        }
    }

    #[test]
    fn device_cameras_are_locked_to_one_owner_who_is_named() {
        let mut info = crate::transport::simulator::info_named("sim:lock");
        assert!(
            acquire(&info, "bench").unwrap().is_none(),
            "no lock for sim"
        );
        info.id = format!("gige:00c0df{:06x}", std::process::id() & 0xff_ffff);
        info.transport = Transport::GigE;
        let held = acquire(&info, "bench").unwrap().unwrap();
        // A second owner, in this process or another, is refused.
        let error = acquire(&info, "other").err().unwrap();
        assert_eq!(crate::cli::error_code(&error), ("camera_busy", 3));
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(details["held_by"]["pid"], std::process::id());
        assert_eq!(details["held_by"]["session"], "bench");
        drop(held);
        assert!(acquire(&info, "other").unwrap().is_some());
    }

    #[test]
    fn only_a_busy_gige_camera_is_waited_for() {
        let info = crate::transport::simulator::info_named("sim:0");
        let mut tries = 0;
        let error = open_released(
            &info,
            Duration::from_millis(10),
            || -> Result<()> {
                tries += 1;
                Err(Categorized::error(ErrorKind::CameraBusy, "busy"))
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(tries, 1);
        assert_eq!(crate::cli::error_code(&error).0, "camera_busy");
    }

    #[test]
    fn validated_monitor_bypasses_controller_lock_and_dead_lease_retry() {
        let mut info = crate::transport::simulator::info_named("sim:0");
        info.id = "gige-monitor://192.0.2.10?interface=192.0.2.20&mac=003053061a58".into();
        info.transport = Transport::GigE;
        assert!(acquire(&info, "monitor").unwrap().is_none());
        let (mut tries, mut beats) = (0, 0);
        let error = open_released(
            &info,
            Duration::from_millis(10),
            || -> Result<()> {
                tries += 1;
                Err(Categorized::error(
                    ErrorKind::CameraBusy,
                    "synthetic busy monitor",
                ))
            },
            || beats += 1,
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::CameraBusy);
        assert_eq!(tries, 1);
        assert_eq!(beats, 0);
        info.transport = Transport::Usb3;
        assert_eq!(
            crate::types::classify(&acquire(&info, "monitor").err().unwrap()).kind,
            ErrorKind::Usage
        );
    }
    #[test]
    fn force_ip_retains_controller_then_exclusive_presence_and_releases_both() {
        use crate::transport::gige::monitor::presence::{test_monitor_in, test_network_in};
        let folder = tempfile::tempdir().unwrap();
        let presence = folder.path().join("presence");
        let controller = folder.path().join("controller.lock");
        let id = "gige:003053061a58";
        let monitor = test_monitor_in(id, &presence).unwrap();
        let error = reserve_network_with(id, &controller, || {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&controller)
                .unwrap();
            assert!(
                !try_lock(&file),
                "controller reservation must precede presence"
            );
            test_network_in(id, &presence)
        })
        .err()
        .unwrap();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::CameraBusy);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&controller)
            .unwrap();
        assert!(
            try_lock(&file),
            "failed presence acquisition must release controller"
        );
        drop(file);
        drop(monitor);
        let reservation =
            reserve_network_with(id, &controller, || test_network_in(id, &presence)).unwrap();
        assert_eq!(
            crate::types::classify(&test_monitor_in(id, &presence).unwrap_err()).kind,
            ErrorKind::CameraBusy
        );
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&controller)
            .unwrap();
        assert!(!try_lock(&file));
        drop(reservation);
        assert!(try_lock(&file));
        drop(file);
        drop(test_monitor_in(id, &presence).unwrap());
    }
}
