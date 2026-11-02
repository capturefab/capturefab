//! `capturefab shell`: many commands in one process, their cameras kept
//! connected between them. At a terminal it is a prompt with line editing,
//! history and Tab completion; otherwise it reads one command a line from
//! stdin (or each `-e`) and prints one result a line back, which gives a
//! script or an agent several commands for one start. Each command is a
//! command of the command line without `capturefab`, parsed by the same
//! clap tree and run by the same handlers, its result printed as there.
//!
//! The cameras stay where a command would keep them: in the session that
//! holds them or the warm `auto` one, which the shell keeps from ending
//! while it runs; with sessions off (`--keep 0`, `--no-session`, no
//! writable runtime folder) in workers of the shell's own.
use super::{
    Cli, Client, Command, Global, Route,
    args::KeepArg,
    output::Report,
    signals::{self, ShellSignal},
    term,
};
use crate::{
    session::SessionHandle,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use clap::{Args, CommandFactory, FromArgMatches, parser::ValueSource};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fmt::Write as _,
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

#[cfg(feature = "shell")]
mod complete;

#[derive(Debug, Args)]
pub struct ShellArgs {
    /// Cameras to connect now and keep connected, repeatable; the first is the camera of commands without -c
    #[arg(
        short = 'c',
        long = "camera",
        value_name = "CAM",
        env = "CAPTUREFAB_CAMERA",
        hide_env_values = true
    )]
    pub cameras: Vec<String>,
    /// Run COMMAND, then the next -e, instead of reading stdin; repeatable
    #[arg(short = 'e', long = "execute", value_name = "COMMAND")]
    pub execute: Vec<String>,
    /// Without a terminal: go on after a command fails; the exit code is then the first failure's
    #[arg(long)]
    pub keep_going: bool,
    /// Keep the cameras open in a session for this long after the shell's last use (default 60s; 0 holds them in the shell's own process)
    #[arg(
        long,
        value_name = "DUR",
        env = "CAPTUREFAB_KEEP",
        value_parser = super::args::duration
    )]
    pub keep: Option<Duration>,
}

/// The shell's own commands, besides the command line's.
const BUILTINS: &[(&str, &str)] = &[
    (
        "use",
        "use CAM: the camera for commands without -c (use none: no default)",
    ),
    (
        "cameras",
        "the shell's cameras and whether they are connected",
    ),
    ("show", "draw a fresh frame here: inspect --show"),
    (
        "help",
        "help [COMMAND]: the commands, or one command's help",
    ),
    ("exit", "leave the shell (or Ctrl-D)"),
];

/// Commands a default camera from `use` or `-c` is not added to: they name
/// cameras for something else than using one.
const NO_DEFAULT_CAMERA: &[&str] = &["session start", "serve", "network", "shell"];

/// The signal a person sends with Ctrl-C.
const SIGINT: i32 = 2;

/// Run the shell; its exit code.
pub fn main(global: Global, args: ShellArgs, matches: &clap::ArgMatches) -> i32 {
    let out = global.output();
    let typed = |id: &str| {
        matches
            .subcommand_matches("shell")
            .and_then(|shell| shell.value_source(id))
            == Some(ValueSource::CommandLine)
    };
    let keep = args
        .keep
        .filter(|_| typed("keep"))
        .map(crate::types::duration_text);
    let script = !args.execute.is_empty();
    let interactive = !script && term::stdin_is_tty();
    let session_typed = matches.value_source("session") == Some(ValueSource::CommandLine);
    let handle = SessionHandle::direct(global.verbose);
    super::hold(handle.clone());
    let (sender, events) = mpsc::channel();
    {
        let sender = sender.clone();
        signals::shell(move |signal| match signal {
            // Ctrl-C at a prompt that reads plain lines has nothing to
            // stop; any other signal between commands ends the shell.
            ShellSignal::Idle(signal) if interactive && signal == SIGINT => {}
            ShellSignal::Idle(signal) => {
                platform::restore_terminal();
                let _ = io::stdout().flush();
                std::process::exit(128 + signal);
            }
            signal => {
                let _ = sender.send(Event::Signal(signal));
            }
        });
    }
    platform::save_terminal();
    let shared = Arc::new(Shared {
        global: Global {
            quiet: true,
            verbose: false,
            json: false,
            ..global.clone()
        },
        route: KeepArg { keep: args.keep }.route(),
        interactive,
        known: Mutex::default(),
    });
    let mut shell = Shell {
        global,
        session_typed,
        keep,
        keep_going: args.keep_going,
        shared,
        sender,
        events,
    };
    let mut cameras = Vec::new();
    for camera in &args.cameras {
        match super::caller_selector(camera) {
            Ok(selector) => cameras.push(selector),
            Err(error) => return out.print_error(&error),
        }
    }
    shell.shared.known().default = cameras.first().cloned();
    for camera in cameras {
        shell.shared.connect(camera);
    }
    keep_sessions(&shell.shared);
    let exit = if script {
        shell.batch(args.execute.into_iter().map(Ok))
    } else if interactive {
        shell.interactive()
    } else {
        shell.batch(io::stdin().lock().lines())
    };
    handle.shutdown();
    exit
}

