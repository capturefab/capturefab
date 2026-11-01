mod human;
pub mod login;
pub(crate) mod paths;
mod resources;
mod setup;
mod summary;
pub use resources::ResourcesCommand;
pub use setup::SetupCommand;

use crate::{
    ipc,
    session::{ActivityOrigin, ActorKind, SessionActor, SessionCommand, SessionHandle},
};
use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum, builder::TypedValueParser};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
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
    bin_name = "capturefab",
    version,
    about = "Discover, configure, capture and record cameras.",
    long_about = "Capturefab discovers, configures, captures and records cameras for people and automation. Camera commands use the only running serve session automatically. Use --session NAME to choose one, or --direct --camera ID for a temporary camera worker. Local output paths are relative to this terminal, including when controlling a session.",
    after_help = "Quick start:\n  capturefab discover --simulate                    Find sources (including a test camera)\n  capturefab --camera sim:0 capture -o shot.png     Capture via the running session, or a temporary worker if none\n  capturefab --direct -c sim:0 capture -o shot.png  Always use a temporary worker\n  capturefab serve --name bench --camera sim:0      Keep a camera open for other commands\n  capturefab --session bench set ExposureTime=5000  Change a setting in that session\n\nExplore: <command> --help, schema --json, doctor --json, completions <shell>.\nPersistent work: start/stop, auto/manual, capture --interval, record, jobs/cancel.\nUse --json for one versioned result; progress and diagnostics go to stderr."
)]
pub struct Cli {
    /// Emit one versioned JSON result on stdout; diagnostics stay on stderr
    #[arg(long, global = true)]
    pub json: bool,
    /// Camera/scanner ID, serial, IP, media source, hub:NAME/CAMERA, sim:0, or sim:replay:PATH
    #[arg(
        short = 'c',
        long,
        global = true,
        env = "CAPTUREFAB_CAMERA",
        hide_env_values = true,
        help_heading = "Target"
    )]
    pub camera: Option<String>,
    /// Choose a serve session (default: CAPTUREFAB_SESSION, then the only running session)
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
    /// Decoding acceleration for new direct/serve workers; existing sessions keep their setting
    #[arg(
        long,
        global = true,
        env = "CAPTUREFAB_HWACCEL",
        default_value = "auto"
    )]
    pub hwaccel: String,
    /// Efficiency mode for this process and the camera workers it starts (default: CAPTUREFAB_EFFICIENCY, then auto, which is efficient on small or low-power computers)
    #[arg(
        long,
        global = true,
        value_parser = clap::builder::PossibleValuesParser::new(["auto", "smooth", "efficient"])
            .map(|mode| mode.parse::<crate::efficiency::Mode>().unwrap_or_default()),
        help_heading = "Runtime"
    )]
    pub efficiency: Option<crate::efficiency::Mode>,
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
        || crate::naming::names_directory(output)
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

/// A CLI path belongs to the caller, even when the worker lives in a session
/// started from a different directory. Named destinations retain their own base.
fn caller_output(command: &mut SessionCommand) -> Result<()> {
    if matches!(command, SessionCommand::Capture { output, .. } if output == "-") {
        return Ok(());
    }
    paths::resolve_outputs(command, &std::env::current_dir()?)
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
#[derive(Debug, Subcommand)]
pub enum DestinationCommand {
    /// List saved folders
    List,
    /// Save a folder on this computer, a removable disk or a mounted drive
    AddFolder { name: String, path: PathBuf },
    /// Remove a saved folder from the list without deleting its files
    Remove { name: String },
    /// Check that a saved folder is present and writable
    Test { name: String },
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
pub enum Command {
    /// Discover GigE Vision, USB3 Vision, ONVIF, host cameras, document scanners and cameras on saved hubs
    #[command(
        visible_aliases=["list","cameras"],
        after_help = "Examples:\n  capturefab discover                      List cameras on this computer and network\n  capturefab discover --simulate           Include the built-in test camera sim:0\n  capturefab --json discover               One versioned JSON result for scripts\n  capturefab --timeout-ms 5000 discover    Wait longer for slow networks"
    )]
    Discover,
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
    #[command(
        after_help = "Examples:\n  capturefab -c sim:0 set ExposureTime=5000\n  capturefab -c sim:0 set Width=640 Height=480     Applied in order\n  capturefab -c sim:0 features --filter Exposure   Find feature names and bounds"
    )]
    Set {
        /// FEATURE=VALUE, e.g. ExposureTime=5000; see `features` for names
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
    /// Save a tuned camera's settings by name and restore them later, with a preview of what changes
    #[command(visible_alias = "setups")]
    Setup {
        #[command(subcommand)]
        command: SetupCommand,
    },
    /// Connect a camera in an existing persistent session
    #[command(
        after_help = "Examples:\n  capturefab discover                       List camera IDs\n  capturefab connect sim:0                  Connect the test camera\n  capturefab --camera 'avfoundation:HD Pro Webcam C920' connect\n  capturefab connect C920                   A unique model or name match also works"
    )]
    Connect {
        /// Camera ID, serial, IP, media URI, hub:NAME/CAMERA or sim:0 (default: --camera / CAPTUREFAB_CAMERA)
        #[arg(value_name = "CAMERA")]
        target: Option<String>,
    },
    /// Set the session's default camera for CLI calls without --camera
    Select {
        /// Connected camera to make the default (default: --camera / CAPTUREFAB_CAMERA)
        #[arg(value_name = "CAMERA")]
        target: Option<String>,
    },
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
    #[command(
        visible_alias = "record",
        after_help = "Examples:\n  capturefab -c sim:0 record -o clip.mp4 --duration 30s\n  capturefab -c sim:0 record -o clip.mkv --codec h265 --fps 15 --duration 5m\n  capturefab -c sim:0 forward -o rtsp://localhost:8554/cam\n  capturefab --session bench record -o clip.mp4 --duration 1m --on-exists number"
    )]
    Forward {
        /// Local file (.mp4, .mov, .mkv, .ts, or .webm with av1) or a stream URL (rtsp://, srt://, rtmp://, udp://)
        #[arg(short = 'o', long)]
        output: String,
        /// Video codec: h264, h265/hevc or av1
        #[arg(long, default_value = "h264")]
        codec: String,
        /// FFmpeg encoder name (e.g. libx264, h264_nvenc), or auto to probe available encoders
        #[arg(long, default_value = "auto")]
        encoder: String,
        /// Output frame rate
        #[arg(long, default_value_t = 30.0)]
        fps: f64,
        /// Target bitrate, e.g. 4M or 800k
        #[arg(long, default_value = "4M")]
        bitrate: String,
        /// Stop after this duration, e.g. 30s or 5m; the session ends the recording itself and the CLI waits for it. Omitted runs until Ctrl-C in direct mode
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
    #[command(
        after_help = "Examples:\n  capturefab -c sim:0 capture -o shot.png\n  capturefab -c sim:0 capture -n 50 --interval 2s -o 'line1/{camera}-{seq}.jpg'\n  capturefab -c sim:0 capture -n 10 -o frames/            A folder of frame-NNNNNN.png files\n  capturefab --direct -c sim:0 capture -o - > frame.png   One image to a pipe or file"
    )]
    Capture {
        /// File, folder (an existing one, or a name ending in /), or template with {camera} {date} {time} {datetime} {seq} {frame} {tag:KEY}; a name without an extension gets the format's; '-' writes one image to stdout
        #[arg(short = 'o', long)]
        output: String,
        /// Number of frames (1-100000). With more than 1, give a folder (frames/ holds frame-NNNNNN files) or a template with {seq} or {frame}; one image name such as shot.jpg is refused
        #[arg(short='n',long,default_value_t=1,value_parser=clap::value_parser!(u32).range(1..=100_000))]
        count: u32,
        /// Image format; inferred from the file or template extension, otherwise PNG. An image extension in the name must match it
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
    },
    /// List capture schedules in a persistent session
    Jobs,
    /// Cancel a scheduled capture job on the selected camera
    Cancel {
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        id: u64,
    },
    /// Manage saved folders on this computer, removable disks and mounted drives
    #[command(visible_alias = "destinations")]
    Destination {
        #[command(subcommand)]
        command: DestinationCommand,
    },
    /// Save Home Assistant, Frigate, go2rtc and UniFi Protect servers and open their cameras as hub:NAME/CAMERA
    #[command(visible_alias = "hubs")]
    Hub {
        #[command(subcommand)]
        command: crate::hubs::command::HubCommand,
    },
    /// Save user names and passwords for rtsp://, rtsps:// and ONVIF cameras in the OS credential store
    #[command(visible_alias = "logins")]
    Login {
        #[command(subcommand)]
        command: login::LoginCommand,
    },
    /// List mounted external, removable and network volumes (macOS and Linux omit read-only ones)
    Volumes,
    /// Manage CPU and camera-buffer limits; show current resource headroom
    Resources {
        #[command(subcommand)]
        command: Option<ResourcesCommand>,
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
    #[command(
        after_help = "Examples:\n  capturefab serve --name bench --camera sim:0      Keep a camera open for other commands\n  capturefab --session bench capture -o shot.png    Use it from another terminal"
    )]
    Serve {
        /// Session name other commands pass to --session
        #[arg(long, default_value = "bench")]
        name: String,
        /// Start live acquisition as soon as the camera connects
        #[arg(long)]
        stream: bool,
        #[command(flatten)]
        auto: AutoArgs,
    },
    /// List local sessions
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
    /// Check this computer (efficiency mode, session and state folders, GigE and USB limits, storage budget) and print a fix under each warning; --json adds interfaces and compiled capabilities
    Doctor,
    /// Print this build's version and what the last update check found; --check asks capturefab.com now
    Version {
        /// Read the public release list now (one GET of capturefab.com/releases.json)
        #[arg(long)]
        check: bool,
    },
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
                        SessionCommand::connect(selector.clone(), cli.timeout_ms),
                        request_timeout(cli, &SessionCommand::Status)?,
                        Some(&c.actor),
                    )?;
                    note_loose_match(cli, selector, &result);
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
            let result = c.call(SessionCommand::connect(selector.clone(), cli.timeout_ms))?;
            note_loose_match(cli, selector, &result);
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

/// Say on stderr which camera a loose name such as "C920" opened.
fn note_loose_match(cli: &Cli, selector: &str, result: &Value) {
    let connected = &result["connected"];
    let Some(id) = connected["id"].as_str() else {
        return;
    };
    if cli.json
        || crate::camera::is_explicit_selector(selector)
        || [
            &connected["id"],
            &connected["serial"],
            &connected["address"],
        ]
        .iter()
        .any(|value| value.as_str() == Some(selector))
    {
        return;
    }
    eprintln!(
        "capturefab: using {} for '{}'",
        crate::media::redact_url(id),
        crate::media::redact_url(selector)
    );
}

fn request_timeout(cli: &Cli, command: &SessionCommand) -> Result<Duration> {
    Ok(match &cli.request_timeout {
        Some(timeout) => Duration::from_millis(crate::scheduling::parse_duration(timeout)?),
        None => crate::session::default_request_timeout(command),
    })
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
    matches!(command, Some(Command::Setup { command }) if setup::needs_camera(command))
        || matches!(
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
            )
        )
}

/// Never guess among running sessions. Discovery uses an authenticated IPC
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
pub(crate) struct InvalidInput(pub(crate) String);
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

/// Refuse to dump image bytes into an interactive terminal, as curl does.
fn binary_stdout_allowed(output: &str, stdout_is_terminal: bool) -> Result<()> {
    ensure!(
        output != "-" || !stdout_is_terminal,
        "refusing to write a binary image to the terminal; redirect it (capture -o - > frame.png), pipe it, or use -o frame.png"
    );
    Ok(())
}

