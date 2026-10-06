use super::{Appearance, Tab};
use crate::types::Transport;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const RECENT_LIMIT: usize = 6;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub appearance: Appearance,
    pub include_simulator: bool,
    pub tab: Tab,
    pub histogram_open: bool,
    pub sidebar_open: bool,
    pub inspector_open: bool,
    pub output: String,
    pub format: String,
    pub count: u32,
    pub timeout_ms: u64,
    pub schedule_delay_seconds: u64,
    pub schedule_interval_seconds: f64,
    pub forward_output: String,
    pub forward_codec: String,
    pub forward_encoder: String,
    pub forward_fps: f64,
    pub forward_bitrate: String,
    pub forward_file_mib: f64,
    pub quota_gib: f64,
    pub quota_files: u32,
    pub retention_enabled: bool,
    pub retention_days: u64,
    pub quota_action: String,
    pub recent: Vec<Recent>,
    pub window: Option<[f32; 2]>,
    pub logs_open: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recent {
    pub target: String,
    pub label: String,
    pub detail: String,
    pub transport: Transport,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            include_simulator: false,
            tab: Tab::Features,
            histogram_open: false,
            sidebar_open: true,
            inspector_open: true,
            output: "capture.png".into(),
            format: "png".into(),
            count: 1,
            timeout_ms: 5000,
            schedule_delay_seconds: 5,
            schedule_interval_seconds: 60.0,
            forward_output: "rtsp://127.0.0.1:8554/capturefab".into(),
            forward_codec: "h264".into(),
            forward_encoder: "auto".into(),
            forward_fps: 30.0,
            forward_bitrate: "4M".into(),
            forward_file_mib: 512.0,
            quota_gib: 10.0,
            quota_files: 10_000,
            retention_enabled: false,
            retention_days: 7,
            quota_action: "stop".into(),
            recent: Vec::new(),
            window: None,
            logs_open: false,
        }
    }
}
pub fn storable(address: &str) -> bool {
    crate::media::redact_url(address) == address
}

impl Prefs {
    pub fn remember(&mut self, entry: Recent) {
        if !storable(&entry.target) {
            return;
        }
        self.recent.retain(|r| r.target != entry.target);
        self.recent.insert(0, entry);
        self.recent.truncate(RECENT_LIMIT);
    }
    fn sanitized(&self) -> Self {
        let mut prefs = self.clone();
        if !storable(&prefs.forward_output) {
            prefs.forward_output = Self::default().forward_output;
        }
        prefs.recent.retain(|r| storable(&r.target));
        prefs
    }
}

fn path() -> PathBuf {
    crate::ipc::session_dir().join("gui.json")
}

pub fn load() -> Prefs {
    load_from(&path())
}

pub fn save(prefs: &Prefs) -> Result<()> {
    let dir = crate::ipc::session_dir();
    crate::ipc::ensure_private_dir(&dir)?;
    save_to(&dir.join("gui.json"), prefs)
}

fn load_from(path: &Path) -> Prefs {
    fs::read(path)
        .ok()
        .filter(|bytes| bytes.len() <= 1 << 20)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_to(path: &Path, prefs: &Prefs) -> Result<()> {
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp)?;
    output.write_all(&serde_json::to_vec_pretty(&prefs.sanitized())?)?;
    output.sync_all()?;
    fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recent(target: &str) -> Recent {
        Recent {
            target: target.into(),
            label: "Camera".into(),
            detail: "GigE".into(),
            transport: Transport::GigE,
        }
    }

    #[test]
    fn round_trips_and_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("capturefab-prefs-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gui.json");
        assert_eq!(load_from(&file), Prefs::default());
        let mut prefs = Prefs {
            appearance: Appearance::Dark,
            tab: Tab::Forward,
            count: 12,
            format: "jpeg".into(),
            ..Prefs::default()
        };
        prefs.remember(recent("rtsp://camera.local/stream"));
        save_to(&file, &prefs).unwrap();
        assert_eq!(load_from(&file), prefs);
        fs::write(&file, br#"{"count":3,"unknown":true}"#).unwrap();
        assert_eq!(
            load_from(&file),
            Prefs {
                count: 3,
                ..Prefs::default()
            }
        );
        fs::write(&file, b"not json").unwrap();
        assert_eq!(load_from(&file), Prefs::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn secrets_are_never_written() {
        let mut prefs = Prefs {
            forward_output: "rtsp://user:pass@nvr.local/live".into(),
            ..Prefs::default()
        };
        prefs.remember(recent("rtsp://admin:secret@10.0.0.5/stream"));
        prefs.remember(recent("http://10.0.0.5/video?token=abc"));
        assert!(prefs.recent.is_empty());
        let written = prefs.sanitized();
        assert_eq!(written.forward_output, Prefs::default().forward_output);
        let json = serde_json::to_string(&written).unwrap();
        assert!(!json.contains("pass") && !json.contains("secret"));
    }

    #[test]
    fn recent_is_newest_first_unique_and_capped() {
        let mut prefs = Prefs::default();
        for index in 0..8 {
            prefs.remember(recent(&format!("192.168.1.{index}")));
        }
        prefs.remember(recent("192.168.1.4"));
        let targets: Vec<_> = prefs.recent.iter().map(|r| r.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "192.168.1.4",
                "192.168.1.7",
                "192.168.1.6",
                "192.168.1.5",
                "192.168.1.3",
                "192.168.1.2"
            ]
        );
    }
}
