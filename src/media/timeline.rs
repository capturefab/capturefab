//! Per-frame correspondence between a local video and the host receive clock.
use crate::{
    storage,
    types::{Categorized, ErrorKind, Frame, Usage},
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub path: String,
    pub acquisition_end_ns: u64,
}
impl Config {
    pub(crate) fn validate(&self, output: &str) -> Result<()> {
        if self.acquisition_end_ns == 0
            || !Path::new(&self.path).is_absolute()
            || self.path.contains("://")
            || crate::naming::has_placeholder(&self.path)
        {
            return Err(Usage("video timeline needs an absolute literal local path and a positive acquisition_end_ns".into()).into());
        }
        crate::naming::validate_output(&self.path)?;
        if std::path::absolute(output)? == Path::new(&self.path) {
            return Err(Usage("video and timeline must use different files".into()).into());
        }
        Ok(())
    }
}

/// One encoded presentation frame, in the order input reaches the encoder.
/// `video_time_ns` is the frame-count timestamp rounded down to nanoseconds;
/// the video's container can round it further to its own time base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrameRecord {
    pub frame_index: u64,
    pub video_time_ns: u64,
    pub frame_id: u64,
    pub received_monotonic_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_timestamp: Option<crate::types::DeviceTimestamp>,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_encoding: Option<crate::types::PixelEncoding>,
}

pub(super) struct Writer {
    config: Config,
    file: storage::Recording,
    frames: u64,
    rate: (u64, u64),
}
impl Writer {
    pub(super) fn path(&self) -> &Path {
        self.file.path()
    }
    pub(super) fn create(config: &Config, fps: f64, min_free: Option<u64>) -> Result<Self> {
        let text = fps.to_string();
        let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
        let denominator = 10_u64
            .checked_pow(u32::try_from(fraction.len())?)
            .context("video frame rate exceeds rational range")?;
        let fractional = if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u64>()?
        };
        let numerator = whole
            .parse::<u64>()?
            .checked_mul(denominator)
            .and_then(|whole| whole.checked_add(fractional))
            .filter(|&rate| rate > 0)
            .context("invalid video frame rate")?;
        let file = storage::Recording::create(Path::new(&config.path), None, min_free)?;
        Ok(Self {
            config: config.clone(),
            file,
            frames: 0,
            rate: (numerator, denominator),
        })
    }
    pub(super) fn accepts(&self, frame: &Frame) -> Result<bool> {
        let observed = frame.received_monotonic_ns.ok_or_else(|| {
            Categorized::error(
                ErrorKind::Incomplete,
                "indexed video frame has no host receive timestamp",
            )
        })?;
        Ok(observed <= self.config.acquisition_end_ns)
    }
    /// Called only after the complete input frame was written successfully.
    pub(super) fn wrote(&mut self, frame: &Frame) -> Result<()> {
        let received = frame.received_monotonic_ns.ok_or_else(|| {
            Categorized::error(
                ErrorKind::Incomplete,
                "indexed video frame has no host receive timestamp",
            )
        })?;
        let time = u128::from(self.frames)
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_mul(u128::from(self.rate.1)))
            .map(|value| value / u128::from(self.rate.0))
            .and_then(|value| u64::try_from(value).ok())
            .context("video timeline timestamp exceeds range")?;
        let record = FrameRecord {
            frame_index: self.frames,
            video_time_ns: time,
            frame_id: frame.id,
            received_monotonic_ns: received,
            device_ts_ns: frame.device_ts_ns,
            device_timestamp: frame.device_timestamp,
            width: frame.width,
            height: frame.height,
            pixel_format: frame.pixel_format,
            pixel_encoding: frame.pixel_encoding,
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        let written = self.file.write_all(&bytes);
        if let Some(error) = self.file.alarm().error() {
            return Err(error);
        }
        written.context("write video timeline")?;
        self.frames = self
            .frames
            .checked_add(1)
            .context("video frame counter exhausted")?;
        Ok(())
    }
    pub(super) fn finish(self) -> Result<()> {
        self.file.finish().map(|_| ())
    }
}