/// The camera `connect`/`select` name: their argument, else --camera or
/// CAPTUREFAB_CAMERA. Two different cameras are a usage error.
fn connect_target(cli: &Cli, target: &Option<String>) -> Result<String> {
    match (target, &cli.camera) {
        (Some(target), Some(camera)) => {
            ensure!(
                target == camera,
                "two cameras given ({} and --camera {}); name one",
                crate::media::redact_url(target),
                crate::media::redact_url(camera)
            );
            Ok(target.clone())
        }
        (Some(camera), None) | (None, Some(camera)) => Ok(camera.clone()),
        (None, None) => anyhow::bail!(
            "{} needs a camera: capturefab {0} ID (find IDs with capturefab discover)",
            if matches!(cli.command, Some(Command::Select { .. })) {
                "select"
            } else {
                "connect"
            }
        ),
    }
}

/// Prepare all command arguments before connecting or applying --set/--auto.
fn prepare(cli: &Cli) -> Result<Option<SessionCommand>> {
    cli.session_actor()?.validate()?;
    crate::media::validate_hwaccel(&cli.hwaccel)?;
    if let Some(camera) = &cli.camera {
        crate::session::validate_selector(camera)?;
        if cli.session.is_none() {
            SessionCommand::connect(camera.clone(), cli.timeout_ms).validate()?;
        }
    }
    if let Some(session) = &cli.session {
        ipc::validate_name(session)?;
    }
    crate::session::validate_timeout(cli.timeout_ms)?;
    let command = match &cli.command {
        Some(Command::Activity { after, wait }) => Some(SessionCommand::Activity {
            after: *after,
            timeout_ms: wait
                .as_deref()
                .map(crate::scheduling::parse_duration)
                .transpose()?
                .unwrap_or(0),
        }),
        Some(Command::Resources { command }) => {
            resources::prepare(command.as_ref())?;
            None
        }
        Some(Command::Hub { command }) => {
            crate::hubs::command::prepare(command)?;
            None
        }
        Some(Command::Login { command }) => {
            login::prepare(command)?;
            None
        }
        Some(Command::Setup { command }) => {
            setup::prepare(command)?;
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
                DestinationCommand::Remove { name, .. } | DestinationCommand::Test { name, .. } => {
                    Some(name)
                }
            };
            if let Some(name) = name {
                crate::destination::validate_name(name)?;
            }
            None
        }
        Some(Command::Serve { name, .. }) => {
            ipc::validate_name(name)?;
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
        Some(Command::Connect { target }) => Some(SessionCommand::connect(
            connect_target(cli, target)?,
            cli.timeout_ms,
        )),
        Some(Command::Select { target }) => Some(SessionCommand::Select {
            camera: connect_target(cli, target)?,
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
            {
                use std::io::IsTerminal;
                binary_stdout_allowed(output, io::stdout().is_terminal())?;
            }
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
            // The session ends a timed recording itself; the CLI waits on it.
            let duration_ms = duration
                .as_deref()
                .map(crate::scheduling::parse_duration)
                .transpose()?;
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
                duration_ms,
            })
        }
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
    // Check names as typed, so the advice repeats the person's own spelling.
    if let (
        Some(
            SessionCommand::Capture {
                output,
                count,
                format,
                ..
            }
            | SessionCommand::Schedule {
                output,
                count,
                format,
                ..
            },
        ),
        Some(Command::Capture { output: typed, .. }),
    ) = (command.as_ref(), &cli.command)
        && output != "-"
    {
        crate::engine::check_capture_name(typed, *count, format)?;
    }
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
        // A taken recording file fails before any "Recording to" line.
        if let SessionCommand::Forward {
            output,
            destination,
            on_exists,
            ..
        } = command
        {
            crate::engine::preflight_recording_output(output, destination.as_deref(), *on_exists)?;
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
    // Hub management never targets a camera. Ignore even a malformed stale
    // environment selector before path resolution and shape validation.
    if matches!(
        cli.command,
        Some(Command::Hub { .. } | Command::Login { .. })
    ) {
        cli.camera = None;
    }
    if cli.command.is_none() {
        // A bare `capturefab` explains itself; --json needs a command so
        // stdout stays one result document.
        input((|| {
            ensure!(
                !cli.json,
                "choose a command when using --json; see capturefab --help"
            );
            Ok(())
        })())?;
        Cli::command().print_long_help()?;
        return Ok(());
    }
    let base = std::env::current_dir()?;
    if let Some(camera) = &mut cli.camera {
        input(paths::resolve_selector(camera, &base))?;
    }
    if let Some(Command::Connect { target } | Command::Select { target }) = &mut cli.command
        && let Some(camera) = target
    {
        input(paths::resolve_selector(camera, &base))?;
    }
    let mut prepared = input(prepare(&cli))?;
    resolve_session(&mut cli)?;
    // Hub management does not target a camera.
    if cli.session.is_none()
        && !matches!(
            cli.command,
            Some(Command::Hub { .. } | Command::Login { .. })
        )
        && let Some(camera) = &cli.camera
    {
        crate::camera::validate_selector(camera)?;
    }
    prepare_outputs(&cli, &mut prepared)?;
    input(crate::media::configure_hwaccel(&cli.hwaccel))?;
    crate::efficiency::configure(cli.efficiency);
    match &cli.command {
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
        Some(Command::Doctor) => return output(&cli, doctor()),
        Some(Command::Version { check }) => {
            return output(&cli, crate::updates::version_command(*check)?);
        }
        Some(Command::Resources { command }) => {
            return output(&cli, resources::run(command.as_ref())?);
        }
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
        Some(Command::Setup { command }) if !setup::needs_camera(command) => {
            return output(&cli, setup::run_store(command)?);
        }
        Some(Command::Hub { command }) => {
            let result = crate::hubs::command::run(
                command,
                std::time::Duration::from_millis(cli.timeout_ms),
            )?;
            return output(&cli, result);
        }
        Some(Command::Login { command }) => {
            let result = login::run(
                command,
                cli.session.as_deref(),
                std::time::Duration::from_millis(cli.timeout_ms),
            )?;
            return output(&cli, result);
        }
        Some(Command::Volumes) => {
            return output(&cli, json!({"volumes": crate::volumes::list()}));
        }
        Some(Command::Compatibility { model }) => {
            return output(&cli, crate::compatibility::guide(model.as_deref()));
        }
        Some(Command::Sessions) => {
            let sessions = ipc::list()?;
            if let Some((dir, names)) = ipc::sessions_elsewhere() {
                eprintln!(
                    "capturefab: these sessions run from {}, where a service started without XDG_RUNTIME_DIR keeps them and this login does not look: {}. `capturefab doctor` shows the fix under runtime_dir.",
                    dir.display(),
                    names.join(", ")
                );
            }
            return output(&cli, sessions);
        }
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
        Some(Command::Serve { name, stream, auto }) => {
            ensure!(
                cli.session.is_none(),
                "serve creates a new session; omit --session"
            );
            let client = Client::new(&cli)?;
            let handle = client.local.as_ref().unwrap();
            if *stream {
                client.call(SessionCommand::Start)?;
            }
            auto.apply(&client)?;
            let _server =
                ipc::Server::start_with_kind(handle.clone(), name, ipc::SessionKind::Serve)?;
            // Ctrl-C, SIGTERM from `systemctl stop` or SIGHUP from a closed
            // terminal; in place before serve says it is ready, so a signal
            // sent once it is always finds the handler.
            let (stop, stopped) = std::sync::mpsc::channel();
            let h = handle.clone();
            on_quit_signal(move || {
                let _ = stop.send(());
                h.shutdown();
            })?;
            output(
                &cli,
                json!({"session":name,"pid":std::process::id(),"ready":true}),
            )?;
            // After the handler, so Ctrl-C during the checks still cleans up.
            if !cli.json {
                crate::doctor::print_warnings();
            }
            // Blocks until a signal rather than waking to check for it.
            let _ = stopped.recv();
            // Wait for recordings to finish saving.
            handle.shutdown();
            return Ok(());
        }
        _ => {}
    }
    if let Some(Command::Setup { command }) = &cli.command {
        setup::check_saved(command)?;
    }
    let persistent = matches!(
        &cli.command,
        Some(
            Command::Connect { .. }
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
        )
    );
    input((|| {
        ensure!(
            !persistent || cli.session.is_some(),
            "this command needs a running session; start `capturefab serve --name NAME`, then choose --session NAME"
        );
        Ok(())
    })())?;
    let camera_required = matches!(&cli.command, Some(Command::Setup { .. }))
        || matches!(
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
        on_quit_signal(move || h.shutdown())?;
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
        Command::Connect { .. } => {
            let command = prepared.clone().unwrap();
            let result = client.call(command.clone())?;
            if let SessionCommand::Connect { camera, .. } = &command {
                note_loose_match(&cli, camera, &result);
            }
            result
        }
        Command::Select { .. } => client.call(prepared.clone().unwrap())?,
        Command::Forward {
            output: destination,
            duration,
            destination: _,
            auto,
            ..
        } => {
            let duration_ms = duration
                .as_deref()
                .map(crate::scheduling::parse_duration)
                .transpose()?;
            auto.apply(&client)?;
            // The camera's newest error before the recording: one after it
            // means the recording failed, even before a status shows it.
            let errors = match (duration_ms, &client.local) {
                (None, _) => ErrorMark::default(),
                (Some(_), Some(h)) => ErrorMark::of(&ForwardStatus::of_snapshot(&h.snapshot())),
                (Some(_), None) => ErrorMark::of(&ForwardStatus::of_status(
                    &client.call(SessionCommand::Status)?,
                )),
            };
            let mut result = client.call(prepared.clone().unwrap())?;
            // The session ends a timed recording at its end time; the CLI
            // waits for that, and stops it only on Ctrl-C or as a fallback.
            let mut timed = duration_ms
                .map(|ms| TimedRecording::new(ms, &result, errors))
                .transpose()?;
            let shown = result["path"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::media::redact_url(destination));
            if cli.session.is_none() {
                if !cli.json {
                    eprintln!(
                        "{}",
                        summary::forward_banner(destination, &shown, duration_ms)
                    );
                }
                let epoch = std::time::Instant::now();
                let h = client.local.as_ref().unwrap();
                loop {
                    std::thread::sleep(check_in(
                        timed
                            .as_ref()
                            .map(|timed| timed.duration.saturating_sub(epoch.elapsed())),
                    ));
                    if h.is_shutting_down() {
                        return Ok(());
                    }
                    let state = h.stored();
                    let next = match &mut timed {
                        Some(timed) => timed.next(
                            ForwardStatus::of_snapshot(&state),
                            crate::scheduling::now_ms(),
                            epoch.elapsed(),
                            false,
                        ),
                        None if state.forwarding.is_some() => RecordingWait::Wait,
                        None => RecordingWait::Failed,
                    };
                    match next {
                        RecordingWait::Wait => {}
                        RecordingWait::Finished => {
                            result["stopped"] = timed
                                .as_ref()
                                .unwrap()
                                .finished(&serde_json::to_value(&state.forwarding_progress)?);
                            break;
                        }
                        RecordingWait::Stop => {
                            result["stopped"] =
                                client.call(timed.as_ref().unwrap().stop_command())?;
                            break;
                        }
                        RecordingWait::Failed => anyhow::bail!(
                            "{}",
                            state
                                .last_error
                                .unwrap_or_else(|| "forwarding stopped".into())
                        ),
                    }
                }

                output(&cli, result)?;
                return Ok(());
            }
            if let Some(mut timed) = timed {
                let stopped = Arc::new(AtomicBool::new(false));
                let flag = stopped.clone();
                on_quit_signal(move || flag.store(true, Ordering::Relaxed))?;
                if !cli.json {
                    eprintln!(
                        "{}",
                        summary::forward_banner(destination, &shown, duration_ms)
                    );
                }
                let epoch = std::time::Instant::now();
                loop {
                    std::thread::sleep(check_in(Some(
                        timed.duration.saturating_sub(epoch.elapsed()),
                    )));
                    let state = client.call(SessionCommand::Status)?;
                    match timed.next(
                        ForwardStatus::of_status(&state),
                        crate::scheduling::now_ms(),
                        epoch.elapsed(),
                        stopped.load(Ordering::Relaxed),
                    ) {
                        RecordingWait::Wait => {}
                        // Never stop-forward here: another participant may
                        // already record on this camera.
                        RecordingWait::Finished => {
                            result["stopped"] = timed.finished(&state["forwarding_progress"]);
                            break;
                        }
                        RecordingWait::Stop => {
                            result["stopped"] = client.call(timed.stop_command())?;
                            break;
                        }
                        RecordingWait::Failed => anyhow::bail!(
                            "{}",
                            state["last_error"]
                                .as_str()
                                .unwrap_or("forwarding stopped before its duration elapsed")
                        ),
                    }
                }
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
        Command::Setup { command } => setup::run_camera(&cli, &client, command)?,
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
        Command::Activity { .. } => client.call(prepared.clone().unwrap())?,
        Command::Capture {
            output: destination,
            format,
            count,
            settings,
            destination: _,
            auto,
            ..
        } => {
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
                        "Capture job {id} scheduled: {} frames every {}. Keep this process running; Ctrl-C cancels.",
                        result["job"]["count"],
                        summary::interval(result["job"]["interval_ms"].as_u64().unwrap_or(0))
                    );
                }
                let h = client.local.as_ref().unwrap();
                let mut missing: Option<std::time::Instant> = None;
                let mut shown = 0;
                loop {
                    if h.is_shutting_down() {
                        return Ok(());
                    }
                    // The status poller keeps the stored jobs current.
                    let state = h.stored();
                    let Some(job) = state.jobs.iter().find(|job| job.id == id) else {
                        let since = *missing.get_or_insert_with(std::time::Instant::now);
                        anyhow::ensure!(
                            since.elapsed() < Duration::from_secs(5),
                            "capture job disappeared"
                        );
                        std::thread::sleep(check_in(Some(Duration::ZERO)));
                        continue;
                    };
                    missing = None;
                    if !cli.json && job.captured > shown {
                        // The poller can miss a frame when the interval is
                        // shorter than its period; still count each one.
                        for missed in shown + 1..job.captured {
                            eprintln!("{missed}/{} saved", job.count);
                        }
                        shown = job.captured;
                        eprintln!(
                            "{}",
                            summary::job_progress(&json!(job), crate::scheduling::now_ms())
                        );
                    }
                    match job.status.as_str() {
                        "complete" => {
                            let result = json!({"job":job});
                            return output(&cli, result);
                        }
                        "failed" => anyhow::bail!(
                            "{}",
                            job.error.as_deref().unwrap_or("scheduled capture failed")
                        ),
                        "cancelled" => anyhow::bail!("capture job cancelled"),
                        _ => {}
                    }
                    let next = job.next_at_ms.saturating_sub(crate::scheduling::now_ms());
                    std::thread::sleep(check_in(Some(Duration::from_millis(next))));
                }
            }
            let result = client.call(prepared.clone().unwrap())?;
            if destination == "-" {
                let frame = client.local.as_ref().unwrap().captured_frame()?;
                io::stdout().lock().write_all(&crate::frame::encode(
                    &frame,
                    capture_format(*format, destination, *count)?.name(),
                )?)?;
                return Ok(());
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
                input(paths::resolve_selector(camera, &std::env::current_dir()?))?;
            }
            input(caller_output(&mut command))?;
            client.call(command)?
        }
        _ => unreachable!(),
    };
    output(&cli, result)
}

