use crate::{
    ipc,
    session::{ActivityOrigin, ActorKind, SessionActor, SessionCommand, SessionHandle},
};
use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug, Parser)]
#[command(
    name = "capturefab",
    version,
    about = "Discover, configure, capture and record cameras. No command opens the GUI.",
    long_about = "Capturefab is a camera workbench for people and automation. Camera commands use the only running GUI or headless session automatically. Use --session NAME to choose one, or --direct --camera ID for a temporary camera worker. Local output paths are relative to this terminal, including when controlling the GUI.",
    after_help = "Quick start:\n  capturefab                                  Open the visible workbench\n  capturefab discover --simulate --json        Find sources (including a test camera)\n  capturefab --camera sim:0 capture -o shot.png Capture in the running workbench\n  capturefab --direct -c sim:0 capture -o shot.png\n  capturefab --session gui set ExposureTime=5000\n  capturefab workflow validate workflow.json\n  capturefab --session gui workflow apply workflow.json\n\nExplore: <command> --help, schema --json, doctor --json, completions <shell>.\nPersistent work: start/stop, auto/manual, capture --interval, record, jobs/cancel.\nUse --json for one versioned result; progress and diagnostics go to stderr."
)]
pub struct Cli {
    /// Emit one versioned JSON result on stdout; diagnostics stay on stderr
    #[arg(long, global = true)]
    pub json: bool,
    /// Camera/scanner ID, serial, IP, media source, sim:0, or sim:replay:PATH
    #[arg(
        short = 'c',
        long,
        global = true,
        env = "CAPTUREFAB_CAMERA",
        hide_env_values = true,
        help_heading = "Target"
    )]
    pub camera: Option<String>,
    /// Choose a GUI/serve session (default: CAPTUREFAB_SESSION, then the only running session)
    #[arg(long, global = true, help_heading = "Target")]
    pub session: Option<String>,
    /// Name shown to other people and agents in the shared activity timeline
    #[arg(long, global = true, env = "CAPTUREFAB_ACTOR", help_heading = "Target")]
    pub actor: Option<String>,
    /// Identify the caller as a human or AI agent (attribution, not authentication)
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "human",
        env = "CAPTUREFAB_ACTOR_KIND",
        help_heading = "Target"
    )]
    pub actor_kind: CallerKind,
    /// Open a temporary worker instead of automatically using a running session
    #[arg(
        long,
        global = true,
        conflicts_with = "session",
        help_heading = "Target"
    )]
    pub direct: bool,
    /// Deadline for a session request, e.g. 30s or 5m (default adapts to capture count)
    #[arg(long, global = true, value_parser = parse_duration_text, help_heading = "Runtime")]
    pub request_timeout: Option<String>,
    /// Per-frame or transport timeout in milliseconds; physical scans may need 60000
    #[arg(long,global=true,default_value_t=2000,value_parser=clap::value_parser!(u64).range(1..=60_000))]
    pub timeout_ms: u64,
    /// Include the built-in pattern camera in discovery
    #[arg(long, global = true)]
    pub simulate: bool,
    /// Decoding acceleration for new direct/GUI/serve workers; existing sessions keep their setting
    #[arg(
        long,
        global = true,
        env = "CAPTUREFAB_HWACCEL",
        default_value = "auto"
    )]
    pub hwaccel: String,
    #[command(subcommand)]
    pub command: Option<Command>,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CallerKind {
    Human,
    Agent,
}
impl Cli {
    fn session_actor(&self) -> Result<SessionActor> {
        let kind = match self.actor_kind {
            CallerKind::Human => ActorKind::Human,
            CallerKind::Agent => ActorKind::Agent,
        };
        if let Some(name) = &self.actor {
            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(name.as_bytes());
            let id = format!(
                "cli-{}-{hash:x}",
                match self.actor_kind {
                    CallerKind::Human => "human",
                    CallerKind::Agent => "agent",
                }
            );
            SessionActor::new(id, name.clone(), kind)
        } else {
            let mut actor = SessionActor::process(ActivityOrigin::Client);
            actor.kind = kind;
            Ok(actor)
        }
    }
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ImageFormat {
    Png,
    #[value(alias = "jpg")]
    Jpeg,
    Raw,
    Pgm,
    Ppm,
}
impl ImageFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Raw => "raw",
            Self::Pgm => "pgm",
            Self::Ppm => "ppm",
        }
    }
}
fn capture_format(format: Option<ImageFormat>, output: &str, count: u32) -> Result<ImageFormat> {
    if let Some(format) = format {
        return Ok(format);
    }
    if count > 1 && !crate::naming::has_placeholder(output)
        || output == "-"
        || std::path::Path::new(output).is_dir()
    {
        return Ok(ImageFormat::Png);
    }
    match std::path::Path::new(output)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("png") => Ok(ImageFormat::Png),
        Some("jpg" | "jpeg") => Ok(ImageFormat::Jpeg),
        Some("raw") => Ok(ImageFormat::Raw),
        Some("pgm") => Ok(ImageFormat::Pgm),
        Some("ppm") => Ok(ImageFormat::Ppm),
        Some(extension) => anyhow::bail!(
            "cannot infer an image format from .{extension}; use .png, .jpg, .raw, .pgm, .ppm or specify --format"
        ),
    }
}

/// A CLI path belongs to the caller, even when the worker lives in a GUI
/// launched from a different directory. Named destinations retain their own base.
fn caller_output(command: &mut SessionCommand) -> Result<()> {
    if matches!(command, SessionCommand::Capture { output, .. } if output == "-") {
        return Ok(());
    }
    crate::workflow::resolve_outputs(command, &std::env::current_dir()?)
}
#[derive(Debug, Args)]
pub struct StorageArgs {
    /// Maximum owned bytes across all outputs (default: saved global policy, initially 10GiB)
    #[arg(long, value_parser = parse_bytes_text)]
    max_space: Option<String>,
    /// Maximum owned files (default: saved global policy, initially 10000)
    #[arg(long,value_parser=clap::value_parser!(u32).range(1..=100_000))]
    max_files: Option<u32>,
    /// Explicitly delete oldest owned files when the budget is reached
    #[arg(long, conflicts_with = "on_full")]
    delete_oldest: bool,
    /// Explicit behavior when storage is full
    #[arg(long,value_parser=["stop","delete-oldest"])]
    on_full: Option<String>,
    /// Expire owned files older than this duration; requires delete-oldest policy
    #[arg(long, conflicts_with = "no_max_age", value_parser = parse_age_text)]
    max_age: Option<String>,
    /// Clear a previously configured age limit
    #[arg(long)]
    no_max_age: bool,
}
impl StorageArgs {
    fn policy(&self) -> Result<crate::storage::StoragePolicy> {
        let policy = crate::storage::configured_policy()?;
        input(self.apply_policy(policy))
    }
    fn apply_policy(
        &self,
        mut policy: crate::storage::StoragePolicy,
    ) -> Result<crate::storage::StoragePolicy> {
        if let Some(space) = &self.max_space {
            policy.max_bytes = crate::storage::parse_bytes(space)?;
        }
        if let Some(files) = self.max_files {
            policy.max_files = files;
        }
        if self.delete_oldest {
            policy.on_full = "delete-oldest".into();
        }
        if let Some(on_full) = &self.on_full {
            policy.on_full = on_full.clone();
        }
        if self.no_max_age {
            policy.max_age_seconds = None;
        }
        if let Some(age) = &self.max_age {
            let millis = crate::scheduling::parse_duration(age)?;
            ensure!(
                millis.is_multiple_of(1000),
                "--max-age must resolve to a whole number of seconds (minimum 1s)"
            );
            policy.max_age_seconds = Some(millis / 1000);
        }
        ensure!(
            policy.max_age_seconds.is_none() || policy.on_full == "delete-oldest",
            "--max-age requires --delete-oldest or a saved delete-oldest policy"
        );
        policy.validate()?;
        Ok(policy)
    }
}
#[derive(Debug, Args)]
pub struct AutoArgs {
    /// Enable auto mode first so Capturefab tunes exposure, gain and frame rate
    #[arg(long)]
    auto: bool,
    /// Auto mode trade-off from 0 (image quality) to 1 (frame rate), or quality, balanced, frame-rate
    #[arg(long, requires = "auto", value_parser = parse_balance)]
    balance: Option<f64>,
}
impl AutoArgs {
    fn apply(&self, client: &Client) -> Result<()> {
        if self.auto {
            client.call(SessionCommand::Auto {
                balance: self.balance,
            })?;
        }
        Ok(())
    }
}
#[derive(Debug, Args)]
pub struct DatasetArg {
    /// Catalog saved files in this dataset (.duckdb); created if missing
    #[arg(long, env = "CAPTUREFAB_DATASET")]
    pub dataset: Option<PathBuf>,
}
impl DatasetArg {
    /// Open the dataset in the local or addressed session before the command runs.
    fn apply(&self, client: &Client) -> Result<()> {
        let Some(path) = &self.dataset else {
            return Ok(());
        };
        let path = input(absolute_dataset(path))?;
        if client.local.is_none() {
            // A session keeps the dataset it has; refuse to switch silently.
            let status = client.call(SessionCommand::DatasetStatus)?;
            if let Some(open) = status["path"].as_str() {
                let same =
                    canonical_dataset(&path).is_some_and(|p| p == std::path::Path::new(open));
                ensure!(
                    same,
                    "session {} already records to {}; close it first",
                    client.cli.session.as_deref().unwrap_or(""),
                    open
                );
                return Ok(());
            }
        }
        client.call(SessionCommand::DatasetOpen {
            path: path.to_string_lossy().into_owned(),
            mode: crate::dataset::OpenMode::Auto,
            max_bytes: None,
        })?;
        Ok(())
    }
}
#[derive(Debug, Subcommand)]
pub enum DatasetCommand {
    /// Open or create the session's dataset; existing files are never overwritten
    Open {
        path: PathBuf,
        /// Create a new file; fail if it exists
        #[arg(long, conflicts_with = "existing")]
        new: bool,
        /// Open an existing dataset; fail if it is missing
        #[arg(long)]
        existing: bool,
        /// Largest size the dataset file may grow to, e.g. 8GiB
        #[arg(long, value_parser = parse_bytes_text)]
        max_bytes: Option<String>,
    },
    /// Checkpoint and close the session's dataset
    Close,
    /// Show the session's dataset, or summarize --file
    Status {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Add a timestamped note (shift handover, defect, setup change)
    Note {
        text: String,
        /// Tag the note, e.g. --context line=L1 (repeatable)
        #[arg(long = "context", value_name = "KEY=VALUE", value_parser = crate::context::parse_pair)]
        context: Vec<(String, String)>,
    },
    /// Run one read-only SELECT; --file reads a dataset no session holds
    Query {
        #[arg(long)]
        file: Option<PathBuf>,
        sql: String,
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=10_000))]
        limit: u32,
    },
    /// Write data files, manifest.json and README.md into a new folder
    Export {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(short = 'o', long)]
        output: PathBuf,
        #[arg(long, value_enum, value_delimiter = ',')]
        format: Vec<crate::dataset::ExportFormat>,
        #[arg(long, value_delimiter = ',')]
        tables: Vec<String>,
        /// Only rows at or after this time (RFC 3339 with timezone)
        #[arg(long, value_parser = parse_at_text)]
        since: Option<String>,
        /// Only rows at or before this time (RFC 3339 with timezone)
        #[arg(long, value_parser = parse_at_text)]
        until: Option<String>,
        #[command(flatten)]
        storage: StorageArgs,
    },
}
#[derive(Debug, Subcommand)]
pub enum SignalsCommand {
    /// Read current values once
    Read { uri: String },
    /// List OPC UA nodes, MQTT topics or MTConnect data items
    Browse {
        uri: String,
        #[arg(long, default_value = "5s", value_parser = parse_duration_text)]
        duration: String,
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u32).range(1..=5000))]
        limit: u32,
    },
    /// Start logging in a running session
    Start {
        uri: String,
        #[arg(long, default_value = "1s", value_parser = parse_duration_text)]
        interval: String,
        /// Log only these tags (repeatable)
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Stop logging (ID from `signals`, or `all`)
    Stop { id: String },
}
#[derive(Debug, Subcommand)]
pub enum DestinationCommand {
    /// List saved destinations
    List,
    /// Save a folder destination, such as a directory on an external drive or network share
    AddFolder { name: String, path: PathBuf },
    /// Save an S3-compatible bucket (AWS S3, Cloudflare R2, Backblaze B2, MinIO, RustFS, Wasabi)
    AddS3 {
        name: String,
        #[arg(long)]
        bucket: String,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// Service URL for non-AWS services, e.g. https://ACCOUNT.r2.cloudflarestorage.com or http://nas:9000
        #[arg(long, default_value = "")]
        endpoint: String,
        /// Key prefix, e.g. cameras/line-1/
        #[arg(long, default_value = "")]
        prefix: String,
        /// Address the bucket in the URL path (MinIO and most non-AWS services)
        #[arg(long)]
        path_style: bool,
        /// Access key ID whose secret is kept in the OS credential store
        #[arg(long, conflicts_with_all = ["profile", "env"])]
        access_key_id: Option<String>,
        /// Read the secret access key from stdin and save it in the OS credential store
        #[arg(long, requires = "access_key_id")]
        secret_stdin: bool,
        /// Use this profile from the AWS shared credentials file instead
        #[arg(long, conflicts_with = "env")]
        profile: Option<String>,
        /// Use AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY from the uploading process
        #[arg(long)]
        env: bool,
        /// Keep local copies after a verified upload
        #[arg(long)]
        keep_local: bool,
        /// Save without checking that the bucket is reachable
        #[arg(long)]
        no_check: bool,
        /// Self-hosted service settings (path-style addressing); needs --endpoint
        #[arg(long, value_enum)]
        preset: Option<S3Preset>,
        /// Check that the service refuses to replace objects, and use check-first uploads if it does not (implied by --preset unless --no-check)
        #[arg(long, conflicts_with_all = ["no_check", "check_first"])]
        probe_writes: bool,
        /// Look up each key before writing, for services that ignore conditional writes
        #[arg(long)]
        check_first: bool,
    },
    /// Save an FTP server: explicit FTPS (AUTH TLS) unless --plain
    AddFtp {
        name: String,
        /// DNS name, IPv4 address or [IPv6] address
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 21, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[arg(long)]
        user: String,
        /// Folder for uploads: relative to the login folder, or absolute with a leading /
        #[arg(long, default_value = "")]
        dir: String,
        /// Plain FTP without TLS: files and the password travel unencrypted
        #[arg(long)]
        plain: bool,
        /// Read the password from stdin and save it in the OS credential store
        #[arg(long, conflicts_with_all = ["password_env", "anonymous"])]
        password_stdin: bool,
        /// Use CAPTUREFAB_FTP_PASSWORD from the uploading process
        #[arg(long, conflicts_with = "anonymous")]
        password_env: bool,
        /// Sign in as an anonymous user, without a password
        #[arg(long)]
        anonymous: bool,
        /// Keep local copies after a verified upload
        #[arg(long)]
        keep_local: bool,
        /// Save without checking that the server is reachable
        #[arg(long)]
        no_check: bool,
    },
    /// Save an SFTP server, pinned to its host key
    #[command(group(clap::ArgGroup::new("trust").required(true).args(["host_key", "accept_host_key"])))]
    AddSftp {
        name: String,
        /// DNS name, IPv4 address or [IPv6] address
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 22, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[arg(long)]
        user: String,
        /// Folder for uploads: relative to the login folder, or absolute with a leading /
        #[arg(long, default_value = "")]
        dir: String,
        /// The server's OpenSSH host-key fingerprint, SHA256:... (on the server:
        /// ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub; a wrong one is refused
        /// with the fingerprint the server offered)
        #[arg(long)]
        host_key: Option<String>,
        /// Trust the key the server presents now; confirm its fingerprint with the administrator
        #[arg(long, conflicts_with = "no_check")]
        accept_host_key: bool,
        /// Sign in with this private key; --password-stdin or --password-env then give its passphrase
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Read the password (or key passphrase) from stdin and save it in the OS credential store
        #[arg(long, conflicts_with = "password_env")]
        password_stdin: bool,
        /// Use CAPTUREFAB_SFTP_PASSWORD from the uploading process
        #[arg(long)]
        password_env: bool,
        /// Keep local copies after a verified upload
        #[arg(long)]
        keep_local: bool,
        /// Save without checking that the server is reachable
        #[arg(long)]
        no_check: bool,
    },
    /// Remove a saved destination
    Remove {
        name: String,
        /// Also delete its secret from the OS credential store, unless another destination uses it
        #[arg(long)]
        forget_secret: bool,
    },
    /// Check that a destination is reachable and writable
    Test {
        name: String,
        /// S3: check that the service refuses to replace objects, and switch to check-first uploads if it does not
        #[arg(long)]
        probe_writes: bool,
    },
}

/// Settings for self-hosted S3-compatible services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum S3Preset {
    Minio,
    Rustfs,
}

impl S3Preset {
    fn name(self) -> &'static str {
        match self {
            Self::Minio => "minio",
            Self::Rustfs => "rustfs",
        }
    }
}

