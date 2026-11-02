//! Files a camera worker saved: captures and finished recordings, as the
//! `saved` lines of its protocol report them once they are published, and
//! as the lines of a capture's `--manifest`.
use crate::context::Context;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedItem {
    Capture {
        frame_id: u64,
        /// The camera's clock when it took the frame; see `Frame`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_ts_ns: Option<u64>,
        width: u32,
        height: u32,
        pixel_format: u32,
        format: String,
        /// capture or schedule.
        origin: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<u64>,
        /// The file is the JPEG the source sent, not an encoding of the
        /// frame.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        passthrough: bool,
    },
    Recording {
        started_ms: u64,
        ended_ms: u64,
        codec: String,
        encoder: String,
        fps: f64,
    },
}

/// One finished file, reported by a camera worker to the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedFile {
    /// UTC Unix milliseconds, taken when the frame was received.
    pub wall_ms: u64,
    pub path: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Context::is_empty")]
    pub context: Context,
    #[serde(flatten)]
    pub item: SavedItem,
}

/// A finished local recording, reported by the forwarder when it stops.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordedFile {
    pub path: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub codec: String,
    pub encoder: String,
    pub fps: f64,
}

impl SavedFile {
    /// A capture's `--manifest` line: `{file, camera, time, device_ts_ns?,
    /// frame_id?, width, height, pixel_format, bytes, sha256, settings,
    /// tags}`. `features` are the camera's values when the frame came.
    pub fn manifest_line(&self, camera: &str, features: &Value) -> Value {
        let SavedItem::Capture {
            frame_id,
            device_ts_ns,
            width,
            height,
            pixel_format,
            ..
        } = &self.item
        else {
            return Value::Null;
        };
        crate::types::without_nulls(json!({
            "file": self.path,
            "camera": crate::media::redact_url(camera),
            "time": crate::scheduling::rfc3339(self.wall_ms),
            "device_ts_ns": device_ts_ns,
            "frame_id": (*frame_id > 0).then_some(*frame_id),
            "width": width,
            "height": height,
            "pixel_format": crate::inspect::pixel_format_label(*pixel_format),
            "bytes": self.bytes,
            "sha256": self.sha256,
            "settings": settings(features),
            "tags": self.context,
        }))
    }
}

/// The camera's settings, from the GenICam values a worker read, under the
/// names `inspect` and `info` use, with units in the names: SFNC gives
/// ExposureTime in microseconds and Gain in dB.
pub fn settings(features: &Value) -> Value {
    let number = |names: &[&str]| names.iter().find_map(|name| features[*name].as_f64());
    let mode = |name: &str| features[name].as_str().map(str::to_ascii_lowercase);
    let trigger = match features["TriggerMode"].as_str() {
        Some(mode) if mode.eq_ignore_ascii_case("off") => Some("off".to_owned()),
        Some(_) => Some(
            features["TriggerSource"]
                .as_str()
                .map_or_else(|| "on".to_owned(), str::to_ascii_lowercase),
        ),
        None => None,
    };
    let binning = number(&["BinningHorizontal"])
        .zip(number(&["BinningVertical"]))
        .map(|(x, y)| format!("{x}x{y}"));
    crate::types::without_nulls(json!({
        "exposure_us": number(&["ExposureTime", "ExposureTimeAbs"]),
        "auto_exposure": mode("ExposureAuto"),
        "gain_db": number(&["Gain"]),
        "gain": features["Gain"].is_null().then(|| number(&["GainRaw"])).flatten(),
        "auto_gain": mode("GainAuto"),
        "auto_wb": mode("BalanceWhiteAuto"),
        "fps": number(&["AcquisitionFrameRate", "AcquisitionFrameRateAbs"]),
        "trigger": trigger,
        "black_level": number(&["BlackLevel"]),
        "gamma": number(&["Gamma"]),
        "binning": binning,
        "offset_x": number(&["OffsetX"]),
        "offset_y": number(&["OffsetY"]),
        "reverse_x": features["ReverseX"].as_bool(),
        "reverse_y": features["ReverseY"].as_bool(),
    }))
}

