//! What tools know when they check or run a call: the session (through a
//! `ToolHost`), saved destinations, the storage budget, FFmpeg's encoders
//! and the uploads queue, gathered into one plain `Context`.
//!
//! `Context::gather` does file and process I/O (FFmpeg runs once per
//! process), so callers run it on a worker thread, never the GUI's. Checks
//! then read the `Context` alone. It never holds the storage ledger's path,
//! access key IDs or absolute home paths.
use crate::{
    destination::{Destination, Remote},
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

mod task_data;

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
    /// Create a coordinator-local private data capability. Serialized actor,
    /// task or dataset identifiers cannot select or adopt one.
    fn with_task_data(
        &self,
        _policy: crate::session::task_data::TaskDataPolicy,
    ) -> Result<Arc<dyn ToolHost>> {
        anyhow::bail!(
            "This tool host cannot create private task data; use a local workbench session."
        )
    }
    fn supports_task_data(&self) -> bool {
        false
    }
    fn task_data_reserved_bytes(&self) -> u64 {
        0
    }
    fn task_data_status(&self) -> Option<crate::session::task_data::TaskDataStatus> {
        None
    }
    fn request_close_task_data(&self) {}
    fn close_task_data(
        &self,
        _wait: Duration,
    ) -> Result<Option<crate::session::task_data::TaskDataStatus>> {
        Ok(None)
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
    fn with_task_data(
        &self,
        policy: crate::session::task_data::TaskDataPolicy,
    ) -> Result<Arc<dyn ToolHost>> {
        anyhow::ensure!(
            self.supports_task_data(),
            "Private task data requires a supported local build with Parquet support."
        );
        let data = self.handle.begin_task_data(policy)?;
        Ok(Arc::new(task_data::NativeTaskHost::new(
            Self::new(self.handle.clone(), self.origin, self.actor.clone()),
            data,
        )))
    }
    fn supports_task_data(&self) -> bool {
        cfg!(all(unix, feature = "parquet"))
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

    pub fn name(&self) -> &str {
        &self.name
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
    /// Human-approved source endpoints. Raw locators remain local and are
    /// never serialized in model context; tools use their opaque references.
    pub data_sources: Vec<DataSourceRef>,
    /// Current nonsecret profile metadata, read on the gathering worker. It
    /// never includes usernames, password values or environment names.
    pub(crate) data_profiles: Vec<crate::signals::auth::ProfileInfo>,
    /// Consent to send source values, dataset rows and notes to the model.
    pub allow_data: bool,
    pub data_dataset: Option<String>,
    /// Local host capabilities and promises; model text/IDs cannot create them.
    pub task_data_available: bool,
    pub task_data_reserved_bytes: u64,
    pub destinations: Vec<DestinationInfo>,
    pub storage: StorageInfo,
    pub media: MediaInfo,
    /// The uploads queue, in builds with network destinations.
    pub uploads: Option<UploadInfo>,
    /// Where captures without a destination go.
    pub capture_root: PathBuf,
    pub compiled: Compiled,
    pub defaults: RecordingDefaults,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DataSourceRef {
    pub reference: String,
    pub uri: String,
    /// A coordinator-issued credential epoch. No credential value is held here.
    pub auth: Option<crate::signals::auth::AuthBinding>,
}
impl DataSourceRef {
    pub fn cached(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            uri: String::new(),
            auth: None,
        }
    }
    pub fn cached_source(source: &crate::signals::SourceStatus) -> Self {
        Self {
            reference: source.id.clone(),
            uri: String::new(),
            auth: source.auth.clone(),
        }
    }
    pub fn permits_cached(&self, source: &crate::signals::SourceStatus) -> bool {
        self.reference == source.id && self.auth == source.auth
    }
    pub fn from_uri(uri: &str) -> Result<Self> {
        crate::signals::parse(uri)?;
        Ok(Self {
            reference: crate::signals::source_id(uri),
            uri: uri.into(),
            auth: None,
        })
    }
    /// A local human entry for an assistant binding. Raw CLI tools retain
    /// `from_uri`; assistant forms never accept credentials in their locators.
    pub fn approved(uri: &str) -> Result<Self> {
        let uri = uri.trim();
        let authority = uri.split_once("://").map_or("", |(_, tail)| {
            tail.split(['/', '?', '#']).next().unwrap_or("")
        });
        anyhow::ensure!(
            !authority.contains('@') && super::redact::for_output_text(uri) == uri,
            "Enter a source without passwords or tokens. Start authenticated sources in the workbench first."
        );
        #[cfg(feature = "ai")]
        anyhow::ensure!(
            !crate::assistant::prompt::contains_secret(uri),
            "Enter a source without passwords or tokens. Start authenticated sources in the workbench first."
        );
        Self::from_uri(uri)
            .map_err(|error| anyhow::anyhow!(super::redact::for_output_text(&error.to_string())))
    }
    pub fn from_binding(receipt: &crate::signals::auth::BindingReceipt) -> Result<Self> {
        let mut source = Self::approved(&receipt.endpoint)?;
        anyhow::ensure!(
            source.reference == receipt.reference && source.uri == receipt.endpoint,
            "The saved source binding does not match its endpoint. Use the source again."
        );
        receipt.auth.validate()?;
        source.auth = Some(receipt.auth.clone());
        Ok(source)
    }
    pub fn profile_id(&self) -> Option<&str> {
        self.auth.as_ref().map(|auth| auth.profile.id.as_str())
    }
    pub fn validate_local(&self) -> Result<()> {
        let endpoint = Self::approved(&self.uri)?;
        anyhow::ensure!(
            endpoint.reference == self.reference && endpoint.uri == self.uri,
            "The approved source reference does not match its local endpoint. Add it again."
        );
        if let Some(auth) = &self.auth {
            auth.validate()?;
        }
        Ok(())
    }
    pub fn validate_in(&self, context: &Context) -> Result<()> {
        self.validate_local()?;
        if let Some(auth) = &self.auth {
            anyhow::ensure!(
                context.data_profiles.iter().any(|profile| {
                    profile.profile == auth.profile
                        && profile.endpoint == self.uri
                        && profile.reference == self.reference
                }) && context.snapshot.signal_bindings.iter().any(|receipt| {
                    receipt.auth == *auth
                        && receipt.endpoint == self.uri
                        && receipt.reference == self.reference
                }),
                "The saved source changed or its workbench restarted. Choose Use source again before running it."
            );
        }
        Ok(())
    }
    pub fn identity(&self) -> String {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        hash.update(self.uri.as_bytes());
        if let Some(auth) = &self.auth {
            for value in [&auth.profile.id, &auth.profile.revision, &auth.epoch] {
                hash.update([0]);
                hash.update(value.as_bytes());
            }
        }
        format!("{:x}", hash.finalize())
    }
}
impl std::fmt::Debug for DataSourceRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DataSourceRef")
            .field("reference", &self.reference)
            .field("uri", &super::redact::for_model_text(&self.uri))
            .field("auth", &self.auth)
            .finish()
    }
}

/// The storage budget and its use.
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
/// or the home folder's path.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DestinationInfo {
    /// Private approval identity for the complete saved target, including
    /// login and credential selection. Never part of model or CLI output.
    #[serde(skip)]
    pub(crate) approval_identity: Option<String>,
    pub name: String,
    /// `folder`, `s3`, `ftp` or `sftp`.
    pub kind: String,
    /// Where files go: a folder written from `~`, or the bucket or server.
    pub location: String,
    /// The server files are sent to, for network destinations.
    pub host: Option<String>,
    /// Files travel unencrypted.
    pub insecure: bool,
    /// Why saving there would fail now, such as an unmounted drive.
    pub problem: Option<String>,
    /// Free space where files are written first (the staging folder for
    /// network destinations).
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
        let remote = destination.target.remote();
        let root = destination.local_root();
        let problem = match &remote {
            Some(remote) if !compiled.supports(remote.kind()) => Some(format!(
                "{} destinations need a build with --features {}",
                remote.kind().to_ascii_uppercase(),
                remote.kind()
            )),
            _ => sources.volume_problem(&root),
        };
        Self {
            approval_identity: Self::identity(destination),
            name: destination.name.clone(),
            kind: destination.target.kind().into(),
            location: match &remote {
                Some(remote) => remote.location(),
                None => super::redact::home_path(&root),
            },
            host: remote.as_ref().map(remote_host),
            insecure: remote.as_ref().is_some_and(Remote::insecure),
            free_bytes: sources.free_space(&root),
            problem,
            remote: remote.is_some(),
        }
    }

    pub(crate) fn identity(destination: &Destination) -> Option<String> {
        use sha2::Digest;
        serde_json::to_vec(destination)
            .ok()
            .map(|bytes| format!("{:x}", sha2::Sha256::digest(bytes)))
    }
}