/// A valid stand-in fingerprint, to check the other settings of an SFTP
/// destination whose host key is accepted on first use.
const PENDING_HOST_KEY: &str = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// The FTP or SFTP destination an add-ftp or add-sftp command describes,
/// with `host_key` for an SFTP server.
fn server_destination(
    command: &DestinationCommand,
    host_key: &str,
) -> Option<crate::destination::Destination> {
    use crate::destination::{Destination, FtpServer, FtpTls, SecretSource, SftpServer, Target};
    let secret = |stdin: bool, env: bool, none: bool| {
        if env {
            SecretSource::Environment
        } else if none || !stdin {
            SecretSource::NoSecret
        } else {
            SecretSource::Keychain
        }
    };
    Some(match command {
        DestinationCommand::AddFtp {
            name,
            host,
            port,
            user,
            dir,
            plain,
            password_stdin,
            password_env,
            anonymous,
            keep_local,
            ..
        } => Destination {
            name: name.clone(),
            target: Target::Ftp(FtpServer {
                host: host.clone(),
                port: *port,
                user: user.clone(),
                dir: dir.clone(),
                tls: if *plain {
                    FtpTls::Plain
                } else {
                    FtpTls::Explicit
                },
                // Without a password option, use the one saved earlier.
                password: if !*password_stdin && !*password_env && !*anonymous {
                    SecretSource::Keychain
                } else {
                    secret(*password_stdin, *password_env, *anonymous)
                },
                keep_local: *keep_local,
            }),
        },
        DestinationCommand::AddSftp {
            name,
            host,
            port,
            user,
            dir,
            key_file,
            password_stdin,
            password_env,
            keep_local,
            ..
        } => Destination {
            name: name.clone(),
            target: Target::Sftp(SftpServer {
                host: host.clone(),
                port: *port,
                user: user.clone(),
                dir: dir.clone(),
                host_key: host_key.into(),
                key_file: key_file.clone(),
                password: secret(*password_stdin, *password_env, false),
                keep_local: *keep_local,
            }),
        },
        _ => return None,
    })
}
#[derive(Debug, Subcommand)]
pub enum UploadsCommand {
    /// Show waiting, failed and completed uploads
    Status,
    /// Retry failed uploads (all, or one by ID)
    Retry { id: Option<String> },
    /// Drop a failed upload from the queue, keeping its local file
    Forget { id: String },
    /// Upload everything that is due, then report
    Wait {
        #[arg(long, default_value = "10m", value_parser = parse_duration_text)]
        timeout: String,
    },
}
#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    /// Show global policy, owned files and reservations
    Status,
    /// Persist the global storage ceiling for all cameras and future sessions
    Configure {
        #[command(flatten)]
        storage: StorageArgs,
    },
}
#[derive(Debug, Subcommand)]
pub enum WorkflowCommand {
    /// Validate every step and show the resolved plan without contacting cameras
    Validate { file: PathBuf },
    /// Apply ordered steps to a persistent session; stop on the first failure
    Apply {
        /// JSON workflow file; '-' reads stdin on every platform
        file: PathBuf,
        /// Validate and print the plan without making changes
        #[arg(long)]
        dry_run: bool,
    },
}
#[derive(Debug, Subcommand)]
pub enum CaptureSessionCommand {
    /// Create a new camera-bound DuckDB session; existing files are never overwritten
    New { path: PathBuf },
    /// Resume examples, feedback and pending reviews from an existing DuckDB session
    Resume { path: PathBuf },
    /// Close and checkpoint the active capture database
    Close,
    /// Save a positive training capture in the active database
    Teach,
    /// Enable automatic capture, or pause it while retaining the learned examples
    Auto {
        #[arg(long)]
        pause: bool,
        /// Suggestion sensitivity, from 0 (selective) to 1 (more suggestions)
        #[arg(long, default_value_t = 0.5, value_parser = parse_sensitivity)]
        sensitivity: f64,
        /// Minimum delay between suggestions, in milliseconds
        #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u64).range(250..=3_600_000))]
        cooldown_ms: u64,
    },
    /// Fetch the next pending review, or one pending capture by ID
    Review {
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: Option<u64>,
    },
    /// Keep a suggested capture and learn from the positive feedback
    Keep {
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
    },
    /// Discard a suggested capture and learn from the negative feedback
    Discard {
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
    },
}
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Validate and apply repeatable multi-camera setup, capture and recording plans
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    /// Learn capture preferences and review suggestions in a persistent DuckDB session
    CaptureSession {
        #[command(subcommand)]
        command: CaptureSessionCommand,
    },
    /// Open the desktop GUI and expose its local automation session
    Gui {
        #[arg(long, default_value = "gui")]
        name: String,
        /// Save a real rendered workbench screenshot, then exit (release tooling)
        #[arg(long)]
        screenshot: Option<PathBuf>,
        #[arg(long,default_value_t=1,value_parser=clap::value_parser!(u32).range(1..=16))]
        screenshot_cameras: u32,
        #[command(flatten)]
        dataset: DatasetArg,
    },
    /// Discover GigE Vision, USB3 Vision, ONVIF, host cameras and document scanners
    #[command(visible_aliases=["list","cameras"])]
    Discover,
    /// Host virtual GigE Vision, RTSP or ONVIF cameras using looping test data
    Simulate {
        #[command(subcommand)]
        command: crate::simulation::SimulationCommand,
    },
    /// Inspect GigE interfaces and reachability; tune connected cameras together
    NetworkScan,
    /// List document scanners using the platform scanner driver
    Scanners,
    /// Show camera identity and all readable features
    Info,
    /// List GenICam features, including access, values, bounds and units
    Features {
        #[arg(long)]
        filter: Option<String>,
    },
    /// Read one or more GenICam features
    Get {
        #[arg(required = true)]
        features: Vec<String>,
    },
    /// Set one or more FEATURE=VALUE assignments (in the given order)
    Set {
        #[arg(required = true)]
        assignments: Vec<String>,
    },
    /// Undo the latest reversible camera change in an existing session
    Undo,
    /// Redo the latest undone camera change in an existing session
    Redo,
    /// Show the next undo and redo labels for the selected or addressed camera
    History,
    /// Execute a GenICam command such as TriggerSoftware
    Execute { feature: String },
    /// Connect a camera in an existing persistent session
    Connect { camera: String },
    /// Set the session's default camera for CLI calls without --camera
    Select { camera: String },
    /// Observe all participants' actions, optionally waiting for a revision change
    Activity {
        /// Revision from a previous activity result; omit for immediate history
        #[arg(long)]
        after: Option<u64>,
        /// Wait up to this duration for changes (maximum 25s; default: no wait)
        #[arg(long, requires = "after", value_parser = parse_duration_text)]
        wait: Option<String>,
    },
    /// Disconnect the camera in a persistent session
    Disconnect,
    /// Start live acquisition in a persistent session
    Start,
    /// Stop live acquisition in a persistent session
    Stop,
    /// Let Capturefab tune exposure, gain and frame rate in a persistent session
    Auto {
        /// Trade-off from 0 (image quality) to 1 (frame rate), or quality, balanced, frame-rate; omitted keeps the current value, initially 0.5
        #[arg(long, value_parser = parse_balance)]
        balance: Option<f64>,
    },
    /// Stop auto mode and hold the current exposure, gain and white balance in a persistent session
    Manual {
        /// Restore every feature auto mode changed to its value from before auto mode
        #[arg(long)]
        revert: bool,
    },
    /// Record bounded video or forward frames to MediaMTX/NVR (RTSP, SRT, RTMP, UDP)
    #[command(visible_alias = "record")]
    Forward {
        #[arg(short = 'o', long)]
        output: String,
        #[arg(long, default_value = "h264")]
        codec: String,
        /// FFmpeg encoder name (e.g. libx264, h264_nvenc), or auto to probe available encoders
        #[arg(long, default_value = "auto")]
        encoder: String,
        #[arg(long, default_value_t = 30.0)]
        fps: f64,
        #[arg(long, default_value = "4M")]
        bitrate: String,
        /// Stop after this duration, e.g. 30s or 5m; omitted runs until Ctrl-C in direct mode
        #[arg(long, value_parser = parse_duration_text)]
        duration: Option<String>,
        /// Bounded maximum size of a local recording file
        #[arg(long, default_value = "512MiB", value_parser = parse_bytes_text)]
        max_file_size: String,
        /// Saved destination (see `destination list`); --output is then a name inside it
        #[arg(long)]
        destination: Option<String>,
        /// When the output file exists: fail (default) or number (-0001, -0002, ...)
        #[arg(long, value_enum, default_value_t = crate::naming::OnExists::Fail)]
        on_exists: crate::naming::OnExists,
        /// Tag the recording, e.g. --context line=L1 --context part=A-100 (repeatable)
        #[arg(long = "context", value_name = "KEY=VALUE", value_parser = crate::context::parse_pair)]
        context: Vec<(String, String)>,
        #[command(flatten)]
        storage: StorageArgs,
        #[command(flatten)]
        auto: AutoArgs,
        #[command(flatten)]
        dataset: DatasetArg,
    },
    /// Stop the encoder in a persistent session
    StopForward,
    /// Discover/resolve ONVIF camera profiles and RTSP stream URIs
    Onvif {
        #[command(subcommand)]
        command: OnvifCommand,
    },
    /// List host-driver cameras (AVFoundation, V4L2, DirectShow)
    Native,
    /// Inspect bundled FFmpeg, codecs and supported protocols
    Ffmpeg,
    /// Capture complete frames to files; existing files are never overwritten
    Capture {
        /// File, directory, or template with {camera} {date} {time} {datetime} {seq} {frame}; '-' writes one image to stdout
        #[arg(short = 'o', long)]
        output: String,
        #[arg(short='n',long,default_value_t=1,value_parser=clap::value_parser!(u32).range(1..=100_000))]
        count: u32,
        /// Image format; inferred from a single file/template extension, otherwise PNG
        #[arg(short = 'f', long, value_enum)]
        format: Option<ImageFormat>,
        /// Set FEATURE=VALUE before acquisition; may be repeated
        #[arg(long = "set")]
        settings: Vec<String>,
        /// Begin at a timestamp with timezone, e.g. 2026-10-05T09:00:00-04:00
        #[arg(long, conflicts_with = "delay", value_parser = parse_at_text)]
        at: Option<String>,
        /// Begin after a delay, e.g. 30s or 5m
        #[arg(long, value_parser = parse_duration_text)]
        delay: Option<String>,
        /// Time between frames, e.g. 10s; count bounds the time lapse
        #[arg(long, value_parser = parse_duration_text)]
        interval: Option<String>,
        /// Saved destination (see `destination list`); --output is then a name inside it
        #[arg(long)]
        destination: Option<String>,
        /// When the output file exists: fail (default) or number (-0001, -0002, ...). {seq} always picks a free number
        #[arg(long, value_enum, default_value_t = crate::naming::OnExists::Fail)]
        on_exists: crate::naming::OnExists,
        /// Tag the capture, e.g. --context line=L1 --context part=A-100 (repeatable)
        #[arg(long = "context", value_name = "KEY=VALUE", value_parser = crate::context::parse_pair)]
        context: Vec<(String, String)>,
        #[command(flatten)]
        storage: StorageArgs,
        #[command(flatten)]
        auto: AutoArgs,
        #[command(flatten)]
        dataset: DatasetArg,
    },
    /// List capture schedules in a persistent session
    Jobs,
    /// Cancel a scheduled capture job on the selected camera
    Cancel {
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        id: u64,
    },
    /// Manage saved capture destinations: folders, external drives, S3 buckets and FTP/SFTP servers
    #[command(visible_alias = "destinations")]
    Destination {
        #[command(subcommand)]
        command: DestinationCommand,
    },
    /// List mounted external, removable and network volumes
    Volumes,
    /// Show and manage the queue of captures waiting for upload
    Uploads {
        #[command(subcommand)]
        command: Option<UploadsCommand>,
    },
    /// Show owned file usage and reserved recording space
    Storage {
        #[command(subcommand)]
        command: Option<StorageCommand>,
    },
    /// Show connection routes, including mirrorless cameras via HDMI capture devices
    Compatibility { model: Option<String> },
    /// Show the state of an existing persistent session
    Status,
    /// Run a persistent headless camera session in the foreground
    Serve {
        #[arg(long, default_value = "bench")]
        name: String,
        #[arg(long)]
        stream: bool,
        /// Serve the WASM remote workbench on this address (e.g. 100.64.0.1:8787)
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Built browser assets; defaults to web-client/dist when --listen is used
        #[arg(long, requires = "listen")]
        web_dir: Option<PathBuf>,
        #[command(flatten)]
        auto: AutoArgs,
        #[command(flatten)]
        dataset: DatasetArg,
    },
    /// List local GUI and headless sessions
    Sessions,
    /// Send a SessionCommand JSON object from stdin to an existing session
    Rpc,
    /// Print the GenICam XML from a connected camera
    Xml {
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    /// Read or write raw camera memory for protocol debugging
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Catalog captures, recordings and signals in a DuckDB dataset and export analysis bundles
    Dataset {
        #[command(subcommand)]
        command: DatasetCommand,
    },
    /// Read, browse and log sensor and PLC signals (Modbus TCP, MQTT, OPC UA, MTConnect, sim-signal://)
    Signals {
        #[command(subcommand)]
        command: Option<SignalsCommand>,
    },
    /// Log signal sources for a duration into a dataset and/or CSV file, then exit
    Log {
        #[arg(required = true, num_args = 1..=16)]
        uris: Vec<String>,
        #[arg(long, default_value = "1s", value_parser = parse_duration_text)]
        interval: String,
        /// Stop after this duration, e.g. 10m; omitted runs until Ctrl-C
        #[arg(long, value_parser = parse_duration_text)]
        duration: Option<String>,
        /// Also write samples to this new CSV file
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Log only these tags (repeatable)
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[command(flatten)]
        dataset: DatasetArg,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Report interfaces, platform and compiled capabilities
    Doctor,
    /// Emit the versioned CLI and session automation contract as JSON
    Schema,
    /// Generate native shell completions (bash, zsh, fish, PowerShell, Elvish)
    Completions { shell: clap_complete::Shell },
}
#[derive(Debug, Subcommand)]
pub enum OnvifCommand {
    Discover,
    Resolve {
        endpoint: String,
        #[arg(long)]
        username: Option<String>,
        #[arg(long, env = "CAPTUREFAB_CAMERA_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },
}
#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    Read {
        #[arg(value_parser=parse_address)]
        address: u64,
        #[arg(default_value_t=4,value_parser=clap::value_parser!(u32).range(1..=65536))]
        length: u32,
    },
    Write {
        #[arg(value_parser=parse_address)]
        address: u64,
        /// Hex bytes, such as 00000001
        data: String,
    },
}
fn parse_address(s: &str) -> std::result::Result<u64, String> {
    if let Some(v) = s.strip_prefix("0x") {
        u64::from_str_radix(v, 16)
    } else {
        s.parse()
    }
    .map_err(|e| e.to_string())
}
fn parse_duration_text(s: &str) -> std::result::Result<String, String> {
    crate::scheduling::parse_duration(s)
        .map(|_| s.into())
        .map_err(|e| e.to_string())
}
fn parse_age_text(s: &str) -> std::result::Result<String, String> {
    let millis = crate::scheduling::parse_duration(s).map_err(|e| e.to_string())?;
    if !millis.is_multiple_of(1000) {
        return Err("maximum age must resolve to a whole number of seconds (minimum 1s)".into());
    }
    Ok(s.into())
}
fn parse_at_text(s: &str) -> std::result::Result<String, String> {
    crate::scheduling::parse_at(s)
        .map(|_| s.into())
        .map_err(|e| e.to_string())
}
fn parse_bytes_text(s: &str) -> std::result::Result<String, String> {
    crate::storage::parse_bytes(s)
        .map(|_| s.into())
        .map_err(|e| e.to_string())
}
fn parse_balance(s: &str) -> std::result::Result<f64, String> {
    match s {
        "quality" => Ok(0.0),
        "balanced" => Ok(0.5),
        "frame-rate" | "speed" => Ok(1.0),
        _ => s
            .parse()
            .ok()
            .filter(|b| (0.0..=1.0).contains(b))
            .ok_or_else(|| {
                "expected a number from 0 (quality) to 1 (frame rate), or quality, balanced, frame-rate".into()
            }),
    }
}
fn parse_sensitivity(s: &str) -> std::result::Result<f64, String> {
    s.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .ok_or_else(|| "expected a finite sensitivity from 0 to 1".into())
}
fn assignment(s: &str) -> Result<(String, String)> {
    let (k, v) = s.split_once('=').context("expected FEATURE=VALUE")?;
    ensure!(
        !k.is_empty(),
        "expected a nonempty feature name in FEATURE=VALUE"
    );
    SessionCommand::Set {
        feature: k.into(),
        value: v.into(),
    }
    .validate()?;
    Ok((k.into(), v.into()))
}
fn hex(s: &str) -> Result<Vec<u8>> {
    ensure!(
        !s.is_empty() && s.len().is_multiple_of(2) && s.is_ascii(),
        "hex data needs an even number of ASCII digits"
    );
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| Ok(u8::from_str_radix(std::str::from_utf8(p)?, 16)?))
        .collect()
}

#[cfg(feature = "s3")]
fn read_secret(reader: impl io::Read) -> Result<String> {
    let secret = crate::transfer::read_stdin_secret(reader, "S3 secret")?;
    crate::s3::validate_secret(&secret)?;
    Ok(secret)
}

struct Client<'a> {
    cli: &'a Cli,
    local: Option<SessionHandle>,
    target: Option<String>,
    actor: SessionActor,
}
impl<'a> Client<'a> {
    fn new(cli: &'a Cli) -> Result<Self> {
        let local = if cli.session.is_none() {
            Some(SessionHandle::new())
        } else {
            None
        };
        let mut c = Self {
            cli,
            local,
            target: cli.camera.clone(),
            actor: cli.session_actor()?,
        };
        if let Some(name) = &cli.session
            && camera_command(cli.command.as_ref())
        {
            let state = ipc::call(
                name,
                SessionCommand::Status,
                request_timeout(cli, &SessionCommand::Status)?,
            )?;
            if let Some(selector) = &cli.camera {
                let connected = addressed_camera(&state, selector);
                if let Some(connected) = connected {
                    c.target = connected["info"]["id"].as_str().map(str::to_owned);
                } else if one_shot_camera_command(cli.command.as_ref()) {
                    let result = ipc::call_to_as(
                        name,
                        None,
                        SessionCommand::Connect {
                            camera: selector.clone(),
                            timeout_ms: cli.timeout_ms,
                        },
                        request_timeout(cli, &SessionCommand::Status)?,
                        Some(&c.actor),
                    )?;
                    c.target = result["connected"]["id"].as_str().map(str::to_owned);
                }
            } else {
                c.target = state["active_camera"].as_str().map(str::to_owned);
            }
            ensure!(
                c.target.is_some(),
                "no camera connected; use --camera ID or connect ID"
            );
        }
        if cli.session.is_none()
            && let Some(selector) = &cli.camera
        {
            c.call(SessionCommand::Connect {
                camera: selector.clone(),
                timeout_ms: cli.timeout_ms,
            })?;
        }
        Ok(c)
    }
    fn call(&self, command: SessionCommand) -> Result<Value> {
        command.validate()?;
        if let Some(name) = &self.cli.session {
            let target = match &command {
                SessionCommand::Connect { .. }
                | SessionCommand::Select { .. }
                | SessionCommand::Discover { .. }
                | SessionCommand::NetworkScan { .. } => None,
                SessionCommand::Status
                    if self.cli.camera.is_none() && !camera_command(self.cli.command.as_ref()) =>
                {
                    None
                }
                SessionCommand::Activity { .. } => None,
                command if command.is_dataset() || command.is_signal() => None,
                _ => self.target.as_deref(),
            };
            ipc::call_to_as(
                name,
                target,
                command.clone(),
                request_timeout(self.cli, &command)?,
                Some(&self.actor),
            )
        } else {
            self.local.as_ref().unwrap().request_as(
                command,
                ActivityOrigin::Client,
                self.actor.clone(),
            )
        }
    }
    fn set(&self, feature: String, value: String) -> Result<Value> {
        let result = self.call(SessionCommand::Set { feature, value })?;
        if !self.cli.json && result.get("auto").is_some_and(Value::is_null) {
            eprintln!(
                "capturefab: switched to manual mode because {} is managed by auto mode",
                result["name"].as_str().unwrap_or_default()
            );
        }
        Ok(result)
    }
}

fn request_timeout(cli: &Cli, command: &SessionCommand) -> Result<Duration> {
    let millis = if let Some(timeout) = &cli.request_timeout {
        crate::scheduling::parse_duration(timeout)?
    } else {
        match command {
            SessionCommand::Capture {
                count, timeout_ms, ..
            } => timeout_ms
                .saturating_mul(u64::from(*count))
                .saturating_add(60_000),
            SessionCommand::CaptureExample { timeout_ms } => timeout_ms.saturating_add(60_000),
            SessionCommand::DatasetExport { .. } => 600_000,
            SessionCommand::DatasetQuery { timeout_ms, .. } => (if *timeout_ms == 0 {
                30_000
            } else {
                *timeout_ms
            })
            .saturating_add(10_000),
            SessionCommand::SignalRead { timeout_ms, .. } => timeout_ms.saturating_add(10_000),
            SessionCommand::SignalBrowse { duration_ms, .. } => duration_ms.saturating_add(15_000),
            _ => 60_000,
        }
    };
    Ok(Duration::from_millis(millis))
}
fn addressed_camera<'a>(state: &'a Value, selector: &str) -> Option<&'a Value> {
    let cameras = state["cameras"].as_array()?;
    cameras
        .iter()
        .find(|camera| camera["info"]["id"].as_str() == Some(selector))
        .or_else(|| {
            cameras.iter().find(|camera| {
                ["serial", "address"]
                    .iter()
                    .any(|key| camera["info"][*key].as_str() == Some(selector))
            })
        })
}

