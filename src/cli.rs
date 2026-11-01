//! The command line: global flags, dispatch, and how a camera command
//! reaches its camera (a session that holds it, or a worker of its own).
mod args;
mod auto;
mod camera;
mod capture;
mod debug;
mod doctor;
mod inspect;
mod jobs;
pub mod login;
mod network;
mod onvif;
pub(crate) mod output;
pub(crate) mod paths;
mod preset;
mod record;
mod schema;
mod serve;
mod session;
mod signals;

#[cfg(test)]
pub(crate) use output::error_code;

use crate::{
    ipc,
    session::{SessionCommand, SessionHandle},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use output::{Output, Report};
use serde_json::{Value, json};
use std::time::Duration;

const OVERVIEW: &str = "Capturefab: camera capture for people and agents.

Usage: capturefab [--json] COMMAND [-c CAMERA] ...

  capturefab list                       cameras on this computer and network
  capturefab info -c sim:0              what a camera is and its controls
  capturefab set -c sim:0 exposure=5ms  change settings as one atomic change
  capturefab capture -c sim:0 -o shot.png

Agents: use --json (one document on stdout, typed exit codes); `capturefab schema` describes every command.
More: capturefab --help, capturefab COMMAND --help";

#[derive(Debug, Parser)]
#[command(
    name = "capturefab",
    bin_name = "capturefab",
    version,
    about = "Camera capture for people and agents: list, configure, capture and record cameras.",
    long_about = "Camera capture for people and agents: list, configure, capture and record cameras.\n\nWith --json every command prints exactly one JSON document on stdout: the result when it exits 0, {\"error\":{code,message,hint?,details?}} otherwise. Exit codes: 0 ok, 1 failed, 2 usage, 3 unavailable, 4 timeout, 5 unsupported, 6 storage full, 130 interrupted. `capturefab schema` describes every command, its arguments, results and errors.",
    after_help = "Examples:\n  capturefab list --kind sim\n  capturefab info -c sim:0\n  capturefab set -c sim:0 exposure=5ms gain=3\n  capturefab --json get -c sim:0 ExposureTime TriggerMode[FrameStart]",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Flags every command takes, before or after its name.
#[derive(Debug, Clone, Default, Args)]
pub struct Global {
    /// One JSON document on stdout: the result, or {"error":{...}} with a non-zero exit
    #[arg(
        long,
        global = true,
        env = "CAPTUREFAB_JSON",
        action = clap::ArgAction::SetTrue,
        value_parser = clap::builder::FalseyValueParser::new()
    )]
    pub json: bool,
    /// No progress or notes on stderr; errors still print
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,
    /// More detail on stderr, including camera worker logs
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
    /// Use this running session (session start or serve --name NAME)
    #[arg(long, global = true, value_name = "NAME", env = "CAPTUREFAB_SESSION")]
    pub session: Option<String>,
    /// Never use a running session: open the camera in a worker of this command
    #[arg(long, global = true)]
    pub no_session: bool,
    /// The session came from CAPTUREFAB_SESSION, not --session: a default
    /// that --no-session and binary stdout override.
    #[arg(skip)]
    pub session_from_env: bool,
    /// The command's --keep came from CAPTUREFAB_KEEP: a default that
    /// --no-session and binary stdout override.
    #[arg(skip)]
    pub keep_from_env: bool,
}
impl Global {
    /// Let `--no-session` override a CAPTUREFAB_SESSION default; both typed
    /// is a mistake.
    pub fn settle_session(&mut self, typed: bool) -> Result<()> {
        if self.session.is_some() && self.no_session {
            if typed {
                return Err(Usage(
                    "--session and --no-session contradict each other; pass one".into(),
                )
                .into());
            }
            self.session = None;
        }
        self.session_from_env = self.session.is_some() && !typed;
        Ok(())
    }
    pub fn output(&self) -> Output {
        Output {
            json: self.json,
            quiet: self.quiet,
            verbose: self.verbose,
        }
    }
}

