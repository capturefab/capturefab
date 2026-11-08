//! A camera worker process: the hidden `__worker` mode of this program. Its
//! stdin thread never waits on the camera: it queues requests, answers
//! status and ping from shared state, and passes cancels and stops on. One
//! writer thread owns a private copy of the original stdout, which only the
//! protocol uses; anything else printed goes to stderr. A panic on any
//! thread ends the process, which its coordinator sees.
use super::protocol::{self, Input, MAX_LINE, MAX_REQUEST, Output, PROTOCOL};
use crate::engine::{WorkerHandle, events::Events, events::Next};
use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{BufRead as _, Read as _, Write as _},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// How long a worker told to stop may take before it exits regardless:
/// a driver call that never returns must not keep the camera held.
const WATCHDOG: Duration = Duration::from_secs(10);

/// The worker's arguments after `__worker`.
struct Arguments {
    verbose: bool,
    session: String,
    /// The inherited pipe an image captured to stdout goes to.
    output: Option<u64>,
}
fn arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<Arguments> {
    let mut arguments = Arguments {
        verbose: false,
        session: String::from(crate::session::DIRECT),
        output: None,
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .and_then(|value| value.into_string().ok())
                .with_context(|| format!("{name} needs a value"))
        };
        match arg.to_str() {
            Some("--verbose") => arguments.verbose = true,
            Some("--session") => arguments.session = value("--session")?,
            Some("--output-fd") => {
                arguments.output = Some(
                    value("--output-fd")?
                        .parse()
                        .context("--output-fd is not a descriptor")?,
                );
            }
            _ => anyhow::bail!("unknown worker argument {}", arg.to_string_lossy()),
        }
    }
    Ok(arguments)
}

#[derive(Serialize)]
struct LineRef<'a> {
    seq: u64,
    #[serde(flatten)]
    output: &'a Output,
}

/// Run as a camera worker; `args` follow `__worker`.
pub(crate) fn run_worker(args: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
    crate::host::return_freed_frames();
    let args = arguments(args)?;
    // A panic on any thread, the camera's or a helper's, ends the process,
    // which its coordinator reports and recovers from; a worker whose
    // camera thread died would otherwise answer every request with errors.
    std::panic::set_hook(Box::new(|info| {
        // The message last, so the tail of stderr a crash report keeps
        // always holds it.
        let backtrace = std::backtrace::Backtrace::force_capture();
        crate::eprintln!("{backtrace}");
        crate::eprintln!(
            "capturefab worker panicked: {}",
            crate::scrub::scrub(&info.to_string()).replace('\n', " ")
        );
        std::process::abort();
    }));
    // Its own process group is in the background of a terminal, where a
    // write to it stops the worker when `stty tostop` is set, unless SIGTTOU
    // is ignored. FFmpeg and the helpers inherit that.
    #[cfg(unix)]
    // SAFETY: setting a standard signal's disposition to ignore has no
    // preconditions.
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let output = platform::protocol_output()?;
    let events = Events::attached(args.verbose);
    let writer = {
        let events = events.clone();
        std::thread::Builder::new()
            .name("capturefab-writer".into())
            .spawn(move || write_lines(output, &events))
            .context("start the worker's writer")?
    };
    events.emit(Output::Hello {
        protocol: PROTOCOL,
        build: env!("CARGO_PKG_VERSION").into(),
    });
    let output = args.output.map(platform::inherited).transpose()?;
    if args.session != super::DIRECT {
        let _ = super::SERVED.set(args.session.clone());
    }
    let handle = WorkerHandle::serving(events.clone(), &args.session, output);
    let stopping = Arc::new(AtomicBool::new(false));
    {
        // The camera thread ends only when told to; if it ends otherwise
        // the worker is of no use.
        let handle = handle.clone();
        let stopping = stopping.clone();
        let _ = std::thread::Builder::new()
            .name("capturefab-monitor".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    if handle.finished() && !stopping.load(Ordering::Acquire) {
                        crate::eprintln!("capturefab worker: the camera thread ended");
                        std::process::exit(70);
                    }
                }
            });
    }
    let read = read_requests(&handle, &events);
    stopping.store(true, Ordering::Release);
    // Told to stop, or the coordinator is gone: whatever happens below,
    // exit soon.
    let _ = std::thread::Builder::new()
        .name("capturefab-watchdog".into())
        .spawn(|| {
            std::thread::sleep(WATCHDOG);
            crate::eprintln!(
                "capturefab worker: still finishing after {} s; exiting",
                WATCHDOG.as_secs()
            );
            std::process::exit(1);
        });
    handle.shutdown();
    crate::copies::report();
    events.close();
    let _ = writer.join();
    read
}