fn one_shot_camera_command(command: Option<&Command>) -> bool {
    matches!(
        command,
        Some(
            Command::Info
                | Command::Features { .. }
                | Command::Get { .. }
                | Command::Set { .. }
                | Command::Execute { .. }
                | Command::Capture { .. }
                | Command::Forward { .. }
                | Command::Xml { .. }
                | Command::Memory { .. }
        )
    )
}
fn camera_command(command: Option<&Command>) -> bool {
    one_shot_camera_command(command)
        || matches!(
            command,
            Some(
                Command::Disconnect
                    | Command::CaptureSession { .. }
                    | Command::Start
                    | Command::Stop
                    | Command::Auto { .. }
                    | Command::Manual { .. }
                    | Command::Undo
                    | Command::Redo
                    | Command::History
                    | Command::StopForward
                    | Command::Jobs
                    | Command::Cancel { .. }
            )
        )
}

fn uses_session(command: Option<&Command>) -> bool {
    camera_command(command)
        || matches!(
            command,
            Some(
                Command::Connect { .. }
                    | Command::Select { .. }
                    | Command::Status
                    | Command::Activity { .. }
                    | Command::Rpc
                    | Command::Discover
                    | Command::NetworkScan
                    | Command::Workflow {
                        command: WorkflowCommand::Apply { dry_run: false, .. }
                    }
                    | Command::Signals { .. }
                    | Command::Dataset {
                        command: DatasetCommand::Open { .. }
                            | DatasetCommand::Close
                            | DatasetCommand::Note { .. }
                            | DatasetCommand::Status { file: None }
                            | DatasetCommand::Query { file: None, .. }
                            | DatasetCommand::Export { file: None, .. }
                    }
            )
        )
}