// Parsed once per process; boxing would only obscure the clap tree.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List cameras: GigE Vision, USB3 Vision, webcams, ONVIF
    #[command(
        visible_alias = "ls",
        after_help = "Examples:\n  capturefab list\n  capturefab list --kind gige,u3v --timeout 3s\n  capturefab list --kind sim\n  capturefab --json list"
    )]
    List(camera::ListArgs),
    /// What a camera is and what it can do: size, format, exposure, gain, trigger, controls
    #[command(
        after_help = "Examples:\n  capturefab info -c sim:0\n  capturefab --json info -c gige:00c0df123456\n  capturefab info -c sim:0 --keep 5m"
    )]
    Info(camera::InfoArgs),
    /// List a camera's features with values, ranges, choices and locks
    #[command(
        after_help = "Examples:\n  capturefab features -c sim:0\n  capturefab features -c sim:0 Trigger\n  capturefab features -c sim:0 'Exposure*' --all\n  capturefab features -c sim:0 --category ImageFormatControl"
    )]
    Features(camera::FeaturesArgs),
    /// Read features or controls in one call
    #[command(
        after_help = "Examples:\n  capturefab get -c sim:0 ExposureTime Gain\n  capturefab get -c sim:0 exposure trigger roi\n  capturefab get -c sim:0 TriggerMode[FrameStart]\n  capturefab get -c sim:0 -c sim:1 Width"
    )]
    Get(camera::GetArgs),
    /// Change features or controls as one atomic change; a failure restores every value
    #[command(
        after_help = "Examples:\n  capturefab set -c sim:0 exposure=5ms gain=3\n  capturefab set -c sim:0 ExposureTime=10ms Width=640\n  capturefab set -c sim:0 TriggerMode[FrameStart]=On trigger=line0:rising\n  capturefab set -c sim:0 roi=0,0,320,240 --dry-run"
    )]
    Set(camera::SetArgs),
    /// Run a command feature, such as UserSetSave[UserSet1]
    #[command(
        after_help = "Examples:\n  capturefab exec -c sim:0 TriggerSoftware[FrameStart]\n  capturefab exec -c gige:00c0df123456 UserSetSave[UserSet1]"
    )]
    Exec(camera::ExecArgs),
    /// Save frames: one shot, a burst, a time lapse or a delayed capture; never overwrites
    #[command(
        after_help = "Examples:\n  capturefab capture -c sim:0 -o shot.png\n  capturefab capture -c sim:0 -n 10 -o frames/\n  capturefab capture -c sim:0 --every 10s --for 1h -o 'lapse/{seq}.jpg' --background\n  capturefab capture -c sim:0 -o - > frame.png"
    )]
    Capture(capture::CaptureArgs),
    /// Record video to a file or stream it to rtsp://, srt://, rtmp:// or udp://
    #[command(
        after_help = "Examples:\n  capturefab record -c sim:0 -o clip.mp4 --for 30s\n  capturefab record -c sim:0 -o clip.mkv --codec ffv1 --for 5m\n  capturefab record -c sim:0 -o rtsp://localhost:8554/cam --background"
    )]
    Record(record::RecordArgs),
    /// Measure a fresh frame: exposure, clipping, focus, suggested exposure, a preview
    #[command(
        after_help = "Examples:\n  capturefab inspect -c sim:0\n  capturefab inspect -c sim:0 --set exposure=20ms --region 0.25,0.25,0.5,0.5\n  capturefab inspect -c sim:0 --keep 5m --preview none"
    )]
    Inspect(inspect::InspectArgs),
    /// Settle exposure and gain, then hold them; or turn auto exposure off or on
    #[command(
        after_help = "Examples:\n  capturefab auto -c sim:0\n  capturefab auto -c sim:0 --timeout 10s\n  capturefab auto -c sim:0 --off"
    )]
    Auto(auto::AutoArgs),
    /// Saved camera settings (host-side, portable between cameras of a model)
    #[command(
        subcommand,
        after_help = "A preset lives on this computer and can be applied to any camera of the model; a user set (exec UserSetSave[UserSet1], or preset apply --persist) lives in the camera.\n\nExamples:\n  capturefab preset save bench -c sim:0\n  capturefab preset apply bench -c sim:0 --dry-run\n  capturefab preset list"
    )]
    Preset(preset::PresetCommand),
    /// Captures and recordings running in sessions: list, stop, wait
    #[command(
        subcommand_required = false,
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  capturefab jobs\n  capturefab jobs wait auto-1 --timeout 10m\n  capturefab jobs stop all"
    )]
    Jobs(jobs::JobsArgs),
    /// Sessions keep cameras open between commands: start one in the background, stop it, see it
    #[command(
        subcommand,
        after_help = "Commands use a session that holds their camera on their own; --keep 5m starts one for interactive loops.\n\nExamples:\n  capturefab session start -c sim:0 --live\n  capturefab session status\n  capturefab session stop"
    )]
    Session(session::SessionAction),
    /// Run a session in the foreground (systemd, a terminal); Ctrl-C stops it
    #[command(
        after_help = "Examples:\n  capturefab serve --name bench -c sim:0\n  capturefab serve -c gige:00c0df123456 --live"
    )]
    Serve(serve::ServeArgs),
    /// Save camera sign-ins for rtsp:// and ONVIF cameras in the OS credential store
    #[command(
        subcommand,
        after_help = "Examples:\n  printf '%s\\n' \"$PASS\" | capturefab login add rtsp://cam.local/stream --user admin --password-stdin\n  capturefab login list\n  capturefab login rm rtsp://cam.local/stream"
    )]
    Login(login::LoginCommand),
    /// Resolve an ONVIF camera's RTSP streams
    #[command(
        subcommand,
        after_help = "Examples:\n  capturefab onvif resolve cam.local\n  capturefab onvif resolve cam.local --profile Profile_1"
    )]
    Onvif(onvif::OnvifCommand),
    /// Check GigE network paths; --tune applies bandwidth sharing to cameras
    #[command(
        after_help = "Examples:\n  capturefab network\n  capturefab network --tune -c gige:00c0df123456 -c gige:00c0df654321"
    )]
    Network(network::NetworkArgs),
    /// Check this computer: FFmpeg, folders, USB and GigE limits, with a fix for each warning
    #[command(after_help = "Examples:\n  capturefab doctor\n  capturefab --json doctor")]
    Doctor,
    /// Describe commands, arguments, results and error codes as JSON
    #[command(
        after_help = "Examples:\n  capturefab schema\n  capturefab schema set\n  capturefab schema preset apply"
    )]
    Schema {
        /// A command, such as set or preset apply
        command: Vec<String>,
    },
    /// Print shell completions: bash, zsh, fish, powershell, elvish
    #[command(
        after_help = "Examples:\n  capturefab completions zsh > ~/.zfunc/_capturefab\n  capturefab completions bash > ~/.local/share/bash-completion/completions/capturefab"
    )]
    Completions { shell: clap_complete::Shell },
    /// Protocol tools: GenICam XML, raw registers, one session request
    #[command(
        subcommand,
        after_help = "Examples:\n  capturefab debug xml -c sim:0 > camera.xml\n  capturefab debug read -c sim:0 0x100 4\n  echo '{\"op\":\"status\"}' | capturefab --session bench debug rpc"
    )]
    Debug(debug::DebugCommand),
    /// Print Capturefab's license and the bundled FFmpeg's license notice
    #[command(after_help = "Examples:\n  capturefab licenses\n  capturefab --json licenses")]
    Licenses,
}

