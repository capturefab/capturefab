//! The command line: global flags, dispatch, and how a camera command
//! reaches its camera (a session that holds it, or a worker of its own).
mod args;
mod camera;
mod debug;
mod doctor;
mod legacy;
pub mod login;
mod network;
mod onvif;
pub(crate) mod output;
pub(crate) mod paths;
mod preset;
mod schema;
mod summary;

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
    /// Use this running session (`serve --name NAME`)
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        env = "CAPTUREFAB_SESSION",
        conflicts_with = "no_session"
    )]
    pub session: Option<String>,
    /// Never use a running session: open the camera in a worker of this command
    #[arg(long, global = true)]
    pub no_session: bool,
}
impl Global {
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
        after_help = "Examples:\n  capturefab info -c sim:0\n  capturefab --json info -c gige:00c0df123456"
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
    /// Saved camera settings (host-side, portable between cameras of a model)
    #[command(subcommand)]
    Preset(preset::PresetCommand),
    #[command(flatten)]
    Legacy(legacy::LegacyCommand),
    /// Save camera sign-ins for rtsp:// and ONVIF cameras in the OS credential store
    #[command(subcommand)]
    Login(login::LoginCommand),
    /// Resolve an ONVIF camera's RTSP streams
    #[command(subcommand)]
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
    #[command(subcommand)]
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
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return clap_failure(&error.format(&mut command), json),
    };
    let out = cli.global.output();
    match run(cli) {
        Ok(Some(report)) => out.result(report),
        Ok(None) => 0,
        Err(error) => out.error(&error),
    }
}

fn clap_failure(error: &clap::Error, json: bool) -> i32 {
    use clap::error::ErrorKind as Clap;
    if matches!(error.kind(), Clap::DisplayHelp | Clap::DisplayVersion) {
        let _ = error.print();
        return 0;
    }
    if json {
        let _ = output::write_json(&output::clap_error(error));
    } else {
        let _ = error.print();
    }
    2
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
        Command::Preset(command) => preset::run(global, command)?,
        Command::Legacy(command) => return legacy::run(global, command),
        Command::Login(command) => login::run(global, &command)?,
        Command::Onvif(command) => onvif::run(global, command)?,
        Command::Network(args) => network::run(global, args)?,
        Command::Doctor => doctor::doctor(global),
        Command::Schema { command } => schema::run(global, &command)?,
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "capturefab",
                &mut std::io::stdout(),
            );
            return Ok(None);
        }
        Command::Debug(command) => return debug::run(global, command),
        Command::Licenses => doctor::licenses(),
    }))
}

/// How long opening a camera may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long finding the cameras of a default `-c` waits.
const DEFAULT_DISCOVERY: Duration = Duration::from_secs(1);
/// How long a session probe waits.
const PROBE: Duration = Duration::from_secs(2);