/// What happened to the command running, or to the shell.
enum Event {
    Done(u64, Result<Option<Report>>),
    Signal(ShellSignal),
}

/// How a line went.
enum Step {
    /// Its exit code.
    Done(i32),
    Exit,
}

struct Shell {
    global: Global,
    /// The shell's --session was typed, not taken from CAPTUREFAB_SESSION.
    session_typed: bool,
    /// The shell's --keep as typed: commands that do not type their own
    /// take it.
    keep: Option<String>,
    keep_going: bool,
    shared: Arc<Shared>,
    sender: Sender<Event>,
    events: Receiver<Event>,
}

/// What the shell's threads share: how to reach cameras, and what it
/// knows of them.
struct Shared {
    /// The shell's flags, quiet: background work prints nothing.
    global: Global,
    route: Route,
    interactive: bool,
    known: Mutex<Known>,
}

/// The shell's cameras.
#[derive(Default)]
struct Known {
    /// The camera commands without -c use.
    default: Option<String>,
    cameras: Vec<Held>,
    /// What connecting cameras said, for the next prompt. (rustyline's
    /// external printer would say it at once, but makes a pasted line wait
    /// for the next key.)
    notices: Vec<String>,
}

/// How long a prompt waits for cameras still connecting: one that is ready
/// by then is shown ready, a prompt not being redrawn once it is.
const SETTLE: Duration = Duration::from_millis(100);

/// A camera the shell connected, by the selector it was named with.
struct Held {
    selector: String,
    state: Connection,
    /// The person was told it is connecting.
    told: bool,
    /// The session it is held in, and how long that is kept after use;
    /// none for the shell's own workers.
    session: Option<String>,
    keep: Option<Duration>,
    /// What completion knows of it.
    #[cfg(feature = "shell")]
    completion: complete::Cache,
}

