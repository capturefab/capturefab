//! What tools know when they check or run a call: the session (through a
//! `ToolHost`), saved destinations, the storage budget, FFmpeg's encoders
//! gathered into one plain `Context`.
//!
//! `Context::gather` does file and process I/O (FFmpeg runs once per
//! process), so callers run it on a worker thread, never the GUI's. Checks
//! then read the `Context` alone. It never holds the storage ledger's path,
//! access key IDs or absolute home paths.
use crate::{
    destination::Destination,
    session::{
        ActivityOrigin, SessionActor, SessionCommand, SessionHandle, SessionSnapshot,
        default_request_timeout,
    },
    storage::StoragePolicy,
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, FixedOffset, Local, Offset, SecondsFormat, TimeZone};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// The session a tool reads and commands.
pub trait ToolHost: Send + Sync {
    fn snapshot(&self) -> Result<SessionSnapshot>;
    /// Send one command and wait for its reply. Every camera-scoped command
    /// names its camera; only Connect and Discover pass None.
    fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value>;
    /// A session is running; without one only offline tools work.
    fn has_session(&self) -> bool;
    /// The actor this host's commands are credited to, which tells its own
    /// session activity from other participants'.
    fn actor_id(&self) -> Option<String> {
        None
    }
    /// Bind a run's actual commands to its actor, rather than merely changing
    /// the identity reported to risk classification. Unknown hosts refuse.
    fn with_actor(&self, _actor: SessionActor) -> Result<Arc<dyn ToolHost>> {
        anyhow::bail!("This tool host cannot attribute assistant task commands.")
    }
}

/// The session in this process: the desktop app (origin Gui) or the CLI's
/// temporary local session (origin Client).
pub struct SessionHost {
    handle: SessionHandle,
    origin: ActivityOrigin,
    actor: SessionActor,
}

impl SessionHost {
    pub fn new(handle: SessionHandle, origin: ActivityOrigin, actor: SessionActor) -> Self {
        Self {
            handle,
            origin,
            actor,
        }
    }
}

impl ToolHost for SessionHost {
    fn snapshot(&self) -> Result<SessionSnapshot> {
        Ok(self.handle.snapshot())
    }

    fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
        match camera {
            Some(camera) => {
                self.handle
                    .request_to_as(camera, command, self.origin, self.actor.clone())
            }
            None => self
                .handle
                .request_as(command, self.origin, self.actor.clone()),
        }
    }

    fn has_session(&self) -> bool {
        true
    }

    fn actor_id(&self) -> Option<String> {
        Some(self.actor.id.clone())
    }
    fn with_actor(&self, actor: SessionActor) -> Result<Arc<dyn ToolHost>> {
        Ok(Arc::new(Self::new(self.handle.clone(), self.origin, actor)))
    }
}

/// A named session in another process (`gui --name` or `serve --name`),
/// over local RPC.
pub struct IpcHost {
    name: String,
    actor: Option<SessionActor>,
    /// Every command's RPC deadline; by default each command's own
    /// (`session::default_request_timeout`).
    timeout_override: Option<Duration>,
}

impl IpcHost {
    pub fn new(
        name: impl Into<String>,
        actor: Option<SessionActor>,
        timeout_override: Option<Duration>,
    ) -> Self {
        Self {
            name: name.into(),
            actor,
            timeout_override,
        }
    }

    /// The actor commands are credited to: the caller's, or this process
    /// as `ipc::call_to` would send it.
    fn actor(&self) -> SessionActor {
        self.actor
            .clone()
            .unwrap_or_else(|| SessionActor::process(ActivityOrigin::Client))
    }
}

impl ToolHost for IpcHost {
    fn snapshot(&self) -> Result<SessionSnapshot> {
        snapshot_from(self.execute(None, SessionCommand::Status)?)
    }

    fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
        let timeout = self
            .timeout_override
            .unwrap_or_else(|| default_request_timeout(&command));
        crate::ipc::call_to_as(&self.name, camera, command, timeout, Some(&self.actor()))
    }

    fn has_session(&self) -> bool {
        true
    }

    fn actor_id(&self) -> Option<String> {
        Some(self.actor().id)
    }
    fn with_actor(&self, actor: SessionActor) -> Result<Arc<dyn ToolHost>> {
        Ok(Arc::new(Self::new(
            self.name.clone(),
            Some(actor),
            self.timeout_override,
        )))
    }
}

/// An unaddressed Status reply as the snapshot it serializes.
pub fn snapshot_from(status: Value) -> Result<SessionSnapshot> {
    serde_json::from_value(status).context("the session sent an unreadable status")
}