/// Read requests until stdin closes or a shutdown comes.
fn read_requests(handle: &WorkerHandle, events: &Events) -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut data = Vec::new();
    loop {
        data.clear();
        reader
            .by_ref()
            .take(MAX_REQUEST as u64 + 1)
            .read_until(b'\n', &mut data)?;
        if data.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            data.len() <= MAX_REQUEST && data.last() == Some(&b'\n'),
            "worker request exceeds 1 MiB"
        );
        let input = match serde_json::from_slice::<Input>(&data) {
            Ok(input) => input,
            Err(error) => {
                // Answer the request if its number can be read.
                let id = serde_json::from_slice::<Value>(&data)
                    .ok()
                    .and_then(|value| value["id"].as_u64());
                let error = anyhow::anyhow!("unreadable worker request: {error}");
                match id {
                    Some(id) => answer(events, id, Err(error)),
                    None => events.log("error", &format!("{error:#}")),
                }
                continue;
            }
        };
        match input {
            Input::Run { id, command } => {
                let answers = events.clone();
                let queued = handle.queue_request(
                    Some(id),
                    command,
                    Box::new(move |result| answer(&answers, id, result)),
                );
                match queued {
                    Ok(()) => events.emit(Output::Accepted { id }),
                    Err((error, reply)) => reply(Err(error)),
                }
            }
            Input::Cancel { id } => handle.cancel(id),
            Input::Stop { job } => handle.stop_job(job),
            Input::Status { id } => {
                answer(
                    events,
                    id,
                    serde_json::to_value(handle.snapshot()).map_err(Into::into),
                );
            }
            Input::Ping { id } => answer(
                events,
                id,
                Ok(
                    json!({"heartbeat_age_ms": u64::try_from(handle.heartbeat_age().as_millis()).unwrap_or(u64::MAX)}),
                ),
            ),
            Input::Shutdown => return Ok(()),
        }
    }
}

/// Reply to request `id`.
fn answer(events: &Events, id: u64, result: Result<Value>) {
    let (ok, result, error) = match result {
        Ok(result) => (true, Some(result), None),
        Err(error) => (false, None, Some(protocol::wire_error(&error))),
    };
    events.emit(Output::Reply {
        id,
        ok,
        result,
        error,
    });
}

/// Write the outbox's lines, numbered, until it closes.
fn write_lines(mut output: std::fs::File, events: &Events) {
    let mut seq = 0u64;
    loop {
        let line = match events.next(Duration::from_secs(1)) {
            Next::Line(line) => *line,
            Next::Idle => continue,
            Next::Closed => return,
        };
        let Some(mut data) = encode(seq, &line) else {
            continue;
        };
        data.push(b'\n');
        if output
            .write_all(&data)
            .and_then(|()| output.flush())
            .is_err()
        {
            // The coordinator is gone; stdin closes too.
            return;
        }
        seq += 1;
    }
}