enum Connection {
    Connecting,
    Ready,
    Failed(String),
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Known {
    fn held(&mut self, selector: &str) -> Option<&mut Held> {
        self.cameras
            .iter_mut()
            .find(|held| held.selector == selector)
    }
    /// The selector of the camera `name` names: one the shell holds, also
    /// by the name it shows for it, without a password; else `name`.
    fn named(&self, name: String) -> String {
        let mut shown = self.cameras.iter().filter(|held| {
            held.selector == name || crate::media::redact_url(&held.selector) == name
        });
        match (shown.next(), shown.next()) {
            (Some(held), None) => held.selector.clone(),
            _ => name,
        }
    }
    /// Say `notice` before the next prompt.
    fn notice(&mut self, notice: String) {
        self.notices.push(notice);
    }
    /// What to say before a prompt: what connections said since the last
    /// one, and which cameras are still connecting, once each.
    fn pending(&mut self) -> Vec<String> {
        let mut said = std::mem::take(&mut self.notices);
        for held in &mut self.cameras {
            if matches!(held.state, Connection::Connecting) && !held.told {
                held.told = true;
                said.push(format!(
                    "connecting to {}…",
                    crate::media::redact_url(&held.selector)
                ));
            }
        }
        said
    }
    /// The prompt: the default camera, and whether it failed to connect.
    /// One still connecting is not said to be, as the prompt would go on
    /// saying so once it is ready; a line above says it instead.
    fn prompt(&self) -> String {
        let Some(camera) = &self.default else {
            return "capturefab> ".into();
        };
        let shown = crate::media::redact_url(camera);
        match self.cameras.iter().find(|held| held.selector == *camera) {
            Some(Held {
                state: Connection::Failed(_),
                ..
            }) => format!("{shown} (not connected)> "),
            _ => format!("{shown}> "),
        }
    }
    /// The cameras completion offers, as commands name them: without the
    /// passwords in their URLs, which the shell never shows. A command
    /// finds the camera by that name as it finds any connected one.
    #[cfg(feature = "shell")]
    fn offered(&self) -> Vec<String> {
        self.cameras
            .iter()
            .map(|held| crate::media::redact_url(&held.selector))
            .collect()
    }
}

impl Shared {
    fn known(&self) -> MutexGuard<'_, Known> {
        locked(&self.known)
    }
    /// Wait up to `within` for the cameras still connecting.
    fn settle(&self, within: Duration) {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline
            && self
                .known()
                .cameras
                .iter()
                .any(|held| matches!(held.state, Connection::Connecting))
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// Connect `selector` in the background, where commands will use it;
    /// a camera already connected or connecting is left alone.
    fn connect(self: &Arc<Self>, selector: String) {
        {
            let mut known = self.known();
            if known
                .held(&selector)
                .is_some_and(|held| !matches!(held.state, Connection::Failed(_)))
            {
                return;
            }
            known.cameras.retain(|held| held.selector != selector);
            known.cameras.push(Held {
                selector: selector.clone(),
                state: Connection::Connecting,
                told: false,
                session: None,
                keep: None,
                #[cfg(feature = "shell")]
                completion: complete::Cache::default(),
            });
        }
        let shared = self.clone();
        let spawned = std::thread::Builder::new()
            .name("capturefab-connect".into())
            .spawn(move || shared.connected(&selector));
        if let Err(error) = spawned {
            crate::eprintln!("capturefab: cannot connect in the background: {error}");
        }
    }
    fn connected(self: &Arc<Self>, selector: &str) {
        let began = Instant::now();
        let opened = Client::open_with(&self.global, Some(selector), self.route);
        let shown = crate::media::redact_url(selector);
        let client = {
            let mut known = self.known();
            let Some(held) = known.held(selector) else {
                return;
            };
            match opened {
                Ok(client) => {
                    held.state = Connection::Ready;
                    held.session = client.session.clone();
                    held.keep = client.keep;
                    if self.interactive {
                        known.notice(format!(
                            "{shown} connected ({:.1}s)",
                            began.elapsed().as_secs_f64()
                        ));
                    }
                    client
                }
                Err(error) => {
                    let message = crate::scrub::scrub(&format!("{error:#}")).into_owned();
                    held.state = Connection::Failed(message.clone());
                    if self.interactive {
                        known.notice(format!("{shown}: {message}"));
                    }
                    return;
                }
            }
        };
        #[cfg(feature = "shell")]
        if self.interactive {
            self.fetch_features(&client, selector, None);
        }
        #[cfg(not(feature = "shell"))]
        drop(client);
    }
    /// The features of `selector` for completion: the first list of them,
    /// or those starting with `letter`.
    #[cfg(feature = "shell")]
    fn fetch_features(&self, client: &Client, selector: &str, letter: Option<char>) {
        let filter = crate::controls::FeatureFilter {
            pattern: letter.map(|letter| format!("{letter}*")),
            all: true,
            category: None,
        };
        let Ok(list) = client.call(crate::session::SessionCommand::FeatureList { filter }) else {
            return;
        };
        let rows = list["features"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(complete::Feature::of)
            .collect::<Vec<_>>();
        let mut known = self.known();
        let Some(held) = known.held(selector) else {
            return;
        };
        let cache = &mut held.completion;
        if letter.is_none() {
            cache.partial = list.get("truncated").is_some();
        }
        for row in rows {
            if !cache.features.iter().any(|known| known.name == row.name) {
                cache.features.push(row);
            }
        }
    }
    /// Ask for the features of `selector` starting with `letter` in the
    /// background, once, when the first list did not hold them all.
    #[cfg(feature = "shell")]
    fn want_letter(self: &Arc<Self>, selector: &str, letter: char) {
        let letter = letter.to_ascii_uppercase();
        {
            let mut known = self.known();
            let Some(held) = known.held(selector) else {
                return;
            };
            if !held.completion.partial
                || !matches!(held.state, Connection::Ready)
                || !held.completion.letters.insert(letter)
            {
                return;
            }
        }
        let (shared, selector) = (self.clone(), selector.to_owned());
        let _ = std::thread::Builder::new()
            .name("capturefab-features".into())
            .spawn(move || {
                if let Ok(client) = Client::open_with(&shared.global, Some(&selector), shared.route)
                {
                    shared.fetch_features(&client, &selector, Some(letter));
                }
            });
    }
    /// What completion needs now.
    #[cfg(feature = "shell")]
    fn hints(&self) -> complete::Hints {
        let known = self.known();
        let default = known
            .default
            .as_deref()
            .and_then(|selector| known.cameras.iter().find(|held| held.selector == selector));
        let mut cameras = known.offered();
        for session in crate::ipc::sessions() {
            cameras.extend(session.cameras.into_iter().map(|camera| camera.id));
        }
        cameras.push("sim:0".into());
        cameras.dedup();
        complete::Hints {
            cameras,
            features: default
                .map(|held| held.completion.features.clone())
                .unwrap_or_default(),
            camera: default.map(|held| held.selector.clone()),
        }
    }
}

/// Keep the shell's cameras, and the sessions they are in, from being let
/// go while it runs: each is used again well before its keep runs out.
/// Once the shell exits they go as after any command.
fn keep_sessions(shared: &Arc<Shared>) {
    if shared.route.keep.is_none() {
        return;
    }
    let shared = Arc::downgrade(shared);
    let _ = std::thread::Builder::new()
        .name("capturefab-keep".into())
        .spawn(move || {
            loop {
                let Some(alive) = shared.upgrade() else {
                    return;
                };
                let cameras = alive
                    .known()
                    .cameras
                    .iter()
                    .filter(|held| matches!(held.state, Connection::Ready))
                    .filter_map(|held| {
                        // Named as commands name it to the session.
                        let camera = super::caller_selector(&held.selector)
                            .unwrap_or_else(|_| held.selector.clone());
                        Some((held.session.clone()?, camera, held.keep?))
                    })
                    .collect::<BTreeSet<_>>();
                drop(alive);
                let mut next = Duration::from_secs(20);
                for (session, camera, keep) in cameras {
                    let _ = crate::ipc::keep(&session, Some(&camera), keep);
                    next = next.min(keep / 3);
                }
                std::thread::sleep(next.max(Duration::from_secs(1)));
            }
        });
}

impl Shell {
    /// Run `lines` in order: the first failure ends the run unless
    /// --keep-going, and its exit code is the shell's; a signal ends it
    /// with the signal's.
    fn batch(&mut self, lines: impl Iterator<Item = io::Result<String>>) -> i32 {
        let mut failed = None;
        for line in lines {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    let error = anyhow::Error::new(error).context("reading commands");
                    return self.global.output().print_error(&error);
                }
            };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let step = self.execute(line);
            if signals::interrupted() {
                return signals::exit_code();
            }
            match step {
                Step::Exit => break,
                Step::Done(0) => {}
                Step::Done(code) => {
                    failed.get_or_insert(code);
                    if !self.keep_going {
                        break;
                    }
                }
            }
        }
        failed.unwrap_or(0)
    }

