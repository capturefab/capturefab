//! Full-resolution stills through the existing isolated camera worker.
use super::{Reading, Source};
use crate::{
    session::SessionCommand,
    transport::multipart::GvspPartFormat,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub camera: String,
    pub output: String,
    #[serde(default)]
    pub part_format: Option<GvspPartFormat>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub part_index: Option<u32>,
    #[serde(default = "default_signal")]
    pub signal: String,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub quality: Option<u8>,
    #[serde(default, deserialize_with = "crate::config_number::unsigned")]
    pub warmup_ms: u64,
    #[serde(default)]
    pub settings: Vec<String>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub action_device_key: Option<u32>,
}
impl Config {
    fn schedule(&self, format: &str, timeout_ms: u64, min_free: Option<u64>) -> SessionCommand {
        SessionCommand::Schedule {
            output: self.output.clone(),
            count: 1,
            timeout_ms,
            format: format.into(),
            part_format: self.part_format,
            part_index: self.part_index,
            first_at_ms: 0,
            interval_ms: 0,
            on_exists: crate::naming::OnExists::Fail,
            manifest: None,
            sync: crate::storage::SyncMode::File,
            min_free,
            context: BTreeMap::new(),
            utc_offset_s: Some(0),
            quality: self.quality,
            fresh: 0,
            warmup_ms: Some(self.warmup_ms),
            inspect: false,
            job: None,
            lease: true,
        }
    }
}
fn default_signal() -> String {
    "frame".into()
}

