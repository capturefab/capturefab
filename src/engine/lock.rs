//! A camera's advisory lock: the worker that owns a device camera holds
//! `<runtime>/cameras/<hash>.lock`, which names it, so another process asking
//! for the camera learns at once who holds it, and a camera still held by a
//! worker that just died is told from one another application holds.
use crate::types::{CameraInfo, Categorized, ErrorKind, Transport};
use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

/// The lock of an open camera, released when dropped or when its worker
/// exits however it exits.
pub(crate) struct CameraLock(#[allow(dead_code)] std::fs::File);

/// Whether a camera is a device one process at a time owns. Streams, files
/// and the simulator are read by as many as ask.
fn lockable(info: &CameraInfo) -> bool {
    match info.transport {
        Transport::GigE | Transport::Usb3 => true,
        Transport::Media => crate::media::is_host_camera(&info.id),
        Transport::Simulator => crate::transport::simulator::exclusive(&info.id),
    }
}

fn path(info: &CameraInfo) -> PathBuf {
    let digest = Sha256::digest(info.id.as_bytes());
    let name = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    crate::ipc::runtime_dir()
        .join("cameras")
        .join(format!("{name}.lock"))
}

/// Take the lock of the camera `info` names, for session `session`. None
/// for a camera that needs none, or when the runtime folder cannot hold
/// locks: the device itself still refuses a second owner.
pub(crate) fn acquire(info: &CameraInfo, session: &str) -> Result<Option<CameraLock>> {
    if !lockable(info) {
        return Ok(None);
    }
    let path = path(info);
    let Some(dir) = path.parent() else {
        return Ok(None);
    };
    if crate::ipc::ensure_private_dir(dir).is_err() {
        return Ok(None);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(&path) else {
        return Ok(None);
    };
    if fs2::FileExt::try_lock_exclusive(&file).is_err() {
        let mut owner = String::new();
        let _ = file.read_to_string(&mut owner);
        return Err(busy(info, serde_json::from_str(&owner).unwrap_or_default()));
    }
    let owner = json!({"pid": std::process::id(), "session": session});
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = file.write_all(owner.to_string().as_bytes());
    let _ = file.flush();
    Ok(Some(CameraLock(file)))
}

/// `camera_busy` for the camera `info` names, which the worker `owner`
/// names holds: a session, with how long until it lets the camera go
/// unused, or a command of its own.
fn busy(info: &CameraInfo, mut owner: Value) -> anyhow::Error {
    let camera = crate::media::redact_url(&info.id);
    let session = owner["session"]
        .as_str()
        .filter(|session| *session != crate::session::DIRECT)
        .map(str::to_owned);
    let left = session.as_deref().and_then(crate::ipc::idle_left);
    if let (Some(left), Some(owner)) = (left, owner.as_object_mut()) {
        owner.insert(
            "idle_releases_in_s".into(),
            json!(left.as_millis().div_ceil(1000)),
        );
    }
    let pid = owner["pid"]
        .as_u64()
        .map(|pid| format!(" (process {pid})"))
        .unwrap_or_default();
    let releases = left
        .map(|left| {
            format!(
                ", which lets it go after {} unused",
                crate::scheduling::format_duration(left.as_millis().div_ceil(1000) as u64 * 1000)
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

/// Open a GigE camera whose control privilege may still be held by a worker
/// of this program that just died: nothing holds its lock, so retry until the
/// camera's heartbeat timeout has passed, plus half a second. `open` is the
/// attempt; `beat` keeps the caller counted as alive while it waits.
pub(crate) fn open_released<T>(
    info: &CameraInfo,
    timeout: Duration,
    mut open: impl FnMut() -> Result<T>,
    mut beat: impl FnMut(),
) -> Result<T> {
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
        // Windows keeps a locked file unreadable, so only the refusal shows.
        #[cfg(unix)]
        {
            let details = crate::types::classify(&error).details.unwrap();
            assert_eq!(details["held_by"]["pid"], std::process::id());
            assert_eq!(details["held_by"]["session"], "bench");
        }
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
}
