//! The session coordinator: one isolated worker process per camera, which
//! it starts, follows through its replies and events, reconnects after a
//! crash and ends with everything it started; the session's jobs; and the
//! public command set IPC clients send.
use crate::types::{CameraInfo, Categorized, ErrorKind};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, PoisonError, RwLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

pub(crate) mod jobs;
pub(crate) mod protocol;
mod run;
mod worker;
use jobs::{JobBook, JobKind};
use protocol::WorkerCommand;
pub use run::run_worker;
#[cfg(jpeg_helper)]
pub(crate) use worker::executable;
use worker::{Signal, Worker};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionCommand {
    /// Inspect all GigE paths; with `tune`, apply bandwidth sharing to the
    /// connected cameras together.
    NetworkScan {
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tune: bool,
    },
    Connect {
        camera: String,
        timeout_ms: u64,
        /// A camera sign-in sent beside a credential-free selector, over local
        /// IPC and the worker pipe.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login: Option<crate::logins::CameraLogin>,
        /// Whether the worker may use a sign-in saved on this host when no
        /// `login` is sent.
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        saved_login: bool,
    },
    Disconnect,
    /// Read features, banks and canonical controls in one round trip.
    GetValues {
        names: Vec<crate::controls::Name>,
    },
    /// Write features, banks and canonical controls as one ordered change:
    /// a failure restores every value written. With `dry_run`, only check
    /// and plan.
    SetValues {
        values: Vec<crate::controls::Assignment>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dry_run: bool,
    },
    /// Run a command feature, at a bank.
    Exec {
        name: crate::controls::Name,
    },
    /// What an agent needs to know before capturing.
    Info,
    /// Features with their type, access, bounds, selectors and locks.
    FeatureList {
        #[serde(default)]
        filter: crate::controls::FeatureFilter,
    },
    Start,
    Stop,
    /// Start acquisition, as before a capture of several cameras together,
    /// warming up a camera that sets its own exposure; the stream runs on
    /// until stopped.
    Arm {
        /// The longest warm-up, in milliseconds; 2 s when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warmup_ms: Option<u64>,
        /// A GigE camera's share of the host interface it streams on beside
        /// the others armed with it, which it is paced to before it starts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes_per_second: Option<u64>,
    },
    /// Read the camera's writable configuration values for a new saved
    /// setup. Read-only: selectors are switched only to read and restored.
    ReadSetup,
    /// Compare a saved setup with the camera, by name (resolved by the
    /// coordinator from the setup store) or inline. Read-only.
    SetupDiff {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::setup::deserialize_inline"
        )]
        setup: Option<crate::setup::Setup>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_other_model: bool,
    },
    /// Restore a saved setup as one camera change: a failure restores every
    /// value.
    ApplySetup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::setup::deserialize_inline"
        )]
        setup: Option<crate::setup::Setup>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_other_model: bool,
        /// Refuse, before any write, a setup with values this camera lacks,
        /// cannot write or would not accept.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        strict: bool,
    },
    Auto {
        /// Keep adjusting as a job of the session, until stopped.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        continuous: bool,
        /// Meter only this normalized x, y, width, height part of the frame.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<[f64; 4]>,
        /// The longest exposure, in microseconds, before gain is raised.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_exposure_us: Option<f64>,
        /// The job's number, which the coordinator assigns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<u64>,
    },
    /// Leave Capturefab's auto mode, the camera keeping the values it set.
    Manual,
    /// One frame, encoded to the client's stdout: by a worker of the
    /// client's own to its stdout, else through the private `stdout` file.
    /// Files are saved by `schedule`, one frame or more.
    Capture {
        timeout_ms: u64,
        format: String,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
        /// JPEG quality 1..100. Unset, a Motion JPEG source's own JPEG is
        /// sent as it is, and other frames are encoded at 90.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
        /// Frames to let pass after the first frame exposed after the
        /// camera's last settings change.
        #[serde(default, skip_serializing_if = "is_zero")]
        fresh: u32,
        /// How long a camera that sets its own exposure may take to settle
        /// after it starts, in milliseconds; 0 takes its frames as they
        /// come, unset waits up to 2 s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warmup_ms: Option<u64>,
        /// Through a session: the private file, in the runtime folder's
        /// `out`, the image goes to for the client to stream to its stdout
        /// and delete.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout: Option<String>,
    },
    Schedule {
        output: String,
        count: u32,
        timeout_ms: u64,
        format: String,
        first_at_ms: u64,
        interval_ms: u64,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// The absolute JSON Lines file each saved file's line is appended to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manifest: Option<String>,
        /// Whether each file reaches the disk before it is reported.
        #[serde(default, skip_serializing_if = "crate::storage::SyncMode::is_file")]
        sync: crate::storage::SyncMode,
        /// The client's own `CAPTUREFAB_MIN_FREE`, in bytes, when it set
        /// one: the files leave that much free on their disk as well as the
        /// worker's own floor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_free: Option<u64>,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
        /// The caller's UTC offset in seconds, for {date} and {time}.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utc_offset_s: Option<i32>,
        /// JPEG quality 1..100. Unset, a Motion JPEG source's own JPEG is
        /// stored as it is, and other frames are encoded at 90.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
        /// Frames to let pass after the first frame exposed after the
        /// camera's last settings change.
        #[serde(default, skip_serializing_if = "is_zero")]
        fresh: u32,
        /// How long a camera that sets its own exposure may take to settle
        /// after it starts, in milliseconds; 0 takes its frames as they
        /// come, unset waits up to 2 s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warmup_ms: Option<u64>,
        /// Measure the last frame on its encoder, and preview it, as
        /// inspect does.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        inspect: bool,
        /// The job's number, which the coordinator assigns so that jobs are
        /// numbered across a session's cameras.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<u64>,
        /// The client waits for the job: when no wait comes for a while, the
        /// session stops it, as its client is gone.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        lease: bool,
    },
    /// The session's jobs, ended ones included.
    Jobs,
    /// Wait until job `job` (`<session>-<n>`) changes after `after_seq`, or
    /// ends, for at most `timeout_ms` (25 s).
    Wait {
        job: String,
        #[serde(default)]
        after_seq: u64,
        timeout_ms: u64,
    },
    /// Stop a job and wait for it to end; files saved so far stay.
    StopJob {
        job: String,
    },
    Status,
    /// A frame of one camera, measured, with a bounded preview written to a
    /// file. Never saves captures or changes settings; a stopped camera
    /// acquires its frames and stays stopped.
    Frame {
        /// Frames to let pass after the request before the one measured; 0
        /// and 1 measure the next frame. Up to one more than a capture's
        /// `fresh`, which does not count the frame it saves.
        #[serde(default = "crate::inspect::default_fresh")]
        fresh: u32,
        #[serde(default = "crate::inspect::default_wait_ms")]
        timeout_ms: u64,
        /// The preview's longest edge in pixels; 0 returns measurements only.
        #[serde(default = "crate::inspect::default_edge")]
        max_dimension: u32,
        #[serde(default)]
        format: crate::inspect::PreviewFormat,
        /// A normalized x, y, width, height crop of the full-resolution
        /// frame, also the focus region.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<[f64; 4]>,
        /// The absolute file the preview is written to, which must not
        /// exist; the private inspect folder when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        /// The client's own `CAPTUREFAB_MIN_FREE`, in bytes, when it set
        /// one: the files leave that much free on their disk as well as the
        /// worker's own floor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_free: Option<u64>,
        /// How long a camera that sets its own exposure may take to settle
        /// after it starts, in milliseconds; 0 takes its frames as they
        /// come, unset waits up to 2 s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warmup_ms: Option<u64>,
    },
    Xml,
    ReadMemory {
        address: u64,
        length: usize,
    },
    WriteMemory {
        address: u64,
        data: Vec<u8>,
    },
    Forward {
        output: String,
        codec: String,
        encoder: String,
        fps: f64,
        bitrate: String,
        /// End the recording, as asked, at exactly this many bytes; none by
        /// default, when the free-space floor alone ends it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_file_bytes: Option<u64>,
        /// The client's own `CAPTUREFAB_MIN_FREE`, in bytes, when it set
        /// one: the files leave that much free on their disk as well as the
        /// worker's own floor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_free: Option<u64>,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
        /// Stop after this many milliseconds. The camera's worker ends the
        /// recording itself, as stopping its job would, so it ends on time
        /// even when the client that started it has gone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// The caller's UTC offset in seconds, for {date} and {time}.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utc_offset_s: Option<i32>,
        /// How long a camera that sets its own exposure may take to settle
        /// after it starts, in milliseconds, before the first frame is
        /// recorded; 0 records its frames as they come, unset waits up to
        /// 2 s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warmup_ms: Option<u64>,
        /// The job's number, which the coordinator assigns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<u64>,
        /// The client waits for the recording, as for a capture job.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        lease: bool,
        /// Whether a compressed source's own video is recorded as it
        /// arrives, without decoding or encoding it: when it is in `codec`
        /// (`same`), or in any codec a recording copies (`any`).
        #[serde(default, skip_serializing_if = "crate::media::StreamCopy::is_never")]
        copy: crate::media::StreamCopy,
    },
}
/// Most values one `SetValues` writes.
pub const MAX_SET_VALUES: usize = 32;
/// Most names one `GetValues` reads.
pub const MAX_NAMES: usize = 64;
/// Deserialize JSON control messages without accepting ignored fields on
/// Serde's internally tagged unit variants or duplicate object fields.
pub fn deserialize_command<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<SessionCommand, D::Error> {
    let StrictValue(value) = StrictValue::deserialize(deserializer)?;
    if let Value::Object(fields) = &value
        && fields.get("op").and_then(Value::as_str).is_some_and(|op| {
            matches!(
                op,
                "disconnect"
                    | "start"
                    | "stop"
                    | "jobs"
                    | "status"
                    | "xml"
                    | "read_setup"
                    | "info"
                    | "manual"
            )
        })
        && let Some(field) = fields.keys().find(|key| key.as_str() != "op")
    {
        return Err(serde::de::Error::custom(format!(
            "unknown field `{field}`; this operation only accepts `op`"
        )));
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictValue;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                value: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(value)))
            }
            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(value.into())))
            }
            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(value.into())))
            }
            fn visit_f64<E: serde::de::Error>(
                self,
                value: f64,
            ) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| StrictValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("JSON numbers must be finite"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(value.into())))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                value: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(value)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut values: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut result = Vec::new();
                while let Some(StrictValue(value)) = values.next_element()? {
                    result.push(value);
                }
                Ok(StrictValue(Value::Array(result)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut values: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut result = serde_json::Map::new();
                while let Some(key) = values.next_key::<String>()? {
                    if result.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                    }
                    let StrictValue(value) = values.next_value()?;
                    result.insert(key, value);
                }
                Ok(StrictValue(Value::Object(result)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
fn yes() -> bool {
    true
}
fn is_true(value: &bool) -> bool {
    *value
}
impl SessionCommand {
    /// A plain connect: no sign-in sent, saved sign-ins allowed.
    pub fn connect(camera: String, timeout_ms: u64) -> Self {
        Self::Connect {
            camera,
            timeout_ms,
            login: None,
            saved_login: true,
        }
    }
    pub fn from_json_slice(data: &[u8]) -> Result<Self> {
        let mut deserializer = serde_json::Deserializer::from_slice(data);
        let command = deserialize_command(&mut deserializer)?;
        deserializer.end()?;
        Ok(command)
    }
    /// Validate supplied values before queueing a command or changing a camera.
    /// Live feature bounds and output availability are checked again by
    /// their owners when the command executes.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::NetworkScan { timeout_ms, .. } => {
                validate_timeout(*timeout_ms)?;
            }
            Self::Connect {
                camera,
                timeout_ms,
                login,
                ..
            } => {
                validate_selector(camera)?;
                validate_timeout(*timeout_ms)?;
                if login.is_some() {
                    validate_login_selector(camera)?;
                }
                maximum_cameras()?;
            }
            Self::GetValues { names } => {
                if !(1..=MAX_NAMES).contains(&names.len()) {
                    return Err(crate::types::Usage(format!(
                        "get reads 1..{MAX_NAMES} names at once"
                    ))
                    .into());
                }
                for name in names {
                    name.validate()?;
                }
            }
            Self::SetValues { values, .. } => {
                if !(1..=MAX_SET_VALUES).contains(&values.len()) {
                    return Err(crate::types::Usage(format!(
                        "set writes 1..{MAX_SET_VALUES} values at once"
                    ))
                    .into());
                }
                for value in values {
                    value.validate()?;
                }
            }
            Self::Exec { name } => name.validate()?,
            Self::FeatureList { filter } => filter.validate()?,
            Self::Frame {
                fresh,
                timeout_ms,
                max_dimension,
                region,
                preview,
                warmup_ms,
                ..
            } => {
                // Its count takes in the frame measured, so it may be one
                // more than the frames a capture lets pass.
                crate::inspect::validate(
                    fresh.saturating_sub(1),
                    *timeout_ms,
                    *max_dimension,
                    *region,
                )?;
                validate_warmup(*warmup_ms)?;
                if let Some(preview) = preview {
                    ensure!(
                        std::path::Path::new(preview).is_absolute(),
                        "the preview file must be an absolute path"
                    );
                }
            }
            Self::SetupDiff { name, setup, .. } | Self::ApplySetup { name, setup, .. } => {
                ensure!(
                    name.is_some() != setup.is_some(),
                    "name a saved setup or pass one inline, not both"
                );
                if let Some(name) = name {
                    crate::setup::validate_name(name)?;
                }
                if let Some(setup) = setup {
                    crate::setup::validate(setup)?;
                }
            }
            Self::Auto {
                region,
                max_exposure_us,
                ..
            } => {
                ensure!(
                    max_exposure_us.is_none_or(|us| us.is_finite() && us > 0.0),
                    "the longest exposure must be a positive time"
                );
                crate::inspect::validate(0, 1, 0, *region)?;
            }
            Self::Capture {
                timeout_ms,
                format,
                context,
                quality,
                fresh,
                warmup_ms,
                stdout,
            } => {
                validate_frame_wait(*timeout_ms)?;
                validate_warmup(*warmup_ms)?;
                ensure!(
                    *fresh <= crate::inspect::MAX_FRESH,
                    "fresh frames must be 0..10"
                );
                ensure!(
                    ["png", "jpeg", "raw", "pgm", "ppm"].contains(&format.as_str()),
                    "format must be png, jpeg, raw, pgm or ppm"
                );
                crate::context::validate(context)?;
                ensure!(
                    quality.is_none_or(|quality| (1..=100).contains(&quality)),
                    "JPEG quality must be 1..100"
                );
                if let Some(file) = stdout {
                    stdout_file(file)?;
                }
            }
            Self::Schedule {
                output,
                count,
                timeout_ms,
                format,
                on_exists,
                context,
                utc_offset_s,
                fresh,
                warmup_ms,
                ..
            } => {
                validate_frame_wait(*timeout_ms)?;
                validate_warmup(*warmup_ms)?;
                ensure!(
                    *fresh <= crate::inspect::MAX_FRESH,
                    "fresh frames must be 0..10"
                );
                crate::naming::validate_offset(*utc_offset_s)?;
                ensure!((1..=100_000).contains(count), "count must be 1..100000");
                ensure!(
                    ["png", "jpeg", "raw", "pgm", "ppm"].contains(&format.as_str()),
                    "format must be png, jpeg, raw, pgm or ppm"
                );
                crate::engine::validate_capture_output(output)?;
                crate::naming::validate_request(output, *count, *on_exists, context)?;
                ensure!(
                    output != "-",
                    "scheduled capture requires a file or directory"
                );
                if let Self::Schedule {
                    first_at_ms,
                    interval_ms,
                    quality,
                    job,
                    manifest,
                    ..
                } = self
                {
                    crate::scheduling::validate_schedule(*first_at_ms, *interval_ms, *count)?;
                    if let Some(manifest) = manifest {
                        crate::naming::validate_output(manifest)?;
                        ensure!(
                            std::path::Path::new(manifest).is_absolute(),
                            "a manifest path must be absolute"
                        );
                    }
                    ensure!(
                        quality.is_none_or(|quality| (1..=100).contains(&quality)),
                        "JPEG quality must be 1..100"
                    );
                    ensure!(*job != Some(0), "capture job ID must be positive");
                }
            }
            Self::Arm {
                warmup_ms,
                bytes_per_second,
            } => {
                validate_warmup(*warmup_ms)?;
                ensure!(
                    *bytes_per_second != Some(0),
                    "a camera's share of its network must be positive"
                );
            }
            Self::Wait {
                job, timeout_ms, ..
            } => {
                validate_job(job)?;
                ensure!(
                    *timeout_ms <= jobs::MAX_WAIT_MS,
                    "a wait lasts at most 25000 ms; wait again"
                );
            }
            Self::StopJob { job } => validate_job(job)?,
            Self::ReadMemory { address, length } => validate_memory(*address, *length)?,
            Self::WriteMemory { address, data } => validate_memory(*address, data.len())?,
            Self::Forward {
                output,
                codec,
                encoder,
                fps,
                bitrate,
                max_file_bytes,
                on_exists,
                context,
                duration_ms,
                utc_offset_s,
                warmup_ms,
                ..
            } => {
                crate::naming::validate_offset(*utc_offset_s)?;
                validate_warmup(*warmup_ms)?;
                // The range `record --for` accepts.
                ensure!(
                    duration_ms.is_none_or(|duration| {
                        (1..=crate::scheduling::MAX_DURATION_MS).contains(&duration)
                    }),
                    "recording duration must be 1 ms to 10 years"
                );
                crate::naming::validate_request(output, 1, *on_exists, context)?;
                crate::media::ForwardConfig {
                    output: output.clone(),
                    codec: codec.clone(),
                    encoder: encoder.clone(),
                    fps: *fps,
                    bitrate: bitrate.clone(),
                    max_file_bytes: *max_file_bytes,
                    min_free: None,
                }
                .validate()?;
            }
            _ => {}
        }
        Ok(())
    }
}
/// How long a client waits for the session to answer `command` unless told
/// otherwise: a minute, or longer for commands whose own timeouts or work
/// can exceed it (multi-frame captures, setup restores).
pub fn default_request_timeout(command: &SessionCommand) -> Duration {
    Duration::from_millis(match command {
        SessionCommand::Capture {
            timeout_ms,
            warmup_ms,
            fresh,
            ..
        } => frame_wait_ms(*timeout_ms)
            .saturating_mul(u64::from(1 + *fresh))
            .saturating_add(warmup_ms.unwrap_or(DEFAULT_WARMUP_MS))
            .saturating_add(60_000),
        // Waiting for the camera's turn, or starting a stopped camera and
        // settling its exposure, can outlast the frame wait itself.
        SessionCommand::Frame {
            timeout_ms,
            warmup_ms,
            fresh,
            ..
        } => frame_wait_ms(*timeout_ms)
            .saturating_mul(u64::from(*fresh).max(1))
            .saturating_add(warmup_ms.unwrap_or(DEFAULT_WARMUP_MS))
            .saturating_add(15_000),
        // The worker bounds the writes to 90 s; a rollback may take as long,
        // and restarting the stream follows.
        SessionCommand::ApplySetup { .. } => 240_000,
        SessionCommand::Wait { timeout_ms, .. } => timeout_ms.saturating_add(5_000),
        // A recording's encoder may take 10 s to finish its file.
        SessionCommand::StopJob { .. } => 30_000,
        _ => 60_000,
    })
}
/// A capture's wait for each frame: up to ten years, which `none` (wait
/// for a trigger until stopped) asks for, or 0 for the camera's default.
pub fn validate_frame_wait(timeout_ms: u64) -> Result<()> {
    ensure!(
        timeout_ms <= crate::scheduling::MAX_DURATION_MS,
        "frame wait must be at most 10 years"
    );
    Ok(())
}
/// How long a camera that sets its own exposure may take to settle after
/// it starts, unless a request says otherwise.
pub const DEFAULT_WARMUP_MS: u64 = 2_000;
/// A request's frame wait for its timeouts: as asked, or for the camera's
/// default (0), a minute, which covers all but very long exposures.
fn frame_wait_ms(timeout_ms: u64) -> u64 {
    if timeout_ms == 0 { 60_000 } else { timeout_ms }
}
/// The longest warm-up a request may ask for.
pub const MAX_WARMUP_MS: u64 = 60_000;
fn validate_warmup(warmup_ms: Option<u64>) -> Result<()> {
    ensure!(
        warmup_ms.is_none_or(|ms| ms <= MAX_WARMUP_MS),
        "warm-up must be at most 60 s"
    );
    Ok(())
}
fn is_zero(value: &u32) -> bool {
    *value == 0
}
pub fn validate_timeout(timeout_ms: u64) -> Result<()> {
    ensure!(
        (1..=60_000).contains(&timeout_ms),
        "timeout must be 1..60000 milliseconds"
    );
    Ok(())
}
pub fn validate_selector(camera: &str) -> Result<()> {
    crate::camera::validate_selector_shape(camera)
}
/// Where a session client receives an image captured to stdout: a private
/// file it names `<pid>-<hex>` in the runtime folder's `out`, or in this
/// user's temporary folder when that is unusable.
pub(crate) fn stdout_file(file: &str) -> Result<std::path::PathBuf> {
    let path = std::path::Path::new(file);
    let named = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once('-'))
        .is_some_and(|(pid, rest)| {
            !pid.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && !rest.is_empty()
                && rest.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    let parent = path.parent().map(std::path::Path::to_path_buf);
    let out = stdout_dir();
    ensure!(
        named && parent.is_some() && (parent == out || parent == crate::dirs::temp()),
        "a stdout file must be a <pid>-<hex> file in {}",
        out.unwrap_or_default().display()
    );
    Ok(path.to_path_buf())
}
/// The private folder images captured to stdout through a session wait in.
pub(crate) fn stdout_dir() -> Option<std::path::PathBuf> {
    crate::dirs::runtime().ok().map(|dir| dir.join("out"))
}
/// Remove the images captured to stdout in `dir` that commands which died
/// left: older than ten minutes, named for a process that is gone.
pub(crate) fn sweep_stdout_files(dir: &std::path::Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(600);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(pid) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified < old);
        if stale && !crate::ipc::process_alive(pid) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A job ID: a session name, a dash and a positive number.
fn validate_job(job: &str) -> Result<()> {
    split_job(job).map(drop)
}
/// The session and number of job ID `job`.
pub(crate) fn split_job(job: &str) -> Result<(&str, u64)> {
    job.rsplit_once('-')
        .and_then(|(session, number)| {
            Some((session, number.parse::<u64>().ok().filter(|n| *n > 0)?))
        })
        .filter(|(session, _)| crate::ipc::validate_name(session).is_ok() || *session == DIRECT)
        .ok_or_else(|| {
            Categorized::new(ErrorKind::Usage, format!("'{job}' is not a job ID"))
                .hint("job IDs look like auto-1; capturefab jobs lists them")
                .into()
        })
}
/// A sign-in goes only with an rtsp://, rtsps:// or onvif: address, or a bare
/// name the coordinator may resolve to a discovered ONVIF camera; never
/// with credentials already in the address.
fn validate_login_selector(camera: &str) -> Result<()> {
    let usage = |message: &str| -> anyhow::Error { crate::types::Usage(message.into()).into() };
    if crate::logins::split_userinfo(camera).is_ok_and(|(_, login)| login.is_some())
        || camera.split_once("://").is_some_and(|(_, rest)| {
            rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())].contains('@')
        })
    {
        return Err(usage(
            "leave the user name and password out of the address when signing in",
        ));
    }
    let other = camera.starts_with("sim:")
        || camera.starts_with("gige:")
        || camera.starts_with("usb:")
        || camera.parse::<std::net::Ipv4Addr>().is_ok()
        || crate::media::is_host_camera(camera)
        || camera.to_ascii_lowercase().starts_with("file:");
    // A bare name may be the ID or serial of a discovered ONVIF camera,
    // which only the coordinator can resolve; a path or another URL scheme
    // is never one.
    let alias = !camera.contains("://") && !camera.contains(['/', '\\']);
    if other || !(crate::logins::key(camera).is_ok() || alias) {
        return Err(usage(
            "sign-in is supported for rtsp://, rtsps:// and onvif: cameras",
        ));
    }
    Ok(())
}
pub(crate) fn validate_feature(feature: &str) -> Result<()> {
    ensure!(
        !feature.is_empty()
            && !feature
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '='),
        "feature name must be nonempty and contain no whitespace, control characters or '='"
    );
    Ok(())
}
fn validate_memory(address: u64, length: usize) -> Result<()> {
    ensure!(
        (1..=65_536).contains(&length),
        "memory length must be 1..65536"
    );
    ensure!(
        address.checked_add((length - 1) as u64).is_some(),
        "memory address range overflows"
    );
    Ok(())
}
fn env_usize(name: &str, default: usize) -> Result<usize> {
    match std::env::var_os(name) {
        None => Ok(default),
        Some(value) => value
            .to_str()
            .with_context(|| format!("{name} must be a positive integer"))?
            .parse::<usize>()
            .with_context(|| format!("{name} must be a positive integer")),
    }
}
fn maximum_cameras() -> Result<usize> {
    let maximum = env_usize("CAPTUREFAB_MAX_CAMERAS", 16)?;
    ensure!(
        (1..=64).contains(&maximum),
        "CAPTUREFAB_MAX_CAMERAS must be 1..64"
    );
    Ok(maximum)
}
/// Lines the session log keeps.
const LOG_LINES: usize = 200;
/// The job IDs of a command's own worker, outside any session.
pub(crate) const DIRECT: &str = "direct";
/// The session whose camera this process serves, set once as a camera
/// worker of a session starts; None in every other process, including the
/// workers of one command.
static SERVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
pub(crate) fn served_session() -> Option<&'static str> {
    SERVED.get().map(String::as_str)
}
/// What an error about the camera worker's `variable` adds in a session:
/// its workers have the environment the session started with, so the value
/// a later command sets does not reach them.
pub(crate) fn environment_hint(variable: &str) -> Option<String> {
    let name = served_session()?;
    Some(format!(
        "session {name} keeps the {variable} it started with: stop it with capturefab session stop {name} and run the command again, or run it with --no-session"
    ))
}
/// `hint`, followed in a session's camera worker by [`environment_hint`]
/// for `variable`, which the hint may tell a person to change.
pub(crate) fn with_environment_hint(hint: String, variable: &str) -> String {
    match environment_hint(variable) {
        Some(session) => format!("{hint}; {session}"),
        None => hint,
    }
}
/// One line of a camera worker's or the session's log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    /// When it was logged, in UTC Unix milliseconds.
    pub unix_ms: u64,
    pub level: String,
    pub message: String,
}
impl LogEntry {
    pub fn now(level: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            unix_ms: crate::scheduling::now_ms(),
            level: level.into(),
            message: message.into(),
        }
    }
}
/// The session's newest log lines, its own and its camera workers', in the
/// order they arrived.
#[derive(Default)]
struct SessionLog(VecDeque<LogEntry>);
impl SessionLog {
    fn note(&mut self, entry: LogEntry) {
        if self.0.len() == LOG_LINES {
            self.0.pop_front();
        }
        self.0.push_back(entry);
    }
    fn lines(&self) -> Vec<LogEntry> {
        self.0.iter().cloned().collect()
    }
}
/// A worker's line as the session log shows it: tagged with the worker's
/// camera, and with URL credentials redacted.
fn camera_line(entry: LogEntry, camera: Option<&str>) -> LogEntry {
    let message = match camera {
        Some(camera) => format!("[{}] {}", crate::media::redact_url(camera), entry.message),
        None => entry.message,
    };
    LogEntry {
        message: crate::scrub::redact_urls(&message),
        ..entry
    }
}
/// Frames a worker lost on the way, never frames skipped for a preview.
fn frames_lost(worker: &WorkerSnapshot) -> u64 {
    let transport = worker
        .transport
        .as_ref()
        .map_or(0, |t| t.incomplete_frames + t.lost_frames);
    worker.dropped.saturating_add(transport)
}