fn remote_host(remote: &Remote) -> String {
    match remote {
        Remote::S3(bucket) => {
            let url = bucket.service_url();
            let authority = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
            let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host)
                .to_owned()
        }
        Remote::Ftp(server) => server.host.clone(),
        Remote::Sftp(server) => server.host.clone(),
    }
}

/// The uploads queue in brief.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UploadInfo {
    pub pending: u64,
    pub uploading: u64,
    pub failed: u64,
    pub waiting_bytes: u64,
    pub uploaded: u64,
    pub last_error: Option<String>,
    /// A process (the desktop app, `serve` or a CLI capture) is uploading.
    pub uploader_running: bool,
    /// Why the queue could not be read, when it could not.
    pub problem: Option<String>,
}

impl UploadInfo {
    fn from_status(status: Result<Value>) -> Self {
        match status {
            Ok(status) => {
                let count = |key: &str| status[key].as_u64().unwrap_or(0);
                Self {
                    pending: count("pending"),
                    uploading: count("uploading"),
                    failed: count("failed"),
                    waiting_bytes: count("waiting_bytes"),
                    uploaded: count("uploaded"),
                    last_error: status["last_error"].as_str().map(str::to_owned),
                    uploader_running: status["uploader_running"].as_bool().unwrap_or(false),
                    problem: None,
                }
            }
            Err(error) => Self {
                problem: Some(format!("{error:#}")),
                ..Self::default()
            },
        }
    }
}

