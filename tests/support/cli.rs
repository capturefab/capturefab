//! Run the built binary with its own state folders, and read what it says.
#![allow(dead_code)]
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Spawned binaries keep their cache, such as the extracted FFmpeg, out of
/// the user's.
pub fn test_cache() -> PathBuf {
    std::env::temp_dir().join("capturefab-test-cache")
}

/// A private folder for one test, removed when it drops.
pub struct Home {
    pub root: PathBuf,
    pub sessions: PathBuf,
    pub work: PathBuf,
}
impl Home {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "capturefab-{name}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = Self {
            sessions: root.join("sessions"),
            work: root.join("work"),
            root,
        };
        std::fs::create_dir_all(&home.work).unwrap();
        home
    }
    /// The binary with isolated state, run in the work folder. Camera
    /// commands open their camera for themselves alone, as with `--keep 0`,
    /// unless a test asks for the warm default (`warm`).
    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
        command
            .env_remove("CAPTUREFAB_SESSION")
            .env_remove("CAPTUREFAB_CAMERA")
            .env_remove("CAPTUREFAB_JSON")
            .env_remove("CAPTUREFAB_CAMERA_PASSWORD")
            // It would take state out of the test's folder.
            .env_remove("CAPTUREFAB_STATE_DIR")
            .env("CAPTUREFAB_KEEP", "0")
            // Stills compared byte for byte come from libjpeg-turbo, not
            // whichever hardware engine the host has.
            .env("CAPTUREFAB_JPEG", "cpu")
            .env("CAPTUREFAB_SESSION_DIR", &self.sessions)
            .env("CAPTUREFAB_CACHE_DIR", test_cache())
            .current_dir(&self.work)
            .stdin(Stdio::null());
        command
    }
    pub fn run(&self, args: &[&str]) -> Run {
        Run::of(self.command().args(args).output().unwrap(), args)
    }
    /// The binary as people run it: camera commands keep their camera warm
    /// in the `auto` session.
    pub fn warm_command(&self) -> Command {
        let mut command = self.command();
        command.env_remove("CAPTUREFAB_KEEP");
        command
    }
    /// `ARGS` with the warm default.
    pub fn warm(&self, args: &[&str]) -> Run {
        Run::of(self.warm_command().args(args).output().unwrap(), args)
    }
    pub fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Run {
        Run::of(
            self.command()
                .envs(env.iter().copied())
                .args(args)
                .output()
                .unwrap(),
            args,
        )
    }
    pub fn run_stdin(&self, input: &str, args: &[&str]) -> Run {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        Run::of(child.wait_with_output().unwrap(), args)
    }
    /// `--json ARGS`, which must succeed: its result document.
    pub fn ok(&self, args: &[&str]) -> Value {
        let run = self.run(&[&["--json"][..], args].concat());
        run.success()
    }
    /// `--json ARGS`, which must fail with `exit`: its error object.
    pub fn fails(&self, exit: i32, args: &[&str]) -> Value {
        let run = self.run(&[&["--json"][..], args].concat());
        run.failure(exit)
    }
    /// A `serve` session in the foreground of a child process, ready.
    pub fn serve(&self, name: &str, cameras: &[&str]) -> Session {
        self.serve_with(name, cameras, &[])
    }
    /// `serve` with more arguments, such as `--live`.
    pub fn serve_with(&self, name: &str, cameras: &[&str], extra: &[&str]) -> Session {
        let mut command = self.command();
        self.serve_in(&mut command, name, cameras, extra)
    }
    /// `serve` from `command`, which may set its own folder and variables.
    pub fn serve_in(
        &self,
        command: &mut Command,
        name: &str,
        cameras: &[&str],
        extra: &[&str],
    ) -> Session {
        let mut args = vec!["--json", "serve", "--name", name];
        for camera in cameras {
            args.extend(["-c", camera]);
        }
        args.extend(extra);
        command
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let ready: Value = serde_json::from_str(&ready).unwrap_or_else(|_| panic!("{ready}"));
        assert_eq!(ready["ready"], true, "{ready}");
        Session { child }
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.work.join(name)
    }
    /// `path`, as the binary sees it: with symlinks resolved, as in
    /// macOS's /private/var for /var.
    pub fn real(&self, name: &str) -> PathBuf {
        self.work.canonicalize().unwrap().join(name)
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        // Sessions started in the background by `session start`, --keep or
        // --background outlive the command that started them.
        let detached = std::fs::read_dir(&self.sessions)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|e| e == "json"));
        if detached {
            let listed = self.run(&["--json", "session", "list"]);
            if let Ok(list) = serde_json::from_str::<Value>(&listed.stdout) {
                for session in list["sessions"].as_array().into_iter().flatten() {
                    if let Some(name) = session["session"].as_str() {
                        let _ = self.run(&["--json", "session", "stop", name]);
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub struct Session {
    pub child: Child,
}
impl Session {
    /// Send `signal` and wait up to 30 s for the session to exit.
    #[cfg(unix)]
    pub fn signal(&mut self, signal: i32) -> std::process::ExitStatus {
        signal_and_wait(&mut self.child, signal)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Send `signal` to `child` and wait up to 30 s for it to exit.
#[cfg(unix)]
pub fn signal_and_wait(child: &mut Child, signal: i32) -> std::process::ExitStatus {
    // SAFETY: kill only sends a signal to the child this test spawned.
    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(std::time::Instant::now() < deadline, "still running");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Whether process `pid` still exists, within a few seconds for its
/// parent to reap it.
#[cfg(unix)]
pub fn process_lives(pid: i32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The FFmpeg tests decode with: CAPTUREFAB_FFMPEG, else one on PATH.
pub fn ffmpeg() -> String {
    std::env::var("CAPTUREFAB_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

/// Whether FFmpeg decodes `path` without an error.
pub fn decodes(path: &Path) -> bool {
    let decoded = Command::new(ffmpeg())
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    decoded.status.success() && decoded.stderr.is_empty()
}

/// What one run printed and how it exited.
pub struct Run {
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub raw: Vec<u8>,
}
impl Run {
    /// What a command run by the caller printed.
    pub fn of_output(output: std::process::Output, args: &[&str]) -> Self {
        Self::of(output, args)
    }
    fn of(output: std::process::Output, args: &[&str]) -> Self {
        Self {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            raw: output.stdout,
        }
    }
    /// The single JSON document on stdout.
    pub fn json(&self) -> Value {
        assert_eq!(
            self.stdout.trim_end().lines().count(),
            1,
            "{:?}: stdout must be exactly one line\n{}\n{}",
            self.args,
            self.stdout,
            self.stderr
        );
        assert!(self.stdout.ends_with('\n'), "{:?}", self.args);
        let value: Value = serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("{:?}: {e}\n{}", self.args, self.stdout));
        assert_no_nulls(&value, &self.args);
        value
    }
    pub fn success(&self) -> Value {
        assert_eq!(
            self.code,
            Some(0),
            "{:?}\n{}\n{}",
            self.args,
            self.stdout,
            self.stderr
        );
        let value = self.json();
        assert!(value.get("error").is_none(), "{:?}: {value}", self.args);
        value
    }
    pub fn failure(&self, exit: i32) -> Value {
        assert_eq!(
            self.code,
            Some(exit),
            "{:?}\n{}\n{}",
            self.args,
            self.stdout,
            self.stderr
        );
        let value = self.json();
        let error = value["error"].clone();
        assert!(
            error["code"].is_string() && error["message"].is_string(),
            "{value}"
        );
        assert_eq!(
            value.as_object().unwrap().len(),
            1,
            "an error document has only error: {value}"
        );
        error
    }
}

pub fn assert_no_nulls(value: &Value, context: &[String]) {
    match value {
        Value::Null => panic!("{context:?}: a null value in the result"),
        Value::Array(items) => items.iter().for_each(|item| assert_no_nulls(item, context)),
        Value::Object(map) => map.values().for_each(|item| assert_no_nulls(item, context)),
        _ => {}
    }
}

/// Whether `path` exists, for checks that nothing was written.
pub fn exists(path: &Path) -> bool {
    path.exists()
}
