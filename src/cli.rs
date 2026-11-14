//! The command line: global flags, dispatch, and how a camera command
//! reaches its camera (a session that holds it, or a worker of its own).
#![deny(clippy::missing_const_for_fn)]
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::panic, clippy::unreachable))]
// Command output must not depend on randomized hash-table iteration.
#![deny(clippy::iter_over_hash_type)]
mod args;
mod auto;
mod camera;
mod capture;
mod data;
mod debug;
mod doctor;
mod inspect;
mod jobs;
pub(crate) mod login;
mod monitor;
mod network;
mod onvif;
pub(crate) mod output;
pub(crate) mod paths;
mod preset;
mod record;
mod schema;
mod serve;
mod session;
mod shell;
mod signals;
pub(crate) mod term;
mod watch;

#[cfg(test)]
mod fuzz;

#[cfg(test)]
pub(crate) use output::error_code;

/// Stop an owned subprocess before a second signal forces the CLI to exit.
pub(crate) fn on_second_signal(killer: impl FnMut() + Send + 'static) {
    signals::on_second(killer);
}

use crate::{
    ipc,
    session::{SessionCommand, SessionHandle},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use clap::{Args, CommandFactory as _, FromArgMatches as _, Parser, Subcommand};
use output::{Output, Report};
use serde_json::{Value, json};
use std::time::Duration;

const OVERVIEW: &str = "Capturefab: images, video and industrial data for people and agents.

Usage: capturefab [--json] COMMAND [-c CAMERA] ...

  capturefab list                       cameras on this computer and network
  capturefab info -c sim:0              what a camera is and its controls
  capturefab set -c sim:0 exposure=5ms  change settings as one atomic change
  capturefab capture -c sim:0 -o shot.png
  capturefab data protocols --json      industrial adapters and an example config

Use --json for structured results. Run `capturefab schema` for the command contract.
More: capturefab --help, capturefab COMMAND --help";

#[derive(Debug, Parser)]
#[command(
    name = "capturefab",
    bin_name = "capturefab",
    version,
    about = "Capture images, video and industrial time series.",
    long_about = "Find cameras, save images and video, and record industrial signals on a shared timeline.\n\nUse --json for structured output. Most commands print one result or error as a JSON line on stdout. Watch and shell can print multiple lines. When capture -o - writes the image to stdout, it sends JSON to stderr. Help and version flags print text.\n\nRun capturefab schema for commands and exit codes. Run capturefab schema COMMAND for options, results and errors.",
    after_help = "Examples:\n  capturefab list --kind sim\n  capturefab info -c sim:0\n  capturefab set -c sim:0 exposure=5ms gain=3\n  capturefab --json get -c sim:0 ExposureTime 'TriggerMode[FrameStart]'",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Flags every command takes, before or after its name.
#[derive(Debug, Clone, Default, Args)]
pub(crate) struct Global {
    /// Print JSON results and errors
    #[arg(
        long,
        global = true,
        env = "CAPTUREFAB_JSON",
        action = clap::ArgAction::SetTrue,
        value_parser = clap::builder::FalseyValueParser::new()
    )]
    pub json: bool,
    /// Hide status and notes. Still report errors and explicit --progress output
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,
    /// More detail on stderr, including camera worker logs
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
    /// Use this running session (session start or serve --name NAME)
    #[arg(long, global = true, value_name = "NAME", env = "CAPTUREFAB_SESSION")]
    pub session: Option<String>,
    /// Bypass sessions for camera commands
    #[arg(long, global = true)]
    pub no_session: bool,
    /// The session came from `CAPTUREFAB_SESSION`, not --session: a default
    /// that --no-session and binary stdout override.
    #[arg(skip)]
    pub session_from_env: bool,
    /// The command's --keep came from `CAPTUREFAB_KEEP`: a default that
    /// --no-session and binary stdout override.
    #[arg(skip)]
    pub keep_from_env: bool,
}
impl Global {
    /// Let `--no-session` override a `CAPTUREFAB_SESSION` default; both typed
    /// is a mistake.
    pub(crate) fn settle_session(&mut self, typed: bool) -> Result<()> {
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
    pub(crate) const fn output(&self) -> Output {
        Output {
            json: self.json,
            quiet: self.quiet,
            verbose: self.verbose,
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Record industrial signals on a shared timeline and export CSV or JSONL
    #[command(
        subcommand,
        after_help = "Examples:\n  capturefab data protocols --json\n  capturefab data record --config sources.json --for 30s -o run.jsonl\n  capturefab data export run.jsonl -o run.csv"
    )]
    Data(data::DataCommand),
    /// List cameras: `GigE` Vision, USB3 Vision, webcams, ONVIF
    #[command(
        about = "List cameras: GigE Vision, USB3 Vision, webcams, ONVIF",
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
        after_help = "Examples:\n  capturefab get -c sim:0 ExposureTime Gain\n  capturefab get -c sim:0 exposure trigger roi\n  capturefab get -c sim:0 'TriggerMode[FrameStart]'\n  capturefab get -c sim:0 -c sim:1 Width"
    )]
    Get(camera::GetArgs),
    /// Change features or controls as one atomic change; a failure restores every value
    #[command(
        after_help = "Examples:\n  capturefab set -c sim:0 exposure=5ms gain=3\n  capturefab set -c sim:0 ExposureTime=10ms Width=640\n  capturefab set -c sim:0 'TriggerMode[FrameStart]=On' 'TriggerSource[FrameStart]=Line0'\n  capturefab set -c sim:0 roi=0,0,320,240 --dry-run"
    )]
    Set(camera::SetArgs),
    /// Run a command feature, such as `UserSetSave[UserSet1]`.
    #[command(
        about = "Run a command feature, such as UserSetSave[UserSet1]",
        after_help = "Examples:\n  capturefab exec -c sim:0 'TriggerSoftware[FrameStart]'\n  capturefab exec -c gige:00c0df123456 'UserSetSave[UserSet1]'"
    )]
    Exec(camera::ExecArgs),
    /// Save an image, burst, time lapse or scheduled capture without overwriting files
    #[command(
        after_help = "Examples:\n  capturefab capture -c sim:0 -o shot.png\n  capturefab capture -c sim:0 -n 10 -o frames/\n  capturefab capture -c sim:0 --every 10s --for 1h -o 'lapse/{seq}.jpg' --background\n  capturefab capture -c sim:0 -o - > frame.png"
    )]
    Capture(capture::CaptureArgs),
    /// Record video to a file or stream it to rtsp://, srt://, rtmp:// or udp://
    #[command(
        after_help = "Examples:\n  capturefab record -c sim:0 -o clip.mp4 --for 30s\n  capturefab record -c sim:0 -o clip.mkv --codec ffv1 --for 5m\n  capturefab record -c sim:0 -o rtsp://localhost:8554/cam --background"
    )]
    Record(record::RecordArgs),
    /// Measure brightness, clipping and focus, with an exposure suggestion and a preview
    #[command(
        after_help = "Examples:\n  capturefab inspect -c sim:0\n  capturefab inspect -c sim:0 --set exposure=20ms --region 0.25,0.25,0.5,0.5\n  capturefab inspect -c sim:0 --keep 5m --preview none\n  capturefab inspect -c sim:0 --show"
    )]
    Inspect(inspect::InspectArgs),
    /// Measure brightness and focus live while you adjust the camera
    #[command(
        after_help = "Use Ctrl-C to stop. The command exits with code 0. When you redirect stdout or use --json, watch writes one JSON line per measurement.\n\nExamples:\n  capturefab watch -c sim:0\n  capturefab watch -c sim:0 --show --region 0.4,0.4,0.2,0.2\n  capturefab watch -c sim:0 --json --for 10s --every 1s"
    )]
    Watch(watch::WatchArgs),
    /// Settle exposure and gain, then hold them; or turn auto exposure off or on
    #[command(
        after_help = "Examples:\n  capturefab auto -c sim:0\n  capturefab auto -c sim:0 --timeout 10s\n  capturefab auto -c sim:0 --off"
    )]
    Auto(auto::AutoArgs),
    /// Saved camera settings (host-side, portable between cameras of a model)
    #[command(
        subcommand,
        after_help = "A preset lives on this computer and can be applied to any camera of the model; a user set (exec 'UserSetSave[UserSet1]', or preset apply --persist) lives in the camera.\n\nExamples:\n  capturefab preset save bench -c sim:0\n  capturefab preset apply bench -c sim:0 --dry-run\n  capturefab preset list"
    )]
    Preset(preset::PresetCommand),
    /// Captures, recordings and auto exposure run in sessions, ended ones included: list, stop, wait
    #[command(
        subcommand_required = false,
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  capturefab jobs\n  capturefab jobs wait auto-1 --timeout 10m\n  capturefab jobs stop all"
    )]
    Jobs(jobs::JobsArgs),
    /// Sessions keep cameras open between commands: start one in the background, stop it, see it
    #[command(
        subcommand,
        after_help = "Camera commands can reuse a session that already holds the camera. Most keep the camera open in the auto session for 60s after use. Use --keep DUR to change that time. Use --keep 0 to avoid starting a session for a foreground camera command. Use --no-session to bypass existing sessions.\n\nExamples:\n  capturefab session start -c sim:0 --live\n  capturefab session status\n  capturefab session stop"
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
    /// Check `GigE` network paths; --tune applies bandwidth sharing to cameras
    #[command(
        about = "Check GigE paths; --tune shares bandwidth; force-ip assigns a temporary camera address",
        after_help = "Examples:\n  capturefab network\n  capturefab network --tune -c gige:00c0df123456 -c gige:00c0df654321\n  capturefab network force-ip --mac 003053061a58 --interface 192.168.50.1 --address 192.168.50.20 --netmask 255.255.255.0 --gateway 0.0.0.0"
    )]
    Network(network::NetworkArgs),
    /// Check this computer: `FFmpeg`, folders, USB and `GigE` limits, with a fix for each warning
    #[command(
        about = "Check this computer: FFmpeg, folders, USB and GigE limits, with a fix for each warning",
        after_help = "Examples:\n  capturefab doctor\n  capturefab --json doctor"
    )]
    Doctor,
    /// Describe commands, arguments, results and error codes as JSON
    #[command(
        after_help = "Examples:\n  capturefab schema\n  capturefab schema set\n  capturefab schema preset apply"
    )]
    Schema {
        /// A command, such as set or preset apply
        command: Vec<String>,
    },
    /// Run commands one after another with the cameras kept connected: a prompt in a terminal, one command a line otherwise
    #[command(
        after_help = "Commands are the ones above without `capturefab`. In a terminal: line editing, history, Tab completion, `use CAM`, `cameras`, `show`, `help`, `exit`; Ctrl-C stops the command running. Otherwise each line of stdin (or each -e) runs in order and prints one result, one JSON line each with --json; the first failure ends the run unless --keep-going, and sets the exit code.\n\nExamples:\n  capturefab shell -c sim:0\n  printf 'set exposure=5ms\\ncapture -o a.png\\ninspect\\n' | capturefab shell -c sim:0 --json\n  capturefab shell -c sim:0 -e 'set gain=0' -e 'capture -o shot.png'"
    )]
    Shell(shell::ShellArgs),
    /// Print shell completions: bash, zsh, fish, powershell, elvish
    #[command(
        after_help = "Examples:\n  capturefab completions zsh > ~/.zfunc/_capturefab\n  capturefab completions bash > ~/.local/share/bash-completion/completions/capturefab"
    )]
    Completions { shell: clap_complete::Shell },
    /// Protocol tools: `GenICam` XML, raw registers, one session request
    #[command(
        about = "Protocol tools: GenICam XML, raw registers, one session request",
        subcommand,
        after_help = "Examples:\n  capturefab debug xml -c sim:0 > camera.xml\n  capturefab debug read -c sim:0 0x100 4\n  echo '{\"op\":\"status\"}' | capturefab --session bench debug rpc"
    )]
    Debug(debug::DebugCommand),
    /// Print Capturefab's license and the bundled `FFmpeg`'s license notice
    #[command(
        about = "Print Capturefab's license and the bundled FFmpeg's license notice",
        after_help = "Examples:\n  capturefab licenses\n  capturefab --json licenses"
    )]
    Licenses,
}