    /// A prompt until `exit` or Ctrl-D.
    fn interactive(&mut self) -> i32 {
        let out = self.global.output();
        let mut reader = match editor::open(self.shared.clone(), history_path()) {
            Ok(reader) => reader,
            Err(error) => return out.print_error(&error),
        };
        if !self.global.quiet {
            crate::eprintln!(
                "Capturefab shell: commands as on the command line, without `capturefab`.{} help lists them; Ctrl-D leaves.",
                if cfg!(feature = "shell") {
                    " Tab completes;"
                } else {
                    ""
                }
            );
        }
        loop {
            self.shared.settle(SETTLE);
            let (said, prompt) = {
                let mut known = self.shared.known();
                (known.pending(), known.prompt())
            };
            for notice in said {
                crate::eprintln!("{notice}");
            }
            let read = {
                let _paused = term::pause();
                reader.read(&prompt)
            };
            match read {
                // A pasted block is its lines, one command each.
                Ok(Read::Line(text)) => {
                    let mut exit = false;
                    for line in text.lines() {
                        reader.remember(line);
                        exit = matches!(self.execute(line), Step::Exit);
                        reader.save();
                        if exit {
                            break;
                        }
                    }
                    if exit {
                        break;
                    }
                    // SIGTERM or SIGHUP during a command ends the shell
                    // once that command has; Ctrl-C only the command.
                    if signals::interrupted() && signals::exit_code() != 128 + SIGINT {
                        return signals::exit_code();
                    }
                }
                Ok(Read::Interrupted) => {}
                Ok(Read::End) => break,
                Err(error) => {
                    out.print_error(&error);
                    break;
                }
            }
        }
        0
    }

    /// Run one line.
    fn execute(&mut self, line: &str) -> Step {
        let out = self.global.output();
        let mut words = match split(line) {
            Ok(words) => words,
            Err(error) => return Step::Done(out.print_error(&Usage(error).into())),
        };
        let Some(first) = words.first().cloned() else {
            return Step::Done(0);
        };
        let rest = &words[1..];
        match first.as_str() {
            "exit" | "quit" => return Step::Exit,
            "use" => return Step::Done(self.use_camera(rest)),
            "cameras" => return Step::Done(self.cameras()),
            "help" => return Step::Done(self.help(rest)),
            "show" => {
                words.splice(..1, ["inspect".to_owned(), "--show".to_owned()]);
            }
            _ => {}
        }
        match self.parse(&words) {
            Ok(Parsed::Command(cli, label)) => Step::Done(self.run(*cli, label)),
            Ok(Parsed::Printed(code)) => Step::Done(code),
            Err(error) => {
                let json = out.json || words.iter().any(|word| word == "--json");
                Step::Done(super::output::Output { json, ..out }.print_error(&error))
            }
        }
    }

    /// The command `words` name, as the command line would parse it, with
    /// the shell's flags and its default camera filled in where the line
    /// leaves them out.
    fn parse(&self, words: &[String]) -> Result<Parsed> {
        let mut argv = std::iter::once("capturefab".to_owned())
            .chain(words.iter().cloned())
            .collect::<Vec<_>>();
        let json = self.global.json || words.iter().any(|word| word == "--json");
        let mut command = Cli::command();
        let matches = match command.try_get_matches_from_mut(&argv) {
            Ok(matches) => matches,
            Err(error) => return Ok(Parsed::Printed(self.clap_failure(&error, json, words))),
        };
        let (path, leaf) = leaf(&matches);
        let path = path.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let tree = Cli::tree();
        let takes = |id: &str| {
            subcommand(&tree, &path).is_some_and(|command| {
                command
                    .get_arguments()
                    .any(|arg| arg.get_id().as_str() == id)
            })
        };
        let typed = |id: &str| takes(id) && leaf.value_source(id) == Some(ValueSource::CommandLine);
        let mut extra = Vec::new();
        let default = self.shared.known().default.clone();
        if let Some(camera) = default
            && !NO_DEFAULT_CAMERA.contains(&path.join(" ").as_str())
            && ["camera", "cameras"]
                .into_iter()
                .any(|id| takes(id) && !typed(id))
        {
            extra.extend(["-c".to_owned(), camera]);
        }
        if let Some(keep) = &self.keep
            && takes("keep")
            && !typed("keep")
        {
            extra.extend(["--keep".to_owned(), keep.clone()]);
        }
        let matches = if extra.is_empty() {
            matches
        } else {
            let at = argv
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(argv.len());
            argv.splice(at..at, extra);
            match Cli::command().try_get_matches_from_mut(&argv) {
                Ok(matches) => matches,
                Err(error) => {
                    return Ok(Parsed::Printed(self.clap_failure(&error, json, words)));
                }
            }
        };
        let mut cli = match Cli::from_arg_matches(&matches) {
            Ok(cli) => cli,
            Err(error) => {
                let error = error.format(&mut Cli::command());
                return Ok(Parsed::Printed(self.clap_failure(&error, json, words)));
            }
        };
        if let Some(command) = &cli.command
            && let Some(why) = refused(command)
        {
            return Err(Usage(why.into()).into());
        }
        // The shell's flags hold for every command; what a line types
        // holds for it.
        let line_session = matches.value_source("session") == Some(ValueSource::CommandLine);
        cli.global.settle_session(line_session)?;
        cli.global.json |= self.global.json;
        cli.global.quiet |= self.global.quiet;
        cli.global.verbose |= self.global.verbose;
        if !line_session && !cli.global.no_session {
            if self.global.session.is_some() {
                cli.global.session = self.global.session.clone();
                cli.global.session_from_env = !self.session_typed;
            } else if self.global.no_session {
                cli.global.no_session = true;
                cli.global.session = None;
            }
        }
        cli.global.keep_from_env = super::keep_from_env(&matches);
        Ok(Parsed::Command(Box::new(cli), path.join(" ")))
    }

