//! Multi-camera coordinator: one isolated process per camera, shared-memory pixels.
use crate::{
    genicam::FeatureInfo,
    shared_memory::{DEFAULT_CAPACITY, FrameMeta, RingCounters, SharedRing},
    types::{CameraInfo, Categorized, ErrorKind, Frame},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::{
        Arc, Condvar, Mutex, PoisonError, RwLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionCommand {
    Discover {
        timeout_ms: u64,
        simulated: bool,
    },
    /// Inspect all GigE paths; with `tune`, apply bandwidth sharing to the
    /// connected cameras together.
    NetworkScan {
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tune: bool,
    },
    /// Per-camera worker operation used by the network scan coordinator.
    OptimizeNetwork {
        bytes_per_second: u64,
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
    Get {
        feature: String,
    },
    Set {
        feature: String,
        value: String,
    },
    /// Write several features in order as one camera change: a failure
    /// restores every value.
    SetMany {
        values: Vec<FeatureValue>,
    },
    Execute {
        feature: String,
    },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        balance: Option<f64>,
    },
    Manual {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        revert: bool,
    },
    Capture {
        output: String,
        count: u32,
        timeout_ms: u64,
        format: String,
        #[serde(default)]
        storage: crate::storage::StoragePolicy,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
        /// The caller's UTC offset in seconds, for {date} and {time}.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utc_offset_s: Option<i32>,
    },
    Schedule {
        output: String,
        count: u32,
        timeout_ms: u64,
        format: String,
        first_at_ms: u64,
        interval_ms: u64,
        #[serde(default)]
        storage: crate::storage::StoragePolicy,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
        /// The caller's UTC offset in seconds, for {date} and {time}.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utc_offset_s: Option<i32>,
        /// JPEG quality 1..100; 90 when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
        /// The job's number, which the coordinator assigns so that jobs are
        /// numbered across a session's cameras.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<u64>,
    },
    Jobs,
    CancelJob {
        id: u64,
    },
    Status,
    /// Newest frame of one camera as a bounded preview plus measurements.
    /// Never saves files or changes settings; a stopped camera acquires one
    /// frame (`Grab`) and stays stopped.
    Frame {
        /// Frames to wait for after the request; 0 accepts the newest.
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
    },
    /// Worker-only: acquire one frame into the shared ring without saving it.
    Grab {
        timeout_ms: u64,
    },
    Features,
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
        #[serde(default)]
        storage: crate::storage::StoragePolicy,
        #[serde(default = "crate::media::default_recording_cap")]
        max_file_bytes: u64,
        /// What to do when an output name without {seq} exists; fail by default.
        #[serde(default, skip_serializing_if = "crate::naming::OnExists::is_fail")]
        on_exists: crate::naming::OnExists,
        /// Tags such as line, station, part, lot and note.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        context: crate::context::Context,
        /// Stop after this many milliseconds. The camera's worker ends the
        /// recording or stream itself, as stop-forward would, so it ends on
        /// time even when the client that started it has gone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// The caller's UTC offset in seconds, for {date} and {time}.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utc_offset_s: Option<i32>,
    },
    StopForward,
    /// Stop only the forward whose opaque worker-issued identity still matches.
    StopForwardIf {
        forwarding_id: String,
    },
}
/// One feature assignment of `SetMany`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureValue {
    pub feature: String,
    pub value: String,
}
/// Most values one `SetMany` or `SetValues` writes.
pub const MAX_SET_MANY: usize = 32;
/// Most names one `GetValues` reads.
pub const MAX_NAMES: usize = 64;
/// The opaque identity of one worker-created recording or forward.
pub fn valid_forwarding_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
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
                    | "features"
                    | "xml"
                    | "stop_forward"
                    | "read_setup"
                    | "info"
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
            Self::Discover { timeout_ms, .. } | Self::NetworkScan { timeout_ms, .. } => {
                validate_timeout(*timeout_ms)?;
            }
            Self::OptimizeNetwork { bytes_per_second } => {
                ensure!(
                    (1..=125_000_000_000).contains(bytes_per_second),
                    "network budget must be 1..125000000000 bytes per second"
                );
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
                frame_capacity()?;
            }
            Self::Get { feature } | Self::Execute { feature } | Self::Set { feature, .. } => {
                validate_feature(feature)?;
            }
            Self::SetMany { values } => {
                ensure!(
                    (1..=MAX_SET_MANY).contains(&values.len()),
                    "set_many takes 1..32 feature values"
                );
                for value in values {
                    validate_feature(&value.feature)?;
                }
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
                if !(1..=MAX_SET_MANY).contains(&values.len()) {
                    return Err(crate::types::Usage(format!(
                        "set writes 1..{MAX_SET_MANY} values at once"
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
                ..
            } => crate::inspect::validate(*fresh, *timeout_ms, *max_dimension, *region)?,
            Self::Grab { timeout_ms } => validate_timeout(*timeout_ms)?,
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
                balance: Some(balance),
            } => {
                ensure!(
                    balance.is_finite() && (0.0..=1.0).contains(balance),
                    "auto balance must be a finite number from 0 (quality) to 1 (frame rate)"
                );
            }
            Self::Capture {
                output,
                count,
                timeout_ms,
                format,
                storage,
                on_exists,
                context,
                utc_offset_s,
            }
            | Self::Schedule {
                output,
                count,
                timeout_ms,
                format,
                storage,
                on_exists,
                context,
                utc_offset_s,
                ..
            } => {
                validate_frame_wait(*timeout_ms)?;
                crate::naming::validate_offset(*utc_offset_s)?;
                ensure!((1..=100_000).contains(count), "count must be 1..100000");
                ensure!(
                    ["png", "jpeg", "raw", "pgm", "ppm"].contains(&format.as_str()),
                    "format must be png, jpeg, raw, pgm or ppm"
                );
                storage.validate()?;
                crate::engine::validate_capture_output(output)?;
                crate::naming::validate_request(output, *count, *on_exists, context)?;
                if matches!(self, Self::Schedule { .. }) {
                    ensure!(
                        output != "-",
                        "scheduled capture requires a file or directory"
                    );
                } else {
                    ensure!(
                        output != "-" || *count == 1,
                        "stdout output needs one frame"
                    );
                }
                if let Self::Schedule {
                    first_at_ms,
                    interval_ms,
                    quality,
                    job,
                    ..
                } = self
                {
                    crate::scheduling::validate_schedule(*first_at_ms, *interval_ms, *count)?;
                    ensure!(
                        quality.is_none_or(|quality| (1..=100).contains(&quality)),
                        "JPEG quality must be 1..100"
                    );
                    ensure!(*job != Some(0), "capture job ID must be positive");
                }
            }
            Self::CancelJob { id } => ensure!(*id > 0, "capture job ID must be positive"),
            Self::StopForwardIf { forwarding_id } => ensure!(
                valid_forwarding_id(forwarding_id),
                "forwarding ID must be 32 lowercase hexadecimal characters"
            ),
            Self::ReadMemory { address, length } => validate_memory(*address, *length)?,
            Self::WriteMemory { address, data } => validate_memory(*address, data.len())?,
            Self::Forward {
                output,
                codec,
                encoder,
                fps,
                bitrate,
                storage,
                max_file_bytes,
                on_exists,
                context,
                duration_ms,
                utc_offset_s,
            } => {
                crate::naming::validate_offset(*utc_offset_s)?;
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
                    storage: storage.clone(),
                    max_file_bytes: *max_file_bytes,
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
            count, timeout_ms, ..
        } => timeout_ms
            .saturating_mul(u64::from(*count))
            .saturating_add(60_000),
        // Waiting for the camera's turn, or starting a stopped camera and
        // settling its auto exposure, can outlast the frame wait itself.
        SessionCommand::Frame { timeout_ms, .. } => timeout_ms.saturating_add(15_000),
        // The worker bounds the writes to 90 s; a rollback may take as long,
        // and restarting the stream follows.
        SessionCommand::ApplySetup { .. } => 240_000,
        _ => 60_000,
    })
}
/// A capture's wait for each frame: up to ten years, which `none` (wait
/// for a trigger until stopped) asks for.
pub fn validate_frame_wait(timeout_ms: u64) -> Result<()> {
    ensure!(
        (1..=crate::scheduling::MAX_DURATION_MS).contains(&timeout_ms),
        "frame wait must be 1 ms to 10 years"
    );
    Ok(())
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
/// The size of each slot of a worker's shared ring.
fn frame_capacity() -> Result<usize> {
    let capacity = env_usize("CAPTUREFAB_FRAME_BYTES", DEFAULT_CAPACITY)?;
    ensure!(
        (4096..=256 * 1024 * 1024).contains(&capacity) && capacity.is_multiple_of(64),
        "CAPTUREFAB_FRAME_BYTES must be 4096..268435456, aligned to 64 bytes"
    );
    Ok(capacity)
}
/// Lines the session log keeps.
const LOG_LINES: usize = 200;
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
        Some(camera) => format!("[{camera}] {}", entry.message),
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
/// Compact delivery counters for one exact recording or forward. Final
/// counters remain visible after it stops; the next forward replaces them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardingProgress {
    pub id: String,
    pub accepted: u64,
    pub written: u64,
    pub dropped: u64,
    pub late: u64,
    pub stalled: bool,
    /// The finished local file, once a recording that wrote one stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<FinishedRecording>,
}
/// Where a stopped recording's file is and how much it holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishedRecording {
    pub path: String,
    pub bytes: u64,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub encoder: String,
}
/// A camera worker's state, which it sends with every reply: its camera,
/// stream, recording and jobs. Its features stay in the worker, and a reply
/// carries only the log lines added since the one before.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub connected: Option<CameraInfo>,
    /// The features as last walked; `features` returns them.
    #[serde(skip)]
    pub features: Vec<FeatureInfo>,
    pub streaming: bool,
    pub frames: u64,
    /// Payload bytes acquired by this worker, before preview downsampling or encoding.
    #[serde(default)]
    pub received_bytes: u64,
    pub dropped: u64,
    pub fps: f64,
    pub last_error: Option<String>,
    /// When the worker last logged an error, UTC Unix milliseconds, so the
    /// same error logged again can be told from the one before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_logged_ms: Option<u64>,
    pub logs: Vec<LogEntry>,
    /// Lines logged in all, which tells a reply's new lines from the rest.
    #[serde(skip)]
    pub logged: u64,
    #[serde(default)]
    pub forwarding: Option<String>,
    /// Opaque identity of this exact recording or forward, including replacements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_progress: Option<ForwardingProgress>,
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
    #[serde(default)]
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
    /// When this camera last logged an error, UTC Unix milliseconds, so the
    /// same error logged again can be told from the one before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_logged_ms: Option<u64>,
    pub worker_pid: u32,
    #[serde(default)]
    pub forwarding: Option<String>,
    /// Opaque identity of this exact recording or forward, including replacements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_progress: Option<ForwardingProgress>,
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
    #[serde(default)]
    pub jobs: Vec<crate::scheduling::CaptureJob>,
    #[serde(default)]
    pub auto: Option<crate::auto::AutoStatus>,
    #[serde(default)]
    pub transport: Option<crate::types::TransportStats>,
    /// Frames lost on the way: the worker's receive errors plus the
    /// transport's incomplete and missing frames.
    #[serde(default)]
    pub lost: u64,
    /// Preview frames the shared ring replaced while a reader was reading,
    /// because a newer one came first; `dropped` includes them. Captures and
    /// recordings still receive those frames.
    #[serde(default)]
    pub preview_skipped: u64,
    /// The session job number of the current or last recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_job: Option<u64>,
    /// The worker was busy, so this entry repeats its last report. Kept for
    /// this process's own use; not serialized.
    #[serde(skip)]
    pub stale: bool,
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
fn merge_session_metadata(snapshot: &mut SessionSnapshot, current: &SessionSnapshot) {
    // A busy worker's entry was copied from the stored snapshot before this
    // one was built; the command that kept it busy may have stored a newer
    // one since, such as the job it just scheduled.
    for camera in snapshot.cameras.iter_mut().filter(|camera| camera.stale) {
        if let Some(newer) = current.cameras.iter().find(|c| c.info.id == camera.info.id) {
            *camera = CameraSnapshot {
                stale: true,
                ..newer.clone()
            };
        }
    }
    // Polling does worker I/O outside the state lock. Preserve a discovery
    // that completed while that poll was in flight.
    snapshot.devices = current.devices.clone();
}
type Reply = Receiver<Result<Value>>;
/// A camera's frames: the newest-frame cache behind its own lock, and the
/// ring's counters beside it, which sequence polls read without that lock.
struct Feed {
    counters: RingCounters,
    frames: Mutex<FrameFeed>,
}
impl Feed {
    fn new(ring: SharedRing) -> Self {
        Self {
            counters: ring.counters(),
            frames: Mutex::new(FrameFeed::new(ring)),
        }
    }
}
/// Newest-frame cache fed by a worker's shared ring. It has its own lock so
/// inspections never wait behind a slow command on the worker's stdio pipe.
/// Lock order: a `Process` lock may be held while taking a feed lock, never
/// the reverse.
struct FrameFeed {
    ring: SharedRing,
    latest: Option<Arc<Frame>>,
    /// The acquisition sequence number and publish time of `latest`.
    latest_seq: u32,
    latest_received_ms: u64,
}
impl FrameFeed {
    fn new(ring: SharedRing) -> Self {
        Self {
            ring,
            latest: None,
            latest_seq: 0,
            latest_received_ms: 0,
        }
    }
    fn poll(&mut self, watch: bool) -> Option<Arc<Frame>> {
        // Overwrite the cached frame in place when no consumer still holds it,
        // so steady-state streaming reuses one pixel allocation.
        let taken = match self.latest.as_mut().and_then(Arc::get_mut) {
            Some(frame) => self.ring.take_into(frame, watch),
            None => {
                let mut frame = Frame::default();
                let taken = self.ring.take_into(&mut frame, watch);
                if matches!(taken, Ok(Some(_))) {
                    self.latest = Some(Arc::new(frame));
                }
                taken
            }
        };
        match taken {
            Ok(Some(meta)) => {
                self.latest_seq = meta.seq;
                self.latest_received_ms = meta.received_ms;
            }
            Ok(None) => {}
            Err(e) => eprintln!("capturefab: ignoring unreadable shared frame: {e:#}"),
        }
        self.latest.clone()
    }
    fn latest_meta(&self) -> FrameMeta {
        FrameMeta {
            seq: self.latest_seq,
            received_ms: self.latest_received_ms,
        }
    }
}
/// A camera worker that exited while the session still held it.
fn worker_crashed(status: std::process::ExitStatus) -> anyhow::Error {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    Categorized::new(
        ErrorKind::WorkerCrashed,
        format!("camera worker exited ({status})"),
    )
    .hint("try again; a session keeps the exited worker until capturefab session stop")
    .details(json!({"exit": status.code(), "signal": signal}))
    .into()
}

