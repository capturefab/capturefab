//! Interruption. SIGINT, SIGTERM and SIGHUP (not when the process ignores
//! SIGHUP, as under nohup) run the registered handlers once. A command that
//! finishes its own work on a signal (capture, record, serve) takes them
//! over; any other exits at once with `interrupted`, leaving its camera
//! worker to finish the request in hand and close. A second signal always
//! exits at once, ending the camera workers this process started with
//! everything they started, with an `interrupted` error unless the command
//! already printed its document. The first signal sets the exit code: 130
//! for SIGINT, 143 for SIGTERM, 129 for SIGHUP.
//!
//! In a shell each command runs in a scope of its own: a signal interrupts
//! the command running, as it would the process, and the shell goes on. A
//! command that does not take signals over, or one interrupted twice, is
//! left to finish on its own while the shell takes the next.
#![deny(trivial_numeric_casts)]
use super::output::Output;
use anyhow::Result;
use std::{
    cell::Cell,
    sync::{
        Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

type Handler = Box<dyn FnMut() + Send>;

/// The first signal, 0 before any (in a shell: in the running command).
static SIGNAL: AtomicI32 = AtomicI32::new(0);
/// The latest signal.
static LAST: AtomicI32 = AtomicI32::new(0);
/// Signals seen (in a shell: by the running command).
static SEEN: AtomicUsize = AtomicUsize::new(0);
/// Set by commands that end their own work on a signal.
static GRACEFUL: AtomicBool = AtomicBool::new(false);
static HANDLERS: Mutex<Vec<Handler>> = Mutex::new(Vec::new());
/// Run on a second signal, before the process exits.
static KILLERS: Mutex<Vec<Handler>> = Mutex::new(Vec::new());
static OUTPUT: OnceLock<Output> = OnceLock::new();
/// Wakes `sleep`ers when a signal arrives.
static NAP: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// What a shell is told of a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellSignal {
    /// The command of this scope was interrupted and does not end its own
    /// work: stop waiting for it.
    Interrupted(u64),
    /// The command of this scope was interrupted again.
    Again(u64),
    /// A signal between commands.
    Idle(i32),
}
type Notify = Box<dyn Fn(ShellSignal) + Send + Sync>;
/// Set once commands run as a shell's: where signals go.
static SHELL: OnceLock<Notify> = OnceLock::new();
/// The newest command scope begun, and the one running (0 between them).
static NEWEST: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicU64 = AtomicU64::new(0);
/// The newest scope a signal interrupted.
static SIGNALLED: AtomicU64 = AtomicU64::new(0);
thread_local! {
    /// The scope of the command this thread runs, 0 for other threads.
    static SCOPE: Cell<u64> = const { Cell::new(0) };
}

const SIGINT: i32 = 2;

/// Whether a signal has arrived: in a shell, for the command this thread
/// runs, or for one the shell has since left behind.
pub(super) fn interrupted() -> bool {
    if SHELL.get().is_none() {
        return SIGNAL.load(Ordering::Acquire) != 0;
    }
    let newest = NEWEST.load(Ordering::Acquire);
    let mine = match SCOPE.with(Cell::get) {
        0 => newest,
        scope => scope,
    };
    mine < newest || (mine > 0 && SIGNALLED.load(Ordering::Acquire) >= mine)
}

/// The exit code of a command a signal interrupted: 128 plus the signal.
pub(super) fn exit_code() -> i32 {
    match SIGNAL.load(Ordering::Acquire) {
        0 => 128 + SIGINT,
        signal => 128 + signal,
    }
}

/// This command ends its own work when a signal arrives: it watches
/// `interrupted`, and its handlers run, but it is not exited.
pub(super) fn take_over() {
    GRACEFUL.store(true, Ordering::Release);
}

/// Run `handler` once, on the first signal.
pub(super) fn on_signal(handler: impl FnMut() + Send + 'static) {
    HANDLERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Box::new(handler));
}

/// Run `killer` on a second signal, just before the process exits: what
/// ends the camera workers this process started, which lead process
/// groups of their own and so never see the terminal's signal.
pub(super) fn on_second(killer: impl FnMut() + Send + 'static) {
    KILLERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Box::new(killer));
}