    /// Report a line clap could not parse, or print the help it asked for:
    /// the exit code. With --json the help is a line like any result, as
    /// `help` prints it.
    fn clap_failure(&self, error: &clap::Error, json: bool, words: &[String]) -> i32 {
        use clap::error::ErrorKind as Clap;
        if matches!(error.kind(), Clap::DisplayHelp | Clap::DisplayVersion) {
            if json {
                let text = error.render().to_string();
                let out = super::output::Output {
                    json,
                    ..self.global.output()
                };
                return out.print(Report::json(json!({ "help": text })));
            }
            term::before_output();
            let _ = error.print();
            return 0;
        }
        if json {
            let group = super::missing_subcommand(words.iter().cloned());
            let _ = super::output::write_json(&super::output::clap_error(error, group));
        } else {
            term::before_output();
            let _ = error.print();
        }
        2
    }

    /// Run a parsed command on a thread of its own, in a signal scope of
    /// its own: its exit code once it ends, or once a signal leaves it
    /// behind.
    fn run(&mut self, cli: Cli, label: String) -> i32 {
        let out = cli.global.output();
        // -q on the line silences the status line, as it would alone.
        term::hush(cli.global.quiet);
        let scope = signals::begin();
        let owner = crate::ipc::new_owner();
        let sender = self.sender.clone();
        let spawned = std::thread::Builder::new()
            .name("capturefab-command".into())
            .stack_size(8 << 20)
            .spawn(move || {
                signals::enter(scope);
                crate::ipc::own_requests(owner);
                let activity = (label != "login add").then(|| term::activity(format!("{label}…")));
                let result = super::run(cli);
                drop(activity);
                let _ = sender.send(Event::Done(scope, result));
            });
        if let Err(error) = spawned {
            signals::end();
            term::hush(false);
            return out.print_error(&anyhow::Error::new(error).context("start the command"));
        }
        let exit = loop {
            match self.events.recv() {
                Ok(Event::Done(done, result)) if done == scope => {
                    break match result {
                        Ok(Some(report)) => out.print(report),
                        Ok(None) => 0,
                        Err(error) => out.print_error(&error),
                    };
                }
                Ok(Event::Signal(ShellSignal::Interrupted(at) | ShellSignal::Again(at)))
                    if at == scope =>
                {
                    // It does not stop on its own: what only it waits for
                    // is cancelled, the rest finishes in the background,
                    // and what it says then is not shown.
                    crate::ipc::close_requests(owner);
                    term::forget();
                    let error = Categorized::error(ErrorKind::Interrupted, "interrupted");
                    break out.print_error(&error);
                }
                Ok(_) => {}
                Err(_) => break 1,
            }
        };
        signals::end();
        term::forget();
        term::hush(false);
        let _ = io::stdout().flush();
        exit
    }

    /// `use [CAM|none]`.
    fn use_camera(&mut self, args: &[String]) -> i32 {
        let out = self.global.output();
        let chosen = match args {
            [] => self.shared.known().default.clone(),
            [none] if none == "none" => {
                self.shared.known().default = None;
                None
            }
            [camera] => match super::caller_selector(camera) {
                Ok(selector) => {
                    let selector = self.shared.known().named(selector);
                    self.shared.known().default = Some(selector.clone());
                    self.shared.connect(selector.clone());
                    Some(selector)
                }
                Err(error) => return out.print_error(&error),
            },
            _ => {
                return out.print_error(&Usage("use takes one camera, or none".into()).into());
            }
        };
        let report = match chosen {
            Some(camera) => {
                let shown = crate::media::redact_url(&camera);
                Report::json(json!({ "camera": shown }))
                    .text(format!("Commands without -c use {shown}.\n"))
            }
            None => Report::json(json!({}))
                .text("No default camera; commands without -c find theirs.\n"),
        };
        out.print(report)
    }