/// No session: offline tools still work, and anything that would command a
/// camera refuses.
pub struct NoSession;

/// The CLI's message for commands that need a running session.
pub const NO_SESSION: &str = "this command needs a running session; open `capturefab gui` or `capturefab serve --name NAME`, then choose --session NAME";

impl ToolHost for NoSession {
    fn snapshot(&self) -> Result<SessionSnapshot> {
        Ok(SessionSnapshot::default())
    }

    fn execute(&self, _camera: Option<&str>, _command: SessionCommand) -> Result<Value> {
        anyhow::bail!(NO_SESSION)
    }

    fn has_session(&self) -> bool {
        false
    }
}

/// The time zone wall-clock times are read and shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// This computer's zone, with its daylight saving rules.
    Local,
    /// A fixed offset east of UTC in seconds, for tests.
    Fixed(i32),
}

impl Zone {
    /// The UTC offset in effect at `unix_ms`.
    pub fn offset(self, unix_ms: u64) -> FixedOffset {
        let utc = FixedOffset::east_opt(0).expect("UTC is a valid offset");
        match self {
            Zone::Fixed(seconds) => FixedOffset::east_opt(seconds).unwrap_or(utc),
            Zone::Local => Local
                .timestamp_millis_opt(i64::try_from(unix_ms).unwrap_or(i64::MAX))
                .single()
                .map_or(utc, |time| time.offset().fix()),
        }
    }

    pub fn at(self, unix_ms: u64) -> DateTime<FixedOffset> {
        DateTime::from_timestamp_millis(i64::try_from(unix_ms).unwrap_or(i64::MAX))
            .unwrap_or_default()
            .with_timezone(&self.offset(unix_ms))
    }