/// A camera worker's state, which its status request returns: its camera,
/// stream and recording. Its log and jobs stay in the worker; jobs and log
/// lines reach the coordinator as events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub connected: Option<CameraInfo>,
    pub streaming: bool,
    pub frames: u64,
    /// Payload bytes acquired by this worker, before preview downsampling or encoding.
    #[serde(default)]
    pub received_bytes: u64,
    pub dropped: u64,
    pub fps: f64,
    pub last_error: Option<String>,
    /// The newest lines logged, and how many in all, for tests: log lines
    /// reach the coordinator as events.
    #[cfg(test)]
    #[serde(skip)]
    pub logs: Vec<LogEntry>,
    #[cfg(test)]
    #[serde(skip)]
    pub logged: u64,
    #[serde(default)]
    pub forwarding: Option<String>,
    /// Opaque identity of this exact recording or forward, including replacements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_id: Option<String>,
    /// The session job of the recording or forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_job: Option<u64>,
    /// The absolute file a local recording writes, with its template
    /// expanded; unset for network forwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_path: Option<String>,
    /// When the current recording or forward started, UTC Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_since_ms: Option<u64>,
    /// When the worker stops a timed recording or forward, UTC Unix
    /// milliseconds; unset when it runs until stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_until_ms: Option<u64>,
    #[serde(skip)]
    pub jobs: Vec<crate::scheduling::CaptureJob>,
    #[serde(default)]
    pub auto: Option<crate::auto::AutoStatus>,
    #[serde(default)]
    pub transport: Option<crate::types::TransportStats>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraSnapshot {
    pub info: CameraInfo,
    pub streaming: bool,
    pub frames: u64,
    /// Payload bytes acquired by this worker, before preview downsampling or encoding.
    #[serde(default)]
    pub received_bytes: u64,
    pub dropped: u64,
    pub fps: f64,
    pub last_error: Option<String>,
    /// The camera's worker exited; the next command on the camera starts
    /// another.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exited: bool,
    /// Why the worker exited: `{code, message, hint?, details:{exit,
    /// signal, stderr_tail}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// `unknown` once a worker ended in the middle of a settings change,
    /// until settings are written or a preset applied again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_state: Option<String>,
    pub worker_pid: u32,
    #[serde(default)]
    pub forwarding: Option<String>,
    /// The absolute file a local recording writes, with its template
    /// expanded; unset for network forwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_path: Option<String>,
    #[serde(default)]
    pub auto: Option<crate::auto::AutoStatus>,
    #[serde(default)]
    pub transport: Option<crate::types::TransportStats>,
    /// Frames lost on the way: the worker's receive errors plus the
    /// transport's incomplete and missing frames.
    #[serde(default)]
    pub lost: u64,
    /// Frames the transport received incomplete, where it can tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<u64>,
    /// The camera's jobs, as `jobs` lists them.
    #[serde(default)]
    pub jobs: Vec<Value>,
}
/// The session's state: each connected camera, the cameras the last
/// discovery found, and the session log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub devices: Vec<CameraInfo>,
    #[serde(default)]
    pub cameras: Vec<CameraSnapshot>,
    pub logs: Vec<LogEntry>,
}
type Reply = Receiver<Result<Value>>;
/// The connected cameras an error offers instead, with credentials redacted.
fn candidates<'a>(ids: impl Iterator<Item = &'a String>) -> Vec<Value> {
    ids.map(|id| json!({"id": crate::media::redact_url(id)}))
        .collect()
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a camera was connected, which a reconnect repeats.
#[derive(Clone)]
struct ConnectSpec {
    selector: String,
    timeout_ms: u64,
    login: Option<crate::logins::CameraLogin>,
    saved_login: bool,
}

/// Crashes within this window, past `CRASHES`, leave a camera in error.
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const CRASHES: usize = 3;

/// A camera the session holds, through a worker that may be replaced.
struct Held {
    info: CameraInfo,
    spec: ConnectSpec,
    /// It streams on its own: a reconnect starts it again.
    live: AtomicBool,
    worker: RwLock<Option<Arc<Worker>>>,
    /// Why its last worker exited, while no other runs.
    crash: Mutex<Option<Value>>,
    /// A worker ended in the middle of a settings change.
    config_unknown: AtomicBool,
    crashes: Mutex<VecDeque<Instant>>,
    /// One reconnect at a time.
    reconnecting: Mutex<()>,
    /// How long it stays unused, and its use.
    keep: Mutex<Keep>,
}
/// How long a held camera stays connected unused, and its use by a
/// session's clients.
struct Keep {
    /// The longest a client's command asked to keep it after its last use
    /// (`--keep`, or the warm default); none holds it as long as the
    /// session.
    keep: Option<Duration>,
    /// The session was started with it, which holds it as long as the
    /// session whatever a command asks.
    pinned: bool,
    /// When a client last used it.
    used: Instant,
    /// Client requests using it now.
    busy: usize,
    /// Its keep ran out unused: it is let go once its turn comes, unless
    /// a request uses it first.
    releasing: bool,
    /// It is being let go: requests find it gone.
    released: bool,
}
impl Default for Keep {
    fn default() -> Self {
        Self {
            keep: None,
            pinned: false,
            used: Instant::now(),
            busy: 0,
            releasing: false,
            released: false,
        }
    }
}
/// How long until a held camera is let go unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unused {
    /// A request or a job uses it now.
    InUse,
    /// Its own keep runs out after this.
    After(Duration),
    /// It stays as long as the session.
    WithSession,
}
/// A client's request using a held camera: it stays connected while the
/// request runs, and its keep counts from the request's end.
pub struct CameraUse(Arc<Held>);
impl Drop for CameraUse {
    fn drop(&mut self) {
        let mut keep = locked(&self.0.keep);
        keep.busy = keep.busy.saturating_sub(1);
        keep.used = Instant::now();
    }
}
impl Held {
    /// A request uses it, which wants it kept at least `keep` after; false
    /// once it is being let go.
    fn enter(&self, keep: Option<Duration>) -> bool {
        let mut state = locked(&self.keep);
        if state.released {
            return false;
        }
        state.busy += 1;
        state.used = Instant::now();
        state.keep = state.keep.max(keep);
        true
    }
    /// Whether its keep ran out unused, with no request and no job (`jobs`)
    /// using it: it is to be let go. A job counts as use.
    fn expired(&self, jobs: bool) -> bool {
        let mut state = locked(&self.keep);
        let Some(keep) = state.keep.filter(|_| !state.pinned) else {
            return false;
        };
        if state.releasing || state.released {
            return false;
        }
        if state.busy > 0 || jobs {
            state.used = Instant::now();
            return false;
        }
        state.releasing = state.used.elapsed() >= keep;
        state.releasing
    }
    /// Whether to let it go now, its turn come: it is still unused, and
    /// from now on requests find it gone.
    fn let_go(&self, jobs: bool) -> bool {
        let mut state = locked(&self.keep);
        state.releasing = false;
        let unused = state
            .keep
            .filter(|_| !state.pinned)
            .is_some_and(|keep| state.used.elapsed() >= keep && state.busy == 0 && !jobs);
        state.released = unused;
        unused
    }
    /// How long until it is let go unused, with `jobs` using it or not.
    fn unused_left(&self, jobs: bool) -> Unused {
        let state = locked(&self.keep);
        if state.busy > 0 || jobs {
            return Unused::InUse;
        }
        match state.keep.filter(|_| !state.pinned) {
            Some(keep) => Unused::After(keep.saturating_sub(state.used.elapsed())),
            None => Unused::WithSession,
        }
    }
    fn worker(&self) -> Option<Arc<Worker>> {
        self.worker
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|worker| !worker.exited())
    }
    /// Its worker exited `CRASHES` times within `CRASH_WINDOW`.
    fn crash_looping(&self) -> bool {
        let mut crashes = locked(&self.crashes);
        while crashes
            .front()
            .is_some_and(|at| at.elapsed() > CRASH_WINDOW)
        {
            crashes.pop_front();
        }
        crashes.len() >= CRASHES
    }
}

