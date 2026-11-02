//! A camera worker process as its coordinator holds it: a re-exec of this
//! program that leads a process group (Unix) or runs in a job object
//! (Windows) with everything it starts, read continuously on a thread of its
//! own, watched for hangs, and ended with everything it started.
use super::protocol::{self, Input, Line, Output, PROTOCOL, WorkerCommand};
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// How long a starting worker may take to say hello.
const HELLO: Duration = Duration::from_secs(10);
/// Worker stderr lines kept for a crash report.
const TAIL: usize = 32;
/// The shortest silence or stalled heartbeat that counts as a hang.
pub(super) const HANG: Duration = Duration::from_secs(10);
/// How long a shutdown waits for a worker before ending it.
const SHUTDOWN: Duration = Duration::from_secs(15);

/// What a worker tells its coordinator besides replies.
pub(super) enum Signal {
    /// Saved files, job progress and ends, and log lines.
    Output(Output),
    /// The process is gone and reaped, with what that means for requests.
    Exited(anyhow::Error),
}
pub(super) type Sink = Arc<dyn Fn(&Shared, Signal) + Send + Sync>;

/// How to start a worker.
pub(super) struct Options {
    /// Info log lines are sent too.
    pub verbose: bool,
    /// The session it serves, which its camera lock names.
    pub session: String,
}

/// Who waits for a reply.
enum Waiter {
    Reply {
        sender: mpsc::Sender<Result<Value>>,
        /// When the camera thread took the request, and its budget.
        started: Option<(Instant, Duration)>,
        /// It writes camera settings.
        changes: bool,
    },
    Ping,
}

/// What a worker's reader thread and its holder share.
pub(super) struct Shared {
    pid: u32,
    child: Mutex<Child>,
    input: Mutex<Option<ChildStdin>>,
    calls: Mutex<BTreeMap<u64, Waiter>>,
    next_id: AtomicU64,
    /// When it last wrote a line, or was given work while idle.
    heard: Mutex<Instant>,
    /// Being watched for hangs: it has requests or jobs.
    watched: AtomicBool,
    ping: AtomicBool,
    /// It was ended for not answering.
    hung: AtomicBool,
    /// It was told to exit.
    closing: AtomicBool,
    /// It ended in the middle of a settings change.
    interrupted_change: AtomicBool,
    exit: Mutex<Option<Value>>,
    exited: Condvar,
    tail: Mutex<VecDeque<String>>,
    /// Its stderr reached its end, so the tail holds its last words.
    errors_ended: AtomicBool,
    /// The ID of the camera it serves, once connected.
    pub(super) label: Mutex<String>,
    tree: tree::Tree,
}

/// A worker process. Dropping it ends the process.
pub(super) struct Worker(Arc<Shared>);

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// This program, to run as a camera worker or a worker's helper. On Linux
/// the running image, so a session outlives an upgrade of the file it was
/// started from.
pub(crate) fn executable() -> Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("CAPTUREFAB_EXECUTABLE") {
        return Ok(path.into());
    }
    if cfg!(target_os = "linux") {
        return Ok("/proc/self/exe".into());
    }
    Ok(std::env::current_exe()?)
}

/// A camera worker leads a process group of its own, which its FFmpeg and helper
/// processes join. A terminal sends Ctrl-C, and the shell sends the SIGHUP of
/// a closed terminal, to the whole foreground group: in this one, they would
/// end a recording before this process could finish it. The worker exits when
/// told to or when its stdin closes, and its group is ended with it.
pub(super) fn command(executable: std::path::PathBuf, options: &Options) -> Command {
    let mut command = Command::new(executable);
    command.arg("__worker");
    if options.verbose {
        command.arg("--verbose");
    }
    command
        .args(["--session", &options.session])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // The console's Ctrl-C reaches every process attached to it, except
    // those in a new process group, and the processes they start.
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::os::windows::process::CommandExt::creation_flags(
            &mut command,
            CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
        );
    }
    command
}