/// Never guess among running workbenches. Discovery uses an authenticated IPC
/// probe so a stale descriptor or reused TCP port cannot select the wrong app.
fn resolve_session(cli: &mut Cli) -> Result<()> {
    if cli.session.is_some() || cli.direct || !uses_session(cli.command.as_ref()) {
        return Ok(());
    }
    // Binary stdout belongs to the temporary local frame ring, including when
    // an environment default selects a persistent session for other commands.
    if matches!(&cli.command, Some(Command::Capture { output, .. }) if output == "-") {
        return Ok(());
    }
    if let Some(session) = std::env::var_os("CAPTUREFAB_SESSION") {
        let session = input(
            session
                .into_string()
                .map_err(|_| anyhow::anyhow!("CAPTUREFAB_SESSION must be valid UTF-8")),
        )?;
        input(ipc::validate_name(&session))?;
        cli.session = Some(session);
        return Ok(());
    }
    let sessions = ipc::active_sessions()?;
    match sessions.as_slice() {
        [] => {}
        [session] => {
            if !cli.json {
                eprintln!(
                    "capturefab: using session '{}' ({:?}); use --direct for a temporary worker",
                    session.name, session.kind
                );
            }
            cli.session = Some(session.name.clone());
        }
        // A one-shot read or browse needs no workbench: with several
        // running, it runs here instead of refusing to choose.
        _ if matches!(
            &cli.command,
            Some(Command::Signals {
                command: Some(SignalsCommand::Read { .. } | SignalsCommand::Browse { .. }),
                ..
            })
        ) => {}
        _ => {
            return input(Err(anyhow::anyhow!(
                "multiple sessions are running ({}); choose --session NAME or --direct --camera ID; list them with capturefab sessions",
                sessions
                    .iter()
                    .map(|session| session.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct InvalidInput(String);
impl std::fmt::Display for InvalidInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for InvalidInput {}
fn input<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.chain().any(|cause| cause.is::<std::io::Error>()) {
            error
        } else {
            InvalidInput(format!("{error:#}")).into()
        }
    })
}

/// Prepare all command arguments before connecting or applying --set/--auto.
fn prepare(cli: &Cli) -> Result<Option<SessionCommand>> {
    cli.session_actor()?.validate()?;
    crate::media::validate_hwaccel(&cli.hwaccel)?;
    if let Some(camera) = &cli.camera {
        crate::session::validate_selector(camera)?;
        if cli.session.is_none() {
            SessionCommand::Connect {
                camera: camera.clone(),
                timeout_ms: cli.timeout_ms,
            }
            .validate()?;
        }
    }
    if let Some(session) = &cli.session {
        ipc::validate_name(session)?;
    }
    crate::session::validate_timeout(cli.timeout_ms)?;
    let command = match &cli.command {
        Some(Command::CaptureSession { command }) => Some(match command {
            CaptureSessionCommand::New { path } | CaptureSessionCommand::Resume { path } => {
                SessionCommand::CaptureSessionOpen {
                    path: path
                        .to_str()
                        .context("capture session path must be valid UTF-8")?
                        .into(),
                    create: matches!(command, CaptureSessionCommand::New { .. }),
                }
            }
            CaptureSessionCommand::Close => SessionCommand::CaptureSessionClose,
            CaptureSessionCommand::Teach => SessionCommand::CaptureExample {
                timeout_ms: cli.timeout_ms,
            },
            CaptureSessionCommand::Auto {
                pause,
                sensitivity,
                cooldown_ms,
            } => SessionCommand::AutoCapture {
                enabled: !pause,
                sensitivity: *sensitivity,
                cooldown_ms: *cooldown_ms,
            },
            CaptureSessionCommand::Review { id } => SessionCommand::CaptureReview { id: *id },
            CaptureSessionCommand::Keep { id } => SessionCommand::ReviewCapture {
                id: *id,
                keep: true,
            },
            CaptureSessionCommand::Discard { id } => SessionCommand::ReviewCapture {
                id: *id,
                keep: false,
            },
        }),
        Some(Command::Activity { after, wait }) => Some(SessionCommand::Activity {
            after: *after,
            timeout_ms: wait
                .as_deref()
                .map(crate::scheduling::parse_duration)
                .transpose()?
                .unwrap_or(0),
        }),
        Some(Command::Simulate { command }) => {
            ensure!(
                cli.session.is_none() && cli.camera.is_none(),
                "simulate creates its own virtual camera; use --source instead of --camera and omit --session"
            );
            if let crate::simulation::SimulationCommand::Run(options) = command {
                options.validate()?;
            }
            None
        }
        Some(Command::Destination { command }) => {
            let name = match command {
                DestinationCommand::List => None,
                DestinationCommand::AddFolder { name, path } => {
                    ensure!(
                        !path.as_os_str().is_empty(),
                        "destination folder path must be nonempty"
                    );
                    Some(name)
                }
                DestinationCommand::AddS3 {
                    name,
                    preset,
                    endpoint,
                    ..
                } => {
                    if let Some(preset) = preset {
                        ensure!(
                            !endpoint.is_empty(),
                            "--preset {} needs --endpoint http://HOST:9000",
                            preset.name()
                        );
                    }
                    Some(name)
                }
                DestinationCommand::AddFtp { name, .. }
                | DestinationCommand::AddSftp { name, .. } => {
                    let host_key = match command {
                        DestinationCommand::AddSftp {
                            host_key: Some(key),
                            ..
                        } => key.as_str(),
                        _ => PENDING_HOST_KEY,
                    };
                    server_destination(command, host_key)
                        .expect("server destination")
                        .validate()?;
                    Some(name)
                }
                DestinationCommand::Remove { name, .. } | DestinationCommand::Test { name, .. } => {
                    Some(name)
                }
            };
            if let Some(name) = name {
                crate::destination::validate_name(name)?;
            }
            None
        }
        Some(Command::Uploads {
            command: Some(UploadsCommand::Retry { id: Some(id) } | UploadsCommand::Forget { id }),
        }) => {
            #[cfg(feature = "uploads")]
            crate::upload::validate_id(id)?;
            #[cfg(not(feature = "uploads"))]
            let _ = id;
            None
        }
        Some(Command::Gui {
            name, screenshot, ..
        }) => {
            ipc::validate_name(name)?;
            if let Some(path) = screenshot {
                ensure!(
                    !path.as_os_str().is_empty(),
                    "screenshot output path must be nonempty"
                );
            }
            None
        }
        Some(Command::Serve {
            name,
            listen,
            web_dir,
            ..
        }) => {
            ipc::validate_name(name)?;
            if listen.is_some() {
                crate::remote::validate_web_root(
                    web_dir
                        .as_deref()
                        .unwrap_or(std::path::Path::new("web-client/dist")),
                )?;
            }
            None
        }
        Some(Command::Onvif {
            command:
                OnvifCommand::Resolve {
                    endpoint,
                    username,
                    password,
                    profile,
                },
        }) => {
            ensure!(
                username.is_some() == password.is_some(),
                "provide both --username and --password (or CAPTUREFAB_CAMERA_PASSWORD)"
            );
            if let Some(username) = username {
                ensure!(
                    !username.is_empty() && !username.chars().any(char::is_control),
                    "ONVIF username must be nonempty and contain no control characters"
                );
            }
            if let Some(profile) = profile {
                ensure!(
                    !profile.is_empty()
                        && profile.len() <= 1024
                        && !profile.chars().any(char::is_control),
                    "ONVIF profile must be nonempty and contain no control characters (maximum 1024 bytes)"
                );
            }
            crate::onvif::validate_source(&format!("onvif:{endpoint}"))?;
            None
        }
        Some(Command::Set { assignments }) => {
            for value in assignments {
                assignment(value)?;
            }
            None
        }
        Some(Command::Get { features }) => {
            for feature in features {
                SessionCommand::Get {
                    feature: feature.clone(),
                }
                .validate()?;
            }
            None
        }
        Some(Command::Execute { feature }) => Some(SessionCommand::Execute {
            feature: feature.clone(),
        }),
        Some(Command::Connect { camera }) => Some(SessionCommand::Connect {
            camera: camera.clone(),
            timeout_ms: cli.timeout_ms,
        }),
        Some(Command::Select { camera }) => Some(SessionCommand::Select {
            camera: camera.clone(),
        }),
        Some(Command::Capture {
            output,
            count,
            format,
            settings,
            at,
            delay,
            interval,
            destination,
            on_exists,
            context,
            ..
        }) => {
            let context = crate::context::from_pairs(context)?;
            ensure!(
                output != "-" || (cli.session.is_none() && !cli.json && *count == 1),
                "binary stdout needs one frame, a direct --camera, and no --json"
            );
            for setting in settings {
                assignment(setting)?;
            }
            let format = capture_format(*format, output, *count)?;
            let command = if at.is_some() || delay.is_some() || interval.is_some() {
                let first_at_ms = match at {
                    Some(at) => crate::scheduling::parse_at(at)?,
                    None => crate::scheduling::now_ms()
                        .checked_add(
                            delay
                                .as_deref()
                                .map(crate::scheduling::parse_duration)
                                .transpose()?
                                .unwrap_or(0),
                        )
                        .context("capture time overflows")?,
                };
                SessionCommand::Schedule {
                    output: output.clone(),
                    count: *count,
                    timeout_ms: cli.timeout_ms,
                    format: format.name().into(),
                    first_at_ms,
                    interval_ms: interval
                        .as_deref()
                        .map(crate::scheduling::parse_duration)
                        .transpose()?
                        .unwrap_or(0),
                    storage: crate::storage::StoragePolicy::default(),
                    destination: destination.clone(),
                    on_exists: *on_exists,
                    context: context.clone(),
                }
            } else {
                SessionCommand::Capture {
                    output: output.clone(),
                    count: *count,
                    timeout_ms: cli.timeout_ms,
                    format: format.name().into(),
                    storage: crate::storage::StoragePolicy::default(),
                    destination: destination.clone(),
                    on_exists: *on_exists,
                    context,
                }
            };
            Some(command)
        }
        Some(Command::Forward {
            output,
            codec,
            encoder,
            fps,
            bitrate,
            duration,
            max_file_size,
            destination,
            on_exists,
            context,
            ..
        }) => {
            let context = crate::context::from_pairs(context)?;
            if let Some(duration) = duration {
                crate::scheduling::parse_duration(duration)?;
            }
            Some(SessionCommand::Forward {
                output: output.clone(),
                codec: codec.clone(),
                encoder: encoder.clone(),
                fps: *fps,
                bitrate: bitrate.clone(),
                storage: crate::storage::StoragePolicy {
                    max_bytes: u64::MAX,
                    ..Default::default()
                },
                max_file_bytes: crate::storage::parse_bytes(max_file_size)?,
                destination: destination.clone(),
                on_exists: *on_exists,
                context,
            })
        }
        Some(Command::Dataset { .. }) => prepare_dataset(cli)?,
        Some(Command::Signals { .. } | Command::Log { .. }) => prepare_signals(cli)?,
        Some(Command::Memory { command }) => Some(match command {
            MemoryCommand::Read { address, length } => SessionCommand::ReadMemory {
                address: *address,
                length: *length as usize,
            },
            MemoryCommand::Write { address, data } => SessionCommand::WriteMemory {
                address: *address,
                data: hex(data)?,
            },
        }),
        Some(Command::Xml { output: Some(path) }) => {
            ensure!(
                !path.as_os_str().is_empty(),
                "XML output path must be nonempty"
            );
            None
        }
        _ => None,
    };
    if let Some(command) = &command {
        command.validate()?;
    }
    Ok(command)
}
fn prepare_outputs(cli: &Cli, command: &mut Option<SessionCommand>) -> Result<()> {
    if let Some(command) = command {
        input(caller_output(command))?;
    }
    if let (
        Some(command),
        Some(Command::Capture { storage, .. } | Command::Forward { storage, .. }),
    ) = (command, &cli.command)
    {
        match command {
            SessionCommand::Capture {
                storage: policy, ..
            }
            | SessionCommand::Schedule {
                storage: policy, ..
            }
            | SessionCommand::Forward {
                storage: policy, ..
            } => *policy = storage.policy()?,
            _ => unreachable!(),
        }
        input(command.validate())?;
        if let SessionCommand::Capture {
            output,
            count,
            format,
            destination,
            on_exists,
            ..
        }
        | SessionCommand::Schedule {
            output,
            count,
            format,
            destination,
            on_exists,
            ..
        } = command
            && output != "-"
        {
            let resolved = crate::destination::resolve(destination.as_deref(), output)?;
            crate::engine::preflight_capture_output(&resolved.output, *count, format, *on_exists)?;
        }
    }
    Ok(())
}
impl Drop for Client<'_> {
    fn drop(&mut self) {
        if let Some(h) = &self.local {
            h.shutdown();
        }
    }
}

pub fn run(mut cli: Cli) -> Result<()> {
    let base = std::env::current_dir()?;
    if let Some(camera) = &mut cli.camera {
        input(crate::workflow::resolve_selector(camera, &base))?;
    }
    if let Some(Command::Connect { camera } | Command::Select { camera }) = &mut cli.command {
        input(crate::workflow::resolve_selector(camera, &base))?;
    }
    let mut prepared = input(prepare(&cli))?;
    resolve_session(&mut cli)?;
    if cli.session.is_none()
        && let Some(camera) = &cli.camera
    {
        crate::camera::validate_selector(camera)?;
    }
    prepare_outputs(&cli, &mut prepared)?;
    input(crate::media::configure_hwaccel(&cli.hwaccel))?;
    match &cli.command {
        None | Some(Command::Gui { .. }) => {
            ensure!(
                cli.session.is_none(),
                "use an explicit command with --session, such as status"
            );
            ensure!(!cli.json, "choose a CLI command when using --json");
            let (name, screenshot, demo_cameras, dataset) = match &cli.command {
                Some(Command::Gui {
                    name,
                    screenshot,
                    screenshot_cameras,
                    dataset,
                }) => (
                    name.as_str(),
                    screenshot.clone(),
                    *screenshot_cameras,
                    Some(dataset),
                ),
                _ => ("gui", None, 1, None),
            };
            let client = Client::new(&cli)?;
            // --dataset wins over the GUI's remembered dataset.
            if let Some(dataset) = dataset {
                dataset.apply(&client)?;
            }
            let handle = client.local.as_ref().unwrap().clone();
            #[cfg(feature = "gui")]
            crate::gui::migrate_prefs();
            let _server =
                ipc::Server::start_with_kind(handle.clone(), name, ipc::SessionKind::Gui)?;
            #[cfg(feature = "uploads")]
            crate::upload::start();
            #[cfg(feature = "gui")]
            {
                crate::gui::run_capture(
                    handle,
                    name.into(),
                    cli.simulate,
                    screenshot,
                    demo_cameras,
                )?;
                return Ok(());
            }
            #[cfg(not(feature = "gui"))]
            {
                let _ = (handle, screenshot, demo_cameras);
                anyhow::bail!(
                    "GUI support is disabled; build with default features or use a CLI subcommand"
                )
            }
        }
        Some(Command::Completions { shell }) => {
            clap_complete::generate(*shell, &mut Cli::command(), "capturefab", &mut io::stdout());
            return Ok(());
        }
        Some(Command::Schema) => {
            return if cli.json {
                output(&cli, schema())
            } else {
                write_json(&schema())
            };
        }
        Some(Command::Workflow { command }) => {
            let (file, dry_run) = match command {
                WorkflowCommand::Validate { file } => (file, true),
                WorkflowCommand::Apply { file, dry_run } => (file, *dry_run),
            };
            let workflow = input(crate::workflow::load(file))?;
            input((|| {
                ensure!(
                    cli.camera.is_none(),
                    "workflow steps choose their own cameras; omit --camera"
                );
                Ok(())
            })())?;
            if dry_run {
                return output(&cli, workflow.plan());
            }
            let name = input(cli.session.as_deref().context(
                "workflow apply needs a persistent session; open capturefab gui or capturefab serve first, then use --session NAME"
            ))?;
            let result = workflow.apply(|camera, command| {
                if !cli.json {
                    eprintln!(
                        "capturefab: applying workflow step{}",
                        camera
                            .map(|c| format!(" on {}", crate::media::redact_url(c)))
                            .unwrap_or_default()
                    );
                }
                let timeout = request_timeout(&cli, &command)?;
                ipc::call_to_as(name, camera, command, timeout, Some(&cli.session_actor()?))
            })?;
            return output(&cli, result);
        }
        Some(Command::Simulate { command }) => match command {
            crate::simulation::SimulationCommand::Presets => {
                return output(&cli, crate::simulation::presets());
            }
            crate::simulation::SimulationCommand::Run(options) => {
                let server = options.start(Duration::from_millis(cli.timeout_ms))?;
                let stopped = Arc::new(AtomicBool::new(false));
                let flag = stopped.clone();
                ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
                    .context("install signal handler")?;
                let mut ready = server.endpoints();
                ready["ready"] = json!(true);
                ready["preset"] = json!(options.preset);
                ready["pid"] = json!(std::process::id());
                output(&cli, ready)?;
                let deadline = options
                    .duration
                    .as_deref()
                    .map(|duration| {
                        crate::scheduling::parse_duration(duration)
                            .map(|ms| std::time::Instant::now() + Duration::from_millis(ms))
                    })
                    .transpose()?;
                while !stopped.load(Ordering::Relaxed)
                    && deadline.is_none_or(|end| std::time::Instant::now() < end)
                {
                    server.check()?;
                    std::thread::sleep(Duration::from_millis(50));
                }
                return server.check();
            }
        },
        Some(Command::Doctor) => return output(&cli, doctor()),
        Some(Command::Storage { command }) => {
            let result = match command {
                Some(StorageCommand::Configure { storage }) => {
                    crate::storage::configure_policy(&storage.policy()?)?
                }
                _ => crate::storage::quota_status()?,
            };
            return output(&cli, result);
        }
        Some(Command::Destination { command }) => {
            return output(&cli, destination_command(command)?);
        }
        Some(Command::Volumes) => {
            return output(&cli, json!({"volumes": crate::volumes::list()}));
        }
        Some(Command::Uploads { command }) => {
            return output(&cli, uploads_command(command.as_ref())?);
        }
        Some(Command::Compatibility { model }) => {
            return output(&cli, crate::compatibility::guide(model.as_deref()));
        }
        Some(Command::Sessions) => return output(&cli, ipc::list()?),
        Some(Command::Native) => return output(&cli, crate::media::native_devices()?),
        Some(Command::Scanners) => {
            ensure!(
                cfg!(feature = "scanner"),
                "scanner support disabled in this build; rebuild with --features scanner"
            );
            return output(
                &cli,
                json!({"devices":crate::scanner::discover(Duration::from_millis(cli.timeout_ms))?}),
            );
        }
        Some(Command::Ffmpeg) => return output(&cli, crate::media::ffmpeg_info()?),
        Some(Command::Onvif { command }) => {
            let duration = Duration::from_millis(cli.timeout_ms);
            let result = match command {
                OnvifCommand::Discover => json!({"devices":crate::onvif::discover(duration)?}),
                OnvifCommand::Resolve {
                    endpoint,
                    username,
                    password,
                    profile,
                } => {
                    ensure!(
                        username.is_some() == password.is_some(),
                        "provide both --username and --password (or CAPTUREFAB_CAMERA_PASSWORD)"
                    );
                    let credentials =
                        username
                            .as_ref()
                            .zip(password.as_ref())
                            .map(|(username, password)| crate::onvif::Credentials {
                                username: username.clone(),
                                password: password.clone(),
                            });
                    json!({"streams":crate::onvif::resolve(endpoint,credentials.as_ref(),profile.as_deref(),duration)?})
                }
            };
            return output(&cli, result);
        }
        Some(Command::Log { .. }) => return log_command(&cli),
        Some(Command::Dataset {
            command:
                command @ (DatasetCommand::Status { file: Some(_) }
                | DatasetCommand::Query { file: Some(_), .. }
                | DatasetCommand::Export { file: Some(_), .. }),
        }) => {
            ensure!(
                cli.session.is_none(),
                "--file reads a dataset no session holds; omit --session"
            );
            let result = dataset_file(command, prepared.clone())?;
            return output(&cli, result);
        }
        Some(Command::Serve {
            name,
            stream,
            listen,
            web_dir,
            auto,
            dataset,
        }) => {
            ensure!(
                cli.session.is_none(),
                "serve creates a new session; omit --session"
            );
            let client = Client::new(&cli)?;
            let handle = client.local.as_ref().unwrap();
            if *stream {
                client.call(SessionCommand::Start)?;
            }
            dataset.apply(&client)?;
            auto.apply(&client)?;
            let _server =
                ipc::Server::start_with_kind(handle.clone(), name, ipc::SessionKind::Serve)?;
            let remote_server = listen
                .map(|address| {
                    crate::remote::Server::start(
                        handle.clone(),
                        address,
                        web_dir
                            .clone()
                            .unwrap_or_else(|| PathBuf::from("web-client/dist")),
                    )
                })
                .transpose()?;
            // A persistent session uploads what its cameras queue.
            #[cfg(feature = "uploads")]
            crate::upload::start();
            let mut ready = json!({"session":name,"pid":std::process::id(),"ready":true});
            if let Some(server) = &remote_server {
                ready["remote_url"] = json!(format!("http://{}/", server.local_addr()));
            }
            output(&cli, ready)?;
            let stopped = Arc::new(AtomicBool::new(false));
            let flag = stopped.clone();
            let h = handle.clone();
            ctrlc::set_handler(move || {
                flag.store(true, Ordering::Relaxed);
                h.shutdown();
            })
            .context("install signal handler")?;
            while !stopped.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            return Ok(());
        }
        _ => {}
    }
    let persistent = matches!(
        &cli.command,
        Some(
            Command::Connect { .. }
                | Command::CaptureSession { .. }
                | Command::Select { .. }
                | Command::Disconnect
                | Command::Start
                | Command::Stop
                | Command::Auto { .. }
                | Command::Manual { .. }
                | Command::Undo
                | Command::Redo
                | Command::History
                | Command::StopForward
                | Command::Status
                | Command::Activity { .. }
                | Command::Rpc
                | Command::Jobs
                | Command::Cancel { .. }
                | Command::Dataset {
                    command: DatasetCommand::Open { .. }
                        | DatasetCommand::Close
                        | DatasetCommand::Note { .. }
                }
                | Command::Signals {
                    command: None
                        | Some(SignalsCommand::Start { .. } | SignalsCommand::Stop { .. })
                }
        )
    );
    input((|| {
        ensure!(
            !persistent || cli.session.is_some(),
            "this command needs a running session; open `capturefab gui` or `capturefab serve --name NAME`, then choose --session NAME"
        );
        Ok(())
    })())?;
    let camera_required = matches!(
        &cli.command,
        Some(
            Command::Info
                | Command::Features { .. }
                | Command::Get { .. }
                | Command::Set { .. }
                | Command::Execute { .. }
                | Command::Capture { .. }
                | Command::Forward { .. }
                | Command::Xml { .. }
                | Command::Memory { .. }
        )
    );
    input((|| {
        ensure!(
            !camera_required || cli.camera.is_some() || cli.session.is_some(),
            "choose --camera ID or --session NAME (discover IDs with capturefab discover)"
        );
        Ok(())
    })())?;
    let client = Client::new(&cli)?;
    // Ctrl-C stops direct acquisition and releases the camera rather than leaving control held.
    if let Some(h) = &client.local {
        let h = h.clone();
        ctrlc::set_handler(move || h.shutdown()).context("install signal handler")?;
    }
    let result = match cli.command.as_ref().unwrap() {
        Command::Discover => client.call(SessionCommand::Discover {
            timeout_ms: cli.timeout_ms,
            simulated: cli.simulate,
        })?,
        Command::NetworkScan => client.call(SessionCommand::NetworkScan {
            timeout_ms: cli.timeout_ms,
        })?,
        Command::Info => {
            let state = client.call(SessionCommand::Status)?;
            let features = client.call(SessionCommand::Features)?;
            json!({"camera":state["connected"],"features":features["features"]})
        }
        Command::Features { filter } => {
            let mut v = client.call(SessionCommand::Features)?;
            if let Some(filter) = filter {
                let f = filter.to_ascii_lowercase();
                if let Some(features) = v["features"].as_array_mut() {
                    features.retain(|x| {
                        x["name"]
                            .as_str()
                            .unwrap_or("")
                            .to_ascii_lowercase()
                            .contains(&f)
                    });
                }
            }
            v
        }
        Command::Get { features } => {
            let mut values = Vec::new();
            for feature in features {
                values.push(client.call(SessionCommand::Get {
                    feature: feature.clone(),
                })?)
            }
            json!({"features":values})
        }
        Command::Set { assignments } => {
            // Validate every assignment's syntax before the first camera write. Camera constraints remain live.
            let parsed = assignments
                .iter()
                .map(|s| assignment(s))
                .collect::<Result<Vec<_>>>()?;
            let mut values = Vec::new();
            for (feature, value) in parsed {
                values.push(client.set(feature, value)?)
            }
            json!({"features":values})
        }
        Command::Execute { feature } => client.call(SessionCommand::Execute {
            feature: feature.clone(),
        })?,
        Command::Connect { camera } => client.call(SessionCommand::Connect {
            camera: camera.clone(),
            timeout_ms: cli.timeout_ms,
        })?,
        Command::Select { camera } => client.call(SessionCommand::Select {
            camera: camera.clone(),
        })?,
        Command::Forward {
            output: destination,
            duration,
            destination: saved,
            auto,
            dataset,
            ..
        } => {
            let until = duration
                .as_deref()
                .map(crate::scheduling::parse_duration)
                .transpose()?
                .map(Duration::from_millis);
            dataset.apply(&client)?;
            auto.apply(&client)?;
            let result = client.call(prepared.clone().unwrap())?;
            if cli.session.is_none() {
                if !cli.json {
                    eprintln!(
                        "Forwarding to {}. Press Ctrl-C to stop.",
                        crate::media::redact_url(destination)
                    );
                }
                let epoch = std::time::Instant::now();
                let h = client.local.as_ref().unwrap();
                while until.is_none_or(|d| epoch.elapsed() < d) {
                    std::thread::sleep(Duration::from_millis(100));
                    if h.is_shutting_down() {
                        return Ok(());
                    }
                    let state = h.snapshot();
                    if state.forwarding.is_none() {
                        anyhow::bail!(
                            "{}",
                            state
                                .last_error
                                .unwrap_or_else(|| "forwarding stopped".into())
                        );
                    }
                }
                let stopped = client.call(SessionCommand::StopForward)?;
                let mut result = result;
                result["stopped"] = stopped;
                wait_for_uploads(&cli, saved.as_deref(), &mut result)?;
                output(&cli, result)?;
                return Ok(());
            }
            if let Some(until) = until {
                let stopped = Arc::new(AtomicBool::new(false));
                let flag = stopped.clone();
                ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
                    .context("install signal handler")?;
                if !cli.json {
                    eprintln!(
                        "Recording/forwarding for {}. Ctrl-C stops this encoder.",
                        duration.as_deref().unwrap()
                    );
                }
                let epoch = std::time::Instant::now();
                while epoch.elapsed() < until && !stopped.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    let state = client.call(SessionCommand::Status)?;
                    if state["forwarding"].is_null() {
                        anyhow::bail!(
                            "{}",
                            state["last_error"]
                                .as_str()
                                .unwrap_or("forwarding stopped before its duration elapsed")
                        );
                    }
                }
                let finished = client.call(SessionCommand::StopForward)?;
                let mut result = result;
                result["stopped"] = finished;
                return output(&cli, result);
            }
            result
        }
        Command::Jobs => client.call(SessionCommand::Jobs)?,
        Command::Cancel { id } => client.call(SessionCommand::CancelJob { id: *id })?,
        Command::StopForward => client.call(SessionCommand::StopForward)?,
        Command::Disconnect => client.call(SessionCommand::Disconnect)?,
        Command::Start => client.call(SessionCommand::Start)?,
        Command::Stop => client.call(SessionCommand::Stop)?,
        Command::Auto { balance } => client.call(SessionCommand::Auto { balance: *balance })?,
        Command::Manual { revert } => client.call(SessionCommand::Manual { revert: *revert })?,
        Command::Undo => client.call(SessionCommand::Undo)?,
        Command::Redo => client.call(SessionCommand::Redo)?,
        Command::History => {
            let status = client.call(SessionCommand::Status)?;
            ensure!(
                status["connected"].is_object(),
                "no camera connected; use connect"
            );
            json!({"camera":status["connected"]["id"],"history":status["history"]})
        }
        Command::Status => client.call(SessionCommand::Status)?,
        Command::CaptureSession { .. } => client.call(prepared.clone().unwrap())?,
        Command::Activity { .. } => client.call(prepared.clone().unwrap())?,
        Command::Capture {
            output: destination,
            format,
            count,
            settings,
            destination: saved,
            auto,
            dataset,
            ..
        } => {
            dataset.apply(&client)?;
            let settings = settings
                .iter()
                .map(|s| assignment(s))
                .collect::<Result<Vec<_>>>()?;
            for (feature, value) in settings {
                client.set(feature, value)?;
            }
            auto.apply(&client)?;
            if matches!(prepared, Some(SessionCommand::Schedule { .. })) {
                let result = client.call(prepared.clone().unwrap())?;
                if cli.session.is_some() {
                    return output(&cli, result);
                }
                let id = result["job"]["id"]
                    .as_u64()
                    .context("schedule returned no job ID")?;
                if !cli.json {
                    eprintln!(
                        "Capture job {id} scheduled. Keep this process running; Ctrl-C cancels."
                    );
                }
                loop {
                    if client.local.as_ref().unwrap().is_shutting_down() {
                        return Ok(());
                    }
                    let jobs = client.call(SessionCommand::Jobs)?;
                    let job = jobs["jobs"]
                        .as_array()
                        .and_then(|j| j.iter().find(|j| j["id"] == id))
                        .context("capture job disappeared")?;
                    match job["status"].as_str() {
                        Some("complete") => {
                            let mut result = json!({"job":job});
                            wait_for_uploads(&cli, saved.as_deref(), &mut result)?;
                            return output(&cli, result);
                        }
                        Some("failed") => anyhow::bail!(
                            "{}",
                            job["error"].as_str().unwrap_or("scheduled capture failed")
                        ),
                        Some("cancelled") => anyhow::bail!("capture job cancelled"),
                        _ => {}
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let result = client.call(prepared.clone().unwrap())?;
            if destination == "-" {
                let frame = client
                    .local
                    .as_ref()
                    .unwrap()
                    .latest_frame()
                    .context("capture returned no frame")?;
                io::stdout().lock().write_all(&crate::frame::encode(
                    &frame,
                    capture_format(*format, destination, *count)?.name(),
                )?)?;
                return Ok(());
            }
            let mut result = result;
            if cli.session.is_none() {
                wait_for_uploads(&cli, saved.as_deref(), &mut result)?;
            }
            result
        }
        Command::Xml { output: path } => {
            let v = client.call(SessionCommand::Xml)?;
            let xml = v["xml"].as_str().context("missing XML")?;
            if let Some(path) = path {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                f.write_all(xml.as_bytes())?;
                json!({"output":path})
            } else if !cli.json {
                io::stdout().lock().write_all(xml.as_bytes())?;
                return Ok(());
            } else {
                v
            }
        }
        Command::Memory { command } => match command {
            MemoryCommand::Read { address, length } => client.call(SessionCommand::ReadMemory {
                address: *address,
                length: *length as usize,
            })?,
            MemoryCommand::Write { address, data } => client.call(SessionCommand::WriteMemory {
                address: *address,
                data: hex(data)?,
            })?,
        },
        Command::Dataset { command } => dataset_command(&cli, &client, command, prepared.clone())?,
        Command::Signals { command } => {
            signals_command(&cli, &client, command.as_ref(), prepared.clone())?
        }
        Command::Rpc => {
            use std::io::Read;
            let mut data = Vec::new();
            io::stdin().take(1024 * 1024 + 1).read_to_end(&mut data)?;
            if data.len() > 1024 * 1024 {
                return Err(InvalidInput("RPC stdin exceeds 1 MiB".into()).into());
            }
            let mut command: SessionCommand = input(
                SessionCommand::from_json_slice(&data)
                    .context("stdin must be a SessionCommand JSON object; see capturefab schema"),
            )?;
            input(command.validate())?;
            if matches!(&command, SessionCommand::Capture { output, .. } if output == "-") {
                return Err(InvalidInput("remote captures require an output file".into()).into());
            }
            if let SessionCommand::Connect { camera, .. } | SessionCommand::Select { camera } =
                &mut command
            {
                input(crate::workflow::resolve_selector(
                    camera,
                    &std::env::current_dir()?,
                ))?;
            }
            input(caller_output(&mut command))?;
            client.call(command)?
        }
        _ => unreachable!(),
    };
    output(&cli, result)
}

pub fn write_json(value: &Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}
fn output(cli: &Cli, value: Value) -> Result<()> {
    if cli.json {
        let mut envelope = json!({"version":ipc::VERSION,"ok":true,"result":value});
        if uses_session(cli.command.as_ref())
            && let Some(session) = &cli.session
        {
            envelope["session"] = json!(session);
        }
        return write_json(&envelope);
    }
    let mut out = io::stdout().lock();
    if matches!(&cli.command, Some(Command::Undo | Command::Redo)) {
        let (verb, key) = if matches!(&cli.command, Some(Command::Undo)) {
            ("Undid", "undone")
        } else {
            ("Redid", "redone")
        };
        writeln!(out, "{verb} {}", value[key].as_str().unwrap_or("change"))?;
    } else if matches!(&cli.command, Some(Command::History)) {
        writeln!(out, "Camera {}", value["camera"].as_str().unwrap_or(""))?;
        writeln!(
            out,
            "Undo: {}",
            value["history"]["undo"]
                .as_str()
                .unwrap_or("Nothing to undo")
        )?;
        writeln!(
            out,
            "Redo: {}",
            value["history"]["redo"]
                .as_str()
                .unwrap_or("Nothing to redo")
        )?;
    } else if let Some(Command::CaptureSession { command }) = &cli.command {
        match command {
            CaptureSessionCommand::Close => writeln!(out, "Capture session closed.")?,
            CaptureSessionCommand::Review { .. } => {
                if let Some(id) = value["capture"]["id"].as_u64() {
                    writeln!(
                        out,
                        "Capture #{id}: {} (score {:.3})",
                        value["capture"]["reason"].as_str().unwrap_or("Suggestion"),
                        value["capture"]["score"].as_f64().unwrap_or(0.0)
                    )?;
                    writeln!(
                        out,
                        "Use capture-session keep {id} or discard {id}; --json includes the RGBA preview."
                    )?;
                } else {
                    writeln!(out, "No captures pending review.")?;
                }
            }
            CaptureSessionCommand::Keep { id } => {
                writeln!(out, "Kept capture #{id}; session model updated.")?
            }
            CaptureSessionCommand::Discard { id } => {
                writeln!(out, "Discarded capture #{id}; session model updated.")?
            }
            CaptureSessionCommand::Teach => {
                writeln!(out, "Saved training capture #{}.", value["id"])?
            }
            _ => {
                let session = &value["capture_session"];
                writeln!(
                    out,
                    "{}: {} · {} manual · {} kept · {} discarded · {} pending",
                    session["path"].as_str().unwrap_or("Capture session"),
                    session["state"].as_str().unwrap_or("ready"),
                    session["manual_count"],
                    session["kept_count"],
                    session["discarded_count"],
                    session["pending_count"]
                )?;
            }
        }
    } else if let Some(Command::Dataset { command }) = &cli.command {
        dataset_output(&mut out, command, &value)?;
    } else if matches!(
        &cli.command,
        Some(Command::Signals { .. } | Command::Log { .. })
    ) {
        out.write_all(crate::signals::report::human(&value).as_bytes())?;
    } else if matches!(&cli.command, Some(Command::Activity { .. })) {
        writeln!(out, "Session activity · revision {}", value["revision"])?;
        for entry in value["activity"].as_array().into_iter().flatten() {
            let actor = entry["actor"]["name"].as_str().unwrap_or("Participant");
            let state = entry["job"]["status"]
                .as_str()
                .or_else(|| entry["state"].as_str())
                .unwrap_or("updated");
            writeln!(
                out,
                "#{} · {} · {} · {} · {}",
                entry["id"],
                actor,
                entry["camera"].as_str().unwrap_or("session"),
                state,
                entry["label"].as_str().unwrap_or("")
            )?;
            if let Some(detail) = entry["detail"].as_str() {
                writeln!(out, "  {detail}")?;
            }
        }
    } else if matches!(&cli.command, Some(Command::Status)) {
        writeln!(
            out,
            "Session {}",
            cli.session.as_deref().unwrap_or("temporary")
        )?;
        let cameras = value["cameras"].as_array();
        if let Some(cameras) = cameras {
            writeln!(
                out,
                "{:<28} {:<10} {:>8} {:>10} OUTPUT",
                "CAMERA", "STATE", "FPS", "FRAMES"
            )?;
            for camera in cameras {
                if let Some(target) = value["target_camera"].as_str()
                    && camera["info"]["id"].as_str() != Some(target)
                {
                    continue;
                }
                writeln!(
                    out,
                    "{:<28} {:<10} {:>8.1} {:>10} {}{}",
                    camera["info"]["id"].as_str().unwrap_or(""),
                    if camera["streaming"] == true {
                        "streaming"
                    } else {
                        "idle"
                    },
                    camera["fps"].as_f64().unwrap_or(0.0),
                    camera["frames"].as_u64().unwrap_or(0),
                    camera["forwarding"].as_str().unwrap_or(""),
                    if camera["info"]["id"] == value["active_camera"] {
                        " (session default)"
                    } else {
                        ""
                    }
                )?;
                if let Some(error) = camera["last_error"].as_str() {
                    writeln!(out, "  Error: {error}")?;
                }
            }
            if cameras.is_empty() {
                writeln!(
                    out,
                    "No cameras connected. Use connect ID or --camera ID capture -o frame.png."
                )?;
            }
        } else if let Some(camera) = value["connected"]["id"].as_str() {
            writeln!(
                out,
                "Camera {camera}: {}, {:.1} fps, {} frames",
                if value["streaming"] == true {
                    "streaming"
                } else {
                    "idle"
                },
                value["fps"].as_f64().unwrap_or(0.0),
                value["frames"].as_u64().unwrap_or(0)
            )?;
        }
        writeln!(
            out,
            "Use status --json for full state, jobs for captures, and history for undo/redo."
        )?;
    } else if let Some(sessions) = value["sessions"].as_array() {
        if sessions.is_empty() {
            writeln!(
                out,
                "No sessions running. Open capturefab gui or capturefab serve --name NAME."
            )?;
        } else {
            writeln!(out, "{:<28} {:<8} {:<8} STATE", "SESSION", "KIND", "PID")?;
            for session in sessions {
                writeln!(
                    out,
                    "{:<28} {:<8} {:<8} {}",
                    session["name"].as_str().unwrap_or(""),
                    session["kind"].as_str().unwrap_or(""),
                    session["pid"].as_u64().unwrap_or(0),
                    if session["active"] == true {
                        "active"
                    } else {
                        "stale"
                    }
                )?;
            }
        }
    } else if matches!(&cli.command, Some(Command::Workflow { .. })) {
        if value["completed_steps"].is_number() {
            writeln!(
                out,
                "Applied {} workflow steps in session {}.",
                value["completed_steps"],
                cli.session.as_deref().unwrap_or("")
            )?;
            for step in value["completed"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "  {}. {}{}",
                    step["step"],
                    step["command"]["op"].as_str().unwrap_or(""),
                    step["camera"]
                        .as_str()
                        .map(|camera| format!(" on {camera}"))
                        .unwrap_or_default()
                )?;
            }
        } else {
            writeln!(
                out,
                "Validated {} steps. Resolved plan:",
                value["step_count"]
            )?;
            writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
        }
    } else if let Some(files) = value["files"].as_array() {
        writeln!(out, "Saved {} frame(s):", files.len())?;
        for file in files {
            writeln!(out, "  {}", file.as_str().unwrap_or(""))?;
        }
    } else if let Some(devices) = value["devices"].as_array() {
        if devices.is_empty() {
            writeln!(
                out,
                "No cameras found. Try --simulate or a direct --camera IPv4 address."
            )?;
        } else {
            writeln!(
                out,
                "{:<28} {:<10} {:<24} SERIAL",
                "ID", "TRANSPORT", "MODEL"
            )?;
            for d in devices {
                writeln!(
                    out,
                    "{:<28} {:<10} {:<24} {}",
                    d["id"].as_str().unwrap_or(""),
                    d["transport"].as_str().unwrap_or(""),
                    d["model"]
                        .as_str()
                        .or_else(|| d["name"].as_str())
                        .unwrap_or(""),
                    d["serial"].as_str().unwrap_or("")
                )?;
            }
        }
        if let Some(w) = value["warnings"].as_array() {
            for w in w {
                eprintln!("capturefab: {}", w.as_str().unwrap_or(""));
            }
        }
    } else if let Some(features) = value["features"].as_array() {
        if let Some(camera) = value.get("camera") {
            writeln!(out, "{}", serde_json::to_string_pretty(camera)?)?;
        }
        for f in features {
            writeln!(
                out,
                "{:<34} {}{}",
                f["name"].as_str().unwrap_or(""),
                f.get("value")
                    .map(|value| value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string()))
                    .unwrap_or_default(),
                f["error"]
                    .as_str()
                    .map(|s| format!("  ({s})"))
                    .unwrap_or_default()
            )?;
        }
    } else {
        writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
    }
    Ok(())
}
fn doctor() -> Value {
    let interfaces=if_addrs::get_if_addrs().map(|items|items.into_iter().filter_map(|i|match i.addr{if_addrs::IfAddr::V4(a)=>Some(json!({"name":i.name,"ip":a.ip,"netmask":a.netmask,"broadcast":a.broadcast})),_=>None}).collect::<Vec<_>>()).unwrap_or_default();
    json!({"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"features":{"gui":cfg!(feature="gui"),"usb":cfg!(feature="usb"),"scanner":cfg!(feature="scanner"),"wgpu":cfg!(feature="gui"),"jpeg":cfg!(feature="jpeg"),"nvjpeg":cfg!(feature="nvjpeg"),"vaapi":cfg!(feature="vaapi"),"videotoolbox":cfg!(feature="videotoolbox"),"s3":cfg!(feature="s3"),"ftp":cfg!(feature="ftp"),"sftp":cfg!(feature="sftp"),"parquet":cfg!(feature="parquet"),"modbus":cfg!(feature="modbus"),"mqtt":cfg!(feature="mqtt"),"opcua":cfg!(feature="opcua"),"mtconnect":cfg!(feature="mtconnect")},"signals":crate::signals::compiled_protocols(),"scanners":crate::scanner::doctor(),"jpeg":jpeg_backends(),"session_directory":ipc::session_dir(),"interfaces":interfaces,"runtime":"No Aravis, libusb or vendor SDK required. Native OS graphics/USB drivers required; NVIDIA CUDA and nvJPEG, and libva with a JPEG-capable driver, are used when installed; Apple silicon uses its JPEG engine through VideoToolbox. Document scanning uses installed platform scanner drivers (SANE scanimage on Linux).","usb_access":if cfg!(target_os="windows"){"USB3 camera interfaces must use WinUSB"}else if cfg!(target_os="linux"){"Read/write permission on camera /dev/bus/usb node required"}else{"IOKit camera interface must be available to userspace"}})
}
/// JPEG encoders available to this build and host, best first.
fn jpeg_backends() -> Value {
    #[cfg(feature = "nvjpeg")]
    let nvjpeg = match crate::nvjpeg::probe() {
        Ok(status) => json!({"available":true,"status":status}),
        Err(error) => json!({"available":false,"reason":format!("{error:#}")}),
    };
    #[cfg(not(feature = "nvjpeg"))]
    let nvjpeg = json!({"available":false,"reason":"not compiled in"});
    #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
    let videotoolbox = match crate::vtjpeg::probe() {
        Ok(status) => json!({"available":true,"status":status,"formats":"color"}),
        Err(error) => json!({"available":false,"reason":format!("{error:#}")}),
    };
    #[cfg(not(all(feature = "videotoolbox", target_os = "macos")))]
    let videotoolbox = json!({"available":false,"reason":if cfg!(target_os="macos"){"not compiled in"}else{"macOS only"}});
    #[cfg(feature = "vaapi")]
    let vaapi = match crate::vajpeg::probe() {
        Ok(status) => json!({"available":true,"status":status}),
        Err(error) => json!({"available":false,"reason":format!("{error:#}")}),
    };
    #[cfg(not(feature = "vaapi"))]
    let vaapi = json!({"available":false,"reason":"not compiled in"});
    json!({"videotoolbox":videotoolbox,"nvjpeg":nvjpeg,"vaapi":vaapi,"libjpeg_turbo":cfg!(feature="jpeg"),"quality":crate::jpeg::QUALITY})
}
fn command_schema(command: &clap::Command) -> Value {
    json!({
        "name":command.get_name(),
        "description":command.get_about().map(ToString::to_string),
        "aliases":command.get_visible_aliases().collect::<Vec<_>>(),
        "arguments":command.get_arguments().filter(|arg| !arg.is_hide_set()).map(|arg| json!({
            "name":arg.get_id().as_str(),
            "long":arg.get_long(),
            "short":arg.get_short().map(|short| short.to_string()),
            "description":arg.get_help().map(ToString::to_string),
            "required":arg.is_required_set(),
            "global":arg.is_global_set(),
            "action":format!("{:?}", arg.get_action()),
            "default":arg.get_default_values().iter().map(|v| v.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "choices":arg.get_value_parser().possible_values().map(|values| values.filter(|v| !v.is_hide_set()).map(|v| v.get_name().to_owned()).collect::<Vec<_>>())
        })).collect::<Vec<_>>(),
        "commands":command.get_subcommands().filter(|command| !command.is_hide_set()).map(command_schema).collect::<Vec<_>>()
    })
}
fn schema() -> Value {
    let mut contract = json!({"version":ipc::VERSION,"program":"capturefab","default":"gui","json_result":{"version":1,"ok":true,"result":"command-specific JSON"},"json_error":{"version":1,"ok":false,"error":{"code":"category","message":"description"}},"exit_codes":{"0":"success (also closed stdout pipe)","1":"operation or I/O failure","2":"invalid CLI usage","3":"camera/session unavailable","4":"timeout","5":"unsupported feature/format"},"selection":{"direct":"--camera ID|serial|IPv4|sim:0","existing_session":"--session NAME","environment":"CAPTUREFAB_SESSION, CAPTUREFAB_SESSION_DIR"},"limits":{"timeout_ms":[1,60000],"count":[1,100000],"rpc_bytes":1048576,"payload_bytes":268435456},"rpc":{"transport":"authenticated loopback TCP; one newline-delimited request/response per connection","client":"capturefab --session NAME rpc < command.json","command_tag":"op","commands":[{"op":"status"},{"op":"discover","timeout_ms":1000,"simulated":false},{"op":"connect","camera":"sim:0","timeout_ms":2000},{"op":"disconnect"},{"op":"features"},{"op":"get","feature":"Width"},{"op":"set","feature":"ExposureTime","value":"5000"},{"op":"execute","feature":"TriggerSoftware"},{"op":"start"},{"op":"stop"},{"op":"capture","output":"frames","count":10,"timeout_ms":2000,"format":"png"},{"op":"capture","output":"run-1","count":10,"timeout_ms":2000,"format":"png","destination":"archive"},{"op":"xml"},{"op":"read_memory","address":256,"length":4},{"op":"write_memory","address":256,"data":[0,0,2,128]}]},"capture":{"formats":["png","jpeg","raw","pgm","ppm"],"stdout":"--camera ... capture -n 1 -o - --format raw; no --json","destination":"--destination NAME (or \"destination\" in capture/schedule/forward RPC): -o is a relative name inside a saved folder, S3 bucket, FTP(S) or SFTP server; see destination list, volumes and uploads"},"writes":"Multiple set assignments apply sequentially; failures may leave earlier assignments applied. Session operations are serialized with GUI operations."});
    contract["exit_codes"]["6"] = json!("storage quota or disk full");
    let mut command = Cli::command();
    command.build();
    contract["cli"] = command_schema(&command);
    contract["selection"]["automatic"] = json!(
        "Camera/session commands attach to the only authenticated active session; zero sessions falls back to direct operation where supported; multiple sessions require --session NAME or --direct. Binary stdout captures always use a temporary worker."
    );
    contract["selection"]["environment"] = json!([
        "CAPTUREFAB_CAMERA",
        "CAPTUREFAB_SESSION",
        "CAPTUREFAB_SESSION_DIR",
        "CAPTUREFAB_HWACCEL",
        "CAPTUREFAB_ACTOR",
        "CAPTUREFAB_ACTOR_KIND"
    ]);
    contract["selection"]["direct_mode"] = json!(
        "--direct --camera ID bypasses automatic session attachment; --direct conflicts with explicit --session and overrides CAPTUREFAB_SESSION"
    );
    contract["capture"]["format_default"] = json!(
        "Known single file/template extension infers png/jpeg/raw/pgm/ppm; sequences to directories and stdout default to png; --format overrides inference"
    );
    contract["capture"]["path_base"] = json!(
        "CLI capture/record/RPC local output paths resolve in the calling terminal; workflow paths resolve beside their file; named destination paths remain relative to the destination"
    );
    contract["capture"]["paths"] = json!(
        "file, directory (continues frame-NNNNNN numbering after the highest existing number) or template; {seq} picks the next free number; other existing names fail unless on_exists=number. Files are never overwritten."
    );
    contract["capture"]["placeholders"] = json!({
        "camera":"model-serial, limited to A-Z a-z 0-9 . _ - (at most 64 bytes)",
        "date":"YYYY-MM-DD","time":"HHMMSS","datetime":"YYYYMMDD-HHMMSS",
        "seq":"next free number in that folder for that pattern, 4 or more digits; file name only",
        "frame":"1-based index within the request, 6 digits",
        "clock":"local time when the frame is requested; wall_ms is UTC milliseconds"
    });
    contract["capture"]["on_exists"] = json!({
        "values":["fail","number"],"default":"fail",
        "number":"an existing name without {seq} becomes stem-0001.ext, stem-0002.ext, ...",
        "applies_to":"capture, schedule and local recordings (record/forward)"
    });
    contract["capture"]["context"] = json!({
        "cli":"--context KEY=VALUE (repeatable)","keys":"^[a-z][a-z0-9_]{0,31}$",
        "max_entries":crate::context::MAX_ENTRIES,"max_value_bytes":crate::context::MAX_VALUE,
        "duplicate_key":"usage error (exit 2)",
        "recorded":"echoed in the capture result, kept on scheduled jobs and stored with each file in an open dataset"
    });
    contract["capture"]["result"] = json!({
        "capture":"{files:[path],frames:[{id,width,height,pixel_format,timestamp_ns,bytes,wall_ms,file?}],count,context?,destination?,queued_uploads?,queued?:[{file,upload_id,key,url}]}",
        "record":"{forwarding,since_ms,path?}; path is the local recording file",
        "stop_forward":"{forwarding:null,stats:{accepted,written,dropped,late,stalled,encoder,error,recording?}|null}; record --duration adds it as stopped. late counts frames lost while the encoder was still busy (dropped includes them); stalled means no new camera frame for 2 s",
        "status":"forwarding_since_ms per camera and at the top level while forwarding"
    });
    contract["capture_session"] = json!({
        "cli":"--session NAME [--camera ID] capture-session new PATH.duckdb | resume PATH.duckdb | close | teach | auto [--pause] [--sensitivity 0.5] [--cooldown-ms 2000] | review [--id ID] | keep ID | discard ID",
        "scope":"Camera-bound persistent controls; connect the camera in a GUI/serve session first. Temporary --direct workers are rejected.",
        "storage":"One .duckdb file stores raw saved frames, descriptors, capture decisions, settings and background prototypes. New refuses existing paths; resume requires an existing compatible session for this camera. Close checkpoints the database. Discard removes raw pixels while retaining negative descriptors and the decision.",
        "paths":"CLI and RPC database paths resolve in the calling terminal; workflow database paths resolve beside the workflow file.",
        "learning":"Manual training captures and kept suggestions are positive examples; discarded suggestions are negative examples. Pending suggestions do not train the model. The built-in model is a native visual-descriptor nearest-example baseline with background novelty; no pretrained neural weights are downloaded.",
        "controls":"Auto capture is independent of auto exposure. auto enables acquisition; --pause pauses suggestions and keeps examples. Sensitivity and cooldown default to 0.5 and 2000ms on each auto command. Resume restores saved settings; hardware must be reconnected.",
        "status":"status.capture_session and cameras[].capture_session; null when no capture database is open",
        "review":"Returns {capture:null|{id,score,reason,width,height,rgba},capture_session:...}; rgba is a base64-encoded 480x360-or-smaller RGBA preview. Review IDs address pending captures; stale or repeated decisions fail.",
        "limits":{"sensitivity":[0.0,1.0],"cooldown_ms":[250,3600000],"pending":200,"stored_pixel_bytes":2147483648_u64},
        "research":"docs/auto-capture-models.md"
    });
    contract["selection"]["source_base"] = json!(
        "Relative file: and sim:replay: inputs, and existing bare media paths, resolve in the calling terminal for CLI/RPC and beside the file for workflows; network/device locators keep their meaning"
    );
    contract["selection"]["addressed_status"] = json!(
        "status --camera retains session context and actual active_camera selection; target_camera identifies the camera whose top-level connected/features/streaming/forwarding/jobs/history fields are returned"
    );
    contract["workflow"] = json!({
        "cli":"workflow validate FILE | workflow apply FILE [--dry-run]; '-' reads stdin",
        "document":{"version":1,"steps":[{"command":{"op":"connect","camera":"sim:0","timeout_ms":2000}},{"camera":"sim:0","command":{"op":"set","feature":"ExposureTime","value":"5000"}},{"camera":"sim:0","command":{"op":"capture","output":"shot.png","count":1,"timeout_ms":2000,"format":"png"}}]},
        "limits":{"bytes":1048576,"steps":256},
        "validation":"Strict fields; validate every step before execution; stdout captures rejected; camera-specific constraints remain live",
        "execution":"Apply requires a persistent session. Steps execute in order and stop on first failure; earlier effects remain applied. JSON error.details records completed steps and the failing step. Streams/jobs continue in the session after the CLI exits.",
        "paths":"Relative outputs resolve beside the workflow file; stdin uses caller directory; destination-relative names and stream URLs retain their meaning"
    });
    contract["activity"] = json!({
        "snapshot":"status.activity; status.activity_revision",
        "actor":{"fields":{"id":"stable participant identifier","name":"display name","kind":"human or agent"},"cli":"--actor NAME --actor-kind human|agent; CAPTUREFAB_ACTOR and CAPTUREFAB_ACTOR_KIND", "identity":"Self-reported attribution; not authentication. Unnamed CLI calls use a process identity."},
        "origins":["client","gui"],"states":["queued","running","succeeded","failed"],
        "job":"Schedule acknowledgment is succeeded; entry.job tracks id, worker_pid, status, count, captured, last_file and error through completion/cancellation/failure.",
        "display":"Desktop and browser show actor, target, command lifecycle, progress, errors and saved outputs across cameras. Completed commands remain visible after selection/disconnect. Each human view keeps its own selected camera.",
        "updates":"GET /api/activity returns {revision,activity}; ?after=REV&timeout_ms=20000 waits for any lifecycle or job change, returning current history. Reconnect with the last revision, or omit after for the initial history. Maximum wait 25000ms; history retains 200 completed entries plus active work.",
        "local_updates":"activity --json returns {revision,activity}; activity --after REV --wait 20s --json waits for shared changes through private local IPC, without an HTTP listener. RPC command: {op:activity,after:REV,timeout_ms:20000}.",
        "polling":"Status, activity, get, jobs, features, xml, read_memory and authenticated discovery probes do not create activity entries; targeted status includes all participants without waiting behind captures.",
        "waiting_observers":{"private_ipc":8,"http":32,"capacity":"Waiting observers reserve capacity for commands; extra waits fail explicitly and can be retried."}
    });
    contract["writes"] = json!(
        "Camera operations are ordered per camera across GUI, browser and IPC participants; different cameras run independently. Concurrent scheduled jobs on the same camera share acquisition. Multiple set assignments apply sequentially; failures may leave earlier assignments applied."
    );
    contract["logs"] = json!({
        "snapshot":"status.logs; GET /api/session result.logs",
        "legacy_fields":["time","level","message"],
        "time":"Retained producer clock text without a date/zone. Do not parse it as an absolute timestamp.",
        "timestamp":{"optional":true,"unix_ms":"signed Unix milliseconds (absolute UTC instant)","utc_offset_seconds":"producer offset used for the legacy time string, seconds east of UTC (-86399..86399)"},
        "compatibility":"Old entries omit timestamp. Unknown or malformed optional timestamp metadata is treated as unavailable; legacy diagnostics remain readable. Desktop/browser format valid instants in the viewer's local zone; copied desktop logs include a dated numeric offset. The original three fields and version-1 envelope remain unchanged."
    });
    contract["forwarding"] = json!({"cli":"record|forward -o OUTPUT [--duration 30s] [--on-exists fail|number] [--context KEY=VALUE]...; a local file output accepts the capture placeholders","duration":"Works with direct workers and persistent sessions. A duration keeps the CLI alive and stops the encoder at the deadline or Ctrl-C. Without duration, persistent session forwarding returns immediately; direct forwarding runs until Ctrl-C.","request_timeout":"--request-timeout controls a session request deadline, not recording duration; a timed-out request may still complete in the session. Inspect status before retrying writes."});
    contract["forwarding"]["timeline"] = json!(
        "Each new frame goes to FFmpeg once, stamped when FFmpeg reads it; the output repeats or drops frames to keep --fps constant, so a recording lasts as long as it ran"
    );
    contract["forwarding"]["encoders"] = json!(
        "auto probes encoders at the frame size and remembers results for the FFmpeg build until reboot; an auto encoder that exits in its first seconds is replaced once by the next in a new file; a codec with no working encoder fails before recording with unsupported (exit 5) and names the working codecs"
    );
    contract["forwarding"]["sources"] = json!(
        "Network streams reconnect after FFmpeg exits, backing off from 1 s to 30 s; frame waits meanwhile time out (exit 4), and five refusals in a row (rejected credentials, missing path) stop acquisition. Video files play at their frame rate unless PlaybackSpeed=Fastest and stop with 'Video file finished'."
    );
    contract["remote"] = json!({
        "start":"serve --listen HOST:PORT [--web-dir web-client/dist]",
        "authentication":"Network access controls: anyone who can reach the listener can control the session. No HTTP login or token.",
        "browser":"Rust WASM client built with web-client/build.sh; same-origin HTTP API",
        "routes":{"GET /api/session":"SessionSnapshot", "GET /api/activity?after=REV&timeout_ms=20000":"{revision,activity}; waits for shared actions and job progress", "POST /api/command":"{version:1,camera?:ID,actor?:{id,name,kind},command:SessionCommand}", "GET /api/preview?camera=ID":"bounded PNG preview", "GET /api/destinations":"saved destination names and locations", "GET /api/storage":"global quota status", "GET, HEAD /health":"plain JSON readiness, uptime, camera counts and analytics health (200 ready, 503 unavailable)", "GET, HEAD /metrics":"Prometheus text exposition", "GET, HEAD /api/analytics":"AnalyticsSnapshot for the current session", "GET, HEAD /api/analytics.svg":"Plotters SVG acquisition dashboard", "GET, HEAD /api/signals?history=0|1":"{sources,persisting,dropped,sink_error}; history=1 adds recent points per tag"},
        "result":"JSON APIs use the versioned ok/result/error envelope; /health returns plain JSON",
        "analytics":"In-memory DuckDB collects independently of clients; at most 24 hours of time-series history and a fixed sample limit. Session counters survive camera disconnects and reset with the server process.",
        "outputs":"Capture paths and destinations belong to the server. Stdout captures are rejected.",
        "lifetime":"Browser disconnection leaves cameras and jobs running in the serve session. Jobs stop when the server exits.",
        "network":"No remote listener unless --listen is given. Bind a Tailscale address, or loopback behind Tailscale Serve."
    });
    contract["selection"]["direct"] = json!(
        "--camera ID|serial|IPv4|sim:NAME|RTSP/SRT/media URI|avfoundation:INDEX|v4l2:/dev/videoN|dshow:video=NAME|onvif:ENDPOINT|scanner:sane:DEVICE|scanner:wia:ID|scanner:ica:UUID"
    );
    contract["selection"]["existing_session"] =
        json!("--session NAME [--camera ID] targets an existing visible GUI or headless worker");
    contract["limits"]["active_jobs_per_camera"] = json!(32);
    contract["limits"]["camera_workers_default"] = json!(16);
    #[cfg(feature = "ai")]
    {
        contract["remote"]["routes"]["GET, HEAD /api/measurements"] =
            json!("Retained prompt-defined numeric measurements; no API keys or frames");
        contract["remote"]["routes"]["GET, HEAD /api/measurements.svg"] =
            json!("Plotters visualization of prompt measurements");
        contract["video_analytics"] = json!({
            "desktop":"Settings configures provider API keys in the OS native credential store; Analytics configures prompts and explicitly starts collection on a live camera.",
            "providers":["openai","gemini","claude"],
            "environment":{"openai":["OPENAI_API_KEY"],"gemini":["GOOGLE_API_KEY","GEMINI_API_KEY"],"claude":["ANTHROPIC_API_KEY"]},
            "credentials":"First nonempty environment variable takes precedence over the saved native-keyring credential. Keys are never included in preferences or measurement history.",
            "measurements":"Up to 16 named prompts with expected units; structured nullable value, confidence and note per measurement.",
            "sampling":"5..3600 seconds; one request at a time; starts paused and stops on errors.",
            "retention":"In-memory DuckDB; at most 24 hours and 10000 measurement rows; history resets with the process."
        });
    }
    contract["limits"]["shared_memory_slots_per_camera"] = json!(3);
    contract["camera_profiles"] = json!({
        "location":"CameraInfo.profile: discover.devices[], info.camera, status.connected and status.cameras[].info",
        "kinds":["unknown","area_scan","line_scan","depth","microscope","thermal","document_scanner"],
        "sources":["unknown","transport","identity","features","device_metadata"],
        "capabilities":["exposure","gain","roi","binning","frame_rate","line_rate","trigger","encoder","depth_components","depth_range","spatial_calibration","focus","magnification","illumination","temperature","scan_resolution","scan_source"],
        "fields":{
            "kind":"Camera purpose from kinds; independent of its transport",
            "source":"Source of the classification from sources",
            "evidence":"Array of strings explaining the detected kind",
            "capabilities":"Array of advertised control families; FeatureInfo remains authoritative for live availability, access, selectors, choices and limits"
        },
        "detection":"Discovery may be provisional; connection refines from readable features. Priority: scanner transport, explicit DeviceType/CameraType/SensorType class, explicit Linescan geometry, specialty feature families, remaining declared scan geometry, model identity, conventional image dimensions and frame timing, then unknown. A one-row ROI, high bit depth or vendor name alone does not identify a specialty camera.",
        "simulators":["sim:0","sim:NAME","sim:line[:name]","sim:depth[:name]","sim:microscope[:name]","sim:thermal[:name]","sim:replay:PATH"],
        "ui":"Prioritizes advertised type-specific controls. Line history shows recent received display rows with gaps for skipped blocks; scalar depth/thermal previews automatically color raw sample bounds without applying device calibration; Sensor pixels returns to the ordinary preview. Captures save individual blocks, not display history.",
        "measurements":"Raw captures preserve acquired bytes and sample precision without embedded calibration. Keep component, unit, scale, offset and calibration features separately. PNG/JPEG/PGM/PPM and forwarded video are 8-bit visualizations; forwarding carries no measurement calibration or GUI color map.",
        "auto":"Generic auto exposure is unsupported for line_scan, depth, thermal and document_scanner. Image cameras, including microscopes with suitable advertised controls, retain support.",
        "stream_limits":"Existing transports acquire single rasters. Depth requires a compatible single-component stream; multipart, GenDC, point-cloud payloads and vendor SDK-only access remain unsupported. Detection does not establish hardware compatibility."
    });
    contract["simulation"] = json!({
        "cli":"simulate presets | simulate run --preset gige|rtsp|onvif|vision [--source PATH] [--duration 30s]",
        "presets":crate::simulation::presets()["presets"],
        "replay":"--camera sim:replay:PATH uses a local simulator with configurable Width, Height and AcquisitionFrameRate; network presets use --source PATH",
        "playlist":"A file or one-level directory of supported images/videos. Regular files only, lexicographically sorted by filename, no recursion or symlink traversal; loops until stopped.",
        "timing":"Fixed delivered frame rate; frames are letterboxed to camera dimensions. Frame IDs and timestamps continue across file and playlist boundaries.",
        "network":"Loopback by default. Use --bind LAN_IP for remote clients, or --bind 0.0.0.0 --advertise LAN_IP. Ports default to GigE 3956/UDP, RTSP 8554/TCP and ONVIF 8080/HTTP.",
        "lifetime":"Runs in foreground until Ctrl-C or --duration; protocol listeners and decoder/encoder children are stopped on exit."
    });
    contract["scanners"] = json!({"enumeration":"scanners --json or discover --json; transport is scanner","backends":{"linux":"SANE scanimage (installed separately)","windows":"Windows Image Acquisition (WIA)","macos":"ImageCaptureCore (bundled native helper)"},"feature":"scanner; enabled by default","settings":{"ScanResolution":{"default":150,"range":[75,600],"unit":"dpi"},"ScanMode":{"default":"Color","choices":["Color","Gray","Lineart"]},"ScanSource":{"default":"Default","choices":["Default","Flatbed","Feeder"]}},"acquisition":"capture requests one page; count requests successive pages, including feeder pages; connecting/start never scans; forwarding and auto exposure are unsupported","timeout":"Use --timeout-ms 60000 for physical scans; GUI scanner timeout defaults to 60000. Each page has its own deadline and timeout cancels the helper.","outputs":"Existing capture image formats, destinations and storage quotas; no PDF/OCR conversion","limits":{"frame_bytes":134217728,"shared_ring_default_bytes_per_slot":134217728},"compatibility":"Driver-dependent settings; unsupported resolutions, modes and source names fail without silent fallback. SANE source names vary between drivers."});
    contract["storage"] = json!({"default":crate::storage::StoragePolicy::default(),"scope":"All Capturefab-owned capture and recording files across output directories in this user's session directory. Reservations count toward quota. Existing/unowned/modified files are never deleted.","recording_file_cap_default":crate::media::default_recording_cap(),"on_full":["stop","delete-oldest"],"status":"capturefab storage --json","error":"storage_full; exit 6"});
    contract["scheduling"] = json!({"cli":"capture --at RFC3339 | --delay 30s; --interval 10s -n 100","lifetime":"Schedules run while the GUI, serve session or direct CLI process remains running; jobs are not persisted across exit.","interval":"First time plus captured count times interval; zero interval captures consecutive frames. All frames must fall within 10 years. A past first timestamp starts immediately. Durations must resolve to whole milliseconds. Each job has a per-frame transport timeout.","states":["pending","running","complete","cancelled","failed"]});
    contract["auto"] = json!({"cli":"--session NAME auto [--balance B] | manual [--revert]; capture|forward|serve --auto [--balance B]","default":"manual; connecting never enables auto mode and it ends on disconnect","balance":"0 favors image quality, 1 favors frame rate; quality, balanced and frame-rate mean 0, 0.5 and 1; omitted keeps the current value, initially 0.5","status":"status.auto and cameras[].auto; null in manual mode","strategies":["firmware","software","video-mode","none"],"states":["waiting","converging","stable","limited"],"settled":["stable","limited"],"manual":"Setting a feature in auto.managed switches that camera to manual first and the set result adds \"auto\":null; manual turns ExposureAuto, GainAuto and BalanceWhiteAuto off so values hold","persistence":"Camera settings written by auto mode remain after manual, like set; manual --revert, disconnect and session exit restore their previous values (best effort)","capture":"--set applies before --auto; capture saves once exposure settles or after min(timeout_ms/2, 3 s) and result.auto reports the state; with --session auto mode stays on afterwards"});
    contract["history"] = json!({
        "cli":"--session NAME [--camera ID] history | undo | redo",
        "scope":"Reversible camera changes are recorded by the camera worker, including changes made through the GUI, CLI and RPC; each connected camera has its own history.",
        "status":"status.history and cameras[].history contain undo and redo labels, or null when unavailable; history returns the selected or addressed camera's labels.",
        "lifetime":"History is bounded and lasts while the camera remains connected; a new change clears its redo history.",
        "checkpoints":"Successful reversible commands also return camera_change:{token,camera,label,last_changed_at_us}. replay_camera_change targets that token with undo:true or undo:false for desktop history; expired tokens and conflicting camera state fail.",
        "conflicts":"Replay checks current camera state before changing it. A conflicting or failed replay reports an error and retains the history entry.",
        "effects":"Captures, uploads, recordings, schedules, connection changes, arbitrary commands and raw memory writes cannot be reversed by camera history. Stopping a stream with schedules or forwarding is also excluded. Scanner settings are reversible; scanner acquisition, including Start and Stop, may move or consume physical pages and cannot be undone or replayed."
    });
    let commands = contract["rpc"]["commands"].as_array_mut().unwrap();
    commands.push(json!({"op":"activity","after":12,"timeout_ms":20000}));
    commands.extend([
        json!({"op":"network_scan","timeout_ms":1000}),
        json!({"op":"optimize_network","bytes_per_second":100000000}),
    ]);
    commands.extend([json!({"op":"select","camera":"sim:0"}),json!({"op":"jobs"}),json!({"op":"cancel_job","id":1}),json!({"op":"schedule","output":"timelapse","count":10,"timeout_ms":2000,"format":"png","first_at_ms":crate::scheduling::now_ms()+60000,"interval_ms":10000,"storage":crate::storage::StoragePolicy::default()}),json!({"op":"forward","output":"rtsp://localhost:8554/camera","codec":"h264","encoder":"auto","fps":30,"bitrate":"4M","storage":crate::storage::StoragePolicy::default(),"max_file_bytes":crate::media::default_recording_cap()}),json!({"op":"stop_forward"}),json!({"op":"auto","balance":0.5}),json!({"op":"manual"})]);
    commands.extend([
        json!({"op":"undo"}),
        json!({"op":"redo"}),
        json!({"op":"replay_camera_change","token":"TOKEN_FROM_CAMERA_CHANGE","undo":true}),
    ]);
    commands.extend([
        json!({"op":"capture_session_open","path":"session.duckdb","create":true}),
        json!({"op":"capture_session_open","path":"session.duckdb","create":false}),
        json!({"op":"capture_session_close"}),
        json!({"op":"capture_example","timeout_ms":2000}),
        json!({"op":"auto_capture","enabled":true,"sensitivity":0.5,"cooldown_ms":2000}),
        json!({"op":"capture_review","id":null}),
        json!({"op":"review_capture","id":1,"keep":true}),
        json!({"op":"review_capture","id":2,"keep":false}),
    ]);
    commands.push(json!({"op":"capture","output":"line1/{date}/{camera}-{datetime}-{seq}.png","count":1,"timeout_ms":2000,"format":"png","on_exists":"number","context":{"line":"L1","part":"A-100"}}));
    commands.extend(crate::dataset::rpc_examples());
    commands.extend(crate::signals::rpc_examples());
    contract["dataset"] = crate::dataset::schema_contract();
    contract["signals"] = crate::signals::schema_contract();
    contract
}

/// A dataset path made absolute against the working directory.
fn absolute_dataset(path: &std::path::Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty(),
        "dataset path must be nonempty"
    );
    ensure!(
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("duckdb")),
        "dataset files use the .duckdb extension"
    );
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}
/// The path a session reports for this dataset, when its folder exists.
fn canonical_dataset(path: &std::path::Path) -> Option<PathBuf> {
    Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?))
}

/// Arguments of `dataset` subcommands, checked before any session is touched.
fn prepare_dataset(cli: &Cli) -> Result<Option<SessionCommand>> {
    let Some(Command::Dataset { command }) = &cli.command else {
        return Ok(None);
    };
    let command = match command {
        DatasetCommand::Open {
            path,
            new,
            existing,
            max_bytes,
        } => SessionCommand::DatasetOpen {
            path: absolute_dataset(path)?.to_string_lossy().into_owned(),
            mode: if *new {
                crate::dataset::OpenMode::Create
            } else if *existing {
                crate::dataset::OpenMode::Open
            } else {
                crate::dataset::OpenMode::Auto
            },
            max_bytes: max_bytes
                .as_deref()
                .map(crate::storage::parse_bytes)
                .transpose()?,
        },
        DatasetCommand::Close => SessionCommand::DatasetClose,
        DatasetCommand::Status { file } => {
            if let Some(file) = file {
                absolute_dataset(file)?;
            }
            SessionCommand::DatasetStatus
        }
        DatasetCommand::Note { text, context } => SessionCommand::DatasetNote {
            text: text.clone(),
            context: crate::context::from_pairs(context)?,
            camera: cli.camera.clone(),
        },
        DatasetCommand::Query { file, sql, limit } => {
            if let Some(file) = file {
                absolute_dataset(file)?;
            }
            SessionCommand::DatasetQuery {
                sql: sql.clone(),
                limit: *limit,
                timeout_ms: 0,
            }
        }
        DatasetCommand::Export {
            file,
            output,
            format,
            tables,
            since,
            until,
            storage,
        } => {
            if let Some(file) = file {
                absolute_dataset(file)?;
            }
            ensure!(
                !output.as_os_str().is_empty(),
                "export folder must be nonempty"
            );
            let output = if output.is_absolute() {
                output.clone()
            } else {
                std::env::current_dir()?.join(output)
            };
            SessionCommand::DatasetExport {
                output: output.to_string_lossy().into_owned(),
                formats: crate::dataset::ExportFormat::or_default(format.clone()),
                tables: tables.clone(),
                since_ms: since
                    .as_deref()
                    .map(crate::scheduling::parse_at)
                    .transpose()?,
                until_ms: until
                    .as_deref()
                    .map(crate::scheduling::parse_at)
                    .transpose()?,
                storage: storage.policy()?,
            }
        }
    };
    command.validate()?;
    Ok(Some(command))
}

/// `dataset status|query|export --file`: read a dataset no session holds.
fn dataset_file(command: &DatasetCommand, prepared: Option<SessionCommand>) -> Result<Value> {
    let (DatasetCommand::Status { file: Some(file) }
    | DatasetCommand::Query {
        file: Some(file), ..
    }
    | DatasetCommand::Export {
        file: Some(file), ..
    }) = command
    else {
        anyhow::bail!("dataset --file needs a file");
    };
    let dataset = crate::dataset::Dataset::open_read_only(&absolute_dataset(file)?)?;
    let usage = |error: anyhow::Error| -> anyhow::Error {
        if error.is::<crate::dataset::Usage>() {
            InvalidInput(error.to_string()).into()
        } else {
            error
        }
    };
    match prepared {
        Some(SessionCommand::DatasetQuery { sql, limit, .. }) => {
            dataset.reader()?.query(&sql, limit, 0).map_err(usage)
        }
        Some(SessionCommand::DatasetExport {
            output,
            formats,
            tables,
            since_ms,
            until_ms,
            storage,
        }) => dataset
            .reader()?
            .export(&crate::dataset::export::ExportRequest {
                output: output.into(),
                formats,
                tables,
                since_ms,
                until_ms,
                storage,
            }),
        _ => Ok(serde_json::to_value(dataset.status())?),
    }
}

/// `dataset` subcommands in a session.
fn dataset_command(
    cli: &Cli,
    client: &Client,
    _command: &DatasetCommand,
    prepared: Option<SessionCommand>,
) -> Result<Value> {
    input((|| {
        ensure!(
            cli.session.is_some(),
            "no session; pass --file FILE.duckdb, or --session NAME for a running session"
        );
        Ok(())
    })())?;
    client.call(prepared.context("dataset command")?)
}

/// Human-readable dataset results.
fn dataset_output(out: &mut impl Write, command: &DatasetCommand, value: &Value) -> Result<()> {
    let status_line = |status: &Value| {
        let name = status["path"]
            .as_str()
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_owned())
            })
            .unwrap_or_default();
        format!(
            "{name}: {} captures, {} recordings, {} signals, {} samples, {} events",
            status["captures"],
            status["recordings"],
            status["signals"],
            status["samples"],
            status["events"]
        )
    };
    match command {
        DatasetCommand::Close => match value.get("closed") {
            Some(status) if !status.is_null() => writeln!(out, "Closed {}", status_line(status))?,
            _ => writeln!(out, "No dataset was open.")?,
        },
        DatasetCommand::Note { .. } => writeln!(out, "Note #{} added.", value["id"])?,
        DatasetCommand::Open { .. } | DatasetCommand::Status { .. } => {
            if value.is_null() {
                writeln!(out, "No dataset is open.")?;
            } else {
                writeln!(out, "{}", status_line(value))?;
            }
        }
        DatasetCommand::Query { .. } => {
            let cell = |v: &Value| match v {
                Value::String(s) => s.replace(['\t', '\n'], " "),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let names: Vec<String> = value["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| cell(&c["name"]))
                .collect();
            writeln!(out, "{}", names.join("\t"))?;
            for row in value["rows"].as_array().into_iter().flatten() {
                let cells: Vec<String> = row.as_array().into_iter().flatten().map(cell).collect();
                writeln!(out, "{}", cells.join("\t"))?;
            }
            if value["truncated"] == true {
                eprintln!("capturefab: result truncated; raise --limit or narrow the query");
            }
        }
        DatasetCommand::Export { .. } => {
            for file in value["files"].as_array().into_iter().flatten() {
                writeln!(out, "{}", file["path"].as_str().unwrap_or(""))?;
            }
            writeln!(
                out,
                "Exported to {}",
                value["output"].as_str().unwrap_or("")
            )?;
        }
    }
    Ok(())
}

/// Arguments of `signals` and `log`, checked before any session is touched.
/// Only the URI shape is checked here; protocols missing from this build
/// fail when the command runs.
fn prepare_signals(cli: &Cli) -> Result<Option<SessionCommand>> {
    Ok(match &cli.command {
        Some(Command::Signals { command }) => Some(match command {
            None => SessionCommand::Signals,
            Some(SignalsCommand::Read { uri }) => SessionCommand::SignalRead {
                uri: uri.clone(),
                timeout_ms: cli.timeout_ms,
            },
            Some(SignalsCommand::Browse {
                uri,
                duration,
                limit,
            }) => SessionCommand::SignalBrowse {
                uri: uri.clone(),
                duration_ms: crate::scheduling::parse_duration(duration)?,
                limit: *limit,
            },
            Some(SignalsCommand::Start {
                uri,
                interval,
                tags,
            }) => SessionCommand::SignalStart {
                uri: uri.clone(),
                interval_ms: crate::scheduling::parse_duration(interval)?,
                tags: tags.clone(),
            },
            Some(SignalsCommand::Stop { id }) => SessionCommand::SignalStop { id: id.clone() },
        }),
        Some(Command::Log { .. }) => {
            crate::signals::log::validate(&log_options(cli)?)?;
            None
        }
        _ => None,
    })
}

/// `signals` subcommands; the command was built by `prepare_signals`.
fn signals_command(
    _cli: &Cli,
    client: &Client,
    _command: Option<&SignalsCommand>,
    prepared: Option<SessionCommand>,
) -> Result<Value> {
    client.call(prepared.context("signals command was not prepared")?)
}

/// `log`: run a local session that logs signal sources into a dataset or
/// CSV file for a duration or until Ctrl-C, then print the result.
fn log_command(cli: &Cli) -> Result<()> {
    let options = log_options(cli)?;
    let handle = SessionHandle::new();
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
        .context("install signal handler")?;
    let result = crate::signals::log::run(&handle, &options, &stopped);
    handle.shutdown();
    output(cli, result?)
}

/// `log` arguments as the signals module takes them.
fn log_options(cli: &Cli) -> Result<crate::signals::log::LogOptions> {
    let Some(Command::Log {
        uris,
        interval,
        duration,
        csv,
        tags,
        dataset,
        storage,
    }) = &cli.command
    else {
        anyhow::bail!("not a log command");
    };
    Ok(crate::signals::log::LogOptions {
        uris: uris.clone(),
        interval_ms: crate::scheduling::parse_duration(interval)?,
        duration_ms: duration
            .as_deref()
            .map(crate::scheduling::parse_duration)
            .transpose()?,
        csv: csv.clone(),
        dataset: dataset.dataset.clone(),
        tags: tags.clone(),
        policy: storage.policy()?,
        progress: !cli.json,
    })
}

/// A direct CLI capture to a network destination uploads before exiting, so
/// files are not left waiting for a process that never runs.
fn wait_for_uploads(cli: &Cli, destination: Option<&str>, result: &mut Value) -> Result<()> {
    let Some(name) = destination else {
        return Ok(());
    };
    if cli.session.is_some() || !crate::destination::get(name)?.target.is_remote() {
        return Ok(());
    }
    #[cfg(not(feature = "uploads"))]
    let _ = result;
    #[cfg(feature = "uploads")]
    {
        if !cli.json {
            eprintln!("Uploading to {name}...");
        }
        let status = crate::upload::drain(Duration::from_secs(600))?;
        if !cli.json {
            eprintln!(
                "{} waiting, {} failed{}",
                status["pending"].as_u64().unwrap_or(0) + status["uploading"].as_u64().unwrap_or(0),
                status["failed"].as_u64().unwrap_or(0),
                status["last_error"]
                    .as_str()
                    .map(|e| format!("; last error: {e}"))
                    .unwrap_or_default()
            );
        }
        result["uploads"] = status;
    }
    Ok(())
}

/// The bucket an add-s3 command describes.
fn s3_bucket(command: &DestinationCommand) -> Result<crate::destination::Bucket> {
    use crate::destination::{Bucket, Credentials, NoOverwrite};
    let DestinationCommand::AddS3 {
        bucket,
        region,
        endpoint,
        prefix,
        path_style,
        access_key_id,
        profile,
        env,
        keep_local,
        preset,
        check_first,
        ..
    } = command
    else {
        anyhow::bail!("not an add-s3 command");
    };
    let credentials = match (access_key_id, profile, env) {
        (Some(id), _, _) => Credentials::Keychain {
            access_key_id: id.clone(),
        },
        (_, Some(name), _) => Credentials::Profile { name: name.clone() },
        (_, _, true) => Credentials::Environment,
        _ => {
            return Err(InvalidInput(
                "choose credentials: --access-key-id ID --secret-stdin, --profile NAME or --env"
                    .into(),
            )
            .into());
        }
    };
    Ok(Bucket {
        endpoint: endpoint.clone(),
        region: region.clone(),
        bucket: bucket.clone(),
        prefix: prefix.clone(),
        // MinIO and RustFS serve buckets in the URL path, and an IP or
        // localhost endpoint cannot take the bucket in its host name.
        path_style: *path_style
            || preset.is_some()
            || crate::destination::endpoint_needs_path_style(endpoint),
        credentials,
        keep_local: *keep_local,
        no_overwrite: if *check_first {
            NoOverwrite::CheckFirst
        } else {
            NoOverwrite::IfNoneMatch
        },
    })
}

/// Find out whether a bucket refuses to replace objects; switch the saved
/// destination to check-first uploads when it does not, and back when it
/// does. The switch is a compare-and-swap, so a concurrent edit wins.
#[cfg(feature = "s3")]
fn probe_writes(saved: &crate::destination::Destination) -> Result<Value> {
    use crate::destination::{NoOverwrite, Target};
    let Target::S3(bucket) = &saved.target else {
        return Err(InvalidInput("--probe-writes applies to S3 destinations only".into()).into());
    };
    let (writes, removed) = crate::s3::Client::new(bucket)?.probe_conditional_writes()?;
    let mode = if writes == crate::s3::ConditionalWrites::Enforced {
        NoOverwrite::IfNoneMatch
    } else {
        NoOverwrite::CheckFirst
    };
    let saved_now = if mode == bucket.no_overwrite {
        true
    } else {
        let mut after = saved.clone();
        if let Target::S3(bucket) = &mut after.target {
            bucket.no_overwrite = mode;
        }
        crate::destination::replace(Some(saved), Some(&after)).is_ok()
    };
    Ok(json!({
        "ok": true,
        "conditional_writes": writes.as_str(),
        "no_overwrite": mode,
        "saved": saved_now,
        "probe_removed": removed,
    }))
}

fn destination_command(command: &DestinationCommand) -> Result<Value> {
    use crate::destination::{self, Destination, Target};
    let describe = |d: &Destination| {
        let mut value = serde_json::to_value(d).unwrap_or_default();
        value["location"] = json!(d.describe());
        if let Some(remote) = d.target.remote() {
            value["insecure"] = json!(remote.insecure());
        }
        #[cfg(feature = "s3")]
        if let Target::S3(crate::destination::Bucket {
            credentials: crate::destination::Credentials::Keychain { access_key_id },
            ..
        }) = &d.target
        {
            value["secret_saved"] = json!(crate::s3::has_secret(access_key_id));
        }
        #[cfg(feature = "uploads")]
        if let Some((service, account)) = d.target.remote().and_then(|r| r.secret_ref()) {
            value["secret_saved"] = json!(crate::transfer::has_secret(service, &account));
        }
        value
    };
    Ok(match command {
        DestinationCommand::List => {
            json!({"destinations": destination::list()?.iter().map(describe).collect::<Vec<_>>()})
        }
        DestinationCommand::AddFolder { name, path } => {
            let path = std::path::absolute(path)?;
            let saved = Destination {
                name: name.clone(),
                target: Target::Folder { path: path.clone() },
            };
            input(saved.validate())?;
            crate::volumes::ensure_present(&path)?;
            std::fs::create_dir_all(&path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            destination::save(saved.clone())?;
            json!({"destination": describe(&saved)})
        }
        DestinationCommand::AddS3 {
            name,
            access_key_id,
            secret_stdin,
            no_check,
            preset,
            probe_writes: probe,
            check_first,
            ..
        } => {
            let saved = Destination {
                name: name.clone(),
                target: Target::S3(s3_bucket(command)?),
            };
            input(saved.validate())?;
            #[cfg(not(feature = "s3"))]
            {
                let _ = (
                    access_key_id,
                    secret_stdin,
                    no_check,
                    preset,
                    probe,
                    check_first,
                );
                anyhow::bail!(
                    "S3 destinations are unsupported in this build; rebuild with --features s3"
                );
            }
            #[cfg(feature = "s3")]
            {
                if *secret_stdin {
                    let secret = input(read_secret(io::stdin().lock()))?;
                    crate::s3::store_secret(access_key_id.as_deref().unwrap_or(""), &secret)?;
                }
                let Target::S3(bucket) = &saved.target else {
                    unreachable!()
                };
                let check = if *no_check {
                    None
                } else {
                    Some(crate::s3::Client::new(bucket).and_then(|client| client.check()))
                };
                if let Some(Err(error)) = &check {
                    if crate::s3::retryable(error) {
                        anyhow::bail!(
                            "cannot reach the bucket: {error:#}; fix the settings or save with --no-check"
                        );
                    }
                    // Write-only keys may not inspect the bucket yet can
                    // upload: save with a warning. Anything else (refused
                    // credentials, a missing bucket) is not saved.
                    if !format!("{error:#}").contains("may not inspect it") {
                        anyhow::bail!(
                            "the bucket check failed: {error:#}; fix the settings or save with --no-check"
                        );
                    }
                }
                destination::save(saved.clone())?;
                let mut value = json!({"destination": describe(&saved)});
                if let Some(check) = check {
                    value["check"] = match check {
                        Ok(()) => json!({"ok": true}),
                        Err(error) => json!({"ok": false, "warning": format!("{error:#}")}),
                    };
                }
                // Self-hosted services differ in conditional-write support,
                // so presets find out unless asked not to check.
                if *probe || (preset.is_some() && !*no_check && !*check_first) {
                    value["probe"] = probe_writes(&saved).unwrap_or_else(
                        |error| json!({"ok": false, "warning": format!("{error:#}")}),
                    );
                    if let Ok(current) = destination::get(name) {
                        value["destination"] = describe(&current);
                    }
                }
                value
            }
        }
        DestinationCommand::AddFtp {
            password_stdin,
            no_check,
            ..
        }
        | DestinationCommand::AddSftp {
            password_stdin,
            no_check,
            ..
        } => {
            let (host, port, given, accept) = match command {
                DestinationCommand::AddSftp {
                    host,
                    port,
                    host_key,
                    accept_host_key,
                    ..
                } => (host, *port, host_key.clone(), *accept_host_key),
                DestinationCommand::AddFtp { host, port, .. } => (host, *port, None, false),
                _ => unreachable!(),
            };
            let sftp = matches!(command, DestinationCommand::AddSftp { .. });
            let (kind, feature) = if sftp {
                ("SFTP", cfg!(feature = "sftp"))
            } else {
                ("FTP", cfg!(feature = "ftp"))
            };
            // Shape checks first: a bad setting is a usage error even in a
            // build without this destination kind.
            input(
                server_destination(command, given.as_deref().unwrap_or(PENDING_HOST_KEY))
                    .expect("server destination")
                    .validate(),
            )?;
            ensure!(
                feature,
                "{kind} destinations are unsupported in this build; rebuild with --features {}",
                kind.to_ascii_lowercase()
            );
            let host_key = match given {
                Some(key) => key,
                None if accept => {
                    #[cfg(feature = "sftp")]
                    {
                        let key = crate::transfer::sftp::fetch_host_key(host, port).map_err(
                            |error| {
                                anyhow::anyhow!(
                                    "cannot reach the server: {error:#}; fix the settings and try again"
                                )
                            },
                        )?;
                        eprintln!(
                            "capturefab: trusting SFTP host key {key} for {host}:{port} on first use"
                        );
                        key
                    }
                    #[cfg(not(feature = "sftp"))]
                    {
                        let _ = (host, port);
                        unreachable!("checked above")
                    }
                }
                None => String::new(),
            };
            let saved = server_destination(command, &host_key).expect("server destination");
            input(saved.validate())?;
            let remote = saved.target.remote().expect("network destination");
            #[cfg(not(feature = "uploads"))]
            {
                let _ = (password_stdin, no_check, remote);
                unreachable!("checked above")
            }
            #[cfg(feature = "uploads")]
            {
                if *password_stdin {
                    let secret = input(crate::transfer::read_stdin_secret(
                        io::stdin().lock(),
                        "password",
                    ))?;
                    let (service, account) = remote.secret_ref().expect("keychain secret");
                    crate::transfer::store_secret(service, &account, &secret)?;
                }
                let check = (!*no_check)
                    .then(|| crate::transfer::connect(&remote).and_then(|mut u| u.check()));
                if let Some(Err(error)) = &check {
                    // A refused sign-in, a wrong host key or an untrusted
                    // certificate would only fail every upload: not saved.
                    if crate::transfer::retryable(error) {
                        anyhow::bail!(
                            "cannot reach the server: {error:#}; fix the settings or save with --no-check"
                        );
                    }
                    anyhow::bail!(
                        "the server check failed: {error:#}; fix the settings or save with --no-check"
                    );
                }
                destination::save(saved.clone())?;
                let mut value = json!({"destination": describe(&saved)});
                if sftp {
                    value["host_key"] = json!({
                        "fingerprint": host_key,
                        "trust": if accept { "accepted_on_first_use" } else { "pinned" },
                    });
                }
                if let Some(check) = check {
                    value["check"] = match check {
                        Ok(()) => json!({"ok": true}),
                        Err(error) => json!({"ok": false, "warning": format!("{error:#}")}),
                    };
                }
                value
            }
        }
        DestinationCommand::Remove {
            name,
            forget_secret,
        } => {
            let removed = destination::remove(name)?;
            let mut value = json!({"removed": removed.name});
            #[cfg(feature = "uploads")]
            if *forget_secret {
                // S3 keychain keys are stored by access key ID; servers by
                // user@host:port.
                let secret = |target: &Target| match target {
                    Target::S3(crate::destination::Bucket {
                        credentials: crate::destination::Credentials::Keychain { access_key_id },
                        ..
                    }) => Some(("capturefab-s3", access_key_id.clone())),
                    target => target.remote().and_then(|r| r.secret_ref()),
                };
                if let Some(reference) = secret(&removed.target) {
                    match destination::list()?
                        .into_iter()
                        .find(|d| secret(&d.target).as_ref() == Some(&reference))
                    {
                        Some(other) => {
                            value["secret_kept"] = json!(format!("used by {}", other.name))
                        }
                        None => crate::transfer::delete_secret(reference.0, &reference.1)?,
                    }
                }
            }
            #[cfg(not(feature = "uploads"))]
            let _ = (forget_secret, &mut value);
            value
        }
        DestinationCommand::Test {
            name,
            probe_writes: probe,
        } => {
            let saved = destination::get(name)?;
            if *probe {
                if !matches!(saved.target, Target::S3(_)) {
                    return Err(InvalidInput(
                        "--probe-writes applies to S3 destinations only".into(),
                    )
                    .into());
                }
                #[cfg(feature = "s3")]
                return probe_writes(&saved);
            }
            match &saved.target {
                Target::Folder { path } => {
                    crate::volumes::ensure_present(path)?;
                    ensure!(path.is_dir(), "{} does not exist", path.display());
                    let probe = path.join(format!(".capturefab-write-test-{}", std::process::id()));
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&probe)
                        .with_context(|| format!("cannot write to {}", path.display()))?;
                    std::fs::remove_file(&probe)?;
                    json!({"ok": true, "free_bytes": fs2::available_space(path).ok()})
                }
                Target::S3(bucket) => {
                    #[cfg(feature = "s3")]
                    {
                        crate::s3::Client::new(bucket)?.check()?;
                        json!({"ok": true})
                    }
                    #[cfg(not(feature = "s3"))]
                    {
                        let _ = bucket;
                        anyhow::bail!(
                            "S3 destinations are unsupported in this build; rebuild with --features s3"
                        )
                    }
                }
                Target::Ftp(_) | Target::Sftp(_) => {
                    let remote = saved.target.remote().expect("network destination");
                    #[cfg(feature = "uploads")]
                    {
                        let probe = crate::transfer::connect(&remote)?.probe()?;
                        json!({"ok": true, "kind": remote.kind(), "probe": probe})
                    }
                    #[cfg(not(feature = "uploads"))]
                    {
                        anyhow::bail!(
                            "{} destinations are unsupported in this build",
                            remote.kind().to_uppercase()
                        )
                    }
                }
            }
        }
    })
}

fn uploads_command(command: Option<&UploadsCommand>) -> Result<Value> {
    #[cfg(feature = "uploads")]
    {
        Ok(match command {
            None | Some(UploadsCommand::Status) => crate::upload::status()?,
            Some(UploadsCommand::Retry { id }) => {
                json!({"retried": crate::upload::retry(id.as_deref())?})
            }
            Some(UploadsCommand::Forget { id }) => {
                json!({"forgotten": crate::upload::forget(id)?})
            }
            Some(UploadsCommand::Wait { timeout }) => crate::upload::drain(Duration::from_millis(
                crate::scheduling::parse_duration(timeout)?,
            ))?,
        })
    }
    #[cfg(not(feature = "uploads"))]
    {
        let _ = command;
        anyhow::bail!(
            "uploads are unsupported in this build; rebuild with --features s3, ftp or sftp"
        )
    }
}

pub fn error_code(error: &anyhow::Error) -> (&'static str, i32) {
    if error
        .chain()
        .any(|cause| cause.is::<InvalidInput>() || cause.is::<crate::dataset::Usage>())
    {
        return ("usage", 2);
    }
    let s = format!("{error:#}").to_ascii_lowercase();
    if s.contains("storage full") || s.contains("no space left") {
        ("storage_full", 6)
    } else if s.contains("connection refused")
        // A host name that does not resolve is as unreachable as a refused
        // port (getaddrinfo wording on macOS, Linux and Windows).
        || s.contains("failed to lookup address")
        || s.contains("cannot resolve")
        || s.contains("name or service not known")
        || s.contains("nodename nor servname")
        || s.contains("temporary failure in name resolution")
        || s.contains("no such host is known")
    {
        ("unavailable", 3)
    } else if s.contains("timed out")
        || s.contains("frame timeout")
        || s.contains("timeout waiting")
        || s.contains("deadline exceeded")
    {
        ("timeout", 4)
    } else if s.contains("unsupported") || s.contains("not supported") || s.contains("disabled") {
        ("unsupported", 5)
    } else if s.contains("not found")
        || s.contains("is not mounted")
        || s.contains("no camera")
        || s.contains("not responding")
        || s.contains("already connected")
        || s.contains("controlled by another application")
        || s.contains("camera access denied")
        || s.contains("camera is in use by another application")
        || s.contains("is locked by another process")
    {
        ("unavailable", 3)
    } else {
        ("operation_failed", 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_camera_identity_takes_priority_over_another_cameras_serial() {
        let state = json!({"cameras":[{"info":{"id":"sim:first","serial":"sim:second"}}, {"info":{"id":"sim:second","serial":"SECOND"}}]});
        assert_eq!(
            addressed_camera(&state, "sim:second").unwrap()["info"]["id"],
            "sim:second"
        );
        assert_eq!(
            addressed_camera(&state, "SECOND").unwrap()["info"]["id"],
            "sim:second"
        );
    }
    #[test]
    fn formats_follow_files_templates_and_explicit_overrides() {
        for (path, count, expected) in [
            ("shot.JPG", 1, "jpeg"),
            ("shot.raw", 1, "raw"),
            ("frame-{frame}.ppm", 10, "ppm"),
            ("frames.raw", 10, "png"),
            ("-", 1, "png"),
        ] {
            assert_eq!(capture_format(None, path, count).unwrap().name(), expected);
        }
        assert!(capture_format(None, "shot.gif", 1).is_err());
        assert_eq!(
            capture_format(Some(ImageFormat::Png), "shot.raw", 1)
                .unwrap()
                .name(),
            "png"
        );
    }
    #[test]
    #[cfg(feature = "s3")]
    fn secret_stdin_preserves_key_bytes_and_bounds_reads() {
        for (input, expected) in [
            (" key \t", " key \t"),
            (" key \t\n", " key \t"),
            (" key \t\r\n", " key \t"),
            ("clé 密钥 \n", "clé 密钥 "),
            ("key\n\n", "key\n"),
            ("key\r", "key\r"),
        ] {
            assert_eq!(read_secret(input.as_bytes()).unwrap(), expected);
        }
        let maximum = "x".repeat(4096);
        assert_eq!(read_secret(maximum.as_bytes()).unwrap(), maximum);
        assert_eq!(
            read_secret(format!("{maximum}\r\n").as_bytes()).unwrap(),
            maximum
        );
        for input in [
            String::new(),
            "\r\n".into(),
            "x".repeat(4097),
            format!("{}\n", "x".repeat(4097)),
            "x".repeat(10000),
        ] {
            assert!(read_secret(input.as_bytes()).is_err());
        }
        assert!(read_secret(&b"\xff\n"[..]).is_err());
    }
    #[test]
    fn unix_cli_contract() {
        Cli::command().debug_assert();
        assert!(
            Cli::try_parse_from(["capturefab"])
                .unwrap()
                .command
                .is_none()
        );
        assert!(
            Cli::try_parse_from(["capturefab", "--json", "discover"])
                .unwrap()
                .json
        );
        assert!(
            Cli::try_parse_from([
                "capturefab",
                "--camera",
                "sim:0",
                "capture",
                "-o",
                "-",
                "-f",
                "raw"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["capturefab", "--timeout-ms", "0", "discover"]).is_err());
        assert!(
            Cli::try_parse_from(["capturefab", "--camera", "a", "--session", "b", "status"])
                .is_ok()
        );
        for args in [
            &["auto", "--balance", "frame-rate"][..],
            &["manual", "--revert"],
            &["capture", "--auto", "--balance", "0.25", "-o", "x"],
            &["forward", "--auto", "-o", "x"],
            &["serve", "--auto", "--balance", "quality"],
        ] {
            assert!(Cli::try_parse_from(["capturefab"].iter().chain(args)).is_ok());
        }
        for args in [
            &["auto", "--balance", "1.5"][..],
            &["auto", "--balance", "NaN"],
            &["capture", "--balance", "0.5", "-o", "x"],
            &["forward", "--balance", "1", "-o", "x"],
            &["serve", "--balance", "0"],
        ] {
            assert!(Cli::try_parse_from(["capturefab"].iter().chain(args)).is_err());
        }
        for action in ["undo", "redo", "history"] {
            assert!(Cli::try_parse_from(["capturefab", "--session", "bench", action]).is_ok());
            assert!(
                Cli::try_parse_from([
                    "capturefab",
                    "--session",
                    "bench",
                    "--camera",
                    "sim:1",
                    action,
                    "--json"
                ])
                .is_ok()
            );
        }
    }
    #[test]
    fn balance_values() {
        for (text, balance) in [
            ("0", 0.0),
            ("0.25", 0.25),
            ("1", 1.0),
            ("quality", 0.0),
            ("balanced", 0.5),
            ("frame-rate", 1.0),
            ("speed", 1.0),
        ] {
            assert_eq!(parse_balance(text), Ok(balance));
        }
        for text in ["-0.1", "1.01", "inf", "NaN", "fast", ""] {
            assert!(
                parse_balance(text)
                    .unwrap_err()
                    .contains("quality, balanced, frame-rate")
            );
        }
    }
    #[test]
    fn capture_session_commands_are_camera_bound_and_prepare_before_execution() {
        for (args, expected) in [
            (
                vec!["new", "session.duckdb"],
                json!({"op":"capture_session_open","path":"session.duckdb","create":true}),
            ),
            (
                vec!["resume", "session.duckdb"],
                json!({"op":"capture_session_open","path":"session.duckdb","create":false}),
            ),
            (vec!["close"], json!({"op":"capture_session_close"})),
            (
                vec!["teach"],
                json!({"op":"capture_example","timeout_ms":2000}),
            ),
            (
                vec!["auto"],
                json!({"op":"auto_capture","enabled":true,"sensitivity":0.5,"cooldown_ms":2000}),
            ),
            (
                vec![
                    "auto",
                    "--pause",
                    "--sensitivity",
                    "0.25",
                    "--cooldown-ms",
                    "500",
                ],
                json!({"op":"auto_capture","enabled":false,"sensitivity":0.25,"cooldown_ms":500}),
            ),
            (vec!["review"], json!({"op":"capture_review","id":null})),
            (
                vec!["review", "--id", "2"],
                json!({"op":"capture_review","id":2}),
            ),
            (
                vec!["keep", "3"],
                json!({"op":"review_capture","id":3,"keep":true}),
            ),
            (
                vec!["discard", "4"],
                json!({"op":"review_capture","id":4,"keep":false}),
            ),
        ] {
            let cli = Cli::try_parse_from(
                [
                    "capturefab",
                    "--session",
                    "bench",
                    "--camera",
                    "sim:0",
                    "capture-session",
                ]
                .into_iter()
                .chain(args),
            )
            .unwrap();
            assert!(camera_command(cli.command.as_ref()));
            assert!(uses_session(cli.command.as_ref()));
            assert!(
                !one_shot_camera_command(cli.command.as_ref()),
                "capture-session controls must not silently connect temporary cameras"
            );
            let command = prepare(&cli).unwrap().unwrap();
            assert_eq!(serde_json::to_value(&command).unwrap(), expected);
            command.validate().unwrap();
        }
        let cli = Cli::try_parse_from([
            "capturefab",
            "--session",
            "bench",
            "capture-session",
            "resume",
            "relative.duckdb",
        ])
        .unwrap();
        let mut command = prepare(&cli).unwrap().unwrap();
        caller_output(&mut command).unwrap();
        assert!(
            matches!(command, SessionCommand::CaptureSessionOpen { path, create: false } if std::path::Path::new(&path).is_absolute() && path.ends_with("relative.duckdb"))
        );
    }
    #[test]
    fn capture_session_rejects_invalid_settings_ids_and_paths() {
        for args in [
            vec!["auto", "--sensitivity", "NaN"],
            vec!["auto", "--sensitivity", "inf"],
            vec!["auto", "--sensitivity", "1.1"],
            vec!["auto", "--cooldown-ms", "249"],
            vec!["auto", "--cooldown-ms", "3600001"],
            vec!["review", "--id", "0"],
            vec!["keep", "0"],
            vec!["discard", "9223372036854775808"],
            vec!["new"],
        ] {
            assert!(
                Cli::try_parse_from(["capturefab", "capture-session"].into_iter().chain(args))
                    .is_err()
            );
        }
        assert!(Cli::try_parse_from(["capturefab", "capture-session", "new", ""]).is_err());
        for path in ["   ", ":memory:", "session.csv", "session\n.duckdb"] {
            let cli = Cli::try_parse_from([
                "capturefab",
                "--session",
                "bench",
                "capture-session",
                "new",
                path,
            ])
            .unwrap();
            assert!(prepare(&cli).is_err(), "invalid path accepted: {path:?}");
        }
        for sensitivity in ["0", "1"] {
            let cli = Cli::try_parse_from([
                "capturefab",
                "capture-session",
                "auto",
                "--sensitivity",
                sensitivity,
            ])
            .unwrap();
            prepare(&cli).unwrap().unwrap().validate().unwrap();
        }
    }
    #[test]
    fn schema_commands_parse() {
        let contract = schema();
        let commands = contract["rpc"]["commands"].as_array().unwrap();
        for command in commands {
            serde_json::from_value::<SessionCommand>(command.clone()).unwrap();
            SessionCommand::from_json_slice(command.to_string().as_bytes())
                .unwrap()
                .validate()
                .unwrap_or_else(|e| panic!("{command}: {e:#}"));
        }
        assert!(
            [
                "auto",
                "manual",
                "undo",
                "redo",
                "replay_camera_change",
                "activity",
                "capture_session_open",
                "capture_session_close",
                "capture_example",
                "auto_capture",
                "capture_review",
                "review_capture",
                "dataset_open",
                "dataset_close",
                "dataset_status",
                "dataset_note",
                "dataset_query",
                "dataset_export",
                "signals",
                "signal_read",
                "signal_browse",
                "signal_start",
                "signal_stop"
            ]
            .iter()
            .all(|op| commands.iter().any(|c| c["op"] == *op))
        );
        assert_eq!(contract["auto"]["settled"], json!(["stable", "limited"]));
    }
    #[test]
    fn multiplayer_identity_and_observer_contract_validate_before_execution() {
        let actor = |kind| {
            Cli::try_parse_from([
                "capturefab",
                "--actor",
                "Focus agent",
                "--actor-kind",
                kind,
                "schema",
            ])
            .unwrap()
            .session_actor()
            .unwrap()
        };
        let agent = actor("agent");
        assert_eq!(agent.kind, ActorKind::Agent);
        assert_eq!(agent, actor("agent"));
        assert_ne!(agent.id, actor("human").id);
        let invalid = Cli::try_parse_from(["capturefab", "--actor", "", "schema"]).unwrap();
        assert!(invalid.session_actor().is_err());
        assert!(Cli::try_parse_from(["capturefab", "activity", "--wait", "20s"]).is_err());
        let observer = Cli::try_parse_from([
            "capturefab",
            "--session",
            "bench",
            "activity",
            "--after",
            "12",
            "--wait",
            "25s",
        ])
        .unwrap();
        assert!(matches!(
            prepare(&observer).unwrap(),
            Some(SessionCommand::Activity {
                after: Some(12),
                timeout_ms: 25_000
            })
        ));
        let excessive = Cli::try_parse_from([
            "capturefab",
            "--session",
            "bench",
            "activity",
            "--after",
            "12",
            "--wait",
            "26s",
        ])
        .unwrap();
        assert!(prepare(&excessive).is_err());
    }
    #[test]
    fn error_codes() {
        for unresolved in [
            "cannot reach the server: cannot resolve ftps://nonexistent.invalid:21: failed to lookup address information: nodename nor servname provided, or not known",
            "resolve Modbus server plc.invalid: failed to lookup address information: Name or service not known",
            "connecting to sftp://x:22: Temporary failure in name resolution",
        ] {
            assert_eq!(
                error_code(&anyhow::anyhow!("{unresolved}")),
                ("unavailable", 3),
                "{unresolved}"
            );
        }
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "dataset d.duckdb is locked by another process; reach it with --session NAME"
            )),
            ("unavailable", 3)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!("datasets are not implemented yet")),
            ("operation_failed", 1)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "camera is controlled by another application; disconnect it first"
            )),
            ("unavailable", 3)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "640x480@60 is not supported by FaceTime HD Camera"
            )),
            ("unsupported", 5)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "FTP destinations are unsupported in this build; rebuild with --features ftp"
            )),
            ("unsupported", 5)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "SFTP host key changed: expected SHA256:a, server offered SHA256:b; confirm with the server administrator and update the destination"
            )),
            ("operation_failed", 1)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "cannot reach the server: connecting on ftp://h:21: Connection refused (os error 61); fix the settings or save with --no-check"
            )),
            ("unavailable", 3)
        );
    }
    #[test]
    fn s3_presets_set_path_style_and_require_endpoint() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(
                [
                    "capturefab",
                    "destination",
                    "add-s3",
                    "x",
                    "--bucket",
                    "camera-archive",
                    "--env",
                ]
                .iter()
                .chain(args),
            )
        };
        let cli = parse(&["--preset", "rustfs"]).unwrap();
        let error = prepare(&cli).unwrap_err().to_string();
        assert!(
            error.contains("--preset rustfs needs --endpoint"),
            "{error}"
        );
        let cli = parse(&["--preset", "minio", "--endpoint", "http://nas:9000"]).unwrap();
        assert!(prepare(&cli).is_ok());
        let Some(Command::Destination { command }) = &cli.command else {
            panic!("destination")
        };
        let DestinationCommand::AddS3 {
            preset, path_style, ..
        } = command
        else {
            panic!("add-s3")
        };
        assert_eq!((*preset, *path_style), (Some(S3Preset::Minio), false));
        assert!(
            s3_bucket(command).unwrap().path_style,
            "presets imply path-style addressing"
        );
        assert!(parse(&["--probe-writes", "--no-check"]).is_err());
        assert!(parse(&["--probe-writes", "--check-first"]).is_err());
        assert!(parse(&["--preset", "aws"]).is_err());
    }
    #[test]
    fn server_destinations_validate_before_touching_settings() {
        let parse =
            |args: &[&str]| Cli::try_parse_from(["capturefab", "destination"].iter().chain(args));
        for args in [
            &["add-ftp", "x", "--host", "ftp://h", "--user", "u"][..],
            &[
                "add-ftp", "x", "--host", "h", "--user", "u", "--dir", "../a",
            ],
            &[
                "add-sftp",
                "x",
                "--host",
                "h",
                "--user",
                "u",
                "--host-key",
                "bogus",
            ],
            &[
                "add-sftp",
                "x",
                "--host",
                "h",
                "--user",
                "u",
                "--accept-host-key",
                "--key-file",
                "rel/id",
            ],
        ] {
            let cli = parse(args).unwrap();
            assert!(prepare(&cli).is_err(), "{args:?}");
        }
        assert!(parse(&["add-ftp", "x", "--host", "h", "--user", "u", "--port", "0"]).is_err());
        assert!(parse(&["add-sftp", "x", "--host", "h", "--user", "u"]).is_err());
        assert!(
            parse(&[
                "add-sftp",
                "x",
                "--host",
                "h",
                "--user",
                "u",
                "--accept-host-key",
                "--no-check"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "add-ftp",
                "x",
                "--host",
                "h",
                "--user",
                "u",
                "--anonymous",
                "--password-env"
            ])
            .is_err()
        );
        let cli = parse(&[
            "add-sftp",
            "x",
            "--host",
            "h",
            "--user",
            "u",
            "--accept-host-key",
        ])
        .unwrap();
        assert!(prepare(&cli).is_ok());
    }
    #[test]
    fn assignments_and_hex() {
        assert_eq!(assignment("A=B=C").unwrap(), ("A".into(), "B=C".into()));
        assert!(assignment("oops").is_err());
        assert_eq!(hex("00aaff").unwrap(), vec![0, 170, 255]);
        assert!(hex("0").is_err());
        assert!(hex("💡").is_err());
    }
}