/// A `--manifest` file: one JSON line appended per published file, each in
/// one write to a file opened for appending, so the lines of several
/// cameras' workers never interleave. The job that writes it syncs it when
/// it ends.
#[derive(Debug)]
pub struct Manifest {
    file: File,
    path: PathBuf,
}
impl Manifest {
    /// Open `path` for appending, creating it and its folder when missing.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            crate::volumes::ensure_present(parent)?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create the folder of {}", path.display()))?;
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .with_context(|| format!("cannot open manifest {}", path.display()))?;
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "manifest {} is not a file",
            path.display()
        );
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
    pub fn append(&self, line: &Value) -> Result<()> {
        let mut text = serde_json::to_vec(line)?;
        text.push(b'\n');
        (&self.file)
            .write_all(&text)
            .with_context(|| format!("cannot append to manifest {}", self.path.display()))
    }
    pub fn sync(&self) -> Result<()> {
        self.file
            .sync_data()
            .with_context(|| format!("cannot sync manifest {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_take_the_names_inspect_uses() {
        let shown = settings(&json!({
            "ExposureTime": 10000.0, "ExposureAuto": "Off", "Gain": 0.0,
            "AcquisitionFrameRate": 30.0, "TriggerMode": "On", "TriggerSource": "Line0",
            "BinningHorizontal": 2, "BinningVertical": 2,
        }));
        assert_eq!(
            shown,
            json!({"exposure_us": 10000.0, "auto_exposure": "off", "gain_db": 0.0,
                "fps": 30.0, "trigger": "line0", "binning": "2x2"})
        );
        assert_eq!(
            settings(&json!({"TriggerMode": "Off", "GainRaw": 4})),
            json!({"trigger": "off", "gain": 4.0})
        );
        assert_eq!(settings(&Value::Null), json!({}));
    }

    #[test]
    fn manifest_lines_describe_each_file_and_append_whole() {
        let file = SavedFile {
            wall_ms: 1_793_000_000_123,
            path: "/data/shot-0001.png".into(),
            bytes: 2048,
            sha256: Some("ab".repeat(32)),
            context: Context::from([("lot".into(), "7".into())]),
            item: SavedItem::Capture {
                frame_id: 12,
                device_ts_ns: Some(5_000),
                width: 64,
                height: 48,
                pixel_format: crate::types::MONO8,
                format: "png".into(),
                origin: "schedule".into(),
                job_id: Some(1),
                passthrough: false,
            },
        };
        let features = json!({"ExposureTime": 2000.0});
        let line = file.manifest_line("rtsp://user:secret@cam/live", &features);
        assert_eq!(
            line,
            json!({
                "file": "/data/shot-0001.png",
                "camera": "rtsp://<redacted>@cam/live",
                "time": "2026-10-26T07:33:20.123Z",
                "device_ts_ns": 5000,
                "frame_id": 12,
                "width": 64,
                "height": 48,
                "pixel_format": "Mono8",
                "bytes": 2048,
                "sha256": "ab".repeat(32),
                "settings": {"exposure_us": 2000.0},
                "tags": {"lot": "7"},
            })
        );
        // A frame without the camera's clock has no device time.
        let mut hosted = file.clone();
        if let SavedItem::Capture { device_ts_ns, .. } = &mut hosted.item {
            *device_ts_ns = None;
        }
        let hosted = hosted.manifest_line("sim:0", &Value::Null);
        assert!(hosted.get("device_ts_ns").is_none(), "{hosted}");
        let dir = std::env::temp_dir().join(format!("capturefab-manifest-{}", std::process::id()));
        let path = dir.join("runs/manifest.jsonl");
        let manifest = Manifest::open(&path).unwrap();
        manifest.append(&line).unwrap();
        Manifest::open(&path).unwrap().append(&line).unwrap();
        manifest.sync().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(
            text.lines()
                .all(|row| serde_json::from_str::<Value>(row).unwrap() == line)
        );
        assert!(Manifest::open(&dir).is_err(), "a folder is no manifest");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