impl Worker {
    /// Start a worker and wait for it to say hello. With `output`, the
    /// worker inherits that pipe's write end, which an image captured to
    /// stdout goes to.
    pub(super) fn spawn(
        options: &Options,
        output: Option<std::io::PipeWriter>,
        sink: Sink,
    ) -> Result<Self> {
        let mut command = command(executable()?, options);
        if let Some(output) = &output {
            tree::pass(&mut command, output)?;
        }
        let child = command.spawn().context("launch camera capture process")?;
        // Only the worker holds the write end now, so the reader sees the
        // end of the image when the worker closes it, or exits.
        drop(output);
        Self::attach(child, sink)
    }
    /// Take over a worker process.
    pub(super) fn attach(mut child: Child, sink: Sink) -> Result<Self> {
        let tree = tree::Tree::adopt(&child);
        let input = child.stdin.take().context("worker stdin")?;
        let output = child.stdout.take().context("worker stdout")?;
        let errors = child.stderr.take().context("worker stderr")?;
        let shared = Arc::new(Shared {
            pid: child.id(),
            child: Mutex::new(child),
            input: Mutex::new(Some(input)),
            calls: Mutex::default(),
            next_id: AtomicU64::new(1),
            heard: Mutex::new(Instant::now()),
            watched: AtomicBool::new(false),
            ping: AtomicBool::new(false),
            hung: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            interrupted_change: AtomicBool::new(false),
            exit: Mutex::new(None),
            exited: Condvar::new(),
            tail: Mutex::default(),
            errors_ended: AtomicBool::new(false),
            label: Mutex::new(String::new()),
            tree,
        });
        let worker = Self(shared.clone());
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("capturefab-worker-stderr".into())
                .spawn(move || shared.read_errors(errors))
                .context("start worker stderr reader")?;
        }
        let (hello, said) = mpsc::channel();
        std::thread::Builder::new()
            .name("capturefab-worker".into())
            .spawn(move || shared.read(output, hello, sink))
            .context("start worker reader")?;
        let started = match said.recv_timeout(HELLO) {
            Ok(started) => started,
            Err(_) => Err(Categorized::new(
                ErrorKind::Timeout,
                format!(
                    "the camera worker did not start within {}",
                    crate::types::duration_text(HELLO)
                ),
            )
            .into()),
        };
        if started.is_err() {
            worker.kill();
        }
        started.map(|()| worker)
    }
    pub(super) fn pid(&self) -> u32 {
        self.0.pid
    }
    pub(super) fn shared(&self) -> &Arc<Shared> {
        &self.0
    }
    /// Whether the process is gone.
    pub(super) fn exited(&self) -> bool {
        locked(&self.0.exit).is_some()
    }
    /// What its exit means for a request, once it is gone.
    pub(super) fn exit_error(&self) -> Option<anyhow::Error> {
        locked(&self.0.exit).as_ref().map(protocol::from_wire)
    }
    /// Send `command`: its request number and where its reply comes.
    pub(super) fn submit(
        &self,
        command: WorkerCommand,
    ) -> Result<(u64, mpsc::Receiver<Result<Value>>)> {
        if let Some(error) = self.exit_error() {
            return Err(error);
        }
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        locked(&self.0.calls).insert(
            id,
            Waiter::Reply {
                sender,
                started: None,
                changes: command.changes_settings(),
            },
        );
        if let Err(error) = self.0.send(&Input::Run { id, command }) {
            locked(&self.0.calls).remove(&id);
            return Err(self.exit_error().unwrap_or(error));
        }
        Ok((id, receiver))
    }
    /// Run `command` and wait for its reply.
    pub(super) fn call(&self, command: impl Into<WorkerCommand>) -> Result<Value> {
        let (_, reply) = self.submit(command.into())?;
        self.wait(&reply)
    }
    /// Run `command`, ending the worker if no reply comes within `limit`:
    /// for a worker the hang watch does not see yet, such as one connecting.
    pub(super) fn call_within(
        &self,
        command: impl Into<WorkerCommand>,
        limit: Duration,
    ) -> Result<Value> {
        let (_, reply) = self.submit(command.into())?;
        match reply.recv_timeout(limit) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.0.hang();
                self.wait_exit(Instant::now() + Duration::from_secs(5));
                Err(self.exit_error().unwrap_or_else(|| {
                    Categorized::new(ErrorKind::Timeout, "the camera worker did not answer").into()
                }))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => self.wait(&reply),
        }
    }
    /// The reply `reply` brings, or why none will come.
    pub(super) fn wait(&self, reply: &mpsc::Receiver<Result<Value>>) -> Result<Value> {
        reply.recv().unwrap_or_else(|_| {
            Err(self
                .exit_error()
                .unwrap_or_else(|| anyhow::anyhow!("the camera worker stopped answering")))
        })
    }
    /// The worker's state, as its reader thread answers it at once.
    pub(super) fn status(&self, wait: Duration) -> Result<crate::session::WorkerSnapshot> {
        if let Some(error) = self.exit_error() {
            return Err(error);
        }
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        locked(&self.0.calls).insert(
            id,
            Waiter::Reply {
                sender,
                started: None,
                changes: false,
            },
        );
        self.0.send(&Input::Status { id })?;
        let result = receiver.recv_timeout(wait);
        locked(&self.0.calls).remove(&id);
        match result {
            Ok(value) => Ok(serde_json::from_value(value?)?),
            Err(_) => Err(Categorized::new(
                ErrorKind::Timeout,
                "the camera worker did not answer a status request",
            )
            .into()),
        }
    }
    /// Cancel request `id`.
    pub(super) fn cancel(&self, id: u64) {
        let _ = self.0.send(&Input::Cancel { id });
    }
    /// Cancel every request in progress.
    pub(super) fn cancel_all(&self) {
        let ids = locked(&self.0.calls)
            .iter()
            .filter(|(_, waiter)| matches!(waiter, Waiter::Reply { .. }))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ids {
            self.cancel(id);
        }
    }
    /// Stop job `job`.
    pub(super) fn stop_job(&self, job: u64) -> Result<()> {
        self.0.send(&Input::Stop { job })
    }
    /// Tell the worker to finish and exit, and close its stdin.
    pub(super) fn close(&self) {
        if !self.0.closing.swap(true, Ordering::AcqRel) {
            let _ = self.0.send(&Input::Shutdown);
            locked(&self.0.input).take();
        }
    }
    /// Wait until the process is gone; false at `deadline`.
    pub(super) fn wait_exit(&self, deadline: Instant) -> bool {
        let mut exit = locked(&self.0.exit);
        while exit.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            exit = self
                .0
                .exited
                .wait_timeout(exit, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
    /// End the process and everything it started.
    pub(super) fn kill(&self) {
        self.0.kill();
    }
    /// Look for a hang while the worker has work: `jobs` says it runs jobs.
    /// It is ended when it says nothing, or its camera thread does nothing,
    /// for longer than the budget of what it runs, and at least `HANG`.
    pub(super) fn watch(&self, jobs: bool) {
        let shared = &self.0;
        if self.exited() {
            return;
        }
        let busy = jobs
            || locked(&shared.calls)
                .values()
                .any(|waiter| matches!(waiter, Waiter::Reply { .. }));
        if !busy {
            shared.watched.store(false, Ordering::Release);
            return;
        }
        if !shared.watched.swap(true, Ordering::AcqRel) {
            *locked(&shared.heard) = Instant::now();
        }
        if locked(&shared.heard).elapsed() > shared.limit() {
            shared.hang();
            return;
        }
        if !shared.ping.swap(true, Ordering::AcqRel) {
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            locked(&shared.calls).insert(id, Waiter::Ping);
            let _ = shared.send(&Input::Ping { id });
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.exited() {
            return;
        }
        self.close();
        if !self.wait_exit(Instant::now() + SHUTDOWN) {
            self.kill();
            self.wait_exit(Instant::now() + Duration::from_secs(2));
        }
    }
}

impl Shared {
    /// Whether it ended in the middle of a settings change, which may have
    /// left the camera half-configured.
    pub(super) fn interrupted_change(&self) -> bool {
        self.interrupted_change.load(Ordering::Acquire)
    }
    fn send(&self, input: &Input) -> Result<()> {
        let mut line = serde_json::to_vec(input)?;
        line.push(b'\n');
        let mut stdin = locked(&self.input);
        let stdin = stdin.as_mut().context("the camera worker is closing")?;
        stdin.write_all(&line)?;
        stdin.flush()?;
        Ok(())
    }
    /// The longest a busy worker may stay silent: the budget of the request
    /// its camera thread runs, and at least `HANG`.
    fn limit(&self) -> Duration {
        locked(&self.calls)
            .values()
            .filter_map(|waiter| match waiter {
                Waiter::Reply {
                    started: Some((_, budget)),
                    ..
                } => Some(*budget),
                _ => None,
            })
            .fold(HANG, Duration::max)
    }
    fn hang(&self) {
        if !self.hung.swap(true, Ordering::AcqRel) {
            crate::eprintln!(
                "capturefab: [{}] the camera worker stopped responding; ending it",
                self.shown()
            );
            self.kill();
        }
    }
    /// End the process and everything it started.
    pub(super) fn kill(&self) {
        let mut child = locked(&self.child);
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if !self.tree.terminate(self.pid) {
            let _ = child.kill();
        }
    }
    /// Read the worker's lines until it closes its end: hello first, then
    /// replies and events.
    fn read(
        self: Arc<Self>,
        output: std::process::ChildStdout,
        hello: mpsc::Sender<Result<()>>,
        sink: Sink,
    ) {
        let mut reader = BufReader::new(output);
        let mut hello = Some(hello);
        // A worker run by a test harness prints before it starts.
        let mut skipped = 0;
        loop {
            let line = match read_line(&mut reader, protocol::MAX_LINE) {
                Ok(Some(Some(line))) => line,
                Ok(Some(None)) => {
                    crate::eprintln!("capturefab: ignoring an oversized line from a camera worker");
                    continue;
                }
                Ok(None) | Err(_) => break,
            };
            *locked(&self.heard) = Instant::now();
            let line = match serde_json::from_slice::<Line>(&line) {
                Ok(line) => line,
                Err(error) => {
                    if hello.is_some() && skipped < 64 {
                        skipped += 1;
                    } else {
                        crate::eprintln!("capturefab: ignoring an unreadable worker line: {error}");
                    }
                    continue;
                }
            };
            if let Some(said) = hello.take() {
                let Output::Hello { protocol, build } = line.output else {
                    let _ = said.send(Err(anyhow::anyhow!(
                        "the camera worker did not start with hello"
                    )));
                    self.kill();
                    break;
                };
                if protocol != PROTOCOL {
                    let _ = said.send(Err(anyhow::anyhow!(
                        "the camera worker ({build}) speaks protocol {protocol}, not {PROTOCOL}; run one capturefab version"
                    )));
                    self.kill();
                    break;
                }
                let _ = said.send(Ok(()));
                continue;
            }
            match line.output {
                Output::Hello { .. } | Output::Accepted { .. } => {}
                Output::Started { id, budget_ms } => {
                    if let Some(Waiter::Reply { started, .. }) = locked(&self.calls).get_mut(&id) {
                        *started = Some((Instant::now(), Duration::from_millis(budget_ms)));
                    }
                }
                Output::Reply {
                    id,
                    ok,
                    result,
                    error,
                } => {
                    let waiter = locked(&self.calls).remove(&id);
                    match waiter {
                        Some(Waiter::Reply { sender, .. }) => {
                            let _ = sender.send(if ok {
                                Ok(result.unwrap_or_default())
                            } else {
                                Err(protocol::from_wire(&error.unwrap_or_default()))
                            });
                        }
                        Some(Waiter::Ping) => {
                            self.ping.store(false, Ordering::Release);
                            let age = result
                                .as_ref()
                                .and_then(|result| result["heartbeat_age_ms"].as_u64())
                                .unwrap_or(0);
                            if Duration::from_millis(age) > self.limit() {
                                self.hang();
                            }
                        }
                        None => {}
                    }
                }
                output => sink(&self, Signal::Output(output)),
            }
        }
        if let Some(said) = hello.take() {
            self.drain_errors();
            let tail = self.tail();
            let _ = said.send(Err(anyhow::anyhow!(
                "the camera worker exited before it started{}",
                tail.last()
                    .map(|line| format!(": {line}"))
                    .unwrap_or_default()
            )));
        }
        let status = self.reap();
        let error = self.exit_error(status);
        let wire = protocol::wire_error(&error);
        *locked(&self.exit) = Some(wire.clone());
        self.exited.notify_all();
        let calls = std::mem::take(&mut *locked(&self.calls));
        if calls.values().any(|waiter| {
            matches!(
                waiter,
                Waiter::Reply {
                    started: Some(_),
                    changes: true,
                    ..
                }
            )
        }) {
            self.interrupted_change.store(true, Ordering::Release);
        }
        for waiter in calls.into_values() {
            if let Waiter::Reply { sender, .. } = waiter {
                let _ = sender.send(Err(protocol::from_wire(&wire)));
            }
        }
        sink(&self, Signal::Exited(error));
    }
    /// Wait for the process after it closed its output, ending it if it
    /// lingers, then end what it left running in its group.
    fn reap(&self) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(2);
        // A worker exits within a millisecond or two of closing its
        // output: look again soon, then less and less often.
        let mut pause = Duration::from_micros(50);
        let status = loop {
            if let Ok(Some(status)) = locked(&self.child).try_wait() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                self.kill();
                break locked(&self.child).wait().ok();
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(5));
        };
        // A worker that died leaves its FFmpeg running in its group.
        self.tree.terminate(self.pid);
        status
    }
    /// What the exit means: a hang, a shutdown, or a crash with how it ended.
    fn exit_error(&self, status: Option<ExitStatus>) -> anyhow::Error {
        self.drain_errors();
        let tail = self.tail();
        #[cfg(unix)]
        let signal =
            status.and_then(|status| std::os::unix::process::ExitStatusExt::signal(&status));
        #[cfg(not(unix))]
        let signal: Option<i32> = None;
        let code = status.and_then(|status| status.code());
        let details = json!({"exit": code, "signal": signal, "stderr_tail": tail});
        if self.hung.load(Ordering::Acquire) {
            return Categorized::new(
                ErrorKind::Timeout,
                "the camera worker stopped responding and was ended",
            )
            .hint("the next command reconnects the camera")
            .details(details)
            .into();
        }
        if self.closing.load(Ordering::Acquire) && status.is_some_and(|status| status.success()) {
            return super::stopping();
        }
        let how = status.map_or_else(
            || "with an unknown status".into(),
            |status| status.to_string(),
        );
        Categorized::new(
            ErrorKind::WorkerCrashed,
            format!("camera worker exited ({how})"),
        )
        .hint("the next command reconnects the camera")
        .details(details)
        .into()
    }
    /// Its camera as log lines name it, with credentials redacted.
    fn shown(&self) -> String {
        let label = locked(&self.label);
        if label.is_empty() {
            "camera".into()
        } else {
            crate::media::redact_url(&label)
        }
    }
    fn tail(&self) -> Vec<String> {
        locked(&self.tail).iter().cloned().collect()
    }
    /// Wait a moment for the stderr lines still in the pipe when the worker's
    /// exit is seen, such as a panic's message after its backtrace.
    fn drain_errors(&self) {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut pause = Duration::from_micros(50);
        while !self.errors_ended.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(5));
        }
    }
    /// Keep the worker's last stderr lines and pass them on, tagged with its
    /// camera.
    fn read_errors(&self, errors: std::process::ChildStderr) {
        let mut reader = BufReader::new(errors);
        loop {
            let line = match read_line(&mut reader, 64 * 1024) {
                Ok(Some(Some(line))) => line,
                Ok(Some(None)) => continue,
                Ok(None) | Err(_) => {
                    self.errors_ended.store(true, Ordering::Release);
                    return;
                }
            };
            let text = crate::scrub::redact_urls(String::from_utf8_lossy(&line).trim_end());
            if text.is_empty() {
                continue;
            }
            crate::eprintln!("capturefab: [{}] {text}", self.shown());
            let mut tail = locked(&self.tail);
            if tail.len() == TAIL {
                tail.pop_front();
            }
            tail.push_back(text);
        }
    }
}