/// Run the command line; the process exit code.
pub fn main() -> i32 {
    let args = std::env::args_os().collect::<Vec<_>>();
    let json = output::json_requested(&args);
    let mut command = Cli::command();
    let matches = match command.try_get_matches_from_mut(&args) {
        Ok(matches) => matches,
        Err(error) => return clap_failure(&error, json),
    };
    let mut cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return clap_failure(&error.format(&mut command), json),
    };
    let out = cli.global.output();
    let typed = matches.value_source("session") == Some(clap::parser::ValueSource::CommandLine);
    if let Err(error) = cli.global.settle_session(typed) {
        return out.error(&error);
    }
    cli.global.keep_from_env = keep_from_env(&matches);
    if let Err(error) = signals::install(out) {
        return out.error(&error);
    }
    match run(cli) {
        Ok(Some(report)) => out.result(report),
        Ok(None) => 0,
        Err(error) => out.error(&error),
    }
}

/// Whether the command's `--keep` came from CAPTUREFAB_KEEP.
fn keep_from_env(matches: &clap::ArgMatches) -> bool {
    let mut matches = matches;
    while let Some((_, sub)) = matches.subcommand() {
        matches = sub;
    }
    matches.ids().any(|id| id == "keep")
        && matches.value_source("keep") == Some(clap::parser::ValueSource::EnvVariable)
}

fn clap_failure(error: &clap::Error, json: bool) -> i32 {
    use clap::error::ErrorKind as Clap;
    if matches!(error.kind(), Clap::DisplayHelp | Clap::DisplayVersion) {
        let _ = error.print();
        return 0;
    }
    if json {
        let _ = output::write_json(&output::clap_error(error, missing_subcommand()));
    } else {
        let _ = error.print();
    }
    2
}

/// The deepest command group the arguments name, with its subcommands:
/// what a missing-subcommand error lists.
fn missing_subcommand() -> Option<(String, Vec<String>)> {
    let mut command = Cli::tree();
    let mut path = vec!["capturefab".to_owned()];
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--session" {
            args.next();
        }
        if arg.starts_with('-') {
            continue;
        }
        match command.find_subcommand(&arg) {
            Some(sub) => {
                path.push(sub.get_name().to_owned());
                command = sub.clone();
            }
            None => break,
        }
    }
    let subcommands = command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| sub.get_name().to_owned())
        .collect::<Vec<_>>();
    (!subcommands.is_empty()).then(|| (path.join(" "), subcommands))
}