/// A connect in progress, which concurrent connects to the same camera wait
/// for instead of starting their own.
#[derive(Default)]
struct Flight {
    result: Mutex<Option<std::result::Result<Value, Value>>>,
    done: Condvar,
}
impl Flight {
    fn finish(&self, result: &Result<Value>) {
        *locked(&self.result) = Some(match result {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(protocol::wire_error(error)),
        });
        self.done.notify_all();
    }
    fn wait(&self) -> Result<Value> {
        let mut result = locked(&self.result);
        loop {
            if let Some(result) = result.as_ref() {
                return result.clone().map_err(|wire| protocol::from_wire(&wire));
            }
            result = self
                .done
                .wait(result)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A caller's hold on its request: cancelling it ends the request in the
/// worker that runs it, or before it starts.
#[derive(Default)]
struct Cancel {
    cancelled: AtomicBool,
    running: Mutex<Option<(Arc<Worker>, u64)>>,
}
impl Cancel {
    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel)
            && let Some((worker, id)) = locked(&self.running).as_ref()
        {
            worker.cancel(*id);
        }
    }
    /// The request is `id` of `worker` now.
    fn running(&self, worker: &Arc<Worker>, id: u64) {
        *locked(&self.running) = Some((worker.clone(), id));
        if self.cancelled.load(Ordering::Acquire) {
            worker.cancel(id);
        }
    }
}
/// How often a request run for a client looks whether the client is gone.
const CLIENT_WATCH: Duration = Duration::from_millis(250);

struct Coordinator {
    /// The name job IDs start with.
    name: String,
    verbose: bool,
    /// Worker warnings and errors are printed as they come, to a session's
    /// stderr or log.
    print_logs: bool,
    devices: RwLock<Vec<CameraInfo>>,
    cameras: RwLock<BTreeMap<String, Arc<Held>>>,
    /// The selectors cameras were connected by, with each camera's ID, so a
    /// client can name a camera the way it was opened.
    aliases: Mutex<BTreeMap<String, String>>,
    flights: Mutex<BTreeMap<String, Arc<Flight>>>,
    /// Camera slots: cameras held and connects in progress.
    slots: AtomicUsize,
    quitting: AtomicBool,
    /// Set when `shutdown` has finished; later callers wait for it.
    shut_down: (Mutex<bool>, Condvar),
    pending: AtomicUsize,
    /// Frame inspections in progress; they wait outside the action slots.
    inspecting: AtomicUsize,
    command_order: Mutex<BTreeMap<CommandTarget, Weak<CommandOrder>>>,
    log: Mutex<SessionLog>,
    jobs: JobBook,
    watchdog: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set by `shutdown` to wake the watchdog from its wait at once.
    watch_stop: Arc<(Mutex<bool>, Condvar)>,
    /// The write end of the pipe the next worker writes an image captured
    /// to stdout to, which `stdout_pipe` made.
    output: Mutex<Option<std::io::PipeWriter>>,
    /// Every worker started, connected yet or not, for `killer`.
    spawned: Mutex<Vec<Weak<worker::Shared>>>,
}
/// Holds one of the bounded command slots; released even if the command panics.
/// Frame inspections count against their own bound, so observing never
/// takes a slot from camera actions.
struct PendingSlot {
    inner: Arc<Coordinator>,
    inspection: bool,
}
impl PendingSlot {
    fn acquire(inner: &Arc<Coordinator>, inspection: bool) -> Result<Self> {
        let slot = Self {
            inner: inner.clone(),
            inspection,
        };
        // Count first; dropping the guard undoes the increment when full.
        let previous = slot.counter().fetch_add(1, Ordering::AcqRel);
        if inspection {
            ensure!(
                previous < MAX_INSPECTIONS,
                "too many frame inspections are waiting; retry when one returns"
            );
        } else {
            ensure!(previous < MAX_PENDING, "camera command queue is full");
        }
        Ok(slot)
    }
    fn counter(&self) -> &AtomicUsize {
        if self.inspection {
            &self.inner.inspecting
        } else {
            &self.inner.pending
        }
    }
}
impl Drop for PendingSlot {
    fn drop(&mut self) {
        self.counter().fetch_sub(1, Ordering::AcqRel);
    }
}
/// A camera slot taken for a connect, given back unless kept.
struct SlotGuard<'a>(Option<&'a AtomicUsize>);
impl SlotGuard<'_> {
    fn keep(mut self) {
        self.0 = None;
    }
}
impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        if let Some(slots) = self.0 {
            slots.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CommandTarget {
    Camera(String),
    Discovery,
}
#[derive(Default)]
struct CommandOrder {
    state: Mutex<CommandOrderState>,
    ready: Condvar,
}
#[derive(Default)]
struct CommandOrderState {
    next: u64,
    pending: VecDeque<u64>,
}
/// Reserved before spawning so thread scheduling cannot reorder commands.
/// Dropping an unstarted task removes its ticket without releasing a later
/// command while an earlier command is still running.
struct CommandTurn {
    order: Arc<CommandOrder>,
    ticket: u64,
}
impl CommandOrder {
    fn reserve(self: &Arc<Self>) -> CommandTurn {
        let mut state = locked(&self.state);
        let ticket = state.next;
        state.next = state.next.wrapping_add(1);
        state.pending.push_back(ticket);
        CommandTurn {
            order: self.clone(),
            ticket,
        }
    }
}
impl CommandTurn {
    fn wait(&self) {
        let mut state = locked(&self.order.state);
        while state.pending.front() != Some(&self.ticket) {
            state = self
                .order
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}
impl Drop for CommandTurn {
    fn drop(&mut self) {
        let mut state = locked(&self.order.state);
        if let Some(position) = state
            .pending
            .iter()
            .position(|ticket| *ticket == self.ticket)
        {
            state.pending.remove(position);
        }
        self.order.ready.notify_all();
    }
}
const MAX_PENDING: usize = 32;
/// Frame inspections waiting at once; their workers measure as many.
const MAX_INSPECTIONS: usize = crate::engine::inspector::MAX_INSPECTIONS;
/// How long a status request waits for each worker.
const STATUS_WAIT: Duration = Duration::from_secs(2);
/// How long stopping a job waits for it to end: a recording's encoder may
/// take 10 s to finish its file.
const STOP_WAIT: Duration = Duration::from_secs(15);
/// How long a session's shutdown waits for every worker together.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Coordinator>,
}
impl Default for SessionHandle {
    fn default() -> Self {
        Self::new()
    }
}
impl SessionHandle {
    /// The workers of one command, outside any session.
    pub fn new() -> Self {
        Self::direct(false)
    }
    /// The workers of one command; `verbose` workers send info log lines.
    pub fn direct(verbose: bool) -> Self {
        Self::create(DIRECT, verbose, false, JobBook::new(DIRECT))
    }
    /// Session `name`, which keeps its jobs in the state folder: the jobs
    /// an earlier session of that name left are listed as lost. Its workers
    /// send info log lines too, for the log a client shows under `-v`; its
    /// own output has only warnings and errors.
    pub fn named(name: &str) -> Result<Self> {
        crate::ipc::validate_name(name)?;
        if let Some(dir) = stdout_dir() {
            sweep_stdout_files(&dir);
        }
        Ok(Self::create(name, true, true, JobBook::saved(name)))
    }
    fn create(name: &str, verbose: bool, print_logs: bool, jobs: JobBook) -> Self {
        let handle = Self {
            inner: Arc::new(Coordinator {
                name: name.into(),
                verbose,
                print_logs,
                devices: RwLock::default(),
                cameras: RwLock::default(),
                aliases: Mutex::default(),
                flights: Mutex::default(),
                slots: AtomicUsize::new(0),
                quitting: AtomicBool::new(false),
                shut_down: (Mutex::new(false), Condvar::new()),
                pending: AtomicUsize::new(0),
                inspecting: AtomicUsize::new(0),
                command_order: Mutex::new(BTreeMap::new()),
                log: Mutex::default(),
                jobs,
                watchdog: Mutex::new(None),
                watch_stop: Arc::default(),
                output: Mutex::new(None),
                spawned: Mutex::default(),
            }),
        };
        let weak = Arc::downgrade(&handle.inner);
        let stop = handle.inner.watch_stop.clone();
        let watchdog = std::thread::Builder::new()
            .name("capturefab-watchdog".into())
            .spawn(move || watch(weak, &stop));
        if let Ok(watchdog) = watchdog {
            *locked(&handle.inner.watchdog) = Some(watchdog);
        }
        handle
    }
    /// Replace the discovered cameras, as a discovery would.
    #[cfg(test)]
    pub(crate) fn set_devices_for_test(&self, devices: Vec<CameraInfo>) {
        *self.edit_devices() = devices;
    }
    fn edit_devices(&self) -> std::sync::RwLockWriteGuard<'_, Vec<CameraInfo>> {
        self.inner
            .devices
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
    fn devices(&self) -> Vec<CameraInfo> {
        self.inner
            .devices
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn cameras(&self) -> Vec<(String, Arc<Held>)> {
        self.inner
            .cameras
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(id, held)| (id.clone(), held.clone()))
            .collect()
    }
    fn note(&self, level: &str, message: String) {
        locked(&self.inner.log).note(LogEntry::now(level, message));
    }
    fn loose_camera(&self, camera: String) -> String {
        if crate::camera::is_explicit_selector(&camera) {
            return camera;
        }
        let resolution = crate::camera::resolve_selector(&camera, &self.devices());
        match resolution {
            crate::camera::Resolution::Loose { info, matched_on } => {
                self.note(
                    "info",
                    format!(
                        "Using {} for '{}' (matched its {matched_on})",
                        crate::media::redact_url(&info.id),
                        crate::media::redact_url(&camera)
                    ),
                );
                info.id
            }
            _ => camera,
        }
    }
    /// The ONVIF address of a discovered or connected ONVIF camera named by
    /// its ID or serial; any other selector unchanged.
    fn onvif_alias(&self, camera: String) -> String {
        if crate::onvif::is_source(&camera) || camera.contains("://") {
            return camera;
        }
        let held = self
            .cameras()
            .into_iter()
            .map(|(_, held)| held.info.clone());
        self.devices()
            .into_iter()
            .chain(held)
            .find(|device| {
                crate::onvif::is_source(&device.id)
                    && (device.id == camera || device.serial == camera)
            })
            .map(|device| device.id)
            .unwrap_or(camera)
    }
    /// The session now: each camera as its worker reports it, asked at once
    /// and in parallel, and the cameras the last discovery found.
    pub fn snapshot(&self) -> SessionSnapshot {
        let cameras = self.cameras();
        let jobs = self.inner.jobs.all();
        let session = self.inner.jobs.session();
        let cameras = std::thread::scope(|scope| {
            let threads = cameras
                .iter()
                .map(|(id, held)| scope.spawn(|| self.camera_snapshot(id, held, &jobs, &session)))
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .filter_map(|thread| thread.join().ok())
                .collect::<Vec<_>>()
        });
        SessionSnapshot {
            devices: self.devices(),
            cameras,
            logs: locked(&self.inner.log).lines(),
        }
    }
    fn camera_snapshot(
        &self,
        id: &str,
        held: &Held,
        jobs: &[jobs::Job],
        session: &str,
    ) -> CameraSnapshot {
        let jobs = jobs
            .iter()
            .filter(|job| job.key == id)
            .map(|job| job.doc(session))
            .collect();
        let worker = held.worker();
        let report = worker.as_ref().map(|worker| worker.status(STATUS_WAIT));
        let crash = locked(&held.crash).clone();
        let error = match (&report, crash) {
            (Some(Err(error)), _) => Some(protocol::wire_error(error)),
            (_, crash) => crash,
        };
        let report = report.and_then(Result::ok).unwrap_or_default();
        CameraSnapshot {
            info: held.info.clone(),
            streaming: report.streaming,
            frames: report.frames,
            received_bytes: report.received_bytes,
            dropped: report.dropped,
            fps: report.fps,
            last_error: error
                .as_ref()
                .and_then(|error| error["message"].as_str().map(str::to_owned))
                .or(report.last_error.clone()),
            exited: worker.is_none(),
            worker_pid: worker.as_ref().map_or(0, |worker| worker.pid()),
            forwarding: report.forwarding.clone(),
            recording_path: report.recording_path.clone(),
            auto: report.auto.clone(),
            transport: report.transport.clone(),
            lost: frames_lost(&report),
            config_state: held
                .config_unknown
                .load(Ordering::Acquire)
                .then(|| "unknown".into()),
            incomplete: report
                .transport
                .as_ref()
                .map(|transport| transport.incomplete_frames),
            error,
            jobs,
        }
    }
    /// The one connected camera, which a command that names none addresses.
    fn only_camera(&self) -> Result<String> {
        let cameras = self
            .inner
            .cameras
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let mut ids = cameras.keys();
        match (ids.next(), ids.next()) {
            (Some(id), None) => Ok(id.clone()),
            (None, _) => Err(Categorized::new(
                ErrorKind::CameraNotFound,
                "no camera connected in this session",
            )
            .hint("name one with -c CAMERA")
            .details(json!({"candidates": []}))
            .into()),
            (Some(_), Some(_)) => Err(Categorized::new(
                ErrorKind::CameraAmbiguous,
                format!(
                    "{} cameras are connected; name one with -c CAMERA",
                    cameras.len()
                ),
            )
            .details(json!({"candidates": candidates(cameras.keys())}))
            .into()),
        }
    }
    /// The held camera `target` names, or the only one.
    fn target(&self, target: Option<&str>) -> Result<(String, Arc<Held>)> {
        let id = match target {
            Some(target) => self.camera_id(target),
            None => self.only_camera()?,
        };
        let cameras = self
            .inner
            .cameras
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = cameras.get(&id) {
            return Ok((id, held.clone()));
        }
        if let Some((key, held)) = cameras
            .iter()
            .find(|(_, held)| held.info.serial == id || held.info.address.as_deref() == Some(&id))
        {
            return Ok((key.clone(), held.clone()));
        }
        Err(Categorized::new(
            ErrorKind::CameraNotFound,
            format!(
                "camera '{}' not connected in this session",
                crate::media::redact_url(&id)
            ),
        )
        .details(json!({"candidates": candidates(cameras.keys())}))
        .into())
    }
    pub fn submit(&self, c: SessionCommand) -> Result<Reply> {
        self.submit_target(None, c, None)
    }
    pub fn submit_to(&self, camera: &str, c: SessionCommand) -> Result<Reply> {
        self.submit_target(Some(camera.to_owned()), c, None)
    }
    /// Run `command` for a client of the session, on `camera` when given,
    /// while `gone` tells whether that client went away. A job wait then
    /// ends at once, so the job's lease runs out from then. An inspection
    /// or a capture to stdout, whose only product is its reply, is
    /// cancelled in its worker; other commands carry on, as they do when a
    /// client stops waiting for them.
    pub fn request_for(
        &self,
        camera: Option<&str>,
        command: SessionCommand,
        gone: &dyn Fn() -> bool,
    ) -> Result<Value> {
        let caller_only = matches!(
            &command,
            SessionCommand::Frame { .. }
                | SessionCommand::Capture {
                    stdout: Some(_),
                    ..
                }
        );
        if let SessionCommand::Wait {
            job,
            after_seq,
            timeout_ms,
        } = &command
        {
            command.validate()?;
            if let Some(camera) = camera {
                validate_selector(camera)?;
            }
            if self.inner.quitting.load(Ordering::Relaxed) {
                return Err(stopping());
            }
            return self.wait_job_while(job, *after_seq, Duration::from_millis(*timeout_ms), gone);
        }
        let cancel = caller_only.then(|| Arc::new(Cancel::default()));
        let reply = self.submit_target(camera.map(str::to_owned), command, cancel.clone())?;
        let Some(cancel) = cancel else {
            return reply.recv().context("coordinator stopped")?;
        };
        loop {
            match reply.recv_timeout(CLIENT_WATCH) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if gone() {
                        cancel.cancel();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("coordinator stopped")
                }
            }
        }
    }
    /// Resolve aliases from cached identities, without waiting for a busy
    /// worker. All names for one camera share an ordering gate.
    fn camera_id(&self, selector: &str) -> String {
        let cameras = self.cameras();
        let devices = self.devices();
        let identities = || {
            cameras
                .iter()
                .map(|(_, held)| &held.info)
                .chain(devices.iter())
        };
        if identities().any(|info| info.id == selector) {
            return selector.to_owned();
        }
        if let Some(id) = locked(&self.inner.aliases).get(selector) {
            return id.clone();
        }
        if let Some(info) = identities()
            .find(|info| info.serial == selector || info.address.as_deref() == Some(selector))
        {
            return info.id.clone();
        }
        // A connected camera named as clients see it, with its credentials
        // redacted.
        let redacted = crate::media::redact_url(selector);
        let mut shown = cameras
            .iter()
            .filter(|(id, _)| crate::media::redact_url(id) == redacted);
        match (shown.next(), shown.next()) {
            (Some((id, _)), None) if redacted != selector => id.clone(),
            _ => selector.to_owned(),
        }
    }
    /// The connected cameras as a session descriptor lists them, with
    /// credentials redacted: what clients match a selector against.
    pub fn held_cameras(&self) -> Vec<crate::ipc::HeldCamera> {
        let aliases = locked(&self.inner.aliases).clone();
        let mut held = self
            .cameras()
            .into_iter()
            .map(|(id, held)| {
                crate::ipc::HeldCamera::new(
                    &held.info,
                    aliases
                        .iter()
                        .filter(|(_, camera)| **camera == id)
                        .map(|(alias, _)| alias.as_str()),
                )
            })
            .collect::<Vec<_>>();
        held.sort_by(|a, b| a.id.cmp(&b.id));
        held
    }
    /// Whether any camera has a job that has yet to end.
    pub fn has_jobs(&self) -> bool {
        self.inner.jobs.active(None)
    }
    /// A client's request uses the held camera `camera` names, or the only
    /// one, and wants it kept at least `keep` after it ends: it stays while
    /// the request runs. None when no such camera is held, or it is being
    /// let go.
    pub fn hold_camera(&self, camera: Option<&str>, keep: Option<Duration>) -> Option<CameraUse> {
        let (_, held) = self.target(camera).ok()?;
        held.enter(keep).then(|| CameraUse(held))
    }
    /// Keep the held camera `camera` names at least `keep` after its last
    /// use, or with none as long as the session: what a session started
    /// with a camera does with it.
    pub fn keep_camera(&self, camera: &str, keep: Option<Duration>) {
        if let Ok((_, held)) = self.target(Some(camera)) {
            let mut state = locked(&held.keep);
            match keep {
                Some(keep) => state.keep = state.keep.max(Some(keep)),
                None => state.pinned = true,
            }
        }
    }
    /// How long until the held camera `camera` names is let go unused;
    /// None when no such camera is held.
    pub fn unused_left(&self, camera: &str) -> Option<Unused> {
        let (key, held) = self.target(Some(camera)).ok()?;
        Some(held.unused_left(self.inner.jobs.active(Some(&key))))
    }
    /// Let go of the cameras whose keep ran out unused, each in its turn
    /// after the commands before.
    fn release_unused(&self) {
        for (key, held) in self.cameras() {
            if !held.expired(self.inner.jobs.active(Some(&key))) {
                continue;
            }
            let h = self.clone();
            let releasing = held.clone();
            let spawned = std::thread::Builder::new()
                .name("capturefab-release".into())
                .spawn(move || h.release(&key, &releasing));
            if spawned.is_err() {
                locked(&held.keep).releasing = false;
            }
        }
    }
    /// Let go of camera `key`, as `held`, once the commands before have run,
    /// unless one used it meanwhile.
    fn release(&self, key: &str, held: &Arc<Held>) {
        let turn = self.command_turn(Some(key), &SessionCommand::Disconnect);
        if let Some(turn) = &turn {
            turn.wait();
        }
        let current = self
            .inner
            .cameras
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .is_some_and(|camera| Arc::ptr_eq(camera, held));
        if !current || self.inner.quitting.load(Ordering::Acquire) {
            locked(&held.keep).releasing = false;
            return;
        }
        if !held.let_go(self.inner.jobs.active(Some(key))) {
            return;
        }
        let keep = locked(&held.keep).keep.unwrap_or_default();
        let camera = crate::media::redact_url(key);
        match self.disconnect(Some(key)) {
            Ok(_) => self.note(
                "info",
                format!(
                    "Closed {camera}: unused for {}",
                    crate::types::duration_text(keep)
                ),
            ),
            Err(error) => self.note(
                "warn",
                crate::scrub::redact_urls(&format!("Closing {camera}, unused: {error:#}")),
            ),
        }
    }
    fn command_turn(&self, target: Option<&str>, command: &SessionCommand) -> Option<CommandTurn> {
        if inline(command) {
            return None;
        }
        let target = match command {
            SessionCommand::NetworkScan { .. } => CommandTarget::Discovery,
            SessionCommand::Connect { camera, .. } => CommandTarget::Camera(self.camera_id(camera)),
            _ => CommandTarget::Camera(self.camera_id(target?)),
        };
        let mut orders = locked(&self.inner.command_order);
        // Idle gates have no state to retain. At most the bounded pending
        // commands keep gates alive, including across disconnect/reconnect.
        orders.retain(|_, order| order.strong_count() > 0);
        let order = orders
            .get(&target)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let order = Arc::new(CommandOrder::default());
                orders.insert(target, Arc::downgrade(&order));
                order
            });
        Some(order.reserve())
    }
    fn submit_target(
        &self,
        target: Option<String>,
        command: SessionCommand,
        cancel: Option<Arc<Cancel>>,
    ) -> Result<Reply> {
        command.validate()?;
        if let Some(camera) = &target {
            validate_selector(camera)?;
        }
        if self.inner.quitting.load(Ordering::Relaxed) {
            return Err(stopping());
        }
        // Looking at the session and its jobs never waits for a camera's
        // turn or an action slot.
        if inline(&command) {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(self.process(target.as_deref(), command, None));
            return Ok(rx);
        }
        let inspection = matches!(command, SessionCommand::Frame { .. });
        let slot = PendingSlot::acquire(&self.inner, inspection)?;
        // A camera command that names no camera is for the only one, and
        // waits its turn behind that camera's earlier commands.
        let target = target.or_else(|| {
            if matches!(
                &command,
                SessionCommand::Connect { .. } | SessionCommand::NetworkScan { .. }
            ) {
                None
            } else {
                self.only_camera().ok()
            }
        });
        let target = target.map(|camera| self.camera_id(&camera));
        let turn = self.command_turn(target.as_deref(), &command);
        let h = self.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("capturefab-command".into())
            .spawn(move || {
                let _slot = slot;
                let _turn = turn;
                if let Some(turn) = &_turn {
                    turn.wait();
                }
                let result = if h.inner.quitting.load(Ordering::Acquire) {
                    Err(stopping())
                } else {
                    // The worker never reads the state directory: a named
                    // setup is loaded here, off the caller's thread.
                    crate::setup::resolve(command).and_then(|command| {
                        h.process(target.as_deref(), command, cancel.as_deref())
                    })
                };
                // An inspection's failure is its caller's to report; a slow
                // or triggered camera must not fill the session log.
                if let Err(e) = &result
                    && !inspection
                {
                    h.note("error", crate::scrub::redact_urls(&format!("{e:#}")));
                }
                let _ = tx.send(result);
            })
            .context("start camera command thread")?;
        Ok(rx)
    }
    pub fn request(&self, c: SessionCommand) -> Result<Value> {
        self.submit(c)?.recv().context("coordinator stopped")?
    }
    pub fn request_to(&self, camera: &str, c: SessionCommand) -> Result<Value> {
        self.submit_to(camera, c)?
            .recv()
            .context("coordinator stopped")?
    }
    fn process(
        &self,
        target: Option<&str>,
        command: SessionCommand,
        cancel: Option<&Cancel>,
    ) -> Result<Value> {
        match command {
            SessionCommand::NetworkScan { timeout_ms, tune } => {
                self.scan_network(Duration::from_millis(timeout_ms), tune)
            }
            SessionCommand::Status => {
                // A camera's status shows that camera alone.
                let camera = target
                    .map(|target| self.target(Some(target)).map(|(id, _)| id))
                    .transpose()?;
                let mut status = self.snapshot();
                if let Some(camera) = camera {
                    status.cameras.retain(|c| c.info.id == camera);
                }
                Ok(serde_json::to_value(status)?)
            }
            SessionCommand::Jobs => {
                let session = self.inner.jobs.session();
                Ok(json!({"jobs": self
                    .inner
                    .jobs
                    .all()
                    .iter()
                    .map(|job| job.doc(&session))
                    .collect::<Vec<_>>()}))
            }
            SessionCommand::Wait {
                job,
                after_seq,
                timeout_ms,
            } => self.wait_job(&job, after_seq, Duration::from_millis(timeout_ms)),
            SessionCommand::StopJob { job } => self.stop_job(&job),
            SessionCommand::Connect {
                camera,
                timeout_ms,
                login,
                saved_login,
            } => self.connect(ConnectSpec {
                selector: camera,
                timeout_ms,
                login,
                saved_login,
            }),
            SessionCommand::Disconnect => self.disconnect(target),
            command @ (SessionCommand::Schedule { .. }
            | SessionCommand::Forward { .. }
            | SessionCommand::Auto {
                continuous: true, ..
            }) => self.start_job(target, command),
            command => {
                let (_, held) = self.target(target)?;
                let live = match &command {
                    SessionCommand::Start => Some(true),
                    SessionCommand::Stop => Some(false),
                    _ => None,
                };
                // Settings written in full again, or a preset applied, are
                // known again.
                let known = matches!(
                    command,
                    SessionCommand::SetValues { dry_run: false, .. }
                        | SessionCommand::ApplySetup { .. }
                );
                let worker = self.worker_of(&held)?;
                let result = match cancel {
                    // The client left while the request waited for the
                    // camera's turn: it is not started.
                    Some(cancel) if cancel.cancelled.load(Ordering::Acquire) => Err(
                        Categorized::error(ErrorKind::Interrupted, "cancelled before it started"),
                    ),
                    Some(cancel) => {
                        let (id, reply) = worker.submit(command.into())?;
                        cancel.running(&worker, id);
                        worker.wait(&reply)
                    }
                    None => worker.call(command),
                }?;
                if let Some(live) = live {
                    held.live.store(live, Ordering::Release);
                }
                if known {
                    held.config_unknown.store(false, Ordering::Release);
                }
                Ok(result)
            }
        }
    }
    /// The worker of `held`; one exited is replaced by a reconnect, unless
    /// it keeps crashing.
    fn worker_of(&self, held: &Arc<Held>) -> Result<Arc<Worker>> {
        if let Some(worker) = held.worker() {
            return Ok(worker);
        }
        let _turn = locked(&held.reconnecting);
        if let Some(worker) = held.worker() {
            return Ok(worker);
        }
        if held.crash_looping() {
            let crash = locked(&held.crash)
                .clone()
                .unwrap_or_else(|| json!({"message": "camera worker exited"}));
            let error = protocol::from_wire(&crash);
            return Err(Categorized::new(
                ErrorKind::WorkerCrashed,
                format!(
                    "the camera's worker exited {CRASHES} times within {} s: {error:#}",
                    CRASH_WINDOW.as_secs()
                ),
            )
            .hint(format!(
                "connect it again with capturefab session start -c {}",
                crate::hint::shell_quote(&crate::media::redact_url(&held.spec.selector))
            ))
            .details(crash["details"].clone())
            .into());
        }
        self.reconnect(held)
    }
    /// Start a worker for `held` again, as it was connected, streaming again
    /// if it streamed on its own.
    fn reconnect(&self, held: &Arc<Held>) -> Result<Arc<Worker>> {
        let worker = Arc::new(self.spawn_worker()?);
        *locked(&worker.shared().label) = held.info.id.clone();
        let spec = &held.spec;
        let connect = SessionCommand::Connect {
            camera: spec.selector.clone(),
            timeout_ms: spec.timeout_ms,
            login: spec.login.clone(),
            saved_login: spec.saved_login,
        };
        let limit = connect_limit(&connect);
        worker.call_within(connect, limit)?;
        if held.live.load(Ordering::Acquire) {
            worker.call(SessionCommand::Start)?;
        }
        {
            let mut current = held.worker.write().unwrap_or_else(PoisonError::into_inner);
            // As in connect_new: a shutdown has already taken this camera.
            if self.inner.quitting.load(Ordering::Acquire) {
                drop(current);
                worker.close();
                return Err(stopping());
            }
            *current = Some(worker.clone());
        }
        *locked(&held.crash) = None;
        self.note(
            "info",
            format!(
                "Reconnected {} after its worker exited",
                crate::media::redact_url(&held.info.id)
            ),
        );
        Ok(worker)
    }
    /// A pipe the next camera worker writes an image captured to stdout
    /// to: its read end, which this command copies to its own stdout.
    pub fn stdout_pipe(&self) -> Result<std::io::PipeReader> {
        let (reader, writer) = std::io::pipe().context("stdout capture pipe")?;
        *locked(&self.inner.output) = Some(writer);
        Ok(reader)
    }
    fn spawn_worker(&self) -> Result<Worker> {
        let weak = Arc::downgrade(&self.inner);
        let worker = Worker::spawn(
            &worker::Options {
                verbose: self.inner.verbose,
                session: self.inner.name.clone(),
            },
            locked(&self.inner.output).take(),
            Arc::new(move |shared, signal| {
                if let Some(inner) = weak.upgrade() {
                    SessionHandle { inner }.on_signal(shared, signal);
                }
            }),
        )?;
        let mut spawned = locked(&self.inner.spawned);
        spawned.retain(|shared| shared.strong_count() > 0);
        spawned.push(Arc::downgrade(worker.shared()));
        Ok(worker)
    }
    /// What ends every worker of this coordinator and everything they
    /// started, at once and without finishing anything: for a process about
    /// to exit on a second signal. It holds the coordinator weakly.
    pub fn killer(&self) -> impl FnMut() + Send + 'static {
        let coordinator = Arc::downgrade(&self.inner);
        move || {
            let Some(inner) = coordinator.upgrade() else {
                return;
            };
            let spawned = locked(&inner.spawned)
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            for shared in spawned {
                shared.kill();
            }
        }
    }
    /// A worker's event, or its exit.
    fn on_signal(&self, shared: &worker::Shared, signal: Signal) {
        let key = locked(&shared.label).clone();
        match signal {
            Signal::Output(protocol::Output::Log { level, message }) => {
                let entry = camera_line(
                    LogEntry::now(level, message),
                    (!key.is_empty()).then_some(key.as_str()),
                );
                if self.inner.print_logs && entry.level != "info" {
                    crate::eprintln!("capturefab: {}", entry.message);
                }
                locked(&self.inner.log).note(entry);
            }
            Signal::Output(output) => self.inner.jobs.event(&output),
            Signal::Exited(error) => {
                let held = self
                    .inner
                    .cameras
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&key)
                    .cloned();
                let Some(held) = held else {
                    return;
                };
                let current = held
                    .worker
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .is_some_and(|worker| std::ptr::eq(worker.shared().as_ref(), shared));
                if !current || is_stopping(&error) {
                    return;
                }
                self.inner.jobs.fail_camera(&key, &error);
                if shared.interrupted_change() {
                    held.config_unknown.store(true, Ordering::Release);
                }
                locked(&held.crashes).push_back(Instant::now());
                *locked(&held.crash) = Some(protocol::wire_error(&error));
                let message = crate::scrub::redact_urls(&format!(
                    "[{}] {error:#}",
                    crate::media::redact_url(&key)
                ));
                if self.inner.print_logs {
                    crate::eprintln!("capturefab: {message}");
                }
                self.note("error", message);
            }
        }
    }
    /// Connect a camera, or find it already held. Concurrent connects to
    /// one camera share one attempt; a camera found under another name is
    /// the one already held.
    fn connect(&self, spec: ConnectSpec) -> Result<Value> {
        let typed = spec.selector.clone();
        let remember = |id: &str| {
            if typed != id {
                locked(&self.inner.aliases).insert(typed.clone(), id.to_owned());
            }
        };
        // A discovered ONVIF camera named by its serial or ID opens
        // by its ONVIF address, which is what a sign-in is saved
        // under.
        let camera = self.onvif_alias(spec.selector.clone());
        // A unique loose name ("C920") among cached cameras becomes
        // that camera's ID, so connected workers see one identity.
        let camera = self.loose_camera(camera);
        if spec.login.is_some() && crate::logins::key(&camera).is_err() {
            return Err(crate::types::Usage(
                "sign-in is supported for rtsp://, rtsps:// and onvif: cameras".into(),
            )
            .into());
        }
        if let Ok((id, held)) = self.target(Some(&camera)) {
            let worker = match held.worker() {
                Some(worker) => worker,
                // Connecting it again by name clears its crash count.
                None => {
                    let _turn = locked(&held.reconnecting);
                    locked(&held.crashes).clear();
                    match held.worker() {
                        Some(worker) => worker,
                        None => self.reconnect(&held)?,
                    }
                }
            };
            remember(&id);
            return Ok(json!({"connected": held.info, "worker_pid": worker.pid()}));
        }
        crate::camera::validate_selector(&camera)?;
        let key = self.camera_id(&camera);
        let (flight, leading) = {
            let mut flights = locked(&self.inner.flights);
            match flights.get(&key) {
                Some(flight) => (flight.clone(), false),
                None => {
                    let flight = Arc::new(Flight::default());
                    flights.insert(key.clone(), flight.clone());
                    (flight, true)
                }
            }
        };
        if !leading {
            let result = flight.wait()?;
            if let Some(id) = result["connected"]["id"].as_str() {
                remember(id);
            }
            return Ok(result);
        }
        let result = self.connect_new(ConnectSpec {
            selector: camera,
            ..spec
        });
        flight.finish(&result);
        locked(&self.inner.flights).remove(&key);
        if let Some(id) = result
            .as_ref()
            .ok()
            .and_then(|result| result["connected"]["id"].as_str())
        {
            remember(id);
        }
        result
    }
    fn connect_new(&self, spec: ConnectSpec) -> Result<Value> {
        let maximum = maximum_cameras()?;
        let slots = &self.inner.slots;
        let slot = SlotGuard(Some(slots));
        if slots.fetch_add(1, Ordering::AcqRel) >= maximum {
            return Err(Categorized::new(
                ErrorKind::Unavailable,
                format!(
                    "camera limit reached ({maximum}); disconnect a camera or increase CAPTUREFAB_MAX_CAMERAS"
                ),
            )
            .into());
        }
        let worker = Arc::new(self.spawn_worker()?);
        let connect = SessionCommand::Connect {
            camera: spec.selector.clone(),
            timeout_ms: spec.timeout_ms,
            login: spec.login.clone(),
            saved_login: spec.saved_login,
        };
        let limit = connect_limit(&connect);
        let reply = worker.call_within(connect, limit);
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                // Held by this session under another name: that camera.
                if let Some(found) = self.held_by_us(&error) {
                    return Ok(found);
                }
                return Err(error);
            }
        };
        let info: CameraInfo = serde_json::from_value(reply["connected"].clone())
            .context("worker connected without identity")?;
        *locked(&worker.shared().label) = info.id.clone();
        let mut cameras = self
            .inner
            .cameras
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // A shutdown that began meanwhile has taken the cameras already; it
        // would never end this worker.
        if self.inner.quitting.load(Ordering::Acquire) {
            drop(cameras);
            worker.close();
            return Err(stopping());
        }
        if let Some(existing) = cameras.get(&info.id).cloned() {
            // Another name for a camera connected meanwhile: keep the
            // worker clients already use.
            drop(cameras);
            worker.close();
            let pid = existing.worker().map_or(0, |worker| worker.pid());
            return Ok(json!({"connected": existing.info, "worker_pid": pid}));
        }
        let pid = worker.pid();
        cameras.insert(
            info.id.clone(),
            Arc::new(Held {
                info: info.clone(),
                spec,
                live: AtomicBool::new(false),
                worker: RwLock::new(Some(worker)),
                crash: Mutex::default(),
                config_unknown: AtomicBool::new(false),
                crashes: Mutex::default(),
                reconnecting: Mutex::default(),
                keep: Mutex::default(),
            }),
        );
        slot.keep();
        Ok(json!({"connected": info, "worker_pid": pid}))
    }
    /// The camera an error says is held by one of this session's workers.
    fn held_by_us(&self, error: &anyhow::Error) -> Option<Value> {
        let classified = crate::types::classify(error);
        if classified.kind != ErrorKind::CameraBusy {
            return None;
        }
        let pid = classified.details?["held_by"]["pid"].as_u64()?;
        self.cameras().into_iter().find_map(|(_, held)| {
            let worker = held.worker()?;
            (u64::from(worker.pid()) == pid)
                .then(|| json!({"connected": held.info, "worker_pid": worker.pid()}))
        })
    }
    fn disconnect(&self, target: Option<&str>) -> Result<Value> {
        let (id, held) = self.target(target)?;
        let result = match held.worker() {
            Some(worker) => worker.call(SessionCommand::Disconnect),
            None => Ok(json!({"connected": null})),
        };
        let removed = self
            .inner
            .cameras
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if removed.is_some() {
            self.inner.slots.fetch_sub(1, Ordering::AcqRel);
        }
        locked(&self.inner.aliases).retain(|_, camera| *camera != id);
        if let Some(worker) = held
            .worker
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            worker.close();
        }
        result
    }
    /// Run a capture, recording or continuous auto exposure as a job of the
    /// session: numbered here, then followed by its worker's events.
    fn start_job(&self, target: Option<&str>, mut command: SessionCommand) -> Result<Value> {
        let (key, held) = self.target(target)?;
        let (kind, planned, lease) = match &command {
            SessionCommand::Schedule { count, lease, .. } => {
                (JobKind::Capture, Some(u64::from(*count)), *lease)
            }
            SessionCommand::Forward { lease, .. } => (JobKind::Record, None, *lease),
            _ => (JobKind::Auto, None, false),
        };
        let worker = self.worker_of(&held)?;
        let number = self.inner.jobs.start(kind, &key, planned, lease);
        match &mut command {
            SessionCommand::Schedule { job, .. }
            | SessionCommand::Forward { job, .. }
            | SessionCommand::Auto { job, .. } => *job = Some(number),
            _ => {}
        }
        let live = matches!(command, SessionCommand::Auto { .. });
        let mut reply = match worker.call(command) {
            Ok(reply) => reply,
            Err(error) => {
                self.inner.jobs.refused(number);
                return Err(error);
            }
        };
        let detail = match kind {
            JobKind::Capture => reply["job"].clone(),
            // The warm-up before the first frame is the recording's to
            // report when it ends.
            JobKind::Record => crate::types::without_nulls(json!({
                "since_ms": reply["since_ms"],
                "until_ms": reply["until_ms"],
                "path": reply["path"],
                "warmup_ms": reply["warmup_ms"],
                "settled": reply["settled"],
            })),
            JobKind::Auto => reply["auto"].clone(),
        };
        self.inner.jobs.accepted(number, detail);
        // Capturefab's continuous auto exposure meters a stream.
        if live {
            if worker
                .status(STATUS_WAIT)
                .is_ok_and(|status| !status.streaming)
            {
                worker.call(SessionCommand::Start)?;
            }
            held.live.store(true, Ordering::Release);
        }
        let job = self
            .inner
            .jobs
            .get(number)
            .map(|job| job.doc(&self.inner.jobs.session()));
        reply["job"] = json!(job);
        reply["worker_pid"] = json!(worker.pid());
        Ok(reply)
    }
    /// The number of `job`, a job of this session.
    fn job_number(&self, job: &str) -> Result<u64> {
        let (session, number) = split_job(job)?;
        if session != self.inner.name {
            return Err(Categorized::new(
                ErrorKind::Usage,
                format!("{job} is not a job of session {}", self.inner.name),
            )
            .hint("capturefab jobs lists every session's jobs")
            .into());
        }
        Ok(number)
    }
    fn no_job(&self, job: &str) -> anyhow::Error {
        Categorized::new(
            ErrorKind::Usage,
            format!("session {} has no job {job}", self.inner.name),
        )
        .hint("capturefab jobs lists every session's jobs")
        .into()
    }
    /// Wait for `job` to change after `after_seq`, or end: `{seq, job,
    /// detail}`.
    pub(crate) fn wait_job(&self, job: &str, after_seq: u64, wait: Duration) -> Result<Value> {
        self.wait_job_while(job, after_seq, wait, &|| false)
    }
    /// A leased capture job of one frame on `camera` that no worker runs,
    /// for tests of waiting.
    #[cfg(test)]
    pub(crate) fn test_job(&self, camera: &str) -> String {
        let number = self
            .inner
            .jobs
            .start(JobKind::Capture, camera, Some(1), true);
        format!("{}-{number}", self.inner.name)
    }
    /// `wait_job` for a client, which ends early once `gone` says the
    /// client went away.
    fn wait_job_while(
        &self,
        job: &str,
        after_seq: u64,
        wait: Duration,
        gone: &dyn Fn() -> bool,
    ) -> Result<Value> {
        let number = self.job_number(job)?;
        let wait = wait.min(Duration::from_millis(jobs::MAX_WAIT_MS));
        let (seq, found) = self
            .inner
            .jobs
            .wait(number, after_seq, wait, gone)
            .ok_or_else(|| self.no_job(job))?;
        Ok(json!({
            "seq": seq,
            "job": found.doc(&self.inner.jobs.session()),
            "detail": found.detail(),
        }))
    }
    /// Stop `job`, and wait for it to end: `{job, detail}`.
    pub(crate) fn stop_job(&self, job: &str) -> Result<Value> {
        let number = self.job_number(job)?;
        let (key, found) = self
            .inner
            .jobs
            .stopping(number)
            .ok_or_else(|| self.no_job(job))?;
        if !found.state.ended() {
            let worker = self
                .inner
                .cameras
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&key)
                .and_then(|held| held.worker());
            if let Some(worker) = worker {
                worker.stop_job(number)?;
            }
        }
        let deadline = Instant::now() + STOP_WAIT;
        let mut seq = 0;
        loop {
            let (now, found) = self
                .inner
                .jobs
                .wait(
                    number,
                    seq,
                    deadline.saturating_duration_since(Instant::now()),
                    &|| false,
                )
                .ok_or_else(|| self.no_job(job))?;
            seq = now;
            if found.state.ended() || Instant::now() >= deadline {
                return Ok(json!({
                    "job": found.doc(&self.inner.jobs.session()),
                    "detail": found.detail(),
                }));
            }
        }
    }
    /// Check the network paths of the GigE cameras that answer and those
    /// this session holds; with `tune`, give each connected camera its share
    /// of the interface it streams on.
    fn scan_network(&self, timeout: Duration, tune: bool) -> Result<Value> {
        let before = self.snapshot();
        let active: Vec<_> = before
            .cameras
            .iter()
            .map(|camera| camera.info.clone())
            .collect();
        let mut devices: Vec<_> = before
            .devices
            .iter()
            .filter(|camera| camera.transport != crate::types::Transport::GigE)
            .cloned()
            .collect();
        let mut warnings = Vec::new();
        match crate::transport::gige::discover(timeout) {
            Ok(discovered) => devices.extend(discovered),
            Err(error) => {
                warnings.push(format!("GigE discovery: {error:#}"));
                devices.extend(
                    before
                        .devices
                        .iter()
                        .filter(|camera| camera.transport == crate::types::Transport::GigE)
                        .cloned(),
                );
            }
        }
        for device in &mut devices {
            device.profile = before
                .devices
                .iter()
                .chain(active.iter())
                .find(|previous| previous.id == device.id)
                .map_or_else(
                    || crate::camera_profile::detect(device, &[]),
                    |previous| previous.profile.clone(),
                );
        }
        let mut report = crate::network::scan(&devices, &active, timeout)?;
        report.warnings.extend(warnings);
        // One budget per physical receive interface, including connected cameras
        // that are idle now but may be started together later.
        let plans: Vec<_> = report
            .cameras
            .iter()
            .enumerate()
            .filter(|(_, camera)| tune && camera.active && camera.reachable)
            .map(|(index, camera)| {
                let sharers = report
                    .cameras
                    .iter()
                    .filter(|other| other.active && other.interface == camera.interface)
                    .count();
                (
                    index,
                    camera.camera.id.clone(),
                    crate::network::budget_bytes_per_second(camera, &report.interfaces, sharers),
                )
            })
            .collect();
        for batch in plans.chunks(4) {
            let results = std::thread::scope(|scope| {
                let before = &before;
                let threads: Vec<_> = batch
                    .iter()
                    .map(|(index, camera, budget)| {
                        (
                            *index,
                            scope.spawn(move || {
                                let (_, held) = self.target(Some(camera))?;
                                let worker = held.worker();
                                let original = before.cameras.iter().find(|item| item.info.id == *camera);
                                let unchanged = worker.as_ref().zip(original).is_some_and(|(worker, then)| {
                                    worker.pid() == then.worker_pid
                                        && held.info.address == then.info.address
                                });
                                ensure!(unchanged,
                                    "Camera connection changed during the scan; scan again to apply current defaults"
                                );
                                self.worker_of(&held)?.call(WorkerCommand::OptimizeNetwork {
                                    bytes_per_second: *budget,
                                })
                            }),
                        )
                    })
                    .collect();
                threads
                    .into_iter()
                    .map(|(index, thread)| {
                        (
                            index,
                            thread.join().unwrap_or_else(|_| {
                                Err(anyhow::anyhow!("network tuning worker stopped"))
                            }),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            for (index, result) in results {
                let camera = &mut report.cameras[index];
                match result.and_then(|value| {
                    serde_json::from_value::<crate::network::NetworkTuning>(value["tuning"].clone())
                        .context("reading camera network tuning")
                }) {
                    Ok(tuning) => {
                        camera.tuning = Some(tuning);
                    }
                    Err(error) => {
                        camera.tuning = Some(crate::network::NetworkTuning {
                            warnings: vec![format!("Defaults were not applied: {error:#}")],
                            ..Default::default()
                        })
                    }
                }
            }
        }
        let after = self.snapshot();
        if after.cameras.len() != before.cameras.len()
            || after.cameras.iter().any(|camera| {
                !before.cameras.iter().any(|old| {
                    old.info.id == camera.info.id
                        && old.worker_pid == camera.worker_pid
                        && old.info.address == camera.info.address
                })
            })
        {
            report.warnings.push("Camera connections changed during the scan. Scan again to refresh shared bandwidth budgets and reachability.".into());
        }
        for camera in &mut report.cameras {
            camera.active = after.cameras.iter().any(|active| {
                active.info.id == camera.camera.id && active.info.address == camera.camera.address
            });
            if let Some(current) = after
                .cameras
                .iter()
                .find(|active| active.info.id == camera.camera.id)
                && let Some(previous) = before
                    .cameras
                    .iter()
                    .find(|active| active.info.id == camera.camera.id)
                && !current.exited
                && !previous.exited
                && current.worker_pid == previous.worker_pid
                && current.info.address == previous.info.address
                && let (Some(now), Some(then)) = (&current.transport, &previous.transport)
            {
                let losses = now.lost_frames.saturating_sub(then.lost_frames)
                    + now.incomplete_frames.saturating_sub(then.incomplete_frames);
                let resends = now.resend_requested.saturating_sub(then.resend_requested);
                if losses > 0 || resends > 0 {
                    camera.health = crate::network::NetworkHealth::Unstable;
                    camera.summary = format!(
                        "Stream needs attention · {losses} frames lost, {resends} packet retries during scan"
                    );
                    camera.details.push("Check the cable and switch, use a dedicated Ethernet port, or reduce the camera data rate; scan again to retest.".into());
                } else if current.streaming
                    && current.frames > previous.frames
                    && now.packets_received > then.packets_received
                    && camera.health == crate::network::NetworkHealth::Unverified
                    && camera
                        .tuning
                        .as_ref()
                        .is_some_and(|tuning| tuning.warnings.is_empty())
                    && camera.interface.as_ref().is_some_and(|name| {
                        report.interfaces.iter().any(|interface| {
                            interface.name == *name
                                && camera.host_address == Some(interface.address)
                                && interface.kind == crate::network::InterfaceKind::Wired
                                && interface.speed_mbps.is_some_and(|speed| speed >= 1000)
                        })
                    })
                {
                    camera.health = crate::network::NetworkHealth::Ready;
                    camera.summary = "Frames received without loss during scan".into();
                }
            }
        }
        *self.edit_devices() = devices;
        Ok(json!({"network_scan":report}))
    }
    /// Stop taking commands; every worker finishes its jobs and recordings
    /// and exits, all at once within one deadline, after which those left
    /// are ended with everything they started.
    pub fn shutdown(&self) {
        if self.inner.quitting.swap(true, Ordering::AcqRel) {
            // Another thread (a Ctrl-C handler) is finishing recordings;
            // returning now would let the process exit first.
            let (done, finished) = &self.inner.shut_down;
            let _ = finished
                .wait_timeout_while(locked(done), SHUTDOWN_WAIT * 2, |done| !*done)
                .unwrap_or_else(PoisonError::into_inner);
            return;
        }
        /// Releases those waiting callers when shutdown ends, even by a panic.
        struct Finished<'a>(&'a (Mutex<bool>, Condvar));
        impl Drop for Finished<'_> {
            fn drop(&mut self) {
                let (done, finished) = self.0;
                *locked(done) = true;
                finished.notify_all();
            }
        }
        let _finished = Finished(&self.inner.shut_down);
        let held = std::mem::take(
            &mut *self
                .inner
                .cameras
                .write()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let workers = held
            .values()
            .filter_map(|held| {
                held.worker
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
            })
            .collect::<Vec<_>>();
        for worker in &workers {
            worker.close();
        }
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        for worker in &workers {
            if !worker.wait_exit(deadline) {
                crate::eprintln!("capturefab: a camera worker did not finish in time; ending it");
                worker.kill();
            }
        }
        drop(workers);
        self.inner.slots.store(0, Ordering::Release);
        // Jobs of a worker that was ended had no word of their end.
        for (key, _) in held {
            self.inner
                .jobs
                .end_camera(&key, jobs::JobState::Interrupted, &stopping());
        }
        self.inner.jobs.flush();
        {
            let (stop, wake) = &*self.inner.watch_stop;
            *locked(stop) = true;
            wake.notify_all();
        }
        if let Some(watchdog) = locked(&self.inner.watchdog).take() {
            let _ = watchdog.join();
        }
    }
    /// Cancel every camera's request in progress, such as a capture to
    /// stdout: it ends at its next frame wait.
    pub fn interrupt(&self) {
        for (_, held) in self.cameras() {
            if let Some(worker) = held.worker() {
                worker.cancel_all();
            }
        }
    }
}

/// How long a connect may take before its worker counts as hung: its
/// budget, and the hang limit beyond it.
fn connect_limit(connect: &SessionCommand) -> Duration {
    Duration::from_millis(WorkerCommand::Session(connect.clone()).budget_ms()) + worker::HANG
}

/// Commands answered on the caller's thread: they look at the session or
/// its jobs and never wait for a camera's turn.
fn inline(command: &SessionCommand) -> bool {
    matches!(
        command,
        SessionCommand::Status
            | SessionCommand::Jobs
            | SessionCommand::Wait { .. }
            | SessionCommand::StopJob { .. }
    )
}

/// Watch a coordinator's workers until it shuts down: end those that hang,
/// stop jobs whose client stopped waiting, and save the job book. `stop`
/// ends the wait between looks at once, so a shutdown never waits for it.
fn watch(coordinator: Weak<Coordinator>, stop: &(Mutex<bool>, Condvar)) {
    loop {
        let (stopped, wake) = stop;
        let stopped = wake
            .wait_timeout_while(locked(stopped), Duration::from_millis(500), |stopped| {
                !*stopped
            })
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        if *stopped {
            return;
        }
        drop(stopped);
        let Some(inner) = coordinator.upgrade() else {
            return;
        };
        // The shutdown ends the workers and saves the book itself.
        if inner.quitting.load(Ordering::Acquire) {
            return;
        }
        let handle = SessionHandle { inner };
        for (key, held) in handle.cameras() {
            if let Some(worker) = held.worker() {
                worker.watch(handle.inner.jobs.active(Some(&key)));
            }
        }
        handle.release_unused();
        for (key, number) in handle.inner.jobs.abandoned() {
            handle.note(
                "warn",
                format!(
                    "Stopping job {}-{number}: the client waiting for it is gone",
                    handle.inner.name
                ),
            );
            let worker = handle
                .inner
                .cameras
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&key)
                .and_then(|held| held.worker());
            handle.inner.jobs.stopping(number);
            if let Some(worker) = worker {
                let _ = worker.stop_job(number);
            }
        }
        handle.inner.jobs.flush();
    }
}

/// A request that reached a session as it shuts down. A client that chose
/// the session itself tries again with a worker of its own.
pub fn stopping() -> anyhow::Error {
    Categorized::new(ErrorKind::Unavailable, "session is shutting down")
        .details(json!({"session": "stopping"}))
        .into()
}
/// Whether `error` says the session was shutting down.
pub fn is_stopping(error: &anyhow::Error) -> bool {
    crate::types::classify(error)
        .details
        .is_some_and(|details| details["session"] == "stopping")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loose_names_resolve_to_one_local_camera_id() {
        let handle = SessionHandle::new();
        let mut webcam = crate::transport::simulator::info_named("sim:0");
        webcam.id = "avfoundation:HD Pro Webcam C920".into();
        webcam.model = "HD Pro Webcam C920".into();
        webcam.serial = webcam.id.clone();
        handle.set_devices_for_test(vec![webcam]);
        assert_eq!(
            handle.loose_camera("c920".into()),
            "avfoundation:HD Pro Webcam C920"
        );
        assert_eq!(handle.loose_camera("front door".into()), "front door");
        assert_eq!(handle.loose_camera("sim:3".into()), "sim:3");
        handle.shutdown();
    }
    #[test]
    fn request_timeouts_cover_the_work_a_command_can_wait_for() {
        let timeout = |json: &str| {
            default_request_timeout(&SessionCommand::from_json_slice(json.as_bytes()).unwrap())
        };
        // Every frame may wait its full timeout, after a warm-up of 2 s,
        // plus a minute to send.
        assert_eq!(
            timeout(r#"{"op":"capture","timeout_ms":10000,"format":"png"}"#),
            Duration::from_secs(72)
        );
        assert_eq!(
            timeout(
                r#"{"op":"capture","timeout_ms":60000,"format":"png","warmup_ms":0,"fresh":2}"#
            ),
            Duration::from_secs(240)
        );
        assert_eq!(
            timeout(r#"{"op":"apply_setup","name":"bench"}"#),
            Duration::from_secs(240)
        );
        assert_eq!(timeout(r#"{"op":"status"}"#), Duration::from_secs(60));
        // A frame after a warm-up of 2 s, or the camera's default wait.
        assert_eq!(timeout(r#"{"op":"frame"}"#), Duration::from_secs(20));
        assert_eq!(
            timeout(r#"{"op":"frame","timeout_ms":10000,"warmup_ms":0}"#),
            Duration::from_secs(25)
        );
        assert_eq!(
            timeout(r#"{"op":"frame","timeout_ms":0}"#),
            Duration::from_secs(77)
        );
        assert_eq!(
            timeout(r#"{"op":"wait","job":"auto-1","timeout_ms":25000}"#),
            Duration::from_secs(30)
        );
        assert_eq!(
            default_request_timeout(&SessionCommand::Disconnect),
            Duration::from_secs(60)
        );
    }
    #[test]
    fn plant_floor_command_defaults_and_unit_ops_are_strict() {
        let capture = r#"{"op":"schedule","output":"a.png","count":1,"timeout_ms":100,"format":"png","first_at_ms":0,"interval_ms":0}"#;
        let command = SessionCommand::from_json_slice(capture.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_value(&command).unwrap()["on_exists"],
            Value::Null
        );
        assert!(
            serde_json::to_string(&command)
                .unwrap()
                .find("context")
                .is_none()
        );
        for op in ["jobs", "read_setup", "manual"] {
            let extra = format!(r#"{{"op":"{op}","x":1}}"#);
            assert!(
                SessionCommand::from_json_slice(extra.as_bytes()).is_err(),
                "{op}"
            );
            let bare = format!(r#"{{"op":"{op}"}}"#);
            SessionCommand::from_json_slice(bare.as_bytes()).unwrap();
        }
        let snapshot = serde_json::to_value(WorkerSnapshot::default()).unwrap();
        assert!(snapshot.get("forwarding_until_ms").is_none());
        assert!(snapshot.get("logs").is_none(), "log lines are events");
        // A forward without a duration keeps its old shape both ways, and
        // one without a maximum size has none.
        let old = br#"{"op":"forward","output":"a.mkv","codec":"h264","encoder":"auto","fps":30,"bitrate":"4M"}"#;
        let command = SessionCommand::from_json_slice(old).unwrap();
        assert!(matches!(
            command,
            SessionCommand::Forward {
                duration_ms: None,
                max_file_bytes: None,
                min_free: None,
                ..
            }
        ));
        assert!(
            serde_json::to_value(&command)
                .unwrap()
                .get("duration_ms")
                .is_none()
        );
        let mut timed = serde_json::to_value(&command).unwrap();
        timed["duration_ms"] = json!(3_600_000);
        let command = SessionCommand::from_json_slice(timed.to_string().as_bytes()).unwrap();
        assert!(matches!(
            command,
            SessionCommand::Forward {
                duration_ms: Some(3_600_000),
                ..
            }
        ));
        assert_eq!(serde_json::to_value(&command).unwrap(), timed);
    }
    #[test]
    fn recording_duration_matches_the_cli_duration_range() {
        let forward = |duration_ms| SessionCommand::Forward {
            output: "clip.mkv".into(),
            codec: "h264".into(),
            encoder: "auto".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            max_file_bytes: None,
            min_free: None,
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms,
            utc_offset_s: None,
            job: None,
            lease: false,
            copy: Default::default(),
            warmup_ms: None,
        };
        let max = crate::scheduling::MAX_DURATION_MS;
        for duration in [None, Some(1), Some(500), Some(3_600_000), Some(max)] {
            forward(duration).validate().unwrap();
        }
        for duration in [0, max + 1, u64::MAX] {
            let error = forward(Some(duration)).validate().unwrap_err();
            assert_eq!(
                error.to_string(),
                "recording duration must be 1 ms to 10 years"
            );
        }
        // `record --for 500ms` must keep working.
        forward(Some(500)).validate().unwrap();
    }
    #[test]
    fn worker_lines_are_tagged_with_their_camera_and_redacted() {
        let line = LogEntry {
            unix_ms: 1_792_017_000_123,
            level: "error".into(),
            message: "rtsp://user:secret@example.test/live failed".into(),
        };
        let tagged = camera_line(line.clone(), Some("camera-a"));
        assert_eq!(tagged.unix_ms, line.unix_ms);
        assert_eq!(tagged.level, "error");
        assert!(
            tagged.message.starts_with("[camera-a] rtsp://"),
            "{tagged:?}"
        );
        assert!(!tagged.message.contains("secret"), "{tagged:?}");
        // Before its camera opens, a worker's line is only redacted.
        let untagged = camera_line(line, None);
        assert!(untagged.message.starts_with("rtsp://"), "{untagged:?}");
        assert!(!untagged.message.contains("secret"), "{untagged:?}");
    }
    #[test]
    fn a_camera_status_lists_that_camera_alone() {
        let handle = SessionHandle::new();
        handle.set_devices_for_test(vec![crate::transport::simulator::info_named("sim:2")]);
        // No worker holds sim:1, so it is not this session's camera.
        let error = handle
            .request_to("sim:1", SessionCommand::Status)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "camera 'sim:1' not connected in this session"
        );
        let status = handle.request(SessionCommand::Status).unwrap();
        assert_eq!(status["cameras"], json!([]), "only workers' cameras show");
        assert_eq!(status["devices"][0]["id"], "sim:2");
        for removed in ["active_camera", "target_camera", "connected", "activity"] {
            assert!(status.get(removed).is_none(), "{removed}");
        }
        handle.shutdown();
    }
    #[test]
    fn strict_json_commands_reject_ignored_or_duplicate_fields() {
        for op in [
            "activity",
            "select",
            "undo",
            "redo",
            "replay_camera_change",
            "features",
            "stop_forward",
            "cancel_job",
        ] {
            let removed = format!(r#"{{"op":"{op}"}}"#);
            assert!(
                SessionCommand::from_json_slice(removed.as_bytes()).is_err(),
                "{op}"
            );
        }
        for op in ["disconnect", "start", "stop", "jobs", "status", "xml"] {
            let valid = format!(r#"{{"op":"{op}"}}"#);
            assert!(SessionCommand::from_json_slice(valid.as_bytes()).is_ok());
            let extra = format!(r#"{{"op":"{op}","unknown":1}}"#);
            assert!(
                SessionCommand::from_json_slice(extra.as_bytes())
                    .unwrap_err()
                    .to_string()
                    .contains("unknown field `unknown`")
            );
        }
        for (command, field) in [
            (r#"{"op":"status","op":"start"}"#, "op"),
            (
                r#"{"op":"capture","timeout_ms":1000,"format":"png","context":{"line":"1","line":"2"}}"#,
                "line",
            ),
        ] {
            assert!(
                SessionCommand::from_json_slice(command.as_bytes())
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("duplicate field `{field}`"))
            );
        }
        assert!(SessionCommand::from_json_slice(br#"{"op":"status"} {}"#).is_err());
    }
    #[test]
    fn frame_and_set_values_have_strict_shapes_and_documented_defaults() {
        let SessionCommand::Frame {
            min_free: None,
            fresh,
            timeout_ms,
            max_dimension,
            format,
            region,
            preview,
            warmup_ms,
        } = SessionCommand::from_json_slice(br#"{"op":"frame"}"#).unwrap()
        else {
            panic!("not a frame")
        };
        assert_eq!(
            (fresh, timeout_ms, max_dimension, format, region, preview),
            (
                1,
                3000,
                1024,
                crate::inspect::PreviewFormat::Auto,
                None,
                None
            )
        );
        assert_eq!(warmup_ms, None);
        let full = br#"{"op":"frame","fresh":0,"timeout_ms":50,"max_dimension":0,"format":"png","region":[0.0,0.5,1.0,0.5]}"#;
        let command = SessionCommand::from_json_slice(full).unwrap();
        command.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&command).unwrap(),
            serde_json::from_slice::<Value>(full).unwrap()
        );
        let bare = SessionCommand::from_json_slice(br#"{"op":"frame"}"#).unwrap();
        assert!(serde_json::to_value(bare).unwrap().get("region").is_none());
        // inspect --fresh 10 lets as many frames pass as capture --fresh 10:
        // the frame's count takes in the frame measured.
        let frame = |fresh: u32| {
            let json = format!(r#"{{"op":"frame","fresh":{fresh}}}"#);
            SessionCommand::from_json_slice(json.as_bytes())
                .unwrap()
                .validate()
        };
        let capture = |fresh: u32| {
            let json =
                format!(r#"{{"op":"capture","timeout_ms":0,"format":"png","fresh":{fresh}}}"#);
            SessionCommand::from_json_slice(json.as_bytes())
                .unwrap()
                .validate()
        };
        let most = crate::inspect::MAX_FRESH;
        frame(most + 1).unwrap();
        capture(most).unwrap();
        assert!(frame(most + 2).is_err());
        assert!(capture(most + 1).is_err());
        for invalid in [
            r#"{"op":"frame","unknown":1}"#,
            r#"{"op":"frame","fresh":1,"fresh":2}"#,
            r#"{"op":"frame","format":"gif"}"#,
            r#"{"op":"frame","region":[0,0,1]}"#,
            r#"{"op":"frame","fresh":-1}"#,
            r#"{"op":"grab","timeout_ms":5}"#,
            r#"{"op":"set_many","values":[{"feature":"Gain","value":"1"}]}"#,
        ] {
            assert!(
                SessionCommand::from_json_slice(invalid.as_bytes()).is_err(),
                "{invalid}"
            );
        }
        let values = |count: usize| SessionCommand::SetValues {
            values: (0..count)
                .map(|i| crate::controls::Assignment::parse(&format!("Gain[Tap{i}]=1")).unwrap())
                .collect(),
            dry_run: false,
        };
        values(MAX_SET_VALUES).validate().unwrap();
        for invalid in [values(0), values(MAX_SET_VALUES + 1)] {
            assert_eq!(
                invalid.validate().unwrap_err().to_string(),
                "set writes 1..32 values at once"
            );
        }
    }
    #[test]
    fn invalid_command_values_are_rejected_before_queueing() {
        let handle = SessionHandle::new();
        for command in [
            SessionCommand::NetworkScan {
                timeout_ms: 0,
                tune: false,
            },
            SessionCommand::connect("sim:0".into(), 0),
            SessionCommand::Auto {
                continuous: false,
                job: None,
                max_exposure_us: Some(f64::NAN),
                region: None,
            },
            SessionCommand::Auto {
                continuous: false,
                job: None,
                max_exposure_us: Some(0.0),
                region: None,
            },
            SessionCommand::ReadMemory {
                address: u64::MAX,
                length: 2,
            },
            SessionCommand::WriteMemory {
                address: 0,
                data: vec![],
            },
            SessionCommand::Frame {
                min_free: None,
                fresh: 12,
                timeout_ms: 3000,
                max_dimension: 1024,
                format: Default::default(),
                region: None,
                preview: None,
                warmup_ms: None,
            },
            SessionCommand::Frame {
                min_free: None,
                fresh: 1,
                timeout_ms: 3000,
                max_dimension: 1024,
                format: Default::default(),
                region: Some([0.5, 0.5, 0.5, f64::INFINITY]),
                preview: None,
                warmup_ms: None,
            },
            SessionCommand::Wait {
                job: "auto-0".into(),
                after_seq: 0,
                timeout_ms: 1,
            },
            SessionCommand::Wait {
                job: "auto-1".into(),
                after_seq: 0,
                timeout_ms: 25_001,
            },
            SessionCommand::StopJob { job: "auto".into() },
        ] {
            assert!(handle.submit(command).is_err());
        }
        let snapshot = handle.snapshot();
        assert!(snapshot.cameras.is_empty());
        assert!(snapshot.logs.is_empty());
        handle.shutdown();
    }

    fn width_command() -> SessionCommand {
        SessionCommand::SetValues {
            values: vec![crate::controls::Assignment::parse("Width=128").unwrap()],
            dry_run: false,
        }
    }

    #[test]
    fn camera_command_turns_follow_submission_order_with_independent_targets() {
        let handle = SessionHandle::new();
        let first = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let second = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let third = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let other = handle
            .command_turn(Some("camera-b"), &width_command())
            .unwrap();
        first.wait();
        let (events, received) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let spawn =
            |turn: CommandTurn, number, events: mpsc::Sender<_>, started: mpsc::Sender<_>| {
                std::thread::spawn(move || {
                    started.send(number).unwrap();
                    turn.wait();
                    events.send(number).unwrap();
                })
            };
        // Start later submissions first to exercise the ordering gate rather
        // than relying on the scheduler to happen to start threads in order.
        let third_thread = spawn(third, 3, events.clone(), started.clone());
        assert_eq!(running.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        let second_thread = spawn(second, 2, events.clone(), started.clone());
        assert_eq!(running.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        let other_thread = spawn(other, 4, events, started);
        assert_eq!(running.recv_timeout(Duration::from_secs(2)).unwrap(), 4);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 4);
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(first);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        for thread in [third_thread, second_thread, other_thread] {
            thread.join().unwrap();
        }
    }

    #[test]
    fn canceled_or_panicking_command_turns_do_not_deadlock_or_bypass_earlier_commands() {
        let order = Arc::new(CommandOrder::default());
        let first = order.reserve();
        let canceled = order.reserve();
        let last = order.reserve();
        first.wait();
        // Builder::spawn drops its closure on failure. This exercises that
        // exact ownership path without depending on OS thread exhaustion.
        let unstarted_task = move || {
            canceled.wait();
        };
        drop(unstarted_task);
        assert_eq!(locked(&order.state).pending.len(), 2);
        let (started, running) = mpsc::channel();
        let (completed, received) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            last.wait();
            completed.send(()).unwrap();
        });
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        // A running task unwinding also releases the head reservation.
        assert!(
            std::panic::catch_unwind(move || {
                let _first = first;
                panic!("simulated command failure");
            })
            .is_err()
        );
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
        assert!(locked(&order.state).pending.is_empty());
        order.reserve().wait();
    }

    #[test]
    fn submission_threads_obey_the_target_gate_and_allow_other_targets_to_finish() {
        let handle = SessionHandle::new();
        let running = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        running.wait();
        // Disconnected-camera errors are enough to exercise the real
        // submission thread without launching a subprocess in a unit test.
        let blocked = handle.submit_to("camera-a", width_command()).unwrap();
        let independent = handle.submit_to("camera-b", width_command()).unwrap();
        assert!(
            independent
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(matches!(
            blocked.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(running);
        assert!(
            blocked
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        // Both failures are logged, and status adds nothing.
        let logs = handle.snapshot().logs;
        assert_eq!(logs.len(), 2);
        assert!(
            logs.iter()
                .all(|entry| entry.level == "error" && entry.unix_ms > 0)
        );
        handle.request(SessionCommand::Status).unwrap();
        assert_eq!(handle.snapshot().logs, logs);
        handle.shutdown();
    }

    #[test]
    fn frame_inspections_wait_their_turn_outside_the_action_queue() {
        let handle = SessionHandle::new();
        let frame = || SessionCommand::Frame {
            min_free: None,
            fresh: 1,
            timeout_ms: 50,
            max_dimension: 0,
            format: Default::default(),
            region: None,
            preview: None,
            warmup_ms: None,
        };
        let running = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        running.wait();
        let actions: Vec<_> = (0..MAX_PENDING)
            .map(|_| handle.submit_to("camera-a", width_command()).unwrap())
            .collect();
        assert!(handle.submit_to("camera-a", width_command()).is_err());
        // A full action queue does not refuse an inspection, but the
        // inspection still waits for the camera's earlier commands.
        let inspections: Vec<_> = (0..MAX_INSPECTIONS)
            .map(|_| handle.submit_to("camera-a", frame()).unwrap())
            .collect();
        assert!(
            handle
                .submit_to("camera-a", frame())
                .unwrap_err()
                .to_string()
                .contains("too many frame inspections")
        );
        assert!(matches!(
            inspections[0].recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(running);
        for reply in actions.into_iter().chain(inspections) {
            assert!(reply.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        }
        // Inspections are observations: no session log line.
        let logs = handle.snapshot().logs.len();
        let error = handle.request_to("camera-a", frame()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "camera 'camera-a' not connected in this session"
        );
        assert_eq!(handle.snapshot().logs.len(), logs);
        assert_eq!(handle.inner.inspecting.load(Ordering::Acquire), 0);
        handle.shutdown();
    }

    #[test]
    fn status_remains_visible_when_action_queue_is_full() {
        let handle = SessionHandle::new();
        let running = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        running.wait();
        let replies: Vec<_> = (0..MAX_PENDING)
            .map(|_| handle.submit_to("camera-a", width_command()).unwrap())
            .collect();
        assert!(
            handle
                .submit_to("camera-a", width_command())
                .unwrap_err()
                .to_string()
                .contains("queue is full")
        );
        let started = Instant::now();
        let status = handle.request(SessionCommand::Status).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(status["cameras"], json!([]));
        drop(running);
        for reply in replies {
            assert!(reply.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        }
        handle.shutdown();
    }

    #[test]
    fn camera_aliases_and_lifecycle_commands_share_a_gate_but_discovery_is_separate() {
        let handle = SessionHandle::new();
        handle.set_devices_for_test(vec![CameraInfo {
            id: "camera-a".into(),
            transport: crate::types::Transport::GigE,
            vendor: "Test".into(),
            model: "Test".into(),
            serial: "serial-a".into(),
            address: Some("192.0.2.1".into()),
            profile: Default::default(),
        }]);
        let set = handle
            .command_turn(Some("camera-a"), &width_command())
            .unwrap();
        let alias = handle
            .command_turn(Some("serial-a"), &width_command())
            .unwrap();
        let disconnect = handle
            .command_turn(Some("192.0.2.1"), &SessionCommand::Disconnect)
            .unwrap();
        let connect = handle
            .command_turn(None, &SessionCommand::connect("serial-a".into(), 1000))
            .unwrap();
        for turn in [&alias, &disconnect, &connect] {
            assert!(Arc::ptr_eq(&set.order, &turn.order));
        }
        let discover = SessionCommand::NetworkScan {
            timeout_ms: 1000,
            tune: false,
        };
        let first_discovery = handle.command_turn(None, &discover).unwrap();
        let second_discovery = handle.command_turn(None, &discover).unwrap();
        assert!(Arc::ptr_eq(&first_discovery.order, &second_discovery.order));
        assert!(!Arc::ptr_eq(&set.order, &first_discovery.order));
        for inline in [
            SessionCommand::Status,
            SessionCommand::Jobs,
            SessionCommand::StopJob {
                job: "direct-1".into(),
            },
        ] {
            assert!(handle.command_turn(Some("serial-a"), &inline).is_none());
        }
    }

    #[test]
    fn the_session_log_keeps_its_newest_lines_in_arrival_order() {
        let mut log = SessionLog::default();
        // Each line is older than the one before: arrival decides the order.
        for i in 0..LOG_LINES as u64 + 50 {
            log.note(LogEntry {
                unix_ms: 1_000 - i,
                level: "info".into(),
                message: format!("line {i}"),
            });
        }
        let lines = log.lines();
        assert_eq!(lines.len(), LOG_LINES);
        assert_eq!(lines[0].message, "line 50");
        assert_eq!(
            lines[LOG_LINES - 1].message,
            format!("line {}", LOG_LINES + 49)
        );
    }
    const TEST_WORKER: &str = "CAPTUREFAB_TEST_WORKER";
    /// This test binary as a camera worker, for `test_worker`.
    #[test]
    fn serve_as_test_worker() {
        if std::env::var_os(TEST_WORKER).is_none() {
            return;
        }
        run_worker(std::iter::empty()).unwrap();
    }
    /// A camera worker run by this test binary, whose events go nowhere.
    #[cfg(unix)]
    fn test_worker() -> Worker {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "session::tests::serve_as_test_worker",
                "--nocapture",
            ])
            .env(TEST_WORKER, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn().unwrap();
        Worker::attach(child, Arc::new(|_, _| {})).unwrap()
    }
    #[test]
    #[cfg(unix)]
    fn worker_error_categories_survive_the_pipe() {
        let worker = test_worker();
        worker.status(Duration::from_secs(5)).unwrap();
        for _ in 0..2 {
            // The selector mentions timeout, but naming a camera hub is a
            // usage error. Its type must survive the worker pipe.
            let error = worker
                .call(SessionCommand::connect(
                    "hub:merge-test/camera.timeout".into(),
                    100,
                ))
                .unwrap_err();
            assert_eq!(crate::types::error_transport_code(&error), Some("usage"));
            worker.status(Duration::from_secs(5)).unwrap();
        }
        let connected = worker
            .call(SessionCommand::connect("sim:0".into(), 1000))
            .unwrap();
        assert_eq!(connected["connected"]["id"], "sim:0");
        // A request cancelled while it waits for a frame ends at once.
        worker
            .call(SessionCommand::SetValues {
                values: vec![
                    crate::controls::Assignment::parse("TriggerMode=On").unwrap(),
                    crate::controls::Assignment::parse("TriggerSource=Line0").unwrap(),
                ],
                dry_run: false,
            })
            .unwrap();
        let (id, reply) = worker
            .submit(
                SessionCommand::Frame {
                    min_free: None,
                    fresh: 1,
                    timeout_ms: 10_000,
                    max_dimension: 0,
                    format: Default::default(),
                    region: None,
                    preview: None,
                    warmup_ms: None,
                }
                .into(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let asked = Instant::now();
        worker.cancel(id);
        let error = worker.wait(&reply).unwrap_err();
        assert!(asked.elapsed() < Duration::from_secs(5));
        assert_eq!(crate::cli::error_code(&error).0, "interrupted", "{error:#}");
        worker.close();
        assert!(worker.wait_exit(Instant::now() + Duration::from_secs(10)));
        assert!(is_stopping(&worker.exit_error().unwrap()));
    }

    #[test]
    fn sign_ins_go_only_with_camera_addresses_that_take_them() {
        let login = || Some(crate::logins::CameraLogin::new("admin", "pw").unwrap());
        let connect = |camera: &str| SessionCommand::Connect {
            camera: camera.into(),
            timeout_ms: 1000,
            login: login(),
            saved_login: true,
        };
        for camera in [
            "rtsp://cam/live",
            "rtsps://cam/live",
            "onvif://cam",
            "onvif:http://cam/onvif/device_service",
            "urn:uuid:1234",
            "ACME-SERIAL-1",
        ] {
            connect(camera).validate().unwrap();
        }
        for camera in [
            "sim:0",
            "gige:192.168.1.10",
            "192.168.1.10",
            "hub:home/front",
            "rtsp://admin:x@cam/live",
            "/tmp/clip.mp4",
            "file:///tmp/clip.mp4",
            "avfoundation:Camera",
        ] {
            let error = connect(camera).validate().unwrap_err();
            assert_eq!(
                crate::types::error_transport_code(&error),
                Some("usage"),
                "{camera}: {error:#}"
            );
        }
        let text = r#"{"op":"connect","camera":"rtsp://cam/x","timeout_ms":1000,"login":{"user":"a","password":"b"},"saved_login":false}"#;
        let command = SessionCommand::from_json_slice(text.as_bytes()).unwrap();
        assert!(matches!(
            &command,
            SessionCommand::Connect { login: Some(login), saved_login: false, .. } if login.user == "a"
        ));
        assert_eq!(serde_json::to_string(&command).unwrap(), text);
        let plain = serde_json::to_string(&SessionCommand::connect("sim:0".into(), 5)).unwrap();
        assert_eq!(plain, r#"{"op":"connect","camera":"sim:0","timeout_ms":5}"#);
        assert!(
            SessionCommand::from_json_slice(
                br#"{"op":"connect","camera":"sim:0","timeout_ms":5,"bogus":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn onvif_serials_resolve_to_their_address() {
        let handle = SessionHandle::new();
        handle.set_devices_for_test(vec![CameraInfo {
            id: "onvif:http://session-onvif.invalid/onvif/device_service".into(),
            transport: crate::types::Transport::Media,
            vendor: "ONVIF".into(),
            model: "Cam".into(),
            serial: "urn:uuid:session".into(),
            address: None,
            profile: Default::default(),
        }]);
        assert_eq!(
            handle.onvif_alias("urn:uuid:session".into()),
            "onvif:http://session-onvif.invalid/onvif/device_service"
        );
        assert_eq!(handle.onvif_alias("sim:0".into()), "sim:0");
        handle.shutdown();
    }
    #[test]
    fn setup_commands_parse_strictly_and_validate_their_shape() {
        for valid in [
            r#"{"op":"read_setup"}"#,
            r#"{"op":"setup_diff","name":"bench"}"#,
            r#"{"op":"setup_diff","name":"bench","allow_other_model":true}"#,
            r#"{"op":"apply_setup","name":"bench","strict":true}"#,
        ] {
            SessionCommand::from_json_slice(valid.as_bytes())
                .unwrap_or_else(|e| panic!("{valid}: {e}"))
                .validate()
                .unwrap_or_else(|e| panic!("{valid}: {e}"));
        }
        for invalid in [
            r#"{"op":"read_setup","name":"bench"}"#,
            r#"{"op":"setup_diff","name":"bench","extra":1}"#,
            r#"{"op":"apply_setup","name":"bench","name":"other"}"#,
            // The two-phase confirmation and digest pinning are gone.
            r#"{"op":"apply_setup","name":"bench","confirmed_disruptive":["TriggerMode"]}"#,
            r#"{"op":"apply_setup","name":"bench","digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}"#,
        ] {
            assert!(
                SessionCommand::from_json_slice(invalid.as_bytes()).is_err(),
                "{invalid}"
            );
        }
        for invalid in [
            r#"{"op":"apply_setup"}"#,
            r#"{"op":"apply_setup","name":"con"}"#,
            r#"{"op":"setup_diff","name":"bench","setup":{"version":1,"name":"bench","saved_at":"x","camera":{"transport":"simulator"},"values":[{"feature":"Width","value":1}]}}"#,
            r#"{"op":"setup_diff","setup":{"version":1,"name":"bench","saved_at":"x","camera":{"transport":"simulator"},"values":[{"feature":"GevSCDA","value":1}]}}"#,
        ] {
            let command = SessionCommand::from_json_slice(invalid.as_bytes()).unwrap();
            assert!(command.validate().is_err(), "{invalid}");
        }
        assert_eq!(
            default_request_timeout(
                &SessionCommand::from_json_slice(br#"{"op":"apply_setup","name":"b"}"#).unwrap()
            ),
            Duration::from_secs(240)
        );
        assert_eq!(
            default_request_timeout(&SessionCommand::ReadSetup),
            Duration::from_secs(60)
        );
    }
    #[test]
    fn named_setups_resolve_in_the_coordinator() {
        let h = crate::engine::WorkerHandle::new();
        h.request(SessionCommand::connect("sim:0".into(), 1000))
            .unwrap();
        let reading =
            serde_json::from_value(h.request(SessionCommand::ReadSetup).unwrap()).unwrap();
        h.shutdown();
        let name = format!("resolve-{}", std::process::id());
        let setup = crate::setup::from_read(&name, None, reading).unwrap();
        let store = crate::setup::Store::session().unwrap();
        store.save(&setup, true).unwrap();
        let apply = SessionCommand::ApplySetup {
            name: Some(name.clone()),
            setup: None,
            allow_other_model: false,
            strict: false,
        };
        let resolved = crate::setup::resolve(apply).unwrap();
        assert!(
            matches!(&resolved, SessionCommand::ApplySetup { name: None, setup: Some(s), .. } if *s == setup)
        );
        resolved.validate().unwrap();
        // A missing setup fails on the command thread, before the worker.
        let handle = SessionHandle::new();
        let missing = handle
            .request(SessionCommand::SetupDiff {
                name: Some("no-such-setup".into()),
                setup: None,
                allow_other_model: false,
            })
            .unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&missing),
            Some("unavailable")
        );
        handle.shutdown();
        store.remove(&name).unwrap();
    }
    #[test]
    fn job_ids_name_this_session() {
        let handle = SessionHandle::new();
        assert_eq!(handle.job_number("direct-3").unwrap(), 3);
        for other in ["auto-3", "direct", "direct-0"] {
            assert_eq!(
                crate::cli::error_code(&handle.job_number(other).unwrap_err()),
                ("usage", 2),
                "{other}"
            );
        }
        assert_eq!(split_job("line-1-12").unwrap(), ("line-1", 12));
        let missing = handle
            .request(SessionCommand::StopJob {
                job: "direct-9".into(),
            })
            .unwrap_err();
        assert!(missing.to_string().contains("has no job"), "{missing}");
        handle.shutdown();
    }
}