/// One line of at most `max` bytes, without its newline: None at the end,
/// Some(None) for a longer line, which is skipped.
fn read_line(reader: &mut impl BufRead, max: usize) -> std::io::Result<Option<Option<Vec<u8>>>> {
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(max as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        return Ok(Some(Some(line)));
    }
    if line.len() <= max {
        // The last line, without a newline.
        return Ok(Some(Some(line)));
    }
    // Skip the rest of the oversized line.
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(Some(None));
        }
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                reader.consume(end + 1);
                return Ok(Some(None));
            }
            None => {
                let length = buffer.len();
                reader.consume(length);
            }
        }
    }
}

#[cfg(unix)]
mod tree {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};

    /// Let the worker `command` starts inherit `output`, and name it.
    pub(super) fn pass(
        command: &mut std::process::Command,
        output: &std::io::PipeWriter,
    ) -> anyhow::Result<()> {
        let fd = output.as_raw_fd();
        command.args(["--output-fd", &fd.to_string()]);
        // SAFETY: fcntl is async-signal-safe, as pre_exec requires; the
        // descriptor is this process's own pipe, which outlives the spawn.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// A worker's process group, which it leads.
    pub(super) struct Tree;
    impl Tree {
        pub(super) fn adopt(_child: &std::process::Child) -> Self {
            Self
        }
        /// End the group: the worker and everything it started.
        pub(super) fn terminate(&self, pid: u32) -> bool {
            let Ok(group) = libc::pid_t::try_from(pid) else {
                return false;
            };
            // SAFETY: killpg only sends a signal, to the group this worker
            // leads; a group outlives its leader while any member runs.
            unsafe { libc::killpg(group, libc::SIGKILL) == 0 }
        }
    }
}