/// Run a parsed command. `None` when it printed its own output.
pub fn run(mut cli: Cli) -> Result<Option<Report>> {
    if let Some(session) = &cli.global.session {
        ipc::validate_name(session).map_err(|error| Usage(format!("{error:#}")))?;
    }
    let Some(command) = cli.command.take() else {
        if cli.global.json {
            return Err(Categorized::new(ErrorKind::Usage, "choose a command")
                .hint("`capturefab schema` lists the commands")
                .into());
        }
        println!("{OVERVIEW}");
        return Ok(None);
    };
    let global = &cli.global;
    Ok(Some(match command {
        Command::List(args) => camera::list(global, args)?,
        Command::Info(args) => camera::info(global, args)?,
        Command::Features(args) => camera::features(global, args)?,
        Command::Get(args) => camera::get(global, args)?,
        Command::Set(args) => camera::set(global, args)?,
        Command::Exec(args) => camera::exec(global, args)?,
        Command::Capture(args) => return capture::run(global, args),
        Command::Record(args) => record::run(global, args)?,
        Command::Inspect(args) => inspect::run(global, args)?,
        Command::Auto(args) => auto::run(global, args)?,
        Command::Preset(command) => preset::run(global, command)?,
        Command::Jobs(args) => jobs::run(global, args)?,
        Command::Session(command) => session::run(global, command)?,
        Command::Serve(args) => return serve::run(global, args),
        Command::Login(command) => login::run(global, &command)?,
        Command::Onvif(command) => onvif::run(global, command)?,
        Command::Network(args) => network::run(global, args)?,
        Command::Doctor => doctor::doctor(global),
        Command::Schema { command } => schema::run(global, &command)?,
        Command::Completions { shell } => {
            if !global.json {
                clap_complete::generate(
                    shell,
                    &mut Cli::command(),
                    "capturefab",
                    &mut std::io::stdout(),
                );
                return Ok(None);
            }
            // One document in JSON mode: the script as a string.
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "capturefab", &mut script);
            Report::json(json!({
                "shell": shell.to_string(),
                "script": String::from_utf8_lossy(&script),
            }))
        }
        Command::Debug(command) => return debug::run(global, command),
        Command::Licenses => doctor::licenses(),
    }))
}

/// How long opening a camera may take.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long finding the cameras of a default `-c` waits.
const DEFAULT_DISCOVERY: Duration = Duration::from_secs(1);
/// The session `--keep` and `--background` start when none holds the camera.
pub(crate) const AUTO_SESSION: &str = "auto";
/// How long a session started for `--background` alone stays once its jobs
/// are done.
const BACKGROUND_IDLE: Duration = Duration::from_secs(60);

/// What a camera command asks of its route besides the global flags.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Route {
    /// `--keep DUR`: run through a session that holds the camera and stays
    /// at least this long after its last use.
    pub keep: Option<Duration>,
    /// `--background`: the command leaves a job running in a session.
    pub background: bool,
    /// How long opening the camera may take, when longer than usual:
    /// capture's and record's `--timeout`, for slow streams.
    pub connect: Option<Duration>,
    /// `capture -o -`: a worker of this command writes the image to a pipe
    /// this command reads.
    pub stdout: bool,
}
impl Route {
    pub fn keep(keep: Option<Duration>) -> Self {
        Self {
            keep,
            ..Self::default()
        }
    }
    /// A route whose camera may take up to `wait` (none: a minute) to open.
    pub fn waiting(mut self, wait: args::Wait) -> Self {
        let wait = match wait {
            args::Wait::For(wait) => wait,
            args::Wait::Forever => Duration::from_secs(60),
        };
        self.connect = Some(wait.clamp(CONNECT_TIMEOUT, Duration::from_secs(60)));
        self
    }
    fn connect(self) -> u64 {
        millis(self.connect.unwrap_or(CONNECT_TIMEOUT))
    }
    /// A session must hold the camera.
    fn needs_session(self) -> bool {
        self.keep.is_some() || self.background
    }
    /// How long a session started for this command waits unused.
    fn idle(self) -> Duration {
        self.keep.unwrap_or(BACKGROUND_IDLE)
    }
}