/// Optional features this build has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Compiled {
    pub s3: bool,
    pub ftp: bool,
    pub sftp: bool,
    pub uploads: bool,
    pub jpeg: bool,
}

impl Compiled {
    pub fn current() -> Self {
        Self {
            s3: cfg!(feature = "s3"),
            ftp: cfg!(feature = "ftp"),
            sftp: cfg!(feature = "sftp"),
            uploads: cfg!(feature = "uploads"),
            jpeg: cfg!(feature = "jpeg"),
        }
    }

    /// Destinations of `kind` work in this build.
    pub fn supports(&self, kind: &str) -> bool {
        match kind {
            "folder" => true,
            "s3" => self.s3,
            "ftp" => self.ftp,
            "sftp" => self.sftp,
            _ => false,
        }
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
    fn signal_profiles(&self) -> Result<Vec<crate::signals::auth::ProfileInfo>> {
        Ok(Vec::new())
    }
    fn destinations(&self) -> Result<Vec<Destination>>;
    fn configured_policy(&self) -> Result<StoragePolicy>;
    /// `storage::quota_status()`.
    fn quota(&self) -> Result<Value>;
    fn encoders(&self) -> Result<Arc<Vec<String>>>;
    /// `upload::status()`, or None in builds without uploads.
    fn uploads(&self) -> Option<Result<Value>>;
    /// Free space at `path`, or at its nearest existing parent.
    fn free_space(&self, path: &Path) -> Option<u64>;
    /// Why `path` cannot be written now: an external volume that is not
    /// mounted.
    fn volume_problem(&self, path: &Path) -> Option<String>;
}

/// This computer.
struct System;

impl Sources for System {
    fn signal_profiles(&self) -> Result<Vec<crate::signals::auth::ProfileInfo>> {
        crate::signals::auth::Store::session().list()
    }
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

    #[cfg(feature = "uploads")]
    fn uploads(&self) -> Option<Result<Value>> {
        Some(crate::upload::status())
    }

    #[cfg(not(feature = "uploads"))]
    fn uploads(&self) -> Option<Result<Value>> {
        None
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
    let data_profiles = sources
        .signal_profiles()
        .context("read saved data sources")?;
    let data_sources = snapshot
        .signals
        .iter()
        .map(DataSourceRef::cached_source)
        .collect();
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
    let media = match sources.encoders() {
        Ok(names) => MediaInfo::from_names(&names),
        Err(error) => MediaInfo::unavailable(format!("{error:#}")),
    };
    Ok(Context {
        now_ms,
        zone: Zone::Local,
        session,
        snapshot,
        data_sources,
        data_profiles,
        allow_data: false,
        data_dataset: None,
        task_data_available: host.supports_task_data(),
        task_data_reserved_bytes: host.task_data_reserved_bytes(),
        destinations,
        storage,
        media,
        uploads: sources.uploads().map(UploadInfo::from_status),
        capture_root,
        compiled,
        defaults,
    })
}

#[cfg(test)]
mod approval_identity_tests {
    use super::*;
    use crate::destination::{FtpServer, SecretSource, Target};

    #[test]
    fn signal_bindings_pin_profile_revision_epoch_and_coordinator() {
        let mut context =
            crate::tools::tests::fixture_context(crate::tools::tests::fixture_snapshot());
        let source = crate::tools::tests::fixture_bound_source(&mut context, "mqtts://host/topic");
        source.validate_in(&context).unwrap();
        let identity = source.identity();
        let mut rotated = source.clone();
        rotated.auth.as_mut().unwrap().epoch = "d".repeat(32);
        assert_ne!(identity, rotated.identity());
        assert!(rotated.validate_in(&context).is_err());
        context.data_profiles[0].profile.revision = "e".repeat(32);
        assert!(source.validate_in(&context).is_err());
        context.data_profiles[0].profile = source.auth.as_ref().unwrap().profile.clone();
        source.validate_in(&context).unwrap();
        context.snapshot.signal_bindings.clear();
        assert!(source.validate_in(&context).is_err());
    }

    #[test]
    fn identity_binds_the_login_and_credential_selection_omitted_from_location() {
        let mut destination: Destination = serde_json::from_value(serde_json::json!({
            "name":"server", "kind":"ftp", "host":"files.example.test", "user":"alice"
        }))
        .unwrap();
        let Target::Ftp(server) = &destination.target else {
            panic!("FTP fixture");
        };
        let location = Remote::Ftp(server.clone()).location();
        let first = DestinationInfo::identity(&destination).unwrap();
        let Target::Ftp(server) = &mut destination.target else {
            unreachable!()
        };
        server.user = "bob".into();
        assert_eq!(Remote::Ftp(server.clone()).location(), location);
        let second = DestinationInfo::identity(&destination).unwrap();
        assert_ne!(first, second);
        let Target::Ftp(FtpServer { password, .. }) = &mut destination.target else {
            unreachable!()
        };
        *password = SecretSource::Environment;
        assert_ne!(second, DestinationInfo::identity(&destination).unwrap());
    }
}