    /// `cameras`: what the shell connected, and how that went.
    fn cameras(&self) -> i32 {
        let known = self.shared.known();
        let rows = known
            .cameras
            .iter()
            .map(|held| {
                let mut row = json!({
                    "id": crate::media::redact_url(&held.selector),
                    "state": match held.state {
                        Connection::Connecting => "connecting",
                        Connection::Ready => "connected",
                        Connection::Failed(_) => "failed",
                    },
                });
                if known.default.as_deref() == Some(held.selector.as_str()) {
                    row["default"] = json!(true);
                }
                if let Some(session) = &held.session {
                    row["session"] = json!(session);
                }
                if let Connection::Failed(error) = &held.state {
                    row["error"] = json!(error);
                }
                row
            })
            .collect::<Vec<_>>();
        drop(known);
        let mut text = String::new();
        if rows.is_empty() {
            text.push_str("No cameras yet: use CAM connects one.\n");
        }
        for row in &rows {
            let _ = writeln!(
                text,
                "{}{}  {}{}{}",
                if row["default"] == true { "* " } else { "  " },
                row["id"].as_str().unwrap_or_default(),
                row["state"].as_str().unwrap_or_default(),
                row["session"]
                    .as_str()
                    .map(|session| format!(" in session {session}"))
                    .unwrap_or_default(),
                row["error"]
                    .as_str()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default(),
            );
        }
        self.global
            .output()
            .print(Report::json(json!({ "cameras": rows })).text(text))
    }

    /// `help [COMMAND...]`.
    fn help(&self, words: &[String]) -> i32 {
        let out = self.global.output();
        let tree = Cli::tree();
        let text = if words.is_empty() {
            let mut text = String::from("Commands, as on the command line without `capturefab`:\n");
            for command in tree.get_subcommands().filter(|sub| !sub.is_hide_set()) {
                if command.get_name() == "shell" {
                    continue;
                }
                let _ = writeln!(
                    text,
                    "  {:<12}{}",
                    command.get_name(),
                    command
                        .get_about()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                );
            }
            text.push_str("Shell commands:\n");
            for (name, about) in BUILTINS {
                let _ = writeln!(text, "  {name:<12}{about}");
            }
            text.push_str("COMMAND --help, or help COMMAND, shows a command's flags.\n");
            text
        } else {
            let path = words.iter().map(String::as_str).collect::<Vec<_>>();
            match subcommand(&tree, &path) {
                Some(command) => command.clone().render_long_help().to_string(),
                None => {
                    let error = Usage(format!("no command '{}'", words.join(" ")));
                    return out.print_error(&error.into());
                }
            }
        };
        out.print(Report::json(json!({ "help": text })).text(text))
    }
}

/// A line parsed: a command to run (and its name), or clap's own output
/// already printed, with its exit code.
enum Parsed {
    Command(Box<Cli>, String),
    Printed(i32),
}

/// Why a command cannot run in a shell, which owns stdin and stdout.
fn refused(command: &Command) -> Option<&'static str> {
    match command {
        Command::Shell(_) => Some("this is a shell already; type its commands"),
        Command::Serve(_) => {
            Some("serve runs until stopped; start a session with session start instead")
        }
        Command::Debug(super::debug::DebugCommand::Rpc { .. }) => {
            Some("debug rpc reads stdin, which carries this shell's commands; run it on its own")
        }
        Command::Login(super::login::LoginCommand::Add {
            password_stdin: true,
            ..
        }) => Some(
            "--password-stdin reads stdin, which carries this shell's commands; set CAPTUREFAB_CAMERA_PASSWORD instead",
        ),
        Command::Capture(args) if args.output.as_deref() == Some("-") => Some(
            "-o - writes the image to stdout, which carries this shell's results; give a file or folder",
        ),
        _ => None,
    }
}

/// The subcommand names `matches` went through, and the innermost
/// matches.
fn leaf(matches: &clap::ArgMatches) -> (Vec<&str>, &clap::ArgMatches) {
    let mut path = Vec::new();
    let mut matches = matches;
    while let Some((name, sub)) = matches.subcommand() {
        path.push(name);
        matches = sub;
    }
    (path, matches)
}

/// The command at `path` (names or aliases) under `tree`.
fn subcommand<'a, S: AsRef<str>>(tree: &'a clap::Command, path: &[S]) -> Option<&'a clap::Command> {
    let mut command = tree;
    for name in path {
        command = command.find_subcommand(name.as_ref())?;
    }
    Some(command)
}

/// Where history goes: the state folder, when it can be written.
fn history_path() -> Option<PathBuf> {
    let dir = crate::dirs::state().ok()?;
    crate::dirs::ensure_private_dir(&dir).ok()?;
    let path = dir.join("shell_history");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&path).ok()?;
    Some(path)
}