/// A camera command's connection: a running session that holds (or now
/// holds) the camera, or a worker of this command.
pub(crate) struct Client {
    pub out: Output,
    session: Option<String>,
    /// The camera as the session is asked for it: the selector typed, which
    /// the session resolves, or none for the session's only camera.
    target: Option<String>,
    local: Option<SessionHandle>,
    /// `--keep`: sent with each request that uses the session.
    keep: Option<Duration>,
    /// The pipe this command's worker writes an image captured to stdout to.
    stdout: Option<std::io::PipeReader>,
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(local) = &self.local {
            if self.out.verbose {
                for line in local.snapshot().logs {
                    eprintln!("capturefab: [{}] {}", line.level, line.message);
                }
            }
            local.shutdown();
        }
    }
}
/// Why a session could not take a command: it is stopping or gone, which
/// lets automatic routing use a worker of this command, or it failed.
enum Unusable {
    Gone(anyhow::Error),
    Failed(anyhow::Error),
}
impl Unusable {
    fn error(self) -> anyhow::Error {
        match self {
            Self::Gone(error) | Self::Failed(error) => error,
        }
    }
}
impl Client {
    /// Open `selector`, or the default camera, through the route the global
    /// flags choose.
    pub fn open(global: &Global, selector: Option<&str>) -> Result<Self> {
        Self::open_with(global, selector, Route::default())
    }
    /// Open `selector` as `route` asks: through `--session`, a running
    /// session that holds the camera, or for `--keep`/`--background` one
    /// started for it; else a worker of this command.
    pub fn open_with(global: &Global, selector: Option<&str>, route: Route) -> Result<Self> {
        let selector = selector.map(caller_selector).transpose()?;
        let out = global.output();
        // CAPTUREFAB_KEEP is a default, which --no-session overrides.
        let route = if global.no_session && global.keep_from_env {
            Route {
                keep: None,
                ..route
            }
        } else {
            route
        };
        if route.needs_session() && global.no_session {
            return Err(Usage(
                "--keep and --background run through a session; drop --no-session".into(),
            )
            .into());
        }
        if let Some(name) = &global.session {
            if route.needs_session() {
                session::ensure(name, selector.as_deref(), route.idle(), global)?;
            }
            return Self::in_session(out, name, selector, route).map_err(Unusable::error);
        }
        if !global.no_session
            && let Some(name) = route_to(&ipc::sessions(), selector.as_deref())
        {
            match Self::in_session(out, &name, selector.clone(), route) {
                Ok(client) => return Ok(client),
                // A session that is stopping, or gone, leaves the camera to
                // this command; one that holds it and failed says why.
                Err(Unusable::Gone(_)) if !route.needs_session() => {}
                Err(unusable) => return Err(unusable.error()),
            }
        }
        if route.needs_session() {
            let selector = match selector {
                Some(selector) => selector,
                None => default_camera()?,
            };
            session::ensure(AUTO_SESSION, Some(&selector), route.idle(), global)?;
            return Self::in_session(out, AUTO_SESSION, Some(selector), route)
                .map_err(Unusable::error);
        }
        let selector = match selector {
            Some(selector) => selector,
            None => default_camera()?,
        };
        crate::camera::validate_selector(&selector)?;
        let local = SessionHandle::direct(out.verbose);
        signals::on_second(local.killer());
        let stdout = route.stdout.then(|| local.stdout_pipe()).transpose()?;
        let client = Self {
            out,
            session: None,
            target: None,
            local: Some(local.clone()),
            keep: None,
            stdout,
        };
        client.call(SessionCommand::connect(selector, route.connect()))?;
        Ok(client)
    }
    /// Use session `name` for `selector`, connecting the camera there when
    /// it does not hold it yet; otherwise a status request checks that the
    /// session still runs and holds it.
    fn in_session(
        out: Output,
        name: &str,
        selector: Option<String>,
        route: Route,
    ) -> std::result::Result<Self, Unusable> {
        let keep = route.keep;
        let unusable = |error: anyhow::Error| {
            if session_gone(&error) {
                Unusable::Gone(error)
            } else {
                Unusable::Failed(error)
            }
        };
        let descriptor = ipc::read_descriptor(name).map_err(unusable)?;
        let held = selector.as_deref().is_none_or(|selector| {
            descriptor
                .cameras
                .iter()
                .any(|camera| camera.named_by(selector).is_some())
        });
        let mut client = Self {
            out,
            session: Some(name.to_owned()),
            target: selector.clone(),
            local: None,
            keep,
            stdout: None,
        };
        let (target, first) = match (&selector, held) {
            (Some(selector), false) => (
                None,
                SessionCommand::connect(selector.clone(), route.connect()),
            ),
            _ => (client.target.as_deref(), SessionCommand::Status),
        };
        let reply = ipc::call_keeping(name, target, first.clone(), default_timeout(&first), keep)
            .map_err(unusable)?;
        // Without -c, the session's only camera, named from now on so a
        // camera connected later does not change what this command uses.
        if selector.is_none() {
            let cameras = reply["cameras"].as_array().cloned().unwrap_or_default();
            client.target = Some(only_camera(name, &cameras).map_err(Unusable::Failed)?);
        }
        Ok(client)
    }
    pub fn call(&self, command: SessionCommand) -> Result<Value> {
        command.validate()?;
        match (&self.session, &self.local) {
            (Some(session), _) => {
                // Looking does not keep a session; using it does.
                let keep = self
                    .keep
                    .filter(|_| !matches!(command, SessionCommand::Status));
                ipc::call_keeping(
                    session,
                    self.target.as_deref(),
                    command.clone(),
                    default_timeout(&command),
                    keep,
                )
            }
            (None, Some(local)) => local.request(command),
            (None, None) => unreachable!("a client has a session or a worker"),
        }
    }
    /// The camera's entry in a session status, as `Status` reports it.
    pub fn status(&self) -> Result<Value> {
        let state = self.call(SessionCommand::Status)?;
        let cameras = state["cameras"].as_array().cloned().unwrap_or_default();
        match cameras.as_slice() {
            [camera] => Ok(camera.clone()),
            _ => Err(Categorized::new(
                ErrorKind::Disconnected,
                "the camera is no longer connected",
            )
            .into()),
        }
    }
    /// The session this client uses, if any.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }
    /// Where this client's jobs run.
    pub fn host(&self) -> JobHost {
        match (&self.session, &self.local) {
            (Some(session), _) => JobHost::Session(session.clone()),
            (None, Some(local)) => JobHost::Direct(local.clone()),
            (None, None) => unreachable!("a client has a session or a worker"),
        }
    }
    /// Job `id` once it changed after `seq` or ended, waiting at most
    /// `wait`: `{seq, job, detail}`.
    pub fn wait_job(&self, id: &str, seq: u64, wait: Duration) -> Result<Value> {
        self.host().wait(id, seq, wait)
    }
    /// Stop job `id` when a signal arrives, as one that came already does.
    pub fn stop_on_signal(&self, id: &str) {
        let host = self.host();
        let id = id.to_owned();
        let stop = move || {
            let (host, id) = (host.clone(), id.clone());
            // Off the signal thread, which must stay free for a second
            // signal.
            let _ = std::thread::Builder::new()
                .name("capturefab-stop".into())
                .spawn(move || {
                    let _ = host.stop(&id);
                });
        };
        if signals::interrupted() {
            stop();
        } else {
            signals::on_signal(stop);
        }
    }
    /// The worker of this command, without a session.
    pub fn local(&self) -> Option<&SessionHandle> {
        self.local.as_ref()
    }
    /// The pipe this command's worker writes an image captured to stdout
    /// to, once.
    pub fn take_stdout(&mut self) -> Option<std::io::PipeReader> {
        self.stdout.take()
    }
}