/// The connected cameras an error offers instead, with credentials redacted.
fn candidates<'a>(ids: impl Iterator<Item = &'a String>) -> Vec<Value> {
    ids.map(|id| json!({"id": crate::media::redact_url(id)}))
        .collect()
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
#[derive(Serialize)]
struct WorkerReply<'a> {
    ok: bool,
    /// Omitted for Status, whose result is the snapshot itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The error's reason, hint and details, so the parent reports it the
    /// same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<crate::types::ErrorTag>,
    snapshot: &'a WorkerSnapshot,
    /// Files saved since the previous reply. The coordinator does not read
    /// them; draining the worker's log in every reply keeps it bounded.
    #[serde(skip_serializing_if = "crate::engine::saved::SavedBatch::is_empty")]
    saved: crate::engine::saved::SavedBatch,
}
/// A worker reply as the coordinator reads it. `S` is the snapshot, or
/// `IgnoredAny` to keep the rest of a reply whose snapshot is unreadable.
#[derive(Deserialize)]
struct ParentReply<S = WorkerSnapshot> {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    tag: Option<crate::types::ErrorTag>,
    #[serde(default)]
    snapshot: Option<S>,
}
struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    feed: Arc<Feed>,
    /// The worker's latest snapshot, without its log lines.
    report: WorkerSnapshot,
    /// The session log, which the worker's lines join as they arrive.
    log: Arc<Mutex<SessionLog>>,
}
/// A camera worker leads a process group of its own, which its FFmpeg and helper
/// processes join. A terminal sends Ctrl-C, and the shell sends the SIGHUP of
/// a closed terminal, to the whole foreground group: in this one, they would
/// end a recording before this process could finish it, catalog it and queue
/// its local save. The worker still exits when told to or when its stdin closes.
fn worker_command(executable: std::path::PathBuf) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command
        .arg("__worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // The console's Ctrl-C reaches every process attached to it, except
    // those in a new process group, and the processes they start.
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std::os::windows::process::CommandExt::creation_flags(
            &mut command,
            CREATE_NEW_PROCESS_GROUP,
        );
    }
    command
}

