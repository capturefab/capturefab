//! Bounded, session-wide command activity. Kept outside worker snapshots so
//! changing or disconnecting the selected camera never erases client work.
use super::{SessionCommand, locked};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
}

/// Display attribution supplied by a participant, not an authorization claim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionActor {
    pub id: String,
    pub name: String,
    pub kind: ActorKind,
}
impl SessionActor {
    pub fn new(id: impl Into<String>, name: impl Into<String>, kind: ActorKind) -> Result<Self> {
        let actor = Self {
            id: id.into(),
            name: name.into(),
            kind,
        };
        actor.validate()?;
        Ok(actor)
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.id.is_empty()
                && self.id.len() <= 128
                && self.id.bytes().all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b':')),
            "actor id must use 1..128 letters, digits, '-', '_', '.' or ':'"
        );
        anyhow::ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            "actor name must use 1..128 bytes and contain no control characters"
        );
        Ok(())
    }
    pub fn process(origin: ActivityOrigin) -> Self {
        let pid = std::process::id();
        let (id, name) = match origin {
            ActivityOrigin::Client => ("cli", "CLI"),
            ActivityOrigin::Gui => ("desktop", "Desktop"),
        };
        Self {
            id: format!("{id}:{pid}"),
            name: format!("{name} {pid}"),
            kind: ActorKind::Human,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivityView {
    pub revision: u64,
    pub activity: Vec<SessionActivity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityJob {
    pub id: u64,
    pub worker_pid: u32,
    pub status: String,
    pub count: u32,
    pub captured: u32,
    pub last_file: Option<String>,
    pub error: Option<String>,
}
impl ActivityJob {
    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "running")
    }
    fn from_job(job: &crate::scheduling::CaptureJob, worker_pid: u32) -> Self {
        Self {
            id: job.id,
            worker_pid,
            status: job.status.clone(),
            count: job.count,
            captured: job.captured,
            last_file: job
                .last_file
                .as_deref()
                .map(|file| bounded(&redact(file), 1024)),
            error: job
                .error
                .as_deref()
                .map(|error| bounded(&redact(error), 4096)),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityOrigin {
    /// Commands received from the CLI, an AI agent or another API client.
    Client,
    Gui,
}
impl ActivityOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Client => "CLI / API",
            Self::Gui => "GUI",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Queued,
    Running,
    Succeeded,
    Failed,
}
impl ActivityState {
    pub fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Running => "Running",
            Self::Succeeded => "Done",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionActivity {
    pub id: u64,
    pub origin: ActivityOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<SessionActor>,
    pub operation: String,
    pub camera: Option<String>,
    pub feature: Option<String>,
    pub label: String,
    pub state: ActivityState,
    pub submitted_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub detail: Option<String>,
    /// Total saved files; `files` contains at most the first eight paths.
    pub file_count: usize,
    pub files: Vec<String>,
    /// A schedule continues producing files after its command is acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<ActivityJob>,
}

#[derive(Default)]
pub(super) struct Activity {
    entries: Mutex<ActivityEntries>,
    changed: Condvar,
}
#[derive(Default)]
struct ActivityEntries {
    next_id: u64,
    revision: u64,
    entries: VecDeque<SessionActivity>,
    stopped: bool,
}
const HISTORY: usize = 200;
impl Activity {
    pub(super) fn snapshot(&self) -> ActivityView {
        let entries = locked(&self.entries);
        ActivityView {
            revision: entries.revision,
            activity: entries.entries.iter().cloned().collect(),
        }
    }
    pub(super) fn revision(&self) -> u64 {
        locked(&self.entries).revision
    }
    pub(super) fn wait(&self, after: u64, timeout: Duration) -> u64 {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return self.revision();
        };
        let mut entries = locked(&self.entries);
        while entries.revision == after && !entries.stopped {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next, waited) = self
                .changed
                .wait_timeout(entries, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries = next;
            if waited.timed_out() {
                break;
            }
        }
        entries.revision
    }
    fn notify(&self, entries: &mut ActivityEntries) {
        entries.revision = entries.revision.saturating_add(1);
        self.changed.notify_all();
    }
    pub(super) fn stop(&self) {
        locked(&self.entries).stopped = true;
        self.changed.notify_all();
    }
    pub(super) fn observe_jobs(&self, cameras: &[super::CameraSnapshot]) {
        let mut entries = locked(&self.entries);
        let mut changed = false;
        for entry in &mut entries.entries {
            let Some(previous) = entry.job.as_ref() else {
                continue;
            };
            if !previous.active() {
                continue;
            }
            let camera = cameras.iter().find(|camera| {
                Some(camera.info.id.as_str()) == entry.camera.as_deref()
                    && camera.worker_pid == previous.worker_pid
            });
            let next = camera
                .and_then(|camera| camera.jobs.iter().find(|job| job.id == previous.id))
                .map(|job| ActivityJob::from_job(job, previous.worker_pid));
            // A concurrent snapshot may predate a new connection or schedule.
            // Only explicit lifecycle transitions cancel a missing worker.
            let Some(next) = next else {
                continue;
            };
            // Concurrent snapshots can contain older cached progress. Frames
            // already announced to participants must never disappear.
            if next.captured < previous.captured
                || previous.status == "running" && next.status == "pending"
            {
                continue;
            }
            if &next != previous {
                entry.file_count = next.captured as usize;
                if let Some(file) = &next.last_file {
                    entry.files = vec![bounded(file, 1024)];
                }
                entry.detail = Some(bounded(
                    &format!(
                        "Capture job {} · {} · {}/{} frames{}",
                        next.id,
                        next.status,
                        next.captured,
                        next.count,
                        next.error
                            .as_ref()
                            .map(|error| format!(" · {error}"))
                            .unwrap_or_default()
                    ),
                    4096,
                ));
                entry.job = Some(next);
                changed = true;
            }
        }
        if changed {
            trim(&mut entries.entries);
            self.notify(&mut entries);
        }
    }
    pub(super) fn cancel_jobs(&self, camera: Option<&str>, worker_pid: Option<u32>, reason: &str) {
        let mut entries = locked(&self.entries);
        let mut changed = false;
        for entry in &mut entries.entries {
            if camera.is_some_and(|camera| entry.camera.as_deref() != Some(camera)) {
                continue;
            }
            let Some(job) = entry.job.as_mut() else {
                continue;
            };
            if !job.active() || worker_pid.is_some_and(|pid| job.worker_pid != pid) {
                continue;
            }
            job.status = "cancelled".into();
            job.error = Some(reason.into());
            entry.detail = Some(format!(
                "Capture job {} · cancelled · {}/{} frames · {reason}",
                job.id, job.captured, job.count
            ));
            changed = true;
        }
        if changed {
            trim(&mut entries.entries);
            self.notify(&mut entries);
        }
    }
    #[cfg(test)]
    pub(super) fn begin(
        self: &Arc<Self>,
        origin: ActivityOrigin,
        camera: Option<&str>,
        command: &SessionCommand,
    ) -> Option<CommandActivity> {
        self.begin_as(origin, SessionActor::process(origin), camera, command)
    }
    pub(super) fn begin_as(
        self: &Arc<Self>,
        origin: ActivityOrigin,
        actor: SessionActor,
        camera: Option<&str>,
        command: &SessionCommand,
    ) -> Option<CommandActivity> {
        // Worker status refreshes and CLI watch requests are observations,
        // not actions. Recording them would hide the work being observed.
        if matches!(
            command,
            SessionCommand::Status
                | SessionCommand::Activity { .. }
                | SessionCommand::Get { .. }
                | SessionCommand::Frame { .. }
                | SessionCommand::Grab { .. }
                | SessionCommand::Jobs
                | SessionCommand::Features
                | SessionCommand::Xml
                | SessionCommand::ReadMemory { .. }
                | SessionCommand::CaptureReview { .. }
                | SessionCommand::DatasetStatus
                | SessionCommand::DatasetQuery { .. }
                | SessionCommand::Signals
                | SessionCommand::SignalRead { .. }
                | SessionCommand::SignalBrowse { .. }
        ) {
            return None;
        }
        let (operation, label) = describe(command);
        let camera = match command {
            SessionCommand::Connect { camera, .. } | SessionCommand::Select { camera } => {
                Some(camera.as_str())
            }
            _ => camera,
        };
        let feature = match command {
            SessionCommand::Get { feature }
            | SessionCommand::Set { feature, .. }
            | SessionCommand::Execute { feature } => Some(bounded(feature, 512)),
            _ => None,
        };
        let mut entries = locked(&self.entries);
        entries.next_id += 1;
        let id = entries.next_id;
        entries.entries.push_back(SessionActivity {
            id,
            origin,
            actor: Some(SessionActor {
                name: bounded(&redact(&actor.name), 128),
                ..actor
            }),
            operation: operation.into(),
            camera: camera.map(|camera| bounded(&redact(camera), 1024)),
            feature,
            label: bounded(&redact(&label), 1024),
            state: ActivityState::Queued,
            submitted_at_ms: crate::scheduling::now_ms(),
            started_at_ms: None,
            finished_at_ms: None,
            detail: None,
            file_count: 0,
            files: Vec::new(),
            job: None,
        });
        trim(&mut entries.entries);
        self.notify(&mut entries);
        Some(CommandActivity {
            activity: self.clone(),
            id,
            finished: false,
        })
    }
}

pub(super) struct CommandActivity {
    activity: Arc<Activity>,
    id: u64,
    finished: bool,
}
impl CommandActivity {
    pub(super) fn running(&self) {
        let mut entries = locked(&self.activity.entries);
        if let Some(entry) = entries.entries.iter_mut().find(|entry| entry.id == self.id) {
            entry.state = ActivityState::Running;
            entry.started_at_ms = Some(crate::scheduling::now_ms());
            self.activity.notify(&mut entries);
        }
    }
    pub(super) fn finish(&mut self, result: &Result<Value>) {
        let mut entries = locked(&self.activity.entries);
        if let Some(entry) = entries.entries.iter_mut().find(|entry| entry.id == self.id) {
            entry.finished_at_ms = Some(crate::scheduling::now_ms());
            match result {
                Ok(value) => {
                    entry.state = ActivityState::Succeeded;
                    if let Some(camera) = value["connected"]["id"]
                        .as_str()
                        .or_else(|| value["active_camera"].as_str())
                    {
                        entry.camera = Some(bounded(&redact(camera), 1024));
                    }
                    entry.file_count = value["files"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|file| file.is_string())
                        .count();
                    entry.files = value["files"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .take(8)
                        .map(|file| bounded(&redact(file), 1024))
                        .collect();
                    entry.detail = result_detail(value, &entry.files, entry.file_count)
                        .map(|detail| bounded(&detail, 4096));
                    if let (Ok(job), Some(pid)) = (
                        serde_json::from_value::<crate::scheduling::CaptureJob>(
                            value["job"].clone(),
                        ),
                        value["worker_pid"]
                            .as_u64()
                            .and_then(|pid| u32::try_from(pid).ok()),
                    ) {
                        entry.job = Some(ActivityJob::from_job(&job, pid));
                    }
                }
                Err(error) => {
                    entry.state = ActivityState::Failed;
                    entry.detail = Some(bounded(&redact(&format!("{error:#}")), 4096));
                }
            }
        }
        trim(&mut entries.entries);
        self.activity.notify(&mut entries);
        self.finished = true;
    }
}
impl Drop for CommandActivity {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(&Err(anyhow::anyhow!("command stopped before completion")));
        }
    }
}
fn trim(entries: &mut VecDeque<SessionActivity>) {
    // Pending commands and active schedules have their own queue bounds. Keep
    // them visible even when faster work on other cameras finishes first.
    let mut excess = entries
        .iter()
        .filter(|entry| !retained(entry))
        .count()
        .saturating_sub(HISTORY);
    entries.retain(|entry| {
        if excess > 0 && !retained(entry) {
            excess -= 1;
            false
        } else {
            true
        }
    });
}
fn retained(entry: &SessionActivity) -> bool {
    entry.state.active() || entry.job.as_ref().is_some_and(ActivityJob::active)
}

fn describe(command: &SessionCommand) -> (&'static str, String) {
    use SessionCommand::*;
    match command {
        Discover { simulated, .. } => (
            "discover",
            if *simulated {
                "Discover cameras (including simulators)"
            } else {
                "Discover cameras"
            }
            .into(),
        ),
        NetworkScan { .. } => ("network_scan", "Scan network and tune cameras".into()),
        OptimizeNetwork { bytes_per_second } => (
            "optimize_network",
            format!("Tune network to {bytes_per_second} bytes/s"),
        ),
        Connect { camera, .. } => ("connect", format!("Connect {camera}")),
        Select { camera } => ("select", format!("Select {camera}")),
        Disconnect => ("disconnect", "Disconnect camera".into()),
        Get { feature } => ("get", format!("Read {feature}")),
        Set { feature, value } => ("set", format!("Set {feature} = {value}")),
        SetMany { values } => (
            "set_many",
            if values.len() <= 3 {
                format!(
                    "Set {}",
                    values
                        .iter()
                        .map(|v| format!("{} = {}", v.feature, v.value))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                format!("Set {} feature values", values.len())
            },
        ),
        Execute { feature } => ("execute", format!("Execute {feature}")),
        Start => ("start", "Start acquisition".into()),
        Stop => ("stop", "Stop acquisition".into()),
        Undo => ("undo", "Undo camera change".into()),
        Redo => ("redo", "Redo camera change".into()),
        ReplayCameraChange { undo, .. } => (
            "replay_camera_change",
            format!("{} camera change", if *undo { "Undo" } else { "Redo" }),
        ),
        Auto { balance } => (
            "auto",
            balance.map_or_else(
                || "Enable auto mode".into(),
                |balance| format!("Enable auto mode (balance {balance})"),
            ),
        ),
        Manual { revert } => (
            "manual",
            if *revert {
                "Restore manual settings"
            } else {
                "Switch to manual mode"
            }
            .into(),
        ),
        CaptureSessionOpen { path, create, .. } => (
            "capture_session_open",
            format!(
                "{} capture session {path}",
                if *create { "Create" } else { "Resume" }
            ),
        ),
        CaptureSessionClose => ("capture_session_close", "Close capture session".into()),
        AutoCapture { enabled, .. } => (
            "auto_capture",
            format!("{} auto capture", if *enabled { "Enable" } else { "Pause" }),
        ),
        CaptureExample { .. } => ("capture_example", "Teach capture preference".into()),
        CaptureReview { .. } => ("capture_review", "Read capture review".into()),
        ReviewCapture { id, keep } => (
            "review_capture",
            format!(
                "{} auto capture {id}",
                if *keep { "Keep" } else { "Discard" }
            ),
        ),
        Capture {
            output,
            count,
            format,
            destination,
            ..
        } => (
            "capture",
            format!(
                "Capture {count} {format} frame(s) → {}",
                output_label(output, destination)
            ),
        ),
        Schedule {
            output,
            count,
            format,
            destination,
            interval_ms,
            ..
        } => (
            "schedule",
            format!(
                "Schedule {count} {format} frame(s), every {interval_ms} ms → {}",
                output_label(output, destination)
            ),
        ),
        Jobs => ("jobs", "Read capture jobs".into()),
        CancelJob { id } => ("cancel_job", format!("Cancel capture job {id}")),
        Status => ("status", "Read session status".into()),
        Frame { .. } => ("frame", "Inspect frame".into()),
        Grab { .. } => ("grab", "Acquire one frame".into()),
        Activity { .. } => ("activity", "Read session activity".into()),
        Features => ("features", "Read camera features".into()),
        Xml => ("xml", "Read GenICam XML".into()),
        ReadMemory { address, length } => (
            "read_memory",
            format!("Read {length} bytes at 0x{address:x}"),
        ),
        WriteMemory { address, data } => (
            "write_memory",
            format!("Write {} bytes at 0x{address:x}", data.len()),
        ),
        Forward {
            output,
            codec,
            destination,
            duration_ms,
            ..
        } => (
            "forward",
            format!(
                "{} {codec}{} → {}",
                if crate::media::is_recording(output) {
                    "Record"
                } else {
                    "Forward"
                },
                duration_ms
                    .map(|ms| format!(" for {}", crate::scheduling::format_duration(ms)))
                    .unwrap_or_default(),
                output_label(output, destination)
            ),
        ),
        StopForward => ("stop_forward", "Stop forwarding / recording".into()),
        DatasetOpen { path, .. } => ("dataset_open", format!("Open dataset {}", file_name(path))),
        DatasetClose => ("dataset_close", "Close dataset".into()),
        DatasetStatus => ("dataset_status", "Read dataset status".into()),
        DatasetNote { .. } => ("dataset_note", "Add dataset note".into()),
        DatasetQuery { .. } => ("dataset_query", "Query dataset".into()),
        DatasetExport { output, .. } => (
            "dataset_export",
            format!("Export dataset → {}", file_name(output)),
        ),
        SignalRead { uri, .. } => (
            "signal_read",
            format!("Read {}", crate::media::redact_url(uri)),
        ),
        SignalBrowse { uri, .. } => (
            "signal_browse",
            format!("Browse {}", crate::media::redact_url(uri)),
        ),
        SignalStart { uri, .. } => (
            "signal_start",
            format!("Log {}", crate::media::redact_url(uri)),
        ),
        SignalStop { id } => ("signal_stop", format!("Stop logging {id}")),
        Signals => ("signals", "Signal status".into()),
    }
}
/// The last component of a path, so activity never shows a full folder tree.
fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}
fn output_label(output: &str, destination: &Option<String>) -> String {
    match destination {
        Some(destination) => format!("{destination} / {output}"),
        None => output.into(),
    }
}
fn result_detail(value: &Value, files: &[String], file_count: usize) -> Option<String> {
    if !files.is_empty() {
        let mut detail = files.join("\n");
        if file_count > files.len() {
            detail.push_str(&format!(
                "\n{} more files saved ({file_count} total)",
                file_count - files.len()
            ));
        }
        return Some(detail);
    }
    if let Some(id) = value["job"]["id"].as_u64() {
        return Some(format!("Capture job {id} scheduled"));
    }
    if let Some(output) = value["forwarding"].as_str() {
        return Some(redact(output));
    }
    if let Some(warnings) = value["warnings"].as_array() {
        let warnings: Vec<_> = warnings
            .iter()
            .filter_map(Value::as_str)
            .map(redact)
            .collect();
        if !warnings.is_empty() {
            return Some(warnings.join("\n"));
        }
    }
    value["history_unavailable"].as_str().map(redact)
}

/// Redact every URL in a sentence, including diagnostics containing several
/// sources; URL credentials must never enter snapshots or the activity UI.
pub(super) fn redact(value: &str) -> String {
    value
        .split_inclusive(char::is_whitespace)
        .map(|part| {
            let body = part.trim_end_matches(char::is_whitespace);
            let starts: Vec<_> = body
                .match_indices("://")
                .map(|(at, _)| {
                    body[..at]
                        .char_indices()
                        .rev()
                        .take_while(|(_, character)| {
                            character.is_ascii_alphanumeric()
                                || matches!(character, '+' | '-' | '.')
                        })
                        .last()
                        .map_or(at, |(start, _)| start)
                })
                .collect();
            if starts.is_empty() {
                return part.to_owned();
            }
            let mut redacted = body[..starts[0]].to_owned();
            for (index, start) in starts.iter().enumerate() {
                let end = starts.get(index + 1).copied().unwrap_or(body.len());
                redacted.push_str(&crate::media::redact_url(&body[*start..end]));
            }
            redacted.push_str(&part[body.len()..]);
            redacted
        })
        .collect()
}
fn bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .take_while(|(at, _)| *at <= max_bytes.saturating_sub(3))
        .last()
        .map_or(0, |(at, _)| at);
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn participant_identity_is_validated_and_legacy_entries_deserialize() {
        let actor = SessionActor::new("agent:planner", "Planning agent", ActorKind::Agent).unwrap();
        assert_eq!(serde_json::to_value(&actor).unwrap()["kind"], "agent");
        for (id, name) in [
            ("", "Alice"),
            ("bad id", "Alice"),
            ("valid", "\nAlice"),
            ("valid", " "),
        ] {
            assert!(SessionActor::new(id, name, ActorKind::Human).is_err());
        }
        assert!(SessionActor::new("a".repeat(129), "Alice", ActorKind::Human).is_err());
        assert!(SessionActor::new("valid", "☀".repeat(44), ActorKind::Human).is_err());
        let activity = Arc::new(Activity::default());
        let command = activity
            .begin_as(
                ActivityOrigin::Client,
                actor.clone(),
                Some("sim:0"),
                &SessionCommand::Start,
            )
            .unwrap();
        assert_eq!(activity.snapshot().activity[0].actor.as_ref(), Some(&actor));
        let mut legacy = serde_json::to_value(&activity.snapshot().activity[0]).unwrap();
        legacy.as_object_mut().unwrap().remove("actor");
        assert!(
            serde_json::from_value::<SessionActivity>(legacy)
                .unwrap()
                .actor
                .is_none()
        );
        drop(command);
    }
    #[test]
    fn lifecycle_updates_wake_every_observer_without_polling_or_read_noise() {
        let activity = Arc::new(Activity::default());
        let observers: Vec<_> = (0..2)
            .map(|_| {
                let activity = activity.clone();
                std::thread::spawn(move || activity.wait(0, Duration::from_secs(2)))
            })
            .collect();
        let mut command = activity
            .begin(
                ActivityOrigin::Client,
                Some("sim:0"),
                &SessionCommand::Start,
            )
            .unwrap();
        for observer in observers {
            assert_eq!(observer.join().unwrap(), 1);
        }
        command.running();
        assert_eq!(activity.wait(1, Duration::ZERO), 2);
        command.finish(&Ok(json!({})));
        let view = activity.snapshot();
        assert_eq!(view.revision, 3);
        assert_eq!(
            activity.wait(999, Duration::from_secs(2)),
            3,
            "a cursor from an older session returns immediately"
        );
        assert_eq!(view.activity[0].state, ActivityState::Succeeded);
        for read in [
            SessionCommand::Status,
            SessionCommand::Activity {
                after: Some(3),
                timeout_ms: 1000,
            },
            SessionCommand::Jobs,
            SessionCommand::Features,
            SessionCommand::Xml,
            SessionCommand::Get {
                feature: "Width".into(),
            },
            SessionCommand::ReadMemory {
                address: 0,
                length: 4,
            },
            SessionCommand::Frame {
                fresh: 1,
                timeout_ms: 3000,
                max_dimension: 1024,
                format: Default::default(),
                region: None,
            },
            SessionCommand::Grab { timeout_ms: 1000 },
        ] {
            assert!(
                activity
                    .begin(ActivityOrigin::Client, Some("sim:0"), &read)
                    .is_none()
            );
        }
        assert_eq!(
            describe(&SessionCommand::Frame {
                fresh: 0,
                timeout_ms: 1,
                max_dimension: 0,
                format: Default::default(),
                region: None,
            }),
            ("frame", "Inspect frame".into())
        );
        assert_eq!(
            describe(&SessionCommand::Grab { timeout_ms: 1 }),
            ("grab", "Acquire one frame".into())
        );
        assert_eq!(activity.wait(3, Duration::from_millis(5)), 3);
    }
    #[test]
    fn scheduled_work_keeps_its_actor_and_wakes_observers_until_completion() {
        let activity = Arc::new(Activity::default());
        let mut schedule = activity
            .begin_as(
                ActivityOrigin::Client,
                SessionActor::new("agent:scheduler", "Scheduler", ActorKind::Agent).unwrap(),
                Some("sim:0"),
                &SessionCommand::Start,
            )
            .unwrap();
        let job = crate::scheduling::CaptureJob {
            id: 1,
            status: "pending".into(),
            output: "frames".into(),
            count: 3,
            captured: 0,
            first_at_ms: 1,
            interval_ms: 10,
            next_at_ms: 1,
            last_file: None,
            error: None,
            destination: None,
            context: Default::default(),
        };
        schedule.finish(&Ok(json!({"job":job,"worker_pid":42})));
        for _ in 0..HISTORY + 1 {
            let mut short = activity
                .begin(ActivityOrigin::Gui, Some("sim:1"), &SessionCommand::Start)
                .unwrap();
            short.finish(&Ok(json!({})));
        }
        assert_eq!(
            activity.snapshot().activity[0].id,
            1,
            "active schedules survive trimming"
        );
        let mut camera: super::super::CameraSnapshot = serde_json::from_value(json!({
            "info":crate::transport::simulator::info_named("sim:0"), "features":[], "streaming":true,
            "frames":1,"dropped":0,"fps":30.0,"last_error":null,"worker_pid":42,
            "jobs":[job]
        })).unwrap();
        camera.jobs[0].status = "running".into();
        camera.jobs[0].captured = 1;
        camera.jobs[0].last_file = Some("frame-1.png".into());
        let before = activity.revision();
        activity.observe_jobs(&[camera.clone()]);
        let view = activity.snapshot();
        assert_eq!(view.revision, before + 1);
        assert_eq!(
            view.activity[0].actor.as_ref().unwrap().id,
            "agent:scheduler"
        );
        assert_eq!(view.activity[0].file_count, 1);
        assert_eq!(view.activity[0].job.as_ref().unwrap().captured, 1);
        activity.observe_jobs(&[camera.clone()]);
        assert_eq!(
            activity.revision(),
            before + 1,
            "identical reports do not wake observers"
        );
        let mut old = camera.clone();
        old.jobs[0].status = "pending".into();
        activity.observe_jobs(&[old.clone()]);
        assert_eq!(
            activity.snapshot().activity[0].job.as_ref().unwrap().status,
            "running",
            "older pending status cannot rewind a running job"
        );
        old.jobs[0].captured = 0;
        old.jobs[0].last_file = None;
        activity.observe_jobs(&[old]);
        activity.observe_jobs(&[]);
        assert_eq!(
            activity.revision(),
            before + 1,
            "in-flight older snapshots do not roll back or cancel progress"
        );
        camera.worker_pid = 43;
        activity.observe_jobs(&[camera]);
        assert_eq!(
            activity.revision(),
            before + 1,
            "a replacement worker cannot update an old schedule"
        );
        activity.cancel_jobs(
            Some("sim:0"),
            Some(42),
            "camera disconnected before schedule completed",
        );
        assert_eq!(activity.revision(), before + 2);
        assert!(
            activity
                .snapshot()
                .activity
                .iter()
                .all(|entry| entry.id != 1),
            "old worker's canceled job is terminal and can be trimmed"
        );
    }
    #[test]
    fn activity_tracks_origin_target_and_all_lifecycle_states() {
        let activity = Arc::new(Activity::default());
        let mut command = activity
            .begin(
                ActivityOrigin::Client,
                None,
                &SessionCommand::Connect {
                    camera: "SIM0".into(),
                    timeout_ms: 1000,
                },
            )
            .unwrap();
        assert_eq!(activity.snapshot().activity[0].state, ActivityState::Queued);
        command.running();
        assert_eq!(
            activity.snapshot().activity[0].state,
            ActivityState::Running
        );
        command.finish(&Ok(json!({"connected": {"id": "sim:0"}})));
        let entry = activity.snapshot().activity.remove(0);
        assert_eq!(entry.state, ActivityState::Succeeded);
        assert_eq!(entry.camera.as_deref(), Some("sim:0"));
        assert_eq!(entry.origin, ActivityOrigin::Client);
        assert!(entry.started_at_ms.is_some() && entry.finished_at_ms.is_some());
        assert!(
            activity
                .begin(ActivityOrigin::Client, None, &SessionCommand::Status)
                .is_none()
        );
    }
    #[test]
    fn abandoned_commands_fail_and_history_never_prunes_active_work() {
        let activity = Arc::new(Activity::default());
        let active = activity
            .begin(
                ActivityOrigin::Client,
                Some("sim:0"),
                &SessionCommand::Start,
            )
            .unwrap();
        for _ in 0..HISTORY + 10 {
            let mut command = activity
                .begin(ActivityOrigin::Gui, Some("sim:1"), &SessionCommand::Start)
                .unwrap();
            command.finish(&Ok(json!({})));
        }
        assert_eq!(activity.snapshot().activity.len(), HISTORY + 1);
        assert_eq!(activity.snapshot().activity[0].id, 1);
        drop(active);
        assert_eq!(activity.snapshot().activity.len(), HISTORY);
        let abandoned = activity
            .begin(ActivityOrigin::Client, None, &SessionCommand::Start)
            .unwrap();
        drop(abandoned);
        assert_eq!(
            activity.snapshot().activity.last().unwrap().state,
            ActivityState::Failed
        );
    }
    #[test]
    fn activity_redacts_all_urls_and_keeps_capture_results() {
        let activity = Arc::new(Activity::default());
        let mut command = activity
            .begin(
                ActivityOrigin::Client,
                None,
                &SessionCommand::Connect {
                    camera: "rtsp://user:password@camera/live?token=secret".into(),
                    timeout_ms: 1000,
                },
            )
            .unwrap();
        command.finish(&Err(anyhow::anyhow!(
            "failed rtsp://user:password@one/live then srt://two?passphrase=secret"
        )));
        let serialized = serde_json::to_string(&activity.snapshot()).unwrap();
        assert!(!serialized.contains("password"));
        assert!(!serialized.contains("secret"));
        assert!(serialized.contains("<redacted>"));
        let punctuation = redact(
            "failed rtsp://user:password@one/live,rtsp://user:secret@two/live then srt://three?token=secret next",
        );
        assert!(!punctuation.contains("password") && !punctuation.contains("secret"));
        assert!(
            punctuation.ends_with(" next"),
            "redaction preserves sentence spacing"
        );
        let mut capture = activity
            .begin(
                ActivityOrigin::Client,
                Some("sim:0"),
                &SessionCommand::Start,
            )
            .unwrap();
        capture.finish(&Ok(json!({"files": ["one.png", "two.png"]})));
        assert_eq!(
            activity.snapshot().activity.last().unwrap().files,
            ["one.png", "two.png"]
        );
    }
    #[test]
    fn large_results_have_bounded_previews_and_an_accurate_file_count() {
        let activity = Arc::new(Activity::default());
        let mut command = activity
            .begin(
                ActivityOrigin::Client,
                None,
                &SessionCommand::Set {
                    feature: "Description".into(),
                    value: "☀".repeat(10_000),
                },
            )
            .unwrap();
        let files: Vec<_> = (0..100_000)
            .map(|index| format!("capture-{index}.png"))
            .collect();
        command.finish(&Ok(json!({"files": files})));
        let entry = activity.snapshot().activity.remove(0);
        assert!(entry.label.len() <= 1024);
        assert_eq!(entry.files.len(), 8);
        assert_eq!(entry.file_count, 100_000);
        assert!(entry.detail.unwrap().contains("100000 total"));
        let mut error = activity
            .begin(ActivityOrigin::Client, None, &SessionCommand::Start)
            .unwrap();
        error.finish(&Err(anyhow::anyhow!("☀".repeat(10_000))));
        assert!(
            activity
                .snapshot()
                .activity
                .last()
                .unwrap()
                .detail
                .as_ref()
                .unwrap()
                .len()
                <= 4096
        );
    }
    #[test]
    fn set_many_labels_list_short_writes_and_count_long_ones() {
        let label = |count: usize| {
            let values = (0..count)
                .map(|i| super::super::FeatureValue {
                    feature: format!("F{i}"),
                    value: i.to_string(),
                })
                .collect();
            describe(&SessionCommand::SetMany { values })
        };
        assert_eq!(label(2), ("set_many", "Set F0 = 0, F1 = 1".into()));
        assert_eq!(label(5), ("set_many", "Set 5 feature values".into()));
    }
    #[test]
    fn forward_labels_say_what_they_record_and_for_how_long() {
        let forward = |output: &str, destination: Option<&str>, duration_ms| {
            let activity = Arc::new(Activity::default());
            let command = SessionCommand::Forward {
                output: output.into(),
                codec: "h264".into(),
                encoder: "auto".into(),
                fps: 30.0,
                bitrate: "4M".into(),
                storage: Default::default(),
                max_file_bytes: crate::media::default_recording_cap(),
                destination: destination.map(Into::into),
                on_exists: Default::default(),
                context: Default::default(),
                duration_ms,
            };
            let entry = activity.begin(ActivityOrigin::Client, Some("sim:0"), &command);
            let label = activity.snapshot().activity[0].label.clone();
            drop(entry);
            label
        };
        assert_eq!(
            forward("line1/run.mp4", None, Some(3_600_000)),
            "Record h264 for 1 h → line1/run.mp4"
        );
        assert_eq!(
            forward("run.mkv", Some("archive"), Some(90_000)),
            "Record h264 for 1 min 30 s → archive / run.mkv"
        );
        assert_eq!(forward("run.mkv", None, None), "Record h264 → run.mkv");
        let stream = forward("rtsp://user:secret@nvr/live", None, Some(30_000));
        assert!(
            stream.starts_with("Forward h264 for 30 s → rtsp://"),
            "{stream}"
        );
        assert!(!stream.contains("secret"), "{stream}");
    }
}