/// Where a job runs: a session, or the workers of this command.
#[derive(Clone)]
pub(crate) enum JobHost {
    Session(String),
    Direct(SessionHandle),
}
impl JobHost {
    /// Job `id` once it changed after `seq` or ended, waiting at most
    /// `wait`: `{seq, job, detail}`.
    pub fn wait(&self, id: &str, seq: u64, wait: Duration) -> Result<Value> {
        let wait = wait.min(Duration::from_millis(crate::session::jobs::MAX_WAIT_MS));
        match self {
            Self::Session(name) => {
                let command = SessionCommand::Wait {
                    job: id.into(),
                    after_seq: seq,
                    timeout_ms: millis(wait),
                };
                ipc::call(name, command.clone(), default_timeout(&command))
            }
            Self::Direct(local) => local.wait_job(id, seq, wait),
        }
    }
    /// Stop job `id` and wait for it to end: `{job, detail}`. A session is
    /// asked on a connection of its own.
    pub fn stop(&self, id: &str) -> Result<Value> {
        match self {
            Self::Session(name) => {
                let command = SessionCommand::StopJob { job: id.into() };
                ipc::call(name, command.clone(), default_timeout(&command))
            }
            Self::Direct(local) => local.stop_job(id),
        }
    }
}

/// The only camera of session `name`, whose status lists `cameras`; with
/// none or several, which to name, with credentials redacted.
fn only_camera(name: &str, cameras: &[Value]) -> Result<String> {
    match cameras {
        [camera] => camera["info"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("session '{name}' lists a camera without an ID")),
        [] => Err(Categorized::new(
            ErrorKind::CameraNotFound,
            format!("session '{name}' has no camera connected"),
        )
        .hint("name one with -c CAMERA")
        .details(json!({"candidates": []}))
        .into()),
        cameras => {
            let ids = cameras
                .iter()
                .filter_map(|camera| camera["info"]["id"].as_str())
                .map(|id| json!({"id": crate::media::redact_url(id)}))
                .collect::<Vec<_>>();
            Err(Categorized::new(
                ErrorKind::CameraAmbiguous,
                format!(
                    "session '{name}' has {} cameras connected; choose one with -c",
                    ids.len()
                ),
            )
            .details(json!({ "candidates": ids }))
            .into())
        }
    }
}