    /// RFC 3339 with the local offset, to the second:
    /// "2026-10-18T17:00:00+02:00".
    pub fn rfc3339(self, unix_ms: u64) -> String {
        self.at(unix_ms).to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    /// The local time of day, "17:00:05".
    pub fn clock(self, unix_ms: u64) -> String {
        self.at(unix_ms).format("%H:%M:%S").to_string()
    }
}

/// Everything checks need, as plain data: checking a call does no I/O.
#[derive(Clone, Debug)]
pub struct Context {
    /// When this was gathered, UTC Unix milliseconds.
    pub now_ms: u64,
    pub zone: Zone,
    /// A session is running; without one `snapshot` is empty.
    pub session: bool,
    pub snapshot: SessionSnapshot,
    /// Consent to send camera dataset rows and notes to the model.
    pub allow_data: bool,
    pub data_dataset: Option<String>,
    pub destinations: Vec<DestinationInfo>,
    pub storage: StorageInfo,
    pub media: MediaInfo,
    /// Where captures without a destination go.
    pub capture_root: PathBuf,
    pub compiled: Compiled,
    pub defaults: RecordingDefaults,
    /// Saved camera setups (`setup::Store`), in name order. Unreadable
    /// files are left out, as `setup list` leaves them out.
    pub setups: Vec<SavedSetup>,
    /// Why the setup store, or some of its files, could not be read: the
    /// setup tools report it instead of claiming no setup is saved.
    pub setups_problem: Option<String>,
}

/// A saved setup as checks read it, with the digest that pins its content.
#[derive(Clone, Debug)]
pub struct SavedSetup {
    pub setup: Arc<crate::setup::Setup>,
    pub digest: String,
}

impl SavedSetup {
    pub fn new(setup: crate::setup::Setup) -> Self {
        Self {
            digest: crate::setup::digest(&setup),
            setup: Arc::new(setup),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StorageInfo {
    /// The policy commands send: the desktop's storage settings, or the
    /// configured policy on the command line.
    pub request: StoragePolicy,
    /// What applies: `request` within the configured policy.
    pub effective: StoragePolicy,
    /// All charged bytes, including outstanding reservations.
    pub allocated_bytes: u64,
    /// Unwritten reserved bytes, already included in `allocated_bytes`.
    pub reserved_bytes: u64,
    pub files: u64,
    /// Free disk space at the capture folder (or its nearest existing
    /// parent).
    pub free_disk: Option<u64>,
}

impl StorageInfo {
    /// Bytes the budget still allows after all global charges.
    pub fn free_budget(&self) -> u64 {
        self.effective
            .max_bytes
            .saturating_sub(self.allocated_bytes)
    }
}

/// FFmpeg's video encoders, by the codec they write (`h264`, `hevc`, `av1`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct MediaInfo {
    pub available: bool,
    pub encoders_by_codec: BTreeMap<String, Vec<String>>,
    /// Why FFmpeg could not be asked, when it could not.
    pub problem: Option<String>,
}

impl MediaInfo {
    /// The video encoders among FFmpeg's encoder `names`.
    pub fn from_names(names: &[String]) -> Self {
        let mut encoders_by_codec: BTreeMap<String, Vec<String>> = ["h264", "hevc", "av1"]
            .into_iter()
            .map(|codec| (codec.to_owned(), Vec::new()))
            .collect();
        for name in names {
            if let Some(codec) = crate::media::encoder_family(name) {
                encoders_by_codec
                    .entry(codec.to_owned())
                    .or_default()
                    .push(name.clone());
            }
        }
        Self {
            available: true,
            encoders_by_codec,
            problem: None,
        }
    }

    fn unavailable(problem: String) -> Self {
        Self {
            available: false,
            problem: Some(problem),
            ..Self::from_names(&[])
        }
    }
}

/// What recordings use when a call leaves codec, fps or bitrate null: the
/// desktop's Video settings, or h264 at 30 fps and 4 Mbit/s on the command
/// line.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordingDefaults {
    pub codec: String,
    pub fps: f64,
    pub bitrate: String,
}

impl Default for RecordingDefaults {
    fn default() -> Self {
        Self {
            codec: "h264".into(),
            fps: 30.0,
            bitrate: "4M".into(),
        }
    }
}

/// A saved destination as tools show it: never credentials, access key IDs
/// or the home folder's absolute path.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DestinationInfo {
    /// Private approval identity for the complete saved target, including
    /// login and credential selection. Never part of model or CLI output.
    #[serde(skip)]
    pub(crate) approval_identity: Option<String>,
    pub name: String,
    /// The destination kind, always `folder`.
    pub kind: String,
    /// Where files go: a folder written from `~`.
    pub location: String,
    /// The server files are sent to, for network destinations.
    pub host: Option<String>,
    /// Files travel unencrypted.
    pub insecure: bool,
    /// Why saving there would fail now, such as an unmounted drive.
    pub problem: Option<String>,
    /// Free space on the destination volume.
    pub free_bytes: Option<u64>,
    /// Files leave this computer.
    pub remote: bool,
}

impl DestinationInfo {
    pub(crate) fn new(
        destination: &Destination,
        compiled: &Compiled,
        sources: &dyn Sources,
    ) -> Self {
        let root = destination.local_root();
        let _ = compiled;
        Self {
            approval_identity: Self::identity(destination),
            name: destination.name.clone(),
            kind: "folder".into(),
            location: super::redact::home_path(&root),
            host: None,
            insecure: false,
            free_bytes: sources.free_space(&root),
            problem: sources.volume_problem(&root),
            remote: false,
        }
    }

    pub(crate) fn identity(destination: &Destination) -> Option<String> {
        use sha2::Digest;
        serde_json::to_vec(destination)
            .ok()
            .map(|bytes| format!("{:x}", sha2::Sha256::digest(bytes)))
    }
}

/// Optional features this build has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Compiled {
    pub jpeg: bool,
}

impl Compiled {
    pub fn current() -> Self {
        Self {
            jpeg: cfg!(feature = "jpeg"),
        }
    }
    pub fn supports(&self, kind: &str) -> bool {
        kind == "folder"
    }
}

impl Context {
    /// Read the session through `host` and everything else from this
    /// computer. `capture_root` is where captures without a destination go;
    /// `request_policy` is the storage policy commands will send.
    pub fn gather(
        host: &dyn ToolHost,
        capture_root: PathBuf,
        request_policy: StoragePolicy,
        defaults: RecordingDefaults,
    ) -> Result<Context> {
        gather_with(&System, host, capture_root, request_policy, defaults)
    }
}

/// Where `Context::gather` reads what the session snapshot does not hold.
/// Tests substitute fixed values.
pub(crate) trait Sources {
    fn destinations(&self) -> Result<Vec<Destination>>;
    fn configured_policy(&self) -> Result<StoragePolicy>;
    /// `storage::quota_status()`.
    fn quota(&self) -> Result<Value>;
    fn encoders(&self) -> Result<Arc<Vec<String>>>;
    /// Free space at `path`, or at its nearest existing parent.
    fn free_space(&self, path: &Path) -> Option<u64>;
    /// Why `path` cannot be written now: an external volume that is not
    /// mounted.
    fn volume_problem(&self, path: &Path) -> Option<String>;
    /// Every readable saved setup, and why the store or some of its files
    /// could not be read.
    fn setups(&self) -> (Vec<crate::setup::Setup>, Option<String>);
}