/// Run the command line; the process exit code.
pub(crate) fn main() -> i32 {
    let args = std::env::args_os().collect::<Vec<_>>();
    let json = output::json_requested(&args);
    let args = help_words(args, json);
    let mut command = Cli::command();
    let matches = match command.try_get_matches_from_mut(&args) {
        Ok(matches) => matches,
        Err(error) => return clap_failure(&error, json),
    };
    let mut cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return clap_failure(&error.format(&mut command), json),
    };
    if let Some(Command::Set(args)) = cli.command.as_mut() {
        args.camera_explicit = matches.subcommand_matches("set").is_some_and(|set| {
            set.value_source("camera") == Some(clap::parser::ValueSource::CommandLine)
        });
    }
    let out = cli.global.output();
    let typed = matches.value_source("session") == Some(clap::parser::ValueSource::CommandLine);
    if let Err(error) = cli.global.settle_session(typed) {
        return out.error(&error);
    }
    cli.global.keep_from_env = keep_from_env(&matches);
    if let Err(error) = signals::install(out) {
        return out.error(&error);
    }
    term::enable(out.quiet);
    if let Some(Command::Shell(_)) = &cli.command
        && let Some(Command::Shell(args)) = cli.command.take()
    {
        return shell::main(cli.global, args, &matches);
    }
    // Every command says what it is doing once it takes a moment, except
    // those that wait for typing on stdin, and serve, which says it is ready.
    let path = command_path(&matches);
    let _activity = (!matches!(path.as_str(), "serve" | "debug rpc" | "login add"))
        .then(|| term::activity(format!("{path}…")));
    match run(cli) {
        Ok(Some(report)) => out.result(report),
        Ok(None) => 0,
        Err(error) => out.error(&error),
    }
}