/// The words of `line` as a POSIX shell splits it, without expansions:
/// whitespace between words, 'single' and "double" quotes keep text
/// together, and `#` at the start of a word comments out the rest. A
/// backslash escapes whitespace, a quote, `#` or another backslash, and
/// is kept before anything else, so Windows paths need no doubling.
pub(super) fn split(line: &str) -> std::result::Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '#' if !in_word => break,
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a ' quote is not closed".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"' | '\\')) => {
                            word.extend(chars.next());
                        }
                        Some(c) => word.push(c),
                        None => return Err("a \" quote is not closed".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.peek() {
                    Some(&next) if escapable(next) => {
                        word.push(next);
                        chars.next();
                    }
                    _ => word.push('\\'),
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn escapable(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | '\\' | '#')
}

#[cfg(feature = "shell")]
mod editor {
    //! The prompt at a terminal: rustyline's line editing, history and
    //! completion.
    use super::{Read, Shared, complete::complete, plain::Plain};
    use anyhow::Result;
    use rustyline::{
        Context, Editor, completion::Completer, error::ReadlineError, highlight::Highlighter,
        hint::Hinter, history::DefaultHistory, validate::Validator,
    };
    use std::{path::PathBuf, sync::Arc};

    pub(super) enum Reader {
        Edit {
            editor: Box<Editor<Helper, DefaultHistory>>,
            history: Option<PathBuf>,
        },
        /// Typing at a terminal while stdout goes elsewhere: line editing
        /// would write there, so plain lines.
        Plain(Plain),
    }

    pub(super) fn open(shared: Arc<Shared>, history: Option<PathBuf>) -> Result<Reader> {
        if !super::term::stdout_is_tty() {
            return Ok(Reader::Plain(Plain));
        }
        let config = rustyline::Config::builder()
            .auto_add_history(false)
            .history_ignore_space(true)
            .history_ignore_dups(true)?
            .max_history_size(1000)?
            .completion_type(rustyline::CompletionType::List)
            .build();
        let mut editor = Editor::with_config(config)?;
        editor.set_helper(Some(Helper {
            tree: super::Cli::tree(),
            shared,
        }));
        if let Some(path) = &history {
            let _ = editor.load_history(path);
        }
        Ok(Reader::Edit {
            editor: Box::new(editor),
            history,
        })
    }

    impl Reader {
        pub(super) fn read(&mut self, prompt: &str) -> Result<Read> {
            let editor = match self {
                Self::Edit { editor, .. } => editor,
                Self::Plain(plain) => return plain.read(prompt),
            };
            match editor.readline(prompt) {
                Ok(line) => Ok(Read::Line(line)),
                Err(ReadlineError::Interrupted | ReadlineError::Signal(_)) => Ok(Read::Interrupted),
                Err(ReadlineError::Eof) => Ok(Read::End),
                Err(error) => Err(error.into()),
            }
        }
        /// Keep `line` in the history, with any password in it removed.
        pub(super) fn remember(&mut self, line: &str) {
            if let Self::Edit { editor, .. } = self
                && !line.trim().is_empty()
            {
                let _ = editor.add_history_entry(crate::scrub::redact_urls(line));
            }
        }
        pub(super) fn save(&mut self) {
            if let Self::Edit {
                editor,
                history: Some(path),
            } = self
            {
                let _ = editor.append_history(path);
            }
        }
    }

    pub(super) struct Helper {
        tree: clap::Command,
        shared: Arc<Shared>,
    }
    impl Completer for Helper {
        type Candidate = String;
        fn complete(
            &self,
            line: &str,
            pos: usize,
            _: &Context<'_>,
        ) -> rustyline::Result<(usize, Vec<String>)> {
            let hints = self.shared.hints();
            let before = &line[..pos];
            let (start, candidates) = complete(&self.tree, before, &hints);
            // Features beyond the first list come by first letter.
            if let (Some(camera), Some(letter)) = (
                &hints.camera,
                before[start..]
                    .trim_start_matches(['\'', '"'])
                    .chars()
                    .next()
                    .filter(char::is_ascii_alphabetic),
            ) {
                self.shared.want_letter(camera, letter);
            }
            Ok((start, candidates))
        }
    }
    impl Hinter for Helper {
        type Hint = String;
    }
    impl Highlighter for Helper {}
    impl Validator for Helper {}
    impl rustyline::Helper for Helper {}
}

#[cfg(not(feature = "shell"))]
mod editor {
    //! Builds without line editing read plain lines.
    use super::{Read, Shared};
    use anyhow::Result;
    use std::{path::PathBuf, sync::Arc};

    pub(super) struct Reader(super::plain::Plain);

    pub(super) fn open(_: Arc<Shared>, _: Option<PathBuf>) -> Result<Reader> {
        Ok(Reader(super::plain::Plain))
    }
    impl Reader {
        pub(super) fn read(&mut self, prompt: &str) -> Result<Read> {
            self.0.read(prompt)
        }
        pub(super) fn remember(&mut self, _: &str) {}
        pub(super) fn save(&mut self) {}
    }
}

/// What reading the next line gave.
enum Read {
    Line(String),
    /// Ctrl-C at the prompt: a fresh prompt.
    Interrupted,
    /// Ctrl-D, or the end of stdin.
    End,
}

mod plain {
    //! A prompt on stderr and lines from stdin, without editing.
    use super::Read;
    use anyhow::Result;
    use std::io::{BufRead, Write};

    pub(super) struct Plain;
    impl Plain {
        pub(super) fn read(&mut self, prompt: &str) -> Result<Read> {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "{prompt}").and_then(|()| err.flush());
            drop(err);
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                Ok(0) => Ok(Read::End),
                Ok(_) => Ok(Read::Line(line.trim_end_matches(['\n', '\r']).to_owned())),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    Ok(Read::Interrupted)
                }
                Err(error) => Err(error.into()),
            }
        }
    }
}