/// This computer.
struct System;

impl Sources for System {
    fn destinations(&self) -> Result<Vec<Destination>> {
        crate::destination::list()
    }

    fn configured_policy(&self) -> Result<StoragePolicy> {
        crate::storage::configured_policy()
    }

    fn quota(&self) -> Result<Value> {
        crate::storage::quota_status()
    }

    fn encoders(&self) -> Result<Arc<Vec<String>>> {
        crate::media::encoder_names()
    }

    fn free_space(&self, path: &Path) -> Option<u64> {
        let existing = path.ancestors().find(|ancestor| {
            !ancestor.as_os_str().is_empty() && std::fs::metadata(ancestor).is_ok()
        })?;
        fs2::available_space(existing).ok()
    }

    fn volume_problem(&self, path: &Path) -> Option<String> {
        crate::volumes::ensure_present(path)
            .err()
            .map(|error| format!("{error:#}"))
    }

    fn setups(&self) -> (Vec<crate::setup::Setup>, Option<String>) {
        let store = crate::setup::Store::session();
        setups_in(&store)
    }
}

/// The setups `store` holds, and a problem naming the store's folder when
/// it or some of its files cannot be read.
pub(crate) fn setups_in(store: &crate::setup::Store) -> (Vec<crate::setup::Setup>, Option<String>) {
    let dir = store.dir().display();
    match store.load() {
        Ok((setups, unreadable)) if unreadable.is_empty() => (setups, None),
        Ok((setups, unreadable)) => {
            let names: Vec<&str> = unreadable.iter().map(String::as_str).collect();
            (
                setups,
                Some(format!(
                    "{} in {dir} could not be read and {} left out; run capturefab setup list.",
                    super::list(&names),
                    if names.len() == 1 { "is" } else { "are" }
                )),
            )
        }
        Err(error) => (
            Vec::new(),
            Some(format!(
                "The setup store {dir} could not be read: {error:#}; run capturefab setup list."
            )),
        ),
    }
}

pub(crate) fn gather_with(
    sources: &dyn Sources,
    host: &dyn ToolHost,
    capture_root: PathBuf,
    request_policy: StoragePolicy,
    defaults: RecordingDefaults,
) -> Result<Context> {
    let now_ms = crate::scheduling::now_ms();
    let session = host.has_session();
    let snapshot = if session {
        host.snapshot()?
    } else {
        SessionSnapshot::default()
    };
    let compiled = Compiled::current();
    let destinations = sources
        .destinations()
        .context("read saved destinations")?
        .iter()
        .map(|destination| DestinationInfo::new(destination, &compiled, sources))
        .collect();
    let configured = sources
        .configured_policy()
        .context("read the storage policy")?;
    let quota = sources.quota().context("read storage use")?;
    let count = |key: &str| quota[key].as_u64().unwrap_or(0);
    let storage = StorageInfo {
        effective: request_policy.constrained_by(&configured),
        request: request_policy,
        allocated_bytes: count("allocated_bytes"),
        reserved_bytes: count("reserved_bytes"),
        files: count("files"),
        free_disk: sources.free_space(&capture_root),
    };
    // A setup store that cannot be read leaves only the setup tools without
    // setups, and they say why; every other tool still works.
    let (setups, setups_problem) = sources.setups();
    let setups = setups.into_iter().map(SavedSetup::new).collect();
    let media = match sources.encoders() {
        Ok(names) => MediaInfo::from_names(&names),
        Err(error) => MediaInfo::unavailable(format!("{error:#}")),
    };
    Ok(Context {
        now_ms,
        zone: Zone::Local,
        session,
        snapshot,
        allow_data: false,
        data_dataset: None,
        destinations,
        storage,
        media,
        capture_root,
        compiled,
        defaults,
        setups,
        setups_problem,
    })
}

#[cfg(test)]
mod approval_identity_tests {
    use super::*;
    use crate::destination::Target;
    #[test]
    fn identity_binds_the_complete_folder_destination() {
        let mut destination = Destination {
            name: "capture".into(),
            target: Target::Folder {
                path: std::env::temp_dir().join("first"),
            },
        };
        let before = DestinationInfo::identity(&destination).unwrap();
        destination.target = Target::Folder {
            path: std::env::temp_dir().join("second"),
        };
        assert_ne!(before, DestinationInfo::identity(&destination).unwrap());
    }
}