/// `capturefab [GROUP] help [COMMAND...]`, which people and agents try
/// first, as `COMMAND --help`; with --json as `schema COMMAND`, the help
/// an agent reads as JSON. Any other arguments are left as they are.
fn help_words(args: Vec<std::ffi::OsString>, json: bool) -> Vec<std::ffi::OsString> {
    let mut command = Cli::tree();
    // The words naming a command group before `help`, such as `session`.
    let mut groups = Vec::new();
    let mut index = 1;
    while let Some(arg) = args.get(index).and_then(|arg| arg.to_str()) {
        if arg == "--session" {
            index += 2;
            continue;
        }
        if arg == "--" || !command.has_subcommands() {
            break;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if arg == "help" && command.find_subcommand(arg).is_none() {
            let (before, after) = (&args[..index], &args[index + 1..]);
            if !json {
                let mut words = [before, after].concat();
                words.push("--help".into());
                return words;
            }
            let mut words = before
                .iter()
                .enumerate()
                .filter(|(at, _)| !groups.contains(at))
                .map(|(_, arg)| arg.clone())
                .collect::<Vec<_>>();
            words.push("schema".into());
            words.extend(groups.iter().map(|at| args[*at].clone()));
            words.extend_from_slice(after);
            return words;
        }
        match command.find_subcommand(arg) {
            Some(sub) => {
                command = sub.clone();
                groups.push(index);
                index += 1;
            }
            None => break,
        }
    }
    args
}

/// The command `matches` names, with its subcommands: `preset apply`.
fn command_path(matches: &clap::ArgMatches) -> String {
    let mut path = Vec::new();
    let mut matches = matches;
    while let Some((name, sub)) = matches.subcommand() {
        path.push(name);
        matches = sub;
    }
    path.join(" ")
}

/// Whether the command's `--keep` came from `CAPTUREFAB_KEEP`.
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
        let group = missing_subcommand(std::env::args().skip(1));
        let _ = output::write_json(&output::clap_error(error, group));
    } else {
        let _ = error.print();
    }
    2
}

