//! `shell` and `watch`: several commands in one process with the camera
//! kept connected, one result a line; and live measurements, one JSON line
//! each.
#![deny(clippy::doc_markdown)]
mod support;

use serde_json::Value;
use std::time::{Duration, Instant};
use support::cli::{Home, Run, assert_no_nulls};

/// Each line of stdout as a JSON document without nulls.
fn lines(run: &Run) -> Vec<Value> {
    run.stdout
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{:?}: {e}\n{line}\n{}", run.args, run.stderr));
            assert_no_nulls(&value, &run.args);
            value
        })
        .collect()
}

#[test]
fn commands_on_stdin_print_one_json_line_each_in_order() {
    let home = Home::new("shell-batch");
    let run = home.run_stdin(
        "set exposure=5ms\n\n# a comment\ncapture -o a.png\n  inspect --preview none\nget exposure Width\n",
        &["shell", "-c", "sim:0", "--json"],
    );
    assert_eq!(run.code, Some(0), "{}\n{}", run.stdout, run.stderr);
    let results = lines(&run);
    assert_eq!(results.len(), 4, "{}", run.stdout);
    assert_eq!(results[0]["changed"][0]["name"], "ExposureTime");
    assert_eq!(results[1]["count"], 1);
    assert_eq!(results[1]["settings"]["exposure_us"], 5000.0);
    assert!(home.path("a.png").exists());
    // The commands ran in order, against the camera the shell holds.
    assert_eq!(results[2]["settings"]["exposure_us"], 5000.0);
    assert_eq!(results[3]["values"]["exposure"], "5ms");
    assert_eq!(results[3]["values"]["Width"], 640);
    for result in &results {
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(result["camera"], "sim:0");
    }
    // Text, one command's output after another.
    let text = home.run_stdin("get Width\nget Height\n", &["shell", "-c", "sim:0"]);
    assert_eq!(text.code, Some(0), "{}", text.stderr);
    assert_eq!(text.stdout, "Width = 640\nHeight = 480\n");
}

#[test]
fn the_first_failure_ends_the_run_unless_it_keeps_going() {
    let home = Home::new("shell-failure");
    let input = "get Width\nget Nope\nset Width=5\nget Height\n";
    let stopped = home.run_stdin(input, &["shell", "-c", "sim:0", "--json"]);
    assert_eq!(stopped.code, Some(2), "{}", stopped.stderr);
    let results = lines(&stopped);
    assert_eq!(results.len(), 2);
    assert_eq!(results[1]["error"]["code"], "feature_not_found");
    assert_eq!(results[1].as_object().unwrap().len(), 1, "{}", results[1]);
    // Going on, every line answers, and the exit code is the first
    // failure's.
    let going = home.run_stdin(input, &["--json", "shell", "-c", "sim:0", "--keep-going"]);
    assert_eq!(going.code, Some(2));
    let results = lines(&going);
    assert_eq!(results.len(), 4);
    assert_eq!(results[2]["error"]["code"], "out_of_range");
    assert_eq!(results[3]["values"]["Height"], 480);
    // A line clap cannot parse is a usage document like any other error.
    let typo = home.run_stdin(
        "gett Width\nget Width\n",
        &["shell", "-c", "sim:0", "--json"],
    );
    assert_eq!(typo.code, Some(2));
    let results = lines(&typo);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["error"]["code"], "usage");
    assert_eq!(results[0]["error"]["details"]["kind"], "invalid_subcommand");
    // A later failure of another kind does not change the exit code.
    let later = home.run_stdin(
        "get Nope\nexec Width\n",
        &["shell", "-c", "sim:0", "--json", "--keep-going"],
    );
    assert_eq!(later.code, Some(2), "{}", later.stdout);
    assert_eq!(lines(&later).len(), 2);
    // An empty run succeeds.
    let empty = home.run_stdin("\n# nothing\n", &["shell", "--json"]);
    assert_eq!(empty.code, Some(0));
    assert_eq!(empty.stdout, "");
}