#[cfg(unix)]
mod platform {
    //! The terminal's settings when the shell began, restored when a signal
    //! ends it in the middle of line editing.
    use std::sync::OnceLock;

    static SAVED: OnceLock<libc::termios> = OnceLock::new();

    pub(super) fn save_terminal() {
        // SAFETY: tcgetattr writes one termios into the local.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut saved) } == 0 {
            let _ = SAVED.set(saved);
        }
    }
    pub(super) fn restore_terminal() {
        if let Some(saved) = SAVED.get() {
            // SAFETY: restores settings read from this terminal.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, saved);
            }
        }
    }
}

#[cfg(not(unix))]
mod platform {
    pub(super) fn save_terminal() {}
    pub(super) fn restore_terminal() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn lines_split_like_a_posix_shell_without_expansions() {
        let words = |line: &str| split(line).unwrap();
        assert_eq!(
            words("set exposure=5ms  gain=0"),
            ["set", "exposure=5ms", "gain=0"]
        );
        assert_eq!(
            words("capture -c 'avfoundation:HD Camera' -o \"a b.png\""),
            ["capture", "-c", "avfoundation:HD Camera", "-o", "a b.png"]
        );
        assert_eq!(words("get Width # the rest is a comment"), ["get", "Width"]);
        assert_eq!(words("get Wi#dth"), ["get", "Wi#dth"]);
        assert_eq!(words(r"capture -o a\ b.png"), ["capture", "-o", "a b.png"]);
        assert_eq!(
            words(r"capture -o C:\Users\me\shot.png"),
            ["capture", "-o", r"C:\Users\me\shot.png"]
        );
        assert_eq!(words(r#"set "Name=a \"b\"""#), ["set", r#"Name=a "b""#]);
        assert_eq!(words("get ''"), ["get", ""]);
        assert!(words("   ").is_empty());
        assert!(split("get 'Width").is_err());
        assert!(split("get \"Width").is_err());
    }

    #[test]
    fn stdin_and_stdout_belong_to_the_shell() {
        let parsed = |line: &str| {
            let words = split(line).unwrap();
            let mut argv = vec!["capturefab".to_owned()];
            argv.extend(words);
            Cli::try_parse_from(argv).unwrap().command.unwrap()
        };
        for line in [
            "shell",
            "serve",
            "debug rpc",
            "login add rtsp://cam/x --user a --password-stdin",
            "capture -c sim:0 -o -",
        ] {
            assert!(refused(&parsed(line)).is_some(), "{line}");
        }
        for line in [
            "capture -c sim:0 -o shot.png",
            "login add rtsp://cam/x --user a",
            "info",
        ] {
            assert!(refused(&parsed(line)).is_none(), "{line}");
        }
    }

    /// A prompt is not redrawn, so it never says a camera is connecting:
    /// a line above it does, once, and the outcome follows.
    #[test]
    fn the_prompt_says_nothing_that_goes_stale() {
        let held = |selector: &str, state| Held {
            selector: selector.into(),
            state,
            told: false,
            session: None,
            keep: None,
            #[cfg(feature = "shell")]
            completion: complete::Cache::default(),
        };
        let mut known = Known::default();
        assert_eq!(known.prompt(), "capturefab> ");
        known.default = Some("rtsp://admin:secret@cam/live".into());
        known
            .cameras
            .push(held("rtsp://admin:secret@cam/live", Connection::Connecting));
        known.cameras.push(held("sim:1", Connection::Ready));
        assert_eq!(known.prompt(), "rtsp://<redacted>@cam/live> ");
        assert_eq!(
            known.pending(),
            ["connecting to rtsp://<redacted>@cam/live…"]
        );
        assert!(known.pending().is_empty(), "said once");
        known.cameras[0].state = Connection::Failed("refused".into());
        known.notice("rtsp://<redacted>@cam/live: refused".into());
        assert_eq!(
            known.prompt(),
            "rtsp://<redacted>@cam/live (not connected)> "
        );
        assert_eq!(known.pending(), ["rtsp://<redacted>@cam/live: refused"]);
    }

    /// The shell shows cameras without their passwords, also where Tab
    /// lists them, and takes that name back for the camera.
    #[cfg(feature = "shell")]
    #[test]
    fn completion_offers_cameras_without_their_passwords() {
        let url = "rtsp://admin:secret@cam/live";
        let mut known = Known::default();
        known.cameras.push(Held {
            selector: url.into(),
            state: Connection::Ready,
            told: false,
            session: None,
            keep: None,
            completion: complete::Cache::default(),
        });
        let hints = complete::Hints {
            cameras: known.offered(),
            ..complete::Hints::default()
        };
        let tree = Cli::tree();
        for line in ["use ", "use rt", "info -c ", "get --camera rtsp"] {
            let (_, candidates) = complete::complete(&tree, line, &hints);
            assert!(!candidates.is_empty(), "{line}");
            for candidate in candidates {
                assert!(!candidate.contains("secret"), "{line}: {candidate}");
            }
        }
        let shown = crate::media::redact_url(url);
        assert_eq!(known.named(shown), url);
        assert_eq!(known.named(url.into()), url);
        assert_eq!(known.named("sim:0".into()), "sim:0");
    }
}
