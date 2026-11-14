//! Actual worker/helper/encoder startup receipts; no physical camera/readiness proof.
use super::{Home, Run, producer, selector};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read as _, Write as _},
    os::{
        fd::{AsRawFd, FromRawFd as _},
        unix::{fs::PermissionsExt as _, process::CommandExt as _},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const PIXELS: [u8; 6] = [1, 2, 3, 4, 5, 6];
const KEY: u32 = 0x1122_3344;
const FILE_CAP: u32 = 4 * 1024 * 1024;
const LINE_CAP: usize = 64 * 1024;
const PROXY_MODE: &str = "CAPTUREFAB_VIDEO_PROXY_MODE";

fn fail(message: &str) -> io::Error {
    io::Error::other(message)
}
fn duplicate(fd: i32) -> io::Result<File> {
    // SAFETY: fd is a live borrowed standard pipe; dup returns a new owner.
    let copied = unsafe { libc::dup(fd) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful dup is transferred to exactly one File.
    let file = unsafe { File::from_raw_fd(copied) };
    // SAFETY: only close-on-exec changes on the owned duplicate.
    if unsafe { libc::fcntl(copied, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
fn operation_ns() -> io::Result<u64> {
    #[cfg(target_os = "macos")]
    let clock = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_os = "macos"))]
    let clock = libc::CLOCK_MONOTONIC;
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime writes this valid complete timespec on success.
    let result = unsafe { libc::clock_gettime(clock, time.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized the complete timespec.
    let time = unsafe { time.assume_init() };
    let seconds = u64::try_from(time.tv_sec).map_err(|_| fail("negative operation clock"))?;
    let nanos = u64::try_from(time.tv_nsec).map_err(|_| fail("negative operation clock"))?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(nanos))
        .ok_or_else(|| fail("operation clock overflow"))
}
fn bytes(path: &Path) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)?
        .take(u64::from(FILE_CAP) + 1)
        .read_to_end(&mut data)?;
    if data.len() > usize::try_from(FILE_CAP).map_err(|_| fail("file cap overflow"))? {
        return Err(fail("fixture file exceeds cap"));
    }
    Ok(data)
}
fn rows(path: &Path) -> Vec<Value> {
    let data = bytes(path).unwrap();
    assert!(
        data.is_empty() || data.last() == Some(&b'\n'),
        "incomplete final JSONL row"
    );
    data.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}
fn no_key(value: &Value) {
    match value {
        Value::Object(object) => {
            for (name, value) in object {
                assert!(!matches!(
                    name.as_str(),
                    "ActionDeviceKey" | "action_device_key"
                ));
                no_key(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                no_key(value);
            }
        }
        _ => {}
    }
}

struct Flags {
    fd: i32,
    old: i32,
}
impl Flags {
    fn nonblocking(fd: i32) -> io::Result<Self> {
        // SAFETY: fd is borrowed from a live owned stream.
        let old = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if old < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: only O_NONBLOCK changes on this same borrowed descriptor.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, old | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, old })
    }
}
impl Drop for Flags {
    fn drop(&mut self) {
        // SAFETY: every guard is dropped before its borrowed stream.
        unsafe {
            libc::fcntl(self.fd, libc::F_SETFL, self.old);
        }
    }
}
fn ready(fd: i32, events: i16, stop: &AtomicBool, until: Instant) -> io::Result<bool> {
    if stop.load(Ordering::Acquire) {
        return Err(io::Error::from(io::ErrorKind::Interrupted));
    }
    if Instant::now() >= until {
        return Err(io::Error::from(io::ErrorKind::TimedOut));
    }
    let mut poll = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: poll is one initialized entry borrowing this live descriptor.
    let result = unsafe { libc::poll(&raw mut poll, 1, 10) };
    if result > 0 {
        return Ok(true);
    }
    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
        return Err(io::Error::last_os_error());
    }
    Ok(false)
}
fn write_bounded<W: io::Write + AsRawFd>(
    output: &mut W,
    mut data: &[u8],
    stop: &AtomicBool,
    until: Instant,
) -> io::Result<()> {
    while !data.is_empty() {
        if !ready(output.as_raw_fd(), libc::POLLOUT, stop, until)? {
            continue;
        }
        match output.write(data) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => data = &data[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    output.flush()
}
fn lines<R: io::Read + AsRawFd>(
    input: &mut R,
    stop: &AtomicBool,
    until: Instant,
    mut idle: impl FnMut() -> io::Result<()>,
    mut consume: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let _flags = Flags::nonblocking(input.as_raw_fd())?;
    let mut pending = Vec::new();
    let mut block = [0; 4096];
    loop {
        idle()?;
        if !ready(input.as_raw_fd(), libc::POLLIN, stop, until)? {
            continue;
        }
        match input.read(&mut block) {
            Ok(0) => {
                return if pending.is_empty() {
                    Ok(())
                } else {
                    Err(fail("unterminated proxy line"))
                };
            }
            Ok(n) => {
                for &byte in &block[..n] {
                    if pending.len() >= LINE_CAP {
                        return Err(fail("proxy protocol line exceeds cap"));
                    }
                    pending.push(byte);
                    if byte == b'\n' {
                        consume(&pending)?;
                        pending.clear();
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
}
type Trace = Arc<Mutex<File>>;
fn trace(log: &Trace, value: &Value) -> io::Result<()> {
    let mut file = log.lock().map_err(|_| fail("proxy trace lock poisoned"))?;
    serde_json::to_writer(&mut *file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.flush()
}

struct Process {
    child: Child,
    group: bool,
    stop: Arc<AtomicBool>,
}
impl Process {
    fn wait(&mut self, until: Instant) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= until {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.group {
            let pid = i32::try_from(self.child.id()).unwrap();
            // SAFETY: this child was spawned as leader of its own fixture group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        } else {
            let _ = self.child.kill();
        }
        let until = Instant::now() + Duration::from_secs(3);
        let reaped = loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) => {}
                Err(error) => {
                    eprintln!("fixture wait failed: {error}");
                    break false;
                }
            }
            if Instant::now() >= until {
                break false;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            reaped || thread::panicking(),
            "fixture child did not retire after kill"
        );
    }
}
fn limits() -> io::Result<()> {
    for (resource, cap) in [(libc::RLIMIT_FSIZE, FILE_CAP), (libc::RLIMIT_CORE, 0)] {
        let mut value = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: getrlimit initializes this complete output on success.
        let result = unsafe { libc::getrlimit(resource, value.as_mut_ptr()) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: getrlimit succeeded and initialized the complete value.
        let mut value = unsafe { value.assume_init() };
        value.rlim_cur = value
            .rlim_cur
            .min(value.rlim_max)
            .min(libc::rlim_t::from(cap));
        // SAFETY: only lowering the inherited soft bound; hard bound is retained.
        if unsafe { libc::setrlimit(resource, &raw const value) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn bounded_command(command: &mut Command) {
    // SAFETY: the post-fork hook only calls get/setrlimit and reads errno; no
    // locks, allocation or other parent-process state is used on its happy path.
    unsafe {
        command.pre_exec(limits);
    }
}

#[derive(Clone, Copy)]
struct Target {
    id: u64,
    job: u64,
    deadline: u64,
}
#[derive(Default)]
struct Gate {
    target: Option<Target>,
    receipt: Option<Value>,
    held: Option<Vec<u8>>,
    written: u64,
    causal: bool,
}
fn inspect_input(value: &Value, state: &Mutex<Gate>, log: &Trace) -> io::Result<()> {
    let op = value["op"].as_str().ok_or_else(|| fail("input lacks op"))?;
    if op == "run" {
        let command = &value["command"];
        let inner = if command["op"] == "session_until" || command["op"] == "forward_observed_until"
        {
            &command["command"]
        } else {
            command
        };
        let operation = inner["op"]
            .as_str()
            .ok_or_else(|| fail("command lacks op"))?;
        if command["op"] == "forward_observed_until" {
            if operation != "forward" {
                return Err(fail("observed wrapper is not Forward"));
            }
            let target = Target {
                id: value["id"]
                    .as_u64()
                    .ok_or_else(|| fail("missing request id"))?,
                job: inner["job"]
                    .as_u64()
                    .ok_or_else(|| fail("missing global job"))?,
                deadline: command["deadline_ns"]
                    .as_u64()
                    .ok_or_else(|| fail("missing original expiry"))?,
            };
            let now = operation_ns()?;
            if target.deadline <= now || target.deadline - now > 1_500_000_000 {
                return Err(fail("Forward did not retain original startup budget"));
            }
            let mut state = state.lock().map_err(|_| fail("proxy state poisoned"))?;
            if state.target.is_some() {
                return Err(fail("second Forward/retry observed"));
            }
            state.target = Some(target);
            trace(
                log,
                &json!({"op":"forward","id":target.id,"job":target.job,"deadline_ns":target.deadline,"observed_ns":now}),
            )?;
        } else {
            trace(log, &json!({"op":operation,"operation_ns":operation_ns()?}))?;
        }
    } else if op == "stop" {
        trace(
            log,
            &json!({"op":"stop","job":value["job"],"operation_ns":operation_ns()?}),
        )?;
    } else {
        trace(log, &json!({"op":op}))?;
    }
    Ok(())
}
fn causal(state: &mut Gate, log: &Trace, hold: bool) -> io::Result<()> {
    if state.causal || state.written == 0 || (hold && state.held.is_none()) {
        return Ok(());
    }
    let (Some(target), Some(receipt)) = (state.target, state.receipt.as_ref()) else {
        return Ok(());
    };
    let path = receipt["index_path"]
        .as_str()
        .ok_or_else(|| fail("receipt lacks index path"))?;
    let data = bytes(Path::new(path))?;
    let Some(at) = data.iter().position(|byte| *byte == b'\n') else {
        return Ok(());
    };
    let row: Value = serde_json::from_slice(&data[..at]).map_err(io::Error::other)?;
    if row["frame_index"] != 0
        || row["width"] != 3
        || row["height"] != 2
        || row["pixel_format"] != 0x0108_0001_u32
    {
        return Err(fail("unexpected actual first index row"));
    }
    let now = operation_ns()?;
    if now >= target.deadline {
        return Err(fail("no written/index proof before original expiry"));
    }
    state.causal = true;
    trace(
        log,
        &json!({"op":"causal","job":target.job,"written":state.written,"operation_ns":now,"deadline_ns":target.deadline,"first_index_row":row}),
    )
}
fn inspect_output(
    value: &Value,
    line: &[u8],
    state: &Mutex<Gate>,
    log: &Trace,
    hold: bool,
) -> io::Result<(bool, Option<Vec<u8>>)> {
    let mut state = state.lock().map_err(|_| fail("proxy state poisoned"))?;
    let Some(target) = state.target else {
        return Ok((true, None));
    };
    let event = value["event"]
        .as_str()
        .ok_or_else(|| fail("output lacks event"))?;
    if event == "index_owned" && value["job"] == target.job {
        if state.receipt.is_some() {
            return Err(fail("duplicate ownership receipt"));
        }
        let cwd = std::env::current_dir()?;
        let index = value["index_path"]
            .as_str()
            .ok_or_else(|| fail("receipt lacks index"))?;
        let output = value["output_path"]
            .as_str()
            .ok_or_else(|| fail("receipt lacks output"))?;
        let id = value["forwarding_id"]
            .as_str()
            .ok_or_else(|| fail("receipt lacks forwarding id"))?;
        if Path::new(index) != cwd.join("clip.frames.jsonl")
            || Path::new(output) != cwd.join("clip.mkv")
            || id.is_empty()
            || id.len() > 256
            || id.chars().any(char::is_control)
        {
            return Err(fail("receipt does not describe this exact owned output"));
        }
        state.receipt = Some(value.clone());
        trace(log, &json!({"op":"index_owned","receipt":value}))?;
    }
    if event == "progress" && value["job"] == target.job {
        let written = value["progress"]["written"]
            .as_u64()
            .ok_or_else(|| fail("progress lacks written count"))?;
        if written < state.written {
            return Err(fail("written progress decreased"));
        }
        state.written = written;
    }
    if event == "reply" && value["id"] == target.id && hold {
        if value["ok"] != true {
            return Err(fail("actual Forward refused before gate"));
        }
        if state.held.is_some() {
            return Err(fail("second correlated reply"));
        }
        state.held = Some(line.to_vec());
        trace(log, &json!({"op":"held","id":target.id,"job":target.job}))?;
        causal(&mut state, log, hold)?;
        return Ok((false, None));
    }
    causal(&mut state, log, hold)?;
    if event == "job_end" && value["job"] == target.job {
        trace(
            log,
            &json!({"op":"job_end","event":value,"operation_ns":operation_ns()?}),
        )?;
        if hold {
            if !state.causal || operation_ns()? < target.deadline {
                return Err(fail("JobEnd preceded the required original timeout"));
            }
            let late = state
                .held
                .take()
                .ok_or_else(|| fail("no successful held reply at finalization"))?;
            trace(
                log,
                &json!({"op":"released_after_end","job":target.job,"id":target.id}),
            )?;
            return Ok((true, Some(late)));
        }
    }
    Ok((true, None))
}

fn run_proxy(hold: bool) -> io::Result<()> {
    let log = Arc::new(Mutex::new(File::create("proxy.jsonl")?));
    // SAFETY: these calls only inspect this process and its inherited group.
    let pid = unsafe { libc::getpid() };
    // SAFETY: getpgrp has no arguments and no side effects.
    if unsafe { libc::getpgrp() } != pid {
        return Err(fail("proxy is not its owned group leader"));
    }
    let mut ids = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("proxy.pids")?;
    writeln!(ids, "{pid}")?;
    ids.flush()?;
    let stop = Arc::new(AtomicBool::new(false));
    let until = Instant::now() + Duration::from_secs(20);
    let session = std::env::var("CAPTUREFAB_VIDEO_PROXY_SESSION").map_err(io::Error::other)?;
    // Flush the libtest prefix before using an unbuffered duplicate for JSON.
    io::stdout().write_all(b"\n")?;
    io::stdout().flush()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_capturefab"))
        .args(["__worker", "--session", &session])
        .env_remove("CAPTUREFAB_EXECUTABLE")
        .env_remove(PROXY_MODE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut child_input = child
        .stdin
        .take()
        .ok_or_else(|| fail("missing real worker stdin"))?;
    let mut child_output = child
        .stdout
        .take()
        .ok_or_else(|| fail("missing real worker stdout"))?;
    let mut process = Process {
        child,
        group: false,
        stop: stop.clone(),
    };
    trace(
        &log,
        &json!({"op":"proxy_start","pid":pid,"real_worker_pid":process.child.id(),"hold":hold}),
    )?;
    let mut input = duplicate(libc::STDIN_FILENO)?;
    let state = Arc::new(Mutex::new(Gate::default()));
    let input_stop = stop.clone();
    let input_state = state.clone();
    let input_log = log.clone();
    let input_thread = thread::spawn(move || {
        let flags = Flags::nonblocking(child_input.as_raw_fd());
        let result = flags.and_then(|_flags| {
            lines(
                &mut input,
                &input_stop,
                until,
                || Ok(()),
                |line| {
                    let value: Value = serde_json::from_slice(line).map_err(io::Error::other)?;
                    inspect_input(&value, &input_state, &input_log)?;
                    write_bounded(&mut child_input, line, &input_stop, until)
                },
            )
        });
        if result.is_err() {
            input_stop.store(true, Ordering::Release);
        }
        result
    });
    let result = (|| {
        let mut output = duplicate(libc::STDOUT_FILENO)?;
        let _flags = Flags::nonblocking(output.as_raw_fd())?;
        lines(
            &mut child_output,
            &stop,
            until,
            || {
                let mut state = state.lock().map_err(|_| fail("proxy state poisoned"))?;
                causal(&mut state, &log, hold)
            },
            |line| {
                let value: Value = serde_json::from_slice(line).map_err(io::Error::other)?;
                let (pass, late) = inspect_output(&value, line, &state, &log, hold)?;
                if pass {
                    write_bounded(&mut output, line, &stop, until)?;
                }
                if let Some(late) = late {
                    write_bounded(&mut output, &late, &stop, until)?;
                }
                Ok(())
            },
        )
    })();
    stop.store(true, Ordering::Release);
    let join_until = Instant::now() + Duration::from_secs(3);
    while !input_thread.is_finished() {
        if Instant::now() >= join_until {
            return Err(fail("proxy input thread did not stop"));
        }
        thread::sleep(Duration::from_millis(5));
    }
    let input_result = input_thread
        .join()
        .map_err(|_| fail("proxy input thread panicked"))?;
    result?;
    if let Err(error) = input_result
        && error.kind() != io::ErrorKind::Interrupted
    {
        return Err(error);
    }
    let status = process.wait(Instant::now() + Duration::from_secs(3))?;
    trace(&log, &json!({"op":"proxy_end","success":status.success()}))?;
    if !status.success() {
        return Err(fail("real worker failed"));
    }
    Ok(())
}

/// Invoked only through this test executable's private worker wrapper.
#[test]
fn proxy_worker() {
    let Ok(mode) = std::env::var(PROXY_MODE) else {
        return;
    };
    assert!(matches!(mode.as_str(), "hold" | "pass"));
    run_proxy(mode == "hold").unwrap();
}

fn wrapper(home: &Home) -> PathBuf {
    let path = home.path("proxy.sh");
    fs::write(&path,b"#!/bin/sh\n[ \"$1\" = __worker ] || exit 91\nshift\nif [ \"$1\" = --verbose ]; then shift; fi\n[ \"$1\" = --session ] && [ \"$#\" = 2 ] || exit 92\nexport CAPTUREFAB_VIDEO_PROXY_SESSION=\"$2\"\nexec \"$CAPTUREFAB_VIDEO_PROXY_EXE\" --exact video_startup::proxy_worker --nocapture\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn source_config(home: &Home, camera: &str) {
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[{
        "id":"video","protocol":"video","camera":camera,"output":"clip.mkv","index":"clip.frames.jsonl",
        "codec":"ffv1","fps":5,"settings":["Width=3"],"action_device_key":KEY,
        "warmup_ms":0,"timeout_ms":1500
    }]}).to_string()).unwrap();
}
struct Recording {
    process: Process,
    stdout: PathBuf,
    stderr: PathBuf,
    work: PathBuf,
    sdk: PathBuf,
}
impl Recording {
    fn start(home: &Home, cti: &Path, hold: bool) -> Self {
        let stdout = home.path("command.stdout");
        let stderr = home.path("command.stderr");
        let mut command = home.command();
        command
            .env("CAPTUREFAB_EXECUTABLE", wrapper(home))
            .env(
                "CAPTUREFAB_VIDEO_PROXY_EXE",
                std::env::current_exe().unwrap(),
            )
            .env(PROXY_MODE, if hold { "hold" } else { "pass" })
            .env_remove("GENICAM_GENTL64_PATH")
            .env("CAPTUREFAB_GENTL_PRODUCERS", cti)
            .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
            .env("CAPTUREFAB_GENTL_MOCK_LOCK", home.root.join("sdk.lock"))
            .env_remove("CAPTUREFAB_GENTL_MOCK_EPOCH_LOSS")
            .args([
                "--json",
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                if hold { "4s" } else { "2s" },
                "-o",
                "run.jsonl",
            ])
            .stdout(Stdio::from(File::create(&stdout).unwrap()))
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .process_group(0);
        bounded_command(&mut command);
        Self {
            process: Process {
                child: command.spawn().unwrap(),
                group: true,
                stop: Arc::new(AtomicBool::new(false)),
            },
            stdout,
            stderr,
            work: home.work.clone(),
            sdk: home.root.join("sdk.log"),
        }
    }
    fn finish(&mut self) -> Run {
        let status = self
            .process
            .wait(Instant::now() + Duration::from_secs(30))
            .unwrap();
        let output = Output {
            status,
            stdout: bytes(&self.stdout).unwrap(),
            stderr: bytes(&self.stderr).unwrap(),
        };
        Run::of_output(output, &["data", "record", "startup-receipt-fixture"])
    }
}
impl Drop for Recording {
    fn drop(&mut self) {
        let mut groups = BTreeSet::new();
        let paths = [
            (self.work.join("proxy.pids"), false),
            (self.sdk.clone(), true),
        ];
        for (path, sdk) in paths {
            let data = match bytes(&path) {
                Ok(data) => String::from_utf8(data).unwrap(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => panic!("owned process log: {e}"),
            };
            for line in data.lines() {
                let pid = if sdk {
                    let Some((pid, "init")) = line.split_once(' ') else {
                        continue;
                    };
                    pid
                } else {
                    line
                };
                groups.insert(pid.parse::<i32>().unwrap());
            }
        }
        for pid in groups {
            assert!(pid > 1);
            // SAFETY: getpgrp only inspects this fixture; never target its group.
            assert_ne!(pid, unsafe { libc::getpgrp() });
            // SAFETY: private logs identify only owned proxy leaders and the
            // known MODE200 helper. GenTL process::prepare makes that helper a
            // separate group; the real worker/FFmpeg inherit the proxy group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

fn operations(home: &Home) -> Vec<String> {
    String::from_utf8(bytes(&home.root.join("sdk.log")).unwrap())
        .unwrap()
        .lines()
        .map(|line| line.split_once(' ').unwrap().1.to_owned())
        .collect()
}
fn settle_proxy(home: &Home) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let data = bytes(&home.path("proxy.jsonl")).unwrap();
        let complete = data.ends_with(b"\n")
            && data
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .any(|row| row["op"] == "proxy_end");
        if complete {
            return;
        }
        assert!(
            Instant::now() < until,
            "proxy did not finish its actual worker"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn sdk_once(home: &Home) {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let operations = operations(home);
        if operations.iter().any(|op| op == "close_lib") {
            break;
        }
        assert!(
            Instant::now() < until,
            "actual helper teardown did not finish: {operations:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let operations = operations(home);
    for expected in [
        "init",
        "producer_locked",
        "dev_open",
        "action_key_write",
        "action_key_bytes_one",
        "dev_close",
        "close_lib",
    ] {
        assert_eq!(
            operations
                .iter()
                .filter(|op| op.as_str() == expected)
                .count(),
            1,
            "{operations:?}"
        );
    }
    for forbidden in [
        "forbidden_action_key_read",
        "industrial_stream_before_key",
        "industrial_acquire_before_key",
        "producer_lock_busy",
    ] {
        assert!(
            !operations.iter().any(|op| op == forbidden),
            "{operations:?}"
        );
    }
    let keyed = operations
        .iter()
        .position(|op| op == "action_key_bytes_one")
        .unwrap();
    for expected in ["ds_start", "acq_start", "frame"] {
        assert!(keyed < operations.iter().position(|op| op == expected).unwrap());
    }
}
fn assert_trace(home: &Home, hold: bool) -> Value {
    let records = rows(&home.path("proxy.jsonl"));
    for op in [
        "proxy_start",
        "connect",
        "set_values",
        "write_action_device_key_until",
        "forward",
        "index_owned",
        "causal",
        "job_end",
        "proxy_end",
    ] {
        assert_eq!(
            records.iter().filter(|row| row["op"] == op).count(),
            1,
            "missing/duplicate {op}: {records:?}"
        );
    }
    let at = |op: &str| records.iter().position(|row| row["op"] == op).unwrap();
    assert!(at("connect") < at("set_values"));
    assert!(at("set_values") < at("write_action_device_key_until"));
    assert!(at("write_action_device_key_until") < at("forward"));
    let target = records.iter().find(|row| row["op"] == "forward").unwrap();
    let connected = &records[at("connect")];
    assert!(
        target["deadline_ns"].as_u64().unwrap()
            <= connected["operation_ns"].as_u64().unwrap() + 1_500_000_000
    );
    let pids = String::from_utf8(bytes(&home.path("proxy.pids")).unwrap()).unwrap();
    let pids = pids.lines().collect::<Vec<_>>();
    assert_eq!(pids.len(), 1);
    assert_eq!(
        u64::from(pids[0].parse::<u32>().unwrap()),
        records[at("proxy_start")]["pid"].as_u64().unwrap()
    );
    let causal = records.iter().find(|row| row["op"] == "causal").unwrap();
    let owned = &records
        .iter()
        .find(|row| row["op"] == "index_owned")
        .unwrap()["receipt"];
    let ended = &records.iter().find(|row| row["op"] == "job_end").unwrap()["event"];
    assert_eq!(owned["job"], target["job"]);
    assert_eq!(ended["job"], target["job"]);
    let directory = home.work.canonicalize().unwrap();
    assert_eq!(
        owned["index_path"],
        directory
            .join("clip.frames.jsonl")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        owned["output_path"],
        directory.join("clip.mkv").to_string_lossy().as_ref()
    );
    assert!(
        owned["forwarding_id"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert!(causal["written"].as_u64().unwrap() > 0);
    assert!(causal["operation_ns"].as_u64().unwrap() < target["deadline_ns"].as_u64().unwrap());
    let stops = records
        .iter()
        .filter(|row| row["op"] == "stop")
        .collect::<Vec<_>>();
    if hold {
        assert_ne!(stops, [] as [&Value; 0]);
        let stopped = stops[0]["operation_ns"].as_u64().unwrap();
        let expiry = target["deadline_ns"].as_u64().unwrap();
        assert!((expiry..=expiry + 1_000_000_000).contains(&stopped));
        assert!(
            records.iter().position(|row| row["op"] == "stop").unwrap()
                < records
                    .iter()
                    .position(|row| row["op"] == "job_end")
                    .unwrap()
        );
        assert_eq!(records.iter().filter(|row| row["op"] == "held").count(), 1);
        assert_eq!(
            records
                .iter()
                .filter(|row| row["op"] == "released_after_end")
                .count(),
            1
        );
        let end_index = records
            .iter()
            .position(|row| row["op"] == "job_end")
            .unwrap();
        let release = records
            .iter()
            .position(|row| row["op"] == "released_after_end")
            .unwrap();
        assert!(end_index < release);
        assert!(
            records[end_index]["operation_ns"].as_u64().unwrap()
                >= target["deadline_ns"].as_u64().unwrap()
        );
    } else {
        assert!(!records.iter().any(|row| row["op"] == "held"));
    }
    for stop in stops {
        assert_eq!(stop["job"], target["job"]);
    }
    assert_eq!(records.last().unwrap()["op"], "proxy_end");
    assert_eq!(records.last().unwrap()["success"], true);
    no_key(&json!(records));
    Value::clone(ended)
}
fn assert_video(home: &Home, hold: bool, ended: &Value) -> usize {
    let recording = rows(&home.path("run.jsonl"));
    let end = recording.last().unwrap();
    assert_eq!(end["status"], if hold { "failed" } else { "complete" });
    let samples = recording
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(end["samples"], samples.len());
    let frames = samples
        .iter()
        .filter(|row| row["value"]["kind"] == "video_frame")
        .collect::<Vec<_>>();
    assert_ne!(frames, [] as [&&Value; 0]);
    let index = rows(&home.path("clip.frames.jsonl"));
    assert_eq!(index.len(), frames.len());
    assert_eq!(samples.len(), frames.len() + 1);
    assert!(samples.iter().all(|sample| sample["source"] == "video"));

    let origin = index[0]["received_monotonic_ns"]
        .as_u64()
        .unwrap()
        .checked_sub(frames[0]["elapsed_ns"].as_u64().unwrap())
        .unwrap();
    for (number, (row, sample)) in index.iter().zip(&frames).enumerate() {
        assert_eq!(row["frame_index"], number);
        assert_eq!(
            row["video_time_ns"],
            u64::try_from(number).unwrap() * 200_000_000
        );
        assert_eq!(row["width"], 3);
        assert_eq!(row["height"], 2);
        assert_eq!(row["pixel_format"], 0x0108_0001_u32);
        assert!(
            row.get("kind").is_none()
                && row.get("job").is_none()
                && row.get("forwarding_id").is_none()
        );
        let allowed = [
            "frame_index",
            "video_time_ns",
            "frame_id",
            "received_monotonic_ns",
            "width",
            "height",
            "pixel_format",
            "device_ts_ns",
            "device_timestamp",
            "pixel_encoding",
        ];
        assert!(
            row.as_object()
                .unwrap()
                .keys()
                .all(|key| allowed.contains(&key.as_str()))
        );
        for key in [
            "frame_index",
            "video_time_ns",
            "frame_id",
            "width",
            "height",
        ] {
            assert_eq!(row[key], sample["value"][key]);
        }
        assert_eq!(sample["value"]["pixel_format"], "Mono8");
        assert!(row.get("pixel_encoding").is_none());
        assert!(sample["value"].get("pixel_encoding").is_none());
        for key in ["device_ts_ns", "device_timestamp"] {
            assert_eq!(row.get(key), sample["value"].get(key));
        }
        let elapsed = row["received_monotonic_ns"].as_u64().unwrap() - origin;
        assert_eq!(sample["elapsed_ns"], elapsed);
        assert!(
            elapsed
                <= if hold {
                    4_000_000_000_u64
                } else {
                    2_000_000_000_u64
                }
        );
        if number > 0 {
            assert!(
                index[number - 1]["received_monotonic_ns"].as_u64().unwrap()
                    <= row["received_monotonic_ns"].as_u64().unwrap()
            );
        }
    }
    let summary = samples
        .iter()
        .find(|row| row["value"]["kind"] == "video_recording")
        .unwrap();
    assert_eq!(summary["signal"], "recording");
    assert_eq!(summary["value"]["frames"], index.len());
    assert_eq!(summary["value"]["finalized"], true);
    assert_eq!(summary["value"]["codec"], "ffv1");
    assert_eq!(ended["summary"]["written"], index.len());
    assert_eq!(ended["summary"]["finalized"], true);
    for (number, sample) in samples.iter().enumerate() {
        assert_eq!(sample["seq"], number + 1);
        assert!(sample.get("source_time").is_none());
        for key in ["elapsed_ns", "read_duration_ns", "delivery_latency_ns"] {
            assert!(sample[key].is_u64());
        }
    }
    no_key(&json!(recording));
    no_key(&json!(index));
    let mut args = vec![
        "data",
        "export",
        "run.jsonl",
        "-o",
        "export.jsonl",
        "--format",
        "jsonl",
    ];
    if hold {
        args.push("--allow-incomplete");
    }
    assert_eq!(home.ok(&args)["samples"], samples.len());
    let exported = rows(&home.path("export.jsonl"));
    assert_eq!(exported.len(), samples.len());
    for (row, original) in exported.iter().zip(&samples) {
        let mut original = Value::clone(*original);
        assert_eq!(
            original.as_object_mut().unwrap().remove("kind"),
            Some(json!("sample"))
        );
        assert_eq!(*row, original);
    }
    index.len()
}
fn decoded(home: &Home, frames: usize) {
    let output = home.path("decoded.gray");
    let errors = home.path("decode.stderr");
    let mut command =
        Command::new(std::env::var_os("CAPTUREFAB_FFMPEG").unwrap_or_else(|| "ffmpeg".into()));
    command
        .args(["-nostdin", "-v", "error", "-threads", "1", "-i"])
        .arg(home.path("clip.mkv"))
        .args([
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&output).unwrap()))
        .stderr(Stdio::from(File::create(&errors).unwrap()))
        .process_group(0);
    bounded_command(&mut command);
    let mut process = Process {
        child: command.spawn().unwrap(),
        group: true,
        stop: Arc::new(AtomicBool::new(false)),
    };
    assert!(
        process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .success(),
        "{}",
        String::from_utf8_lossy(&bytes(&errors).unwrap())
    );
    let pixels = bytes(&output).unwrap();
    assert_eq!(pixels.len(), frames * PIXELS.len());
    let (pixels, remainder) = pixels.as_chunks::<6>();
    assert_eq!(remainder, &[] as &[u8]);
    for frame in pixels {
        assert_eq!(frame, &PIXELS);
    }
}
fn tree_bound(path: &Path) -> u64 {
    let mut total = 0;
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let metadata = entry.path().symlink_metadata().unwrap();
        assert!(!metadata.file_type().is_symlink());
        total += if metadata.is_dir() {
            tree_bound(&entry.path())
        } else {
            metadata.len()
        };
    }
    assert!(
        total <= 32 * 1024 * 1024,
        "fixture tree exceeds post-run bound"
    );
    total
}

#[test]
#[ignore = "requires existing FFmpeg; actual GenTL/encoder/index startup-reply gate"]
fn owned_forward_late_reply_drains_real_ffv1_prefix() {
    let cti_home = Home::new("video-startup-producer");
    let cti = producer(&cti_home.root, 200);
    for hold in [false, true] {
        let home = Home::new("video-startup-real");
        source_config(&home, &selector(&cti));
        let mut recording = Recording::start(&home, &cti, hold);
        let run = recording.finish();
        settle_proxy(&home);
        if hold {
            assert_eq!(run.failure(4)["code"], "timeout");
        } else {
            run.success();
        }
        let ended = assert_trace(&home, hold);
        let count = assert_video(&home, hold, &ended);
        decoded(&home, count);
        sdk_once(&home);
        tree_bound(&home.root);
    }
}

#[test]
fn preexisting_foreign_index_and_output_are_never_adopted() {
    for existing_index in [true, false] {
        let home = Home::new("video-startup-foreign");
        let cti = home.root.join("must-not-load.cti");
        source_config(&home, &selector(&cti));
        let path = home.path(if existing_index {
            "clip.frames.jsonl"
        } else {
            "clip.mkv"
        });
        let foreign = if existing_index {
            b"{\"frame_index\":0,\"video_time_ns\":0,\"frame_id\":999,\"received_monotonic_ns\":1,\"width\":3,\"height\":2,\"pixel_format\":17301505}\n".as_slice()
        } else {
            b"foreign video bytes".as_slice()
        };
        fs::write(&path, foreign).unwrap();
        assert_eq!(
            Recording::start(&home, &cti, false).finish().failure(1)["code"],
            "exists"
        );
        assert_eq!(bytes(&path).unwrap(), foreign);
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("proxy.pids").exists());
        assert!(!home.root.join("sdk.log").exists());
        assert!(
            !home
                .path(if existing_index {
                    "clip.mkv"
                } else {
                    "clip.frames.jsonl"
                })
                .exists()
        );
        tree_bound(&home.root);
    }
}