impl Process {
    fn spawn(log: Arc<Mutex<SessionLog>>) -> Result<Self> {
        let capacity = frame_capacity()?;
        let dir = crate::ipc::runtime_dir();
        crate::ipc::ensure_private_dir(&dir)?;
        // Ring files are left behind only when a process dies; clear them
        // before this coordinator's first camera.
        static SWEEP: std::sync::Once = std::sync::Once::new();
        SWEEP.call_once(|| crate::shared_memory::sweep(&dir));
        let ring = SharedRing::create(&dir, capacity)?;
        let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE")
            .map(std::path::PathBuf::from)
            .unwrap_or(std::env::current_exe()?);
        let mut command = worker_command(executable);
        ring.pass_to(&mut command);
        let child = command.spawn().context("launch camera capture process")?;
        Self::attach(child, ring, log)
    }
    /// Take over a worker started with `ring`.
    fn attach(mut child: Child, mut ring: SharedRing, log: Arc<Mutex<SessionLog>>) -> Result<Self> {
        ring.passed();
        let input = child.stdin.take().context("worker stdin")?;
        let output = BufReader::new(child.stdout.take().context("worker stdout")?);
        let feed = Arc::new(Feed::new(ring));
        Ok(Self {
            child,
            input,
            output,
            feed,
            report: WorkerSnapshot::default(),
            log,
        })
    }
    /// The worker exited: what it was doing ended with `error`, which the
    /// next status shows, so nobody waits for its jobs or recording.
    fn lost(&mut self, error: anyhow::Error) -> anyhow::Error {
        let report = &mut self.report;
        report.streaming = false;
        report.forwarding = None;
        report.forwarding_id = None;
        report.recording_path = None;
        for job in report.jobs.iter_mut().filter(|job| job.active()) {
            job.end("failed", Some(&error));
        }
        report.last_error = Some(format!("{error:#}"));
        error
    }
    /// Send one command and keep the reply's snapshot.
    fn call(&mut self, command: SessionCommand) -> Result<Value> {
        if let Some(status) = self.child.try_wait()? {
            return Err(self.lost(worker_crashed(status)));
        }
        serde_json::to_writer(&mut self.input, &command)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let mut data = Vec::new();
        use std::io::Read;
        self.output
            .by_ref()
            .take(16 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut data)?;
        if data.is_empty() {
            // The pipe closes as the worker dies; give the exit a moment to
            // be reaped so the error can say how it ended.
            for _ in 0..20 {
                if let Some(status) = self.child.try_wait()? {
                    return Err(self.lost(worker_crashed(status)));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        ensure!(
            data.len() <= 16 * 1024 * 1024 && data.last() == Some(&b'\n'),
            "camera worker exited or sent an invalid response"
        );
        let reply = match serde_json::from_slice::<ParentReply>(&data) {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("capturefab: ignoring unreadable worker snapshot: {e}");
                let reply: ParentReply<serde::de::IgnoredAny> = serde_json::from_slice(&data)?;
                ParentReply {
                    ok: reply.ok,
                    result: reply.result,
                    error: reply.error,
                    tag: reply.tag,
                    snapshot: None,
                }
            }
        };
        if let Some(mut snapshot) = reply.snapshot {
            let lines = std::mem::take(&mut snapshot.logs);
            if !lines.is_empty() {
                // A worker that just disconnected still names its camera.
                let camera = snapshot
                    .connected
                    .as_ref()
                    .or(self.report.connected.as_ref())
                    .map(|info| info.id.clone());
                let mut log = locked(&self.log);
                for line in lines {
                    log.note(camera_line(line, camera.as_deref()));
                }
            }
            self.report = snapshot;
        }
        if !reply.ok {
            return Err(crate::types::error_from_tag(
                reply.tag,
                reply
                    .error
                    .unwrap_or_else(|| "worker command failed".into()),
            ));
        }
        Ok(reply.result.unwrap_or_default())
    }
    /// Ask for the worker's status.
    fn refresh(&mut self) -> Result<()> {
        self.call(SessionCommand::Status).map(drop)
    }
    /// This worker's camera as session snapshots show it.
    fn camera(&self) -> Option<CameraSnapshot> {
        let preview_skipped = u64::from(self.feed.counters.preview_skipped());
        camera_of(&self.report, self.child.id(), preview_skipped)
    }
}
/// A worker's camera, from its latest snapshot, as session snapshots show it.
fn camera_of(s: &WorkerSnapshot, worker_pid: u32, preview_skipped: u64) -> Option<CameraSnapshot> {
    Some(CameraSnapshot {
        info: s.connected.clone()?,
        streaming: s.streaming,
        frames: s.frames,
        received_bytes: s.received_bytes,
        dropped: s.dropped + preview_skipped,
        fps: s.fps,
        last_error: s.last_error.clone(),
        error_logged_ms: s.error_logged_ms,
        worker_pid,
        forwarding: s.forwarding.clone(),
        forwarding_id: s.forwarding_id.clone(),
        forwarding_progress: s.forwarding_progress.clone(),
        recording_path: s.recording_path.clone(),
        forwarding_since_ms: s.forwarding_since_ms,
        forwarding_until_ms: s.forwarding_until_ms,
        jobs: s.jobs.clone(),
        auto: s.auto.clone(),
        transport: s.transport.clone(),
        lost: frames_lost(s),
        preview_skipped,
        record_job: None,
        stale: false,
    })
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.input.write_all(b"{\"shutdown\":true}\n");
        let _ = self.input.flush();
        // An idle worker exits within a few milliseconds of being told to.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut wait = Duration::from_millis(1);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Coordinator {
    state: RwLock<SessionSnapshot>,
    workers: RwLock<BTreeMap<String, Arc<Mutex<Process>>>>,
    feeds: RwLock<BTreeMap<String, Arc<Feed>>>,
    quitting: AtomicBool,
    /// Set when `shutdown` has finished; later callers wait for it.
    shut_down: (Mutex<bool>, Condvar),
    pending: AtomicUsize,
    /// Frame inspections in progress; they wait outside the action slots.
    inspecting: AtomicUsize,
    connecting: Mutex<()>,
    command_order: Mutex<BTreeMap<CommandTarget, Weak<CommandOrder>>>,
    log: Arc<Mutex<SessionLog>>,
    jobs: Mutex<JobBook>,
    /// The selectors cameras were connected by, with each camera's ID, so a
    /// client can name a camera the way it was opened.
    aliases: Mutex<BTreeMap<String, String>>,
}
/// Job numbers across a session's cameras. A capture job carries its own; a
/// recording's is kept beside its forward's identity.
#[derive(Default)]
struct JobBook {
    last: u64,
    recordings: VecDeque<(u64, String)>,
}
/// Recordings whose job numbers are remembered.
const RECORDING_JOBS: usize = 256;
impl JobBook {
    fn next(&mut self) -> u64 {
        self.last += 1;
        self.last
    }
    fn recording(&self, forwarding_id: &str) -> Option<u64> {
        self.recordings
            .iter()
            .find(|(_, id)| id == forwarding_id)
            .map(|(job, _)| *job)
    }
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
/// How often a frame inspection looks for a new frame in a camera's ring.
const FRAME_POLL: Duration = Duration::from_millis(5);
/// How often a waiting frame inspection asks an idle worker whether its
/// camera still streams.
const STREAM_CHECK: Duration = Duration::from_millis(250);
/// Each inspection may hold a full-resolution frame and its conversions.
const MAX_INSPECTIONS: usize = 8;
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
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Coordinator {
                state: RwLock::new(SessionSnapshot::default()),
                workers: RwLock::new(BTreeMap::new()),
                feeds: RwLock::new(BTreeMap::new()),
                quitting: AtomicBool::new(false),
                shut_down: (Mutex::new(false), Condvar::new()),
                pending: AtomicUsize::new(0),
                inspecting: AtomicUsize::new(0),
                connecting: Mutex::new(()),
                command_order: Mutex::new(BTreeMap::new()),
                log: Arc::default(),
                jobs: Mutex::default(),
                aliases: Mutex::default(),
            }),
        }
    }
    /// Replace the discovered cameras, as a discovery would.
    #[cfg(test)]
    pub(crate) fn set_devices_for_test(&self, devices: Vec<CameraInfo>) {
        self.edit_state().devices = devices;
    }
    /// The stored state, for an edit outside a status request.
    fn edit_state(&self) -> std::sync::RwLockWriteGuard<'_, SessionSnapshot> {
        self.inner
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
    fn loose_camera(&self, camera: String) -> String {
        if crate::camera::is_explicit_selector(&camera) {
            return camera;
        }
        let resolution = crate::camera::resolve_selector(
            &camera,
            &self
                .inner
                .state
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .devices,
        );
        match resolution {
            crate::camera::Resolution::Loose { info, matched_on } => {
                locked(&self.inner.log).note(LogEntry::now(
                    "info",
                    format!(
                        "Using {} for '{}' (matched its {matched_on})",
                        crate::media::redact_url(&info.id),
                        crate::media::redact_url(&camera)
                    ),
                ));
                info.id
            }
            _ => camera,
        }
    }
    /// Refresh every idle worker's report; busy workers report on their reply.
    fn refresh_workers(&self) {
        for (_, w) in self.workers() {
            if let Ok(mut p) = w.try_lock() {
                let _ = p.refresh();
            }
        }
    }
    /// Give a connected worker's frames and status to the session.
    fn add_worker(&self, id: String, p: Process) {
        self.inner
            .feeds
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), p.feed.clone());
        self.inner
            .workers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::new(Mutex::new(p)));
    }
    /// The ONVIF address of a discovered or connected ONVIF camera named by
    /// its ID or serial; any other selector unchanged.
    fn onvif_alias(&self, camera: String) -> String {
        if crate::onvif::is_source(&camera) || camera.contains("://") {
            return camera;
        }
        let state = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        state
            .devices
            .iter()
            .chain(state.cameras.iter().map(|camera| &camera.info))
            .find(|device| {
                crate::onvif::is_source(&device.id)
                    && (device.id == camera || device.serial == camera)
            })
            .map(|device| device.id.clone())
            .unwrap_or(camera)
    }
    fn workers(&self) -> Vec<(String, Arc<Mutex<Process>>)> {
        self.inner
            .workers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    /// The session now. Idle workers are asked for their status first; a
    /// busy one shows what it last reported.
    pub fn snapshot(&self) -> SessionSnapshot {
        self.refresh_workers();
        self.store_snapshot();
        self.inner
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    /// Build a snapshot from every worker's latest report.
    fn assemble(&self) -> SessionSnapshot {
        let mut state = SessionSnapshot {
            devices: self
                .inner
                .state
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .devices
                .clone(),
            ..Default::default()
        };
        for (id, w) in self.workers() {
            if let Ok(p) = w.try_lock() {
                state.cameras.extend(p.camera());
            } else if let Some(c) = self
                .inner
                .state
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .cameras
                .iter()
                .find(|c| c.info.id == id)
            {
                state.cameras.push(CameraSnapshot {
                    stale: true,
                    ..c.clone()
                });
            }
        }
        state
    }
    /// The one connected camera, which a command that names none addresses.
    fn only_camera(&self) -> Result<String> {
        let workers = self.inner.workers.read().unwrap_or_else(|e| e.into_inner());
        let mut ids = workers.keys();
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
                    workers.len()
                ),
            )
            .details(json!({"candidates": candidates(workers.keys())}))
            .into()),
        }
    }
    fn target(&self, target: Option<&str>) -> Result<Arc<Mutex<Process>>> {
        let id = match target {
            Some(target) => self.camera_id(target),
            None => self.only_camera()?,
        };
        let workers = self.inner.workers.read().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = workers.get(&id) {
            return Ok(w.clone());
        }
        for w in workers.values() {
            if let Ok(p) = w.try_lock()
                && p.report
                    .connected
                    .as_ref()
                    .is_some_and(|c| c.serial == id || c.address.as_deref() == Some(&id))
            {
                return Ok(w.clone());
            }
        }
        Err(Categorized::new(
            ErrorKind::CameraNotFound,
            format!(
                "camera '{}' not connected in this session",
                crate::media::redact_url(&id)
            ),
        )
        .details(json!({"candidates": candidates(workers.keys())}))
        .into())
    }
    /// The frame a capture just published, for binary stdout. Only a
    /// session of one camera, as a direct capture is, has one. A downscaled
    /// preview of a frame larger than the ring is refused rather than written
    /// in its place.
    pub fn captured_frame(&self) -> Result<Arc<Frame>> {
        let feed = {
            let feeds = self
                .inner
                .feeds
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            ensure!(feeds.len() == 1, "capture returned no frame");
            feeds
                .values()
                .next()
                .cloned()
                .context("capture returned no frame")?
        };
        let mut frames = locked(&feed.frames);
        let frame = frames.poll(false).context("capture returned no frame")?;
        ensure!(
            !frames.ring.preview_only(),
            "the frame is larger than the {} shared frame buffer; save it to a file, or raise CAPTUREFAB_FRAME_BYTES",
            crate::types::human_bytes(frames.ring.capacity() as u64)
        );
        Ok(frame)
    }
    /// Wait until camera `id` acquires a frame at least `fresh` frames after
    /// ring position `after_seq` (the ring's acquired sequence before the
    /// request), and return it with its position and publish time. `fresh` 0
    /// accepts the newest frame. None at `deadline`; fails as soon as the
    /// camera has stopped streaming with no such frame left in its ring.
    fn await_frame(
        &self,
        id: &str,
        after_seq: u32,
        fresh: u32,
        deadline: Instant,
    ) -> Result<Option<(Arc<Frame>, FrameMeta)>> {
        let id = self.camera_id(id);
        let feed = self.feed(&id).with_context(|| {
            format!(
                "camera '{}' not connected in this session",
                crate::media::redact_url(&id)
            )
        })?;
        let mut checked = Instant::now();
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            // Inspections ask for one frame at a time without leaving the
            // ring watched after they finish. Release the feed lock before
            // trying the worker's below.
            {
                let mut feed = locked(&feed.frames);
                if let Some(frame) = feed.poll(false) {
                    let meta = feed.latest_meta();
                    let ahead = meta.seq.wrapping_sub(after_seq);
                    if ahead < u32::MAX / 2 && ahead >= fresh && Instant::now() < deadline {
                        ensure!(
                            !feed.ring.preview_only(),
                            "the frame is larger than the {} shared frame buffer; raise CAPTUREFAB_FRAME_BYTES to inspect full-resolution pixels",
                            crate::types::human_bytes(feed.ring.capacity() as u64)
                        );
                        return Ok(Some((frame, meta)));
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if self.is_shutting_down() {
                return Err(stopping());
            }
            let ask = now.duration_since(checked) >= STREAM_CHECK;
            if ask {
                checked = now;
            }
            if let Some(error) = self.stopped_streaming(&id, ask) {
                anyhow::bail!(
                    "{} stopped streaming{}",
                    crate::media::redact_url(&id),
                    error.map(|error| format!(": {error}")).unwrap_or_default()
                );
            }
            feed.counters.ask();
            std::thread::sleep(FRAME_POLL.min(deadline - now));
        }
    }
    /// Some(last error) when camera `id`'s worker reports that it is not
    /// streaming, asking it first when `ask`: nothing else polls an idle
    /// worker, and a stream can end on its own. None while it streams, and
    /// while it is busy, since its report then may be out of date.
    fn stopped_streaming(&self, id: &str, ask: bool) -> Option<Option<String>> {
        let w = self
            .inner
            .workers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()?;
        let mut p = w.try_lock().ok()?;
        if ask {
            let _ = p.refresh();
        }
        (!p.report.streaming).then(|| p.report.last_error.clone())
    }
    fn feed(&self, id: &str) -> Option<Arc<Feed>> {
        let id = self.camera_id(id);
        let feeds = self
            .inner
            .feeds
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(feed) = feeds.get(&id) {
            return Some(feed.clone());
        }
        drop(feeds);
        // Other selectors resolve through the worker table.
        let w = self.target(Some(&id)).ok()?;
        let p = w.try_lock().ok()?;
        Some(p.feed.clone())
    }
    pub fn submit(&self, c: SessionCommand) -> Result<Reply> {
        self.submit_target(None, c)
    }
    pub fn submit_to(&self, camera: &str, c: SessionCommand) -> Result<Reply> {
        self.submit_target(Some(camera.to_owned()), c)
    }
    /// Resolve aliases from cached identities, without waiting for a busy
    /// worker's command pipe. All names for one camera share an ordering gate.
    fn camera_id(&self, selector: &str) -> String {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let identities = || {
            state
                .cameras
                .iter()
                .map(|camera| &camera.info)
                .chain(state.devices.iter())
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
        let mut shown = state
            .cameras
            .iter()
            .filter(|camera| crate::media::redact_url(&camera.info.id) == redacted);
        match (shown.next(), shown.next()) {
            (Some(camera), None) if redacted != selector => camera.info.id.clone(),
            _ => selector.to_owned(),
        }
    }
    /// The connected cameras as a session descriptor lists them, with
    /// credentials redacted: what clients match a selector against.
    pub fn held_cameras(&self) -> Vec<crate::ipc::HeldCamera> {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let aliases = locked(&self.inner.aliases);
        let mut held = state
            .cameras
            .iter()
            .map(|camera| {
                let info = &camera.info;
                crate::ipc::HeldCamera::new(
                    info,
                    aliases
                        .iter()
                        .filter(|(_, id)| **id == info.id)
                        .map(|(alias, _)| alias.as_str()),
                )
            })
            .collect::<Vec<_>>();
        held.sort_by(|a, b| a.id.cmp(&b.id));
        held
    }
    fn command_turn(&self, target: Option<&str>, command: &SessionCommand) -> Option<CommandTurn> {
        if matches!(command, SessionCommand::Status) {
            return None;
        }
        let target = match command {
            SessionCommand::Discover { .. } | SessionCommand::NetworkScan { .. } => {
                CommandTarget::Discovery
            }
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
    fn submit_target(&self, target: Option<String>, command: SessionCommand) -> Result<Reply> {
        command.validate()?;
        if let Some(camera) = &target {
            validate_selector(camera)?;
        }
        if self.inner.quitting.load(Ordering::Relaxed) {
            return Err(stopping());
        }
        ensure!(
            !matches!(command, SessionCommand::Grab { .. }),
            "grab is reserved for camera workers; use frame to inspect a camera"
        );
        // Status remains responsive even when every action slot is occupied.
        // It never waits for a command gate or a busy worker's pipe.
        if matches!(command, SessionCommand::Status) {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(self.process(target.as_deref(), command));
            return Ok(rx);
        }
        let inspection = matches!(command, SessionCommand::Frame { .. });
        let slot = PendingSlot::acquire(&self.inner, inspection)?;
        // A camera command that names no camera is for the only one, and
        // waits its turn behind that camera's earlier commands.
        let target = target.or_else(|| {
            if matches!(
                &command,
                SessionCommand::Connect { .. }
                    | SessionCommand::Discover { .. }
                    | SessionCommand::NetworkScan { .. }
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
                    crate::setup::resolve(command)
                        .and_then(|command| h.process(target.as_deref(), command))
                };
                // An inspection's failure is its caller's to report; a slow
                // or triggered camera must not fill the session log.
                if let Err(e) = &result
                    && !inspection
                {
                    locked(&h.inner.log).note(LogEntry::now(
                        "error",
                        crate::scrub::redact_urls(&format!("{e:#}")),
                    ));
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
    fn scan_network(&self, timeout: Duration, tune: bool) -> Result<Value> {
        self.refresh_workers();
        let before = self.assemble();
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
                                let current = self.assemble();
                                let original = before.cameras.iter().find(|item| item.info.id == *camera);
                                let unchanged = current.cameras.iter()
                                    .find(|item| item.info.id == *camera).zip(original)
                                    .is_some_and(|(now, then)| {
                                        now.worker_pid == then.worker_pid
                                            && now.info.address == then.info.address
                                    });
                                ensure!(unchanged,
                                    "Camera connection changed during the scan; scan again to apply current defaults"
                                );
                                self.request_to(
                                    camera,
                                    SessionCommand::OptimizeNetwork {
                                        bytes_per_second: *budget,
                                    },
                                )
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
        self.refresh_workers();
        let after = self.assemble();
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
                && !current.stale
                && !previous.stale
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
        self.edit_state().devices = devices;
        Ok(json!({"network_scan":report}))
    }
    fn process(&self, target: Option<&str>, command: SessionCommand) -> Result<Value> {
        match command {
            SessionCommand::NetworkScan { timeout_ms, tune } => {
                self.scan_network(Duration::from_millis(timeout_ms), tune)
            }
            SessionCommand::Status => {
                // A camera's status shows that camera alone.
                let camera = match target {
                    Some(target) => {
                        self.target(Some(target))?;
                        Some(self.camera_id(target))
                    }
                    None => None,
                };
                let mut status = self.snapshot();
                if let Some(camera) = camera {
                    status.cameras.retain(|c| c.info.id == camera);
                }
                Ok(serde_json::to_value(status)?)
            }
            SessionCommand::Frame {
                fresh,
                timeout_ms,
                max_dimension,
                format,
                region,
            } => self.inspect_frame(
                target,
                fresh,
                Duration::from_millis(timeout_ms),
                max_dimension,
                format,
                region,
            ),
            SessionCommand::Discover {
                timeout_ms,
                simulated,
            } => {
                ensure!(
                    (1..=60000).contains(&timeout_ms),
                    "timeout must be 1..60000 milliseconds"
                );
                let (devices, warnings) =
                    crate::camera::discover(Duration::from_millis(timeout_ms), simulated)?;
                self.edit_state().devices = devices.clone();
                let mut log = locked(&self.inner.log);
                for warning in &warnings {
                    log.note(LogEntry::now("warn", warning.clone()));
                }
                drop(log);
                Ok(json!({"devices":devices,"warnings":warnings}))
            }
            SessionCommand::Connect {
                camera,
                timeout_ms,
                login,
                saved_login,
            } => {
                let _connect = self
                    .inner
                    .connecting
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let typed = camera.clone();
                let remember = |id: &str| {
                    if typed != id {
                        locked(&self.inner.aliases).insert(typed.clone(), id.to_owned());
                    }
                };
                // A discovered ONVIF camera named by its serial or ID opens
                // by its ONVIF address, which is what a sign-in is saved
                // under.
                let camera = self.onvif_alias(camera);
                // A unique loose name ("C920") among cached cameras becomes
                // that camera's ID, so connected workers see one identity.
                let camera = self.loose_camera(camera);
                if login.is_some() && crate::logins::key(&camera).is_err() {
                    return Err(crate::types::Usage(
                        "sign-in is supported for rtsp://, rtsps:// and onvif: cameras".into(),
                    )
                    .into());
                }
                if let Ok(w) = self.target(Some(&camera)) {
                    let p = w.lock().unwrap_or_else(|e| e.into_inner());
                    let info = p.report.connected.clone().context("worker has no camera")?;
                    remember(&info.id);
                    return Ok(json!({"connected":info,"worker_pid":p.child.id()}));
                }
                crate::camera::validate_selector(&camera)?;
                let maximum = maximum_cameras()?;
                ensure!(
                    self.workers().len() < maximum,
                    "camera limit reached ({maximum}); disconnect a camera or increase CAPTUREFAB_MAX_CAMERAS"
                );
                let mut p = Process::spawn(self.inner.log.clone())?;
                let result = p.call(SessionCommand::Connect {
                    camera,
                    timeout_ms,
                    login,
                    saved_login,
                })?;
                let id = p
                    .report
                    .connected
                    .as_ref()
                    .context("worker connected without identity")?
                    .id
                    .clone();
                let pid = p.child.id();
                remember(&id);
                self.add_worker(id, p);
                self.store_snapshot();
                Ok(json!({"connected":result["connected"],"worker_pid":pid}))
            }
            SessionCommand::Disconnect => {
                let w = self.target(target)?;
                let p = w.lock().unwrap_or_else(|e| e.into_inner());
                let id = p
                    .report
                    .connected
                    .as_ref()
                    .context("worker has no camera")?
                    .id
                    .clone();
                drop(p);
                let result = w
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .call(SessionCommand::Disconnect)?;
                self.inner
                    .feeds
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                self.inner
                    .workers
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                locked(&self.inner.aliases).retain(|_, camera| *camera != id);
                drop(w);
                self.store_snapshot();
                Ok(result)
            }
            mut command => {
                let w = self.target(target)?;
                let schedule = matches!(command, SessionCommand::Schedule { .. });
                let forward = matches!(command, SessionCommand::Forward { .. });
                if let SessionCommand::Schedule { job, .. } = &mut command {
                    *job = Some(locked(&self.inner.jobs).next());
                }
                let (result, fresh) = {
                    let mut worker = w.lock().unwrap_or_else(|e| e.into_inner());
                    let mut result = worker.call(command);
                    if schedule
                        && let Ok(value) = &mut result
                        && let Some(fields) = value.as_object_mut()
                    {
                        fields.insert("worker_pid".into(), json!(worker.child.id()));
                    }
                    // A recording is a job too, numbered with the captures.
                    if forward
                        && let Ok(value) = &mut result
                        && let Some(id) = value["forwarding_id"].as_str().map(str::to_owned)
                    {
                        let mut book = locked(&self.inner.jobs);
                        let job = book.next();
                        book.recordings.push_back((job, id));
                        if book.recordings.len() > RECORDING_JOBS {
                            book.recordings.pop_front();
                        }
                        value["job"] = json!(job);
                    }
                    (result, worker.camera())
                };
                self.store_snapshot_with(fresh);
                result
            }
        }
    }
    /// Answer `Frame`. The camera's command turn is held, so the frame
    /// follows any change submitted before it, but the worker's lock is not:
    /// status continues while it waits.
    fn inspect_frame(
        &self,
        target: Option<&str>,
        fresh: u32,
        timeout: Duration,
        max_dimension: u32,
        format: crate::inspect::PreviewFormat,
        region: Option<[f64; 4]>,
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        let w = self.target(target)?;
        let (info, streaming, feed) = {
            let mut p = locked(&w);
            // The cached report may predate a stream that ended on its own,
            // such as one a finished schedule started.
            let _ = p.refresh();
            let info = p.report.connected.clone().context("worker has no camera")?;
            (info, p.report.streaming, p.feed.clone())
        };
        let start = feed.counters.acquired_sequence();
        // A stopped camera acquires one frame, which is new by definition.
        let (source, fresh, notes) = if streaming {
            ("live", fresh, Vec::new())
        } else {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(crate::types::frame_timeout(no_new_frame(&info.id, timeout)));
            }
            let grab = SessionCommand::Grab {
                timeout_ms: u64::try_from(remaining.as_millis())
                    .unwrap_or(u64::MAX)
                    .clamp(1, 60_000),
            };
            let grabbed = locked(&w).call(grab);
            self.store_snapshot();
            let grabbed = grabbed.map_err(|error| grab_failed(error, &info.id, timeout))?;
            let notes = if grabbed["auto_settled"] == false {
                vec!["auto exposure had not settled when this frame was taken".to_owned()]
            } else {
                Vec::new()
            };
            ("grab", 1, notes)
        };
        let (frame, meta) = self
            .await_frame(&info.id, start, fresh, deadline)?
            .ok_or_else(|| crate::types::frame_timeout(no_new_frame(&info.id, timeout)))?;
        let mut reply = serde_json::to_value(crate::inspect::inspect(
            &frame,
            info.profile.kind,
            region,
            max_dimension,
            format,
        )?)?;
        if !notes.is_empty() {
            reply["settled"] = json!(false);
        }
        if let Some(measured) = reply["notes"].as_array_mut() {
            measured.extend(notes.into_iter().map(Value::from));
        }
        reply["camera"] = json!(crate::media::redact_url(&info.id));
        reply["frame"] = json!({
            "id": frame.id,
            "seq": meta.seq,
            "width": frame.width,
            "height": frame.height,
            "pixel_format": crate::inspect::pixel_format_label(frame.pixel_format),
            "timestamp_ns": frame.timestamp_ns,
            "age_ms": crate::scheduling::now_ms().saturating_sub(meta.received_ms),
            "source": source,
            "waited_frames": meta.seq.wrapping_sub(start),
        });
        Ok(reply)
    }
    fn store_snapshot(&self) {
        self.store_snapshot_with(None);
    }
    /// Store a snapshot, with `fresh` for its camera's entry if that worker
    /// is busy again, as when a status request asks for it right after a
    /// command: the entry that command left is newer than the stored one.
    fn store_snapshot_with(&self, fresh: Option<CameraSnapshot>) {
        let mut snapshot = self.assemble();
        if let Some(fresh) = fresh
            && let Some(entry) = snapshot
                .cameras
                .iter_mut()
                .find(|camera| camera.stale && camera.info.id == fresh.info.id)
        {
            *entry = fresh;
        }
        self.commit(snapshot);
    }
    /// Store a snapshot with the session log as it is now.
    fn commit(&self, mut snapshot: SessionSnapshot) {
        {
            let book = locked(&self.inner.jobs);
            for camera in &mut snapshot.cameras {
                camera.record_job = camera
                    .forwarding_id
                    .as_deref()
                    .or(camera.forwarding_progress.as_ref().map(|p| p.id.as_str()))
                    .and_then(|id| book.recording(id));
            }
        }
        let mut state = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
        merge_session_metadata(&mut snapshot, &state);
        snapshot.logs = locked(&self.inner.log).lines();
        *state = snapshot;
    }
    /// Before workers are dropped: stop recordings cleanly.
    fn finalize_workers(&self) {
        for (id, w) in self.workers() {
            let deadline = Instant::now() + Duration::from_secs(5);
            let guard = loop {
                match w.try_lock() {
                    Ok(p) => break Some(p),
                    Err(std::sync::TryLockError::Poisoned(e)) => break Some(e.into_inner()),
                    Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(std::sync::TryLockError::WouldBlock) => break None,
                }
            };
            let Some(mut p) = guard else {
                eprintln!(
                    "capturefab: camera {id} stayed busy; a recording on it may not be finalized"
                );
                continue;
            };
            if p.report.forwarding.is_some()
                && let Err(e) = p.call(SessionCommand::StopForward)
            {
                eprintln!("capturefab: stop recording on {id}: {e:#}");
            }
        }
    }
    pub fn shutdown(&self) {
        if self.inner.quitting.swap(true, Ordering::AcqRel) {
            // Another thread (a Ctrl-C handler) is finishing recordings;
            // returning now would let the process exit first.
            let (done, finished) = &self.inner.shut_down;
            let _ = finished
                .wait_timeout_while(locked(done), Duration::from_secs(30), |done| !*done)
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
        self.finalize_workers();
        for feed in self
            .inner
            .feeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            feed.counters.cancel();
        }
        let workers = std::mem::take(
            &mut *self
                .inner
                .workers
                .write()
                .unwrap_or_else(|e| e.into_inner()),
        );
        drop(workers);
        self.inner
            .feeds
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn is_shutting_down(&self) -> bool {
        self.inner.quitting.load(Ordering::Relaxed)
    }
    /// Make every camera's command in progress, such as a capture to
    /// stdout, end at its next frame wait. The workers then exit: this is
    /// for a process that is about to.
    pub fn interrupt(&self) {
        for feed in self
            .inner
            .feeds
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            feed.counters.cancel();
        }
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

/// Why an inspection has no frame; "timed out" makes the CLI exit 4.
fn no_new_frame(camera: &str, wait: Duration) -> String {
    // Rounded up, so a deadline set for 3 s just before reads "3 s".
    let ms = u64::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(u64::MAX);
    format!(
        "no new frame from {} within {} (timed out); a triggered camera needs TriggerSoftware",
        crate::media::redact_url(camera),
        crate::scheduling::format_duration(ms)
    )
}

/// Explain a `Grab` that received no frame in time; any other failure, a
/// transport timeout included, keeps its own diagnostic.
fn grab_failed(error: anyhow::Error, camera: &str, wait: Duration) -> anyhow::Error {
    if crate::types::classify(&error).kind == crate::types::ErrorKind::FrameTimeout {
        crate::types::frame_timeout(no_new_frame(camera, wait))
    } else {
        error
    }
}

/// A line a coordinator writes to its worker: a session command, or the
/// shutdown only a coordinator sends.
#[derive(Debug)]
enum WorkerRequest {
    Shutdown,
    Command(Box<SessionCommand>),
}
impl WorkerRequest {
    fn read(data: &[u8]) -> Result<Self> {
        if serde_json::from_slice::<Value>(data).is_ok_and(|v| v["shutdown"] == true) {
            return Ok(Self::Shutdown);
        }
        Ok(Self::Command(Box::new(SessionCommand::from_json_slice(
            data,
        )?)))
    }
}

/// Keep in `snapshot` only the lines logged after the first `sent`, and
/// count those as sent.
fn keep_new_lines(snapshot: &mut WorkerSnapshot, sent: &mut u64) {
    let new = usize::try_from(snapshot.logged.saturating_sub(*sent))
        .unwrap_or(usize::MAX)
        .min(snapshot.logs.len());
    snapshot.logs.drain(..snapshot.logs.len() - new);
    *sent = snapshot.logged;
}

/// Hidden mode: stdio controls, separate publisher, mmap frame transfer.
/// `ring` is what `SharedRing::pass_to` gave the worker.
pub fn run_worker(ring: &std::ffi::OsStr) -> Result<()> {
    // Its own process group is in the background of a terminal, where a
    // write to it stops the worker when `stty tostop` is set, unless SIGTTOU
    // is ignored. FFmpeg and the helpers inherit that.
    #[cfg(unix)]
    // SAFETY: setting a standard signal's disposition to ignore has no
    // preconditions.
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let ring = Arc::new(Mutex::new(SharedRing::open(ring)?));
    let handle = crate::engine::WorkerHandle::with_ring(ring);
    let result = (|| -> Result<()> {
        let input = std::io::stdin();
        let mut output = std::io::stdout().lock();
        let mut reader = input.lock();
        // Lines earlier replies carried, which later replies leave out.
        let mut sent = 0;
        loop {
            let mut data = Vec::new();
            use std::io::Read;
            reader
                .by_ref()
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut data)?;
            if data.is_empty() {
                break;
            }
            ensure!(
                data.len() <= 1024 * 1024 && data.last() == Some(&b'\n'),
                "worker command exceeds 1 MiB"
            );
            let result = match WorkerRequest::read(&data)? {
                WorkerRequest::Shutdown => break,
                WorkerRequest::Command(command) => match *command {
                    SessionCommand::Status => Ok(None),
                    command => handle.request(command).map(Some),
                },
            };
            // One snapshot clone per reply, serialized straight to the pipe; the
            // clone keeps the state lock out of the blocking write.
            let mut snapshot = handle.snapshot();
            keep_new_lines(&mut snapshot, &mut sent);
            let (ok, result, error, mut tag) = match result {
                Ok(result) => (true, result, None, None),
                Err(e) => (
                    false,
                    None,
                    Some(format!("{e:#}")),
                    crate::types::error_tag(&e),
                ),
            };
            // Camera passwords this worker holds never leave it, even in text
            // a dependency produced.
            let scrub = |text: &mut String| {
                if let std::borrow::Cow::Owned(clean) = crate::scrub::scrub(text) {
                    *text = clean;
                }
            };
            let mut error = error;
            if crate::scrub::active() {
                error.iter_mut().for_each(scrub);
                if let Some(tag) = &mut tag {
                    tag.hint.iter_mut().for_each(scrub);
                    if let Some(details) = &mut tag.details {
                        let mut text = details.to_string();
                        scrub(&mut text);
                        *details = serde_json::from_str(&text).unwrap_or_default();
                    }
                }
                snapshot.last_error.iter_mut().for_each(scrub);
                snapshot
                    .logs
                    .iter_mut()
                    .for_each(|entry| scrub(&mut entry.message));
            }
            serde_json::to_writer(
                &mut output,
                &WorkerReply {
                    ok,
                    result,
                    error,
                    tag,
                    snapshot: &snapshot,
                    saved: handle.take_saved(2048),
                },
            )?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok(())
    })();
    handle.shutdown();
    result
}

#[cfg(test)]
impl SessionHandle {
    /// A session whose only camera, "cam", is fed by a ring without a
    /// worker, and that ring's writer, for tests of frame consumers.
    pub(crate) fn fed(capacity: usize) -> (Self, SharedRing) {
        let writer = SharedRing::create(&std::env::temp_dir(), capacity).unwrap();
        let handle = Self::new();
        handle
            .inner
            .feeds
            .write()
            .unwrap()
            .insert("cam".into(), Arc::new(Feed::new(writer.reader().unwrap())));
        (handle, writer)
    }
    /// Camera `id`'s newest acquisition sequence, read or not; pass it to
    /// `wait_frame` to wait for later exposures.
    fn frame_sequence_for(&self, id: &str) -> Option<u32> {
        Some(self.feed(id)?.counters.acquired_sequence())
    }
    /// `await_frame`, failing at the deadline as an inspection does.
    fn wait_frame(
        &self,
        id: &str,
        after_seq: u32,
        fresh: u32,
        deadline: Instant,
    ) -> Result<(Arc<Frame>, FrameMeta)> {
        let window = deadline.saturating_duration_since(Instant::now());
        self.await_frame(id, after_seq, fresh, deadline)?
            .ok_or_else(|| crate::types::frame_timeout(no_new_frame(id, window)))
    }
    /// Camera `id`'s newest frame as a reader that keeps watching its ring
    /// takes it.
    fn watch_frame(&self, id: &str) -> Option<Arc<Frame>> {
        locked(&self.feed(id)?.frames).poll(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::Published;
    #[test]
    #[cfg(unix)]
    fn a_killed_worker_is_reported_as_crashed() {
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .status()
            .unwrap();
        let error = worker_crashed(status);
        assert_eq!(crate::cli::error_code(&error), ("worker_crashed", 3));
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(details["signal"], 9);
        assert!(details.get("exit").is_none(), "{details}");
    }
    #[test]
    fn workers_start_in_the_hidden_worker_mode() {
        let command = worker_command("capturefab".into());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("__worker")]
        );
    }
    #[test]
    fn loose_names_resolve_to_one_local_camera_id() {
        let handle = SessionHandle::new();
        let mut webcam = crate::transport::simulator::info_named("sim:0");
        webcam.id = "avfoundation:HD Pro Webcam C920".into();
        webcam.model = "HD Pro Webcam C920".into();
        webcam.serial = webcam.id.clone();
        handle.inner.state.write().unwrap().devices = vec![webcam];
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
        // Every frame may wait its full timeout, plus a minute to save.
        assert_eq!(
            timeout(
                r#"{"op":"capture","output":"a.png","count":5,"timeout_ms":10000,"format":"png"}"#
            ),
            Duration::from_secs(110)
        );
        assert_eq!(
            timeout(
                r#"{"op":"capture","output":"a.png","count":1,"timeout_ms":60000,"format":"png"}"#
            ),
            Duration::from_secs(120)
        );
        assert_eq!(
            timeout(r#"{"op":"apply_setup","name":"bench"}"#),
            Duration::from_secs(240)
        );
        assert_eq!(timeout(r#"{"op":"status"}"#), Duration::from_secs(60));
        assert_eq!(timeout(r#"{"op":"frame"}"#), Duration::from_secs(18));
        assert_eq!(
            timeout(r#"{"op":"frame","timeout_ms":10000}"#),
            Duration::from_secs(25)
        );
        assert_eq!(
            default_request_timeout(&SessionCommand::Disconnect),
            Duration::from_secs(60)
        );
    }
    #[test]
    fn plant_floor_command_defaults_and_unit_ops_are_strict() {
        let capture =
            r#"{"op":"capture","output":"a.png","count":1,"timeout_ms":100,"format":"png"}"#;
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
        for op in ["jobs", "read_setup"] {
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
        // A forward without a duration keeps its old shape both ways.
        let old = br#"{"op":"forward","output":"a.mkv","codec":"h264","encoder":"auto","fps":30,"bitrate":"4M"}"#;
        let command = SessionCommand::from_json_slice(old).unwrap();
        assert!(matches!(
            command,
            SessionCommand::Forward {
                duration_ms: None,
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
        let camera: CameraSnapshot = serde_json::from_value(json!({
            "info": crate::transport::simulator::info_named("sim:0"),
            "streaming": true, "frames": 1, "dropped": 0, "fps": 30.0, "last_error": null,
            "worker_pid": 1, "forwarding": "a.mkv", "forwarding_since_ms": 5
        }))
        .unwrap();
        assert_eq!(camera.forwarding_until_ms, None);
        assert_eq!(camera.forwarding_id, None);
        assert!(
            serde_json::to_value(camera)
                .unwrap()
                .get("forwarding_id")
                .is_none()
        );
    }
    /// Send `worker` on a pipe as `run_worker` does and read the reply as
    /// the coordinator does.
    fn relay(worker: &WorkerSnapshot) -> WorkerSnapshot {
        let line = serde_json::to_vec(&WorkerReply {
            ok: true,
            result: None,
            error: None,
            tag: None,
            snapshot: worker,
            saved: Default::default(),
        })
        .unwrap();
        let reply: ParentReply = serde_json::from_slice(&line).unwrap();
        reply.snapshot.unwrap()
    }
    #[test]
    fn worker_replies_keep_live_and_final_encoder_progress() {
        for (active, written, dropped) in [(true, 8, 1), (true, 18, 3), (false, 22, 3)] {
            let worker = WorkerSnapshot {
                connected: Some(crate::transport::simulator::info_named("sim:0")),
                forwarding_id: active.then(|| "recording-one".into()),
                forwarding_progress: Some(ForwardingProgress {
                    id: "recording-one".into(),
                    accepted: 25,
                    written,
                    dropped,
                    late: 2,
                    stalled: false,
                    recording: None,
                }),
                ..Default::default()
            };
            let camera = camera_of(&relay(&worker), 7, 0).unwrap();
            let progress = camera.forwarding_progress.as_ref().unwrap();
            assert_eq!((progress.written, progress.dropped), (written, dropped));
            assert_eq!(progress.id, "recording-one");
            assert_eq!(camera.forwarding_id.is_some(), active);
        }
    }

    #[test]
    fn conditional_forward_stop_requires_an_exact_opaque_identity() {
        let command = SessionCommand::from_json_slice(
            br#"{"op":"stop_forward_if","forwarding_id":"0123456789abcdef0123456789abcdef"}"#,
        )
        .unwrap();
        command.validate().unwrap();
        assert_eq!(
            serde_json::to_value(command).unwrap(),
            json!({"op":"stop_forward_if","forwarding_id":"0123456789abcdef0123456789abcdef"})
        );
        for id in [
            "",
            "short",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(
                (SessionCommand::StopForwardIf {
                    forwarding_id: id.into()
                })
                .validate()
                .is_err()
            );
        }
        assert!(SessionCommand::from_json_slice(br#"{"op":"stop_forward_if","forwarding_id":"0123456789abcdef0123456789abcdef","extra":1}"#).is_err());
    }
    #[test]
    fn recording_duration_matches_the_cli_duration_range() {
        let forward = |duration_ms| SessionCommand::Forward {
            output: "clip.mkv".into(),
            codec: "h264".into(),
            encoder: "auto".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            storage: Default::default(),
            max_file_bytes: crate::media::default_recording_cap(),
            on_exists: Default::default(),
            context: Default::default(),
            duration_ms,
            utc_offset_s: None,
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
    fn cached_camera(id: &str, frames: u64) -> CameraSnapshot {
        CameraSnapshot {
            info: crate::transport::simulator::info_named(id),
            streaming: true,
            frames,
            received_bytes: frames * 100,
            dropped: 0,
            fps: 30.0,
            last_error: None,
            error_logged_ms: None,
            worker_pid: 1,
            forwarding: Some(format!("{id}.mp4")),
            jobs: Vec::new(),
            auto: None,
            transport: None,
            lost: 0,
            preview_skipped: 0,
            record_job: None,
            stale: true,
            forwarding_id: Some(format!("{frames:032x}")),
            forwarding_progress: None,
            recording_path: None,
            forwarding_since_ms: Some(frames * 1000),
            forwarding_until_ms: Some(frames * 1000 + 60_000),
        }
    }
    #[test]
    fn snapshot_commits_preserve_discovery_completed_during_a_poll() {
        let mut in_flight = SessionSnapshot {
            cameras: vec![cached_camera("sim:0", 10), cached_camera("sim:1", 20)],
            ..Default::default()
        };
        let current = SessionSnapshot {
            devices: vec![crate::transport::simulator::info_named("sim:2")],
            ..Default::default()
        };
        merge_session_metadata(&mut in_flight, &current);
        assert_eq!(in_flight.devices[0].id, "sim:2");
        assert_eq!(in_flight.cameras.len(), 2);
    }
    #[test]
    fn a_poll_that_found_a_worker_busy_keeps_what_its_command_stored_since() {
        // The poll copied sim:0's entry while a schedule held its worker;
        // the schedule's own snapshot, with the new job, was stored first.
        let mut in_flight = SessionSnapshot {
            cameras: vec![CameraSnapshot {
                stale: true,
                ..cached_camera("sim:0", 10)
            }],
            ..Default::default()
        };
        let job: crate::scheduling::CaptureJob = serde_json::from_value(json!({
            "id": 7, "status": "pending", "output": "t", "count": 3, "captured": 0,
            "first_at_ms": 0, "interval_ms": 400, "next_at_ms": 0,
            "last_file": null, "error": null
        }))
        .unwrap();
        let current = SessionSnapshot {
            cameras: vec![CameraSnapshot {
                jobs: vec![job],
                ..cached_camera("sim:0", 12)
            }],
            ..Default::default()
        };
        merge_session_metadata(&mut in_flight, &current);
        assert_eq!(in_flight.cameras[0].jobs.len(), 1, "the job stays");
        assert_eq!(in_flight.cameras[0].frames, 12);
        assert!(in_flight.cameras[0].stale, "still shown as busy");
    }
    #[test]
    fn lines_logged_while_a_poll_builds_its_snapshot_are_stored_with_it() {
        let handle = SessionHandle::new();
        let in_flight = handle.assemble();
        locked(&handle.inner.log).note(LogEntry::now("error", "client failure"));
        handle.commit(in_flight);
        let stored = handle.inner.state.read().unwrap().logs.clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].message, "client failure");
        handle.shutdown();
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
        {
            let mut state = handle.inner.state.write().unwrap();
            state.cameras = vec![cached_camera("sim:0", 10), cached_camera("sim:1", 20)];
            state.devices = vec![crate::transport::simulator::info_named("sim:2")];
        }
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
        for op in ["activity", "select", "undo", "redo", "replay_camera_change"] {
            let removed = format!(r#"{{"op":"{op}"}}"#);
            assert!(
                SessionCommand::from_json_slice(removed.as_bytes()).is_err(),
                "{op}"
            );
        }
        for op in [
            "disconnect",
            "start",
            "stop",
            "jobs",
            "status",
            "features",
            "xml",
            "stop_forward",
        ] {
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
                r#"{"op":"set","feature":"Width","feature":"Height","value":"8"}"#,
                "feature",
            ),
            (
                r#"{"op":"capture","output":"frame.png","count":1,"timeout_ms":1000,"format":"png","storage":{"max_files":1,"max_files":2}}"#,
                "max_files",
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
    fn frame_and_set_many_have_strict_shapes_and_documented_defaults() {
        let SessionCommand::Frame {
            fresh,
            timeout_ms,
            max_dimension,
            format,
            region,
        } = SessionCommand::from_json_slice(br#"{"op":"frame"}"#).unwrap()
        else {
            panic!("not a frame")
        };
        assert_eq!(
            (fresh, timeout_ms, max_dimension, format, region),
            (1, 3000, 1024, crate::inspect::PreviewFormat::Auto, None)
        );
        let full = br#"{"op":"frame","fresh":0,"timeout_ms":50,"max_dimension":0,"format":"png","region":[0.0,0.5,1.0,0.5]}"#;
        let command = SessionCommand::from_json_slice(full).unwrap();
        command.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&command).unwrap(),
            serde_json::from_slice::<Value>(full).unwrap()
        );
        let bare = SessionCommand::from_json_slice(br#"{"op":"frame"}"#).unwrap();
        assert!(serde_json::to_value(bare).unwrap().get("region").is_none());
        for invalid in [
            r#"{"op":"frame","unknown":1}"#,
            r#"{"op":"frame","fresh":1,"fresh":2}"#,
            r#"{"op":"frame","format":"gif"}"#,
            r#"{"op":"frame","region":[0,0,1]}"#,
            r#"{"op":"frame","fresh":-1}"#,
            r#"{"op":"grab"}"#,
            r#"{"op":"set_many"}"#,
            r#"{"op":"set_many","values":[{"feature":"Gain"}]}"#,
            r#"{"op":"set_many","values":[{"feature":"Gain","value":"1","unit":"dB"}]}"#,
            r#"{"op":"set_many","values":[{"feature":"Gain","feature":"Gamma","value":"1"}]}"#,
        ] {
            assert!(
                SessionCommand::from_json_slice(invalid.as_bytes()).is_err(),
                "{invalid}"
            );
        }
        let values = |count: usize| SessionCommand::SetMany {
            values: (0..count)
                .map(|i| FeatureValue {
                    feature: if i % 2 == 0 {
                        "BalanceRatioSelector"
                    } else {
                        "BalanceRatio"
                    }
                    .into(),
                    value: i.to_string(),
                })
                .collect(),
        };
        // The same feature may repeat: selectors address several banks.
        values(MAX_SET_MANY).validate().unwrap();
        for invalid in [values(0), values(MAX_SET_MANY + 1)] {
            assert_eq!(
                invalid.validate().unwrap_err().to_string(),
                "set_many takes 1..32 feature values"
            );
        }
        let parsed = SessionCommand::from_json_slice(
            br#"{"op":"set_many","values":[{"feature":"ExposureTime","value":"8000"}]}"#,
        )
        .unwrap();
        assert!(
            matches!(&parsed, SessionCommand::SetMany { values } if values[0] == FeatureValue { feature: "ExposureTime".into(), value: "8000".into() })
        );
    }
    #[test]
    fn invalid_command_values_are_rejected_before_queueing() {
        let handle = SessionHandle::new();
        for command in [
            SessionCommand::NetworkScan {
                timeout_ms: 0,
                tune: false,
            },
            SessionCommand::OptimizeNetwork {
                bytes_per_second: 0,
            },
            SessionCommand::OptimizeNetwork {
                bytes_per_second: u64::MAX,
            },
            SessionCommand::connect("sim:0".into(), 0),
            SessionCommand::Auto {
                balance: Some(f64::NAN),
            },
            SessionCommand::Auto {
                balance: Some(1.01),
            },
            SessionCommand::Get {
                feature: "Width\n".into(),
            },
            SessionCommand::ReadMemory {
                address: u64::MAX,
                length: 2,
            },
            SessionCommand::WriteMemory {
                address: 0,
                data: vec![],
            },
            SessionCommand::CancelJob { id: 0 },
            SessionCommand::Frame {
                fresh: 11,
                timeout_ms: 3000,
                max_dimension: 1024,
                format: Default::default(),
                region: None,
            },
            SessionCommand::Frame {
                fresh: 1,
                timeout_ms: 3000,
                max_dimension: 1024,
                format: Default::default(),
                region: Some([0.5, 0.5, 0.5, f64::INFINITY]),
            },
            SessionCommand::SetMany { values: Vec::new() },
            // Valid, but only camera workers acquire frames without a reader.
            SessionCommand::Grab { timeout_ms: 1000 },
        ] {
            assert!(handle.submit(command).is_err());
        }
        let snapshot = handle.snapshot();
        assert!(snapshot.cameras.is_empty());
        assert!(snapshot.logs.is_empty());
        handle.shutdown();
    }

    fn width_command() -> SessionCommand {
        SessionCommand::Set {
            feature: "Width".into(),
            value: "128".into(),
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
            fresh: 1,
            timeout_ms: 50,
            max_dimension: 0,
            format: Default::default(),
            region: None,
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
        handle
            .inner
            .state
            .write()
            .unwrap()
            .devices
            .push(CameraInfo {
                id: "camera-a".into(),
                transport: crate::types::Transport::GigE,
                vendor: "Test".into(),
                model: "Test".into(),
                serial: "serial-a".into(),
                address: Some("192.0.2.1".into()),
                profile: Default::default(),
            });
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
        let discover = SessionCommand::Discover {
            timeout_ms: 1000,
            simulated: true,
        };
        let first_discovery = handle.command_turn(None, &discover).unwrap();
        let second_discovery = handle.command_turn(None, &discover).unwrap();
        assert!(Arc::ptr_eq(&first_discovery.order, &second_discovery.order));
        assert!(!Arc::ptr_eq(&set.order, &first_discovery.order));
        assert!(handle.command_turn(None, &SessionCommand::Status).is_none());
        assert!(
            handle
                .command_turn(Some("serial-a"), &SessionCommand::Status)
                .is_none()
        );
    }

    fn ring_frame(id: u64) -> Frame {
        Frame {
            id,
            width: 4,
            height: 4,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![id as u8; 16],
        }
    }
    #[test]
    fn frame_feed_reuses_unshared_frames_and_never_mutates_shared_ones() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut feed = FrameFeed::new(writer.reader().unwrap());
        let frame = ring_frame;
        assert!(feed.poll(true).is_none());
        writer.write(&frame(1), false).unwrap();
        let held = feed.poll(true).unwrap();
        // A consumer still holds frame 1, so frame 2 must get its own buffer.
        writer.write(&frame(2), false).unwrap();
        let second = feed.poll(true).unwrap();
        assert_eq!((held.id, held.data.as_slice()), (1, &[1; 16][..]));
        assert_eq!((second.id, second.data.as_slice()), (2, &[2; 16][..]));
        assert_ne!(held.data.as_ptr(), second.data.as_ptr());
        let buffer = second.data.as_ptr();
        drop((held, second));
        // Once released, the cached allocation is overwritten in place.
        writer.write(&frame(3), false).unwrap();
        let third = feed.poll(true).unwrap();
        assert_eq!((third.id, third.data.as_ptr()), (3, buffer));
        drop(third);
        // Without a newer frame the cached one is returned unchanged.
        assert_eq!(feed.poll(true).unwrap().id, 3);
    }
    #[test]
    fn stdout_captures_refuse_downscaled_previews() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        writer.write(&ring_frame(1), true).unwrap();
        assert_eq!(handle.captured_frame().unwrap().id, 1);
        let large = Frame {
            id: 2,
            width: 80,
            height: 80,
            data: vec![9; 6400],
            ..ring_frame(2)
        };
        writer.write(&large, true).unwrap();
        let error = handle.captured_frame().unwrap_err().to_string();
        assert!(
            error.contains("larger than the 4.0 KiB shared frame buffer"),
            "{error}"
        );
        handle.shutdown();
    }

    #[test]
    fn sim_camera_reports_survive_the_worker_pipe() {
        let worker = crate::engine::WorkerHandle::new();
        let step = |command: Option<SessionCommand>| {
            if let Some(command) = command {
                let _ = worker.request(command);
            }
            let snapshot = worker.snapshot();
            let sent = relay(&snapshot);
            // Features stay in the worker; the camera's report crosses whole.
            assert!(sent.features.is_empty());
            assert_eq!(
                serde_json::to_value(camera_of(&sent, 7, 3)).unwrap(),
                serde_json::to_value(camera_of(&snapshot, 7, 3)).unwrap()
            );
            snapshot
        };
        let connected = step(Some(SessionCommand::connect("sim:0".into(), 1000)));
        assert!(!connected.features.is_empty());
        step(Some(SessionCommand::Set {
            feature: "Width".into(),
            value: "320".into(),
        }));
        step(Some(SessionCommand::Set {
            feature: "NoSuchFeature".into(),
            value: "1".into(),
        }));
        step(Some(SessionCommand::Start));
        std::thread::sleep(Duration::from_millis(200));
        assert!(step(None).frames > 0);
        step(Some(SessionCommand::Stop));
        worker.shutdown();
    }
    #[test]
    fn a_reply_carries_only_the_lines_logged_since_the_last() {
        let line = |i: u64| LogEntry {
            unix_ms: i,
            level: "info".into(),
            message: format!("line {i}"),
        };
        let mut worker = WorkerSnapshot::default();
        let mut sent = 0;
        let reply = |worker: &WorkerSnapshot, sent: &mut u64| {
            let mut reply = worker.clone();
            keep_new_lines(&mut reply, sent);
            reply
                .logs
                .iter()
                .map(|line| line.unix_ms)
                .collect::<Vec<_>>()
        };
        assert!(reply(&worker, &mut sent).is_empty());
        for i in 0..3 {
            worker.logs.push(line(i));
            worker.logged += 1;
        }
        assert_eq!(reply(&worker, &mut sent), [0, 1, 2]);
        assert!(reply(&worker, &mut sent).is_empty());
        worker.logs.push(line(3));
        worker.logged += 1;
        assert_eq!(reply(&worker, &mut sent), [3]);
        // Lines that left the worker's window before a reply are gone.
        worker.logs = (10..14).map(line).collect();
        worker.logged = 14;
        assert_eq!(reply(&worker, &mut sent), [10, 11, 12, 13]);
    }
    #[test]
    fn worker_requests_are_commands_or_shutdown() {
        let request = |line: Value| WorkerRequest::read(line.to_string().as_bytes());
        assert!(matches!(
            request(json!({"shutdown":true})).unwrap(),
            WorkerRequest::Shutdown
        ));
        assert!(matches!(
            request(json!({"op":"status"})).unwrap(),
            WorkerRequest::Command(command) if matches!(*command, SessionCommand::Status)
        ));
        assert!(request(json!({"refresh":{"full":true}})).is_err());
        assert!(request(json!({"op":"status","full":true})).is_err());
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
        println!("test worker ready");
        // `SharedRing::pass_to` adds the ring last.
        run_worker(&std::env::args_os().last().unwrap()).unwrap();
    }
    /// A camera worker run by this test binary.
    #[cfg(unix)]
    fn test_worker() -> Process {
        let ring = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut command = worker_command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "session::tests::serve_as_test_worker",
                "--nocapture",
            ])
            .env(TEST_WORKER, "1");
        ring.pass_to(&mut command);
        let child = command.spawn().unwrap();
        let mut worker = Process::attach(child, ring, Arc::default()).unwrap();
        // Past what the test harness prints first.
        let mut line = String::new();
        while !line.ends_with("test worker ready\n") {
            line.clear();
            let read = worker.output.read_line(&mut line).unwrap();
            assert_ne!(read, 0, "test worker did not start");
        }
        worker
    }
    #[test]
    #[cfg(unix)]
    fn worker_error_categories_survive_the_pipe() {
        let mut worker = test_worker();
        worker.refresh().unwrap();
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
            worker.refresh().unwrap();
        }
    }
    /// Nothing polls an idle worker, so its cached report can still say
    /// streaming after its stream ended on its own; a frame wait asks again.
    #[test]
    #[cfg(unix)]
    fn frame_waits_notice_a_stream_that_ended_since_the_last_report() {
        let mut worker = test_worker();
        worker
            .call(SessionCommand::connect("sim:0".into(), 1000))
            .unwrap();
        assert!(!worker.report.streaming);
        worker.report.streaming = true;
        let handle = SessionHandle::new();
        handle.add_worker("sim:0".into(), worker);
        let asked = Instant::now();
        let error = handle
            .await_frame("sim:0", 0, 1, asked + Duration::from_secs(10))
            .unwrap_err();
        assert_eq!(error.to_string(), "sim:0 stopped streaming");
        assert!(asked.elapsed() < Duration::from_secs(2), "{error}");
        handle.shutdown();
    }
    #[test]
    fn frame_feeds_track_ring_sequence_and_wait_for_fresh_frames() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let handle = SessionHandle::new();
        handle.inner.feeds.write().unwrap().insert(
            "sim:feed".into(),
            Arc::new(Feed::new(writer.reader().unwrap())),
        );
        let frame = |id: u64| Frame {
            id,
            width: 4,
            height: 4,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![id as u8; 16],
        };
        let soon = || Instant::now() + Duration::from_millis(50);
        assert_eq!(handle.frame_sequence_for("sim:feed"), Some(0));
        assert_eq!(handle.frame_sequence_for("sim:unknown"), None);
        // No frame at all: even fresh 0 waits for one, and fails as a timeout.
        let error = handle.wait_frame("sim:feed", 0, 0, soon()).unwrap_err();
        assert_eq!(
            crate::cli::error_code(&error),
            ("frame_timeout", 4),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .starts_with("no new frame from sim:feed within "),
            "{error}"
        );
        let before = crate::scheduling::now_ms();
        for id in 1..=5 {
            writer.write(&frame(id), true).unwrap();
        }
        let start = handle.frame_sequence_for("sim:feed").unwrap();
        assert_eq!(start, 5);
        assert!(
            handle
                .await_frame("sim:feed", start, 0, Instant::now())
                .unwrap()
                .is_none(),
            "An already cached frame cannot satisfy an expired inspection deadline."
        );
        // The newest frame counts for fresh 0, not for fresh 1.
        let (newest, meta) = handle.wait_frame("sim:feed", start, 0, soon()).unwrap();
        assert_eq!((newest.id, meta.seq), (5, 5));
        assert!((before..=crate::scheduling::now_ms()).contains(&meta.received_ms));
        assert!(handle.wait_frame("sim:feed", start, 1, soon()).is_err());
        // A frame published while waiting satisfies it.
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            for id in 6..=7 {
                writer.write(&frame(id), true).unwrap();
            }
            writer
        });
        let (fresh, meta) = handle
            .wait_frame(
                "sim:feed",
                start,
                2,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
        assert!(meta.seq >= start + 2, "{meta:?}");
        assert_eq!(u64::from(meta.seq), fresh.id);
        let _writer = publisher.join().unwrap();
        // Fresh frames are counted from the given position, cached or not.
        assert_eq!(
            handle
                .wait_frame("sim:feed", start, 2, soon())
                .unwrap()
                .1
                .seq,
            7
        );
        let error = handle.wait_frame("sim:feed", 7, 1, soon()).unwrap_err();
        assert!(error.to_string().contains("(timed out)"), "{error}");
        let error = handle
            .wait_frame("sim:gone", 0, 0, soon())
            .unwrap_err()
            .to_string();
        assert_eq!(error, "camera 'sim:gone' not connected in this session");
        assert_eq!(
            no_new_frame("rtsp://user:secret@cam/live", Duration::from_millis(3000)),
            "no new frame from rtsp://<redacted>@cam/live within 3 s (timed out); a triggered camera needs TriggerSoftware"
        );
        handle.shutdown();
    }

    #[test]
    fn held_frames_cannot_satisfy_a_fresh_inspection() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        writer.write(&ring_frame(1), false).unwrap();
        assert_eq!(
            writer.write(&ring_frame(2), false).unwrap(),
            Published::Skipped
        );
        writer.hold(ring_frame(2));
        let start = handle.frame_sequence_for("cam").unwrap();
        assert_eq!(start, 2);
        // Starting to watch republishes the pre-request held frame, but an
        // inspection still needs an exposure.
        assert_eq!(handle.watch_frame("cam").unwrap().id, 1);
        writer.flush().unwrap();
        assert_eq!(handle.feed("cam").unwrap().counters.sequence(), 3);
        assert_eq!(handle.frame_sequence_for("cam"), Some(start));
        let soon = || Instant::now() + Duration::from_millis(20);
        let (held, meta) = handle.wait_frame("cam", start, 0, soon()).unwrap();
        assert_eq!((held.id, meta.seq), (2, start));
        assert!(handle.wait_frame("cam", start, 1, soon()).is_err());
        writer.write(&ring_frame(3), true).unwrap();
        let (fresh, meta) = handle.wait_frame("cam", start, 1, soon()).unwrap();
        assert_eq!((fresh.id, meta.seq), (3, start + 1));
        // Publication 4 is only the first post-request exposure, not two.
        assert!(handle.wait_frame("cam", start, 2, soon()).is_err());
        writer.write(&ring_frame(4), true).unwrap();
        let (fresh, meta) = handle.wait_frame("cam", start, 2, soon()).unwrap();
        assert_eq!((fresh.id, meta.seq), (4, start + 2));
        handle.shutdown();
    }

    #[test]
    fn inspections_do_not_keep_an_unwatched_ring_copying() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        for id in 1..=3 {
            if writer.write(&ring_frame(id), false).unwrap() == Published::Skipped {
                writer.hold(ring_frame(id));
            }
        }
        let start = handle.frame_sequence_for("cam").unwrap();
        let (sampled, finished) = mpsc::channel();
        let acquisition = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            let mut id = 4;
            loop {
                if writer.write(&ring_frame(id), false).unwrap() == Published::Skipped {
                    writer.hold(ring_frame(id));
                }
                writer.flush().unwrap();
                id += 1;
                if finished.try_recv().is_ok() {
                    break;
                }
                assert!(Instant::now() < until, "inspection never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
            // After the inspection has returned, a pending one-shot ask may
            // copy one frame. The following frames must remain uncopied.
            let copied = (id..id + 16)
                .filter(|&id| writer.write(&ring_frame(id), false).unwrap() != Published::Skipped)
                .count();
            (writer, copied)
        });
        let (frame, meta) = handle
            .wait_frame("cam", start, 1, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert!(frame.id >= 4 && meta.seq > start);
        sampled.send(()).unwrap();
        let (writer, copied) = acquisition.join().unwrap();
        assert!(copied <= 1, "inspection kept copying {copied} later frames");
        assert_eq!(writer.preview_skipped(), 0);
        handle.shutdown();
    }

    #[test]
    fn inspections_refuse_downscaled_ring_stand_ins() {
        let (handle, mut writer) = SessionHandle::fed(4096);
        let large = Frame {
            width: 80,
            height: 80,
            data: vec![9; 6400],
            ..ring_frame(1)
        };
        writer.write(&large, true).unwrap();
        let error = handle
            .wait_frame("cam", 0, 1, Instant::now() + Duration::from_secs(1))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("larger than the 4.0 KiB shared frame buffer"),
            "{error}"
        );
        assert!(error.contains("CAPTUREFAB_FRAME_BYTES"), "{error}");
        writer.write(&ring_frame(2), true).unwrap();
        assert_eq!(
            handle
                .wait_frame("cam", 1, 1, Instant::now() + Duration::from_secs(1))
                .unwrap()
                .0
                .id,
            2
        );
        handle.shutdown();
    }

    #[test]
    fn only_a_grab_deadline_reads_as_a_missing_frame() {
        let wait = Duration::from_millis(3000);
        let missed = grab_failed(
            crate::types::frame_timeout(crate::engine::GRAB_TIMEOUT),
            "sim:0",
            wait,
        );
        assert_eq!(missed.to_string(), no_new_frame("sim:0", wait));
        assert_eq!(crate::cli::error_code(&missed), ("frame_timeout", 4));
        // Transport timeouts keep their diagnostic and their exit code.
        for cause in [
            "GVCP command 0x0080 timed out after 200ms (three attempts); check camera address, network and firewall",
            "USB camera control acknowledgement timed out",
        ] {
            let error = grab_failed(
                crate::types::Categorized::error(crate::types::ErrorKind::Timeout, cause),
                "sim:0",
                wait,
            );
            assert_eq!(format!("{error:#}"), cause);
            assert_eq!(crate::cli::error_code(&error), ("timeout", 4));
        }
        let refused = grab_failed(
            anyhow::anyhow!("camera payload exceeds 256 MiB limit"),
            "sim:0",
            wait,
        );
        assert_eq!(refused.to_string(), "camera payload exceeds 256 MiB limit");
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
        let store = crate::setup::Store::session();
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
}