/// Original camera payloads, indexed by successful encoder-input frame. The
/// pair is created lazily: ordinary cameras produce no chunk artifacts.
pub(super) struct ChunkWriter {
    output: PathBuf,
    min_free: Option<u64>,
    files: Option<(storage::Recording, storage::Recording)>,
    frames: u64,
    chunk_frames: u64,
    complete: bool,
}
impl ChunkWriter {
    pub(super) const fn new(output: PathBuf, min_free: Option<u64>) -> Self {
        Self {
            output,
            min_free,
            files: None,
            frames: 0,
            chunk_frames: 0,
            complete: true,
        }
    }
    pub(super) fn wrote(&mut self, frame: &Frame) -> Result<()> {
        let result = self.append(frame);
        if result.is_err() {
            self.complete = false;
        }
        result
    }
    fn append(&mut self, frame: &Frame) -> Result<()> {
        let index = self.frames;
        self.frames = self
            .frames
            .checked_add(1)
            .context("video chunk frame counter exhausted")?;
        let Some(chunks) = &frame.chunks else {
            return Ok(());
        };
        ensure!(
            chunks.payload.len() == chunks.payload_bytes,
            Categorized::error(
                ErrorKind::Incomplete,
                "video chunk payload size does not match its metadata"
            )
        );
        if self.files.is_none() {
            let suffix = |extension: &str| {
                let mut path = self.output.as_os_str().to_os_string();
                path.push(extension);
                PathBuf::from(path)
            };
            let payload = storage::Recording::create(&suffix(".chunks.raw"), None, self.min_free)?;
            let index =
                match storage::Recording::create(&suffix(".chunks.jsonl"), None, self.min_free) {
                    Ok(index) => index,
                    Err(error) => {
                        payload
                            .abort()
                            .context("remove unused video chunk payload")?;
                        return Err(error);
                    }
                };
            self.files = Some((payload, index));
        }
        let (payload, journal) = self
            .files
            .as_mut()
            .context("video chunk artifacts missing")?;
        let offset = payload.bytes_written();
        let mut record = serde_json::to_value(chunks.as_ref())?;
        record["format_version"] = 1.into();
        record["frame_index"] = index.into();
        record["frame_id"] = frame.id.into();
        record["wall_ms"] = frame.wall_ms.into();
        record["received_monotonic_ns"] = serde_json::json!(frame.received_monotonic_ns);
        record["device_ts_ns"] = serde_json::json!(frame.device_ts_ns);
        record["device_timestamp"] = serde_json::json!(frame.device_timestamp);
        record["width"] = frame.width.into();
        record["height"] = frame.height.into();
        if let Some(encoding) = frame.pixel_encoding {
            record["pixel_encoding"] = serde_json::json!(encoding);
        } else {
            record["pfnc_code"] = frame.pixel_format.into();
        }
        record["payload_offset"] = offset.into();
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        // An index entry only commits a complete payload. A failed final write
        // can leave an unindexed suffix, retained for diagnosis and recovery.
        let written = payload.write_all(&chunks.payload);
        if let Some(error) = payload.alarm().error() {
            return Err(error);
        }
        written.context("write video chunk payload")?;
        let written = journal.write_all(&line);
        if let Some(error) = journal.alarm().error() {
            return Err(error);
        }
        written.context("write video chunk index")?;
        self.chunk_frames = self
            .chunk_frames
            .checked_add(1)
            .context("video chunk counter exhausted")?;
        Ok(())
    }
    pub(super) fn finish(self) -> Result<Option<serde_json::Value>> {
        let Some((payload, index)) = self.files else {
            return Ok(None);
        };
        let payload_path = payload.path().display().to_string();
        let path = index.path().display().to_string();
        // Try to sync both even when one fails. A failed sync is reported by
        // the writer queue before a recording can be reported as successful.
        let payload_finished = payload.finish();
        let index_finished = index.finish();
        Ok(Some(serde_json::json!({
            "format_version": 1,
            "path": path,
            "payload_path": payload_path,
            "bytes": index_finished?,
            "payload_bytes": payload_finished?,
            "frames": self.frames,
            "chunk_frames": self.chunk_frames,
            "complete": self.complete,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::{ChunkWriter, Config, FrameRecord, Writer};
    use crate::types::{Frame, MONO8};
    use std::{fs, path::PathBuf};

    fn config() -> (PathBuf, Config) {
        let directory = std::env::temp_dir().join(format!(
            "capturefab-video-timeline-{}",
            crate::types::random_hex(8).unwrap()
        ));
        fs::create_dir_all(&directory).unwrap();
        let config = Config {
            path: directory.join("frames.jsonl").display().to_string(),
            acquisition_end_ns: 300,
        };
        (directory, config)
    }
    #[test]
    fn count_timestamps_are_exact_for_fractional_rates_and_cutoff_is_checked() {
        let (directory, config) = config();
        let mut writer = Writer::create(&config, 29.97, None).unwrap();
        let mut frame = Frame {
            pixel_encoding: None,
            id: 5,
            width: 2,
            height: 1,
            pixel_format: MONO8,
            received_monotonic_ns: Some(100),
            ..Frame::default()
        };
        assert!(writer.accepts(&frame).unwrap());
        writer.wrote(&frame).unwrap();
        frame.id = 8;
        frame.received_monotonic_ns = Some(200);
        writer.wrote(&frame).unwrap();
        frame.received_monotonic_ns = Some(301);
        assert!(!writer.accepts(&frame).unwrap());
        writer.finish().unwrap();
        let records = fs::read_to_string(&config.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<FrameRecord>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].video_time_ns, 0);
        assert_eq!(records[1].video_time_ns, 33_366_700);
        assert_eq!(records[1].frame_id, 8);
        assert_eq!(records[1].received_monotonic_ns, 200);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn timeline_never_replaces_a_file_and_rejects_missing_receive_clock() {
        let (directory, config) = config();
        fs::write(&config.path, b"keep").unwrap();
        Writer::create(&config, 30.0, None).err().unwrap();
        assert_eq!(fs::read(&config.path).unwrap(), b"keep");
        fs::remove_file(&config.path).unwrap();
        let writer = Writer::create(&config, 30.0, None).unwrap();
        writer.accepts(&Frame::default()).unwrap_err();
        writer.finish().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    fn chunk_frame(id: u64) -> Frame {
        let mut payload = vec![11, 22, 33, 44];
        payload.extend([7, 8, 9]);
        payload.extend(0xf123_4567u32.to_le_bytes());
        payload.extend(3u32.to_le_bytes());
        let mut chunks = crate::transport::chunks::ChunkData::parse(
            &payload,
            4,
            4,
            crate::transport::chunks::ByteOrder::Little,
            45,
        )
        .unwrap();
        chunks.payload = payload.into();
        chunks.values = Some(serde_json::json!({
            "values": {"ChunkCounter": u64::MAX},
            "units": {"ChunkCounter": "count"},
            "errors": {}, "truncated": false, "context_reads": 0, "context_bytes": 0
        }));
        Frame {
            pixel_encoding: None,
            id,
            width: 4,
            height: 1,
            pixel_format: MONO8,
            data: vec![11, 22, 33, 44].into(),
            chunks: Some(chunks.into()),
            device_ts_ns: Some(100 + id),
            device_timestamp: Some(crate::types::DeviceTimestamp {
                ticks: u64::MAX - id,
                frequency_hz: Some(125_000_000),
            }),
            received_monotonic_ns: Some(200 + id),
            wall_ms: 300 + id,
            ..Frame::default()
        }
    }

    #[test]
    fn video_chunks_are_lazy_and_index_successful_input_frames() {
        let (directory, _) = config();
        let video = directory.join("video.mkv");
        let mut no_chunks = ChunkWriter::new(video.clone(), None);
        no_chunks.wrote(&Frame::default()).unwrap();
        assert!(no_chunks.finish().unwrap().is_none());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        let mut writer = ChunkWriter::new(video, None);
        writer.wrote(&Frame::default()).unwrap();
        let frame = chunk_frame(7);
        let payload = frame.chunks.as_ref().unwrap().payload.to_vec();
        writer.wrote(&frame).unwrap();
        writer.wrote(&Frame::default()).unwrap();
        writer.wrote(&chunk_frame(9)).unwrap();
        let summary = writer.finish().unwrap().unwrap();
        assert_eq!(summary["frames"], 4);
        assert_eq!(summary["chunk_frames"], 2);
        assert_eq!(summary["complete"], true);
        let records = fs::read_to_string(summary["path"].as_str().unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(
            (
                records[0]["frame_index"].as_u64(),
                records[1]["frame_index"].as_u64()
            ),
            (Some(1), Some(3))
        );
        assert_eq!(records[0]["frame_id"], 7);
        assert_eq!(records[0]["received_monotonic_ns"], 207);
        assert_eq!(records[0]["device_timestamp"]["ticks"], u64::MAX - 7);
        assert_eq!(records[0]["device_timestamp"]["frequency_hz"], 125_000_000);
        assert_eq!(records[0]["values"]["values"]["ChunkCounter"], u64::MAX);
        assert_eq!(records[0]["values"]["units"]["ChunkCounter"], "count");
        assert_eq!(records[1]["payload_offset"], payload.len());
        assert_eq!(records[1]["entries"][0]["id"], 0xf123_4567u32);
        let bytes = fs::read(summary["payload_path"].as_str().unwrap()).unwrap();
        assert_eq!(bytes, [payload.clone(), payload].concat());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn video_chunks_preserve_existing_artifacts_and_reject_missing_bytes() {
        let (directory, _) = config();
        let video = directory.join("video.mkv");
        let index = directory.join("video.mkv.chunks.jsonl");
        fs::write(&index, b"keep").unwrap();
        let mut writer = ChunkWriter::new(video.clone(), None);
        writer.wrote(&chunk_frame(1)).unwrap_err();
        assert_eq!(fs::read(&index).unwrap(), b"keep");
        assert!(!directory.join("video.mkv.chunks.raw").exists());
        assert!(writer.finish().unwrap().is_none());
        fs::remove_file(index).unwrap();
        let mut frame = chunk_frame(1);
        std::sync::Arc::make_mut(frame.chunks.as_mut().unwrap()).payload =
            crate::framebuf::FrameBuf::default();
        let mut writer = ChunkWriter::new(video, None);
        writer.wrote(&frame).unwrap_err();
        assert!(writer.finish().unwrap().is_none());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir_all(directory).unwrap();
    }
}