/// Line `seq` as written, without its newline. Camera passwords this worker
/// holds never leave it, even in text a dependency produced: every string
/// of every line is scrubbed, replies, log lines and job ends alike. A line
/// too long for the pipe is never written: a reply becomes an error,
/// anything else is dropped with a note on stderr.
fn encode(seq: u64, line: &Output) -> Option<Vec<u8>> {
    let encoded = if crate::scrub::active() {
        serde_json::to_value(LineRef { seq, output: line }).and_then(|mut value| {
            scrub_strings(&mut value);
            serde_json::to_vec(&value)
        })
    } else {
        serde_json::to_vec(&LineRef { seq, output: line })
    };
    let data = match encoded {
        Ok(data) => data,
        Err(error) => {
            crate::eprintln!("capturefab worker: cannot encode a line: {error}");
            return None;
        }
    };
    if data.len() < MAX_LINE {
        return Some(data);
    }
    let Output::Reply { id, .. } = line else {
        crate::eprintln!("capturefab worker: dropped a {} byte line", data.len());
        return None;
    };
    let error = crate::types::Categorized::error(
        crate::types::ErrorKind::OperationFailed,
        format!(
            "the reply is larger than the {} MiB a reply may be",
            MAX_LINE / (1024 * 1024)
        ),
    );
    serde_json::to_vec(&LineRef {
        seq,
        output: &Output::Reply {
            id: *id,
            ok: false,
            result: None,
            error: Some(protocol::wire_error(&error)),
        },
    })
    .ok()
}

/// Remove registered secrets from every string in `value`, keys included.
fn scrub_strings(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let std::borrow::Cow::Owned(clean) = crate::scrub::scrub(text) {
                *text = clean;
            }
        }
        Value::Array(values) => values.iter_mut().for_each(scrub_strings),
        Value::Object(fields) => {
            let mut clean = serde_json::Map::new();
            for (key, mut value) in std::mem::take(fields) {
                scrub_strings(&mut value);
                clean.insert(crate::scrub::scrub(&key).into_owned(), value);
            }
            *fields = clean;
        }
        _ => {}
    }
}

#[cfg(unix)]
mod platform {

    use anyhow::{Context as _, Result};
    use std::os::fd::FromRawFd as _;