/// Whether `error` says the session is stopping or no longer runs.
fn session_gone(error: &anyhow::Error) -> bool {
    crate::session::is_stopping(error)
        || error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
                )
            })
        })
        || crate::types::classify(error).kind == ErrorKind::SessionNotFound
}

pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn default_timeout(command: &SessionCommand) -> Duration {
    crate::session::default_request_timeout(command)
}

/// A selector as typed, with a relative file or replay path made absolute
/// against this terminal's directory, so a session elsewhere opens the same
/// file.
fn caller_selector(selector: &str) -> Result<String> {
    let mut selector = selector.to_owned();
    paths::resolve_selector(&mut selector, &std::env::current_dir()?).map_err(input)?;
    crate::camera::validate_selector_shape(&selector)?;
    Ok(selector)
}

/// A failure in what the caller typed: usage, unless its cause already
/// gave a reason.
pub(crate) fn input(error: anyhow::Error) -> anyhow::Error {
    if crate::types::classify(&error).kind == ErrorKind::OperationFailed {
        Usage(format!("{error:#}")).into()
    } else {
        error
    }
}

/// The running session a camera command uses without `--session`: the one
/// whose descriptor lists the camera, or, without `-c`, the only session
/// that holds exactly one camera. Matched locally, without asking any.
fn route_to(sessions: &[ipc::Descriptor], selector: Option<&str>) -> Option<String> {
    // A camera's own ID outranks another camera's serial or alias.
    let rank = |session: &ipc::Descriptor| match selector {
        Some(selector) => session
            .cameras
            .iter()
            .filter_map(|camera| camera.named_by(selector))
            .min(),
        None => (session.cameras.len() == 1).then_some(0),
    };
    let ranked = sessions
        .iter()
        .filter_map(|session| Some((rank(session)?, session)))
        .collect::<Vec<_>>();
    let best = ranked.iter().map(|(rank, _)| *rank).min()?;
    let mut found = ranked.iter().filter(|(rank, _)| *rank == best);
    match (found.next(), found.next()) {
        (Some((_, session)), None) => Some(session.name.clone()),
        _ => None,
    }
}

/// The camera a command without `-c` uses: the only one `list` finds,
/// leaving out simulators and screen captures.
fn default_camera() -> Result<String> {
    let kinds = crate::camera::Kinds {
        sim: false,
        ..crate::camera::Kinds::all()
    };
    let (devices, _) = crate::camera::discover_kinds(DEFAULT_DISCOVERY, kinds);
    let devices = devices
        .into_iter()
        .filter(|info| !matches!(crate::camera::kind(info), "sim" | "screen"))
        .collect::<Vec<_>>();
    let candidates = || {
        devices
            .iter()
            .map(camera::row)
            .map(|row| serde_json::to_value(row).unwrap_or_default())
            .collect::<Vec<_>>()
    };
    match devices.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(
            Categorized::new(ErrorKind::CameraNotFound, "no camera found")
                .hint("connect a camera, or try the simulator with -c sim:0")
                .details(json!({"candidates": []}))
                .into(),
        ),
        _ => Err(Categorized::new(
            ErrorKind::CameraAmbiguous,
            format!("{} cameras found; choose one with -c", devices.len()),
        )
        .details(json!({"candidates": candidates()}))
        .into()),
    }
}