/// The `--json` error for an argument-parsing failure: clap's first line
/// as the message, and what an agent needs to fix it as `details`.
pub fn clap_error_envelope(error: &clap::Error) -> Value {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let rendered = error.render().to_string();
    let message = rendered
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("invalid usage");
    let message = message.strip_prefix("error: ").unwrap_or(message);
    let kind = match error.kind() {
        ErrorKind::InvalidValue => "invalid_value".to_owned(),
        ErrorKind::UnknownArgument => "unknown_argument".to_owned(),
        ErrorKind::InvalidSubcommand => "invalid_subcommand".to_owned(),
        ErrorKind::NoEquals => "no_equals".to_owned(),
        ErrorKind::ValueValidation => "value_validation".to_owned(),
        ErrorKind::TooManyValues => "too_many_values".to_owned(),
        ErrorKind::TooFewValues => "too_few_values".to_owned(),
        ErrorKind::WrongNumberOfValues => "wrong_number_of_values".to_owned(),
        ErrorKind::ArgumentConflict => "argument_conflict".to_owned(),
        ErrorKind::MissingRequiredArgument => "missing_required_argument".to_owned(),
        ErrorKind::MissingSubcommand => "missing_subcommand".to_owned(),
        ErrorKind::InvalidUtf8 => "invalid_utf8".to_owned(),
        other => {
            let mut name = String::new();
            for (index, c) in format!("{other:?}").chars().enumerate() {
                if c.is_ascii_uppercase() && index > 0 {
                    name.push('_');
                }
                name.push(c.to_ascii_lowercase());
            }
            name
        }
    };
    let strings = |kind: ContextKind| -> Vec<String> {
        match error.get(kind) {
            Some(ContextValue::String(value)) => vec![value.clone()],
            Some(ContextValue::Strings(values)) => values.clone(),
            Some(ContextValue::StyledStr(value)) => vec![value.to_string()],
            Some(ContextValue::StyledStrs(values)) => {
                values.iter().map(ToString::to_string).collect()
            }
            _ => Vec::new(),
        }
    };
    let mut details = json!({"kind": kind});
    // clap lists the closest match last; agents read the first.
    let mut suggestions = strings(ContextKind::SuggestedSubcommand);
    suggestions.extend(strings(ContextKind::SuggestedArg));
    suggestions.extend(strings(ContextKind::SuggestedValue));
    suggestions.reverse();
    if !suggestions.is_empty() {
        details["suggestions"] = json!(suggestions);
    }
    let invalid = [
        ContextKind::InvalidSubcommand,
        ContextKind::InvalidValue,
        ContextKind::InvalidArg,
    ]
    .into_iter()
    .flat_map(strings)
    .find(|value| !value.is_empty());
    if let Some(invalid) = invalid {
        details["invalid"] = json!(invalid);
    }
    let valid = strings(ContextKind::ValidValue);
    if !valid.is_empty() {
        details["valid_values"] = json!(valid);
    }
    if let Some(usage) = strings(ContextKind::Usage).first() {
        let usage = usage.trim();
        details["usage"] = json!(usage.strip_prefix("Usage: ").unwrap_or(usage).trim());
    }
    json!({"version":1,"ok":false,"error":{"code":"usage","message":message,"details":details}})
}