#[cfg(windows)]
mod tree {
    use anyhow::Context;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
    };

    /// Let the worker `command` starts inherit `output`, and name it.
    pub(super) fn pass(
        command: &mut std::process::Command,
        output: &std::io::PipeWriter,
    ) -> anyhow::Result<()> {
        let handle = output.as_raw_handle();
        // SAFETY: the handle is this process's own pipe end.
        if unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
            == 0
        {
            return Err(std::io::Error::last_os_error()).context("stdout capture pipe");
        }
        command.args(["--output-fd", &(handle as usize).to_string()]);
        Ok(())
    }

    /// A job object holding a worker and every process it starts, which are
    /// ended when it closes.
    pub(super) struct Tree(HANDLE);
    // SAFETY: a job object handle may be used from any thread.
    unsafe impl Send for Tree {}
    unsafe impl Sync for Tree {}
    impl Tree {
        pub(super) fn adopt(child: &std::process::Child) -> Self {
            // SAFETY: a new unnamed job with default security; its handle is
            // checked before use and closed on drop.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Self(std::ptr::null_mut());
                }
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 || AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    crate::eprintln!(
                        "capturefab: camera worker outside a job object; FFmpeg may outlive it"
                    );
                    CloseHandle(job);
                    return Self(std::ptr::null_mut());
                }
                Self(job)
            }
        }
        /// End the worker and everything it started.
        pub(super) fn terminate(&self, _pid: u32) -> bool {
            // SAFETY: the job handle this tree owns.
            !self.0.is_null() && unsafe { TerminateJobObject(self.0, 1) } != 0
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: closing the handle this tree owns ends what is left
                // in the job.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_start_in_the_hidden_worker_mode() {
        let options = Options {
            verbose: false,
            session: "bench".into(),
        };
        let command = command("capturefab".into(), &options);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["__worker", "--session", "bench"]
        );
    }

    #[test]
    fn oversized_lines_are_skipped_whole() {
        let mut input = std::io::Cursor::new(b"short\n0123456789abc\nnext".to_vec());
        assert_eq!(
            read_line(&mut input, 8).unwrap(),
            Some(Some(b"short".to_vec()))
        );
        assert_eq!(read_line(&mut input, 8).unwrap(), Some(None));
        assert_eq!(
            read_line(&mut input, 8).unwrap(),
            Some(Some(b"next".to_vec()))
        );
        assert_eq!(read_line(&mut input, 8).unwrap(), None);
    }
}
