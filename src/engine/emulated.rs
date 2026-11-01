//! Settings a camera worker emulates for its camera, such as a simulator's
//! registers or a webcam's video mode, kept per camera in the runtime
//! folder: the next worker to open the camera takes them back, so a `set`
//! and a later `capture` behave as on a real camera, which keeps its
//! settings until it is powered off. Without a runtime folder, as on a
//! read-only system with nothing writable, nothing is kept.
use anyhow::{Context as _, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{io::Write as _, path::PathBuf};

/// The file of `camera`'s settings, named by a hash: a camera's ID may be a
/// URL with a password, which must not reach a file name.
fn path(camera: &str) -> Result<PathBuf> {
    let digest = Sha256::digest(key(camera).as_bytes());
    let name = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(crate::dirs::runtime()?
        .join("settings")
        .join(format!("{name}.json")))
}
/// The camera a selector names: a simulator's options set up the
/// connection, not the camera.
fn key(camera: &str) -> &str {
    if camera.starts_with("sim:") {
        crate::transport::simulator::base(camera)
    } else {
        camera
    }
}

/// The settings saved for `camera`, if any could be read.
pub(super) fn load(camera: &str) -> Option<Value> {
    let bytes = std::fs::read(path(camera).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Save `settings` for `camera`, replacing what was saved: written whole to
/// a private file first, so a reader never sees half of it.
pub(super) fn save(camera: &str, settings: &Value) -> Result<()> {
    let path = path(camera)?;
    let dir = path.parent().context("settings folder")?;
    crate::dirs::ensure_private_dir(dir)?;
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("camera"),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(settings)?)?;
        drop(file);
        std::fs::rename(&temp, &path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written.with_context(|| format!("save camera settings in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_kept_per_camera_without_its_address() {
        let camera = "rtsp://user:secret-password@cam.test/emulated-settings";
        save(camera, &serde_json::json!({"fps": 10.0})).unwrap();
        assert_eq!(load(camera), Some(serde_json::json!({"fps": 10.0})));
        assert!(!path(camera).unwrap().to_string_lossy().contains("secret"));
        assert_eq!(path("sim:7?drop=0.1").unwrap(), path("sim:7").unwrap());
        assert_ne!(path("sim:7").unwrap(), path("sim:8").unwrap());
        std::fs::remove_file(path(camera).unwrap()).unwrap();
        assert_eq!(load(camera), None);
    }
}
