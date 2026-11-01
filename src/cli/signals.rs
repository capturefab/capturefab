//! Interruption. SIGINT, SIGTERM and SIGHUP (not when the process ignores
//! SIGHUP, as under nohup) run the registered handlers once. A command that
//! finishes its own work on a signal (capture, record, serve) takes them
//! over; any other exits at once with `interrupted`, leaving its camera
//! worker to finish the request in hand and close. A second signal always
//! exits at once, with an `interrupted` error unless the command already
//! printed its document. The first signal sets the exit code: 130 for
//! SIGINT, 143 for SIGTERM, 129 for SIGHUP.
use super::output::Output;
use anyhow::Result;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

type Handler = Box<dyn FnMut() + Send>;

/// The first signal, 0 before any.
static SIGNAL: AtomicI32 = AtomicI32::new(0);
/// Set by commands that end their own work on a signal.
static GRACEFUL: AtomicBool = AtomicBool::new(false);
static HANDLERS: Mutex<Vec<Handler>> = Mutex::new(Vec::new());
static OUTPUT: OnceLock<Output> = OnceLock::new();

const SIGINT: i32 = 2;

/// Whether a signal has arrived.
pub fn interrupted() -> bool {
    SIGNAL.load(Ordering::Acquire) != 0
}

/// The exit code of a command a signal interrupted: 128 plus the signal.
pub fn exit_code() -> i32 {
    match SIGNAL.load(Ordering::Acquire) {
        0 => 128 + SIGINT,
        signal => 128 + signal,
    }
}

/// This command ends its own work when a signal arrives: it watches
/// `interrupted`, and its handlers run, but it is not exited.
pub fn take_over() {
    GRACEFUL.store(true, Ordering::Release);
}

/// Run `handler` once, on the first signal.
pub fn on_signal(handler: impl FnMut() + Send + 'static) {
    HANDLERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(handler));
}

/// Handle signals for this invocation, reporting with `out`.
pub fn install(out: Output) -> Result<()> {
    if OUTPUT.set(out).is_err() {
        return Ok(());
    }
    platform::install()
}

/// The first signal: run the handlers, then exit unless the command took
/// over. Runs on the watcher thread, never in the signal handler itself.
fn first() {
    let handlers = std::mem::take(&mut *HANDLERS.lock().unwrap_or_else(|e| e.into_inner()));
    for mut handler in handlers {
        handler();
    }
    if !GRACEFUL.load(Ordering::Acquire) {
        let out = OUTPUT.get().copied().unwrap_or_default();
        let error =
            crate::types::Categorized::error(crate::types::ErrorKind::Interrupted, "interrupted");
        // A command that already printed its document ends on its own.
        if let Some(exit) = out.error_once(&error) {
            std::process::exit(exit);
        }
    }
}

/// A second signal: give up at once, with the error document unless the
/// command printed its own.
fn second() -> ! {
    let out = OUTPUT.get().copied().unwrap_or_default();
    let error = crate::types::Categorized::error(
        crate::types::ErrorKind::Interrupted,
        "interrupted again; stopped without finishing",
    );
    let _ = out.error_once(&error);
    std::process::exit(exit_code())
}

#[cfg(unix)]
mod platform {
    use super::*;
    use anyhow::Context;

    /// The write end of the pipe the signal handler wakes the watcher with.
    static PIPE: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn handle(signal: libc::c_int) {
        let _ = SIGNAL.compare_exchange(0, signal, Ordering::AcqRel, Ordering::Acquire);
        let fd = PIPE.load(Ordering::Acquire);
        if fd >= 0 {
            let byte = 1u8;
            // SAFETY: write is async-signal-safe; the byte outlives the call.
            unsafe {
                libc::write(fd, (&raw const byte).cast(), 1);
            }
        }
    }

    fn ignored(signal: libc::c_int) -> bool {
        // SAFETY: a null new action only reads the current one into `old`.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, std::ptr::null(), &mut old) == 0
                && old.sa_sigaction == libc::SIG_IGN
        }
    }

    pub(super) fn install() -> Result<()> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe writes two descriptors into the array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("signal pipe");
        }
        for fd in fds {
            // SAFETY: fcntl on descriptors this function owns.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        // SAFETY: as above; a full pipe drops the wake-up, not the signal.
        unsafe {
            let flags = libc::fcntl(fds[1], libc::F_GETFL);
            libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        PIPE.store(fds[1], Ordering::Release);
        let read = fds[0];
        std::thread::Builder::new()
            .name("capturefab-signals".into())
            .spawn(move || {
                let mut seen = 0;
                loop {
                    let mut byte = 0u8;
                    // SAFETY: reads one byte into a local.
                    let count = unsafe { libc::read(read, (&raw mut byte).cast(), 1) };
                    if count == 1 {
                        seen += 1;
                        if seen == 1 {
                            first();
                        } else {
                            second();
                        }
                    } else if count == 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        return;
                    }
                }
            })
            .context("signal watcher")?;
        let mut signals = vec![libc::SIGINT, libc::SIGTERM];
        // nohup ignores SIGHUP so the process outlives its terminal.
        if !ignored(libc::SIGHUP) {
            signals.push(libc::SIGHUP);
        }
        for signal in signals {
            // SAFETY: the handler only touches atomics and writes to a pipe.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handle as *const () as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error()).context("install signal handler");
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;
    use anyhow::Context;

    /// Ctrl-C, Ctrl-Break and closing the console all count as SIGINT.
    pub(super) fn install() -> Result<()> {
        let seen = std::sync::atomic::AtomicUsize::new(0);
        ctrlc::set_handler(move || {
            let _ = SIGNAL.compare_exchange(0, SIGINT, Ordering::AcqRel, Ordering::Acquire);
            if seen.fetch_add(1, Ordering::AcqRel) == 0 {
                first();
            } else {
                second();
            }
        })
        .context("install signal handler")
    }
}