pub fn write_json(value: &Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}
fn output(cli: &Cli, value: Value) -> Result<()> {
    if cli.json {
        let mut envelope = json!({"version":1,"ok":true,"result":value});
        if uses_session(cli.command.as_ref())
            && let Some(session) = &cli.session
        {
            envelope["session"] = json!(session);
        }
        return write_json(&envelope);
    }
    if matches!(&cli.command, Some(Command::Resources { .. })) {
        io::stdout().write_all(resources::human(&value).as_bytes())?;
        return Ok(());
    }
    if let Some(Command::Setup { command }) = &cli.command {
        return setup::print_human(cli, command, &value);
    }
    let mut out = io::stdout().lock();
    if matches!(&cli.command, Some(Command::Doctor)) {
        crate::doctor::print_checklist(&mut out, &value)?;
    } else if matches!(&cli.command, Some(Command::Version { .. })) {
        write!(out, "{}", crate::updates::human(&value))?;
    } else if let Some(Command::Serve { name, .. }) = &cli.command {
        writeln!(
            out,
            "Session {name} is ready (pid {}). Try: capturefab --session {name} discover",
            value["pid"]
        )?;
        writeln!(out, "Ctrl-C stops the session.")?;
    } else if let Some(Command::Login { command }) = &cli.command {
        write!(out, "{}", login::human(command, &value))?;
    } else if let Some(Command::Hub { command }) = &cli.command {
        for warning in value["warnings"].as_array().into_iter().flatten() {
            eprintln!("capturefab: {}", warning.as_str().unwrap_or_default());
        }
        write!(out, "{}", crate::hubs::command::human(command, &value))?;
    } else if matches!(&cli.command, Some(Command::Undo | Command::Redo)) {
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
                for line in detail.lines() {
                    writeln!(out, "  {line}")?;
                }
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
                    "No cameras connected. Use capturefab connect ID, or add --camera ID to capture/record commands."
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
                "No sessions running. Start one with capturefab serve --name NAME."
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
    } else if value["files"].is_array() {
        write!(out, "{}", summary::capture_summary(&value))?;
    } else if matches!(&cli.command, Some(Command::Capture { .. })) && value["job"].is_object() {
        let job = &value["job"];
        match &cli.session {
            Some(session) if job["status"] != "complete" => write!(
                out,
                "{}",
                summary::job_scheduled(job, cli.camera.as_deref(), session)
            )?,
            _ => write!(out, "{}", summary::job_finished(job))?,
        }
    } else if matches!(&cli.command, Some(Command::Jobs)) {
        let jobs = value["jobs"].as_array().cloned().unwrap_or_default();
        write!(
            out,
            "{}",
            summary::jobs_table(&jobs, cli.session.as_deref(), crate::scheduling::now_ms())
        )?;
    } else if matches!(&cli.command, Some(Command::Cancel { .. })) {
        writeln!(out, "Cancelled capture job {}.", value["cancelled"])?;
    } else if matches!(
        &cli.command,
        Some(Command::Storage {
            command: None | Some(StorageCommand::Status | StorageCommand::Configure { .. })
        })
    ) {
        write!(out, "{}", summary::storage_summary(&value))?;
    } else if let Some(Command::Forward { output: target, .. }) = &cli.command {
        if value["stopped"].is_object() {
            let (text, warnings) =
                summary::forward_summary(target, &value, crate::scheduling::now_ms());
            write!(out, "{text}")?;
            out.flush()?;
            for warning in warnings {
                eprintln!("capturefab: {warning}");
            }
        } else {
            write!(
                out,
                "{}",
                summary::forward_started(
                    target,
                    &value,
                    cli.session.as_deref().unwrap_or("temporary"),
                    cli.camera.as_deref()
                )
            )?;
        }
    } else if let Some(devices) = value["devices"].as_array() {
        let kind = match &cli.command {
            Some(Command::Native) => human::DeviceList::Native,
            Some(Command::Scanners) => human::DeviceList::Scanners,
            Some(Command::Onvif { .. }) => human::DeviceList::Onvif,
            _ => human::DeviceList::Discover,
        };
        human::write_device_table(&mut out, devices, kind)?;
        out.flush()?;
        if let Some(w) = value["warnings"].as_array() {
            for w in w {
                eprintln!("capturefab: {}", w.as_str().unwrap_or(""));
            }
        }
        for hint in human::next_steps(devices, kind) {
            eprintln!("{hint}");
        }
    } else if let Some(scan) = value
        .get("network_scan")
        .filter(|_| matches!(&cli.command, Some(Command::NetworkScan)))
        .and_then(|scan| serde_json::from_value::<crate::network::NetworkScan>(scan.clone()).ok())
    {
        crate::network::write_human(&scan, &mut out)?;
        out.flush()?;
        for warning in &scan.warnings {
            eprintln!("capturefab: {warning}");
        }
    } else if matches!(&cli.command, Some(Command::Connect { .. }))
        && let Some(id) = value["connected"]["id"].as_str()
    {
        let connected = &value["connected"];
        let id = crate::media::redact_url(id);
        let serial = connected["serial"]
            .as_str()
            .filter(|serial| !serial.is_empty() && *serial != connected["id"])
            .map(|serial| format!(", S/N {serial}"))
            .unwrap_or_default();
        let session = cli
            .session
            .as_deref()
            .map(|name| format!(" in session {name}"))
            .unwrap_or_default();
        writeln!(
            out,
            "Connected {} ({id}{serial}){session}.",
            connected["model"].as_str().unwrap_or("camera")
        )?;
        // Repeat the session the person named, so the hint reaches it even
        // when several sessions are running.
        let target = format!(
            "{}--camera {}",
            cli.session
                .as_deref()
                .map(|name| format!("--session {} ", crate::hint::shell_quote(name)))
                .unwrap_or_default(),
            crate::hint::shell_quote(&id)
        );
        writeln!(
            out,
            "Next: capturefab {target} capture -o 'shot-{{seq}}.png'   (or start live acquisition: capturefab {target} start)"
        )?;
    } else if matches!(&cli.command, Some(Command::Select { .. })) {
        writeln!(
            out,
            "CLI calls without --camera now use {}.",
            crate::media::redact_url(value["active_camera"].as_str().unwrap_or("no camera"))
        )?;
    } else if matches!(&cli.command, Some(Command::Disconnect)) {
        writeln!(out, "Disconnected.")?;
    } else if let Some(features) = value["features"].as_array() {
        if let Some(camera) = value.get("camera").filter(|camera| camera.is_object()) {
            write!(out, "{}", summary::info_header(camera))?;
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
    let mut report = build_report();
    crate::doctor::add_host(&mut report);
    report
}
/// What this build supports and where it runs; `doctor` adds the host checks.
fn build_report() -> Value {
    let interfaces=if_addrs::get_if_addrs().map(|items|items.into_iter().filter_map(|i|match i.addr{if_addrs::IfAddr::V4(a)=>Some(json!({"name":i.name,"ip":a.ip,"netmask":a.netmask,"broadcast":a.broadcast})),_=>None}).collect::<Vec<_>>()).unwrap_or_default();
    let mut value = json!({"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"features":{"usb":cfg!(feature="usb"),"scanner":cfg!(feature="scanner"),"jpeg":cfg!(feature="jpeg"),"nvjpeg":cfg!(feature="nvjpeg"),"vaapi":cfg!(feature="vaapi"),"videotoolbox":cfg!(feature="videotoolbox"),"hubs":cfg!(feature="hubs"),"keychain":cfg!(feature="keychain")},"scanners":crate::scanner::doctor(),"jpeg":jpeg_backends(),"runtime_directory":ipc::runtime_dir(),"state_directory":ipc::state_dir(),"cache_directory":ipc::cache_dir(),"ring_backing":crate::shared_memory::backing(),"interfaces":interfaces,"runtime":"No Aravis, libusb or vendor SDK required. Native OS USB drivers required; NVIDIA CUDA and nvJPEG, and libva with a JPEG-capable driver, are used when installed; Apple silicon uses its JPEG engine through VideoToolbox. Document scanning uses installed platform scanner drivers (SANE scanimage on Linux).","usb_access":if cfg!(target_os="windows"){"USB3 camera interfaces must use WinUSB"}else if cfg!(target_os="linux"){"Read/write permission on camera /dev/bus/usb node required"}else{"IOKit camera interface must be available to userspace"}});
    value["features"]["updates"] = json!(cfg!(feature = "updates"));
    value["update_check"] = crate::updates::report(
        &crate::updates::Settings::load(&ipc::state_dir()),
        &crate::updates::policy(),
    );
    value
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
    let mut contract = json!({"version":ipc::VERSION,"program":"capturefab","default":"help","json_result":{"version":1,"ok":true,"result":"command-specific JSON"},"json_error":{"version":1,"ok":false,"error":{"code":"category","message":"description"}},"exit_codes":{"0":"success (also closed stdout pipe)","1":"operation or I/O failure","2":"invalid CLI usage","3":"camera/session unavailable","4":"timeout","5":"unsupported feature/format"},"selection":{"direct":"--camera ID|serial|IPv4|sim:0","existing_session":"--session NAME","environment":"CAPTUREFAB_SESSION, CAPTUREFAB_SESSION_DIR"},"limits":{"timeout_ms":[1,60000],"count":[1,100000],"rpc_bytes":1048576,"payload_bytes":268435456},"rpc":{"transport":"authenticated loopback TCP; one newline-delimited request/response per connection","client":"capturefab --session NAME rpc < command.json","command_tag":"op","commands":[{"op":"status"},{"op":"discover","timeout_ms":1000,"simulated":false},{"op":"connect","camera":"sim:0","timeout_ms":2000},{"op":"disconnect"},{"op":"features"},{"op":"get","feature":"Width"},{"op":"set","feature":"ExposureTime","value":"5000"},{"op":"execute","feature":"TriggerSoftware"},{"op":"start"},{"op":"stop"},{"op":"capture","output":"frames","count":10,"timeout_ms":2000,"format":"png"},{"op":"capture","output":"run-1","count":10,"timeout_ms":2000,"format":"png","destination":"archive"},{"op":"xml"},{"op":"read_memory","address":256,"length":4},{"op":"write_memory","address":256,"data":[0,0,2,128]}]},"capture":{"formats":["png","jpeg","raw","pgm","ppm"],"stdout":"--camera ... capture -n 1 -o - --format raw; no --json; stdout must be a pipe or file, not a terminal","destination":"--destination NAME (or \"destination\" in capture/schedule/forward RPC): -o is a relative name inside a saved folder; see destination list and volumes"},"writes":"Multiple set assignments apply sequentially; failures may leave earlier assignments applied. Session operations are serialized per camera."});
    contract["exit_codes"]["6"] = json!("storage quota or disk full");
    contract["exit_codes"]["1"] = json!(
        "operation or I/O failure; error.code operation_failed, auth_required or auth_rejected"
    );
    contract["error_codes"] = json!({
        "auth_required": "exit 1: a camera asked for a user name and password and none was sent; run `capturefab login add ADDRESS --user USER --password-stdin`",
        "auth_rejected": "exit 1: a camera refused the user name and password that were sent",
        "note": "hub credential failures stay operation_failed and set the hub's rejected status; see `hub list`",
    });
    contract["rpc"]["connect_sign_in"] = json!({
        "login": {"user": "USER", "password": "PASSWORD"},
        "saved_login": "default true: the worker may use a sign-in saved with `login add`",
        "note": "local IPC only",
    });
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
        "CAPTUREFAB_CACHE_DIR",
        "CAPTUREFAB_HWACCEL",
        "CAPTUREFAB_EFFICIENCY",
        "CAPTUREFAB_ACTOR",
        "CAPTUREFAB_ACTOR_KIND"
    ]);
    contract["selection"]["direct_mode"] = json!(
        "--direct --camera ID bypasses automatic session attachment; --direct conflicts with explicit --session and overrides CAPTUREFAB_SESSION"
    );
    contract["capture"]["format_default"] = json!(
        "Known single file/template extension infers png/jpeg/raw/pgm/ppm; sequences to directories and stdout default to png; --format overrides inference, but an explicit format that differs from a png/jpg/jpeg/raw/pgm/ppm extension in the name is a usage error (exit 2; jpg and jpeg are the same)"
    );
    contract["capture"]["path_base"] = json!(
        "CLI capture/record/RPC local output paths resolve in the calling terminal; named destination paths remain relative to the destination"
    );
    contract["capture"]["paths"] = json!(
        "file, directory (an existing one or a name ending in a separator; continues frame-NNNNNN numbering after the highest existing number) or template; a file name or template whose last component has no extension gets the format's (.png, .jpg, .raw, .pgm, .ppm); with -n above 1 the CLI refuses a name ending in an image extension that is not an existing directory (usage, exit 2: use a template with {seq} or {frame}, or a folder), and an explicit --format contradicting an image extension; RPC capture steps instead save such a sequence into a folder of that name and keep the given name; {seq} picks the next free number; other existing names fail unless on_exists=number. Files are never overwritten."
    );
    contract["capture"]["placeholders"] = json!({
        "camera":"model-serial, limited to A-Z a-z 0-9 . _ - (at most 64 bytes)",
        "date":"YYYY-MM-DD","time":"HHMMSS","datetime":"YYYYMMDD-HHMMSS",
        "seq":"next free number in that folder for that pattern, 4 or more digits; file name only",
        "frame":"1-based index within the request, 6 digits",
        "tag:KEY":"the capture context value for KEY (--context KEY=VALUE) as one path component: trimmed, / \\ < > : \" | ? * become _, trailing dots and spaces removed, a leading dot or Windows device name prefixed with _, at most 128 bytes; an empty or missing tag is a usage error (exit 2); {seq} cannot directly follow it",
        "schedules":"a schedule keeps the context it was created with, so changing tags later does not move its files",
        "clock":"local time when the frame is requested; wall_ms is UTC milliseconds"
    });
    contract["capture"]["on_exists"] = json!({
        "values":["fail","number"],"default":"fail",
        "number":"an existing name without {seq} becomes stem-0001.ext, stem-0002.ext, ...",
        "applies_to":"capture, schedule and local recordings (record/forward)",
        "taken":"a taken name fails before acquisition or encoding starts (exit 1, operation_failed); the CLI adds error.details.hint with the {seq} template and --on-exists number that would save it"
    });
    contract["capture"]["context"] = json!({
        "cli":"--context KEY=VALUE (repeatable)","keys":"^[a-z][a-z0-9_]{0,31}$",
        "max_entries":crate::context::MAX_ENTRIES,"max_value_bytes":crate::context::MAX_VALUE,
        "duplicate_key":"usage error (exit 2)",
        "recorded":"echoed in the capture result and kept on scheduled jobs",
        "embedded":"written into each saved PNG (iTXt 'capturefab' JSON, and Description as key=value; ...) and JPEG (COM JSON), including note"
    });
    contract["capture"]["metadata"] = json!({
        "png":"iTXt chunk with keyword 'capturefab' holding compact JSON; tags also in iTXt 'Description'",
        "jpeg":"COM segment holding the same JSON",
        "none":["raw","pgm","ppm"],
        "fields":"{capturefab:version,frame_id,timestamp_ns,wall_ms,width,height,pixel_format:name,camera?:{vendor,model},settings?,context?}",
        "settings":crate::engine::ACQUISITION_FEATURES,
        "excludes":"serial numbers, addresses and source URLs",
        "max_bytes":crate::frame::MAX_META,
        "channels":"monochrome PNG and PGM are one 8-bit gray channel; color and Bayer PNG are 8-bit RGB"
    });
    contract["capture"]["result"] = json!({
        "capture":"{files:[path],frames:[{id,width,height,pixel_format,pixel_format_name,timestamp_ns,bytes,file_bytes?,wall_ms,settings?,file?}],count,context?,destination?}; bytes is the raw acquired frame size, file_bytes the saved file size (absent for stdout); settings maps the camera's allowlisted acquisition features to their values. A burst that fails part way keeps its saved frames and its error category; the message starts with saved N of M frames to FOLDER (names)",
        "record":"{forwarding,forwarding_id,since_ms,until_ms?,path?,stopped?}; path is the local recording file; until_ms is the nominal UTC deadline for a timed recording; elapsed time and computer sleep count, and backward clock changes do not extend it; with --duration, stopped is {forwarding:null,by:\"duration\",stats?:{accepted,written,dropped,late,stalled,recording?:{path,bytes,started_ms,ended_ms,encoder}}} when the session ended it on time (stats is absent from a worker that does not report them), else the conditional-stop result",
        "status":"forwarding_id and forwarding_since_ms per camera and at the top level while forwarding, forwarding_until_ms while a timed recording or forward runs, and recording_path (the resolved absolute file; forwarding keeps the requested output) while a local recording runs",
        "conditional_stop":{"rpc":{"op":"stop_forward_if","forwarding_id":"32 lowercase hexadecimal characters"},"behavior":"Stops only the acknowledged forwarding instance. A different current identity is refused without stopping it. Timed CLI fallback uses this form; explicit stop-forward stops the current instance."},
        "stop_forward":"{forwarding:null,stats:{accepted,written,dropped,late,stalled,encoder,error,recording?}|null}; stop_forward_if returns the same shape. Timed CLI fallback or interruption adds the conditional-stop result as stopped; normal timed completion returns {forwarding:null,by:duration,stats?}. late counts frames lost while the encoder was still busy (dropped includes them); stalled means no new camera frame for 2 s",
    });
    contract["status"] = json!({
        "frames":"frames the worker acquired",
        "lost":"frames lost on the camera link (incomplete or missing) or in the worker",
        "preview_skipped":"preview frames the shared ring replaced while a viewer read it; captures and recordings still receive them",
        "dropped":"worker receive errors plus preview_skipped",
        "scope":"top level for the selected camera, and per camera in cameras[]"
    });
    contract["selection"]["source_base"] = json!(
        "Relative file: and sim:replay: inputs, and existing bare media paths, resolve in the calling terminal for CLI/RPC; network/device locators keep their meaning"
    );
    contract["selection"]["addressed_status"] = json!(
        "status --camera retains session context and actual active_camera selection; target_camera identifies the camera whose top-level connected/features/streaming/forwarding/jobs/history fields are returned"
    );
    contract["activity"] = json!({
        "snapshot":"status.activity; status.activity_revision",
        "actor":{"fields":{"id":"stable participant identifier","name":"display name","kind":"human or agent"},"cli":"--actor NAME --actor-kind human|agent; CAPTUREFAB_ACTOR and CAPTUREFAB_ACTOR_KIND", "identity":"Self-reported attribution; not authentication. Unnamed CLI calls use a process identity."},
        "origins":["client"],"states":["queued","running","succeeded","failed"],
        "job":"Schedule acknowledgment is succeeded; entry.job tracks id, worker_pid, status, count, captured, last_file and error through completion/cancellation/failure.",
        "history":"Entries show actor, target, command lifecycle, progress, errors and saved outputs across cameras, and remain after selection/disconnect. History retains 200 completed entries plus active work.",
        "local_updates":"activity --json returns {revision,activity}; activity --after REV --wait 20s --json waits for shared changes through private local IPC, without an HTTP listener. RPC command: {op:activity,after:REV,timeout_ms:20000}.",
        "polling":"Status, activity, get, jobs, features, xml, read_memory and authenticated discovery probes do not create activity entries; targeted status includes all participants without waiting behind captures.",
        "waiting_observers":{"private_ipc":8,"capacity":"Waiting observers reserve capacity for commands; extra waits fail explicitly and can be retried."}
    });
    contract["writes"] = json!(
        "Camera operations are ordered per camera across IPC participants; different cameras run independently. Concurrent scheduled jobs on the same camera share acquisition. Multiple set assignments apply sequentially; failures may leave earlier assignments applied. RPC set_many writes 1..32 {feature, value} pairs in order as one camera change: a failure restores every value it wrote, and success returns one camera_change token that undoes them together."
    );
    contract["jobs"] = json!({
        "in":"schedule result.job; jobs result.jobs; status jobs and cameras[].jobs",
        "fields":["id","status","output","count","captured","skipped","retries","first_at_ms","interval_ms","next_at_ms","last_file","error","destination?","context?"],
        "status":"One of scheduling.states",
        "next_at_ms":"Follows scheduling.interval",
        "saving":"Scheduled frames are encoded and saved on their own thread, which holds at most two frames, so acquisition, previews, recording and forwarding continue meanwhile.",
        "skipped":"Shots not taken because two earlier frames were still being saved. The job takes them at later intervals.",
        "retries":"Shots that failed for a reason that may pass, a busy storage ledger or a disk timeout, and are taken again at the next interval. error holds the message until a later shot saves.",
        "failures":"Storage full and other save errors fail the job."
    });
    contract["logs"] = json!({
        "snapshot":"status.logs",
        "fields":["timestamp","level","message"],
        "timestamp":{"unix_ms":"signed Unix milliseconds (absolute UTC instant)","utc_offset_seconds":"producer offset, seconds east of UTC (-86399..86399)"}
    });
    contract["forwarding"] = json!({"cli":"record|forward -o OUTPUT [--duration 30s] [--on-exists fail|number] [--context KEY=VALUE]...; a local file output accepts the capture placeholders","duration":"Works with direct workers and persistent sessions. The CLI sends duration_ms: the camera worker stops the encoder at since_ms + duration_ms (wall clock), commits the file, and the CLI stays alive until then. Ctrl-C stops it early. A recording that stops early or with a new camera error fails the command with that error. Without duration, persistent session forwarding returns immediately; direct forwarding runs until Ctrl-C.","duration_ms":"RPC forward commands accept duration_ms (1 ms to 10 years); the session ends the recording or stream itself, even after the client that started it exits. Size max_file_bytes for the whole duration: a recording that reaches it stops with storage_full and keeps the video written until then.","request_timeout":"--request-timeout controls a session request deadline, not recording duration; a timed-out request may still complete in the session. Inspect status before retrying writes."});
    contract["forwarding"]["timeline"] = json!(
        "Each new frame goes to FFmpeg once, at most --fps a second, and keeps the time FFmpeg read it, so a recording lasts as long as it ran and a camera stall leaves a gap; streams repeat the last frame at --fps while the camera stalls. A recording whose encoder fails or must be stopped keeps the video it wrote"
    );
    contract["forwarding"]["encoders"] = json!(
        "auto probes encoders at the frame size and remembers working ones for the FFmpeg build until reboot, failures for 5 minutes and timed-out probes not at all; an auto encoder that exits in the first seconds of a local recording, before writing video, is replaced once by the next in a new file; a codec with no working encoder fails before recording with unsupported (exit 5) and names the working codecs"
    );
    contract["forwarding"]["sources"] = json!(
        "Network streams reconnect after FFmpeg exits, backing off from 1 s to 30 s until the stream stays up for 10 s; frame waits meanwhile time out (exit 4), and five credential rejections (401/403) in a row stop acquisition, while a missing path (404) keeps being retried. Local video files play at their frame rate in live view and recordings unless PlaybackSpeed=Fastest, and captures that start acquisition read them as fast as they ask. Local files and http(s) URLs naming a video file stop with 'Video file finished'."
    );
    contract["selection"]["direct"] = json!(
        "--camera ID|serial|IPv4|sim:NAME|RTSP/SRT/media URI|avfoundation:NAME_OR_INDEX[?size=WxH&fps=F]|v4l2:/dev/videoN[?size=WxH&fps=F]|dshow:video=NAME[?size=WxH&fps=F]|onvif:http://HOST[:PORT]/PATH|onvif://[USER:PASS@]HOST[:PORT][/PATH]|hub:NAME/CAMERA[?stream=MODE&fps=F]|scanner:sane:DEVICE|scanner:wia:ID|scanner:ica:UUID"
    );
    contract["selection"]["matching"] = json!(
        "An exact ID, serial or address wins; otherwise a bare name opens the one local camera whose model or name (the ID after its kind) equals it, or whose model contains it as whole words, ignoring case (never part of a serial or ID), and stderr names the camera chosen. Hub cameras need their exact selector; several matches fail with exit 3. connect and select take the camera as an argument or from --camera / CAPTUREFAB_CAMERA. Scripts should use IDs from discover."
    );
    contract["selection"]["existing_session"] =
        json!("--session NAME [--camera ID] targets an existing serve session");
    contract["limits"]["active_jobs_per_camera"] = json!(32);
    contract["limits"]["camera_workers_default"] = json!(16);
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
        "measurements":"Raw captures preserve acquired bytes and sample precision without embedded calibration. Keep component, unit, scale, offset and calibration features separately. PNG/JPEG/PGM/PPM and forwarded video are 8-bit visualizations; forwarding carries no measurement calibration or color map.",
        "auto":"Generic auto exposure is unsupported for line_scan, depth, thermal and document_scanner. Image cameras, including microscopes with suitable advertised controls, retain support.",
        "stream_limits":"Existing transports acquire single rasters. Depth requires a compatible single-component stream; multipart, GenDC, point-cloud payloads and vendor SDK-only access remain unsupported. Detection does not establish hardware compatibility."
    });
    contract["simulation"] = json!({
        "replay":"--camera sim:replay:PATH uses a local simulator with configurable Width, Height and AcquisitionFrameRate",
        "playlist":"A file or one-level directory of supported images/videos. Regular files only, lexicographically sorted by filename, no recursion or symlink traversal; loops until stopped.",
        "timing":"Fixed delivered frame rate; frames are letterboxed to camera dimensions. Frame IDs and timestamps continue across file and playlist boundaries."
    });
    contract["scanners"] = json!({"enumeration":"scanners --json or discover --json; transport is scanner","backends":{"linux":"SANE scanimage (installed separately)","windows":"Windows Image Acquisition (WIA)","macos":"ImageCaptureCore (bundled native helper)"},"feature":"scanner; enabled by default","settings":{"ScanResolution":{"default":150,"range":[75,600],"unit":"dpi"},"ScanMode":{"default":"Color","choices":["Color","Gray","Lineart"]},"ScanSource":{"default":"Default","choices":["Default","Flatbed","Feeder"]}},"acquisition":"capture requests one page; count requests successive pages, including feeder pages; connecting/start never scans; forwarding and auto exposure are unsupported","timeout":"Use --timeout-ms 60000 for physical scans. Each page has its own deadline and timeout cancels the helper.","outputs":"Existing capture image formats, destinations and storage quotas; no PDF/OCR conversion","limits":{"frame_bytes":134217728,"shared_ring_default_bytes_per_slot":134217728},"compatibility":"Driver-dependent settings; unsupported resolutions, modes and source names fail without silent fallback. SANE source names vary between drivers."});
    contract["resources"] = json!({
        "cli":"resources [status] | configure [--cpu-threads auto|N] [--camera-buffers auto|SIZE] | reset",
        "defaults":"Automatic: bounded image/codec threads, a shared camera-buffer budget from host RAM, and existing storage quotas. CPU and low-memory pressure reduce parallel work.",
        "scope":"App-wide local settings in resources.json; new buffers are admitted within budget. Thread changes affect future pixel work and newly started codecs, never restart ongoing recordings.",
        "status":"policy, effective, host, buffers and storage; unknown readings remain null or report an error"
    });
    contract["storage"] = json!({"default":crate::storage::StoragePolicy::default(),"scope":"All Capturefab-owned capture and recording files across output directories, recorded in this user's state directory. Reservations count toward quota. Existing/unowned/modified files are never deleted.","recording_file_cap_default":crate::media::default_recording_cap(),"recording_cut":"A recording that reaches its maximum file size (storage_full) or fails to write stops, and keeps the video written until then, recorded in the ledger","on_full":["stop","delete-oldest"],"status":"capturefab storage --json","error":"storage_full; exit 6","free_space_floor":crate::storage::FREE_SPACE_FLOOR,"free_bytes":"storage --json reports free space on the disk of the default capture folder (~/Capturefab), or null. Every save must leave free_space_floor bytes free on its own disk, counting what active recordings may still write; delete-oldest removes the oldest owned files on that disk to keep it, except on Windows, and not again on a disk where that freed no space until a save there fits."});
    contract["scheduling"] = json!({"cli":"capture --at RFC3339 | --delay 30s; --interval 10s -n 100","lifetime":"Schedules run while the serve session or direct CLI process remains running; jobs are not persisted across exit.","interval":"First time plus interval times the shots so far: captured, being saved, skipped and retried, so a skipped or retried shot moves the schedule one interval later; zero interval captures consecutive frames. All frames must fall within 10 years. A past first timestamp starts immediately. Durations must resolve to whole milliseconds. Each job has a per-frame transport timeout.","states":["pending","running","complete","cancelled","failed"]});
    contract["auto"] = json!({"cli":"--session NAME auto [--balance B] | manual [--revert]; capture|forward|serve --auto [--balance B]","default":"manual; connecting never enables auto mode and it ends on disconnect","balance":"0 favors image quality, 1 favors frame rate; quality, balanced and frame-rate mean 0, 0.5 and 1; omitted keeps the current value, initially 0.5","status":"status.auto and cameras[].auto; null in manual mode","strategies":["firmware","software","video-mode","none"],"states":["waiting","converging","stable","limited"],"settled":["stable","limited"],"manual":"Setting a feature in auto.managed switches that camera to manual first and the set result adds \"auto\":null; manual turns ExposureAuto, GainAuto and BalanceWhiteAuto off so values hold","persistence":"Camera settings written by auto mode remain after manual, like set; manual --revert, disconnect and session exit restore their previous values (best effort)","capture":"--set applies before --auto; capture saves once exposure settles or after min(timeout_ms/2, 3 s) and result.auto reports the state; with --session auto mode stays on afterwards"});
    contract["history"] = json!({
        "cli":"--session NAME [--camera ID] history | undo | redo",
        "scope":"Reversible camera changes are recorded by the camera worker, including changes made through the CLI and RPC; each connected camera has its own history.",
        "status":"status.history and cameras[].history contain undo and redo labels, or null when unavailable, and entries with setting_changes (the semantic assignment count). history returns the selected or addressed camera's labels.",
        "lifetime":"History is bounded and lasts while the camera remains connected; a new change clears its redo history.",
        "checkpoints":"Successful reversible commands also return camera_change:{token,camera,label,last_changed_at_us}. replay_camera_change targets that token with undo:true or undo:false; expired tokens and conflicting camera state fail.",
        "conflicts":"Replay checks current camera state before changing it. A conflicting or failed replay reports an error and retains the history entry.",
        "effects":"Captures, recordings, schedules, connection changes, arbitrary commands and raw memory writes cannot be reversed by camera history. Stopping a stream with schedules or forwarding is also excluded. Scanner settings are reversible; scanner acquisition, including Start and Stop, may move or consume physical pages and cannot be undone or replayed."
    });
    contract["frame"] = json!({
        "rpc":"{\"op\":\"frame\"} through rpc or IPC on a running session; --camera or the request's camera picks the camera",
        "behavior":"Read-only: the newest frame of one camera as measurements and a bounded preview. It never saves files, uses storage or changes settings, and runs after commands sent to that camera before it. A stopped camera acquires one frame and stays stopped; document scanners are refused.",
        "fields":{"fresh":"0..10 frames acquired after the request, default 1 (2 after a change on real hardware); republishing a held frame does not count; 0 accepts the newest frame","timeout_ms":"1..10000, default 3000","max_dimension":"0 for measurements only, or 64..2048: the preview's longest edge, default 1024","format":"auto (JPEG; PNG in builds without the jpeg feature), jpeg or png","region":"optional normalized [x, y, width, height] crop of the full-resolution frame, also the focus region"},
        "result":{"camera":"camera ID","frame":"{id, seq, width, height, pixel_format, timestamp_ns, age_ms, source: live|grab, waited_frames}","region":"{x, y, width, height, normalized}; Bayer crops widen to whole 2x2 quads","exposure":"{mean, p01, p50, p99, shadows, highlights, histogram16, channels: {red, green, blue} or null}, 0..1; null for depth, thermal and line-scan cameras","auto_meter":"{mean, p99} at native bit depth, or null","focus":"{laplacian, tenengrad, brenner, variance}: compare values for one region of one camera; or null","notes":"advisory text (clipped highlights, crushed shadows, dark or bright mean) and why a measurement is missing","image":"{mime, width, height, bytes, base64}, area-averaged to max_dimension and at most 1.5 MiB; null when max_dimension is 0"},
        "errors":"No frame in time fails with \"no new frame from CAMERA within N (timed out)\" (exit 4); a triggered camera needs TriggerSoftware. A camera that stops streaming fails with its last error."
    });
    let commands = contract["rpc"]["commands"].as_array_mut().unwrap();
    commands.extend([
        json!({"op":"frame","fresh":1,"max_dimension":1024}),
        json!({"op":"frame","fresh":2,"timeout_ms":5000,"max_dimension":512,"format":"png","region":[0.25,0.25,0.5,0.5]}),
        json!({"op":"set_many","values":[{"feature":"ExposureTime","value":"8000"},{"feature":"Gain","value":"6"}]}),
    ]);
    commands.push(json!({"op":"activity","after":12,"timeout_ms":20000}));
    commands.extend([
        json!({"op":"network_scan","timeout_ms":1000}),
        json!({"op":"optimize_network","bytes_per_second":100000000}),
    ]);
    commands.extend([json!({"op":"select","camera":"sim:0"}),json!({"op":"jobs"}),json!({"op":"cancel_job","id":1}),json!({"op":"schedule","output":"timelapse","count":10,"timeout_ms":2000,"format":"png","first_at_ms":crate::scheduling::now_ms()+60000,"interval_ms":10000,"storage":crate::storage::StoragePolicy::default()}),json!({"op":"forward","output":"rtsp://localhost:8554/camera","codec":"h264","encoder":"auto","fps":30,"bitrate":"4M","storage":crate::storage::StoragePolicy::default(),"max_file_bytes":crate::media::default_recording_cap()}),json!({"op":"forward","output":"video/run-{seq}.mkv","codec":"h264","encoder":"auto","fps":30,"bitrate":"4M","storage":crate::storage::StoragePolicy::default(),"max_file_bytes":crate::media::recording_reservation("4M",3_600_000).expect("a valid bitrate"),"on_exists":"number","duration_ms":3_600_000}),json!({"op":"stop_forward"}),json!({"op":"stop_forward_if","forwarding_id":"0123456789abcdef0123456789abcdef"}),json!({"op":"auto","balance":0.5}),json!({"op":"manual"})]);
    commands.extend([
        json!({"op":"undo"}),
        json!({"op":"redo"}),
        json!({"op":"replay_camera_change","token":"TOKEN_FROM_CAMERA_CHANGE","undo":true}),
    ]);
    commands.push(json!({"op":"capture","output":"line1/{date}/{camera}-{datetime}-{seq}.png","count":1,"timeout_ms":2000,"format":"png","on_exists":"number","context":{"line":"L1","part":"A-100"}}));
    contract["efficiency"] = json!({
        "cli":"--efficiency auto|smooth|efficient (global)",
        "environment":crate::efficiency::ENV,
        "precedence":"--efficiency, then CAPTUREFAB_EFFICIENCY, then auto; an unknown CAPTUREFAB_EFFICIENCY value means auto",
        "auto":"efficient with at most 2 CPU cores, at most 2 GiB of memory, or the OS low-power or battery-saver mode; running on battery alone stays smooth",
        "scope":"The process that parses it, such as serve or a direct command, and the camera workers it starts, which are told efficient whenever that process resolved efficient; a running session keeps its own setting. In workers, efficient decodes each live stream with one thread and picks a faster libx264 preset; the session polls its workers for status every 500 ms instead of 250 ms while a camera streams or a view watches, and every 2 s instead of 1 s otherwise. Scaled-down previews keep one pixel of each block instead of averaging. Captures and recordings keep every frame in every mode.",
        "status":"doctor --json efficiency {setting,efficient,reasons}"
    });
    contract["doctor"] = json!({
        "host":"{cpu_cores,memory_total,memory_available,on_battery,low_power}; on_battery is reported but never switches efficiency by itself",
        "efficiency":"{setting:auto|smooth|efficient,efficient,reasons}",
        "paths":"{runtime,state,cache,temp,working_directory}, each {path,exists,fs_type,ram_backed,total_bytes,free_bytes} (cache is null without a home folder); without exists the facts describe the nearest existing parent",
        "directories":"runtime_directory holds session descriptors and their locks, scanner and GPU JPEG staging; state_directory holds saved hubs, destinations, setups and sign-in indexes, and the storage ledger; cache_directory holds the extracted FFmpeg and encoder probes. On Linux runtime is $XDG_RUNTIME_DIR/capturefab, else /run/user/<uid>/capturefab when the user lingers (loginctl enable-linger) and that directory is theirs with mode 0700, else ~/.cache/capturefab/sessions, and state is $XDG_STATE_HOME/capturefab or ~/.local/state/capturefab; elsewhere both are one directory. CAPTUREFAB_SESSION_DIR sets both and CAPTUREFAB_CACHE_DIR the cache",
        "limits":"{rmem_max,usbfs_memory_mb}; null outside Linux",
        "checks":"[{id,level:ok|warn,message,fix}]; ids include efficiency, runtime_dir, session_storage, private_dirs, camera_network, ffmpeg, receive_buffer, usb_access, storage_budget and updates (an available version, or an unrecognized CAPTUREFAB_UPDATE_CHECK); checks that do not apply are omitted; camera_network is always ok",
        "update_check":"{compiled,automatic,policy,skipped,last_checked,latest,update_available,last_error}, as version --json; doctor never fetches",
        "ring_backing":"memfd|posix-shm|file: where camera frame rings live; file only on Windows or where anonymous shared memory is unavailable",
        "human":"Plain doctor prints the host summary and one line per check with its fix"
    });
    contract["hubs"] = crate::hubs::schema_contract();
    contract["updates"] = crate::updates::schema_contract();
    setup::extend_schema(&mut contract);
    contract
}