/// A camera command's connection: a running session that holds (or now
/// holds) the camera, or a worker of this command.
pub(crate) struct Client {
    pub out: Output,
    session: Option<String>,
    /// The camera's ID in the session.
    target: Option<String>,
    local: Option<SessionHandle>,
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
impl Client {
    /// Open `selector`, or the default camera, through the route the global
    /// flags choose.
    pub fn open(global: &Global, selector: Option<&str>) -> Result<Self> {
        let selector = selector.map(caller_selector).transpose()?;
        let out = global.output();
        if let Some((session, held)) = route(global, selector.as_deref())? {
            let target = match (held, &selector) {
                (Some(held), _) => Some(held),
                (None, Some(selector)) => {
                    let connected = ipc::call_to(
                        &session,
                        None,
                        SessionCommand::connect(selector.clone(), millis(CONNECT_TIMEOUT)),
                        default_timeout(&SessionCommand::Status),
                    )?;
                    connected["connected"]["id"].as_str().map(str::to_owned)
                }
                (None, None) => None,
            };
            return Ok(Self {
                out,
                session: Some(session),
                target,
                local: None,
            });
        }
        let selector = match selector {
            Some(selector) => selector,
            None => default_camera()?,
        };
        crate::camera::validate_selector(&selector)?;
        let local = SessionHandle::new();
        let client = Self {
            out,
            session: None,
            target: None,
            local: Some(local.clone()),
        };
        // Ctrl-C releases the camera rather than leaving control held.
        let quit = local.clone();
        legacy::on_quit_signal(move || quit.shutdown())?;
        client.call(SessionCommand::connect(selector, millis(CONNECT_TIMEOUT)))?;
        Ok(client)
    }
    pub fn call(&self, command: SessionCommand) -> Result<Value> {
        command.validate()?;
        match (&self.session, &self.local) {
            (Some(session), _) => ipc::call_to(
                session,
                self.target.as_deref(),
                command.clone(),
                default_timeout(&command),
            ),
            (None, Some(local)) => local.request(command),
            (None, None) => unreachable!("a client has a session or a worker"),
        }
    }
}

fn millis(duration: Duration) -> u64 {
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

/// The session a camera command uses, with the camera's ID there when it
/// already holds it: `--session` (or CAPTUREFAB_SESSION); else a running
/// session that holds the camera, or, without `-c`, the one session with
/// exactly one camera. `--no-session` and no match mean a worker of this
/// command.
fn route(global: &Global, selector: Option<&str>) -> Result<Option<(String, Option<String>)>> {
    if global.no_session {
        return Ok(None);
    }
    if let Some(session) = &global.session {
        let state = ipc::call(session, SessionCommand::Status, PROBE)?;
        let held = match selector {
            Some(selector) => held_camera(&state, selector),
            None => Some(only_camera(session, &state)?),
        };
        return Ok(Some((session.clone(), held)));
    }
    let mut found = Vec::new();
    for descriptor in ipc::active_sessions().unwrap_or_default() {
        let Ok(state) = ipc::call(&descriptor.name, SessionCommand::Status, PROBE) else {
            continue;
        };
        let held = match selector {
            Some(selector) => held_camera(&state, selector),
            None => match state["cameras"].as_array().map(Vec::as_slice) {
                Some([camera]) => camera["info"]["id"].as_str().map(str::to_owned),
                _ => None,
            },
        };
        if let Some(held) = held {
            found.push((descriptor.name, held));
        }
    }
    Ok(match found.as_slice() {
        [(session, held)] => Some((session.clone(), Some(held.clone()))),
        _ => None,
    })
}

/// The ID of the camera a session status holds for `selector`: by ID first,
/// then serial or address.
fn held_camera(state: &Value, selector: &str) -> Option<String> {
    let cameras = state["cameras"].as_array()?;
    let info = |camera: &Value| camera["info"].clone();
    cameras
        .iter()
        .map(info)
        .find(|info| info["id"].as_str() == Some(selector))
        .or_else(|| {
            cameras.iter().map(info).find(|info| {
                ["serial", "address"]
                    .iter()
                    .any(|key| info[*key].as_str() == Some(selector))
            })
        })
        .and_then(|info| info["id"].as_str().map(str::to_owned))
}

/// The session's only camera.
fn only_camera(session: &str, state: &Value) -> Result<String> {
    let ids = state["cameras"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|camera| camera["info"]["id"].as_str())
        .collect::<Vec<_>>();
    match ids.as_slice() {
        [id] => Ok((*id).to_owned()),
        [] => Err(Categorized::new(
            ErrorKind::CameraNotFound,
            format!("session '{session}' has no camera connected"),
        )
        .hint("name one with -c CAMERA")
        .details(json!({"candidates": []}))
        .into()),
        _ => Err(Categorized::new(
            ErrorKind::CameraAmbiguous,
            format!(
                "session '{session}' has {} cameras connected; choose one with -c",
                ids.len()
            ),
        )
        .details(json!({"candidates": ids.iter().map(|id| json!({"id": crate::media::redact_url(id)})).collect::<Vec<_>>()}))
        .into()),
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
            &["capturefab", "--session", "a", "--no-session", "info"],
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
    fn a_session_command_without_a_camera_is_for_its_only_one() {
        let state = |ids: &[&str]| json!({"cameras": ids.iter().map(|id| json!({"info": {"id": id}})).collect::<Vec<_>>()});
        assert_eq!(only_camera("bench", &state(&["sim:0"])).unwrap(), "sim:0");
        let none = only_camera("bench", &state(&[])).unwrap_err();
        assert_eq!(error_code(&none), ("camera_not_found", 3));
        let several =
            only_camera("bench", &state(&["sim:0", "rtsp://user:secret@cam/live"])).unwrap_err();
        assert_eq!(error_code(&several), ("camera_ambiguous", 2));
        let details = crate::types::classify(&several).details.unwrap();
        assert!(!details.to_string().contains("secret"), "{details}");
    }

    #[test]
    fn exact_camera_identity_takes_priority_over_another_cameras_serial() {
        let state = json!({"cameras":[{"info":{"id":"sim:first","serial":"sim:second"}}, {"info":{"id":"sim:second","serial":"SECOND"}}]});
        assert_eq!(held_camera(&state, "sim:second").unwrap(), "sim:second");
        assert_eq!(held_camera(&state, "SECOND").unwrap(), "sim:second");
        assert!(held_camera(&state, "sim:third").is_none());
    }
}