impl Cli {
    /// The flags a test or schema reads, as clap built them.
    pub(crate) fn tree() -> clap::Command {
        let mut command = Self::command();
        command.build();
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clap_tree_is_valid_and_globals_go_anywhere() {
        Cli::command().debug_assert();
        for args in [
            &["capturefab", "--json", "get", "-c", "sim:0", "Width"][..],
            &["capturefab", "get", "Width", "-c", "sim:0", "--json"],
            &[
                "capturefab",
                "set",
                "-c",
                "sim:0",
                "exposure=5ms",
                "--no-session",
            ],
            &["capturefab", "--session", "bench", "info"],
            &["capturefab", "ls", "--kind", "gige,sim"],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        for args in [
            &["capturefab", "--camera", "sim:0", "info"][..],
            &["capturefab", "discover"],
            &["capturefab", "version"],
            &["capturefab", "setup", "list"],
            &["capturefab", "--timeout-ms", "5", "list"],
            &["capturefab", "--direct", "list"],
            &["capturefab", "list", "--timeout", "5000"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn no_session_overrides_only_an_environment_session() {
        let global = |session: Option<&str>, no_session| Global {
            session: session.map(str::to_owned),
            no_session,
            ..Global::default()
        };
        let mut env = global(Some("bench"), true);
        env.settle_session(false).unwrap();
        assert_eq!(env.session, None);
        let mut typed = global(Some("bench"), true);
        assert_eq!(
            error_code(&typed.settle_session(true).unwrap_err()),
            ("usage", 2)
        );
        let mut default = global(Some("bench"), false);
        default.settle_session(false).unwrap();
        assert!(default.session_from_env);
        let mut flag = global(Some("bench"), false);
        flag.settle_session(true).unwrap();
        assert!(!flag.session_from_env);
    }

    #[test]
    fn a_session_command_without_a_camera_is_for_its_only_one() {
        let camera = |id: &str| json!({"info": {"id": id}});
        assert_eq!(
            only_camera("bench", &[camera("rtsp://user:secret@cam/live")]).unwrap(),
            "rtsp://user:secret@cam/live"
        );
        let none = only_camera("bench", &[]).unwrap_err();
        assert_eq!(error_code(&none), ("camera_not_found", 3));
        let several = only_camera(
            "bench",
            &[camera("rtsp://user:secret@cam/live"), camera("sim:0")],
        )
        .unwrap_err();
        assert_eq!(error_code(&several), ("camera_ambiguous", 2));
        let details = crate::types::classify(&several).details.unwrap();
        assert_eq!(details["candidates"].as_array().unwrap().len(), 2);
        assert!(!details.to_string().contains("secret"), "{details}");
        assert!(!format!("{several:#}").contains("secret"));
    }

    #[test]
    fn sessions_are_chosen_by_the_cameras_their_descriptors_list() {
        let info = |id: &str, serial: &str| crate::types::CameraInfo {
            id: id.into(),
            transport: crate::types::Transport::Simulator,
            vendor: String::new(),
            model: String::new(),
            serial: serial.into(),
            address: None,
            profile: Default::default(),
        };
        let session = |name: &str, cameras: Vec<ipc::HeldCamera>| ipc::Descriptor {
            version: ipc::VERSION,
            name: name.into(),
            kind: ipc::SessionKind::Serve,
            pid: 1,
            address: "127.0.0.1:1".parse().unwrap(),
            token: String::new(),
            started_ms: 0,
            cameras,
        };
        let one = session(
            "one",
            vec![ipc::HeldCamera::new(
                &info("sim:first", "sim:second"),
                ["front"].into_iter(),
            )],
        );
        let two = session(
            "two",
            vec![
                ipc::HeldCamera::new(&info("sim:second", "SECOND"), std::iter::empty()),
                ipc::HeldCamera::new(&info("rtsp://user:secret@cam/live", ""), std::iter::empty()),
            ],
        );
        let gige = session(
            "gige",
            vec![ipc::HeldCamera::new(
                &info("gige:00c0df123456", "SN9"),
                std::iter::empty(),
            )],
        );
        let all = [one, two, gige];
        assert_eq!(route_to(&all, Some("sim:first")).unwrap(), "one");
        assert_eq!(route_to(&all, Some("front")).unwrap(), "one");
        assert_eq!(route_to(&all, Some("SECOND")).unwrap(), "two");
        // An ID outranks another camera's serial.
        assert_eq!(route_to(&all, Some("sim:second")).unwrap(), "two");
        // The same password is found; credentials never reach descriptors.
        assert_eq!(
            route_to(&all, Some("rtsp://user:secret@cam/live")).unwrap(),
            "two"
        );
        assert!(!serde_json::to_string(&all[1]).unwrap().contains("secret"));
        assert_eq!(route_to(&all, Some("00:C0:DF:12:34:56")).unwrap(), "gige");
        assert!(route_to(&all, Some("sim:third")).is_none());
        // Without -c: the only session with exactly one camera, and two of
        // those are ambiguous.
        assert!(route_to(&all, None).is_none());
        assert_eq!(route_to(&all[1..2], None), None);
        assert_eq!(route_to(&all[..2], None).unwrap(), "one");
    }
}