const RECORDING_GRACE: Duration = Duration::from_secs(2);

/// What `record`/`forward --duration` does after one look at the status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordingWait {
    /// The recording runs on; look again.
    Wait,
    /// The session ended the recording at its end time. Nothing is left to
    /// stop, and stop-forward could stop a newer recording on that camera.
    Finished,
    /// Ctrl-C, or the CLI's own timer ended while still recording: send
    /// stop-forward.
    Stop,
    /// The recording stopped before its duration elapsed, or with an error.
    Failed,
}

/// What one status says about the camera's forward.
#[derive(Clone, Copy)]
struct ForwardStatus<'a> {
    forwarding: bool,
    forwarding_id: Option<&'a str>,
    last_error: Option<&'a str>,
    /// When the camera last logged an error (UTC Unix ms), which tells the
    /// same error logged again from the one before.
    error_logged_ms: Option<u64>,
}
impl<'a> ForwardStatus<'a> {
    /// The selected camera in this process's own session.
    fn of_snapshot(state: &'a crate::session::SessionSnapshot) -> Self {
        Self {
            forwarding: state.forwarding.is_some(),
            forwarding_id: state.forwarding_id.as_deref(),
            last_error: state.last_error.as_deref(),
            error_logged_ms: state
                .connected
                .as_ref()
                .and_then(|camera| crate::session::camera_error_logged_ms(&state.logs, &camera.id)),
        }
    }
    /// The addressed camera in a session's `status` reply.
    fn of_status(state: &'a Value) -> Self {
        let logs =
            <Vec<crate::session::LogEntry> as serde::Deserialize>::deserialize(&state["logs"])
                .unwrap_or_default();
        Self {
            forwarding: !state["forwarding"].is_null(),
            forwarding_id: state["forwarding_id"].as_str(),
            last_error: state["last_error"].as_str(),
            error_logged_ms: state["connected"]["id"]
                .as_str()
                .and_then(|camera| crate::session::camera_error_logged_ms(&logs, camera)),
        }
    }
}

