//! Indexed video and its finalized-file metadata on the industrial timeline.
use super::{Reading, Source};
use crate::{
    media::{
        ForwardConfig, StreamCopy,
        timeline::{Config as TimelineConfig, FrameRecord},
    },
    session::{ForwardObservation, SessionCommand},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead as _, BufReader, Read as _},
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_INDEX_LINE: usize = 4096;
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub camera: String,
    pub output: String,
    #[serde(default)]
    pub index: Option<String>,
    #[serde(default = "default_fps")]
    pub fps: f64,
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_encoder")]
    pub encoder: String,
    #[serde(default = "default_bitrate")]
    pub bitrate: String,
    #[serde(default)]
    pub settings: Vec<String>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub action_device_key: Option<u32>,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub warmup_ms: u64,
    #[serde(default = "default_signal")]
    pub signal: String,
}
const fn default_fps() -> f64 {
    30.0
}
fn default_codec() -> String {
    "h264".into()
}
fn default_encoder() -> String {
    "auto".into()
}
fn default_bitrate() -> String {
    "8M".into()
}
fn default_signal() -> String {
    "frame".into()
}

pub(crate) fn open(
    config: &Config,
    cameras: &mut super::camera_session::Cameras,
) -> Result<Box<dyn Source>> {
    crate::camera::validate_selector(&config.camera)?;
    super::validate_name(&config.signal, "video signal")?;
    if config.signal == "recording" {
        return Err(
            Usage("video signal 'recording' is reserved for final file metadata".into()).into(),
        );
    }
    if config.warmup_ms > 30_000
        || config.output.contains("://")
        || !crate::media::is_recording(&config.output)
        || crate::naming::has_placeholder(&config.output)
    {
        return Err(Usage("video sources need a literal local .mkv/.mp4/.mov/.ts/.webm output and warmup_ms 0–30000".into()).into());
    }
    let mut config = config.clone();
    let cwd = std::env::current_dir()?;
    crate::cli::paths::resolve_path(&mut config.output, &cwd)?;
    let mut index = config
        .index
        .clone()
        .unwrap_or_else(|| format!("{}.frames.jsonl", config.output));
    crate::cli::paths::resolve_path(&mut index, &cwd)?;
    TimelineConfig {
        path: index.clone(),
        acquisition_end_ns: 1,
    }
    .validate(&config.output)?;
    let settings = config
        .settings
        .iter()
        .map(|setting| crate::controls::Assignment::parse(setting))
        .collect::<Result<Vec<_>>>()?;
    if settings.len() > crate::session::MAX_SET_VALUES {
        return Err(Usage("video settings exceeds 32 assignments".into()).into());
    }
    let forwarding = ForwardConfig {
        output: config.output.clone(),
        codec: config.codec.clone(),
        encoder: config.encoder.clone(),
        fps: config.fps,
        bitrate: config.bitrate.clone(),
        max_file_bytes: None,
        min_free: crate::storage::min_free()?,
        timeline: None,
    };
    forwarding
        .validate()
        .map_err(|error| Usage(format!("{error:#}")))?;
    crate::engine::preflight_recording_output(&config.output, crate::naming::OnExists::Fail)?;
    if std::fs::symlink_metadata(&index).is_ok() {
        return Err(crate::storage::OutputExists(index.into()).into());
    }
    if crate::host::monotonic_ns().is_none() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "indexed video needs a host-wide monotonic clock",
        ));
    }
    // Encoder discovery happens before the shared acquisition clock starts.
    crate::media::check_codec(&forwarding)?;
    let session = cameras.get(&config.camera, &settings, config.action_device_key)?;
    Ok(Box::new(Video {
        config,
        index,
        session,
        job: None,
        forwarding_id: None,
        forwarding: None,
        startup_failure: None,
        reader: None,
        index_failure: None,
        pending: Vec::new(),
        frames: 0,
        observed: None,
        acquisition_end: None,
        ended: None,
        finished: false,
        summary_sent: false,
    }))
}
struct Video {
    config: Config,
    index: String,
    session: Arc<super::camera_session::Shared>,
    job: Option<String>,
    forwarding_id: Option<String>,
    forwarding: Option<Arc<ForwardObservation>>,
    startup_failure: Option<Arc<anyhow::Error>>,
    reader: Option<BufReader<File>>,
    index_failure: Option<Arc<anyhow::Error>>,
    pending: Vec<u8>,
    frames: u64,
    observed: Option<u64>,
    acquisition_end: Option<u64>,
    ended: Option<Value>,
    finished: bool,
    summary_sent: bool,
}
#[derive(Debug)]
struct RetainedFailure(&'static str, Arc<anyhow::Error>);
impl std::fmt::Display for RetainedFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for RetainedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.1.as_ref().as_ref())
    }
}
impl Video {
    fn start(&mut self, deadline: Instant) -> Result<()> {
        let acquisition_end_ns = self
            .acquisition_end
            .context("video has no acquisition cutoff")?;
        self.session.initialize(deadline)?;
        let reply = self.session.forward(
            SessionCommand::Forward {
                output: self.config.output.clone(),
                codec: self.config.codec.clone(),
                encoder: self.config.encoder.clone(),
                fps: self.config.fps,
                bitrate: self.config.bitrate.clone(),
                max_file_bytes: None,
                min_free: crate::storage::min_free()?,
                on_exists: crate::naming::OnExists::Fail,
                context: BTreeMap::new(),
                duration_ms: None,
                utc_offset_s: Some(0),
                warmup_ms: Some(self.config.warmup_ms),
                job: None,
                lease: false,
                copy: StreamCopy::Never,
                timeline: Some(TimelineConfig {
                    path: self.index.clone(),
                    acquisition_end_ns,
                }),
            },
            deadline,
            &mut self.forwarding,
        )?;
        if !self.attach_index()? {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "video startup returned without index ownership",
            ));
        }
        if reply["job"]["id"].as_str() != self.job.as_deref() {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "video startup reply differs from its owned job",
            ));
        }
        Ok(())
    }

    fn attach_index(&mut self) -> Result<bool> {
        let Some(observed) = &self.forwarding else {
            return Ok(false);
        };
        let Some(owned) = observed.owned()? else {
            return Ok(false);
        };
        if owned.index_path != self.index
            || owned.output_path != self.config.output
            || self.job.as_ref().is_some_and(|job| *job != owned.job)
            || self
                .forwarding_id
                .as_ref()
                .is_some_and(|id| *id != owned.forwarding_id)
        {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "video ownership receipt differs from its configured job or paths",
            ));
        }
        if self.reader.is_none() {
            self.reader = Some(BufReader::new(
                File::open(&owned.index_path).context("owned video frame index is unavailable")?,
            ));
        }
        self.job = Some(owned.job);
        self.forwarding_id = Some(owned.forwarding_id);
        Ok(true)
    }

    fn poll_job(&mut self) -> Result<()> {
        if self.ended.is_some() {
            return Ok(());
        }
        let observed = self
            .forwarding
            .as_ref()
            .context("video startup observation is missing")?;
        if let Some(ended) = observed.poll_end()? {
            let error = ended["detail"]["error"]
                .is_object()
                .then(|| crate::session::protocol::from_wire(&ended["detail"]["error"]));
            self.ended = Some(ended);
            if let Some(error) = error {
                return Err(error);
            }
        }
        Ok(())
    }
    fn next_frame(&mut self) -> Result<Option<Reading>> {
        if let Some(error) = &self.index_failure {
            return Err(RetainedFailure("video frame index failed", error.clone()).into());
        }
        match self.read_frame() {
            Ok(value) => Ok(value),
            Err(error) => {
                let error = Arc::new(error);
                self.index_failure = Some(error.clone());
                Err(RetainedFailure("video frame index failed", error).into())
            }
        }
    }

    fn read_frame(&mut self) -> Result<Option<Reading>> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        loop {
            let room = MAX_INDEX_LINE + 1 - self.pending.len();
            if room == 0 {
                return Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "video index row exceeds 4096 bytes",
                ));
            }
            let count = reader
                .take(room as u64)
                .read_until(b'\n', &mut self.pending)?;
            if count == 0 {
                if self.finished && !self.pending.is_empty() {
                    return Err(Categorized::error(
                        ErrorKind::Incomplete,
                        "video index ended with an unfinished row",
                    ));
                }
                return Ok(None);
            }
            if self.pending.len() > MAX_INDEX_LINE {
                return Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "video index row exceeds 4096 bytes",
                ));
            }
            if self.pending.last() != Some(&b'\n') {
                continue;
            }
            let frame: FrameRecord =
                serde_json::from_slice(&self.pending).context("invalid video frame index row")?;
            self.pending.clear();
            if frame.frame_index != self.frames
                || self
                    .observed
                    .is_some_and(|previous| frame.received_monotonic_ns < previous)
                || self
                    .acquisition_end
                    .is_some_and(|end| frame.received_monotonic_ns > end)
            {
                return Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "video frame index has an invalid sequence or receive time",
                ));
            }
            self.frames += 1;
            self.observed = Some(frame.received_monotonic_ns);
            return Ok(Some(Reading {
                signal: self.config.signal.clone(),
                source_time: None,
                value: crate::types::without_nulls(
                    json!({"kind":"video_frame","path":self.config.output,"index_path":self.index,"frame_index":frame.frame_index,"video_time_ns":frame.video_time_ns,"frame_id":frame.frame_id,"device_ts_ns":frame.device_ts_ns,"device_timestamp":frame.device_timestamp,"width":frame.width,"height":frame.height,"pixel_format":crate::inspect::pixel_format_label_with_encoding(frame.pixel_format, frame.pixel_encoding),"pixel_encoding":frame.pixel_encoding }),
                ),
            }));
        }
    }
}
impl Source for Video {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now() + timeout;
        if let Some(error) = &self.startup_failure {
            return Err(RetainedFailure("industrial video startup failed", error.clone()).into());
        }
        if let Some(error) = &self.index_failure {
            return Err(RetainedFailure("video frame index failed", error.clone()).into());
        }
        if self.forwarding.is_none()
            && let Err(error) = self.start(deadline)
        {
            let error = Arc::new(error);
            self.startup_failure = Some(error.clone());
            return Err(RetainedFailure("industrial video startup failed", error).into());
        }
        loop {
            self.poll_job()?;
            if let Some(frame) = self.next_frame()? {
                return Ok(vec![frame]);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Vec::new());
            }
            std::thread::sleep(left.min(Duration::from_millis(5)));
        }
    }
    fn ready(&self) -> bool {
        self.frames > 0
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn buffered_timestamps(&self) -> bool {
        true
    }
    fn end_timeout_is_idle(&self) -> bool {
        false
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.acquisition_end = end_ns;
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let settled = self.forwarding.as_ref().map_or_else(
            || Ok(self.ended.clone()),
            |observed| observed.stop_and_wait(timeout.min(Duration::from_secs(30))),
        );
        self.finished = true;
        // A later protocol error invalidates a previously cached end, while
        // the already-owned reader remains available for valid-prefix drain.
        self.ended = settled.as_ref().map_or(None, Clone::clone);
        // A late ownership event remains useful even when startup or stop failed.
        // Retain its reader so drain can publish only the proven complete prefix.
        let owned = self.attach_index();
        settled?;
        owned?;
        if let Some(ended) = &self.ended {
            if ended["detail"]["error"].is_object() {
                return Err(crate::session::protocol::from_wire(
                    &ended["detail"]["error"],
                ));
            }
            if ended["detail"]["finalized"] != true {
                return Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "video or frame index did not finalize",
                ));
            }
        } else if self.reader.is_some() {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "owned video did not return an actual final outcome",
            ));
        }
        Ok(())
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        if let Some(frame) = self.next_frame()? {
            return Ok(Some(frame));
        }
        if self.summary_sent || self.job.is_none() {
            return Ok(None);
        }
        self.summary_sent = true;
        let detail = &self
            .ended
            .as_ref()
            .context("video did not return a final job outcome")?["detail"];
        let written = detail["written"]
            .as_u64()
            .context("video outcome did not report written frames")?;
        if written != self.frames {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "video frame count differs from its index",
            )
            .details(json!({"index_path":self.index,"indexed":self.frames,"written":written}))
            .into());
        }
        if detail["finalized"] != true {
            return Ok(None);
        }
        self.observed = None;
        Ok(Some(Reading {
            signal: "recording".into(),
            source_time: None,
            value: crate::types::without_nulls(
                json!({"kind":"video_recording","path":self.config.output,"index_path":self.index,"frames":written,"fps":self.config.fps,"codec":detail["codec"],"encoder":detail["encoder"],"accepted":detail["accepted"],"dropped":detail["dropped"],"bytes":detail["recording"]["bytes"],"chunks":detail["recording"]["chunks"],"finalized":true}),
            ),
        }))
    }
}

#[cfg(test)]
mod tests;
