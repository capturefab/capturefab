//! `capturefab serve`: a session in the foreground. `session start` runs
//! the same command detached, with a pipe to say when it is ready and an
//! idle time after which it ends.
use super::{CONNECT_TIMEOUT, Global, args::CamerasArg, millis, output::Report, signals};
use crate::{
    ipc,
    session::{SessionCommand, SessionHandle},
};
use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};
use std::{io::Write, time::Duration};

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Session name other commands pass to --session
    #[arg(long, default_value = "default", value_name = "NAME")]
    pub name: String,
    #[command(flatten)]
    pub cameras: CamerasArg,
    /// Keep the cameras streaming, so captures take the newest frame at once
    #[arg(long)]
    pub live: bool,
    /// End after this long with no requests and no jobs (session start)
    #[arg(long, value_name = "DUR", hide = true, value_parser = super::args::duration)]
    pub idle: Option<Duration>,
    /// Write one JSON line to this inherited descriptor once ready or failed (session start)
    #[arg(long, value_name = "FD", hide = true)]
    pub ready_fd: Option<u64>,
}

/// Where `session start` waits to hear whether the session came up.
struct Ready(Option<std::fs::File>);
impl Ready {
    fn open(fd: Option<u64>) -> Result<Self> {
        let Some(fd) = fd else {
            return Ok(Self(None));
        };
        #[cfg(unix)]
        let file = {
            use std::os::fd::FromRawFd;
            let fd = i32::try_from(fd).context("--ready-fd is not a descriptor")?;
            // SAFETY: `session start` passed this descriptor for this
            // process alone to write once and close.
            unsafe { std::fs::File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = {
            use std::os::windows::io::FromRawHandle;
            // SAFETY: `session start` made this pipe handle inheritable for
            // this process alone to write once and close.
            unsafe {
                std::fs::File::from_raw_handle(fd as usize as std::os::windows::io::RawHandle)
            }
        };
        Ok(Self(Some(file)))
    }
    /// Say `line` once and close the pipe.
    fn say(&mut self, line: &Value) {
        if let Some(mut file) = self.0.take() {
            let _ = serde_json::to_writer(&mut file, line);
            let _ = file.write_all(b"\n");
        }
    }
}

/// The ready line of a session that could not start.
pub(super) fn failure(error: &anyhow::Error) -> Value {
    let mut body = json!({"message": crate::scrub::scrub(&format!("{error:#}"))});
    if let Some(tag) = crate::types::error_tag(error)
        && let (Some(body), Ok(Value::Object(tag))) =
            (body.as_object_mut(), serde_json::to_value(tag))
    {
        body.extend(tag);
    }
    json!({ "error": body })
}

pub fn run(global: &Global, args: ServeArgs) -> Result<Option<Report>> {
    signals::take_over();
    let out = global.output();
    let mut ready = Ready::open(args.ready_fd)?;
    let handle = match SessionHandle::named(&args.name).map_err(super::input) {
        Ok(handle) => handle,
        Err(error) => {
            ready.say(&failure(&error));
            return Err(error);
        }
    };
    let started = (|| -> Result<ipc::Server> {
        // Cameras connect before the session is announced: one that fails
        // ends it, as the person who named it asked for all of them.
        for selector in &args.cameras.cameras {
            let selector = super::caller_selector(selector)?;
            let connected =
                handle.request(SessionCommand::connect(selector, millis(CONNECT_TIMEOUT)))?;
            if args.live {
                let id = connected["connected"]["id"]
                    .as_str()
                    .context("camera connected without an ID")?;
                handle.request_to(id, SessionCommand::Start)?;
            }
        }
        ipc::Server::start(handle.clone(), &args.name, args.idle)
    })();
    let server = match started {
        Ok(server) => server,
        Err(error) => {
            ready.say(&failure(&error));
            handle.shutdown();
            return Err(error);
        }
    };
    // A session that `session start` detached writes to its log for as
    // long as it runs.
    if args.ready_fd.is_some() {
        let name = args.name.clone();
        let _ = std::thread::Builder::new()
            .name("capturefab-log".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(10));
                    super::session::rotate_log(&name);
                }
            });
    }
    let quitter = server.quitter();
    signals::on_signal(move || quitter.quit(ipc::Quit::Signal));
    if signals::interrupted() {
        server.quitter().quit(ipc::Quit::Signal);
    }
    let descriptor = server.descriptor();
    let cameras = descriptor
        .cameras
        .iter()
        .map(|camera| camera.id.clone())
        .collect::<Vec<_>>();
    let document = json!({
        "session": args.name,
        "pid": std::process::id(),
        "cameras": cameras,
    });
    ready.say(&document);
    let mut shown = document.clone();
    shown["ready"] = json!(true);
    let name = &args.name;
    let text = format!(
        "Session {name} is ready (pid {}). Commands use it for {}; Ctrl-C stops it.\n",
        std::process::id(),
        if cameras.is_empty() {
            "the cameras it connects".to_owned()
        } else {
            cameras.join(", ")
        }
    );
    out.result(Report::json(shown).text(text));
    if !global.json && !global.quiet {
        crate::doctor::print_warnings();
    }
    let why = server.wait(|| handle.has_jobs());
    // Recordings are finished before the descriptor goes.
    handle.shutdown();
    drop(server);
    if !global.json && !global.quiet {
        crate::eprintln!(
            "capturefab: session {name} ended ({})",
            match why {
                ipc::Quit::Signal => "signal",
                ipc::Quit::Stop => "session stop",
                ipc::Quit::Idle => "idle",
            }
        );
    }
    Ok(None)
}
