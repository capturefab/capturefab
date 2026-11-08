//! `capturefab serve`: a session in the foreground. `session start` runs
//! the same command detached, with a pipe to say when it is ready and an
//! idle time after which it ends.
#![deny(clippy::panic_in_result_fn)]
use super::{CONNECT_TIMEOUT, Global, args::CamerasArg, millis, output::Report, signals};
use crate::{
    ipc,
    session::{SessionCommand, SessionHandle},
};
use anyhow::{Context as _, Result};
use clap::Args;
use serde_json::{Value, json};
use std::{io::Write as _, time::Duration};

#[derive(Debug, Args)]
pub(crate) struct ServeArgs {
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
    /// Let the cameras go after this long unused, as the command that started it for them asked (--keep, --background)
    #[arg(long, value_name = "DUR", hide = true, value_parser = super::args::duration)]
    pub camera_keep: Option<Duration>,
    /// Read more cameras as a JSON array of strings on stdin (session start), which keeps their addresses out of the process list
    #[arg(long, hide = true)]
    pub cameras_stdin: bool,
}

/// The most bytes of cameras `session start` hands over on stdin.
const MOST_CAMERAS: u64 = 1 << 20;

/// The cameras `session start` wrote to stdin, and closed it.
fn read_cameras(reader: impl std::io::Read) -> Result<Vec<String>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader
        .take(MOST_CAMERAS + 1)
        .read_to_end(&mut bytes)
        .context("reading the cameras on stdin")?;
    anyhow::ensure!(
        u64::try_from(bytes.len())? <= MOST_CAMERAS,
        "the cameras on stdin exceed 1 MiB"
    );
    serde_json::from_slice(&bytes).context("the cameras on stdin are not a JSON list of strings")
}

/// Where `session start` waits to hear whether the session came up.
struct Ready(Option<std::fs::File>);
impl Ready {
    fn open(fd: Option<u64>) -> Result<Self> {
        let Some(fd) = fd else {
            return Ok(Self(None));
        };
        // It is kept from the camera workers this process starts, and their
        // FFmpeg: `session start` would otherwise not see the pipe end while
        // they run, should this process die before it says anything.
        #[cfg(unix)]
        let file = {
            use std::os::fd::FromRawFd as _;
            let fd = i32::try_from(fd).context("--ready-fd is not a descriptor")?;
            // SAFETY: `session start` passed this descriptor for this
            // process alone to write once and close.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: fcntl verified the inherited descriptor; this process
            // owns it and closes it after the readiness message.
            unsafe { std::fs::File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = {
            use std::os::windows::io::FromRawHandle as _;
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
            let handle = usize::try_from(fd).context("--ready-fd exceeds host handle size")?
                as std::os::windows::io::RawHandle;
            // SAFETY: `session start` made this pipe handle inheritable for
            // this process alone to write once and close.
            let file = unsafe { std::fs::File::from_raw_handle(handle) };
            // SAFETY: file owns this inherited handle while its flag changes.
            if unsafe { SetHandleInformation(handle.cast(), HANDLE_FLAG_INHERIT, 0) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            file
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

pub(super) fn run(global: &Global, args: &ServeArgs) -> Result<Option<Report>> {
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
    signals::on_second(handle.killer());
    let started = (|| -> Result<ipc::Server> {
        let mut cameras = args.cameras.cameras.clone();
        if args.cameras_stdin {
            cameras.extend(read_cameras(std::io::stdin())?);
        }
        // Cameras connect before the session is announced: one that fails
        // ends it, as the person who named it asked for all of them.
        for selector in &cameras {
            let selector = super::caller_selector(selector)?;
            let connected =
                handle.request(SessionCommand::connect(selector, millis(CONNECT_TIMEOUT)))?;
            let id = connected["connected"]["id"]
                .as_str()
                .context("camera connected without an ID")?;
            // A session holds the cameras it was started with as long as it
            // runs, unless a command started it for its camera, which then
            // goes as that command asked.
            handle.keep_camera(id, args.camera_keep);
            if args.live {
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
            .spawn(move || -> ! {
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

#[cfg(test)]
mod tests {
    #[test]
    fn camera_lists_cannot_hide_input_past_the_limit() {
        let limit = usize::try_from(MOST_CAMERAS).unwrap();
        let mut input = b"[]".to_vec();
        input.resize(limit, b' ');
        assert_eq!(read_cameras(input.as_slice()).unwrap(), [] as [String; 0]);
        input.extend_from_slice(b"not JSON after the limit");
        let mut reader = std::io::Cursor::new(input);
        read_cameras(&mut reader).unwrap_err();
        assert_eq!(reader.position(), MOST_CAMERAS + 1);
    }

    use super::*;

    #[test]
    #[cfg(unix)]
    fn the_ready_pipe_is_not_inherited_by_the_processes_serve_starts() {
        use std::os::fd::{AsRawFd as _, IntoRawFd as _};
        let (reader, writer) = std::io::pipe().unwrap();
        let fd = writer.into_raw_fd();
        // As `session start` passes it: inheritable across the exec.
        // SAFETY: fcntl on a descriptor this test owns.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let mut ready = Ready::open(Some(u64::try_from(fd).unwrap())).unwrap();
        let file = ready.0.as_ref().unwrap();
        // SAFETY: as above.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        ready.say(&json!({"ready": true}));
        let mut line = String::new();
        std::io::Read::read_to_string(&mut &reader, &mut line).unwrap();
        assert_eq!(line, "{\"ready\":true}\n");
    }

    #[test]
    #[cfg(unix)]
    fn invalid_ready_descriptors_are_rejected() {
        assert!(Ready::open(Some(u64::try_from(i32::MAX).unwrap())).is_err());
        assert!(Ready::open(Some(u64::MAX)).is_err());
    }
}
