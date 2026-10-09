use crate::{
    ipc,
    session::{SessionCommand, SessionHandle},
};
use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
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
    version,
    about = "GenICam camera workbench. With no command, open the GUI.",
    after_help = "Examples:\n  capturefab\n  capturefab discover --json\n  capturefab --camera 192.168.1.10 get Width ExposureTime\n  capturefab --camera sim:0 capture -o frame.png\n  capturefab --session bench set ExposureTime=5000\n  capturefab --session bench capture -n 10 -o frames\n\nUse `capturefab schema` for the automation contract."
)]
pub struct Cli {
    /// Emit one versioned JSON result on stdout; diagnostics stay on stderr
    #[arg(long, global = true)]
    pub json: bool,
    /// Camera ID, unique serial, IPv4 address, or sim:0
    #[arg(short = 'c', long, global = true)]
    pub camera: Option<String>,
    /// Control an existing GUI or serve session instead of opening a second camera
    #[arg(long, global = true, env = "CAPTUREFAB_SESSION")]
    pub session: Option<String>,
    /// Transport operation timeout in milliseconds
    #[arg(long,global=true,default_value_t=2000,value_parser=clap::value_parser!(u64).range(1..=60_000))]
    pub timeout_ms: u64,
    /// Include the built-in pattern camera in discovery
    #[arg(long, global = true)]
    pub simulate: bool,
    /// FFmpeg decoding acceleration; encoder selection is controlled by forward --encoder
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
#[derive(Debug, Args)]
pub struct StorageArgs {
    /// Maximum owned bytes across all outputs (default: saved global policy, initially 10GiB)
    #[arg(long)]
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
    #[arg(long, conflicts_with = "no_max_age")]
    max_age: Option<String>,
    /// Clear a previously configured age limit
    #[arg(long)]
    no_max_age: bool,
}
impl StorageArgs {
    fn policy(&self) -> Result<crate::storage::StoragePolicy> {
        let mut policy = crate::storage::configured_policy()?;
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
            policy.max_age_seconds = Some(crate::scheduling::parse_duration(age)?.div_ceil(1000));
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
    /// List saved destinations
    List,
    /// Save a folder destination, such as a directory on an external drive or network share
    AddFolder { name: String, path: PathBuf },
    /// Save an S3-compatible bucket (AWS S3, Cloudflare R2, Backblaze B2, MinIO, Wasabi)
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
    },
    /// Remove a saved destination
    Remove {
        name: String,
        /// Also delete its secret key from the OS credential store
        #[arg(long)]
        forget_secret: bool,
    },
    /// Check that a destination is reachable and writable
    Test { name: String },
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
        #[arg(long, default_value = "10m")]
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
pub enum Command {
    /// Open the desktop GUI and expose its local automation session
    Gui {
        #[arg(long, default_value = "gui")]
        name: String,
        /// Accepted for compatibility; the GUI always renders with wgpu
        #[arg(long, hide = true)]
        wgpu: bool,
        /// Save a real rendered workbench screenshot, then exit (release tooling)
        #[arg(long)]
        screenshot: Option<PathBuf>,
        #[arg(long,default_value_t=1,value_parser=clap::value_parser!(u32).range(1..=16))]
        screenshot_cameras: u32,
    },
    /// Discover GigE Vision, USB3 Vision, ONVIF and host cameras
    #[command(visible_aliases=["list","cameras"])]
    Discover,
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
    /// Execute a GenICam command such as TriggerSoftware
    Execute { feature: String },
    /// Connect a camera in an existing persistent session
    Connect { camera: String },
    /// Select the camera shown in a persistent session's inspector
    Select { camera: String },
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
        #[arg(long, default_value = "auto")]
        encoder: String,
        #[arg(long, default_value_t = 30.0)]
        fps: f64,
        #[arg(long, default_value = "4M")]
        bitrate: String,
        /// Direct capture duration in seconds; with --session forwarding continues until stop-forward
        #[arg(long)]
        duration: Option<String>,
        /// Bounded maximum size of a local recording file
        #[arg(long, default_value = "512MiB")]
        max_file_size: String,
        /// Saved destination (see `destination list`); --output is then a name inside it
        #[arg(long)]
        destination: Option<String>,
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
    Capture {
        /// File for one frame, directory for a sequence, or template containing {frame}; '-' writes one image to stdout
        #[arg(short = 'o', long)]
        output: String,
        #[arg(short='n',long,default_value_t=1,value_parser=clap::value_parser!(u32).range(1..=100_000))]
        count: u32,
        #[arg(short = 'f', long, value_enum, default_value = "png")]
        format: ImageFormat,
        /// Set FEATURE=VALUE before acquisition; may be repeated
        #[arg(long = "set")]
        settings: Vec<String>,
        /// Begin at a timestamp with timezone, e.g. 2026-10-05T09:00:00-04:00
        #[arg(long, conflicts_with = "delay")]
        at: Option<String>,
        /// Begin after a delay, e.g. 30s or 5m
        #[arg(long)]
        delay: Option<String>,
        /// Time between frames, e.g. 10s; count bounds the time lapse
        #[arg(long)]
        interval: Option<String>,
        /// Saved destination (see `destination list`); --output is then a name inside it
        #[arg(long)]
        destination: Option<String>,
        #[command(flatten)]
        storage: StorageArgs,
        #[command(flatten)]
        auto: AutoArgs,
    },
    /// List capture schedules in a persistent session
    Jobs,
    /// Cancel a scheduled capture job on the selected camera
    Cancel { id: u64 },
    /// Manage saved capture destinations: folders, external drives and S3 buckets
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
        #[command(flatten)]
        auto: AutoArgs,
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
        !k.is_empty() && !v.is_empty(),
        "expected a nonempty FEATURE=VALUE"
    );
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
}
impl<'a> Client<'a> {
    fn new(cli: &'a Cli) -> Result<Self> {
        let local = if cli.session.is_none() {
            Some(SessionHandle::new())
        } else {
            None
        };
        let c = Self { cli, local };
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
        if let Some(name) = &self.cli.session {
            let ms = match &command {
                SessionCommand::Capture {
                    count, timeout_ms, ..
                } => timeout_ms
                    .saturating_mul(*count as u64)
                    .saturating_add(60_000),
                _ => 60_000,
            };
            ipc::call_to(
                name,
                self.cli.camera.as_deref(),
                command,
                Duration::from_millis(ms),
            )
        } else {
            self.local.as_ref().unwrap().request(command)
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
impl Drop for Client<'_> {
    fn drop(&mut self) {
        if let Some(h) = &self.local {
            h.shutdown();
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    crate::media::configure_hwaccel(&cli.hwaccel)?;
    match &cli.command {
        None | Some(Command::Gui { .. }) => {
            ensure!(
                cli.session.is_none(),
                "use an explicit command with --session, such as status"
            );
            ensure!(!cli.json, "choose a CLI command when using --json");
            let (name, wgpu, screenshot, demo_cameras) = match &cli.command {
                Some(Command::Gui {
                    name,
                    wgpu,
                    screenshot,
                    screenshot_cameras,
                }) => (
                    name.as_str(),
                    *wgpu,
                    screenshot.clone(),
                    *screenshot_cameras,
                ),
                _ => ("gui", false, None, 1),
            };
            let _ = wgpu;
            let client = Client::new(&cli)?;
            let handle = client.local.as_ref().unwrap().clone();
            #[cfg(feature = "gui")]
            crate::gui::migrate_prefs();
            let _server = ipc::Server::start(handle.clone(), name)?;
            #[cfg(feature = "s3")]
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
        Some(Command::Schema) => return write_json(&schema()),
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
            let _server = ipc::Server::start(handle.clone(), name)?;
            // A persistent session uploads what its cameras queue.
            #[cfg(feature = "s3")]
            crate::upload::start();
            output(
                &cli,
                json!({"session":name,"pid":std::process::id(),"ready":true}),
            )?;
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
                | Command::Select { .. }
                | Command::Disconnect
                | Command::Start
                | Command::Stop
                | Command::Auto { .. }
                | Command::Manual { .. }
                | Command::StopForward
                | Command::Status
                | Command::Rpc
                | Command::Jobs
                | Command::Cancel { .. }
        )
    );
    ensure!(
        !persistent || cli.session.is_some(),
        "this command needs --session NAME; open `capturefab gui --name NAME` or `capturefab serve --name NAME` first"
    );
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
    ensure!(
        !camera_required || cli.camera.is_some() || cli.session.is_some(),
        "choose --camera ID or --session NAME (discover IDs with capturefab discover)"
    );
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
            codec,
            encoder,
            fps,
            bitrate,
            duration,
            max_file_size,
            destination: saved,
            storage,
            auto,
        } => {
            let until = duration
                .as_deref()
                .map(crate::scheduling::parse_duration)
                .transpose()?
                .map(Duration::from_millis);
            auto.apply(&client)?;
            let result = client.call(SessionCommand::Forward {
                output: destination.clone(),
                codec: codec.clone(),
                encoder: encoder.clone(),
                fps: *fps,
                bitrate: bitrate.clone(),
                storage: storage.policy()?,
                max_file_bytes: crate::storage::parse_bytes(max_file_size)?
                    .min(storage.policy()?.max_bytes),
                destination: saved.clone(),
            })?;
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
                client.call(SessionCommand::StopForward)?;
                let mut result = result;
                wait_for_uploads(&cli, saved.as_deref(), &mut result)?;
                output(&cli, result)?;
                return Ok(());
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
        Command::Status => client.call(SessionCommand::Status)?,
        Command::Capture {
            output: destination,
            count,
            format,
            settings,
            at,
            delay,
            interval,
            destination: saved,
            storage,
            auto,
        } => {
            ensure!(
                destination != "-" || (cli.session.is_none() && !cli.json && *count == 1),
                "binary stdout needs one frame, a direct --camera, and no --json"
            );
            let settings = settings
                .iter()
                .map(|s| assignment(s))
                .collect::<Result<Vec<_>>>()?;
            for (feature, value) in settings {
                client.set(feature, value)?;
            }
            auto.apply(&client)?;
            let policy = storage.policy()?;
            if at.is_some() || delay.is_some() || interval.is_some() {
                ensure!(
                    destination != "-",
                    "scheduled capture requires a file or directory"
                );
                let first_at_ms = if let Some(at) = at {
                    crate::scheduling::parse_at(at)?
                } else {
                    crate::scheduling::now_ms().saturating_add(
                        delay
                            .as_deref()
                            .map(crate::scheduling::parse_duration)
                            .transpose()?
                            .unwrap_or(0),
                    )
                };
                let result = client.call(SessionCommand::Schedule {
                    output: destination.clone(),
                    count: *count,
                    timeout_ms: cli.timeout_ms,
                    format: format.name().into(),
                    first_at_ms,
                    interval_ms: interval
                        .as_deref()
                        .map(crate::scheduling::parse_duration)
                        .transpose()?
                        .unwrap_or(0),
                    storage: policy,
                    destination: saved.clone(),
                })?;
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
            let result = client.call(SessionCommand::Capture {
                output: destination.clone(),
                count: *count,
                timeout_ms: cli.timeout_ms,
                format: format.name().into(),
                storage: policy,
                destination: saved.clone(),
            })?;
            if destination == "-" {
                let frame = client
                    .local
                    .as_ref()
                    .unwrap()
                    .latest_frame()
                    .context("capture returned no frame")?;
                io::stdout()
                    .lock()
                    .write_all(&crate::frame::encode(&frame, format.name())?)?;
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
        Command::Rpc => {
            use std::io::Read;
            let mut data = String::new();
            io::stdin()
                .take(1024 * 1024 + 1)
                .read_to_string(&mut data)?;
            ensure!(data.len() <= 1024 * 1024, "RPC stdin exceeds 1 MiB");
            let command: SessionCommand = serde_json::from_str(&data)
                .context("stdin must be a SessionCommand JSON object; see capturefab schema")?;
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
        return write_json(&json!({"version":ipc::VERSION,"ok":true,"result":value}));
    }
    let mut out = io::stdout().lock();
    if let Some(devices) = value["devices"].as_array() {
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
                f.get("value").unwrap_or(&Value::Null),
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
    json!({"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"features":{"gui":cfg!(feature="gui"),"usb":cfg!(feature="usb"),"wgpu":cfg!(feature="gui"),"jpeg":cfg!(feature="jpeg"),"nvjpeg":cfg!(feature="nvjpeg"),"vaapi":cfg!(feature="vaapi"),"videotoolbox":cfg!(feature="videotoolbox")},"jpeg":jpeg_backends(),"session_directory":ipc::session_dir(),"interfaces":interfaces,"runtime":"No Aravis, libusb or vendor SDK required. Native OS graphics/USB drivers required; NVIDIA CUDA and nvJPEG, and libva with a JPEG-capable driver, are used when installed; Apple silicon uses its JPEG engine through VideoToolbox.","usb_access":if cfg!(target_os="windows"){"USB3 camera interfaces must use WinUSB"}else if cfg!(target_os="linux"){"Read/write permission on camera /dev/bus/usb node required"}else{"IOKit camera interface must be available to userspace"}})
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
fn schema() -> Value {
    let mut contract = json!({"version":ipc::VERSION,"program":"capturefab","default":"gui","json_result":{"version":1,"ok":true,"result":"command-specific JSON"},"json_error":{"version":1,"ok":false,"error":{"code":"category","message":"description"}},"exit_codes":{"0":"success (also closed stdout pipe)","1":"operation or I/O failure","2":"invalid CLI usage","3":"camera/session unavailable","4":"timeout","5":"unsupported feature/format"},"selection":{"direct":"--camera ID|serial|IPv4|sim:0","existing_session":"--session NAME","environment":"CAPTUREFAB_SESSION, CAPTUREFAB_SESSION_DIR"},"limits":{"timeout_ms":[1,60000],"count":[1,100000],"rpc_bytes":1048576,"payload_bytes":268435456},"rpc":{"transport":"authenticated loopback TCP; one newline-delimited request/response per connection","client":"capturefab --session NAME rpc < command.json","command_tag":"op","commands":[{"op":"status"},{"op":"discover","timeout_ms":1000,"simulated":false},{"op":"connect","camera":"sim:0","timeout_ms":2000},{"op":"disconnect"},{"op":"features"},{"op":"get","feature":"Width"},{"op":"set","feature":"ExposureTime","value":"5000"},{"op":"execute","feature":"TriggerSoftware"},{"op":"start"},{"op":"stop"},{"op":"capture","output":"frames","count":10,"timeout_ms":2000,"format":"png"},{"op":"capture","output":"run-1","count":10,"timeout_ms":2000,"format":"png","destination":"archive"},{"op":"xml"},{"op":"read_memory","address":256,"length":4},{"op":"write_memory","address":256,"data":[0,0,2,128]}]},"capture":{"formats":["png","jpeg","raw","pgm","ppm"],"paths":"one frame: file; multiple: directory or {frame} template; existing files fail","stdout":"--camera ... capture -n 1 -o - --format raw; no --json","destination":"--destination NAME (or \"destination\" in capture/schedule/forward RPC): -o is a relative name inside a saved folder or S3 bucket; see destination list, volumes and uploads"},"writes":"Multiple set assignments apply sequentially; failures may leave earlier assignments applied. Session operations are serialized with GUI operations."});
    contract["exit_codes"]["6"] = json!("storage quota or disk full");
    contract["selection"]["direct"] = json!(
        "--camera ID|serial|IPv4|sim:NAME|RTSP/SRT/media URI|avfoundation:INDEX|v4l2:/dev/videoN|dshow:video=NAME|onvif:ENDPOINT"
    );
    contract["selection"]["existing_session"] =
        json!("--session NAME [--camera ID] targets an existing visible GUI or headless worker");
    contract["limits"]["active_jobs_per_camera"] = json!(32);
    contract["limits"]["camera_workers_default"] = json!(16);
    contract["limits"]["shared_memory_slots_per_camera"] = json!(3);
    contract["storage"] = json!({"default":crate::storage::StoragePolicy::default(),"scope":"All Capturefab-owned capture and recording files across output directories in this user's session directory. Reservations count toward quota. Existing/unowned/modified files are never deleted.","recording_file_cap_default":crate::media::default_recording_cap(),"on_full":["stop","delete-oldest"],"status":"capturefab storage --json","error":"storage_full; exit 6"});
    contract["scheduling"] = json!({"cli":"capture --at RFC3339 | --delay 30s; --interval 10s -n 100","lifetime":"Schedules run while the GUI, serve session or direct CLI process remains running; jobs are not persisted across exit.","interval":"First time plus captured count times interval; no unbounded backlog. Each job has a per-frame transport timeout.","states":["pending","running","complete","cancelled","failed"]});
    contract["auto"] = json!({"cli":"--session NAME auto [--balance B] | manual [--revert]; capture|forward|serve --auto [--balance B]","default":"manual; connecting never enables auto mode and it ends on disconnect","balance":"0 favors image quality, 1 favors frame rate; quality, balanced and frame-rate mean 0, 0.5 and 1; omitted keeps the current value, initially 0.5","status":"status.auto and cameras[].auto; null in manual mode","strategies":["firmware","software","video-mode","none"],"states":["waiting","converging","stable","limited"],"settled":["stable","limited"],"manual":"Setting a feature in auto.managed switches that camera to manual first and the set result adds \"auto\":null; manual turns ExposureAuto, GainAuto and BalanceWhiteAuto off so values hold","persistence":"Camera settings written by auto mode remain after manual, like set; manual --revert, disconnect and session exit restore their previous values (best effort)","capture":"--set applies before --auto; capture saves once exposure settles or after min(timeout_ms/2, 3 s) and result.auto reports the state; with --session auto mode stays on afterwards"});
    let commands = contract["rpc"]["commands"].as_array_mut().unwrap();
    commands.extend([json!({"op":"select","camera":"sim:0"}),json!({"op":"jobs"}),json!({"op":"cancel_job","id":1}),json!({"op":"schedule","output":"timelapse","count":10,"timeout_ms":2000,"format":"png","first_at_ms":crate::scheduling::now_ms()+60000,"interval_ms":10000,"storage":crate::storage::StoragePolicy::default()}),json!({"op":"forward","output":"rtsp://localhost:8554/camera","codec":"h264","encoder":"auto","fps":30,"bitrate":"4M","storage":crate::storage::StoragePolicy::default(),"max_file_bytes":crate::media::default_recording_cap()}),json!({"op":"stop_forward"}),json!({"op":"auto","balance":0.5}),json!({"op":"manual"})]);
    contract
}

/// A direct CLI capture to a bucket destination uploads before exiting, so
/// files are not left waiting for a process that never runs.
fn wait_for_uploads(cli: &Cli, destination: Option<&str>, result: &mut Value) -> Result<()> {
    let Some(name) = destination else {
        return Ok(());
    };
    if cli.session.is_some()
        || !matches!(
            crate::destination::get(name)?.target,
            crate::destination::Target::S3(_)
        )
    {
        return Ok(());
    }
    #[cfg(not(feature = "s3"))]
    let _ = result;
    #[cfg(feature = "s3")]
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

fn destination_command(command: &DestinationCommand) -> Result<Value> {
    use crate::destination::{self, Bucket, Credentials, Destination, Target};
    let describe = |d: &Destination| {
        let mut value = serde_json::to_value(d).unwrap_or_default();
        value["location"] = json!(d.describe());
        #[cfg(feature = "s3")]
        if let Target::S3(Bucket {
            credentials: Credentials::Keychain { access_key_id },
            ..
        }) = &d.target
        {
            value["secret_saved"] = json!(crate::s3::has_secret(access_key_id));
        }
        value
    };
    Ok(match command {
        DestinationCommand::List => {
            json!({"destinations": destination::list()?.iter().map(describe).collect::<Vec<_>>()})
        }
        DestinationCommand::AddFolder { name, path } => {
            let path = std::path::absolute(path)?;
            crate::volumes::ensure_present(&path)?;
            std::fs::create_dir_all(&path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            let saved = Destination {
                name: name.clone(),
                target: Target::Folder { path },
            };
            destination::save(saved.clone())?;
            json!({"destination": describe(&saved)})
        }
        DestinationCommand::AddS3 {
            name,
            bucket,
            region,
            endpoint,
            prefix,
            path_style,
            access_key_id,
            secret_stdin,
            profile,
            env,
            keep_local,
            no_check,
        } => {
            let credentials = match (access_key_id, profile, env) {
                (Some(id), _, _) => Credentials::Keychain {
                    access_key_id: id.clone(),
                },
                (_, Some(name), _) => Credentials::Profile { name: name.clone() },
                (_, _, true) => Credentials::Environment,
                _ => anyhow::bail!(
                    "choose credentials: --access-key-id ID --secret-stdin, --profile NAME or --env"
                ),
            };
            let saved = Destination {
                name: name.clone(),
                target: Target::S3(Bucket {
                    endpoint: endpoint.clone(),
                    region: region.clone(),
                    bucket: bucket.clone(),
                    prefix: prefix.clone(),
                    path_style: *path_style,
                    credentials,
                    keep_local: *keep_local,
                }),
            };
            saved.validate()?;
            #[cfg(not(feature = "s3"))]
            {
                let _ = (secret_stdin, no_check);
                anyhow::bail!(
                    "S3 destinations are unsupported in this build; rebuild with --features s3"
                );
            }
            #[cfg(feature = "s3")]
            {
                if *secret_stdin {
                    let mut secret = String::new();
                    io::stdin().read_line(&mut secret)?;
                    crate::s3::store_secret(access_key_id.as_deref().unwrap_or(""), secret.trim())?;
                }
                let Target::S3(bucket) = &saved.target else {
                    unreachable!()
                };
                let check = if *no_check {
                    None
                } else {
                    Some(crate::s3::Client::new(bucket).and_then(|client| client.check()))
                };
                if let Some(Err(error)) = &check
                    && crate::s3::retryable(error)
                {
                    anyhow::bail!(
                        "cannot reach the bucket: {error:#}; fix the settings or save with --no-check"
                    );
                }
                destination::save(saved.clone())?;
                let mut value = json!({"destination": describe(&saved)});
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
            #[cfg(feature = "s3")]
            if *forget_secret
                && let Target::S3(Bucket {
                    credentials: Credentials::Keychain { access_key_id },
                    ..
                }) = &removed.target
            {
                crate::s3::delete_secret(access_key_id)?;
            }
            #[cfg(not(feature = "s3"))]
            let _ = forget_secret;
            json!({"removed": removed.name})
        }
        DestinationCommand::Test { name } => {
            let saved = destination::get(name)?;
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
                        anyhow::bail!("S3 destinations are unsupported in this build")
                    }
                }
            }
        }
    })
}

fn uploads_command(command: Option<&UploadsCommand>) -> Result<Value> {
    #[cfg(feature = "s3")]
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
    #[cfg(not(feature = "s3"))]
    {
        let _ = command;
        anyhow::bail!("uploads are unsupported in this build; rebuild with --features s3")
    }
}

pub fn error_code(error: &anyhow::Error) -> (&'static str, i32) {
    let s = format!("{error:#}").to_ascii_lowercase();
    if s.contains("storage full") || s.contains("no space left") {
        ("storage_full", 6)
    } else if s.contains("connection refused") {
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
        }
        assert!(
            ["auto", "manual"]
                .iter()
                .all(|op| commands.iter().any(|c| c["op"] == *op))
        );
        assert_eq!(contract["auto"]["settled"], json!(["stable", "limited"]));
    }
    #[test]
    fn error_codes() {
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