/// The camera's newest error as one status shows it.
#[derive(Clone, Debug, Default, PartialEq)]
struct ErrorMark {
    text: Option<String>,
    logged_ms: Option<u64>,
}
impl ErrorMark {
    fn of(status: &ForwardStatus) -> Self {
        Self {
            text: status.last_error.map(str::to_owned),
            logged_ms: status.error_logged_ms,
        }
    }
    /// Whether `status` shows an error logged after this one: another
    /// error, or the same one again. A line that has left the bounded log
    /// is not a new error.
    fn newer_in(&self, status: &ForwardStatus) -> bool {
        status.last_error != self.text.as_deref() || status.error_logged_ms > self.logged_ms
    }
}

/// A timed recording or forward that the CLI waits on.
struct TimedRecording {
    /// The requested duration, timed by the CLI from the forward reply.
    duration: Duration,
    /// When the session ends it (the reply's `until_ms`, UTC Unix ms).
    until_ms: u64,
    /// The worker-issued identity is authoritative even when a replacement
    /// starts in the same millisecond with the same output name.
    forwarding_id: String,
    /// The camera's newest error from before the recording started, then
    /// as of the last status that showed it running. A newer one once it
    /// has stopped means it failed. A recording that wrote video retains
    /// that file; one that failed before writing may have none. This holds
    /// for a recording that fails before any status
    /// shows it running, as one does when FFmpeg cannot start.
    errors: ErrorMark,
}
impl TimedRecording {
    fn new(duration_ms: u64, reply: &Value, errors: ErrorMark) -> Result<Self> {
        Ok(Self {
            duration: Duration::from_millis(duration_ms),
            until_ms: reply["until_ms"]
                .as_u64()
                .context("The timed recording reply is missing its end time.")?,
            forwarding_id: reply["forwarding_id"]
                .as_str()
                .filter(|id| crate::session::valid_forwarding_id(id))
                .context("The recording reply is missing a valid forwarding identity.")?
                .to_owned(),
            errors,
        })
    }
    /// The `stopped` result of a recording the session ended on time, with
    /// its final counters when `progress` (the status's
    /// `forwarding_progress`) is this recording's, not a later one's.
    fn finished(&self, progress: &Value) -> Value {
        let mut stopped = json!({"forwarding": null, "by": "duration"});
        if progress["id"].as_str() == Some(self.forwarding_id.as_str()) {
            let mut stats = progress.clone();
            if let Some(stats) = stats.as_object_mut() {
                stats.remove("id");
            }
            stopped["stats"] = stats;
        }
        stopped
    }
    fn stop_command(&self) -> SessionCommand {
        SessionCommand::StopForwardIf {
            forwarding_id: self.forwarding_id.clone(),
        }
    }
    /// Decide from a `status` read at `now_ms` (UTC Unix ms), `elapsed`
    /// after the forward reply.
    fn next(
        &mut self,
        status: ForwardStatus,
        now_ms: u64,
        elapsed: Duration,
        interrupted: bool,
    ) -> RecordingWait {
        let ours = status.forwarding && status.forwarding_id == Some(self.forwarding_id.as_str());
        if !ours {
            let grace = RECORDING_GRACE.as_millis() as u64;
            let on_time = now_ms.saturating_add(grace) >= self.until_ms || elapsed >= self.duration;
            return if on_time && !self.errors.newer_in(&status) {
                RecordingWait::Finished
            } else {
                RecordingWait::Failed
            };
        }
        // An error while it still runs did not end it. The worker logs the
        // error that ends a forward in the step that clears it.
        self.errors = ErrorMark::of(&status);
        // When the session keeps the end time, give it the grace to act
        // first; stop-forward is then only a fallback.
        let timer = self.duration.saturating_add(RECORDING_GRACE);
        if interrupted || elapsed >= timer {
            RecordingWait::Stop
        } else {
            RecordingWait::Wait
        }
    }
}