/// The deepest command group `args` name, with its subcommands: what a
/// missing-subcommand error lists.
fn missing_subcommand(mut args: impl Iterator<Item = String>) -> Option<(String, Vec<String>)> {
    let mut command = Cli::tree();
    let mut path = vec!["capturefab".to_owned()];
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
pub(crate) fn run(mut cli: Cli) -> Result<Option<Report>> {
    // Explicit network utilities are local; an ambient session is not a
    // destination or a reason to validate an unrelated session name.
    if cli.global.session_from_env
        && matches!(&cli.command, Some(Command::Network(args)) if matches!(&args.action, Some(network::NetworkCommand::ForceIp(_) | network::NetworkCommand::Action(_))))
    {
        cli.global.session = None;
    }
    if let Some(session) = &cli.global.session {
        ipc::validate_name(session).map_err(|error| Usage(format!("{error:#}")))?;
    }
    let Some(command) = cli.command.take() else {
        if cli.global.json {
            return Err(Categorized::new(ErrorKind::Usage, "choose a command")
                .hint("`capturefab schema` lists the commands")
                .into());
        }
        return Ok(Some(
            Report::json(Value::Null).text(format!("{OVERVIEW}\n")),
        ));
    };
    monitor::preflight(&command)?;
    let global = &cli.global;
    Ok(Some(match command {
        Command::Data(command) => data::run(global, command)?,
        Command::List(args) => camera::list(global, &args)?,
        Command::Info(args) => camera::info(global, &args)?,
        Command::Features(args) => camera::features(global, args)?,
        Command::Get(args) => camera::get(global, &args)?,
        Command::Set(args) => camera::set(global, &args)?,
        Command::Exec(args) => camera::exec(global, &args)?,
        Command::Capture(args) => return capture::run(global, &args),
        Command::Record(args) => record::run(global, &args)?,
        Command::Inspect(args) => inspect::run(global, &args)?,
        Command::Watch(args) => return watch::run(global, &args),
        Command::Auto(args) => auto::run(global, &args)?,
        Command::Preset(command) => preset::run(global, command)?,
        Command::Jobs(args) => jobs::run(global, args)?,
        Command::Session(command) => session::run(global, command)?,
        Command::Serve(args) => return serve::run(global, &args),
        Command::Login(command) => login::run(global, &command)?,
        Command::Onvif(command) => onvif::run(global, command)?,
        Command::Network(args) => network::run(global, &args)?,
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
        Command::Shell(_) => {
            return Err(Usage("this is a shell already; type its commands".into()).into());
        }
    }))
}

/// How long opening a camera may take.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long finding the cameras of a default `-c` waits.
const DEFAULT_DISCOVERY: Duration = Duration::from_secs(1);
/// The session `--keep`, `--background` and the warm default start when
/// none holds the camera.
pub(crate) const AUTO_SESSION: &str = "auto";
/// How long a session started for `--background` alone stays once its jobs
/// are done.
const BACKGROUND_IDLE: Duration = Duration::from_secs(60);
/// How long a camera command keeps its camera open after it, in the `auto`
/// session, without `--keep`: the next command finds it connected.
pub(crate) const WARM: Duration = Duration::from_secs(60);

/// What a camera command asks of its route besides the global flags.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Route {
    /// `--keep DUR`: run through a session that holds the camera and stays
    /// at least this long after its last use.
    pub keep: Option<Duration>,
    /// The keep is the warm default rather than asked for: `--no-session`
    /// and a named session drop it, it never lengthens a session other than
    /// `auto`, and where no session can start the command runs with a
    /// worker of its own.
    pub warm: bool,
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
    pub(crate) fn keep(keep: Option<Duration>) -> Self {
        Self {
            keep,
            ..Self::default()
        }
    }
    /// The default of a camera command: keep the camera warm in the `auto`
    /// session for `WARM`.
    pub(crate) fn warm() -> Self {
        Self {
            keep: Some(WARM),
            warm: true,
            ..Self::default()
        }
    }
    /// The same route without a session to keep.
    const fn cold(self) -> Self {
        Self {
            keep: None,
            warm: false,
            ..self
        }
    }
    /// A route whose camera may take up to `wait` (none: a minute) to open.
    pub(crate) fn waiting(mut self, wait: args::Wait) -> Self {
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
    const fn needs_session(self) -> bool {
        self.keep.is_some() || self.background
    }
    /// How long a session started for this command waits unused.
    fn idle(self) -> Duration {
        self.keep.unwrap_or(BACKGROUND_IDLE)
    }
    /// The keep to send session `name` with each request: the warm default
    /// keeps only the session it starts.
    fn keep_for(self, name: &str) -> Option<Duration> {
        self.keep.filter(|_| !self.warm || name == AUTO_SESSION)
    }
}

/// A camera command's connection: a running session that holds (or now
/// holds) the camera, or a worker of this command.
pub(crate) struct Client {
    pub out: Output,
    host: JobHost,
    /// The camera as the session is asked for it: the selector typed, which
    /// the session resolves, or none for the session's only camera. For the
    /// workers a shell holds, the camera's ID there.
    target: Option<String>,
    /// The direct host holds a shell's cameras, which outlive this command.
    shared: bool,
    /// `--keep`: sent with each request that uses the session.
    keep: Option<Duration>,
    /// The pipe this command's worker writes an image captured to stdout to.
    stdout: Option<std::io::PipeReader>,
    /// When this command began, in Unix milliseconds: `-v` shows what a
    /// session logged for it since.
    since_ms: u64,
}
impl Drop for Client {
    fn drop(&mut self) {
        if let JobHost::Direct(local) = &self.host {
            if self.shared {
                if self.out.verbose
                    && let Ok(state) = self.call(SessionCommand::Status)
                {
                    self.print_log(&state);
                }
                return;
            }
            if self.out.verbose {
                for line in local.snapshot().logs {
                    eprintln!("capturefab: [{}] {}", line.level, line.message);
                }
            }
            local.shutdown();
        } else if self.out.verbose
            && let JobHost::Session(session) = &self.host
        {
            self.print_session_log(session);
        }
    }
}

/// The workers a shell holds its cameras in when no session can: a command
/// that would open a worker of its own uses the shell's, which stays.
static HELD: std::sync::OnceLock<SessionHandle> = std::sync::OnceLock::new();

/// Hold the cameras commands open for themselves in `handle` from now on.
fn hold(handle: SessionHandle) {
    let _ = HELD.set(handle);
}
/// How a command came to use a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// The command named it, or needs one for `--keep` or `--background`.
    Named,
    /// The command started it just now for its camera.
    Started,
    /// Its descriptor lists the command's camera.
    Routed,
}
impl Found {
    const fn of(started: bool) -> Self {
        if started { Self::Started } else { Self::Named }
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
    pub(crate) fn open(global: &Global, selector: Option<&str>) -> Result<Self> {
        Self::open_with(global, selector, Route::default())
    }
    /// Open `selector` as `route` asks: through `--session`, a running
    /// session that holds the camera, or for `--keep`/`--background` one
    /// started for it; else a worker of this command.
    pub(crate) fn open_with(global: &Global, selector: Option<&str>, route: Route) -> Result<Self> {
        let since_ms = crate::scheduling::now_ms();
        let selector = selector.map(caller_selector).transpose()?;
        let out = global.output();
        let _activity = term::activity(selector.as_ref().map_or_else(
            || "connecting…".to_owned(),
            |selector| format!("connecting to {}…", crate::media::redact_url(selector)),
        ));
        // CAPTUREFAB_KEEP and the warm default are defaults, which
        // --no-session overrides.
        let mut route = if global.no_session && (route.warm || global.keep_from_env) {
            route.cold()
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
            // A session named for the command keeps the idle time it was
            // started with.
            if route.warm {
                route = route.cold();
            }
            let started = route.needs_session()
                && session::ensure(name, selector.as_deref(), route.idle(), global)?;
            return Self::in_session(
                out,
                name,
                selector.as_deref(),
                route,
                since_ms,
                Found::of(started),
            )
            .map_err(Unusable::error);
        }
        if !global.no_session
            && let Some(name) = route_to(&ipc::sessions(), selector.as_deref())
        {
            match Self::in_session(
                out,
                &name,
                selector.as_deref(),
                route,
                since_ms,
                Found::Routed,
            ) {
                Ok(client) => return Ok(client),
                // A session that is stopping, or gone, leaves the camera to
                // this command; one that holds it and failed says why.
                Err(Unusable::Gone(_)) if !route.needs_session() || route.warm => {}
                Err(unusable) => return Err(unusable.error()),
            }
        }
        // Where no session can start, such as on a read-only system, the
        // warm default leaves the camera to this command.
        if route.warm && !session::can_start() {
            if out.verbose {
                let why = match crate::dirs::runtime() {
                    Ok(runtime) => {
                        format!("no writable folder for its log in {}", runtime.display())
                    }
                    Err(error) => format!("{error:#}"),
                };
                crate::eprintln!(
                    "capturefab: no session can start ({why}); the camera is opened for this command alone"
                );
            }
            route = route.cold();
        }
        let selector = match selector {
            Some(selector) => selector,
            None => default_camera(out)?,
        };
        if route.needs_session() {
            let started = session::ensure(AUTO_SESSION, Some(&selector), route.idle(), global);
            let found = match started {
                Ok(started) => Some(Found::of(started)),
                // The auto session is ending, as when its idle time ran out
                // just now: the warm default opens the camera in this
                // command instead, as where no session can start.
                Err(error) if route.warm && session_gone(&error) => None,
                Err(error) => return Err(error),
            };
            if let Some(found) = found {
                match Self::in_session(
                    out,
                    AUTO_SESSION,
                    Some(selector.as_str()),
                    route,
                    since_ms,
                    found,
                ) {
                    Ok(client) => return Ok(client),
                    Err(Unusable::Gone(_)) if route.warm => {}
                    Err(unusable) => return Err(unusable.error()),
                }
            }
            route = route.cold();
        }
        crate::camera::validate_selector(&selector)?;
        if let Some(held) = HELD.get().filter(|_| !route.stdout) {
            return Self::held(out, held, selector, route, since_ms);
        }
        let local = SessionHandle::direct(out.verbose);
        signals::on_second(local.killer());
        let stdout = route.stdout.then(|| local.stdout_pipe()).transpose()?;
        let client = Self {
            out,
            host: JobHost::Direct(local),
            target: None,
            shared: false,
            keep: None,
            stdout,
            since_ms,
        };
        client.call(SessionCommand::connect(selector, route.connect()))?;
        Ok(client)
    }
    /// Use the worker a shell holds for `selector`, connecting the camera
    /// there first when it is not yet.
    fn held(
        out: Output,
        handle: &SessionHandle,
        selector: String,
        route: Route,
        since_ms: u64,
    ) -> Result<Self> {
        let held = handle
            .held_cameras()
            .into_iter()
            .filter_map(|camera| Some((camera.named_by(&selector)?, camera.id)))
            .min();
        let id = if let Some((_, id)) = held {
            id
        } else {
            let reply = handle.request(SessionCommand::connect(selector, route.connect()))?;
            reply["connected"]["id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("camera connected without an ID"))?
        };
        Ok(Self {
            out,
            host: JobHost::Direct(handle.clone()),
            target: Some(id),
            shared: true,
            keep: None,
            stdout: None,
            since_ms,
        })
    }
    /// Use session `name`, `found` as it was, for `selector`, connecting the
    /// camera there when it does not hold it yet; otherwise a short request
    /// checks that the session still runs and holds it, unless this command
    /// just started it for the camera.
    fn in_session(
        out: Output,
        name: &str,
        selector: Option<&str>,
        route: Route,
        since_ms: u64,
        found: Found,
    ) -> std::result::Result<Self, Unusable> {
        let keep = route.keep_for(name);
        let unusable = |error: anyhow::Error| {
            if session_gone(&error) {
                Unusable::Gone(error)
            } else {
                Unusable::Failed(error)
            }
        };
        let descriptor = ipc::read_descriptor(name).map_err(unusable)?;
        let held = selector.is_none_or(|selector| {
            descriptor
                .cameras
                .iter()
                .any(|camera| camera.named_by(selector).is_some())
        });
        let mut client = Self {
            out,
            host: JobHost::Session(name.to_owned()),
            target: selector.map(str::to_owned),
            shared: false,
            keep,
            stdout: None,
            since_ms,
        };
        let connect = |selector: &str| {
            let connect = SessionCommand::connect(selector.to_owned(), route.connect());
            let timeout = default_timeout(&connect);
            ipc::call_keeping(name, None, connect, timeout, keep)
                .map(drop)
                .map_err(unusable)
        };
        match (selector, held) {
            (Some(selector), false) => connect(selector)?,
            // Started for it just now, as this command asked.
            (Some(_), true) if found == Found::Started => {}
            // The session runs and holds it: one short request says it still
            // answers, and keeps it, unless its keep ran out just now and it
            // let go of the camera. A command that found the session by the
            // camera alone then goes on as if it had not; one that asked for
            // the session, or to keep the camera, connects it there again.
            (Some(selector), true) => match ipc::touch(&descriptor, selector, keep) {
                Ok(()) => {}
                Err(error) if crate::types::classify(&error).kind == ErrorKind::CameraNotFound => {
                    if found == Found::Routed && keep.is_none() {
                        return Err(Unusable::Gone(error));
                    }
                    connect(selector)?;
                }
                Err(error) => return Err(unusable(error)),
            },
            // Without -c, the session's only camera, named from now on so a
            // camera connected later does not change what this command uses.
            (None, _) => {
                let reply = ipc::call_keeping(
                    name,
                    None,
                    SessionCommand::Status,
                    default_timeout(&SessionCommand::Status),
                    keep,
                )
                .map_err(unusable)?;
                let cameras = reply["cameras"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                client.target = Some(only_camera(name, cameras).map_err(Unusable::Failed)?);
            }
        }
        Ok(client)
    }
    pub(crate) fn call(&self, command: SessionCommand) -> Result<Value> {
        command.validate()?;
        if let Some(target) = &self.target {
            crate::session::monitor::validate_for_selector(target, &command)?;
        }
        match &self.host {
            JobHost::Session(session) => {
                // Looking does not keep a session; using it does.
                let keep = self
                    .keep
                    .filter(|_| !matches!(command, SessionCommand::Status));
                let timeout = default_timeout(&command);
                ipc::call_keeping(session, self.target.as_deref(), command, timeout, keep)
            }
            // A shell's command that a signal interrupted stops waiting,
            // and what only it waited for, such as an inspection, stops.
            JobHost::Direct(local) => match &self.target {
                Some(target) if self.shared => {
                    local.request_for(Some(target), command, &signals::interrupted)
                }
                _ => local.request(command),
            },
        }
    }
    /// What session `session` logged for this command's camera since the
    /// command began, as a worker of the command's own shows under `-v`:
    /// its own lines and the session's, not other cameras'.
    fn print_session_log(&self, session: &str) {
        let Ok(state) = ipc::call_keeping(
            session,
            self.target.as_deref(),
            SessionCommand::Status,
            default_timeout(&SessionCommand::Status),
            None,
        ) else {
            return;
        };
        self.print_log(&state);
    }
    /// The lines of the log in status `state` since this command began.
    fn print_log(&self, state: &Value) {
        let camera = state["cameras"][0]["info"]["id"]
            .as_str()
            .map(|id| format!("[{}] ", crate::media::redact_url(id)));
        let lines = serde_json::from_value::<Vec<crate::session::LogEntry>>(state["logs"].clone())
            .unwrap_or_default();
        for line in lines.iter().filter(|line| line.unix_ms >= self.since_ms) {
            let message = match camera
                .as_deref()
                .and_then(|camera| line.message.strip_prefix(camera))
            {
                Some(message) => message,
                None if line.message.starts_with('[') => continue,
                None => &line.message,
            };
            eprintln!("capturefab: [{}] {message}", line.level);
        }
    }
    /// The camera's entry in a session status, as `Status` reports it.
    pub(crate) fn status(&self) -> Result<Value> {
        let mut state = self.call(SessionCommand::Status)?;
        match state
            .get_mut("cameras")
            .and_then(Value::as_array_mut)
            .map(Vec::as_mut_slice)
        {
            Some([camera]) => Ok(camera.take()),
            _ => Err(Categorized::new(
                ErrorKind::Disconnected,
                "the camera is no longer connected",
            )
            .into()),
        }
    }
    /// The session this client uses, if any.
    pub(crate) const fn session(&self) -> Option<&str> {
        match &self.host {
            JobHost::Session(session) => Some(session.as_str()),
            JobHost::Direct(_) => None,
        }
    }
    /// Where this client's jobs run.
    pub(crate) fn host(&self) -> JobHost {
        self.host.clone()
    }
    /// Job `id` once it changed after `seq` or ended, waiting at most
    /// `wait`: `{seq, job, detail}`.
    pub(crate) fn wait_job(&self, id: &str, seq: u64, wait: Duration) -> Result<Value> {
        self.host().wait(id, seq, wait)
    }
    /// Stop job `id` when a signal arrives, as one that came already does.
    pub(crate) fn stop_on_signal(&self, id: &str) {
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
    pub(crate) const fn local(&self) -> Option<&SessionHandle> {
        match &self.host {
            JobHost::Direct(local) => Some(local),
            JobHost::Session(_) => None,
        }
    }
    /// Whether the camera stays open once this command ends: in a session,
    /// or in the workers a shell holds.
    pub(crate) const fn outlives(&self) -> bool {
        matches!(&self.host, JobHost::Session(_)) || self.shared
    }
    /// The pipe this command's worker writes an image captured to stdout
    /// to, once.
    pub(crate) const fn take_stdout(&mut self) -> Option<std::io::PipeReader> {
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
    pub(crate) fn wait(&self, id: &str, seq: u64, wait: Duration) -> Result<Value> {
        let wait = wait.min(Duration::from_millis(crate::session::jobs::MAX_WAIT_MS));
        match self {
            Self::Session(name) => {
                let command = SessionCommand::Wait {
                    job: id.into(),
                    after_seq: seq,
                    timeout_ms: millis(wait),
                };
                let timeout = default_timeout(&command);
                ipc::call(name, command, timeout)
            }
            Self::Direct(local) => local.wait_job(id, seq, wait),
        }
    }
    /// Stop job `id` and wait for it to end: `{job, detail}`. A session is
    /// asked on a connection of its own.
    pub(crate) fn stop(&self, id: &str) -> Result<Value> {
        match self {
            Self::Session(name) => {
                let command = SessionCommand::StopJob { job: id.into() };
                let timeout = default_timeout(&command);
                ipc::call(name, command, timeout)
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
/// that holds exactly one camera. The auto session, which camera commands
/// start for themselves, holds whatever camera they used last: it answers
/// only for the cameras it is asked for by name, and a command without
/// `-c` finds its camera as it would alone. Matched locally, without asking
/// any.
fn route_to(sessions: &[ipc::Descriptor], selector: Option<&str>) -> Option<String> {
    // A camera's own ID outranks another camera's serial or alias.
    let rank = |session: &ipc::Descriptor| {
        selector.map_or_else(
            || (session.cameras.len() == 1 && session.name != AUTO_SESSION).then_some(0),
            |selector| {
                session
                    .cameras
                    .iter()
                    .filter_map(|camera| camera.named_by(selector))
                    .min()
            },
        )
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
/// leaving out simulators and screen captures. Of several, a person at a
/// terminal picks one.
fn default_camera(out: Output) -> Result<String> {
    let kinds = crate::camera::Kinds {
        sim: false,
        ..crate::camera::Kinds::all()
    };
    let searching = term::activity("looking for cameras…");
    let (devices, _) = crate::camera::discover_kinds(DEFAULT_DISCOVERY, kinds);
    drop(searching);
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
        _ => {
            if let Some(index) = pick_camera(out, &devices)? {
                return Ok(devices[index].id.clone());
            }
            Err(Categorized::new(
                ErrorKind::CameraAmbiguous,
                format!("{} cameras found; choose one with -c", devices.len()),
            )
            .details(json!({"candidates": candidates()}))
            .into())
        }
    }
}

/// Which of `devices` the person at this terminal picks, when someone is
/// there to ask: never for JSON, scripts or a shell's commands, which have
/// `use`. Ctrl-C at the prompt is `interrupted`, also for a command that
/// took signals over, which would otherwise wait on.
fn pick_camera(out: Output, devices: &[crate::types::CameraInfo]) -> Result<Option<usize>> {
    if !term::can_pick(out.json) || signals::in_shell() {
        return Ok(None);
    }
    let interrupted = || Categorized::error(ErrorKind::Interrupted, "interrupted");
    // Ctrl-C while looking for cameras: no one wants to pick one now.
    if signals::interrupted() {
        return Err(interrupted());
    }
    let labels = devices
        .iter()
        .map(|device| {
            let row = camera::row(device);
            let mut label = format!("{}  {}", row.id, row.kind);
            for part in [&row.vendor, &row.model] {
                if !part.is_empty() {
                    label.push_str("  ");
                    label.push_str(part);
                }
            }
            label
        })
        .collect::<Vec<_>>();
    term::before_output();
    let _paused = term::pause();
    match term::pick_until(
        labels,
        || std::io::stdin().lock(),
        std::io::stderr(),
        &signals::interrupted,
    ) {
        Ok(picked) => Ok(picked),
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
            crate::eprintln!();
            Err(interrupted())
        }
        Err(_) => Ok(None),
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
            profile: crate::camera_profile::CameraProfile::default(),
        };
        let session = |name: &str, cameras: Vec<ipc::HeldCamera>| ipc::Descriptor {
            version: ipc::VERSION,
            name: name.into(),
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
        // The auto session answers only for a camera named: beside one
        // other, that one is the only session without -c, and alone it
        // leaves the camera to be found.
        let auto = session(
            AUTO_SESSION,
            vec![ipc::HeldCamera::new(
                &info("sim:0", "SIM0"),
                std::iter::empty(),
            )],
        );
        assert_eq!(
            route_to(std::slice::from_ref(&auto), Some("sim:0")).unwrap(),
            "auto"
        );
        assert!(route_to(std::slice::from_ref(&auto), None).is_none());
        let both = [auto, all[0].clone()];
        assert_eq!(route_to(&both, None).unwrap(), "one");
    }
}
