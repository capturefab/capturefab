//! A camera worker process: the hidden `__worker` mode of this program. Its
//! stdin thread never waits on the camera: it queues requests, answers
//! status and ping from shared state, and passes cancels and stops on. One
//! writer thread owns a private copy of the original stdout, which only the
//! protocol uses; anything else printed goes to stderr. A panic on any
//! thread ends the process, which its coordinator sees.
use super::protocol::{self, Input, MAX_LINE, MAX_REQUEST, Output, PROTOCOL};
use crate::engine::{WorkerHandle, events::Events, events::Next};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
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
    ring: std::ffi::OsString,
}
fn arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<Arguments> {
    let mut verbose = false;
    let mut session = String::from("direct");
    let mut ring = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--verbose") => verbose = true,
            Some("--session") => {
                session = args
                    .next()
                    .and_then(|name| name.into_string().ok())
                    .context("--session needs a name")?;
            }
            _ => ring = Some(arg),
        }
    }
    Ok(Arguments {
        verbose,
        session,
        ring: ring.context("missing shared ring")?,
    })
}

#[derive(Serialize)]
struct LineRef<'a> {
    seq: u64,
    #[serde(flatten)]
    output: &'a Output,
}

/// Run as a camera worker; `args` follow `__worker`.
pub fn run_worker(args: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
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
    let ring = std::sync::Arc::new(std::sync::Mutex::new(
        crate::shared_memory::SharedRing::open(&args.ring)?,
    ));
    let handle = WorkerHandle::serving(ring, events.clone(), args.session);
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
    events.close();
    let _ = writer.join();
    read
}

/// Read requests until stdin closes or a shutdown comes.
fn read_requests(handle: &WorkerHandle, events: &Events) -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    loop {
        let mut data = Vec::new();
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
                Ok(json!({"heartbeat_age_ms": handle.heartbeat_age().as_millis() as u64})),
            ),
            Input::Shutdown => return Ok(()),
        }
    }
}

/// Reply to request `id`. Camera passwords this worker holds never leave
/// it, even in text a dependency produced.
fn answer(events: &Events, id: u64, result: Result<Value>) {
    let (ok, result, error) = match result {
        Ok(result) => (true, Some(result), None),
        Err(error) => {
            let mut wire = protocol::wire_error(&error);
            if crate::scrub::active() {
                let text = wire.to_string();
                if let std::borrow::Cow::Owned(clean) = crate::scrub::scrub(&text) {
                    wire = serde_json::from_str(&clean).unwrap_or(wire);
                }
            }
            (false, None, Some(wire))
        }
    };
    events.emit(Output::Reply {
        id,
        ok,
        result,
        error,
    });
}

/// Write the outbox's lines, numbered, until it closes. A line too long for
/// the pipe is never written: a reply becomes an error, anything else is
/// dropped with a note on stderr.
fn write_lines(mut output: std::fs::File, events: &Events) {
    let mut seq = 0u64;
    loop {
        let line = match events.next(Duration::from_secs(1)) {
            Next::Line(line) => *line,
            Next::Idle => continue,
            Next::Closed => return,
        };
        let mut data = match serde_json::to_vec(&LineRef { seq, output: &line }) {
            Ok(data) => data,
            Err(error) => {
                crate::eprintln!("capturefab worker: cannot encode a line: {error}");
                continue;
            }
        };
        if data.len() >= MAX_LINE {
            let Output::Reply { id, .. } = line else {
                crate::eprintln!("capturefab worker: dropped a {} byte line", data.len());
                continue;
            };
            let error = crate::types::Categorized::error(
                crate::types::ErrorKind::OperationFailed,
                format!(
                    "the reply is larger than the {} MiB a reply may be",
                    MAX_LINE / (1024 * 1024)
                ),
            );
            data = serde_json::to_vec(&LineRef {
                seq,
                output: &Output::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(protocol::wire_error(&error)),
                },
            })
            .unwrap_or_default();
        }
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

#[cfg(unix)]
mod platform {
    use anyhow::Result;
    use std::os::fd::FromRawFd;

    /// Keep the original stdout for the protocol alone, and point fd 1 at
    /// stderr: a stray print, or a child that inherits stdout, cannot
    /// corrupt the protocol.
    pub(super) fn protocol_output() -> Result<std::fs::File> {
        // SAFETY: dup, fcntl and dup2 on this process's own standard
        // descriptors; the duplicate is owned by the returned file.
        unsafe {
            let fd = libc::dup(1);
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            if libc::dup2(2, 1) < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(std::fs::File::from_raw_fd(fd))
        }
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::Result;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE},
        System::{
            Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle},
            Threading::GetCurrentProcess,
        },
    };

    /// Keep the original stdout for the protocol alone, as a handle no child
    /// inherits, and make stderr the standard output: a stray print, or a
    /// child that inherits stdout, cannot corrupt the protocol.
    pub(super) fn protocol_output() -> Result<std::fs::File> {
        // SAFETY: duplicating and replacing this process's own standard
        // handles; the duplicate is owned by the returned file.
        unsafe {
            let original = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut protocol: HANDLE = std::ptr::null_mut();
            if DuplicateHandle(
                GetCurrentProcess(),
                original,
                GetCurrentProcess(),
                &mut protocol,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE)) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // The inheritable original would keep the pipe open in FFmpeg.
            CloseHandle(original);
            Ok(std::fs::File::from_raw_handle(protocol as _))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_arguments_name_the_session_and_ring_last() {
        let parse = |args: &[&str]| arguments(args.iter().map(std::ffi::OsString::from));
        let parsed = parse(&["--verbose", "--session", "bench", "fd:5"]).unwrap();
        assert!(parsed.verbose);
        assert_eq!(parsed.session, "bench");
        assert_eq!(parsed.ring, "fd:5");
        let plain = parse(&["fd:5"]).unwrap();
        assert!(!plain.verbose);
        assert_eq!(plain.session, "direct");
        assert!(parse(&["--session"]).is_err());
        assert!(parse(&[]).is_err());
    }
}