    /// The pipe the coordinator passed as descriptor `fd`.
    pub(super) fn inherited(fd: u64) -> Result<std::fs::File> {
        let fd = i32::try_from(fd).context("--output-fd is not a descriptor")?;
        // SAFETY: the coordinator passed this descriptor for this process
        // alone to write one image to and close; it is set close-on-exec so
        // FFmpeg never holds it.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fcntl verified the inherited descriptor, whose ownership
        // the coordinator transferred to this worker.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    /// Keep the original stdout for the protocol alone, and point fd 1 at
    /// stderr: a stray print, or a child that inherits stdout, cannot
    /// corrupt the protocol.
    pub(super) fn protocol_output() -> Result<std::fs::File> {
        // SAFETY: duplicate this process's standard output.
        let fd = unsafe { libc::dup(1) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: dup returned a new descriptor owned by this file, including
        // when either of the following operations fails.
        let output = unsafe { std::fs::File::from_raw_fd(fd) };
        // SAFETY: fd is the live descriptor owned by output.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: redirect this process's standard output to standard error.
        if unsafe { libc::dup2(2, 1) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(output)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::{AsRawFd as _, IntoRawFd as _};

        #[test]
        fn inherited_descriptors_are_checked_and_kept_out_of_children() {
            inherited(u64::try_from(i32::MAX).unwrap()).unwrap_err();
            let (_reader, writer) = std::io::pipe().unwrap();
            let file = inherited(u64::try_from(writer.into_raw_fd()).unwrap()).unwrap();
            // SAFETY: file owns this live descriptor.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }
}

#[cfg(windows)]
mod platform {

    use anyhow::{Context as _, Result};
    use std::os::windows::io::FromRawHandle as _;

    /// The pipe the coordinator passed as handle `handle`. It is made
    /// uninheritable first, so `FFmpeg` never holds it: the image's reader
    /// would wait for `FFmpeg` to exit to see its end.
    pub(super) fn inherited(handle: u64) -> Result<std::fs::File> {
        let handle =
            usize::try_from(handle).context("--output-fd exceeds host handle size")? as HANDLE;
        // SAFETY: the coordinator made this pipe handle inheritable for this
        // process alone to write one image to and close.
        let file = unsafe { std::fs::File::from_raw_handle(handle.cast()) };
        // SAFETY: file owns this inherited handle while its flag changes.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(file)
    }
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, HANDLE_FLAG_INHERIT,
            SetHandleInformation,
        },
        System::{
            Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle},
            Threading::GetCurrentProcess,
        },
    };

    /// Keep the original stdout for the protocol alone, as a handle no child
    /// inherits, and make stderr the standard output: a stray print, or a
    /// child that inherits stdout, cannot corrupt the protocol.
    pub(super) fn protocol_output() -> Result<std::fs::File> {
        // SAFETY: read this process's own standard handles.
        let original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        // SAFETY: read this process's own standard error handle.
        let errors = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        // SAFETY: the pseudo-handle is valid in this process and is not owned.
        let process = unsafe { GetCurrentProcess() };
        let mut protocol: HANDLE = std::ptr::null_mut();
        // SAFETY: duplicate our stdout within this process, into a writable
        // handle slot, with inheritance disabled.
        if unsafe {
            DuplicateHandle(
                process,
                original,
                process,
                &raw mut protocol,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: DuplicateHandle succeeded and transferred a new handle to us.
        // File closes it if replacing stdout below fails.
        let output = unsafe { std::fs::File::from_raw_handle(protocol.cast()) };
        // SAFETY: replace this process's stdout with its standard error handle.
        if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, errors) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: stdout no longer names original; close the old inheritable
        // handle so FFmpeg cannot keep its pipe open.
        unsafe { CloseHandle(original) };
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_arguments_name_the_session_and_output() {
        let parse = |args: &[&str]| arguments(args.iter().map(std::ffi::OsString::from));
        let parsed = parse(&["--verbose", "--session", "bench", "--output-fd", "5"]).unwrap();
        assert!(parsed.verbose);
        assert_eq!(parsed.session, "bench");
        assert_eq!(parsed.output, Some(5));
        let plain = parse(&[]).unwrap();
        assert!(!plain.verbose);
        assert_eq!(plain.session, "direct");
        assert_eq!(plain.output, None);
        assert!(parse(&["--session"]).is_err());
        assert!(parse(&["--output-fd", "x"]).is_err());
        assert!(parse(&["fd:5"]).is_err());
    }

    #[test]
    fn no_line_carries_a_registered_password() {
        let secret = "worker-line-secret-7d1f";
        crate::scrub::register(secret);
        let lines = [
            Output::Log {
                level: "error".into(),
                message: format!("rtsp open failed: {secret}"),
            },
            Output::Reply {
                id: 3,
                ok: true,
                result: Some(json!({"last_error": format!("auth {secret}"), "logs": [secret]})),
                error: None,
            },
            Output::JobEnd {
                job: 2,
                status: protocol::JobStatus::Failed,
                error: Some(json!({"message": format!("Recording stopped: {secret}")})),
                summary: json!({ secret: 1 }),
            },
        ];
        for (seq, line) in lines.iter().enumerate() {
            let data = encode(seq as u64, line).unwrap();
            let text = String::from_utf8(data).unwrap();
            assert!(!text.contains(secret), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
            let read: protocol::Line = serde_json::from_str(&text).unwrap();
            assert_eq!(read.seq, seq as u64);
        }
    }

    #[test]
    fn a_reply_too_long_for_the_pipe_becomes_an_error() {
        let huge = Output::Reply {
            id: 9,
            ok: true,
            result: Some(json!("x".repeat(MAX_LINE))),
            error: None,
        };
        let read: protocol::Line = serde_json::from_slice(&encode(4, &huge).unwrap()).unwrap();
        assert!(matches!(
            read.output,
            Output::Reply {
                id: 9,
                ok: false,
                ..
            }
        ));
        let log = Output::Log {
            level: "warn".into(),
            message: "x".repeat(MAX_LINE),
        };
        assert!(encode(5, &log).is_none());
    }
}