fn destination_command(command: &DestinationCommand) -> Result<Value> {
    use crate::destination::{self, Destination, Target};
    let describe = |saved: &Destination| {
        let mut value = serde_json::to_value(saved).unwrap_or_default();
        value["location"] = json!(saved.describe());
        value
    };
    Ok(match command {
        DestinationCommand::List => {
            json!({"destinations": destination::list()?.iter().map(describe).collect::<Vec<_>>()})
        }
        DestinationCommand::AddFolder { name, path } => {
            let saved = Destination {
                name: name.clone(),
                target: Target::Folder {
                    path: std::path::absolute(path)?,
                },
            };
            input(saved.validate())?;
            let Target::Folder { path } = &saved.target;
            crate::volumes::ensure_present(path)?;
            std::fs::create_dir_all(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            destination::save(saved.clone())?;
            json!({"destination":describe(&saved)})
        }
        DestinationCommand::Remove { name } => json!({"removed":destination::remove(name)?.name}),
        DestinationCommand::Test { name } => {
            let saved = destination::get(name)?;
            destination::test(&saved)?;
            json!({"ok":true,"free_bytes":fs2::available_space(saved.local_root()).ok(),"destination":describe(&saved)})
        }
    })
}

/// Run `handler` on Ctrl-C, SIGTERM (`systemctl stop`) or SIGHUP (a closed
/// terminal). A SIGHUP this process started out ignoring, as under `nohup`,
/// stays ignored: whoever started it asked it to outlive its terminal.
pub(crate) fn on_quit_signal(handler: impl FnMut() + Send + 'static) -> Result<()> {
    #[cfg(unix)]
    let hangup_ignored = hangup_ignored();
    ctrlc::set_handler(handler).context("install signal handler")?;
    #[cfg(unix)]
    if hangup_ignored {
        // SAFETY: setting a standard signal's disposition to ignore has no
        // preconditions.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }
    Ok(())
}
#[cfg(unix)]
fn hangup_ignored() -> bool {
    // SAFETY: a null new action only reads the current one into `old`.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) == 0
            && old.sa_sigaction == libc::SIG_IGN
    }
}

/// How long a wait loop sleeps before it checks again: a second, or until
/// its next deadline when that is sooner, but not under 100 ms.
fn check_in(deadline: Option<Duration>) -> Duration {
    const MOST: Duration = Duration::from_secs(1);
    deadline.map_or(MOST, |left| left.clamp(Duration::from_millis(100), MOST))
}

impl Cli {
    /// The `-o` of a capture or recording, as typed.
    pub fn typed_output(&self) -> Option<String> {
        match &self.command {
            Some(Command::Capture { output, .. } | Command::Forward { output, .. })
                if output != "-" =>
            {
                Some(output.clone())
            }
            _ => None,
        }
    }
}
/// How to save under a taken output name, built from the `-o` typed; a
/// worker's reply keeps only the text, so that is checked too.
pub fn exists_hint(error: &anyhow::Error, output: &str) -> Option<String> {
    let taken = error
        .chain()
        .any(|cause| cause.is::<crate::storage::OutputExists>())
        || format!("{error:#}").contains("output already exists");
    taken.then(|| summary::exists_hint(output))
}
pub fn error_code(error: &anyhow::Error) -> (&'static str, i32) {
    if error
        .chain()
        .any(|cause| cause.is::<InvalidInput>() || cause.is::<crate::types::Usage>())
    {
        return ("usage", 2);
    }
    if let Some(categorized) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::types::Categorized>())
    {
        return categorized.kind.code();
    }
    let s = format!("{error:#}").to_ascii_lowercase();
    if s.contains("storage full") || s.contains("no space left") {
        ("storage_full", 6)
    } else if s.contains("connection refused")
        // Windows words a refused port this way.
        || s.contains("actively refused")
        // A host name that does not resolve is as unreachable as a refused
        // port (getaddrinfo wording on macOS, Linux and Windows).
        || s.contains("failed to lookup address")
        || s.contains("cannot resolve")
        || s.contains("name or service not known")
        || s.contains("nodename nor servname")
        || s.contains("temporary failure in name resolution")
        || s.contains("no such host is known")
        || s.contains("no route to host")
        || s.contains("network is unreachable")
        || s.contains("host is down")
        || s.contains("is offline")
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
        || s.contains(crate::media::CAMERA_ACCESS_DENIED)
        || s.contains("camera is in use by another application")
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
    fn binary_images_never_go_to_an_interactive_terminal() {
        let error = binary_stdout_allowed("-", true).unwrap_err();
        assert!(error.to_string().contains("terminal"), "{error:#}");
        binary_stdout_allowed("-", false).unwrap();
        binary_stdout_allowed("frame.png", true).unwrap();
    }
    #[test]
    fn connect_takes_its_argument_or_the_global_camera() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();
        let target = |cli: &Cli| match &cli.command {
            Some(Command::Connect { target } | Command::Select { target }) => {
                connect_target(cli, target)
            }
            _ => unreachable!(),
        };
        let cli = parse(&["capturefab", "connect", "sim:0"]);
        assert_eq!(target(&cli).unwrap(), "sim:0");
        let cli = parse(&["capturefab", "connect", "--camera", "sim:1"]);
        assert_eq!(target(&cli).unwrap(), "sim:1");
        let cli = parse(&["capturefab", "--camera", "sim:1", "select", "sim:1"]);
        assert_eq!(target(&cli).unwrap(), "sim:1");
        let cli = parse(&["capturefab", "connect", "sim:0", "--camera", "sim:1"]);
        assert!(
            target(&cli)
                .unwrap_err()
                .to_string()
                .contains("two cameras")
        );
        let cli = parse(&["capturefab", "select"]);
        assert!(
            target(&cli)
                .unwrap_err()
                .to_string()
                .starts_with("select needs a camera")
        );
    }
    #[test]
    fn clap_errors_become_one_line_with_details() {
        let error = Cli::try_parse_from(["capturefab", "discovr"]).unwrap_err();
        let envelope = clap_error_envelope(&error);
        assert_eq!(envelope["error"]["code"], "usage");
        assert_eq!(
            envelope["error"]["message"],
            "unrecognized subcommand 'discovr'"
        );
        assert_eq!(envelope["error"]["details"]["suggestions"][0], "discover");
        assert_eq!(envelope["error"]["details"]["invalid"], "discovr");
        let error = Cli::try_parse_from(["capturefab", "capture"]).unwrap_err();
        let details = &clap_error_envelope(&error)["error"]["details"];
        assert_eq!(details["kind"], "missing_required_argument");
        assert!(
            details["usage"]
                .as_str()
                .unwrap()
                .starts_with("capturefab capture"),
            "{details}"
        );
    }
    #[test]
    fn capture_contract_omits_retired_sensor_commands() {
        for command in ["signals", "log"] {
            assert!(Cli::try_parse_from(["capturefab", command]).is_err());
        }
        let contract = schema();
        assert!(contract.get("signals").is_none());
        assert!(
            contract["rpc"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .all(|command| { !command["op"].as_str().unwrap().starts_with("signal") })
        );
    }
    #[test]
    fn wait_loops_check_in_once_a_second_or_at_their_deadline() {
        let second = Duration::from_secs(1);
        assert_eq!(check_in(None), second);
        assert_eq!(check_in(Some(Duration::from_secs(3600))), second);
        assert_eq!(
            check_in(Some(Duration::from_millis(400))),
            Duration::from_millis(400)
        );
        assert_eq!(check_in(Some(Duration::ZERO)), Duration::from_millis(100));
    }
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
                "frame",
                "set_many"
            ]
            .iter()
            .all(|op| commands.iter().any(|c| c["op"] == *op))
        );
        // Grab belongs to camera workers; sessions refuse it.
        assert!(!commands.iter().any(|c| c["op"] == "grab"));
        assert!(contract["frame"]["fields"]["max_dimension"].is_string());
        // README and agents.md point to these for set_many.
        assert!(contract["writes"].as_str().unwrap().contains("set_many"));
        // A timed recording reserves enough for its whole duration.
        let timed = commands
            .iter()
            .find(|c| c["op"] == "forward" && c["duration_ms"].is_u64())
            .expect("a timed forward example");
        assert!(
            timed["max_file_bytes"].as_u64().unwrap()
                >= crate::media::estimate_recording_bytes(
                    timed["bitrate"].as_str().unwrap(),
                    timed["duration_ms"].as_u64().unwrap()
                )
                .unwrap()
        );
        assert_eq!(contract["auto"]["settled"], json!(["stable", "limited"]));
    }
    #[test]
    fn timed_recordings_end_in_the_session_and_stop_forward_is_a_fallback() {
        use RecordingWait::{Failed, Finished, Stop, Wait};
        let since = 1_000_000;
        let id = "11111111111111111111111111111111";
        let replacement = "22222222222222222222222222222222";
        let reply = json!({"forwarding":"clip.mkv","forwarding_id":id,"since_ms":since,"until_ms":since + 30_000});
        let earlier = Some("an earlier failure");
        let logged = Some(since - 5_000);
        let status = |forwarding, forwarding_id, last_error, error_logged_ms| ForwardStatus {
            forwarding,
            forwarding_id,
            last_error,
            error_logged_ms,
        };
        // The camera's error from before the recording started.
        let before = ErrorMark::of(&status(false, None, earlier, logged));
        let recording = status(true, Some(id), earlier, logged);
        let stopped = status(false, None, earlier, logged);
        let s = Duration::from_secs;
        let mut timed = TimedRecording::new(30_000, &reply, before.clone()).unwrap();
        // Still recording: wait out the duration and the session's grace.
        assert_eq!(timed.next(recording, since + 10_000, s(10), false), Wait);
        assert_eq!(timed.next(recording, since + 31_000, s(31), false), Wait);
        assert_eq!(timed.next(recording, since + 32_000, s(32), false), Stop);
        // Ctrl-C stops it at once.
        assert_eq!(timed.next(recording, since + 1_000, s(1), true), Stop);
        // The session ended it on time, or just before: nothing to stop. An
        // error from before the recording does not count against it.
        for now in [since + 28_000, since + 30_000, since + 45_000] {
            assert_eq!(timed.next(stopped, now, s(30), false), Finished);
        }
        assert_eq!(timed.next(stopped, since + 30_000, s(30), true), Finished);
        // The worker's monotonic timer can finish while UTC is still behind
        // its nominal deadline after a system-clock rollback.
        assert_eq!(timed.next(stopped, since - 60_000, s(30), false), Finished);
        // Another participant's newer recording is not this one to stop.
        let newer = status(true, Some(replacement), earlier, logged);
        assert_eq!(timed.next(newer, since + 31_000, s(33), false), Finished);
        // Stopped early, or with a new error, is a failure even if video was kept.
        assert_eq!(timed.next(stopped, since + 27_999, s(28), false), Failed);
        let early = status(true, Some(replacement), None, None);
        assert_eq!(timed.next(early, since + 6_000, s(6), false), Failed);
        let full = Some("Forwarding stopped: Storage full");
        let storage_full = status(false, None, full, Some(since + 29_000));
        assert_eq!(
            timed.next(storage_full, since + 29_500, s(29), false),
            Failed
        );
        assert_eq!(
            timed.next(storage_full, since + 30_000, s(30), false),
            Failed
        );
        // The same error logged again is a new error, and a line that has
        // left the bounded log is not.
        let again = status(false, None, earlier, Some(since + 29_000));
        assert_eq!(timed.next(again, since + 30_000, s(30), false), Failed);
        let trimmed = status(false, None, earlier, None);
        assert_eq!(timed.next(trimmed, since + 30_000, s(30), false), Finished);
        // An error while it still ran did not end it.
        let mut noted = TimedRecording::new(30_000, &reply, before.clone()).unwrap();
        let busy = Some("Shared frame buffer: busy");
        let warned = status(true, Some(id), busy, Some(since + 10_000));
        assert_eq!(noted.next(warned, since + 10_000, s(10), false), Wait);
        let ended = status(false, None, busy, Some(since + 10_000));
        assert_eq!(noted.next(ended, since + 30_000, s(30), false), Finished);
        // A short recording can fail, or end, before any status shows it
        // running (FFmpeg starts with the first frame). Only an error
        // logged since it started tells the two apart, even the same one.
        let one_second = json!({"forwarding_id":id,"since_ms":since,"until_ms":since + 1_000});
        let short = || TimedRecording::new(1_000, &one_second, before.clone()).unwrap();
        let at = since + 300;
        assert_eq!(short().next(stopped, at, s(0), false), Finished);
        let no_ffmpeg = Some("Forwarding stopped: no FFmpeg");
        let failed = status(false, None, no_ffmpeg, Some(since + 50));
        assert_eq!(short().next(failed, at, s(0), false), Failed);
        let retried = status(false, None, earlier, Some(since + 50));
        assert_eq!(short().next(retried, at, s(0), false), Failed);
        // Final counters are this recording's only, never a replacement's,
        // and an older worker without them reports none rather than zeros.
        let progress =
            json!({"id":id,"accepted":30,"written":30,"dropped":0,"late":0,"stalled":false});
        assert_eq!(
            short().finished(&progress),
            json!({"forwarding":null,"by":"duration","stats":{"accepted":30,"written":30,"dropped":0,"late":0,"stalled":false}})
        );
        let mut other = progress.clone();
        other["id"] = json!("22222222222222222222222222222222");
        for absent in [other, Value::Null] {
            assert_eq!(
                short().finished(&absent),
                json!({"forwarding":null,"by":"duration"})
            );
        }
    }
    #[test]
    fn timed_recording_fallback_cannot_stop_a_same_millisecond_replacement() {
        let first = "11111111111111111111111111111111";
        let replacement = "22222222222222222222222222222222";
        let reply = json!({"forwarding_id":first,"since_ms":1000,"until_ms":31000});
        let mut timed = TimedRecording::new(30_000, &reply, ErrorMark::default()).unwrap();
        let status = |id| ForwardStatus {
            forwarding: true,
            forwarding_id: Some(id),
            last_error: None,
            error_logged_ms: None,
        };
        assert_eq!(
            timed.next(status(first), 2000, Duration::from_secs(1), true),
            RecordingWait::Stop
        );
        assert!(
            matches!(timed.stop_command(), SessionCommand::StopForwardIf { forwarding_id } if forwarding_id == first)
        );
        assert_eq!(
            timed.next(status(replacement), 32000, Duration::from_secs(32), true),
            RecordingWait::Finished
        );
        assert_eq!(
            timed.next(status(replacement), 2000, Duration::from_secs(1), true),
            RecordingWait::Failed
        );
        for incomplete in [
            json!({"until_ms":31000}),
            json!({"forwarding_id":first}),
            json!({"forwarding_id":"invalid","until_ms":31000}),
        ] {
            assert!(TimedRecording::new(30_000, &incomplete, ErrorMark::default()).is_err());
        }
    }
    #[test]
    fn schema_direct_selection_lists_native_options_and_onvif_sign_ins() {
        let direct = schema()["selection"]["direct"].as_str().unwrap().to_owned();
        for form in [
            "avfoundation:NAME_OR_INDEX[?size=WxH&fps=F]",
            "v4l2:/dev/videoN[?size=WxH&fps=F]",
            "dshow:video=NAME[?size=WxH&fps=F]",
            "onvif://[USER:PASS@]HOST[:PORT][/PATH]",
        ] {
            assert!(direct.contains(form), "{form}");
        }
    }
    #[test]
    fn schema_lists_every_capture_job_field() {
        let job = crate::scheduling::CaptureJob {
            id: 1,
            status: "pending".into(),
            output: "frames".into(),
            count: 3,
            captured: 1,
            skipped: 1,
            retries: 1,
            first_at_ms: 1,
            interval_ms: 10,
            next_at_ms: 31,
            last_file: None,
            error: None,
            destination: Some("archive".into()),
            context: crate::context::Context::from([("line".into(), "L1".into())]),
        };
        let job = serde_json::to_value(job).unwrap();
        let fields = schema()["jobs"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field.as_str().unwrap().trim_end_matches('?').to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(fields, job.as_object().unwrap().keys().cloned().collect());
        // Skipped and retried shots each move next_at_ms one interval later.
        let interval = schema()["scheduling"]["interval"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(interval.contains("skipped and retried"), "{interval}");
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
    fn frame_inspection_errors_keep_their_exit_codes() {
        // Bad values are usage errors, never timeouts, whatever their wording.
        for invalid in [
            r#"{"op":"frame","fresh":11}"#,
            r#"{"op":"frame","timeout_ms":0}"#,
            r#"{"op":"frame","timeout_ms":10001}"#,
            r#"{"op":"frame","max_dimension":32}"#,
            r#"{"op":"frame","region":[0.5,0.5,0.6,0.1]}"#,
            r#"{"op":"set_many","values":[]}"#,
            r#"{"op":"set_many","values":[{"feature":"Exposure Time","value":"1"}]}"#,
            r#"{"op":"grab","timeout_ms":0}"#,
        ] {
            let error = SessionCommand::from_json_slice(invalid.as_bytes())
                .unwrap()
                .validate()
                .unwrap_err();
            assert_eq!(
                error_code(&error),
                ("operation_failed", 1),
                "{invalid}: {error}"
            );
            // `rpc` reports a command that fails validation as usage.
            let usage = input::<()>(Err(error)).unwrap_err();
            assert_eq!(error_code(&usage), ("usage", 2));
        }
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "no new frame from sim:0 within 3 s (timed out); a triggered camera needs TriggerSoftware"
            )),
            ("timeout", 4)
        );
        for failure in [
            "a document scanner cannot be inspected without scanning a page",
            "sim:0 stopped streaming: Shared frame buffer: frame exceeds shared-memory capacity",
            "grab is reserved for camera workers; use frame to inspect a camera",
            "preview image exceeds the size limit; request a smaller max_dimension",
            "too many frame inspections are waiting; retry when one returns",
        ] {
            assert_eq!(
                error_code(&anyhow::anyhow!("{failure}")),
                ("operation_failed", 1),
                "{failure}"
            );
        }
    }
    #[test]
    fn error_codes() {
        for unresolved in [
            "cannot reach the server: cannot resolve http://nonexistent.invalid:80: failed to lookup address information: nodename nor servname provided, or not known",
            "resolve camera host camera.invalid: failed to lookup address information: Name or service not known",
            "connecting to rtsp://x:554: Temporary failure in name resolution",
        ] {
            assert_eq!(
                error_code(&anyhow::anyhow!("{unresolved}")),
                ("unavailable", 3),
                "{unresolved}"
            );
        }
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "multipart payloads are not implemented yet"
            )),
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
                "GPU JPEG encoding is unsupported in this build"
            )),
            ("unsupported", 5)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "camera configuration changed unexpectedly; inspect the current settings"
            )),
            ("operation_failed", 1)
        );
        assert_eq!(
            error_code(&anyhow::anyhow!(
                "cannot reach the server: connecting on rtsp://h:554: Connection refused (os error 61); check the camera address"
            )),
            ("unavailable", 3)
        );
        for unreachable in [
            "cannot reach hub home at http://10.0.0.9:8123: no route to host",
            "cannot reach hub home at http://10.0.0.9:8123: network is unreachable",
            "cannot reach hub home at http://10.0.0.9:8123: host is down",
            "camera camera.porch on hub home is offline (HTTP 503)",
        ] {
            assert_eq!(
                error_code(&anyhow::anyhow!("{unreachable}")),
                ("unavailable", 3),
                "{unreachable}"
            );
        }
        // A category set where the error happened wins over words that
        // happen to appear in a camera or hub name.
        use crate::types::{Categorized, ErrorKind};
        assert_eq!(
            error_code(&Categorized::error(
                ErrorKind::Unavailable,
                "camera camera.disabled_porch timed out"
            )),
            ("unavailable", 3)
        );
        assert_eq!(
            error_code(
                &Categorized::error(ErrorKind::Timeout, "hub not_found did not answer")
                    .context("connect")
            ),
            ("timeout", 4)
        );
        assert_eq!(
            error_code(&crate::types::error_from_transport(
                Some("unsupported"),
                "camera hubs are unavailable".into()
            )),
            ("unsupported", 5)
        );
    }
    #[test]
    fn efficiency_flag_parses_known_modes_only() {
        let parse = |args: &[&str]| Cli::try_parse_from(["capturefab"].iter().chain(args));
        assert_eq!(parse(&["doctor"]).unwrap().efficiency, None);
        assert_eq!(
            parse(&["doctor", "--efficiency", "efficient"])
                .unwrap()
                .efficiency,
            Some(crate::efficiency::Mode::Efficient)
        );
        assert_eq!(
            parse(&["--efficiency", "smooth", "serve"])
                .unwrap()
                .efficiency,
            Some(crate::efficiency::Mode::Smooth)
        );
        assert!(parse(&["--efficiency", "turbo", "doctor"]).is_err());
        let contract = schema();
        let flag = contract["cli"]["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|argument| argument["long"] == "efficiency")
            .unwrap();
        assert_eq!(flag["choices"], json!(["auto", "smooth", "efficient"]));
        assert!(
            contract["selection"]["environment"]
                .as_array()
                .unwrap()
                .contains(&json!("CAPTUREFAB_EFFICIENCY"))
        );
        assert_eq!(
            contract["efficiency"]["environment"],
            "CAPTUREFAB_EFFICIENCY"
        );
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