pub(crate) fn open(
    config: &Config,
    cameras: &mut super::camera_session::Cameras,
) -> Result<Box<dyn Source>> {
    crate::camera::validate_selector(&config.camera)?;
    GvspPartFormat::validate_index(config.part_index, config.part_format)
        .map_err(crate::cli::input)?;
    if let Some(format) = config.part_format {
        format
            .validate_binding(config.part_index, &config.camera)
            .map_err(crate::cli::input)?;
    }
    super::validate_name(&config.signal, "camera signal")?;
    crate::naming::validate_output(&config.output)?;
    if config.output == "-"
        || config.output.contains("://")
        || config.warmup_ms > 30_000
        || config
            .quality
            .is_some_and(|quality| !(1..=100).contains(&quality))
    {
        return Err(Usage(
            "camera sources require a local image output, quality 1–100 and warmup_ms 0–30000"
                .into(),
        )
        .into());
    }
    let extension = Path::new(&config.output)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let format = match extension.as_str() {
        "jpg" | "jpeg" => "jpeg",
        "png" => "png",
        "raw" => "raw",
        _ => return Err(Usage(
            "camera output must end in .jpg, .jpeg, .png or .raw; use {seq} for repeated captures"
                .into(),
        )
        .into()),
    };
    let mut config = config.clone();
    crate::cli::paths::resolve_path(&mut config.output, &std::env::current_dir()?)?;
    if config.part_format.is_some() {
        // Check the selected output before the recording opens. The worker
        // verifies the connected transport when it receives the schedule.
        config
            .schedule(format, 0, None)
            .validate()
            .map_err(crate::cli::input)?;
    }
    let settings = config
        .settings
        .iter()
        .map(|value| crate::controls::Assignment::parse(value))
        .collect::<Result<Vec<_>>>()?;
    if settings.len() > crate::session::MAX_SET_VALUES {
        return Err(Usage("camera settings exceeds 32 assignments".into()).into());
    }
    if crate::host::monotonic_ns().is_none() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "joint camera acquisition needs a host-wide monotonic clock",
        ));
    }
    let session = cameras.get(&config.camera, &settings, config.action_device_key)?;
    Ok(Box::new(Camera {
        config,
        format,
        session,
        connected: false,
        observed: None,
        acquisition_end: None,
        pending: None,
        drain_file: None,
        draining: false,
    }))
}
struct Camera {
    config: Config,
    format: &'static str,
    session: Arc<super::camera_session::Shared>,
    connected: bool,
    observed: Option<u64>,
    acquisition_end: Option<u64>,
    pending: Option<Arc<crate::session::SavedCapture>>,
    drain_file: Option<crate::engine::saved::SavedFile>,
    draining: bool,
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "camera source read deadline expired",
        ));
    }
    Ok(left)
}
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}
impl Source for Camera {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        self.draining = false;
        let deadline = Instant::now() + timeout;
        if !self.connected {
            self.session.initialize(deadline)?;
            self.connected = true;
        }
        let (reply, saved) = self.session.capture(
            self.config.schedule(
                self.format,
                millis(remaining(deadline)?),
                crate::storage::min_free()?,
            ),
            deadline,
            &mut self.pending,
        )?;
        let job = reply["job"]["id"]
            .as_str()
            .context("camera capture did not create a job")?
            .to_owned();
        // Stop acquiring at the read deadline, then let an already received frame
        // finish encoding/syncing. A timed-out delivery is an explicit failure,
        // never a successful end-of-run idle timeout.
        let delivery_deadline = deadline + Duration::from_secs(5);
        let mut seq = 0;
        loop {
            let reply = self.session.call(
                SessionCommand::Wait {
                    job: job.clone(),
                    after_seq: seq,
                    timeout_ms: millis(remaining(delivery_deadline)?).min(25_000),
                },
                delivery_deadline,
            )?;
            seq = reply["seq"].as_u64().unwrap_or(seq);
            let state = reply["job"]["state"].as_str().unwrap_or_default();
            if state == "done" {
                break;
            }
            if !matches!(state, "armed" | "running" | "stopping") {
                return Err(crate::session::protocol::from_wire(
                    &reply["detail"]["error"],
                ));
            }
        }
        let file = saved.take()?;
        self.pending = None;
        Ok(vec![self.reading(&file)?])
    }
    fn ready(&self) -> bool {
        self.observed.is_some()
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn buffered_timestamps(&self) -> bool {
        self.draining
    }
    fn end_timeout_is_idle(&self) -> bool {
        false
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.acquisition_end = end_ns;
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        let Some(pending) = &self.pending else {
            return Ok(());
        };
        let deadline = Instant::now() + timeout.min(Duration::from_secs(5));
        loop {
            if let Some(file) = pending.try_take() {
                self.drain_file = Some(file?);
                self.pending = None;
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(5)));
        }
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        let file = self.drain_file.take().map_or_else(
            || {
                self.pending
                    .as_ref()
                    .and_then(|saved| saved.try_take())
                    .transpose()
            },
            |file| Ok(Some(file)),
        )?;
        let Some(file) = file else {
            return Ok(None);
        };
        self.pending = None;
        self.draining = true;
        self.reading(&file).map(Some)
    }
}
impl Camera {
    fn reading(&mut self, file: &crate::engine::saved::SavedFile) -> Result<Reading> {
        if self
            .acquisition_end
            .zip(file.received_monotonic_ns)
            .is_some_and(|(end, observed)| observed > end)
        {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "camera frame arrived after the acquisition cutoff",
            )
            .details(json!({"late_file":file.path}))
            .into());
        }
        self.observed = file.received_monotonic_ns;
        if self.observed.is_none() {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "camera worker did not supply its host receive clock",
            ));
        }
        let crate::engine::saved::SavedItem::Capture {
            pixel_encoding,
            frame_id,
            device_ts_ns,
            width,
            height,
            pixel_format,
            passthrough,
        } = file.item
        else {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "camera source received unexpected video metadata",
            ));
        };
        let (bit_depth, source_bits) =
            crate::frame::bit_depth_with_encoding(pixel_format, pixel_encoding, self.format);
        let sidecar_path =
            (self.format == "raw").then(|| crate::naming::sidecar(Path::new(&file.path)));
        Ok(Reading {
            signal: self.config.signal.clone(),
            source_time: None,
            value: crate::types::without_nulls(json!({
                "path":file.path,"bytes":file.bytes,"sha256":file.sha256,"chunks":file.chunks,"multipart":file.multipart,"frame_id":frame_id,"device_ts_ns":device_ts_ns,"device_timestamp":file.device_timestamp,
                "width":width,"height":height,"pixel_format":crate::inspect::pixel_format_label_with_encoding(pixel_format, pixel_encoding),"pfnc_code":pixel_encoding.is_none().then_some(pixel_format),"pixel_encoding":pixel_encoding,
                "bit_depth":bit_depth,"source_bits":source_bits,"sidecar_path":sidecar_path,"passthrough":passthrough,
                "received_at":crate::scheduling::rfc3339(file.wall_ms)
            })),
        })
    }
}