#[test]
fn commands_as_arguments_run_instead_of_stdin() {
    let home = Home::new("shell-execute");
    let run = home.run_stdin(
        "get Height\n",
        &[
            "shell",
            "-c",
            "sim:0",
            "--json",
            "-e",
            "set gain=0",
            "-e",
            "get gain",
        ],
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let results = lines(&run);
    assert_eq!(results.len(), 2, "stdin is not read: {}", run.stdout);
    assert_eq!(results[1]["values"]["gain"], 0.0);
    // `exit` ends a run early.
    let exited = home.run(&[
        "shell",
        "-c",
        "sim:0",
        "-e",
        "get Width",
        "-e",
        "exit",
        "-e",
        "get Height",
    ]);
    assert_eq!(exited.code, Some(0));
    assert_eq!(exited.stdout, "Width = 640\n");
    // Help is text, and succeeds.
    let help = home.run(&["shell", "-e", "info --help"]);
    assert_eq!(help.code, Some(0));
    assert!(help.stdout.contains("Examples:"), "{}", help.stdout);
    // With --json it is a line like any result, as the help built-in's.
    let help = home.run_stdin(
        "get --help\nhelp get\nget Width\n",
        &["shell", "-c", "sim:0", "--json"],
    );
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    let results = lines(&help);
    assert_eq!(results.len(), 3, "{}", help.stdout);
    for result in &results[..2] {
        assert!(
            result["help"]
                .as_str()
                .unwrap()
                .contains("Usage: capturefab get"),
            "{result}"
        );
    }
    assert_eq!(results[2]["values"]["Width"], 640);
}

#[test]
fn use_picks_the_camera_for_commands_without_one() {
    let home = Home::new("shell-use");
    let run = home.run(&[
        "--json",
        "shell",
        "-c",
        "sim:0",
        "-e",
        "use",
        "-e",
        "use sim:1",
        "-e",
        "info",
        "-e",
        "get -c sim:0 Width",
        "-e",
        "cameras",
        "-e",
        "use none",
        "-e",
        "help",
    ]);
    assert_eq!(run.code, Some(0), "{}\n{}", run.stdout, run.stderr);
    let results = lines(&run);
    assert_eq!(results[0]["camera"], "sim:0");
    assert_eq!(results[1]["camera"], "sim:1");
    assert_eq!(results[2]["camera"], "sim:1");
    // A camera named on the line wins.
    assert_eq!(results[3]["camera"], "sim:0");
    let cameras = results[4]["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 2, "{}", results[4]);
    assert!(
        cameras
            .iter()
            .any(|camera| camera["id"] == "sim:1" && camera["default"] == true)
    );
    assert!(cameras.iter().all(|camera| camera["state"] != "failed"));
    assert!(results[5].as_object().unwrap().is_empty());
    let help = results[6]["help"].as_str().unwrap();
    assert!(
        help.contains("capture") && help.contains("use CAM"),
        "{help}"
    );
    let bad = home.run(&["--json", "shell", "-e", "use sim:0 sim:1"]);
    assert_eq!(bad.code, Some(2), "{}", bad.stdout);
    assert_eq!(lines(&bad)[0]["error"]["code"], "usage");
}

#[test]
fn stdin_and_stdout_stay_the_shells() {
    let home = Home::new("shell-refuse");
    for line in [
        "serve",
        "debug rpc",
        "capture -o -",
        "shell",
        "login add rtsp://cam.local/x --user a --password-stdin",
    ] {
        let run = home.run_stdin(&format!("{line}\n"), &["shell", "-c", "sim:0", "--json"]);
        assert_eq!(run.code, Some(2), "{line}: {}", run.stdout);
        let results = lines(&run);
        assert_eq!(results[0]["error"]["code"], "usage", "{line}");
    }
}

#[test]
fn the_shell_keeps_its_cameras_in_the_warm_session() {
    let home = Home::new("shell-warm");
    let run = home
        .warm_command()
        .args([
            "--json",
            "shell",
            "-c",
            "sim:0",
            "-e",
            "get Width",
            "-e",
            "cameras",
        ])
        .output()
        .unwrap();
    let run = Run::of_output(run, &["shell"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let results = lines(&run);
    assert_eq!(results[0]["values"]["Width"], 640);
    assert_eq!(
        results[1]["cameras"][0]["session"], "auto",
        "{}",
        results[1]
    );
    // Once the shell exits, the camera stays warm for the next command.
    let sessions = home.ok(&["session", "list"]);
    assert_eq!(sessions["sessions"][0]["session"], "auto", "{sessions}");
    assert_eq!(sessions["sessions"][0]["cameras"][0], "sim:0", "{sessions}");
}

/// A signal ends a run of commands: the command it interrupts reports
/// `interrupted`, and nothing after it runs.
#[cfg(unix)]
#[test]
fn a_signal_stops_the_command_and_the_run() {
    use std::{
        io::{BufRead as _, BufReader, Write as _},
        process::Stdio,
    };
    let home = Home::new("shell-signal");
    let mut child = home
        .command()
        .args(["--json", "shell", "-c", "sim:0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    // A line trigger without lines: the inspection waits for a frame.
    stdin
        .write_all(b"set trigger=line0\ninspect --preview none --timeout 1m\nget Width\n")
        .unwrap();
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.contains("changed"), "{first}");
    std::thread::sleep(Duration::from_millis(500));
    let sent = Instant::now();
    let status = support::cli::signal_and_wait(&mut child, libc::SIGINT);
    assert!(
        sent.elapsed() < Duration::from_secs(10),
        "{:?}",
        sent.elapsed()
    );
    assert_eq!(status.code(), Some(130));
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    let rest = rest.lines().collect::<Vec<_>>();
    assert_eq!(rest.len(), 1, "{rest:?}");
    let error: Value = serde_json::from_str(rest[0]).unwrap();
    assert_eq!(error["error"]["code"], "interrupted");
}

/// The shell on a terminal of its own: what it shows, and keys typed at it.
#[cfg(unix)]
struct Pty {
    master: std::fs::File,
    child: std::process::Child,
    screen: std::sync::Arc<std::sync::Mutex<String>>,
}

#[cfg(unix)]
impl Pty {
    fn spawn(mut command: std::process::Command) -> Self {
        use std::os::{fd::FromRawFd as _, unix::process::CommandExt as _};
        let (master, slave) = {
            let (mut master, mut slave) = (0, 0);
            // SAFETY: openpty writes two descriptors this test owns.
            let opened = unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(opened, 0);
            // SAFETY: openpty created the master descriptor, now owned by this File.
            let master = unsafe { std::fs::File::from_raw_fd(master) };
            // SAFETY: openpty created the distinct slave descriptor, owned by this File.
            let slave = unsafe { std::fs::File::from_raw_fd(slave) };
            (master, slave)
        };
        command
            .env("TERM", "xterm-256color")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        let own_terminal = || {
            // Apple's constant is narrower than ioctl's request argument.
            #[cfg(target_vendor = "apple")]
            let request = libc::c_ulong::from(libc::TIOCSCTTY);
            #[cfg(not(target_vendor = "apple"))]
            let request = libc::TIOCSCTTY;
            // SAFETY: setsid takes no pointers and establishes this child's session.
            if unsafe { libc::setsid() } < 0
                // SAFETY: stdin is the slave terminal, with no pointer argument required.
                || unsafe { libc::ioctl(0, request, 0) } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        };
        // SAFETY: setsid and ioctl are async-signal-safe; the closure only uses locals.
        unsafe { command.pre_exec(own_terminal) };
        let child = command.spawn().unwrap();
        drop(command);
        let screen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        {
            let screen = screen.clone();
            let mut reader = master.try_clone().unwrap();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while let Ok(count) = std::io::Read::read(&mut reader, &mut buffer) {
                    if count == 0 {
                        break;
                    }
                    screen
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buffer[..count]));
                }
            });
        }
        Self {
            master,
            child,
            screen,
        }
    }
    /// Where the screen is now: what `wait_for` looks after.
    fn mark(&self) -> usize {
        self.screen.lock().unwrap().len()
    }
    /// Wait for `text` after `mark`.
    fn wait_for(&self, mark: usize, text: &str, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let shown = self.screen.lock().unwrap().clone();
            if shown.get(mark..).is_some_and(|after| after.contains(text)) {
                return;
            }
            assert!(Instant::now() < deadline, "no {text:?} in\n{shown}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn type_in(&mut self, keys: &[u8]) {
        std::io::Write::write_all(&mut self.master, keys).unwrap();
    }
    /// Run `line` and wait for `text` in what it shows: how long it took.
    fn run(&mut self, line: &str, text: &str, within: Duration) -> Duration {
        let (mark, asked) = (self.mark(), Instant::now());
        self.type_in(format!("{line}\r").as_bytes());
        self.wait_for(mark, text, within);
        asked.elapsed()
    }
    /// Start `line`, then press Ctrl-C while it waits, and wait for `text`
    /// after that.
    fn interrupt(&mut self, line: &str, text: &str) {
        self.type_in(format!("{line}\r").as_bytes());
        std::thread::sleep(Duration::from_millis(700));
        let mark = self.mark();
        self.type_in(b"\x03");
        self.wait_for(mark, text, Duration::from_secs(5));
    }
    fn exit(mut self) -> Option<i32> {
        self.type_in(b"exit\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(Instant::now() < deadline, "the shell did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// At a terminal, Ctrl-C stops the command running, not the shell, and
/// the camera is free for the next command at once.
#[cfg(unix)]
#[test]
fn ctrl_c_at_a_terminal_stops_the_command_not_the_shell() {
    let home = Home::new("shell-pty");
    let mut command = home.command();
    command.args(["shell", "-c", "sim:0"]);
    let mut shell = Pty::spawn(command);
    shell.wait_for(0, "sim:0", Duration::from_secs(10));
    shell.run(
        "set trigger=line0",
        "TriggerMode[FrameStart]: Off",
        Duration::from_secs(10),
    );
    shell.interrupt("inspect --preview none --timeout 1m", "interrupted");
    // The camera is not left waiting for the inspection's frame.
    let took = shell.run("get Width", "Width = 640", Duration::from_secs(5));
    assert!(took < Duration::from_secs(5));
    assert_eq!(shell.exit(), Some(0));
}

/// Progress shows at a terminal while a burst saves, unless the line
/// says -q.
#[cfg(unix)]
#[test]
fn quiet_lines_show_no_progress() {
    let home = Home::new("shell-pty-quiet");
    let mut command = home.command();
    command.args(["shell", "-c", "sim:0"]);
    let mut shell = Pty::spawn(command);
    shell.wait_for(0, "sim:0", Duration::from_secs(10));
    let shown = |shell: &mut Pty, line: &str| {
        let mark = shell.mark();
        shell.run(line, "Saved 4 frames", Duration::from_secs(20));
        shell
            .screen
            .lock()
            .unwrap()
            .get(mark..)
            .expect("screen mark lies on a character boundary")
            .to_owned()
    };
    let loud = shown(&mut shell, "capture -n 4 --every 250ms -o loud/");
    assert!(loud.contains("saved ·"), "{loud}");
    let quiet = shown(&mut shell, "capture -q -n 4 --every 250ms -o quiet/");
    assert!(!quiet.contains("saved ·"), "{quiet}");
    assert_eq!(shell.exit(), Some(0));
}

/// Ctrl-C in a shell whose cameras are in a session stops what every
/// thread of the command waits for: a camera of a several-camera
/// inspection, or watch's measurement, takes the next command at once;
/// and watch stops the stream it started.
#[cfg(unix)]
#[test]
fn ctrl_c_in_a_shell_frees_session_cameras_at_once() {
    let home = Home::new("shell-pty-session");
    let mut command = home.warm_command();
    command.args(["shell", "-c", "sim:3"]);
    let mut shell = Pty::spawn(command);
    shell.wait_for(0, "sim:3", Duration::from_secs(10));
    for camera in ["sim:3", "sim:4"] {
        shell.run(
            &format!("set -c {camera} trigger=line0"),
            "TriggerMode[FrameStart]: Off",
            Duration::from_secs(10),
        );
    }
    shell.interrupt(
        "inspect -c sim:3 -c sim:4 --preview none --timeout 1m",
        "interrupted",
    );
    let took = shell.run("set -c sim:4 trigger=off", "Off", Duration::from_secs(5));
    assert!(took < Duration::from_secs(3), "{took:?}");
    // Ctrl-C ends watch normally, its measurement waiting for a frame,
    // which it says it waits for meanwhile.
    let mark = shell.mark();
    shell.type_in(b"watch --timeout 1m\r");
    shell.wait_for(
        mark,
        "waiting for a frame from sim:3",
        Duration::from_secs(5),
    );
    let mark = shell.mark();
    shell.type_in(b"\x03");
    shell.wait_for(mark, "sim:3> ", Duration::from_secs(5));
    let took = shell.run("set trigger=off", "Off", Duration::from_secs(5));
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!(shell.exit(), Some(0));
    let status = home.ok(&["session", "status", "auto"]);
    for camera in status["cameras"].as_array().unwrap() {
        assert_eq!(camera["state"], "idle", "{status}");
    }
}

/// Ctrl-C, how watch usually ends, stops the stream it started in the
/// session before watch exits: the camera does not stream on.
#[cfg(unix)]
#[test]
fn ctrl_c_ends_watch_and_its_stream() {
    use std::{
        io::{BufRead as _, BufReader},
        process::Stdio,
    };
    let home = Home::new("watch-signal");
    let mut child = home
        .warm_command()
        .args(["watch", "-c", "sim:2", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.contains("\"t\""), "{first}");
    let status = support::cli::signal_and_wait(&mut child, libc::SIGINT);
    assert_eq!(status.code(), Some(0));
    let status = home.ok(&["session", "status", "auto"]);
    assert_eq!(status["cameras"][0]["id"], "sim:2", "{status}");
    assert_eq!(status["cameras"][0]["state"], "idle", "{status}");
}

#[test]
fn watch_prints_one_measurement_a_line_without_a_terminal() {
    let home = Home::new("watch");
    let began = Instant::now();
    let run = home.run(&["watch", "-c", "sim:0", "--json", "--for", "1s"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(began.elapsed() < Duration::from_secs(20));
    let results = lines(&run);
    assert!(results.len() >= 2, "{}", run.stdout);
    let mut previous = -1.0;
    for result in &results {
        let t = result["t"].as_f64().unwrap();
        assert!(t > previous, "{result}");
        previous = t;
        assert_eq!(result["camera"], "sim:0");
        assert_eq!(result["frame"]["width"], 640);
        assert!(result["exposure"]["mean"].is_number(), "{result}");
        assert!(result["focus"]["sharpness"].as_f64().unwrap() > 0.0);
        assert!(
            result["focus"]["best"].as_f64().unwrap()
                >= result["focus"]["sharpness"].as_f64().unwrap()
        );
        assert!(result["frames"].as_u64().unwrap() >= 1);
        assert!(result.get("error").is_none());
    }
    // Every key is one schema documents, and so is every error it gives;
    // a shell's lines give any command's, and interrupted.
    let schema = home.ok(&["schema", "watch"]);
    let errors = |schema: &Value| -> Vec<String> {
        serde_json::from_value(schema["errors"].clone()).unwrap()
    };
    for code in ["exists", "feature_not_found", "out_of_range", "interrupted"] {
        assert!(!errors(&schema).iter().any(|error| error == code), "{code}");
    }
    let shell = home.ok(&["schema", "shell"]);
    for code in ["feature_not_found", "out_of_range", "interrupted", "exists"] {
        assert!(errors(&shell).iter().any(|error| error == code), "{code}");
    }
    assert!(shell["result"]["lines"].as_str().unwrap().contains("stdin"));
    let documented = schema["result"].clone();
    for result in &results {
        for key in result.as_object().unwrap().keys() {
            assert!(
                documented.get(key).is_some(),
                "{key} is not in schema watch"
            );
        }
    }
    // Without a terminal the lines are JSON without --json too; a region
    // is measured on its own.
    let plain = home.run(&[
        "watch",
        "-c",
        "sim:0",
        "--for",
        "200ms",
        "--every",
        "100ms",
        "--region",
        "0,0,0.5,0.5",
    ]);
    assert_eq!(plain.code, Some(0), "{}", plain.stderr);
    assert!(lines(&plain).len() >= 2);
    let fast = home.fails(2, &["watch", "-c", "sim:0", "--every", "10ms"]);
    assert_eq!(fast["code"], "usage");
}

#[test]
fn watch_measures_inside_a_shell_too() {
    let home = Home::new("shell-watch");
    let run = home.run_stdin(
        "watch --for 500ms --every 250ms\nget Width\n",
        &["shell", "-c", "sim:0", "--json"],
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let results = lines(&run);
    assert!(results.len() >= 3, "{}", run.stdout);
    assert!(results[0]["t"].is_number());
    assert_eq!(results.last().unwrap()["values"]["Width"], 640);
}