/// Sleep for `duration`, or less when a signal arrives.
pub(super) fn sleep(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut nap = NAP
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !interrupted() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        nap = NAP
            .1
            .wait_timeout(nap, left)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

fn wake_sleepers() {
    let _nap = NAP
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    NAP.1.notify_all();
}

/// Handle signals for this invocation, reporting with `out`.
pub(super) fn install(out: Output) -> Result<()> {
    if OUTPUT.set(out).is_err() {
        return Ok(());
    }
    platform::install()
}

/// Whether commands run as a shell's.
pub(super) fn in_shell() -> bool {
    SHELL.get().is_some()
}

/// Run commands as a shell's from now on: signals go to `notify` instead
/// of ending the process.
pub(super) fn shell(notify: impl Fn(ShellSignal) + Send + Sync + 'static) {
    let _ = SHELL.set(Box::new(notify));
}

/// Begin a command's scope: no signal, handler or takeover of an earlier
/// command carries into it. The thread that runs the command `enter`s it.
pub(super) fn begin() -> u64 {
    let scope = NEWEST.fetch_add(1, Ordering::AcqRel) + 1;
    SIGNAL.store(0, Ordering::Release);
    SEEN.store(0, Ordering::Release);
    GRACEFUL.store(false, Ordering::Release);
    HANDLERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    KILLERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    RUNNING.store(scope, Ordering::Release);
    scope
}

/// Run on this thread as the command of `scope`.
pub(super) fn enter(scope: u64) {
    SCOPE.with(|cell| cell.set(scope));
}

/// The shell is between commands.
pub(super) fn end() {
    RUNNING.store(0, Ordering::Release);
}

/// What a thread a command starts carries over from the command: its
/// scope, and the owner of its session requests. A signal then interrupts
/// the thread with the command, and what it waits for closes with the
/// command's requests.
#[derive(Debug, Clone, Copy)]
pub(super) struct Lineage {
    scope: u64,
    owner: u64,
}
impl Lineage {
    /// This thread's.
    pub(super) fn here() -> Self {
        Self {
            scope: SCOPE.with(Cell::get),
            owner: crate::ipc::requests_owner(),
        }
    }
    /// Run on this thread as part of that command.
    pub(super) fn enter(self) {
        enter(self.scope);
        crate::ipc::own_requests(self.owner);
    }
}

/// A signal arrived: what the process, or the shell, does about it. Runs
/// on the watcher thread, never in the signal handler itself.
fn arrived() {
    let Some(notify) = SHELL.get() else {
        if SEEN.fetch_add(1, Ordering::AcqRel) == 0 {
            wake_sleepers();
            first();
        } else {
            second();
        }
        return;
    };
    let scope = RUNNING.load(Ordering::Acquire);
    if scope == 0 {
        notify(ShellSignal::Idle(LAST.load(Ordering::Acquire)));
        return;
    }
    if SEEN.fetch_add(1, Ordering::AcqRel) > 0 {
        notify(ShellSignal::Again(scope));
        return;
    }
    SIGNALLED.fetch_max(scope, Ordering::AcqRel);
    wake_sleepers();
    run_handlers();
    if !GRACEFUL.load(Ordering::Acquire) {
        notify(ShellSignal::Interrupted(scope));
    }
}

fn run_handlers() {
    let handlers = std::mem::take(
        &mut *HANDLERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for mut handler in handlers {
        handler();
    }
}

/// The first signal: run the handlers, then exit unless the command took
/// over.
fn first() {
    run_handlers();
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
    let killers = std::mem::take(
        &mut *KILLERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for mut killer in killers {
        killer();
    }
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

    use super::{AtomicI32, LAST, Ordering, Result, SIGNAL, arrived};
    use anyhow::Context as _;
    use std::{
        io::Read as _,
        os::fd::{AsRawFd as _, IntoRawFd as _},
    };

    /// The write end of the pipe the signal handler wakes the watcher with.
    static PIPE: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn handle(signal: libc::c_int) {
        let _ = SIGNAL.compare_exchange(0, signal, Ordering::AcqRel, Ordering::Acquire);
        LAST.store(signal, Ordering::Release);
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
        // SAFETY: sigaction's integer and nullable pointer fields permit zero.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: a null new action only reads the current one into `old`.
        let status = unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut old) };
        status == 0 && old.sa_sigaction == libc::SIG_IGN
    }

    fn signal_pipe() -> Result<(std::io::PipeReader, std::io::PipeWriter)> {
        let (reader, writer) = std::io::pipe().context("signal pipe")?;
        let fd = writer.as_raw_fd();
        // SAFETY: writer owns this live descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error()).context("signal pipe flags");
        }
        // SAFETY: writer owns fd; a full pipe must drop the wake-up, not block.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error()).context("nonblocking signal pipe");
        }
        Ok((reader, writer))
    }

    pub(super) fn install() -> Result<()> {
        let (mut reader, writer) = signal_pipe()?;
        std::thread::Builder::new()
            .name("capturefab-signals".into())
            .spawn(move || {
                loop {
                    match reader.read(&mut [0u8; 1]) {
                        Ok(0) => return,
                        Ok(_) => arrived(),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => return,
                    }
                }
            })
            .context("signal watcher")?;
        // The signal handler owns this write end for the process's lifetime.
        PIPE.store(writer.into_raw_fd(), Ordering::Release);
        // nohup ignores SIGHUP so the process outlives its terminal.
        let hangup = (!ignored(libc::SIGHUP)).then_some(libc::SIGHUP);
        for signal in [libc::SIGINT, libc::SIGTERM].into_iter().chain(hangup) {
            // SAFETY: sigaction's integer and nullable pointer fields permit zero.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = handle as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            // SAFETY: action owns a writable signal set.
            if unsafe { libc::sigemptyset(&raw mut action.sa_mask) } != 0 {
                return Err(std::io::Error::last_os_error()).context("signal handler mask");
            }
            // SAFETY: the handler only touches atomics and writes to a pipe.
            if unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) } != 0 {
                return Err(std::io::Error::last_os_error()).context("install signal handler");
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write as _;

        #[test]
        fn signal_pipe_is_nonblocking_and_not_inherited() {
            let (mut reader, mut writer) = signal_pipe().unwrap();
            for fd in [reader.as_raw_fd(), writer.as_raw_fd()] {
                // SAFETY: reader and writer own these live descriptors.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert!(flags >= 0);
                assert_ne!(flags & libc::FD_CLOEXEC, 0);
            }
            // SAFETY: writer owns this live descriptor.
            let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::O_NONBLOCK, 0);
            writer.write_all(&[1]).unwrap();
            let mut byte = [0];
            reader.read_exact(&mut byte).unwrap();
            assert_eq!(byte, [1]);
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use super::{LAST, Ordering, Result, SIGINT, SIGNAL, arrived};
    use anyhow::Context as _;

    /// Ctrl-C, Ctrl-Break and closing the console all count as SIGINT.
    pub(super) fn install() -> Result<()> {
        ctrlc::set_handler(move || {
            let _ = SIGNAL.compare_exchange(0, SIGINT, Ordering::AcqRel, Ordering::Acquire);
            LAST.store(SIGINT, Ordering::Release);
            arrived();
        })
        .context("install signal handler")
    }
}
